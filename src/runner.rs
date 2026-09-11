//! Execution: carry out an [`ExecutionPlan`], consume the model's output
//! and assemble the run's artifacts.

use crate::api::{Client, Connection, GenerateRequest, GenerateResult};
use crate::domain::{
    AppError, AppResult, Artifact, Destination, GenerationStatus, InputPart, MediaKind, Provenance,
};
use crate::plan::{DeliveryMode, ExecutionPlan};
use crate::processors::chunk::{reduce_inputs, ChunkGate};
use crate::processors::ocr::BoundaryGate;
use crate::processors::StepRole;
use crate::spinner::Spinner;
use crate::tasks::ProcessorKind;
use std::cell::RefCell;
use std::io::Write as _;
use std::rc::Rc;

/// What a completed generation produced, ready for delivery and history.
pub struct RunOutput {
    pub artifacts: Vec<Artifact>,
    pub status: GenerationStatus,
    pub warnings: Vec<String>,
    /// Live stdout already printed the text (cannot be taken back).
    pub live_stdout: bool,
    /// Per-part batches only: the parts that failed, in input order.
    /// Surviving parts' artifacts are complete and deliverable; the run
    /// exits 6 (partial) after normal delivery.
    pub failed_parts: Vec<FailedPart>,
    /// Total parts in a per-part batch (0 outside one).
    pub parts_total: usize,
    /// The request error that stopped a single-document run mid-way; a
    /// batch never sets this (its failures are per-part). Whatever text
    /// already merged still reached `artifacts`, so the caller records
    /// the partial run and classifies the exit code from this error.
    pub failure: Option<AppError>,
    /// Steps whose replies completed, of `steps_total` planned requests.
    pub steps_done: usize,
    pub steps_total: usize,
}

/// One failed part of a per-part batch, named by its input file
/// (`b.png`) with the error that killed it. Its artifact never existed:
/// the part's half-finished replies were discarded.
#[derive(Debug, Clone)]
pub struct FailedPart {
    pub name: String,
    pub error: String,
}

impl RunOutput {
    /// Why the artifacts do not satisfy what the plan asked for, if they
    /// do not. The plan's expectations re-validated against the real
    /// response (contract §step 4.8): a missing kind, a short count, or
    /// nothing usable at all. The caller records the run and refuses
    /// delivery instead of discarding what did come back.
    pub fn unsatisfied_reason(&self, plan: &ExecutionPlan) -> Option<String> {
        for kind in &plan.resolved.produce {
            if !self.artifacts.iter().any(|a| a.kind == *kind) {
                return Some(format!(
                    "the response did not produce the requested '{kind}' output"
                ));
            }
        }
        for (kind, expected) in &plan.expected_counts {
            if let Some(n) = expected {
                let have = self.artifacts.iter().filter(|a| a.kind == *kind).count() as u64;
                if have != *n {
                    return Some(format!("expected {n} {kind} artifact(s), got {have}"));
                }
            }
        }
        if self.artifacts.is_empty() {
            return Some("the model returned no usable content for this run".into());
        }
        None
    }
}

fn env_key(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

struct DeltaSink {
    merged: String,
    live: bool,
    chars_seen: u64,
}

impl DeltaSink {
    fn emit(&mut self, text: &str) {
        self.chars_seen += text.chars().count() as u64;
        self.merged.push_str(text);
        if self.live {
            print!("{text}");
            let _ = std::io::stdout().flush();
        }
    }
}

/// Merges a multi-request run's replies into one text stream. The right
/// join depends on the strategy: ocr-tiles re-shows overlap bands and
/// dedups them; chunk-join carries context instead, so chunks join with
/// a plain paragraph break. A chunk-reduce run never merges — its map
/// replies are intermediate and the reduce reply is the whole artifact.
enum SliceMerger {
    Boundary(BoundaryGate),
    Chunk(ChunkGate),
}

impl SliceMerger {
    fn push_delta(&mut self, delta: &str) {
        match self {
            Self::Boundary(gate) => gate.push_delta(delta),
            Self::Chunk(gate) => gate.push_delta(delta),
        }
    }

    fn slice_end(&mut self, hard: bool) {
        match self {
            Self::Boundary(gate) => gate.slice_end(hard),
            Self::Chunk(gate) => gate.slice_end(),
        }
    }

    fn finish(&mut self) {
        match self {
            Self::Boundary(gate) => gate.finish(),
            Self::Chunk(gate) => gate.finish(),
        }
    }
}

/// Run every step of the plan. Text deltas stream live when the plan says
/// so; slice replies merge through the strategy's gate so live and
/// buffered delivery end up byte-identical.
///
/// In a per-part batch (steps tagged with parts), each part owns its own
/// sink and gate: its replies merge only within the part and promote to
/// one artifact named by the part's stem. A part that fails — transport
/// error or truncated reply — is dropped with a warning and the run
/// continues with the next part; the caller reports exit 6 after
/// delivering the survivors.
///
/// Outside a batch, a request error stops the run but never discards it:
/// the output is `Ok` with status `Incomplete`, the merged text of the
/// replies that did arrive, and the error itself in `failure` — the
/// caller records the partial generation instead of losing it.
pub async fn execute(plan: &ExecutionPlan) -> AppResult<RunOutput> {
    let api_key = plan.resolved.api_key_env.as_deref().and_then(|name| {
        // The default provider also accepts the conventional OpenAI name.
        env_key(name).or_else(|| {
            (name == "AIDO_API_KEY")
                .then(|| env_key("OPENAI_API_KEY"))
                .flatten()
        })
    });
    let conn = Connection {
        base_url: plan.resolved.base_url.clone(),
        api_key,
        timeout: plan.timeout,
        total_timeout: plan.total_timeout,
        adapter: plan.resolved.adapter,
    };
    let client = Client::new(&conn).map_err(AppError::from)?;

    let live_stdout = plan.delivery == DeliveryMode::Live
        && plan.destinations.contains(&Destination::Stdout)
        && plan.resolved.produce.contains(&MediaKind::Text);

    // Parts present and their step counts: a batch has more than one, and
    // a group with several requests merges through the strategy's gate.
    let mut part_sizes: Vec<(Option<usize>, usize)> = Vec::new();
    for step in &plan.steps {
        match part_sizes.iter_mut().find(|(id, _)| *id == step.part) {
            Some((_, n)) => *n += 1,
            None => part_sizes.push((step.part, 1)),
        }
    }
    let batch = part_sizes.iter().filter(|(id, _)| id.is_some()).count() > 1;
    let parts_total = part_sizes.iter().filter(|(id, _)| id.is_some()).count();
    // A chunk-reduce run (map steps plus one reduce step per group) holds
    // map replies back as the reduce request's material instead of
    // merging them for delivery.
    let reduce_plan = plan.steps.iter().any(|s| s.role == StepRole::Reduce);
    let group_size = |id: Option<usize>| {
        part_sizes
            .iter()
            .find(|(pid, _)| *pid == id)
            .map(|(_, n)| *n)
            .unwrap_or(1)
    };

    let spinner = if plan.quiet {
        Spinner::disabled()
    } else {
        Spinner::start(&format!("asking {}...", plan.resolved.model))
    };

    let mut media_artifacts: Vec<Artifact> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut overall = GenerationStatus::Complete;
    let mut artifacts: Vec<Artifact> = Vec::new();
    let mut failed_parts: Vec<FailedPart> = Vec::new();
    let mut failure: Option<AppError> = None;
    let mut steps_done = 0usize;

    // The part currently receiving replies. Outside a batch this is one
    // unnamed group spanning every step — the exact pre-batch behavior.
    struct Group {
        id: Option<usize>,
        stem: String,
        /// Every request whose reply feeds this group's artifact, in step
        /// order. A reduce group holds only its reduce request: the map
        /// replies are intermediate material, so the artifact names the
        /// one request that produced it.
        requests: Vec<usize>,
        sink: Rc<RefCell<DeltaSink>>,
        gate: Option<SliceMerger>,
        /// A reduce run's per-map-step replies, in step order: the reduce
        /// request's future material, never delivered on their own.
        sections: Vec<String>,
    }
    // Warnings and failures name the part by its input name (`b.png`),
    // not its artifact stem (`b`).
    let part_name = |plan: &ExecutionPlan, id: Option<usize>, stem: &str| {
        id.and_then(|i| plan.inputs.get(i))
            .map(|p| p.name.clone())
            .unwrap_or_else(|| stem.to_string())
    };
    let mut group: Option<Group> = None;
    let mut skip_part: Option<usize> = None;
    let mut total_chars = 0u64;

    // Merge a finished group into an artifact. An empty part in a batch
    // is a failure, not a silent gap.
    macro_rules! close_group {
        ($group:expr, $artifacts:expr, $failed:expr) => {{
            let mut g = $group;
            if let Some(gate) = g.gate.as_mut() {
                gate.finish();
            }
            let merged = g.sink.borrow();
            total_chars += merged.chars_seen;
            if !merged.merged.is_empty() {
                // One reply is that request's artifact; several joined
                // replies name every request they merged, in order.
                let provenance = match g.requests.as_slice() {
                    [only] => Provenance::Request { index: *only },
                    many => Provenance::Merged {
                        requests: many.to_vec(),
                    },
                };
                $artifacts.push(Artifact {
                    id: g.stem.clone(),
                    kind: MediaKind::Text,
                    mime: "text/plain".into(),
                    format: "text".into(),
                    bytes: merged.merged.clone().into_bytes(),
                    provenance,
                });
            } else if g.id.is_some() {
                let name = part_name(plan, g.id, &g.stem);
                $failed.push(FailedPart {
                    name: name.clone(),
                    error: "the model returned no usable text for this part".into(),
                });
                warnings.push(format!("part '{name}' failed: no usable text"));
            }
        }};
    }

    for step in &plan.steps {
        if let Some(id) = skip_part {
            if step.part == Some(id) {
                continue;
            }
            skip_part = None;
        }
        // A part boundary closes the previous group with its artifact.
        if let Some(g) = group.as_ref() {
            if g.id != step.part {
                let done = group.take().unwrap();
                close_group!(done, artifacts, failed_parts);
            }
        }
        if group.is_none() {
            let sink = Rc::new(RefCell::new(DeltaSink {
                merged: String::new(),
                live: live_stdout && !batch,
                chars_seen: 0,
            }));
            // A reduce run never merges through a gate: map replies
            // accumulate as sections (the reduce request's material) and
            // the reduce reply is a single request's output.
            let gate = (group_size(step.part) > 1 && !reduce_plan).then(|| {
                let gate_sink = sink.clone();
                match plan.processor {
                    ProcessorKind::ChunkJoin | ProcessorKind::ChunkReduce => {
                        SliceMerger::Chunk(ChunkGate::new(move |t: &str| {
                            gate_sink.borrow_mut().emit(t)
                        }))
                    }
                    // Single never multi-steps, but the arm keeps the
                    // match total so a future variant must pick its merge
                    // explicitly.
                    ProcessorKind::Single | ProcessorKind::OcrTiles => {
                        SliceMerger::Boundary(BoundaryGate::new(move |t: &str| {
                            gate_sink.borrow_mut().emit(t)
                        }))
                    }
                }
            });
            group = Some(Group {
                id: step.part,
                stem: step.artifact_stem.clone().unwrap_or_else(|| "text".into()),
                requests: Vec::new(),
                sink,
                gate,
                sections: Vec::new(),
            });
        }
        // Map replies of a reduce run are intermediate: each accumulates
        // into its own section and never reaches stdout or an artifact —
        // only the reduce reply streams live.
        if reduce_plan && step.role == StepRole::Map {
            group.as_mut().unwrap().sections.push(String::new());
        }
        if !plan.quiet {
            let label = if plan.steps.len() > 1 {
                format!(
                    "asking {} ({}/{}) — {}...",
                    plan.resolved.model,
                    step.index + 1,
                    plan.steps.len(),
                    step.label
                )
            } else {
                format!("asking {}...", plan.resolved.model)
            };
            spinner.set_message(&label);
        }
        // A reduce step's material is its group's collected map replies —
        // placeholders in the plan, filled in here. The task's original
        // instruction attaches to the reduce request like any other.
        let reduce_material;
        let inputs: &[InputPart] = if step.role == StepRole::Reduce {
            reduce_material = reduce_inputs(&group.as_ref().unwrap().sections);
            &reduce_material
        } else {
            &step.inputs
        };
        let request = GenerateRequest {
            instruction: (!plan.instruction.is_empty()).then_some(plan.instruction.as_str()),
            requirement: plan.requirement.as_deref(),
            inputs,
            model: &plan.resolved.model,
            max_tokens: plan.resolved.max_tokens,
            temperature: plan.resolved.temperature,
            outputs: &plan.resolved.produce,
            options: &plan.resolved.options,
        };
        // Deltas flow through the merge gate when a group has several
        // requests; a single request goes straight to the sink. The
        // closure's last use is inside the request call (plus the
        // buffered replay right after), so later group access is fine.
        let collect_section = reduce_plan && step.role == StepRole::Map;
        let mut on_delta = |delta: &str| {
            let g = group.as_mut().unwrap();
            if collect_section {
                g.sections.last_mut().unwrap().push_str(delta);
                return;
            }
            match g.gate.as_mut() {
                Some(gate) => gate.push_delta(delta),
                None => g.sink.borrow_mut().emit(delta),
            }
        };
        let result = if plan.transport_stream {
            client.generate_stream(&request, &mut on_delta).await
        } else {
            client.generate(&request).await
        };
        match result {
            Ok(mut reply) => {
                steps_done += 1;
                // The reply learns which request it answered only here:
                // an adapter saw one exchange, never the run.
                reply.request_index = step.index;
                // A buffered reply arrives whole: run it through the same
                // path the deltas would take.
                if !plan.transport_stream && !reply.text.is_empty() {
                    on_delta(&reply.text.clone());
                }
                // The reply is in, so the group records its request. A
                // reduce group keeps only the reduce request: its artifact
                // is the consolidation, not the map replies it consumed.
                let g = group.as_mut().unwrap();
                if step.role == StepRole::Reduce {
                    g.requests.clear();
                }
                g.requests.push(step.index);
                let truncated = reply.status != GenerationStatus::Complete;
                absorb(reply, &mut media_artifacts, &mut warnings, &mut overall);
                if let Some(g) = group.as_mut() {
                    if let Some(gate) = g.gate.as_mut() {
                        gate.slice_end(step.hard_cut_end);
                    }
                }
                // In a batch a truncated part fails alone; outside one the
                // run keeps the pre-batch behavior (recorded, undelivered).
                if truncated && batch {
                    let g = group.take().unwrap();
                    let error = "the reply was truncated".to_string();
                    let name = part_name(plan, g.id, &g.stem);
                    warnings.push(format!("part '{name}' failed: {error}"));
                    failed_parts.push(FailedPart { name, error });
                    skip_part = step.part;
                }
            }
            Err(e) => {
                let error = AppError::from(e);
                if !batch {
                    // The remaining requests stop here, but the replies that
                    // already arrived are real generated content: the run
                    // ends incomplete (naming the failed request), keeps
                    // whatever merged, and the caller records it instead of
                    // discarding it — re-running does not have to pay for
                    // the requests that succeeded.
                    overall = GenerationStatus::Incomplete {
                        reason: format!(
                            "request {}/{} failed: {}",
                            step.index + 1,
                            plan.steps.len(),
                            error.chain()
                        ),
                    };
                    // A reduce run has no final result without its reduce
                    // reply: map replies are intermediate material and a
                    // half-streamed reduce reply is not the consolidation
                    // either, so nothing is kept as the artifact.
                    if reduce_plan {
                        if let Some(g) = group.as_mut() {
                            g.sink.borrow_mut().merged.clear();
                        }
                    }
                    failure = Some(error);
                    break;
                }
                let g = group.take().unwrap();
                let message = error.chain();
                let name = part_name(plan, g.id, &g.stem);
                warnings.push(format!("part '{name}' failed: {message}"));
                failed_parts.push(FailedPart {
                    name,
                    error: message,
                });
                skip_part = step.part;
            }
        }
    }
    if let Some(g) = group.take() {
        close_group!(g, artifacts, failed_parts);
    }
    spinner.set_progress(total_chars);
    spinner.stop();
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }

    // Media artifacts keep their global promotion: a batch task produces
    // text per part; media stays an aggregate of the run. Provenance came
    // with the reply (which request produced it); the run-wide id is
    // assigned here.
    for (i, artifact) in media_artifacts.into_iter().enumerate() {
        artifacts.push(Artifact {
            id: format!("{}-{}", artifact.kind, i + 1),
            ..artifact
        });
    }

    Ok(RunOutput {
        artifacts,
        status: overall,
        warnings,
        live_stdout,
        failed_parts,
        parts_total: if batch { parts_total } else { 0 },
        failure,
        steps_done,
        steps_total: plan.steps.len(),
    })
}

fn absorb(
    reply: GenerateResult,
    media_artifacts: &mut Vec<Artifact>,
    warnings: &mut Vec<String>,
    overall: &mut GenerationStatus,
) {
    for warning in reply.warnings {
        if !warnings.contains(&warning) {
            warnings.push(warning);
        }
    }
    // The adapter's raw media becomes a domain artifact here, where the
    // run's context exists: provenance names the request the reply
    // answered. The id stays empty — the promotion below names the
    // artifact for the whole run.
    media_artifacts.extend(reply.artifacts.into_iter().map(|raw| Artifact {
        id: String::new(),
        kind: raw.kind,
        mime: raw.mime,
        format: raw.format,
        bytes: raw.bytes,
        provenance: Provenance::Request {
            index: reply.request_index,
        },
    }));
    if reply.status != GenerationStatus::Complete && *overall == GenerationStatus::Complete {
        *overall = reply.status;
    }
}

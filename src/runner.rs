//! Execution: carry out an [`ExecutionPlan`], consume the model's output
//! and assemble the run's artifacts.

use crate::api::{Adapter, Client, Connection, GenerateRequest, GenerateResult};
use crate::config::resolve::Resolved;
use crate::domain::{
    AppError, AppResult, Artifact, Destination, GenerationStatus, InputPart, MediaKind, Provenance,
};
use crate::plan::{DeliveryMode, ExecutionPlan};
use crate::processors::chunk::{reduce_inputs, ChunkGate};
use crate::processors::ocr::BoundaryGate;
use crate::processors::StepRole;
use crate::spinner::Spinner;
use crate::tasks::ProcessorKind;
use anyhow::anyhow;
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
    /// Characters actually printed live across the run's sinks. Unlike
    /// `live_stdout` (the mode) this is 0 when nothing reached the
    /// terminal — a reduce run keeps its map replies as buffered
    /// intermediates, so a failure there streamed nothing to take back.
    pub live_chars: u64,
    /// Per-part batches only: the parts that failed, in input order.
    /// Surviving parts' artifacts are complete and deliverable; the run
    /// exits 6 (partial) after normal delivery.
    pub failed_parts: Vec<FailedPart>,
    /// Total parts in a per-part batch (0 outside one).
    pub parts_total: usize,
    /// The request error that stopped a single-document run mid-way; a
    /// batch never sets this (its failures are per-part). Whatever text
    /// already arrived still reached `artifacts` — merged replies, or a
    /// reduce run's map replies kept as intermediates — so the caller
    /// records the partial run and classifies the exit code from this
    /// error.
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
        self.unsatisfied_reason_in(plan, &self.artifacts)
    }

    /// The same judgment over a slice of the artifacts — a chain judges
    /// its final stage's deliverables only, so an upstream artifact can
    /// never stand in for what the last stage did not produce.
    pub fn unsatisfied_reason_in(
        &self,
        plan: &ExecutionPlan,
        artifacts: &[Artifact],
    ) -> Option<String> {
        for kind in &plan.resolved.produce {
            if !artifacts.iter().any(|a| a.kind == *kind) {
                return Some(format!(
                    "the response did not produce the requested '{kind}' output"
                ));
            }
        }
        for (kind, expected) in &plan.expected_counts {
            if let Some(n) = expected {
                let have = artifacts.iter().filter(|a| a.kind == *kind).count() as u64;
                if have != *n {
                    return Some(format!("expected {n} {kind} artifact(s), got {have}"));
                }
            }
        }
        if artifacts.is_empty() {
            return Some("the model returned no usable content for this run".into());
        }
        None
    }
}

fn env_key(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// The run's spinner, shared with the live sinks: every step's first live
/// delta stops whatever spinner that step started with (see
/// `DeltaSink::emit`).
type SharedSpinner = Rc<RefCell<Option<Spinner>>>;

struct DeltaSink {
    merged: String,
    live: bool,
    /// Present only when the live text and the spinner share one terminal
    /// (stdout is a TTY too): a permanent handle on the shared cell. Each
    /// step's first live emit stops whatever spinner the step started
    /// with — the cell is emptied, this handle is not, so a restarted
    /// step spinner retires the same way. A piped stdout never displays
    /// the content, so there the spinner runs to the end of the run as
    /// before.
    spinner: Option<SharedSpinner>,
    chars_seen: u64,
    /// Only the text printed live: `chars_seen` also counts buffered
    /// sinks, whose output never reached the terminal.
    live_chars: u64,
    /// Whether the terminal cursor sits on a half-written content line:
    /// the last live print did not end in a newline. A step banner drawn
    /// after it must break the line first, or its `\r` redraws chop the
    /// content.
    line_open: bool,
}

impl DeltaSink {
    fn emit(&mut self, text: &str) {
        self.chars_seen += text.chars().count() as u64;
        self.merged.push_str(text);
        if self.live {
            // The spinner and the live reply share one terminal: before
            // this step's first delta reaches stdout, retire whatever
            // spinner the step started with — its \r-redraws would
            // otherwise land inside the streamed lines and chop them up
            // (issue #62). stop() erases the line and joins the thread,
            // so the content starts on a clean screen. The cell empties;
            // the handle stays, so a spinner restarted for a later step
            // (issue #81) retires here exactly the same way.
            if let Some(cell) = &self.spinner {
                if let Some(spinner) = cell.borrow_mut().take() {
                    spinner.stop();
                }
            }
            print!("{text}");
            let _ = std::io::stdout().flush();
            self.live_chars += text.chars().count() as u64;
            // An empty delta prints nothing, so it cannot move the cursor.
            if !text.is_empty() {
                self.line_open = !text.ends_with('\n');
            }
        }
    }
}

/// Merges a multi-request run's replies into one text stream. The right
/// join depends on the strategy: ocr-tiles re-shows overlap bands and
/// dedups them; chunk-join carries context instead, so chunks join with
/// a plain paragraph break. A chunk-reduce group never merges — its map
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

/// The spinner's opening phrase for a run: "asking <model>", except for
/// speech, which is synthesized rather than asked for. The edge-tts
/// adapter owns its endpoint — the profile's model never enters the
/// request, so naming it would report a party that does nothing (issue
/// #72: "asking deepseek-flash" while edge-tts speaks) — while the
/// OpenAI speech adapter does send the model, so it stays in the line.
fn spinner_prefix(resolved: &Resolved) -> String {
    match resolved.adapter {
        Adapter::EdgeTts => "synthesizing speech (edge-tts)".into(),
        Adapter::Speech => format!("synthesizing speech ({})", resolved.model),
        _ => format!("asking {}", resolved.model),
    }
}

/// Announce a step on the terminal, spinner-wise. A live sink retires the
/// spinner at its first delta (issue #62), so a multi-request run's later
/// steps found the cell empty and went silent — minutes of frozen cursor
/// between an image's slices (issue #81). `announce_step` restarts the
/// spinner for such a step; this step's first delta retires it again
/// exactly like the first one's.
///
/// `line_open` says the streamed content stopped mid-line: break it
/// before drawing, or the fresh spinner's `\r` redraws would chop it. A
/// non-terminal stderr never restarts — and never breaks the line either:
/// `2>log` must not gain blank lines inside the content. Only called for
/// non-quiet runs; a quiet one never starts a spinner to restart.
fn announce_step(cell: &SharedSpinner, label: &str, stderr_tty: bool, line_open: bool) {
    if let Some(spinner) = cell.borrow().as_ref() {
        spinner.set_message(label);
        return;
    }
    if !stderr_tty {
        return;
    }
    if line_open {
        println!();
    }
    *cell.borrow_mut() = Some(Spinner::start(label));
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
    // The same shared judgment the dry-run's credential line uses: the
    // provider's variable, or the conventional OpenAI name when the
    // default provider's key falls back to it.
    let api_key =
        crate::config::effective_key_env(plan.resolved.api_key_env.as_deref()).and_then(env_key);
    let conn = Connection {
        base_url: plan.resolved.base_url.clone(),
        api_key,
        timeout: plan.timeout,
        total_timeout: plan.total_timeout,
        adapter: plan.resolved.adapter,
    };
    let checkpoint = crate::transcription::Checkpoint::open(plan).map_err(AppError::from)?;
    let client = Client::new(&conn).map_err(AppError::from)?;

    // `--total-timeout` caps the whole run, not any single request: one
    // deadline fixed before the first request goes out, and every request
    // of the run must fit inside whatever remains of the budget.
    let deadline = plan.total_timeout.map(|t| tokio::time::Instant::now() + t);

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
    let group_size = |id: Option<usize>| {
        part_sizes
            .iter()
            .find(|(pid, _)| *pid == id)
            .map(|(_, n)| *n)
            .unwrap_or(1)
    };

    let mut prefix = spinner_prefix(&plan.resolved);
    // A chain stage says which stage of the run is asking: the spinner is
    // the only per-stage signal a quiet terminal gets.
    if let Some(label) = &plan.stage_label {
        prefix = format!("{label} — {prefix}");
    }

    let spinner: SharedSpinner = if plan.quiet {
        Rc::new(RefCell::new(None))
    } else {
        Rc::new(RefCell::new(Some(Spinner::start(&format!("{prefix}...")))))
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
        /// A reduce group's per-map-step replies, each paired with the
        /// index of the request that produced it: the reduce request's
        /// future material, never delivered on their own — unless the run
        /// fails, when history keeps them as intermediate artifacts.
        sections: Vec<(usize, String)>,
        /// Whether this group ends in a reduce step (chunk-reduce). The
        /// decision is per group, not per run: in a per-part batch one
        /// long file's reduce step must not treat a single-chunk file's
        /// only map reply as intermediate material.
        reduces: bool,
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
    let mut total_live_chars = 0u64;

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
            total_live_chars += merged.live_chars;
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
            // A chunk-reduce group (map steps plus one reduce step of THIS
            // part) holds map replies back as the reduce request's material
            // instead of merging them for delivery; a group without a
            // reduce step — a single-chunk file in a per-part batch, say —
            // delivers its map replies as its own output.
            let reduces = plan
                .steps
                .iter()
                .any(|s| s.part == step.part && s.role == StepRole::Reduce);
            let live = live_stdout && !batch;
            let sink = Rc::new(RefCell::new(DeltaSink {
                merged: String::new(),
                live,
                // Only a terminal stdout shares the screen with the
                // spinner; a piped one never displays the content, so it
                // keeps the indicator for the whole run.
                spinner: (live && plan.terminal.stdout).then(|| spinner.clone()),
                chars_seen: 0,
                live_chars: 0,
                line_open: false,
            }));
            // A reduce group never merges through a gate: map replies
            // accumulate as sections (the reduce request's material) and
            // the reduce reply is a single request's output.
            let gate = (group_size(step.part) > 1 && !reduces).then(|| {
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
                reduces,
            });
        }
        // Map replies of a reduce group are intermediate: each accumulates
        // into its own section and never reaches stdout or an artifact —
        // only the reduce reply streams live.
        let reduces = group.as_ref().unwrap().reduces;
        if reduces && step.role == StepRole::Map {
            group
                .as_mut()
                .unwrap()
                .sections
                .push((step.index, String::new()));
        }
        if !plan.quiet {
            let label = if plan.steps.len() > 1 {
                format!(
                    "{} ({}/{}) — {}...",
                    prefix,
                    step.index + 1,
                    plan.steps.len(),
                    step.label
                )
            } else {
                format!("{prefix}...")
            };
            // The current group's sink is the terminal's writer: its
            // `line_open` says where a restarted banner may draw.
            let line_open = group.as_ref().unwrap().sink.borrow().line_open;
            announce_step(&spinner, &label, plan.terminal.stderr, line_open);
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
        let collect_section = reduces && step.role == StepRole::Map;
        let mut on_delta = |delta: &str| {
            let g = group.as_mut().unwrap();
            if collect_section {
                g.sections.last_mut().unwrap().1.push_str(delta);
                return;
            }
            match g.gate.as_mut() {
                Some(gate) => gate.push_delta(delta),
                None => g.sink.borrow_mut().emit(delta),
            }
        };
        // One request future for either transport, so the budget below
        // bounds exactly what runs: the HTTP exchange plus the media
        // downloads inside generate() (image URL fetches). The block
        // scope ends with this statement, dropping the future (and its
        // `on_delta` borrow) before the reply handling below.
        let result = {
            let send = async {
                if let Some(state) = &checkpoint {
                    if let Some(reply) = state.load(step.index)? {
                        return Ok(reply);
                    }
                }
                let reply = if plan.transport_stream {
                    client.generate_stream(&request, &mut on_delta).await?
                } else {
                    client.generate(&request).await?
                };
                if let Some(state) = &checkpoint {
                    state.save(step.index, &reply)?;
                }
                Ok(reply)
            };
            // The budget is enforced inside the edge-tts adapter as well
            // (it receives total_timeout through Connection); for edge
            // runs the adapter's more specific error may surface instead
            // of this wrap's — the wrap is what extends the cap to every
            // adapter without its own.
            match deadline {
                // No budget: each request is bounded only by its own
                // per-request timeout, exactly as before.
                None => send.await,
                Some(d) => {
                    // A deadline exists only when a budget does, so the
                    // plan's seconds are always the cap actually set.
                    let secs = plan.total_timeout.map(|t| t.as_secs()).unwrap_or_default();
                    let spent = || anyhow!("the run exceeded its total time budget ({secs}s)");
                    let remaining = d.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        // The budget was spent before this request could
                        // start: nothing is sent, and the error flows
                        // down the same path as any request error.
                        Err(spent())
                    } else {
                        tokio::time::timeout(remaining, send)
                            .await
                            .unwrap_or_else(|_| Err(spent()))
                    }
                }
            }
        };
        let result = result.map_err(|error| {
            if let Some(dir) = &plan.transcribe_state {
                error.context(format!(
                    "transcription segment {}/{} failed; completed segments are in {}; rerun the same command to resume",
                    step.index + 1, plan.steps.len(), dir.display()
                ))
            } else {
                error
            }
        });
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
                // The status carries the batch failure message, and
                // `absorb` consumes the reply — read it before the move.
                let batch_failure = (truncated && batch).then(|| status_failure(&reply.status));
                absorb(reply, &mut media_artifacts, &mut warnings, &mut overall);
                if let Some(g) = group.as_mut() {
                    if let Some(gate) = g.gate.as_mut() {
                        gate.slice_end(step.hard_cut_end);
                    }
                }
                // In a batch a truncated part fails alone; outside one the
                // run keeps the pre-batch behavior (recorded, undelivered).
                if let Some(error) = batch_failure {
                    let g = group.take().unwrap();
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
                            // One line: the reason renders inside the
                            // `history list` status column.
                            error.chain_inline()
                        ),
                    };
                    // A reduce group has no final result without its reduce
                    // reply: a half-streamed reduce reply is not the
                    // consolidation, so nothing of it is kept. The map
                    // replies are different — each is a paid, complete
                    // reply — so every non-empty section survives as an
                    // intermediate artifact in the run's record (never
                    // delivered: the run is incomplete).
                    if let Some(g) = group.as_mut() {
                        if g.reduces {
                            g.sink.borrow_mut().merged.clear();
                            let name = part_name(plan, g.id, &g.stem);
                            let mut kept = 0usize;
                            for (i, (step_index, text)) in g.sections.iter().enumerate() {
                                if text.is_empty() {
                                    continue;
                                }
                                kept += 1;
                                artifacts.push(Artifact {
                                    id: format!("{}-chunk-{}", g.stem, i + 1),
                                    kind: MediaKind::Text,
                                    mime: "text/plain".into(),
                                    format: "text".into(),
                                    bytes: text.clone().into_bytes(),
                                    provenance: Provenance::Request { index: *step_index },
                                });
                            }
                            if kept > 0 {
                                let plural = if kept == 1 { "y" } else { "ies" };
                                warnings.push(format!(
                                    "kept {kept} intermediate map repl{plural} of '{name}' \
                                     after the failure; not delivered — see aido history show"
                                ));
                            }
                        }
                    }
                    failure = Some(error);
                    break;
                }
                let g = group.take().unwrap();
                // One line per part: the message feeds the partial-exit
                // listing and the record's `failed_parts`, both read as
                // single lines.
                let message = error.chain_inline();
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
    // A live run retired its spinner at the first delta; this covers
    // every other path (buffered, batch, no live output, and a restarted
    // step spinner whose reply never streamed).
    if let Some(spinner) = spinner.borrow_mut().take() {
        spinner.set_progress(total_chars);
        spinner.stop();
    }
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
        live_chars: total_live_chars,
        failed_parts,
        parts_total: if batch { parts_total } else { 0 },
        failure,
        steps_done,
        steps_total: plan.steps.len(),
    })
}

/// The failure message a non-Complete reply earns in a per-part batch:
/// the status's own reason when it carries one, the variant named in
/// plain words otherwise. "Truncated" is never guessed here — only a
/// status that says so itself may report it.
fn status_failure(status: &GenerationStatus) -> String {
    match status {
        GenerationStatus::Incomplete { reason } if !reason.trim().is_empty() => reason.clone(),
        GenerationStatus::Failed => "the reply failed".to_string(),
        GenerationStatus::Cancelled => "the reply was cancelled".to_string(),
        // An empty reason, or a status without one (the adapters never
        // deliver a Running reply): only "the reply did not finish" is
        // known.
        _ => "the reply was incomplete".to_string(),
    }
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

#[cfg(test)]
mod tests;

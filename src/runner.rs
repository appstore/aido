//! Execution: carry out an [`ExecutionPlan`], consume the model's output
//! and assemble the run's artifacts.

use crate::api::{Client, Connection, GenerateRequest, GenerateResult};
use crate::domain::{
    AppError, AppResult, Artifact, Destination, ErrorKind, GenerationStatus, MediaKind, Provenance,
};
use crate::plan::{DeliveryMode, ExecutionPlan};
use crate::processors::ocr::BoundaryGate;
use crate::spinner::Spinner;
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

/// Run every step of the plan. Text deltas stream live when the plan says
/// so; slice replies merge through the boundary gate so live and buffered
/// delivery end up byte-identical.
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
        adapter: plan.resolved.adapter,
    };
    let client = Client::new(&conn).map_err(AppError::from)?;

    let live_stdout = plan.delivery == DeliveryMode::Live
        && plan.destinations.contains(&Destination::Stdout)
        && plan.resolved.produce.contains(&MediaKind::Text);
    let sink = Rc::new(RefCell::new(DeltaSink {
        merged: String::new(),
        live: live_stdout,
        chars_seen: 0,
    }));
    let multi_step = plan.steps.len() > 1;
    let mut gate = multi_step.then(|| {
        let gate_sink = sink.clone();
        BoundaryGate::new(move |t: &str| gate_sink.borrow_mut().emit(t))
    });

    let spinner = if plan.quiet {
        Spinner::disabled()
    } else {
        Spinner::start(&format!("asking {}...", plan.resolved.model))
    };

    let mut media_artifacts: Vec<Artifact> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut overall = GenerationStatus::Complete;

    for step in &plan.steps {
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
        let request = GenerateRequest {
            instruction: (!plan.instruction.is_empty()).then_some(plan.instruction.as_str()),
            requirement: plan.requirement.as_deref(),
            inputs: &step.inputs,
            model: &plan.resolved.model,
            max_tokens: plan.resolved.max_tokens,
            temperature: plan.resolved.temperature,
            outputs: &plan.resolved.produce,
            options: &plan.resolved.options,
        };
        // Deltas flow through the merge gate when a run has several
        // requests; a single request goes straight to the sink.
        let mut on_delta = |delta: &str| {
            if let Some(gate) = gate.as_mut() {
                gate.push_delta(delta);
            } else {
                sink.borrow_mut().emit(delta);
            }
        };
        let result = if plan.transport_stream {
            client.generate_stream(&request, &mut on_delta).await
        } else {
            client.generate(&request).await
        };
        match result {
            Ok(reply) => {
                // A buffered reply arrives whole: run it through the same
                // path the deltas would take.
                if !plan.transport_stream && !reply.text.is_empty() {
                    on_delta(&reply.text.clone());
                }
                absorb(reply, &mut media_artifacts, &mut warnings, &mut overall);
                if let Some(gate) = gate.as_mut() {
                    gate.slice_end(step.hard_cut_end);
                }
            }
            Err(e) => {
                spinner.stop();
                return Err(AppError::from(e));
            }
        }
    }
    if let Some(gate) = gate.as_mut() {
        gate.finish();
    }
    spinner.set_progress(sink.borrow().chars_seen);
    spinner.stop();
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }

    // Promote the merged text to the text artifact.
    let mut artifacts: Vec<Artifact> = Vec::new();
    {
        let merged = sink.borrow();
        if !merged.merged.is_empty() {
            artifacts.push(Artifact {
                id: "text".into(),
                kind: MediaKind::Text,
                mime: "text/plain".into(),
                format: "text".into(),
                bytes: merged.merged.clone().into_bytes(),
                provenance: Provenance::Request { index: 0 },
            });
        }
    }
    for (i, artifact) in media_artifacts.into_iter().enumerate() {
        artifacts.push(Artifact {
            id: format!("{}-{}", artifact.kind, i + 1),
            provenance: Provenance::Request { index: 0 },
            ..artifact
        });
    }

    // Post-checks on what actually came back (the plan's expectations are
    // re-validated against the real response, contract §step 4.8).
    for kind in &plan.resolved.produce {
        if !artifacts.iter().any(|a| a.kind == *kind) {
            return Err(AppError::generation(format!(
                "the response did not produce the requested '{kind}' output"
            )));
        }
    }
    for (kind, expected) in &plan.expected_counts {
        if let Some(n) = expected {
            let have = artifacts.iter().filter(|a| a.kind == *kind).count() as u64;
            if have != *n {
                return Err(AppError::generation(format!(
                    "expected {n} {kind} artifact(s), got {have}"
                )));
            }
        }
    }
    if artifacts.is_empty() {
        return Err(AppError::generation(
            "the model returned no usable content for this run",
        ));
    }

    Ok(RunOutput {
        artifacts,
        status: overall,
        warnings,
        live_stdout,
    })
}

fn absorb(
    reply: GenerateResult,
    media_artifacts: &mut Vec<Artifact>,
    warnings: &mut Vec<String>,
    overall: &mut GenerationStatus,
) {
    warnings.extend(reply.warnings);
    media_artifacts.extend(reply.artifacts);
    if reply.status != GenerationStatus::Complete && *overall == GenerationStatus::Complete {
        *overall = reply.status;
    }
}

/// The exit-code kind for a run whose generation did not complete cleanly.
pub fn classify(output: &RunOutput) -> Option<ErrorKind> {
    if output.status.is_complete() {
        None
    } else {
        Some(ErrorKind::Generation)
    }
}

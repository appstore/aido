//! Live runs: a registry of in-flight executions. Each run gets its own
//! thread with a current-thread runtime — the runner is deliberately
//! single-threaded (its sinks are `Rc`), so it cannot live on an axum
//! worker — streams progress over a broadcast channel (the SSE feed),
//! and records through the same `assemble_record` → `save_generation` →
//! `prune` path the CLI's `finish_run` walks.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::{broadcast, watch};

use super::api::run_dto;
use crate::domain::{ErrorKind, GenerationStatus, RunRecord};
use crate::plan::ExecutionPlan;
use crate::runner::{self, EventSink, RunEvent};

/// One SSE frame. `Done` flattens the run's report — the same object
/// `GET /api/runs/{id}` answers with, plus an `error` the CLI would have
/// exited non-zero for (kind and message, no exit codes over HTTP).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SseEvent {
    Delta {
        text: String,
    },
    Step {
        done: usize,
        total: usize,
        label: String,
    },
    Warning {
        text: String,
    },
    Done {
        #[serde(flatten)]
        report: serde_json::Value,
    },
    Error {
        kind: String,
        message: String,
    },
    Cancelled {
        run_id: String,
    },
}

impl From<RunEvent> for SseEvent {
    fn from(event: RunEvent) -> Self {
        match event {
            RunEvent::Delta(text) => Self::Delta { text },
            RunEvent::Step { done, total, label } => Self::Step { done, total, label },
            RunEvent::Warning(text) => Self::Warning { text },
        }
    }
}

struct RunHandle {
    cancel: watch::Sender<bool>,
    events: broadcast::Sender<SseEvent>,
}

#[derive(Default)]
pub struct Runs {
    inner: Mutex<HashMap<String, RunHandle>>,
}

impl Runs {
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn register(&self, run_id: &str, handle: RunHandle) {
        self.inner
            .lock()
            .unwrap()
            .insert(run_id.to_string(), handle);
    }

    fn remove(&self, run_id: &str) {
        self.inner.lock().unwrap().remove(run_id);
    }

    /// Subscribe to a live run's events. The channel closes when the run
    /// ends (after its `done`/`error`/`cancelled` frame), which is the
    /// SSE stream's natural end.
    pub fn subscribe(&self, run_id: &str) -> Option<broadcast::Receiver<SseEvent>> {
        self.inner
            .lock()
            .unwrap()
            .get(run_id)
            .map(|handle| handle.events.subscribe())
    }

    /// Ask a live run to stop. True when the run was there to ask.
    pub fn cancel(&self, run_id: &str) -> bool {
        match self.inner.lock().unwrap().get(run_id) {
            Some(handle) => handle.cancel.send_if_modified(|asked| {
                if !*asked {
                    *asked = true;
                    true
                } else {
                    false
                }
            }),
            None => false,
        }
    }
}

/// Start a run: register it, then execute on a dedicated thread. The
/// caller answers `202 {run_id}` as soon as this returns.
pub fn spawn(plan: ExecutionPlan, runs: Arc<Runs>, run_id: String) {
    let (events, _) = broadcast::channel(256);
    let (cancel, cancelled) = watch::channel(false);
    runs.register(
        &run_id,
        RunHandle {
            cancel,
            events: events.clone(),
        },
    );
    let record_history = plan.record_history;
    let (keep, budget) = retention();
    let name = format!("aido-ui-run-{run_id}");
    // A spawn failure (thread exhaustion) still ends the feed: the entry
    // self-removes, subscribers see a closed channel.
    let cleanup = (runs.clone(), run_id.clone());
    let spawned = std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                runs.remove(&run_id);
                return;
            };
            let local = tokio::task::LocalSet::new();
            local.block_on(&runtime, async move {
                let sink: EventSink = {
                    let events = events.clone();
                    Rc::new(RefCell::new(move |event: RunEvent| {
                        let _ = events.send(event.into());
                    }))
                };
                // Cancelled mirrors the CLI's Ctrl+C: the future drops
                // mid-flight (the runner's own discipline keeps partial
                // text recoverable) and a cancelled record lands in
                // history, exactly as `record_cancelled` writes one.
                tokio::select! {
                    biased;
                    _ = await_cancel(cancelled) => {
                        if record_history {
                            let record = cancelled_record(&plan, &run_id);
                            best_effort_save(&record, false);
                        }
                        let _ = events.send(SseEvent::Cancelled { run_id: run_id.clone() });
                    }
                    output = runner::execute_with(&plan, Some(sink)) => match output {
                        Ok(output) => finish(&plan, output, &run_id, record_history, keep, budget, &events),
                        Err(e) => {
                            let _ = events.send(SseEvent::Error {
                                kind: e.kind.as_str().to_string(),
                                message: e.chain_inline(),
                            });
                        }
                    },
                }
                runs.remove(&run_id);
            });
        })
        .is_ok();
    if !spawned {
        cleanup.0.remove(&cleanup.1);
    }
}

async fn await_cancel(mut cancelled: watch::Receiver<bool>) {
    while cancelled.changed().await.is_ok() {
        if *cancelled.borrow() {
            return;
        }
    }
}

/// The retention bounds `finish_run` prunes with, re-read per run: the
/// config file is the truth, and a UI run must obey an edit made after
/// the server started.
fn retention() -> (usize, u64) {
    let cfg = crate::config::load().ok();
    (
        cfg.as_ref()
            .and_then(|c| c.settings.history_keep)
            .unwrap_or(crate::history::DEFAULT_KEEP),
        cfg.as_ref()
            .and_then(|c| c.settings.history_bytes)
            .unwrap_or(crate::history::DEFAULT_HISTORY_BYTES),
    )
}

/// Record, save and report a finished generation — `finish_run`'s
/// decisions without its delivery (the browser is the destination) and
/// without its exit codes (the HTTP status is always "the run happened";
/// `error` inside the frame says how it went).
fn finish(
    plan: &ExecutionPlan,
    output: runner::RunOutput,
    run_id: &str,
    record_history: bool,
    keep: usize,
    budget: u64,
    events: &broadcast::Sender<SseEvent>,
) {
    let assembled =
        crate::app::assemble_record(&plan.task.name, plan, &output, None, Vec::new(), run_id);
    if record_history {
        // finish_run's rule: a complete generation saves normally; an
        // incomplete one keeps whatever arrived when it is worth
        // keeping (validated artifacts of an unsatisfied run, or text
        // that streamed before a request died).
        let keep_artifacts = if assembled.record.generation.is_complete() {
            false
        } else {
            assembled.keep_artifacts
        };
        best_effort_save(&assembled.record, keep_artifacts);
        history_prune(keep, budget);
    }
    let mut report = run_dto(&assembled.record);
    if let Some(error) = error_of(&assembled, &output) {
        report["error"] = error;
    }
    let _ = events.send(SseEvent::Done { report });
}

/// The failure the CLI would have exited non-zero for, as data: the
/// generation that did not complete or satisfy the request, or a
/// batch's partial failure.
fn error_of(
    assembled: &crate::app::AssembledRun,
    output: &runner::RunOutput,
) -> Option<serde_json::Value> {
    let record = &assembled.record;
    if !record.generation.is_complete() {
        let reason = match &record.generation {
            GenerationStatus::Incomplete { reason } => format!(" ({reason})"),
            _ => String::new(),
        };
        let message = if assembled.unsatisfied.is_some() && output.failure.is_none() {
            format!(
                "the generation did not satisfy the request{reason}; the result is not delivered"
            )
        } else {
            format!("the generation did not complete{reason}; the result is not delivered")
        };
        let kind = output
            .failure
            .as_ref()
            .map(|f| f.kind)
            .unwrap_or(ErrorKind::Generation);
        return Some(serde_json::json!({
            "kind": kind.as_str(),
            "message": message,
        }));
    }
    if assembled.batch_partial {
        let listed = output
            .failed_parts
            .iter()
            .map(|f| format!("{}: {}", f.name, f.error))
            .collect::<Vec<_>>()
            .join("; ");
        return Some(serde_json::json!({
            "kind": "partial",
            "message": format!(
                "{}/{} input part(s) failed; the rest were delivered — {listed}",
                output.failed_parts.len(),
                output.parts_total.max(output.failed_parts.len()),
            ),
        }));
    }
    None
}

/// The record a stopped run leaves — `app::record_cancelled`'s shape: a
/// real run id, the plan's summary, nothing delivered.
fn cancelled_record(plan: &ExecutionPlan, run_id: &str) -> RunRecord {
    RunRecord {
        run_id: run_id.to_string(),
        task: Some(plan.task.name.clone()),
        created_at: crate::app::now_iso(),
        summary: crate::plan::summarize(plan),
        generation: GenerationStatus::Cancelled,
        artifacts: Vec::new(),
        warnings: Vec::new(),
        failed_parts: Vec::new(),
        parts_total: 0,
        deliveries: Vec::new(),
        stages: Vec::new(),
        last_stage_len: 0,
    }
}

fn best_effort_save(record: &RunRecord, keep_artifacts: bool) {
    if let Err(e) = crate::history::save_generation(record, keep_artifacts) {
        eprintln!("warning: failed to record the run: {e:#}");
    }
}

fn history_prune(keep: usize, budget: u64) {
    crate::history::prune(keep, budget);
}

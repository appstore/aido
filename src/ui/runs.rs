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
        part: Option<String>,
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
            RunEvent::Step {
                done,
                total,
                label,
                part,
            } => Self::Step {
                done,
                total,
                label,
                part,
            },
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

    /// Ask a live run to stop. True when the run was there to ask —
    /// asking twice is fine (the run is already stopping), so this is
    /// idempotent while the entry exists.
    pub fn cancel(&self, run_id: &str) -> bool {
        match self.inner.lock().unwrap().get(run_id) {
            Some(handle) => {
                let _ = handle.cancel.send_if_modified(|asked| {
                    let first = !*asked;
                    *asked = true;
                    first
                });
                true
            }
            None => false,
        }
    }
}

/// The run-thread skeleton: register the run, hand its event channel
/// and cancel receiver to a dedicated thread with its own current-thread
/// runtime (the runner is Rc-singly-threaded by design), and always
/// deregister at the end — a closed channel is the subscriber's natural
/// end. A spawn failure (thread exhaustion) deregisters the same way.
fn spawn_run_thread<F, Fut>(runs: &Arc<Runs>, run_id: &str, name: String, body: F)
where
    F: FnOnce(broadcast::Sender<SseEvent>, watch::Receiver<bool>, Arc<Runs>, String) -> Fut
        + Send
        + 'static,
    Fut: std::future::Future<Output = ()>,
{
    let (events, _) = broadcast::channel(256);
    let (cancel, cancelled) = watch::channel(false);
    runs.register(
        run_id,
        RunHandle {
            cancel,
            events: events.clone(),
        },
    );
    // Panic-safe cleanup: a panic inside the run body (or a runtime
    // build failure) unwinds through this guard, so the registry entry
    // never leaks and subscribers always see their channel close.
    struct RemoveOnDrop(Arc<Runs>, String);
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            self.0.remove(&self.1);
        }
    }
    let cleanup = (runs.clone(), run_id.to_string());
    let runs = runs.clone();
    let owned_id = run_id.to_string();
    let spawned = std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            let _guard = RemoveOnDrop(runs.clone(), owned_id.clone());
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            let local = tokio::task::LocalSet::new();
            local.block_on(&runtime, body(events, cancelled, runs, owned_id));
        })
        .is_ok();
    if !spawned {
        cleanup.0.remove(&cleanup.1);
    }
}

/// Start a run: register it, then execute on a dedicated thread. The
/// caller answers `202 {run_id}` as soon as this returns. `deliver` is
/// the request's own server-side delivery intent (`out_dir`/`out_file`
/// in the whitelist) — a plan may carry a Directory destination without
/// it (the per-part batch placeholder), which must never be written.
pub fn spawn(plan: ExecutionPlan, runs: Arc<Runs>, run_id: String, deliver: bool) {
    let record_history = plan.record_history;
    let (keep, budget) = retention();
    let name = format!("aido-ui-run-{run_id}");
    spawn_run_thread(
        &runs,
        &run_id,
        name,
        move |events, cancelled, runs, run_id| async move {
            let created_at = crate::app::now_iso();
            let ctx = RunContext {
                run_id: run_id.clone(),
                record_history,
                keep,
                budget,
                deliver,
                events: events.clone(),
            };
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
                        let record = cancelled_record(&plan, &run_id, &created_at);
                        best_effort_save(&record, false);
                    }
                    let _ = events.send(SseEvent::Cancelled { run_id: run_id.clone() });
                }
                output = runner::execute_with(&plan, Some(sink)) => match output {
                    Ok(output) => {
                        finish(&plan.task.name, &plan, output, None, Vec::new(), &ctx);
                    }
                    Err(e) => {
                        let _ = events.send(SseEvent::Error {
                            kind: e.kind.as_str().to_string(),
                            message: e.chain_inline(),
                        });
                    }
                },
            }
            runs.remove(&run_id);
        },
    );
}

/// Start a chain run: the same registry and thread, the chain's own
/// executor with the sink threaded through every stage. The
/// interrupted-run placeholder mirrors `run_chain`'s: `on_started` and
/// `on_progress` snapshot what has been paid for, so a cancel mid-chain
/// records the earlier stages' artifacts instead of losing them.
pub fn spawn_chain(
    prepared: crate::chain::PreparedChain,
    runs: Arc<Runs>,
    run_id: String,
    record_history: bool,
) {
    let (keep, budget) = retention();
    let name = format!("aido-ui-chain-{run_id}");
    spawn_run_thread(
        &runs,
        &run_id,
        name,
        move |events, cancelled, runs, run_id| async move {
            let created_at = crate::app::now_iso();
            let ctx = RunContext {
                run_id: run_id.clone(),
                record_history,
                keep,
                budget,
                // Chains deliver nothing server-side in v1: the last
                // stage's artifacts reach the browser through history.
                deliver: false,
                events: events.clone(),
            };
            let sink: EventSink = {
                let events = events.clone();
                Rc::new(RefCell::new(move |event: RunEvent| {
                    let _ = events.send(event.into());
                }))
            };
            let task_label = prepared.task_label();
            // What the chain has paid for so far — the cancelled record's
            // content, updated by the same callbacks run_chain feeds its
            // Ctrl+C placeholder.
            let snapshot = Rc::new(RefCell::new(ChainSnapshot::default()));
            let started = snapshot.clone();
            let mut on_started = move |summary: crate::domain::RunSummary| {
                started.borrow_mut().summary = Some(summary);
            };
            let progressed = snapshot.clone();
            let mut on_progress =
                move |artifacts: &[crate::domain::Artifact],
                      stages: &[crate::domain::RunSummary]| {
                    let mut snapshot = progressed.borrow_mut();
                    snapshot.artifacts = artifacts.to_vec();
                    snapshot.stages = stages.to_vec();
                };
            let terminal = crate::plan::TerminalInfo {
                stdin: false,
                stdout: false,
                stderr: false,
            };
            let mut empty = std::io::empty();
            let mut probe = || false;
            let mut no_clipboard = || {
                Err(anyhow::anyhow!(
                    "the clipboard is not available to the web UI; paste it as a file"
                ))
            };
            let mut env = crate::input::InputEnv::custom(&mut empty, &mut probe, &mut no_clipboard);
            let cfg = match crate::config::load() {
                Ok(cfg) => cfg,
                Err(e) => {
                    let _ = events.send(SseEvent::Error {
                        kind: "usage".into(),
                        message: format!("{e:#}"),
                    });
                    runs.remove(&run_id);
                    return;
                }
            };
            tokio::select! {
                biased;
                _ = await_cancel(cancelled) => {
                    if record_history {
                        let record = snapshot.borrow().cancelled_record(&run_id, &task_label, &created_at);
                        let kept = !record.artifacts.is_empty();
                        best_effort_save(&record, kept);
                    }
                    let _ = events.send(SseEvent::Cancelled { run_id: run_id.clone() });
                }
                run = crate::chain::execute_with(
                    &prepared, &cfg, terminal, &mut env, &mut on_started, &mut on_progress, Some(sink),
                ) => match run {
                    Ok(run) => finish(
                        &task_label,
                        &run.plan,
                        run.output,
                        Some(run.deliverable_start),
                        run.stage_summaries,
                        &ctx,
                    ),
                    Err(e) => {
                        let _ = events.send(SseEvent::Error {
                            kind: e.kind.as_str().to_string(),
                            message: e.chain_inline(),
                        });
                    }
                },
            }
            runs.remove(&run_id);
        },
    );
}

/// A chain in flight: what a cancel would record right now.
#[derive(Default)]
struct ChainSnapshot {
    summary: Option<crate::domain::RunSummary>,
    artifacts: Vec<crate::domain::Artifact>,
    stages: Vec<crate::domain::RunSummary>,
}

impl ChainSnapshot {
    /// `app::cancelled_record`'s chain shape: the paid-for artifacts
    /// travel into the record, kept when there are any.
    fn cancelled_record(
        &self,
        run_id: &str,
        task_label: &str,
        created_at: &str,
    ) -> crate::domain::RunRecord {
        let kept = !self.artifacts.is_empty();
        crate::domain::RunRecord {
            run_id: run_id.to_string(),
            task: Some(task_label.to_string()),
            created_at: created_at.to_string(),
            summary: self.summary.clone().unwrap_or_default(),
            generation: GenerationStatus::Cancelled,
            artifacts: self.artifacts.clone(),
            warnings: vec![if kept {
                "cancelled from the web UI; the completed stages' artifacts are kept in this record"
                    .into()
            } else {
                "cancelled from the web UI; nothing was delivered".into()
            }],
            failed_parts: Vec::new(),
            parts_total: 0,
            deliveries: Vec::new(),
            stages: self.stages.clone(),
            last_stage_len: 0,
        }
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

/// What a finishing run needs from its thread: its identity, its
/// history policy, its delivery intent, and its event feed.
struct RunContext {
    run_id: String,
    record_history: bool,
    keep: usize,
    budget: u64,
    deliver: bool,
    events: broadcast::Sender<SseEvent>,
}

/// Record, save and report a finished generation — `finish_run`'s
/// decisions without its stdout/clipboard delivery (the browser is the
/// destination) and without its exit codes (the HTTP status is always
/// "the run happened"; `error` inside the frame says how it went). A
/// chain passes its deliverable split and per-stage summaries, exactly
/// as `finish_run` receives them from `run_chain`. A run the request
/// gave a whitelisted `out_dir`/`out_file` delivers through the same
/// `output::deliver` the CLI walks, and its per-destination states ride
/// the record and the report.
fn finish(
    task_label: &str,
    plan: &ExecutionPlan,
    output: runner::RunOutput,
    deliverable_start: Option<usize>,
    stage_summaries: Vec<crate::domain::RunSummary>,
    ctx: &RunContext,
) {
    let (record_history, keep, budget, events) =
        (ctx.record_history, ctx.keep, ctx.budget, &ctx.events);
    let assembled = crate::app::assemble_record(
        task_label,
        plan,
        &output,
        deliverable_start,
        stage_summaries,
        &ctx.run_id,
    );
    let mut record = assembled.record;
    if record_history {
        // finish_run's rule: a complete generation saves normally; an
        // incomplete one keeps whatever arrived when it is worth
        // keeping (validated artifacts of an unsatisfied run, a chain's
        // upstream artifacts, or text that streamed before a request
        // died).
        let keep_artifacts = if record.generation.is_complete() {
            false
        } else {
            assembled.keep_artifacts
        };
        best_effort_save(&record, keep_artifacts);
    }
    // Server-side delivery, only when the request asked for it (the
    // plan alone cannot say: the per-part batch placeholder is also a
    // Directory destination, and it must never be written). The same
    // call `finish_run` makes — quiet, never json, so nothing reaches
    // the server's own stdout.
    let mut saved = std::collections::BTreeMap::new();
    let mut delivery_error: Option<crate::domain::AppError> = None;
    if ctx.deliver {
        let hold_secs = crate::config::load()
            .ok()
            .and_then(|c| c.settings.hold_secs)
            .unwrap_or(crate::config::DEFAULT_HOLD_SECS);
        let failed_parts: Vec<(String, String)> = output
            .failed_parts
            .iter()
            .map(|f| (f.name.clone(), f.error.clone()))
            .collect();
        let args = crate::output::DeliverArgs {
            artifacts: &record.artifacts,
            produce: &plan.resolved.produce,
            destinations: &plan.destinations,
            overwrite: false,
            live_stdout: false,
            hold_secs,
            quiet: true,
            json: false,
            run_id: &ctx.run_id,
            task: Some(task_label),
            failed_parts: &failed_parts,
            dir_extras: &[],
        };
        let outcome = crate::output::deliver(&args);
        record.deliveries = outcome.states;
        if record_history {
            best_effort(
                crate::history::update_deliveries(&record),
                "failed to update the run record",
            );
        }
        saved = outcome.saved;
        delivery_error = outcome.error;
    }
    if record_history {
        history_prune(keep, budget);
    }
    let mut report = run_dto(&record);
    if !saved.is_empty() {
        report["saved"] = serde_json::json!(saved
            .iter()
            .map(|(k, v)| (k.clone(), v.display().to_string()))
            .collect::<std::collections::BTreeMap<String, String>>());
    }
    if let Some(error) = error_of(
        &record,
        assembled.unsatisfied.as_deref(),
        assembled.batch_partial,
        &output,
    ) {
        report["error"] = error;
    } else if let Some(e) = delivery_error {
        // The CLI exits 5 here with successes kept; over HTTP the run
        // still "happened" — the error says what did not land.
        report["error"] = serde_json::json!({
            "kind": e.kind.as_str(),
            "message": e.chain_inline(),
        });
    }
    let _ = events.send(SseEvent::Done { report });
}

/// The failure the CLI would have exited non-zero for, as data: the
/// generation that did not complete or satisfy the request, or a
/// batch's partial failure.
fn error_of(
    record: &RunRecord,
    unsatisfied: Option<&str>,
    batch_partial: bool,
    output: &runner::RunOutput,
) -> Option<serde_json::Value> {
    if !record.generation.is_complete() {
        let reason = match &record.generation {
            GenerationStatus::Incomplete { reason } => format!(" ({reason})"),
            _ => String::new(),
        };
        let message = if unsatisfied.is_some() && output.failure.is_none() {
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
    if batch_partial {
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

/// The record a stopped run leaves — `app::cancelled_record`'s shape: a
/// real run id, the plan's summary, a warning saying what stopped it,
/// nothing delivered. `created_at` is the run's start, not the cancel
/// moment — a cancelled record must sort where the run began, exactly
/// like the CLI's PendingRun keeps it.
fn cancelled_record(plan: &ExecutionPlan, run_id: &str, created_at: &str) -> RunRecord {
    RunRecord {
        run_id: run_id.to_string(),
        task: Some(plan.task.name.clone()),
        created_at: created_at.to_string(),
        summary: crate::plan::summarize(plan),
        generation: GenerationStatus::Cancelled,
        artifacts: Vec::new(),
        warnings: vec!["cancelled from the web UI; nothing was delivered".into()],
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

fn best_effort(result: anyhow::Result<()>, warning: &str) {
    if let Err(e) = result {
        eprintln!("warning: {warning}: {e:#}");
    }
}

fn history_prune(keep: usize, budget: u64) {
    crate::history::prune(keep, budget);
}

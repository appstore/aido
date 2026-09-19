//! Application orchestration: dispatch a normalized invocation, run one
//! task end to end, and map outcomes to exit codes.

use crate::cli::{self, Cli, Commands, ConfigCmd, HistoryCmd, Normalized, SourceSpec, TasksCmd};
use crate::config;
use crate::domain::{
    first_line, AppError, AppResult, Destination, ErrorKind, GenerationStatus, MediaKind,
    RunRecord, RunSummary,
};
use crate::history::{self, civil_from_days};
use crate::input::InputEnv;
use crate::output::{self, DeliverArgs};
use crate::plan::{self, TerminalInfo};
use crate::runner;
use crate::tasks;
use anyhow::Result;
use clap::Parser as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

/// Exit code for "the user pressed Ctrl+C".
pub const EXIT_CANCEL: i32 = 130;

/// Why a run was cancelled: the history record names the signal that
/// actually arrived instead of blaming Ctrl+C for everything.
enum CancelReason {
    CtrlC,
    Sigterm,
}

impl CancelReason {
    fn history_warning(self) -> &'static str {
        match self {
            Self::CtrlC => "interrupted by Ctrl+C",
            Self::Sigterm => "interrupted by SIGTERM",
        }
    }
}

/// What the Ctrl+C handler needs to leave an honest trace of an
/// interrupted run: the run was started (a history dir may exist) but no
/// result ever existed.
struct PendingRun {
    run_id: String,
    task: Option<String>,
    created_at: String,
    summary: RunSummary,
    record_history: bool,
}

/// Shared run state. The interrupted-run placeholder is claimed once the
/// outcome exists — a later Ctrl+C must not overwrite a real record with
/// an empty cancelled one — while the run identity stays until the end:
/// a failure after the generation ran still names the run it belongs to.
/// The watch daemon holds a handle to the same state so a Ctrl+C that
/// interrupts a watched file records that file's run as cancelled.
#[derive(Default)]
pub(crate) struct RunState {
    pending: Option<PendingRun>,
    identity: Option<(String, Option<String>)>,
}

pub async fn run() -> i32 {
    // A normalize error fires before clap ever parses, so the --json
    // decision starts as an arity-aware argv scan (a "--json" that is
    // some flag's value is not a request); once parsing succeeded, the
    // parsed flag overrides it.
    let wants_json = cli::argv_wants_json();
    let argv: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let normalized = match cli::normalize(argv) {
        Ok(n) => n,
        Err(e) => return fail(&AppError::usage(format!("{e:#}")), wants_json, None, None),
    };
    let cli = match Cli::try_parse_from(
        std::iter::once(std::ffi::OsString::from("aido")).chain(normalized.argv.clone()),
    ) {
        Ok(cli) => cli,
        Err(e) => {
            // A parse error exits here without ever reaching fail(), so
            // the --json contract needs the report emitted by hand; the
            // argv scan is the only signal available (clap never
            // produced a Cli). Help and version print to stdout and exit
            // 0 — no report for those.
            if e.use_stderr() && wants_json {
                let report = output::error_report(ErrorKind::Usage, &e.to_string(), None, None);
                let mut out = std::io::stdout().lock();
                let _ = serde_json::to_writer_pretty(&mut out, &report);
                let _ = out.write_all(b"\n");
                let _ = out.flush();
            }
            let _ = e.print();
            return if e.use_stderr() { 2 } else { 0 };
        }
    };
    let wants_json = cli.json;
    // Ctrl+C anywhere in a run cancels it (exit 130) instead of hanging on
    // a slow request or leaving a half-written delivery. The interrupted
    // run is recorded as cancelled when a plan had already been built.
    // SIGTERM takes the same path on Unix: a terminated daemon must leave
    // the same honest trace a Ctrl+C does.
    let state: Arc<std::sync::Mutex<RunState>> = Arc::default();
    let result = tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => {
            record_cancelled(&state, CancelReason::CtrlC);
            return EXIT_CANCEL;
        }
        _ = sigterm() => {
            record_cancelled(&state, CancelReason::Sigterm);
            return EXIT_CANCEL;
        }
        result = dispatch(cli, normalized, state.clone()) => result,
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            // The run identity is known once a plan was built; earlier
            // failures have neither a run id nor a task.
            let identity = state.lock().ok().and_then(|s| s.identity.clone());
            let (run_id, task) = match &identity {
                Some((id, task)) => (Some(id.as_str()), task.as_deref()),
                None => (None, None),
            };
            fail(&e, wants_json, run_id, task)
        }
    }
}

fn record_cancelled(state: &std::sync::Mutex<RunState>, reason: CancelReason) {
    let claimed = state.lock().ok().and_then(|mut s| s.pending.take());
    match claimed {
        Some(p) => {
            eprintln!("interrupted — the run was cancelled, nothing was delivered");
            if p.record_history {
                let record = RunRecord {
                    run_id: p.run_id,
                    task: p.task,
                    created_at: p.created_at,
                    summary: p.summary,
                    generation: GenerationStatus::Cancelled,
                    artifacts: Vec::new(),
                    warnings: vec![reason.history_warning().into()],
                    failed_parts: Vec::new(),
                    parts_total: 0,
                    deliveries: Vec::new(),
                };
                best_effort(
                    history::save_generation(&record, false),
                    "failed to record the interrupted run",
                );
            }
        }
        None => eprintln!("interrupted"),
    }
}

/// SIGTERM joins Ctrl+C (exit 130) on Unix: same cleanup, same exit code.
/// Registration failure parks the future forever so the select still
/// works; the platform simply keeps its default disposition then.
async fn sigterm() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            term.recv().await;
            return;
        }
    }
    std::future::pending::<()>().await;
}

fn fail(e: &AppError, json: bool, run_id: Option<&str>, task: Option<&str>) -> i32 {
    let _ = std::io::stdout().flush();
    // Delivery and partial failures already carry the full JSON run report
    // printed by the delivery path (exit 5/6); another report here would
    // append a second JSON document to stdout. Every other failure class
    // has no report yet — with --json, emit the shared error envelope so
    // script callers get one report on every exit code.
    if json
        && matches!(
            e.kind,
            ErrorKind::Usage | ErrorKind::Service | ErrorKind::Generation
        )
    {
        let report = output::error_report(e.kind, &e.chain(), run_id, task);
        let mut out = std::io::stdout().lock();
        let _ = serde_json::to_writer_pretty(&mut out, &report);
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    }
    eprintln!("error: {}", e.chain());
    e.kind.exit_code()
}

async fn dispatch(
    cli: Cli,
    normalized: Normalized,
    state: Arc<std::sync::Mutex<RunState>>,
) -> AppResult<()> {
    // Management subcommands.
    match &cli.command {
        Some(Commands::Tasks { cmd }) => return manage_tasks(cmd),
        Some(Commands::Profiles) => return manage_profiles(),
        Some(Commands::Config { cmd }) => return manage_config(cmd),
        Some(Commands::History { cmd }) => return manage_history(&cli, cmd).await,
        Some(Commands::Hold { image, secs }) => return run_hold(*image, *secs).await,
        None => {}
    }

    warn_deprecated_env_vars();

    // The watch daemon owns everything after its own flags: it re-enters
    // this pipeline once per arriving file instead of running one here.
    if let Some(args) = normalized.watch.clone() {
        return crate::watch::run(&cli, args, state).await;
    }

    let Some(task_name) = normalized.task.or_else(|| cli.task.clone()) else {
        print_help();
        return Err(AppError::usage(
            "nothing to do: name a task (aido <TASK>), or ask with -p (see --help)",
        ));
    };

    if task_name == cli::LAST_TASK {
        return run_last(&cli).await;
    }

    run_task(&cli, task_name, normalized.specs.clone(), &state, None).await
}

fn warn_deprecated_env_vars() {
    for var in config::DEPRECATED_ENV_VARS {
        if std::env::var(var)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            eprintln!(
                "warning: {var} no longer overrides anything; set the value on the \
                 profile's provider in the config instead"
            );
        }
    }
}

/// `ask` is nothing without its instruction. Both the single-run path and
/// the watch precheck refuse it up front — a watch that starts without a
/// prompt would fail every arriving file.
pub(crate) fn require_ask_prompt(cli: &Cli, task_name: &str) -> AppResult<()> {
    if task_name == "ask" && cli.prompt.as_deref().is_none_or(str::is_empty) {
        return Err(AppError::usage(
            "`ask` needs -p with the instruction, e.g. `aido ask -p \"summarize this\" file.md`",
        ));
    }
    Ok(())
}

/// One task run, end to end: resolve, plan, precheck, execute, record,
/// deliver. The single-run path and every watched file share it — a
/// watched file is just an ordinary run whose input arrives later.
/// `stem_hint` (watch only) names the artifacts after the arriving file
/// (full name plus a short hash, so no two arrivals collide) so one
/// `--out-dir` can collect a stream of results.
pub(crate) async fn run_task(
    cli: &Cli,
    task_name: String,
    specs: Vec<SourceSpec>,
    state: &Arc<std::sync::Mutex<RunState>>,
    stem_hint: Option<&str>,
) -> AppResult<()> {
    require_ask_prompt(cli, &task_name)?;

    let task = tasks::get(&task_name).map_err(|e| AppError::usage(format!("{e:#}")))?;
    let cfg = config::load().map_err(|e| AppError::usage(format!("{e:#}")))?;
    let terminal = TerminalInfo::real();
    let mut env = InputEnv::real();

    let mut plan = plan::build(cli, &task, &specs, &cfg, terminal, &mut env)?;

    // Watched runs derive artifact names from the input file (shot.png →
    // shot-txt--<hash>.txt; see watch::watch_artifact_stem), so two
    // arrivals never fight over one `text.txt`. Steps that already carry
    // a stem (per-part batches) keep theirs.
    if let Some(stem) = stem_hint {
        for step in &mut plan.steps {
            if step.artifact_stem.is_none() {
                step.artifact_stem = Some(stem.to_string());
            }
        }
    }

    if cli.dry_run {
        print!("{}", plan::describe(&plan));
        return Ok(());
    }

    // A knowable `-o` collision fails before the request: the plan names
    // the file exactly, so this is usage (exit 2), not a paid delivery
    // failure (exit 5).
    output::precheck_file_targets(&plan.destinations, cli.overwrite)?;

    // The run id exists before the request so a Ctrl+C mid-flight can be
    // recorded. With history on the run dir is created exclusively here;
    // with history off nothing is created.
    let record_history = plan.record_history;
    let run_id = if record_history {
        history::new_run_id()
    } else {
        history::stamp_now()
    };
    if let Ok(mut state) = state.lock() {
        state.identity = Some((run_id.clone(), Some(task.name.clone())));
        state.pending = Some(PendingRun {
            run_id: run_id.clone(),
            task: Some(task.name.clone()),
            created_at: now_iso(),
            summary: plan::summarize(&plan),
            record_history,
        });
    }

    // Run: the runner streams, merges and assembles artifacts.
    let output = runner::execute(&plan).await?;
    // The outcome now exists and is recorded below; a Ctrl+C from here on
    // (during the save or the delivery) must not overwrite that record
    // with an empty cancelled placeholder. The identity stays: the error
    // report still names the run it belongs to.
    if let Ok(mut state) = state.lock() {
        state.pending = None;
    }

    // A generation that finished cleanly but did not satisfy the request
    // (missing kind, short count) is recorded, clearly marked as
    // incomplete, and not delivered.
    let unsatisfied = output.unsatisfied_reason(&plan);
    let unsatisfied = if output.artifacts.is_empty() && !output.failed_parts.is_empty() {
        Some(format!(
            "all {} input part(s) failed — first error: {}",
            output.failed_parts.len(),
            output.failed_parts[0].error
        ))
    } else {
        unsatisfied
    };
    // In a batch with surviving parts, part failures live in the record's
    // warnings and surface as exit 6 after normal delivery; they do not
    // taint the survivors' generation.
    let batch_partial = !output.failed_parts.is_empty() && !output.artifacts.is_empty();
    let generation = if batch_partial {
        GenerationStatus::Complete
    } else {
        match (&output.status, &unsatisfied) {
            (GenerationStatus::Complete, Some(reason)) => GenerationStatus::Incomplete {
                reason: reason.clone(),
            },
            (status, _) => status.clone(),
        }
    };

    // Truncated or otherwise incomplete generations are recorded but not
    // delivered (what streamed live already cannot be taken back).
    let mut record = RunRecord {
        run_id: run_id.clone(),
        task: Some(task.name.clone()),
        created_at: now_iso(),
        summary: plan::summarize(&plan),
        generation,
        artifacts: output.artifacts.clone(),
        warnings: output.warnings.clone(),
        failed_parts: output
            .failed_parts
            .iter()
            .map(|f| (f.name.clone(), f.error.clone()))
            .collect(),
        parts_total: output.parts_total,
        deliveries: Vec::new(),
    };
    if !record.generation.is_complete() {
        if plan.record_history {
            // `output.status` is Complete for an unsatisfied generation:
            // its validated artifacts stay in the record's directory
            // instead of being dropped. A run whose requests died mid-way
            // keeps the same promise for whatever text did arrive — the
            // merged replies, or a reduce run's collected map replies as
            // intermediate artifacts — but a failure before any text
            // arrived records metadata only, exactly like a truncated
            // stream.
            let keep_artifacts = output.status.is_complete()
                || (output.failure.is_some() && !output.artifacts.is_empty());
            best_effort(
                history::save_generation(&record, keep_artifacts),
                "failed to record the run",
            );
            // Text that already streamed live cannot be taken back; point
            // the user at the record that now holds it. `live_chars` says
            // what actually reached the terminal: a reduce run streams no
            // map reply (its kept intermediates were buffered), and a
            // truncated stream records metadata only (no `failure`, no
            // warning).
            if output.live_stdout
                && output.failure.is_some()
                && output.steps_done > 0
                && output.live_chars > 0
            {
                eprintln!(
                    "warning: the first {}/{} parts already streamed to stdout; the full record is in `aido history show {}`",
                    output.steps_done, output.steps_total, run_id
                );
            }
        }
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
        if let Some(failure) = output.failure.as_ref() {
            // The failed request's own class decides the exit code (a 500
            // is a service error, 3) instead of a blanket generation
            // failure; only a failure-less incompleteness stays exit 4.
            return Err(AppError::new(failure.kind, message));
        }
        return Err(AppError::generation(message));
    }

    // Save before delivery: a generation is recoverable even when every
    // destination fails.
    if plan.record_history {
        best_effort(
            history::save_generation(&record, false),
            "failed to record the run",
        );
    }

    let hold_secs = cfg.settings.hold_secs.unwrap_or(config::DEFAULT_HOLD_SECS);
    let failed_parts: Vec<(String, String)> = output
        .failed_parts
        .iter()
        .map(|f| (f.name.clone(), f.error.clone()))
        .collect();
    let deliver_args = DeliverArgs {
        artifacts: &output.artifacts,
        produce: &plan.resolved.produce,
        destinations: &plan.destinations,
        overwrite: cli.overwrite,
        live_stdout: output.live_stdout,
        hold_secs,
        quiet: cli.quiet,
        json: cli.json,
        run_id: &run_id,
        task: Some(&task.name),
        failed_parts: &failed_parts,
    };
    // The outcome keeps every destination's real state, on success and on
    // failure alike: partial deliveries are the recoverable path when a
    // later destination (say, the clipboard) failed.
    let outcome = output::deliver(&deliver_args);
    record.deliveries = outcome.states;
    if plan.record_history {
        best_effort(
            history::update_deliveries(&record),
            "failed to update the run record",
        );
        history::prune(
            cfg.settings.history_keep.unwrap_or(history::DEFAULT_KEEP),
            cfg.settings
                .history_bytes
                .unwrap_or(history::DEFAULT_HISTORY_BYTES),
        );
    }
    match outcome.error {
        Some(e) => Err(e),
        None => {
            // Every destination got its artifact; the non-zero exit is
            // only about the parts that failed along the way.
            if batch_partial {
                let listed = output
                    .failed_parts
                    .iter()
                    .map(|f| format!("{}: {}", f.name, f.error))
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(AppError::partial(format!(
                    "{}/{} input part(s) failed; the rest were delivered — {}",
                    output.failed_parts.len(),
                    output.parts_total.max(output.failed_parts.len()),
                    listed
                )));
            }
            Ok(())
        }
    }
}

fn best_effort(result: Result<()>, what: &str) {
    if let Err(e) = result {
        eprintln!("warning: {what}: {e:#}");
    }
}

fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let tod = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

fn print_help() {
    let mut cmd = <Cli as clap::CommandFactory>::command();
    cmd.print_help().ok();
}

// ---------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------

/// Output choices for redelivering a recorded run — the shared surface of
/// `aido last` and `aido history show`.
struct RestoreOptions {
    output: Option<PathBuf>,
    out_dir: Option<PathBuf>,
    stdout: bool,
    json: bool,
    copy: bool,
    overwrite: bool,
    quiet: bool,
}

impl RestoreOptions {
    fn from_cli(cli: &Cli) -> Self {
        Self {
            output: cli.output.clone(),
            out_dir: cli.out_dir.clone(),
            stdout: cli.stdout,
            json: cli.json,
            copy: cli.copy,
            overwrite: cli.overwrite,
            quiet: cli.quiet,
        }
    }
}

async fn run_last(cli: &Cli) -> AppResult<()> {
    let Some(record) = history::last_complete().map_err(|e| AppError::usage(format!("{e:#}")))?
    else {
        return Err(AppError::usage(
            "no completed runs in history yet; only complete generations are recoverable",
        ));
    };
    deliver_restored(&RestoreOptions::from_cli(cli), record).await
}

async fn manage_history(cli: &Cli, cmd: &HistoryCmd) -> AppResult<()> {
    match cmd {
        HistoryCmd::List => {
            let ids = history::list_ids().map_err(|e| AppError::usage(format!("{e:#}")))?;
            if ids.is_empty() {
                println!("no runs recorded yet");
                return Ok(());
            }
            let width = ids.len().to_string().len();
            // Oldest first, so the newest row lands at the bottom of the
            // terminal where the reader is looking (issue #64). The printed
            // number is still what `history show` takes as its operand:
            // 1 = newest, exactly the last row printed.
            for (i, id) in ids.iter().enumerate() {
                let n = ids.len() - i;
                // Manifest-only: the list labels runs without reading
                // their artifact bytes back.
                match history::load_meta(id) {
                    Ok(Some(meta)) => {
                        let mut label = generation_label(&meta.generation);
                        if meta.failed_parts > 0 {
                            label.push_str(&format!(
                                "; {}/{} input part(s) failed",
                                meta.failed_parts,
                                meta.parts_total.max(meta.failed_parts)
                            ));
                        }
                        println!(
                            "{n:>width$}  {id}  {:<12} {label}",
                            meta.task.as_deref().unwrap_or("-")
                        );
                    }
                    Ok(None) => println!("{n:>width$}  {id}  (unreadable)"),
                    Err(e) => println!("{n:>width$}  {id}  (error: {e:#})"),
                }
            }
            Ok(())
        }
        HistoryCmd::Show { target } => {
            let record = resolve_run(target)?;
            if !record.generation.is_complete() {
                let kept = if record.artifacts.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; this run kept {} intermediate chunk result(s) in its history directory",
                        record.artifacts.len()
                    )
                };
                println!(
                    "run {}: generation {} — artifacts are not delivered for \
                     incomplete runs{kept}; try another index, or `aido last` for \
                     the newest complete run",
                    record.run_id,
                    generation_label(&record.generation)
                );
                return Ok(());
            }
            // The normalizer hoists flags ahead of the management words, so
            // clap assigned them to the top-level `Cli`; that is the single
            // source of truth here, exactly as for `last`.
            deliver_restored(&RestoreOptions::from_cli(cli), record).await
        }
    }
}

/// Resolve a `history show` operand: a 1-based index into `history list`
/// order (1 = the newest entry), a full run id, or a unique id prefix.
/// Run ids always contain '-' and '.', so an all-digit operand can only
/// be an index.
fn resolve_run(target: &str) -> AppResult<RunRecord> {
    let ids = history::list_ids().map_err(|e| AppError::usage(format!("{e:#}")))?;
    if ids.is_empty() {
        return Err(AppError::usage("no runs recorded yet"));
    }
    let id = if target.bytes().all(|b| b.is_ascii_digit()) {
        let Ok(index) = target.parse::<usize>() else {
            // Past usize is past any list length, so the same out-of-range
            // answer applies, quoting what the user typed.
            return Err(AppError::usage(format!(
                "no run #{target}; `aido history list` shows only {}",
                ids.len()
            )));
        };
        if index == 0 {
            return Err(AppError::usage(
                "run indexes start at 1 (the newest); see `aido history list`",
            ));
        }
        let Some(offset) = ids.len().checked_sub(index) else {
            return Err(AppError::usage(format!(
                "no run #{index}; `aido history list` shows only {}",
                ids.len()
            )));
        };
        ids[offset].clone()
    } else if ids.iter().any(|id| id == target) {
        target.to_string()
    } else {
        let matches: Vec<&String> = ids.iter().filter(|id| id.starts_with(target)).collect();
        match matches.as_slice() {
            [] => return Err(AppError::usage(format!("no run '{target}' in history"))),
            [only] => (*only).clone(),
            many => {
                return Err(AppError::usage(format!(
                    "'{target}' is ambiguous: it prefixes {} runs; add characters \
                     or see `aido history list`",
                    many.len()
                )));
            }
        }
    };
    history::load(&id)
        .map_err(|e| AppError::usage(format!("{e:#}")))?
        .ok_or_else(|| AppError::usage(format!("no run '{id}' in history")))
}

/// Restore delivers through the normal output system without touching the
/// service (or credentials).
async fn deliver_restored(options: &RestoreOptions, record: RunRecord) -> AppResult<()> {
    let produce: Vec<MediaKind> = record.artifacts.iter().map(|a| a.kind).collect();
    let destinations = restore_destinations(options, &record)?;
    output::precheck_file_targets(&destinations, options.overwrite)?;
    let hold_secs = config::load()
        .ok()
        .and_then(|c| c.settings.hold_secs)
        .unwrap_or(config::DEFAULT_HOLD_SECS);
    let args = DeliverArgs {
        artifacts: &record.artifacts,
        produce: &produce,
        destinations: &destinations,
        overwrite: options.overwrite,
        live_stdout: false,
        hold_secs,
        quiet: options.quiet,
        json: options.json,
        run_id: &record.run_id,
        task: record.task.as_deref(),
        // The restored report describes the run as it was: a partially
        // failed batch must not read as a full success here either.
        failed_parts: &record.failed_parts,
    };
    output::deliver(&args).result().map(|_| ())
}

fn restore_destinations(
    options: &RestoreOptions,
    record: &RunRecord,
) -> AppResult<Vec<Destination>> {
    let mut destinations: Vec<Destination> = Vec::new();
    if let Some(path) = &options.output {
        if path.as_os_str() == "-" {
            destinations.push(Destination::Stdout);
        } else {
            destinations.push(Destination::File { path: path.clone() });
        }
    }
    if let Some(dir) = &options.out_dir {
        destinations.push(Destination::Directory { path: dir.clone() });
    }
    if options.stdout {
        destinations.push(Destination::Stdout);
    }
    if options.copy {
        if record.artifacts.len() > 1 {
            return Err(AppError::usage(
                "the clipboard takes one artifact; use --out-dir to restore this run",
            ));
        }
        destinations.push(Destination::Clipboard);
    }
    if destinations.is_empty() {
        // Default stdout: binary on a terminal is refused, exactly as the
        // live plan refuses it — media goes to -o/--out-dir or a pipe.
        let binary = record.artifacts.iter().any(|a| a.kind != MediaKind::Text);
        if binary && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
            return Err(AppError::usage(
                "this run produced binary artifacts; use -o FILE or --out-dir, or pipe stdout",
            ));
        }
        destinations.push(Destination::Stdout);
    }
    Ok(destinations)
}

fn generation_label(status: &GenerationStatus) -> String {
    match status {
        GenerationStatus::Running => "running".into(),
        GenerationStatus::Complete => "complete".into(),
        GenerationStatus::Incomplete { reason } => format!("incomplete ({reason})"),
        GenerationStatus::Failed => "failed".into(),
        GenerationStatus::Cancelled => "cancelled".into(),
    }
}

// ---------------------------------------------------------------------------
// Management commands
// ---------------------------------------------------------------------------

/// How many chars of the instruction column `tasks list` shows: narrower
/// than a full report line (see plan::DRY_RUN_LINE_MAX) because the row
/// also carries the task name, operation and output types.
const TASKS_LIST_INSTRUCTION_MAX: usize = 64;

fn manage_tasks(cmd: &TasksCmd) -> AppResult<()> {
    match cmd {
        TasksCmd::List => {
            let all = tasks::load_all().map_err(|e| AppError::usage(format!("{e:#}")))?;
            for (name, task) in &all {
                println!(
                    "{name:<14} {} [{}, {}]",
                    first_line(&task.instruction, TASKS_LIST_INSTRUCTION_MAX),
                    task.operation,
                    task.output_types
                        .iter()
                        .map(|k| k.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                );
            }
            if let Some(dir) = tasks::tasks_dir() {
                eprintln!("\ncustom tasks: drop NAME.toml into {}", dir.display());
                eprintln!("run one with: aido <NAME> [INPUT...], or aido run <NAME>");
            }
            Ok(())
        }
        TasksCmd::Show { task } => {
            let all = tasks::load_all().map_err(|e| AppError::usage(format!("{e:#}")))?;
            let Some(task) = all.get(task) else {
                return Err(AppError::usage(format!(
                    "unknown task '{task}' (see `aido tasks list`)"
                )));
            };
            println!(
                "task:        {} {}",
                task.name,
                if task.builtin { "(builtin)" } else { "(user)" }
            );
            println!("operation:   {}", task.operation);
            if let Some(profile) = &task.profile {
                println!("profile:     {profile}");
            }
            if !task.instruction.is_empty() {
                println!("instruction:\n{}", indent(&task.instruction));
            }
            println!(
                "input types:  {}",
                task.input_types
                    .as_ref()
                    .map(|t| t
                        .iter()
                        .map(|k| k.to_string())
                        .collect::<Vec<_>>()
                        .join(","))
                    .unwrap_or_else(|| "any".into())
            );
            if !task.required_types.is_empty() {
                println!(
                    "requires:    {}",
                    task.required_types
                        .iter()
                        .map(|k| k.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                );
            }
            println!(
                "output types: {}",
                task.output_types
                    .iter()
                    .map(|k| k.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            );
            println!(
                "processor:   {}{}",
                processor_name(task.processor),
                if task.per_part { " (per-part)" } else { "" }
            );
            if !task.params.is_empty() {
                println!(
                    "parameters:  {}",
                    task.params
                        .iter()
                        .map(|p| format!("--{}", p.name()))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
            Ok(())
        }
    }
}

fn processor_name(kind: crate::tasks::ProcessorKind) -> &'static str {
    match kind {
        crate::tasks::ProcessorKind::Single => "single",
        crate::tasks::ProcessorKind::OcrTiles => "ocr-tiles",
        crate::tasks::ProcessorKind::ChunkJoin => "chunk-join",
        crate::tasks::ProcessorKind::ChunkReduce => "chunk-reduce",
    }
}

fn indent(s: &str) -> String {
    s.lines()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn manage_profiles() -> AppResult<()> {
    let cfg = config::load().map_err(|e| AppError::usage(format!("{e:#}")))?;
    let default = cfg
        .default_profile
        .clone()
        .unwrap_or_else(|| "default".into());
    for (name, profile) in &cfg.profiles {
        let provider = profile.provider.as_deref().unwrap_or("(unset)");
        let model = profile.model.as_deref().unwrap_or("(adapter default)");
        let operations = profile
            .operations
            .as_ref()
            .map(|ops| {
                ops.iter()
                    .map(|o| o.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_else(|| "any".into());
        println!(
            "{:<16} provider={:<10} model={:<22} operations={}",
            format!("{name}{}", if *name == default { " *" } else { "" }),
            provider,
            model,
            operations
        );
    }
    for (name, provider) in &cfg.providers {
        println!(
            "provider {name:<10} {} (key: {})",
            provider.base_url.as_deref().unwrap_or("(no base_url)"),
            provider.api_key_env.as_deref().unwrap_or("AIDO_API_KEY")
        );
    }
    Ok(())
}

fn manage_config(cmd: &ConfigCmd) -> AppResult<()> {
    match cmd {
        ConfigCmd::Init => config::init().map_err(|e| AppError::usage(format!("{e:#}"))),
        ConfigCmd::Check => {
            let cfg = config::load().map_err(|e| AppError::usage(format!("{e:#}")))?;
            let issues = config::check(&cfg);
            if issues.is_empty() {
                println!("config ok");
            } else {
                for issue in &issues {
                    println!("issue: {issue}");
                }
                return Err(AppError::usage(format!(
                    "{} config issue(s) found",
                    issues.len()
                )));
            }
            Ok(())
        }
    }
}

async fn run_hold(image: bool, secs: u64) -> AppResult<()> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::io::stdin()
        .read_to_end(&mut bytes)
        .map_err(|e| AppError::service(format!("cannot read stdin: {e}")))?;
    let mut cb = arboard::Clipboard::new()
        .map_err(|e| AppError::service(format!("cannot access the clipboard: {e}")))?;
    if image {
        crate::clipboard::set_image(&mut cb, &bytes)
            .map_err(|e| AppError::service(format!("{e:#}")))?;
    } else {
        cb.set_text(
            String::from_utf8(bytes)
                .map_err(|e| AppError::usage(format!("clipboard text must be valid UTF-8: {e}")))?,
        )
        .map_err(|e| AppError::service(format!("failed to write clipboard: {e}")))?;
    }
    // Keep the clipboard alive without blocking Ctrl+C while holding it.
    tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_dates_match_calendar() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20705), (2026, 9, 9));
    }
}

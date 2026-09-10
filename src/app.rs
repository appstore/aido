//! Application orchestration: dispatch a normalized invocation, run one
//! task end to end, and map outcomes to exit codes.

use crate::cli::{self, Cli, Commands, ConfigCmd, HistoryCmd, Normalized, TasksCmd};
use crate::config;
use crate::domain::{
    AppError, AppResult, DeliveryState, DeliveryStatus, Destination, GenerationStatus, MediaKind,
    RunRecord,
};
use crate::history;
use crate::input::InputEnv;
use crate::output::{self, DeliverArgs};
use crate::plan::{self, ExecutionPlan, TerminalInfo};
use crate::runner;
use crate::tasks;
use anyhow::Result;
use clap::Parser as _;
use std::io::Write as _;

/// Exit code for "the user pressed Ctrl+C".
pub const EXIT_CANCEL: i32 = 130;

pub async fn run() -> i32 {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let normalized = match cli::normalize(argv) {
        Ok(n) => n,
        Err(e) => return fail(&AppError::usage(format!("{e:#}"))),
    };
    let cli = match Cli::try_parse_from(
        std::iter::once(std::ffi::OsString::from("aido")).chain(normalized.argv.clone()),
    ) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() { 2 } else { 0 };
        }
    };
    // Ctrl+C anywhere in a run cancels it (exit 130) instead of hanging on
    // a slow request or leaving a half-written delivery.
    let result = tokio::select! {
        biased;
        _ = tokio::signal::ctrl_c() => return EXIT_CANCEL,
        result = dispatch(cli, normalized) => result,
    };
    match result {
        Ok(()) => 0,
        Err(e) => fail(&e),
    }
}

fn fail(e: &AppError) -> i32 {
    let _ = std::io::stdout().flush();
    eprintln!("error: {}", e.chain());
    e.kind.exit_code()
}

async fn dispatch(cli: Cli, normalized: Normalized) -> AppResult<()> {
    // Management subcommands.
    match &cli.command {
        Some(Commands::Tasks { cmd }) => return manage_tasks(cmd),
        Some(Commands::Profiles) => return manage_profiles(),
        Some(Commands::Config { cmd }) => return manage_config(cmd),
        Some(Commands::History { cmd }) => return manage_history(cmd).await,
        Some(Commands::Hold { image, secs }) => return run_hold(*image, *secs),
        None => {}
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

    if task_name == "ask" && cli.prompt.as_deref().is_none_or(str::is_empty) {
        return Err(AppError::usage(
            "`ask` needs -p with the instruction, e.g. `aido ask -p \"summarize this\" file.md`",
        ));
    }

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

    let task = tasks::get(&task_name).map_err(|e| AppError::usage(format!("{e:#}")))?;
    let cfg = config::load().map_err(|e| AppError::usage(format!("{e:#}")))?;
    let terminal = TerminalInfo::real();
    let mut env = InputEnv::real();
    let specs = normalized.specs.clone();

    let plan = plan::build(&cli, &task, &specs, &cfg, terminal, &mut env)?;

    if cli.dry_run {
        print!("{}", plan::describe(&plan));
        return Ok(());
    }

    // Run: the runner streams, merges and assembles artifacts.
    let output = runner::execute(&plan).await?;

    // Truncated or otherwise incomplete generations are recorded but not
    // delivered (what streamed live already cannot be taken back).
    let run_id = history::new_run_id();
    let mut record = RunRecord {
        run_id: run_id.clone(),
        task: Some(task.name.clone()),
        created_at: now_iso(),
        summary: plan::summarize(&plan),
        generation: output.status.clone(),
        artifacts: output.artifacts.clone(),
        warnings: output.warnings.clone(),
        deliveries: Vec::new(),
    };
    if !output.status.is_complete() {
        if plan.record_history {
            best_effort(
                history::save_generation(&record),
                "failed to record the run",
            );
        }
        let reason = match &output.status {
            GenerationStatus::Incomplete { reason } => format!(" ({reason})"),
            _ => String::new(),
        };
        return Err(AppError::generation(format!(
            "the generation did not complete{reason}; the result is not delivered"
        )));
    }

    // Save before delivery: a generation is recoverable even when every
    // destination fails.
    if plan.record_history {
        best_effort(
            history::save_generation(&record),
            "failed to record the run",
        );
    }

    let hold_secs = cfg.settings.hold_secs.unwrap_or(45);
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
    };
    match output::deliver(&deliver_args) {
        Ok(outcome) => {
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
            Ok(())
        }
        Err(e) => {
            // Keep whatever succeeded on record: partial deliveries are
            // the recoverable path when the clipboard failed.
            record.deliveries = fallback_states(&plan);
            if plan.record_history {
                best_effort(
                    history::update_deliveries(&record),
                    "failed to update the run record",
                );
            }
            Err(e)
        }
    }
}

/// Best-effort per-destination states when delivery reported an error:
/// stdout streamed live already, so it "succeeded" as far as bytes go.
fn fallback_states(plan: &ExecutionPlan) -> Vec<DeliveryState> {
    plan.destinations
        .iter()
        .map(|d| DeliveryState {
            destination: d.clone(),
            status: DeliveryStatus::Failed {
                error: "delivery failed".into(),
            },
        })
        .collect()
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

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn print_help() {
    let mut cmd = <Cli as clap::CommandFactory>::command();
    cmd.print_help().ok();
}

// ---------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------

async fn run_last(cli: &Cli) -> AppResult<()> {
    let Some(record) = history::last_complete().map_err(|e| AppError::usage(format!("{e:#}")))?
    else {
        return Err(AppError::usage(
            "no completed runs in history yet; only complete generations are recoverable",
        ));
    };
    deliver_restored(cli, record).await
}

async fn manage_history(cmd: &HistoryCmd) -> AppResult<()> {
    match cmd {
        HistoryCmd::List => {
            let ids = history::list_ids().map_err(|e| AppError::usage(format!("{e:#}")))?;
            if ids.is_empty() {
                println!("no runs recorded yet");
                return Ok(());
            }
            for id in ids.iter().rev() {
                match history::load(id) {
                    Ok(Some(record)) => println!(
                        "{id}  {:<12} {}",
                        record.task.as_deref().unwrap_or("-"),
                        generation_label(&record.generation)
                    ),
                    Ok(None) => println!("{id}  (unreadable)"),
                    Err(e) => println!("{id}  (error: {e:#})"),
                }
            }
            Ok(())
        }
        HistoryCmd::Show { run_id } => {
            let record = history::load(run_id).map_err(|e| AppError::usage(format!("{e:#}")))?;
            let Some(record) = record else {
                return Err(AppError::usage(format!("no run '{run_id}' in history")));
            };
            if !record.generation.is_complete() {
                println!(
                    "run {run_id}: generation {} — artifacts are not delivered \
                     for incomplete runs",
                    generation_label(&record.generation)
                );
                return Ok(());
            }
            deliver_restored(&empty_cli_for_restore(), record).await
        }
    }
}

/// Restore delivers through the normal output system without touching the
/// service (or credentials, or the config).
async fn deliver_restored(cli: &Cli, record: RunRecord) -> AppResult<()> {
    let produce: Vec<MediaKind> = record.artifacts.iter().map(|a| a.kind).collect();
    let destinations: Vec<Destination> = restore_destinations(cli, &record)?;
    let args = DeliverArgs {
        artifacts: &record.artifacts,
        produce: &produce,
        destinations: &destinations,
        overwrite: cli.overwrite,
        live_stdout: false,
        hold_secs: 45,
        quiet: cli.quiet,
        json: cli.json,
        run_id: &record.run_id,
        task: record.task.as_deref(),
    };
    output::deliver(&args).map(|_| ())
}

fn restore_destinations(cli: &Cli, record: &RunRecord) -> AppResult<Vec<Destination>> {
    let mut destinations: Vec<Destination> = Vec::new();
    if let Some(path) = &cli.output {
        if path.as_os_str() == "-" {
            destinations.push(Destination::Stdout);
        } else {
            destinations.push(Destination::File { path: path.clone() });
        }
    }
    if let Some(dir) = &cli.out_dir {
        destinations.push(Destination::Directory { path: dir.clone() });
    }
    if cli.stdout {
        destinations.push(Destination::Stdout);
    }
    if cli.copy {
        if record.artifacts.len() > 1 {
            return Err(AppError::usage(
                "the clipboard takes one artifact; use --out-dir to restore this run",
            ));
        }
        destinations.push(Destination::Clipboard);
    }
    if destinations.is_empty() {
        destinations.push(Destination::Stdout);
    }
    Ok(destinations)
}

/// A CLI with only defaults, for `history show` (no user flags reach it).
fn empty_cli_for_restore() -> Cli {
    Cli::try_parse_from(["aido"]).expect("an empty invocation always parses")
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

fn manage_tasks(cmd: &TasksCmd) -> AppResult<()> {
    match cmd {
        TasksCmd::List => {
            let all = tasks::load_all().map_err(|e| AppError::usage(format!("{e:#}")))?;
            for (name, task) in &all {
                println!(
                    "{name:<14} {} [{}, {}]",
                    first_line(&task.instruction),
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
            println!("processor:   {}", processor_name(task.processor));
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
    }
}

fn first_line(s: &str) -> String {
    let line = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if line.chars().count() > 64 {
        format!("{}...", line.chars().take(64).collect::<String>())
    } else {
        line.to_string()
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

fn run_hold(image: bool, secs: u64) -> AppResult<()> {
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
    std::thread::sleep(std::time::Duration::from_secs(secs));
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

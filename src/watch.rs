//! The directory watcher: `aido watch DIR -- TASK [FLAGS]` runs the task
//! on every file that lands in DIR and delivers the result — the
//! place-it-and-forget-it form of the run path (screenshot inbox, recording
//! inbox, asset pipeline front end).
//!
//! The daemon owns no execution machinery of its own: every file is fed
//! through the ordinary run path ([`crate::app::run_task`]), so a watched
//! file is an ordinary history record and an ordinary delivery. What the
//! daemon adds is a startup precheck (nothing guards with a plan that
//! cannot run), a size-stability debounce (a half-written file waits until
//! it stops growing), and an in-memory processed set (a restart never
//! replays old files unless `--include-existing` says otherwise).

use crate::app::{run_task, RunState};
use crate::cli::{self, Cli, SourceSpec, WatchArgs};
use crate::config::{self, Config};
use crate::domain::{first_line, AppError, AppResult, MediaKind};
use crate::input::InputEnv;
use crate::plan::{self, TerminalInfo};
use crate::tasks::{self, Task};
use clap::Parser as _;
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Floor for the poll interval (`--interval`, config, default): below it
/// the daemon would busy-list the directory for no observable gain.
const MIN_INTERVAL_MS: u64 = 50;
/// Floor for the stability window (`--stable-ms`, config, default): a
/// zero-length window would fire on files that are still being written.
const MIN_STABLE_MS: u64 = 100;

pub(crate) async fn run(
    cli: &Cli,
    args: WatchArgs,
    state: Arc<std::sync::Mutex<RunState>>,
) -> AppResult<()> {
    // --- startup precheck ---------------------------------------------------
    // Everything that can be known to fail before the first file arrives is
    // checked here, so a broken watch exits 2 instead of guarding and then
    // failing file after file.
    if !args.dir.is_dir() {
        return Err(AppError::usage(format!(
            "watch directory '{}' is not a directory",
            args.dir.display()
        )));
    }
    let guard = absolute(&args.dir);
    let probe = probe(cli, &args, &guard)?;

    let cfg = config::load().map_err(|e| AppError::usage(format!("{e:#}")))?;
    let (interval, stable) = resolve_timing(&args, &cfg);

    if cli.dry_run {
        if !cli.quiet {
            eprintln!(
                "watch: showing the probe plan for '{}'; each arriving file runs this plan",
                args.dir.display()
            );
        }
        print!("{}", plan::describe(&probe.plan));
        return Ok(());
    }

    // Startup inventory is seeded before the banner goes out: the banner
    // is the deterministic "now everything already on disk counts as
    // history, everything arriving next is a fresh file" mark. The
    // baseline must be real: treating a failed scan as an empty directory
    // would replay every old file once the directory is readable again.
    let bell = bell_enabled(cli.quiet, std::io::stderr().is_terminal());
    let existing = initial_inventory(&guard, &args.dir)?;
    let mut watched = WatchState::new(stable, existing, args.include_existing);
    let mut unreadable = false;

    if !cli.quiet {
        eprintln!(
            "watch: guarding {} → {} (interval {} ms, stability window {} ms); Ctrl+C to stop",
            args.dir.display(),
            probe.task_name,
            interval.as_millis(),
            stable.as_millis()
        );
    }

    loop {
        match scan_dir(&guard) {
            Ok(files) => {
                if unreadable {
                    unreadable = false;
                    if !cli.quiet {
                        eprintln!("watch: '{}' is readable again", args.dir.display());
                    }
                }
                if let Some(path) = watched.next_ready(std::time::Instant::now(), &files) {
                    let outcome = run_one(cli, &args, &path, &state).await;
                    // Failed or not, the file is done: a watch never
                    // retries (a broken input stays broken), it keeps
                    // guarding.
                    let failure = match &outcome {
                        Ok(()) => None,
                        Err(e) => Some(first_line(&e.chain(), 160)),
                    };
                    announce(&path, &probe.task_name, failure, cli.quiet, bell);
                    watched.mark_done(&path);
                    // No sleep before the next verdict: the loop folds a
                    // fresh listing in immediately, so a file that grew
                    // while this one ran is re-debounced on new stats
                    // instead of riding an older listing's ready verdict.
                    continue;
                }
            }
            Err(e) => {
                // A vanished or permission-stripped directory is not the
                // end of the watch: warn once and keep polling in case it
                // comes back.
                if !unreadable {
                    unreadable = true;
                    if !cli.quiet {
                        eprintln!(
                            "warning: cannot read '{}': {e}; still watching",
                            args.dir.display()
                        );
                    }
                }
            }
        }
        tokio::time::sleep(interval).await;
    }
}

/// One file, one ordinary run: re-normalize the task invocation with the
/// file appended (after `--`, so no glob expansion and no flag confusion)
/// and hand it to the shared pipeline. The parent's `--quiet`/`--json`
/// ride along; `--dry-run` never reaches a file (it exits before the loop).
/// The artifacts are named after the file (shot.png →
/// shot-txt--<hash>.txt) so one `--out-dir` collects a stream of results
/// instead of one `text.txt`.
async fn run_one(
    cli: &Cli,
    args: &WatchArgs,
    path: &Path,
    state: &Arc<std::sync::Mutex<RunState>>,
) -> AppResult<()> {
    let mut invocation = build_invocation(&args.task_argv, path)?;
    invocation.cli.quiet |= cli.quiet;
    invocation.cli.json |= cli.json;
    let stem = watch_artifact_stem(path);
    run_task(
        &invocation.cli,
        invocation.task_name,
        invocation.specs,
        state,
        stem.as_deref(),
    )
    .await
}

/// The artifact stem for one watched file: the sanitized full file name
/// plus a short hash of the raw name (`report.png` →
/// `report-png--<hash>`). Two inputs must never land on one artifact:
/// bare stems collide (`report.png`/`report.jpg` share `report`), and even
/// full names collide once sanitized (`a.b` and `a-b` both become `a-b`),
/// so the raw bytes go into a stable FNV-1a suffix. The hash identifies,
/// it does not protect — no crate, no secrets.
fn watch_artifact_stem(path: &Path) -> Option<String> {
    let name = path.file_name()?;
    let readable = crate::output::sanitize_stem(&name.to_string_lossy());
    let hash = fnv1a64(name.as_encoded_bytes());
    Some(format!("{readable}--{:08x}", (hash & 0xffff_ffff) as u32))
}

/// FNV-1a 64-bit, hand-rolled: the std hashers are keyed and deliberately
/// not stable across releases, and this hash only has to be stable.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The one-line outcome of one watched file, plus a terminal bell: in the
/// screenshot flow the user has usually switched away from the watch
/// terminal by the time the clipboard is ready, and the bell lights up the
/// tab with no platform machinery. `--quiet` swallows line and bell; a
/// piped stderr keeps the line but never rings.
fn announce(path: &Path, task: &str, failure: Option<String>, quiet: bool, bell: bool) {
    announce_to(&mut std::io::stderr(), path, task, failure, quiet, bell);
}

/// The sink-taking core of [`announce`]: the line first, then the bell
/// byte, through one writer — so tests read exactly what a terminal or a
/// pipe would receive.
fn announce_to(
    out: &mut dyn std::io::Write,
    path: &Path,
    task: &str,
    failure: Option<String>,
    quiet: bool,
    bell: bool,
) {
    if quiet {
        return;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    match failure {
        None => {
            let _ = writeln!(out, "[watch] {name} → {task}: done");
        }
        Some(reason) => {
            let _ = writeln!(
                out,
                "[watch] {name} → {task}: failed (not retried; still watching): {reason}"
            );
        }
    }
    if bell {
        // BEL: the terminal (or its tab) marks the spot. Rust has no `\a`
        // escape, so the byte is spelled out.
        let _ = out.write_all(b"\x07");
    }
    let _ = out.flush();
}

/// The bell needs both a listener and a place to ring: `--quiet` silences
/// everything, and a piped stderr (test harness, log file) never rings.
fn bell_enabled(quiet: bool, stderr_is_tty: bool) -> bool {
    !quiet && stderr_is_tty
}

/// What the precheck proved about the watch: the resolved task name for
/// the banner and result lines, and the validated probe plan.
struct Probe {
    task_name: String,
    plan: plan::ExecutionPlan,
}

/// The startup precheck: build the per-file invocation against a probe
/// file, so task resolution, capability intersection, delivery targets and
/// credentials are all validated before the daemon settles in. The probe
/// is a placeholder input standing in for each arriving file.
fn probe(cli: &Cli, args: &WatchArgs, guard: &Path) -> AppResult<Probe> {
    // The task is whoever the normalizer says it is — the same parse the
    // per-file runs go through, so a single-task form a plain run accepts
    // (`--copy ocr`, `run ocr`, an implicit `-p` ask) cannot be refused
    // here, and a chain spelling is refused identically in both places.
    let invocation = parse_task_invocation(args.task_argv.clone())?;
    let task = tasks::get(&invocation.task_name).map_err(|e| AppError::usage(format!("{e:#}")))?;

    // Try the task's own input kinds first, then every kind: the probe
    // content must classify as something the task (and its profile) takes,
    // and classification is by content, not extension.
    let mut last_err: Option<AppError> = None;
    for kind in probe_kinds(&task) {
        let probe_path = match write_probe_file(kind) {
            Ok(p) => p,
            Err(e) => {
                return Err(AppError::usage(format!(
                    "cannot create the watch precheck probe file: {e}"
                )));
            }
        };
        let result = probe_plan(cli, args, guard, &probe_path);
        let _ = std::fs::remove_file(&probe_path);
        match result {
            Ok(plan) => {
                return Ok(Probe {
                    task_name: task.name.clone(),
                    plan,
                });
            }
            Err(e) => {
                last_err.get_or_insert(e);
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| AppError::usage("the task accepts no input a probe could stand in for")))
}

/// Validate one probe invocation end to end: parse, delivery rules,
/// destination sanity, then the real plan build (which runs the same
/// input, capability and credential checks a run would).
fn probe_plan(
    cli: &Cli,
    args: &WatchArgs,
    guard: &Path,
    probe_path: &Path,
) -> AppResult<plan::ExecutionPlan> {
    let mut invocation = build_invocation(&args.task_argv, probe_path)?;
    invocation.cli.quiet |= cli.quiet;
    invocation.cli.json |= cli.json;

    // Task-level shape the plan build does not check: `ask` without -p
    // would start a watch that fails every file.
    crate::app::require_ask_prompt(&invocation.cli, &invocation.task_name)?;

    // Delivery must be explicit: a watched task's stdout has no audience,
    // and `-o` names one fixed file that every arrival would fight over.
    if invocation.cli.output.is_some() {
        return Err(AppError::usage(
            "a watched task cannot use -o (one fixed file cannot take a result per \
             arriving file); use --out-dir",
        ));
    }
    if !invocation.cli.copy && invocation.cli.out_dir.is_none() {
        return Err(AppError::usage(
            "watch needs an explicit delivery destination: pass --copy or --out-dir \
             to the task after the `--` separator",
        ));
    }
    if invocation
        .specs
        .iter()
        .any(|s| matches!(s, SourceSpec::Stdin))
    {
        return Err(AppError::usage(
            "a watched task cannot read stdin; drop the `-` input",
        ));
    }
    // Artifacts landing in the guarded directory itself would re-trigger
    // the watch; a subdirectory of it is fine (the scan never descends).
    if let Some(out_dir) = &invocation.cli.out_dir {
        if absolute(out_dir) == guard {
            return Err(AppError::usage(format!(
                "--out-dir '{}' is the watched directory; the outputs would \
                 re-trigger the watch",
                out_dir.display()
            )));
        }
    }

    let task = tasks::get(&invocation.task_name).map_err(|e| AppError::usage(format!("{e:#}")))?;
    let cfg = config::load().map_err(|e| AppError::usage(format!("{e:#}")))?;
    let terminal = TerminalInfo::real();
    let mut env = InputEnv::real();
    let plan = plan::build(
        &invocation.cli,
        &task,
        &invocation.specs,
        &cfg,
        terminal,
        &mut env,
    )?;

    if plan.credentials_available == Some(false) {
        let env_var = plan
            .resolved
            .api_key_env
            .clone()
            .unwrap_or_else(|| "the provider's api_key_env".to_string());
        return Err(AppError::usage(format!(
            "{env_var} is not set — every watched file would fail; export it \
             before starting the watch"
        )));
    }
    Ok(plan)
}

/// A fully parsed task invocation: the clap `Cli` a run will use, the task
/// name the normalizer resolved, and the input specs.
#[derive(Debug)]
struct TaskInvocation {
    cli: Cli,
    task_name: String,
    specs: Vec<SourceSpec>,
}

/// Parse the single-task invocation used by watch.
///
/// The ordinary normalizer still decides task names, flag ownership, `run
/// NAME`, implicit `-p` ask and input specs, so watch does not maintain a
/// second task grammar — the startup precheck and the per-file runs can
/// never disagree about what an invocation means. Watch v1 deliberately
/// rejects [`cli::Normalized::Chain`]: each arriving file runs exactly one
/// task, and chain composition (file injection, artifact naming, history,
/// cancellation semantics) is defined later, if at all.
fn parse_task_invocation(argv: Vec<OsString>) -> AppResult<TaskInvocation> {
    let normalized = cli::normalize(argv).map_err(|e| AppError::usage(format!("{e:#}")))?;
    let (cli, task_name, specs) = match &normalized {
        cli::Normalized::Single { task, specs, argv } => {
            let cli =
                Cli::try_parse_from(std::iter::once(OsString::from("aido")).chain(argv.clone()))
                    .map_err(|e| AppError::usage(e.to_string()))?;
            let task_name = task.clone().or_else(|| cli.task.clone()).ok_or_else(|| {
                AppError::usage(
                    "the word after `watch DIR --` must be a task (see `aido tasks list`)",
                )
            })?;
            (cli, task_name, specs.clone())
        }
        // v1 watches run one task per file: the junction contract and the
        // per-file stem naming have no chain story yet.
        cli::Normalized::Chain { .. } => {
            return Err(AppError::usage(
                "task chains are not supported inside watch v1; \
                 watch runs exactly one task per arriving file",
            ));
        }
        cli::Normalized::Watch(_) => {
            return Err(AppError::usage("a watched task cannot be another watch"));
        }
    };
    Ok(TaskInvocation {
        cli,
        task_name,
        specs,
    })
}

/// The per-file invocation: the task argv with `file` appended after `--`,
/// parsed by [`parse_task_invocation`]. Watched deliveries always
/// overwrite: the daemon reuses one `--out-dir` across runs, and the
/// ordinary no-clobber preflights (an existing `manifest.json`, an
/// existing artifact) would refuse every arrival after the first.
fn build_invocation(task_argv: &[OsString], file: &Path) -> AppResult<TaskInvocation> {
    let mut argv: Vec<OsString> = task_argv.to_vec();
    argv.push(OsString::from("--"));
    argv.push(file.as_os_str().to_os_string());
    let mut invocation = parse_task_invocation(argv)?;
    invocation.cli.overwrite = true;
    Ok(invocation)
}

/// Probe content for each kind — magic bytes only: the gather-time
/// classifier sniffs content, and the probe never reaches a decode step
/// because the watch stops at the plan.
fn probe_kinds(task: &Task) -> Vec<MediaKind> {
    let mut kinds = Vec::new();
    if let Some(list) = &task.input_types {
        kinds.extend(list.iter().copied());
    }
    for kind in [MediaKind::Text, MediaKind::Image, MediaKind::Audio] {
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    kinds
}

/// Probe content for each kind. Classification is by content, and some
/// processors decode further at plan time (the OCR tile planner reads the
/// image's dimensions), so the image probe is a real, complete 1×1 PNG —
/// not a bare signature; audio and text are sniffed by magic/UTF-8 only.
fn probe_bytes(kind: MediaKind) -> &'static [u8] {
    match kind {
        MediaKind::Image => &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x00, 0x00, 0x00,
            0x00, 0x3a, 0x7e, 0x9b, 0x55, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0x60, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01, 0x48, 0xaf, 0xa4, 0x71, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ],
        MediaKind::Audio => b"RIFF\0\0\0\0WAVE",
        MediaKind::Text => b"watch precheck probe\n",
    }
}

/// A short-lived dot-named file in the temp dir; removed as soon as its
/// plan is built. Created no-clobber, so a stale leftover (or a parallel
/// daemon with a recycled pid) cannot be followed or fought over.
fn write_probe_file(kind: MediaKind) -> std::io::Result<PathBuf> {
    for attempt in 0..64 {
        let path = std::env::temp_dir().join(format!(
            ".aido-watch-probe-{}-{attempt}",
            std::process::id()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(probe_bytes(kind))?;
                return Ok(path);
            }
            // A stale leftover from a dead process: try the next name.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no free probe file name",
    ))
}

/// Poll cadence: `--interval` (seconds) over the config over one second;
/// `--stable-ms` over the config over half a second. Both are floored so
/// a typo cannot turn the daemon into a busy loop or fire on half-written
/// files.
fn resolve_timing(args: &WatchArgs, cfg: &Config) -> (Duration, Duration) {
    let interval_ms = args
        .interval
        .map(|secs| (secs * 1000.0) as u64)
        .or(cfg.settings.watch_interval_ms)
        .unwrap_or(config::DEFAULT_WATCH_INTERVAL_MS)
        .max(MIN_INTERVAL_MS);
    let stable_ms = args
        .stable_ms
        .or(cfg.settings.watch_stable_ms)
        .unwrap_or(config::DEFAULT_WATCH_STABLE_MS)
        .max(MIN_STABLE_MS);
    (
        Duration::from_millis(interval_ms),
        Duration::from_millis(stable_ms),
    )
}

/// Absolute form of a path. A path that exists is canonicalized (symlink
/// truth); one that does not exist yet gets a lexically normalized
/// absolute form, so `a/b/..` cannot smuggle a different directory past
/// the out-dir comparison before the path is ever created.
fn absolute(path: &Path) -> PathBuf {
    if let Ok(p) = path.canonicalize() {
        return p;
    }
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// The startup inventory: the listing a watch's baseline is seeded from.
/// A failure here is fatal (exit 2, before the banner) — an empty
/// fallback would mark every pre-existing file as a fresh arrival once
/// the directory is readable again, replaying them all.
fn initial_inventory(guard: &Path, display_dir: &Path) -> AppResult<Vec<(PathBuf, u64)>> {
    scan_dir(guard).map_err(|e| {
        AppError::usage(format!(
            "cannot read watch directory '{}': {e}",
            display_dir.display()
        ))
    })
}

/// The guarded directory's top-level regular files, sorted by name, with
/// their sizes: the same one-level, dot-free rule a DIR input uses (see
/// `input::expand_file_spec`) — subdirectories are never descended, and
/// entries that vanish between listing and stat are simply skipped.
fn scan_dir(dir: &Path) -> std::io::Result<Vec<(PathBuf, u64)>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        // metadata follows symlinks: a link to a file is a file, a link
        // to a directory is a directory (skipped), a broken link is gone.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        files.push((path, meta.len()));
    }
    files.sort();
    Ok(files)
}

/// Debounce bookkeeping for one guarded directory. Pure state over an
/// injected clock: `next_ready` decides which file runs next, and the
/// daemon only executes what it returns — so timing behavior is unit-test
/// territory, not daemon-loop territory.
struct WatchState {
    stable: Duration,
    pending: BTreeMap<PathBuf, Pending>,
    done: HashSet<PathBuf>,
}

/// A file seen growing: its last observed size and the moment it last
/// changed. A file whose size holds still for the stability window is
/// considered fully written.
struct Pending {
    size: u64,
    changed: std::time::Instant,
}

impl WatchState {
    /// Seed from the directory's current contents. By default existing
    /// files are marked done: a watch guards what arrives next, it does
    /// not replay the past (restart to reset). `include_existing` turns
    /// them into the first batch instead.
    fn new(stable: Duration, existing: Vec<(PathBuf, u64)>, include_existing: bool) -> Self {
        let mut state = Self {
            stable,
            pending: BTreeMap::new(),
            done: HashSet::new(),
        };
        if !include_existing {
            state
                .done
                .extend(existing.into_iter().map(|(path, _)| path));
        }
        state
    }

    /// Fold one directory listing in and return the one file to run next,
    /// if any: new files start pending, a size change resets the stability
    /// clock, a file that held still long enough fires. At most one file
    /// leaves per call — the daemon runs it, marks it done and folds the
    /// next listing in, so a file that changed while an earlier file ran
    /// is judged on fresh stats, never on an older listing's verdict.
    /// Pending files that vanished are dropped silently.
    fn next_ready(&mut self, now: std::time::Instant, files: &[(PathBuf, u64)]) -> Option<PathBuf> {
        let seen: HashSet<&Path> = files.iter().map(|(p, _)| p.as_path()).collect();
        self.pending.retain(|p, _| seen.contains(p.as_path()));
        let mut ready: Option<PathBuf> = None;
        for (path, size) in files {
            if self.done.contains(path) {
                continue;
            }
            match self.pending.get_mut(path) {
                Some(pending) => {
                    if *size != pending.size {
                        pending.size = *size;
                        pending.changed = now;
                    }
                    // Lexically smallest first, so processing order never
                    // depends on the caller's listing order (scan_dir
                    // sorts; the state machine does not trust that).
                    if now.duration_since(pending.changed) >= self.stable
                        && ready
                            .as_ref()
                            .is_none_or(|current| path.as_path() < current.as_path())
                    {
                        ready = Some(path.clone());
                    }
                }
                None => {
                    self.pending.insert(
                        path.clone(),
                        Pending {
                            size: *size,
                            changed: now,
                        },
                    );
                }
            }
        }
        ready
    }

    fn mark_done(&mut self, path: &Path) {
        self.pending.remove(path);
        self.done.insert(path.to_path_buf());
    }
}

#[cfg(test)]
mod tests;

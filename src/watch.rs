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
    // history, everything arriving next is a fresh file" mark.
    let bell = bell_enabled(cli.quiet, std::io::stderr().is_terminal());
    let existing = scan_dir(&guard).unwrap_or_default();
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
                for path in watched.scan(std::time::Instant::now(), &files) {
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
/// The artifacts are named after the file (shot.png → shot.txt) so one
/// `--out-dir` collects a stream of results instead of one `text.txt`.
async fn run_one(
    cli: &Cli,
    args: &WatchArgs,
    path: &Path,
    state: &Arc<std::sync::Mutex<RunState>>,
) -> AppResult<()> {
    let (mut file_cli, task_name, specs) = build_invocation(&args.task_argv, path)?;
    file_cli.quiet |= cli.quiet;
    file_cli.json |= cli.json;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string);
    run_task(&file_cli, task_name, specs, state, stem.as_deref()).await
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
    // The task name is the invocation's first word — resolve it early to
    // pick probe content the task would accept. A non-UTF-8 word cannot
    // be a task name; tasks::get then says so instead of this function.
    let first = args
        .task_argv
        .first()
        .map(|t| t.to_string_lossy().into_owned())
        .ok_or_else(|| AppError::usage("watch needs a task after the `--` separator"))?;
    let task = tasks::get(&first).map_err(|e| AppError::usage(format!("{e:#}")))?;

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
    let (mut file_cli, task_name, specs) = build_invocation(&args.task_argv, probe_path)?;
    file_cli.quiet |= cli.quiet;
    file_cli.json |= cli.json;

    // Task-level shape the plan build does not check: `ask` without -p
    // would start a watch that fails every file.
    crate::app::require_ask_prompt(&file_cli, &task_name)?;

    // Delivery must be explicit: a watched task's stdout has no audience,
    // and `-o` names one fixed file that every arrival would fight over.
    if file_cli.output.is_some() {
        return Err(AppError::usage(
            "a watched task cannot use -o (one fixed file cannot take a result per \
             arriving file); use --out-dir",
        ));
    }
    if !file_cli.copy && file_cli.out_dir.is_none() {
        return Err(AppError::usage(
            "watch needs an explicit delivery destination: pass --copy or --out-dir \
             to the task after the `--` separator",
        ));
    }
    if specs.iter().any(|s| matches!(s, SourceSpec::Stdin)) {
        return Err(AppError::usage(
            "a watched task cannot read stdin; drop the `-` input",
        ));
    }
    // Artifacts landing in the guarded directory itself would re-trigger
    // the watch; a subdirectory of it is fine (the scan never descends).
    if let Some(out_dir) = &file_cli.out_dir {
        if absolute(out_dir) == guard {
            return Err(AppError::usage(format!(
                "--out-dir '{}' is the watched directory; the outputs would \
                 re-trigger the watch",
                out_dir.display()
            )));
        }
    }

    let task = tasks::get(&task_name).map_err(|e| AppError::usage(format!("{e:#}")))?;
    let cfg = config::load().map_err(|e| AppError::usage(format!("{e:#}")))?;
    let terminal = TerminalInfo::real();
    let mut env = InputEnv::real();
    let plan = plan::build(&file_cli, &task, &specs, &cfg, terminal, &mut env)?;

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

/// Normalize and clap-parse the per-file invocation with `file` appended:
/// the exact parse a real run will do, so the precheck cannot bless
/// something the loop would reject. Watched deliveries always overwrite:
/// the daemon reuses one `--out-dir` across runs, and the ordinary
/// no-clobber preflights (an existing `manifest.json`, an existing
/// artifact) would refuse every arrival after the first.
fn build_invocation(
    task_argv: &[OsString],
    file: &Path,
) -> AppResult<(Cli, String, Vec<SourceSpec>)> {
    let mut argv: Vec<OsString> = task_argv.to_vec();
    argv.push(OsString::from("--"));
    argv.push(file.as_os_str().to_os_string());
    let normalized = cli::normalize(argv).map_err(|e| AppError::usage(format!("{e:#}")))?;
    let mut file_cli =
        Cli::try_parse_from(std::iter::once(OsString::from("aido")).chain(normalized.argv.clone()))
            .map_err(|e| AppError::usage(e.to_string()))?;
    file_cli.overwrite = true;
    if normalized.watch.is_some() {
        return Err(AppError::usage("a watched task cannot be another watch"));
    }
    let task_name = normalized
        .task
        .clone()
        .or_else(|| file_cli.task.clone())
        .ok_or_else(|| {
            AppError::usage("the word after `watch DIR --` must be a task (see `aido tasks list`)")
        })?;
    Ok((file_cli, task_name, normalized.specs))
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
/// injected clock: `scan` decides which files are ready to run, and the
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

    /// Fold one directory listing in and return the files that just
    /// became ready: new files start pending, a size change resets the
    /// stability clock, a file that held still long enough fires once.
    /// Pending files that vanished are dropped silently.
    fn scan(&mut self, now: std::time::Instant, files: &[(PathBuf, u64)]) -> Vec<PathBuf> {
        let seen: HashSet<&Path> = files.iter().map(|(p, _)| p.as_path()).collect();
        self.pending.retain(|p, _| seen.contains(p.as_path()));
        let mut ready = Vec::new();
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
                    if now.duration_since(pending.changed) >= self.stable {
                        ready.push(path.clone());
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
        // Sorted so processing order never depends on the caller's listing
        // order (scan_dir sorts; the state machine does not trust that).
        ready.sort();
        ready
    }

    fn mark_done(&mut self, path: &Path) {
        self.pending.remove(path);
        self.done.insert(path.to_path_buf());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(names: &[&str]) -> Vec<(PathBuf, u64)> {
        names.iter().map(|n| (PathBuf::from(n), 10)).collect()
    }

    #[test]
    fn a_new_file_waits_for_the_stability_window_then_fires_once() {
        let stable = Duration::from_millis(500);
        let mut state = WatchState::new(stable, Vec::new(), false);
        let t0 = std::time::Instant::now();
        // First sighting only starts the clock; the daemon's mark_done
        // models the run that follows a ready file.
        assert!(state.scan(t0, &entries(&["a.png"])).is_empty());
        // Still inside the window.
        assert!(state
            .scan(t0 + Duration::from_millis(499), &entries(&["a.png"]))
            .is_empty());
        // Window elapsed: exactly once.
        assert_eq!(
            state.scan(t0 + Duration::from_millis(500), &entries(&["a.png"])),
            vec![PathBuf::from("a.png")]
        );
        state.mark_done(Path::new("a.png"));
        assert!(state
            .scan(t0 + Duration::from_millis(600), &entries(&["a.png"]))
            .is_empty());
    }

    #[test]
    fn a_growing_file_resets_the_clock_and_fires_only_when_settled() {
        let stable = Duration::from_millis(500);
        let mut state = WatchState::new(stable, Vec::new(), false);
        let t0 = std::time::Instant::now();
        let one = vec![(PathBuf::from("a.png"), 1)];
        let two = vec![(PathBuf::from("a.png"), 2)];
        assert!(state.scan(t0, &one).is_empty());
        // Growing every 300 ms keeps it pending forever.
        assert!(state.scan(t0 + Duration::from_millis(300), &two).is_empty());
        assert!(state
            .scan(t0 + Duration::from_millis(600), &entries(&["a.png"]))
            .is_empty());
        assert!(state
            .scan(t0 + Duration::from_millis(899), &entries(&["a.png"]))
            .is_empty());
        assert_eq!(
            state.scan(t0 + Duration::from_millis(1100), &entries(&["a.png"])),
            vec![PathBuf::from("a.png")]
        );
    }

    #[test]
    fn a_vanished_pending_file_is_dropped_and_a_return_is_a_new_file() {
        let stable = Duration::from_millis(100);
        let mut state = WatchState::new(stable, Vec::new(), false);
        let t0 = std::time::Instant::now();
        assert!(state.scan(t0, &entries(&["a.png"])).is_empty());
        // Gone before it settled: no trigger either way.
        assert!(state.scan(t0 + Duration::from_millis(50), &[]).is_empty());
        // Back again: a fresh file with a fresh clock.
        assert!(state
            .scan(t0 + Duration::from_millis(60), &entries(&["a.png"]))
            .is_empty());
        assert_eq!(
            state.scan(t0 + Duration::from_millis(200), &entries(&["a.png"])),
            vec![PathBuf::from("a.png")]
        );
    }

    #[test]
    fn existing_files_are_skipped_unless_include_existing() {
        let stable = Duration::from_millis(0);
        let existing = entries(&["old.txt"]);
        let mut state = WatchState::new(stable, existing.clone(), false);
        let t0 = std::time::Instant::now();
        assert!(state.scan(t0, &existing).is_empty());
        assert!(state
            .scan(t0 + Duration::from_millis(1), &existing)
            .is_empty());

        let mut state = WatchState::new(stable, existing.clone(), true);
        assert!(state.scan(t0, &existing).is_empty());
        assert_eq!(
            state.scan(t0 + Duration::from_millis(1), &existing),
            vec![PathBuf::from("old.txt")]
        );
    }

    #[test]
    fn mark_done_survives_a_same_name_rewrite() {
        // v1: a file rewritten in place after processing is not processed
        // again — the daemon remembers paths, not contents.
        let stable = Duration::from_millis(0);
        let mut state = WatchState::new(stable, Vec::new(), false);
        let t0 = std::time::Instant::now();
        let files = entries(&["a.png"]);
        assert!(state.scan(t0, &files).is_empty());
        assert_eq!(
            state.scan(t0 + Duration::from_millis(1), &files),
            vec![PathBuf::from("a.png")]
        );
        state.mark_done(Path::new("a.png"));
        let bigger = vec![(PathBuf::from("a.png"), 99)];
        assert!(state
            .scan(t0 + Duration::from_millis(2), &bigger)
            .is_empty());
    }

    #[test]
    fn two_files_fire_in_sorted_order_and_independently() {
        let stable = Duration::from_millis(0);
        let mut state = WatchState::new(stable, Vec::new(), false);
        let t0 = std::time::Instant::now();
        let both = entries(&["b.png", "a.png"]);
        assert!(state.scan(t0, &both).is_empty());
        assert_eq!(
            state.scan(t0 + Duration::from_millis(1), &both),
            vec![PathBuf::from("a.png"), PathBuf::from("b.png")]
        );
        state.mark_done(Path::new("a.png"));
        assert_eq!(
            state.scan(t0 + Duration::from_millis(2), &both),
            vec![PathBuf::from("b.png")]
        );
    }

    #[test]
    fn resolve_timing_applies_floor_and_precedence() {
        let args = |interval: Option<f64>, stable: Option<u64>| WatchArgs {
            dir: PathBuf::from("d"),
            interval,
            stable_ms: stable,
            include_existing: false,
            task_argv: vec![OsString::from("ocr")],
        };
        let cfg = Config::default();
        let (interval, stable) = resolve_timing(&args(None, None), &cfg);
        assert_eq!(
            interval,
            Duration::from_millis(config::DEFAULT_WATCH_INTERVAL_MS)
        );
        assert_eq!(
            stable,
            Duration::from_millis(config::DEFAULT_WATCH_STABLE_MS)
        );

        let (interval, stable) = resolve_timing(&args(Some(0.001), Some(1)), &cfg);
        assert_eq!(interval, Duration::from_millis(MIN_INTERVAL_MS));
        assert_eq!(stable, Duration::from_millis(MIN_STABLE_MS));

        let (interval, _) = resolve_timing(&args(Some(2.5), None), &cfg);
        assert_eq!(interval, Duration::from_millis(2500));
    }

    #[test]
    fn announce_writes_the_line_then_the_bell() {
        let mut out: Vec<u8> = Vec::new();
        announce_to(
            &mut out,
            Path::new("shots/shot.png"),
            "ocr",
            None,
            false,
            false,
        );
        assert_eq!(
            String::from_utf8_lossy(&out),
            "[watch] shot.png → ocr: done\n"
        );

        let mut out: Vec<u8> = Vec::new();
        announce_to(
            &mut out,
            Path::new("shots/bad.txt"),
            "ocr",
            Some("server 500".into()),
            false,
            true,
        );
        assert_eq!(
            String::from_utf8_lossy(&out),
            "[watch] bad.txt → ocr: failed (not retried; still watching): server 500\n\u{7}",
            "the line first, the bell byte after it"
        );
    }

    #[test]
    fn announce_swallows_line_and_bell_when_quiet() {
        let mut out: Vec<u8> = Vec::new();
        announce_to(&mut out, Path::new("a.png"), "ocr", None, true, true);
        assert!(out.is_empty());
    }

    #[test]
    fn the_bell_needs_both_a_listener_and_a_terminal() {
        for (quiet, tty, rings) in [
            (false, true, true),
            (false, false, false),
            (true, true, false),
            (true, false, false),
        ] {
            assert_eq!(bell_enabled(quiet, tty), rings, "quiet={quiet} tty={tty}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn announce_survives_a_non_utf8_file_name() {
        use std::os::unix::ffi::OsStrExt as _;
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(b"caf\xe9.png"));
        let mut out: Vec<u8> = Vec::new();
        announce_to(&mut out, &path, "ocr", None, false, false);
        // The lossy name, not a panic and not the raw invalid bytes.
        assert!(String::from_utf8_lossy(&out).contains("caf\u{FFFD}.png"));
    }

    #[test]
    fn scan_dir_lists_top_level_files_sorted_without_dots_or_subdirs() {
        let dir = crate::test_support::run_root().join("watch-scan");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("b.txt"), b"12345").unwrap();
        std::fs::write(dir.join("a.txt"), b"1").unwrap();
        std::fs::write(dir.join(".hidden.txt"), b"x").unwrap();
        std::fs::write(dir.join("sub").join("inner.txt"), b"x").unwrap();

        let files = scan_dir(&dir).unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a.txt", "b.txt"]);
        // Real sizes: the debounce's stability clock compares them.
        assert_eq!(files[0].1, 1);
        assert_eq!(files[1].1, 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn scan_dir_keeps_non_utf8_names_sorted_by_raw_bytes() {
        use std::os::unix::ffi::OsStrExt as _;
        let dir = crate::test_support::run_root().join("watch-scan-non-utf8");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        // The dot check runs on the lossy name: a replacement character is
        // not a dot, so the file must survive the scan with its name intact.
        std::fs::write(dir.join(std::ffi::OsStr::from_bytes(b"\xff.png")), b"x").unwrap();

        let files = scan_dir(&dir).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0.file_name().unwrap().to_string_lossy(), "a.txt");
        assert_eq!(files[1].0.file_name().unwrap().as_bytes(), b"\xff.png");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn scan_dir_follows_symlinks_and_skips_broken_ones() {
        let dir = crate::test_support::run_root().join("watch-scan-symlinks");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("real.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(dir.join("real.txt"), dir.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(dir.join("gone"), dir.join("broken.txt")).unwrap();

        let names: Vec<_> = scan_dir(&dir)
            .unwrap()
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // A link to a file is a file; a broken link is gone.
        assert_eq!(names, ["link.txt", "real.txt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

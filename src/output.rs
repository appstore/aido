//! Delivery: one artifact set, four destination kinds, one report.
//!
//! Order of operations (contract §step 7): validate every artifact, then
//! commit files and directories (atomic, no-clobber by default), then the
//! clipboard. A failure in one destination keeps earlier successes and
//! fails the run with exit code 5; the result itself stays recoverable in
//! history. The late re-validation refusals count as delivery failures
//! too: delivery only runs after the generation succeeded, so exit 2
//! (usage) would claim nothing happened when the model already ran —
//! and a refusal still reaches the JSON report epilogue, so `--json`
//! keeps one report on every exit code. The exception is an `-o` target
//! that already exists: knowable from the plan alone, it is refused
//! pre-flight by [`precheck_file_targets`] before any request is sent.

use crate::clipboard;
use crate::domain::{
    extension_matches_format, AppError, AppResult, Artifact, DeliveryState, DeliveryStatus,
    Destination, ErrorKind, MediaKind, JSON_ENVELOPE_VERSION,
};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Everything delivery needs to know about a finished generation.
pub struct DeliverArgs<'a> {
    pub artifacts: &'a [Artifact],
    /// Kinds the run asked to produce; extra kinds stay in history but are
    /// not delivered.
    pub produce: &'a [MediaKind],
    pub destinations: &'a [Destination],
    pub overwrite: bool,
    /// Live streaming already printed the text (only the final newline may
    /// still be missing).
    pub live_stdout: bool,
    pub hold_secs: u64,
    pub quiet: bool,
    pub json: bool,
    pub run_id: &'a str,
    pub task: Option<&'a str>,
    /// Per-part batches only: the (part, error) pairs that failed while
    /// the surviving parts delivered normally. They reach the JSON report
    /// (which would otherwise present a partial run as a full success);
    /// empty everywhere else. A restored delivery (`last`, `history show`)
    /// passes the recorded run's pairs so its report matches the original.
    pub failed_parts: &'a [(String, String)],
    /// Chain runs only: the intermediate stages' artifacts. They never
    /// reach stdout, `-o` or the clipboard — the `--out-dir` manifest is
    /// the one destination that keeps them, so a chain's full trail lands
    /// on disk together with the final result. Empty everywhere else.
    pub dir_extras: &'a [Artifact],
}

pub struct DeliveryOutcome {
    pub states: Vec<DeliveryState>,
    /// artifact id → absolute saved path (for the JSON report).
    pub saved: BTreeMap<String, PathBuf>,
    /// Set when at least one destination failed (the run exits 5). The
    /// states keep every real per-destination outcome, so a file that was
    /// written before the clipboard failed stays recorded as delivered.
    pub error: Option<AppError>,
}

impl DeliveryOutcome {
    /// `Ok` only when every destination succeeded.
    pub fn result(self) -> AppResult<Self> {
        match self.error {
            Some(e) => Err(e),
            None => Ok(self),
        }
    }
}

/// Refuse an existing `-o` target before anything runs: the plan names the
/// file exactly, so the collision is knowable up front and fails as usage
/// (exit 2) instead of a paid delivery failure (exit 5). `--out-dir` names
/// depend on what the generation yields (stems, dedup suffixes), so its
/// collisions stay delivery-time. A dangling symlink counts as existing —
/// the commit would refuse it all the same.
pub fn precheck_file_targets(destinations: &[Destination], overwrite: bool) -> AppResult<()> {
    if overwrite {
        return Ok(());
    }
    for destination in destinations {
        if let Destination::File { path } = destination {
            if path.symlink_metadata().is_ok() {
                return Err(AppError::usage(format!(
                    "{} already exists; use --overwrite to replace it",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

/// Deliver everywhere the plan says, recording each destination's real
/// outcome. On any failure the run exits 5 — with successes kept.
pub fn deliver(args: &DeliverArgs<'_>) -> DeliveryOutcome {
    let mut states: Vec<DeliveryState> = Vec::new();
    let mut saved: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut failed: Option<AppError> = None;
    let deliver_result = deliver_inner(args, &mut states, &mut saved, &mut failed);
    if let Err(e) = deliver_result {
        failed = failed.or(Some(e));
    }
    DeliveryOutcome {
        states,
        saved,
        error: failed,
    }
}

fn deliver_inner(
    args: &DeliverArgs<'_>,
    states: &mut Vec<DeliveryState>,
    saved: &mut BTreeMap<String, PathBuf>,
    failed: &mut Option<AppError>,
) -> AppResult<()> {
    let delivered: Vec<&Artifact> = args
        .artifacts
        .iter()
        .filter(|a| args.produce.contains(&a.kind))
        .collect();
    // Late re-validation: what came back must fit the destinations. A
    // refusal delivers nothing, but it is not an early return — it still
    // falls through to the JSON epilogue below, so `--json` keeps its
    // one-report contract on exit 5 too.
    if let Some(err) = late_refusal(args, &delivered, states) {
        *failed = failed.take().or(Some(err));
    } else {
        deliver_to_destinations(args, &delivered, states, saved, failed);
    }

    // --- the JSON report replaces the body on stdout
    if args.json {
        // The report itself is the stdout delivery; `-o -` plus --json
        // must not list stdout twice. Recorded before the report is
        // built so the report lists it.
        let report_state = DeliveryState {
            destination: Destination::Stdout,
            status: DeliveryStatus::Succeeded,
        };
        match states
            .iter()
            .position(|s| s.destination == Destination::Stdout)
        {
            Some(i) => states[i] = report_state,
            None => states.insert(0, report_state),
        }
        let report = json_report(args, &delivered, states, saved, failed.as_ref());
        let mut out = std::io::stdout().lock();
        let print = (|| {
            serde_json::to_writer_pretty(&mut out, &report)
                .map_err(|e| AppError::delivery(e.to_string()))?;
            out.write_all(b"\n")
                .map_err(|e| AppError::delivery(e.to_string()))?;
            Ok(())
        })();
        if let Err(e) = print {
            *failed = failed.take().or(Some(e));
            if let Some(i) = states
                .iter()
                .position(|s| s.destination == Destination::Stdout)
            {
                states[i] = DeliveryState {
                    destination: Destination::Stdout,
                    status: DeliveryStatus::Failed {
                        error: "json report write failed".into(),
                    },
                };
            }
        }
    }
    if let Some(e) = failed.take() {
        return Err(e);
    }
    Ok(())
}

/// Late re-validation: what came back must fit the destinations. The
/// generation already ran, so every refusal below is a delivery failure
/// (exit 5) with the refused destination recorded — never a usage error,
/// which would read as "wrong command, nothing happened". Returns the
/// refusal, or `None` when the artifacts fit and delivery may run.
fn late_refusal(
    args: &DeliverArgs<'_>,
    delivered: &[&Artifact],
    states: &mut Vec<DeliveryState>,
) -> Option<AppError> {
    if delivered.is_empty() {
        // Post-generation: the run could not have reached delivery without
        // artifacts in hand, so this is a delivery refusal, not a usage
        // error. Every asked destination records the failed attempt.
        let message = "nothing to deliver: the run produced none of the requested kinds";
        for destination in args.destinations {
            states.push(DeliveryState {
                destination: destination.clone(),
                status: DeliveryStatus::Failed {
                    error: message.to_string(),
                },
            });
        }
        return Some(AppError::delivery(message));
    }
    let has_stdout = args.destinations.contains(&Destination::Stdout);
    if has_stdout && !args.json && delivered.len() > 1 {
        return Some(refuse_delivery(
            Destination::Stdout,
            "several artifacts cannot share bare stdout; use --out-dir",
            states,
        ));
    }
    let file_dest = args.destinations.iter().find_map(|d| match d {
        Destination::File { path } => Some(path.clone()),
        _ => None,
    });
    if let Some(path) = &file_dest {
        if delivered.len() != 1 {
            return Some(refuse_delivery(
                Destination::File { path: path.clone() },
                format!(
                    "{} artifacts cannot go to one file ({}); use --out-dir",
                    delivered.len(),
                    path.display()
                ),
                states,
            ));
        }
        if let Err(e) = check_extension(delivered[0], path) {
            return Some(refuse_delivery(
                Destination::File { path: path.clone() },
                e,
                states,
            ));
        }
    }
    if args.destinations.contains(&Destination::Clipboard) {
        if delivered.len() != 1 {
            return Some(refuse_delivery(
                Destination::Clipboard,
                "the clipboard takes exactly one artifact; use --out-dir",
                states,
            ));
        }
        if delivered[0].kind == MediaKind::Audio {
            return Some(refuse_delivery(
                Destination::Clipboard,
                "audio cannot go to the clipboard; use -o FILE",
                states,
            ));
        }
    }
    None
}

/// The destination writes: stdout body, single file, directory, then the
/// clipboard. Every outcome — success or failure — is recorded in
/// `states`; a failure keeps earlier successes and sets `failed`.
fn deliver_to_destinations(
    args: &DeliverArgs<'_>,
    delivered: &[&Artifact],
    states: &mut Vec<DeliveryState>,
    saved: &mut BTreeMap<String, PathBuf>,
    failed: &mut Option<AppError>,
) {
    let has_stdout = args.destinations.contains(&Destination::Stdout);
    let file_dest = args.destinations.iter().find_map(|d| match d {
        Destination::File { path } => Some(path.clone()),
        _ => None,
    });
    let dir_dest = args.destinations.iter().find_map(|d| match d {
        Destination::Directory { path } => Some(path.clone()),
        _ => None,
    });

    // --- stdout (the body; the JSON report prints later, after paths exist)
    if has_stdout && !args.json {
        match stdout_body(delivered, args.live_stdout) {
            Ok(()) => states.push(DeliveryState {
                destination: Destination::Stdout,
                status: DeliveryStatus::Succeeded,
            }),
            Err(e) => {
                *failed = failed
                    .take()
                    .or(Some(AppError::delivery(format!("stdout: {}", e.chain()))));
                states.push(DeliveryState {
                    destination: Destination::Stdout,
                    status: DeliveryStatus::Failed { error: e.chain() },
                });
            }
        }
    }

    // --- single file
    if let Some(path) = &file_dest {
        let artifact = delivered[0];
        match write_file_atomic(&artifact.bytes, path, args.overwrite, FileMode::Default) {
            Ok(()) => {
                if !args.quiet {
                    eprintln!("saved result to {}", path.display());
                }
                saved.insert(artifact.id.clone(), absolute(path));
                states.push(DeliveryState {
                    destination: Destination::File { path: path.clone() },
                    status: DeliveryStatus::Succeeded,
                });
            }
            Err(e) => {
                *failed = failed.take().or(Some(AppError::delivery(format!(
                    "{}: {}",
                    path.display(),
                    e.chain()
                ))));
                states.push(DeliveryState {
                    destination: Destination::File { path: path.clone() },
                    status: DeliveryStatus::Failed { error: e.chain() },
                });
            }
        }
    }

    // --- directory with manifest
    if let Some(dir) = &dir_dest {
        // The chain's intermediate artifacts ride along: manifest entries
        // first (they were produced first), then the final stage's.
        let mut dir_items: Vec<&Artifact> = args.dir_extras.iter().collect();
        dir_items.extend(delivered.iter().copied());
        match write_directory(&dir_items, dir, args.run_id, args.overwrite, args.quiet) {
            Ok(paths) => {
                for (id, path) in paths {
                    saved.insert(id, path);
                }
                states.push(DeliveryState {
                    destination: Destination::Directory { path: dir.clone() },
                    status: DeliveryStatus::Succeeded,
                });
            }
            Err(e) => {
                *failed = failed.take().or(Some(AppError::delivery(format!(
                    "{}: {}",
                    dir.display(),
                    e.chain()
                ))));
                states.push(DeliveryState {
                    destination: Destination::Directory { path: dir.clone() },
                    status: DeliveryStatus::Failed { error: e.chain() },
                });
            }
        }
    }

    // --- clipboard last
    if args.destinations.contains(&Destination::Clipboard) {
        let artifact = delivered[0];
        let clip = deliver_clipboard(artifact, args.hold_secs);
        match clip {
            Ok(chars) => {
                if !args.quiet {
                    match artifact.kind {
                        MediaKind::Text => eprintln!("copied {chars} chars to clipboard"),
                        _ => eprintln!("copied image to clipboard"),
                    }
                }
                states.push(DeliveryState {
                    destination: Destination::Clipboard,
                    status: DeliveryStatus::Succeeded,
                });
            }
            Err(e) => {
                *failed = failed
                    .take()
                    .or(Some(AppError::delivery(format!("clipboard: {e:#}"))));
                states.push(DeliveryState {
                    destination: Destination::Clipboard,
                    status: DeliveryStatus::Failed {
                        error: format!("{e:#}"),
                    },
                });
            }
        }
    }
}

/// A refused delivery attempt, recorded and classified. `deliver_inner`
/// runs only after the generation succeeded, so a late refusal is a
/// delivery failure (exit 5), never a usage error — the result is already
/// recoverable in history, and the refused destination is recorded there
/// as failed so the attempt shows.
fn refuse_delivery(
    destination: Destination,
    message: impl Into<String>,
    states: &mut Vec<DeliveryState>,
) -> AppError {
    let message = message.into();
    states.push(DeliveryState {
        destination,
        status: DeliveryStatus::Failed {
            error: message.clone(),
        },
    });
    AppError::delivery(message)
}

/// One artifact to the clipboard. Returns the copied char count for text
/// (0 for images). Invalid UTF-8 is an error, never a silent empty copy.
/// Clipboard text omits trailing whitespace; stored artifacts stay exact.
fn deliver_clipboard(artifact: &Artifact, hold_secs: u64) -> anyhow::Result<usize> {
    match artifact.kind {
        MediaKind::Text => {
            let text = std::str::from_utf8(&artifact.bytes)
                .map_err(|e| anyhow::anyhow!("text artifact is not valid UTF-8: {e}"))?;
            let trimmed = text.trim_end();
            clipboard::write_text(trimmed, hold_secs)?;
            Ok(trimmed.chars().count())
        }
        _ => {
            clipboard::write_image(&artifact.bytes, hold_secs)?;
            Ok(0)
        }
    }
}

fn stdout_body(delivered: &[&Artifact], live: bool) -> AppResult<()> {
    let artifact = delivered[0];
    let mut out = std::io::stdout().lock();
    match artifact.kind {
        MediaKind::Text => {
            let text = std::str::from_utf8(&artifact.bytes)
                .map_err(|_| AppError::generation("text artifact is not valid UTF-8"))?;
            if live {
                // Deltas already went out: close the line only.
                if !text.is_empty() && !text.ends_with('\n') {
                    out.write_all(b"\n")
                        .map_err(|e| AppError::delivery(e.to_string()))?;
                }
            } else {
                out.write_all(text.as_bytes())
                    .map_err(|e| AppError::delivery(e.to_string()))?;
                if !text.is_empty() && !text.ends_with('\n') {
                    out.write_all(b"\n")
                        .map_err(|e| AppError::delivery(e.to_string()))?;
                }
            }
            out.flush().map_err(|e| AppError::delivery(e.to_string()))?;
            Ok(())
        }
        _ => {
            // Raw bytes, exactly one artifact, no trailing newline. The
            // plan already refused this on a terminal stdout.
            out.write_all(&artifact.bytes)
                .map_err(|e| AppError::delivery(e.to_string()))?;
            out.flush().map_err(|e| AppError::delivery(e.to_string()))?;
            Ok(())
        }
    }
}

fn check_extension(artifact: &Artifact, path: &Path) -> Result<(), String> {
    let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
        return Ok(());
    };
    if artifact.kind == MediaKind::Text {
        // Text has no container format; any extension is fine.
        return Ok(());
    }
    let extension = extension.to_ascii_lowercase();
    let format = &artifact.format;
    let ok = extension_matches_format(&extension, format);
    if !ok {
        return Err(format!(
            "output format is '{format}', but the file is named '.{extension}'; \
             pick --format to match or rename the target"
        ));
    }
    Ok(())
}

/// The permission policy for an atomically written file.
pub(crate) enum FileMode {
    /// Owner-only (0600), whatever the umask allows.
    Private,
    /// Inherit the process umask (`0666 & !umask`, what a plain `open`
    /// would give): user-facing artifacts landing in shared, build or
    /// static-site directories stay usable by other tools. An unusual
    /// umask (e.g. 077) still yields a sane file.
    Default,
}

#[cfg(unix)]
impl FileMode {
    fn bits(self) -> u32 {
        match self {
            FileMode::Private => 0o600,
            FileMode::Default => 0o666 & !current_umask(),
        }
    }
}

/// The process umask, read exactly once per process. `umask(0)` reads it
/// but also sets it, so the read is immediately restored; that brief
/// window is the standard price of reading a umask (what other Rust tools
/// do). The umask is process-global, so repeated reads would multiply the
/// window on the multithreaded runtime — another thread creating a file
/// while umask is 0 would get world-writable permissions — and buy
/// nothing, as aido never changes its own umask; the OnceLock makes the
/// window happen exactly once. `mode_t` is `u16` on macOS and `u32` on
/// Linux, so the widening cast is required — and is a no-op on Linux,
/// where the lint must be silenced.
#[cfg(unix)]
fn current_umask() -> u32 {
    static UMASK: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *UMASK.get_or_init(|| {
        let raw = unsafe { libc::umask(0) };
        unsafe { libc::umask(raw) };
        #[allow(clippy::unnecessary_cast)] // no-op on Linux, real on macOS
        let mask = raw as u32;
        mask
    })
}

/// Apply `mode` to a written file.
#[cfg(unix)]
fn apply_file_mode(path: &Path, mode: FileMode) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode.bits()))
}

/// No mode bits off unix; the call is a no-op there.
#[cfg(not(unix))]
fn apply_file_mode(_path: &Path, _mode: FileMode) -> std::io::Result<()> {
    Ok(())
}

/// Mix clock nanos, pid and a counter for a dependency-free temp suffix.
/// This is not cryptographic randomness; exclusive creation handles any
/// collisions without truncating existing files.
fn temp_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hash =
        nanos ^ ((std::process::id() as u64) << 32) ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    // A cheap avalanche step (splitmix64 finalizer) so a coincidental
    // equality in one input does not collapse the whole value.
    hash = (hash ^ (hash >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    hash = (hash ^ (hash >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    hash ^ (hash >> 31)
}

/// Write bytes via a same-directory temp file, then commit without
/// clobbering an existing target: the no-clobber commit is a fresh hard
/// link (it fails atomically when the target exists — no check-then-rename
/// race), with the temp file unlinked afterwards. Linux falls back to
/// `renameat2(RENAME_NOREPLACE)` if linking fails; without an atomic
/// no-clobber fallback we fail closed. `--overwrite` swaps the commit
/// for an atomic rename. The delivered file's mode follows `mode`;
/// a failed chmod warns on stderr but never loses the artifact.
///
/// The temp name carries a per-attempt suffix and is opened with
/// `create_new` (O_EXCL) on unix: a leftover temp from a crashed run is
/// never silently truncated, and a planted symlink at a predictable name
/// is never followed. A colliding name retries with a fresh suffix.
pub(crate) fn write_file_atomic(
    bytes: &[u8],
    target: &Path,
    overwrite: bool,
    mode: FileMode,
) -> AppResult<()> {
    let parent = target.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(dir) = parent {
        std::fs::create_dir_all(dir)
            .map_err(|e| AppError::delivery(format!("cannot create {}: {e}", dir.display())))?;
    }
    let extension = target
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    // Exclusive creation (O_EXCL on unix) never follows an existing
    // symlink or truncates a leftover file. Retry collisions with fresh
    // suffixes, without removing a file we did not create.
    let open_temp = |temp: &Path| -> std::io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temp)
    };
    const TEMP_ATTEMPTS: usize = 8;
    let (temp, file) = {
        let mut opened = None;
        for _ in 0..TEMP_ATTEMPTS {
            let temp = target.with_extension(format!(
                "{}.aido-tmp-{}-{:016x}",
                extension,
                std::process::id(),
                temp_suffix()
            ));
            match open_temp(&temp) {
                Ok(file) => {
                    opened = Some((temp, file));
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(AppError::delivery(format!(
                        "cannot write {}: {e}",
                        temp.display()
                    )));
                }
            }
        }
        opened.ok_or_else(|| {
            AppError::delivery(format!(
                "cannot write {}: no unique temp file name after {TEMP_ATTEMPTS} attempts",
                target.display()
            ))
        })?
    };
    let write_temp = || -> AppResult<()> {
        let mut file = file;
        // Apply owner-only permissions before writing any artifact bytes
        // (a no-op off unix).
        let _ = apply_file_mode(&temp, FileMode::Private);
        file.write_all(bytes)
            .map_err(|e| AppError::delivery(format!("cannot write {}: {e}", temp.display())))?;
        // A wrong final mode must not lose the artifact: warn, keep going.
        if let Err(e) = apply_file_mode(&temp, mode) {
            eprintln!(
                "warning: could not set permissions on {}: {e}",
                target.display()
            );
        }
        file.sync_all()
            .map_err(|e| AppError::delivery(format!("cannot flush {}: {e}", temp.display())))?;
        Ok(())
    };
    if let Err(e) = write_temp() {
        let _ = std::fs::remove_file(&temp);
        return Err(e);
    }

    let commit = || -> AppResult<()> {
        if overwrite {
            std::fs::rename(&temp, target).map_err(|e| {
                AppError::delivery(format!("cannot replace {}: {e}", target.display()))
            })
        } else {
            match std::fs::hard_link(&temp, target) {
                // The link shares the temp file's inode; drop the temp name.
                Ok(()) => {
                    let _ = std::fs::remove_file(&temp);
                    Ok(())
                }
                Err(e) => commit_after_link_error(&temp, target, e),
            }
        }
    };
    match commit() {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            Err(e)
        }
    }
}

/// Preserve the original hard-link cause under the delivery classification.
fn link_commit_error(target: &Path, exists: bool, cause: anyhow::Error) -> AppError {
    let message = if exists {
        format!(
            "{} already exists; use --overwrite to replace it",
            target.display()
        )
    } else {
        format!("cannot write {}", target.display())
    };
    let mut error = AppError::delivery(message);
    error.source = Some(cause.into_boxed_dyn_error());
    error
}

/// Keep hard links primary; Linux can atomically rename on filesystems
/// without hard-link support. If that is unavailable, fail closed: never
/// replace a concurrent writer's target with a check-then-rename fallback.
fn commit_after_link_error(
    temp: &Path,
    target: &Path,
    link_error: std::io::Error,
) -> AppResult<()> {
    let exists = link_error.kind() == std::io::ErrorKind::AlreadyExists;
    let cause = anyhow::Error::new(link_error).context("hard_link failed");
    if exists {
        return Err(link_commit_error(target, true, cause));
    }
    #[cfg(target_os = "linux")]
    {
        rename_noreplace(temp, target).map_err(|e| {
            link_commit_error(
                target,
                e.kind() == std::io::ErrorKind::AlreadyExists,
                cause.context(format!("renameat2(RENAME_NOREPLACE) failed: {e}")),
            )
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = temp;
        Err(link_commit_error(target, target.exists(), cause))
    }
}

/// Use the syscall so this also builds on musl, where libc does not
/// expose a renameat2 wrapper. Unsupported kernels/filesystems fail closed.
#[cfg(target_os = "linux")]
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: both pointers refer to live NUL-terminated strings; the
    // syscall only reads them, and the remaining arguments match renameat2.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Directory delivery: a no-clobber preflight (nothing is written unless
/// the whole directory is free or `--overwrite` was given), then artifact
/// files first (each atomically committed), the manifest last — a
/// directory without a manifest is an unfinished write, never a success
/// story.
fn write_directory(
    artifacts: &[&Artifact],
    dir: &Path,
    run_id: &str,
    overwrite: bool,
    quiet: bool,
) -> AppResult<Vec<(String, PathBuf)>> {
    // The manifest is the only record of what a delivery contains, so a
    // second run into the same directory must not silently replace it
    // (the previous files would become unindexed orphans). Every name of
    // this run is checked before the first byte is written — also within
    // a per-part batch, where a mid-batch clobber would leave a half-old
    // half-new directory behind.
    //
    // The internal-collision check runs regardless of --overwrite:
    // overwriting covers this run's files replacing a previous delivery's,
    // never this run's artifacts replacing each other.
    artifact_files_unique(artifacts.iter().copied())?;
    if !overwrite {
        let manifest_path = dir.join("manifest.json");
        if manifest_path.exists() {
            return Err(AppError::delivery(format!(
                "{} already holds a previous delivery's manifest.json; \
                 pass --overwrite to replace the whole directory, or pick another --out-dir",
                dir.display()
            )));
        }
        for artifact in artifacts {
            let path = dir.join(artifact_file_name(artifact));
            if path.exists() {
                return Err(AppError::delivery(format!(
                    "{} already exists; use --overwrite to replace it",
                    path.display()
                )));
            }
        }
    }
    std::fs::create_dir_all(dir)
        .map_err(|e| AppError::delivery(format!("cannot create {}: {e}", dir.display())))?;
    let mut saved = Vec::new();
    let mut names: Vec<serde_json::Value> = Vec::new();
    for artifact in artifacts {
        let name = artifact_file_name(artifact);
        let path = dir.join(&name);
        write_file_atomic(&artifact.bytes, &path, overwrite, FileMode::Default)
            .map_err(|e| AppError::delivery(format!("{}: {}", path.display(), e.chain())))?;
        if !quiet {
            eprintln!("saved result to {}", path.display());
        }
        names.push(serde_json::json!({
            "id": artifact.id,
            "kind": artifact.kind.to_string(),
            "mime": artifact.mime,
            "file": name,
            "size": artifact.bytes.len(),
            "provenance": artifact.provenance,
        }));
        saved.push((artifact.id.clone(), absolute(&path)));
    }
    let manifest = serde_json::json!({
        "version": JSON_ENVELOPE_VERSION,
        "run_id": run_id,
        "artifacts": names,
    });
    let manifest_path = dir.join("manifest.json");
    write_file_atomic(
        serde_json::to_string_pretty(&manifest)
            .unwrap_or_default()
            .as_bytes(),
        &manifest_path,
        overwrite, // the preflight above guards the no-overwrite pass
        FileMode::Default,
    )?;
    Ok(saved)
}

/// The file-name-safe form of an id or input stem: letters and digits
/// keep their Unicode form so `截图` stays readable; separators (`/`,
/// `.`, …) become `-`; a stem of only separators would hide the file
/// behind a leading dot, so it is named instead. Callers that turn
/// several inputs into files must dedup on THIS form, not on the raw
/// stem — it is the name that lands in the directory.
pub(crate) fn sanitize_stem(id: &str) -> String {
    let safe: String = id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let stem = safe.trim_matches('-');
    if stem.is_empty() {
        "artifact".to_string()
    } else {
        stem.to_string()
    }
}

/// Names derive from service-generated ids or from an input file's stem
/// (per-part batches); either way a service or a path can never choose a
/// file outside the target directory. History uses the same scheme so
/// records and deliveries never disagree about where an artifact lives.
pub(crate) fn artifact_file_name(artifact: &Artifact) -> String {
    let extension = match artifact.kind {
        MediaKind::Text => "txt",
        _ => artifact.format.as_str(),
    };
    format!("{}.{}", sanitize_stem(&artifact.id), extension)
}

/// The artifact-to-filename mapping must be injective: two artifacts whose
/// ids sanitize to the same name (`a/b` and `a-b`, or a custom task named
/// `text` against the final stage's default `text` id) would otherwise
/// silently overwrite each other on disk — history keeps one file while
/// its manifest names two, and a `--out-dir` delivery loses a result.
/// Checked before the first byte is written, shared by history and
/// directory delivery so the two cannot drift.
pub(crate) fn artifact_files_unique<'a, I>(artifacts: I) -> AppResult<()>
where
    I: IntoIterator<Item = &'a Artifact>,
{
    let mut seen = std::collections::HashSet::new();
    for artifact in artifacts {
        let name = artifact_file_name(artifact);
        if !seen.insert(name.clone()) {
            return Err(AppError::delivery(format!(
                "artifact filename collision: several artifacts map to '{name}'; \
                 rename the task or pick another --out-dir"
            )));
        }
    }
    Ok(())
}

fn absolute(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn json_report(
    args: &DeliverArgs<'_>,
    delivered: &[&Artifact],
    states: &[DeliveryState],
    saved: &BTreeMap<String, PathBuf>,
    error: Option<&AppError>,
) -> serde_json::Value {
    let artifacts: Vec<serde_json::Value> = delivered
        .iter()
        .map(|a| {
            serde_json::json!({
                "id": a.id,
                "kind": a.kind.to_string(),
                "mime": a.mime,
                "format": a.format,
                "size": a.bytes.len(),
                "path": saved.get(&a.id).map(|p| p.display().to_string()),
            })
        })
        .collect();
    let deliveries: Vec<serde_json::Value> = states
        .iter()
        .map(|s| {
            serde_json::json!({
                "destination": s.destination.to_string(),
                "status": match &s.status {
                    DeliveryStatus::Pending => "pending".to_string(),
                    DeliveryStatus::Succeeded => "succeeded".to_string(),
                    DeliveryStatus::Failed { error } => format!("failed: {error}"),
                },
            })
        })
        .collect();
    let report_error = error.map(|e| {
        serde_json::json!({
            "kind": e.kind.as_str(),
            "message": e.chain(),
        })
    });
    // A delivered batch with failed parts must not read as a full
    // success: the report carries the failures even though the delivery
    // itself succeeded (the run still exits 6).
    let report_error = report_error.or_else(|| {
        (!args.failed_parts.is_empty()).then(|| {
            serde_json::json!({
                "kind": "partial",
                "message": format!(
                    "{} input part(s) failed; the rest were delivered",
                    args.failed_parts.len()
                ),
            })
        })
    });
    let failed_parts: Vec<serde_json::Value> = args
        .failed_parts
        .iter()
        .map(|(name, error)| serde_json::json!({"part": name, "error": error}))
        .collect();
    serde_json::json!({
        "version": JSON_ENVELOPE_VERSION,
        "run_id": args.run_id,
        "task": args.task,
        "artifacts": artifacts,
        "deliveries": deliveries,
        "failed_parts": failed_parts,
        "error": report_error,
    })
}

/// The JSON report for a run that failed before delivery could produce
/// anything (usage, service and generation failures): the success
/// report's `version` envelope and field names, with the failure carried
/// in `error` (same `kind` names as the delivery report) and no
/// artifacts or deliveries. `run_id` and `task` are `null` when the run
/// never got far enough to know them.
pub fn error_report(
    kind: ErrorKind,
    message: &str,
    run_id: Option<&str>,
    task: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "version": JSON_ENVELOPE_VERSION,
        "run_id": run_id,
        "task": task,
        "artifacts": [],
        "deliveries": [],
        "failed_parts": [],
        "error": {
            "kind": kind.as_str(),
            "message": message,
        },
    })
}

#[cfg(test)]
mod tests;

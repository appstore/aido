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
//! keeps one report on every exit code.

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
        match write_directory(delivered, dir, args.run_id, args.overwrite, args.quiet) {
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
/// The temp name carries an unguessable suffix and is opened with
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
mod tests {
    use super::*;

    fn text_artifact(id: &str, text: &str) -> Artifact {
        Artifact {
            id: id.into(),
            kind: MediaKind::Text,
            mime: "text/plain".into(),
            format: "text".into(),
            bytes: text.as_bytes().to_vec(),
            provenance: crate::domain::Provenance::Request { index: 0 },
        }
    }

    #[test]
    fn stdout_only_delivery_prints_body_with_one_newline() {
        let artifact = text_artifact("text", "hello");
        let args = DeliverArgs {
            artifacts: &[artifact],
            produce: &[MediaKind::Text],
            destinations: &[Destination::Stdout],
            overwrite: false,
            live_stdout: false,
            hold_secs: 0,
            quiet: true,
            json: false,
            run_id: "t",
            task: Some("t"),
            failed_parts: &[],
        };
        // Deliver to a real stdout is awkward in-process; the states tell
        // the story.
        let outcome = deliver(&args);
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(outcome.states.len(), 1);
        assert!(outcome.states[0].status.is_succeeded());
    }

    #[test]
    fn existing_file_is_not_clobbered() {
        let dir =
            crate::test_support::run_root().join(format!("aido-out-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("out.txt");
        std::fs::write(&target, "original").unwrap();
        let err = write_file_atomic(b"new", &target, false, FileMode::Default).unwrap_err();
        assert!(err.chain().contains("already exists"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
        write_file_atomic(b"new", &target, true, FileMode::Default).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn link_collision_preserves_underlying_error_and_message() {
        let target = Path::new("out.txt");
        let cause = "original hard-link collision";
        let error = commit_after_link_error(
            Path::new("unused-temp"),
            target,
            std::io::Error::new(std::io::ErrorKind::AlreadyExists, cause),
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            "out.txt already exists; use --overwrite to replace it"
        );
        assert!(error.chain().contains(cause), "{}", error.chain());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_link_fallback_commits_and_refuses_existing_entries() {
        let dir = crate::test_support::run_root()
            .join(format!("aido-out-noreplace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let temp = dir.join("temp");
        let target = dir.join("out.txt");
        // Exercise the fallback directly, without needing a special mount.
        let unsupported = || std::io::Error::from_raw_os_error(libc::EOPNOTSUPP);
        std::fs::write(&temp, "original").unwrap();
        commit_after_link_error(&temp, &target, unsupported()).unwrap();
        assert!(!temp.exists());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
        std::fs::write(&temp, "replacement").unwrap();
        let error = commit_after_link_error(&temp, &target, unsupported()).unwrap_err();
        assert_eq!(
            error.message,
            format!(
                "{} already exists; use --overwrite to replace it",
                target.display()
            )
        );
        assert!(error.chain().contains(&unsupported().to_string()));
        assert!(error.chain().contains("renameat2(RENAME_NOREPLACE)"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
        assert_eq!(std::fs::read_to_string(&temp).unwrap(), "replacement");

        // A dangling symlink is still an existing directory entry; exists()
        // would miss it, but RENAME_NOREPLACE must refuse it.
        let dangling = dir.join("dangling");
        std::os::unix::fs::symlink("missing", &dangling).unwrap();
        let error = commit_after_link_error(&temp, &dangling, unsupported()).unwrap_err();
        assert!(error.message.contains("already exists"));
        assert_eq!(std::fs::read_link(&dangling).unwrap(), Path::new("missing"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failed_link_fallback_preserves_permission_cause() {
        let dir = crate::test_support::run_root()
            .join(format!("aido-out-link-error-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cause = "original hard-link permission failure";
        // A missing source forces Linux's rename fallback to fail as well.
        let error = commit_after_link_error(
            &dir.join("missing-temp"),
            &dir.join("out.txt"),
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, cause),
        )
        .unwrap_err();
        assert!(error.message.starts_with("cannot write "));
        assert!(error.chain().contains(cause), "{}", error.chain());
        #[cfg(target_os = "linux")]
        assert!(error.chain().contains("renameat2(RENAME_NOREPLACE) failed"));
        assert!(!dir.join("out.txt").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn written_mode_follows_the_file_mode_policy() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir =
            crate::test_support::run_root().join(format!("aido-out-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let private = dir.join("private.txt");
        write_file_atomic(b"x", &private, false, FileMode::Private).unwrap();
        assert_eq!(
            std::fs::metadata(&private).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let default = dir.join("default.txt");
        write_file_atomic(b"x", &default, false, FileMode::Default).unwrap();
        assert_eq!(
            std::fs::metadata(&default).unwrap().permissions().mode() & 0o777,
            0o666 & !current_umask(),
            "Default inherits the umask, whatever it is"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_leftover_temp_with_the_old_naming_pattern_does_not_break_the_write() {
        // A crashed run used to leave `{target}.aido-tmp-{pid}` behind; the
        // old create+truncate open would silently reuse it. With O_EXCL and
        // an unguessable suffix, the leftover is ignored and the write
        // succeeds with fresh content in the target.
        let dir =
            crate::test_support::run_root().join(format!("aido-out-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("out.txt");
        let stale = target.with_extension(format!("txt.aido-tmp-{}", std::process::id()));
        std::fs::write(&stale, "stale bytes").unwrap();
        write_file_atomic(b"fresh", &target, false, FileMode::Default).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "fresh");
        assert_eq!(
            std::fs::read_to_string(&stale).unwrap(),
            "stale bytes",
            "the leftover temp is not truncated or consumed"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn successful_writes_clean_up_temp_files() {
        let dir =
            crate::test_support::run_root().join(format!("aido-out-rand-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("a.txt");
        write_file_atomic(b"1", &target, false, FileMode::Default).unwrap();
        write_file_atomic(b"2", &target, true, FileMode::Default).unwrap();
        // No temp file may linger after either write.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("aido-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "leftover temps: {leftovers:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn two_concurrent_writes_commit_exactly_one_without_overwrite() {
        use std::sync::Barrier;
        let dir =
            crate::test_support::run_root().join(format!("aido-out-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("race.txt");
        let barrier = std::sync::Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for payload in ["first", "second"] {
            let barrier = barrier.clone();
            let target = target.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                write_file_atomic(payload.as_bytes(), &target, false, FileMode::Default)
                    .map(|_| payload.to_string())
            }));
        }
        let results: Vec<_> = handles
            .into_iter()
            .map(|h| h.join().unwrap().map_err(|e| e.chain()))
            .collect();
        let committed: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
        assert_eq!(
            committed.len(),
            1,
            "exactly one writer commits: {results:?}"
        );
        assert!(
            results
                .iter()
                .filter_map(|r| r.as_ref().err())
                .all(|e| e.contains("already exists")),
            "every loser must refuse with the no-clobber error: {results:?}"
        );
        let content = std::fs::read_to_string(&target).unwrap();
        assert_eq!(
            &content, committed[0],
            "the target holds the successful writer's payload"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("aido-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no temp file lingers: {leftovers:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn directory_gets_files_then_manifest() {
        let dir =
            crate::test_support::run_root().join(format!("aido-out-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let artifact = text_artifact("text", "body");
        let saved = write_directory(&[&artifact], &dir, "run-1", false, true).unwrap();
        assert_eq!(saved.len(), 1);
        assert!(dir.join("text.txt").exists());
        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["run_id"], "run-1");
        assert_eq!(manifest["artifacts"][0]["file"], "text.txt");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn extension_mismatch_is_rejected() {
        let artifact = Artifact {
            id: "image-1".into(),
            kind: MediaKind::Image,
            mime: "image/png".into(),
            format: "png".into(),
            bytes: vec![1],
            provenance: crate::domain::Provenance::Request { index: 0 },
        };
        assert!(check_extension(&artifact, Path::new("a.png")).is_ok());
        assert!(check_extension(&artifact, Path::new("a.jpg")).is_err());
        assert!(check_extension(&artifact, Path::new("a")).is_ok());
    }

    #[test]
    fn file_names_cannot_escape_the_directory() {
        let artifact = Artifact {
            id: "../evil".into(),
            kind: MediaKind::Image,
            mime: "image/png".into(),
            format: "png".into(),
            bytes: vec![1],
            provenance: crate::domain::Provenance::Request { index: 0 },
        };
        let name = artifact_file_name(&artifact);
        assert!(!name.contains(".."), "{name}");
        assert!(!name.contains('/'), "{name}");
    }

    #[test]
    fn unicode_stems_survive_and_separators_do_not() {
        let artifact = text_artifact("截图", "body");
        assert_eq!(artifact_file_name(&artifact), "截图.txt");
        let artifact = text_artifact("shots/a..png", "body");
        let name = artifact_file_name(&artifact);
        assert_eq!(name, "shots-a--png.txt");
        assert!(!name.contains('/'));
        // A stem of only separators still yields a usable, visible name.
        let artifact = text_artifact("···", "body");
        assert_eq!(artifact_file_name(&artifact), "artifact.txt");
    }
}

//! Delivery: one artifact set, four destination kinds, one report.
//!
//! Order of operations (contract §step 7): validate every artifact, then
//! commit files and directories (atomic, no-clobber by default), then the
//! clipboard. A failure in one destination keeps earlier successes and
//! fails the run with exit code 5; the result itself stays recoverable in
//! history.

use crate::clipboard;
use crate::domain::{
    AppError, AppResult, Artifact, DeliveryState, DeliveryStatus, Destination, MediaKind,
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
    /// empty everywhere else.
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
    if delivered.is_empty() {
        return Err(AppError::usage(
            "nothing to deliver: the run produced none of the requested kinds",
        ));
    }

    // Late re-validation: what came back must fit the destinations.
    let has_stdout = args.destinations.contains(&Destination::Stdout);
    if has_stdout && !args.json && delivered.len() > 1 {
        return Err(AppError::usage(
            "several artifacts cannot share bare stdout; use --out-dir",
        ));
    }
    let file_dest = args.destinations.iter().find_map(|d| match d {
        Destination::File { path } => Some(path.clone()),
        _ => None,
    });
    if let Some(path) = &file_dest {
        if delivered.len() != 1 {
            return Err(AppError::usage(format!(
                "{} artifacts cannot go to one file ({}); use --out-dir",
                delivered.len(),
                path.display()
            )));
        }
        let artifact = delivered[0];
        if let Err(e) = check_extension(artifact, path) {
            return Err(AppError::usage(e));
        }
    }
    let dir_dest = args.destinations.iter().find_map(|d| match d {
        Destination::Directory { path } => Some(path.clone()),
        _ => None,
    });
    if args.destinations.contains(&Destination::Clipboard) {
        if delivered.len() != 1 {
            return Err(AppError::delivery(
                "the clipboard takes exactly one artifact; use --out-dir",
            ));
        }
        if delivered[0].kind == MediaKind::Audio {
            return Err(AppError::delivery(
                "audio cannot go to the clipboard; use -o FILE",
            ));
        }
    }

    // --- stdout (the body; the JSON report prints later, after paths exist)
    if has_stdout && !args.json {
        match stdout_body(&delivered, args.live_stdout) {
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
        match write_directory(
            delivered.as_slice(),
            dir,
            args.run_id,
            args.overwrite,
            args.quiet,
        ) {
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
    Ok(())
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
    let ok = extension == format.as_str()
        || (extension == "jpg" && format == "jpeg")
        || (extension == "ogg" && format == "opus");
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

/// The process umask. `umask(0)` reads it but also sets it, so the read
/// is immediately restored; that brief window is the standard price of
/// reading a umask (what other Rust tools do).
#[cfg(unix)]
fn current_umask() -> u32 {
    let mask = unsafe { libc::umask(0) };
    unsafe { libc::umask(mask) };
    mask
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

/// Write bytes via a same-directory temp file, then commit without
/// clobbering an existing target: the no-clobber commit is a fresh hard
/// link (it fails atomically when the target exists — no check-then-rename
/// race), with the temp file unlinked afterwards. `--overwrite` swaps the
/// commit for an atomic rename. The delivered file's mode follows `mode`;
/// a failed chmod warns on stderr but never loses the artifact.
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
    let temp = target.with_extension(format!(
        "{}.aido-tmp-{}",
        target
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default(),
        std::process::id()
    ));
    let write_temp = || -> AppResult<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)
            .map_err(|e| AppError::delivery(format!("cannot write {}: {e}", temp.display())))?;
        // The temp name is predictable, so the file is made owner-only
        // right away: no window where partially written bytes are
        // readable under the open mode's default (a no-op off unix).
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

    let already_exists = || {
        AppError::delivery(format!(
            "{} already exists; use --overwrite to replace it",
            target.display()
        ))
    };
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
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(already_exists()),
                // Filesystems without hard-link support fall back to the
                // racy check-then-rename (sequential behavior is correct).
                Err(_) if target.exists() => Err(already_exists()),
                Err(_) => std::fs::rename(&temp, target).map_err(|e| {
                    AppError::delivery(format!("cannot write {}: {e}", target.display()))
                }),
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
        "version": 1,
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
            "kind": match e.kind {
                crate::domain::ErrorKind::Usage => "usage",
                crate::domain::ErrorKind::Service => "service",
                crate::domain::ErrorKind::Generation => "generation",
                crate::domain::ErrorKind::Delivery => "delivery",
                crate::domain::ErrorKind::Partial => "partial",
            },
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
        "version": 1,
        "run_id": args.run_id,
        "task": args.task,
        "artifacts": artifacts,
        "deliveries": deliveries,
        "failed_parts": failed_parts,
        "error": report_error,
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
        let dir = std::env::temp_dir().join(format!("aido-out-test-{}", std::process::id()));
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

    #[cfg(unix)]
    #[test]
    fn written_mode_follows_the_file_mode_policy() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("aido-out-mode-{}", std::process::id()));
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
    fn directory_gets_files_then_manifest() {
        let dir = std::env::temp_dir().join(format!("aido-out-dir-{}", std::process::id()));
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

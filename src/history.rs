//! History: one directory per run, holding the manifest and the raw
//! artifacts. A generation is saved before delivery and updated with each
//! destination's outcome, so "generated fine but the clipboard failed" is
//! recoverable without asking the model again.
//!
//! Pre-2.0 entries (flat `.txt` and `.json` files) are read lazily and
//! never rewritten.

use crate::domain::{
    Artifact, DeliveryState, GenerationStatus, MediaKind, Provenance, RunRecord, RunSummary,
    JSON_ENVELOPE_VERSION,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_KEEP: usize = 50;
pub const DEFAULT_HISTORY_BYTES: u64 = 512 * 1024 * 1024;

/// Where runs live. AIDO_HISTORY_DIR overrides the platform default.
pub fn history_dir() -> Option<PathBuf> {
    std::env::var("AIDO_HISTORY_DIR")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::data_local_dir().map(|d| d.join("aido").join("history")))
}

/// The on-disk shape: artifact bytes live in sibling files, never inline.
#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    run_id: String,
    task: Option<String>,
    created_at: String,
    #[serde(default)]
    summary: RunSummary,
    generation: GenerationStatus,
    #[serde(default)]
    warnings: Vec<String>,
    /// Per-part batches only: the structured (part, error) pairs. Defaults
    /// keep manifests written before this field parsing.
    #[serde(default)]
    failed_parts: Vec<(String, String)>,
    /// Total parts of the per-part batch (0 outside one).
    #[serde(default)]
    parts_total: usize,
    #[serde(default)]
    deliveries: Vec<DeliveryState>,
    /// Chain runs only: one summary per stage in run order. Defaults keep
    /// manifests written before this field parsing.
    #[serde(default)]
    stages: Vec<RunSummary>,
    /// Chain runs only: how many artifacts, counting from the end, belong
    /// to the final stage (the redeliverable slice). 0 = pre-chain shape.
    #[serde(default)]
    last_stage_len: usize,
    #[serde(default)]
    artifacts: Vec<ManifestArtifact>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ManifestArtifact {
    id: String,
    kind: MediaKind,
    mime: String,
    format: String,
    file: String,
    size: u64,
    /// Which request(s) produced this artifact. Absent in records written
    /// before 0.3.0 (the field is a 0.3.0 additive, mirroring the out-dir
    /// manifest's provenance); older records load as Restored.
    #[serde(default)]
    provenance: Option<Provenance>,
}

/// Reserve a sortable millisecond run id when history is writable.
/// Reservation failures warn but do not prevent generation; saving history
/// remains best-effort.
pub fn new_run_id() -> String {
    new_run_id_in(history_dir().as_deref())
}

fn new_run_id_in(dir: Option<&Path>) -> String {
    let mut elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let Some(d) = dir else {
        return stamp(elapsed);
    };
    if let Err(e) = std::fs::create_dir_all(d) {
        eprintln!(
            "warning: cannot create history directory {}: {e}",
            d.display()
        );
        return stamp(elapsed);
    }
    loop {
        let id = stamp(elapsed);
        match std::fs::create_dir(d.join(&id)) {
            Ok(()) => return id,
            // Lost a millisecond-stamp race with another process: bump.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                elapsed += Duration::from_millis(1);
            }
            Err(e) => {
                eprintln!(
                    "warning: cannot reserve a history run directory {}: {e}",
                    d.join(&id).display()
                );
                return id;
            }
        }
    }
}

/// A run id that creates nothing (used when history is off and the id
/// only labels a report). Legacy flat entries are still avoided.
pub fn stamp_now() -> String {
    let dir = history_dir();
    let mut elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    loop {
        let id = stamp(elapsed);
        let taken = dir
            .as_ref()
            .is_some_and(|d| d.join(&id).exists() || d.join(format!("{id}.txt")).exists());
        if !taken {
            return id;
        }
        elapsed += Duration::from_millis(1);
    }
}

/// Save a finished generation (before delivery). Artifacts are saved for
/// complete runs; `keep_artifacts` extends that to runs whose generation
/// finished cleanly but did not satisfy the request (e.g. a short image
/// count) — their bytes stay in the clearly-marked record instead of
/// being dropped. Truncated streams keep metadata only: partial text is
/// never a "recoverable" artifact.
pub fn save_generation(record: &RunRecord, keep_artifacts: bool) -> Result<()> {
    let Some(dir) = history_dir() else {
        eprintln!("warning: cannot determine a history directory; result not kept on disk");
        return Ok(());
    };
    // Two artifacts that sanitize to the same file name would overwrite
    // each other while the manifest still names both — refuse before the
    // first byte lands, so the record never lies about what is on disk.
    crate::output::artifact_files_unique(&record.artifacts)?;
    let run_dir = dir.join(&record.run_id);
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("cannot create {}", run_dir.display()))?;
    #[cfg(unix)]
    set_mode(&run_dir, 0o700);

    let mut artifacts = Vec::new();
    if record.generation.is_complete() || keep_artifacts {
        for artifact in &record.artifacts {
            let file = crate::output::artifact_file_name(artifact);
            let path = run_dir.join(&file);
            std::fs::write(&path, &artifact.bytes)
                .with_context(|| format!("failed to write {}", path.display()))?;
            #[cfg(unix)]
            set_mode(&path, 0o600);
            artifacts.push(ManifestArtifact {
                id: artifact.id.clone(),
                kind: artifact.kind,
                mime: artifact.mime.clone(),
                format: artifact.format.clone(),
                file,
                size: artifact.bytes.len() as u64,
                provenance: Some(artifact.provenance.clone()),
            });
        }
    }
    let manifest = Manifest {
        version: JSON_ENVELOPE_VERSION,
        run_id: record.run_id.clone(),
        task: record.task.clone(),
        created_at: record.created_at.clone(),
        summary: record.summary.clone(),
        generation: record.generation.clone(),
        warnings: record.warnings.clone(),
        failed_parts: record.failed_parts.clone(),
        parts_total: record.parts_total,
        deliveries: Vec::new(),
        stages: record.stages.clone(),
        last_stage_len: record.last_stage_len,
        artifacts,
    };
    write_manifest(&run_dir, &manifest)
}

/// Update the delivery states after the run finished delivering.
pub fn update_deliveries(record: &RunRecord) -> Result<()> {
    let Some(dir) = history_dir() else {
        return Ok(());
    };
    let run_dir = dir.join(&record.run_id);
    if !run_dir.join("manifest.json").exists() {
        // A run that never saved its generation has nothing to update.
        return Ok(());
    }
    let mut manifest = read_manifest(&run_dir)?;
    manifest.deliveries = record.deliveries.clone();
    write_manifest(&run_dir, &manifest)
}

fn write_manifest(run_dir: &Path, manifest: &Manifest) -> Result<()> {
    // The manifest commits last: a run dir without one is unfinished.
    // The temp file is fsynced so a crash cannot leave an empty or
    // truncated manifest behind a successful rename.
    let tmp = run_dir.join("manifest.json.tmp");
    {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&tmp)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        file.write_all(&serde_json::to_vec_pretty(manifest)?)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("cannot flush {}", tmp.display()))?;
    }
    #[cfg(unix)]
    set_mode(&tmp, 0o600);
    std::fs::rename(&tmp, run_dir.join("manifest.json"))
        .with_context(|| format!("cannot commit the run manifest in {}", run_dir.display()))
}

fn read_manifest(run_dir: &Path) -> Result<Manifest> {
    let raw = std::fs::read_to_string(run_dir.join("manifest.json"))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("invalid run manifest in {}", run_dir.display()))
}

/// The run's directory and manifest, when the run exists: the shared front
/// half of [`load`] and [`load_meta`]. `Ok(None)` when there is no history
/// dir or the run dir has no manifest (unfinished or in-flight).
fn open_manifest(run_id: &str) -> Result<Option<(PathBuf, Manifest)>> {
    let Some(dir) = history_dir() else {
        return Ok(None);
    };
    let run_dir = dir.join(run_id);
    if !run_dir.join("manifest.json").exists() {
        return Ok(None);
    }
    let manifest = read_manifest(&run_dir)?;
    Ok(Some((run_dir, manifest)))
}

/// Load one run, artifact bytes included.
pub fn load(run_id: &str) -> Result<Option<RunRecord>> {
    let Some((run_dir, manifest)) = open_manifest(run_id)? else {
        return Ok(None);
    };
    let mut artifacts = Vec::new();
    for meta in &manifest.artifacts {
        let path = run_dir.join(&meta.file);
        let bytes = std::fs::read(&path)
            .with_context(|| format!("cannot read the recorded artifact {}", path.display()))?;
        artifacts.push(Artifact {
            id: meta.id.clone(),
            kind: meta.kind,
            mime: meta.mime.clone(),
            format: meta.format.clone(),
            bytes,
            // Records written before 0.3.0 carry no provenance: their
            // artifacts were not produced in this process, so Restored is
            // the honest answer.
            provenance: meta.provenance.clone().unwrap_or(Provenance::Restored),
        });
    }
    Ok(Some(RunRecord {
        run_id: manifest.run_id,
        task: manifest.task,
        created_at: manifest.created_at,
        summary: manifest.summary,
        generation: manifest.generation,
        artifacts,
        warnings: manifest.warnings,
        failed_parts: manifest.failed_parts,
        parts_total: manifest.parts_total,
        deliveries: manifest.deliveries,
        stages: manifest.stages,
        last_stage_len: manifest.last_stage_len,
    }))
}

/// The manifest-only view of one run: what `history list` labels need,
/// with no artifact bytes read. A run whose artifacts are damaged still
/// lists normally — its manifest is the record of what happened.
pub struct RunMeta {
    pub task: Option<String>,
    pub generation: GenerationStatus,
    pub failed_parts: usize,
    pub parts_total: usize,
}

/// One run's metadata without its artifact bytes. Same existence and
/// error semantics as [`load`]: `Ok(None)` when the run dir has no
/// manifest, `Err` when it cannot be read or parsed.
pub fn load_meta(run_id: &str) -> Result<Option<RunMeta>> {
    let Some((_, manifest)) = open_manifest(run_id)? else {
        return Ok(None);
    };
    Ok(Some(RunMeta {
        failed_parts: manifest.failed_parts.len(),
        parts_total: manifest.parts_total,
        task: manifest.task,
        generation: manifest.generation,
    }))
}

/// The most recent complete generation. A damaged entry is skipped (with
/// a note on stderr) instead of bricking recovery: older complete runs
/// stay reachable.
pub fn last_complete() -> Result<Option<RunRecord>> {
    let Some(dir) = history_dir() else {
        return Ok(None);
    };
    let mut ids = all_ids(&dir)?;
    ids.reverse(); // newest first
    for id in ids {
        let record = match load(&id) {
            Ok(record) => record,
            Err(e) => {
                eprintln!("warning: skipping unreadable history entry {id}: {e:#}");
                continue;
            }
        };
        if let Some(record) = record {
            if record.generation.is_complete() {
                return Ok(Some(record));
            }
        }
    }
    Ok(None)
}

/// Every recorded run id, oldest first.
pub fn list_ids() -> Result<Vec<String>> {
    match history_dir() {
        Some(dir) => all_ids(&dir),
        None => Ok(Vec::new()),
    }
}

fn all_ids(dir: &Path) -> Result<Vec<String>> {
    let mut ids: Vec<String> = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        other => other.with_context(|| format!("cannot read {}", dir.display()))?,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_stamp(name) {
            continue;
        }
        // Dirs without a manifest are unfinished or in-flight: not listed.
        if !path.join("manifest.json").exists() {
            continue;
        }
        ids.push(name.to_string());
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

fn is_stamp(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 19 {
        return false;
    }
    b[..8].iter().all(|c| c.is_ascii_digit())
        && b[8] == b'-'
        && b[9..15].iter().all(|c| c.is_ascii_digit())
        && b[15] == b'.'
        && b[16..].iter().all(|c| c.is_ascii_digit())
}

/// Enforce both budgets: newest runs first, older ones removed. In-flight
/// dirs (no manifest) are never touched by the budgets — but long-abandoned
/// ones (crashed before their manifest committed) are reclaimed.
pub fn prune(keep: usize, budget: u64) {
    let Some(dir) = history_dir() else {
        return;
    };
    reclaim_abandoned(&dir);
    let Ok(mut ids) = all_ids(&dir) else {
        return;
    };
    // Oldest first; drop from the front.
    while ids.len() > keep {
        let oldest = ids.remove(0);
        remove_run(&dir, &oldest);
    }
    // Byte budget over the remaining runs. The newest entry is always
    // kept: it is the run just saved, and the recovery promise ("a failed
    // clipboard write is recoverable via `aido last`") depends on it —
    // even when a single artifact exceeds the whole budget.
    let mut entries: Vec<(String, u64)> = ids
        .into_iter()
        .map(|id| {
            let size = dir_size(&dir.join(&id));
            (id, size)
        })
        .collect();
    entries.sort(); // stamp order
    let mut total: u64 = entries.iter().map(|(_, size)| size).sum();
    while total > budget && entries.len() > 1 {
        let (oldest, size) = entries.remove(0);
        total = total.saturating_sub(size);
        remove_run(&dir, &oldest);
    }
}

/// Reclaim manifest-less directories after 24 hours without filesystem
/// activity. This is an age heuristic, not a liveness check: an active
/// request that writes nothing for that long is indistinguishable from
/// abandoned work.
fn reclaim_abandoned(dir: &Path) {
    const ABANDONED_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_stamp(name) || path.join("manifest.json").exists() {
            continue;
        }
        let newest = match newest_file_mtime(&path, 0) {
            // No files at all: the dir's own mtime is the fallback.
            Ok(None) => entry.metadata().and_then(|m| m.modified()).ok(),
            Ok(Some(mtime)) => entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .map(|dir_mtime| dir_mtime.max(mtime)),
            // An unreadable run dir is kept: reclaim only removes
            // provable litter, never a run it could not inspect.
            Err(_) => None,
        };
        let age_ok = newest
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age > ABANDONED_AFTER);
        if age_ok {
            remove_run(dir, name);
        }
    }
}

/// Scan files, not directory mtimes; `Ok(None)` means no files were found.
/// Run dirs are normally flat. Allow three nested directories, but retain
/// a run if the walk cannot finish (errors or excess depth), rather than
/// mistaking an unobserved recent file for abandoned litter. DirEntry's
/// metadata does not follow symlinks, so we never recurse through them.
fn newest_file_mtime(dir: &Path, depth: u8) -> std::io::Result<Option<SystemTime>> {
    const MAX_DEPTH: u8 = 3;
    if depth > MAX_DEPTH {
        return Err(std::io::Error::other("history scan depth exceeded"));
    }
    let mut newest: Option<SystemTime> = None;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let mtime = if meta.is_dir() {
            newest_file_mtime(&entry.path(), depth + 1)?
        } else {
            Some(meta.modified()?)
        };
        if let Some(mtime) = mtime {
            newest = Some(newest.map_or(mtime, |n| n.max(mtime)));
        }
    }
    Ok(newest)
}

/// The mtime `path` would report, for tests that fake age.
#[cfg(all(test, unix))]
fn set_mtime(path: &Path, mtime: SystemTime) {
    use std::fs::FileTimes;
    let file = std::fs::File::open(path).expect("open for set_mtime");
    file.set_times(FileTimes::new().set_modified(mtime))
        .expect("set mtime");
}

fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

fn remove_run(dir: &Path, id: &str) {
    let run_dir = dir.join(id);
    if let Err(e) = std::fs::remove_dir_all(&run_dir) {
        eprintln!("warning: failed to prune history entry {id}: {e}");
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

/// Milliseconds since the epoch as a sortable stamp.
fn stamp(elapsed: Duration) -> String {
    let secs = elapsed.as_secs() as i64;
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let tod = secs.rem_euclid(86_400);
    format!(
        "{y:04}{mo:02}{d:02}-{:02}{:02}{:02}.{:03}",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60,
        elapsed.subsec_millis()
    )
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20705), (2026, 9, 9));
        assert_eq!(civil_from_days(11016), (2000, 2, 29)); // leap day
        assert_eq!(civil_from_days(11017), (2000, 3, 1));
    }

    #[test]
    fn stamps_sort_as_written() {
        let a = stamp(Duration::from_millis(1_788_912_000_123));
        let b = stamp(Duration::from_millis(1_788_912_000_999));
        let c = stamp(Duration::from_millis(1_788_912_001_000));
        assert_eq!(a, "20260909-000000.123");
        assert_eq!(b, "20260909-000000.999");
        assert_eq!(c, "20260909-000001.000");
        assert!(a < b && b < c);
    }

    #[test]
    fn only_stamped_names_are_entries() {
        assert!(is_stamp("20260909-153012.123"));
        assert!(!is_stamp("notes"));
        assert!(!is_stamp("20260909-153012"));
    }

    /// The manifest round-trip is what save/load actually do: write_manifest
    /// serializes the Manifest (its artifacts included), read_manifest parses
    /// it back. Provenance must survive that loop, and records written
    /// before the field existed must still parse (as None — `load` then
    /// answers Restored).
    #[test]
    fn manifest_artifact_round_trips_provenance() {
        let manifest = Manifest {
            version: 1,
            run_id: "20260909-153012.123".into(),
            task: None,
            created_at: "2026-09-09T15:30:12Z".into(),
            summary: Default::default(),
            generation: GenerationStatus::Complete,
            warnings: Vec::new(),
            failed_parts: Vec::new(),
            parts_total: 0,
            deliveries: Vec::new(),
            stages: Vec::new(),
            last_stage_len: 0,
            artifacts: vec![ManifestArtifact {
                id: "text".into(),
                kind: MediaKind::Text,
                mime: "text/plain".into(),
                format: "text".into(),
                file: "text.txt".into(),
                size: 5,
                provenance: Some(Provenance::Request { index: 2 }),
            }],
        };
        let raw = serde_json::to_string(&manifest).unwrap();
        let back: Manifest = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            back.artifacts[0].provenance,
            Some(Provenance::Request { index: 2 })
        );

        // A pre-0.3.0 record: the same entry without a provenance field.
        let old = r#"{"version":1,"run_id":"20260909-153012.123","task":null,
            "created_at":"2026-09-09T15:30:12Z","generation":{"status":"complete"},
            "artifacts":[{"id":"text","kind":"text","mime":"text/plain",
            "format":"text","file":"text.txt","size":5}]}"#;
        let old: Manifest = serde_json::from_str(old).unwrap();
        assert_eq!(old.artifacts[0].provenance, None);
    }

    #[test]
    fn reservation_creates_missing_parents_and_unique_directories() {
        let dir = crate::test_support::run_root()
            .join("hist-reserve")
            .join("nested");
        let first = new_run_id_in(Some(&dir));
        let second = new_run_id_in(Some(&dir));
        assert_ne!(first, second);
        assert!(dir.join(first).is_dir());
        assert!(dir.join(second).is_dir());
    }

    #[test]
    fn reservation_failure_keeps_generation_possible() {
        let file = crate::test_support::run_root().join("hist-reserve-file");
        std::fs::write(&file, b"keep").unwrap();
        assert!(is_stamp(&new_run_id_in(Some(&file))));
        assert_eq!(std::fs::read(file).unwrap(), b"keep");
        assert!(is_stamp(&new_run_id_in(None)));
    }

    #[cfg(unix)]
    #[test]
    fn reclaim_keeps_an_old_dir_with_a_recently_written_file() {
        let dir = crate::test_support::run_root().join("hist-reclaim-recent");
        std::fs::create_dir_all(&dir).unwrap();
        let run = dir.join("20260101-000000.000");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("image.png.tmp"), b"partial").unwrap();
        // The dir itself (and its litter) is well over the age limit.
        let old = SystemTime::now() - Duration::from_secs(48 * 3600);
        set_mtime(&dir, old);
        set_mtime(&run, old);

        reclaim_abandoned(&dir);
        assert!(
            run.is_dir(),
            "an in-flight run with fresh writes must survive reclaim"
        );
    }

    /// A dead litter dir whose files stopped refreshing is still litter:
    /// all file mtimes (not just the dir's) are old, so it is reclaimed.
    #[cfg(unix)]
    #[test]
    fn reclaim_still_removes_an_old_dir_with_only_old_files() {
        let dir = crate::test_support::run_root().join("hist-reclaim-dead");
        std::fs::create_dir_all(&dir).unwrap();
        let run = dir.join("20260101-000000.000");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("image.png.tmp"), b"partial").unwrap();
        let old = SystemTime::now() - Duration::from_secs(48 * 3600);
        set_mtime(&dir, old);
        set_mtime(&run, old);
        set_mtime(&run.join("image.png.tmp"), old);

        reclaim_abandoned(&dir);
        assert!(!run.exists(), "a dead litter dir must still be reclaimed");
    }

    /// With no files at all there is no file mtime to read: the dir's own
    /// mtime is the fallback. Old empty litter goes; fresh empty dirs stay.
    #[cfg(unix)]
    #[test]
    fn reclaim_falls_back_to_dir_mtime_when_empty() {
        let dir = crate::test_support::run_root().join("hist-reclaim-empty");
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("20260101-000000.000");
        let fresh = dir.join("20260101-000000.001");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&fresh).unwrap();
        set_mtime(&old, SystemTime::now() - Duration::from_secs(48 * 3600));

        reclaim_abandoned(&dir);
        assert!(!old.exists(), "an old empty dir must still be reclaimed");
        assert!(fresh.exists(), "a fresh empty dir must survive reclaim");
    }

    /// The fresh-mtime scan is recursive: a recent write in a nested
    /// directory (unexpected for a run dir, but possible) also protects it.
    #[cfg(unix)]
    #[test]
    fn reclaim_reads_nested_files() {
        let dir = crate::test_support::run_root().join("hist-reclaim-nested");
        std::fs::create_dir_all(&dir).unwrap();
        let run = dir.join("20260101-000000.000");
        let nested = run.join("parts").join("0");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("chunk.txt"), b"recent").unwrap();
        let old = SystemTime::now() - Duration::from_secs(48 * 3600);
        set_mtime(&dir, old);
        set_mtime(&run, old);
        set_mtime(&nested, old);

        reclaim_abandoned(&dir);
        assert!(run.is_dir(), "a nested recent write must protect the run");
    }
}

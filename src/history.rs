//! History: one directory per run, holding the manifest and the raw
//! artifacts. A generation is saved before delivery and updated with each
//! destination's outcome, so "generated fine but the clipboard failed" is
//! recoverable without asking the model again.
//!
//! Pre-2.0 entries (flat `.txt` and `.json` files) are read lazily and
//! never rewritten.

use crate::domain::{
    Artifact, DeliveryState, GenerationStatus, MediaKind, Provenance, RunRecord, RunSummary,
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
    #[serde(default)]
    deliveries: Vec<DeliveryState>,
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
}

/// A fresh run id (sortable, millisecond resolution).
pub fn new_run_id() -> String {
    let mut elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let dir = history_dir();
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

/// Save a finished generation (before delivery). Artifacts are only saved
/// for runs whose generation completed: a truncated or failed run keeps
/// its metadata for diagnosis, never a "recoverable" partial binary.
pub fn save_generation(record: &RunRecord) -> Result<()> {
    let Some(dir) = history_dir() else {
        eprintln!("warning: cannot determine a history directory; result not kept on disk");
        return Ok(());
    };
    let run_dir = dir.join(&record.run_id);
    std::fs::create_dir_all(&run_dir)
        .with_context(|| format!("cannot create {}", run_dir.display()))?;
    #[cfg(unix)]
    set_mode(&run_dir, 0o700);

    let mut artifacts = Vec::new();
    if record.generation.is_complete() {
        for artifact in &record.artifacts {
            let file = format!("{}.{}", artifact.id, extension_for(artifact));
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
            });
        }
    }
    let manifest = Manifest {
        version: 1,
        run_id: record.run_id.clone(),
        task: record.task.clone(),
        created_at: record.created_at.clone(),
        summary: record.summary.clone(),
        generation: record.generation.clone(),
        warnings: record.warnings.clone(),
        deliveries: Vec::new(),
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
    let tmp = run_dir.join("manifest.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(manifest)?)?;
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

/// Load one run, artifact bytes included.
pub fn load(run_id: &str) -> Result<Option<RunRecord>> {
    let Some(dir) = history_dir() else {
        return Ok(None);
    };
    let run_dir = dir.join(run_id);
    if !run_dir.join("manifest.json").exists() {
        return Ok(None);
    }
    let manifest = read_manifest(&run_dir)?;
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
            provenance: Provenance::Restored,
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
        deliveries: manifest.deliveries,
    }))
}

/// The most recent complete generation.
pub fn last_complete() -> Result<Option<RunRecord>> {
    let Some(dir) = history_dir() else {
        return Ok(None);
    };
    let mut ids = all_ids(&dir)?;
    ids.reverse(); // newest first
    for id in ids {
        if let Some(record) =
            load(&id).with_context(|| format!("cannot read the history entry {id}"))?
        {
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
/// dirs (no manifest) are never touched.
pub fn prune(keep: usize, budget: u64) {
    let Some(dir) = history_dir() else {
        return;
    };
    let Ok(mut ids) = all_ids(&dir) else {
        return;
    };
    // Oldest first; drop from the front.
    while ids.len() > keep {
        let oldest = ids.remove(0);
        remove_run(&dir, &oldest);
    }
    // Byte budget over the remaining runs.
    let mut total = 0u64;
    let mut dirs: Vec<PathBuf> = ids.iter().map(|id| dir.join(id)).collect();
    dirs.sort(); // stamp order
    for path in &dirs {
        total += dir_size(path);
    }
    while total > budget && !dirs.is_empty() {
        let oldest = dirs.remove(0);
        total = total.saturating_sub(dir_size(&oldest));
        remove_run(&dir, &oldest.to_string_lossy());
    }
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

/// Run directories are `YYYYMMDD-HHMMSS.mmm`; only such directories belong
/// to the history — anything else in the dir is the user's.
fn extension_for(artifact: &Artifact) -> String {
    match artifact.kind {
        MediaKind::Text => "txt".into(),
        _ => artifact.format.clone(),
    }
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
}

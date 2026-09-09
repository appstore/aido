use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How many results to keep when settings.history_keep is unset.
pub const DEFAULT_KEEP: usize = 50;

/// Where past results live. AIDO_HISTORY_DIR overrides the platform
/// default (tests use it to avoid touching real user data).
pub fn history_dir() -> Option<PathBuf> {
    std::env::var("AIDO_HISTORY_DIR")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::data_local_dir().map(|d| d.join("aido").join("history")))
}

/// Persist a result and prune old entries. Best-effort: the result has
/// already been produced, so a history failure is reported on stderr but
/// never fails the run.
pub fn record(text: &str, keep: usize) {
    if keep == 0 || text.trim().is_empty() {
        return;
    }
    let Some(dir) = history_dir() else {
        eprintln!("warning: cannot determine a history directory; result not kept on disk");
        return;
    };
    if let Err(e) = save_entry(&dir, text) {
        eprintln!("warning: failed to save the result to history: {e:#}");
        return;
    }
    prune(&dir, keep);
}

/// Read the most recent saved result, if there is one.
pub fn last_result() -> Result<Option<crate::api::GenerateResult>> {
    let Some(dir) = history_dir() else {
        return Ok(None);
    };
    let Some(path) = entries(&dir)?.pop() else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    if path.extension().is_some_and(|e| e == "json") {
        let result: crate::api::GenerateResult =
            serde_json::from_str(&text).context("invalid media history entry")?;
        for artifact in &result.artifacts {
            artifact.validate()?;
        }
        Ok(Some(result))
    } else {
        Ok(Some(crate::api::GenerateResult {
            text,
            ..Default::default()
        }))
    }
}

pub fn record_result(result: &crate::api::GenerateResult, keep: usize) {
    if result.artifacts.is_empty() {
        record(&result.text, keep);
        return;
    }
    if keep == 0 {
        return;
    }
    let Some(dir) = history_dir() else {
        eprintln!("warning: cannot determine history directory");
        return;
    };
    let save = || -> Result<()> {
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        set_mode(&dir, 0o700);
        let path = next_entry_path(&dir).with_extension("json");
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(result)?)?;
        #[cfg(unix)]
        set_mode(&tmp, 0o600);
        std::fs::rename(tmp, path)?;
        Ok(())
    };
    if let Err(error) = save() {
        eprintln!("warning: failed to save the result to history: {error:#}");
        return;
    }
    prune(&dir, keep);
}

fn save_entry(dir: &Path, text: &str) -> Result<()> {
    if !dir.exists() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        #[cfg(unix)]
        set_mode(dir, 0o700);
    }
    let path = next_entry_path(dir);
    // Write-then-rename: an interrupted run must not leave a truncated
    // entry that `aido last` would hand back as a complete result. The
    // `.txt.tmp` leftover matches no entry pattern, so it is neither read
    // nor pruned.
    let tmp = path.with_extension("txt.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("failed to write {}", tmp.display()))?;
    #[cfg(unix)]
    set_mode(&tmp, 0o600);
    std::fs::rename(&tmp, &path).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Tighten permissions on what we create — history is plain text that may
/// hold anything the user had on their clipboard. Best-effort: a
/// restrictive umask already yields these modes.
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

/// Keep only the newest `keep` entries.
fn prune(dir: &Path, keep: usize) {
    let mut entries = match entries(dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("warning: failed to prune history: {e:#}");
            return;
        }
    };
    while entries.len() > keep {
        let oldest = entries.remove(0);
        if let Err(e) = std::fs::remove_file(&oldest) {
            eprintln!("warning: failed to prune {}: {e}", oldest.display());
        }
    }
}

/// Entry names sorted as text are oldest..newest.
fn entries(dir: &Path) -> Result<Vec<PathBuf>> {
    let rd = match std::fs::read_dir(dir) {
        // A missing dir just means "no history yet".
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        other => other.with_context(|| format!("cannot read {}", dir.display()))?,
    };
    let mut names: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| is_entry(p))
        .collect();
    names.sort();
    Ok(names)
}

fn next_entry_path(dir: &Path) -> PathBuf {
    let mut elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    loop {
        let path = dir.join(format!("{}.txt", stamp(elapsed)));
        if !path.exists() && !path.with_extension("json").exists() {
            return path;
        }
        // Two runs within the same millisecond would overwrite each
        // other's entry; nudge the stamp instead of losing one.
        elapsed += Duration::from_millis(1);
    }
}

/// Entry names are `YYYYMMDD-HHMMSS.mmm.txt`; only such files belong to
/// the history (and may be pruned) — anything else in the dir is the
/// user's and is left alone.
fn is_entry(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some(stem) = name
        .strip_suffix(".txt")
        .or_else(|| name.strip_suffix(".json"))
    else {
        return false;
    };
    let b = stem.as_bytes();
    b.len() == 19
        && b[..8].iter().all(|c| c.is_ascii_digit())
        && b[8] == b'-'
        && b[9..15].iter().all(|c| c.is_ascii_digit())
        && b[15] == b'.'
        && b[16..].iter().all(|c| c.is_ascii_digit())
}

/// Milliseconds since the epoch as a sortable file-name stamp.
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
        assert_eq!(civil_from_days(20705), (2026, 9, 9)); // today
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
    fn only_stamped_files_are_entries() {
        assert!(is_entry(Path::new("20260909-153012.123.txt")));
        assert!(!is_entry(Path::new("notes.txt")));
        assert!(!is_entry(Path::new("20260909-153012.123.md")));
        assert!(!is_entry(Path::new("20260909-153012.txt"))); // missing millis
    }
}

//! Shared temp-dir discipline for the unit-test binary: every artifact a
//! unit test creates lives under one per-process run root, and each new
//! run sweeps leftovers abandoned by earlier runs (panic, kill, crash)
//! once they are over a day old. tests/support/mod.rs keeps the same
//! discipline for the integration binaries; the two cannot share code.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// Names below temp_dir that belong to this suite's tests. Extend when a
/// new prefix appears; legacy flat entries age out via the sweep either way.
const OUR_PREFIXES: [&str; 4] = ["aido-test-", "aido-input-", "aido-out-", "aido-it-"];

/// Root for this process's temp artifacts (`{temp_dir}/aido-test-run-{pid}`).
pub fn run_root() -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let base = std::env::temp_dir();
        let root = base.join(format!("aido-test-run-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        sweep_stale_leftovers(&base, &root);
        root
    })
    .clone()
}

/// Best-effort removal of this suite's leftovers under `base` that have
/// been untouched for over a day: old run roots plus pre-root flat
/// entries. Fresh entries stay: a concurrent run may still be using them.
fn sweep_stale_leftovers(base: &Path, keep: &Path) {
    const STALE_AFTER: Duration = Duration::from_secs(24 * 3600);
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let ours = OUR_PREFIXES.iter().any(|p| name.starts_with(p));
        if !ours || entry.path() == keep {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        // An unknown age (clock skew) counts as fresh: removal is best-effort.
        let stale = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age >= STALE_AFTER);
        if !stale {
            continue;
        }
        if meta.is_dir() {
            let _ = std::fs::remove_dir_all(entry.path());
        } else {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

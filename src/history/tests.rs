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

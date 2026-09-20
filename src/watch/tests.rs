use super::*;
use crate::domain::ErrorKind;

fn argv(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

fn entries(names: &[&str]) -> Vec<(PathBuf, u64)> {
    names.iter().map(|n| (PathBuf::from(n), 10)).collect()
}

/// Both chain spellings reduce to [`cli::Normalized::Chain`] in the
/// normalizer, so both must hit the same watch-v1 refusal — a bypass
/// through either entry point would be a contract hole.
#[test]
fn watch_rejects_chain_sugar() {
    let err = parse_task_invocation(argv(&["chain", "ocr | translate", "--copy"])).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Usage);
    assert!(
        err.message
            .contains("task chains are not supported inside watch v1"),
        "{err}"
    );
}

#[test]
fn watch_rejects_then_chain() {
    let err = parse_task_invocation(argv(&["ocr", "--then", "translate", "--copy"])).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Usage);
    assert!(
        err.message
            .contains("task chains are not supported inside watch v1"),
        "{err}"
    );
}

#[test]
fn a_new_file_waits_for_the_stability_window_then_fires_once() {
    let stable = Duration::from_millis(500);
    let mut state = WatchState::new(stable, Vec::new(), false);
    let t0 = std::time::Instant::now();
    // First sighting only starts the clock; the daemon's mark_done
    // models the run that follows a ready file.
    assert!(state.next_ready(t0, &entries(&["a.png"])).is_none());
    // Still inside the window.
    assert!(state
        .next_ready(t0 + Duration::from_millis(499), &entries(&["a.png"]))
        .is_none());
    // Window elapsed: exactly once.
    assert_eq!(
        state.next_ready(t0 + Duration::from_millis(500), &entries(&["a.png"])),
        Some(PathBuf::from("a.png"))
    );
    state.mark_done(Path::new("a.png"));
    assert!(state
        .next_ready(t0 + Duration::from_millis(600), &entries(&["a.png"]))
        .is_none());
}

#[test]
fn a_growing_file_resets_the_clock_and_fires_only_when_settled() {
    let stable = Duration::from_millis(500);
    let mut state = WatchState::new(stable, Vec::new(), false);
    let t0 = std::time::Instant::now();
    let one = vec![(PathBuf::from("a.png"), 1)];
    let two = vec![(PathBuf::from("a.png"), 2)];
    assert!(state.next_ready(t0, &one).is_none());
    // Growing every 300 ms keeps it pending forever.
    assert!(state
        .next_ready(t0 + Duration::from_millis(300), &two)
        .is_none());
    assert!(state
        .next_ready(t0 + Duration::from_millis(600), &entries(&["a.png"]))
        .is_none());
    assert!(state
        .next_ready(t0 + Duration::from_millis(899), &entries(&["a.png"]))
        .is_none());
    assert_eq!(
        state.next_ready(t0 + Duration::from_millis(1100), &entries(&["a.png"])),
        Some(PathBuf::from("a.png"))
    );
}

#[test]
fn a_vanished_pending_file_is_dropped_and_a_return_is_a_new_file() {
    let stable = Duration::from_millis(100);
    let mut state = WatchState::new(stable, Vec::new(), false);
    let t0 = std::time::Instant::now();
    assert!(state.next_ready(t0, &entries(&["a.png"])).is_none());
    // Gone before it settled: no trigger either way.
    assert!(state
        .next_ready(t0 + Duration::from_millis(50), &[])
        .is_none());
    // Back again: a fresh file with a fresh clock.
    assert!(state
        .next_ready(t0 + Duration::from_millis(60), &entries(&["a.png"]))
        .is_none());
    assert_eq!(
        state.next_ready(t0 + Duration::from_millis(200), &entries(&["a.png"])),
        Some(PathBuf::from("a.png"))
    );
}

#[test]
fn existing_files_are_skipped_unless_include_existing() {
    let stable = Duration::from_millis(0);
    let existing = entries(&["old.txt"]);
    let mut state = WatchState::new(stable, existing.clone(), false);
    let t0 = std::time::Instant::now();
    assert!(state.next_ready(t0, &existing).is_none());
    assert!(state
        .next_ready(t0 + Duration::from_millis(1), &existing)
        .is_none());

    let mut state = WatchState::new(stable, existing.clone(), true);
    assert!(state.next_ready(t0, &existing).is_none());
    assert_eq!(
        state.next_ready(t0 + Duration::from_millis(1), &existing),
        Some(PathBuf::from("old.txt"))
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
    assert!(state.next_ready(t0, &files).is_none());
    assert_eq!(
        state.next_ready(t0 + Duration::from_millis(1), &files),
        Some(PathBuf::from("a.png"))
    );
    state.mark_done(Path::new("a.png"));
    let bigger = vec![(PathBuf::from("a.png"), 99)];
    assert!(state
        .next_ready(t0 + Duration::from_millis(2), &bigger)
        .is_none());
}

#[test]
fn two_ready_files_fire_one_per_listing_in_sorted_order() {
    let stable = Duration::from_millis(0);
    let mut state = WatchState::new(stable, Vec::new(), false);
    let t0 = std::time::Instant::now();
    let both = entries(&["b.png", "a.png"]);
    assert!(state.next_ready(t0, &both).is_none());
    // One verdict per listing: the smallest ready path first...
    assert_eq!(
        state.next_ready(t0 + Duration::from_millis(1), &both),
        Some(PathBuf::from("a.png"))
    );
    state.mark_done(Path::new("a.png"));
    // ...then the next one, judged on the same listing shape.
    assert_eq!(
        state.next_ready(t0 + Duration::from_millis(2), &both),
        Some(PathBuf::from("b.png"))
    );
}

#[test]
fn a_file_that_grew_while_another_ran_redebounces() {
    // a and b are ready in the same listing; a runs (the daemon is
    // busy), b grows meanwhile. The next verdict must come from fresh
    // stats: b re-waits its stability window instead of riding the old
    // listing's ready verdict.
    let stable = Duration::from_millis(500);
    let mut state = WatchState::new(stable, Vec::new(), false);
    let t0 = std::time::Instant::now();
    // The daemon always folds in the whole directory listing.
    let listing = |a_size: u64, b_size: u64| {
        vec![
            (PathBuf::from("a.txt"), a_size),
            (PathBuf::from("b.txt"), b_size),
        ]
    };
    assert!(state.next_ready(t0, &listing(1, 1)).is_none());
    // Both settled by t0+500: a fires — one verdict per listing.
    assert_eq!(
        state.next_ready(t0 + Duration::from_millis(500), &listing(1, 1)),
        Some(PathBuf::from("a.txt"))
    );
    state.mark_done(Path::new("a.txt"));
    // While a ran, b grew; the next listing folds the growth in: b's
    // clock resets...
    assert!(state
        .next_ready(t0 + Duration::from_millis(600), &listing(1, 2))
        .is_none());
    // ...and it stays pending until the fresh window elapses.
    assert!(state
        .next_ready(t0 + Duration::from_millis(1000), &listing(1, 2))
        .is_none());
    assert_eq!(
        state.next_ready(t0 + Duration::from_millis(1100), &listing(1, 2)),
        Some(PathBuf::from("b.txt"))
    );
}

#[test]
fn artifact_stems_distinguish_what_stems_and_sanitizing_would_collapse() {
    // Same stem, different extension: the classic collision.
    let png = watch_artifact_stem(Path::new("shots/report.png")).unwrap();
    let jpg = watch_artifact_stem(Path::new("shots/report.jpg")).unwrap();
    assert_ne!(png, jpg);
    // The readable part still reads like the input...
    assert!(png.starts_with("report-png--"), "{png}");
    // ...and the stem survives a second sanitizing unchanged, so the
    // artifact name on disk is exactly this stem plus an extension.
    assert_eq!(crate::output::sanitize_stem(&png), png);
    // Names that sanitize to the same form stay distinct: the raw
    // bytes are in the hash.
    let dot = watch_artifact_stem(Path::new("a.b")).unwrap();
    let dash = watch_artifact_stem(Path::new("a-b")).unwrap();
    assert_ne!(dot, dash);
    // The hash covers the raw name, so it is stable per input.
    assert_eq!(
        watch_artifact_stem(Path::new("shots/report.png")).unwrap(),
        png
    );
}

#[cfg(unix)]
#[test]
fn artifact_stem_survives_a_non_utf8_name() {
    use std::os::unix::ffi::OsStrExt as _;
    let path = PathBuf::from(std::ffi::OsStr::from_bytes(b"caf\xe9.png"));
    let stem = watch_artifact_stem(&path).unwrap();
    assert!(stem.starts_with("caf"), "{stem}");
    // Distinct from every UTF-8 lookalike: the hash reads raw bytes.
    assert_ne!(stem, watch_artifact_stem(Path::new("café.png")).unwrap());
}

#[test]
fn resolve_timing_applies_floor_and_precedence() {
    let args = |interval: Option<f64>, stable: Option<u64>| WatchArgs {
        dir: PathBuf::from("d"),
        interval,
        stable_ms: stable,
        include_existing: false,
        task_argv: vec![OsString::from("ocr")],
        parent_argv: Vec::new(),
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

// Linux only: macOS filesystems reject raw non-UTF-8 name bytes
// outright ("Illegal byte sequence"), so the fixture cannot exist
// there — the behavior under test is a raw-bytes property, not a
// scan_dir bug to work around.
#[cfg(target_os = "linux")]
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

// Linux only: macOS filesystems refuse to make the fixture unreadable
// in a way the scan would see, and root reads through a 0o000 mode
// anyway — the error path is asserted where it is exercisable.
#[cfg(target_os = "linux")]
#[test]
fn failed_initial_inventory_is_a_usage_error() {
    use std::os::unix::fs::PermissionsExt as _;

    // Root reads through a 0o000 mode, so the fixture cannot fail the
    // scan there; the CI runners run unprivileged.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }

    let dir = crate::test_support::run_root().join("watch-inventory-unreadable");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("old.txt"), b"old").unwrap();

    let original = std::fs::metadata(&dir).unwrap().permissions();
    let mut blocked = original.clone();
    blocked.set_mode(0o000);
    std::fs::set_permissions(&dir, blocked).unwrap();

    let result = initial_inventory(&dir, &dir);

    // Restore before asserting: a failure must not leave a directory
    // this process can no longer clean up.
    std::fs::set_permissions(&dir, original).unwrap();

    let err = result.expect_err("an unreadable startup directory must fail");
    assert_eq!(err.kind, crate::domain::ErrorKind::Usage);
    assert!(
        err.message.contains("cannot read watch directory"),
        "{}",
        err.message
    );
    let _ = std::fs::remove_dir_all(&dir);
}

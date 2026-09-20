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
        dir_extras: &[],
    };
    // Deliver to a real stdout is awkward in-process; the states tell
    // the story.
    let outcome = deliver(&args);
    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(outcome.states.len(), 1);
    assert!(outcome.states[0].status.is_succeeded());
}

#[test]
fn precheck_refuses_only_existing_file_targets() {
    let dir = crate::test_support::run_root().join(format!("aido-precheck-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("out.txt");
    std::fs::write(&target, "original").unwrap();
    let destinations = vec![
        Destination::File {
            path: target.clone(),
        },
        Destination::Clipboard,
    ];
    let err = precheck_file_targets(&destinations, false).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Usage);
    assert!(err.message.contains("already exists"), "{}", err.message);
    // --overwrite lifts the refusal; stdout/clipboard targets never trip it.
    assert!(precheck_file_targets(&destinations, true).is_ok());
    assert!(precheck_file_targets(&[Destination::Stdout, Destination::Clipboard], false).is_ok());
    // A missing path is fine.
    let missing = vec![Destination::File {
        path: dir.join("nope.txt"),
    }];
    assert!(precheck_file_targets(&missing, false).is_ok());
    // A dangling symlink is not: the commit would refuse it, so the
    // precheck refuses it early for the same reason.
    #[cfg(unix)]
    {
        let link = dir.join("dangling.txt");
        std::os::unix::fs::symlink(dir.join("no-target-here"), &link).unwrap();
        let err = precheck_file_targets(&[Destination::File { path: link }], false).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Usage);
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn existing_file_is_not_clobbered() {
    let dir = crate::test_support::run_root().join(format!("aido-out-test-{}", std::process::id()));
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
    let dir =
        crate::test_support::run_root().join(format!("aido-out-noreplace-{}", std::process::id()));
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
    let dir =
        crate::test_support::run_root().join(format!("aido-out-link-error-{}", std::process::id()));
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
    let dir = crate::test_support::run_root().join(format!("aido-out-mode-{}", std::process::id()));
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
    // a per-attempt suffix, the leftover is ignored and the write
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
    let dir = crate::test_support::run_root().join(format!("aido-out-rand-{}", std::process::id()));
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
    let dir = crate::test_support::run_root().join(format!("aido-out-race-{}", std::process::id()));
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
    let dir = crate::test_support::run_root().join(format!("aido-out-dir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let artifact = text_artifact("text", "body");
    let saved = write_directory(&[&artifact], &dir, "run-1", false, true).unwrap();
    assert_eq!(saved.len(), 1);
    assert!(dir.join("text.txt").exists());
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
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

/// Two distinct ids can sanitize to one file name (`a/b` and `a-b`):
/// the delivery must refuse before writing anything, and `--overwrite`
/// covers a previous delivery's files, never this run's artifacts
/// replacing each other.
#[test]
fn write_directory_refuses_internal_filename_collisions_before_writing() {
    for overwrite in [false, true] {
        let dir = crate::test_support::run_root()
            .join(format!("aido-collide-{}-{overwrite}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let artifacts = [
            text_artifact("a/b", "first"),
            text_artifact("a-b", "second"),
        ];
        assert_eq!(
            artifact_file_name(&artifacts[0]),
            artifact_file_name(&artifacts[1]),
            "the ids collide on the sanitized file name"
        );
        let err = write_directory(
            &artifacts.iter().collect::<Vec<_>>(),
            &dir,
            "run",
            overwrite,
            true,
        )
        .unwrap_err();
        assert!(
            err.message.contains("filename collision"),
            "{:?}",
            err.message
        );
        assert_eq!(err.kind, ErrorKind::Delivery);
        // Nothing was written — not even the directory.
        assert!(
            !dir.exists(),
            "the refusal happens before any byte is written"
        );
    }
}

#[test]
fn distinct_ids_still_deliver_to_a_directory() {
    let dir =
        crate::test_support::run_root().join(format!("aido-collide-ok-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let a = text_artifact("a/b", "first");
    let b = text_artifact("a_b", "second");
    let saved =
        write_directory(&[&a, &b], &dir, "run", false, true).expect("distinct names deliver");
    assert_eq!(saved.len(), 2);
    assert!(dir.join("a-b.txt").exists());
    assert!(dir.join("a_b.txt").exists());
}

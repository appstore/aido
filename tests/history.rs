//! History as run records: save-before-deliver, recovery through `last`
//! and `history show`, retention by count, and `--no-history`.

mod support;

use support::*;

fn chat_cfg(url: &str) -> std::path::PathBuf {
    settings_config(&format!(
        "[settings]\nhistory_keep = 5\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{url}\""
    ))
}

fn run_dirs(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut dirs: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

#[test]
fn runs_are_recorded_with_manifest_and_artifacts() {
    let server = Server::json(chat_body("SAVED"));
    let dir = temp_dir("hist-record");
    let cfg = chat_cfg(&server.url());
    let out = run_with(
        &["summarize", "--profile", "test"],
        b"hi\n",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
        ],
        cfg.to_str().unwrap(),
    );
    out.assert_code(0);
    let runs = run_dirs(&dir);
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    assert!(run.join("manifest.json").exists());
    assert_eq!(
        std::fs::read_to_string(run.join("text.txt")).unwrap(),
        "SAVED"
    );
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(run.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["task"], "summarize");
    assert_eq!(manifest["generation"]["status"], "complete");
    assert_eq!(manifest["artifacts"][0]["file"], "text.txt");
    // credentials never appear in the record
    let raw = std::fs::read_to_string(run.join("manifest.json")).unwrap();
    assert!(!raw.contains("api_key"));
}

#[test]
fn last_recovers_the_result_without_a_new_request() {
    let server = Server::json(chat_body("SAVED"));
    let dir = temp_dir("hist-last");
    let cfg = chat_cfg(&server.url());
    let envs = [
        ("AIDO_CONFIG", cfg.to_str().unwrap()),
        ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
    ];
    let out = run_with(
        &["summarize", "--profile", "test"],
        b"hi\n",
        &envs,
        cfg.clone(),
    );
    out.assert_code(0);
    let _ = server.request();

    // recovery works with no provider configured at all
    let out = run(
        &["last"],
        b"",
        &[("AIDO_HISTORY_DIR", dir.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "SAVED\n");
}

#[test]
fn failed_clipboard_delivery_stays_recoverable() {
    // Force a delivery failure on every platform by pointing --out-dir at
    // a path below a regular file: the generation was saved before
    // delivery, so `last` brings it back without a new request.
    let server = Server::json(chat_body("CLIPPED"));
    let dir = temp_dir("hist-clip");
    let blocker = temp_file("blocker", b"x");
    let bad_dir = blocker.join("sub");
    let cfg = chat_cfg(&server.url());
    let envs = [
        ("AIDO_CONFIG", cfg.to_str().unwrap()),
        ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
    ];
    let out = run_with(
        &[
            "summarize",
            "--profile",
            "test",
            "--copy",
            "--out-dir",
            bad_dir.to_str().unwrap(),
        ],
        b"hi\n",
        &envs,
        cfg.to_str().unwrap(),
    );
    assert_eq!(
        out.code(),
        5,
        "delivery failure must exit 5; stderr: {}",
        out.stderr()
    );
    assert!(out.stdout().is_empty());

    let out = run(
        &["last"],
        b"",
        &[("AIDO_HISTORY_DIR", dir.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "CLIPPED\n");
}

#[cfg(target_os = "linux")]
#[test]
fn clipboard_write_failure_exits_five_and_stays_recoverable() {
    // On the headless Linux CI host the clipboard write itself fails
    // (exit 5) while the generation was saved first. macOS runners have a
    // working clipboard, so this exact assertion is Linux-only.
    let server = Server::json(chat_body("CLIPPED"));
    let dir = temp_dir("hist-clip2");
    let cfg = chat_cfg(&server.url());
    let envs = [
        ("AIDO_CONFIG", cfg.to_str().unwrap()),
        ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
    ];
    let out = run_with(
        &["summarize", "--profile", "test", "--copy"],
        b"hi\n",
        &envs,
        cfg.to_str().unwrap(),
    );
    assert_eq!(
        out.code(),
        5,
        "clipboard failure must exit 5; stderr: {}",
        out.stderr()
    );
    assert!(out.stdout().is_empty());

    let out = run(
        &["last"],
        b"",
        &[("AIDO_HISTORY_DIR", dir.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "CLIPPED\n");
}

#[test]
fn history_list_and_show_work_on_recorded_runs() {
    let server = Server::json(chat_body("one"));
    let dir = temp_dir("hist-list");
    let cfg = chat_cfg(&server.url());
    let envs = [
        ("AIDO_CONFIG", cfg.to_str().unwrap()),
        ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
    ];
    let out = run_with(
        &["summarize", "--profile", "test"],
        b"hi\n",
        &envs,
        cfg.clone(),
    );
    out.assert_code(0);

    let out = run(
        &["history", "list"],
        b"",
        &[("AIDO_HISTORY_DIR", dir.to_str().unwrap())],
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("summarize"), "{stdout}");
    assert!(stdout.contains("complete"), "{stdout}");

    let id = run_dirs(&dir)[0]
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let out = run(
        &["history", "show", &id],
        b"",
        &[("AIDO_HISTORY_DIR", dir.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "one\n");
}

#[test]
fn retention_keeps_only_the_newest_runs() {
    let dir = temp_dir("hist-prune");
    let envs = [("AIDO_HISTORY_DIR", dir.to_str().unwrap())];
    for text in ["FIRST", "SECOND"] {
        let server = Server::json(chat_body(text));
        let cfg = settings_config(&format!(
            "[settings]\nhistory_keep = 1\n\
             [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
             [providers.srv]\nbase_url = \"{}\"",
            server.url()
        ));
        let out = run_with(
            &["summarize", "--profile", "test"],
            b"hi\n",
            &[("AIDO_HISTORY_DIR", dir.to_str().unwrap())],
            cfg,
        );
        out.assert_code(0);
    }
    let runs = run_dirs(&dir);
    assert_eq!(runs.len(), 1, "history_keep = 1 must prune the older run");
    let out = run(&["last"], b"", &envs);
    out.assert_code(0);
    assert_eq!(out.stdout(), "SECOND\n");
}

#[test]
fn incomplete_runs_are_recorded_but_not_recoverable() {
    let dir = temp_dir("hist-incomplete");
    // one complete run first
    let server = Server::json(chat_body("GOOD"));
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 5\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let envs = [
        ("AIDO_CONFIG", cfg.to_str().unwrap()),
        ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
    ];
    let out = run_with(
        &["summarize", "--profile", "test"],
        b"hi\n",
        &envs,
        cfg.clone(),
    );
    out.assert_code(0);

    // then a truncated one
    let server =
        Server::json(r#"{"choices":[{"message":{"content":"CUT"},"finish_reason":"length"}]}"#);
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 5\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let out = run_with(
        &["summarize", "--profile", "test"],
        b"hi\n",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
        ],
        cfg.to_str().unwrap(),
    );
    out.assert_code(4);

    let out = run(
        &["last"],
        b"",
        &[("AIDO_HISTORY_DIR", dir.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(
        out.stdout(),
        "GOOD\n",
        "`last` restores the newest COMPLETE generation"
    );
}

#[test]
fn no_history_leaves_nothing_behind() {
    let server = Server::json(chat_body("EPHEMERAL"));
    let dir = temp_dir("hist-off");
    let cfg = chat_cfg(&server.url());
    let out = run_with(
        &["summarize", "--profile", "test", "--no-history"],
        b"hi\n",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
        ],
        cfg.to_str().unwrap(),
    );
    out.assert_code(0);
    assert_eq!(run_dirs(&dir).len(), 0);
}

#[test]
fn last_without_history_fails_cleanly() {
    let dir = temp_dir("hist-empty");
    let out = run(
        &["last"],
        b"",
        &[("AIDO_HISTORY_DIR", dir.to_str().unwrap())],
    );
    out.assert_code(2);
    assert!(
        out.stderr().contains("no completed runs"),
        "stderr: {}",
        out.stderr()
    );
}

#[test]
fn last_can_restore_media_into_a_directory() {
    let bytes = b"RIFF\x26\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x40\x1f\0\0\x40\x1f\0\0\x01\0\x08\0data\x02\0\0\0\0\0";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\n\r\n{}",
        bytes.len(),
        std::str::from_utf8(bytes).unwrap()
    );
    let server = SseServer::start(&[response]);
    let dir = temp_dir("hist-media");
    let restore = temp_dir("hist-restore");
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 3\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"tts-1\"\noperations = [\"speech\"]\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let out = run_tty_with(
        &[
            "tts",
            "--profile",
            "test",
            "--text",
            "hi",
            "--option",
            "format=wav",
            "-o",
            dir.join("a.wav").to_str().unwrap(),
        ],
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
        ],
        cfg.clone(),
    );
    out.assert_code(0);

    // restore the audio bytes into a fresh directory
    let out = run(
        &["last", "--out-dir", restore.to_str().unwrap()],
        b"",
        &[("AIDO_HISTORY_DIR", dir.to_str().unwrap())],
    );
    out.assert_code(0);
    assert!(restore.join("audio-1.wav").exists(), "files in restore dir");
    assert_eq!(
        std::fs::read(restore.join("audio-1.wav")).unwrap(),
        bytes.to_vec()
    );
}

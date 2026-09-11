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

/// A hand-written complete run record, for cases the live pipeline cannot
/// produce on demand (fixed ids, a chosen prefix shape).
fn fake_complete_run(dir: &std::path::Path, id: &str, text: &str) {
    let run_dir = dir.join(id);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("text.txt"), text).unwrap();
    let manifest = format!(
        r#"{{"version":1,"run_id":"{id}","task":"summarize","created_at":"2026-01-01T00:00:00Z","generation":{{"status":"complete"}},"artifacts":[{{"id":"text","kind":"text","mime":"text/plain","format":"text","file":"text.txt","size":{}}}]}}"#,
        text.len()
    );
    std::fs::write(run_dir.join("manifest.json"), manifest).unwrap();
}

#[test]
fn history_list_numbers_newest_first_and_show_takes_an_index() {
    let dir = temp_dir("hist-index");
    let envs = [("AIDO_HISTORY_DIR", dir.to_str().unwrap())];

    // with no history at all, an index fails cleanly
    let out = run(&["history", "show", "1"], b"", &envs);
    out.assert_code(2);
    assert!(
        out.stderr().contains("no runs recorded"),
        "stderr: {}",
        out.stderr()
    );

    // two complete runs: "one" (older) then "two" (newest)
    for text in ["one", "two"] {
        let server = Server::json(chat_body(text));
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
        out.assert_code(0);
    }
    let runs = run_dirs(&dir);
    assert_eq!(runs.len(), 2);
    let older = runs[0].file_name().unwrap().to_str().unwrap();
    let newest = runs[1].file_name().unwrap().to_str().unwrap();

    // the list is numbered newest first, and those numbers address `show`
    let out = run(&["history", "list"], b"", &envs);
    out.assert_code(0);
    let lines: Vec<String> = out.stdout().lines().map(String::from).collect();
    assert_eq!(lines.len(), 2, "{}", out.stdout());
    assert!(lines[0].starts_with("1  "), "{}", out.stdout());
    assert!(lines[0].contains(newest), "{}", out.stdout());
    assert!(lines[1].starts_with("2  "), "{}", out.stdout());
    assert!(lines[1].contains(older), "{}", out.stdout());

    // `show 1` redelivers the newest run; delivery flags apply
    let file = dir.join("out.txt");
    let out = run(
        &["history", "show", "1", "-o", file.to_str().unwrap()],
        b"",
        &envs,
    );
    out.assert_code(0);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "two");
    let out = run(&["history", "show", "2"], b"", &envs);
    out.assert_code(0);
    assert_eq!(out.stdout(), "one\n");

    // flags before the management word reach the restore too
    let out = run(&["--json", "history", "show", "1"], b"", &envs);
    out.assert_code(0);
    let report: serde_json::Value = serde_json::from_str(&out.stdout())
        .unwrap_or_else(|e| panic!("--json report: {e}; stdout: {}", out.stdout()));
    assert_eq!(report["run_id"], newest);

    // and flags after the operand as well: the normalizer hoists them in
    // front of the management words, so the same top-level flag set applies
    let out = run(&["history", "show", "1", "--json"], b"", &envs);
    out.assert_code(0);
    let report: serde_json::Value = serde_json::from_str(&out.stdout())
        .unwrap_or_else(|e| panic!("--json report: {e}; stdout: {}", out.stdout()));
    assert_eq!(report["run_id"], newest);

    // bad indexes
    let out = run(&["history", "show", "0"], b"", &envs);
    out.assert_code(2);
    assert!(
        out.stderr().contains("start at 1"),
        "stderr: {}",
        out.stderr()
    );
    let out = run(&["history", "show", "3"], b"", &envs);
    out.assert_code(2);
    assert!(
        out.stderr().contains("no run #3"),
        "stderr: {}",
        out.stderr()
    );

    // a number past usize overflows the parse into the same out-of-range
    // message, quoting the digits as typed
    let out = run(&["history", "show", "99999999999999999999999"], b"", &envs);
    out.assert_code(2);
    assert!(
        out.stderr().contains("no run #99999999999999999999999"),
        "stderr: {}",
        out.stderr()
    );
    assert!(
        out.stderr().contains("shows only 2"),
        "stderr: {}",
        out.stderr()
    );
}

#[test]
fn history_show_accepts_ids_and_unique_prefixes() {
    let dir = temp_dir("hist-prefix");
    let envs = [("AIDO_HISTORY_DIR", dir.to_str().unwrap())];
    fake_complete_run(&dir, "20260101-000000.000", "A");
    fake_complete_run(&dir, "20260101-000001.000", "B");

    // the exact full id still works
    let out = run(&["history", "show", "20260101-000000.000"], b"", &envs);
    out.assert_code(0);
    assert_eq!(out.stdout(), "A\n");

    // a unique prefix resolves to its one run
    let out = run(&["history", "show", "20260101-000000"], b"", &envs);
    out.assert_code(0);
    assert_eq!(out.stdout(), "A\n");

    // an ambiguous prefix is refused with guidance (a pure-digit string
    // would be an index, so the prefix keeps the '-')
    let out = run(&["history", "show", "20260101-"], b"", &envs);
    out.assert_code(2);
    assert!(
        out.stderr().contains("ambiguous"),
        "stderr: {}",
        out.stderr()
    );

    // an unknown id is refused; digits stay index-only
    let out = run(&["history", "show", "no-such-run"], b"", &envs);
    out.assert_code(2);
    assert!(
        out.stderr().contains("no run 'no-such-run'"),
        "stderr: {}",
        out.stderr()
    );

    // index 1 is the newest of the two entries
    let out = run(&["history", "show", "1"], b"", &envs);
    out.assert_code(0);
    assert_eq!(out.stdout(), "B\n");
}

#[test]
fn history_show_on_an_incomplete_run_explains_and_points_to_last() {
    let dir = temp_dir("hist-index-incomplete");
    let envs = [("AIDO_HISTORY_DIR", dir.to_str().unwrap())];
    let server = Server::json(chat_body("GOOD"));
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
    out.assert_code(0);

    // then a truncated one: recorded, incomplete, exit 4
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

    // index 1 is the literal newest entry, which is incomplete: explained,
    // nothing delivered, and pointed at `last` / another index
    let out = run(&["history", "show", "1"], b"", &envs);
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("incomplete"), "{stdout}");
    assert!(stdout.contains("aido last"), "{stdout}");

    // the older complete run is still addressable as #2
    let out = run(&["history", "show", "2"], b"", &envs);
    out.assert_code(0);
    assert_eq!(out.stdout(), "GOOD\n");
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

#[cfg(target_os = "linux")]
#[test]
fn partial_delivery_records_each_destination_honestly() {
    // The file write succeeds and the clipboard write fails (headless):
    // exit 5, and the manifest must keep the real per-destination states
    // — file delivered, clipboard failed — instead of marking everything
    // failed.
    let server = Server::json(chat_body("PARTIAL"));
    let dir = temp_dir("hist-partial");
    let out_file = dir.join("out.txt");
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
            "-o",
            out_file.to_str().unwrap(),
            "--copy",
        ],
        b"hi\n",
        &envs,
        cfg.to_str().unwrap(),
    );
    out.assert_code(5);
    let runs = run_dirs(&dir);
    assert_eq!(runs.len(), 1);
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(runs[0].join("manifest.json")).unwrap())
            .unwrap();
    let deliveries = manifest["deliveries"].as_array().unwrap();
    assert_eq!(deliveries.len(), 2, "{deliveries:?}");
    let file_state = deliveries
        .iter()
        .find(|d| d["destination"]["type"] == "file")
        .expect("file destination recorded");
    assert_eq!(file_state["status"], "succeeded", "{deliveries:?}");
    let clip_state = deliveries
        .iter()
        .find(|d| d["destination"]["type"] == "clipboard")
        .expect("clipboard destination recorded");
    assert!(
        clip_state["status"].get("failed").is_some(),
        "{deliveries:?}"
    );
}

#[test]
fn byte_budget_never_deletes_the_newest_run() {
    // An old complete run heavier than the whole budget is pruned, but
    // the run just saved always survives — the recovery promise depends
    // on it — even while the total stays over budget.
    let dir = temp_dir("hist-bytes");
    let old_id = "20260101-000000.000";
    let old_dir = dir.join(old_id);
    std::fs::create_dir_all(&old_dir).unwrap();
    std::fs::write(old_dir.join("text.txt"), vec![b'x'; 4096]).unwrap();
    std::fs::write(
        old_dir.join("manifest.json"),
        format!(
            r#"{{"version":1,"run_id":"{old_id}","task":"summarize","created_at":"2026-01-01T00:00:00Z","generation":{{"status":"complete"}},"artifacts":[{{"id":"text","kind":"text","mime":"text/plain","format":"text","file":"text.txt","size":4096}}]}}"#
        ),
    )
    .unwrap();

    let server = Server::json(chat_body("NEW"));
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 50\nhistory_bytes = 1024\n\
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
    out.assert_code(0);

    let runs = run_dirs(&dir);
    assert_eq!(runs.len(), 1, "the over-budget old run is pruned");
    assert_ne!(runs[0].file_name().unwrap(), old_id);
    // the newest run is still recoverable
    let out = run(
        &["last"],
        b"",
        &[("AIDO_HISTORY_DIR", dir.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "NEW\n");
}

/// Changing the process-wide cwd touches every test in this binary, so
/// cwd-changing tests serialize on this lock. Every other path in the
/// suite is absolute (`temp_dir` et al. build on `std::env::temp_dir()`,
/// and `AIDO_CONFIG` is always set), so nothing else reads the cwd.
static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Restores the original cwd on drop, even when an assertion panics.
struct CwdGuard {
    original: std::path::PathBuf,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
    }
}

fn enter_cwd(dir: &std::path::Path) -> CwdGuard {
    let lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let original = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();
    CwdGuard {
        original,
        _lock: lock,
    }
}

#[test]
fn byte_budget_prunes_with_a_relative_history_dir() {
    // AIDO_HISTORY_DIR may be relative: it resolves against the cwd, which
    // the spawned aido inherits. The budget loop used to join the already
    // joined run path back onto the dir, so a relative setting produced
    // `hist/hist/…`, every removal failed, and the budget never bit.
    let base = temp_dir("hist-relative");
    let hist = base.join("hist");
    std::fs::create_dir_all(&hist).unwrap();
    let old_id = "20260101-000000.000";
    fake_complete_run(&hist, old_id, &"x".repeat(4096));

    let _cwd = enter_cwd(&base);
    let server = Server::json(chat_body("NEW"));
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 50\nhistory_bytes = 1024\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let out = run_with(
        &["summarize", "--profile", "test"],
        b"hi\n",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", "hist"),
        ],
        cfg.to_str().unwrap(),
    );
    out.assert_code(0);
    let stderr = out.stderr();
    assert!(
        !stderr.contains("failed to prune history entry"),
        "a relative history dir must prune cleanly; stderr: {stderr}"
    );

    let runs = run_dirs(&hist);
    assert_eq!(runs.len(), 1, "the over-budget old run is pruned");
    assert_ne!(runs[0].file_name().unwrap(), old_id);

    // the newest run is still recoverable, through the same relative path
    let out = run(&["last"], b"", &[("AIDO_HISTORY_DIR", "hist")]);
    out.assert_code(0);
    assert_eq!(out.stdout(), "NEW\n");
}

#[test]
fn last_skips_a_damaged_newest_entry() {
    // One corrupted manifest must not brick recovery: `last` skips the
    // unreadable entry and restores the older complete run.
    let dir = temp_dir("hist-corrupt");
    let server = Server::json(chat_body("OLDER"));
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
    out.assert_code(0);

    let bad = dir.join("99999999-999999.999");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join("manifest.json"), "{ not json").unwrap();

    let out = run(
        &["last"],
        b"",
        &[("AIDO_HISTORY_DIR", dir.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "OLDER\n");
}

#[test]
fn unsatisfied_generation_is_recorded_with_its_artifacts() {
    // --count 2 but the service returns one image: exit 4, and the good
    // image stays in the clearly-marked record instead of being dropped.
    let dir = temp_dir("hist-unsatisfied");
    let body = serde_json::json!({"created": 1, "data": [{"b64_json": b64(&solid_png(2, 2))}]})
        .to_string();
    let server = Server::json(&body);
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 5\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"gpt-image-1\"\noperations = [\"image\"]\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let out = run_tty_with(
        &[
            "image",
            "--profile",
            "test",
            "--text",
            "a dog",
            "--count",
            "2",
            // satisfies the "multiple images need a directory" precheck;
            // the run never reaches delivery
            "--out-dir",
            dir.join("never").to_str().unwrap(),
        ],
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
        ],
        cfg.clone(),
    );
    out.assert_code(4);
    assert!(out.stderr().contains("expected 2"), "{}", out.stderr());
    let runs = run_dirs(&dir);
    assert_eq!(runs.len(), 1, "the run is recorded");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(runs[0].join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["generation"]["status"], "incomplete");
    assert!(manifest["generation"]["reason"]
        .as_str()
        .unwrap()
        .contains("expected 2"));
    assert!(runs[0].join("image-1.png").exists(), "artifact bytes kept");
    assert!(!runs[0].join("image-2.png").exists());
}

#[cfg(unix)]
#[test]
fn ctrl_c_records_a_cancelled_run_and_exits_130() {
    use std::io::Write;
    use std::net::TcpListener;
    use std::process::{Command, Stdio};

    // A server that accepts and never answers: the request hangs until
    // the interrupt arrives.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let holder = std::thread::spawn(move || {
        let _stream = listener.accept();
        std::thread::sleep(std::time::Duration::from_secs(30));
    });
    let dir = temp_dir("hist-cancel");
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 5\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:{port}\""
    ));
    let mut child = Command::new(EXE)
        .args(["ask", "-", "-p", "hang", "--profile", "test"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("AIDO_CONFIG", cfg.to_str().unwrap())
        .env("AIDO_TASKS_DIR", "/nonexistent/aido-test-tasks")
        .env("AIDO_HISTORY_DIR", dir.to_str().unwrap())
        .spawn()
        .unwrap();
    // Feed the piped material and close stdin so gathering finishes and
    // the request starts hanging.
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"hello\n").unwrap();
    drop(stdin);
    std::thread::sleep(std::time::Duration::from_millis(1000));
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(130),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    holder.join().ok();

    let runs = run_dirs(&dir);
    assert_eq!(runs.len(), 1, "the interrupted run leaves a trace");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(runs[0].join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["generation"]["status"], "cancelled");
}

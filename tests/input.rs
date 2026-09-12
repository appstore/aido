//! Input and command-resolution: the §2.2 decision table, ordered mixed
//! material, task placement, and usage errors — all against the binary.

mod support;

use support::*;

/// A config pointing the "test" profile at a server.
fn server_config(url: &str, extra: &str) -> std::path::PathBuf {
    settings_config(&format!(
        "default_profile = \"test\"\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"test-model\"\n\
         [providers.srv]\nbase_url = \"{url}\"\n{extra}"
    ))
}

#[test]
fn piped_stdin_is_material_for_a_task() {
    let server = Server::json(chat_body("WORLD"));
    let cfg = server_config(&server.url(), "");
    let out = run(
        &["code-review"],
        b"hello\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "WORLD\n");
    let req = request_json(&server.request());
    assert_eq!(req["messages"][0]["role"], "system");
    assert!(req["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains("Review"));
    assert_eq!(req["messages"][1]["role"], "user");
    assert_eq!(req["messages"][1]["content"], "hello\n");
    // no token limit is sent by default anymore
    assert!(req.get("max_tokens").is_none());
}

#[test]
fn dash_reads_stdin_at_its_position_between_files() {
    let server = Server::json(chat_body("ok"));
    let file = temp_file("contrib.md", b"contrib guide\n");
    let out = run(
        &["code-review", file.to_str().unwrap(), "-"],
        b"piped diff\n",
        &[(
            "AIDO_CONFIG",
            server_config(&server.url(), "").to_str().unwrap(),
        )],
    );
    out.assert_code(0);
    let req = request_json(&server.request());
    let content = req["messages"][1]["content"].as_str().unwrap();
    // stdin comes after the file, order preserved
    assert!(content.contains("contrib guide"), "{content}");
    assert!(content.contains("piped diff"), "{content}");
    assert!(
        content.find("contrib guide").unwrap() < content.find("piped diff").unwrap(),
        "{content}"
    );
}

#[test]
fn unconsumed_pipe_with_explicit_material_is_an_error() {
    let file = temp_file("a.txt", b"from file\n");
    // The bytes are in the pipe before the binary starts, so the probe
    // cannot lose a race against the parent's write. Windows keeps the
    // write-after-spawn pipe: no prefill helper there.
    #[cfg(unix)]
    let out = run_prefilled_pipe(
        &["summarize", file.to_str().unwrap()],
        b"pipe\n",
        &[],
        empty_config(),
    );
    #[cfg(windows)]
    let out = run(&["summarize", file.to_str().unwrap()], b"pipe\n", &[]);
    out.assert_code(2);
    assert!(out.stderr().contains("add `-`"), "stderr: {}", out.stderr());
}

#[test]
fn closed_empty_pipe_with_explicit_material_runs() {
    // The shape every CI runner gives a command: stdin is a pipe that was
    // closed without a single byte. Not a terminal, but no data either —
    // explicit material must run exactly as it would in a terminal.
    let server = Server::json(chat_body("ok"));
    let file = temp_file("a.txt", b"from file\n");
    let cfg = server_config(&server.url(), "");
    let out = run(
        &["summarize", file.to_str().unwrap()],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "ok\n");
}

#[test]
fn null_stdin_with_explicit_material_runs() {
    // stdin redirected from the null device: same verdict, same exit 0.
    let server = Server::json(chat_body("ok"));
    let file = temp_file("a.txt", b"from file\n");
    let cfg = server_config(&server.url(), "");
    let out = run_null_stdin(
        &["summarize", file.to_str().unwrap()],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        &cfg,
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "ok\n");
}

#[test]
fn empty_piped_stdin_is_an_error_and_never_touches_the_clipboard() {
    let out = run(&["summarize"], b"", &[]);
    out.assert_code(2);
    assert!(
        out.stderr().contains("stdin is empty"),
        "stderr: {}",
        out.stderr()
    );
}

#[cfg(unix)]
#[test]
fn text_files_keep_their_order_and_get_labels() {
    let a = temp_file("a.txt", b"alpha\n");
    let b = temp_file("b.txt", b"beta\n");
    let server = Server::json(chat_body("ok"));
    let cfg = server_config(&server.url(), "");
    let out = run_tty_with(
        &["summarize", a.to_str().unwrap(), b.to_str().unwrap()],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    let content = request_json(&server.request())["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(content.contains("-a.txt ---"), "{content}");
    assert!(content.contains("-b.txt ---"), "{content}");
    assert!(content.find("alpha").unwrap() < content.find("beta").unwrap());
}

#[test]
fn mixed_material_order_survives_across_types() {
    let png = solid_png(3, 3);
    let a = temp_file("a.png", &png);
    let out = run(
        &[
            "ask",
            "--text",
            "图一",
            a.to_str().unwrap(),
            "--text",
            "图二",
            "-",
            "-p",
            "比较两图",
        ],
        "附注\n".as_bytes(),
        &[(
            "AIDO_CONFIG",
            server_config("http://127.0.0.1:1", "").to_str().unwrap(),
        )],
    );
    // ask over a vision-capable route with a dead server: the plan
    // validated and the request was attempted → service failure (3).
    out.assert_code(3);
}

#[cfg(unix)]
#[test]
fn image_file_becomes_vision_message_with_original_png_bytes() {
    let png = solid_png(3, 3);
    let file = temp_file("shot.png", &png);
    let server = Server::json(chat_body("red square"));
    let cfg = server_config(&server.url(), "");
    let out = run_tty_with(
        &["ocr", file.to_str().unwrap()],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "red square\n");
    let req = request_json(&server.request());
    let content = &req["messages"][1]["content"];
    assert!(content.is_array());
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "image_url");
    let url = content[1]["image_url"]["url"].as_str().unwrap();
    assert!(url.starts_with("data:image/png;base64,"), "got: {url}");
}

#[cfg(unix)]
#[test]
fn task_may_come_after_flags() {
    let png = solid_png(3, 3);
    let file = temp_file("shot.png", &png);
    let server = Server::json(chat_body("ok"));
    let cfg = server_config(&server.url(), "");
    let out = run_tty_with(
        &["--profile", "test", "ocr", file.to_str().unwrap()],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    let raw = server.request();
    assert!(request_path(&raw).contains("chat/completions"));
}

#[cfg(unix)]
#[test]
fn run_reaches_custom_tasks_with_reserved_names() {
    let dir = temp_dir("tasks-reserved");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("last.toml"),
        "operation = 'generate'\ninput_types = ['text']\noutput_types = ['text']\n",
    )
    .unwrap();
    let out = run_tty_with(
        &["run", "last", "input.txt"],
        &[("AIDO_TASKS_DIR", dir.to_str().unwrap())],
        empty_config(),
    );
    // input.txt does not exist: the task ran and failed on the input read.
    out.assert_code(2);
    assert!(
        out.stderr().contains("input.txt"),
        "stderr: {}",
        out.stderr()
    );
}

#[test]
fn unknown_task_gets_a_suggestion() {
    let out = run(&["transalte"], b"hi\n", &[]);
    out.assert_code(2);
    let err = out.stderr();
    assert!(err.contains("unknown task"), "{err}");
    assert!(err.contains("translate"), "{err}");
}

#[cfg(unix)]
#[test]
fn file_without_task_or_prompt_points_at_usage() {
    let out = run_tty(&["notes.txt"], &[]);
    out.assert_code(2);
    let err = out.stderr();
    assert!(err.contains("requires a task"), "{err}");
    assert!(err.contains("notes.txt"), "{err}");
    assert!(!err.contains("unknown task"), "{err}");
}

#[cfg(unix)]
#[test]
fn prompt_with_positionals_selects_ask() {
    let server = Server::json(chat_body("ok"));
    let cfg = server_config(&server.url(), "");
    let file = temp_file("笔记.txt", "中文内容\n".as_bytes());
    let out = run_tty_with(
        &["-p", "润色这段话", file.to_str().unwrap()],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    let req = request_json(&server.request());
    assert_eq!(req["messages"][0]["role"], "system");
    assert_eq!(req["messages"][0]["content"], "润色这段话");
    assert_eq!(req["messages"][1]["content"], "中文内容\n");
}

#[test]
fn ask_without_prompt_is_a_usage_error() {
    let out = run(&["ask", "-"], b"hi\n", &[]);
    out.assert_code(2);
    assert!(out.stderr().contains("-p"), "stderr: {}", out.stderr());
}

#[cfg(unix)]
#[test]
fn separator_makes_leading_dashes_files() {
    let out = run_tty(&["ocr", "--", "./-strange.png"], &[]);
    out.assert_code(2);
    // the file is missing: a path error, not a flag parse error
    assert!(
        out.stderr().contains("-strange.png"),
        "stderr: {}",
        out.stderr()
    );
}

#[test]
fn duplicate_stdin_is_rejected_before_reading() {
    let out = run(&["code-review", "-", "-"], b"hi\n", &[]);
    out.assert_code(2);
    assert!(out.stderr().contains("once"), "stderr: {}", out.stderr());
}

#[test]
fn removed_old_flags_error_with_migration_hints() {
    for (flag, hint) in [
        ("--save", "-o FILE"),
        ("--save-dir", "--out-dir"),
        ("--preset", "run"),
        ("--no-spinner", "--quiet"),
        ("--output-mode", "--produce"),
        ("--adapter", "provider"),
        ("--base-url", "provider"),
        ("--api-key", "api_key_env"),
    ] {
        let out = run(&[flag, "x", "ask", "-p", "hi", "-"], b"x", &[]);
        assert_eq!(out.code(), 2, "{flag}");
        assert!(
            out.stderr().contains("no longer accepted"),
            "{flag}: {}",
            out.stderr()
        );
        assert!(out.stderr().contains(hint), "{flag}: {}", out.stderr());
    }
}

#[test]
fn old_output_mode_values_in_o_are_caught() {
    for value in ["clipboard", "both", "stdout"] {
        let out = run(&["ask", "-p", "hi", "-", "-o", value], b"x", &[]);
        out.assert_code(2);
        let err = out.stderr();
        assert!(err.contains(value), "{err}");
        assert!(err.contains("--copy") || err.contains("--stdout"), "{err}");
    }
    // A literal file with that name is still possible via ./
    let out = run(
        &["ask", "-p", "hi", "-", "-o", "./clipboard"],
        b"x",
        &[(
            "AIDO_CONFIG",
            server_config("http://127.0.0.1:1", "").to_str().unwrap(),
        )],
    );
    out.assert_code(3);
}

#[cfg(unix)]
#[test]
fn empty_file_is_an_error_not_a_skip() {
    let file = temp_file("empty.txt", b"");
    let out = run_tty(&["summarize", file.to_str().unwrap()], &[]);
    out.assert_code(2);
    assert!(out.stderr().contains("empty"), "stderr: {}", out.stderr());
}

#[cfg(unix)]
#[test]
fn missing_file_fails_with_the_path() {
    let out = run_tty(&["ocr", "nope.png"], &[]);
    out.assert_code(2);
    assert!(
        out.stderr().contains("nope.png"),
        "stderr: {}",
        out.stderr()
    );
}

#[cfg(unix)]
#[test]
fn unparseable_pattern_without_a_file_reports_no_file_first() {
    // `shot[1.png` cannot parse as a glob (unclosed `[`) and no literal
    // file exists: the error must read like the shell's — no such file —
    // with the syntax problem only as secondary context.
    let out = run_tty(&["ocr", "shot[1.png"], &[]);
    out.assert_code(2);
    let err = out.stderr();
    assert!(err.contains("no files match"), "{err}");
    assert!(err.contains("shot[1.png"), "{err}");
    assert!(err.contains("not a valid glob pattern"), "{err}");
}

#[cfg(unix)]
#[test]
fn literal_file_with_unclosed_bracket_expands_past_the_pattern() {
    // The same unparseable name, but the file exists: the literal-file
    // escape wins and the run proceeds past expansion.
    let png = solid_png(2, 2);
    let file = temp_file("shot[1.png", &png);
    let server = Server::json(chat_body("ok"));
    let cfg = server_config(&server.url(), "");
    let out = run_tty_with(
        &["ocr", file.to_str().unwrap()],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "ok\n");
}

#[test]
fn ocr_requires_image_material() {
    let out = run(&["ocr"], b"only text\n", &[]);
    out.assert_code(2);
    let err = out.stderr();
    assert!(err.contains("image"), "{err}");
}

#[test]
fn transcribe_rejects_multiple_audios() {
    let a = temp_file("a.wav", b"RIFF\x04\0\0\0WAVE");
    let out = run(
        &["transcribe", a.to_str().unwrap(), "-"],
        b"RIFF\x04\0\0\0WAVE",
        &[],
    );
    out.assert_code(2);
    assert!(out.stderr().contains("at most"), "stderr: {}", out.stderr());
}

#[test]
fn task_rejects_foreign_parameters() {
    let out = run(&["ocr", "--to", "zh-CN", "-"], b"x", &[]);
    out.assert_code(2);
    let err = out.stderr();
    assert!(err.contains("--to"), "{err}");
    assert!(err.contains("ocr"), "{err}");
}

#[test]
fn dry_run_does_not_touch_the_clipboard() {
    // --paste under --dry-run must plan against a placeholder: the
    // clipboard is never read (the test env has no display, so a read
    // would fail with a clipboard error instead of producing a plan).
    let out = run_tty(&["ocr", "--paste", "--dry-run"], &[]);
    let err = out.stderr();
    assert!(!err.contains("clipboard:"), "{err}");
    // ocr requires image material, but the unread clipboard's kind is
    // decided at runtime — the plan must preview, not fail the type check.
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(
        stdout.contains("clipboard (not read under --dry-run)"),
        "{stdout}"
    );
    assert!(stdout.contains("unknown (decided at runtime)"), "{stdout}");
}

fn expansion_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("aido-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(unix)]
#[test]
fn glob_and_directory_expand_sorted_into_the_plan() {
    let dir = expansion_dir("glob-plan");
    std::fs::write(dir.join("b.txt"), b"beta\n").unwrap();
    std::fs::write(dir.join("a.txt"), b"alpha\n").unwrap();
    std::fs::write(dir.join(".hidden.txt"), b"no\n").unwrap();

    // run() pipes stdin, so a material spec without `-` would trip the
    // unconsumed-pipe check: file material tests use a tty stdin.
    let pattern = format!("{}/*.txt", dir.display());
    let out = run_tty(&["summarize", "--dry-run", &pattern], &[]);
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("1. a.txt"), "{stdout}");
    assert!(stdout.contains("2. b.txt"), "{stdout}");
    assert!(!stdout.contains(".hidden"), "{stdout}");

    // A directory argument: same one-level sorted list.
    let out = run_tty(&["summarize", "--dry-run", dir.to_str().unwrap()], &[]);
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("a.txt"), "{stdout}");
    assert!(stdout.contains("b.txt"), "{stdout}");
    assert!(!stdout.contains(".hidden"), "{stdout}");

    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
#[test]
fn glob_without_matches_fails_before_any_request() {
    let out = run_tty(&["summarize", "--dry-run", "no-such-input-*.png"], &[]);
    out.assert_code(2);
    assert!(
        out.stderr().contains("no files match"),
        "stderr: {}",
        out.stderr()
    );
}

#[cfg(unix)]
#[test]
fn directory_with_subdirectory_fails_with_a_glob_hint() {
    let dir = expansion_dir("dirsub");
    std::fs::create_dir(dir.join("raw")).unwrap();
    std::fs::write(dir.join("a.txt"), b"a\n").unwrap();
    let out = run_tty(&["summarize", "--dry-run", dir.to_str().unwrap()], &[]);
    out.assert_code(2);
    let err = out.stderr();
    assert!(err.contains("expands one level"), "{err}");
    assert!(err.contains("glob"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
#[test]
fn glob_material_reaches_the_request_in_order() {
    let server = Server::json(chat_body("ok"));
    let dir = expansion_dir("globreq");
    std::fs::write(dir.join("b.txt"), b"beta\n").unwrap();
    std::fs::write(dir.join("a.txt"), b"alpha\n").unwrap();
    let pattern = format!("{}/*.txt", dir.display());
    let cfg = server_config(&server.url(), "");
    let out = run_tty(
        &["summarize", &pattern],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    let req = request_json(&server.request());
    let content = req["messages"][1]["content"].as_str().unwrap();
    let a = content.find("alpha").unwrap();
    let b = content.find("beta").unwrap();
    assert!(a < b, "{content}");
    std::fs::remove_dir_all(&dir).ok();
}

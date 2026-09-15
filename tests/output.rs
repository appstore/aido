//! Delivery: destinations, atomic files, directories with manifests, the
//! JSON report, and the exit-code contract for delivery failures.

mod support;

use support::*;

fn chat_cfg(url: &str) -> std::path::PathBuf {
    settings_config(&format!(
        "[settings]\nhistory_keep = 0\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{url}\""
    ))
}

#[test]
fn stdout_gets_exactly_one_trailing_newline() {
    let server = Server::json(chat_body("no newline at end"));
    let cfg = chat_cfg(&server.url());
    let out = run(
        &["summarize", "--profile", "test"],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "no newline at end\n");

    let server = Server::json(chat_body("ends with newline\n"));
    let cfg = chat_cfg(&server.url());
    let out = run(
        &["summarize", "--profile", "test"],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "ends with newline\n");
}

#[test]
fn output_file_takes_the_body_instead_of_stdout() {
    let server = Server::json(chat_body("TO FILE"));
    let dir = temp_dir("out-file");
    let file = dir.join("summary.md");
    let cfg = chat_cfg(&server.url());
    let out = run(
        &[
            "summarize",
            "--profile",
            "test",
            "-o",
            file.to_str().unwrap(),
        ],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "", "explicit destination replaces stdout");
    assert_eq!(std::fs::read(&file).unwrap(), b"TO FILE");
    assert!(out.stderr().contains("saved"), "stderr: {}", out.stderr());
}

#[test]
fn existing_output_file_refuses_without_overwrite() {
    let server = Server::json(chat_body("new"));
    let dir = temp_dir("out-exists");
    let file = dir.join("summary.txt");
    std::fs::write(&file, "original").unwrap();
    let cfg = chat_cfg(&server.url());
    let out = run(
        &[
            "summarize",
            "--profile",
            "test",
            "-o",
            file.to_str().unwrap(),
        ],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(5);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "original",
        "the old content survives"
    );

    // --overwrite replaces it atomically
    let server = Server::json(chat_body("new"));
    let cfg = chat_cfg(&server.url());
    let out = run(
        &[
            "summarize",
            "--profile",
            "test",
            "-o",
            file.to_str().unwrap(),
            "--overwrite",
        ],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "new");
}

#[cfg(unix)]
#[test]
fn media_extension_mismatch_fails_before_any_request() {
    let dir = temp_dir("out-ext");
    let file = dir.join("picture.png");
    let cfg = settings_config(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\noperations = [\"image\"]\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"",
    );
    let out = run_tty_with(
        &[
            "image",
            "--profile",
            "test",
            "--text",
            "dog",
            "--format",
            "jpeg",
            "-o",
            file.to_str().unwrap(),
        ],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(2);
    let err = out.stderr();
    assert!(err.contains("jpeg"), "{err}");
    assert!(err.contains(".png"), "{err}");
}

#[cfg(unix)]
#[test]
fn image_to_clipboard_in_a_terminal_is_a_valid_plan() {
    // `--copy` is a documented destination for a binary artifact: in a
    // real terminal session (stdout is a tty too) the plan must list the
    // clipboard instead of demanding -o/--out-dir.
    let cfg = settings_config(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\noperations = [\"image\"]\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"",
    );
    let out = run_full_tty(
        &[
            "image",
            "--profile",
            "test",
            "--text",
            "dog",
            "--copy",
            "--dry-run",
        ],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    let plan = out.stdout();
    assert!(plan.contains("destinations:"), "{plan}");
    assert!(plan.contains("clipboard"), "{plan}");
}

#[cfg(unix)]
#[test]
fn image_without_any_destination_in_a_terminal_is_still_refused() {
    // The guard itself stays: a bare terminal cannot receive binary.
    let cfg = settings_config(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\noperations = [\"image\"]\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"",
    );
    let out = run_full_tty(
        &["image", "--profile", "test", "--text", "dog", "--dry-run"],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(2);
    assert!(
        out.stderr().contains("binary output needs"),
        "stderr: {}",
        out.stderr()
    );
}

#[cfg(unix)]
#[test]
fn json_binary_task_in_a_terminal_names_the_json_cause() {
    // --json alone in a terminal trips the binary precheck, but the sole
    // stdout destination is the report itself: the error must say so, not
    // the generic "binary output needs ..." advice that reads like a
    // missing -o. The --json error contract holds on a terminal too: the
    // pty drain (stdout here) carries one report with the same message.
    let cfg = settings_config(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\noperations = [\"image\"]\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"",
    );
    let out = run_full_tty(
        &[
            "image",
            "--profile",
            "test",
            "--text",
            "dog",
            "--json",
            "--dry-run",
        ],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(2);
    let err = out.stderr();
    assert!(err.contains("--json"), "stderr: {err}");
    assert!(err.contains("carries no artifact bytes"), "stderr: {err}");
    let stdout = out.stdout();
    let report: serde_json::Value = serde_json::from_str(stdout.replace('\r', "").as_str())
        .unwrap_or_else(|e| panic!("stdout must hold one JSON report: {e}\n{stdout}"));
    assert_eq!(report["error"]["kind"], "usage", "report: {report}");
    assert!(
        report["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("--json"),
        "report: {report}"
    );
}

#[test]
fn json_binary_task_over_a_stdout_pipe_still_plans() {
    // The same command over a pipe passes the precheck unchanged: the
    // dry-run keeps listing stdout as the destination, no regression.
    let cfg = settings_config(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\noperations = [\"image\"]\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"",
    );
    let out = run(
        &[
            "image",
            "--profile",
            "test",
            "--text",
            "dog",
            "--json",
            "--dry-run",
        ],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    let plan = out.stdout();
    assert!(plan.contains("destinations:"), "{plan}");
    assert!(plan.contains("stdout"), "{plan}");
}

#[test]
fn json_with_an_output_file_still_delivers_the_image() {
    // -o gives the binary artifact its home, so --json composes fine: the
    // file gets the real bytes while stdout carries the report.
    let encoded = encode_png();
    let body = serde_json::json!({"data":[{"b64_json":encoded}]}).to_string();
    let server = Server::json(&body);
    let dir = temp_dir("out-json-image");
    let file = dir.join("dog.png");
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 0\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\noperations = [\"image\"]\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let out = run(
        &[
            "image",
            "--profile",
            "test",
            "--text",
            "dog",
            "--json",
            "-o",
            file.to_str().unwrap(),
        ],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(std::fs::read(&file).unwrap(), solid_png(2, 2));
    let report: serde_json::Value = serde_json::from_str(&out.stdout())
        .unwrap_or_else(|e| panic!("stdout must hold one JSON report: {e}\n{}", out.stdout()));
    assert_eq!(report["artifacts"][0]["kind"], "image", "report: {report}");
    assert!(report["error"].is_null(), "report: {report}");
    assert!(out.stderr().contains("saved"), "stderr: {}", out.stderr());
}

#[cfg(unix)]
#[test]
fn directory_delivery_writes_artifacts_then_manifest() {
    let png = solid_png(2, 2);
    let encoded = {
        let alphabet: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in png.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            out.push(alphabet[(n >> 18) as usize & 63] as char);
            out.push(alphabet[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                alphabet[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                alphabet[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    };
    let body = serde_json::json!({"status":"completed","output":[
        {"type":"message","content":[{"type":"output_text","text":"A dog"}]},
        {"type":"image_generation_call","result":encoded}
    ]})
    .to_string();
    let server = Server::json(&body);
    let dir = temp_dir("out-dir");
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{url}\"\n\
         [providers.srv.routes]\ngenerate = \"openai-responses\"",
        url = server.url()
    ));
    let out = run_tty_with(
        &[
            "ask",
            "-p",
            "画一只柴犬并说明",
            "--profile",
            "test",
            "--produce",
            "text,image",
            "--out-dir",
            dir.to_str().unwrap(),
            "--no-stream",
        ],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    assert_eq!(std::fs::read(dir.join("text.txt")).unwrap(), b"A dog");
    assert_eq!(std::fs::read(dir.join("image-1.png")).unwrap(), png);
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["artifacts"].as_array().unwrap().len(), 2);
}

#[cfg(unix)]
#[test]
fn out_dir_keeps_the_previous_delivery_without_overwrite() {
    // Run 1: a plain summary lands as text.txt plus its manifest.
    let server = Server::json(chat_body("first"));
    let dir = temp_dir("out-dir-twice");
    let cfg = chat_cfg(&server.url());
    let out = run(
        &[
            "summarize",
            "--profile",
            "test",
            "--out-dir",
            dir.to_str().unwrap(),
        ],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(
        std::fs::read_to_string(dir.join("text.txt")).unwrap(),
        "first"
    );
    let manifest_before = std::fs::read(dir.join("manifest.json")).unwrap();

    // Run 2: a translate batch names its files a.txt/b.txt, so only the
    // manifest collides. Without --overwrite the run fails before writing
    // a byte: no new files, and the old manifest still describes the
    // first delivery exactly as run 1 left it.
    let inputs = temp_dir("out-dir-twice-inputs");
    let a = inputs.join("a.md");
    let b = inputs.join("b.md");
    std::fs::write(&a, "hello").unwrap();
    std::fs::write(&b, "world").unwrap();
    let server = MultiServer::start(&[chat_body("你好"), chat_body("世界")]);
    let cfg = chat_cfg(&server.url());
    let out = run_tty_with(
        &[
            "translate",
            "--profile",
            "test",
            "--no-stream",
            "--to",
            "zh-CN",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--out-dir",
            dir.to_str().unwrap(),
        ],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(5);
    let err = out.stderr();
    assert!(err.contains("manifest.json"), "{err}");
    assert!(err.contains("--overwrite"), "{err}");
    assert!(!dir.join("a.txt").exists(), "no byte may be written");
    assert!(!dir.join("b.txt").exists(), "no byte may be written");
    assert_eq!(
        std::fs::read(dir.join("manifest.json")).unwrap(),
        manifest_before,
        "the old manifest survives untouched"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("text.txt")).unwrap(),
        "first"
    );

    // Run 3: the same batch with --overwrite replaces the whole delivery;
    // the manifest now lists only this run's artifacts.
    let server = MultiServer::start(&[chat_body("你好"), chat_body("世界")]);
    let cfg = chat_cfg(&server.url());
    let out = run_tty_with(
        &[
            "translate",
            "--profile",
            "test",
            "--no-stream",
            "--to",
            "zh-CN",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--out-dir",
            dir.to_str().unwrap(),
            "--overwrite",
        ],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "你好");
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "世界");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    let files: Vec<_> = manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["file"].as_str().unwrap())
        .collect();
    assert_eq!(files, ["a.txt", "b.txt"]);
}

#[cfg(unix)]
#[test]
fn single_image_to_piped_stdout_is_exact_bytes() {
    let png = solid_png(2, 2);
    let encoded = {
        let alphabet: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in png.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            out.push(alphabet[(n >> 18) as usize & 63] as char);
            out.push(alphabet[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                alphabet[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                alphabet[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    };
    let body = serde_json::json!({"data":[{"b64_json":encoded}]}).to_string();
    let server = Server::json(&body);
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\noperations = [\"image\"]\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let out = run_tty_with(
        &["image", "--profile", "test", "--text", "dog"],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    assert_eq!(out.output.stdout, png);
}

#[test]
fn json_report_replaces_the_body() {
    let server = Server::json(chat_body("hidden body"));
    let dir = temp_dir("out-json");
    let file = dir.join("summary.txt");
    let cfg = chat_cfg(&server.url());
    let out = run(
        &[
            "summarize",
            "--profile",
            "test",
            "--json",
            "-o",
            file.to_str().unwrap(),
        ],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    let report: serde_json::Value = serde_json::from_str(&out.stdout()).unwrap();
    assert_eq!(report["version"], 1);
    assert_eq!(report["artifacts"][0]["kind"], "text");
    assert_eq!(report["artifacts"][0]["size"], "hidden body".len());
    assert!(report["artifacts"][0]["path"]
        .as_str()
        .unwrap()
        .ends_with("summary.txt"));
    let deliveries = report["deliveries"].as_array().unwrap();
    assert_eq!(deliveries.len(), 2);
    assert!(deliveries.iter().all(|d| d["status"] == "succeeded"));
    assert!(report["error"].is_null());
    // the body itself never hits stdout
    assert!(!out.stdout().contains("hidden body") || out.stdout().starts_with('{'));
    assert_eq!(std::fs::read(&file).unwrap(), b"hidden body");
}

#[test]
fn json_conflicts_with_explicit_stdout() {
    let out = run(&["ask", "-p", "hi", "--json", "--stdout"], b"", &[]);
    out.assert_code(2);
    assert!(out.stderr().contains("stdout"), "stderr: {}", out.stderr());
}

/// The `--json` error contract: on usage (2), service (3) and generation
/// (4) failures stdout still carries exactly one valid JSON report — the
/// success report's envelope and field names, with `error` filled in —
/// and stderr keeps the human-readable line.
fn assert_json_error_report(out: &RunOutcome, code: i32, kind: &str) {
    out.assert_code(code);
    let report: serde_json::Value = serde_json::from_str(&out.stdout())
        .unwrap_or_else(|e| panic!("stdout must hold one JSON report: {e}\n{}", out.stdout()));
    assert_eq!(report["version"], 1);
    assert_eq!(report["error"]["kind"], kind, "report: {report}");
    assert!(
        !report["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "report: {report}"
    );
    assert!(out.stderr().contains("error:"), "stderr: {}", out.stderr());
}

#[test]
fn json_error_report_covers_usage_exit_two() {
    // `--produce audio` exceeds what the summarize adapter can output: a
    // preflight usage error, before any run exists.
    let out = run(
        &[
            "summarize",
            "--profile",
            "test",
            "--produce",
            "audio",
            "--json",
            "--text",
            "hi",
        ],
        b"",
        &[],
    );
    assert_json_error_report(&out, 2, "usage");
    let report: serde_json::Value = serde_json::from_str(&out.stdout()).unwrap();
    assert!(report["run_id"].is_null(), "no run yet: {report}");
    assert!(report["task"].is_null(), "no run yet: {report}");
}

#[test]
fn json_error_report_covers_errors_before_clap_parses() {
    // An unknown task fails inside the argv normalizer, before clap could
    // have parsed --json; the naive argv scan must still yield the report.
    let out = run(&["--json", "nosuchtask", "notes.txt"], b"", &[]);
    assert_json_error_report(&out, 2, "usage");
    let report: serde_json::Value = serde_json::from_str(&out.stdout()).unwrap();
    assert!(report["run_id"].is_null(), "no run yet: {report}");
    assert!(report["task"].is_null(), "no run yet: {report}");
}

#[test]
fn json_error_report_covers_clap_parse_errors() {
    // An unknown flag dies inside clap's own parser, which exits without
    // ever reaching fail(); with --json in argv, stdout still carries the
    // usage report while stderr keeps clap's own message. Help (exit 0)
    // must not grow a report.
    let out = run(&["--json", "--bogus-flag"], b"", &[]);
    assert_json_error_report(&out, 2, "usage");
    let report: serde_json::Value = serde_json::from_str(&out.stdout()).unwrap();
    assert!(report["run_id"].is_null(), "no run yet: {report}");
    assert!(report["task"].is_null(), "no run yet: {report}");
    assert!(
        out.stderr().contains("--bogus-flag"),
        "stderr keeps clap's message: {}",
        out.stderr()
    );

    let out = run(&["--json", "--help"], b"", &[]);
    out.assert_code(0);
    assert!(
        out.stdout().contains("Usage"),
        "help stays plain text: {}",
        out.stdout()
    );
}

#[test]
fn json_scan_does_not_treat_a_flag_value_as_the_json_flag() {
    // `--json` here is -p's value, not a request for the report: the
    // clap error must stay plain (no JSON on stdout), because the
    // arity-aware scan knows -p consumes the next token.
    let out = run(&["ask", "-p", "--json", "--bogus-flag"], b"", &[]);
    out.assert_code(2);
    assert!(
        out.stdout().is_empty(),
        "no JSON report without a real --json: {}",
        out.stdout()
    );
    assert!(
        out.stderr().contains("--bogus-flag"),
        "stderr keeps clap's message: {}",
        out.stderr()
    );
}

#[test]
fn json_error_report_covers_service_exit_three() {
    let server = Server::start(
        "500 Internal Server Error",
        r#"{"error":{"message":"service exploded"}}"#,
    );
    let cfg = chat_cfg(&server.url());
    let out = run(
        &["summarize", "--profile", "test", "--json"],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    assert_json_error_report(&out, 3, "service");
    let report: serde_json::Value = serde_json::from_str(&out.stdout()).unwrap();
    assert_eq!(report["task"], "summarize", "report: {report}");
    assert!(report["run_id"].is_string(), "the run existed: {report}");
    assert!(
        out.stderr().contains("service exploded"),
        "stderr: {}",
        out.stderr()
    );
}

#[test]
fn json_error_report_covers_truncated_generation_exit_four() {
    // The same truncated reply as protocol.rs's exit-4 fixture: with
    // --json, stdout carries the error report instead of staying empty.
    let server =
        Server::json(r#"{"choices":[{"message":{"content":"partial"},"finish_reason":"length"}]}"#);
    let cfg = chat_cfg(&server.url());
    let out = run(
        &["summarize", "--profile", "test", "--json"],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    assert_json_error_report(&out, 4, "generation");
    let report: serde_json::Value = serde_json::from_str(&out.stdout()).unwrap();
    assert_eq!(report["task"], "summarize", "report: {report}");
    assert!(report["run_id"].is_string(), "the run existed: {report}");
    assert!(
        out.stderr().contains("not delivered"),
        "stderr: {}",
        out.stderr()
    );
}

#[test]
fn json_report_still_prints_when_a_late_refusal_fails_delivery() {
    // F10 made late refusals exit 5, and F11's fail() skips the Delivery
    // kind because the delivery path prints its own report — so the
    // refusal must reach that report. With --json, stdout still carries
    // exactly one JSON document: the two artifacts, the stdout delivery
    // (this report), the refused file destination, and the error.
    let encoded = encode_png();
    let body = serde_json::json!({"data":[{"b64_json":encoded},{"b64_json":encoded}]}).to_string();
    let server = Server::json(&body);
    let dir = temp_dir("json-refusal");
    let file = dir.join("one.png");
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\noperations = [\"image\"]\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let out = run(
        &[
            "image",
            "--profile",
            "test",
            "--text",
            "dog",
            "-o",
            file.to_str().unwrap(),
            "--json",
        ],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(5);
    let report: serde_json::Value = serde_json::from_str(&out.stdout())
        .unwrap_or_else(|e| panic!("stdout must hold one JSON report: {e}\n{}", out.stdout()));
    assert_eq!(report["version"], 1, "report: {report}");
    assert_eq!(report["error"]["kind"], "delivery", "report: {report}");
    assert!(
        report["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("one file"),
        "report: {report}"
    );
    assert_eq!(report["artifacts"].as_array().unwrap().len(), 2);
    let deliveries = report["deliveries"].as_array().unwrap();
    // stdout (this very report) and the refused file
    assert_eq!(deliveries.len(), 2, "deliveries: {deliveries:?}");
    assert!(
        deliveries
            .iter()
            .any(|d| d["destination"] == "stdout" && d["status"] == "succeeded"),
        "deliveries: {deliveries:?}"
    );
    assert!(
        deliveries.iter().any(|d| d["destination"]
            .as_str()
            .unwrap_or_default()
            .contains("one.png")
            && d["status"]
                .as_str()
                .unwrap_or_default()
                .starts_with("failed")),
        "deliveries: {deliveries:?}"
    );
    assert!(
        !file.exists(),
        "the refused target must not have been written"
    );
    assert!(out.stderr().contains("error:"), "stderr: {}", out.stderr());
}

#[cfg(unix)]
#[test]
fn several_artifacts_cannot_share_bare_stdout() {
    // produce two kinds and pipe stdout: the late check catches it — as a
    // delivery failure (exit 5), since the generation already ran.
    let body = serde_json::json!({"status":"completed","output":[
        {"type":"message","content":[{"type":"output_text","text":"text part"}]},
        {"type":"image_generation_call","result":encode_png()}
    ]})
    .to_string();
    let server = Server::json(&body);
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{url}\"\n\
         [providers.srv.routes]\ngenerate = \"openai-responses\"",
        url = server.url()
    ));
    let out = run_tty_with(
        &[
            "ask",
            "-p",
            "draw and explain",
            "--profile",
            "test",
            "--produce",
            "text,image",
            "--no-stream",
        ],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    assert_eq!(out.code(), 5, "stderr: {}", out.stderr());
    assert!(out
        .stderr()
        .contains("several artifacts cannot share bare stdout"));
    assert!(out.stdout().is_empty());
}

#[cfg(unix)]
#[test]
fn two_images_to_one_file_fail_delivery_but_stay_recoverable() {
    // The service returns two images but only `-o one.png` was given: the
    // generation succeeded, so the refusal is a delivery failure (exit 5),
    // the run's history records the failed file attempt, and
    // `aido last --out-dir` recovers both artifacts without the model.
    let png = solid_png(2, 2);
    let encoded = encode_png();
    let body = serde_json::json!({"data":[{"b64_json":encoded},{"b64_json":encoded}]}).to_string();
    let server = Server::json(&body);
    let history = temp_dir("delivery-history");
    let file = temp_dir("delivery-target").join("one.png");
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\noperations = [\"image\"]\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let out = run_tty_with(
        &[
            "image",
            "--profile",
            "test",
            "--text",
            "dog",
            "-o",
            file.to_str().unwrap(),
        ],
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", history.to_str().unwrap()),
        ],
        cfg.clone(),
    );
    out.assert_code(5);
    let err = out.stderr();
    assert!(err.contains("2 artifacts cannot go to one file"), "{err}");
    assert!(
        !file.exists(),
        "the refused target must not have been written"
    );

    // The run's manifest shows a complete generation and the failed file
    // delivery, next to the kept artifacts.
    let mut entries: Vec<_> = std::fs::read_dir(&history)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(entries.len(), 1, "one run dir, got {entries:?}");
    let run_dir = entries.pop().unwrap();
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(run_dir.join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["generation"]["status"], "complete");
    assert_eq!(manifest["artifacts"].as_array().unwrap().len(), 2);
    let deliveries = manifest["deliveries"].as_array().unwrap();
    assert_eq!(deliveries.len(), 1, "deliveries: {deliveries:?}");
    assert_eq!(deliveries[0]["destination"]["type"], "file");
    assert!(deliveries[0]["status"]["failed"]["error"]
        .as_str()
        .unwrap()
        .contains("one file"));

    // Recovery: redeliver the recorded run into a fresh directory.
    let recovered = temp_dir("delivery-recovered");
    let out = run(
        &["last", "--out-dir", recovered.to_str().unwrap()],
        b"",
        &[
            ("AIDO_CONFIG", empty_config().to_str().unwrap()),
            ("AIDO_HISTORY_DIR", history.to_str().unwrap()),
        ],
    );
    out.assert_code(0);
    assert_eq!(std::fs::read(recovered.join("image-1.png")).unwrap(), png);
    assert_eq!(std::fs::read(recovered.join("image-2.png")).unwrap(), png);
}

#[test]
fn last_json_report_still_prints_when_a_restored_delivery_refuses_an_existing_file() {
    // The restore path (`aido last` → deliver_restored → output::deliver)
    // runs the same JSON epilogue as a live run. Here the -o target already
    // exists and no --overwrite is given, so the refusal happens inside
    // deliver_to_destinations (the no-clobber commit fails): the run must
    // exit 5 with stdout still carrying exactly one JSON report — the
    // recorded artifact, the stdout delivery (this report), the refused
    // file destination, and the delivery error — while the existing file
    // keeps its original bytes.
    let server = Server::json(chat_body("kept in history"));
    let history = temp_dir("restore-refusal-hist");
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 5\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let out = run(
        &["summarize", "--profile", "test"],
        b"hi\n",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", history.to_str().unwrap()),
        ],
    );
    out.assert_code(0);

    // Restore into a target that already exists, without --overwrite.
    let file = temp_dir("restore-refusal-target").join("summary.txt");
    std::fs::write(&file, "original bytes").unwrap();
    let out = run(
        &["last", "--json", "-o", file.to_str().unwrap()],
        b"",
        &[
            ("AIDO_CONFIG", empty_config().to_str().unwrap()),
            ("AIDO_HISTORY_DIR", history.to_str().unwrap()),
        ],
    );
    out.assert_code(5);
    let report: serde_json::Value = serde_json::from_str(&out.stdout())
        .unwrap_or_else(|e| panic!("stdout must hold one JSON report: {e}\n{}", out.stdout()));
    assert_eq!(report["version"], 1, "report: {report}");
    assert_eq!(report["error"]["kind"], "delivery", "report: {report}");
    let message = report["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("summary.txt"), "report: {report}");
    assert!(message.contains("--overwrite"), "report: {report}");
    // The restored report describes the recorded run, not a new one.
    assert!(report["run_id"].is_string(), "report: {report}");
    assert_eq!(report["task"], "summarize", "report: {report}");
    assert_eq!(report["artifacts"].as_array().unwrap().len(), 1);
    let deliveries = report["deliveries"].as_array().unwrap();
    // stdout (this very report) and the refused file
    assert_eq!(deliveries.len(), 2, "deliveries: {deliveries:?}");
    assert!(
        deliveries
            .iter()
            .any(|d| d["destination"] == "stdout" && d["status"] == "succeeded"),
        "deliveries: {deliveries:?}"
    );
    assert!(
        deliveries.iter().any(|d| d["destination"]
            .as_str()
            .unwrap_or_default()
            .contains("summary.txt")
            && d["status"]
                .as_str()
                .unwrap_or_default()
                .starts_with("failed")),
        "deliveries: {deliveries:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "original bytes",
        "the old content survives"
    );
    assert!(out.stderr().contains("error:"), "stderr: {}", out.stderr());
}

#[cfg(unix)]
#[test]
fn clipboard_refusal_records_the_failed_attempt() {
    // --count 2 --copy overflows the clipboard after the generation: a
    // late refusal, so like every delivery failure the attempt must show
    // in the run record (deliveries names the clipboard as failed), the
    // out-dir stays untouched, and `aido last --out-dir` recovers both.
    let encoded = encode_png();
    let body = serde_json::json!({"data":[{"b64_json":encoded},{"b64_json":encoded}]}).to_string();
    let server = Server::json(&body);
    let history = temp_dir("clip-refusal-hist");
    let out_dir = temp_dir("clip-refusal-out");
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\noperations = [\"image\"]\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let out = run_tty_with(
        &[
            "image",
            "--profile",
            "test",
            "--text",
            "dog",
            "--count",
            "2",
            "--copy",
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", history.to_str().unwrap()),
        ],
        cfg.clone(),
    );
    out.assert_code(5);
    assert!(
        out.stderr()
            .contains("the clipboard takes exactly one artifact"),
        "stderr: {}",
        out.stderr()
    );
    assert!(
        !out_dir.join("manifest.json").exists(),
        "a refusal delivers nothing"
    );

    let mut entries: Vec<_> = std::fs::read_dir(&history)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(entries.len(), 1, "one run dir, got {entries:?}");
    let run_dir = entries.pop().unwrap();
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(run_dir.join("manifest.json")).unwrap())
            .unwrap();
    let deliveries = manifest["deliveries"].as_array().unwrap();
    assert_eq!(deliveries.len(), 1, "deliveries: {deliveries:?}");
    assert_eq!(deliveries[0]["destination"]["type"], "clipboard");
    assert!(
        deliveries[0]["status"]["failed"]["error"]
            .as_str()
            .unwrap()
            .contains("exactly one"),
        "deliveries: {deliveries:?}"
    );

    // Recovery: the two images are complete in the record.
    let recovered = temp_dir("clip-refusal-recovered");
    let out = run(
        &["last", "--out-dir", recovered.to_str().unwrap()],
        b"",
        &[
            ("AIDO_CONFIG", empty_config().to_str().unwrap()),
            ("AIDO_HISTORY_DIR", history.to_str().unwrap()),
        ],
    );
    out.assert_code(0);
    assert!(recovered.join("image-1.png").exists());
    assert!(recovered.join("image-2.png").exists());
}

pub fn encode_png() -> String {
    let png = solid_png(2, 2);
    let alphabet: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in png.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(alphabet[(n >> 18) as usize & 63] as char);
        out.push(alphabet[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            alphabet[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            alphabet[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(unix)]
#[test]
fn attached_short_o_delivers_to_the_named_file() {
    // `-ofile` (attached short value) must reach --output, never collapse
    // into the prompt.
    let server = Server::json(chat_body("FILED"));
    let dir = temp_dir("out-attached");
    let file = dir.join("out.txt");
    let cfg = chat_cfg(&server.url());
    let out = run_tty_with(
        &[
            "summarize",
            "--profile",
            "test",
            "--text",
            "hi",
            &format!("-o{}", file.display()),
        ],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "FILED");
}

#[cfg(unix)]
#[test]
fn output_file_mode_follows_the_umask() {
    use std::os::unix::fs::PermissionsExt as _;
    // The child inherits this process's umask; read it the way the
    // delivery code does — umask(0) also sets, so restore immediately.
    let raw = unsafe { libc::umask(0) };
    unsafe { libc::umask(raw) };
    // mode_t is u16 on macOS and u32 on Linux; widen for the mode math.
    #[allow(clippy::unnecessary_cast)] // no-op on Linux, real on macOS
    let mask = raw as u32;
    let server = Server::json(chat_body("UMASKED"));
    let dir = temp_dir("out-umask");
    let file = dir.join("summary.md");
    let cfg = chat_cfg(&server.url());
    let out = run(
        &[
            "summarize",
            "--profile",
            "test",
            "-o",
            file.to_str().unwrap(),
        ],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o666 & !mask, "delivered files follow the umask");
}

#[cfg(unix)]
#[test]
fn history_artifacts_stay_private() {
    use std::os::unix::fs::PermissionsExt as _;
    let server = Server::json(chat_body("KEPT"));
    let history = temp_dir("history-mode");
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 2\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{url}\"",
        url = server.url()
    ));
    let out = run(
        &["summarize", "--profile", "test"],
        b"hi\n",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", history.to_str().unwrap()),
        ],
    );
    out.assert_code(0);
    // save_generation stores the artifact bytes as text.txt inside the
    // one run directory, next to the run manifest.
    let mut entries: Vec<_> = std::fs::read_dir(&history)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(entries.len(), 1, "one run dir, got {entries:?}");
    let run_dir = entries.pop().unwrap();
    let mode = std::fs::metadata(run_dir.join("text.txt"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "history artifacts stay owner-only");
}

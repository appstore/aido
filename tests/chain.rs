//! Task chains: `--then` and the `chain "a | b"` sugar. This file covers
//! the full contract: the plan-time checks (parsing, junction types, the
//! dry-run preview) that fire before any request exists, and the
//! execution semantics — material flow, single history record, failure
//! stop, last-stage delivery.

mod support;

use support::*;

/// A config whose provider points at port 1: nothing listens there, so a
/// test that succeeds proves no request was ever sent.
fn dead_cfg() -> std::path::PathBuf {
    settings_config(
        "default_profile = \"test\"\n\
         [settings]\nhistory_keep = 0\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"",
    )
}

/// A config with history recording on, for the record-shape tests.
fn live_cfg(url: &str) -> std::path::PathBuf {
    settings_config(&format!(
        "default_profile = \"test\"\n\
         [settings]\nhistory_keep = 5\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{url}\""
    ))
}

fn run_dirs(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut ids: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("manifest.json").exists())
        .collect();
    ids.sort();
    ids
}

#[test]
fn chain_dry_run_prints_each_stage_and_sends_nothing() {
    let cfg = dead_cfg();
    let out = run_with(
        &[
            "chain",
            "summarize | translate --to zh-CN",
            "--text",
            "hello",
            "--dry-run",
        ],
        b"",
        &[],
        &cfg,
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("chain: summarize|translate"), "{stdout}");
    assert!(stdout.contains("stage 1/2: summarize"), "{stdout}");
    assert!(stdout.contains("stage 2/2: translate"), "{stdout}");
    assert!(
        stdout.contains("material:    stage 1 output (text)"),
        "{stdout}"
    );
    assert!(stdout.contains("→ feeds stage 2: translate"), "{stdout}");
    assert!(stdout.contains("no request is sent"), "{stdout}");
    // Outer delivery flags land on the last stage.
    let out = run_with(
        &[
            "chain",
            "summarize|translate",
            "--text",
            "hello",
            "-o",
            "out.txt",
            "--dry-run",
        ],
        b"",
        &[],
        &cfg,
    );
    out.assert_code(0);
    assert!(out.stdout().contains("file out.txt"), "{}", out.stdout());
}

#[test]
fn then_form_dry_run_is_the_same_contract() {
    let cfg = dead_cfg();
    let out = run_with(
        &[
            "summarize",
            "--text",
            "hello",
            "--then",
            "translate",
            "--dry-run",
        ],
        b"",
        &[],
        &cfg,
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("chain: summarize|translate"), "{stdout}");
    assert!(stdout.contains("stage 2/2: translate"), "{stdout}");
}

#[test]
fn junction_type_mismatch_exits_2_with_zero_requests() {
    let cfg = dead_cfg();
    // transcribe requires audio; summarize produces text.
    let out = run_with(
        &[
            "chain",
            "summarize|transcribe",
            "--text",
            "hello",
            "--dry-run",
        ],
        b"",
        &[],
        &cfg,
    );
    out.assert_code(2);
    assert!(
        out.stderr().contains("does not accept text input"),
        "stderr: {}",
        out.stderr()
    );
    // The same check fires without --dry-run: the chain dies in planning,
    // still before any request.
    let out = run_with(
        &["chain", "summarize|transcribe", "--text", "hello"],
        b"",
        &[],
        &cfg,
    );
    out.assert_code(2);
}

#[test]
fn per_part_batches_refuse_the_chain_but_single_files_pass() {
    let cfg = dead_cfg();
    let png = solid_png(8, 8);
    let a = temp_file("a.png", &png);
    let b = temp_file("b.png", &png);
    // Two images make ocr's per-part batch real: the stage-1 plan refuses
    // it at plan time (the generic batch rule fires before the chain's
    // own check — same contract: exit 2, zero requests).
    let out = run_with(
        &[
            "chain",
            "ocr|tts",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--dry-run",
        ],
        b"",
        &[],
        &cfg,
    );
    out.assert_code(2);
    assert!(
        out.stderr().contains("one request each"),
        "stderr: {}",
        out.stderr()
    );
    // One image is an ordinary run: the flagship chain plans cleanly.
    let out = run_with(
        &["chain", "ocr|tts", a.to_str().unwrap(), "--dry-run"],
        b"",
        &[],
        &cfg,
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("chain: ocr|tts"), "{stdout}");
    assert!(stdout.contains("stage 2/2: tts"), "{stdout}");
}

#[test]
fn stage_level_flag_outside_the_spec_is_a_usage_error() {
    let cfg = dead_cfg();
    let out = run_with(
        &["chain", "summarize|translate", "--text", "hi", "--to", "en"],
        b"",
        &[],
        &cfg,
    );
    out.assert_code(2);
    assert!(
        out.stderr().contains("inside the chain spec"),
        "stderr: {}",
        out.stderr()
    );
}

#[test]
fn stage_two_material_is_rejected_in_the_then_form() {
    let cfg = dead_cfg();
    let extra = temp_file("extra.txt", b"more");
    let out = run_with(
        &[
            "summarize",
            "--text",
            "hi",
            "--then",
            "translate",
            extra.to_str().unwrap(),
        ],
        b"",
        &[],
        &cfg,
    );
    out.assert_code(2);
    assert!(
        out.stderr().contains("takes no input material"),
        "stderr: {}",
        out.stderr()
    );
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

#[test]
fn three_stage_chain_flows_material_and_delivers_last_stage_only() {
    let server = MultiServer::start(&[chat_body("ONE"), chat_body("TWO"), chat_body("THREE")]);
    let cfg = live_cfg(&server.url());
    let out = run_with(
        &[
            "chain",
            "summarize|translate|code-review",
            "--text",
            "material body",
        ],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        &cfg,
    );
    out.assert_code(0);
    // Only the final stage's body reaches stdout.
    assert_eq!(out.stdout(), "THREE\n");
    // Each stage received the previous stage's reply as its material.
    let requests = server.requests();
    assert_eq!(requests.len(), 3, "one request per stage");
    assert!(
        find_sub(&requests[0], b"material body").is_some(),
        "stage 1 got the material"
    );
    assert!(
        find_sub(&requests[1], b"ONE").is_some(),
        "stage 2 got stage 1's reply"
    );
    assert!(
        find_sub(&requests[2], b"TWO").is_some(),
        "stage 3 got stage 2's reply"
    );
}

#[test]
fn single_history_record_lists_stages_and_global_provenance() {
    let server = MultiServer::start(&[chat_body("ONE"), chat_body("TWO"), chat_body("THREE")]);
    let cfg = live_cfg(&server.url());
    let hist = temp_dir("chain-history");
    let out = run_with(
        &[
            "chain",
            "summarize|translate|code-review",
            "--text",
            "material body",
        ],
        b"",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", hist.to_str().unwrap()),
        ],
        &cfg,
    );
    out.assert_code(0);
    let runs = run_dirs(&hist);
    assert_eq!(runs.len(), 1, "one record for the whole chain");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(runs[0].join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["task"], "summarize|translate|code-review");
    assert_eq!(manifest["generation"]["status"], "complete");
    let stages = manifest["stages"].as_array().unwrap();
    assert_eq!(stages.len(), 3);
    assert_eq!(stages[0]["task"], "summarize");
    assert_eq!(stages[2]["task"], "code-review");
    let artifacts = manifest["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 3, "every stage's artifact is recorded");
    let ids: Vec<&str> = artifacts
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    // Intermediates carry a stage-namespaced stem so repeated tasks — and
    // a task named like the final stage's default `text` — stay distinct.
    assert_eq!(ids, ["stage-1-summarize", "stage-2-translate", "text"]);
    let indices: Vec<usize> = artifacts
        .iter()
        .map(|a| a["provenance"]["index"].as_u64().unwrap() as usize)
        .collect();
    assert_eq!(indices, [0, 1, 2], "run-global request numbering");
}

#[test]
fn chain_minus_o_delivers_the_last_stage_to_the_file() {
    let server = MultiServer::start(&[chat_body("ONE"), chat_body("TWO")]);
    let cfg = live_cfg(&server.url());
    let file = temp_dir("chain-o").join("out.txt");
    let out = run_with(
        &[
            "chain",
            "summarize|translate",
            "--text",
            "hi",
            "-o",
            file.to_str().unwrap(),
        ],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        &cfg,
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "", "explicit destination replaces stdout");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "TWO");
}

#[test]
fn stage2_http_500_stops_the_chain_at_exit_3() {
    let server = MultiServer::start_statuses(&[
        ("200 OK", chat_body("OK1")),
        (
            "500 Internal Server Error",
            "{\"error\":{\"message\":\"boom\"}}",
        ),
    ]);
    let cfg = live_cfg(&server.url());
    let hist = temp_dir("chain-fail-500");
    let out = run_with(
        &["chain", "summarize|translate|code-review", "--text", "hi"],
        b"",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", hist.to_str().unwrap()),
        ],
        &cfg,
    );
    out.assert_code(3);
    assert!(
        out.stderr().contains("stage 2/3 (translate) failed"),
        "stderr: {}",
        out.stderr()
    );
    // The chain stopped: stage 3 never sent anything.
    assert_eq!(server.requests().len(), 2, "stage 3 sent no request");
    // The upstream stage's artifact is kept in history.
    let runs = run_dirs(&hist);
    assert_eq!(runs.len(), 1);
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(runs[0].join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["generation"]["status"], "incomplete");
    let artifacts = manifest["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 1, "only stage 1 produced anything");
    assert_eq!(
        std::fs::read_to_string(runs[0].join("stage-1-summarize.txt")).unwrap(),
        "OK1"
    );
}

#[test]
fn stage2_truncated_generation_exits_4() {
    let server = MultiServer::start_statuses(&[
        ("200 OK", chat_body("OK1")),
        // A finished-but-truncated reply (finish_reason "length"): the
        // adapter returns it as Incomplete, not as a transport error.
        (
            "200 OK",
            "{\"choices\":[{\"message\":{\"content\":\"partial\"},\"finish_reason\":\"length\"}]}",
        ),
    ]);
    let cfg = live_cfg(&server.url());
    let out = run_with(
        &["chain", "summarize|translate", "--text", "hi"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        &cfg,
    );
    out.assert_code(4);
    assert!(
        out.stderr().contains("stage 2/2 (translate) failed"),
        "stderr: {}",
        out.stderr()
    );
}

/// Ctrl+C mid-chain: the completed upstream stages' paid-for artifacts
/// must land in the cancelled record instead of dying with the run. The
/// stage-1 reply answers instantly and the server then writes a marker
/// file the test waits for; the stage-2 reply is held until a gate file
/// appears (it never does), so the client sits mid-chain until the SIGINT
/// drops its connection. Unix-only: it needs kill(2).
#[cfg(unix)]
#[test]
fn ctrl_c_mid_chain_keeps_completed_stages_in_history() {
    use std::io::Write as _;

    let hist = temp_dir("chain-cancel-history");
    let marks = temp_dir("chain-cancel");
    let answered = marks.join("stage1.answered");
    let gate = marks.join("stage2.hold");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    // Detached on purpose: the held stage-2 reply's write fails once the
    // cancelled client hangs up (that is the point), and the thread ends
    // on its own — joining would wait out the hold instead.
    {
        let answered = answered.clone();
        let gate = gate.clone();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            let mut stream = accept(&listener, deadline);
            stream.set_nonblocking(false).unwrap();
            let _ = read_request(&mut stream);
            write_response(&mut stream, "200 OK", chat_body("stage one text"));
            std::fs::write(&answered, b"1").unwrap();
            let mut stream = accept(&listener, deadline);
            stream.set_nonblocking(false).unwrap();
            let _ = read_request(&mut stream);
            let hold = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !gate.exists() && std::time::Instant::now() < hold {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let body = chat_body("stage two");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });
    }

    let cfg = live_cfg(&format!("http://127.0.0.1:{port}"));
    let child = spawn(
        &["chain", "summarize|translate", "--text", "hi"],
        &[("AIDO_HISTORY_DIR", hist.to_str().unwrap())],
        &cfg,
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !answered.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "stage 1 never completed"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // Grace for the client to finish parsing stage 1 and snapshot its
    // artifact into the cancelled-run placeholder.
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) },
        0,
        "kill(SIGINT) failed"
    );
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(130), "exit code after Ctrl+C");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("kept in history"), "stderr: {stderr}");

    let runs = run_dirs(&hist);
    assert_eq!(runs.len(), 1);
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(runs[0].join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["generation"]["status"], "cancelled", "{manifest}");
    assert_eq!(manifest["task"], "summarize|translate");
    assert!(
        manifest["warnings"][0]
            .as_str()
            .unwrap()
            .contains("kept in this record"),
        "{manifest}"
    );
    // One summary per completed stage: only stage 1 finished.
    assert_eq!(manifest["stages"].as_array().unwrap().len(), 1);
    // The paid-for artifact is on disk next to the manifest.
    let artifacts = manifest["artifacts"].as_array().unwrap();
    assert_eq!(
        artifacts.len(),
        1,
        "stage 1's artifact is kept: {artifacts:?}"
    );
    assert_eq!(artifacts[0]["id"], "stage-1-summarize");
    assert_eq!(
        std::fs::read_to_string(runs[0].join("stage-1-summarize.txt")).unwrap(),
        "stage one text"
    );
}

#[test]
fn sugar_and_then_forms_produce_identical_delivery() {
    let bodies = &[chat_body("ONE"), chat_body("TWO")];
    let server_a = MultiServer::start(bodies);
    let cfg_a = live_cfg(&server_a.url());
    let out_a = run_with(
        &["chain", "summarize|translate", "--text", "hi"],
        b"",
        &[("AIDO_CONFIG", cfg_a.to_str().unwrap())],
        &cfg_a,
    );
    out_a.assert_code(0);

    let server_b = MultiServer::start(bodies);
    let cfg_b = live_cfg(&server_b.url());
    let out_b = run_with(
        &["summarize", "--text", "hi", "--then", "translate"],
        b"",
        &[("AIDO_CONFIG", cfg_b.to_str().unwrap())],
        &cfg_b,
    );
    out_b.assert_code(0);

    assert_eq!(out_a.stdout(), out_b.stdout());
    assert_eq!(out_a.code(), out_b.code());
}

#[test]
fn json_report_lists_the_last_stage_under_the_chain_task() {
    let server = MultiServer::start(&[chat_body("ONE"), chat_body("TWO")]);
    let cfg = live_cfg(&server.url());
    let out = run_with(
        &["chain", "summarize|translate", "--text", "hi", "--json"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        &cfg,
    );
    out.assert_code(0);
    let report: serde_json::Value = serde_json::from_str(&out.stdout()).unwrap();
    assert_eq!(report["task"], "summarize|translate");
    let artifacts = report["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 1, "the report carries the final result");
    assert_eq!(artifacts[0]["id"], "text");
}

#[test]
fn outdir_manifest_keeps_every_stage() {
    let server = MultiServer::start(&[chat_body("ONE"), chat_body("TWO")]);
    let cfg = live_cfg(&server.url());
    let dir = temp_dir("chain-outdir");
    let out = run_with(
        &[
            "chain",
            "summarize|translate",
            "--text",
            "hi",
            "--out-dir",
            dir.to_str().unwrap(),
        ],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        &cfg,
    );
    out.assert_code(0);
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    let artifacts = manifest["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 2, "intermediate + final");
    assert_eq!(artifacts[0]["id"], "stage-1-summarize");
    assert_eq!(artifacts[0]["provenance"]["index"], 0);
    assert_eq!(artifacts[1]["id"], "text");
    assert_eq!(artifacts[1]["provenance"]["index"], 1);
    assert_eq!(
        std::fs::read_to_string(dir.join("stage-1-summarize.txt")).unwrap(),
        "ONE"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("text.txt")).unwrap(),
        "TWO"
    );
}

#[test]
fn history_show_redelivers_the_last_stage_only() {
    let server = MultiServer::start(&[chat_body("ONE"), chat_body("TWO")]);
    let cfg = live_cfg(&server.url());
    let hist = temp_dir("chain-restore");
    let out = run_with(
        &["chain", "summarize|translate", "--text", "hi"],
        b"",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", hist.to_str().unwrap()),
        ],
        &cfg,
    );
    out.assert_code(0);
    let file = temp_dir("chain-restore-out").join("again.txt");
    let out = run_with(
        &["history", "show", "1", "-o", file.to_str().unwrap()],
        b"",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", hist.to_str().unwrap()),
        ],
        &cfg,
    );
    out.assert_code(0);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "TWO");
    // The out-dir restore keeps every stage's artifact.
    let dir = temp_dir("chain-restore-dir");
    let out = run_with(
        &["history", "show", "1", "--out-dir", dir.to_str().unwrap()],
        b"",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", hist.to_str().unwrap()),
        ],
        &cfg,
    );
    out.assert_code(0);
    assert!(dir.join("stage-1-summarize.txt").exists());
    assert!(dir.join("text.txt").exists());
}

#[test]
fn long_stage1_reply_chunks_inside_stage2() {
    let long_reply = chat_body(
        &(0..200)
            .map(|i| format!("第{i}段，这是用来测试长文分块的内容句子。"))
            .collect::<Vec<_>>()
            .join("\n\n"),
    );
    let server = MultiServer::start(&[long_reply, chat_body("C1"), chat_body("C2")]);
    let cfg = live_cfg(&server.url());
    let out = run_with(
        &["chain", "summarize|translate", "--text", "hi"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        &cfg,
    );
    out.assert_code(0);
    assert_eq!(
        out.stdout(),
        "C1\n\nC2\n",
        "the joined chunk replies are the final artifact"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 3, "stage 2 chunked the long reply in two");
    assert!(
        find_sub(&requests[1], "第0段".as_bytes()).is_some(),
        "first chunk carries the head"
    );
    assert!(
        find_sub(&requests[2], "第19".as_bytes()).is_some(),
        "second chunk carries the tail"
    );
}

// ---------------------------------------------------------------------------
// Review regressions: the plan-time contract holds under adversarial flags
// ---------------------------------------------------------------------------

#[test]
fn stage2_param_error_fires_before_any_request() {
    // `--count` belongs to image generation, not translate: the chain
    // must die at parse time, not after stage 1 paid its request.
    let cfg = dead_cfg();
    let out = run_with(
        &["chain", "summarize | translate --count 3", "--text", "hi"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        &cfg,
    );
    out.assert_code(2);
    assert!(
        out.stderr().contains("does not accept --count"),
        "stderr: {}",
        out.stderr()
    );
}

#[test]
fn produce_on_a_non_last_stage_is_refused() {
    // A stage's outputs are fixed by the junction contract; merging
    // --produce across stages would reshape the last stage's request
    // after earlier stages paid.
    let cfg = dead_cfg();
    let out = run_with(
        &["chain", "summarize --produce text | tts", "--text", "hi"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        &cfg,
    );
    out.assert_code(2);
    assert!(
        out.stderr().contains("belongs to a single stage"),
        "stderr: {}",
        out.stderr()
    );
}

#[test]
fn cross_stage_output_and_outdir_conflict_is_usage() {
    // Clap only sees conflicts per stage; the merged run-level surface
    // must refuse the combination the single-run surface refuses.
    let cfg = dead_cfg();
    let out = run_with(
        &[
            "chain",
            "summarize -o somewhere.md | translate",
            "--text",
            "hi",
            "--out-dir",
            "somewhere-else",
        ],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        &cfg,
    );
    out.assert_code(2);
    assert!(
        out.stderr().contains("two delivery contracts"),
        "stderr: {}",
        out.stderr()
    );
}

#[test]
fn chain_help_prints_usage_without_a_spec() {
    let cfg = dead_cfg();
    let out = run_with(
        &["chain", "--help"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        &cfg,
    );
    out.assert_code(0);
    assert!(out.stdout().contains("Usage"), "{}", out.stdout());
    assert!(out.stdout().contains("chain"), "{}", out.stdout());
}

#[test]
fn o_target_collision_fails_before_requests() {
    let cfg = dead_cfg();
    let existing = temp_file("chain-collision.txt", b"occupied");
    let out = run_with(
        &[
            "chain",
            "summarize|translate",
            "--text",
            "hi",
            "-o",
            existing.to_str().unwrap(),
        ],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        &cfg,
    );
    out.assert_code(2);
    assert!(
        out.stderr().contains("already exists"),
        "stderr: {}",
        out.stderr()
    );
}

#[test]
fn repeated_task_names_keep_distinct_artifacts() {
    let server = MultiServer::start(&[chat_body("A"), chat_body("B"), chat_body("C")]);
    let cfg = live_cfg(&server.url());
    let dir = temp_dir("chain-dupe");
    let out = run_with(
        &[
            "chain",
            "summarize|summarize|summarize",
            "--text",
            "hi",
            "--out-dir",
            dir.to_str().unwrap(),
        ],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        &cfg,
    );
    out.assert_code(0);
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    let ids: Vec<&str> = manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["stage-1-summarize", "stage-2-summarize", "text"],
        "no collisions"
    );
    assert!(dir.join("stage-1-summarize.txt").exists());
    assert!(dir.join("stage-2-summarize.txt").exists());
}

/// A custom task literally named `text` used to rename stage 1's artifact
/// to the final stage's default id — one file silently overwrote the
/// other in `--out-dir` and in history. The stage-namespaced intermediate
/// stem keeps them apart, and the pre-write uniqueness check (shared with
/// history) turns any residual mapping into an error, never an overwrite.
#[test]
fn custom_task_named_text_does_not_collide_with_the_final_text() {
    let tasks = temp_dir("chain-text-task");
    std::fs::write(
        tasks.join("text.toml"),
        "operation = \"generate\"\n\
         input_types = [\"text\"]\n\
         output_types = [\"text\"]\n",
    )
    .unwrap();
    let server = MultiServer::start(&[chat_body("ONE"), chat_body("TWO")]);
    let cfg = live_cfg(&server.url());
    let hist = temp_dir("chain-text-collide");
    let dir = temp_dir("chain-text-collide-dir");
    let out = run_with(
        &[
            "chain",
            "text | translate",
            "--text",
            "hi",
            "--out-dir",
            dir.to_str().unwrap(),
        ],
        b"",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_TASKS_DIR", tasks.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", hist.to_str().unwrap()),
        ],
        &cfg,
    );
    out.assert_code(0);
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    let artifacts = manifest["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 2, "intermediate + final: {manifest}");
    assert_eq!(artifacts[0]["id"], "stage-1-text");
    assert_eq!(artifacts[1]["id"], "text");
    let files: Vec<&str> = artifacts
        .iter()
        .map(|a| a["file"].as_str().unwrap())
        .collect();
    assert_ne!(files[0], files[1], "the manifest names two real files");
    assert_eq!(std::fs::read_to_string(dir.join(files[0])).unwrap(), "ONE");
    assert_eq!(std::fs::read_to_string(dir.join(files[1])).unwrap(), "TWO");
    // History holds both too, under the same distinct names.
    let runs = run_dirs(&hist);
    assert_eq!(runs.len(), 1);
    let hmanifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(runs[0].join("manifest.json")).unwrap())
            .unwrap();
    let harts = hmanifest["artifacts"].as_array().unwrap();
    assert_eq!(harts.len(), 2);
    assert_ne!(harts[0]["file"], harts[1]["file"]);
    assert!(runs[0].join(harts[0]["file"].as_str().unwrap()).exists());
    assert!(runs[0].join(harts[1]["file"].as_str().unwrap()).exists());
}

// ---------------------------------------------------------------------------
// Review regressions, round two: merged run-level flags must be visible to
// the last stage's plan checks, and a mis-shaped stage must fail the chain
// instead of impersonating its result.
// ---------------------------------------------------------------------------

#[test]
fn then_form_outer_outdir_reaches_the_last_stage_preflight() {
    // The run-level --out-dir sits in stage 1's argv in the --then form;
    // after the merge it satisfies image's multi-count delivery rule.
    // Both spellings must plan identically (this used to hard-reject).
    let cfg = dead_cfg();
    let out = run_with(
        &[
            "summarize",
            "--text",
            "hi",
            "--out-dir",
            "imgs",
            "--then",
            "image",
            "--count",
            "5",
            "--dry-run",
        ],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        &cfg,
    );
    out.assert_code(0);
    assert!(
        out.stdout().contains("stage 2/2: image"),
        "{}",
        out.stdout()
    );
}

#[test]
fn merged_minus_o_encoding_conflict_fails_at_parse_zero_requests() {
    let cfg = dead_cfg();
    // `-o out.png` rides in stage 1's argv; merged onto the tts stage it
    // contradicts the mp3 encoding. The conflict must die at parse time —
    // stage 1's request used to fire first (a paid request before a
    // usage error). Exit 2 against a dead endpoint proves no request.
    let out = run_with(
        &[
            "summarize",
            "--text",
            "hi",
            "-o",
            "out.png",
            "--then",
            "tts",
        ],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        &cfg,
    );
    out.assert_code(2);
    assert!(
        out.stderr().contains("output encoding"),
        "stderr: {}",
        out.stderr()
    );
    // The sugar spelling converges to the same parse-time rejection.
    let out = run_with(
        &["chain", "summarize|tts", "--text", "hi", "-o", "out.png"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        &cfg,
    );
    out.assert_code(2);
    assert!(
        out.stderr().contains("output encoding"),
        "stderr: {}",
        out.stderr()
    );
}

#[test]
fn stream_on_a_nonspeaking_last_stage_fails_at_parse() {
    let cfg = dead_cfg();
    // --stream merges onto the edge-tts stage, whose adapter cannot
    // stream: refused at parse (exit 2), not after stage 1 paid.
    let out = run_with(
        &["chain", "summarize|tts", "--text", "hi", "--stream"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        &cfg,
    );
    out.assert_code(2);
    assert!(
        out.stderr().contains("does not support --stream"),
        "stderr: {}",
        out.stderr()
    );
}

#[test]
fn chain_short_version_flag_prints_the_version() {
    let cfg = dead_cfg();
    let out = run_with(
        &["chain", "-V"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        &cfg,
    );
    out.assert_code(0);
    assert!(out.stdout().contains("aido"), "{}", out.stdout());
}

#[test]
fn empty_last_stage_reply_is_a_failure_not_a_delivery() {
    // Stage 2 answers 200 with empty content: it completes with no
    // artifact. The chain must refuse delivery of stage 1's artifact and
    // record an incomplete generation — not hand "OK1" to -o with exit 0.
    let server = MultiServer::start_statuses(&[
        ("200 OK", chat_body("OK1")),
        (
            "200 OK",
            "{\"choices\":[{\"message\":{\"content\":\"\"},\"finish_reason\":\"stop\"}]}",
        ),
    ]);
    let cfg = live_cfg(&server.url());
    let hist = temp_dir("chain-empty-last");
    let file = temp_dir("chain-empty-last-o").join("out.txt");
    let out = run_with(
        &[
            "chain",
            "summarize|translate",
            "--text",
            "hi",
            "-o",
            file.to_str().unwrap(),
        ],
        b"",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", hist.to_str().unwrap()),
        ],
        &cfg,
    );
    out.assert_code(4);
    assert_eq!(out.stdout(), "", "nothing is delivered");
    assert!(!file.exists(), "stage 1's artifact must not pose as -o");
    let runs = run_dirs(&hist);
    assert_eq!(runs.len(), 1);
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(runs[0].join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["generation"]["status"], "incomplete");
    let artifacts = manifest["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 1, "only stage 1 produced anything");
}

#[test]
fn empty_middle_stage_stops_the_chain_and_keeps_upstream() {
    // Stage 2 finishes with no artifact: the junction stops the chain as
    // a stage failure (exit 4), stage 3 sends nothing, and stage 1's
    // paid artifact stays in history instead of being lost with a bare
    // error.
    let server = MultiServer::start_statuses(&[
        ("200 OK", chat_body("OK1")),
        (
            "200 OK",
            "{\"choices\":[{\"message\":{\"content\":\"\"},\"finish_reason\":\"stop\"}]}",
        ),
    ]);
    let cfg = live_cfg(&server.url());
    let hist = temp_dir("chain-empty-mid");
    let out = run_with(
        &["chain", "summarize|translate|code-review", "--text", "hi"],
        b"",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", hist.to_str().unwrap()),
        ],
        &cfg,
    );
    out.assert_code(4);
    assert!(
        out.stderr()
            .contains("stage 2/3 (translate) produced 0 text artifact(s)"),
        "stderr: {}",
        out.stderr()
    );
    assert_eq!(server.requests().len(), 2, "stage 3 sent no request");
    let runs = run_dirs(&hist);
    assert_eq!(runs.len(), 1);
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(runs[0].join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["generation"]["status"], "incomplete");
    let artifacts = manifest["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 1, "stage 1's artifact is kept");
    assert_eq!(
        std::fs::read_to_string(runs[0].join("stage-1-summarize.txt")).unwrap(),
        "OK1"
    );
}

#[test]
fn failed_stage_cause_reaches_stderr_and_the_record() {
    let server = MultiServer::start_statuses(&[
        ("200 OK", chat_body("OK1")),
        (
            "500 Internal Server Error",
            "{\"error\":{\"message\":\"boom the service exploded\"}}",
        ),
    ]);
    let cfg = live_cfg(&server.url());
    let hist = temp_dir("chain-cause");
    let out = run_with(
        &["chain", "summarize|translate|code-review", "--text", "hi"],
        b"",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", hist.to_str().unwrap()),
        ],
        &cfg,
    );
    out.assert_code(3);
    assert!(
        out.stderr().contains("boom the service exploded"),
        "stderr: {}",
        out.stderr()
    );
    let runs = run_dirs(&hist);
    assert_eq!(runs.len(), 1);
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(runs[0].join("manifest.json")).unwrap())
            .unwrap();
    let reason = manifest["generation"]["reason"].as_str().unwrap();
    assert!(
        reason.contains("boom the service exploded"),
        "record reason: {reason}"
    );
}

#[test]
fn no_history_records_nothing_even_when_a_chain_stage_fails() {
    // Success and mid-chain failure alike: --no-history on the run leaves
    // no record, even though the failure record's plan travels from an
    // earlier stage.
    let server = MultiServer::start_statuses(&[
        ("200 OK", chat_body("OK1")),
        ("200 OK", chat_body("OK2")),
        (
            "500 Internal Server Error",
            "{\"error\":{\"message\":\"boom\"}}",
        ),
        ("200 OK", chat_body("OK1")),
        ("200 OK", chat_body("OK2")),
    ]);
    let cfg = live_cfg(&server.url());
    let hist = temp_dir("chain-no-history");
    let out = run_with(
        &[
            "chain",
            "summarize|translate|code-review",
            "--text",
            "hi",
            "--no-history",
        ],
        b"",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", hist.to_str().unwrap()),
        ],
        &cfg,
    );
    out.assert_code(3);
    assert!(run_dirs(&hist).is_empty(), "a failed chain keeps no record");
    let out = run_with(
        &[
            "chain",
            "summarize|translate",
            "--text",
            "hi",
            "--no-history",
        ],
        b"",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", hist.to_str().unwrap()),
        ],
        &cfg,
    );
    out.assert_code(0);
    assert!(
        run_dirs(&hist).is_empty(),
        "a successful chain keeps no record"
    );
}

#[test]
fn then_form_stage1_batch_with_outer_outdir_is_refused_without_paying() {
    // A real per-part batch as stage 1 used to run and pay when --out-dir
    // sat on stage 1 in the --then form, failing only at the junction.
    // The delivery flag now merges to the last stage, and the batch gate
    // refuses before any request (dead endpoint: exit 2, not exit 3).
    let cfg = dead_cfg();
    let png = solid_png(8, 8);
    let a = temp_file("ba.png", &png);
    let b = temp_file("bb.png", &png);
    let out = run_with(
        &[
            "ocr",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--out-dir",
            "pages",
            "--then",
            "translate",
            "--to",
            "zh-CN",
        ],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        &cfg,
    );
    out.assert_code(2);
    assert!(
        out.stderr().contains("hand off a single result"),
        "stderr: {}",
        out.stderr()
    );
}

// ---------------------------------------------------------------------------
// Review regressions, round three: the parse-time preflight must cover
// every judgment that does not need real material, so a chain can never
// pay stage 1 before learning that a later stage cannot run.
// ---------------------------------------------------------------------------

/// Without the edge-tts feature the tts stage cannot run at all; the
/// refusal must come at parse time (exit 2, zero requests) instead of
/// after stage 1's paid request. An exit of 3 here would mean the request
/// went out (connection refused), so the code alone proves the contract.
#[cfg(not(feature = "edge-tts"))]
#[test]
fn chain_rejects_unavailable_edge_tts_before_stage1_request() {
    // Stage 1 keeps a plain chat route; the tts stage's profile routes
    // speech to the edge-tts adapter the binary was built without.
    let cfg = settings_config(
        "default_profile = \"test\"\n\
         [settings]\nhistory_keep = 0\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [profiles.speech]\nprovider = \"msft\"\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"\n\
         [providers.msft]\n[providers.msft.routes]\nspeech = \"edge-tts\"",
    );
    let out = run_with(
        &["chain", "summarize | tts --profile speech", "--text", "hi"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        &cfg,
    );
    out.assert_code(2);
    assert!(
        out.stderr()
            .contains("stage 2 (tts): the 'edge-tts' adapter is not compiled"),
        "stderr: {}",
        out.stderr()
    );
}

/// The junction hands exactly one text part downstream, so the
/// downstream's `max_inputs` is judged at parse time with the same rule
/// the plan build applies — before stage 1 pays.
#[test]
fn downstream_max_inputs_zero_fails_the_junction_before_stage1() {
    let tasks = temp_dir("chain-max-inputs");
    std::fs::write(
        tasks.join("solo.toml"),
        "operation = \"generate\"\n\
         input_types = [\"text\"]\n\
         max_inputs = 0\n\
         output_types = [\"text\"]\n",
    )
    .unwrap();
    let out = run_with(
        &["chain", "summarize | solo", "--text", "hi"],
        b"",
        &[("AIDO_TASKS_DIR", tasks.to_str().unwrap())],
        dead_cfg(),
    );
    out.assert_code(2);
    assert!(
        out.stderr().contains("accepts at most 0 input(s)"),
        "stderr: {}",
        out.stderr()
    );
    assert!(
        out.stderr()
            .contains("stage 1 (summarize) → stage 2 (solo)"),
        "stderr: {}",
        out.stderr()
    );
}

/// `--json` inside the chain spec is invisible to the raw argv scan (the
/// spec is one shell token), yet the run itself would report as JSON — so
/// a parse/preflight error must honor the same contract and print the
/// error envelope, not plain text.
#[test]
fn chain_json_inside_the_spec_formats_parse_errors_as_json() {
    // translate refuses --count: a preflight error, zero requests.
    let out = run_with(
        &[
            "chain",
            "summarize | translate --count 3 --json",
            "--text",
            "hi",
        ],
        b"",
        &[],
        dead_cfg(),
    );
    out.assert_code(2);
    let report: serde_json::Value = serde_json::from_str(&out.stdout())
        .unwrap_or_else(|e| panic!("stdout is not JSON: {e}; stdout: {}", out.stdout()));
    assert_eq!(report["error"]["kind"], "usage", "{report}");
    assert!(
        !out.stderr().contains("\"version\""),
        "stderr must not carry the JSON report: {}",
        out.stderr()
    );

    // The outer --json spelling behaves the same (regression guard).
    let out = run_with(
        &[
            "chain",
            "summarize | translate --count 3",
            "--json",
            "--text",
            "hi",
        ],
        b"",
        &[],
        dead_cfg(),
    );
    out.assert_code(2);
    let report: serde_json::Value = serde_json::from_str(&out.stdout())
        .unwrap_or_else(|e| panic!("stdout is not JSON: {e}; stdout: {}", out.stdout()));
    assert_eq!(report["error"]["kind"], "usage", "{report}");
}

/// A `--json` that is some flag's value is not a request for JSON, even
/// when a sibling stage fails to parse: the arity-aware scan must not
/// mistake the value for the flag.
#[test]
fn json_as_a_flag_value_inside_a_stage_is_not_json() {
    // Stage 1's -p swallows "--json" as its value; stage 2 fails to parse.
    let out = run_with(
        &[
            "chain",
            "summarize -p --json | translate --count 3",
            "--text",
            "hi",
        ],
        b"",
        &[],
        dead_cfg(),
    );
    out.assert_code(2);
    assert!(
        !out.stdout().contains("\"version\""),
        "the value-position --json must not produce a JSON report: {}",
        out.stdout()
    );
    assert!(
        out.stderr().contains("does not accept --count"),
        "stderr: {}",
        out.stderr()
    );
}

/// Stage `--help`/`--version` is a display request, not a run: it must
/// work without a readable config (a single task's does), exit 0, and
/// print to stdout. The library never terminates the process for it.
#[test]
fn chain_help_and_version_need_no_config() {
    let broken = settings_config("invalid = [");

    // A stage's --help prints usage (the shared clap surface — which
    // stage's banner shows is today's behavior and stays that way).
    let out = run_with(&["chain", "summarize --help | tts"], b"", &[], &broken);
    assert_eq!(out.code(), 0, "stderr: {}", out.stderr());
    assert!(out.stdout().contains("Usage"), "{}", out.stdout());

    // A stage's --version prints the version.
    let out = run_with(&["chain", "summarize -V | tts"], b"", &[], &broken);
    assert_eq!(out.code(), 0, "stderr: {}", out.stderr());
    assert!(out.stdout().contains("aido 0."), "{}", out.stdout());

    // The --then form: the help lands in the last stage's argv.
    let out = run_with(&["summarize", "--then", "tts", "--help"], b"", &[], &broken);
    assert_eq!(out.code(), 0, "stderr: {}", out.stderr());
    assert!(out.stdout().contains("Usage"), "{}", out.stdout());

    // Top-level chain help/version stay clap-owned and config-free too.
    let out = run_with(&["chain", "--help"], b"", &[], &broken);
    assert_eq!(out.code(), 0);
    let out = run_with(&["chain", "-V"], b"", &[], &broken);
    assert_eq!(out.code(), 0);

    // A real syntax error still exits 2 with its stage prefix — no config
    // was needed to know that.
    let out = run_with(&["chain", "summarize --bogus | tts"], b"", &[], &broken);
    out.assert_code(2);
    assert!(
        out.stderr()
            .contains("stage 1 (summarize): error: unexpected argument '--bogus'"),
        "stderr: {}",
        out.stderr()
    );
}

/// A normalize-time error (an unknown task, found while walking the
/// stages) happens before `Normalized::wants_json()` can run, so the raw
/// argv scan must see the spec's `--json` itself — the error envelope,
/// not plain text.
#[test]
fn chain_json_inside_spec_formats_normalize_errors_as_json() {
    let out = run_with(
        &["chain", "summarize --json | transalte", "--text", "hi"],
        b"",
        &[],
        dead_cfg(),
    );
    out.assert_code(2);
    let report: serde_json::Value = serde_json::from_str(&out.stdout())
        .unwrap_or_else(|e| panic!("stdout is not JSON: {e}; stdout: {}", out.stdout()));
    assert_eq!(report["error"]["kind"], "usage", "{report}");
    assert!(
        report["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("unknown task 'transalte'")),
        "{report}"
    );
}

/// A `--json` that is a stage's prompt value is not a request for JSON:
/// the normalize error stays plain text.
#[test]
fn json_used_as_prompt_does_not_format_normalize_errors_as_json() {
    let out = run_with(
        &["chain", "summarize -p --json | transalte", "--text", "hi"],
        b"",
        &[],
        dead_cfg(),
    );
    out.assert_code(2);
    assert!(
        serde_json::from_str::<serde_json::Value>(&out.stdout()).is_err(),
        "stdout must not be JSON: {}",
        out.stdout()
    );
    assert!(
        out.stderr().contains("unknown task 'transalte'"),
        "stderr: {}",
        out.stderr()
    );
}

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
    assert_eq!(ids, ["summarize", "translate", "text"]);
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
        std::fs::read_to_string(runs[0].join("summarize.txt")).unwrap(),
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
    assert_eq!(artifacts[0]["id"], "summarize");
    assert_eq!(artifacts[0]["provenance"]["index"], 0);
    assert_eq!(artifacts[1]["id"], "text");
    assert_eq!(artifacts[1]["provenance"]["index"], 1);
    assert_eq!(
        std::fs::read_to_string(dir.join("summarize.txt")).unwrap(),
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
    assert!(dir.join("summarize.txt").exists());
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

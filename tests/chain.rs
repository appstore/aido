//! Task chains: `--then` and the `chain "a | b"` sugar. This file covers
//! the plan-time contract — parsing, junction type checks, the dry-run
//! preview — all of which must fire before any request exists. Execution
//! tests land with the chain runner.

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

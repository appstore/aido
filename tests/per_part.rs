//! The per-part batch strategy end to end: multiple file inputs become
//! one request sequence per file, artifacts carry the input's name, and a
//! failing part never sinks the parts that succeeded.

mod support;

use support::*;

/// Two small images in one fresh directory, named exactly `a.png` / `b.png`
/// so artifact names are predictable (temp_file's counter prefixes would
/// leak into the stems).
fn two_images(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = temp_dir(tag);
    let png = solid_png(2, 2);
    let a = dir.join("a.png");
    let b = dir.join("b.png");
    std::fs::write(&a, &png).unwrap();
    std::fs::write(&b, &png).unwrap();
    (a, b)
}

fn batch_cfg(url: &str) -> std::path::PathBuf {
    settings_config(&format!(
        "[settings]\nhistory_keep = 0\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"test-model\"\n\
         [providers.srv]\nbase_url = \"{url}\""
    ))
}

/// Run aido with a terminal stdin (file material, no `-`), the test
/// profile selected, and nothing else leaking in.
fn run_ocr(cfg: &std::path::Path, args: &[&str]) -> RunOutcome {
    let arg = cfg.display().to_string();
    run_tty_with(args, &[("AIDO_CONFIG", arg.as_str())], cfg.to_path_buf())
}

#[test]
fn two_images_become_two_requests_and_two_named_files() {
    let (a, b) = two_images("perpart-basic");
    let out_dir = temp_dir("perpart-basic-out");
    let server = MultiServer::start(&[chat_body("text of A"), chat_body("text of B")]);
    let cfg = batch_cfg(&server.url());
    let out = run_ocr(
        &cfg,
        &[
            "ocr",
            "--profile",
            "test",
            "--no-stream",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    out.assert_code(0);
    assert_eq!(
        std::fs::read_to_string(out_dir.join("a.txt")).unwrap(),
        "text of A"
    );
    assert_eq!(
        std::fs::read_to_string(out_dir.join("b.txt")).unwrap(),
        "text of B"
    );
    // One request per image, each carrying exactly that image.
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(count_images(&requests[0]), 1);
    assert_eq!(count_images(&requests[1]), 1);
    // The manifest explains where every artifact came from.
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out_dir.join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["artifacts"][0]["id"], "a");
    assert_eq!(manifest["artifacts"][0]["provenance"]["type"], "request");
    assert_eq!(manifest["artifacts"][1]["id"], "b");
}

#[test]
fn a_failing_part_does_not_sink_the_batch() {
    let (a, b) = two_images("perpart-partial");
    let out_dir = temp_dir("perpart-partial-out");
    let server = MultiServer::start_statuses(&[
        ("200 OK", chat_body("text of A")),
        (
            "500 Internal Server Error",
            r#"{"error":{"message":"boom"}}"#,
        ),
    ]);
    let cfg = batch_cfg(&server.url());
    let out = run_ocr(
        &cfg,
        &[
            "ocr",
            "--profile",
            "test",
            "--no-stream",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    out.assert_code(6);
    assert_eq!(
        std::fs::read_to_string(out_dir.join("a.txt")).unwrap(),
        "text of A"
    );
    assert!(!out_dir.join("b.txt").exists());
    let err = out.stderr();
    assert!(err.contains("part 'b.png' failed"), "{err}");
    assert!(err.contains("1/2 input part(s) failed"), "{err}");
}

#[test]
fn all_parts_failing_exits_four_without_delivery() {
    let (a, b) = two_images("perpart-allfail");
    let out_dir = temp_dir("perpart-allfail-out");
    let boom = r#"{"error":{"message":"boom"}}"#;
    let server = MultiServer::start_statuses(&[
        ("500 Internal Server Error", boom),
        ("500 Internal Server Error", boom),
    ]);
    let cfg = batch_cfg(&server.url());
    let out = run_ocr(
        &cfg,
        &[
            "ocr",
            "--profile",
            "test",
            "--no-stream",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    out.assert_code(4);
    let err = out.stderr();
    assert!(err.contains("all 2 input part(s) failed"), "{err}");
    assert!(!out_dir.join("a.txt").exists());
    assert!(!out_dir.join("b.txt").exists());
}

#[test]
fn a_batch_without_out_dir_is_refused_before_any_request() {
    let (a, b) = two_images("perpart-nodir");
    let cfg = batch_cfg("http://127.0.0.1:1");
    let out = run_ocr(
        &cfg,
        &[
            "ocr",
            "--profile",
            "test",
            "--no-stream",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
        ],
    );
    out.assert_code(2);
    assert!(out.stderr().contains("--out-dir"), "{}", out.stderr());
}

#[test]
fn a_single_file_keeps_the_legacy_delivery() {
    let (a, _b) = two_images("perpart-single");
    let out_dir = temp_dir("perpart-single-out");
    let server = MultiServer::start(&[chat_body("only text")]);
    let cfg = batch_cfg(&server.url());
    let out = run_ocr(
        &cfg,
        &[
            "ocr",
            "--profile",
            "test",
            "--no-stream",
            a.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    out.assert_code(0);
    // One part is not a batch: the artifact keeps its program name.
    assert!(out_dir.join("text.txt").exists());
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn unicode_stems_name_their_artifacts() {
    let dir = temp_dir("perpart-cjk");
    let shot = dir.join("截图.png");
    std::fs::write(&shot, solid_png(2, 2)).unwrap();
    let other = dir.join("a.png");
    std::fs::write(&other, solid_png(2, 2)).unwrap();
    let out_dir = temp_dir("perpart-cjk-out");
    let server = MultiServer::start(&[chat_body("文本"), chat_body("fallback")]);
    let cfg = batch_cfg(&server.url());
    let out = run_ocr(
        &cfg,
        &[
            "ocr",
            "--profile",
            "test",
            "--no-stream",
            shot.to_str().unwrap(),
            other.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    out.assert_code(0);
    assert!(out_dir.join("截图.txt").exists());
    assert!(out_dir.join("a.txt").exists());
}

#[test]
fn same_stem_in_two_directories_gets_a_suffix() {
    let png = solid_png(2, 2);
    let dir = temp_dir("perpart-collide");
    let a = dir.join("a.png");
    std::fs::write(&a, &png).unwrap();
    let sub = dir.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let b = sub.join("a.png");
    std::fs::write(&b, &png).unwrap();
    let out_dir = temp_dir("perpart-collide-out");
    let server = MultiServer::start(&[chat_body("first a"), chat_body("second a")]);
    let cfg = batch_cfg(&server.url());
    let out = run_ocr(
        &cfg,
        &[
            "ocr",
            "--profile",
            "test",
            "--no-stream",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    out.assert_code(0);
    assert_eq!(
        std::fs::read_to_string(out_dir.join("a.txt")).unwrap(),
        "first a"
    );
    assert_eq!(
        std::fs::read_to_string(out_dir.join("a-2.txt")).unwrap(),
        "second a"
    );
}

#[test]
fn shared_text_rides_with_every_part() {
    let (a, b) = two_images("perpart-shared");
    let out_dir = temp_dir("perpart-shared-out");
    let server = MultiServer::start(&[chat_body("A"), chat_body("B")]);
    let cfg = batch_cfg(&server.url());
    let out = run_ocr(
        &cfg,
        &[
            "ocr",
            "--profile",
            "test",
            "--no-stream",
            "--text",
            "focus on headers",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    out.assert_code(0);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    for raw in &requests {
        let body = String::from_utf8_lossy(raw);
        assert!(body.contains("focus on headers"), "{body}");
        assert_eq!(count_images(raw), 1, "no other image may ride along");
    }
}

#[test]
fn no_split_still_batches_one_request_per_file() {
    let (a, b) = two_images("perpart-nosplit");
    let out_dir = temp_dir("perpart-nosplit-out");
    let server = MultiServer::start(&[chat_body("A"), chat_body("B")]);
    let cfg = batch_cfg(&server.url());
    let out = run_ocr(
        &cfg,
        &[
            "ocr",
            "--profile",
            "test",
            "--no-stream",
            "--no-split",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    out.assert_code(0);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(count_images(&requests[0]), 1);
    assert_eq!(count_images(&requests[1]), 1);
    assert!(out_dir.join("a.txt").exists());
    assert!(out_dir.join("b.txt").exists());
}

#[test]
fn a_dry_run_batch_without_out_dir_is_refused_like_the_real_run() {
    // The delivery-target rules are pure prechecks, so --dry-run does not
    // exempt a batch from them: no plan is shown for a run that would be
    // rejected anyway.
    let (a, b) = two_images("perpart-dry-nodir");
    let cfg = batch_cfg("http://127.0.0.1:1");
    let out = run_ocr(
        &cfg,
        &[
            "ocr",
            "--profile",
            "test",
            "--dry-run",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
        ],
    );
    out.assert_code(2);
    assert!(out.stderr().contains("--out-dir"), "{}", out.stderr());
    assert!(!out.stdout().contains("per-part batch"), "{}", out.stdout());
}

#[test]
fn dry_run_shows_the_batch_plan_without_requesting() {
    // The dead base_url is the point: a valid batch dry-runs to a printed
    // plan and exit 0, so no request can have gone anywhere.
    let (a, b) = two_images("perpart-dry");
    let out_dir = temp_dir("perpart-dry-out");
    let cfg = batch_cfg("http://127.0.0.1:1");
    let out = run_ocr(
        &cfg,
        &[
            "ocr",
            "--profile",
            "test",
            "--dry-run",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("per-part batch"), "{stdout}");
    assert!(stdout.contains("a.png"), "{stdout}");
    assert!(stdout.contains("b.png"), "{stdout}");
    // The plan's destinations name the collection directory.
    assert!(
        stdout.contains(&format!("directory {}", out_dir.display())),
        "{stdout}"
    );
    // And nothing was delivered: the directory stays empty.
    assert_eq!(std::fs::read_dir(&out_dir).unwrap().count(), 0);
}

#[test]
fn translate_applies_per_part_to_multiple_files() {
    let dir = temp_dir("perpart-translate");
    let a = dir.join("a.md");
    let b = dir.join("b.md");
    std::fs::write(&a, "hello").unwrap();
    std::fs::write(&b, "world").unwrap();
    let out_dir = temp_dir("perpart-translate-out");
    let server = MultiServer::start(&[chat_body("你好"), chat_body("世界")]);
    let cfg = batch_cfg(&server.url());
    let out = run_ocr(
        &cfg,
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
            out_dir.to_str().unwrap(),
        ],
    );
    out.assert_code(0);
    assert_eq!(
        std::fs::read_to_string(out_dir.join("a.txt")).unwrap(),
        "你好"
    );
    assert_eq!(
        std::fs::read_to_string(out_dir.join("b.txt")).unwrap(),
        "世界"
    );
}

#[test]
fn a_recorded_batch_is_recovered_with_the_same_names() {
    let (a, b) = two_images("perpart-history");
    let out_dir = temp_dir("perpart-history-out");
    let restore_dir = temp_dir("perpart-history-restore");
    let server = MultiServer::start(&[chat_body("text of A"), chat_body("text of B")]);
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 5\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"test-model\"\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let history = temp_dir("perpart-history-dir");
    let arg = cfg.display().to_string();
    let hist = history.display().to_string();
    let envs = [
        ("AIDO_CONFIG", arg.as_str()),
        ("AIDO_HISTORY_DIR", hist.as_str()),
    ];
    let out = run_tty_with(
        &[
            "ocr",
            "--profile",
            "test",
            "--no-stream",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
        &envs,
        cfg.clone(),
    );
    out.assert_code(0);
    // The batch restores as a unit, under its original per-part names.
    let out = run_tty_with(
        &["last", "--out-dir", restore_dir.to_str().unwrap()],
        &envs,
        cfg,
    );
    out.assert_code(0);
    assert_eq!(
        std::fs::read_to_string(restore_dir.join("a.txt")).unwrap(),
        "text of A"
    );
    assert_eq!(
        std::fs::read_to_string(restore_dir.join("b.txt")).unwrap(),
        "text of B"
    );
}

#[test]
fn a_truncated_part_fails_alone_and_the_rest_deliver() {
    let (a, b) = two_images("perpart-trunc");
    let out_dir = temp_dir("perpart-trunc-out");
    let server = SseServer::start(&[
        sse_response(&["写了一半的"], "length"),
        sse_response(&["text of B"], "stop"),
    ]);
    let cfg = batch_cfg(&server.url());
    let out = run_ocr(
        &cfg,
        &[
            "ocr",
            "--profile",
            "test",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    out.assert_code(6);
    assert!(!out_dir.join("a.txt").exists());
    assert_eq!(
        std::fs::read_to_string(out_dir.join("b.txt")).unwrap(),
        "text of B"
    );
    let err = out.stderr();
    assert!(err.contains("part 'a.png' failed"), "{err}");
    // The failure names the status's own reason (finish_reason "length"),
    // not a hardcoded "truncated"; the separate token-limit hint may
    // still say truncated, so match the whole failure line.
    assert!(err.contains("part 'a.png' failed: length"), "{err}");
}

#[test]
fn a_middle_failure_still_runs_every_other_part() {
    let dir = temp_dir("perpart-middle");
    let png = solid_png(2, 2);
    let paths: Vec<std::path::PathBuf> = ["a.png", "b.png", "c.png"]
        .iter()
        .map(|name| {
            let p = dir.join(name);
            std::fs::write(&p, &png).unwrap();
            p
        })
        .collect();
    let out_dir = temp_dir("perpart-middle-out");
    let boom = r#"{"error":{"message":"boom"}}"#;
    let server = MultiServer::start_statuses(&[
        ("200 OK", chat_body("text of A")),
        ("500 Internal Server Error", boom),
        ("200 OK", chat_body("text of C")),
    ]);
    let cfg = batch_cfg(&server.url());
    let strs: Vec<&str> = paths.iter().map(|p| p.to_str().unwrap()).collect();
    let out = run_ocr(
        &cfg,
        &[
            "ocr",
            "--profile",
            "test",
            "--no-stream",
            strs[0],
            strs[1],
            strs[2],
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    out.assert_code(6);
    assert_eq!(
        std::fs::read_to_string(out_dir.join("a.txt")).unwrap(),
        "text of A"
    );
    assert!(!out_dir.join("b.txt").exists());
    assert_eq!(
        std::fs::read_to_string(out_dir.join("c.txt")).unwrap(),
        "text of C"
    );
    // The failed part does not stop the parts after it.
    assert_eq!(server.requests().len(), 3);
}

#[test]
fn a_multi_request_part_failing_mid_slices_drops_whole_part() {
    let dir = temp_dir("perpart-slices");
    let tall = dir.join("a.png");
    std::fs::write(&tall, solid_png(64, 3200)).unwrap();
    let small = dir.join("b.png");
    std::fs::write(&small, solid_png(2, 2)).unwrap();
    let out_dir = temp_dir("perpart-slices-out");
    // Part a needs two slice requests; the second one fails, so `a`
    // delivers nothing even though its first slice succeeded.
    let server = MultiServer::start_statuses(&[
        ("200 OK", chat_body("first slice of a")),
        (
            "500 Internal Server Error",
            r#"{"error":{"message":"boom"}}"#,
        ),
        ("200 OK", chat_body("text of B")),
    ]);
    let cfg = batch_cfg(&server.url());
    let out = run_ocr(
        &cfg,
        &[
            "ocr",
            "--profile",
            "test",
            "--no-stream",
            tall.to_str().unwrap(),
            small.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    out.assert_code(6);
    assert!(!out_dir.join("a.txt").exists());
    assert_eq!(
        std::fs::read_to_string(out_dir.join("b.txt")).unwrap(),
        "text of B"
    );
    assert_eq!(server.requests().len(), 3);
    let err = out.stderr();
    assert!(err.contains("part 'a.png' failed"), "{err}");
}

#[test]
fn streaming_transport_writes_the_same_named_files() {
    let (a, b) = two_images("perpart-stream");
    let out_dir = temp_dir("perpart-stream-out");
    let server = SseServer::start(&[
        sse_response(&["text of ", "A"], "stop"),
        sse_response(&["text of B"], "stop"),
    ]);
    let cfg = batch_cfg(&server.url());
    let out = run_ocr(
        &cfg,
        &[
            "ocr",
            "--profile",
            "test",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    out.assert_code(0);
    assert_eq!(
        std::fs::read_to_string(out_dir.join("a.txt")).unwrap(),
        "text of A"
    );
    assert_eq!(
        std::fs::read_to_string(out_dir.join("b.txt")).unwrap(),
        "text of B"
    );
}

#[test]
fn empty_replies_fail_their_parts_instead_of_writing_gaps() {
    let (a, b) = two_images("perpart-empty");
    let out_dir = temp_dir("perpart-empty-out");
    let server = MultiServer::start(&[chat_body(""), chat_body("")]);
    let cfg = batch_cfg(&server.url());
    let out = run_ocr(
        &cfg,
        &[
            "ocr",
            "--profile",
            "test",
            "--no-stream",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    out.assert_code(4);
    let err = out.stderr();
    assert!(err.contains("no usable text"), "{err}");
    assert!(err.contains("all 2 input part(s) failed"), "{err}");
    assert!(!out_dir.join("a.txt").exists());
    assert!(!out_dir.join("b.txt").exists());
}

#[test]
fn json_report_on_a_successful_batch_lists_every_artifact() {
    let (a, b) = two_images("perpart-json");
    let out_dir = temp_dir("perpart-json-out");
    let server = MultiServer::start(&[chat_body("text of A"), chat_body("text of B")]);
    let cfg = batch_cfg(&server.url());
    let out = run_ocr(
        &cfg,
        &[
            "ocr",
            "--profile",
            "test",
            "--no-stream",
            "--json",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    out.assert_code(0);
    let report: serde_json::Value = serde_json::from_str(&out.stdout()).unwrap();
    assert_eq!(report["artifacts"].as_array().unwrap().len(), 2);
    assert_eq!(report["error"], serde_json::Value::Null);
    assert_eq!(report["failed_parts"].as_array().unwrap().len(), 0);
}

#[test]
fn json_report_on_a_partial_batch_carries_the_failures() {
    let (a, b) = two_images("perpart-jsonp");
    let out_dir = temp_dir("perpart-jsonp-out");
    let server = MultiServer::start_statuses(&[
        (
            "500 Internal Server Error",
            r#"{"error":{"message":"boom"}}"#,
        ),
        ("200 OK", chat_body("text of B")),
    ]);
    let cfg = batch_cfg(&server.url());
    let out = run_ocr(
        &cfg,
        &[
            "ocr",
            "--profile",
            "test",
            "--no-stream",
            "--json",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    out.assert_code(6);
    let report: serde_json::Value = serde_json::from_str(&out.stdout()).unwrap();
    assert_eq!(report["error"]["kind"], "partial");
    let failed = report["failed_parts"].as_array().unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["part"], "a.png");
    // The survivor is right there in the report, deliverable.
    assert_eq!(report["artifacts"].as_array().unwrap().len(), 1);
    assert_eq!(report["artifacts"][0]["id"], "b");
}

#[test]
fn a_batch_cannot_target_the_clipboard() {
    let (a, b) = two_images("perpart-copy");
    let out_dir = temp_dir("perpart-copy-out");
    let cfg = batch_cfg("http://127.0.0.1:1");
    let out = run_ocr(
        &cfg,
        &[
            "ocr",
            "--profile",
            "test",
            "--no-stream",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--copy",
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
    );
    out.assert_code(2);
    assert!(out.stderr().contains("clipboard"), "{}", out.stderr());
}

/// Count data-URL images in a raw request body.
fn count_images(raw: &[u8]) -> usize {
    let mut n = 0;
    let mut pos = 0;
    while let Some(i) = find_sub(&raw[pos..], b"data:image") {
        n += 1;
        pos += i + 1;
    }
    n
}

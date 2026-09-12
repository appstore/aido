//! Artifact provenance end to end: every delivered artifact names the
//! request(s) that produced it — a single reply is `request`, several
//! joined replies (ocr slices, chunk joins) are `merged` with the request
//! list in order, and artifacts restored from history are `restored`.
//! The provenance lives in the `--out-dir` delivery manifest (history
//! manifests store metadata only), so every test reads that manifest.

mod support;

use support::*;

fn run_cfg(url: &str) -> std::path::PathBuf {
    settings_config(&format!(
        "[settings]\nhistory_keep = 0\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"test-model\"\n\
         [providers.srv]\nbase_url = \"{url}\""
    ))
}

fn manifest_of(dir: &std::path::Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap()
}

/// Three paragraphs of exactly 2000 chars each: any two overflow the
/// 4000-char packing target, so the text chunks into exactly three (the
/// same shape tests/chunk.rs uses).
fn three_chunk_text() -> String {
    (0..3)
        .map(|i| format!("第{i}部分。{}", "甲".repeat(1995)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[test]
fn a_single_request_run_names_request_zero() {
    let out_dir = temp_dir("prov-single-out");
    let server = Server::json(chat_body("single reply"));
    let cfg = run_cfg(&server.url());
    let arg = cfg.display().to_string();
    let out = run_tty_with(
        &[
            "summarize",
            "--profile",
            "test",
            "--no-stream",
            "--text",
            "hi",
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
        &[("AIDO_CONFIG", arg.as_str())],
        cfg,
    );
    out.assert_code(0);
    let manifest = manifest_of(&out_dir);
    assert_eq!(manifest["artifacts"][0]["id"], "text");
    assert_eq!(
        manifest["artifacts"][0]["provenance"],
        serde_json::json!({"type": "request", "index": 0})
    );
}

#[test]
fn ocr_slices_merge_under_one_merged_provenance() {
    // A 3200px-tall image splits into two slice requests; the replies
    // merge into one text artifact that names both requests.
    let dir = temp_dir("prov-ocr");
    let tall = dir.join("tall.png");
    std::fs::write(&tall, solid_png(64, 3200)).unwrap();
    let out_dir = temp_dir("prov-ocr-out");
    let server = MultiServer::start(&[chat_body("第一片"), chat_body("第二片")]);
    let cfg = run_cfg(&server.url());
    let arg = cfg.display().to_string();
    let out = run_tty_with(
        &[
            "ocr",
            "--profile",
            "test",
            "--no-stream",
            tall.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
        &[("AIDO_CONFIG", arg.as_str())],
        cfg,
    );
    out.assert_code(0);
    assert_eq!(server.requests().len(), 2, "two slices, two requests");
    let manifest = manifest_of(&out_dir);
    assert_eq!(manifest["artifacts"][0]["id"], "text");
    assert_eq!(
        manifest["artifacts"][0]["provenance"],
        serde_json::json!({"type": "merged", "requests": [0, 1]})
    );
    let text = std::fs::read_to_string(out_dir.join("text.txt")).unwrap();
    assert!(text.contains("第一片") && text.contains("第二片"), "{text}");
}

#[test]
fn chunk_join_names_every_request_it_joined() {
    let file = temp_file("book.txt", three_chunk_text().as_bytes());
    let out_dir = temp_dir("prov-join-out");
    let server = MultiServer::start(&[chat_body("一"), chat_body("二"), chat_body("三")]);
    let cfg = run_cfg(&server.url());
    let arg = cfg.display().to_string();
    let out = run_tty_with(
        &[
            "translate",
            "--profile",
            "test",
            "--no-stream",
            file.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
        &[("AIDO_CONFIG", arg.as_str())],
        cfg,
    );
    out.assert_code(0);
    assert_eq!(server.requests().len(), 3, "one request per chunk");
    let manifest = manifest_of(&out_dir);
    assert_eq!(
        manifest["artifacts"][0]["provenance"],
        serde_json::json!({"type": "merged", "requests": [0, 1, 2]})
    );
    // The provenance describes the whole artifact: every chunk reply is in it.
    assert_eq!(
        std::fs::read_to_string(out_dir.join("text.txt")).unwrap(),
        "一\n\n二\n\n三"
    );
}

#[test]
fn chunk_reduce_names_only_the_reduce_request() {
    // Three map replies are intermediate material; the delivered artifact
    // is the reduce reply alone, so its provenance is that one request.
    let file = temp_file("book.txt", three_chunk_text().as_bytes());
    let out_dir = temp_dir("prov-reduce-out");
    let server = MultiServer::start(&[
        chat_body("第一块的摘要"),
        chat_body("第二块的摘要"),
        chat_body("第三块的摘要"),
        chat_body("整篇的最终摘要"),
    ]);
    let cfg = run_cfg(&server.url());
    let arg = cfg.display().to_string();
    let out = run_tty_with(
        &[
            "summarize",
            "--profile",
            "test",
            "--no-stream",
            file.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
        &[("AIDO_CONFIG", arg.as_str())],
        cfg,
    );
    out.assert_code(0);
    assert_eq!(server.requests().len(), 4, "three map requests, one reduce");
    let manifest = manifest_of(&out_dir);
    assert_eq!(
        manifest["artifacts"][0]["provenance"],
        serde_json::json!({"type": "request", "index": 3})
    );
    assert_eq!(
        std::fs::read_to_string(out_dir.join("text.txt")).unwrap(),
        "整篇的最终摘要"
    );
}

#[test]
fn image_artifacts_name_the_request_that_made_them() {
    // --count 2 is one request asking for two images: both artifacts come
    // from that single request, so both name index 0.
    let body = serde_json::json!({
        "created": 1,
        "data": [
            {"b64_json": b64(&solid_png(2, 2))},
            {"b64_json": b64(&solid_png(2, 2))}
        ]
    })
    .to_string();
    let server = Server::json(&body);
    let out_dir = temp_dir("prov-image-out");
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 0\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"gpt-image-1\"\noperations = [\"image\"]\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let arg = cfg.display().to_string();
    let out = run_tty_with(
        &[
            "image",
            "--profile",
            "test",
            "--text",
            "a dog",
            "--count",
            "2",
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
        &[("AIDO_CONFIG", arg.as_str())],
        cfg,
    );
    out.assert_code(0);
    let manifest = manifest_of(&out_dir);
    let artifacts = manifest["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 2);
    let expected = serde_json::json!({"type": "request", "index": 0});
    assert_eq!(artifacts[0]["id"], "image-1");
    assert_eq!(artifacts[0]["provenance"], expected);
    assert_eq!(artifacts[1]["id"], "image-2");
    assert_eq!(artifacts[1]["provenance"], expected);
}

#[test]
fn a_speech_artifact_names_its_request() {
    let bytes = b"RIFF\x26\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x40\x1f\0\0\x40\x1f\0\0\x01\0\x08\0data\x02\0\0\0\0\0";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\n\r\n{}",
        bytes.len(),
        std::str::from_utf8(bytes).unwrap()
    );
    let server = SseServer::start(&[response]);
    let out_dir = temp_dir("prov-speech-out");
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 0\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"tts-1\"\noperations = [\"speech\"]\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let arg = cfg.display().to_string();
    let out = run_tty_with(
        &[
            "tts",
            "--profile",
            "test",
            "--text",
            "hi",
            "--option",
            "format=wav",
            "--out-dir",
            out_dir.to_str().unwrap(),
        ],
        &[("AIDO_CONFIG", arg.as_str())],
        cfg,
    );
    out.assert_code(0);
    let manifest = manifest_of(&out_dir);
    assert_eq!(manifest["artifacts"][0]["id"], "audio-1");
    assert_eq!(
        manifest["artifacts"][0]["provenance"],
        serde_json::json!({"type": "request", "index": 0})
    );
}

#[test]
fn restored_artifacts_carry_restored_provenance() {
    // `last` re-delivers a recorded run without a new request: its
    // delivery manifest must say "restored", not claim a fresh request.
    let server = Server::json(chat_body("KEPT"));
    let hist = temp_dir("prov-restore-hist");
    let restore = temp_dir("prov-restore-out");
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 5\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let arg = cfg.display().to_string();
    let hist_str = hist.to_str().unwrap().to_string();
    let out = run_tty_with(
        &["summarize", "--profile", "test", "--text", "hi"],
        &[
            ("AIDO_CONFIG", arg.as_str()),
            ("AIDO_HISTORY_DIR", &hist_str),
        ],
        cfg,
    );
    out.assert_code(0);

    let out = run(
        &["last", "--out-dir", restore.to_str().unwrap()],
        b"",
        &[("AIDO_HISTORY_DIR", &hist_str)],
    );
    out.assert_code(0);
    let manifest = manifest_of(&restore);
    assert_eq!(
        manifest["artifacts"][0]["provenance"],
        serde_json::json!({"type": "restored"})
    );
}

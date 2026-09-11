//! Protocol adapters: exact request shapes and response handling for the
//! five routes, against the local fake servers.

mod support;

use std::io::Write as _;

use support::*;

fn chat_cfg(url: &str) -> std::path::PathBuf {
    settings_config(&format!(
        "[settings]\nhistory_keep = 0\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"test-model\"\n\
         [providers.srv]\nbase_url = \"{url}\""
    ))
}

fn responses_body(text: &str) -> String {
    serde_json::json!({"status":"completed","output":[{"type":"reasoning"},{"type":"message","content":[{"type":"output_text","text":text}]}]}).to_string()
}

// --- chat -----------------------------------------------------------------

#[test]
fn default_transport_streams_and_the_reply_survives_a_json_fallback() {
    let server = Server::json(chat_body("plain"));
    let cfg = chat_cfg(&server.url());
    let out = run(
        &["summarize", "--profile", "test"],
        b"hello\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "plain\n");
    assert_eq!(request_json(&server.request())["stream"], true);
}

#[test]
fn no_stream_sends_a_buffered_request() {
    let server = Server::json(chat_body("ok"));
    let cfg = chat_cfg(&server.url());
    let out = run(
        &["summarize", "--profile", "test", "--no-stream"],
        b"hello\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert!(request_json(&server.request()).get("stream").is_none());
}

#[test]
fn task_instruction_and_requirement_share_the_system_channel() {
    let server = Server::json(chat_body("ok"));
    let cfg = chat_cfg(&server.url());
    let out = run(
        &["translate", "--profile", "test", "-p", "保持正式语气"],
        b"hello\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    let req = request_json(&server.request());
    let system = req["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("Translate"), "{system}");
    assert!(system.contains("保持正式语气"), "{system}");
    assert!(system.contains('\n'), "kept as two blocks: {system}");
}

#[test]
fn stream_flag_prints_deltas_with_one_final_newline() {
    let server = SseServer::start(&[sse_response(&["Hello", ", ", "世界"], "stop")]);
    let cfg = chat_cfg(&server.url());
    let out = run(
        &["summarize", "--profile", "test", "--stream"],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "Hello, 世界\n");
    assert_eq!(request_json(&server.requests().remove(0))["stream"], true);
}

#[test]
fn stream_error_payload_fails_the_run_with_service_exit_code() {
    let body = concat!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        "data: {\"error\":{\"message\":\"model overloaded\"}}\n\n",
    );
    let server = SseServer::start(&[body.to_string()]);
    let cfg = chat_cfg(&server.url());
    let out = run(
        &["summarize", "--profile", "test"],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(3);
    assert!(
        out.stderr().contains("model overloaded"),
        "stderr: {}",
        out.stderr()
    );
}

#[test]
fn truncated_reply_is_not_delivered_and_exits_four() {
    let server =
        Server::json(r#"{"choices":[{"message":{"content":"partial"},"finish_reason":"length"}]}"#);
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 5\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let file = temp_file("out.txt", b"original");
    let dir = temp_dir("hist-truncated");
    let out = run_with(
        &[
            "summarize",
            "--profile",
            "test",
            "-o",
            file.to_str().unwrap(),
        ],
        b"hi\n",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
        ],
        cfg.clone(),
    );
    out.assert_code(4);
    assert_eq!(out.stdout(), "", "truncated runs deliver nothing");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
    // the incomplete run is recorded for diagnosis, but not recoverable
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().flatten().count(),
        1,
        "one run dir"
    );
}

// --- mid-run failures ------------------------------------------------------

/// Three paragraphs of exactly 2000 chars each: any two overflow the
/// 4000-char packing target, so the text chunks into exactly three (the
/// same shape as tests/chunk.rs's `three_chunk_text`).
fn three_chunk_text() -> String {
    (0..3)
        .map(|i| format!("第{i}部分。{}", "甲".repeat(1995)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[test]
fn mid_run_failure_keeps_finished_replies_recoverable_and_exits_three() {
    // The third of three chunk requests dies with a 500: the first two
    // replies are real generated content — the run records them instead of
    // discarding them, and the exit code is the failed request's own class
    // (service error, 3), not a blanket generation failure.
    let file = temp_file("book.txt", three_chunk_text().as_bytes());
    let server = MultiServer::start_statuses(&[
        ("200 OK", chat_body("第一块的结果")),
        ("200 OK", chat_body("第二块的结果")),
        (
            "500 Internal Server Error",
            r#"{"error":{"message":"service exploded"}}"#,
        ),
    ]);
    let dir = temp_dir("hist-midrun");
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 5\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let out = run_with(
        &[
            "translate",
            "--profile",
            "test",
            "--no-stream",
            file.to_str().unwrap(),
        ],
        &[],
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
        ],
        cfg.to_str().unwrap(),
    );
    out.assert_code(3);
    assert!(
        out.stdout().is_empty(),
        "an incomplete run delivers nothing"
    );
    let err = out.stderr();
    assert!(err.contains("request 3/3 failed"), "{err}");
    assert!(err.contains("service exploded"), "{err}");
    assert_eq!(
        server.requests().len(),
        3,
        "the run stops at the failed request"
    );

    // one incomplete record holding the two replies that did arrive
    let runs: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(runs.len(), 1, "the partial run is recorded");
    let run = &runs[0];
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(run.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["generation"]["status"], "incomplete");
    assert!(manifest["generation"]["reason"]
        .as_str()
        .unwrap()
        .contains("request 3/3 failed"));
    assert_eq!(manifest["artifacts"][0]["file"], "text.txt");
    assert_eq!(
        std::fs::read_to_string(run.join("text.txt")).unwrap(),
        "第一块的结果\n\n第二块的结果",
        "the joined replies of chunks 1 and 2 stay recoverable"
    );
}

#[test]
fn live_partial_run_warns_where_the_full_record_lives() {
    // The same failure while the replies stream live: stdout keeps what it
    // already printed, and stderr names how much streamed and which record
    // now holds the partial text.
    let file = temp_file("book.txt", three_chunk_text().as_bytes());
    let server = MultiServer::start_statuses(&[
        ("200 OK", chat_body("第一块的结果")),
        ("200 OK", chat_body("第二块的结果")),
        (
            "500 Internal Server Error",
            r#"{"error":{"message":"service exploded"}}"#,
        ),
    ]);
    let dir = temp_dir("hist-midrun-live");
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 5\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let out = run_with(
        &[
            "translate",
            "--profile",
            "test",
            "--stream",
            file.to_str().unwrap(),
        ],
        &[],
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
        ],
        cfg.to_str().unwrap(),
    );
    out.assert_code(3);
    assert!(
        out.stdout().contains("第一块的结果"),
        "streamed text cannot be taken back: {}",
        out.stdout()
    );
    let err = out.stderr();
    assert!(err.contains("已输出前 2/3 个分片的结果"), "{err}");
    let runs: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(runs.len(), 1);
    let id = runs[0].file_name().unwrap().to_str().unwrap();
    assert!(
        err.contains(&format!("aido history show {id}")),
        "warning names the record: {err}"
    );
}

#[test]
fn empty_reply_is_a_generation_failure() {
    let server = Server::json(chat_body(""));
    let cfg = chat_cfg(&server.url());
    let out = run(
        &["summarize", "--profile", "test"],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(4);
    assert!(out.stdout().is_empty());
}

#[test]
fn api_error_exits_three() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let mut stream = accept(
            &listener,
            std::time::Instant::now() + std::time::Duration::from_secs(10),
        );
        stream.set_nonblocking(false).unwrap();
        let raw = read_request(&mut stream);
        write_response(
            &mut stream,
            "401 Unauthorized",
            r#"{"error":{"message":"bad api key"}}"#,
        );
        raw
    });
    let cfg = chat_cfg(&format!("http://127.0.0.1:{port}"));
    let out = run(
        &["summarize", "--profile", "test"],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(3);
    assert!(
        out.stderr().contains("bad api key"),
        "stderr: {}",
        out.stderr()
    );
    let _ = handle.join().unwrap();
}

#[test]
fn connection_failure_exits_three() {
    let cfg = chat_cfg("http://127.0.0.1:1");
    let out = run(
        &["summarize", "--profile", "test"],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(3);
}

#[test]
fn missing_required_artifact_kind_is_a_generation_failure() {
    // tts route returns JSON instead of audio bytes
    let server = Server::start("200 OK", r#"{"error":"no voice"}"#);
    let cfg = chat_cfg(&server.url());
    let out = run_tty_with(
        &["tts", "--profile", "test", "--text", "hi"],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        settings_config(&format!(
            "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\noperations = [\"speech\"]\n\
             [providers.srv]\nbase_url = \"{}\"",
            server.url()
        )),
    );
    assert_eq!(out.code(), 3);
    assert!(out.stdout().is_empty());
}

// --- responses ------------------------------------------------------------

#[test]
fn responses_route_instruction_and_parts() {
    let server = Server::json(&responses_body("answer"));
    let _ = chat_cfg(&server.url());
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 0\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{}/proxy/?v=1\"\n\
         [providers.srv.routes]\ngenerate = \"openai-responses\"",
        server.url()
    ));
    let out = run_with(
        &[
            "summarize",
            "--profile",
            "test",
            "--no-stream",
            "-p",
            "extra care",
        ],
        b"question",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.to_str().unwrap(),
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "answer\n");
    let raw = server.request();
    assert!(String::from_utf8_lossy(&raw).starts_with("POST /proxy/responses?v=1 "));
    let body = request_json(&raw);
    assert!(body["instructions"].as_str().unwrap().contains("Summarize"));
    assert!(body["instructions"]
        .as_str()
        .unwrap()
        .contains("extra care"));
    assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
    assert_eq!(body["store"], false);
}

// --- speech ---------------------------------------------------------------

#[test]
fn speech_send_material_as_input_and_instructions_separately() {
    let bytes = b"RIFF\x26\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x40\x1f\0\0\x40\x1f\0\0\x01\0\x08\0data\x02\0\0\0\0\0";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\n\r\n{}",
        bytes.len(),
        std::str::from_utf8(bytes).unwrap()
    );
    let server = SseServer::start(&[response]);
    let out_file = temp_dir("tts");
    let file = out_file.join("hello.wav");
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"tts-1\"\noperations = [\"speech\"]\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let out = run_tty_with(
        &[
            "tts",
            "--profile",
            "test",
            "--text",
            "你好",
            "-p",
            "用高兴的语气",
            "--voice",
            "alloy",
            "--option",
            "format=wav",
            "-o",
            file.to_str().unwrap(),
        ],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    assert_eq!(std::fs::read(&file).unwrap(), bytes.to_vec());
    let req = request_json(&server.requests().remove(0));
    assert_eq!(req["input"], "你好");
    assert_eq!(req["instructions"], "用高兴的语气");
    assert_eq!(req["voice"], "alloy");
    assert_eq!(req["response_format"], "wav");
}

// --- transcription --------------------------------------------------------

#[test]
fn transcription_uploads_one_audio_as_multipart() {
    let server = Server::json(r#"{"text":"meeting notes"}"#);
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"whisper-1\"\noperations = [\"transcribe\"]\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let wav = temp_file("meeting.m4a", b"RIFF\x04\x00\x00\x00WAVEftyp");
    let out = run_tty_with(
        &[
            "transcribe",
            "--profile",
            "test",
            wav.to_str().unwrap(),
            "--option",
            "language=zh",
        ],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "meeting notes\n");
    let raw = String::from_utf8_lossy(&server.request()).into_owned();
    assert!(raw.starts_with("POST /v1/audio/transcriptions "));
    assert!(raw.contains("multipart/form-data"));
    assert!(raw.contains("whisper-1"));
    assert!(raw.contains("zh"));
}

// --- images ---------------------------------------------------------------

#[test]
fn image_generation_sends_prompt_and_count_and_writes_all() {
    let png = solid_png(2, 2);
    // raw base64 via a tiny encoder
    let encoded = b64(&png);
    let body = serde_json::json!({"data":[{"b64_json":encoded},{"b64_json":encoded}]}).to_string();
    let server = Server::json(&body);
    let dir = temp_dir("images");
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 0\n\
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
            "一只柴犬",
            "--count",
            "2",
            "--out-dir",
            dir.to_str().unwrap(),
        ],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    assert_eq!(std::fs::read(dir.join("image-1.png")).unwrap(), png);
    assert_eq!(std::fs::read(dir.join("image-2.png")).unwrap(), png);
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["artifacts"].as_array().unwrap().len(), 2);
    let req = request_json(&server.request());
    assert!(req["prompt"].as_str().unwrap().contains("柴犬"));
    assert_eq!(req["n"], 2);
}

#[test]
fn generated_image_download_does_not_forward_api_credentials() {
    let png = solid_png(2, 2);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let image_port = listener.local_addr().unwrap().port();
    let bytes = png.clone();
    let image_server = std::thread::spawn(move || {
        let mut socket = accept(
            &listener,
            std::time::Instant::now() + std::time::Duration::from_secs(10),
        );
        socket.set_nonblocking(false).unwrap();
        let request = read_request(&mut socket);
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\n\r\n",
            bytes.len()
        );
        socket.write_all(header.as_bytes()).unwrap();
        socket.write_all(&bytes).unwrap();
        request
    });
    let body = serde_json::json!({"data":[{"url":format!("http://127.0.0.1:{image_port}/asset.png?signature=example")}]}).to_string();
    let server = Server::json(&body);
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 0\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\noperations = [\"image\"]\n\
         [providers.srv]\nbase_url = \"{}\"\napi_key_env = \"MY_KEY\"",
        server.url()
    ));
    let out = run_tty_with(
        &["image", "--profile", "test", "--text", "dog"],
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("MY_KEY", "test-secret"),
        ],
        cfg.clone(),
    );
    out.assert_code(0);
    assert_eq!(out.output.stdout, png);
    assert!(String::from_utf8_lossy(&server.request())
        .to_lowercase()
        .contains("authorization: bearer test-secret"));
    let image_request = String::from_utf8_lossy(&image_server.join().unwrap()).to_lowercase();
    assert!(!image_request.contains("authorization"));
    assert!(!image_request.contains("test-secret"));
}

// --- OCR slices -----------------------------------------------------------

#[test]
fn tall_image_is_sliced_and_replies_merge_on_one_boundary() {
    let png = solid_png(64, 3200);
    let file = temp_file("long.png", &png);
    let server = MultiServer::start(&[
        r#"{"choices":[{"message":{"content":"first half"}}]}"#,
        r#"{"choices":[{"message":{"content":"second half"}}]}"#,
    ]);
    let cfg = chat_cfg(&server.url());
    let out = run_tty_with(
        &["ocr", "--profile", "test", file.to_str().unwrap()],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "first half\nsecond half\n");
    let err = out.stderr();
    assert!(err.contains("split into 2 slices"), "{err}");
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    // the second request explains itself as a slice
    let second = request_json(&requests[1]);
    let second_text = second["messages"][1]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(second_text.contains("slice"), "{second_text}");
    // the first request carries the fixed instruction
    let first = request_json(&requests[0]);
    assert!(
        first["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("Extract"),
        "instruction must reach every slice"
    );
}

#[test]
fn no_split_sends_the_whole_image() {
    let png = solid_png(64, 3200);
    let file = temp_file("long.png", &png);
    let server = Server::json(chat_body("whole"));
    let cfg = chat_cfg(&server.url());
    let out = run_tty_with(
        &[
            "ocr",
            "--profile",
            "test",
            "--no-split",
            file.to_str().unwrap(),
        ],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    let req = request_json(&server.request());
    let content = &req["messages"][1]["content"];
    assert_eq!(content.as_array().unwrap().len(), 2);
}

#[test]
fn decompression_bomb_jpeg_is_refused_as_a_usage_error() {
    // A real 2×2 JPEG whose SOF0 header declares 20 000×20 000 (400 MP):
    // the whole file is a few hundred bytes, but the old path fully
    // decoded it — the RGBA bitmap alone would need ~1.6 GB. The
    // header-only guard must refuse it at plan time, before any pixel
    // work, as a usage error.
    let jpg = huge_jpeg(20_000, 20_000);
    assert!(
        jpg.len() < 4096,
        "the bomb must stay tiny: {} bytes",
        jpg.len()
    );
    let file = temp_file("huge.jpg", &jpg);
    let out = run(&["ocr", "--dry-run", file.to_str().unwrap()], b"", &[]);
    out.assert_code(2);
    let err = out.stderr();
    assert!(err.contains("refusing to decode"), "{err}");
    assert!(err.contains("20000"), "{err}");
}

// --- flag semantics -------------------------------------------------------

#[test]
fn no_stream_is_a_buffering_request_even_on_non_streaming_adapters() {
    // `--no-stream` forces buffering; on an adapter that never streams it
    // must be a no-op, not "does not support streaming" (it is the very
    // fix the `--stream` error recommends).
    let out = run_tty(&["tts", "--text", "hi", "--no-stream", "--dry-run"], &[]);
    out.assert_code(0);
    assert!(out.stdout().contains("buffered"), "{}", out.stdout());
    // `--stream` on a non-streaming adapter is still refused, with the
    // (now working) advice.
    let out = run_tty(&["tts", "--text", "hi", "--stream", "--dry-run"], &[]);
    out.assert_code(2);
    assert!(out.stderr().contains("--no-stream"), "{}", out.stderr());
}

// --- responses ------------------------------------------------------------

#[test]
fn responses_instruction_only_run_sends_the_instruction_once() {
    let server = Server::json(&responses_body("ok"));
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 0\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{}\"\n\
         [providers.srv.routes]\ngenerate = \"openai-responses\"",
        server.url()
    ));
    let out = run_tty_with(
        &["ask", "-p", "summarize this", "--profile", "test"],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    let body = request_json(&server.request());
    // instruction-only: the user turn carries it; the `instructions`
    // field must stay unset or the model would see the text twice.
    assert!(
        body.get("instructions").is_none(),
        "instruction duplicated: {body}"
    );
    assert_eq!(body["input"][0]["content"][0]["text"], "summarize this");
}

// --- speech ---------------------------------------------------------------

#[test]
fn speech_input_is_plain_text_without_file_labels() {
    let bytes = b"RIFF\x26\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x40\x1f\0\0\x40\x1f\0\0\x01\0\x08\0data\x02\0\0\0\0\0";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\n\r\n{}",
        bytes.len(),
        std::str::from_utf8(bytes).unwrap()
    );
    let server = SseServer::start(&[response]);
    let out_file = temp_dir("tts-labels");
    let file = out_file.join("out.wav");
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"tts-1\"\noperations = [\"speech\"]\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let out = run_tty_with(
        &[
            "tts",
            "--profile",
            "test",
            "--text",
            "one",
            "--text",
            "two",
            "--option",
            "format=wav",
            "-o",
            file.to_str().unwrap(),
        ],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    // The spoken input must be the plain text; `--- name ---` labels
    // would be read aloud.
    let req = request_json(&server.requests().remove(0));
    assert_eq!(req["input"], "one\n\ntwo");
}

/// Minimal base64 encoder (standard alphabet, padding).
fn b64(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

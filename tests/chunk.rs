//! The chunk strategies end to end: long text becomes per-chunk requests
//! and later chunks carry the previous chunk's tail as context. translate
//! declares chunk-join (the replies join with one paragraph break);
//! summarize declares chunk-reduce (one more request consolidates the
//! collected replies, and only its reply is the result).

mod support;

use support::*;

/// ~4600 chars of CJK paragraph text: past the 4000-char chunk target. A
/// byte-based splitter would cut this into four; a char-based one into two.
fn long_text() -> String {
    (0..200)
        .map(|i| format!("第{i}段，这是用来测试长文分块的内容句子。"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Three paragraphs of exactly 2000 chars each: any two overflow the
/// 4000-char packing target, so the text chunks into exactly three.
fn three_chunk_text() -> String {
    (0..3)
        .map(|i| format!("第{i}部分。{}", "甲".repeat(1995)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn chunk_cfg(url: &str) -> std::path::PathBuf {
    settings_config(&format!(
        "[settings]\nhistory_keep = 0\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"test-model\"\n\
         [providers.srv]\nbase_url = \"{url}\""
    ))
}

/// The same config but with recording on, for the failure tests that
/// assert what history kept.
fn chunk_hist_cfg(url: &str) -> std::path::PathBuf {
    settings_config(&format!(
        "[settings]\nhistory_keep = 5\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"test-model\"\n\
         [providers.srv]\nbase_url = \"{url}\""
    ))
}

/// The recorded run's parsed manifest (one run dir, expected).
fn recorded_manifest(dir: &std::path::Path) -> (std::path::PathBuf, serde_json::Value) {
    let mut runs: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    runs.sort();
    assert_eq!(runs.len(), 1, "exactly one run is recorded");
    let run = runs.pop().unwrap();
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(run.join("manifest.json")).unwrap()).unwrap();
    (run, manifest)
}

#[test]
fn long_text_splits_into_two_requests_and_replies_join() {
    let file = temp_file("book.txt", long_text().as_bytes());
    let server = MultiServer::start(&[chat_body("第一块的结果"), chat_body("第二块的结果")]);
    let cfg = chunk_cfg(&server.url());
    let out = run_tty_with(
        &[
            "translate",
            "--profile",
            "test",
            "--no-stream",
            file.to_str().unwrap(),
        ],
        &[],
        cfg,
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "第一块的结果\n\n第二块的结果\n");
    let err = out.stderr();
    assert!(err.contains("split into 2 chunks"), "{err}");

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        2,
        "chunk-join never adds a request beyond the chunks themselves"
    );
    // The first request carries only its chunk, unlabeled (single part).
    let first_json = request_json(&requests[0]);
    let first = first_json["messages"][1]["content"].as_str().unwrap();
    assert!(first.starts_with("第0段"), "{first}");
    assert!(!first.contains("Context from the end"), "{first}");
    // The second request explains itself, carries the previous chunk's
    // tail as context-only, and labels its chunk.
    let second_json = request_json(&requests[1]);
    let second = second_json["messages"][1]["content"].as_str().unwrap();
    assert!(
        second.contains("one chunk of a longer document"),
        "{second}"
    );
    assert!(
        second.contains("Context from the end of the previous chunk"),
        "{second}"
    );
    assert!(second.contains("book.txt [chunk 2/2] ---"), "{second}");
}

#[test]
fn streaming_chunks_join_buffered_ones_byte_for_byte() {
    let file = temp_file("book.txt", long_text().as_bytes());
    let server = SseServer::start(&[
        sse_response(&["第一块", "的流式结果"], "stop"),
        sse_response(&["第二块"], "stop"),
    ]);
    let cfg = chunk_cfg(&server.url());
    let out = run_tty_with(
        &["translate", "--profile", "test", file.to_str().unwrap()],
        &[],
        cfg,
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "第一块的流式结果\n\n第二块\n");
}

#[test]
fn chunk_reduce_collects_the_replies_into_one_consolidation_request() {
    let file = temp_file("book.txt", three_chunk_text().as_bytes());
    let server = MultiServer::start(&[
        chat_body("第一块的摘要"),
        chat_body("第二块的摘要"),
        chat_body("第三块的摘要"),
        chat_body("整篇的最终摘要"),
    ]);
    let cfg = chunk_cfg(&server.url());
    let out = run_tty_with(
        &[
            "summarize",
            "--profile",
            "test",
            "--no-stream",
            file.to_str().unwrap(),
        ],
        &[],
        cfg,
    );
    out.assert_code(0);
    let requests = server.requests();
    assert_eq!(requests.len(), 4, "three map requests plus one reduce");
    let err = out.stderr();
    assert!(err.contains("split into 3 chunks"), "{err}");

    // The reduce request's material is the collected map replies, in
    // order, with section markers — and it still carries the task's
    // original instruction.
    let reduce_json = request_json(&requests[3]);
    let system = reduce_json["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("Summarize"), "{system}");
    let material = reduce_json["messages"][1]["content"].as_str().unwrap();
    for piece in ["第一块的摘要", "第二块的摘要", "第三块的摘要"] {
        assert!(material.contains(piece), "missing {piece} in: {material}");
    }
    for marker in ["--- result 1 of 3 ---", "--- result 3 of 3 ---"] {
        assert!(material.contains(marker), "missing {marker} in: {material}");
    }
    assert!(material.contains("one longer document"), "{material}");

    // The map replies are intermediate: stdout carries only the reduce
    // reply, byte for byte.
    assert_eq!(out.stdout(), "整篇的最终摘要\n");
}

#[test]
fn streaming_reduce_prints_only_the_final_reply() {
    let file = temp_file("book.txt", three_chunk_text().as_bytes());
    let server = SseServer::start(&[
        sse_response(&["第一块的", "摘要"], "stop"),
        sse_response(&["第二块的摘要"], "stop"),
        sse_response(&["第三块的摘要"], "stop"),
        sse_response(&["整篇的", "最终摘要"], "stop"),
    ]);
    let cfg = chunk_cfg(&server.url());
    let out = run_tty_with(
        &["summarize", "--profile", "test", file.to_str().unwrap()],
        &[],
        cfg,
    );
    out.assert_code(0);
    assert_eq!(server.requests().len(), 4);
    // Map deltas streamed in but are intermediate material; even over a
    // streaming transport, stdout carries only the reduce reply.
    assert_eq!(out.stdout(), "整篇的最终摘要\n");
}

#[test]
fn single_chunk_summarize_makes_one_request_without_reduce() {
    let file = temp_file("short.txt", "只是一段短文本，无需分块。".as_bytes());
    let server = MultiServer::start(&[chat_body("短文本的摘要")]);
    let cfg = chunk_cfg(&server.url());
    let out = run_tty_with(
        &[
            "summarize",
            "--profile",
            "test",
            "--no-stream",
            file.to_str().unwrap(),
        ],
        &[],
        cfg,
    );
    out.assert_code(0);
    // One chunk is the whole document: no map/reduce split, one request.
    assert_eq!(server.requests().len(), 1);
    assert_eq!(out.stdout(), "短文本的摘要\n");
}

#[test]
fn no_split_sends_the_text_whole() {
    let file = temp_file("book.txt", long_text().as_bytes());
    let server = MultiServer::start(&[chat_body("整篇结果")]);
    let cfg = chunk_cfg(&server.url());
    let out = run_tty_with(
        &[
            "summarize",
            "--profile",
            "test",
            "--no-stream",
            "--no-split",
            file.to_str().unwrap(),
        ],
        &[],
        cfg,
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "整篇结果\n");
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn dry_run_shows_the_chunk_plan_without_requesting() {
    let file = temp_file("book.txt", long_text().as_bytes());
    let out = run_tty_with(
        &[
            "summarize",
            "--profile",
            "test",
            "--dry-run",
            file.to_str().unwrap(),
        ],
        &[],
        chunk_cfg("http://127.0.0.1:1"),
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("chunk-reduce — chunk 1/2 of"), "{stdout}");
    assert!(stdout.contains("; chunk 2/2 of"), "{stdout}");
    // The reduce step appears in the planned sequence.
    assert!(stdout.contains("; consolidate 2 chunks"), "{stdout}");
}

#[test]
fn glossary_travels_with_every_chunk_in_command_line_order() {
    // The "one document as context to process another" case: the glossary
    // is unsliced, so it must ride with every chunk's request — in the
    // order the user listed it, ahead of the long document.
    let glossary = temp_file("术语表.md", "术语：aido=助手；chunk=分块。".as_bytes());
    let book = temp_file("长文.md", three_chunk_text().as_bytes());
    let server = MultiServer::start(&[
        chat_body("第一块的摘要"),
        chat_body("第二块的摘要"),
        chat_body("第三块的摘要"),
        chat_body("整篇的最终摘要"),
    ]);
    let cfg = chunk_cfg(&server.url());
    let out = run_tty_with(
        &[
            "summarize",
            "--profile",
            "test",
            "--no-stream",
            glossary.to_str().unwrap(),
            book.to_str().unwrap(),
        ],
        &[],
        cfg,
    );
    out.assert_code(0);
    let requests = server.requests();
    assert_eq!(requests.len(), 4, "three map requests plus one reduce");
    for (i, raw) in requests.iter().take(3).enumerate() {
        let material = request_json(raw)["messages"][1]["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            material.contains("aido=助手"),
            "chunk {} never sees the glossary: {material}",
            i + 1
        );
        // Command-line order in every request: glossary first, chunk after.
        let glossary_at = material.find("aido=助手").unwrap();
        let chunk_at = material
            .find(&format!("长文.md [chunk {}/3]", i + 1))
            .unwrap();
        assert!(
            glossary_at < chunk_at,
            "chunk {}: material order wrong: {material}",
            i + 1
        );
    }
    // The untouched material rides with the map requests only; the reduce
    // step's material is the collected map replies.
    let reduce = request_json(&requests[3])["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!reduce.contains("aido=助手"), "{reduce}");
}

#[test]
fn material_order_follows_the_command_line_when_the_document_is_first() {
    // Reversed command line: every request carries the chunk first and the
    // glossary behind it, matching how the user listed the inputs.
    let book = temp_file("长文.md", three_chunk_text().as_bytes());
    let glossary = temp_file("术语表.md", "术语：aido=助手。".as_bytes());
    let server = MultiServer::start(&[
        chat_body("第一块的摘要"),
        chat_body("第二块的摘要"),
        chat_body("第三块的摘要"),
        chat_body("整篇的最终摘要"),
    ]);
    let cfg = chunk_cfg(&server.url());
    let out = run_tty_with(
        &[
            "summarize",
            "--profile",
            "test",
            "--no-stream",
            book.to_str().unwrap(),
            glossary.to_str().unwrap(),
        ],
        &[],
        cfg,
    );
    out.assert_code(0);
    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    for (i, raw) in requests.iter().take(3).enumerate() {
        let material = request_json(raw)["messages"][1]["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            material.contains("aido=助手"),
            "chunk {}: {material}",
            i + 1
        );
        let glossary_at = material.find("aido=助手").unwrap();
        let chunk_at = material
            .find(&format!("长文.md [chunk {}/3]", i + 1))
            .unwrap();
        assert!(
            chunk_at < glossary_at,
            "chunk {}: material order wrong: {material}",
            i + 1
        );
    }
}

#[test]
fn oversized_glossary_falls_back_to_the_first_request_only() {
    // A context file over half the chunk budget (>2000 chars) but under
    // the chunking target (<4000) stays unsliced, yet repeating it in
    // every request would cost more tokens than it is worth: it rides
    // with the first request only, and stderr says why.
    let glossary = format!("术语表：aido=助手。{}", "词".repeat(2400));
    let book = temp_file("长文.md", three_chunk_text().as_bytes());
    let server = MultiServer::start(&[
        chat_body("第一块的译文"),
        chat_body("第二块的译文"),
        chat_body("第三块的译文"),
    ]);
    let cfg = chunk_cfg(&server.url());
    let out = run_tty_with(
        &[
            "translate",
            "--profile",
            "test",
            "--no-stream",
            "--text",
            &glossary,
            book.to_str().unwrap(),
        ],
        &[],
        cfg,
    );
    out.assert_code(0);
    let err = out.stderr();
    assert!(err.contains("travels with the first request only"), "{err}");
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    let first = request_json(&requests[0])["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(first.contains("aido=助手"), "{first}");
    for (i, raw) in requests.iter().enumerate().skip(1) {
        let material = request_json(raw)["messages"][1]["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            !material.contains("aido=助手"),
            "chunk {} must not repeat the oversized material: {material}",
            i + 1
        );
    }
}

// --- a failing reduce run keeps its map replies (R07) ----------------------

#[test]
fn reduce_failure_keeps_every_map_reply_in_history() {
    // Three chunks, so three paid map replies — and the reduce request
    // dies with a 500. The run fails (exit 3) and delivers nothing, but
    // the record keeps all three map replies as intermediate artifacts
    // instead of dropping content the user already paid for.
    let file = temp_file("book.txt", three_chunk_text().as_bytes());
    let server = MultiServer::start_statuses(&[
        ("200 OK", chat_body("第一块的摘要")),
        ("200 OK", chat_body("第二块的摘要")),
        ("200 OK", chat_body("第三块的摘要")),
        (
            "500 Internal Server Error",
            r#"{"error":{"message":"service exploded"}}"#,
        ),
    ]);
    let dir = temp_dir("chunk-reduce-fail");
    let cfg = chunk_hist_cfg(&server.url());
    let out = run_tty_with(
        &[
            "summarize",
            "--profile",
            "test",
            "--no-stream",
            "--out-dir",
            dir.join("never").to_str().unwrap(),
            file.to_str().unwrap(),
        ],
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
        ],
        cfg.clone(),
    );
    out.assert_code(3);
    // Map replies never stream live in a reduce run, and the failed
    // reduce request streamed nothing: stdout carries no text at all.
    assert!(out.stdout().is_empty(), "{}", out.stdout());
    let err = out.stderr();
    assert!(err.contains("request 4/4 failed"), "{err}");
    assert!(err.contains("service exploded"), "{err}");
    assert!(err.contains("kept 3 intermediate map replies"), "{err}");
    assert!(err.contains("aido history show"), "{err}");

    // The record holds one artifact per non-empty map reply, and each
    // file carries exactly that reply's text.
    let (run_dir, manifest) = recorded_manifest(&dir);
    assert_eq!(manifest["generation"]["status"], "incomplete");
    assert!(
        manifest["generation"]["reason"]
            .as_str()
            .unwrap()
            .contains("request 4/4 failed"),
        "{}",
        manifest
    );
    let ids: Vec<&str> = manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["text-chunk-1", "text-chunk-2", "text-chunk-3"]);
    let files: Vec<&str> = manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["file"].as_str().unwrap())
        .collect();
    assert_eq!(
        files,
        ["text-chunk-1.txt", "text-chunk-2.txt", "text-chunk-3.txt"]
    );
    for (i, text) in ["第一块的摘要", "第二块的摘要", "第三块的摘要"]
        .iter()
        .enumerate()
    {
        assert_eq!(
            std::fs::read_to_string(run_dir.join(files[i])).unwrap(),
            *text,
            "map reply {} is kept verbatim",
            i + 1
        );
    }
    // Nothing was delivered: the out-dir never came to be.
    assert!(!dir.join("never").exists());
    assert_eq!(server.requests().len(), 4, "the run stops at the reduce");

    // `history show` still refuses to restore the incomplete run, but
    // its message now says the intermediates exist in the directory.
    let shown = run(
        &["history", "show", "1"],
        b"",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
        ],
    );
    shown.assert_code(0);
    let stdout = shown.stdout();
    assert!(stdout.contains("artifacts are not delivered"), "{stdout}");
    assert!(
        stdout.contains("kept 3 intermediate chunk result(s)"),
        "{stdout}"
    );
}

#[test]
fn map_failure_keeps_the_replies_that_arrived() {
    // The second of three map requests fails: the run stops with only
    // chunk 1's reply in hand, and the record keeps exactly that one
    // intermediate artifact.
    let file = temp_file("book.txt", three_chunk_text().as_bytes());
    let server = MultiServer::start_statuses(&[
        ("200 OK", chat_body("第一块的摘要")),
        (
            "500 Internal Server Error",
            r#"{"error":{"message":"service exploded"}}"#,
        ),
    ]);
    let dir = temp_dir("chunk-map-fail");
    let cfg = chunk_hist_cfg(&server.url());
    let out = run_tty_with(
        &[
            "summarize",
            "--profile",
            "test",
            "--no-stream",
            file.to_str().unwrap(),
        ],
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
        ],
        cfg.clone(),
    );
    out.assert_code(3);
    assert!(out.stdout().is_empty(), "{}", out.stdout());
    let err = out.stderr();
    assert!(err.contains("request 2/4 failed"), "{err}");
    assert!(err.contains("kept 1 intermediate map reply"), "{err}");

    let (run, manifest) = recorded_manifest(&dir);
    assert_eq!(manifest["generation"]["status"], "incomplete");
    let artifacts = manifest["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 1, "{}", manifest);
    assert_eq!(artifacts[0]["id"], "text-chunk-1");
    assert_eq!(artifacts[0]["file"], "text-chunk-1.txt");
    assert_eq!(
        std::fs::read_to_string(run.join("text-chunk-1.txt")).unwrap(),
        "第一块的摘要"
    );
    // The failed map step's section is empty, so no artifact exists for
    // chunk 2 — and chunk 3 was never requested.
    assert!(!run.join("text-chunk-2.txt").exists());
    assert!(!run.join("text-chunk-3.txt").exists());
    assert_eq!(
        server.requests().len(),
        2,
        "the run stops at the failed map request"
    );
}

// --- --total-timeout caps the whole run, not each request (F41) -----------

#[test]
fn total_timeout_stops_the_second_chunk_and_keeps_the_first_reply() {
    // Two chunks; the second reply is delayed far past the 3s whole-run
    // budget, which the 120s per-request timeout would happily wait out.
    // The budget kills the second request, the run exits 3, and the
    // record keeps the first chunk's paid reply.
    let file = temp_file("book.txt", long_text().as_bytes());
    let server = DelayServer::start(
        &[chat_body("第一块的结果"), chat_body("第二块的结果")],
        std::time::Duration::from_secs(10),
    );
    let dir = temp_dir("chunk-budget");
    let cfg = chunk_hist_cfg(&server.url());
    let out = run_tty_with(
        &[
            "translate",
            "--profile",
            "test",
            "--no-stream",
            "--total-timeout",
            "3",
            file.to_str().unwrap(),
        ],
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
        ],
        cfg.clone(),
    );
    out.assert_code(3);
    let err = out.stderr();
    assert!(err.contains("request 2/2 failed"), "{err}");
    assert!(
        err.contains("the run exceeded its total time budget (3s)"),
        "{err}"
    );

    // The run is recorded incomplete, and its kept artifact file holds
    // exactly the reply that did arrive.
    let (run, manifest) = recorded_manifest(&dir);
    assert_eq!(manifest["generation"]["status"], "incomplete");
    assert!(
        manifest["generation"]["reason"]
            .as_str()
            .unwrap()
            .contains("total time budget"),
        "{manifest}"
    );
    assert_eq!(
        std::fs::read_to_string(run.join("text.txt")).unwrap(),
        "第一块的结果",
        "the first chunk's reply is kept verbatim"
    );
}

#[test]
fn without_a_total_timeout_a_delayed_second_reply_still_completes() {
    // The same two-request shape with no --total-timeout: the budget wrap
    // must be a no-op, so a 1s delay — far under the per-request timeout
    // — still delivers both chunks normally.
    let file = temp_file("book.txt", long_text().as_bytes());
    let server = DelayServer::start(
        &[chat_body("第一块的结果"), chat_body("第二块的结果")],
        std::time::Duration::from_secs(1),
    );
    let out = run_tty_with(
        &[
            "translate",
            "--profile",
            "test",
            "--no-stream",
            file.to_str().unwrap(),
        ],
        &[],
        chunk_cfg(&server.url()),
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "第一块的结果\n\n第二块的结果\n");
    assert_eq!(server.requests().len(), 2);
}

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

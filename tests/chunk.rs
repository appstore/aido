//! The chunk-map-reduce strategy end to end: long text becomes per-chunk
//! requests, later chunks carry the previous chunk's tail as context, and
//! the replies join with one paragraph break.

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
    assert_eq!(out.stdout(), "第一块的结果\n\n第二块的结果\n");
    let err = out.stderr();
    assert!(err.contains("split into 2 chunks"), "{err}");

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        2,
        "chars are the unit of chunking, so two chunks — not four"
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
        &["summarize", "--profile", "test", file.to_str().unwrap()],
        &[],
        cfg,
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "第一块的流式结果\n\n第二块\n");
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
    assert!(
        stdout.contains("chunk-map-reduce — chunk 1/2 of"),
        "{stdout}"
    );
    assert!(stdout.contains("; chunk 2/2 of"), "{stdout}");
}

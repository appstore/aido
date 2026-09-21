use super::*;

#[test]
fn refusals_and_multiple_messages_preserve_text() {
    let body = json!({"status":"completed","output":[{"type":"reasoning"},{"type":"message","content":[{"type":"output_text","text":"one"},{"type":"refusal","refusal":"two"}]},{"type":"message","content":[{"type":"output_text","text":"three"}]}]});
    let result = parse(&body.to_string()).unwrap();
    assert_eq!(result.text, "onetwothree");
    assert_eq!(result.warnings.len(), 1);
}
#[test]
fn failed_and_unknown_statuses_are_errors() {
    for status in ["failed", "cancelled", "in_progress", "unknown"] {
        assert!(parse(&json!({"status":status,"output":[]}).to_string()).is_err());
    }
}
#[test]
fn terminal_snapshot_is_not_duplicated() {
    let mut stream = Stream::default();
    let mut text = String::new();
    let mut emit = |delta: &str| text.push_str(delta);
    stream
        .feed(
            r#"{"type":"response.output_text.delta","delta":"你"}"#,
            &mut emit,
        )
        .unwrap();
    let terminal = json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"你好"}]}]}});
    assert!(stream.feed(&terminal.to_string(), &mut emit).unwrap());
    let result = stream.finish().unwrap();
    assert_eq!(result.text, "你好");
    assert_eq!(text, "你好");
    // The snapshot extends the stream, so no disagreement is recorded.
    assert!(result.warnings.is_empty());
}

#[test]
fn terminal_snapshot_equal_to_the_stream_emits_nothing() {
    let mut stream = Stream::default();
    let mut deltas = Vec::new();
    let mut emit = |delta: &str| deltas.push(delta.to_owned());
    stream
        .feed(
            r#"{"type":"response.output_text.delta","delta":"你好"}"#,
            &mut emit,
        )
        .unwrap();
    let terminal = json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"你好"}]}]}});
    assert!(stream.feed(&terminal.to_string(), &mut emit).unwrap());
    let result = stream.finish().unwrap();
    assert_eq!(result.text, "你好");
    assert_eq!(deltas, ["你好"]);
    assert!(result.warnings.is_empty());
}

#[test]
fn rewritten_shorter_or_empty_terminal_snapshots_are_refused() {
    // The artifact text comes from the emitted deltas, not the terminal
    // snapshot: accepting a disagreeing snapshot would silently keep
    // the streamed bytes while claiming the final text won. Refuse.
    for (streamed, snapshot) in [("你好", "再见"), ("你好", "你"), ("你好", "")] {
        let mut stream = Stream::default();
        let mut deltas = Vec::new();
        let mut emit = |delta: &str| deltas.push(delta.to_owned());
        stream
            .feed(
                &json!({"type":"response.output_text.delta","delta":streamed}).to_string(),
                &mut emit,
            )
            .unwrap();
        let terminal = json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":snapshot}]}]}});
        let err = stream.feed(&terminal.to_string(), &mut emit).unwrap_err();
        assert!(
            err.to_string()
                .contains("final text disagrees with streamed output"),
            "{err}"
        );
        // Nothing extra was emitted, and nothing can be un-emitted.
        assert_eq!(deltas, [streamed]);
    }
}
#[test]
fn eof_and_stream_errors_are_not_success() {
    let mut stream = Stream::default();
    stream
        .feed(
            r#"{"type":"response.output_text.delta","delta":"partial"}"#,
            &mut |_| {},
        )
        .unwrap();
    assert!(stream.finish().is_err());
    for event in ["response.failed", "error"] {
        assert!(Stream::default()
            .feed(&json!({"type":event}).to_string(), &mut |_| {})
            .is_err());
    }
}

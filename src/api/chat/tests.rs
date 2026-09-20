use super::*;
use crate::domain::{InputContent, InputSource};

fn text_part(id: usize, s: &str) -> InputPart {
    InputPart {
        id,
        source: InputSource::Literal,
        name: format!("t{id}"),
        kind: MediaKind::Text,
        unknown_kind: false,
        mime: "text/plain".into(),
        content: InputContent::Text(s.into()),
        unit: None,
    }
}

fn png_part(id: usize) -> InputPart {
    // Real (tiny) PNG bytes: the adapter boundary reads the header of
    // every image now, so placeholder bytes would be rejected.
    let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 255, 255, 255]));
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    InputPart {
        id,
        source: InputSource::File("a.png".into()),
        name: "a.png".into(),
        kind: MediaKind::Image,
        unknown_kind: false,
        mime: "image/png".into(),
        content: InputContent::Media(png),
        unit: None,
    }
}

#[test]
fn delta_only_eof_is_not_success() {
    let mut stream = Stream::default();
    stream
        .feed(
            r#"{"choices":[{"delta":{"content":"partial"}}]}"#,
            &mut |_| {},
        )
        .unwrap();
    assert!(stream.finish().is_err());
}

#[test]
fn terminal_reason_without_done_is_compatible() {
    let mut stream = Stream::default();
    stream
        .feed(
            r#"{"choices":[{"delta":{"content":"partial"},"finish_reason":"length"}]}"#,
            &mut |_| {},
        )
        .unwrap();
    let result = stream.finish().unwrap();
    assert_eq!(result.text, "partial");
    assert_eq!(
        result.status,
        GenerationStatus::Incomplete {
            reason: "length".into()
        }
    );
}

#[test]
fn stream_snapshots_emit_only_the_missing_suffix() {
    for (prefix, snapshots, expected_deltas) in [
        (None, vec!["answer"], vec!["answer"]),
        (None, vec!["answer", "answer"], vec!["answer"]),
        (Some("answer"), vec!["answer"], vec!["answer"]),
        (Some("Hello"), vec!["Hello world"], vec!["Hello", " world"]),
        (
            Some("你"),
            vec!["你好", "你好世界"],
            vec!["你", "好", "世界"],
        ),
    ] {
        let mut stream = Stream::default();
        let mut deltas = Vec::new();
        let mut emit = |delta: &str| deltas.push(delta.to_owned());
        if let Some(prefix) = prefix {
            let chunk = serde_json::json!({"choices":[{"delta":{"content":prefix}}]});
            assert!(!stream.feed(&chunk.to_string(), &mut emit).unwrap());
        }
        for snapshot in &snapshots {
            let chunk = serde_json::json!({"choices":[{"message":{"content":snapshot}}]});
            assert!(!stream.feed(&chunk.to_string(), &mut emit).unwrap());
        }
        // A full message still permits EOF without finish_reason or [DONE].
        let result = stream.finish().unwrap();
        assert_eq!(result.text, *snapshots.last().unwrap());
        assert_eq!(result.status, GenerationStatus::Complete);
        assert!(result.warnings.is_empty());
        assert_eq!(deltas, expected_deltas);
        assert_eq!(deltas.concat(), result.text);
    }
}

#[test]
fn stream_snapshot_mismatch_does_not_emit_or_rewrite_text() {
    // Changed, shortened, and empty snapshots all disagree with emitted text.
    for snapshot in ["你们", "你", ""] {
        let mut stream = Stream::default();
        let mut deltas = Vec::new();
        stream
            .feed(
                r#"{"choices":[{"delta":{"content":"你好"}}]}"#,
                &mut |delta| deltas.push(delta.to_owned()),
            )
            .unwrap();
        let chunk = serde_json::json!({"choices":[{"message":{"content":snapshot}}]});
        let err = stream
            .feed(&chunk.to_string(), &mut |delta| {
                deltas.push(delta.to_owned())
            })
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("Chat snapshot disagrees with streamed output"));
        assert_eq!(stream.result.text, "你好");
        assert_eq!(deltas, ["你好"]);
    }
}

#[test]
fn stream_snapshot_in_same_chunk_reconciles_after_delta() {
    let mut stream = Stream::default();
    let mut deltas = Vec::new();
    stream
        .feed(
            r#"{"choices":[{"delta":{"content":"你"},"message":{"content":"你好"}}]}"#,
            &mut |delta| deltas.push(delta.to_owned()),
        )
        .unwrap();
    assert_eq!(stream.finish().unwrap().text, "你好");
    assert_eq!(deltas, ["你", "好"]);
}

#[test]
fn stream_snapshot_preserves_incomplete_status() {
    for reason in ["length", "content_filter"] {
        let mut stream = Stream::default();
        stream
            .feed(r#"{"choices":[{"delta":{"content":"part"}}]}"#, &mut |_| {})
            .unwrap();
        let chunk = serde_json::json!({"choices":[{
            "message":{"content":"partial"}, "finish_reason":reason
        }]});
        stream.feed(&chunk.to_string(), &mut |_| {}).unwrap();
        assert!(stream.feed("[DONE]", &mut |_| {}).unwrap());
        let result = stream.finish().unwrap();
        assert_eq!(result.text, "partial");
        assert_eq!(
            result.status,
            GenerationStatus::Incomplete {
                reason: reason.into()
            }
        );
        assert_eq!(result.warnings.len(), 1);
    }
}

#[test]
fn standard_deltas_and_done_still_complete() {
    let mut stream = Stream::default();
    let mut deltas = Vec::new();
    for text in ["", "你", "好"] {
        let chunk = serde_json::json!({"choices":[{"delta":{"content":text}}]});
        assert!(!stream
            .feed(&chunk.to_string(), &mut |delta| deltas
                .push(delta.to_owned()))
            .unwrap());
    }
    assert!(stream.feed("[DONE]", &mut |_| {}).unwrap());
    let result = stream.finish().unwrap();
    assert_eq!(result.text, "你好");
    assert_eq!(result.status, GenerationStatus::Complete);
    assert_eq!(deltas, ["你", "好"]);
}

#[test]
fn empty_input_is_rejected_only_without_instructions() {
    assert!(build_messages(None, None, &[]).is_err());
    // instruction-only runs are valid: the ask journey relies on it
    let messages = build_messages(Some("draw a dog"), None, &[]).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, "user");
    assert!(matches!(
        &messages[0].content,
        Content::Text(t) if t == "draw a dog"
    ));
}

#[test]
fn mixed_input_keeps_order() {
    let parts = vec![text_part(0, "one"), png_part(1), text_part(2, "two")];
    let messages = build_messages(Some("sys"), None, &parts).unwrap();
    let Content::Parts(content) = &messages[1].content else {
        panic!("expected parts");
    };
    // text, image, text goes on the wire in exactly that order
    assert_eq!(content.len(), 3);
    let Part::Text { text } = &content[0] else {
        panic!("expected a text part first");
    };
    assert_eq!(text, "one");
    assert!(matches!(content[1], Part::ImageUrl { .. }));
    let Part::Text { text } = &content[2] else {
        panic!("expected the trailing text part");
    };
    assert_eq!(text, "two");
}

#[test]
fn audio_only_input_is_refused_not_dropped() {
    let audio = InputPart {
        id: 0,
        source: crate::domain::InputSource::File("a.mp3".into()),
        name: "a.mp3".into(),
        kind: MediaKind::Audio,
        unknown_kind: false,
        mime: "audio/mpeg".into(),
        content: crate::domain::InputContent::Media(vec![1, 2, 3]),
        unit: None,
    };
    let err = build_messages(Some("sys"), None, &[audio]).unwrap_err();
    assert!(err.to_string().contains("audio"), "{err}");
}

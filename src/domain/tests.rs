use super::*;

fn text_part(id: usize, s: &str) -> InputPart {
    InputPart {
        id,
        source: InputSource::Literal,
        name: format!("part-{id}"),
        kind: MediaKind::Text,
        unknown_kind: false,
        mime: "text/plain".into(),
        content: InputContent::Text(s.into()),
        unit: None,
    }
}

fn image_part(id: usize) -> InputPart {
    InputPart {
        id,
        source: InputSource::File("a.png".into()),
        name: "a.png".into(),
        kind: MediaKind::Image,
        unknown_kind: false,
        mime: "image/png".into(),
        content: InputContent::Media(vec![1, 2, 3]),
        unit: None,
    }
}

#[test]
fn extension_matches_format_accepts_aliases_not_case_variants() {
    // Exact names and the two conventional aliases match.
    assert!(extension_matches_format("png", "png"));
    assert!(extension_matches_format("jpg", "jpeg"));
    assert!(extension_matches_format("ogg", "opus"));
    // Anything else does not — including the alias read backwards (a
    // `.jpeg` file with `--format jpg`) and uppercase letters:
    // callers lowercase the extension before asking, so this fn
    // compares strings exactly.
    assert!(!extension_matches_format("png", "mp3"));
    assert!(!extension_matches_format("jpeg", "jpg"));
    assert!(!extension_matches_format("JPG", "jpeg"));
}

#[test]
fn first_line_takes_the_first_non_empty_line() {
    // Leading blank and indented lines are skipped; the survivor is
    // trimmed, and a text with no content at all yields "".
    assert_eq!(first_line("\n  \nsecond line\nthird", 64), "second line");
    assert_eq!(first_line("  padded  ", 64), "padded");
    assert_eq!(first_line("", 64), "");
    assert_eq!(first_line("\n \n", 64), "");
}

#[test]
fn first_line_elides_only_past_the_limit() {
    // At exactly max chars the line is kept whole; one char more and
    // it is cut to max chars plus the `...` marker.
    let exact: String = "x".repeat(8);
    assert_eq!(first_line(&exact, 8), exact);
    let over = format!("{exact}y");
    let expected = format!("{exact}...");
    assert_eq!(first_line(&over, 8), expected);
}

#[test]
fn first_line_counts_chars_not_bytes_so_cjk_survives_the_cut() {
    // 8 CJK chars are 24 bytes; eliding at 5 chars must keep whole
    // characters, not slice through a multi-byte sequence.
    assert_eq!(first_line("一二三四五六七八", 8), "一二三四五六七八");
    assert_eq!(first_line("一二三四五六七八", 5), "一二三四五...");
    assert_eq!(first_line("一二三四五六七八", 2), "一二...");
}

#[test]
fn ordered_mixed_input_keeps_position() {
    // text before image before text: order must survive, not be grouped.
    let parts = [text_part(0, "first"), image_part(1), text_part(2, "third")];
    assert_eq!(parts[0].kind, MediaKind::Text);
    assert_eq!(parts[1].kind, MediaKind::Image);
    assert_eq!(parts[2].text(), Some("third"));
}

#[test]
fn run_record_represents_generated_but_undelivered() {
    // generation complete, clipboard delivery failed: recoverable.
    let record = RunRecord {
        run_id: "r1".into(),
        task: Some("ocr".into()),
        created_at: "2026-09-10T00:00:00Z".into(),
        summary: RunSummary {
            task: Some("ocr".into()),
            profile: None,
            provider: None,
            model: None,
            adapter: None,
            inputs: Vec::new(),
            processor: None,
        },
        generation: GenerationStatus::Complete,
        artifacts: vec![Artifact {
            id: "a0".into(),
            kind: MediaKind::Text,
            mime: "text/plain".into(),
            format: "text".into(),
            bytes: b"hello".to_vec(),
            provenance: Provenance::Request { index: 0 },
        }],
        warnings: Vec::new(),
        failed_parts: Vec::new(),
        parts_total: 0,
        deliveries: vec![DeliveryState {
            destination: Destination::Clipboard,
            status: DeliveryStatus::Failed {
                error: "no display".into(),
            },
        }],
        stages: Vec::new(),
        last_stage_len: 0,
    };
    assert!(record.generation.is_complete());
    assert!(!record.deliveries[0].status.is_succeeded());
    assert_eq!(record.artifacts[0].text(), Some("hello"));
}

#[test]
fn app_error_chain_prints_causes() {
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
    let err = AppError::from(io_err);
    assert!(err.chain().contains("gone"));
    assert_eq!(err.kind.exit_code(), 3);
}

#[test]
fn app_error_from_io_error_prints_the_message_once() {
    // `From<io::Error>` stores the same text as message and source; the
    // chain walk must not print it twice.
    let err = AppError::from(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"));
    assert_eq!(err.chain(), "gone");
    assert_eq!(err.chain_inline(), "gone");
}

/// A cause layer with fixed text and an optional deeper cause, so tests
/// build chains without one struct per layer.
#[derive(Debug)]
struct Cause {
    text: &'static str,
    deeper: Option<Box<Cause>>,
}

impl std::fmt::Display for Cause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.text)
    }
}

impl std::error::Error for Cause {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.deeper
            .as_deref()
            .map(|c| c as &(dyn std::error::Error + 'static))
    }
}

#[test]
fn app_error_chain_skips_a_repeated_layer_and_keeps_distinct_ones() {
    // A cause repeating the text of the layer above it carries no new
    // information and is skipped; the distinct deeper cause still
    // renders, one line per layer.
    let err = AppError {
        kind: ErrorKind::Service,
        message: "outer".into(),
        source: Some(Box::new(Cause {
            text: "outer",
            deeper: Some(Box::new(Cause {
                text: "deep cause",
                deeper: None,
            })),
        })),
    };
    let chain = err.chain();
    let lines: Vec<&str> = chain.lines().collect();
    assert_eq!(lines, ["outer", "deep cause"]);
}

#[test]
fn app_error_chain_inline_joins_one_line_and_dedups() {
    // Same walk as chain(), but "; "-joined and with the same duplicate
    // skip: single-line contexts (history list labels, batch part
    // failure listings) get every distinct layer on their one row.
    let err = AppError {
        kind: ErrorKind::Service,
        message: "top message".into(),
        source: Some(Box::new(Cause {
            text: "top message",
            deeper: Some(Box::new(Cause {
                text: "mid cause",
                deeper: Some(Box::new(Cause {
                    text: "leaf cause",
                    deeper: None,
                })),
            })),
        })),
    };
    let inline = err.chain_inline();
    assert_eq!(inline, "top message; mid cause; leaf cause");
    assert!(!inline.contains('\n'));
}

#[test]
fn app_error_chain_walks_every_cause_layer_one_per_line() {
    // `From<io::Error>` only ever attaches one layer, so build a source
    // chain two layers deep by hand: the walk must not stop after the
    // first cause.
    #[derive(Debug)]
    struct Leaf;
    impl std::fmt::Display for Leaf {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("leaf cause")
        }
    }
    impl std::error::Error for Leaf {}

    #[derive(Debug)]
    struct Mid;
    impl std::fmt::Display for Mid {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("mid cause")
        }
    }
    impl std::error::Error for Mid {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&Leaf)
        }
    }

    let err = AppError {
        kind: ErrorKind::Service,
        message: "top message".into(),
        source: Some(Box::new(Mid)),
    };
    let chain = err.chain();
    let lines: Vec<&str> = chain.lines().collect();
    assert_eq!(lines, ["top message", "mid cause", "leaf cause"]);
}

#[test]
fn provenance_keeps_the_tagged_form_old_records_carry() {
    // The shape the --out-dir delivery manifests carry. Old records
    // hold "request" and "restored"; "merged" only joins them, so
    // every historical form still parses.
    assert_eq!(
        serde_json::to_value(Provenance::Request { index: 0 }).unwrap(),
        serde_json::json!({"type": "request", "index": 0})
    );
    for (raw, parsed) in [
        (
            r#"{"type":"request","index":3}"#,
            Provenance::Request { index: 3 },
        ),
        (r#"{"type":"restored"}"#, Provenance::Restored),
        (
            r#"{"type":"merged","requests":[0,1,2]}"#,
            Provenance::Merged {
                requests: vec![0, 1, 2],
            },
        ),
    ] {
        assert_eq!(serde_json::from_str::<Provenance>(raw).unwrap(), parsed);
    }
}

#[test]
fn exit_codes_are_distinct() {
    let codes = [
        ErrorKind::Usage.exit_code(),
        ErrorKind::Service.exit_code(),
        ErrorKind::Generation.exit_code(),
        ErrorKind::Delivery.exit_code(),
        ErrorKind::Partial.exit_code(),
    ];
    assert_eq!(codes, [2, 3, 4, 5, 6]);
}

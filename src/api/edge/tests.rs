use super::*;
use crate::domain::{InputContent, InputPart, InputSource, MediaKind};
use kothok_edge_tts::TtsError;
use std::future::Future;

fn text_part(s: &str) -> InputPart {
    InputPart {
        id: 0,
        source: InputSource::Literal,
        name: "--text".into(),
        kind: MediaKind::Text,
        unknown_kind: false,
        mime: "text/plain".into(),
        content: InputContent::Text(s.into()),
        unit: None,
    }
}

fn spec(text: &str, options: &[(&str, serde_json::Value)]) -> GenerateRequest<'static> {
    GenerateRequest {
        instruction: None,
        requirement: None,
        inputs: Box::leak(vec![text_part(text)].into_boxed_slice()),
        model: "edge",
        max_tokens: None,
        temperature: None,
        outputs: Box::leak(vec![MediaKind::Audio].into_boxed_slice()),
        options: Box::leak(Box::new(
            options
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect::<std::collections::BTreeMap<_, _>>(),
        )),
    }
}

#[test]
fn rate_mapping_covers_bounds_and_default() {
    assert_eq!(rate_from_speed(None), "+0%");
    assert_eq!(rate_from_speed(Some(&serde_json::json!(1.0))), "+0%");
    assert_eq!(rate_from_speed(Some(&serde_json::json!(0.25))), "-75%");
    assert_eq!(rate_from_speed(Some(&serde_json::json!(4.0))), "+300%");
    assert_eq!(rate_from_speed(Some(&serde_json::json!(1.5))), "+50%");
}

#[test]
fn lang_follows_the_voice_locale() {
    assert_eq!(lang_from_voice("zh-CN-XiaoxiaoNeural"), "zh-CN");
    assert_eq!(lang_from_voice("en-US-EmmaMultilingualNeural"), "en-US");
    assert_eq!(lang_from_voice("weird"), "en-US");
}

#[test]
fn short_text_stays_one_chunk() {
    assert_eq!(
        chunk_by_escaped_bytes("你好，世界", 4096),
        vec!["你好，世界"]
    );
}

#[test]
fn control_chars_are_sanitized_before_measuring() {
    assert_eq!(chunk_by_escaped_bytes("a\u{0b}b", 4096), vec!["a b"]);
}

#[test]
fn long_chinese_text_splits_after_sentence_punctuation() {
    let sentence = "这是一句话。";
    let text = sentence.repeat(1500);
    let chunks = chunk_by_escaped_bytes(&text, 4096);
    assert!(chunks.len() > 3);
    for chunk in &chunks {
        assert!(chunk.len() <= 4096);
        assert!(chunk.ends_with('。'));
    }
    assert_eq!(chunks.concat(), text);
}

#[test]
fn escape_expansion_is_respected() {
    // 1000 '&' occupy 5000 bytes escaped — one chunk cannot hold them.
    let text = "&".repeat(1000);
    let chunks = chunk_by_escaped_bytes(&text, 4096);
    assert!(chunks.len() >= 2);
    let escaped: usize = chunks.iter().map(|c| c.len()).sum();
    assert_eq!(escaped, 1000); // raw '&' bytes are preserved
}

#[test]
fn multibyte_chars_are_never_cut() {
    // '日' is 3 bytes; a budget of 10 fits three of them (9 bytes).
    let text = "日日日日";
    let chunks = chunk_by_escaped_bytes(text, 10);
    assert_eq!(chunks, vec!["日日日", "日"]);
}

#[test]
fn whitespace_only_text_yields_nothing() {
    assert!(chunk_by_escaped_bytes("  \n ", 4096).is_empty());
}

struct FakeEngine(Result<Vec<TtsEvent>, TtsError>);

impl Engine for FakeEngine {
    fn synthesize(
        &self,
        text: &str,
        _voice: &str,
        _rate: &str,
        _lang: &str,
    ) -> impl Future<Output = Result<Vec<TtsEvent>, TtsError>> + Send {
        // Echo each chunk as an mp3-tagged frame so ordering is
        // observable and the result passes mp3 signature validation.
        let frame = [b"ID3\x04".as_slice(), text.as_bytes()].concat();
        let result = match &self.0 {
            Ok(_) => Ok(vec![TtsEvent::Audio(frame), TtsEvent::TurnEnd]),
            Err(TtsError::NoAudio) => Err(TtsError::NoAudio),
            Err(other) => Err(TtsError::Connect(other.to_string())),
        };
        async move { result }
    }
}

/// Sleeps before answering, so total-budget enforcement is observable.
struct SlowEngine(Duration);

impl Engine for SlowEngine {
    fn synthesize(
        &self,
        _text: &str,
        _voice: &str,
        _rate: &str,
        _lang: &str,
    ) -> impl Future<Output = Result<Vec<TtsEvent>, TtsError>> + Send {
        let delay = self.0;
        async move {
            tokio::time::sleep(delay).await;
            Ok(vec![
                TtsEvent::Audio(b"ID3\x04".to_vec()),
                TtsEvent::TurnEnd,
            ])
        }
    }
}

#[tokio::test]
async fn audio_events_are_joined_in_order() {
    // 3000 CJK chars exceed the 4 KiB escaped budget → several chunks.
    let text = "甲".repeat(3000);
    let chunk_count = chunk_by_escaped_bytes(&text, CHUNK_ESCAPED_BYTES).len();
    assert!(chunk_count >= 2);

    let engine = FakeEngine(Ok(vec![TtsEvent::Audio(vec![]), TtsEvent::TurnEnd]));
    let result = synthesize_with(&engine, &spec(&text, &[]), Duration::from_secs(5), None)
        .await
        .unwrap();

    assert_eq!(result.artifacts.len(), 1);
    let audio = &result.artifacts[0].bytes;
    let expected = chunk_by_escaped_bytes(&text, CHUNK_ESCAPED_BYTES)
        .iter()
        .map(|chunk| [b"ID3\x04".as_slice(), chunk.as_bytes()].concat())
        .collect::<Vec<_>>()
        .concat();
    assert_eq!(audio, &expected);
}

#[tokio::test]
async fn engine_errors_surface_with_context() {
    let engine = FakeEngine(Err(TtsError::NoAudio));
    let err = synthesize_with(&engine, &spec("hi", &[]), Duration::from_secs(5), None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("edge-tts synthesis failed"));
}

#[tokio::test]
async fn instructions_are_refused() {
    let mut s = spec("hi", &[]);
    s.requirement = Some("不要读这一句");
    let err = synthesize_with(&FakeEngine(Ok(vec![])), &s, Duration::from_secs(5), None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no instruction channel"));
}

#[tokio::test]
async fn total_timeout_bounds_the_whole_synthesis() {
    // One chunk that answers long after the whole-run budget has passed.
    let err = synthesize_with(
        &SlowEngine(Duration::from_secs(60)),
        &spec("hi", &[]),
        Duration::from_secs(120),
        Some(Duration::from_millis(50)),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("total time budget"), "{err}");
}

#[tokio::test]
async fn without_a_total_timeout_per_chunk_bounds_remain() {
    // A slow engine inside the per-chunk budget finishes even though no
    // whole-run budget is set.
    let result = synthesize_with(
        &SlowEngine(Duration::from_millis(50)),
        &spec("hi", &[]),
        Duration::from_secs(5),
        None,
    )
    .await
    .unwrap();
    assert_eq!(result.artifacts.len(), 1);
}

#[tokio::test]
#[ignore = "hits the live Edge endpoint; run with: cargo test edge_live -- --ignored"]
async fn edge_live_synthesizes_mp3() {
    let result = synthesize(&spec("你好，世界。", &[]), Duration::from_secs(120), None)
        .await
        .unwrap();
    assert_eq!(result.artifacts.len(), 1);
    assert_eq!(result.artifacts[0].format, "mp3");
    assert!(result.artifacts[0].bytes.starts_with(b"ID3") || result.artifacts[0].bytes[0] == 0xFF);
}

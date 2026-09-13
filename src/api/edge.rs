//! Edge TTS adapter: maps aido's request contract onto the
//! `kothok-edge-tts` client (Microsoft's unofficial Read Aloud protocol).
//!
//! The endpoint, DRM (`Sec-MS-GEC`) token, WebSocket framing and SSML
//! encoding live in the dependency, whose protocol internals were verified
//! against the reference `edge-tts` implementation (rany2). This module owns
//! what the protocol pushes to the caller: chunking (one SSML request is
//! capped near 4 KiB escaped), speed→rate mapping, the whole-run time
//! budget, and the instruction channel, which the protocol has no place
//! for. Microsoft rotates the DRM constants periodically; protocol
//! breakage is fixed upstream, not here.

use super::{media, plain_text, GenerateRequest, GenerateResult};
use anyhow::{anyhow, bail, Result};
use futures_util::StreamExt;
use kothok_edge_tts::{EdgeTts, Engine, TtsEvent};
use std::time::Duration;

/// Byte budget for the XML-escaped text of one SSML request — the same ~4 KiB
/// cap the reference implementation applies. Chunks are independent requests
/// whose MP3 output is concatenated, so the budget is enforced on the escaped
/// size (the dependency escapes when building the SSML).
const CHUNK_ESCAPED_BYTES: usize = 4096;

/// Chunks are independent; a few run concurrently, reassembled in text order
/// so the MP3 stream stays contiguous.
const CONCURRENCY: usize = 4;

/// The protocol has no server-side default voice; Chinese is the sane
/// default for this tool's audience.
const DEFAULT_VOICE: &str = "zh-CN-XiaoxiaoNeural";

const DEFAULT_LANG: &str = "en-US";

pub(super) async fn synthesize(
    spec: &GenerateRequest<'_>,
    timeout: Duration,
    total_timeout: Option<Duration>,
) -> Result<GenerateResult> {
    synthesize_with(&EdgeTts, spec, timeout, total_timeout).await
}

async fn synthesize_with<E: Engine>(
    engine: &E,
    spec: &GenerateRequest<'_>,
    timeout: Duration,
    total_timeout: Option<Duration>,
) -> Result<GenerateResult> {
    let text = plain_text(spec.inputs)?;
    // The protocol has no instruction channel: the direction would either be
    // dropped silently or read aloud — refuse instead of guessing.
    if !spec.instruction_channel().is_empty() {
        bail!(
            "{}; drop -p or the task's fixed instruction, or use a provider \
             whose speech route has one",
            super::EDGE_NO_INSTRUCTION_CHANNEL
        );
    }
    let voice = spec
        .options
        .get("voice")
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(DEFAULT_VOICE)
        .to_owned();
    let rate = rate_from_speed(spec.options.get("speed"));
    let lang = lang_from_voice(&voice);

    let chunks = chunk_by_escaped_bytes(&text, CHUNK_ESCAPED_BYTES);
    if chunks.is_empty() {
        bail!("there is nothing to speak");
    }
    kothok_edge_tts::init_tls();

    // Bind by reference: every chunk's future must share them (FnMut closure).
    let (voice, rate, lang) = (&voice, &rate, &lang);
    // The whole run — every chunk — must fit the total budget when one is
    // set; without it the per-chunk timeouts are the only bound.
    let pieces: Vec<Result<Vec<u8>>> = {
        let work = futures_util::stream::iter(chunks)
            .map(|chunk| async move {
                let events =
                    tokio::time::timeout(timeout, engine.synthesize(&chunk, voice, rate, lang))
                        .await
                        .map_err(|_| {
                            anyhow!(
                                "edge-tts synthesis timed out ({}s per chunk)",
                                timeout.as_secs()
                            )
                        })?
                        .map_err(|e| anyhow!("edge-tts synthesis failed: {e}"))?;
                Ok(events
                    .into_iter()
                    .filter_map(|event| match event {
                        TtsEvent::Audio(bytes) => Some(bytes),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .concat())
            })
            .buffered(CONCURRENCY)
            .collect::<Vec<_>>();
        match total_timeout {
            Some(total) => tokio::time::timeout(total, work).await.map_err(|_| {
                anyhow!(
                    "edge-tts synthesis exceeded the total time budget ({}s); \
                         split the text or raise --total-timeout",
                    total.as_secs()
                )
            })?,
            None => work.await,
        }
    };

    let mut audio = Vec::new();
    for piece in pieces {
        audio.extend_from_slice(&piece?);
    }
    // Reuse the OpenAI speech path's response validation (mp3 signature) and
    // artifact assembly; the payload format is identical.
    media::speech(audio, "mp3", "audio/mpeg")
}

/// `--speed` 0.25..=4 → SSML prosody rate: 1.0 → "+0%", 0.25 → "-75%".
fn rate_from_speed(speed: Option<&serde_json::Value>) -> String {
    let pct = speed
        .and_then(|v| v.as_f64())
        .map(|s| ((s - 1.0) * 100.0).round() as i64)
        .unwrap_or(0);
    format!("{pct:+}%")
}

/// Voice short-names embed their locale: "zh-CN-XiaoxiaoNeural" → "zh-CN".
fn lang_from_voice(voice: &str) -> String {
    let mut parts = voice.split('-');
    match (parts.next(), parts.next()) {
        (Some(a), Some(b)) if a.len() == 2 && b.len() == 2 => format!("{a}-{b}"),
        _ => DEFAULT_LANG.to_string(),
    }
}

/// Bytes the XML escape adds for one character ('&' → "&amp;" etc.).
fn escaped_len(c: char) -> usize {
    match c {
        '&' => 5,
        '<' | '>' => 4,
        '\'' | '"' => 6,
        _ => c.len_utf8(),
    }
}

/// Replace the control characters the endpoint rejects with spaces (same set
/// as the reference implementation: 0x00-08, 0x0B-0C, 0x0E-1F).
fn sanitize_control_chars(text: &str) -> String {
    text.chars()
        .map(|c| {
            let code = c as u32;
            if code <= 8 || (11..=12).contains(&code) || (14..=31).contains(&code) {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Split text so each chunk stays within `budget` bytes once XML-escaped.
///
/// Splits prefer the last newline, then CJK/ASCII sentence punctuation, then
/// a space, inside the window; walking chars keeps UTF-8 boundaries intact
/// and guarantees progress. Whole raw chunks are escaped later by the
/// dependency, so an escape sequence can never be cut in half.
fn chunk_by_escaped_bytes(text: &str, budget: usize) -> Vec<String> {
    let text = sanitize_control_chars(text);
    let mut chunks = Vec::new();
    let mut rest = text.as_str();
    while !rest.is_empty() {
        let mut escaped = 0usize;
        // Bytes of `rest` that fit the budget; the first char always fits,
        // so this advances by at least one character per chunk.
        let mut hard_end = 0usize;
        for (off, c) in rest.char_indices() {
            let size = escaped_len(c);
            if escaped + size > budget && hard_end > 0 {
                break;
            }
            escaped += size;
            hard_end = off + c.len_utf8();
        }
        let window = &rest[..hard_end];
        let cut = if hard_end >= rest.len() {
            // Everything left fits — no split needed, so no boundary hunt
            // (it would carve "a b" into "a"/"b" around the separator).
            hard_end
        } else {
            preferred_boundary(window)
                .filter(|&pos| pos > 0 && pos < hard_end)
                .unwrap_or(hard_end)
        };
        let chunk = rest[..cut].trim();
        if !chunk.is_empty() {
            chunks.push(chunk.to_string());
        }
        rest = &rest[cut..];
    }
    chunks
}

/// Offset just past the last preferred split point in `window`, if any.
fn preferred_boundary(window: &str) -> Option<usize> {
    if let Some(pos) = window.rfind('\n') {
        return Some(pos + 1);
    }
    let punctuation = ['。', '！', '？', '；', '…'];
    if let Some(pos) = window.rfind(punctuation) {
        let len = window[pos..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
        return Some(pos + len);
    }
    window.rfind(' ').map(|pos| pos + 1)
}

#[cfg(test)]
mod tests {
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
        assert!(
            result.artifacts[0].bytes.starts_with(b"ID3") || result.artifacts[0].bytes[0] == 0xFF
        );
    }
}

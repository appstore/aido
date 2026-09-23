//! Offline audio decoding, backed by asr-core.
//!
//! All decode logic, format coverage (mp3, m4a/aac, flac, ogg/vorbis,
//! mkv, wav) and the failure philosophy — stream changes fail loudly,
//! locally damaged data only loses the affected packets — live in
//! [`asr_core::audio::decode_audio`] (the `decode-symphonia` feature).
//! This module keeps only what aido knows and asr-core does not: routing
//! webm/opus users to the cloud transcription route, refusing empty
//! results, and the sample budget.

use anyhow::{bail, Context, Result};

/// Decode audio bytes to mono f32 samples at the source sample rate.
///
/// `max_samples` caps the decoded buffer length; the whole decode result
/// is resident in memory before the check runs (the memory contract is
/// the host's per asr-core's own docs, and prevention waits on upstream
/// chunked decoding) — so this is a detection, not a guard against the
/// allocation itself.
pub fn decode_mono(bytes: &[u8], max_samples: usize) -> Result<asr_core::AudioBuffer> {
    // webm audio is opus in practice, and opus has no decoder in
    // symphonia — the attempt is guaranteed to fail, so answer with the
    // actionable message instead of a codec error. MKV shares webm's EBML
    // magic, so the DocType string decides; matroska files say
    // "matroska" and decode fine.
    if is_webm(bytes) {
        bail!(
            "webm audio uses opus, which the offline engine cannot decode; \
             use the cloud transcription route"
        );
    }
    let buffer = asr_core::audio::decode_audio(bytes).context("offline decoding failed")?;
    if buffer.samples.is_empty() {
        bail!("no audio could be decoded from the input");
    }
    if buffer.samples.len() > max_samples {
        bail!(
            "decoded audio exceeds the sample budget ({max_samples} \
             samples); the input is too long for offline decoding"
        );
    }
    Ok(buffer)
}

/// EBML magic plus the DocType element, matched as an anchored whole:
/// element ID 0x4282, size vint 0x84 (the DocType for webm is exactly the
/// four-byte string "webm"), then the string. Searching for the element
/// rather than the bare string keeps stray header bytes from matching;
/// matroska/mka files carry "matroska" instead and do not match. A
/// non-minimal size vint (legal per EBML) falls through and gets the
/// generic decode error — correct, just less specific.
fn is_webm(bytes: &[u8]) -> bool {
    if !bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return false;
    }
    let head = &bytes[..bytes.len().min(128)];
    head.windows(7)
        .any(|w| w == [0x42, 0x82, 0x84, b'w', b'e', b'b', b'm'])
}

#[cfg(test)]
mod tests;

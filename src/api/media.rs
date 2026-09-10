//! Buffered media protocols. Generation options are serialized structurally,
//! never interpolated into JSON templates.
use super::{merged_text, single_audio, GenerateRequest, GenerateResult};
use crate::domain::{Artifact, MediaKind};
use anyhow::{bail, Context, Result};
use base64::Engine as _;
use serde_json::{json, Value};

/// Speech: the material is the text to read; the instruction channel
/// (task direction and -p) goes to `instructions`, never into the speech.
pub(super) fn encode_speech(spec: &GenerateRequest<'_>) -> Result<Value> {
    let input = merged_text(spec.inputs)?;
    let mut body = json!({
        "model": spec.model,
        "input": input,
        "response_format": "mp3",
    });
    let instructions = spec.instruction_channel();
    if !instructions.is_empty() {
        body["instructions"] = json!(instructions);
    }
    for (key, value) in spec.options {
        let key = match key.as_str() {
            "format" => "response_format",
            other => other,
        };
        body[key] = value.clone();
    }
    Ok(body)
}

/// Image generation: the prompt is the instruction channel plus the text
/// material — the protocol has no separate instruction field.
pub(super) fn encode_images(spec: &GenerateRequest<'_>) -> Result<Value> {
    let instructions = spec.instruction_channel();
    // The description is the text material when there is any; otherwise the
    // instruction alone drives the generation (`ask -p "draw a dog"`).
    let material = merged_text(spec.inputs).unwrap_or_default();
    let prompt = match (instructions.is_empty(), material.is_empty()) {
        (false, false) => format!("{instructions}\n\n{material}"),
        (false, true) => instructions,
        (true, false) => material,
        (true, true) => bail!("image generation needs a description (--text or -p)"),
    };
    let mut body = json!({"model": spec.model, "prompt": prompt});
    if spec.model.starts_with("dall-e-") {
        body["response_format"] = json!("b64_json");
    }
    for (key, value) in spec.options {
        let key = match key.as_str() {
            "format" => "output_format",
            other => other,
        };
        body[key] = value.clone();
    }
    Ok(body)
}

pub(super) fn transcription(spec: &GenerateRequest<'_>) -> Result<reqwest::multipart::Form> {
    let audio = single_audio(spec.inputs)?;
    let bytes = match &audio.content {
        crate::domain::InputContent::Media(b) => b.clone(),
        _ => bail!("transcription input must be audio"),
    };
    let extension = audio.mime.rsplit('/').next().unwrap_or("bin");
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name(format!("input.{extension}"))
        .mime_str(&audio.mime)?;
    let mut form = reqwest::multipart::Form::new()
        .text("model", spec.model.to_owned())
        .text("response_format", "json")
        .part("file", part);
    let prompt = spec.instruction_channel();
    if !prompt.is_empty() {
        form = form.text("prompt", prompt);
    }
    if let Some(language) = spec.options.get("language").and_then(Value::as_str) {
        form = form.text("language", language.to_owned());
    }
    if let Some(temperature) = spec.temperature {
        form = form.text("temperature", temperature.to_string());
    }
    Ok(form)
}

pub(super) fn parse_transcription(body: &str) -> Result<GenerateResult> {
    let body: Value = serde_json::from_str(body).context("unexpected transcription response")?;
    let text = body["text"]
        .as_str()
        .context("transcription response is missing text")?;
    Ok(GenerateResult::complete_with_text(text))
}

pub(super) fn image(encoded: &str) -> Result<Artifact> {
    image_bytes(
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .context("invalid image base64")?,
    )
}

pub(super) fn image_bytes(bytes: Vec<u8>) -> Result<Artifact> {
    let format = image::guess_format(&bytes).context("response is not a supported image")?;
    let (format, mime) = match format {
        image::ImageFormat::Png => ("png", "image/png"),
        image::ImageFormat::Jpeg => ("jpeg", "image/jpeg"),
        image::ImageFormat::WebP => ("webp", "image/webp"),
        _ => bail!("unsupported generated image format"),
    };
    // Validate decoded pixels before saving a success/history entry.
    image::load_from_memory(&bytes).context("invalid generated image")?;
    Ok(Artifact {
        id: String::new(), // assigned by the runner
        kind: MediaKind::Image,
        mime: mime.into(),
        format: format.into(),
        bytes,
        provenance: crate::domain::Provenance::Restored,
    })
}

pub(super) fn speech(bytes: Vec<u8>, format: &str, content_type: &str) -> Result<GenerateResult> {
    if bytes.is_empty() {
        bail!("speech response is empty");
    }
    let content_type = content_type.to_ascii_lowercase();
    if content_type.contains("json") || content_type.starts_with("text/") {
        bail!("speech endpoint returned {content_type}, expected audio");
    }
    if let Ok(body) = serde_json::from_slice::<Value>(&bytes) {
        if let Some(error) = body.get("error") {
            bail!("speech API error: {error}");
        }
    }
    let mime = match format {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "opus" => "audio/ogg",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "pcm" => "audio/pcm",
        _ => bail!("unsupported audio format '{format}'"),
    };
    let declared = match content_type.split(';').next().unwrap_or("").trim() {
        "" | "application/octet-stream" | "binary/octet-stream" => None,
        "audio/mpeg" | "audio/mp3" => Some("mp3"),
        "audio/wav" | "audio/wave" | "audio/x-wav" | "audio/vnd.wave" => Some("wav"),
        "audio/ogg" | "application/ogg" | "audio/opus" => Some("opus"),
        "audio/aac" | "audio/aacp" => Some("aac"),
        "audio/flac" | "audio/x-flac" => Some("flac"),
        "audio/pcm" | "audio/l16" => Some("pcm"),
        other => bail!("unsupported speech Content-Type '{other}'"),
    };
    if let Some(actual) = declared {
        if actual != format {
            bail!("speech response format '{actual}' does not match requested '{format}'");
        }
    }
    let detected = audio_format(&bytes);
    if let Some(actual) = detected {
        if actual != format {
            bail!("speech response bytes are '{actual}', expected '{format}'");
        }
    } else if format != "pcm" {
        bail!("speech response has no recognizable '{format}' header");
    }
    Ok(GenerateResult {
        status: crate::domain::GenerationStatus::Complete,
        artifacts: vec![Artifact {
            id: String::new(),
            kind: MediaKind::Audio,
            mime: mime.into(),
            format: format.into(),
            bytes,
            provenance: crate::domain::Provenance::Restored,
        }],
        ..Default::default()
    })
}

// Header checks identify the container/codec; they do not fully decode audio.
fn audio_format(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE") {
        Some("wav")
    } else if bytes.starts_with(b"fLaC") {
        Some("flac")
    } else if bytes.starts_with(b"OggS") && bytes.windows(8).any(|w| w == b"OpusHead") {
        Some("opus")
    } else if bytes.starts_with(b"ID3") {
        Some("mp3")
    } else if bytes.len() >= 2 && bytes[0] == 0xff && bytes[1] & 0xf6 == 0xf0 {
        Some("aac")
    } else if bytes.len() >= 2
        && bytes[0] == 0xff
        && bytes[1] & 0xe0 == 0xe0
        && bytes[1] & 0x06 != 0
        && bytes[1] & 0x18 != 0x08
    {
        Some("mp3")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speech_checks_mime_and_signature_independently() {
        let wav = b"RIFF\x04\0\0\0WAVE".to_vec();
        assert!(speech(wav.clone(), "mp3", "audio/wav").is_err());
        assert!(speech(wav.clone(), "mp3", "audio/mpeg").is_err());
        assert!(speech(wav.clone(), "mp3", "application/octet-stream").is_err());
        assert!(speech(wav, "wav", "Audio/Wav; charset=binary").is_ok());
        assert!(speech(b"garbage".to_vec(), "mp3", "audio/mpeg").is_err());
    }

    #[test]
    fn speech_accepts_supported_headers_and_headerless_pcm() {
        for (format, bytes) in [
            ("mp3", b"ID3\x04\0\0\0\0\0\0".as_slice()),
            ("mp3", b"\xff\xfb\x90\0"),
            ("aac", b"\xff\xf1\x50\x80"),
            ("flac", b"fLaC\0\0\0\0"),
            ("opus", b"OggS\0\0OpusHead"),
            ("pcm", b"\0\0\x01\0"),
        ] {
            assert!(
                speech(bytes.to_vec(), format, "application/octet-stream").is_ok(),
                "{format}"
            );
        }
    }
}

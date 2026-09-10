//! Application-level generation contracts. Wire formats stay in adapters.
mod chat;
mod media;
mod responses;
mod sse;
mod transport;

use crate::domain::{Artifact, GenerationStatus, InputPart, MediaKind};
use anyhow::{bail, Result};
use clap::ValueEnum;
use serde::Deserialize;
pub use transport::{normalize_base_url, Client, Connection};

pub type MediaMode = MediaKind;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Adapter {
    #[default]
    #[serde(rename = "openai-chat")]
    #[value(name = "openai-chat")]
    Chat,
    #[serde(rename = "openai-responses")]
    #[value(name = "openai-responses")]
    Responses,
    #[serde(rename = "openai-speech")]
    #[value(name = "openai-speech")]
    Speech,
    #[serde(rename = "openai-transcription")]
    #[value(name = "openai-transcription")]
    Transcription,
    #[serde(rename = "openai-images")]
    #[value(name = "openai-images")]
    Images,
}

impl std::fmt::Display for Adapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Chat => "openai-chat",
            Self::Responses => "openai-responses",
            Self::Speech => "openai-speech",
            Self::Transcription => "openai-transcription",
            Self::Images => "openai-images",
        })
    }
}

impl Adapter {
    pub fn inputs(self) -> &'static [MediaKind] {
        use MediaKind::*;
        match self {
            Self::Chat | Self::Responses => &[Text, Image],
            Self::Speech | Self::Images => &[Text],
            Self::Transcription => &[Audio],
        }
    }
    pub fn outputs(self) -> &'static [MediaKind] {
        use MediaKind::*;
        match self {
            Self::Chat | Self::Transcription => &[Text],
            Self::Responses => &[Text, Image],
            Self::Speech => &[Audio],
            Self::Images => &[Image],
        }
    }
    pub fn default_model(self) -> &'static str {
        match self {
            Self::Speech => "tts-1",
            Self::Transcription => "whisper-1",
            Self::Images => "gpt-image-1",
            _ => "gpt-4o-mini",
        }
    }
    pub fn streams(self) -> bool {
        matches!(self, Self::Chat | Self::Responses)
    }
    pub fn validate_options(
        self,
        options: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<()> {
        let allowed: &[&str] = match self {
            Self::Chat => &[],
            Self::Responses => &["format", "size", "quality", "background"],
            Self::Speech => &["voice", "format", "speed"],
            Self::Transcription => &["language"],
            Self::Images => &["format", "size", "quality", "background", "n"],
        };
        for (key, value) in options {
            if !allowed.contains(&key.as_str()) {
                bail!("adapter '{self}' does not support option '{key}'");
            }
            match key.as_str() {
                "speed" => {
                    if !value.as_f64().is_some_and(|v| (0.25..=4.0).contains(&v)) {
                        bail!("speed must be a number between 0.25 and 4");
                    }
                }
                "n" => {
                    if !value.as_u64().is_some_and(|v| (1..=10).contains(&v)) {
                        bail!("n must be an integer between 1 and 10");
                    }
                }
                _ => {
                    if value.as_str().is_none_or(|s| s.trim().is_empty()) {
                        bail!("option '{key}' must be a nonempty string");
                    }
                }
            }
        }
        if let Some(format) = options.get("format").and_then(|v| v.as_str()) {
            let formats: &[&str] = if self == Self::Speech {
                &["mp3", "opus", "aac", "flac", "wav", "pcm"]
            } else {
                &["png", "jpeg", "webp"]
            };
            if !formats.contains(&format) {
                bail!("unsupported format '{format}' for adapter '{self}'");
            }
        }
        Ok(())
    }
}

/// One request to one adapter. `instruction` is the task's fixed direction,
/// `requirement` is this run's -p; they stay separate until the adapter
/// maps them onto whatever instruction channel the protocol has.
pub struct GenerateRequest<'a> {
    pub instruction: Option<&'a str>,
    pub requirement: Option<&'a str>,
    pub inputs: &'a [InputPart],
    pub model: &'a str,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub outputs: &'a [MediaKind],
    pub options: &'a std::collections::BTreeMap<String, serde_json::Value>,
}

impl GenerateRequest<'_> {
    /// The full instruction channel content (fixed direction + -p), for
    /// protocols with a single instructions field.
    pub fn instruction_channel(&self) -> String {
        [self.instruction, self.requirement]
            .into_iter()
            .flatten()
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// One adapter response, in domain terms. Text is not an artifact yet —
/// the runner promotes it when the run's artifacts are assembled. The
/// default status is Complete because every parse path that keeps the
/// default produced a fully-formed reply.
#[derive(Debug, Default)]
pub struct GenerateResult {
    pub text: String,
    pub artifacts: Vec<Artifact>,
    pub status: GenerationStatus,
    pub warnings: Vec<String>,
}

impl GenerateResult {
    pub fn complete() -> Self {
        Self {
            status: GenerationStatus::Complete,
            ..Default::default()
        }
    }

    pub fn complete_with_text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            status: GenerationStatus::Complete,
            ..Default::default()
        }
    }
}

impl GenerateResult {
    /// Surface truncation/refusal diagnostics as user-visible warnings,
    /// so an incomplete run explains what to do about it (the runner
    /// prints warnings and keeps them in the run record).
    pub fn note_incomplete(&mut self) {
        if let GenerationStatus::Incomplete { reason } = &self.status {
            let hint = match reason.as_str() {
                "length" | "max_output_tokens" => {
                    "reply hit the token limit and was truncated; set --max-tokens \
                     higher if text is missing"
                        .to_string()
                }
                "content_filter" => "reply was cut short by the server's content filter".into(),
                _ => format!("reply is incomplete: {reason}"),
            };
            if !self.warnings.contains(&hint) {
                self.warnings.push(hint);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared input helpers
// ---------------------------------------------------------------------------

/// Every text part in order, file-sourced ones labeled so the model can
/// tell them apart. A lone file text is passed through untouched (old
/// behavior); labels appear as soon as there is more than one text part
/// or any media next to it.
pub(crate) fn labeled_texts(inputs: &[InputPart]) -> Vec<String> {
    let text_count = inputs.iter().filter(|p| p.kind == MediaKind::Text).count();
    inputs
        .iter()
        .filter(|p| p.kind == MediaKind::Text)
        .map(|p| {
            let raw = p.text().unwrap_or_default();
            let is_file = matches!(
                p.source,
                crate::domain::InputSource::File(_) | crate::domain::InputSource::Stdin
            );
            if is_file && (text_count > 1 || inputs.len() > 1) {
                format!("--- {} ---\n\n{raw}", p.name)
            } else {
                raw.to_string()
            }
        })
        .collect()
}

/// Text-field routes (image prompts) take one string: text parts in
/// order, joined with a blank line. File-sourced parts are labeled so the
/// model can tell them apart.
pub(crate) fn merged_text(inputs: &[InputPart]) -> Result<String> {
    let parts = labeled_texts(inputs);
    if parts.is_empty() {
        bail!("this operation requires text material");
    }
    Ok(parts.join("\n\n"))
}

/// Speech input: the text parts in order, joined with a blank line — no
/// file-name labels, which the voice would read aloud.
pub(crate) fn plain_text(inputs: &[InputPart]) -> Result<String> {
    let parts: Vec<String> = inputs
        .iter()
        .filter(|p| p.kind == MediaKind::Text)
        .map(|p| p.text().unwrap_or_default().to_string())
        .collect();
    if parts.is_empty() {
        bail!("this operation requires text material");
    }
    Ok(parts.join("\n\n"))
}

/// Image parts as PNG bytes (the only encoding the chat/responses routes
/// send); non-PNG inputs are re-encoded here, at the adapter boundary.
pub(crate) fn image_as_png(part: &InputPart) -> Result<Vec<u8>> {
    let bytes = match &part.content {
        crate::domain::InputContent::Media(b) => b,
        _ => bail!("'{}' is not an image", part.name),
    };
    if part.mime == "image/png" {
        return Ok(bytes.clone());
    }
    let img = image::load_from_memory(bytes)
        .map_err(|e| anyhow::anyhow!("failed to decode image '{}': {e}", part.name))?;
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| anyhow::anyhow!("failed to re-encode image '{}': {e}", part.name))?;
    Ok(png)
}

pub(crate) fn single_audio(inputs: &[InputPart]) -> Result<&InputPart> {
    let audios: Vec<&InputPart> = inputs
        .iter()
        .filter(|p| p.kind == MediaKind::Audio)
        .collect();
    match audios.len() {
        1 => Ok(audios[0]),
        0 => bail!("this operation requires exactly one audio input; none given"),
        n => bail!("this operation requires exactly one audio input; {n} given"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{InputContent, InputSource};

    fn text(name: &str, source: InputSource, s: &str) -> InputPart {
        InputPart {
            id: 0,
            source,
            name: name.into(),
            kind: MediaKind::Text,
            mime: "text/plain".into(),
            content: InputContent::Text(s.into()),
        }
    }

    #[test]
    fn single_text_stays_raw() {
        let parts = vec![text("a.txt", InputSource::File("a.txt".into()), "alpha\n")];
        assert_eq!(labeled_texts(&parts), vec!["alpha\n"]);
    }

    #[test]
    fn multiple_texts_are_labeled_in_order() {
        let parts = vec![
            text("a.txt", InputSource::File("a.txt".into()), "alpha\n"),
            text("--text #1", InputSource::Literal, "literal\n"),
            text("stdin", InputSource::Stdin, "piped\n"),
        ];
        let labeled = labeled_texts(&parts);
        assert!(labeled[0].contains("--- a.txt ---"));
        assert_eq!(labeled[1], "literal\n");
        assert!(labeled[2].contains("piped\n"));
    }

    #[test]
    fn merged_text_joins_with_blank_line() {
        let parts = vec![
            text("a.txt", InputSource::File("a.txt".into()), "alpha"),
            text("b.txt", InputSource::File("b.txt".into()), "beta"),
        ];
        assert!(merged_text(&parts).unwrap().contains("\n\n"));
    }

    #[test]
    fn instruction_channel_joins_both_parts() {
        let req = GenerateRequest {
            instruction: Some("be brief"),
            requirement: Some("in english"),
            inputs: &[],
            model: "m",
            max_tokens: None,
            temperature: None,
            outputs: &[],
            options: &Default::default(),
        };
        assert_eq!(req.instruction_channel(), "be brief\n\nin english");
    }
}

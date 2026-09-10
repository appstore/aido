//! Application-level generation contracts. Wire formats stay in adapters.
mod chat;
mod media;
mod responses;
mod sse;
mod transport;

use crate::input::UserContent;
use anyhow::{bail, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
pub use transport::{normalize_base_url, Client};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum MediaMode {
    Text,
    Image,
    Audio,
}

impl std::fmt::Display for MediaMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::Audio => "audio",
        })
    }
}

/// Allowed input types and required members are separate contracts. An omitted
/// allow-list accepts every type supported by the adapter, not every type known
/// to the application. CLI input modes constrain detected data; they never cast it.
#[derive(Debug)]
pub struct Modes {
    pub inputs: Option<Vec<MediaMode>>,
    pub required: Vec<MediaMode>,
    pub outputs: Vec<MediaMode>,
}

impl Modes {
    pub fn validate(&self, adapter: Adapter) -> Result<()> {
        if self.inputs.as_ref().is_some_and(Vec::is_empty) || self.outputs.is_empty() {
            bail!("input_modes and output_modes must not be empty");
        }
        for mode in self.inputs.iter().flatten().chain(&self.required) {
            if !adapter.inputs().contains(mode) {
                bail!("adapter '{adapter}' does not support input mode '{mode}'");
            }
        }
        for mode in &self.outputs {
            if !adapter.outputs().contains(mode) {
                bail!("adapter '{adapter}' does not support output mode '{mode}'");
            }
        }
        for mode in &self.required {
            if self
                .inputs
                .as_ref()
                .is_some_and(|allowed| !allowed.contains(mode))
            {
                bail!("required input '{mode}' is excluded by input_modes / --input-mode");
            }
        }
        Ok(())
    }

    pub fn validate_input(&self, user: &UserContent, adapter: Adapter) -> Result<()> {
        let mut present = Vec::new();
        if user.text.is_some() {
            present.push(MediaMode::Text);
        }
        if !user.images.is_empty() {
            present.push(MediaMode::Image);
        }
        if !user.audios.is_empty() {
            present.push(MediaMode::Audio);
        }
        if present.is_empty()
            || (user.images.is_empty()
                && user.audios.is_empty()
                && user.text.as_ref().is_some_and(|s| s.trim().is_empty()))
        {
            bail!("no input content to send");
        }
        for mode in &present {
            if !adapter.inputs().contains(mode) {
                bail!("adapter '{adapter}' does not support input mode '{mode}'");
            }
            if self
                .inputs
                .as_ref()
                .is_some_and(|allowed| !allowed.contains(mode))
            {
                bail!("input contains '{mode}', which is excluded by input_modes / --input-mode");
            }
        }
        for mode in &self.required {
            if !present.contains(mode) {
                bail!("required input '{mode}' is missing");
            }
        }
        Ok(())
    }
}

pub struct GenerateRequest<'a> {
    pub system: Option<&'a str>,
    pub user: &'a UserContent,
    pub model: &'a str,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub outputs: &'a [MediaMode],
    pub options: &'a std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompletionStatus {
    #[default]
    Complete,
    Incomplete {
        reason: String,
    },
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GenerateResult {
    pub text: String,
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    pub status: CompletionStatus,
    pub warnings: Vec<String>,
}

impl GenerateResult {
    pub fn diagnostics(&self) -> Vec<String> {
        let mut warnings = self.warnings.clone();
        if let CompletionStatus::Incomplete { reason } = &self.status {
            warnings.push(match reason.as_str() {
                "length" | "max_output_tokens" => "reply hit the token limit and was truncated; raise --max-tokens / AIDO_MAX_TOKENS if text is missing".into(),
                "content_filter" => "reply was cut short by the server's content filter".into(),
                _ => format!("reply is incomplete: {reason}"),
            });
        }
        warnings
    }
}

impl Adapter {
    pub fn inputs(self) -> &'static [MediaMode] {
        use MediaMode::*;
        match self {
            Self::Chat | Self::Responses => &[Text, Image],
            Self::Speech | Self::Images => &[Text],
            Self::Transcription => &[Audio],
        }
    }
    pub fn outputs(self) -> &'static [MediaMode] {
        use MediaMode::*;
        match self {
            Self::Chat | Self::Transcription => &[Text],
            Self::Responses => &[Text, Image],
            Self::Speech => &[Audio],
            Self::Images => &[Image],
        }
    }
    pub fn default_outputs(self) -> Vec<MediaMode> {
        vec![match self {
            Self::Speech => MediaMode::Audio,
            Self::Images => MediaMode::Image,
            _ => MediaMode::Text,
        }]
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

#[derive(Debug, Serialize, Deserialize)]
pub struct Artifact {
    pub mode: MediaMode,
    pub mime: String,
    pub format: String,
    #[serde(with = "encoded_bytes")]
    pub bytes: Vec<u8>,
}
mod encoded_bytes {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(serde::de::Error::custom)
    }
}

impl Artifact {
    pub fn validate(&self) -> Result<()> {
        let formats: &[&str] = match self.mode {
            MediaMode::Image => &["png", "jpeg", "webp"],
            MediaMode::Audio => &["mp3", "opus", "aac", "flac", "wav", "pcm"],
            MediaMode::Text => bail!("text must not be stored as a binary artifact"),
        };
        if !formats.contains(&self.format.as_str()) || self.bytes.is_empty() {
            bail!("invalid {} artifact format or empty data", self.mode);
        }
        Ok(())
    }
}

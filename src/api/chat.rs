use super::transport::{error_message, truncate_chars, ApiErrorValue};
use super::{image_as_png, labeled_texts, GenerateRequest, GenerateResult};
use crate::domain::{GenerationStatus, InputPart, MediaKind};
use anyhow::{bail, Context, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    // Only sent for streaming runs, so buffered requests stay byte-identical.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
}

#[derive(Debug, Serialize)]
pub struct Message {
    pub role: &'static str,
    pub content: Content,
}

impl Message {
    fn system(text: impl Into<String>) -> Self {
        Self {
            role: "system",
            content: Content::Text(text.into()),
        }
    }

    fn user(content: Content) -> Self {
        Self {
            role: "user",
            content,
        }
    }
}

// OpenAI accepts either a plain string or an array of typed parts.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Parts(Vec<Part>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Serialize)]
pub struct ImageUrl {
    pub url: String,
}

/// Build the message list from the instruction channel and the ordered
/// material. The instruction and this run's requirement share the system
/// role (the chat protocol's instruction channel); material parts keep
/// their order and are never regrouped by type — a `text, image, text`
/// input goes on the wire in exactly that order.
pub fn build_messages(
    instruction: Option<&str>,
    requirement: Option<&str>,
    inputs: &[InputPart],
) -> Result<Vec<Message>> {
    if inputs
        .iter()
        .all(|p| p.kind != MediaKind::Text && p.kind != MediaKind::Image)
    {
        if inputs.iter().any(|p| p.kind == MediaKind::Audio) {
            bail!("the chat adapter does not take audio input");
        }
        // An instruction-only run is a valid request.
        let joined: Vec<&str> = [instruction, requirement]
            .into_iter()
            .flatten()
            .filter(|s| !s.trim().is_empty())
            .collect();
        if joined.is_empty() {
            bail!("no input content to send");
        }
        return Ok(vec![Message::user(Content::Text(joined.join("\n\n")))]);
    }
    let mut messages = Vec::new();
    let system: Vec<&str> = [instruction, requirement]
        .into_iter()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .collect();
    if !system.is_empty() {
        // Fixed instruction and -p are stored separately; the chat wire
        // has one system channel, so they are concatenated there.
        messages.push(Message::system(system.join("\n\n")));
    }
    let images = inputs.iter().any(|p| p.kind == MediaKind::Image);
    if !images {
        // Text-only: one user message; when several text parts are present
        // each carries its own label block, in order.
        let texts = labeled_texts(inputs);
        if texts.is_empty() {
            bail!("no input content to send");
        }
        let joined = texts.join("\n\n");
        messages.push(Message::user(Content::Text(joined)));
        return Ok(messages);
    }
    // With images the content must be a typed part array. Parts keep the
    // material's order, text and images interleaved as they were given.
    let texts = labeled_texts(inputs);
    let mut next_text = texts.into_iter();
    let mut parts: Vec<Part> = Vec::new();
    for part in inputs {
        match part.kind {
            MediaKind::Text => {
                if let Some(text) = next_text.next() {
                    parts.push(Part::Text { text });
                }
            }
            MediaKind::Image => parts.push(Part::ImageUrl {
                image_url: ImageUrl {
                    url: png_data_url(&image_as_png(part)?),
                },
            }),
            MediaKind::Audio => bail!("the chat adapter does not take audio input"),
        }
    }
    // Some servers reject image-only messages: at least one text part
    // must precede the images.
    if !parts.iter().any(|p| matches!(p, Part::Text { .. })) {
        let filler = if system.is_empty() {
            "Describe the attached image(s)."
        } else {
            "Process the attached image(s) according to the system instructions."
        };
        parts.insert(
            0,
            Part::Text {
                text: filler.to_string(),
            },
        );
    }
    messages.push(Message::user(Content::Parts(parts)));
    Ok(messages)
}

pub(super) fn png_data_url(png: &[u8]) -> String {
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png)
    )
}

/// One `data:` payload of a streaming reply: `delta` carries the next
/// token(s); `message` appears when a server ignores `stream: true` and
/// answers with an ordinary completion; `error` is how some gateways report
/// failures despite responding HTTP 200.
#[derive(Debug, Deserialize)]
struct StreamChunk {
    choices: Option<Vec<StreamChoice>>,
    error: Option<ApiErrorValue>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: Option<Delta>,
    message: Option<ChoiceMessage>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Delta {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletion {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ChoiceMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChoiceMessage {
    #[serde(default)]
    content: Option<String>,
}

pub(super) fn encode(spec: &GenerateRequest<'_>, stream: bool) -> Result<serde_json::Value> {
    Ok(serde_json::to_value(ChatRequest {
        model: spec.model.to_owned(),
        messages: build_messages(spec.instruction, spec.requirement, spec.inputs)?,
        max_tokens: spec.max_tokens,
        temperature: spec.temperature,
        stream,
    })?)
}

fn status(reason: Option<&str>) -> GenerationStatus {
    match reason {
        Some("length" | "content_filter") => GenerationStatus::Incomplete {
            reason: reason.unwrap().to_owned(),
        },
        _ => GenerationStatus::Complete,
    }
}

pub(super) fn parse(body: &str) -> Result<GenerateResult> {
    let parsed: ChatCompletion = serde_json::from_str(body)
        .with_context(|| format!("unexpected response format: {}", truncate_chars(body, 300)))?;
    let Some(choice) = parsed.choices.into_iter().next() else {
        bail!(
            "response contains no choices: {}",
            truncate_chars(body, 300)
        );
    };
    let mut result = GenerateResult {
        text: choice.message.content.unwrap_or_default(),
        status: status(choice.finish_reason.as_deref()),
        warnings: Vec::new(),
        artifacts: Vec::new(),
        request_index: 0, // the runner records which request this answers
    };
    result.note_incomplete();
    Ok(result)
}

#[derive(Default)]
pub(super) struct Stream {
    result: GenerateResult,
    finish_reason: Option<String>,
    full_message: bool,
    done: bool,
}

impl Stream {
    pub fn feed(&mut self, data: &str, on_delta: &mut impl FnMut(&str)) -> Result<bool> {
        if data.trim() == "[DONE]" {
            self.done = true;
            return Ok(true);
        }
        let chunk: StreamChunk = serde_json::from_str(data)
            .with_context(|| format!("unexpected SSE payload: {}", truncate_chars(data, 300)))?;
        if let Some(error) = chunk.error {
            bail!("API error in stream: {}", error_message(error));
        }
        if let Some(choice) = chunk.choices.into_iter().flatten().next() {
            if choice.finish_reason.is_some() {
                self.finish_reason = choice.finish_reason;
            }
            self.full_message |= choice.message.is_some();
            if let Some(content) = choice.delta.and_then(|d| d.content) {
                self.result.text.push_str(&content);
                if !content.is_empty() {
                    on_delta(&content);
                }
            }
            if let Some(snapshot) = choice.message.and_then(|m| m.content) {
                // A full message can repeat prior deltas or snapshots. Only
                // emit its missing suffix; already emitted text cannot change.
                let suffix = snapshot
                    .strip_prefix(&self.result.text)
                    .context("Chat snapshot disagrees with streamed output")?;
                if !suffix.is_empty() {
                    on_delta(suffix);
                }
                self.result.text = snapshot;
            }
        }
        Ok(false)
    }

    pub fn finish(mut self) -> Result<GenerateResult> {
        // Some compatible servers omit [DONE] but send finish_reason or a
        // complete message. A delta-only EOF is never a successful completion.
        if !self.done && self.finish_reason.is_none() && !self.full_message {
            bail!(
                "stream interrupted after {} chars: missing completion event",
                self.result.text.chars().count()
            );
        }
        self.result.status = status(self.finish_reason.as_deref());
        self.result.note_incomplete();
        Ok(self.result)
    }
}

#[cfg(test)]
mod tests;

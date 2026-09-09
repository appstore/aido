use super::transport::{error_message, truncate_chars, ApiErrorValue};
use super::{CompletionStatus, GenerateRequest, GenerateResult};
use crate::input::UserContent;
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

pub fn build_messages(system: Option<&str>, user: &UserContent) -> Result<Vec<Message>> {
    if user.text.is_none() && user.images.is_empty() {
        bail!("no input content to send");
    }
    let system = system.filter(|s| !s.trim().is_empty());
    let mut messages = Vec::new();
    if let Some(s) = system {
        messages.push(Message::system(s));
    }
    if user.images.is_empty() {
        if let Some(text) = &user.text {
            messages.push(Message::user(Content::Text(text.clone())));
        }
    } else {
        // A text part is included because some servers reject image-only messages.
        let text = user.text.clone().unwrap_or_else(|| {
            let noun = if user.images.len() > 1 {
                "images"
            } else {
                "image"
            };
            if system.is_some() {
                format!("Process the attached {noun} according to the system instructions.")
            } else {
                format!("Describe the attached {noun}.")
            }
        });
        let mut parts = vec![Part::Text { text }];
        parts.extend(user.images.iter().map(|png| Part::ImageUrl {
            image_url: ImageUrl {
                url: png_data_url(png),
            },
        }));
        messages.push(Message::user(Content::Parts(parts)));
    }
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
        messages: build_messages(spec.system, spec.user)?,
        max_tokens: spec.max_tokens,
        temperature: spec.temperature,
        stream,
    })?)
}

fn status(reason: Option<&str>) -> CompletionStatus {
    match reason {
        Some("length" | "content_filter") => CompletionStatus::Incomplete {
            reason: reason.unwrap().to_owned(),
        },
        _ => CompletionStatus::Complete,
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
    Ok(GenerateResult {
        text: choice.message.content.unwrap_or_default(),
        status: status(choice.finish_reason.as_deref()),
        warnings: Vec::new(),
        artifacts: Vec::new(),
    })
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
            if let Some(content) = choice
                .delta
                .and_then(|d| d.content)
                .or_else(|| choice.message.and_then(|m| m.content))
            {
                self.result.text.push_str(&content);
                if !content.is_empty() {
                    on_delta(&content);
                }
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
        Ok(self.result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            CompletionStatus::Incomplete {
                reason: "length".into()
            }
        );
    }
    #[test]
    fn empty_input_is_rejected() {
        assert!(build_messages(Some("hello"), &UserContent::default()).is_err());
    }
}

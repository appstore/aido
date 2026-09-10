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
/// their order and are never regrouped by type.
pub fn build_messages(
    instruction: Option<&str>,
    requirement: Option<&str>,
    inputs: &[InputPart],
) -> Result<Vec<Message>> {
    if inputs
        .iter()
        .all(|p| p.kind != MediaKind::Text && p.kind != MediaKind::Image)
    {
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
    if let Some(first) = system.first() {
        // Fixed instruction and -p are stored separately; the chat wire
        // has one system channel, so they are concatenated there.
        let joined = system.join("\n\n");
        messages.push(Message::system(joined));
        let _ = first;
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
    // With images the content must be a typed part array. A text part is
    // included because some servers reject image-only messages.
    let texts = labeled_texts(inputs);
    let text = if texts.is_empty() {
        let system = instruction.is_some_and(|s| !s.trim().is_empty());
        if system {
            "Process the attached image(s) according to the system instructions.".to_string()
        } else {
            "Describe the attached image(s).".to_string()
        }
    } else {
        texts.join("\n\n")
    };
    let mut parts = vec![Part::Text { text }];
    for part in inputs.iter().filter(|p| p.kind == MediaKind::Image) {
        parts.push(Part::ImageUrl {
            image_url: ImageUrl {
                url: png_data_url(&image_as_png(part)?),
            },
        });
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
    use crate::domain::{InputContent, InputSource};

    fn text_part(id: usize, s: &str) -> InputPart {
        InputPart {
            id,
            source: InputSource::Literal,
            name: format!("t{id}"),
            kind: MediaKind::Text,
            mime: "text/plain".into(),
            content: InputContent::Text(s.into()),
        }
    }

    fn png_part(id: usize) -> InputPart {
        InputPart {
            id,
            source: InputSource::File("a.png".into()),
            name: "a.png".into(),
            kind: MediaKind::Image,
            mime: "image/png".into(),
            content: InputContent::Media(vec![1, 2, 3]),
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
        // texts are folded into the leading text part, then the image part
        assert_eq!(content.len(), 2);
        let Part::Text { text } = &content[0] else {
            panic!("expected a text part first");
        };
        assert_eq!(text, "one\n\ntwo");
        assert!(matches!(content[1], Part::ImageUrl { .. }));
    }
}

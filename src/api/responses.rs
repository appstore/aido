use super::chat::png_data_url;
use super::{image_as_png, GenerateRequest, GenerateResult};
use crate::domain::{GenerationStatus, MediaKind};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

pub(super) fn encode(spec: &GenerateRequest<'_>, stream: bool) -> Result<Value> {
    let mut parts = Vec::new();
    for part in spec.inputs {
        match part.kind {
            MediaKind::Text => parts.push(json!({
                "type": "input_text",
                "text": part.text().unwrap_or_default()
            })),
            MediaKind::Image => parts.push(json!({
                "type": "input_image",
                "image_url": png_data_url(&image_as_png(part)?)
            })),
            MediaKind::Audio => bail!("the responses adapter does not take audio input"),
        }
    }
    // An instruction-only run (`ask -p ...` with no material) is a valid
    // request: the instruction is the whole user turn. It must not ALSO
    // go into the `instructions` field — the model would see it twice.
    let instruction_only = parts.is_empty();
    if instruction_only {
        let instructions = spec.instruction_channel();
        if instructions.is_empty() {
            bail!("no input content to send");
        }
        parts.push(json!({"type": "input_text", "text": instructions}));
    }
    let mut body = json!({
        "model": spec.model,
        "input": [{"role": "user", "content": parts}],
        "store": false,
    });
    if !instruction_only {
        let instructions = spec.instruction_channel();
        if !instructions.is_empty() {
            body["instructions"] = json!(instructions);
        }
    }
    if let Some(tokens) = spec.max_tokens {
        body["max_output_tokens"] = json!(tokens);
    }
    if let Some(temperature) = spec.temperature {
        body["temperature"] = json!(temperature);
    }
    if spec.outputs.contains(&MediaKind::Image) {
        let mut tool = json!({"type": "image_generation"});
        for (key, value) in spec.options {
            tool[if key == "format" {
                "output_format"
            } else {
                key
            }] = value.clone();
        }
        body["tools"] = json!([tool]);
        body["tool_choice"] = json!({"type": "image_generation"});
    }
    if stream {
        body["stream"] = json!(true);
    }
    Ok(body)
}

#[derive(Deserialize)]
struct Response {
    status: String,
    #[serde(default)]
    output: Vec<Item>,
    error: Option<Value>,
    incomplete_details: Option<Value>,
}
#[derive(Deserialize)]
struct Item {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    content: Vec<Content>,
    result: Option<String>,
}
#[derive(Deserialize)]
struct Content {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
    refusal: Option<String>,
}

pub(super) fn parse(body: &str) -> Result<GenerateResult> {
    let response: Response =
        serde_json::from_str(body).context("unexpected Responses response format")?;
    let mut result = GenerateResult::default();
    match response.status.as_str() {
        "completed" => result.status = GenerationStatus::Complete,
        "incomplete" => {
            result.status = GenerationStatus::Incomplete {
                reason: response
                    .incomplete_details
                    .as_ref()
                    .and_then(|d| d["reason"].as_str())
                    .unwrap_or("unknown")
                    .into(),
            };
        }
        "failed" | "cancelled" => bail!(
            "Responses request {}: {}",
            response.status,
            response.error.unwrap_or(Value::Null)
        ),
        status => bail!("Responses request ended without a terminal status: {status}"),
    }
    if let Some(error) = response.error {
        bail!("Responses API error: {error}");
    }
    for item in response.output {
        match item.kind.as_str() {
            "message" => {
                for part in item.content {
                    match part.kind.as_str() {
                        "output_text" => result.text.push_str(
                            part.text
                                .as_deref()
                                .context("output_text is missing text")?,
                        ),
                        "refusal" => {
                            result.text.push_str(
                                part.refusal
                                    .as_deref()
                                    .context("refusal is missing its explanation")?,
                            );
                            warn(&mut result, "model refused the request".into());
                        }
                        kind => warn(
                            &mut result,
                            format!("unsupported output content '{kind}' was not rendered"),
                        ),
                    }
                }
            }
            "reasoning" => {}
            "image_generation_call" => {
                if let Some(encoded) = item.result {
                    result.artifacts.push(super::media::image(&encoded)?);
                } else if result.status == GenerationStatus::Complete {
                    bail!("image generation result is missing");
                }
            }
            kind => warn(
                &mut result,
                format!("unsupported output item '{kind}' was not rendered"),
            ),
        }
    }
    if result.text.is_empty()
        && result.artifacts.is_empty()
        && result.warnings.iter().any(|w| w.starts_with("unsupported"))
    {
        bail!(
            "response contains unsupported output and no text: {}",
            result.warnings.join("; ")
        );
    }
    result.note_incomplete();
    Ok(result)
}

fn warn(result: &mut GenerateResult, warning: String) {
    if !result.warnings.contains(&warning) {
        result.warnings.push(warning);
    }
}

#[derive(Default)]
pub(super) struct Stream {
    text: String,
    result: Option<GenerateResult>,
}
impl Stream {
    pub fn feed(&mut self, data: &str, on_delta: &mut impl FnMut(&str)) -> Result<bool> {
        let event: Value =
            serde_json::from_str(data).context("unexpected Responses SSE payload")?;
        let kind = event["type"]
            .as_str()
            .context("Responses SSE event is missing type")?;
        match kind {
            "response.output_text.delta" | "response.refusal.delta" => {
                let delta = event["delta"]
                    .as_str()
                    .context("Responses delta is missing text")?;
                self.text.push_str(delta);
                if !delta.is_empty() {
                    on_delta(delta);
                }
            }
            "response.completed" | "response.incomplete" => {
                let result = parse(&event["response"].to_string())?;
                let expected = if kind == "response.completed" {
                    "completed"
                } else {
                    "incomplete"
                };
                if event["response"]["status"].as_str() != Some(expected) {
                    bail!("Responses terminal event disagrees with response status");
                }
                // The final object repeats the text already streamed. Emit only
                // a missing suffix, never the full snapshot a second time.
                let suffix = result
                    .text
                    .strip_prefix(&self.text)
                    .context("Responses final text disagrees with streamed output")?;
                if !suffix.is_empty() {
                    on_delta(suffix);
                }
                self.result = Some(result);
                return Ok(true);
            }
            "response.failed" | "error" => bail!("Responses API error in stream: {event}"),
            _ => {} // Progress and future nonterminal events carry no rendered text.
        }
        Ok(false)
    }
    pub fn finish(self) -> Result<GenerateResult> {
        self.result.with_context(|| {
            format!(
                "stream interrupted after {} chars: missing Responses completion event",
                self.text.chars().count()
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_and_multiple_messages_preserve_text() {
        let body = json!({"status":"completed","output":[{"type":"reasoning"},{"type":"message","content":[{"type":"output_text","text":"one"},{"type":"refusal","refusal":"two"}]},{"type":"message","content":[{"type":"output_text","text":"three"}]}]});
        let result = parse(&body.to_string()).unwrap();
        assert_eq!(result.text, "onetwothree");
        assert_eq!(result.warnings.len(), 1);
    }
    #[test]
    fn failed_and_unknown_statuses_are_errors() {
        for status in ["failed", "cancelled", "in_progress", "unknown"] {
            assert!(parse(&json!({"status":status,"output":[]}).to_string()).is_err());
        }
    }
    #[test]
    fn terminal_snapshot_is_not_duplicated() {
        let mut stream = Stream::default();
        let mut text = String::new();
        let mut emit = |delta: &str| text.push_str(delta);
        stream
            .feed(
                r#"{"type":"response.output_text.delta","delta":"你"}"#,
                &mut emit,
            )
            .unwrap();
        let terminal = json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"你好"}]}]}});
        assert!(stream.feed(&terminal.to_string(), &mut emit).unwrap());
        assert_eq!(stream.finish().unwrap().text, "你好");
        assert_eq!(text, "你好");
    }
    #[test]
    fn eof_and_stream_errors_are_not_success() {
        let mut stream = Stream::default();
        stream
            .feed(
                r#"{"type":"response.output_text.delta","delta":"partial"}"#,
                &mut |_| {},
            )
            .unwrap();
        assert!(stream.finish().is_err());
        for event in ["response.failed", "error"] {
            assert!(Stream::default()
                .feed(&json!({"type":event}).to_string(), &mut |_| {})
                .is_err());
        }
    }
}

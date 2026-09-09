use crate::config::Resolved;
use crate::input::UserContent;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
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

fn png_data_url(png: &[u8]) -> String {
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png)
    )
}

/// Trim trailing slashes; append `/v1` when the URL has no path at all
/// (the convention for vLLM / SGLang / llama.cpp / Ollama / LM Studio).
pub fn normalize_base_url(input: &str) -> String {
    let s = input.trim().trim_end_matches('/');
    let path_start = s.find("://").map(|i| i + 3).unwrap_or(0);
    if s[path_start..].contains('/') {
        s.to_string()
    } else {
        format!("{s}/v1")
    }
}

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    timeout_secs: u64,
}

impl Client {
    pub fn new(resolved: &Resolved) -> Result<Self> {
        // No client-wide timeout: buffered requests get a per-request total
        // timeout, while a stream must never be cut off mid-reply and applies
        // the timeout to each read instead.
        let http = reqwest::Client::builder()
            .user_agent(concat!("aido/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            http,
            base_url: resolved.base_url.clone(),
            api_key: resolved.api_key.clone(),
            timeout_secs: resolved.timeout_secs,
        })
    }

    fn post(&self, request: &ChatRequest) -> reqwest::RequestBuilder {
        let url = format!("{}/chat/completions", self.base_url);
        let mut req = self.http.post(&url).json(request);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        req
    }

    pub async fn chat(&self, request: &ChatRequest) -> Result<String> {
        let resp = self
            .post(request)
            .timeout(Duration::from_secs(self.timeout_secs))
            .send()
            .await
            .context("request failed")?;
        let status = resp.status();
        let body = resp.text().await.context("failed to read response body")?;

        if !status.is_success() {
            return Err(api_error(status, &body));
        }
        parse_completion(&body)
    }

    /// Like [`Client::chat`] with `stream: true`: every content delta goes
    /// to `on_delta` as it arrives, and the full reply is also returned.
    /// The timeout bounds the wait for the response headers and each gap
    /// between bytes — never the whole reply, which may legitimately take
    /// longer than the total limit. A response that is not an event
    /// stream — a server that ignored `stream: true` — is parsed as an
    /// ordinary completion and delivered to `on_delta` in one piece.
    pub async fn chat_stream(
        &self,
        request: &ChatRequest,
        mut on_delta: impl FnMut(&str),
    ) -> Result<String> {
        // Bound the connect + headers wait exactly like the buffered path
        // does; only the streamed body may take as long as it needs.
        let mut resp = match tokio::time::timeout(
            Duration::from_secs(self.timeout_secs),
            self.post(request).send(),
        )
        .await
        {
            Err(_) => bail!(
                "timed out after {}s waiting for response headers",
                self.timeout_secs
            ),
            Ok(r) => r.context("request failed")?,
        };
        let status = resp.status();
        if !status.is_success() {
            // Error responses are ordinary bodies, not event streams.
            let body = resp.text().await.unwrap_or_default();
            return Err(api_error(status, &body));
        }

        let idle = Duration::from_secs(self.timeout_secs);
        // The decoder would drop a non-`data:` line, so a server that
        // ignored `stream: true` must be parsed as an ordinary completion.
        if !is_event_stream(&resp) {
            let body = match tokio::time::timeout(idle, resp.text()).await {
                Err(_) => bail!(
                    "timed out after {}s reading response body",
                    self.timeout_secs
                ),
                Ok(r) => r.context("failed to read response body")?,
            };
            let content = parse_completion(&body)?;
            if !content.is_empty() {
                on_delta(&content);
            }
            return Ok(content);
        }

        let mut decoder = SseDecoder::new();
        let mut text = String::new();
        let mut finish_reason: Option<String> = None;
        let mut done = false;
        loop {
            let chunk = match tokio::time::timeout(idle, resp.chunk()).await {
                Err(_) => {
                    return Err(interrupted(
                        &text,
                        format!("no stream data for {}s", self.timeout_secs),
                    ))
                }
                Ok(Err(e)) => return Err(interrupted(&text, e)),
                Ok(Ok(None)) => break,
                Ok(Ok(Some(bytes))) => bytes,
            };
            for event in decoder.feed(&chunk) {
                if apply_event(event, &mut text, &mut finish_reason, &mut on_delta)? {
                    done = true;
                    break;
                }
            }
        }
        if !done {
            for event in decoder.finish() {
                apply_event(event, &mut text, &mut finish_reason, &mut on_delta)?;
            }
        }
        warn_finish_reason(finish_reason.as_deref());
        Ok(text)
    }
}

/// Parse an ordinary (non-streamed) chat completion body into its text.
fn parse_completion(body: &str) -> Result<String> {
    let parsed: ChatCompletion = serde_json::from_str(body)
        .with_context(|| format!("unexpected response format: {}", truncate_chars(body, 300)))?;
    let Some(choice) = parsed.choices.into_iter().next() else {
        bail!(
            "response contains no choices: {}",
            truncate_chars(body, 300)
        );
    };
    warn_finish_reason(choice.finish_reason.as_deref());
    Ok(choice.message.content.unwrap_or_default())
}

/// Whether a response is `text/event-stream` (parameters after `;`
/// tolerated), i.e. what `stream: true` should produce.
fn is_event_stream(resp: &reqwest::Response) -> bool {
    resp.headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().contains("text/event-stream"))
}

/// Fail a broken stream, telling the user how much already arrived.
fn interrupted(text: &str, cause: impl std::fmt::Display) -> anyhow::Error {
    let chars = text.chars().count();
    if chars > 0 {
        anyhow!("stream interrupted after {chars} chars: {cause}")
    } else {
        anyhow!("stream failed before any output: {cause}")
    }
}

/// Apply one decoded event, appending content to `text` and reporting
/// deltas through `on_delta`; returns true for the terminating `Done`.
fn apply_event(
    event: SseEvent,
    text: &mut String,
    finish_reason: &mut Option<String>,
    on_delta: &mut impl FnMut(&str),
) -> Result<bool> {
    let data = match event {
        SseEvent::Done => return Ok(true),
        SseEvent::Data(data) => data,
    };
    let chunk: StreamChunk = serde_json::from_str(&data)
        .with_context(|| format!("unexpected SSE payload: {}", truncate_chars(&data, 300)))?;
    if let Some(err) = chunk.error {
        bail!("API error in stream: {}", error_message(err));
    }
    if let Some(choice) = chunk.choices.into_iter().flatten().next() {
        if choice.finish_reason.is_some() {
            *finish_reason = choice.finish_reason;
        }
        // Tokens arrive in `delta`; a full `message` means the server
        // ignored `stream: true` and answered with an ordinary completion —
        // accept it instead of reporting a mysterious empty reply.
        let content = choice
            .delta
            .and_then(|d| d.content)
            .or_else(|| choice.message.and_then(|m| m.content));
        if let Some(content) = content.filter(|c| !c.is_empty()) {
            text.push_str(&content);
            on_delta(&content);
        }
    }
    Ok(false)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}...")
    }
}

fn api_error(status: reqwest::StatusCode, body: &str) -> anyhow::Error {
    if let Ok(err) = serde_json::from_str::<ApiErrorWrapper>(body) {
        return anyhow!("API error (HTTP {status}): {}", error_message(err.error));
    }
    anyhow!("API error (HTTP {status}): {}", truncate_chars(body, 300))
}

fn error_message(value: ApiErrorValue) -> String {
    match value {
        ApiErrorValue::Object {
            message: Some(serde_json::Value::String(s)),
        } => s,
        ApiErrorValue::Object { message: Some(v) } => v.to_string(),
        ApiErrorValue::Object { message: None } => "(no message)".into(),
        ApiErrorValue::Text(s) => s,
    }
}

fn warn_finish_reason(reason: Option<&str>) {
    match reason {
        // A reply that hit a limit looks like a successful, merely short
        // answer — surface it instead of silently losing the tail.
        Some("length") => eprintln!(
            "warning: reply hit the token limit and was truncated; \
             raise --max-tokens / AIDO_MAX_TOKENS if text is missing"
        ),
        Some("content_filter") => {
            eprintln!("warning: reply was cut short by the server's content filter");
        }
        _ => {}
    }
}

// ---- streaming (SSE) ----

/// One decoded `text/event-stream` event.
#[derive(Debug, PartialEq, Eq)]
pub enum SseEvent {
    /// A complete `data:` payload (multi-line data joined with `\n`).
    Data(String),
    /// The `data: [DONE]` sentinel.
    Done,
}

/// Incremental `text/event-stream` decoder: raw bytes in — chunk boundaries
/// may fall anywhere, including inside multi-byte UTF-8 characters — and
/// events out on line boundaries. Lines are terminated by `\n` (optionally
/// preceded by `\r`), which is what every OpenAI-compatible server emits;
/// the spec's bare-`\r` form is not recognized and would buffer until EOF.
#[derive(Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
    data: Vec<String>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Consume raw bytes; returns every event completed by a line break.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            if let Some(ev) = self.line(&line[..line.len() - 1]) {
                events.push(ev);
            }
        }
        events
    }

    /// Flush at end of stream: a final line without a newline, plus any
    /// event whose blank-line terminator never arrived.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            if let Some(ev) = self.line(&line) {
                events.push(ev);
            }
        }
        if !self.data.is_empty() {
            let joined = std::mem::take(&mut self.data).join("\n");
            events.push(Self::event(joined));
        }
        events
    }

    /// One complete line, its newline already stripped; may still end in CR.
    fn line(&mut self, line: &[u8]) -> Option<SseEvent> {
        let line = match line.split_last() {
            Some((&b'\r', rest)) => rest,
            _ => line,
        };
        // A blank line terminates the event; `:`-leading lines are comments
        // (keep-alives); the other fields (`event:`, `id:`, `retry:`) carry
        // no content.
        if line.is_empty() {
            return if self.data.is_empty() {
                None
            } else {
                let joined = std::mem::take(&mut self.data).join("\n");
                Some(Self::event(joined))
            };
        }
        if line[0] == b':' {
            return None;
        }
        if let Some(value) = line.strip_prefix(b"data:".as_slice()) {
            let value = value.strip_prefix(b" ".as_slice()).unwrap_or(value);
            // Lines are complete here, and \n never appears inside a
            // multi-byte sequence, so lossy decoding never replaces bytes.
            self.data.push(String::from_utf8_lossy(value).into_owned());
        }
        None
    }

    fn event(data: String) -> SseEvent {
        if data.trim() == "[DONE]" {
            SseEvent::Done
        } else {
            SseEvent::Data(data)
        }
    }
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

#[derive(Debug, Deserialize)]
struct ApiErrorWrapper {
    error: ApiErrorValue,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ApiErrorValue {
    Object { message: Option<serde_json::Value> },
    Text(String),
}

#[cfg(test)]
mod tests {
    use super::{
        apply_event, build_messages, normalize_base_url, truncate_chars, SseDecoder, SseEvent,
    };
    use crate::input::UserContent;

    #[test]
    fn rejects_empty_user_content() {
        let user = UserContent {
            text: None,
            images: Vec::new(),
        };
        assert!(build_messages(None, &user).is_err());
        assert!(build_messages(Some("do something"), &user).is_err());
    }

    #[test]
    fn normalizes_base_urls() {
        assert_eq!(
            normalize_base_url("http://localhost:30000"),
            "http://localhost:30000/v1"
        );
        assert_eq!(
            normalize_base_url("http://localhost:30000/"),
            "http://localhost:30000/v1"
        );
        assert_eq!(
            normalize_base_url("http://localhost:30000/v1"),
            "http://localhost:30000/v1"
        );
        assert_eq!(
            normalize_base_url("http://localhost:30000/v1/"),
            "http://localhost:30000/v1"
        );
        assert_eq!(
            normalize_base_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1"
        );
        // an explicit path is kept as-is (user knows their proxy layout)
        assert_eq!(
            normalize_base_url("https://gw.example.com/proxy"),
            "https://gw.example.com/proxy"
        );
        assert_eq!(normalize_base_url("localhost:8080"), "localhost:8080/v1");
    }

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        let long = "你好世界".repeat(10);
        let cut = truncate_chars(&long, 6);
        assert_eq!(cut, "你好世界你好...");
    }

    #[test]
    fn sse_decoder_reassembles_any_chunk_split() {
        // Splits at every byte offset — including inside the three-byte 你 —
        // must reassemble to the same single event.
        let full = "data: {\"delta\":{\"content\":\"你\"}}\n\n";
        for split_at in 0..full.len() {
            let mut d = SseDecoder::new();
            let mut events = d.feed(&full.as_bytes()[..split_at]);
            events.extend(d.feed(&full.as_bytes()[split_at..]));
            events.extend(d.finish());
            assert_eq!(
                events,
                vec![SseEvent::Data(r#"{"delta":{"content":"你"}}"#.to_string())],
                "split at byte {split_at}"
            );
        }
    }

    #[test]
    fn sse_decoder_handles_comments_and_done() {
        let mut d = SseDecoder::new();
        let events = d.feed(
            concat!(
                ": keep-alive\n",
                "event: ping\n\n",
                "data: first\n",
                "data: second\n\n",
                "data: [DONE]\n\n",
            )
            .as_bytes(),
        );
        assert_eq!(
            events,
            vec![SseEvent::Data("first\nsecond".to_string()), SseEvent::Done,]
        );
        assert!(d.finish().is_empty());
    }

    #[test]
    fn sse_decoder_flushes_unterminated_event() {
        // A server that closes without the blank line or [DONE] still gets
        // its last data line through.
        let mut d = SseDecoder::new();
        assert!(d.feed(b"data: {\"x\":1}").is_empty());
        assert_eq!(d.finish(), vec![SseEvent::Data("{\"x\":1}".to_string())]);
    }

    #[test]
    fn stream_chunks_extract_deltas_and_finish_reason() {
        let mut text = String::new();
        let mut finish = None;
        let mut seen = Vec::new();
        let mut on = |s: &str| seen.push(s.to_string());
        let role_chunk =
            r#"{"choices":[{"delta":{"role":"assistant"},"finish_reason":null,"index":0}]}"#;
        assert!(!apply_event(
            SseEvent::Data(role_chunk.to_string()),
            &mut text,
            &mut finish,
            &mut on
        )
        .unwrap());
        for payload in [
            r#"{"choices":[{"delta":{"content":"你"},"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{"content":"好"},"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#,
        ] {
            apply_event(
                SseEvent::Data(payload.to_string()),
                &mut text,
                &mut finish,
                &mut on,
            )
            .unwrap();
        }
        assert_eq!(text, "你好");
        assert_eq!(seen.join(""), "你好");
        assert_eq!(finish.as_deref(), Some("length"));
    }

    #[test]
    fn stream_tolerates_choiceless_and_null_payloads() {
        // usage-only final chunks and null choices carry no content
        let mut text = String::new();
        let mut finish = None;
        let mut on = |_s: &str| {};
        for payload in [
            r#"{"choices":[],"usage":{"total_tokens":10}}"#,
            r#"{"choices":null}"#,
        ] {
            assert!(!apply_event(
                SseEvent::Data(payload.to_string()),
                &mut text,
                &mut finish,
                &mut on
            )
            .unwrap());
        }
        assert!(text.is_empty());
    }

    #[test]
    fn stream_accepts_a_non_streamed_reply() {
        // A server that ignores stream:true answers with an ordinary
        // completion body (one line, no [DONE]); its content must come
        // through rather than an empty reply.
        let mut text = String::new();
        let mut finish = None;
        let mut on = |_s: &str| {};
        let payload =
            r#"{"choices":[{"message":{"content":"whole reply"},"finish_reason":"stop"}]}"#;
        apply_event(
            SseEvent::Data(payload.to_string()),
            &mut text,
            &mut finish,
            &mut on,
        )
        .unwrap();
        assert_eq!(text, "whole reply");
    }

    #[test]
    fn stream_error_payload_becomes_an_error() {
        let mut text = String::new();
        let mut finish = None;
        let mut on = |_s: &str| {};
        let payload = r#"{"error":{"message":"rate limited"}}"#;
        let err = apply_event(
            SseEvent::Data(payload.to_string()),
            &mut text,
            &mut finish,
            &mut on,
        )
        .unwrap_err();
        assert!(err.to_string().contains("rate limited"), "was: {err}");
    }
}

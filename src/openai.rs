use crate::config::Resolved;
use crate::input::UserContent;
use anyhow::{bail, Context, Result};
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
}

impl Client {
    pub fn new(resolved: &Resolved) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(resolved.timeout_secs))
            .user_agent(concat!("aido/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            http,
            base_url: resolved.base_url.clone(),
            api_key: resolved.api_key.clone(),
        })
    }

    pub async fn chat(&self, request: &ChatRequest) -> Result<String> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut req = self.http.post(&url).json(request);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await.context("request failed")?;
        let status = resp.status();
        let body = resp.text().await.context("failed to read response body")?;

        if !status.is_success() {
            if let Ok(err) = serde_json::from_str::<ApiErrorWrapper>(&body) {
                let msg = match err.error {
                    ApiErrorValue::Object {
                        message: Some(serde_json::Value::String(s)),
                    } => s,
                    ApiErrorValue::Object { message: Some(v) } => v.to_string(),
                    ApiErrorValue::Object { message: None } => "(no message)".into(),
                    ApiErrorValue::Text(s) => s,
                };
                bail!("API error (HTTP {status}): {msg}");
            }
            bail!("API error (HTTP {status}): {}", truncate_chars(&body, 300));
        }

        let parsed: ChatCompletion = serde_json::from_str(&body).with_context(|| {
            format!("unexpected response format: {}", truncate_chars(&body, 300))
        })?;
        let Some(choice) = parsed.choices.into_iter().next() else {
            bail!(
                "response contains no choices: {}",
                truncate_chars(&body, 300)
            );
        };
        Ok(choice.message.content.unwrap_or_default())
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}...")
    }
}

#[derive(Debug, Deserialize)]
struct ChatCompletion {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ChoiceMessage,
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
    use super::{build_messages, normalize_base_url};
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
}

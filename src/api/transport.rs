#[cfg(feature = "edge-tts")]
use super::edge;
#[cfg(feature = "local-asr")]
use super::local_asr::{self, LocalModels};
use super::{chat, media, responses, sse::SseDecoder, Adapter, GenerateRequest, GenerateResult};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

pub(super) fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}...")
    }
}

pub(super) fn api_error(status: reqwest::StatusCode, body: &str) -> anyhow::Error {
    if let Ok(err) = serde_json::from_str::<ApiErrorWrapper>(body) {
        return anyhow!("API error (HTTP {status}): {}", error_message(err.error));
    }
    anyhow!("API error (HTTP {status}): {}", truncate_chars(body, 300))
}

pub(super) fn error_message(value: ApiErrorValue) -> String {
    match value {
        ApiErrorValue::Object {
            message: Some(serde_json::Value::String(s)),
        } => s,
        ApiErrorValue::Object { message: Some(v) } => v.to_string(),
        ApiErrorValue::Object { message: None } => "(no message)".into(),
        ApiErrorValue::Text(s) => s,
    }
}

#[derive(Debug, Deserialize)]
struct ApiErrorWrapper {
    error: ApiErrorValue,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(super) enum ApiErrorValue {
    Object { message: Option<serde_json::Value> },
    Text(String),
}

/// Keep proxy paths and query parameters; only a root path gets /v1.
pub fn normalize_base_url(input: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(input.trim()).context("invalid base URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("base URL must start with http:// or https:// (got '{input}')");
    }
    if url.fragment().is_some() {
        bail!("base URL must not contain a fragment");
    }
    let path = url.path().trim_end_matches('/');
    let path = if path.is_empty() { "/v1" } else { path }.to_owned();
    url.set_path(&path);
    Ok(url.into())
}

/// The cleartext hint, when a run would put its API key on the wire
/// unencrypted: base_url is plain http, a key is configured, and the host
/// is not this machine. Local inference servers are legitimately http, so
/// loopback hosts are exempt. Shared with the `--dry-run` plan, which
/// never builds a client and must hint at the same exposure.
pub fn cleartext_key_warning(base_url: Option<&str>, api_key_present: bool) -> Option<String> {
    if !api_key_present {
        return None;
    }
    let url = reqwest::Url::parse(base_url?).ok()?;
    if url.scheme() != "http" || is_loopback_host(&url) {
        return None;
    }
    Some(format!(
        "credentials will be sent in cleartext to {} (base_url uses http://)",
        url.host_str().unwrap_or("(unknown host)")
    ))
}

/// Loopback hosts: the `localhost` name and any loopback IPv4/IPv6
/// address — the whole 127.0.0.0/8 block and ::1. The URL serializer
/// hands IPv6 literals over with their brackets (`[::1]`).
fn is_loopback_host(url: &reqwest::Url) -> bool {
    match url.host_str() {
        Some(host) => {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }
        None => false,
    }
}

/// Everything the client needs to reach a service. Credentials are
/// resolved by the caller at send time and never logged. `base_url` is
/// `None` for adapters that own their endpoint (edge-tts, local-asr).
pub struct Connection {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    /// Local model files, consumed only by the local-asr adapter — always
    /// set when `adapter` is LocalAsr (resolve() builds them), None
    /// otherwise. The field lives only in feature-on builds.
    #[cfg(feature = "local-asr")]
    pub models: Option<LocalModels>,
    pub timeout: Duration,
    /// Whether `timeout` is the user's explicit budget (see
    /// ExecutionPlan::timeout_explicit): the local-asr adapter treats it
    /// as a hard wall instead of stretching with the audio length. The
    /// field lives only in feature-on builds.
    #[cfg(feature = "local-asr")]
    pub timeout_explicit: bool,
    /// Whole-run budget. Consumed today only by the edge-tts adapter (it
    /// bounds all chunks of one synthesis); HTTP adapters enforce only the
    /// per-request `timeout`.
    pub total_timeout: Option<Duration>,
    pub adapter: Adapter,
}

pub struct Client {
    http: reqwest::Client,
    base_url: Option<reqwest::Url>,
    api_key: Option<String>,
    /// Local model files, consumed only by the local-asr adapter — always
    /// set when `adapter` is LocalAsr. The field lives only in feature-on
    /// builds.
    #[cfg(feature = "local-asr")]
    models: Option<LocalModels>,
    timeout: Duration,
    /// Consumed only by the local-asr adapter (hard wall vs. stretch) —
    /// the field lives only in feature-on builds.
    #[cfg(feature = "local-asr")]
    timeout_explicit: bool,
    /// Whole-run budget. Consumed today only by the edge-tts adapter (it
    /// bounds all chunks of one synthesis); HTTP adapters enforce only the
    /// per-request `timeout` — so the field lives only in feature-on builds.
    #[cfg(feature = "edge-tts")]
    total_timeout: Option<Duration>,
    adapter: Adapter,
}

/// The rustls config handed to reqwest. Since 0.13 the `rustls` feature
/// wires in the aws-lc-rs provider (the 0.13 default) and platform
/// certificate verification; aido instead builds the config itself — ring
/// as the crypto provider (the provider the edge-tts stack runs with at
/// runtime, and what keeps aws-lc out of `--no-default-features` builds)
/// over the webpki-roots store, matching what the 0.12 `rustls-tls`
/// feature provided. The config carries no ALPN protocols, so no HTTP/2
/// is ever negotiated — same as before the upgrade; enabling reqwest's
/// http2 feature alone would not change that with a preconfigured config.
fn rustls_client_config() -> Result<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth())
}

impl Client {
    pub fn new(conn: &Connection) -> Result<Self> {
        let base_url = match conn.adapter {
            Adapter::EdgeTts | Adapter::LocalAsr => None,
            _ => Some(reqwest::Url::parse(
                conn.base_url
                    .as_deref()
                    .context("provider has no base URL")?,
            )?),
        };
        // One non-blocking hint per run: a key over plain http off this
        // machine would leave in cleartext.
        if let Some(warning) = cleartext_key_warning(
            base_url.as_ref().map(reqwest::Url::as_str),
            conn.api_key.is_some(),
        ) {
            eprintln!("warning: {warning}");
        }
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent(concat!("aido/", env!("CARGO_PKG_VERSION")))
                .tls_backend_preconfigured(rustls_client_config()?)
                .build()
                .context("failed to build HTTP client")?,
            base_url,
            api_key: conn.api_key.clone(),
            #[cfg(feature = "local-asr")]
            models: conn.models.clone(),
            timeout: conn.timeout,
            #[cfg(feature = "local-asr")]
            timeout_explicit: conn.timeout_explicit,
            #[cfg(feature = "edge-tts")]
            total_timeout: conn.total_timeout,
            adapter: conn.adapter,
        })
    }

    fn post(&self, request: &GenerateRequest<'_>, stream: bool) -> Result<reqwest::RequestBuilder> {
        let path = match self.adapter {
            Adapter::Chat => "chat/completions",
            Adapter::Responses => "responses",
            Adapter::Speech => "audio/speech",
            Adapter::Transcription => "audio/transcriptions",
            Adapter::Images => "images/generations",
            Adapter::EdgeTts | Adapter::LocalAsr => {
                bail!("the {} adapter does not use HTTP requests", self.adapter)
            }
        };
        let base = self
            .base_url
            .as_ref()
            .context("adapter has no HTTP base URL")?;
        let mut url = base.clone();
        url.set_path(&format!("{}/{path}", url.path().trim_end_matches('/')));
        let req = self.http.post(url);
        let mut req = match self.adapter {
            Adapter::Chat => req.json(&chat::encode(request, stream)?),
            Adapter::Responses => req.json(&responses::encode(request, stream)?),
            Adapter::Transcription => req.multipart(media::transcription(request)?),
            Adapter::Speech => req.json(&media::encode_speech(request)?),
            Adapter::Images => req.json(&media::encode_images(request)?),
            Adapter::EdgeTts | Adapter::LocalAsr => {
                bail!("the {} adapter does not use HTTP requests", self.adapter)
            }
        };
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        Ok(req)
    }

    fn parse(&self, body: &str) -> Result<GenerateResult> {
        match self.adapter {
            Adapter::Chat => chat::parse(body),
            Adapter::Responses => responses::parse(body),
            Adapter::Transcription => media::parse_transcription(body),
            _ => bail!("binary adapter cannot parse a text response"),
        }
    }

    pub async fn generate(&self, request: &GenerateRequest<'_>) -> Result<GenerateResult> {
        if self.adapter == Adapter::EdgeTts {
            // The variant stays compiled without the feature so a config
            // naming 'edge-tts' still parses; only the adapter is gone.
            #[cfg(feature = "edge-tts")]
            {
                return edge::synthesize(request, self.timeout, self.total_timeout).await;
            }
            #[cfg(not(feature = "edge-tts"))]
            {
                bail!(super::EDGE_TTS_NOT_COMPILED);
            }
        }
        if self.adapter == Adapter::LocalAsr {
            // Same stays-compiled story as edge-tts: a config naming
            // 'local-asr' parses in any build, only the engine is absent.
            // resolve() guarantees models for this adapter, so the expect
            // is an invariant assertion, not input handling.
            #[cfg(feature = "local-asr")]
            {
                let models = self
                    .models
                    .as_ref()
                    .expect("local-asr adapter without resolved models");
                return local_asr::transcribe(request, self.timeout, self.timeout_explicit, models)
                    .await;
            }
            #[cfg(not(feature = "local-asr"))]
            {
                bail!(super::LOCAL_ASR_NOT_COMPILED);
            }
        }
        let resp = self
            .post(request, false)?
            .timeout(self.timeout)
            .send()
            .await
            .context("request failed")?;
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let bytes = read_body(resp).await?;
        if !status.is_success() {
            return Err(api_error(status, &String::from_utf8_lossy(&bytes)));
        }
        if self.adapter == Adapter::Speech {
            let format = request
                .options
                .get("format")
                .and_then(|v| v.as_str())
                .unwrap_or("mp3");
            return media::speech(bytes, format, &content_type);
        }
        let body = std::str::from_utf8(&bytes).context("expected a JSON response")?;
        if self.adapter == Adapter::Images {
            return self.parse_images(body).await;
        }
        self.parse(body)
    }

    async fn parse_images(&self, body: &str) -> Result<GenerateResult> {
        let body: serde_json::Value =
            serde_json::from_str(body).context("unexpected image response")?;
        let items = body["data"]
            .as_array()
            .context("image response is missing data")?;
        if items.is_empty() {
            bail!("image response contains no images");
        }
        let mut result = GenerateResult::complete();
        for item in items {
            let artifact = if let Some(encoded) = item["b64_json"].as_str() {
                media::image(encoded)?
            } else if let Some(url) = item["url"].as_str() {
                // Generated asset URLs may be signed. Never attach API credentials.
                let url = reqwest::Url::parse(url).context("invalid generated image URL")?;
                if !matches!(url.scheme(), "http" | "https") {
                    bail!("unsupported generated image URL scheme");
                }
                let response = self
                    .http
                    .get(url)
                    .timeout(self.timeout)
                    .send()
                    .await?
                    .error_for_status()?;
                let bytes = read_body(response).await?;
                media::image_bytes(bytes)?
            } else {
                bail!("image response has neither b64_json nor url");
            };
            result.artifacts.push(artifact);
        }
        Ok(result)
    }

    pub async fn generate_stream(
        &self,
        request: &GenerateRequest<'_>,
        mut on_delta: impl FnMut(&str),
    ) -> Result<GenerateResult> {
        let mut resp = tokio::time::timeout(self.timeout, self.post(request, true)?.send())
            .await
            .with_context(|| {
                format!(
                    "timed out after {}s waiting for response headers",
                    self.timeout.as_secs()
                )
            })?
            .context("request failed")?;
        let status = resp.status();
        let event_stream = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| {
                ct.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("text/event-stream")
            });
        if !status.is_success() || !event_stream {
            let body = tokio::time::timeout(self.timeout, read_body(resp))
                .await
                .context("timed out reading response body")?
                .context("failed to read response body")?;
            if !status.is_success() {
                return Err(api_error(status, &String::from_utf8_lossy(&body)));
            }
            let body = std::str::from_utf8(&body).context("expected a JSON response")?;
            let result = self.parse(body)?;
            if !result.text.is_empty() {
                on_delta(&result.text);
            }
            return Ok(result);
        }
        let mut frames = SseDecoder::new();
        let mut codec = match self.adapter {
            Adapter::Chat => StreamCodec::Chat(chat::Stream::default()),
            Adapter::Responses => StreamCodec::Responses(responses::Stream::default()),
            _ => bail!("adapter does not support SSE streaming"),
        };
        let mut chars = 0;
        let mut received_bytes = 0usize;
        let mut emit = |text: &str| {
            chars += text.chars().count();
            on_delta(text);
        };
        'read: loop {
            let chunk = tokio::time::timeout(self.timeout, resp.chunk()).await;
            let bytes = match chunk {
                Err(_) => bail!(
                    "stream interrupted after {chars} chars: no stream data for {}s",
                    self.timeout.as_secs()
                ),
                Ok(Err(error)) => bail!("stream interrupted after {chars} chars: {error}"),
                Ok(Ok(None)) => {
                    for data in frames.finish() {
                        if codec.feed(&data, &mut emit)? {
                            break;
                        }
                    }
                    break;
                }
                Ok(Ok(Some(bytes))) => bytes,
            };
            received_bytes = received_bytes.saturating_add(bytes.len());
            if received_bytes > MAX_RESPONSE_BYTES {
                bail!("stream exceeds the 128 MiB response limit");
            }
            for data in frames.feed(&bytes) {
                if codec.feed(&data, &mut emit)? {
                    break 'read;
                }
            }
        }
        codec.finish()
    }
}

enum StreamCodec {
    Chat(chat::Stream),
    Responses(responses::Stream),
}
impl StreamCodec {
    fn feed(&mut self, data: &str, on_delta: &mut impl FnMut(&str)) -> Result<bool> {
        match self {
            Self::Chat(codec) => codec.feed(data, on_delta),
            Self::Responses(codec) => codec.feed(data, on_delta),
        }
    }
    fn finish(self) -> Result<GenerateResult> {
        match self {
            Self::Chat(codec) => codec.finish(),
            Self::Responses(codec) => codec.finish(),
        }
    }
}

const MAX_RESPONSE_BYTES: usize = 128 * 1024 * 1024;

async fn read_body(mut response: reqwest::Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        bail!("response exceeds the 128 MiB limit");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed to read response body")?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            bail!("response exceeds the 128 MiB limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests;

//! The ASR server: one offline engine, loaded once, exposed over the
//! OpenAI-compatible transcription API.
//!
//! `POST /v1/audio/transcriptions` answers the same multipart shape the
//! openai-transcription adapter sends (and returns the same `{"text"}`
//! body that adapter parses), so `aido transcribe` reaches this server
//! with nothing but a provider whose base_url points here — the task
//! itself does not know local inference exists. `GET /health` and
//! `GET /v1/models` exist for orchestration and for clients that list
//! models before they talk.
//!
//! The engine face is a trait ([`PreparedEngine`]) so the handlers test
//! against a mock without any model files; the production engine is
//! prepared once at startup (`Engine::prepare` — the seconds-to-minutes
//! part, which is the whole point of a server) and shared across
//! requests: each request gets its own recognition session, bounded by
//! the engine's `max_active_sessions`. A request beyond that budget is
//! refused with 503, never queued. The HTTP layer lives in every
//! asr-server build; the engine assembly (family detection, model
//! loading) needs the sherpa backend and is gated on `local-asr` —
//! asr-core gates its own model-layout discovery on that backend too.

use axum::extract::{
    multipart::MultipartError, DefaultBodyLimit, FromRequest, Multipart, Request, State,
};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(feature = "local-asr")]
use crate::cli::AsrServeArgs;
#[cfg(feature = "local-asr")]
use crate::domain::AppResult;
#[cfg(feature = "local-asr")]
use anyhow::Context as _;
#[cfg(feature = "local-asr")]
use asr_core::{Engine, EngineConfig, OfflineConfig, OfflineFamily, PunctConfig, VadConfig};

// The engine vocabulary, re-exported: `PreparedEngine` is public (mock
// engines in tests and library embedders implement it), so its argument
// and return types must be nameable from outside the crate.
pub use asr_core::{
    AudioBuffer, Segment, SessionOptions, SessionOutcome, SessionResult, Transcript,
};

/// The engine face the handlers consume, as a trait so the sherpa stack
/// can stand down in tests (a mock needs no model load). asr-core's
/// `Engine` is the production implementor.
pub trait PreparedEngine: Send + Sync {
    fn transcribe(
        &self,
        buffer: &AudioBuffer,
        options: SessionOptions,
        deadline: Instant,
    ) -> SessionResult;
}

#[cfg(feature = "local-asr")]
impl PreparedEngine for Engine {
    fn transcribe(
        &self,
        buffer: &AudioBuffer,
        options: SessionOptions,
        deadline: Instant,
    ) -> SessionResult {
        Engine::transcribe(self, buffer, options, deadline)
    }
}

/// The per-request knobs the CLI flags boil down to, shared by the
/// handlers and the tests. `language` is pinned at startup: the engine
/// config is fixed once `Engine::prepare` ran, so a request naming a
/// different language is refused with an explanation rather than
/// silently transcribed under the wrong one.
#[derive(Debug, Clone)]
pub struct ServerKnobs {
    /// Decode budget: `max_audio_secs * 48_000` samples.
    pub max_samples: usize,
    /// Per-request hard wall; `None` stretches with the audio length
    /// (`max(120s, audio_secs * 3)` — local inference usually runs
    /// faster than real time, so the stretch never bites early).
    pub timeout: Option<Duration>,
    /// The recognition language pinned by `--language`, if given.
    pub language: Option<String>,
    /// Model id reported by `/v1/models` and `/health`: the ASR
    /// directory's name.
    pub model_id: String,
    /// Engine family name reported by `/health`.
    pub family: String,
}

/// The router over an already-prepared engine. Public so integration
/// tests (and library embedders) can run the server over a mock engine;
/// `run` wires the production engine into the same router.
pub fn router(
    engine: Arc<dyn PreparedEngine>,
    knobs: ServerKnobs,
    max_body_bytes: usize,
) -> Router {
    let state = Arc::new(AppState { engine, knobs });
    Router::new()
        .route("/v1/audio/transcriptions", post(transcriptions))
        .route("/v1/models", get(models_list))
        .route("/health", get(health))
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .with_state(state)
}

struct AppState {
    engine: Arc<dyn PreparedEngine>,
    knobs: ServerKnobs,
}

async fn transcriptions(State(state): State<Arc<AppState>>, req: Request) -> Response {
    // The extractor's rejection is only the missing/invalid boundary
    // (a malformed request); size and stream errors surface through the
    // field reads below, where the 413 mapping lives.
    let mut multipart = match Multipart::from_request(req, &state).await {
        Ok(multipart) => multipart,
        Err(error) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                format!("invalid multipart request: {error}"),
            )
        }
    };
    let mut file: Option<axum::body::Bytes> = None;
    let mut language: Option<String> = None;
    let mut response_format = "json".to_string();
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(error) => return multipart_error(&error),
        };
        match field.name() {
            Some("file") => match field.bytes().await {
                Ok(bytes) => file = Some(bytes),
                Err(error) => return multipart_error(&error),
            },
            Some("language") => match field.text().await {
                Ok(text) => language = Some(text).filter(|s| !s.trim().is_empty()),
                Err(error) => return multipart_error(&error),
            },
            Some("response_format") => match field.text().await {
                Ok(text) => response_format = text,
                Err(error) => return multipart_error(&error),
            },
            // `model` names a model the server does not select on (the
            // engine was loaded at startup); `prompt`/`temperature` have
            // no offline counterpart. Accepted, ignored.
            _ => {}
        }
    }
    let Some(bytes) = file else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "multipart field 'file' is required",
        );
    };
    if response_format != "json" {
        return error_response(
            StatusCode::BAD_REQUEST,
            format!("unsupported response_format '{response_format}'; only 'json' is supported"),
        );
    }
    if let Some(requested) = &language {
        match &state.knobs.language {
            Some(pinned) if pinned == requested => {}
            Some(pinned) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "this server pins language '{pinned}' (--language); the engine \
                         cannot switch languages at runtime — restart with --language \
                         {requested}"
                    ),
                );
            }
            None => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "this server was started without --language; the engine cannot \
                     switch languages at runtime — restart with --language <lang>",
                );
            }
        }
    }
    let engine = state.engine.clone();
    let knobs = state.knobs.clone();
    // The decode and the inference are synchronous CPU work: off the
    // tokio workers, exactly like the engine load. The blocking closure
    // returns the small typed failure; the (large) response body is
    // built outside it.
    let outcome =
        tokio::task::spawn_blocking(move || transcribe_blocking(engine.as_ref(), &bytes, &knobs))
            .await;
    match outcome {
        Ok(Ok(text)) => Json(json!({ "text": text })).into_response(),
        Ok(Err(failure)) => failure_response(failure),
        Err(join) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("transcription task failed: {join}"),
        ),
    }
}

/// Why a request's transcription failed, in status-code terms.
enum RequestFailure {
    BadRequest(String),
    Busy(String),
    Timeout(String),
    Engine(String),
}

fn failure_response(failure: RequestFailure) -> Response {
    match failure {
        RequestFailure::BadRequest(message) => error_response(StatusCode::BAD_REQUEST, message),
        RequestFailure::Busy(message) => error_response(StatusCode::SERVICE_UNAVAILABLE, message),
        RequestFailure::Timeout(message) => error_response(StatusCode::GATEWAY_TIMEOUT, message),
        RequestFailure::Engine(message) => {
            error_response(StatusCode::INTERNAL_SERVER_ERROR, message)
        }
    }
}

/// The synchronous half of one request: decode → session options →
/// transcribe. The failure kinds map onto the OpenAI-style error body:
/// Busy is the concurrency budget (503, never queued), Timeout the
/// deadline (504), InvalidInput a malformed request (400).
fn transcribe_blocking(
    engine: &dyn PreparedEngine,
    bytes: &[u8],
    knobs: &ServerKnobs,
) -> Result<String, RequestFailure> {
    let buffer = crate::audio::decode_mono(bytes, knobs.max_samples)
        .map_err(|e| RequestFailure::BadRequest(format!("audio decoding failed: {e:#}")))?;
    let audio_secs = buffer.samples.len() as u64 / u64::from(buffer.spec.sample_rate.max(1));
    let mut options = SessionOptions::new(buffer.spec);
    options.max_duration = Duration::from_secs(audio_secs + 60);
    options.max_transcript_bytes = 2 * 1024 * 1024;
    // An explicit `--timeout-secs` is the hard wall — no silent
    // stretching. The built-in policy stretches with the audio length:
    // audio_secs * 3 keeps slack for inference of long material and
    // never bites before that would.
    let budget = knobs
        .timeout
        .unwrap_or_else(|| Duration::from_secs((audio_secs * 3).max(120)));
    let deadline = Instant::now() + budget;
    match engine.transcribe(&buffer, options, deadline) {
        Ok(outcome) => Ok(outcome.transcript.text()),
        Err(failure) => {
            let message = format!("transcription failed: {}", failure.error);
            match failure.error.kind {
                asr_core::ErrorKind::Busy => Err(RequestFailure::Busy(
                    "the server is at its concurrent-session limit; retry later".to_string(),
                )),
                asr_core::ErrorKind::Timeout => Err(RequestFailure::Timeout(message)),
                asr_core::ErrorKind::InvalidInput => Err(RequestFailure::BadRequest(message)),
                _ => Err(RequestFailure::Engine(message)),
            }
        }
    }
}

async fn health(State(state): State<Arc<AppState>>) -> Response {
    Json(json!({
        "status": "ok",
        "model": state.knobs.model_id,
        "family": state.knobs.family,
    }))
    .into_response()
}

async fn models_list(State(state): State<Arc<AppState>>) -> Response {
    Json(json!({
        "object": "list",
        "data": [{
            "id": state.knobs.model_id,
            "object": "model",
            "created": 0,
            "owned_by": "aido",
        }],
    }))
    .into_response()
}

/// The OpenAI-style error body. aido's own client parses exactly this
/// shape (`{"error": {"message": ...}}`), so a failure here reads the
/// same whether the caller is `aido transcribe` or anything else.
fn error_response(status: StatusCode, message: impl std::fmt::Display) -> Response {
    let kind = if status.is_client_error() {
        "invalid_request_error"
    } else {
        "server_error"
    };
    (
        status,
        Json(json!({ "error": { "message": message.to_string(), "type": kind } })),
    )
        .into_response()
}

/// Multipart extraction failures carry their own status: a body over
/// `DefaultBodyLimit` is 413, everything else a malformed request (400).
fn multipart_error(error: &MultipartError) -> Response {
    let status = error.status();
    let message = if status == StatusCode::PAYLOAD_TOO_LARGE {
        format!("request body exceeds the server's size limit (--max-body-mb): {error}")
    } else {
        format!("invalid multipart request: {error}")
    };
    error_response(status, message)
}

/// Ctrl+C/SIGTERM stop the accept loop; in-flight requests finish
/// first (axum's graceful shutdown), then the process exits. Only the
/// production `run` shuts the server down — test/embedder routers live
/// as long as their owners keep them.
#[cfg(feature = "local-asr")]
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let sigterm = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
            }
            // Registration failure parks the future forever so the
            // select still works; the platform keeps its default
            // disposition then.
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let sigterm = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = sigterm => {},
    }
}

// ---------------------------------------------------------------------------
// The engine assembly (needs the sherpa backend)
// ---------------------------------------------------------------------------

/// The server's model files, fully resolved: the config layer expands
/// tildes and `resolve_profile_models` refuses a profile that leaves the
/// required fields out, so startup never re-checks them. The ASR
/// directory and the silero VAD file are required; the punctuation
/// directory is optional.
#[cfg(feature = "local-asr")]
#[derive(Debug, Clone)]
pub struct LocalModels {
    pub asr: std::path::PathBuf,
    pub vad: std::path::PathBuf,
    pub punct: Option<std::path::PathBuf>,
}

/// `aido serve asr`: load the profile's models once, then serve until
/// Ctrl+C/SIGTERM. Exists only in builds with the engine (the sherpa
/// backend); the router and its handlers exist in every asr-server build.
#[cfg(feature = "local-asr")]
pub async fn run(args: &AsrServeArgs) -> AppResult<()> {
    use crate::domain::AppError;

    // The bounds the adapter's option whitelist used to validate, now on
    // the flags: threads 1..=96, decode budget 1..=86_400 (a day).
    if let Some(threads) = args.threads {
        if !(1..=96).contains(&threads) {
            return Err(AppError::usage("--threads must be between 1 and 96"));
        }
    }
    if !(1..=86_400).contains(&args.max_audio_secs) {
        return Err(AppError::usage(
            "--max-audio-secs must be between 1 and 86400",
        ));
    }
    let cfg = crate::config::load().map_err(|e| AppError::usage(format!("{e:#}")))?;
    let profile = cfg.profiles.get(&args.profile).ok_or_else(|| {
        AppError::usage(format!(
            "profile '{}' not found; `aido serve asr` loads its models from a \
             profile's asr/vad/punct fields (see `aido profiles list`)",
            args.profile
        ))
    })?;
    let models = resolve_profile_models(&args.profile, profile)
        .map_err(|e| AppError::usage(format!("{e:#}")))?;

    // The engine load is seconds-to-minutes of CPU: off the tokio
    // workers, exactly like a request's transcription.
    let family = args.family.clone();
    let language = args.language.clone();
    let threads = args.threads;
    let max_active_sessions = args.max_active_sessions;
    let load_models = models.clone();
    let started = Instant::now();
    let (engine, family_name) = tokio::task::spawn_blocking(move || {
        prepare(
            load_models,
            family.as_deref(),
            language,
            threads,
            max_active_sessions,
        )
    })
    .await
    .map_err(|e| AppError::service(format!("engine load task failed: {e}")))?
    .map_err(|e| AppError::service(format!("{e:#}")))?;

    let model_id = models
        .asr
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "local-asr".to_string());
    let knobs = ServerKnobs {
        max_samples: args.max_audio_secs as usize * 48_000,
        timeout: args.timeout_secs.map(Duration::from_secs),
        language: args.language.clone(),
        model_id,
        family: family_name.clone(),
    };
    let max_body_bytes =
        usize::try_from(args.max_body_mb.saturating_mul(1024 * 1024)).unwrap_or(usize::MAX);
    let app = router(engine, knobs, max_body_bytes);

    let listener = tokio::net::TcpListener::bind(&args.bind)
        .await
        .map_err(|e| AppError::usage(format!("cannot bind {}: {e}", args.bind)))?;
    println!(
        "loaded {family_name} from {} in {:.1}s; listening on http://{} \
         (POST /v1/audio/transcriptions, OpenAI-compatible)",
        models.asr.display(),
        started.elapsed().as_secs_f32(),
        listener
            .local_addr()
            .map_err(|e| AppError::service(format!("{e}")))?,
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| AppError::service(format!("server error: {e}")))?;
    Ok(())
}

/// The profile's model fields, tilde-expanded: `asr` and `vad` are
/// required (the offline engine cannot run without a VAD), `punct` is
/// optional. The profile's provider/model fields are none of the
/// server's business — it serves whatever `asr` points at.
#[cfg(feature = "local-asr")]
fn resolve_profile_models(
    name: &str,
    profile: &crate::config::Profile,
) -> anyhow::Result<LocalModels> {
    use anyhow::bail;

    let Some(asr) = profile.asr.as_deref() else {
        bail!("profile '{name}' sets no 'asr' (the ASR model directory)");
    };
    let Some(vad) = profile.vad.as_deref() else {
        bail!("profile '{name}' sets no 'vad' (the silero VAD file)");
    };
    Ok(LocalModels {
        asr: crate::config::path::expand_home(asr),
        vad: crate::config::path::expand_home(vad),
        punct: profile
            .punct
            .as_deref()
            .map(crate::config::path::expand_home),
    })
}

/// Prepare the production engine: the detect → resolve → precheck chain
/// runs first so misconfiguration fails with a named problem before any
/// model load, then `Engine::prepare` does the expensive part. Returns
/// the engine and the family's display name.
#[cfg(feature = "local-asr")]
fn prepare(
    models: LocalModels,
    family: Option<&str>,
    language: Option<String>,
    threads: Option<usize>,
    max_active_sessions: usize,
) -> anyhow::Result<(Arc<dyn PreparedEngine>, String)> {
    let config = engine_config(&models, family, language, threads)?;
    let family_name = format!("{:?}", engine_config_family(&config));
    let options = asr_core::EngineOptions::new(max_active_sessions)
        .context("--max-active-sessions must be a positive integer")?;
    let engine =
        asr_core::Engine::prepare(config, options).context("failed to load the local ASR model")?;
    Ok((Arc::new(engine), family_name))
}

#[cfg(feature = "local-asr")]
fn engine_config_family(config: &asr_core::EngineConfig) -> &OfflineFamily {
    match config {
        asr_core::EngineConfig::Offline(offline) => &offline.family,
        _ => unreachable!("engine_config builds an offline engine"),
    }
}

/// The engine configuration a run's `prepare` loads and `check_model`
/// prechecks: profile-resolved model files plus the CLI's engine knobs.
#[cfg(feature = "local-asr")]
fn engine_config(
    models: &LocalModels,
    family: Option<&str>,
    language: Option<String>,
    threads: Option<usize>,
) -> anyhow::Result<EngineConfig> {
    use anyhow::Context as _;

    let detected = asr_core::utils::models::detect(&models.asr).with_context(|| {
        format!(
            "'{}' is not a usable ASR model directory",
            models.asr.display()
        )
    })?;
    let family = resolve_family(detected, family)?;
    let vad_path = ensure_vad(&models.vad)?;
    let mut config = OfflineConfig::new(&models.asr, family, VadConfig::new(&vad_path));
    config.language = language;
    config.punctuation = models.punct.as_deref().map(PunctConfig::new);
    if let Some(threads) = threads {
        config.num_threads = threads;
    }
    let engine_config = EngineConfig::Offline(config);
    asr_core::utils::precheck::validate(&engine_config)
        .context("the local ASR configuration failed the engine precheck")?;
    Ok(engine_config)
}

/// The VAD file must exist — its absence is a named request (place the
/// file / fix the profile field) instead of a precheck error about empty
/// files.
#[cfg(feature = "local-asr")]
fn ensure_vad(vad: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    use anyhow::bail;

    if !vad.is_file() {
        bail!(
            "VAD model not found at {}; download silero_vad.onnx there or \
             point the profile's 'vad' field at the file",
            vad.display()
        );
    }
    Ok(vad.to_path_buf())
}

/// Directory layout → engine family. `detect` decides from the file
/// layout and is definitive except for the flat layout (paraformer and
/// firered-ctc share it): there the `--family` flag decides, and its
/// absence is a named request, not a guess.
#[cfg(feature = "local-asr")]
fn resolve_family(
    detected: asr_core::utils::models::LocalModel,
    requested: Option<&str>,
) -> anyhow::Result<OfflineFamily> {
    use anyhow::bail;
    use asr_core::utils::models::LocalModel;

    match (detected, requested) {
        (LocalModel::SenseVoice, None | Some("sensevoice")) => Ok(OfflineFamily::SenseVoice),
        (LocalModel::Transducer, None | Some("transducer")) => Ok(OfflineFamily::Transducer),
        (LocalModel::Qwen3Asr, None | Some("qwen3-asr")) => Ok(OfflineFamily::Qwen3Asr),
        (LocalModel::FunAsrNano, None | Some("funasr-nano")) => Ok(OfflineFamily::FunAsrNano),
        (LocalModel::FireRedAsrAed, None | Some("firered-aed")) => Ok(OfflineFamily::FireRedAsrAed),
        (LocalModel::Flat, Some("paraformer")) => Ok(OfflineFamily::Paraformer),
        (LocalModel::Flat, Some("firered-ctc")) => Ok(OfflineFamily::FireRedAsrCtc),
        (LocalModel::Flat, None) => bail!(
            "the model layout is shared by paraformer and firered-ctc; pass \
             --family to 'aido serve asr' (paraformer or firered-ctc)"
        ),
        (LocalModel::Punct, _) => bail!(
            "the 'asr' field points at a punctuation model, not an ASR model; \
             point 'asr' at a speech model and set the profile's 'punct' \
             field to the punctuation directory"
        ),
        (detected, Some(other)) => bail!(
            "model directory was detected as {detected:?}, but --family \
             says '{other}'"
        ),
        // LocalModel is non-exhaustive upstream: a family aido has no
        // mapping for lands here instead of failing to compile.
        (detected, None) => bail!(
            "model directory was detected as {detected:?}; this aido build \
             has no engine mapping for that family"
        ),
    }
}

/// Why a no-load model check stopped. `AmbiguousFamily` is not a config
/// error: the flat layout's family is a `--family` decision made at
/// serve time, so `config check` downgrades it to a note.
#[cfg(feature = "local-asr")]
pub(crate) enum ModelCheckFailure {
    AmbiguousFamily,
    Failed(anyhow::Error),
}

/// The no-load model check `config check` runs on a profile with ASR
/// model fields: the same detect and cheap checks startup performs, no
/// model load. `family` is always None here — the check has no flag, so
/// a flat layout lands in [`ModelCheckFailure::AmbiguousFamily`].
#[cfg(feature = "local-asr")]
pub(crate) fn check_model(models: &LocalModels) -> Result<(), ModelCheckFailure> {
    let detected = asr_core::utils::models::detect(&models.asr).map_err(|e| {
        ModelCheckFailure::Failed(anyhow::anyhow!(
            "'{}' is not a usable ASR model directory: {e:#}",
            models.asr.display()
        ))
    })?;
    if matches!(detected, asr_core::utils::models::LocalModel::Flat) {
        return Err(ModelCheckFailure::AmbiguousFamily);
    }
    engine_config(models, None, None, None)
        .map(|_| ())
        .map_err(ModelCheckFailure::Failed)
}

#[cfg(test)]
mod tests;

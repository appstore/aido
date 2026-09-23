//! Local offline transcription adapter over asr-core's sherpa engine.
//!
//! The heavy lifting — model loading, VAD segmentation, inference,
//! punctuation — is `Engine::transcribe`; this module owns what aido adds
//! on top: wiring the profile's already-resolved model files (ASR
//! directory, silero VAD, punctuation) and options (family/language/
//! threads/max_audio_secs) into an engine config, the sample budget handed
//! to the decode layer, and the deadline policy. Engine supply goes
//! through [`EngineProvider`]: the production [`FreshEngineProvider`]
//! prepares one engine per run (a CLI run is one task; model reuse across
//! runs is EngineManager territory and would be another impl of the same
//! trait, not a redesign of this adapter).

use super::{single_audio, GenerateRequest, GenerateResult};
use anyhow::{bail, Context, Result};
use asr_core::utils::models::{detect, LocalModel};
use asr_core::{
    AudioBuffer, Engine, EngineConfig, EngineOptions, OfflineConfig, OfflineFamily, PunctConfig,
    SessionOptions, SessionResult, VadConfig,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The local-asr adapter's model files, fully resolved: the config layer
/// expands tildes and refuses a profile that leaves the required fields
/// out, so a run never re-checks them. The ASR directory and the silero
/// VAD file are required; the punctuation directory is optional.
#[derive(Debug, Clone)]
pub struct LocalModels {
    pub asr: PathBuf,
    pub vad: PathBuf,
    pub punct: Option<PathBuf>,
}

/// The engine face this adapter consumes, as a trait so the sherpa stack
/// can stand down in tests (a mock needs no model load). asr-core's
/// `Engine` is the production implementor.
pub(crate) trait PreparedEngine: Send + Sync {
    fn transcribe(
        &self,
        buffer: &AudioBuffer,
        options: SessionOptions,
        deadline: Instant,
    ) -> SessionResult;
}

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

/// Supplies the prepared engine for a run's configuration. The seam where
/// a caching provider (a `HashMap`-keyed model cache) would plug in later;
/// nothing else in the adapter may touch `Engine::prepare`.
pub(crate) trait EngineProvider: Send + Sync {
    fn engine(&self, config: EngineConfig) -> Result<Arc<dyn PreparedEngine>>;
}

/// The production provider: every run prepares its own engine. Model load
/// is seconds-to-minutes of CPU, which is exactly why the seam exists —
/// a future cached provider swaps in without touching this adapter again.
pub(crate) struct FreshEngineProvider;

impl EngineProvider for FreshEngineProvider {
    fn engine(&self, config: EngineConfig) -> Result<Arc<dyn PreparedEngine>> {
        let engine = Engine::prepare(config, EngineOptions::default())
            .context("failed to load the local ASR model")?;
        Ok(Arc::new(engine))
    }
}

pub(super) async fn transcribe(
    request: &GenerateRequest<'_>,
    timeout: Duration,
    timeout_explicit: bool,
    models: &LocalModels,
) -> Result<GenerateResult> {
    transcribe_with(
        request,
        timeout,
        timeout_explicit,
        models,
        Arc::new(FreshEngineProvider),
    )
    .await
}

async fn transcribe_with(
    request: &GenerateRequest<'_>,
    timeout: Duration,
    timeout_explicit: bool,
    models: &LocalModels,
    provider: Arc<dyn EngineProvider>,
) -> Result<GenerateResult> {
    let part = single_audio(request.inputs)?;
    let bytes = match &part.content {
        crate::domain::InputContent::Media(bytes) => bytes.clone(),
        _ => bail!("transcription input must be audio"),
    };
    let opt_str = |key: &str| {
        request
            .options
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    };
    let opt_u64 = |key: &str| request.options.get(key).and_then(|v| v.as_u64());
    let family = opt_str("family");
    let language = opt_str("language");
    let threads = opt_u64("threads").map(|v| v as usize);
    let max_audio_secs = opt_u64("max_audio_secs").unwrap_or(3600) as usize;
    let models = models.clone();

    // The engine calls are synchronous and can hold a CPU for minutes;
    // keep them off the tokio workers.
    let outcome = tokio::task::spawn_blocking(move || {
        let knobs = EngineKnobs {
            family,
            language,
            threads,
            max_samples: max_audio_secs * 48_000,
        };
        run(
            &bytes,
            &models,
            &knobs,
            timeout,
            timeout_explicit,
            &*provider,
        )
    })
    .await
    .context("local transcription task panicked")??;
    Ok(GenerateResult::complete_with_text(
        outcome.transcript.text(),
    ))
}

/// The request's option map, extracted to typed engine knobs before the
/// blocking section.
struct EngineKnobs {
    family: Option<String>,
    language: Option<String>,
    threads: Option<usize>,
    max_samples: usize,
}

/// The synchronous half: decode → detect → configure → precheck → prepare
/// → transcribe. `utils::precheck::validate` runs the cheap checks first
/// so misconfiguration fails with a named problem before any model load.
fn run(
    bytes: &[u8],
    models: &LocalModels,
    knobs: &EngineKnobs,
    timeout: Duration,
    timeout_explicit: bool,
    provider: &dyn EngineProvider,
) -> Result<asr_core::SessionOutcome> {
    let buffer = crate::audio::decode_mono(bytes, knobs.max_samples)
        .context("audio decoding for the offline engine failed")?;
    let detected = detect(&models.asr).with_context(|| {
        format!(
            "'{}' is not a usable ASR model directory",
            models.asr.display()
        )
    })?;
    let family = resolve_family(detected, knobs.family.as_deref())?;
    let vad_path = ensure_vad(&models.vad)?;
    let mut config = OfflineConfig::new(&models.asr, family, VadConfig::new(&vad_path));
    config.language = knobs.language.clone();
    config.punctuation = models.punct.as_deref().map(PunctConfig::new);
    if let Some(threads) = knobs.threads {
        config.num_threads = threads;
    }
    let engine_config = EngineConfig::Offline(config);
    asr_core::utils::precheck::validate(&engine_config)
        .context("the local ASR configuration failed the engine precheck")?;
    let engine = provider.engine(engine_config)?;

    let audio_secs = buffer.samples.len() as u64 / u64::from(buffer.spec.sample_rate.max(1));
    let mut options = SessionOptions::new(buffer.spec);
    options.max_duration = Duration::from_secs(audio_secs + 60);
    options.max_transcript_bytes = 2 * 1024 * 1024;
    // The budget an explicit `--timeout`/`settings.timeout_secs` buys is
    // the hard wall — no silent stretching: a 3-hour recording under a
    // 30s timeout fails at 30s. Only the built-in default (which exists
    // to bound network waits) yields, stretching with the audio: local
    // inference usually runs faster than real time, so audio_secs * 3
    // keeps slack for the model load and never bites before that would.
    let budget = if timeout_explicit {
        timeout
    } else {
        timeout.max(Duration::from_secs(audio_secs * 3))
    };
    let deadline = Instant::now() + budget;
    engine
        .transcribe(&buffer, options, deadline)
        .map_err(|failure| anyhow::anyhow!("local transcription failed: {failure}"))
}

/// The VAD file must exist — its absence is a named request (place the
/// file / fix the profile field) instead of a precheck error about empty
/// files.
fn ensure_vad(vad: &std::path::Path) -> Result<PathBuf> {
    if !vad.is_file() {
        bail!(
            "VAD model not found at {}; download silero_vad.onnx there or \
             point the profile's 'vad' field at the file",
            vad.display()
        );
    }
    Ok(vad.to_path_buf())
}

/// Directory layout → engine family. `detect` decides from the file layout
/// and is definitive except for the flat layout (paraformer and
/// firered-ctc share it): there the profile's `family` option decides, and
/// its absence is a named request, not a guess.
fn resolve_family(detected: LocalModel, requested: Option<&str>) -> Result<OfflineFamily> {
    match (detected, requested) {
        (LocalModel::SenseVoice, None | Some("sensevoice")) => Ok(OfflineFamily::SenseVoice),
        (LocalModel::Transducer, None | Some("transducer")) => Ok(OfflineFamily::Transducer),
        (LocalModel::Qwen3Asr, None | Some("qwen3-asr")) => Ok(OfflineFamily::Qwen3Asr),
        (LocalModel::FunAsrNano, None | Some("funasr-nano")) => Ok(OfflineFamily::FunAsrNano),
        (LocalModel::FireRedAsrAed, None | Some("firered-aed")) => Ok(OfflineFamily::FireRedAsrAed),
        (LocalModel::Flat, Some("paraformer")) => Ok(OfflineFamily::Paraformer),
        (LocalModel::Flat, Some("firered-ctc")) => Ok(OfflineFamily::FireRedAsrCtc),
        (LocalModel::Flat, None) => bail!(
            "the model layout is shared by paraformer and firered-ctc; set \
             option 'family' to one of: paraformer, firered-ctc"
        ),
        (LocalModel::Punct, _) => bail!(
            "the 'asr' field points at a punctuation model, not an ASR model; \
             point 'asr' at a speech model and set the profile's 'punct' \
             field to the punctuation directory"
        ),
        (detected, Some(other)) => bail!(
            "model directory was detected as {detected:?}, but option \
             'family' says '{other}'"
        ),
        // LocalModel is non-exhaustive upstream: a family aido has no
        // mapping for lands here instead of failing to compile.
        (detected, None) => bail!(
            "model directory was detected as {detected:?}; this aido build \
             has no engine mapping for that family"
        ),
    }
}

/// Config-check precheck of a local-asr profile: the same detect and
/// cheap checks a run performs (no model load). The caller — `config
/// check` — has already refused a profile missing the required fields;
/// any failure left is a model-layout issue.
pub(crate) fn check_model(models: &LocalModels, family: Option<&str>) -> Result<()> {
    let detected = detect(&models.asr).with_context(|| {
        format!(
            "'{}' is not a usable ASR model directory",
            models.asr.display()
        )
    })?;
    let family = resolve_family(detected, family)?;
    let vad_path = ensure_vad(&models.vad)?;
    let mut offline = OfflineConfig::new(&models.asr, family, VadConfig::new(&vad_path));
    offline.punctuation = models.punct.as_deref().map(PunctConfig::new);
    asr_core::utils::precheck::validate(&EngineConfig::Offline(offline))
        .context("the 'asr' directory failed the engine precheck")
}

#[cfg(test)]
mod tests;

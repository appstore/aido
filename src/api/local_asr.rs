//! Local offline transcription adapter over asr-core's sherpa engine.
//!
//! The heavy lifting — model loading, VAD segmentation, inference,
//! punctuation — is `Engine::transcribe`; this module owns what aido adds
//! on top: the profile's model files (ASR directory, silero VAD,
//! punctuation), the profile options (family/language/threads/
//! max_audio_secs), the sample budget handed to the decode layer, and the
//! deadline policy (local inference is usually faster than real time, so
//! the deadline never bites before `--timeout` would). Every run prepares
//! its own engine: a CLI run is one task, and model reuse across runs is
//! EngineManager territory, not this adapter's.

use super::{single_audio, GenerateRequest, GenerateResult};
use anyhow::{bail, Context, Result};
use asr_core::utils::models::{detect, LocalModel};
use asr_core::{
    Engine, EngineConfig, EngineOptions, OfflineConfig, OfflineFamily, PunctConfig, SessionOptions,
    VadConfig,
};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The local-asr adapter's model files, resolved from the profile (tilde
/// expanded at resolve time). The ASR directory and the silero VAD file
/// are required for a run; the punctuation directory is optional.
#[derive(Debug, Clone)]
pub struct LocalModels {
    pub asr: Option<String>,
    pub vad: Option<String>,
    pub punct: Option<String>,
}

pub(super) async fn transcribe(
    request: &GenerateRequest<'_>,
    timeout: Duration,
    models: &LocalModels,
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
    let max_audio_secs = opt_u64("max_audio_secs").unwrap_or(7200) as usize;
    // The resolve layer guarantees asr/vad are present for this adapter;
    // run() re-checks as a backstop.
    let models = models.clone();

    // The engine calls are synchronous and can hold a CPU for minutes;
    // keep them off the tokio workers.
    let outcome = tokio::task::spawn_blocking(move || {
        run(
            &bytes,
            &models,
            family.as_deref(),
            language,
            threads,
            max_audio_secs * 48_000,
            timeout,
        )
    })
    .await
    .context("local transcription task panicked")??;
    Ok(GenerateResult::complete_with_text(
        outcome.transcript.text(),
    ))
}

/// Expand a leading `~/` to the home directory (config paths keep their
/// tilde until they meet the filesystem). Other forms pass through.
pub(crate) fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// The synchronous half: decode → detect → configure → precheck → prepare
/// → transcribe. `Engine::prepare` stays the authority on the model
/// directory; `utils::precheck::validate` runs the same cheap checks first
/// so misconfiguration fails with a named problem before any model load.
fn run(
    bytes: &[u8],
    models: &LocalModels,
    family: Option<&str>,
    language: Option<String>,
    threads: Option<usize>,
    max_samples: usize,
    timeout: Duration,
) -> Result<asr_core::SessionOutcome> {
    let buffer = crate::audio::decode_mono(bytes, max_samples)
        .context("audio decoding for the offline engine failed")?;
    // The resolve layer guarantees the ASR directory and VAD file are set
    // for this adapter; this is a backstop, not the user-facing message.
    let model_dir = models
        .asr
        .as_deref()
        .context("the local-asr adapter has no model_dir")?;
    let vad = models
        .vad
        .as_deref()
        .context("the local-asr adapter has no vad")?;
    let model_dir = expand_home(model_dir);
    let detected = detect(&model_dir).with_context(|| {
        format!(
            "'{}' is not a usable ASR model directory",
            model_dir.display()
        )
    })?;
    let family = resolve_family(detected, family)?;
    let vad_path = ensure_vad(vad)?;
    let punct_path = models.punct.as_deref().map(expand_home);
    let mut config = OfflineConfig::new(&model_dir, family, VadConfig::new(&vad_path));
    config.language = language;
    config.punctuation = punct_path.map(PunctConfig::new);
    if let Some(threads) = threads {
        config.num_threads = threads;
    }
    let engine_config = EngineConfig::Offline(config);
    asr_core::utils::precheck::validate(&engine_config)
        .context("the local ASR configuration failed the engine precheck")?;
    let engine = Engine::prepare(engine_config, EngineOptions::default())
        .context("failed to load the local ASR model")?;

    let audio_secs = buffer.samples.len() as u64 / u64::from(buffer.spec.sample_rate.max(1));
    let mut options = SessionOptions::new(buffer.spec);
    options.max_duration = Duration::from_secs(audio_secs + 60);
    options.max_transcript_bytes = 2 * 1024 * 1024;
    let deadline = Instant::now() + timeout.max(Duration::from_secs(audio_secs));
    engine
        .transcribe(&buffer, options, deadline)
        .map_err(|failure| anyhow::anyhow!("local transcription failed: {failure}"))
}

/// The VAD file must exist — its absence is a named request (place the
/// file / fix the profile field) instead of a precheck error about empty
/// files.
fn ensure_vad(vad: &str) -> Result<PathBuf> {
    let path = expand_home(vad);
    if !path.is_file() {
        bail!(
            "VAD model not found at {}; download silero_vad.onnx there or \
             point the profile's 'vad' field at the file",
            path.display()
        );
    }
    Ok(path)
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
            "model_dir contains a punctuation model, not an ASR model; \
             point model_dir at a speech model and set the profile's \
             'punct' field to the punctuation directory"
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
/// cheap checks a run performs. The model files (tilde-expanded here) and
/// the profile's `family` option travel along. Any failure is a config
/// issue.
pub(crate) fn check_model(models: &LocalModels, family: Option<&str>) -> Result<()> {
    let asr = models
        .asr
        .as_deref()
        .context("the profile sets no model_dir")?;
    let vad = models
        .vad
        .as_deref()
        .context("the profile sets no vad model")?;
    let model_dir = expand_home(asr);
    let detected = detect(&model_dir).with_context(|| {
        format!(
            "'{}' is not a usable ASR model directory",
            model_dir.display()
        )
    })?;
    let family = resolve_family(detected, family)?;
    let vad_path = ensure_vad(vad)?;
    let punct = models.punct.as_deref().map(expand_home);
    let mut offline = OfflineConfig::new(&model_dir, family, VadConfig::new(&vad_path));
    offline.punctuation = punct.map(PunctConfig::new);
    asr_core::utils::precheck::validate(&EngineConfig::Offline(offline))
        .context("model_dir failed the engine precheck")
}

#[cfg(test)]
mod tests;

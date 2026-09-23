use super::{check_model, resolve_family, transcribe_with, EngineProvider, LocalModels};
use crate::api::GenerateRequest;
use crate::domain::{GenerationStatus, InputContent, InputPart, InputSource, MediaKind};
use asr_core::utils::models::{detect, LocalModel};
use asr_core::{
    AudioBuffer, OfflineFamily, Segment, SessionOptions, SessionOutcome, SessionResult, Transcript,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// A sense-voice flat layout: model + tokens.txt carrying the language
/// markers that make the family definitive.
fn sense_voice_dir(name: &str) -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("model.int8.onnx"), b"fake").unwrap();
    fs::write(dir.join("tokens.txt"), "<|zh|>\nzh\n<|en|>\nen\n").unwrap();
    (tmp, dir)
}

/// A flat layout without markers: paraformer and firered-ctc share it.
fn flat_dir(name: &str) -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("model.int8.onnx"), b"fake").unwrap();
    fs::write(dir.join("tokens.txt"), "zh 中国\nen hello\n").unwrap();
    (tmp, dir)
}

/// A punctuation layout: model + no tokens.txt.
fn punct_dir(name: &str) -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("model.int8.onnx"), b"fake").unwrap();
    (tmp, dir)
}

fn vad_file() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("vad.onnx"), b"fake-vad").unwrap();
    let path = tmp.path().join("vad.onnx");
    (tmp, path)
}

#[test]
fn flat_layout_demands_a_family_choice() {
    let (_tmp, dir) = flat_dir("some-flat-model");
    let detected = detect(&dir).unwrap();
    assert!(matches!(detected, LocalModel::Flat));
    let error = resolve_family(detected, None).unwrap_err();
    assert!(format!("{error:#}").contains("set option 'family'"));
}

#[test]
fn flat_layout_resolves_via_the_family_option() {
    let (_tmp, dir) = flat_dir("some-flat-model");
    let detected = detect(&dir).unwrap();
    assert!(matches!(
        resolve_family(detected, Some("paraformer")).unwrap(),
        OfflineFamily::Paraformer
    ));
    assert!(matches!(
        resolve_family(detected, Some("firered-ctc")).unwrap(),
        OfflineFamily::FireRedAsrCtc
    ));
}

#[test]
fn definitive_layouts_map_without_a_family_option() {
    let (_tmp, dir) = sense_voice_dir("sensevoice");
    assert!(matches!(
        resolve_family(detect(&dir).unwrap(), None).unwrap(),
        OfflineFamily::SenseVoice
    ));

    // A mismatched option is refused: the markers prove the family.
    let error = resolve_family(detect(&dir).unwrap(), Some("transducer")).unwrap_err();
    assert!(format!("{error:#}").contains("but option 'family'"));
}

#[test]
fn a_punct_directory_is_a_named_mistake() {
    let (_tmp, dir) = punct_dir("punct-model");
    let error = resolve_family(detect(&dir).unwrap(), None).unwrap_err();
    assert!(format!("{error:#}").contains("punctuation model"));
}

#[test]
fn check_model_passes_a_family_configured_flat_layout() {
    let (_tmp, dir) = flat_dir("some-flat-model");
    let (_vad_tmp, vad) = vad_file();
    let models = LocalModels {
        asr: dir,
        vad,
        punct: None,
    };
    // The ambiguity is refused without the option…
    let error = check_model(&models, None).unwrap_err();
    assert!(format!("{error:#}").contains("set option 'family'"));
    // …and passes with it.
    assert!(check_model(&models, Some("firered-ctc")).is_ok());
}

#[test]
fn a_missing_vad_file_is_a_named_error() {
    let (_tmp, dir) = sense_voice_dir("sensevoice");
    let models = LocalModels {
        asr: dir,
        vad: PathBuf::from("/nonexistent/silero_vad.onnx"),
        punct: None,
    };
    let error = check_model(&models, None).unwrap_err();
    assert!(format!("{error:#}").contains("VAD model not found"));
}

// ---------------------------------------------------------------------------
// The pipeline with a stand-in engine: request → adapter → mock → result.
// ---------------------------------------------------------------------------

/// One mono PCM16 wav of `secs` seconds: real bytes, so the decode layer
/// (the pipeline's input half) runs for real.
fn wav_bytes(sample_rate: u32, secs: u32) -> Vec<u8> {
    let samples = sample_rate * secs;
    let data_len = samples * 2;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for i in 0..samples {
        let t = i as f32 / sample_rate as f32;
        let v = (t * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.3;
        out.extend_from_slice(&((v * i16::MAX as f32) as i16).to_le_bytes());
    }
    out
}

/// The wall budget left when the stand-in engine was invoked.
type CapturedBudget = Arc<Mutex<Option<Duration>>>;

/// A provider that hands out an engine without touching sherpa-onnx: the
/// model fixtures above are not real models, so `Engine::prepare` cannot
/// run — the mock records the deadline it was given and returns a fixed
/// transcript.
struct MockProvider {
    budget: CapturedBudget,
}

impl EngineProvider for MockProvider {
    fn engine(
        &self,
        _config: asr_core::EngineConfig,
    ) -> anyhow::Result<Arc<dyn super::PreparedEngine>> {
        Ok(Arc::new(MockEngine {
            budget: self.budget.clone(),
        }))
    }
}

struct MockEngine {
    budget: CapturedBudget,
}

impl super::PreparedEngine for MockEngine {
    fn transcribe(
        &self,
        _buffer: &AudioBuffer,
        _options: SessionOptions,
        deadline: Instant,
    ) -> SessionResult {
        *self.budget.lock().unwrap() = Some(deadline.duration_since(Instant::now()));
        Ok(SessionOutcome {
            transcript: Transcript {
                segments: vec![Segment {
                    id: "s0".to_string(),
                    index: 0,
                    text: "你好世界".to_string(),
                    start_seconds: Some(0.0),
                    end_seconds: Some(1.0),
                }],
            },
            ..Default::default()
        })
    }
}

/// One GenerateRequest over an owned audio input and options map; the
/// caller keeps both alive for the call.
fn audio_request<'a>(
    input: &'a InputPart,
    options: &'a BTreeMap<String, serde_json::Value>,
) -> GenerateRequest<'a> {
    GenerateRequest {
        instruction: None,
        requirement: None,
        inputs: std::slice::from_ref(input),
        model: "local",
        max_tokens: None,
        temperature: None,
        outputs: &[MediaKind::Text],
        options,
    }
}

fn audio_input(bytes: &[u8]) -> InputPart {
    InputPart {
        id: 0,
        source: InputSource::File(PathBuf::from("clip.wav")),
        name: "clip.wav".to_string(),
        kind: MediaKind::Audio,
        unknown_kind: false,
        mime: "audio/wav".to_string(),
        content: InputContent::Media(bytes.to_vec()),
        unit: None,
    }
}

fn models_for(dir: &Path, vad: &Path) -> LocalModels {
    LocalModels {
        asr: dir.to_path_buf(),
        vad: vad.to_path_buf(),
        punct: None,
    }
}

#[tokio::test]
async fn the_pipeline_transcribes_through_the_provider_seam() {
    let (_tmp, dir) = sense_voice_dir("sensevoice");
    let (_vad_tmp, vad) = vad_file();
    let options = BTreeMap::new();
    let input = audio_input(&wav_bytes(8000, 2));
    let request = audio_request(&input, &options);
    let budget: CapturedBudget = Arc::new(Mutex::new(None));
    let provider: Arc<dyn EngineProvider> = Arc::new(MockProvider { budget });
    let result = transcribe_with(
        &request,
        Duration::from_secs(120),
        false,
        &models_for(&dir, &vad),
        provider,
    )
    .await
    .unwrap();
    assert_eq!(result.text, "你好世界");
    assert_eq!(result.status, GenerationStatus::Complete);
}

#[tokio::test]
async fn an_explicit_timeout_is_the_hard_deadline() {
    // 100 seconds of audio under an explicit 30s timeout: the deadline is
    // 30s — the audio length never stretches an explicit budget.
    let (_tmp, dir) = sense_voice_dir("sensevoice");
    let (_vad_tmp, vad) = vad_file();
    let options = BTreeMap::new();
    let input = audio_input(&wav_bytes(8000, 100));
    let request = audio_request(&input, &options);
    let budget: CapturedBudget = Arc::new(Mutex::new(None));
    let provider: Arc<dyn EngineProvider> = Arc::new(MockProvider {
        budget: budget.clone(),
    });
    transcribe_with(
        &request,
        Duration::from_secs(30),
        true,
        &models_for(&dir, &vad),
        provider,
    )
    .await
    .unwrap();
    let captured = budget.lock().unwrap().unwrap();
    assert!(
        captured >= Duration::from_secs(29) && captured <= Duration::from_secs(31),
        "expected the explicit 30s budget, got {captured:?}"
    );
}

#[tokio::test]
async fn the_default_timeout_stretches_with_the_audio() {
    // Without an explicit timeout the built-in default (which exists to
    // bound network waits) only seeds the floor: 100s of audio gets
    // audio * 3 = 300s, not 120s.
    let (_tmp, dir) = sense_voice_dir("sensevoice");
    let (_vad_tmp, vad) = vad_file();
    let options = BTreeMap::new();
    let input = audio_input(&wav_bytes(8000, 100));
    let request = audio_request(&input, &options);
    let budget: CapturedBudget = Arc::new(Mutex::new(None));
    let provider: Arc<dyn EngineProvider> = Arc::new(MockProvider {
        budget: budget.clone(),
    });
    transcribe_with(
        &request,
        Duration::from_secs(120),
        false,
        &models_for(&dir, &vad),
        provider,
    )
    .await
    .unwrap();
    let captured = budget.lock().unwrap().unwrap();
    assert!(
        captured >= Duration::from_secs(299) && captured <= Duration::from_secs(301),
        "expected the audio-derived 300s budget, got {captured:?}"
    );
}

#[test]
fn the_model_placeholder_is_ignored_by_this_adapter() {
    // The profile model slot stays cosmetic here: the engine is picked by
    // the profile's asr field (api/mod.rs hands out the same "local"
    // placeholder).
    assert_eq!(crate::api::Adapter::LocalAsr.default_model(), "local");
}

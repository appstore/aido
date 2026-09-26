use super::*;
use asr_core::{ErrorKind, SessionFailure};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Fixtures shared by both test halves: one mono PCM16 wav of `secs`
// seconds (real bytes, so the decode layer runs for real) and the
// stand-in engine the handlers transcribe through (no model files).
// ---------------------------------------------------------------------------

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

/// The budget the stand-in engine was invoked with, plus the failure kind
/// it answers with (`None` transcribes into a fixed two-segment… well,
/// one-segment transcript).
type Captured = std::sync::Arc<std::sync::Mutex<(Option<Duration>, Option<ErrorKind>)>>;

struct MockEngine {
    captured: Captured,
}

impl PreparedEngine for MockEngine {
    fn transcribe(
        &self,
        _buffer: &AudioBuffer,
        _options: SessionOptions,
        deadline: Instant,
    ) -> SessionResult {
        let mut captured = self.captured.lock().unwrap();
        captured.0 = Some(deadline.duration_since(Instant::now()));
        if let Some(kind) = captured.1 {
            return Err(Box::new(SessionFailure {
                error: asr_core::AsrError::new(kind, "engine", "the mock refuses"),
                outcome: SessionOutcome::default(),
            }));
        }
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

fn mock_knobs() -> ServerKnobs {
    ServerKnobs {
        max_samples: 3600 * 48_000,
        timeout: None,
        language: None,
        model_id: "test-model".to_string(),
        family: "SenseVoice".to_string(),
    }
}

/// Bind the router on a loopback port and return its base URL.
async fn spawn(engine: Arc<dyn PreparedEngine>, knobs: ServerKnobs, max_body: usize) -> String {
    // aido builds reqwest without a default rustls provider (it
    // preconfigures ring per client); the test's plain reqwest client
    // needs a process-level one. Harmless when already installed.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(engine, knobs, max_body))
            .await
            .unwrap();
    });
    format!("http://{addr}")
}

async fn upload(base: &str, form: reqwest::multipart::Form) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/audio/transcriptions"))
        .multipart(form)
        .send()
        .await
        .unwrap()
}

fn file_form(bytes: Vec<u8>) -> reqwest::multipart::Form {
    reqwest::multipart::Form::new().part(
        "file",
        reqwest::multipart::Part::bytes(bytes)
            .file_name("clip.wav")
            .mime_str("audio/wav")
            .unwrap(),
    )
}

// ---------------------------------------------------------------------------
// The engine assembly (family detection, profile model resolution) needs
// the sherpa backend — asr-core gates its own model-layout discovery the
// same way — so these tests only exist in local-asr builds.
// ---------------------------------------------------------------------------

#[cfg(feature = "local-asr")]
mod engine_assembly {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// A sense-voice flat layout: model + tokens.txt carrying the
    /// language markers that make the family definitive.
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
        let detected = asr_core::utils::models::detect(&dir).unwrap();
        assert!(matches!(
            detected,
            asr_core::utils::models::LocalModel::Flat
        ));
        let error = resolve_family(detected, None).unwrap_err();
        assert!(format!("{error:#}").contains("pass --family"), "{error:#}");
    }

    #[test]
    fn flat_layout_resolves_via_the_family_flag() {
        let (_tmp, dir) = flat_dir("some-flat-model");
        let detected = asr_core::utils::models::detect(&dir).unwrap();
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
    fn definitive_layouts_map_without_a_family_flag() {
        let (_tmp, dir) = sense_voice_dir("sensevoice");
        let detected = asr_core::utils::models::detect(&dir).unwrap();
        assert!(matches!(
            resolve_family(detected, None).unwrap(),
            OfflineFamily::SenseVoice
        ));

        // A mismatched flag is refused: the markers prove the family.
        let error = resolve_family(detected, Some("transducer")).unwrap_err();
        assert!(format!("{error:#}").contains("but --family"), "{error:#}");
    }

    #[test]
    fn a_punct_directory_is_a_named_mistake() {
        let (_tmp, dir) = punct_dir("punct-model");
        let detected = asr_core::utils::models::detect(&dir).unwrap();
        let error = resolve_family(detected, None).unwrap_err();
        assert!(format!("{error:#}").contains("punctuation model"));
    }

    #[test]
    fn the_no_load_check_flags_the_ambiguous_layout_as_such() {
        let (_tmp, dir) = flat_dir("some-flat-model");
        let (_vad_tmp, vad) = vad_file();
        let models = LocalModels {
            asr: dir,
            vad,
            punct: None,
        };
        // The flat layout is a decision, not a failure: check() turns
        // this into a note, the serve-time resolve into the named error.
        assert!(matches!(
            check_model(&models).unwrap_err(),
            ModelCheckFailure::AmbiguousFamily
        ));
    }

    #[test]
    fn the_no_load_check_passes_a_definitive_layout() {
        let (_tmp, dir) = sense_voice_dir("sensevoice");
        let (_vad_tmp, vad) = vad_file();
        let models = LocalModels {
            asr: dir,
            vad,
            punct: None,
        };
        assert!(check_model(&models).is_ok());
    }

    #[test]
    fn a_missing_vad_file_is_a_named_error() {
        let (_tmp, dir) = sense_voice_dir("sensevoice");
        let models = LocalModels {
            asr: dir,
            vad: PathBuf::from("/nonexistent/silero_vad.onnx"),
            punct: None,
        };
        let error = check_model(&models).unwrap_err();
        let ModelCheckFailure::Failed(error) = error else {
            panic!("expected a failure, got the ambiguity");
        };
        assert!(format!("{error:#}").contains("VAD model not found"));
    }

    #[test]
    fn profile_model_resolution_names_the_missing_field() {
        let profile = crate::config::Profile::default();
        let error = resolve_profile_models("asr", &profile).unwrap_err();
        assert!(format!("{error:#}").contains("sets no 'asr'"), "{error:#}");

        let profile = crate::config::Profile {
            asr: Some("~/models/asr".into()),
            ..Default::default()
        };
        let error = resolve_profile_models("asr", &profile).unwrap_err();
        assert!(format!("{error:#}").contains("sets no 'vad'"), "{error:#}");
    }
}

// ---------------------------------------------------------------------------
// The HTTP surface: real decode, real multipart, real status mapping.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_well_formed_upload_transcribes_to_the_openai_body() {
    let captured: Captured = Captured::default();
    let base = spawn(
        Arc::new(MockEngine {
            captured: captured.clone(),
        }),
        mock_knobs(),
        512 * 1024 * 1024,
    )
    .await;
    let response = upload(&base, file_form(wav_bytes(8000, 2))).await;
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["text"], "你好世界");
}

#[tokio::test]
async fn a_missing_file_field_is_a_named_bad_request() {
    let base = spawn(
        Arc::new(MockEngine {
            captured: Captured::default(),
        }),
        mock_knobs(),
        512 * 1024 * 1024,
    )
    .await;
    let form = reqwest::multipart::Form::new().text("model", "whisper-1");
    let response = upload(&base, form).await;
    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("'file' is required"));
}

#[tokio::test]
async fn only_json_is_a_supported_response_format() {
    let base = spawn(
        Arc::new(MockEngine {
            captured: Captured::default(),
        }),
        mock_knobs(),
        512 * 1024 * 1024,
    )
    .await;
    let form = file_form(wav_bytes(8000, 1)).text("response_format", "verbose_json");
    let response = upload(&base, form).await;
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn the_body_limit_answers_413() {
    let base = spawn(
        Arc::new(MockEngine {
            captured: Captured::default(),
        }),
        mock_knobs(),
        128,
    )
    .await;
    let response = upload(&base, file_form(wav_bytes(8000, 1))).await;
    assert_eq!(response.status(), 413);
}

#[tokio::test]
async fn engine_failures_map_onto_their_status_codes() {
    for (kind, expected) in [
        (ErrorKind::Busy, 503),
        (ErrorKind::Timeout, 504),
        (ErrorKind::InvalidInput, 400),
        (ErrorKind::Backend, 500),
    ] {
        let captured: Captured = Captured::default();
        captured.lock().unwrap().1 = Some(kind);
        let base = spawn(
            Arc::new(MockEngine {
                captured: captured.clone(),
            }),
            mock_knobs(),
            512 * 1024 * 1024,
        )
        .await;
        let response = upload(&base, file_form(wav_bytes(8000, 1))).await;
        assert_eq!(response.status(), expected, "kind {kind:?}");
        let body: serde_json::Value = response.json().await.unwrap();
        assert!(
            body["error"]["message"].as_str().is_some(),
            "kind {kind:?}: the error body names a message"
        );
    }
}

#[tokio::test]
async fn a_pinned_language_matches_or_refuses_the_request() {
    // Pinned "zh": a request saying "zh" transcribes; "en" is refused
    // with the restart hint.
    let mut knobs = mock_knobs();
    knobs.language = Some("zh".to_string());
    let base = spawn(
        Arc::new(MockEngine {
            captured: Captured::default(),
        }),
        knobs,
        512 * 1024 * 1024,
    )
    .await;
    let form = file_form(wav_bytes(8000, 1)).text("language", "zh");
    assert_eq!(upload(&base, form).await.status(), 200);
    let form = file_form(wav_bytes(8000, 1)).text("language", "en");
    let response = upload(&base, form).await;
    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["error"]["message"].as_str().unwrap().contains("zh"));

    // Unpinned: any request language is refused — the engine cannot
    // switch at runtime.
    let base = spawn(
        Arc::new(MockEngine {
            captured: Captured::default(),
        }),
        mock_knobs(),
        512 * 1024 * 1024,
    )
    .await;
    let form = file_form(wav_bytes(8000, 1)).text("language", "en");
    let response = upload(&base, form).await;
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn an_explicit_timeout_is_the_hard_deadline() {
    // 100 seconds of audio under an explicit 30s wall: the engine sees
    // 30s — the audio length never stretches an explicit budget.
    let captured: Captured = Captured::default();
    let mut knobs = mock_knobs();
    knobs.timeout = Some(Duration::from_secs(30));
    let base = spawn(
        Arc::new(MockEngine {
            captured: captured.clone(),
        }),
        knobs,
        512 * 1024 * 1024,
    )
    .await;
    upload(&base, file_form(wav_bytes(8000, 100))).await;
    let budget = captured.lock().unwrap().0.unwrap();
    assert!(
        budget >= Duration::from_secs(29) && budget <= Duration::from_secs(31),
        "expected the explicit 30s wall, got {budget:?}"
    );
}

#[tokio::test]
async fn the_default_policy_stretches_with_the_audio() {
    // Without --timeout-secs the built-in policy only seeds the floor:
    // 100s of audio gets audio * 3 = 300s, not 120s.
    let captured: Captured = Captured::default();
    let base = spawn(
        Arc::new(MockEngine {
            captured: captured.clone(),
        }),
        mock_knobs(),
        512 * 1024 * 1024,
    )
    .await;
    upload(&base, file_form(wav_bytes(8000, 100))).await;
    let budget = captured.lock().unwrap().0.unwrap();
    assert!(
        budget >= Duration::from_secs(299) && budget <= Duration::from_secs(301),
        "expected the audio-derived 300s budget, got {budget:?}"
    );
}

#[tokio::test]
async fn health_and_models_report_the_loaded_engine() {
    let base = spawn(
        Arc::new(MockEngine {
            captured: Captured::default(),
        }),
        mock_knobs(),
        512 * 1024 * 1024,
    )
    .await;
    let client = reqwest::Client::new();
    let health: serde_json::Value = client
        .get(format!("{base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["status"], "ok");
    assert_eq!(health["model"], "test-model");
    let models: serde_json::Value = client
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(models["data"][0]["id"], "test-model");
}

/// The decode budget is the decode layer's input: a wav longer than the
/// knob allows is a 400, before the engine ever sees it.
#[tokio::test]
async fn the_decode_budget_refuses_over_long_audio() {
    let mut knobs = mock_knobs();
    // 1 second at 8 kHz: a 2-second wav (16k samples) exceeds the
    // 8_000-sample budget.
    knobs.max_samples = 8_000;
    let base = spawn(
        Arc::new(MockEngine {
            captured: Captured::default(),
        }),
        knobs,
        512 * 1024 * 1024,
    )
    .await;
    let response = upload(&base, file_form(wav_bytes(8000, 2))).await;
    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("budget"));
}

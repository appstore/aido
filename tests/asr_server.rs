//! The ASR server through its public seam (`aido::serve::asr::router`):
//! a mock engine stands in for the sherpa stack — no model files — while
//! everything else (router, multipart handling, status mapping) is the
//! real thing the production binary serves.
#![cfg(feature = "asr-server")]

use aido::serve::asr::{
    router, AudioBuffer, PreparedEngine, Segment, ServerKnobs, SessionOptions, SessionOutcome,
    SessionResult, Transcript,
};
use std::sync::Arc;
use std::time::Instant;

struct MockEngine;

impl PreparedEngine for MockEngine {
    fn transcribe(
        &self,
        _buffer: &AudioBuffer,
        _options: SessionOptions,
        _deadline: Instant,
    ) -> SessionResult {
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

fn knobs() -> ServerKnobs {
    ServerKnobs {
        max_samples: 3600 * 48_000,
        timeout: None,
        language: None,
        model_id: "integration-model".to_string(),
        family: "SenseVoice".to_string(),
    }
}

/// One mono PCM16 wav: real bytes, so the decode layer runs for real.
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
    // PCM silence: real samples for the decode layer.
    out.resize(44 + data_len as usize, 0);
    out
}

#[tokio::test]
async fn the_public_router_transcribes_over_openai_compatible_http() {
    // aido builds reqwest without a default rustls provider (it
    // preconfigures ring per client); the test's plain reqwest client
    // needs a process-level one. Harmless when already installed.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            listener,
            router(Arc::new(MockEngine), knobs(), 512 * 1024 * 1024),
        )
        .await
        .unwrap();
    });

    let form = reqwest::multipart::Form::new().part(
        "file",
        reqwest::multipart::Part::bytes(wav_bytes(8000, 1))
            .file_name("clip.wav")
            .mime_str("audio/wav")
            .unwrap(),
    );
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/audio/transcriptions"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["text"], "你好世界");

    // A transcription without material is the named bad request the
    // OpenAI error shape carries.
    let form = reqwest::multipart::Form::new().text("model", "whisper-1");
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/audio/transcriptions"))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("'file' is required"));
}

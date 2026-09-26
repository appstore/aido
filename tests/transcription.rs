//! Real CLI + multipart HTTP: checkpoints outlive a failed process.
#![cfg(feature = "audio-decode")]
mod support;
use support::*;

fn recording() -> Vec<u8> {
    recording_seconds(3)
}

fn recording_seconds(seconds: u32) -> Vec<u8> {
    let rate = 8000u32;
    let len = rate * seconds * 2;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(len + 36).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&rate.to_le_bytes());
    bytes.extend_from_slice(&(rate * 2).to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&len.to_le_bytes());
    for _ in 0..rate * seconds {
        bytes.extend_from_slice(&8000i16.to_le_bytes());
    }
    bytes
}

#[test]
fn failed_segment_resumes_without_resending_completed_audio() {
    let server = MultiServer::start_statuses(&[
        ("200 OK", r#"{"text":"重复。"}"#),
        ("504 Gateway Timeout", r#"{"error":"slow transcription"}"#),
        ("200 OK", r#"{"text":"重复。"}"#),
        ("200 OK", r#"{"text":"结束。"}"#),
    ]);
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"whisper-1\"\noperations = [\"transcribe\"]\n\
         [providers.srv]\nbase_url = \"{}\"", server.url()
    ));
    let dir = tempfile::tempdir().unwrap();
    let audio = dir.path().join("meeting.wav");
    std::fs::write(&audio, recording()).unwrap();
    let state = dir.path().join("state");
    let args = [
        "transcribe",
        "--profile",
        "test",
        audio.to_str().unwrap(),
        "--audio-chunk-secs",
        "1",
        "--transcribe-state",
        state.to_str().unwrap(),
        "--no-history",
    ];
    let env = [("AIDO_CONFIG", cfg.to_str().unwrap())];
    let mut dry_args = args.to_vec();
    dry_args.push("--dry-run");
    run(&dry_args, b"", &env).assert_code(0);
    assert!(!state.exists());
    let first = run(&args, b"", &env);
    first.assert_code(3);
    assert!(first.stdout().is_empty());
    assert!(state.join("segment-000000.json").exists());
    assert!(!state.join("segment-000001.json").exists());
    let resumed = run(&args, b"", &env);
    resumed.assert_code(0);
    assert_eq!(resumed.stdout().matches("重复。").count(), 2);
    assert!(resumed.stdout().ends_with("结束。\n"));
    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    for raw in &requests {
        let raw = String::from_utf8_lossy(raw);
        assert!(raw.contains("audio/wav"));
        assert!(raw.contains("input.wav"));
    }
    // Server is gone: all three cached replies must reconstruct the same output.
    let cached = run(&args, b"", &env);
    cached.assert_code(0);
    assert_eq!(cached.stdout(), resumed.stdout());
    let mut changed = args.to_vec();
    changed.extend(["--option", "language=en"]);
    let refused = run(&changed, b"", &env);
    assert_ne!(refused.code(), 0);
    assert!(refused.stderr().contains("does not match"));
    std::fs::write(&audio, {
        let mut changed = recording();
        *changed.last_mut().unwrap() = 2;
        changed
    })
    .unwrap();
    let refused = run(&args, b"", &env);
    assert_ne!(refused.code(), 0);
    assert!(refused.stderr().contains("does not match"));
}

#[test]
fn defaults_split_and_resume_without_extra_flags() {
    let server = MultiServer::start_statuses(&[
        ("200 OK", r#"{"text":"first"}"#),
        ("504 Gateway Timeout", r#"{"error":"timeout"}"#),
        ("200 OK", r#"{"text":"second"}"#),
        ("200 OK", r#"{"text":"third"}"#),
    ]);
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"whisper-1\"\noperations = [\"transcribe\"]\n[providers.srv]\nbase_url = \"{}\"", server.url()
    ));
    let dir = tempfile::tempdir().unwrap();
    let audio = dir.path().join("meeting.wav");
    std::fs::write(&audio, recording_seconds(121)).unwrap();
    let history = dir.path().join("history");
    let cache = dir.path().join("history-transcription");
    let env = [
        ("AIDO_CONFIG", cfg.to_str().unwrap()),
        ("AIDO_HISTORY_DIR", history.to_str().unwrap()),
    ];
    let args = ["transcribe", "--profile", "test", audio.to_str().unwrap()];
    let mut dry = args.to_vec();
    dry.push("--dry-run");
    let preview = run(&dry, b"", &env);
    preview.assert_code(0);
    assert!(preview.stdout().contains("transcription state:"));
    assert!(preview.stdout().contains("60.0"));
    assert!(!cache.exists());
    run(&args, b"", &env).assert_code(3);
    let state = std::fs::read_dir(&cache)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(state.join("segment-000000.json").exists());
    assert!(!state.join("segment-000001.json").exists());
    let resumed = run(&args, b"", &env);
    resumed.assert_code(0);
    assert_eq!(resumed.stdout(), "first\n\nsecond\n\nthird\n");
    assert_eq!(server.requests().len(), 4);
    let cached = run(&args, b"", &env);
    cached.assert_code(0);
    assert_eq!(cached.stdout(), resumed.stdout());
    // Different request parameters choose a different automatic directory.
    let mut changed = dry.clone();
    changed.extend(["--option", "language=en"]);
    let changed = run(&changed, b"", &env);
    changed.assert_code(0);
    let state_line = |text: String| {
        text.lines()
            .find(|l| l.starts_with("transcription state:"))
            .unwrap()
            .to_owned()
    };
    assert_ne!(state_line(preview.stdout()), state_line(changed.stdout()));
    assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 1);
}

#[test]
fn no_history_disables_automatic_reuse_and_no_split_sends_one_request() {
    let server = MultiServer::start(&[r#"{"text":"ok"}"#; 7]);
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"whisper-1\"\noperations = [\"transcribe\"]\n[providers.srv]\nbase_url = \"{}\"", server.url()
    ));
    let dir = tempfile::tempdir().unwrap();
    let audio = dir.path().join("meeting.wav");
    std::fs::write(&audio, recording_seconds(121)).unwrap();
    let history = dir.path().join("history");
    let env = [
        ("AIDO_CONFIG", cfg.to_str().unwrap()),
        ("AIDO_HISTORY_DIR", history.to_str().unwrap()),
    ];
    let args = [
        "transcribe",
        "--profile",
        "test",
        audio.to_str().unwrap(),
        "--no-history",
    ];
    run(&args, b"", &env).assert_code(0);
    run(&args, b"", &env).assert_code(0);
    assert!(!dir.path().join("history-transcription").exists());
    let mut unsplit = args[..args.len() - 1].to_vec();
    unsplit.push("--no-split");
    run(&unsplit, b"", &env).assert_code(0);
    assert_eq!(server.requests().len(), 7);
    assert!(!dir.path().join("history-transcription").exists());
}

#[test]
fn explicit_state_uses_default_duration_for_short_audio() {
    let server = Server::json(r#"{"text":"short"}"#);
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"whisper-1\"\noperations = [\"transcribe\"]\n[providers.srv]\nbase_url = \"{}\"", server.url()
    ));
    let dir = tempfile::tempdir().unwrap();
    let audio = dir.path().join("short.wav");
    std::fs::write(&audio, recording()).unwrap();
    let state = dir.path().join("state");
    let args = [
        "transcribe",
        "--profile",
        "test",
        audio.to_str().unwrap(),
        "--transcribe-state",
        state.to_str().unwrap(),
        "--no-history",
    ];
    let env = [("AIDO_CONFIG", cfg.to_str().unwrap())];
    run(&args, b"", &env).assert_code(0);
    server.request();
    run(&args, b"", &env).assert_code(0);
    assert!(state.join("segment-000000.json").exists());
}

#[test]
fn short_audio_stays_unsplit_and_is_not_automatically_cached() {
    let server = MultiServer::start(&[r#"{"text":"short"}"#; 2]);
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"whisper-1\"\noperations = [\"transcribe\"]\n[providers.srv]\nbase_url = \"{}\"", server.url()
    ));
    let dir = tempfile::tempdir().unwrap();
    let audio = dir.path().join("short.wav");
    let original = recording();
    std::fs::write(&audio, &original).unwrap();
    let history = dir.path().join("history");
    let args = ["transcribe", "--profile", "test", audio.to_str().unwrap()];
    let env = [
        ("AIDO_CONFIG", cfg.to_str().unwrap()),
        ("AIDO_HISTORY_DIR", history.to_str().unwrap()),
    ];
    run(&args, b"", &env).assert_code(0);
    run(&args, b"", &env).assert_code(0);
    for request in server.requests() {
        assert!(request
            .windows(original.len())
            .any(|window| window == original));
    }
    assert!(!dir.path().join("history-transcription").exists());
}

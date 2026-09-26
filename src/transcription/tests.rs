use super::*;

#[cfg(feature = "audio-decode")]
#[test]
fn silence_cut_preserves_samples_and_never_exceeds_budget() {
    let mut samples = vec![0.5; 1250];
    samples[540..580].fill(0.0);
    let cuts = boundaries(&samples, 100, 6);
    assert_eq!(cuts[0], (0, 570));
    assert_eq!(cuts.last().unwrap().1, samples.len());
    assert!(cuts.iter().all(|(a, b)| b > a && b - a <= 600));
    assert!(cuts.windows(2).all(|w| w[0].1 == w[1].0));
    assert_eq!(
        boundaries(&[0.5; 1250], 100, 6),
        vec![(0, 600), (600, 1200), (1200, 1250)]
    );
}

#[cfg(feature = "audio-decode")]
#[test]
fn wav_segments_decode_and_keep_duration() {
    let bytes = wav(&[0.25; 16000], 16000).unwrap();
    let decoded = crate::audio::decode_mono(&bytes, 16000).unwrap();
    assert_eq!(decoded.samples.len(), 16000);
    assert_eq!(decoded.spec.sample_rate, 16000);
    assert!((decoded.samples[100] - 0.25).abs() < 0.001);
}

#[test]
fn committed_replies_survive_restart_and_incomplete_replies_are_not_saved() {
    let dir = tempfile::tempdir().unwrap();
    let state = Checkpoint {
        dir: dir.path().to_owned(),
    };
    state
        .save(0, &GenerateResult::complete_with_text("重复。重复。"))
        .unwrap();
    let restarted = Checkpoint {
        dir: dir.path().to_owned(),
    };
    assert_eq!(restarted.load(0).unwrap().unwrap().text, "重复。重复。");
    assert!(restarted.load(1).unwrap().is_none());
    let mut partial = GenerateResult::complete_with_text("partial");
    partial.status = crate::domain::GenerationStatus::Incomplete {
        reason: "timeout".into(),
    };
    restarted.save(1, &partial).unwrap();
    assert!(restarted.load(1).unwrap().is_none());
    state
        .save(0, &GenerateResult::complete_with_text("replacement"))
        .unwrap();
    assert_eq!(state.load(0).unwrap().unwrap().text, "重复。重复。");
    std::fs::write(dir.path().join("segment-000002.json"), b"{").unwrap();
    assert!(state.load(2).is_err());
}

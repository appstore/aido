use super::{check_model, resolve_family};
use asr_core::utils::models::{detect, LocalModel};
use asr_core::OfflineFamily;
use std::fs;
use std::path::PathBuf;
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
    let vad = TempDir::new().unwrap();
    fs::write(vad.path().join("vad.onnx"), b"fake-vad").unwrap();
    let vad = vad.path().join("vad.onnx").to_string_lossy().into_owned();
    let models = |vad: Option<String>| super::LocalModels {
        asr: Some(dir.to_string_lossy().into_owned()),
        vad,
        punct: None,
    };
    // The ambiguity is refused without the option…
    let error = check_model(&models(Some(vad.clone())), None).unwrap_err();
    assert!(format!("{error:#}").contains("set option 'family'"));
    // …and passes with it.
    assert!(check_model(&models(Some(vad)), Some("firered-ctc")).is_ok());
}

#[test]
fn a_missing_vad_file_is_a_named_error() {
    let (_tmp, dir) = sense_voice_dir("sensevoice");
    let models = super::LocalModels {
        asr: Some(dir.to_string_lossy().into_owned()),
        vad: Some("/nonexistent/silero_vad.onnx".into()),
        punct: None,
    };
    let error = check_model(&models, None).unwrap_err();
    assert!(format!("{error:#}").contains("VAD model not found"));

    // A profile without a vad field at all says so.
    let models = super::LocalModels {
        asr: Some(dir.to_string_lossy().into_owned()),
        vad: None,
        punct: None,
    };
    let error = check_model(&models, None).unwrap_err();
    assert!(format!("{error:#}").contains("sets no vad"));
}

#[test]
fn the_model_placeholder_is_ignored_by_this_adapter() {
    // The profile model slot stays cosmetic here: the engine is picked by
    // model_dir (api/mod.rs hands out the same "local" placeholder).
    assert_eq!(crate::api::Adapter::LocalAsr.default_model(), "local");
}

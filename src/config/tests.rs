use super::*;

/// One provider + one profile over it, the profile carrying the given
/// local-model fields. The provider's base_url keeps the conventional
/// adapters (whatever the profile's other operations fall back to)
/// covered.
fn serve_profile_cfg(asr: Option<&str>, vad: Option<&str>) -> Config {
    let mut cfg = Config::default();
    cfg.providers.insert(
        "p".into(),
        Provider {
            base_url: Some("https://example.test/v1".into()),
            api_key_env: None,
            routes: BTreeMap::new(),
        },
    );
    cfg.profiles.insert(
        "user".into(),
        Profile {
            provider: Some("p".into()),
            model: Some("m".into()),
            asr: asr.map(str::to_string),
            vad: vad.map(str::to_string),
            ..Default::default()
        },
    );
    cfg.default_profile = Some("user".into());
    cfg
}

#[test]
fn profiles_without_local_model_fields_are_not_asked_for_them() {
    let issues = check(&serve_profile_cfg(None, None));
    assert!(issues.is_empty(), "{issues:?}");
}

#[cfg(feature = "local-asr")]
mod with_server {
    use super::*;

    #[test]
    fn a_partial_field_set_is_an_issue() {
        let issues = check(&serve_profile_cfg(Some("~/models/asr"), None));
        assert!(
            issues.iter().any(|issue| issue.contains("sets no 'vad'")),
            "{issues:?}"
        );
    }

    #[test]
    fn a_broken_model_directory_is_an_issue_while_editing() {
        let issues = check(&serve_profile_cfg(
            Some("/nonexistent/asr-dir"),
            Some("/tmp"),
        ));
        assert!(
            issues
                .iter()
                .any(|issue| issue.contains("not a usable ASR model directory")),
            "{issues:?}"
        );
    }
}

#[cfg(not(feature = "local-asr"))]
#[test]
fn serve_model_fields_on_a_binary_without_the_engine_are_flagged() {
    let issues = check(&serve_profile_cfg(
        Some("~/models/asr"),
        Some("~/models/vad.onnx"),
    ));
    assert!(
        issues
            .iter()
            .any(|issue| issue.contains("no local ASR engine")),
        "{issues:?}"
    );
}

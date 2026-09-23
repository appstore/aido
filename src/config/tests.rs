use super::*;

/// One provider routing transcribe to the local engine, plus one profile
/// over it with the given operations. The provider's base_url keeps the
/// conventional adapters (whatever the profile's other operations fall
/// back to) covered.
fn local_route_cfg(operations: Option<Vec<Operation>>) -> Config {
    let mut cfg = Config::default();
    cfg.providers.insert(
        "p".into(),
        Provider {
            base_url: Some("https://example.test/v1".into()),
            api_key_env: None,
            routes: BTreeMap::from([("transcribe".to_string(), Adapter::LocalAsr)]),
        },
    );
    cfg.profiles.insert(
        "user".into(),
        Profile {
            provider: Some("p".into()),
            model: Some("m".into()),
            operations,
            ..Default::default()
        },
    );
    cfg.default_profile = Some("user".into());
    cfg
}

#[test]
fn profiles_that_never_transcribe_skip_the_local_model_check() {
    // A generate-only profile resolves nothing to the local-asr adapter,
    // so the provider's transcribe route cannot demand model files from
    // it — the same operations filter the base_url judgment uses.
    let issues = check(&local_route_cfg(Some(vec![Operation::Generate])));
    assert!(issues.is_empty(), "{issues:?}");
}

#[cfg(not(feature = "local-asr"))]
#[test]
fn transcribing_profiles_on_a_local_route_are_refused_in_this_binary() {
    let issues = check(&local_route_cfg(Some(vec![Operation::Transcribe])));
    assert!(
        issues.iter().any(|issue| issue.contains("local-asr")),
        "{issues:?}"
    );
}

#[cfg(feature = "local-asr")]
#[test]
fn transcribing_profiles_on_a_local_route_get_the_model_check() {
    // The feature-on counterpart: the profile's own fields go through the
    // same precheck a run performs — a missing 'asr' field is the named
    // issue, without loading any model.
    let issues = check(&local_route_cfg(Some(vec![Operation::Transcribe])));
    assert!(
        issues.iter().any(|issue| issue.contains("sets no 'asr'")),
        "{issues:?}"
    );
}

use super::*;
use crate::domain::ErrorKind;

fn step(index: usize, part: Option<usize>) -> RequestStep {
    RequestStep {
        index,
        inputs: Vec::new(),
        label: "all material".into(),
        hard_cut_end: false,
        part,
        artifact_stem: None,
        role: crate::processors::StepRole::Map,
    }
}

#[test]
fn interleaved_parts_fail_naming_the_step_that_returns() {
    let steps = vec![step(0, Some(0)), step(1, Some(1)), step(2, Some(0))];
    let err = assert_parts_contiguous(&steps).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Usage);
    assert!(
        err.message.contains("step 2 returns to part 0"),
        "message: {}",
        err.message
    );
    assert!(
        err.message.contains("processor bug"),
        "message: {}",
        err.message
    );
}

#[test]
fn contiguous_parts_pass_even_with_untagged_steps_mixed_in() {
    // A multi-step part repeats its id (chunk strategies plan several
    // requests per part); None-part steps are the non-batch path and
    // constrain nothing.
    let steps = vec![
        step(0, None),
        step(1, Some(0)),
        step(2, Some(0)),
        step(3, None),
        step(4, Some(1)),
        step(5, Some(2)),
        step(6, None),
    ];
    assert_parts_contiguous(&steps).unwrap();
}

#[test]
fn empty_and_untagged_step_lists_pass() {
    assert_parts_contiguous(&[]).unwrap();
    assert_parts_contiguous(&[step(0, None), step(1, None)]).unwrap();
}

fn junction_task(name: &str, max_inputs: Option<usize>) -> Task {
    Task {
        name: name.into(),
        operation: crate::tasks::Operation::Generate,
        instruction: String::new(),
        profile: None,
        input_types: None,
        required_types: Vec::new(),
        max_inputs,
        output_types: vec![MediaKind::Text],
        requires_material: true,
        processor: ProcessorKind::Single,
        per_part: false,
        params: Vec::new(),
        defaults: Default::default(),
        options: Default::default(),
        builtin: true,
    }
}

fn chat_resolved() -> Resolved {
    Resolved {
        profile_name: "p".into(),
        provider_name: "p".into(),
        adapter: crate::api::Adapter::Chat,
        base_url: None,
        api_key_env: None,
        model: "m".into(),
        model_source: crate::config::resolve::ParamSource::Default,
        max_tokens: None,
        max_tokens_source: crate::config::resolve::ParamSource::Default,
        temperature: None,
        temperature_source: crate::config::resolve::ParamSource::Default,
        options: Default::default(),
        allowed_inputs: None,
        required_inputs: Vec::new(),
        produce: vec![MediaKind::Text],
    }
}

/// The junction hands exactly one text part downstream, so even the
/// count rule is a parse-time fact: a downstream `max_inputs = 0`
/// fails the junction before stage 1's paid request.
#[test]
fn junction_input_rejects_a_zero_max_inputs_downstream() {
    let task = junction_task("solo", Some(0));
    let err = validate_junction_input(&task, &chat_resolved()).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Usage);
    assert!(err.message.contains("accepts at most 0 input(s)"), "{err}");

    // One text input is exactly the junction's shape.
    let task = junction_task("normal", Some(1));
    validate_junction_input(&task, &chat_resolved()).unwrap();
}

#[test]
fn junction_input_rejects_a_stage_that_refuses_text() {
    let mut resolved = chat_resolved();
    resolved.allowed_inputs = Some(vec![MediaKind::Audio]);
    let task = junction_task("audio-only", None);
    let err = validate_junction_input(&task, &resolved).unwrap_err();
    assert!(err.message.contains("does not accept text input"), "{err}");
}

/// Without the edge-tts feature the adapter variant still parses (a
/// config naming it is readable), but preflight must refuse it: a
/// chain would otherwise pay stage 1 before stage 2's plan build
/// discovered the missing adapter.
#[cfg(not(feature = "edge-tts"))]
#[test]
fn preflight_refuses_an_edge_tts_route_without_the_feature() {
    let resolved = Resolved {
        adapter: crate::api::Adapter::EdgeTts,
        produce: vec![MediaKind::Audio],
        ..chat_resolved()
    };
    let err = validate_adapter_availability(&resolved).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Usage);
    assert!(
        err.message.contains(crate::api::EDGE_TTS_NOT_COMPILED),
        "{err}"
    );
}
#[test]
fn redact_url_hides_userinfo_credentials() {
    assert_eq!(
        redact_url("https://user:s3cret@gw.internal/v1"),
        "https://***:***@gw.internal/v1"
    );
    assert_eq!(
        redact_url("https://user@gw.internal/v1"),
        "https://***@gw.internal/v1"
    );
}
#[test]
fn redact_url_still_masks_query_values() {
    // the `…` marker is percent-encoded when the URL is serialized
    assert_eq!(
        redact_url("https://user:pw@gw.internal/v1?key=topsecret&x=1"),
        "https://***:***@gw.internal/v1?key=%E2%80%A6&x=%E2%80%A6"
    );
}
#[test]
fn redact_url_leaves_plain_urls_alone_apart_from_query() {
    assert_eq!(
        redact_url("https://gw.internal/v1"),
        "https://gw.internal/v1"
    );
    assert_eq!(
        redact_url("https://gw.internal/v1?key=secret"),
        "https://gw.internal/v1?key=%E2%80%A6"
    );
}
#[test]
fn redact_url_hides_unparseable_input() {
    assert_eq!(
        redact_url("not a url at all"),
        "(unparseable base_url, hidden)"
    );
}

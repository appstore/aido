use super::*;

fn stage(task: &str, produce: Vec<MediaKind>, allowed: Option<Vec<MediaKind>>) -> StageParsed {
    // A minimal stage: only the fields the junction checks read.
    let resolved = Resolved {
        profile_name: "default".into(),
        provider_name: "p".into(),
        adapter: crate::api::Adapter::Chat,
        base_url: None,
        api_key_env: None,
        #[cfg(feature = "local-asr")]
        local_models: None,
        model: "m".into(),
        model_source: crate::config::resolve::ParamSource::Default,
        max_tokens: None,
        max_tokens_source: crate::config::resolve::ParamSource::Default,
        temperature: None,
        temperature_source: crate::config::resolve::ParamSource::Default,
        options: Default::default(),
        allowed_inputs: allowed,
        required_inputs: Vec::new(),
        produce: produce.clone(),
    };
    let task = Task {
        name: task.into(),
        operation: crate::tasks::Operation::Generate,
        instruction: String::new(),
        profile: None,
        input_types: None,
        required_types: Vec::new(),
        max_inputs: None,
        output_types: produce,
        requires_material: true,
        processor: ProcessorKind::Single,
        per_part: false,
        params: Vec::new(),
        defaults: Default::default(),
        options: Default::default(),
        builtin: true,
    };
    let cli = Cli::try_parse_from(["aido", "--__task", task.name.as_str()]).unwrap();
    StageParsed {
        task,
        cli,
        specs: Vec::new(),
        resolved,
    }
}

#[test]
fn text_junctions_pass_and_media_ones_fail() {
    let mut stages = vec![
        stage("ocr", vec![MediaKind::Text], None),
        stage("tts", vec![MediaKind::Audio], Some(vec![MediaKind::Text])),
    ];
    validate_chain_types(&stages).unwrap();

    // A downstream stage that refuses text kills the junction.
    stages[1].resolved.allowed_inputs = Some(vec![MediaKind::Audio]);
    let err = validate_chain_types(&stages).unwrap_err();
    assert!(err.message.contains("does not accept text input"), "{err}");
    assert_eq!(err.kind, crate::domain::ErrorKind::Usage);
}

#[test]
fn non_text_handoff_is_rejected_before_the_last_stage() {
    let stages = vec![
        stage("image", vec![MediaKind::Image], None),
        stage("tts", vec![MediaKind::Audio], Some(vec![MediaKind::Text])),
    ];
    let err = validate_chain_types(&stages).unwrap_err();
    assert!(err.message.contains("exactly text"), "{err}");
    assert!(err.message.contains("→ stage 2 (tts)"), "{err}");
}

#[test]
fn required_types_must_be_produced_upstream() {
    // The downstream stage accepts text at large (no allow-list) but
    // declares audio as required: the missing kind is its own error.
    let mut down = stage("transcribe", vec![MediaKind::Text], None);
    down.task.required_types = vec![MediaKind::Audio];
    let stages = vec![stage("ocr", vec![MediaKind::Text], None), down];
    let err = validate_chain_types(&stages).unwrap_err();
    assert!(err.message.contains("requires audio input"), "{err}");
}

#[test]
fn run_level_conflicts_are_usage_errors() {
    let mut stages = vec![
        stage("ocr", vec![MediaKind::Text], None),
        stage("tts", vec![MediaKind::Audio], Some(vec![MediaKind::Text])),
    ];
    stages[0].cli.output = Some("/tmp/a.png".into());
    stages[1].cli.output = Some("/tmp/b.mp3".into());
    let err = merge_run_flags(&mut stages).unwrap_err();
    assert!(err.message.contains("--output"), "{err}");

    // Same value on both stages passes.
    stages[1].cli.output = Some("/tmp/a.png".into());
    merge_run_flags(&mut stages).unwrap();
    assert_eq!(stages[1].cli.output.as_deref(), Some("/tmp/a.png".as_ref()));
}

#[test]
fn stream_pair_conflicts_across_stages() {
    let mut stages = vec![
        stage("ocr", vec![MediaKind::Text], None),
        stage("tts", vec![MediaKind::Audio], Some(vec![MediaKind::Text])),
    ];
    stages[0].cli.no_stream = true;
    stages[1].cli.stream = true;
    let err = merge_run_flags(&mut stages).unwrap_err();
    assert!(err.message.contains("--stream"), "{err}");
}

#[test]
fn run_level_flags_merge_into_the_last_stage() {
    let mut stages = vec![
        stage("ocr", vec![MediaKind::Text], None),
        stage("tts", vec![MediaKind::Audio], Some(vec![MediaKind::Text])),
    ];
    stages[0].cli.json = true;
    stages[0].cli.quiet = true;
    stages[0].cli.total_timeout = Some(30);
    merge_run_flags(&mut stages).unwrap();
    let last = &stages[1].cli;
    assert!(last.json && last.quiet);
    assert_eq!(last.total_timeout, Some(30));
}

#[test]
fn delivery_flags_leave_the_source_stage_and_mode_flags_reach_every_stage() {
    let mut stages = vec![
        stage("ocr", vec![MediaKind::Text], None),
        stage("summarize", vec![MediaKind::Text], None),
        stage("tts", vec![MediaKind::Audio], Some(vec![MediaKind::Text])),
    ];
    // A delivery flag and a run-mode flag on stage 1 (the --then
    // shape): the delivery flag belongs to the last stage alone — a
    // leftover --out-dir would let a real per-part batch run as an
    // intermediate — while the run-mode flag describes every stage.
    stages[0].cli.out_dir = Some("/tmp/x".into());
    stages[0].cli.no_stream = true;
    merge_run_flags(&mut stages).unwrap();
    assert!(
        stages[0].cli.out_dir.is_none() && stages[1].cli.out_dir.is_none(),
        "delivery flags stripped from non-last stages"
    );
    assert_eq!(stages[2].cli.out_dir.as_deref(), Some("/tmp/x".as_ref()));
    assert!(
        stages[0].cli.no_stream && stages[1].cli.no_stream && stages[2].cli.no_stream,
        "run-mode flags propagate to every stage"
    );
}

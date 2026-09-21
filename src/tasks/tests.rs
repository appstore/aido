use super::*;

#[test]
fn builtin_tasks_load_and_are_consistent() {
    let all = load_all().unwrap();
    for name in [
        "ask",
        "code-review",
        "ocr",
        "summarize",
        "transcribe",
        "translate",
        "tts",
        "image",
    ] {
        let task = all.get(name).unwrap_or_else(|| panic!("missing {name}"));
        assert!(!task.output_types.is_empty());
    }
    let ocr = &all["ocr"];
    assert_eq!(ocr.processor, ProcessorKind::OcrTiles);
    assert!(ocr.required_types.contains(&MediaKind::Image));
    // The two text strategies are separate now: translate joins the
    // chunk replies, summarize consolidates them in one more request.
    assert_eq!(all["translate"].processor, ProcessorKind::ChunkJoin);
    assert_eq!(all["summarize"].processor, ProcessorKind::ChunkReduce);
    let tts = &all["tts"];
    assert_eq!(tts.operation, Operation::Speech);
    // tts is useless without material to speak
    assert!(tts.requires_material);
    // ask runs without material
    assert!(!all["ask"].requires_material);
    // transcribe allows exactly one audio
    let tr = &all["transcribe"];
    assert_eq!(tr.max_inputs, Some(1));
}

#[test]
fn unknown_fields_are_rejected_with_context() {
    let err = parse_task(
        "x",
        "operation = 'generate'\noutput_types = ['text']\nsistem = 'x'\n",
        false,
    )
    .unwrap_err();
    assert!(err.to_string().contains("x"));
}

#[test]
fn non_defaultable_defaults_keys_are_rejected_with_their_destinations() {
    // speed is a real parameter but CLI/options-only: a task default
    // for it was silently ignored before.
    let err = parse_task(
        "x",
        "operation = 'speech'\noutput_types = ['audio']\nparams = ['voice', 'speed']\n\
             [defaults]\nspeed = 1.2\n",
        false,
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("speed"), "{msg}");
    assert!(msg.contains("[options]"), "{msg}");

    // voice defaults only when the task declares the parameter.
    let err = parse_task(
        "x",
        "operation = 'speech'\noutput_types = ['audio']\nparams = ['speed']\n\
             [defaults]\nvoice = 'alloy'\n",
        false,
    )
    .unwrap_err();
    assert!(err.to_string().contains("voice"), "{}", err.to_string());

    // the defaultable pair keeps working: declared to with a default.
    let task = parse_task(
        "x",
        "operation = 'generate'\noutput_types = ['text']\nparams = ['to']\n\
             [defaults]\nto = 'auto'\n",
        false,
    )
    .unwrap();
    assert_eq!(task.default_param("to").unwrap(), "auto");
}

#[test]
fn processor_names_parse_with_the_old_name_aliasing_join() {
    let parse = |processor: &str| {
        parse_task(
            "x",
            &format!(
                "operation = 'generate'\noutput_types = ['text']\nprocessor = '{processor}'\n"
            ),
            false,
        )
        .unwrap()
        .processor
    };
    assert_eq!(parse("chunk-join"), ProcessorKind::ChunkJoin);
    assert_eq!(parse("chunk-reduce"), ProcessorKind::ChunkReduce);
    // The pre-split name keeps its old meaning: join, no reduce.
    assert_eq!(parse("chunk-map-reduce"), ProcessorKind::ChunkJoin);
}

#[test]
fn closest_suggests() {
    assert_eq!(
        closest("transalte", &["translate", "ocr"]),
        Some("translate")
    );
}

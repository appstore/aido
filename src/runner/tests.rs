use super::*;

fn resolved(adapter: Adapter, model: &str) -> Resolved {
    Resolved {
        profile_name: "default".into(),
        provider_name: "deepseek".into(),
        adapter,
        base_url: None,
        api_key_env: None,
        vad: None,
        punct: None,
        model_dir: None,
        model: model.into(),
        model_source: crate::config::resolve::ParamSource::Profile,
        max_tokens: None,
        max_tokens_source: crate::config::resolve::ParamSource::Default,
        temperature: None,
        temperature_source: crate::config::resolve::ParamSource::Default,
        options: Default::default(),
        allowed_inputs: None,
        required_inputs: Vec::new(),
        produce: Vec::new(),
    }
}

#[test]
fn spinner_prefix_names_the_model_except_when_the_adapter_owns_the_endpoint() {
    // A chat run's spinner asks the model that answers.
    assert_eq!(
        spinner_prefix(&resolved(Adapter::Chat, "glm-4.6")),
        "asking glm-4.6"
    );
    // Speech is synthesized, not asked for — both speech adapters say
    // so. The OpenAI adapter sends the model, so it stays in the line.
    assert_eq!(
        spinner_prefix(&resolved(Adapter::Speech, "tts-1")),
        "synthesizing speech (tts-1)"
    );
    // A speech run routed to edge-tts never sends the profile's model:
    // the spinner names the engine that actually serves the request
    // (issue #72), not the idle model string.
    assert_eq!(
        spinner_prefix(&resolved(Adapter::EdgeTts, "deepseek-flash")),
        "synthesizing speech (edge-tts)"
    );
}

#[test]
fn live_chars_counts_only_what_was_printed_live() {
    // The same text through a buffered and a live sink: `chars_seen`
    // grows in both, `live_chars` only where the terminal saw it.
    let mut buffered = DeltaSink {
        merged: String::new(),
        live: false,
        spinner: None,
        chars_seen: 0,
        live_chars: 0,
        line_open: false,
    };
    buffered.emit("一二三");
    assert_eq!(buffered.chars_seen, 3);
    assert_eq!(buffered.live_chars, 0);

    // The live sink prints; the test's captured stdout swallows it.
    let mut live = DeltaSink {
        merged: String::new(),
        live: true,
        spinner: None,
        chars_seen: 0,
        live_chars: 0,
        line_open: false,
    };
    live.emit("一二三");
    live.emit("四");
    assert_eq!(live.chars_seen, 4);
    assert_eq!(live.live_chars, 4);
}

#[test]
fn live_emits_track_whether_the_content_line_is_open() {
    // `line_open` decides whether a later step banner must break the line
    // before drawing: only a live print without a trailing newline leaves
    // the cursor mid-content.
    let mut live = DeltaSink {
        merged: String::new(),
        live: true,
        spinner: None,
        chars_seen: 0,
        live_chars: 0,
        line_open: false,
    };
    live.emit("半行");
    assert!(live.line_open);
    live.emit("结尾\n");
    assert!(!live.line_open);
    live.emit("新行\n又半行");
    assert!(live.line_open);
    // An empty delta prints nothing and moves nothing.
    live.emit("");
    assert!(live.line_open);

    // Buffered output never reaches the terminal, so it never opens a
    // line there.
    let mut buffered = DeltaSink {
        merged: String::new(),
        live: false,
        spinner: None,
        chars_seen: 0,
        live_chars: 0,
        line_open: false,
    };
    buffered.emit("no newline");
    assert!(!buffered.line_open);
}

#[test]
fn first_live_emit_retires_the_spinner_buffered_keeps_it() {
    // The spinner and a live reply share one terminal, so the first
    // delta takes the shared spinner out and stops it before printing
    // (issue #62): its redraws can never land inside the content. A
    // buffered sink never touches the cell — its output prints after
    // the run stopped the spinner itself.
    let cell: SharedSpinner = Rc::new(RefCell::new(Some(Spinner::disabled())));
    let mut live = DeltaSink {
        merged: String::new(),
        live: true,
        spinner: Some(cell.clone()),
        chars_seen: 0,
        live_chars: 0,
        line_open: false,
    };
    live.emit("君不见黄河之水天上来");
    assert!(cell.borrow().is_none());
    // Later deltas find an empty cell and keep streaming untouched.
    live.emit("，奔流到海不复回。");
    assert!(cell.borrow().is_none());

    // A fresh cell for the buffered case: its emit must leave the
    // spinner in place.
    let cell: SharedSpinner = Rc::new(RefCell::new(Some(Spinner::disabled())));
    let mut buffered = DeltaSink {
        merged: String::new(),
        live: false,
        spinner: Some(cell.clone()),
        chars_seen: 0,
        live_chars: 0,
        line_open: false,
    };
    buffered.emit("still there");
    assert!(cell.borrow().is_some());
}

#[test]
fn announce_step_swaps_a_live_spinners_message() {
    // The ordinary path: the spinner never retired, so a step updates it
    // in place — no restart, no line break.
    let cell: SharedSpinner = Rc::new(RefCell::new(Some(Spinner::disabled())));
    announce_step(&cell, "asking glm-4.6 (2/3)...", true, false);
    assert!(cell.borrow().is_some());
}

#[test]
fn announce_step_restarts_the_spinner_a_live_stream_retired() {
    // The real two-slice sequence on one sink: slice 1's first delta
    // retires the spinner (issue #62), the next step's banner restarts
    // it (issue #81: later slices went silent) ...
    let cell: SharedSpinner = Rc::new(RefCell::new(Some(Spinner::disabled())));
    let mut live = DeltaSink {
        merged: String::new(),
        live: true,
        spinner: Some(cell.clone()),
        chars_seen: 0,
        live_chars: 0,
        line_open: false,
    };
    live.emit("君不见黄河之水天上来");
    assert!(cell.borrow().is_none());
    announce_step(
        &cell,
        "asking glm-4.6 (2/3) — slice 2/3 of long.png...",
        true,
        false,
    );
    assert!(cell.borrow().is_some());
    // ... and slice 2's first delta retires the restarted spinner too —
    // the sink's handle survives its first emit, or the fresh spinner's
    // redraws would chop this slice's content (#62 again, for step 2+).
    live.emit("第二段从这里开始");
    assert!(cell.borrow().is_none());
}

#[test]
fn announce_step_never_restarts_without_a_terminal_stderr() {
    // A retired cell with stderr redirected (`2>log`) stays retired: no
    // spinner there could show the banner, so the content gains no line
    // break for nothing.
    let cell: SharedSpinner = Rc::new(RefCell::new(None));
    announce_step(&cell, "asking glm-4.6 (2/3)...", false, true);
    assert!(cell.borrow().is_none());
}

#[test]
fn batch_failure_carries_the_status_own_reason() {
    assert_eq!(
        status_failure(&GenerationStatus::Incomplete {
            reason: "length".into()
        }),
        "length"
    );
}

#[test]
fn batch_failure_names_the_variant_not_a_guess() {
    // Failed and Cancelled are the status's own words — never a
    // mislabeled "truncated".
    assert_eq!(
        status_failure(&GenerationStatus::Failed),
        "the reply failed"
    );
    assert_eq!(
        status_failure(&GenerationStatus::Cancelled),
        "the reply was cancelled"
    );
    assert!(
        !status_failure(&GenerationStatus::Failed).contains("truncated")
            && !status_failure(&GenerationStatus::Cancelled).contains("truncated")
    );
    // A status with no usable reason degrades to the generic wording.
    assert_eq!(
        status_failure(&GenerationStatus::Incomplete {
            reason: String::new()
        }),
        "the reply was incomplete"
    );
}

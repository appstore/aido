//! Config resolution: providers, profiles, routes, credentials, and the
//! management commands around the config.

mod support;

use support::*;

#[test]
fn profile_selects_provider_and_route() {
    let server = Server::json(chat_body("from-local"));
    let cfg = settings_config(&format!(
        "default_profile = \"local\"\n\
         [profiles.local]\nprovider = \"srv\"\nmodel = \"qwen3\"\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let out = run(
        &["summarize"],
        b"hello\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "from-local\n");
    let raw = server.request();
    assert!(request_path(&raw).starts_with("POST /v1/chat/completions"));
    assert_eq!(request_json(&raw)["model"], "qwen3");
}

#[test]
fn provider_route_overrides_the_default_adapter() {
    let server = Server::json(
        r#"{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"via responses"}]}]}"#,
    );
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{}\"\n\
         [providers.srv.routes]\ngenerate = \"openai-responses\"",
        server.url()
    ));
    let out = run(
        &["summarize", "--profile", "test"],
        b"hello\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    let raw = server.request();
    assert!(request_path(&raw).starts_with("POST /v1/responses"));
}

#[test]
fn credential_env_var_name_comes_from_the_provider() {
    let server = Server::json(chat_body("ok"));
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{}\"\napi_key_env = \"MY_TEST_AI_KEY\"",
        server.url()
    ));
    let out = run(
        &["summarize", "--profile", "test"],
        b"hello\n",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("MY_TEST_AI_KEY", "secret-key"),
        ],
    );
    out.assert_code(0);
    let raw = String::from_utf8_lossy(&server.request()).to_lowercase();
    assert!(raw.contains("authorization: bearer secret-key"), "{raw}");
}

#[test]
fn explicit_model_overrides_only_the_profile_model() {
    let server = Server::json(chat_body("ok"));
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"profile-model\"\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let env = ("AIDO_CONFIG", cfg.to_str().unwrap());
    let out = run(
        &["summarize", "--profile", "test", "-m", "flag-model"],
        b"x\n",
        &[env],
    );
    out.assert_code(0);
    assert_eq!(request_json(&server.request())["model"], "flag-model");
}

#[test]
fn unknown_profile_lists_the_available_ones() {
    let out = run(&["summarize", "--profile", "ghost"], b"x\n", &[]);
    out.assert_code(2);
    let err = out.stderr();
    assert!(err.contains("ghost"), "{err}");
    assert!(err.contains("profiles list"), "{err}");
}

#[test]
fn profile_operations_gate_the_task() {
    let cfg = settings_config(
        "[profiles.speech-only]\nprovider = \"srv\"\nmodel = \"m\"\noperations = [\"speech\"]\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"",
    );
    let out = run(
        &["summarize", "--profile", "speech-only"],
        b"x\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(2);
    assert!(
        out.stderr().contains("generate"),
        "stderr: {}",
        out.stderr()
    );
}

#[test]
fn profile_input_types_intersect_with_the_task() {
    // ocr needs image input; a text-only profile cannot serve it.
    let cfg = settings_config(
        "[profiles.text-only]\nprovider = \"srv\"\nmodel = \"m\"\ninput_types = [\"text\"]\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"",
    );
    let out = run(
        &["ocr", "--profile", "text-only", "a.png"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(2);
    assert!(out.stderr().contains("image"), "stderr: {}", out.stderr());
}

#[test]
fn disjoint_profile_and_task_input_types_error_at_resolve_time() {
    // summarize takes text only; an audio-only profile shares no input type
    // with it. Resolve must reject the combination itself — every material
    // would be rejected later, and only with an empty `(allowed: )` list.
    // (ocr would not do here: its declared types include text, so a
    // text-only profile still intersects.)
    let cfg = settings_config(
        "[profiles.audio-only]\nprovider = \"srv\"\nmodel = \"m\"\ninput_types = [\"audio\"]\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"",
    );
    let out = run(
        &[
            "summarize",
            "--profile",
            "audio-only",
            "notes.md",
            "--dry-run",
        ],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(2);
    let err = out.stderr();
    assert!(
        err.contains("shares no type with task 'summarize'"),
        "{err}"
    );
    assert!(err.contains("restricts inputs to [audio]"), "{err}");
    assert!(!err.contains("(allowed: )"), "{err}");
}

#[test]
fn deprecated_global_env_vars_are_ignored_with_a_warning() {
    let server = Server::json(chat_body("ok"));
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let out = run(
        &["summarize", "--profile", "test"],
        b"x\n",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_MODEL", "env-model"),
        ],
    );
    out.assert_code(0);
    // the profile model won; the env var only produced a warning
    assert_eq!(request_json(&server.request())["model"], "m");
    assert!(
        out.stderr().contains("AIDO_MODEL"),
        "stderr: {}",
        out.stderr()
    );
}

#[test]
fn config_init_writes_a_sample_and_check_validates_it() {
    let dir = temp_dir("config-init");
    let cfg_path = dir.join("config.toml");
    let out = run(
        &["config", "init"],
        b"",
        &[("AIDO_CONFIG", cfg_path.to_str().unwrap())],
    );
    out.assert_code(0);
    assert!(cfg_path.exists());
    assert!(std::fs::read_to_string(&cfg_path)
        .unwrap()
        .contains("[providers.openai]"));

    // init refuses to clobber
    let out = run(
        &["config", "init"],
        b"",
        &[("AIDO_CONFIG", cfg_path.to_str().unwrap())],
    );
    out.assert_code(2);

    // the sample still carries the YOUR_MODEL placeholder: check must flag
    // the profile (exit 2, the issue on stdout) instead of blessing a
    // config that cannot run
    let out = run(
        &["config", "check"],
        b"",
        &[("AIDO_CONFIG", cfg_path.to_str().unwrap())],
    );
    out.assert_code(2);
    let stdout = out.stdout();
    assert!(stdout.contains("'default'"), "stdout: {stdout}");
    assert!(stdout.contains("no model configured"), "stdout: {stdout}");
    assert!(stdout.contains("YOUR_MODEL"), "stdout: {stdout}");

    // filling in a real model makes check pass
    let sample = std::fs::read_to_string(&cfg_path).unwrap();
    std::fs::write(&cfg_path, sample.replace("YOUR_MODEL", "gpt-5")).unwrap();
    let out = run(
        &["config", "check"],
        b"",
        &[("AIDO_CONFIG", cfg_path.to_str().unwrap())],
    );
    out.assert_code(0);
    assert!(
        out.stdout().contains("config ok"),
        "stdout: {}",
        out.stdout()
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn profiles_list_shows_providers_and_models() {
    let cfg = settings_config(
        "default_profile = \"main\"\n\
         [profiles.main]\nprovider = \"srv\"\nmodel = \"big-model\"\n\
         [providers.srv]\nbase_url = \"http://localhost:9\"\napi_key_env = \"MY_KEY\"",
    );
    let out = run(
        &["profiles"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("main"), "{stdout}");
    assert!(stdout.contains("big-model"), "{stdout}");
    assert!(stdout.contains("MY_KEY"), "{stdout}");
}

#[test]
fn tasks_list_and_show_cover_the_builtins() {
    let out = run(&["tasks", "list"], b"", &[]);
    out.assert_code(0);
    let stdout = out.stdout();
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
        assert!(stdout.contains(name), "missing {name} in:\n{stdout}");
    }

    let out = run(&["tasks", "show", "ocr"], b"", &[]);
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("ocr-tiles"), "{stdout}");
    assert!(stdout.contains("image"), "{stdout}");

    let out = run(&["tasks", "show", "summarize"], b"", &[]);
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("chunk-reduce"), "{stdout}");
    assert!(stdout.contains("text"), "{stdout}");

    let out = run(&["tasks", "show", "nope"], b"", &[]);
    out.assert_code(2);
}

#[test]
fn management_commands_refuse_material_flags() {
    // F42: --text/--paste on a management command must be refused, not
    // silently dropped (the input module drops nothing by design).
    let out = run(&["tasks", "list", "--text", "x"], b"", &[]);
    out.assert_code(2);
    assert!(
        out.stderr().contains("no effect on management commands"),
        "stderr: {}",
        out.stderr()
    );

    let out = run(&["config", "check", "--paste"], b"", &[]);
    out.assert_code(2);
    assert!(
        out.stderr().contains("no effect on management commands"),
        "stderr: {}",
        out.stderr()
    );
}

#[test]
fn custom_tasks_load_from_the_config_dir() {
    let dir = temp_dir("tasks-custom");
    std::fs::write(
        dir.join("polish.toml"),
        "operation = 'generate'\ninstruction = 'polish the text'\ninput_types = ['text']\noutput_types = ['text']\n",
    )
    .unwrap();
    let server = Server::json(chat_body("ok"));
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let out = run(
        &["polish", "--profile", "test"],
        b"rough text\n",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_TASKS_DIR", dir.to_str().unwrap()),
        ],
    );
    out.assert_code(0);
    let req = request_json(&server.request());
    assert_eq!(req["messages"][0]["role"], "system");
    assert_eq!(req["messages"][0]["content"], "polish the text");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn translate_target_language_reaches_the_instruction() {
    let server = Server::json(chat_body("你好"));
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let out = run(
        &["translate", "--profile", "test", "--to", "zh-CN"],
        b"hello\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    let req = request_json(&server.request());
    let system = req["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("Target language: zh-CN"), "{system}");
}

#[test]
fn dry_run_explains_the_plan_without_any_request() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        // Nobody should connect; if they do, the request is recorded.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        accept(&listener, deadline)
    });
    let file = temp_file("notes.md", b"material\n");
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:{port}\"\napi_key_env = \"MY_KEY\"",
    ));
    let out = run(
        &[
            "summarize",
            "--profile",
            "test",
            file.to_str().unwrap(),
            "-",
            "--dry-run",
        ],
        b"piped notes\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("task:        summarize"), "{stdout}");
    assert!(stdout.contains("profile:     test"), "{stdout}");
    assert!(stdout.contains("model:       m"), "{stdout}");
    assert!(stdout.contains("notes.md"), "{stdout}");
    assert!(stdout.contains("destinations"), "{stdout}");
    assert!(stdout.contains("stdout"), "{stdout}");
    assert!(stdout.contains("MY_KEY"), "{stdout}");
    // no connection happened within the window (accept() panics on its
    // deadline, which surfaces as a join error)
    assert!(handle.join().is_err(), "dry-run must not send requests");
}

#[test]
fn dry_run_reports_profile_generation_param_sources() {
    // A profile-set max_tokens/temperature must be reported with its real
    // source; the old report showed max_tokens only for the CLI flag,
    // hardcoded as cli, and dropped temperature entirely.
    let file = temp_file("notes.md", b"material\n");
    let cfg = settings_config(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\nmax_tokens = 2048\ntemperature = 0.2\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"",
    );
    let out = run(
        &[
            "summarize",
            "--profile",
            "test",
            file.to_str().unwrap(),
            "--dry-run",
        ],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("max_tokens = 2048  (profile)"), "{stdout}");
    assert!(stdout.contains("temperature = 0.2  (profile)"), "{stdout}");
}

#[test]
fn dry_run_reports_cli_overrides_of_generation_params() {
    let file = temp_file("notes.md", b"material\n");
    let cfg = settings_config(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\nmax_tokens = 2048\ntemperature = 0.2\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"",
    );
    let out = run(
        &[
            "summarize",
            "--profile",
            "test",
            file.to_str().unwrap(),
            "--max-tokens",
            "99",
            "--temperature",
            "0.9",
            "--dry-run",
        ],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("max_tokens = 99  (cli)"), "{stdout}");
    assert!(stdout.contains("temperature = 0.9  (cli)"), "{stdout}");
    // the profile's values lost the merge and must not be reported
    assert!(!stdout.contains("2048"), "{stdout}");
    assert!(!stdout.contains("0.2"), "{stdout}");
}

#[test]
fn dry_run_reports_typed_param_sources_flag_vs_task_default() {
    // `voice` is the one parameter a task may default: the report must
    // name the task as its source, a --voice flag as cli, and leave an
    // unset parameter marked as not sent.
    let tasks = temp_dir("param-source-task");
    std::fs::write(
        tasks.join("briefing.toml"),
        "operation = \"speech\"\n\
         input_types = [\"text\"]\n\
         required_types = [\"text\"]\n\
         output_types = [\"audio\"]\n\
         params = [\"voice\", \"speed\"]\n\
         \n\
         [defaults]\n\
         voice = \"alloy\"\n",
    )
    .unwrap();
    let envs = [("AIDO_TASKS_DIR", tasks.to_str().unwrap())];
    let out = run(
        &["run", "briefing", "--dry-run", "--text", "你好"],
        b"",
        &envs,
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("voice = alloy  (task)"), "{stdout}");
    assert!(stdout.contains("speed = (not sent)  (default)"), "{stdout}");

    let out = run(
        &[
            "run",
            "briefing",
            "--dry-run",
            "--text",
            "你好",
            "--voice",
            "nova",
        ],
        b"",
        &envs,
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("voice = nova  (cli)"), "{stdout}");
    assert!(!stdout.contains("alloy"), "{stdout}");
    std::fs::remove_dir_all(&tasks).ok();
}

#[test]
fn typed_param_option_mismatch_is_a_usage_error_not_a_service_error() {
    // A typed param maps to an adapter option; when the resolved adapter
    // rejects it, the plan was never sent — exit 2, like the resolve-path
    // [options] validation below, not a service error (3).
    let tasks = temp_dir("param-option-mismatch");
    std::fs::write(
        tasks.join("mytask.toml"),
        "operation = \"generate\"\n\
         output_types = [\"text\"]\n\
         params = [\"voice\"]\n",
    )
    .unwrap();
    // The port-1 base_url is never contacted: validation must fail first.
    let cfg = settings_config(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"",
    );
    let out = run(
        &[
            "mytask",
            "--profile",
            "test",
            "--voice",
            "alloy",
            "--text",
            "hi",
            "--dry-run",
        ],
        b"",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_TASKS_DIR", tasks.to_str().unwrap()),
        ],
    );
    out.assert_code(2);
    let err = out.stderr();
    assert!(err.starts_with("error:"), "{err}");
    assert!(err.contains("does not support option 'voice'"), "{err}");
    std::fs::remove_dir_all(&tasks).ok();
}

#[test]
fn task_options_rejected_by_the_adapter_stay_a_usage_error() {
    // The same validation reached through the task's [options] table
    // (resolve()) has always classified as usage; locked in beside the
    // typed-param path so the two call sites cannot drift apart.
    let tasks = temp_dir("task-options-mismatch");
    std::fs::write(
        tasks.join("opttask.toml"),
        "operation = \"generate\"\n\
         output_types = [\"text\"]\n\
         \n\
         [options]\n\
         voice = \"alloy\"\n",
    )
    .unwrap();
    let cfg = settings_config(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"",
    );
    let out = run(
        &["opttask", "--profile", "test", "--text", "hi", "--dry-run"],
        b"",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_TASKS_DIR", tasks.to_str().unwrap()),
        ],
    );
    out.assert_code(2);
    assert!(
        out.stderr().contains("does not support option 'voice'"),
        "{}",
        out.stderr()
    );
    std::fs::remove_dir_all(&tasks).ok();
}

#[test]
fn dry_run_hides_credentials_embedded_in_the_base_url() {
    let file = temp_file("notes.md", b"material\n");
    let cfg = settings_config(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"https://user:sup3rsecret@gw.internal/v1\"\napi_key_env = \"MY_KEY\"",
    );
    let out = run(
        &[
            "summarize",
            "--profile",
            "test",
            file.to_str().unwrap(),
            "-",
            "--dry-run",
        ],
        b"piped notes\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(
        stdout.contains("https://***:***@gw.internal/v1"),
        "{stdout}"
    );
    assert!(!stdout.contains("sup3rsecret"), "{stdout}");
}

#[test]
fn dry_run_warns_when_a_key_would_traverse_plain_http() {
    // A non-loopback http base_url would carry the key in cleartext; the
    // plan names the host instead of letting the run happen unwarned.
    let file = temp_file("notes.md", b"material\n");
    let cfg = settings_config(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"http://gw.internal:8080/v1\"\napi_key_env = \"MY_KEY\"",
    );
    let out = run(
        &[
            "summarize",
            "--profile",
            "test",
            file.to_str().unwrap(),
            "-",
            "--dry-run",
        ],
        b"piped notes\n",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("MY_KEY", "sk-test"),
        ],
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(
        stdout.contains("credentials will be sent in cleartext to gw.internal"),
        "{stdout}"
    );
    assert!(stdout.contains("base_url uses http://"), "{stdout}");
}

#[test]
fn dry_run_spares_loopback_http_and_https_from_the_cleartext_warning() {
    // Local inference servers are legitimately http (loopback is exempt),
    // and https encrypts the key everywhere.
    let file = temp_file("notes.md", b"material\n");
    for base in [
        "http://127.0.0.1:8080/v1",
        "http://localhost:8080/v1",
        "https://gw.internal:8080/v1",
    ] {
        let cfg = settings_config(&format!(
            "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
             [providers.srv]\nbase_url = \"{base}\"\napi_key_env = \"MY_KEY\""
        ));
        let out = run(
            &[
                "summarize",
                "--profile",
                "test",
                file.to_str().unwrap(),
                "-",
                "--dry-run",
            ],
            b"piped notes\n",
            &[
                ("AIDO_CONFIG", cfg.to_str().unwrap()),
                ("MY_KEY", "sk-test"),
            ],
        );
        out.assert_code(0);
        let stdout = out.stdout();
        assert!(!stdout.contains("cleartext"), "{base}: {stdout}");
    }
}

#[test]
fn dry_run_reports_the_openai_fallback_as_the_effective_key() {
    // F46: the default provider reads AIDO_API_KEY with OPENAI_API_KEY
    // as a fallback, and the dry-run must judge credentials the way the
    // real send does — otherwise it prophesies a failure the run would
    // not have. With only the fallback set, the line names the variable
    // that would actually be read, marked as the fallback.
    let out = run(
        &["ask", "-p", "hi", "--dry-run"],
        b"",
        &[("OPENAI_API_KEY", "sk-fallback")],
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(
        stdout.contains("credentials: OPENAI_API_KEY (fallback) is set"),
        "{stdout}"
    );
    assert!(!stdout.contains("would fail"), "{stdout}");
}

#[test]
fn dry_run_still_predicts_failure_without_any_key_variable() {
    // The other side of the fallback rule: with neither AIDO_API_KEY nor
    // OPENAI_API_KEY in the environment, the honest verdict is still the
    // failure prediction, naming the configured variable.
    let out = run(&["ask", "-p", "hi", "--dry-run"], b"", &[]);
    out.assert_code(0);
    assert!(
        out.stdout()
            .contains("credentials: AIDO_API_KEY is NOT set — the request would fail"),
        "{}",
        out.stdout()
    );
}

#[test]
fn the_openai_fallback_belongs_to_the_default_provider_only() {
    // A provider naming its own variable (CUSTOM_KEY) must not inherit
    // the OPENAI_API_KEY fallback: that convenience exists so the
    // zero-config default provider reuses an existing OpenAI key, not
    // so every provider silently shares it.
    let cfg = settings_config(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"\napi_key_env = \"CUSTOM_KEY\"",
    );
    let out = run(
        &["ask", "-p", "hi", "--profile", "test", "--dry-run"],
        b"",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("OPENAI_API_KEY", "sk-elsewhere"),
        ],
    );
    out.assert_code(0);
    assert!(
        out.stdout()
            .contains("credentials: CUSTOM_KEY is NOT set — the request would fail"),
        "{}",
        out.stdout()
    );
}

#[test]
fn the_cleartext_warning_fires_for_a_fallback_key_over_plain_http() {
    // The cleartext judgment shares the fallback: a key that arrives via
    // OPENAI_API_KEY is as exposed over plain http as a directly set
    // one, and the credential line agrees with the warning about it.
    let cfg = settings_config(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"http://gw.internal:8080/v1\"\napi_key_env = \"AIDO_API_KEY\"",
    );
    let out = run(
        &["ask", "-p", "hi", "--profile", "test", "--dry-run"],
        b"",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("OPENAI_API_KEY", "sk-fallback"),
        ],
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(
        stdout.contains("credentials will be sent in cleartext to gw.internal"),
        "{stdout}"
    );
    assert!(
        stdout.contains("credentials: OPENAI_API_KEY (fallback) is set"),
        "{stdout}"
    );
}

#[test]
fn a_real_run_sends_the_openai_fallback_key() {
    // The send-side of the F46 contract: with only OPENAI_API_KEY set
    // and AIDO_API_KEY named, the request carries the fallback key —
    // exactly what the dry-run now reports, since both go through the
    // same shared judgment.
    let server = Server::json(chat_body("ok"));
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{}\"\napi_key_env = \"AIDO_API_KEY\"",
        server.url()
    ));
    let out = run(
        &["summarize", "--profile", "test"],
        b"hello\n",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("OPENAI_API_KEY", "sk-fallback"),
        ],
    );
    out.assert_code(0);
    let raw = String::from_utf8_lossy(&server.request()).to_lowercase();
    assert!(raw.contains("authorization: bearer sk-fallback"), "{raw}");
}

#[test]
fn config_check_flags_route_keys_that_are_not_operations() {
    // A typo'd route key (`speach`) would silently fall back to the
    // conventional adapter; `config check` must name it.
    let cfg = settings_config(
        "[settings]\nhistory_keep = 0\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"\n\
         [providers.srv.routes]\nspeach = \"openai-speech\"",
    );
    let out = run(
        &["config", "check"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(2);
    assert!(out.stdout().contains("speach"), "{}", out.stdout());
    assert!(
        out.stdout().contains("not an operation"),
        "{}",
        out.stdout()
    );
}

#[test]
fn providers_only_config_keeps_the_builtin_default_profile() {
    // F38: the built-in `default` profile exists exactly when the user
    // defined no profiles at all. A providers-only config is the
    // documented way to override the built-in openai provider wholesale
    // (README, Edge TTS), so it must keep working: check blesses it, and a
    // run serves the builtin default profile with the USER's provider —
    // the distinctive base_url proves it is not the builtin one.
    let cfg = settings_config(
        "[settings]\nhistory_keep = 0\n\
         [providers.openai]\nbase_url = \"http://127.0.0.1:9/v1\"\napi_key_env = \"AIDO_API_KEY\"",
    );
    let out = run(
        &["config", "check"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert!(out.stdout().contains("config ok"), "{}", out.stdout());

    let out = run(
        &["ask", "-p", "hi", "--dry-run"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("http://127.0.0.1:9/v1"), "{stdout}");
}

#[test]
fn config_check_flags_a_missing_default_profile_when_any_profile_exists() {
    // The other side of the F38 rule: the built-in default profile exists
    // only while the user has defined no profiles, so a config that adds
    // [profiles.fast] without a [profiles.default] (and no
    // default_profile) must keep failing check — and a default run must
    // refuse the same way, so check and the runtime agree.
    let cfg = settings_config(
        "[settings]\nhistory_keep = 0\n\
         [profiles.fast]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"",
    );
    let out = run(
        &["config", "check"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(2);
    assert!(
        out.stdout().contains("default profile 'default'"),
        "{}",
        out.stdout()
    );

    let out = run(
        &["ask", "-p", "hi", "--dry-run"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(2);
    assert!(
        out.stderr().contains("profile 'default' not found"),
        "{}",
        out.stderr()
    );
}

#[test]
fn config_check_agrees_with_the_builtin_openai_provider_fallback() {
    // F38 symptom 2: a profile referencing `openai` without the user
    // defining such a provider used to make `config check` report
    // "unknown provider" and exit 2 while the same run succeeded — check
    // lacked the runtime's builtin-openai fallback. The literal issue
    // config (only [profiles.fast]) still fails check, but for the honest
    // reason above (no default profile once any profile exists), never
    // with the unknown-provider message; with default_profile pointing at
    // the profile, check and the run must both succeed.
    let literal = settings_config(
        "[settings]\nhistory_keep = 0\n\
         [profiles.fast]\nprovider = \"openai\"\nmodel = \"gpt-4o-mini\"",
    );
    let out = run(
        &["config", "check"],
        b"",
        &[("AIDO_CONFIG", literal.to_str().unwrap())],
    );
    out.assert_code(2);
    let stdout = out.stdout();
    assert!(!stdout.contains("unknown provider"), "{stdout}");
    assert!(stdout.contains("default profile 'default'"), "{stdout}");

    let cfg = settings_config(
        "default_profile = \"fast\"\n[settings]\nhistory_keep = 0\n\
         [profiles.fast]\nprovider = \"openai\"\nmodel = \"gpt-4o-mini\"",
    );
    let out = run(
        &["config", "check"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert!(out.stdout().contains("config ok"), "{}", out.stdout());

    // the run falls back to the built-in openai provider (official
    // endpoint), the same provider check just blessed.
    let out = run(
        &["ask", "-p", "hi", "--profile", "fast", "--dry-run"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert!(
        out.stdout().contains("https://api.openai.com/v1"),
        "{}",
        out.stdout()
    );
}

#[cfg(unix)]
#[test]
fn zero_config_tts_defaults_to_keyless_edge_tts() {
    // The built-in fallback config routes speech to the Edge adapter so
    // `aido tts` works without any config file or API key. The dry-run
    // must not claim a missing key would fail the request — none is sent.
    let out = run_tty(&["tts", "--dry-run"], &[]);
    assert!(out.ok(), "stderr: {}", out.stderr());
    let stdout = out.stdout();
    assert!(stdout.contains("(route: edge-tts)"), "stdout: {stdout}");
    assert!(
        stdout.contains("endpoint owned by the edge-tts adapter"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("credentials: none required"),
        "stdout: {stdout}"
    );
}

#[test]
fn config_init_keeps_tts_keyless() {
    // The sample's [providers.openai] shadows the built-in provider, so it
    // must carry the same speech route: following README step 1 (`aido
    // config init`) should not turn a keyless `aido tts` into a missing
    // AIDO_API_KEY error.
    let dir = temp_dir("config-init-tts");
    let cfg_path = dir.join("config.toml");
    let out = run(
        &["config", "init"],
        b"",
        &[("AIDO_CONFIG", cfg_path.to_str().unwrap())],
    );
    out.assert_code(0);

    let out = run_with(
        &["tts", "--text", "你好，世界", "--dry-run"],
        b"",
        &[],
        &cfg_path,
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("(route: edge-tts)"), "stdout: {stdout}");
    assert!(
        stdout.contains("credentials: none required"),
        "stdout: {stdout}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn config_check_requires_base_url_when_an_operation_escapes_the_edge_route() {
    // The speech route points at edge-tts (which owns its endpoint), but
    // the profile also allows an unrouted operation whose conventional
    // adapter still needs a base_url; check must flag it before a run does.
    let cfg = settings_config(
        "default_profile = \"x\"\n[settings]\nhistory_keep = 0\n\
         [profiles.x]\nprovider = \"srv\"\nmodel = \"edge\"\noperations = [\"speech\", \"generate\"]\n\
         [providers.srv]\n[providers.srv.routes]\nspeech = \"edge-tts\"",
    );
    let out = run(
        &["config", "check"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(2);
    assert!(
        out.stdout()
            .contains("provider 'srv' (used by 'x'): missing base_url"),
        "{}",
        out.stdout()
    );
}

#[test]
fn config_check_accepts_a_base_url_free_edge_only_provider() {
    // When every operation the profile allows is routed to edge-tts, the
    // provider needs no base_url and check must not ask for one.
    let cfg = settings_config(
        "default_profile = \"x\"\n[settings]\nhistory_keep = 0\n\
         [profiles.x]\nprovider = \"srv\"\nmodel = \"edge\"\noperations = [\"speech\"]\n\
         [providers.srv]\n[providers.srv.routes]\nspeech = \"edge-tts\"",
    );
    let out = run(
        &["config", "check"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert!(out.stdout().contains("config ok"), "{}", out.stdout());
}

/// Run aido with no config file at all: no AIDO_CONFIG, and the default
/// config path (dirs::config_dir(), i.e. $XDG_CONFIG_HOME or $HOME/...)
/// pointing into a nonexistent tree — so `load()` yields the built-in
/// default config. The true zero-config state, which the `run` helpers
/// cannot express (they always point AIDO_CONFIG at a file). The rest of
/// the environment follows `base_command`: isolated tasks/history dirs
/// and no developer credentials leaking in.
fn run_zero_config(args: &[&str]) -> RunOutcome {
    let mut cmd = std::process::Command::new(EXE);
    cmd.args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("AIDO_TASKS_DIR", "/nonexistent/aido-test-tasks")
        .env("AIDO_HISTORY_DIR", "/nonexistent/aido-test-history")
        .env("HOME", "/nonexistent/aido-test-home")
        .env("XDG_CONFIG_HOME", "/nonexistent/aido-test-config");
    for var in [
        "AIDO_CONFIG",
        "AIDO_PROFILE",
        "AIDO_MODEL",
        "AIDO_BASE_URL",
        "AIDO_ADAPTER",
        "OPENAI_API_KEY",
        "AIDO_API_KEY",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XDG_SESSION_TYPE",
    ] {
        cmd.env_remove(var);
    }
    let mut child = cmd.spawn().unwrap();
    // Close stdin (empty piped input) like `run(..., b"", ...)` does.
    drop(child.stdin.take());
    RunOutcome {
        output: child.wait_with_output().unwrap(),
    }
}

#[test]
fn zero_config_check_is_ok_with_a_default_model_note() {
    // F44: zero-config is a first-class state (the README quick start runs
    // `aido tts` with no config), and load() then materializes the built-in
    // default profile with model = None — which runs fine on the adapter's
    // default model. check must bless it: the note belongs on stderr, and
    // stdout stays the success line, not an issue list.
    let out = run_zero_config(&["config", "check"]);
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("config ok"), "stdout: {stdout}");
    assert!(!stdout.contains("issue"), "stdout: {stdout}");
    let err = out.stderr();
    assert!(
        err.contains("profile 'default' has no model set"),
        "stderr: {err}"
    );
    assert!(
        err.contains("the adapter default will be used"),
        "stderr: {err}"
    );
}

#[test]
fn modelless_user_profile_check_is_ok_the_placeholder_stays_an_issue() {
    // F44: a user profile without a model runs on the adapter's default
    // model (resolve()'s fallback), so check must not fail it either — the
    // note goes to stderr. The contrast on the same fixture: the `config
    // init` placeholder must keep failing check (F23), since it would
    // never be what the user wants.
    let cfg = settings_config(
        "default_profile = \"local\"\n[settings]\nhistory_keep = 0\n\
         [profiles.local]\nprovider = \"srv\"\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"",
    );
    let out = run(
        &["config", "check"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert!(out.stdout().contains("config ok"), "{}", out.stdout());
    let err = out.stderr();
    assert!(
        err.contains("profile 'local' has no model set"),
        "stderr: {err}"
    );
    assert!(
        err.contains("the adapter default will be used"),
        "stderr: {err}"
    );

    let cfg = settings_config(
        "default_profile = \"local\"\n[settings]\nhistory_keep = 0\n\
         [profiles.local]\nprovider = \"srv\"\nmodel = \"YOUR_MODEL\"\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"",
    );
    let out = run(
        &["config", "check"],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(2);
    let stdout = out.stdout();
    assert!(stdout.contains("no model configured"), "stdout: {stdout}");
    assert!(stdout.contains("YOUR_MODEL"), "stdout: {stdout}");
}

#[test]
fn zero_config_check_agrees_with_a_real_run() {
    // F44 consistency, F38 style (the F38 tests pair a check verdict with
    // a run on the same config): the zero-config state must pass both
    // `config check` and a run — check's note promises the adapter default
    // model, and the run's dry-run report confirms it would use exactly
    // that, so the two can never disagree about this state.
    let out = run_zero_config(&["config", "check"]);
    out.assert_code(0);
    assert!(out.stdout().contains("config ok"), "{}", out.stdout());

    let out = run_zero_config(&["ask", "-p", "hi", "--dry-run"]);
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(
        stdout
            .lines()
            .any(|l| l.starts_with("  model = ") && l.ends_with("(default)")),
        "stdout: {stdout}"
    );
}

#[cfg(unix)]
#[test]
fn zero_config_text_tasks_still_default_to_openai() {
    // Only speech gets the keyless default; text tasks still point at the
    // OpenAI-compatible provider (they need a key to actually run).
    let out = run_tty(&["ask", "--dry-run", "-p", "hi"], &[]);
    assert!(out.ok(), "stderr: {}", out.stderr());
    assert!(
        out.stdout().contains("(route: openai-chat)"),
        "{}",
        out.stdout()
    );
}

#[cfg(unix)]
#[test]
fn edge_tts_refuses_a_prompt_while_building_the_plan() {
    // The adapter has no instruction channel; the plan builder refuses the
    // run before anything executes, so even --dry-run reports it (exit 2,
    // a usage error — not a mid-synthesis failure). The message names the
    // source: here, -p.
    let out = run_tty(
        &[
            "tts",
            "--dry-run",
            "--text",
            "你好",
            "-o",
            "hello.mp3",
            "-p",
            "不要读这句",
        ],
        &[],
    );
    out.assert_code(2);
    assert!(
        out.stderr().contains("no instruction channel"),
        "{}",
        out.stderr()
    );
    assert!(
        out.stderr().contains("-p would have nowhere to go"),
        "{}",
        out.stderr()
    );
}

#[cfg(unix)]
#[test]
fn edge_tts_refuses_a_task_fixed_instruction_and_names_the_source() {
    // A custom speech task may carry a fixed instruction; on the keyless
    // zero-config edge route the plan refuses it even without -p, and the
    // message points at the actual source instead of blanket "drop -p".
    let tasks = temp_dir("edge-instruction-task");
    std::fs::write(
        tasks.join("briefing.toml"),
        "operation = \"speech\"\n\
         input_types = [\"text\"]\n\
         required_types = [\"text\"]\n\
         output_types = [\"audio\"]\n\
         instruction = \"用轻快的语气朗读\"\n",
    )
    .unwrap();
    let out = run_tty(
        &[
            "run",
            "briefing",
            "--dry-run",
            "--text",
            "你好",
            "-o",
            "hello.mp3",
        ],
        &[("AIDO_TASKS_DIR", tasks.to_str().unwrap())],
    );
    out.assert_code(2);
    assert!(
        out.stderr()
            .contains("the task's fixed instruction would have nowhere to go"),
        "{}",
        out.stderr()
    );
}

/// A generate profile routed to the responses adapter, whose outputs cover
/// both text and image, so `--produce image,text,image` passes capability
/// checks (base_url is never dialed: both tests below stop at plan time).
fn responses_profile_config() -> std::path::PathBuf {
    settings_config(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"\n\
         [providers.srv.routes]\ngenerate = \"openai-responses\"",
    )
}

#[test]
fn produce_repeats_dedup_to_first_occurrence_order() {
    // `image,text,image` used to survive `Vec::dedup` (it only collapses
    // *adjacent* repeats), so the resolved produce list kept three entries
    // and downstream checks double-counted the image kind. Each kind must
    // appear once, in first-occurrence order — that order drives artifact
    // ordering, so no sorting.
    let cfg = responses_profile_config();
    let out = run(
        &[
            "ask",
            "--profile",
            "test",
            "--text",
            "hi",
            "-p",
            "hi",
            "--produce",
            "image,text,image",
            "--dry-run",
        ],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("produce:     image,text"), "{stdout}");
    assert!(!stdout.contains("image,text,image"), "{stdout}");
}

#[test]
fn repeated_produce_kinds_no_longer_make_format_ambiguous() {
    // With the duplicate entry still present, `--format` saw two image
    // kinds in `image,text,image` and refused the run as ambiguous. After
    // the dedup only one image kind remains, so the ambiguity gate passes;
    // what is left is the ordinary multi-kind --format mismatch, naming
    // the deduped, order-preserved produce list.
    let cfg = responses_profile_config();
    let out = run(
        &[
            "ask",
            "--profile",
            "test",
            "--text",
            "hi",
            "-p",
            "hi",
            "--produce",
            "image,text,image",
            "--format",
            "png",
        ],
        b"",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(2);
    let err = out.stderr();
    assert!(!err.contains("ambiguous"), "{err}");
    assert!(
        err.contains("does not match the produced type(s) [image,text]"),
        "{err}"
    );
}

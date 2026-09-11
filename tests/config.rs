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

    // the sample is structurally valid: check passes
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
fn config_check_requires_the_default_profile_to_exist_with_providers() {
    // The built-in `default` profile only exists when no providers are
    // configured; check and a real run must agree about that.
    let cfg = settings_config(
        "[settings]\nhistory_keep = 0\n\
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
}

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

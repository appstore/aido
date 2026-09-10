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

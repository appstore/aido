use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;

const EXE: &str = env!("CARGO_BIN_EXE_aido");

/// A one-shot HTTP server that records the raw request and replies with a
/// canned body, so tests can assert on the exact JSON aido sends.
struct Server {
    port: u16,
    handle: JoinHandle<Vec<u8>>,
}

impl Server {
    fn start(status: &str, body: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        // Poll for the connection instead of blocking in accept(), so a
        // regression that makes aido exit before requesting fails the test
        // instead of hanging it.
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let status = status.to_string();
        let body = body.to_string();
        let handle = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(conn) => break conn,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "aido never connected to the test server"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(e) => panic!("accept failed: {e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(30)))
                .unwrap();
            let mut raw = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let n = stream.read(&mut chunk).unwrap();
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&chunk[..n]);
                if let Some(pos) = find_sub(&raw, b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&raw[..pos]).to_lowercase();
                    let len: usize = headers
                        .lines()
                        .find(|l| l.starts_with("content-length"))
                        .and_then(|l| l.split(':').nth(1))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    while raw.len() < pos + 4 + len {
                        let n = stream.read(&mut chunk).unwrap();
                        if n == 0 {
                            break;
                        }
                        raw.extend_from_slice(&chunk[..n]);
                    }
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            raw
        });
        Self { port, handle }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn request(self) -> Vec<u8> {
        self.handle.join().unwrap()
    }
}

fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

static CONFIG_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// An empty but valid config, so tests never read the developer's real
/// ~/.config/aido/config.toml (AIDO_CONFIG must point at an existing
/// file, hence a temp file rather than a nonexistent path).
fn empty_config() -> std::path::PathBuf {
    let n = CONFIG_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir()
        .join(format!("aido-test-empty-{}-{n}.toml", std::process::id()));
    std::fs::write(&path, "").unwrap();
    path
}

fn run(args: &[&str], stdin_data: &[u8], envs: &[(&str, &str)]) -> std::process::Output {
    let config = empty_config();
    let mut cmd = Command::new(EXE);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("AIDO_CONFIG", &config)
        // Point the preset dir at a nonexistent path so the developer's own
        // custom presets cannot override the built-ins under test.
        .env("AIDO_PRESETS_DIR", "/nonexistent/aido-test-presets");
    for var in [
        "OPENAI_API_KEY",
        "AIDO_API_KEY",
        "OPENAI_BASE_URL",
        "AIDO_BASE_URL",
        "AIDO_MODEL",
        "AIDO_PROFILE",
        "AIDO_MAX_TOKENS",
        "AIDO_TEMPERATURE",
    ] {
        cmd.env_remove(var);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin_data).unwrap();
    let out = child.wait_with_output().unwrap();
    let _ = std::fs::remove_file(&config);
    out
}

fn request_json(raw: &[u8]) -> serde_json::Value {
    let pos = find_sub(raw, b"\r\n\r\n").unwrap();
    serde_json::from_slice(&raw[pos + 4..]).unwrap()
}

fn request_path(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).lines().next().unwrap().to_string()
}

#[test]
fn text_input_stdout_output() {
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"WORLD"}}]}"#);
    let out = run(
        &["--base-url", server.url().as_str(), "-m", "test-model", "--no-spinner"],
        b"hello\n",
        &[],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "WORLD\n");

    let raw = server.request();
    let req = request_json(&raw);
    assert_eq!(req["model"], "test-model");
    assert_eq!(req["messages"][0]["role"], "user");
    assert_eq!(req["messages"][0]["content"], "hello\n");
    assert_eq!(req["max_tokens"], 4096);
    assert!(req.get("temperature").is_none());
    assert!(!String::from_utf8_lossy(&raw).to_lowercase().contains("authorization"));
    // base_url without a path gets "/v1" appended
    assert_eq!(request_path(&raw), "POST /v1/chat/completions HTTP/1.1");
}

#[test]
fn system_prompt_via_flag() {
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let out = run(
        &["--base-url", server.url().as_str(), "--no-spinner", "-p", "be brief"],
        b"hi\n",
        &[],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let req = request_json(&server.request());
    assert_eq!(req["messages"][0]["role"], "system");
    assert_eq!(req["messages"][0]["content"], "be brief");
    assert_eq!(req["messages"][1]["role"], "user");
}

#[test]
fn preset_supplies_system_prompt() {
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let out = run(
        &["--base-url", server.url().as_str(), "--preset", "ocr", "--no-spinner"],
        b"some text\n",
        &[],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let req = request_json(&server.request());
    assert_eq!(req["messages"][0]["role"], "system");
    let sys = req["messages"][0]["content"].as_str().unwrap();
    assert!(sys.to_lowercase().contains("ocr"));
    assert_eq!(req["messages"][1]["role"], "user");
    assert_eq!(req["messages"][1]["content"], "some text\n");
}

#[test]
fn action_syntax_runs_preset() {
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let out = run(
        &["ocr", "--base-url", server.url().as_str(), "--no-spinner"],
        b"some text\n",
        &[],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let req = request_json(&server.request());
    assert_eq!(req["messages"][0]["role"], "system");
    let sys = req["messages"][0]["content"].as_str().unwrap();
    assert!(sys.to_lowercase().contains("ocr"));
    assert_eq!(req["messages"][1]["content"], "some text\n");
}

#[test]
fn action_name_must_come_first() {
    // With flags first there is nowhere for 'ocr' to land — it must fail
    // loudly instead of being sent to the model as a prompt.
    let out = run(
        &["--base-url", "http://127.0.0.1:1", "--no-spinner", "ocr"],
        b"hi\n",
        &[],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unrecognized subcommand"), "stderr was: {err}");
}

#[test]
fn unknown_action_fails_with_suggestion() {
    let out = run(&["transalte", "--no-spinner"], b"hi\n", &[]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown action"), "stderr was: {err}");
    assert!(err.contains("did you mean 'translate'"), "stderr was: {err}");
}

#[test]
fn prompt_like_action_gets_migration_hint() {
    // Old versions took any positional text as the prompt; sentences (with
    // or without spaces) landing in the action slot should point at -p.
    for arg in ["polish this text for me", "润色这段话"] {
        let out = run(&[arg, "--no-spinner"], b"hi\n", &[]);
        assert!(!out.status.success());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("unknown action"), "stderr was: {err}");
        assert!(err.contains("-p/--prompt"), "stderr was: {err}");
    }
}

#[test]
fn unknown_preset_fails_with_suggestion() {
    let out = run(&["--preset", "transalte", "--no-spinner"], b"hi\n", &[]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown preset"), "stderr was: {err}");
    assert!(err.contains("did you mean 'translate'"), "stderr was: {err}");
}

#[test]
fn action_conflicts_with_prompt_flag() {
    let out = run(&["ocr", "-p", "extra", "--no-spinner"], b"hi\n", &[]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cannot be used"), "stderr was: {err}");
}

#[test]
fn list_subcommand_lists_presets() {
    let out = run(&["list"], b"", &[]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    for name in ["code-review", "ocr", "summarize", "translate"] {
        assert!(stdout.contains(name), "missing {name} in:\n{stdout}");
    }
}

#[test]
fn custom_preset_dir_actions_work() {
    let dir = std::env::temp_dir().join(format!("aido-test-actions-{}", std::process::id()));
    let presets_dir = dir.join("presets");
    std::fs::create_dir_all(&presets_dir).unwrap();
    std::fs::write(presets_dir.join("polish.toml"), "system = \"polish the text\"\n").unwrap();
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let out = run(
        &["polish", "--base-url", server.url().as_str(), "--no-spinner"],
        b"hi\n",
        &[("AIDO_PRESETS_DIR", presets_dir.to_str().unwrap())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(request_json(&server.request())["messages"][0]["content"], "polish the text");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn png_stdin_becomes_vision_message() {
    let img = image::RgbaImage::from_pixel(3, 3, image::Rgba([200u8, 10, 10, 255]));
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();

    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"red square"}}]}"#);
    let out = run(
        &["--base-url", server.url().as_str(), "--preset", "ocr", "--no-spinner"],
        &png,
        &[],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "red square\n");

    let req = request_json(&server.request());
    let content = &req["messages"][1]["content"];
    assert!(content.is_array());
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "image_url");
    let url = content[1]["image_url"]["url"].as_str().unwrap();
    assert!(url.starts_with("data:image/png;base64,"), "got: {url}");
}

#[test]
fn max_tokens_flag_overrides_and_zero_omits() {
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    run(
        &["--base-url", server.url().as_str(), "--max-tokens", "99", "--no-spinner"],
        b"x\n",
        &[],
    );
    assert_eq!(request_json(&server.request())["max_tokens"], 99);

    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    run(
        &["--base-url", server.url().as_str(), "--max-tokens", "0", "--no-spinner"],
        b"x\n",
        &[],
    );
    assert!(request_json(&server.request()).get("max_tokens").is_none());
}

#[test]
fn temperature_is_sent_when_set() {
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    run(
        &["--base-url", server.url().as_str(), "--temperature", "0.2", "--no-spinner"],
        b"x\n",
        &[],
    );
    assert_eq!(request_json(&server.request())["temperature"], 0.2);
}

#[test]
fn api_error_is_reported() {
    let server = Server::start("401 Unauthorized", r#"{"error":{"message":"bad api key"}}"#);
    let out = run(&["--base-url", server.url().as_str(), "--no-spinner"], b"hi\n", &[]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("bad api key"), "stderr was: {err}");
}

#[test]
fn unknown_preset_fails() {
    let out = run(&["--preset", "nope", "--no-spinner"], b"hi\n", &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown preset"));
}

#[test]
fn unknown_profile_fails() {
    let out = run(&["--profile", "ghost", "--no-spinner"], b"hi\n", &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("profile"));
}

#[test]
fn list_presets_shows_builtins() {
    let out = run(&["--list-presets"], b"", &[]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    for name in ["code-review", "ocr", "summarize", "translate"] {
        assert!(stdout.contains(name), "missing {name} in:\n{stdout}");
    }
}

#[test]
fn profile_from_config_is_used() {
    let dir = std::env::temp_dir().join(format!("aido-test-profile-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"from-local"}}]}"#);
    let cfg_path = dir.join("config.toml");
    let cfg = format!(
        "default_profile = \"local\"\n\n[profiles.local]\nbase_url = \"{}\"\nmodel = \"qwen3\"\n",
        server.url()
    );
    std::fs::write(&cfg_path, cfg).unwrap();

    let out = run(
        &["--no-spinner"],
        b"hi\n",
        &[("AIDO_CONFIG", cfg_path.to_str().unwrap())],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "from-local\n");

    let raw = server.request();
    assert_eq!(request_json(&raw)["model"], "qwen3");
    assert!(request_path(&raw).starts_with("POST /v1/chat/completions"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn init_writes_sample_config_once() {
    let dir = std::env::temp_dir().join(format!("aido-test-init-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg_path = dir.join("config.toml");

    let out = run(&["--init"], b"", &[("AIDO_CONFIG", cfg_path.to_str().unwrap())]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let content = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(content.contains("[profiles.default]"));

    // A second --init must refuse to clobber the existing file.
    let out2 = run(&["--init"], b"", &[("AIDO_CONFIG", cfg_path.to_str().unwrap())]);
    assert!(!out2.status.success());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn base_url_with_explicit_v1_is_not_duplicated() {
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let url = format!("{}/v1", server.url());
    run(&["--base-url", url.as_str(), "--no-spinner"], b"x\n", &[]);
    assert_eq!(request_path(&server.request()), "POST /v1/chat/completions HTTP/1.1");
}

#[test]
fn base_url_without_scheme_fails() {
    let out = run(&["--base-url", "localhost:8080", "--no-spinner"], b"hi\n", &[]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("http://"), "stderr was: {err}");
}

#[test]
fn explicit_missing_config_fails() {
    let out = run(&[], b"hi\n", &[("AIDO_CONFIG", "/nonexistent/aido/config.toml")]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("AIDO_CONFIG"), "stderr was: {err}");
}

#[test]
fn empty_reply_does_not_clobber_clipboard() {
    // clipboard/both modes must fail instead of writing an empty string.
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":""}}]}"#);
    let out = run(
        &["--base-url", server.url().as_str(), "--copy", "--no-spinner"],
        b"hi\n",
        &[],
    );
    assert!(!out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("empty"));

    // stdout mode keeps the soft warning.
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"  "}}]}"#);
    let out = run(
        &["--base-url", server.url().as_str(), "--no-spinner"],
        b"hi\n",
        &[],
    );
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("warning"));
}

#[test]
fn env_vars_fill_unset_flags() {
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    run(
        &["--base-url", server.url().as_str(), "--no-spinner"],
        b"x\n",
        &[("AIDO_MAX_TOKENS", "77"), ("AIDO_TEMPERATURE", "0.3")],
    );
    let req = request_json(&server.request());
    assert_eq!(req["max_tokens"], 77);
    assert_eq!(req["temperature"], 0.3);
}

#[test]
fn invalid_env_number_fails() {
    let out = run(
        &["--base-url", "http://127.0.0.1:1", "--no-spinner"],
        b"x\n",
        &[("AIDO_MAX_TOKENS", "not-a-number")],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("AIDO_MAX_TOKENS"), "stderr was: {err}");
}

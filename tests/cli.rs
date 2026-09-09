use base64::Engine as _;
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
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let status = status.to_string();
        let body = body.to_string();
        let handle = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            let mut stream = accept(&listener, deadline);
            stream.set_nonblocking(false).unwrap();
            let raw = read_request(&mut stream);
            write_response(&mut stream, &status, &body);
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

/// Serves one canned reply per request for `bodies.len()` sequential
/// requests, recording every raw request.
struct MultiServer {
    port: u16,
    handle: JoinHandle<Vec<Vec<u8>>>,
}

impl MultiServer {
    fn start(bodies: &[&str]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let bodies: Vec<String> = bodies.iter().map(|s| s.to_string()).collect();
        let handle = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            let mut requests = Vec::new();
            for body in &bodies {
                let mut stream = accept(&listener, deadline);
                stream.set_nonblocking(false).unwrap();
                requests.push(read_request(&mut stream));
                write_response(&mut stream, "200 OK", body);
            }
            requests
        });
        Self { port, handle }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn requests(self) -> Vec<Vec<u8>> {
        self.handle.join().unwrap()
    }
}

/// Accept one connection before `deadline`, polling instead of blocking in
/// accept() so a regression that makes aido exit before requesting fails
/// the test instead of hanging it.
fn accept(listener: &TcpListener, deadline: std::time::Instant) -> std::net::TcpStream {
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "aido never connected to the test server"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => panic!("accept failed: {e}"),
        }
    }
}

/// Read one full HTTP request (headers plus the content-length body).
fn read_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
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
    raw
}

fn write_response(stream: &mut std::net::TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
}

fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Serves one canned close-delimited response per request (no
/// Content-Length: the body ends with the connection), recording every raw
/// request. Used for SSE replies, which have no fixed length.
struct SseServer {
    port: u16,
    handle: JoinHandle<Vec<Vec<u8>>>,
}

impl SseServer {
    fn start(responses: &[String]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let responses: Vec<String> = responses.to_vec();
        let handle = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            let mut requests = Vec::new();
            for response in &responses {
                let mut stream = accept(&listener, deadline);
                stream.set_nonblocking(false).unwrap();
                requests.push(read_request(&mut stream));
                stream.write_all(response.as_bytes()).unwrap();
            }
            requests
        });
        Self { port, handle }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn requests(self) -> Vec<Vec<u8>> {
        self.handle.join().unwrap()
    }
}

/// A full raw HTTP response carrying `deltas` as SSE `data:` events,
/// followed by `finish_reason` and the `data: [DONE]` sentinel.
fn sse_response(deltas: &[&str], finish_reason: &str) -> String {
    let mut body = String::from(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
    );
    body.push_str("data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n");
    for delta in deltas {
        let text = serde_json::to_string(delta).unwrap();
        body.push_str(&format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":{text}}}}}]}}\n\n"
        ));
    }
    body.push_str(&format!(
        "data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"{finish_reason}\"}}]}}\n\n"
    ));
    body.push_str("data: [DONE]\n\n");
    body
}

static CONFIG_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// An empty but valid config, so tests never read the developer's real
/// ~/.config/aido/config.toml (AIDO_CONFIG must point at an existing
/// file, hence a temp file rather than a nonexistent path). History is
/// off, so ordinary runs never write anywhere either.
fn empty_config() -> std::path::PathBuf {
    let n = CONFIG_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("aido-test-empty-{}-{n}.toml", std::process::id()));
    std::fs::write(&path, "[settings]\nhistory_keep = 0\n").unwrap();
    path
}

static FILE_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A temp input file with a unique name, so parallel tests never collide.
fn temp_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let n = FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("aido-test-file-{}-{n}-{name}", std::process::id()));
    std::fs::write(&path, bytes).unwrap();
    path
}

static DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A fresh history dir, so history tests never touch the developer's
/// real result store (its contents are asserted via `history_files`).
fn temp_history_dir() -> std::path::PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("aido-test-history-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn history_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .collect();
    files.sort();
    files
}

/// A config file with settings, so tests can flip history behavior.
fn settings_config(content: &str) -> std::path::PathBuf {
    let n = CONFIG_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "aido-test-settings-{}-{n}.toml",
        std::process::id()
    ));
    std::fs::write(&path, content).unwrap();
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
        .env("AIDO_PRESETS_DIR", "/nonexistent/aido-test-presets")
        // Same for the history dir: a writable-looking default would let
        // tests write results into the developer's real store.
        .env("AIDO_HISTORY_DIR", "/nonexistent/aido-test-history");
    for var in [
        "OPENAI_API_KEY",
        "AIDO_API_KEY",
        "OPENAI_BASE_URL",
        "AIDO_BASE_URL",
        "AIDO_MODEL",
        "AIDO_PROFILE",
        "AIDO_MAX_TOKENS",
        "AIDO_TEMPERATURE",
        // No display: clipboard writes fail deterministically here instead
        // of overwriting the developer's real clipboard during tests.
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XDG_SESSION_TYPE",
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
    String::from_utf8_lossy(raw)
        .lines()
        .next()
        .unwrap()
        .to_string()
}

#[test]
fn text_input_stdout_output() {
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"WORLD"}}]}"#);
    let out = run(
        &[
            "--base-url",
            server.url().as_str(),
            "-m",
            "test-model",
            "--no-spinner",
        ],
        b"hello\n",
        &[],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "WORLD\n");

    let raw = server.request();
    let req = request_json(&raw);
    assert_eq!(req["model"], "test-model");
    assert_eq!(req["messages"][0]["role"], "user");
    assert_eq!(req["messages"][0]["content"], "hello\n");
    assert_eq!(req["max_tokens"], 8192);
    assert!(req.get("temperature").is_none());
    assert!(req.get("stream").is_none());
    assert!(!String::from_utf8_lossy(&raw)
        .to_lowercase()
        .contains("authorization"));
    // base_url without a path gets "/v1" appended
    assert_eq!(request_path(&raw), "POST /v1/chat/completions HTTP/1.1");
}

#[test]
fn system_prompt_via_flag() {
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let out = run(
        &[
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            "-p",
            "be brief",
        ],
        b"hi\n",
        &[],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let req = request_json(&server.request());
    assert_eq!(req["messages"][0]["role"], "system");
    assert_eq!(req["messages"][0]["content"], "be brief");
    assert_eq!(req["messages"][1]["role"], "user");
}

#[test]
fn preset_supplies_system_prompt() {
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let out = run(
        &[
            "--base-url",
            server.url().as_str(),
            "--preset",
            "ocr",
            "--no-spinner",
        ],
        b"some text\n",
        &[],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let req = request_json(&server.request());
    assert_eq!(req["messages"][0]["role"], "system");
    let sys = req["messages"][0]["content"].as_str().unwrap();
    assert!(sys.to_lowercase().contains("ocr"));
    assert_eq!(req["messages"][1]["content"], "some text\n");
}

#[test]
fn action_name_must_come_first() {
    // With flags first there is nowhere for 'ocr' to land — the action slot
    // is gone, so 'ocr' parses as a FILE and the error must point out that
    // actions come first instead of being sent to the model as a prompt.
    let out = run(
        &["--base-url", "http://127.0.0.1:1", "--no-spinner", "ocr"],
        b"hi\n",
        &[],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("action name"), "stderr was: {err}");
    assert!(err.contains("first argument"), "stderr was: {err}");
}

#[test]
fn unknown_action_fails_with_suggestion() {
    let out = run(&["transalte", "--no-spinner"], b"hi\n", &[]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown action"), "stderr was: {err}");
    assert!(
        err.contains("did you mean 'translate'"),
        "stderr was: {err}"
    );
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
    assert!(
        err.contains("did you mean 'translate'"),
        "stderr was: {err}"
    );
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
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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
    std::fs::write(
        presets_dir.join("polish.toml"),
        "system = \"polish the text\"\n",
    )
    .unwrap();
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let out = run(
        &[
            "polish",
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
        ],
        b"hi\n",
        &[("AIDO_PRESETS_DIR", presets_dir.to_str().unwrap())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        request_json(&server.request())["messages"][0]["content"],
        "polish the text"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn preset_overrides_profile_for_api_params() {
    // A preset acts as a profile scoped to its action: base_url, model,
    // api_key, max_tokens and temperature all win over the config profile.
    let dir = std::env::temp_dir().join(format!("aido-test-preset-ovr-{}", std::process::id()));
    let presets_dir = dir.join("presets");
    std::fs::create_dir_all(&presets_dir).unwrap();
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    std::fs::write(
        presets_dir.join("special.toml"),
        format!(
            "system = \"be brief\"\nbase_url = \"{}/preset\"\nmodel = \"preset-model\"\n\
             api_key = \"preset-key\"\nmax_tokens = 555\ntemperature = 0.4\n",
            server.url()
        ),
    )
    .unwrap();
    let cfg_path = dir.join("config.toml");
    std::fs::write(
        &cfg_path,
        format!(
            "[profiles.default]\nbase_url = \"{}/profile\"\nmodel = \"profile-model\"\n\
             api_key = \"profile-key\"\n",
            server.url()
        ),
    )
    .unwrap();

    let out = run(
        &["special", "--no-spinner"],
        b"hi\n",
        &[
            ("AIDO_CONFIG", cfg_path.to_str().unwrap()),
            ("AIDO_PRESETS_DIR", presets_dir.to_str().unwrap()),
        ],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let raw = server.request();
    assert_eq!(request_path(&raw), "POST /preset/chat/completions HTTP/1.1");
    let req = request_json(&raw);
    assert_eq!(req["model"], "preset-model");
    assert_eq!(req["max_tokens"], 555);
    assert_eq!(req["temperature"], 0.4);
    let raw_text = String::from_utf8_lossy(&raw).to_lowercase();
    assert!(raw_text.contains("bearer preset-key"), "was: {raw_text}");
    assert!(!raw_text.contains("profile-key"), "was: {raw_text}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn preset_params_beat_env_flags_still_win() {
    // Preset overrides outrank env vars (OPENAI_* vars often belong to
    // other tools), while CLI flags outrank everything.
    let dir = std::env::temp_dir().join(format!("aido-test-preset-env-{}", std::process::id()));
    let presets_dir = dir.join("presets");
    std::fs::create_dir_all(&presets_dir).unwrap();
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    std::fs::write(
        presets_dir.join("special.toml"),
        "system = \"be brief\"\nmodel = \"preset-model\"\n",
    )
    .unwrap();
    let cfg_path = dir.join("config.toml");
    // AIDO_CONFIG must point at an existing file; an empty one means "no
    // profiles defined", keeping the preset as the only config layer here.
    std::fs::write(&cfg_path, "").unwrap();
    let common: &[(&str, &str)] = &[
        ("AIDO_CONFIG", cfg_path.to_str().unwrap()),
        ("AIDO_PRESETS_DIR", presets_dir.to_str().unwrap()),
    ];

    // the preset's model wins over AIDO_MODEL ...
    let out = run(
        &[
            "special",
            "--no-spinner",
            "--base-url",
            server.url().as_str(),
        ],
        b"hi\n",
        &[common[0], common[1], ("AIDO_MODEL", "env-model")],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(request_json(&server.request())["model"], "preset-model");

    // ... but an explicit CLI flag wins over the preset
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let out = run(
        &[
            "special",
            "--no-spinner",
            "--base-url",
            server.url().as_str(),
            "-m",
            "flag-model",
        ],
        b"hi\n",
        common,
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(request_json(&server.request())["model"], "flag-model");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn env_beats_profile() {
    // Below the preset layer the shipped chain is unchanged: env vars
    // override the config profile.
    let dir = std::env::temp_dir().join(format!("aido-test-env-profile-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let cfg_path = dir.join("config.toml");
    std::fs::write(
        &cfg_path,
        format!(
            "[profiles.default]\nbase_url = \"{}\"\nmodel = \"profile-model\"\n",
            server.url()
        ),
    )
    .unwrap();

    let out = run(
        &["--no-spinner"],
        b"hi\n",
        &[
            ("AIDO_CONFIG", cfg_path.to_str().unwrap()),
            ("AIDO_MODEL", "env-model"),
        ],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(request_json(&server.request())["model"], "env-model");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn list_marks_presets_with_api_overrides() {
    // Presets carrying API params are tagged with the overridden field
    // names (values are never shown, so keys stay off the terminal).
    let dir = std::env::temp_dir().join(format!("aido-test-preset-list-{}", std::process::id()));
    let presets_dir = dir.join("presets");
    std::fs::create_dir_all(&presets_dir).unwrap();
    std::fs::write(
        presets_dir.join("special.toml"),
        "system = \"be brief\"\nmodel = \"m\"\napi_key = \"k\"\n",
    )
    .unwrap();
    let out = run(
        &["list"],
        b"",
        &[("AIDO_PRESETS_DIR", presets_dir.to_str().unwrap())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("special"), "stdout was:\n{stdout}");
    assert!(stdout.contains("[api_key, model]"), "stdout was:\n{stdout}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn png_stdin_becomes_vision_message() {
    let img = image::RgbaImage::from_pixel(3, 3, image::Rgba([200u8, 10, 10, 255]));
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();

    let server = Server::start(
        "200 OK",
        r#"{"choices":[{"message":{"content":"red square"}}]}"#,
    );
    let out = run(
        &[
            "--base-url",
            server.url().as_str(),
            "--preset",
            "ocr",
            "--no-spinner",
        ],
        &png,
        &[],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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
fn text_file_input() {
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let file = temp_file("notes.txt", b"file body\n");
    let out = run(
        &[
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            "-p",
            "be brief",
            file.to_str().unwrap(),
        ],
        b"",
        &[],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let req = request_json(&server.request());
    assert_eq!(req["messages"][0]["role"], "system");
    assert_eq!(req["messages"][1]["role"], "user");
    assert_eq!(req["messages"][1]["content"], "file body\n");
    std::fs::remove_file(&file).ok();
}

#[test]
fn image_file_becomes_vision_message() {
    let img = image::RgbaImage::from_pixel(3, 3, image::Rgba([200u8, 10, 10, 255]));
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    let file = temp_file("shot.png", &png);

    let server = Server::start(
        "200 OK",
        r#"{"choices":[{"message":{"content":"red square"}}]}"#,
    );
    let out = run(
        &[
            "ocr", // action syntax: preset first, then the file
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            file.to_str().unwrap(),
        ],
        b"",
        &[],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let req = request_json(&server.request());
    let content = &req["messages"][1]["content"];
    assert!(content.is_array());
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "image_url");
    // PNG files are passed through byte-for-byte
    let url = content[1]["image_url"]["url"].as_str().unwrap();
    let payload = url.strip_prefix("data:image/png;base64,").unwrap();
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(payload)
            .unwrap(),
        png
    );
    std::fs::remove_file(&file).ok();
}

#[test]
fn jpeg_file_is_reencoded_as_png() {
    let img = image::RgbaImage::from_pixel(3, 3, image::Rgba([200u8, 10, 10, 255]));
    let mut jpg = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut jpg),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    let file = temp_file("shot.jpg", &jpg);

    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let out = run(
        &[
            "ocr",
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            file.to_str().unwrap(),
        ],
        b"",
        &[],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let req = request_json(&server.request());
    let content = &req["messages"][1]["content"];
    assert!(content.is_array());
    let url = content[1]["image_url"]["url"].as_str().unwrap();
    assert!(url.starts_with("data:image/png;base64,"), "got: {url}");
    std::fs::remove_file(&file).ok();
}

#[test]
fn mixed_text_and_image_files() {
    let img = image::RgbaImage::from_pixel(3, 3, image::Rgba([200u8, 10, 10, 255]));
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    let png_file = temp_file("shot.png", &png);
    let txt_file = temp_file("notes.txt", b"see the picture\n");

    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let out = run(
        &[
            "ocr",
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            png_file.to_str().unwrap(),
            txt_file.to_str().unwrap(),
        ],
        b"",
        &[],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let req = request_json(&server.request());
    let content = &req["messages"][1]["content"];
    assert!(content.is_array());
    // the user's own text becomes the text part, images follow
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "see the picture\n");
    assert_eq!(content[1]["type"], "image_url");
    assert_eq!(content.as_array().unwrap().len(), 2);
    std::fs::remove_file(&png_file).ok();
    std::fs::remove_file(&txt_file).ok();
}

#[test]
fn multiple_text_files_are_labeled() {
    let a = temp_file("a.txt", b"alpha\n");
    let b = temp_file("b.txt", b"beta\n");

    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let out = run(
        &[
            "summarize",
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
        ],
        b"",
        &[],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let req = request_json(&server.request());
    let content = req["messages"][1]["content"].as_str().unwrap();
    assert!(
        content.contains("--- "),
        "missing file labels in:\n{content}"
    );
    assert!(content.contains("alpha"), "missing alpha in:\n{content}");
    assert!(content.contains("beta"), "missing beta in:\n{content}");
    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[test]
fn bare_file_requires_an_action() {
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let file = temp_file("notes.txt", b"just a file\n");
    let out = run(
        &[
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            file.to_str().unwrap(),
        ],
        b"",
        &[],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("requires an action"), "stderr was: {err}");
    assert!(err.contains("notes.txt"), "stderr was: {err}");
    std::fs::remove_file(&file).ok();
}

#[test]
fn cjk_filename_is_file_input_not_an_action() {
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let file = temp_file("笔记.txt", "中文内容\n".as_bytes());

    // A CJK path in the action slot is file input, not a typo'd action;
    // the error must ask for an action, not report "unknown action".
    let out = run(
        &[
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            file.to_str().unwrap(),
        ],
        b"",
        &[],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("requires an action"), "stderr was: {err}");
    assert!(!err.contains("unknown action"), "stderr was: {err}");

    // The same file, still first, works once a prompt is appended.
    let out = run(
        &[
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            file.to_str().unwrap(),
            "-p",
            "be brief",
        ],
        b"",
        &[],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let req = request_json(&server.request());
    assert_eq!(req["messages"][1]["content"], "中文内容\n");
    std::fs::remove_file(&file).ok();
}

#[test]
fn files_take_priority_over_stdin() {
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let file = temp_file("notes.txt", b"from file\n");
    let out = run(
        &[
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            "-p",
            "be brief",
            file.to_str().unwrap(),
        ],
        b"from stdin\n",
        &[],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let req = request_json(&server.request());
    assert_eq!(req["messages"][1]["content"], "from file\n");
    std::fs::remove_file(&file).ok();
}

#[test]
fn missing_file_fails() {
    let out = run(&["ocr", "--no-spinner", "nope.png"], b"", &[]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("nope.png"), "stderr was: {err}");
}

#[test]
fn empty_file_fails_instead_of_falling_back() {
    let file = temp_file("empty.txt", b"");
    let out = run(
        &[
            "summarize",
            "--base-url",
            "http://127.0.0.1:1",
            "--no-spinner",
            file.to_str().unwrap(),
        ],
        b"",
        &[],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("no text or image content"),
        "stderr was: {err}"
    );
    std::fs::remove_file(&file).ok();
}

#[test]
fn directory_input_fails() {
    let dir = std::env::temp_dir().join(format!("aido-test-input-dir-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = run(
        &[
            "summarize",
            "--base-url",
            "http://127.0.0.1:1",
            "--no-spinner",
            dir.to_str().unwrap(),
        ],
        b"",
        &[],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("directory"), "stderr was: {err}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn whitespace_file_is_skipped_with_a_warning() {
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let blank = temp_file("blank.txt", b"   \n\n  ");
    let real = temp_file("real.txt", b"real content\n");
    let out = run(
        &[
            "summarize",
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            blank.to_str().unwrap(),
            real.to_str().unwrap(),
        ],
        b"",
        &[],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("warning"), "stderr was: {err}");
    assert!(err.contains("blank.txt"), "stderr was: {err}");
    let req = request_json(&server.request());
    assert_eq!(req["messages"][1]["content"], "real content\n");
    std::fs::remove_file(&blank).ok();
    std::fs::remove_file(&real).ok();
}

#[test]
fn oversized_file_is_refused() {
    // set_len creates a sparse file: 33 MB of logical size, ~0 bytes on disk.
    let file = std::env::temp_dir().join(format!("aido-test-huge-{}.bin", std::process::id()));
    std::fs::File::create(&file)
        .unwrap()
        .set_len(33 * 1024 * 1024)
        .unwrap();
    let out = run(
        &[
            "summarize",
            "--base-url",
            "http://127.0.0.1:1",
            "--no-spinner",
            file.to_str().unwrap(),
        ],
        b"",
        &[],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("32 MB"), "stderr was: {err}");
    std::fs::remove_file(&file).ok();
}

#[test]
fn max_tokens_flag_overrides_and_zero_omits() {
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    run(
        &[
            "--base-url",
            server.url().as_str(),
            "--max-tokens",
            "99",
            "--no-spinner",
        ],
        b"x\n",
        &[],
    );
    assert_eq!(request_json(&server.request())["max_tokens"], 99);

    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    run(
        &[
            "--base-url",
            server.url().as_str(),
            "--max-tokens",
            "0",
            "--no-spinner",
        ],
        b"x\n",
        &[],
    );
    assert!(request_json(&server.request()).get("max_tokens").is_none());
}

#[test]
fn temperature_is_sent_when_set() {
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    run(
        &[
            "--base-url",
            server.url().as_str(),
            "--temperature",
            "0.2",
            "--no-spinner",
        ],
        b"x\n",
        &[],
    );
    assert_eq!(request_json(&server.request())["temperature"], 0.2);
}

#[test]
fn api_error_is_reported() {
    let server = Server::start("401 Unauthorized", r#"{"error":{"message":"bad api key"}}"#);
    let out = run(
        &["--base-url", server.url().as_str(), "--no-spinner"],
        b"hi\n",
        &[],
    );
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
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for name in ["code-review", "ocr", "summarize", "translate"] {
        assert!(stdout.contains(name), "missing {name} in:\n{stdout}");
    }
}

#[test]
fn profile_from_config_is_used() {
    let dir = std::env::temp_dir().join(format!("aido-test-profile-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let server = Server::start(
        "200 OK",
        r#"{"choices":[{"message":{"content":"from-local"}}]}"#,
    );
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
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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

    let out = run(
        &["--init"],
        b"",
        &[("AIDO_CONFIG", cfg_path.to_str().unwrap())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let content = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(content.contains("[profiles.default]"));

    // A second --init must refuse to clobber the existing file.
    let out2 = run(
        &["--init"],
        b"",
        &[("AIDO_CONFIG", cfg_path.to_str().unwrap())],
    );
    assert!(!out2.status.success());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn base_url_with_explicit_v1_is_not_duplicated() {
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let url = format!("{}/v1", server.url());
    run(&["--base-url", url.as_str(), "--no-spinner"], b"x\n", &[]);
    assert_eq!(
        request_path(&server.request()),
        "POST /v1/chat/completions HTTP/1.1"
    );
}

#[test]
fn base_url_without_scheme_fails() {
    let out = run(
        &["--base-url", "localhost:8080", "--no-spinner"],
        b"hi\n",
        &[],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("http://"), "stderr was: {err}");
}

#[test]
fn explicit_missing_config_fails() {
    let out = run(
        &[],
        b"hi\n",
        &[("AIDO_CONFIG", "/nonexistent/aido/config.toml")],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("AIDO_CONFIG"), "stderr was: {err}");
}

#[test]
fn empty_reply_does_not_clobber_clipboard() {
    // clipboard/both modes must fail instead of writing an empty string.
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":""}}]}"#);
    let out = run(
        &[
            "--base-url",
            server.url().as_str(),
            "--copy",
            "--no-spinner",
        ],
        b"hi\n",
        &[],
    );
    assert!(
        !out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("empty"));

    // stdout mode keeps the soft warning.
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"  "}}]}"#);
    let out = run(
        &["--base-url", server.url().as_str(), "--no-spinner"],
        b"hi\n",
        &[],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
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

/// A solid image taller than the split threshold; every row is quiet, so
/// the seam lands exactly on the target slice height (2000).
fn tall_png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba([255u8, 255, 255, 255]));
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    png
}

fn image_part_dims(req: &serde_json::Value) -> (u32, u32) {
    let content = &req["messages"][1]["content"];
    let url = content[content.as_array().unwrap().len() - 1]["image_url"]["url"]
        .as_str()
        .unwrap();
    let payload = url.strip_prefix("data:image/png;base64,").unwrap();
    let png = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .unwrap();
    let img = image::load_from_memory(&png).unwrap();
    use image::GenericImageView as _;
    img.dimensions()
}

#[test]
fn tall_image_is_split_into_sequential_requests() {
    let png = tall_png(64, 3200);
    let file = temp_file("long.png", &png);
    let server = MultiServer::start(&[
        r#"{"choices":[{"message":{"content":"first half"}}]}"#,
        r#"{"choices":[{"message":{"content":"second half"}}]}"#,
    ]);
    let out = run(
        &[
            "ocr",
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            file.to_str().unwrap(),
        ],
        b"",
        &[],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // the replies concatenate in slice order
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "first half\nsecond half\n"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("split into 2 slices"), "stderr was: {err}");

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let first = request_json(&requests[0]);
    let second = request_json(&requests[1]);
    // one image per request, tiling the original exactly
    assert_eq!(image_part_dims(&first), (64, 2000));
    assert_eq!(image_part_dims(&second), (64, 1200));
    // the follow-up request explains itself as a slice instead of
    // reusing the default caption text
    let first_text = first["messages"][1]["content"][0]["text"].as_str().unwrap();
    let second_text = second["messages"][1]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(!first_text.contains("slice"), "was: {first_text}");
    assert!(second_text.contains("slice"), "was: {second_text}");
    std::fs::remove_file(&file).ok();
}

#[test]
fn no_split_flag_sends_the_whole_image() {
    let png = tall_png(64, 3200);
    let file = temp_file("long.png", &png);
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"whole"}}]}"#);
    let out = run(
        &[
            "ocr",
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            "--no-split",
            file.to_str().unwrap(),
        ],
        b"",
        &[],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let req = request_json(&server.request());
    let content = &req["messages"][1]["content"];
    assert_eq!(content.as_array().unwrap().len(), 2);
    // PNG bytes pass through untouched, exactly as before
    let url = content[1]["image_url"]["url"].as_str().unwrap();
    let payload = url.strip_prefix("data:image/png;base64,").unwrap();
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(payload)
            .unwrap(),
        png
    );
    std::fs::remove_file(&file).ok();
}

#[test]
fn truncated_reply_warns() {
    let server = Server::start(
        "200 OK",
        r#"{"choices":[{"message":{"content":"partial"},"finish_reason":"length"}]}"#,
    );
    let out = run(
        &["--base-url", server.url().as_str(), "--no-spinner"],
        b"hi\n",
        &[],
    );
    // the (partial) content is still emitted; truncation only warns
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "partial\n");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("truncated"), "stderr was: {err}");
    assert!(err.contains("--max-tokens"), "stderr was: {err}");
}

#[test]
fn history_is_on_by_default_and_last_prints_the_result() {
    let dir = temp_history_dir();
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"SAVED"}}]}"#);
    let out = run(
        &["--base-url", server.url().as_str(), "--no-spinner"],
        b"hi\n",
        &[
            ("AIDO_CONFIG", settings_config("").to_str().unwrap()),
            ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
        ],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let files = history_files(&dir);
    assert_eq!(files.len(), 1, "expected exactly one history entry");
    assert_eq!(std::fs::read_to_string(&files[0]).unwrap(), "SAVED");

    // The result survives a lost clipboard: `aido last` prints it again.
    let out = run(
        &["last"],
        b"",
        &[("AIDO_HISTORY_DIR", dir.to_str().unwrap())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "SAVED\n");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn history_keeps_only_the_newest_n() {
    let dir = temp_history_dir();
    let cfg = settings_config("[settings]\nhistory_keep = 1\n");
    let envs = [
        ("AIDO_CONFIG", cfg.to_str().unwrap()),
        ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
    ];

    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"FIRST"}}]}"#);
    let out = run(
        &["--base-url", server.url().as_str(), "--no-spinner"],
        b"hi\n",
        &envs,
    );
    assert!(out.status.success());
    server.request();

    let server = Server::start(
        "200 OK",
        r#"{"choices":[{"message":{"content":"SECOND"}}]}"#,
    );
    let out = run(
        &["--base-url", server.url().as_str(), "--no-spinner"],
        b"hi\n",
        &envs,
    );
    assert!(out.status.success());
    server.request();

    let files = history_files(&dir);
    assert_eq!(
        files.len(),
        1,
        "history_keep = 1 must prune the older entry"
    );
    let out = run(
        &["last"],
        b"",
        &[("AIDO_HISTORY_DIR", dir.to_str().unwrap())],
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "SECOND\n");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn history_disabled_by_config_writes_nothing() {
    let dir = temp_history_dir();
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let out = run(
        &["--base-url", server.url().as_str(), "--no-spinner"],
        b"hi\n",
        &[
            (
                "AIDO_CONFIG",
                settings_config("[settings]\nhistory_keep = 0\n")
                    .to_str()
                    .unwrap(),
            ),
            ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
        ],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(history_files(&dir).is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn last_without_saved_results_fails() {
    // An existing but empty dir...
    let dir = temp_history_dir();
    let out = run(
        &["last"],
        b"",
        &[("AIDO_HISTORY_DIR", dir.to_str().unwrap())],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no saved results"), "stderr was: {err}");
    std::fs::remove_dir_all(&dir).ok();

    // ...and a missing one — both are "no history yet", not a read error.
    let out = run(
        &["last"],
        b"",
        &[("AIDO_HISTORY_DIR", "/nonexistent/aido-test-hist")],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no saved results"), "stderr was: {err}");
}

#[test]
fn save_flag_writes_the_result_to_the_file() {
    let server = Server::start(
        "200 OK",
        r#"{"choices":[{"message":{"content":"TO FILE"}}]}"#,
    );
    let out_path = std::env::temp_dir().join(format!(
        "aido-test-save-{}-{}.txt",
        std::process::id(),
        DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let out = run(
        &[
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            "--save",
            out_path.to_str().unwrap(),
        ],
        b"hi\n",
        &[],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(&out_path).unwrap(), "TO FILE");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("saved"), "stderr was: {err}");
    std::fs::remove_file(&out_path).ok();
}

#[test]
fn save_flag_creates_missing_parent_dirs() {
    let server = Server::start(
        "200 OK",
        r#"{"choices":[{"message":{"content":"NESTED"}}]}"#,
    );
    let dir = std::env::temp_dir().join(format!(
        "aido-test-save-dirs-{}-{}",
        std::process::id(),
        DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let out_path = dir.join("nested").join("out.txt");
    let out = run(
        &[
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            "--save",
            out_path.to_str().unwrap(),
        ],
        b"hi\n",
        &[],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(&out_path).unwrap(), "NESTED");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn save_flag_skips_empty_reply() {
    // The model's empty reply must not produce an empty file that reads
    // as saved content.
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":""}}]}"#);
    let out_path = std::env::temp_dir().join(format!(
        "aido-test-save-empty-{}-{}.txt",
        std::process::id(),
        DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let out = run(
        &[
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            "--save",
            out_path.to_str().unwrap(),
        ],
        b"hi\n",
        &[],
    );
    assert!(out.status.success());
    assert!(!out_path.exists());
}

#[test]
fn last_honors_parent_flags() {
    // Parent flags must reach the subcommand: `aido --save f last` and
    // `aido -c last` used to be silently ignored.
    let dir = temp_history_dir();
    std::fs::write(dir.join("20260909-120000.000.txt"), "PARENT FLAGS").unwrap();

    let save_path = std::env::temp_dir().join(format!(
        "aido-test-last-save-{}-{}.txt",
        std::process::id(),
        DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let out = run(
        &["--save", save_path.to_str().unwrap(), "last"],
        b"",
        &[("AIDO_HISTORY_DIR", dir.to_str().unwrap())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(&save_path).unwrap(), "PARENT FLAGS");
    std::fs::remove_file(&save_path).ok();

    // With copy intent (from -c or -o both) the result goes to the
    // clipboard (where one exists); either way it must never land on
    // stdout.
    for args in [
        vec!["-c", "last"],
        vec!["-o", "both", "last"],
        vec!["--output", "clipboard", "last"],
    ] {
        let out = run(&args, b"", &[("AIDO_HISTORY_DIR", dir.to_str().unwrap())]);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "",
            "args {args:?} must not print"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn empty_reply_is_not_recorded() {
    let dir = temp_history_dir();
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":""}}]}"#);
    let out = run(
        &["--base-url", server.url().as_str(), "--no-spinner"],
        b"hi\n",
        &[
            ("AIDO_CONFIG", settings_config("").to_str().unwrap()),
            ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
        ],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("warning"));
    assert!(history_files(&dir).is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn stream_flag_prints_deltas_and_sends_stream_true() {
    let server = SseServer::start(&[sse_response(&["Hello", ", ", "世界"], "stop")]);
    let out = run(
        &[
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            "--stream",
        ],
        b"hi\n",
        &[],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // deltas arrive concatenated, closed by one trailing newline
    assert_eq!(String::from_utf8_lossy(&out.stdout), "Hello, 世界\n");
    assert_eq!(request_json(&server.requests().remove(0))["stream"], true);
}

#[test]
fn settings_stream_enables_streaming() {
    let server = SseServer::start(&[sse_response(&["from config"], "stop")]);
    let cfg = settings_config("[settings]\nstream = true\n");
    let out = run(
        &["--base-url", server.url().as_str(), "--no-spinner"],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "from config\n");
    assert_eq!(request_json(&server.requests().remove(0))["stream"], true);
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn no_stream_flag_overrides_settings() {
    // settings.stream = true, but the CLI pair wins: --no-stream turns it off
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let cfg = settings_config("[settings]\nstream = true\n");
    let out = run(
        &[
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            "--no-stream",
        ],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(request_json(&server.request()).get("stream").is_none());
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn stream_error_payload_fails_the_run() {
    // Some gateways report failures inside the stream despite HTTP 200.
    let body = concat!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        "data: {\"error\":{\"message\":\"model overloaded\"}}\n\n",
    );
    let server = SseServer::start(&[body.to_string()]);
    let out = run(
        &[
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            "--stream",
        ],
        b"hi\n",
        &[],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("model overloaded"), "stderr was: {err}");
}

#[test]
fn streamed_truncated_reply_warns() {
    let server = SseServer::start(&[sse_response(&["partial"], "length")]);
    let out = run(
        &[
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            "--stream",
        ],
        b"hi\n",
        &[],
    );
    // the (partial) content is still emitted; truncation only warns
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "partial\n");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("truncated"), "stderr was: {err}");
}

#[test]
fn stream_with_clipboard_output_stays_buffered() {
    // Streaming shows nothing for a clipboard-only run: the request must
    // stay buffered and a note explains why. The clipboard write itself
    // fails on a headless Linux box but succeeds on macOS/Windows, so the
    // exit status is deliberately not asserted here.
    let server = Server::start("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let out = run(
        &[
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            "--stream",
            "--copy",
        ],
        b"hi\n",
        &[],
    );
    assert!(request_json(&server.request()).get("stream").is_none());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("note: streaming"), "stderr was: {err}");
}

#[test]
fn streamed_slices_are_separated_like_the_buffered_join() {
    let png = tall_png(64, 3200);
    let file = temp_file("long.png", &png);
    let server = SseServer::start(&[
        sse_response(&["first half"], "stop"),
        sse_response(&["second half"], "stop"),
    ]);
    let out = run(
        &[
            "ocr",
            "--base-url",
            server.url().as_str(),
            "--no-spinner",
            "--stream",
            file.to_str().unwrap(),
        ],
        b"",
        &[],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // slice replies concatenate in order, joined by exactly one "\n"
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "first half\nsecond half\n"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(request_json(&requests[0])["stream"], true);
    assert_eq!(request_json(&requests[1])["stream"], true);
    std::fs::remove_file(&file).ok();
}

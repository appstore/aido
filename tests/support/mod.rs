//! Shared integration-test harness: a one-shot local HTTP server that
//! records raw requests, plus helpers that run the aido binary in
//! isolated config/tasks/history directories.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;

pub const EXE: &str = env!("CARGO_BIN_EXE_aido");

/// A one-shot HTTP server that records the raw request and replies with a
/// canned body, so tests can assert on the exact JSON aido sends.
pub struct Server {
    pub port: u16,
    handle: JoinHandle<Vec<u8>>,
}

impl Server {
    pub fn start(status: &str, body: &str) -> Self {
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

    pub fn json(body: &str) -> Self {
        Self::start("200 OK", body)
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn request(self) -> Vec<u8> {
        self.handle.join().unwrap()
    }
}

/// Serves one canned reply per request for `bodies.len()` sequential
/// requests, recording every raw request.
pub struct MultiServer {
    pub port: u16,
    handle: JoinHandle<Vec<Vec<u8>>>,
}

impl MultiServer {
    pub fn start(bodies: &[&str]) -> Self {
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

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn requests(self) -> Vec<Vec<u8>> {
        self.handle.join().unwrap()
    }
}

/// Accept one connection before `deadline`, polling instead of blocking so
/// a regression that makes aido exit before requesting fails the test
/// instead of hanging it.
pub fn accept(listener: &TcpListener, deadline: std::time::Instant) -> std::net::TcpStream {
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
pub fn read_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
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

pub fn write_response(stream: &mut std::net::TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
}

pub fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Serves one canned close-delimited response per request (no
/// Content-Length), recording every raw request. Used for SSE and binary
/// replies, which have no fixed length.
pub struct SseServer {
    pub port: u16,
    handle: JoinHandle<Vec<Vec<u8>>>,
}

impl SseServer {
    pub fn start(responses: &[String]) -> Self {
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

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn requests(self) -> Vec<Vec<u8>> {
        self.handle.join().unwrap()
    }
}

/// A full raw HTTP response carrying `deltas` as SSE `data:` events,
/// followed by `finish_reason` and the `data: [DONE]` sentinel.
pub fn sse_response(deltas: &[&str], finish_reason: &str) -> String {
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
static FILE_COUNTER: AtomicUsize = AtomicUsize::new(0);
static DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A valid config with history off, so ordinary runs never write anywhere.
pub fn empty_config() -> std::path::PathBuf {
    let n = CONFIG_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("aido-test-empty-{}-{n}.toml", std::process::id()));
    std::fs::write(&path, "[settings]\nhistory_keep = 0\n").unwrap();
    path
}

/// A config with custom settings content.
pub fn settings_config(content: &str) -> std::path::PathBuf {
    let n = CONFIG_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "aido-test-settings-{}-{n}.toml",
        std::process::id()
    ));
    std::fs::write(&path, content).unwrap();
    path
}

/// A temp input file with a unique name, so parallel tests never collide.
pub fn temp_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let n = FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("aido-test-file-{}-{n}-{name}", std::process::id()));
    std::fs::write(&path, bytes).unwrap();
    path
}

/// A fresh unique directory.
pub fn temp_dir(tag: &str) -> std::path::PathBuf {
    let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("aido-test-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

pub struct RunOutcome {
    pub output: std::process::Output,
}

impl RunOutcome {
    pub fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.output.stdout).into_owned()
    }
    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }
    pub fn code(&self) -> i32 {
        self.output.status.code().unwrap_or(-1)
    }
    pub fn ok(&self) -> bool {
        self.output.status.success()
    }
    /// The exit code must equal `expected` and stderr must not be lost.
    pub fn assert_code(&self, expected: i32) {
        assert_eq!(
            self.code(),
            expected,
            "exit code: stderr was: {}",
            self.stderr()
        );
    }
}

/// Run the aido binary with isolated config/tasks/history; every test gets
/// a silent environment by default.
pub fn run(args: &[&str], stdin_data: &[u8], envs: &[(&str, &str)]) -> RunOutcome {
    run_with(args, stdin_data, envs, empty_config())
}

/// Like [`run`] but with an explicit config path.
pub fn run_with(
    args: &[&str],
    stdin_data: &[u8],
    envs: &[(&str, &str)],
    config: impl AsRef<std::ffi::OsStr>,
) -> RunOutcome {
    let mut cmd = Command::new(EXE);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("AIDO_CONFIG", config.as_ref())
        .env("AIDO_TASKS_DIR", "/nonexistent/aido-test-tasks")
        .env("AIDO_HISTORY_DIR", "/nonexistent/aido-test-history");
    for var in [
        "OPENAI_API_KEY",
        "AIDO_API_KEY",
        "AIDO_PROFILE",
        "AIDO_MODEL",
        "AIDO_BASE_URL",
        "AIDO_ADAPTER",
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
    RunOutcome { output: out }
}

pub fn request_json(raw: &[u8]) -> serde_json::Value {
    let pos = find_sub(raw, b"\r\n\r\n").unwrap();
    serde_json::from_slice(&raw[pos + 4..]).unwrap()
}

pub fn request_path(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw)
        .lines()
        .next()
        .unwrap()
        .to_string()
}

pub fn chat_body(text: &str) -> &'static str {
    // Leak: test binaries are short-lived. The content is JSON-escaped so
    // newlines and quotes in replies stay valid wire data.
    let content = serde_json::to_string(text).unwrap();
    Box::leak(format!(r#"{{"choices":[{{"message":{{"content":{content}}}}}]}}"#).into_boxed_str())
}

/// A solid image taller than the split threshold; every row is quiet, so
/// the seam lands exactly on the target slice height.
pub fn solid_png(w: u32, h: u32) -> Vec<u8> {
    let img = image_png(w, h);
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    png
}

pub fn image_png(w: u32, h: u32) -> image::RgbaImage {
    image::RgbaImage::from_pixel(w, h, image::Rgba([255u8, 255, 255, 255]))
}

/// Run aido with a pseudo-terminal as stdin, for tests of "terminal
/// stdin" rows of the decision table (file material without `-`).
/// Unix-only: CI runs the suite on Linux and macOS.
#[cfg(unix)]
pub fn run_tty(args: &[&str], envs: &[(&str, &str)]) -> RunOutcome {
    run_tty_with(args, envs, empty_config())
}

#[cfg(unix)]
pub fn run_tty_with(
    args: &[&str],
    envs: &[(&str, &str)],
    config: std::path::PathBuf,
) -> RunOutcome {
    use std::os::fd::FromRawFd;
    let mut master = 0i32;
    let mut slave = 0i32;
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(rc, 0, "openpty failed");
    let mut cmd = Command::new(EXE);
    cmd.args(args)
        .stdin(unsafe { Stdio::from_raw_fd(slave) })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("AIDO_CONFIG", &config)
        .env("AIDO_TASKS_DIR", "/nonexistent/aido-test-tasks")
        .env("AIDO_HISTORY_DIR", "/nonexistent/aido-test-history");
    for var in [
        "OPENAI_API_KEY",
        "AIDO_API_KEY",
        "AIDO_PROFILE",
        "AIDO_MODEL",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XDG_SESSION_TYPE",
    ] {
        cmd.env_remove(var);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let child = cmd.spawn().expect("spawn");
    // The slave fd is owned by the child's Stdio now; the master must stay
    // open so the child's tty does not hang up before it exits.
    let _master_guard = MasterFd(master);
    let out = child.wait_with_output().unwrap();
    let _ = std::fs::remove_file(&config);
    RunOutcome { output: out }
}

#[cfg(unix)]
struct MasterFd(i32);
#[cfg(unix)]
impl Drop for MasterFd {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

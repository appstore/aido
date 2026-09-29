//! Black-box tests for `aido ui`: the server runs as a subprocess with a
//! pinned token and isolated config/tasks/history, and every assertion
//! below speaks plain HTTP to it — reqwest is already a dependency, and
//! only loopback http is ever dialed, so the no-TLS client is fine.

#![cfg(feature = "ui")]

mod support;

use std::io::{BufRead as _, Read as _, Write as _};
use std::process::{Child, Command, Stdio};

use support::*;

const TOKEN: &str = "ui-test-token";

/// A plain loopback client. reqwest's rustls-no-provider build demands a
/// crypto provider at Client construction even for plain http, so the
/// ring provider (the one aido itself preconfigures in transport.rs) is
/// installed once per test binary.
fn client() -> reqwest::Client {
    use std::sync::Once;
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    reqwest::Client::new()
}

fn chat_cfg(url: &str) -> std::path::PathBuf {
    settings_config(&format!(
        "[settings]\nhistory_keep = 5\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{url}\""
    ))
}

struct UiServer {
    port: u16,
    child: Child,
}

impl UiServer {
    fn start(envs: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new(EXE);
        cmd.args(["ui", "--port", "0", "--no-open"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env("AIDO_UI_TOKEN", TOKEN)
            .env("AIDO_TASKS_DIR", "/nonexistent/aido-test-tasks");
        // Same discipline as the CLI harness: no developer's ambient
        // config or history leaks into a test; each test sets its own.
        for var in [
            "AIDO_PROFILE",
            "OPENAI_API_KEY",
            "AIDO_API_KEY",
            "AIDO_CONFIG",
            "AIDO_HISTORY_DIR",
        ] {
            cmd.env_remove(var);
        }
        for (key, value) in envs {
            cmd.env(key, value);
        }
        let mut child = cmd.spawn().expect("spawn aido ui");
        // The first stdout line names the port: "aido ui listening on
        // http://127.0.0.1:PORT/?t=...".
        let mut line = String::new();
        {
            let stdout = child.stdout.take().expect("piped stdout");
            let mut reader = std::io::BufReader::new(stdout);
            reader.read_line(&mut line).expect("read the banner");
        }
        let port = line
            .rsplit_once(':')
            .and_then(|(_, rest)| rest.split('/').next().map(str::to_string))
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| panic!("cannot parse the banner: {line}"));
        Self { port, child }
    }

    /// An authorized GET: the response is already resolved, so tests
    /// chain `.status()` / `.json().await` / `.text().await` on it.
    async fn get(&self, path: &str) -> reqwest::Response {
        client()
            .get(format!("http://127.0.0.1:{}{path}", self.port))
            .header("x-aido-token", TOKEN)
            .send()
            .await
            .expect("request reaches the server")
    }
}

impl Drop for UiServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn the_index_is_served_and_the_api_is_gated() {
    let server = UiServer::start(&[("AIDO_HISTORY_DIR", "/nonexistent/aido-test-history")]);

    // The SPA entry (the placeholder page in a cargo-only checkout) and
    // its client-side-route fallback both answer as html.
    let index = server.get("/").await;
    assert_eq!(index.status(), 200);
    assert!(index.text().await.unwrap().contains("aido"));
    let route = server.get("/history").await;
    assert_eq!(route.status(), 200);

    // Health carries the version with the token, 401 without it, 403
    // from a foreign Host or Origin.
    let health: serde_json::Value = server.get("/api/health").await.json().await.unwrap();
    assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(health["ok"], true);

    let no_token = client()
        .get(format!("http://127.0.0.1:{}/api/health", server.port))
        .send()
        .await
        .unwrap();
    assert_eq!(no_token.status(), 401);

    let rebinding = client()
        .get(format!("http://127.0.0.1:{}/api/health", server.port))
        .header("x-aido-token", TOKEN)
        .header("Host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(rebinding.status(), 403);

    let cross_site = client()
        .get(format!(
            "http://127.0.0.1:{}/api/health?t={TOKEN}",
            server.port
        ))
        .header("Origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(cross_site.status(), 403);
}

#[tokio::test]
async fn tasks_list_and_show_expose_the_builtins() {
    let server = UiServer::start(&[("AIDO_HISTORY_DIR", "/nonexistent/aido-test-history")]);

    let listed: serde_json::Value = server.get("/api/tasks").await.json().await.unwrap();
    let names: Vec<&str> = listed["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in [
        "ask",
        "code-review",
        "ocr",
        "summarize",
        "transcribe",
        "translate",
        "tts",
        "image",
    ] {
        assert!(names.contains(&expected), "missing builtin {expected}");
    }

    let ocr: serde_json::Value = server.get("/api/tasks/ocr").await.json().await.unwrap();
    assert_eq!(ocr["processor"], "ocr-tiles");
    assert_eq!(ocr["per_part"], true);
    assert_eq!(ocr["operation"], "generate");
    let required: Vec<&str> = ocr["required_types"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap())
        .collect();
    assert!(required.contains(&"image"));

    // Parameter descriptors carry the same bounds the plan enforces —
    // the form cannot offer what a run would reject.
    let tts: serde_json::Value = server.get("/api/tasks/tts").await.json().await.unwrap();
    let speed = tts["params"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "speed")
        .unwrap();
    assert_eq!(speed["kind"], "number");
    assert_eq!(speed["min"], 0.25);
    assert_eq!(speed["max"], 4.0);
    let image: serde_json::Value = server.get("/api/tasks/image").await.json().await.unwrap();
    let size = image["params"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "size")
        .unwrap();
    assert_eq!(size["kind"], "enum");
    assert!(size["choices"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c == "1024x1024"));

    let unknown = server.get("/api/tasks/nope").await;
    assert_eq!(unknown.status(), 404);
}

/// POST /api/tasks must validate with the loader's own rules, write the
/// file atomically, and — the whole point — drop the task cache so the
/// definition answers the next request without a server restart. The
/// preview at the end proves the reload reaches plan building, not just
/// the list endpoint.
#[tokio::test]
async fn tasks_are_created_reloaded_overwritten_and_deleted() {
    let tasks = temp_dir("ui-tasks");
    let dir = temp_dir("ui-tasks-run");
    let cfg = chat_cfg("http://127.0.0.1:1");
    let server = UiServer::start(&[
        ("AIDO_CONFIG", cfg.to_str().unwrap()),
        ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
        ("AIDO_TASKS_DIR", tasks.to_str().unwrap()),
    ]);

    let post = |body: serde_json::Value| {
        client()
            .post(format!("http://127.0.0.1:{}/api/tasks", server.port))
            .header("x-aido-token", TOKEN)
            .json(&body)
    };

    let toml = "operation = 'generate'\noutput_types = ['text']\ninstruction = 'say hi politely'\n";
    let created = post(serde_json::json!({ "name": "hello-ui", "toml": toml }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);
    let answer: serde_json::Value = created.json().await.unwrap();
    assert_eq!(answer["task"]["name"], "hello-ui");
    assert_eq!(answer["task"]["summary"], "say hi politely");
    assert!(
        answer["path"].as_str().unwrap().ends_with("hello-ui.toml"),
        "{answer}"
    );

    // Same process, no restart: the list and the plan both see it.
    let listed: serde_json::Value = server.get("/api/tasks").await.json().await.unwrap();
    let names: Vec<&str> = listed["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"hello-ui"), "{names:?}");

    let mut form = reqwest::multipart::Form::new().text(
        "request",
        serde_json::json!({ "task": "hello-ui", "profile": "test" }).to_string(),
    );
    form = form.part(
        "file",
        reqwest::multipart::Part::bytes(b"material".to_vec()).file_name("note.txt"),
    );
    let preview = client()
        .post(format!("http://127.0.0.1:{}/api/runs/preview", server.port))
        .header("x-aido-token", TOKEN)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(preview.status(), 200);
    let planned: serde_json::Value = preview.json().await.unwrap();
    assert_eq!(planned["task"], "hello-ui");

    // A second save without overwrite is a conflict; with it, a replace.
    let again = post(serde_json::json!({ "name": "hello-ui", "toml": toml }))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 409);
    let replaced = post(serde_json::json!({
        "name": "hello-ui",
        "toml": toml,
        "overwrite": true,
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(replaced.status(), 201);

    // The loader's own refusals arrive as 400s with the load-time
    // message, and so do unusable file names.
    let invalid = post(serde_json::json!({
        "name": "broken-ui",
        "toml": "operation = 'generate'\noutput_types = []\n",
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(invalid.status(), 400);
    let message: String = invalid.json::<serde_json::Value>().await.unwrap()["error"]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(message.contains("output_types"), "{message}");
    let traversal = post(serde_json::json!({
        "name": "../escape",
        "toml": toml,
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(traversal.status(), 400);

    // The source endpoint answers the custom file first, the embedded
    // bytes for a bare built-in.
    let source: serde_json::Value = server
        .get("/api/tasks/hello-ui/source")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(source["builtin"], false);
    assert_eq!(source["toml"], toml);
    let builtin: serde_json::Value = server
        .get("/api/tasks/ask/source")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(builtin["builtin"], true);
    assert!(builtin["toml"].as_str().unwrap().contains("generate"));

    // Delete removes the file (and only the file's task); a bare
    // built-in has nothing on disk and refuses.
    let removed = client()
        .delete(format!(
            "http://127.0.0.1:{}/api/tasks/hello-ui",
            server.port
        ))
        .header("x-aido-token", TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), 204);
    let gone = server.get("/api/tasks/hello-ui").await;
    assert_eq!(gone.status(), 404);
    let builtin_delete = client()
        .delete(format!("http://127.0.0.1:{}/api/tasks/ask", server.port))
        .header("x-aido-token", TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(builtin_delete.status(), 400);
}

#[tokio::test]
async fn runs_list_detail_and_artifact_bytes_read_a_cli_run() {
    // One ordinary CLI run lands in history; the UI must see exactly
    // what `history list` / `history show` would show — same numbering,
    // same record, same bytes.
    let provider = Server::json(chat_body("SAVED"));
    let dir = temp_dir("ui-read");
    let cfg = chat_cfg(&provider.url());
    let out = run_with(
        &["summarize", "--profile", "test"],
        b"hi\n",
        &[("AIDO_HISTORY_DIR", dir.to_str().unwrap())],
        cfg.to_str().unwrap(),
    );
    out.assert_code(0);
    provider.request();

    let server = UiServer::start(&[
        ("AIDO_CONFIG", cfg.to_str().unwrap()),
        ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
    ]);

    let listed: serde_json::Value = server.get("/api/runs").await.json().await.unwrap();
    let runs = listed["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["seq"], 1);
    assert_eq!(runs[0]["task"], "summarize");
    assert_eq!(runs[0]["status"], "complete");
    assert_eq!(runs[0]["artifacts"], 1);
    let run_id = runs[0]["run_id"].as_str().unwrap().to_string();

    // The detail answers by seq (the CLI's own operand) and by id, with
    // the envelope's field names and no artifact bytes inline.
    for target in ["1", &run_id] {
        let detail: serde_json::Value = server
            .get(&format!("/api/runs/{target}"))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(detail["version"], 1);
        assert_eq!(detail["run_id"], run_id);
        assert_eq!(detail["task"], "summarize");
        assert_eq!(detail["artifacts"][0]["id"], "text");
        assert_eq!(detail["artifacts"][0]["size"], 5);
        assert_eq!(detail["artifacts"][0]["mime"], "text/plain");
        assert!(detail["artifacts"][0].get("bytes").is_none());
    }

    // Artifact bytes come from their own endpoint, typed by mime.
    let bytes = server.get("/api/runs/1/artifacts/text").await;
    assert_eq!(bytes.status(), 200);
    assert_eq!(
        bytes
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "text/plain"
    );
    assert_eq!(bytes.text().await.unwrap(), "SAVED");

    let missing = server.get("/api/runs/1/artifacts/nope").await;
    assert_eq!(missing.status(), 404);
    let unknown_run = server.get("/api/runs/9").await;
    assert_eq!(unknown_run.status(), 404);

    // Filters: a task nothing ran keeps the list honest and empty.
    let filtered: serde_json::Value = server.get("/api/runs?task=ocr").await.json().await.unwrap();
    assert_eq!(filtered["runs"].as_array().unwrap().len(), 0);
}

/// A provider that holds its single reply back for `delay` — the run
/// stays in flight long enough for a test to subscribe to its event
/// stream or cancel it deterministically.
fn slow_provider(
    delay: std::time::Duration,
    body: &'static str,
) -> (u16, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    // Nonblocking, or support::accept's 30s deadline is dead code and a
    // cancel-before-connect race hangs the suite forever.
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut stream = accept(&listener, deadline);
        stream.set_nonblocking(false).unwrap();
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
        std::thread::sleep(delay);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        // A cancelled run hangs up first; that is the point.
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    });
    (port, handle)
}

/// The SSE frames of one finished run, decoded: each `data:` line is one
/// JSON event. Reading with `.text()` works because the stream ends when
/// the run does.
async fn sse_frames(server: &UiServer, run_id: &str) -> Vec<serde_json::Value> {
    let body = server
        .get(&format!("/api/runs/{run_id}/events"))
        .await
        .text()
        .await
        .unwrap();
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).expect("frames are JSON"))
        .collect()
}

async fn post_run(
    server: &UiServer,
    request: &serde_json::Value,
    files: &[(&str, &[u8])],
) -> reqwest::Response {
    let mut form = reqwest::multipart::Form::new().text("request", request.to_string());
    for (name, bytes) in files {
        form = form.part(
            "file",
            reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(name.to_string()),
        );
    }
    client()
        .post(format!("http://127.0.0.1:{}/api/runs", server.port))
        .header("x-aido-token", TOKEN)
        .multipart(form)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_posted_run_streams_deltas_and_lands_in_history() {
    let (port, provider) = slow_provider(
        std::time::Duration::from_millis(300),
        chat_body("FROM THE UI"),
    );
    let dir = temp_dir("ui-run");
    let cfg = chat_cfg(&format!("http://127.0.0.1:{port}"));
    let server = UiServer::start(&[
        ("AIDO_CONFIG", cfg.to_str().unwrap()),
        ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
    ]);

    let request = serde_json::json!({ "task": "summarize", "profile": "test" });
    let posted = post_run(&server, &request, &[("note.txt", b"hello from the ui")]).await;
    assert_eq!(posted.status(), 202);
    let accepted: serde_json::Value = posted.json().await.unwrap();
    let run_id = accepted["run_id"].as_str().unwrap().to_string();
    assert!(!run_id.is_empty());

    // The stream shows the delta (the merged text, exactly what a
    // terminal would print) and closes with a done frame carrying the
    // report. The step event fired before this subscriber joined —
    // broadcast has no replay; the detail endpoint is the catch-up.
    let frames = sse_frames(&server, &run_id).await;
    let kinds: Vec<&str> = frames.iter().map(|f| f["type"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"delta"), "frames: {kinds:?}");
    assert_eq!(*kinds.last().unwrap(), "done");
    let delta_text: String = frames
        .iter()
        .filter(|f| f["type"] == "delta")
        .map(|f| f["text"].as_str().unwrap())
        .collect();
    assert_eq!(delta_text, "FROM THE UI");
    let done = frames.last().unwrap();
    assert_eq!(done["run_id"], run_id);
    assert_eq!(done["task"], "summarize");
    assert_eq!(done["artifacts"][0]["id"], "text");
    assert!(done.get("error").is_none());

    // The record exists for the CLI too, and the artifact bytes match.
    let detail: serde_json::Value = server
        .get(&format!("/api/runs/{run_id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(detail["status"]["status"], "complete");
    let bytes = server
        .get(&format!("/api/runs/{run_id}/artifacts/text"))
        .await
        .text()
        .await
        .unwrap();
    assert_eq!(bytes, "FROM THE UI");
    provider.join().unwrap();
}

#[tokio::test]
async fn cancelling_a_live_run_records_it_as_cancelled() {
    // Four seconds of hang: plenty for the cancel to land early, little
    // enough for the provider thread's join not to slow the suite.
    let (port, provider) = slow_provider(std::time::Duration::from_secs(4), chat_body("too late"));
    let dir = temp_dir("ui-cancel");
    let cfg = chat_cfg(&format!("http://127.0.0.1:{port}"));
    let server = UiServer::start(&[
        ("AIDO_CONFIG", cfg.to_str().unwrap()),
        ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
    ]);

    let request = serde_json::json!({ "task": "summarize", "profile": "test" });
    let posted = post_run(&server, &request, &[("note.txt", b"cancel me")]).await;
    let accepted: serde_json::Value = posted.json().await.unwrap();
    let run_id = accepted["run_id"].as_str().unwrap().to_string();

    // Subscribe while the request hangs, then cancel: the closing frame
    // says cancelled and history keeps a cancelled record, exactly as a
    // Ctrl+C on the terminal would leave. The stream's headers must be
    // awaited first — that is the subscription itself.
    let url = format!("/api/runs/{run_id}/events");
    let stream = server.get(&url).await;
    let cancelled = client()
        .post(format!(
            "http://127.0.0.1:{}/api/runs/{run_id}/cancel",
            server.port
        ))
        .header("x-aido-token", TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(cancelled.status(), 204);

    let body = stream.text().await.unwrap();
    assert!(body.contains("\"type\":\"cancelled\""), "stream: {body}");
    let detail: serde_json::Value = server
        .get(&format!("/api/runs/{run_id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(detail["status"]["status"], "cancelled");

    // A second cancel finds nothing to ask.
    let again = client()
        .post(format!(
            "http://127.0.0.1:{}/api/runs/{run_id}/cancel",
            server.port
        ))
        .header("x-aido-token", TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 404);
    provider.join().unwrap();
}

#[tokio::test]
async fn preview_describes_the_plan_without_running_it() {
    let (port, provider) = slow_provider(std::time::Duration::from_secs(1), chat_body("unused"));
    let cfg = chat_cfg(&format!("http://127.0.0.1:{port}"));
    let server = UiServer::start(&[
        ("AIDO_CONFIG", cfg.to_str().unwrap()),
        ("AIDO_HISTORY_DIR", "/nonexistent/aido-test-history"),
    ]);

    let request = serde_json::json!({ "task": "summarize", "profile": "test" });
    let form_request = request.clone();
    let mut form = reqwest::multipart::Form::new().text("request", form_request.to_string());
    form = form.part(
        "file",
        reqwest::multipart::Part::bytes(b"previewed material".to_vec()).file_name("note.txt"),
    );
    let preview: serde_json::Value = client()
        .post(format!("http://127.0.0.1:{}/api/runs/preview", server.port))
        .header("x-aido-token", TOKEN)
        .multipart(form)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(preview["task"], "summarize");
    assert_eq!(preview["model"], "m");
    assert_eq!(preview["steps"].as_array().unwrap().len(), 1);
    assert!(preview["text"].as_str().unwrap().contains("summarize"));

    // A bad combination answers with the CLI's own usage message, not a
    // five-hundred.
    let bad = serde_json::json!({ "task": "transcribe", "profile": "test" });
    let refused = post_run(&server, &bad, &[("note.txt", b"not audio")]).await;
    assert_eq!(refused.status(), 400);
    let refused: serde_json::Value = refused.json().await.unwrap();
    assert!(refused["error"]["message"]
        .as_str()
        .unwrap()
        .contains("audio"));
    drop(provider);
}

#[tokio::test]
async fn unknown_request_fields_are_refused() {
    let server = UiServer::start(&[("AIDO_HISTORY_DIR", "/nonexistent/aido-test-history")]);
    let request = serde_json::json!({ "task": "summarize", "output": "/tmp/x" });
    let posted = post_run(&server, &request, &[]).await;
    assert_eq!(posted.status(), 400);
}

/// Server-side delivery is a whitelist, not an open path: `out_dir`
/// names a component under aido's deliveries directory, and anything
/// that could escape it answers with the CLI's own usage voice. The
/// happy path then lands real files in that root only.
#[tokio::test]
async fn whitelisted_delivery_lands_under_the_deliveries_root() {
    // Two runs, two replies, one port: the second run must talk to the
    // same provider the server's config names.
    let provider = MultiServer::start(&[chat_body("DELIVERED"), chat_body("FILE BODY")]);
    let dir = temp_dir("ui-deliver");
    let root = temp_dir("ui-deliveries");
    let cfg = chat_cfg(&provider.url());
    let server = UiServer::start(&[
        ("AIDO_CONFIG", cfg.to_str().unwrap()),
        ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
        ("AIDO_DELIVERY_DIR", root.to_str().unwrap()),
    ]);

    // A traversal-shaped name never becomes a path.
    let sneaky = serde_json::json!({ "task": "summarize", "out_dir": "../escape" });
    let refused = post_run(&server, &sneaky, &[("note.txt", b"hi")]).await;
    assert_eq!(refused.status(), 400);
    let message: String = refused.json::<serde_json::Value>().await.unwrap()["error"]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(message.contains("deliveries"), "{message}");

    // The real thing: a run with out_dir lands the artifact set plus
    // its manifest inside the root, and the record carries the
    // per-destination state.
    let request = serde_json::json!({ "task": "summarize", "profile": "test", "out_dir": "job-1" });
    let posted = post_run(&server, &request, &[("note.txt", b"hello")]).await;
    assert_eq!(posted.status(), 202);
    let run_id: String = posted.json::<serde_json::Value>().await.unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let detail = poll_until(&server, &run_id, |d| {
        !d["deliveries"].as_array().unwrap_or(&vec![]).is_empty()
    })
    .await;
    let deliveries = detail["deliveries"].as_array().unwrap();
    assert_eq!(deliveries.len(), 1, "{detail}");
    assert_eq!(deliveries[0]["status"], "succeeded", "{detail}");
    assert_eq!(deliveries[0]["destination"]["type"], "directory");
    let delivered_path = deliveries[0]["destination"]["path"].as_str().unwrap();
    assert!(delivered_path.ends_with("job-1"), "{delivered_path}");
    assert!(
        std::path::Path::new(delivered_path).starts_with(&root),
        "{delivered_path} vs {}",
        root.display()
    );
    let job = root.join("job-1");
    assert!(job.join("manifest.json").is_file());
    assert_eq!(
        std::fs::read_to_string(job.join("text.txt")).unwrap(),
        "DELIVERED"
    );

    // `-o` as a file name: exactly one artifact, written under the same
    // root; a second run at the same name is the CLI's knowable
    // collision (usage, before any request).
    let file_request =
        serde_json::json!({ "task": "summarize", "profile": "test", "out_file": "report.txt" });
    let posted = post_run(&server, &file_request, &[("note.txt", b"hi")]).await;
    assert_eq!(posted.status(), 202);
    let run_id: String = posted.json::<serde_json::Value>().await.unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let detail = poll_until(&server, &run_id, |d| {
        !d["deliveries"].as_array().unwrap_or(&vec![]).is_empty()
    })
    .await;
    assert_eq!(detail["deliveries"][0]["status"], "succeeded", "{detail}");
    assert_eq!(detail["deliveries"][0]["destination"]["type"], "file");
    assert_eq!(
        std::fs::read_to_string(root.join("report.txt")).unwrap(),
        "FILE BODY"
    );

    let collision = post_run(&server, &file_request, &[("note.txt", b"again")]).await;
    assert_eq!(collision.status(), 400);
    let message: String = collision.json::<serde_json::Value>().await.unwrap()["error"]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(message.contains("already exists"), "{message}");
}

/// Poll the detail endpoint until the predicate holds (a finishing run
/// records, delivers and updates its manifest in that order); returns
/// the first detail that satisfies it.
async fn poll_until(
    server: &UiServer,
    run_id: &str,
    ready: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    for _ in 0..100 {
        let detail: serde_json::Value = server
            .get(&format!("/api/runs/{run_id}"))
            .await
            .json()
            .await
            .unwrap();
        if ready(&detail) {
            return detail;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the run never reached the expected state");
}

#[tokio::test]
async fn config_reads_writes_and_checks_round_trip() {
    let dir = temp_dir("ui-config");
    let cfg_path = dir.join("config.toml");
    std::fs::write(
        &cfg_path,
        "default_profile = \"test\"\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"YOUR_MODEL\"\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:9\"\n",
    )
    .unwrap();
    let server = UiServer::start(&[("AIDO_CONFIG", cfg_path.to_str().unwrap())]);

    // The view: the file's own bytes, the parsed shape, and check's
    // verdict on the placeholder model.
    let view: serde_json::Value = server.get("/api/config").await.json().await.unwrap();
    assert_eq!(view["path"], cfg_path.to_str().unwrap());
    assert_eq!(view["exists"], true);
    assert!(view["raw"].as_str().unwrap().contains("default_profile"));
    assert_eq!(view["effective"]["default_profile"], "test");
    let profiles = view["effective"]["profiles"].as_array().unwrap();
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0]["name"], "test");
    assert_eq!(profiles[0]["is_default"], true);
    assert_eq!(profiles[0]["provider"], "srv");
    assert!(view["issues"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i.as_str().unwrap().contains("YOUR_MODEL")));

    // The datalist behind the run form's --profile.
    let listed: serde_json::Value = server.get("/api/profiles").await.json().await.unwrap();
    assert_eq!(listed["default_profile"], "test");
    assert_eq!(listed["profiles"][0]["name"], "test");
    assert_eq!(listed["profiles"][0]["is_default"], true);

    // Save: parse-first (invalid TOML answers 400 and touches nothing),
    // atomic write, check's verdict on exactly what was saved.
    let good = "default_profile = \"test\"\n\
                [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
                [providers.srv]\nbase_url = \"http://127.0.0.1:9\"\n";
    let saved: serde_json::Value = client()
        .put(format!("http://127.0.0.1:{}/api/config", server.port))
        .header("x-aido-token", TOKEN)
        .json(&serde_json::json!({ "toml": good }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(saved["ok"], true);
    assert_eq!(saved["issues"].as_array().unwrap().len(), 0);
    assert_eq!(std::fs::read_to_string(&cfg_path).unwrap(), good);

    let refused = client()
        .put(format!("http://127.0.0.1:{}/api/config", server.port))
        .header("x-aido-token", TOKEN)
        .json(&serde_json::json!({ "toml": "not [ toml" }))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 400);
    assert_eq!(std::fs::read_to_string(&cfg_path).unwrap(), good);

    // An honestly incomplete config saves — with its issues in the
    // answer, next to the editor's save button.
    let placeholder = good.replace("model = \"m\"", "model = \"YOUR_MODEL\"");
    let saved: serde_json::Value = client()
        .put(format!("http://127.0.0.1:{}/api/config", server.port))
        .header("x-aido-token", TOKEN)
        .json(&serde_json::json!({ "toml": placeholder }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(saved["ok"], true);
    assert!(!saved["issues"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn a_config_that_cannot_load_still_opens_the_editor() {
    // An AIDO_CONFIG pointing at a missing file is exactly what the
    // editor should show, not a five-hundred: the view answers with the
    // error and an empty file. The path's parent must be creatable for
    // the save half.
    let dir = temp_dir("ui-config-missing");
    let cfg_path = dir.join("missing").join("config.toml");
    let server = UiServer::start(&[("AIDO_CONFIG", cfg_path.to_str().unwrap())]);
    let view: serde_json::Value = server.get("/api/config").await.json().await.unwrap();
    assert_eq!(view["exists"], false);
    assert_eq!(view["raw"], "");
    assert!(view["load_error"]
        .as_str()
        .unwrap()
        .contains("missing file"));
    // PUT then creates the file and the editor leaves the error state.
    let saved: serde_json::Value = client()
        .put(format!("http://127.0.0.1:{}/api/config", server.port))
        .header("x-aido-token", TOKEN)
        .json(&serde_json::json!({ "toml": "[profiles.p]\nprovider = \"x\"\nmodel = \"m\"\n" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(saved["ok"], true);
    assert!(cfg_path.exists());
}

/// A provider holding back each of its replies — a chain of N stages
/// stays in flight long enough for a test to subscribe deterministically.
fn slow_chain_provider(
    delay: std::time::Duration,
    bodies: Vec<&'static str>,
) -> (u16, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    // Nonblocking, or support::accept's 30s deadline is dead code and a
    // cancel-before-connect race hangs the suite forever.
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        for body in bodies {
            let mut stream = accept(&listener, deadline);
            stream.set_nonblocking(false).unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            std::thread::sleep(delay);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (port, handle)
}

async fn post_chain(
    server: &UiServer,
    body: serde_json::Value,
    files: &[(&str, &[u8])],
) -> reqwest::Response {
    let mut form = reqwest::multipart::Form::new().text("request", body.to_string());
    for (name, bytes) in files {
        form = form.part(
            "file",
            reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(name.to_string()),
        );
    }
    client()
        .post(format!("http://127.0.0.1:{}/api/chain", server.port))
        .header("x-aido-token", TOKEN)
        .multipart(form)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_posted_chain_runs_stage_by_stage_and_records_one_run() {
    let (port, provider) = slow_chain_provider(
        std::time::Duration::from_millis(250),
        vec![chat_body("第一阶段的输出"), chat_body("最终交付的译文")],
    );
    let dir = temp_dir("ui-chain");
    let cfg = chat_cfg(&format!("http://127.0.0.1:{port}"));
    let server = UiServer::start(&[
        ("AIDO_CONFIG", cfg.to_str().unwrap()),
        ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
    ]);

    let request = serde_json::json!({
        "stages": [
            { "task": "ask", "prompt": "把材料原样返回", "profile": "test" },
            { "task": "summarize" , "profile": "test" },
        ],
        "texts": ["链的材料"],
    });
    let posted = post_chain(&server, request, &[]).await;
    assert_eq!(posted.status(), 202);
    let run_id = posted.json::<serde_json::Value>().await.unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();

    let frames = sse_frames(&server, &run_id).await;
    let done = frames
        .iter()
        .find(|f| f["type"] == "done")
        .expect("the chain closes with a done frame");
    // The label and the record shape are the CLI's own: one run, two
    // stages, the trailing artifact is the deliverable.
    assert_eq!(done["task"], "ask|summarize");
    assert_eq!(done["stages"].as_array().unwrap().len(), 2);
    assert_eq!(done["last_stage_len"], 1);
    assert_eq!(done["artifacts"].as_array().unwrap().len(), 2);
    assert!(done["artifacts"][0]["id"]
        .as_str()
        .unwrap()
        .starts_with("stage-1-"));
    assert_eq!(done["artifacts"][1]["id"], "text");
    assert_eq!(done["artifacts"][1]["size"], "最终交付的译文".len());

    // One record for the whole chain; the detail answers with the same
    // stages and the deliverable split.
    let detail: serde_json::Value = server
        .get(&format!("/api/runs/{run_id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(detail["task"], "ask|summarize");
    assert_eq!(detail["stages"].as_array().unwrap().len(), 2);
    assert_eq!(detail["artifacts"].as_array().unwrap().len(), 2);
    let listed: serde_json::Value = server.get("/api/runs").await.json().await.unwrap();
    assert_eq!(listed["runs"].as_array().unwrap().len(), 1);
    provider.join().unwrap();
}

#[tokio::test]
async fn chain_preview_describes_and_junction_mismatches_are_usage_errors() {
    let (port, provider) = slow_chain_provider(
        std::time::Duration::from_secs(1),
        vec![chat_body("x"), chat_body("y")],
    );
    let cfg = chat_cfg(&format!("http://127.0.0.1:{port}"));
    let server = UiServer::start(&[
        ("AIDO_CONFIG", cfg.to_str().unwrap()),
        ("AIDO_HISTORY_DIR", "/nonexistent/aido-test-history"),
    ]);

    // A working chain previews per-stage blocks and makes no request.
    let good = serde_json::json!({
        "stages": [
            { "task": "summarize", "profile": "test" },
            { "task": "translate" , "profile": "test" },
        ],
        "texts": ["preview me"],
    });
    let mut form = reqwest::multipart::Form::new().text("request", good.to_string());
    let preview: serde_json::Value = client()
        .post(format!(
            "http://127.0.0.1:{}/api/chain/preview",
            server.port
        ))
        .header("x-aido-token", TOKEN)
        .multipart(form)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(preview["text"]
        .as_str()
        .unwrap()
        .contains("chain: summarize|translate"));
    assert_eq!(preview["label"], "summarize|translate");
    assert_eq!(preview["stages"].as_array().unwrap().len(), 2);

    // The junction contract fires at plan time, with the stage pair
    // named: transcribe needs audio, translate produces text only.
    let bad = serde_json::json!({
        "stages": [
            { "task": "translate", "profile": "test" },
            { "task": "transcribe" , "profile": "test" },
        ],
        "texts": ["no audio here"],
    });
    form = reqwest::multipart::Form::new().text("request", bad.to_string());
    let refused = client()
        .post(format!(
            "http://127.0.0.1:{}/api/chain/preview",
            server.port
        ))
        .header("x-aido-token", TOKEN)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 400);
    let refused: serde_json::Value = refused.json().await.unwrap();
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("stage 1 (translate) → stage 2 (transcribe)"),
        "{message}"
    );

    // A one-stage "chain" is refused up front.
    let single = serde_json::json!({ "stages": [{ "task": "summarize" }] });
    let posted = post_chain(&server, single, &[]).await;
    assert_eq!(posted.status(), 400);
    drop(provider);
}

#[tokio::test]
async fn cancelling_a_chain_keeps_the_paid_for_stages() {
    // One held reply: the cancel lands during stage 1, so stage 2's
    // request never exists — and the provider must not sit in accept()
    // waiting for it (a blocking listener's accept ignores deadlines).
    let (port, provider) =
        slow_chain_provider(std::time::Duration::from_secs(4), vec![chat_body("held")]);
    let dir = temp_dir("ui-chain-cancel");
    let cfg = chat_cfg(&format!("http://127.0.0.1:{port}"));
    let server = UiServer::start(&[
        ("AIDO_CONFIG", cfg.to_str().unwrap()),
        ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
    ]);

    let request = serde_json::json!({
        "stages": [
            { "task": "ask", "prompt": "第一阶段", "profile": "test" },
            { "task": "summarize", "profile": "test" },
        ],
        "texts": ["cancel mid-chain"],
    });
    let posted = post_chain(&server, request, &[]).await;
    let run_id = posted.json::<serde_json::Value>().await.unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Subscribe first (the headers ARE the subscription), cancel during
    // stage 1's held reply: the cancelled record keeps nothing (stage 1
    // never finished), and a second cancel is a harmless 204.
    let url = format!("/api/runs/{run_id}/events");
    let stream = server.get(&url).await;
    let first = client()
        .post(format!(
            "http://127.0.0.1:{}/api/runs/{run_id}/cancel",
            server.port
        ))
        .header("x-aido-token", TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 204);
    // The second ask may land while the run is still tearing down (204,
    // idempotent) or after the entry is gone (404) — both mean the stop
    // registered; the stream below says which way the run ended.
    let second = client()
        .post(format!(
            "http://127.0.0.1:{}/api/runs/{run_id}/cancel",
            server.port
        ))
        .header("x-aido-token", TOKEN)
        .send()
        .await
        .unwrap();
    assert!(second.status() == 204 || second.status() == 404);

    let body = stream.text().await.unwrap();
    assert!(body.contains("\"type\":\"cancelled\""), "stream: {body}");
    let detail: serde_json::Value = server
        .get(&format!("/api/runs/{run_id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(detail["status"]["status"], "cancelled");
    assert!(detail["warnings"][0]
        .as_str()
        .unwrap()
        .contains("cancelled from the web UI"));
    provider.join().unwrap();
}

#[tokio::test]
async fn the_sse_query_token_path_the_spa_uses_is_authorized() {
    // The SPA's EventSource cannot send headers — the query string is
    // its only token channel, and no other test exercises it (every
    // helper goes through the header). This is exactly its shape.
    let (port, provider) = slow_provider(
        std::time::Duration::from_millis(250),
        chat_body("via query token"),
    );
    let dir = temp_dir("ui-sse-query");
    let cfg = chat_cfg(&format!("http://127.0.0.1:{port}"));
    let server = UiServer::start(&[
        ("AIDO_CONFIG", cfg.to_str().unwrap()),
        ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
    ]);
    let request = serde_json::json!({ "task": "summarize", "profile": "test" });
    let posted = post_run(&server, &request, &[("note.txt", b"token by query")]).await;
    let run_id = posted.json::<serde_json::Value>().await.unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();

    // No header on this request — only ?t=.
    let body = client()
        .get(format!(
            "http://127.0.0.1:{}/api/runs/{run_id}/events?t={TOKEN}",
            server.port
        ))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("\"type\":\"delta\""), "stream: {body}");
    assert!(body.contains("via query token"));
    assert!(body.contains("\"type\":\"done\""));
    provider.join().unwrap();
}

#[tokio::test]
async fn a_cli_partial_batch_reads_back_as_partial_through_the_ui() {
    // The UI cannot run a per-part batch yet (U04: --out-dir is not a
    // UI field), but the CLI's exit-6 runs land in the same history —
    // the list must call them "partial" and the detail must carry the
    // failed-parts list it promises.
    let provider = MultiServer::start_statuses(&[
        ("200 OK", chat_body("SURVIVOR")),
        ("500 Internal Server Error", "{\"error\":\"boom\"}"),
    ]);
    let dir = temp_dir("ui-partial");
    let cfg = chat_cfg(&provider.url());
    let a = temp_file("a.png", &solid_png(8, 8));
    let b = temp_file("b.png", &solid_png(8, 8));
    let out = dir.join("out");
    let outcome = run_with(
        &[
            "ocr",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--profile",
            "test",
            "--out-dir",
            out.to_str().unwrap(),
        ],
        b"",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
        ],
        cfg.to_str().unwrap(),
    );
    assert_eq!(outcome.code(), 6);
    provider.requests();

    let server = UiServer::start(&[
        ("AIDO_CONFIG", cfg.to_str().unwrap()),
        ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
    ]);
    let listed: serde_json::Value = server.get("/api/runs").await.json().await.unwrap();
    let row = &listed["runs"][0];
    assert_eq!(row["status"], "partial");
    assert_eq!(row["failed_parts"], 1);
    assert_eq!(row["artifacts"], 1);
    let detail: serde_json::Value = server
        .get(&format!("/api/runs/{}", row["run_id"].as_str().unwrap()))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        detail["failed_parts"][0]["part"].as_str().unwrap(),
        b.file_name().unwrap().to_str().unwrap()
    );
    assert!(detail["failed_parts"][0]["error"]
        .as_str()
        .unwrap()
        .contains("500"));
}

#[tokio::test]
async fn a_ui_batch_run_reports_its_parts_live() {
    // U04 relaxed: two images through per-part ocr work from the UI
    // (the placeholder out-dir satisfies the precheck; nothing is ever
    // written there — history is the destination). The step frames must
    // name the file they belong to, and the done frame carries both
    // artifacts.
    let (port, provider) = slow_chain_provider(
        std::time::Duration::from_millis(250),
        vec![chat_body("FIRST PAGE"), chat_body("SECOND PAGE")],
    );
    let dir = temp_dir("ui-batch");
    let cfg = chat_cfg(&format!("http://127.0.0.1:{port}"));
    let server = UiServer::start(&[
        ("AIDO_CONFIG", cfg.to_str().unwrap()),
        ("AIDO_HISTORY_DIR", dir.to_str().unwrap()),
    ]);

    let request = serde_json::json!({ "task": "ocr", "profile": "test" });
    let posted = post_run(
        &server,
        &request,
        &[
            ("a.png", solid_png(8, 8).as_slice()),
            ("b.png", solid_png(8, 8).as_slice()),
        ],
    )
    .await;
    assert_eq!(posted.status(), 202);
    let run_id = posted.json::<serde_json::Value>().await.unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();

    let frames = sse_frames(&server, &run_id).await;
    // The first file's step fired before this subscription existed
    // (broadcast has no replay — the standing contract); the second
    // proves the part names ride the step frames.
    let parts: Vec<&str> = frames
        .iter()
        .filter(|f| f["type"] == "step")
        .filter_map(|f| f["part"].as_str())
        .collect();
    assert!(parts.contains(&"b.png"), "step parts: {parts:?}");
    let done = frames.iter().find(|f| f["type"] == "done").unwrap();
    let ids: Vec<&str> = done["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["a", "b"]);
    assert!(done.get("error").is_none());
    provider.join().unwrap();
}

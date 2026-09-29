//! Black-box tests for `aido ui`: the server runs as a subprocess with a
//! pinned token and isolated config/tasks/history, and every assertion
//! below speaks plain HTTP to it — reqwest is already a dependency, and
//! only loopback http is ever dialed, so the no-TLS client is fine.

#![cfg(feature = "ui")]

mod support;

use std::io::BufRead as _;
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
        for var in ["AIDO_PROFILE", "OPENAI_API_KEY", "AIDO_API_KEY"] {
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

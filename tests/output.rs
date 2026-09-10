//! Delivery: destinations, atomic files, directories with manifests, the
//! JSON report, and the exit-code contract for delivery failures.

mod support;

use support::*;

fn chat_cfg(url: &str) -> std::path::PathBuf {
    settings_config(&format!(
        "[settings]\nhistory_keep = 0\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{url}\""
    ))
}

#[test]
fn stdout_gets_exactly_one_trailing_newline() {
    let server = Server::json(chat_body("no newline at end"));
    let cfg = chat_cfg(&server.url());
    let out = run(
        &["summarize", "--profile", "test"],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "no newline at end\n");

    let server = Server::json(chat_body("ends with newline\n"));
    let cfg = chat_cfg(&server.url());
    let out = run(
        &["summarize", "--profile", "test"],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "ends with newline\n");
}

#[test]
fn output_file_takes_the_body_instead_of_stdout() {
    let server = Server::json(chat_body("TO FILE"));
    let dir = temp_dir("out-file");
    let file = dir.join("summary.md");
    let cfg = chat_cfg(&server.url());
    let out = run(
        &[
            "summarize",
            "--profile",
            "test",
            "-o",
            file.to_str().unwrap(),
        ],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(out.stdout(), "", "explicit destination replaces stdout");
    assert_eq!(std::fs::read(&file).unwrap(), b"TO FILE");
    assert!(out.stderr().contains("saved"), "stderr: {}", out.stderr());
}

#[test]
fn existing_output_file_refuses_without_overwrite() {
    let server = Server::json(chat_body("new"));
    let dir = temp_dir("out-exists");
    let file = dir.join("summary.txt");
    std::fs::write(&file, "original").unwrap();
    let cfg = chat_cfg(&server.url());
    let out = run(
        &[
            "summarize",
            "--profile",
            "test",
            "-o",
            file.to_str().unwrap(),
        ],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(5);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "original",
        "the old content survives"
    );

    // --overwrite replaces it atomically
    let server = Server::json(chat_body("new"));
    let cfg = chat_cfg(&server.url());
    let out = run(
        &[
            "summarize",
            "--profile",
            "test",
            "-o",
            file.to_str().unwrap(),
            "--overwrite",
        ],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "new");
}

#[test]
fn media_extension_mismatch_fails_before_any_request() {
    let dir = temp_dir("out-ext");
    let file = dir.join("picture.png");
    let cfg = settings_config(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\noperations = [\"image\"]\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"",
    );
    let out = run_tty_with(
        &[
            "image",
            "--profile",
            "test",
            "--text",
            "dog",
            "--format",
            "jpeg",
            "-o",
            file.to_str().unwrap(),
        ],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(2);
    let err = out.stderr();
    assert!(err.contains("jpeg"), "{err}");
    assert!(err.contains(".png"), "{err}");
}

#[test]
fn directory_delivery_writes_artifacts_then_manifest() {
    let png = solid_png(2, 2);
    let encoded = {
        let alphabet: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in png.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            out.push(alphabet[(n >> 18) as usize & 63] as char);
            out.push(alphabet[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                alphabet[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                alphabet[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    };
    let body = serde_json::json!({"status":"completed","output":[
        {"type":"message","content":[{"type":"output_text","text":"A dog"}]},
        {"type":"image_generation_call","result":encoded}
    ]})
    .to_string();
    let server = Server::json(&body);
    let dir = temp_dir("out-dir");
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{url}\"\n\
         [providers.srv.routes]\ngenerate = \"openai-responses\"",
        url = server.url()
    ));
    let out = run_tty_with(
        &[
            "ask",
            "-p",
            "画一只柴犬并说明",
            "--profile",
            "test",
            "--produce",
            "text,image",
            "--out-dir",
            dir.to_str().unwrap(),
            "--no-stream",
        ],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    assert_eq!(std::fs::read(dir.join("text.txt")).unwrap(), b"A dog");
    assert_eq!(std::fs::read(dir.join("image-1.png")).unwrap(), png);
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["artifacts"].as_array().unwrap().len(), 2);
}

#[test]
fn single_image_to_piped_stdout_is_exact_bytes() {
    let png = solid_png(2, 2);
    let encoded = {
        let alphabet: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in png.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            out.push(alphabet[(n >> 18) as usize & 63] as char);
            out.push(alphabet[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                alphabet[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                alphabet[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    };
    let body = serde_json::json!({"data":[{"b64_json":encoded}]}).to_string();
    let server = Server::json(&body);
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\noperations = [\"image\"]\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let out = run_tty_with(
        &["image", "--profile", "test", "--text", "dog"],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    assert_eq!(out.output.stdout, png);
}

#[test]
fn json_report_replaces_the_body() {
    let server = Server::json(chat_body("hidden body"));
    let dir = temp_dir("out-json");
    let file = dir.join("summary.txt");
    let cfg = chat_cfg(&server.url());
    let out = run(
        &[
            "summarize",
            "--profile",
            "test",
            "--json",
            "-o",
            file.to_str().unwrap(),
        ],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    let report: serde_json::Value = serde_json::from_str(&out.stdout()).unwrap();
    assert_eq!(report["version"], 1);
    assert_eq!(report["artifacts"][0]["kind"], "text");
    assert_eq!(report["artifacts"][0]["size"], "hidden body".len());
    assert!(report["artifacts"][0]["path"]
        .as_str()
        .unwrap()
        .ends_with("summary.txt"));
    let deliveries = report["deliveries"].as_array().unwrap();
    assert_eq!(deliveries.len(), 2);
    assert!(deliveries.iter().all(|d| d["status"] == "succeeded"));
    assert!(report["error"].is_null());
    // the body itself never hits stdout
    assert!(!out.stdout().contains("hidden body") || out.stdout().starts_with('{'));
    assert_eq!(std::fs::read(&file).unwrap(), b"hidden body");
}

#[test]
fn json_conflicts_with_explicit_stdout() {
    let out = run(&["ask", "-p", "hi", "--json", "--stdout"], b"", &[]);
    out.assert_code(2);
    assert!(out.stderr().contains("stdout"), "stderr: {}", out.stderr());
}

#[test]
fn several_artifacts_cannot_share_bare_stdout() {
    // produce two kinds and pipe stdout: the late check catches it.
    let body = serde_json::json!({"status":"completed","output":[
        {"type":"message","content":[{"type":"output_text","text":"text part"}]},
        {"type":"image_generation_call","result":encode_png()}
    ]})
    .to_string();
    let server = Server::json(&body);
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{url}\"\n\
         [providers.srv.routes]\ngenerate = \"openai-responses\"",
        url = server.url()
    ));
    let out = run_tty_with(
        &[
            "ask",
            "-p",
            "draw and explain",
            "--profile",
            "test",
            "--produce",
            "text,image",
            "--no-stream",
        ],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    assert_eq!(out.code(), 4, "stderr: {}", out.stderr());
    assert!(out.stdout().is_empty());
}

pub fn encode_png() -> String {
    let png = solid_png(2, 2);
    let alphabet: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in png.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(alphabet[(n >> 18) as usize & 63] as char);
        out.push(alphabet[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            alphabet[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            alphabet[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

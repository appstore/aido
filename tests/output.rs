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

#[cfg(unix)]
#[test]
fn image_to_clipboard_in_a_terminal_is_a_valid_plan() {
    // `--copy` is a documented destination for a binary artifact: in a
    // real terminal session (stdout is a tty too) the plan must list the
    // clipboard instead of demanding -o/--out-dir.
    let cfg = settings_config(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\noperations = [\"image\"]\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"",
    );
    let out = run_full_tty(
        &[
            "image",
            "--profile",
            "test",
            "--text",
            "dog",
            "--copy",
            "--dry-run",
        ],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    let plan = out.stdout();
    assert!(plan.contains("destinations:"), "{plan}");
    assert!(plan.contains("clipboard"), "{plan}");
}

#[cfg(unix)]
#[test]
fn image_without_any_destination_in_a_terminal_is_still_refused() {
    // The guard itself stays: a bare terminal cannot receive binary.
    let cfg = settings_config(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\noperations = [\"image\"]\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"",
    );
    let out = run_full_tty(
        &["image", "--profile", "test", "--text", "dog", "--dry-run"],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(2);
    assert!(
        out.stderr().contains("binary output needs"),
        "stderr: {}",
        out.stderr()
    );
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
fn out_dir_keeps_the_previous_delivery_without_overwrite() {
    // Run 1: a plain summary lands as text.txt plus its manifest.
    let server = Server::json(chat_body("first"));
    let dir = temp_dir("out-dir-twice");
    let cfg = chat_cfg(&server.url());
    let out = run(
        &[
            "summarize",
            "--profile",
            "test",
            "--out-dir",
            dir.to_str().unwrap(),
        ],
        b"hi\n",
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    assert_eq!(
        std::fs::read_to_string(dir.join("text.txt")).unwrap(),
        "first"
    );
    let manifest_before = std::fs::read(dir.join("manifest.json")).unwrap();

    // Run 2: a translate batch names its files a.txt/b.txt, so only the
    // manifest collides. Without --overwrite the run fails before writing
    // a byte: no new files, and the old manifest still describes the
    // first delivery exactly as run 1 left it.
    let inputs = temp_dir("out-dir-twice-inputs");
    let a = inputs.join("a.md");
    let b = inputs.join("b.md");
    std::fs::write(&a, "hello").unwrap();
    std::fs::write(&b, "world").unwrap();
    let server = MultiServer::start(&[chat_body("你好"), chat_body("世界")]);
    let cfg = chat_cfg(&server.url());
    let out = run_tty_with(
        &[
            "translate",
            "--profile",
            "test",
            "--no-stream",
            "--to",
            "zh-CN",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--out-dir",
            dir.to_str().unwrap(),
        ],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(5);
    let err = out.stderr();
    assert!(err.contains("manifest.json"), "{err}");
    assert!(err.contains("--overwrite"), "{err}");
    assert!(!dir.join("a.txt").exists(), "no byte may be written");
    assert!(!dir.join("b.txt").exists(), "no byte may be written");
    assert_eq!(
        std::fs::read(dir.join("manifest.json")).unwrap(),
        manifest_before,
        "the old manifest survives untouched"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("text.txt")).unwrap(),
        "first"
    );

    // Run 3: the same batch with --overwrite replaces the whole delivery;
    // the manifest now lists only this run's artifacts.
    let server = MultiServer::start(&[chat_body("你好"), chat_body("世界")]);
    let cfg = chat_cfg(&server.url());
    let out = run_tty_with(
        &[
            "translate",
            "--profile",
            "test",
            "--no-stream",
            "--to",
            "zh-CN",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--out-dir",
            dir.to_str().unwrap(),
            "--overwrite",
        ],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "你好");
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "世界");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    let files: Vec<_> = manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["file"].as_str().unwrap())
        .collect();
    assert_eq!(files, ["a.txt", "b.txt"]);
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
    // produce two kinds and pipe stdout: the late check catches it — as a
    // delivery failure (exit 5), since the generation already ran.
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
    assert_eq!(out.code(), 5, "stderr: {}", out.stderr());
    assert!(out
        .stderr()
        .contains("several artifacts cannot share bare stdout"));
    assert!(out.stdout().is_empty());
}

#[test]
fn two_images_to_one_file_fail_delivery_but_stay_recoverable() {
    // The service returns two images but only `-o one.png` was given: the
    // generation succeeded, so the refusal is a delivery failure (exit 5),
    // the run's history records the failed file attempt, and
    // `aido last --out-dir` recovers both artifacts without the model.
    let png = solid_png(2, 2);
    let encoded = encode_png();
    let body = serde_json::json!({"data":[{"b64_json":encoded},{"b64_json":encoded}]}).to_string();
    let server = Server::json(&body);
    let history = temp_dir("delivery-history");
    let file = temp_dir("delivery-target").join("one.png");
    let cfg = settings_config(&format!(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\noperations = [\"image\"]\n\
         [providers.srv]\nbase_url = \"{}\"",
        server.url()
    ));
    let out = run_tty_with(
        &[
            "image",
            "--profile",
            "test",
            "--text",
            "dog",
            "-o",
            file.to_str().unwrap(),
        ],
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", history.to_str().unwrap()),
        ],
        cfg.clone(),
    );
    out.assert_code(5);
    let err = out.stderr();
    assert!(err.contains("2 artifacts cannot go to one file"), "{err}");
    assert!(
        !file.exists(),
        "the refused target must not have been written"
    );

    // The run's manifest shows a complete generation and the failed file
    // delivery, next to the kept artifacts.
    let mut entries: Vec<_> = std::fs::read_dir(&history)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(entries.len(), 1, "one run dir, got {entries:?}");
    let run_dir = entries.pop().unwrap();
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(run_dir.join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["generation"]["status"], "complete");
    assert_eq!(manifest["artifacts"].as_array().unwrap().len(), 2);
    let deliveries = manifest["deliveries"].as_array().unwrap();
    assert_eq!(deliveries.len(), 1, "deliveries: {deliveries:?}");
    assert_eq!(deliveries[0]["destination"]["type"], "file");
    assert!(deliveries[0]["status"]["failed"]["error"]
        .as_str()
        .unwrap()
        .contains("one file"));

    // Recovery: redeliver the recorded run into a fresh directory.
    let recovered = temp_dir("delivery-recovered");
    let out = run(
        &["last", "--out-dir", recovered.to_str().unwrap()],
        b"",
        &[
            ("AIDO_CONFIG", empty_config().to_str().unwrap()),
            ("AIDO_HISTORY_DIR", history.to_str().unwrap()),
        ],
    );
    out.assert_code(0);
    assert_eq!(std::fs::read(recovered.join("image-1.png")).unwrap(), png);
    assert_eq!(std::fs::read(recovered.join("image-2.png")).unwrap(), png);
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

#[test]
fn attached_short_o_delivers_to_the_named_file() {
    // `-ofile` (attached short value) must reach --output, never collapse
    // into the prompt.
    let server = Server::json(chat_body("FILED"));
    let dir = temp_dir("out-attached");
    let file = dir.join("out.txt");
    let cfg = chat_cfg(&server.url());
    let out = run_tty_with(
        &[
            "summarize",
            "--profile",
            "test",
            "--text",
            "hi",
            &format!("-o{}", file.display()),
        ],
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
        cfg.clone(),
    );
    out.assert_code(0);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "FILED");
}

#[cfg(unix)]
#[test]
fn output_file_mode_follows_the_umask() {
    use std::os::unix::fs::PermissionsExt as _;
    // The child inherits this process's umask; read it the way the
    // delivery code does — umask(0) also sets, so restore immediately.
    let mask = unsafe { libc::umask(0) };
    unsafe { libc::umask(mask) };
    let server = Server::json(chat_body("UMASKED"));
    let dir = temp_dir("out-umask");
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
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o666 & !mask, "delivered files follow the umask");
}

#[cfg(unix)]
#[test]
fn history_artifacts_stay_private() {
    use std::os::unix::fs::PermissionsExt as _;
    let server = Server::json(chat_body("KEPT"));
    let history = temp_dir("history-mode");
    let cfg = settings_config(&format!(
        "[settings]\nhistory_keep = 2\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"{url}\"",
        url = server.url()
    ));
    let out = run(
        &["summarize", "--profile", "test"],
        b"hi\n",
        &[
            ("AIDO_CONFIG", cfg.to_str().unwrap()),
            ("AIDO_HISTORY_DIR", history.to_str().unwrap()),
        ],
    );
    out.assert_code(0);
    // save_generation stores the artifact bytes as text.txt inside the
    // one run directory, next to the run manifest.
    let mut entries: Vec<_> = std::fs::read_dir(&history)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(entries.len(), 1, "one run dir, got {entries:?}");
    let run_dir = entries.pop().unwrap();
    let mode = std::fs::metadata(run_dir.join("text.txt"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "history artifacts stay owner-only");
}

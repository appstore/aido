//! watch: the directory daemon. A watched file is an ordinary run, so
//! these tests assert on the ordinary traces — out-dir artifacts, history
//! manifests, stderr lines — plus the daemon-specific exits (130 on
//! SIGINT/SIGTERM, precheck exit 2, the debounce's request timing).

mod support;

use support::*;

use std::io::{BufRead as _, Read as _};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A working config for watch runs: history on (so per-file records can
/// be asserted) and a profile pointed at a local port. The provider names
/// no api_key_env, so no credential is needed.
fn watch_config(port: u16) -> std::path::PathBuf {
    settings_config(&format!(
        "[settings]\nhistory_keep = 5\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:{port}\""
    ))
}

/// A config whose provider demands a credential the environment does not
/// have: the watch precheck must refuse before guarding.
fn keyed_config(port: u16) -> std::path::PathBuf {
    settings_config(&format!(
        "[settings]\nhistory_keep = 0\n\
         [profiles.keyed]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:{port}\"\n\
         api_key_env = \"WATCH_TEST_KEY\""
    ))
}

#[cfg(unix)]
/// Spawn a long-running watch with the same isolation the shared `run`
/// helpers use, except history points at a real directory so per-file
/// records can be counted. stdin is the null device: a daemon's stdin.
/// The daemon's stderr is read line by line in the background so tests
/// can wait for readiness and still assert on the full output later.
fn spawn_watch(args: &[&str], cfg: &std::path::Path, history: &std::path::Path) -> Watch {
    let mut cmd = Command::new(EXE);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("AIDO_CONFIG", cfg)
        .env("AIDO_TASKS_DIR", "/nonexistent/aido-test-tasks")
        .env("AIDO_HISTORY_DIR", history);
    for var in [
        "OPENAI_API_KEY",
        "AIDO_API_KEY",
        "AIDO_PROFILE",
        "AIDO_MODEL",
        "AIDO_BASE_URL",
        "AIDO_ADAPTER",
        "AIDO_MAX_TOKENS",
        "AIDO_TEMPERATURE",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XDG_SESSION_TYPE",
    ] {
        cmd.env_remove(var);
    }
    let mut child = cmd.spawn().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stderr);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    Watch {
        child,
        lines: Vec::new(),
        pending_lines: rx,
    }
}

/// A running watch daemon plus everything it has said so far.
#[cfg(unix)]
struct Watch {
    child: Child,
    lines: Vec<String>,
    pending_lines: std::sync::mpsc::Receiver<String>,
}

#[cfg(unix)]
impl Watch {
    fn drain(&mut self) {
        while let Ok(line) = self.pending_lines.try_recv() {
            self.lines.push(line);
        }
    }

    /// Wait until the daemon is past its precheck and entering the scan
    /// loop. The banner is printed after the startup inventory is seeded:
    /// a file written once it shows is an arrival, never startup history
    /// (which the daemon marks done unless `--include-existing` says
    /// otherwise).
    fn ready(&mut self) {
        self.drain();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if self.lines.iter().any(|l| l.contains("guarding")) {
                return;
            }
            let now = Instant::now();
            if now >= deadline {
                panic!(
                    "the daemon never became ready; stderr so far:\n{}",
                    self.lines.join("\n")
                );
            }
            match self
                .pending_lines
                .recv_timeout((deadline - now).min(Duration::from_millis(100)))
            {
                Ok(line) => self.lines.push(line),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => panic!(
                    "the daemon exited before guarding; stderr:\n{}",
                    self.lines.join("\n")
                ),
            }
        }
    }

    /// Signal the daemon and collect its full output.
    fn kill(&mut self, signal: i32) -> RunOutcome {
        unsafe { libc::kill(self.child.id() as i32, signal) };
        let status = self.child.wait().unwrap();
        // stdout stays empty for a guarding daemon (deliveries go to
        // --out-dir/--copy); read whatever is there after the exit.
        let mut stdout = Vec::new();
        if let Some(mut pipe) = self.child.stdout.take() {
            let _ = pipe.read_to_end(&mut stdout);
        }
        // The reader thread sees EOF once the process is gone.
        while let Ok(line) = self.pending_lines.recv_timeout(Duration::from_secs(5)) {
            self.lines.push(line);
        }
        self.lines.push(String::new());
        RunOutcome {
            output: std::process::Output {
                status,
                stdout,
                stderr: self.lines.join("\n").into_bytes(),
            },
        }
    }
}

/// A test that panics while a daemon runs must not leak it: a leaked
/// daemon holds the harness's output pipes open and hangs the whole
/// cargo invocation.
#[cfg(unix)]
impl Drop for Watch {
    fn drop(&mut self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

#[cfg(unix)]
fn wait_until(cond: impl FnMut() -> bool, timeout: Duration, what: &str) {
    let mut cond = cond;
    let deadline = Instant::now() + timeout;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The inverse of `support::accept`: prove nothing connects for a while.
fn assert_no_connection(listener: &TcpListener, for_: Duration) {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + for_;
    loop {
        match listener.accept() {
            Ok(_) => panic!("aido connected when nothing should have fired"),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("accept failed: {e}"),
        }
    }
}

/// History run directories that carry a manifest, sorted.
#[cfg(unix)]
fn run_dirs(history: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(history).unwrap().flatten() {
        if entry.path().join("manifest.json").is_file() {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    dirs
}

/// The watch arguments for "guard DIR, run ask on every file into OUT".
fn watch_args(dir: &std::path::Path, out: &std::path::Path, extra: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = [
        "watch",
        dir.to_str().unwrap(),
        "--interval",
        "0.1",
        "--stable-ms",
        "100",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(extra.iter().map(|s| s.to_string()));
    args.extend(
        [
            "--",
            "ask",
            "-p",
            "hi",
            "--profile",
            "test",
            "--out-dir",
            out.to_str().unwrap(),
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    args
}

fn args_of(parts: &[String]) -> Vec<&str> {
    parts.iter().map(|s| s.as_str()).collect()
}

#[cfg(unix)]
#[test]
fn a_new_file_runs_the_task_and_sigint_exits_130() {
    let server = MultiServer::start(&[chat_body("watched-ok")]);
    let guard = temp_dir("watch-basic");
    let out = temp_dir("watch-basic-out");
    let history = temp_dir("watch-basic-hist");
    let cfg = watch_config(server.port);
    let mut watch = spawn_watch(&args_of(&watch_args(&guard, &out, &[])), &cfg, &history);
    watch.ready();

    std::fs::write(guard.join("shot.txt"), b"hello").unwrap();
    wait_until(
        || out.join("manifest.json").exists(),
        Duration::from_secs(15),
        "the out-dir manifest",
    );
    // The artifact set landed: the text result plus the manifest.
    let saved: Vec<_> = std::fs::read_dir(&out)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(saved.iter().any(|f| f.ends_with(".txt")), "{saved:?}");

    let out = watch.kill(libc::SIGINT);
    out.assert_code(130);
    let err = out.stderr();
    assert!(err.contains("guarding"), "{err}");
    assert!(err.contains("[watch] shot.txt → ask: done"), "{err}");
    // Piped stderr never rings.
    assert!(!err.contains('\u{7}'), "{err}");
    // One file, one request, one ordinary history record.
    assert_eq!(server.requests().len(), 1);
    assert_eq!(run_dirs(&history).len(), 1);
}

#[cfg(unix)]
#[test]
fn two_files_share_one_out_dir_without_colliding() {
    let server = MultiServer::start(&[chat_body("first"), chat_body("second")]);
    let guard = temp_dir("watch-two");
    let out = temp_dir("watch-two-out");
    let history = temp_dir("watch-two-hist");
    let cfg = watch_config(server.port);
    let mut watch = spawn_watch(&args_of(&watch_args(&guard, &out, &[])), &cfg, &history);
    watch.ready();

    // The second arrival must neither fail on the first run's
    // manifest.json nor clobber the first run's artifact: watched
    // artifacts are named after their inputs.
    std::fs::write(guard.join("a.txt"), b"first").unwrap();
    wait_until(
        || out.join("a.txt").exists(),
        Duration::from_secs(15),
        "the first artifact",
    );
    std::fs::write(guard.join("b.txt"), b"second").unwrap();
    wait_until(
        || out.join("b.txt").exists(),
        Duration::from_secs(15),
        "the second artifact",
    );

    let outcome = watch.kill(libc::SIGINT);
    outcome.assert_code(130);
    assert_eq!(
        std::fs::read_to_string(out.join("a.txt")).unwrap(),
        "first",
        "the first artifact survived the second run"
    );
    assert_eq!(
        std::fs::read_to_string(out.join("b.txt")).unwrap(),
        "second"
    );
    assert!(out.join("manifest.json").exists());
    let err = outcome.stderr();
    assert!(err.contains("[watch] a.txt → ask: done"), "{err}");
    assert!(err.contains("[watch] b.txt → ask: done"), "{err}");
    assert_eq!(run_dirs(&history).len(), 2);
}

#[cfg(unix)]
#[test]
fn existing_files_are_not_replayed_without_include_existing() {
    let server = MultiServer::start(&[chat_body("late-ok")]);
    let guard = temp_dir("watch-existing");
    let out = temp_dir("watch-existing-out");
    let history = temp_dir("watch-existing-hist");
    let cfg = watch_config(server.port);
    // The file is already there when the watch starts: it stays untouched.
    std::fs::write(guard.join("old.txt"), b"old").unwrap();
    let mut watch = spawn_watch(&args_of(&watch_args(&guard, &out, &[])), &cfg, &history);
    watch.ready();

    // A file that arrives later runs; the old one never does.
    std::fs::write(guard.join("new.txt"), b"new").unwrap();
    wait_until(
        || out.join("manifest.json").exists(),
        Duration::from_secs(15),
        "the out-dir manifest",
    );
    let out = watch.kill(libc::SIGINT);
    out.assert_code(130);
    let err = out.stderr();
    assert!(err.contains("[watch] new.txt → ask: done"), "{err}");
    assert!(!err.contains("old.txt"), "{err}");
    assert_eq!(server.requests().len(), 1);
    assert_eq!(run_dirs(&history).len(), 1);
}

#[cfg(unix)]
#[test]
fn include_existing_picks_up_files_already_in_the_directory() {
    let server = MultiServer::start(&[chat_body("old-ok")]);
    let guard = temp_dir("watch-include");
    let out = temp_dir("watch-include-out");
    let history = temp_dir("watch-include-hist");
    let cfg = watch_config(server.port);
    std::fs::write(guard.join("old.txt"), b"old").unwrap();
    let mut watch = spawn_watch(
        &args_of(&watch_args(&guard, &out, &["--include-existing"])),
        &cfg,
        &history,
    );
    watch.ready();

    wait_until(
        || out.join("manifest.json").exists(),
        Duration::from_secs(15),
        "the out-dir manifest",
    );
    let out = watch.kill(libc::SIGINT);
    out.assert_code(130);
    let err = out.stderr();
    assert!(err.contains("[watch] old.txt → ask: done"), "{err}");
    assert_eq!(run_dirs(&history).len(), 1);
}

#[cfg(unix)]
#[test]
fn a_growing_file_is_debounced_until_it_settles() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let cfg = watch_config(port);
    let guard = temp_dir("watch-debounce");
    let out = temp_dir("watch-debounce-out");
    let history = temp_dir("watch-debounce-hist");
    let mut watch = spawn_watch(
        &args_of(&watch_args(&guard, &out, &["--stable-ms", "1500"])),
        &cfg,
        &history,
    );
    watch.ready();

    // Grow the file every 100 ms for a second: a debouncing daemon never
    // fires while the size keeps changing.
    use std::io::Write as _;
    let file = guard.join("growing.txt");
    std::fs::write(&file, b"chunk-0").unwrap();
    for i in 1..=10 {
        std::thread::sleep(Duration::from_millis(100));
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        writeln!(f, "chunk-{i}").unwrap();
    }
    // Still nothing while growing, and for a grace beat after it stops
    // (the stability window has 1500 ms to run).
    assert_no_connection(&listener, Duration::from_millis(300));

    // Settled: exactly one request arrives, carrying the final content.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut stream = accept(&listener, deadline);
    let raw = read_request(&mut stream);
    write_response(&mut stream, "200 OK", chat_body("ok"));
    assert!(
        String::from_utf8_lossy(&raw).contains("chunk-9"),
        "the request ran on the settled file"
    );
    wait_until(
        || out.join("manifest.json").exists(),
        Duration::from_secs(15),
        "the out-dir manifest",
    );
    let out = watch.kill(libc::SIGINT);
    out.assert_code(130);
    // Once, not once per chunk.
    assert_no_connection(&listener, Duration::from_millis(500));
}

#[cfg(unix)]
#[test]
fn dotfiles_and_subdirectories_never_trigger() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let cfg = watch_config(port);
    let guard = temp_dir("watch-skip");
    let out = temp_dir("watch-skip-out");
    let history = temp_dir("watch-skip-hist");
    let mut watch = spawn_watch(&args_of(&watch_args(&guard, &out, &[])), &cfg, &history);
    watch.ready();

    std::fs::write(guard.join(".hidden.txt"), b"nope").unwrap();
    let sub = guard.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("inner.txt"), b"nope").unwrap();
    // The same names before startup are skipped too — one pass covers
    // both, since the scan rule does not care when a file appeared.
    assert_no_connection(&listener, Duration::from_millis(1500));

    let out = watch.kill(libc::SIGINT);
    out.assert_code(130);
    assert!(run_dirs(&history).is_empty());
}

fn precheck_cases(dir: &str, out_dir: &str) -> Vec<(Vec<String>, &'static str)> {
    vec![
        // `ask` without -p would start a watch that fails every file.
        (
            vec![
                "watch".into(),
                dir.into(),
                "--".into(),
                "ask".into(),
                "--profile".into(),
                "test".into(),
                "--copy".into(),
            ],
            "needs -p",
        ),
        // No explicit delivery destination.
        (
            vec![
                "watch".into(),
                dir.into(),
                "--".into(),
                "ask".into(),
                "-p".into(),
                "hi".into(),
                "--profile".into(),
                "test".into(),
            ],
            "explicit delivery destination",
        ),
        // -o names one fixed file every arrival would fight over.
        (
            vec![
                "watch".into(),
                dir.into(),
                "--".into(),
                "ask".into(),
                "-p".into(),
                "hi".into(),
                "--profile".into(),
                "test".into(),
                "-o".into(),
                out_dir.into(),
            ],
            "cannot use -o",
        ),
        // A task flag in front of the separator.
        (
            vec![
                "watch".into(),
                dir.into(),
                "--out-dir".into(),
                out_dir.into(),
                "--".into(),
                "ask".into(),
                "-p".into(),
                "hi".into(),
                "--profile".into(),
                "test".into(),
            ],
            "belong after the `--` separator",
        ),
        // A task that does not exist.
        (
            vec![
                "watch".into(),
                dir.into(),
                "--".into(),
                "nosuch".into(),
                "--profile".into(),
                "test".into(),
                "--out-dir".into(),
                out_dir.into(),
            ],
            "unknown task",
        ),
        // The invocation leans on stdin.
        (
            vec![
                "watch".into(),
                dir.into(),
                "--".into(),
                "ask".into(),
                "-p".into(),
                "hi".into(),
                "-".into(),
                "--profile".into(),
                "test".into(),
                "--out-dir".into(),
                out_dir.into(),
            ],
            "cannot read stdin",
        ),
        // Delivering into the guarded directory re-triggers the watch.
        (
            vec![
                "watch".into(),
                dir.into(),
                "--".into(),
                "ask".into(),
                "-p".into(),
                "hi".into(),
                "--profile".into(),
                "test".into(),
                "--out-dir".into(),
                dir.into(),
            ],
            "re-trigger the watch",
        ),
        // A per-file dry-run would mark files done without delivering.
        (
            vec![
                "watch".into(),
                dir.into(),
                "--".into(),
                "ask".into(),
                "-p".into(),
                "hi".into(),
                "--profile".into(),
                "test".into(),
                "--out-dir".into(),
                out_dir.into(),
                "--dry-run".into(),
            ],
            "cannot be part of a watched task",
        ),
        // The guarded path is not a directory.
        (
            vec![
                "watch".into(),
                "/nonexistent/aido-watch-guard".into(),
                "--".into(),
                "ask".into(),
                "-p".into(),
                "hi".into(),
                "--profile".into(),
                "test".into(),
                "--copy".into(),
            ],
            "is not a directory",
        ),
    ]
}

#[test]
fn precheck_failures_exit_2_without_guarding() {
    let server = Server::json(chat_body("never-asked"));
    let guard = temp_dir("watch-precheck");
    let out = temp_dir("watch-precheck-out");
    let cfg = watch_config(server.port);
    for (args, needle) in precheck_cases(guard.to_str().unwrap(), out.to_str().unwrap()) {
        let args = args.iter().map(|s| s.as_str()).collect::<Vec<_>>();
        let out = run_with(&args, b"", &[], cfg.clone());
        out.assert_code(2);
        assert!(
            out.stderr().contains(needle),
            "`{}`: wanted {needle:?} in stderr: {}",
            args.join(" "),
            out.stderr()
        );
    }
}

#[test]
fn the_missing_credential_is_named_in_the_precheck_error() {
    let server = Server::json(chat_body("never-asked"));
    let cfg = keyed_config(server.port);
    let guard = temp_dir("watch-cred");
    let out = run_with(
        &[
            "watch",
            guard.to_str().unwrap(),
            "--",
            "ask",
            "-p",
            "hi",
            "--profile",
            "keyed",
            "--copy",
        ],
        b"",
        &[],
        &cfg,
    );
    out.assert_code(2);
    assert!(out.stderr().contains("WATCH_TEST_KEY"), "{}", out.stderr());
}

#[test]
fn dry_run_prints_the_probe_plan_and_exits_without_a_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let cfg = watch_config(port);
    let guard = temp_dir("watch-dry");
    let out = temp_dir("watch-dry-out");
    let out = run_with(
        &args_of(&watch_args(&guard, &out, &["--dry-run"])),
        b"",
        &[],
        &cfg,
    );
    out.assert_code(0);
    // The preamble names the probe; the plan itself goes to stdout.
    assert!(
        out.stderr().contains("showing the probe plan"),
        "{}",
        out.stderr()
    );
    assert!(out.stdout().contains("ask"), "{}", out.stdout());
    // A dry-run makes no request and never settles into the loop.
    assert_no_connection(&listener, Duration::from_millis(300));
}

#[cfg(unix)]
#[test]
fn a_failed_file_does_not_stop_the_watch() {
    let server = MultiServer::start_statuses(&[
        (
            "500 Internal Server Error",
            "{\"error\":{\"message\":\"boom\"}}",
        ),
        ("200 OK", chat_body("recovered")),
    ]);
    let guard = temp_dir("watch-recover");
    let out = temp_dir("watch-recover-out");
    let history = temp_dir("watch-recover-hist");
    let cfg = watch_config(server.port);
    let mut watch = spawn_watch(&args_of(&watch_args(&guard, &out, &[])), &cfg, &history);
    watch.ready();

    std::fs::write(guard.join("bad.txt"), b"bad").unwrap();
    // Give the first file time to fire and fail.
    std::thread::sleep(Duration::from_millis(900));
    std::fs::write(guard.join("good.txt"), b"good").unwrap();
    wait_until(
        || out.join("manifest.json").exists(),
        Duration::from_secs(15),
        "the second file's manifest",
    );
    let out = watch.kill(libc::SIGINT);
    out.assert_code(130);
    let err = out.stderr();
    assert!(
        err.contains("[watch] bad.txt → ask: failed (not retried"),
        "{err}"
    );
    assert!(err.contains("still watching"), "{err}");
    assert!(err.contains("[watch] good.txt → ask: done"), "{err}");
    assert_eq!(server.requests().len(), 2);
    // Only the good file produced an ordinary record.
    assert_eq!(run_dirs(&history).len(), 2);
}

#[cfg(unix)]
#[test]
fn sigterm_exits_130_like_ctrl_c() {
    let server = MultiServer::start(&[chat_body("never-asked")]);
    let guard = temp_dir("watch-term");
    let out = temp_dir("watch-term-out");
    let history = temp_dir("watch-term-hist");
    let cfg = watch_config(server.port);
    let mut watch = spawn_watch(&args_of(&watch_args(&guard, &out, &[])), &cfg, &history);
    watch.ready();
    // Through the precheck, into the loop, idle.
    std::thread::sleep(Duration::from_millis(500));
    let out = watch.kill(libc::SIGTERM);
    out.assert_code(130);
    assert!(out.stderr().contains("guarding"), "{}", out.stderr());
}

#[cfg(unix)]
#[test]
fn ctrl_c_during_a_run_records_it_cancelled_and_exits_130() {
    use std::io::Read;

    // A server that accepts and never answers: the watched file's request
    // hangs until the interrupt arrives. The holder reads until aido dies
    // so the join returns as soon as the daemon is gone.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (in_flight_tx, in_flight_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut stream = accept(&listener, deadline);
        stream.set_nonblocking(false).unwrap();
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        let _ = in_flight_tx.send(());
        let mut buf = [0u8; 8192];
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });
    let guard = temp_dir("watch-cancel");
    let out = temp_dir("watch-cancel-out");
    let history = temp_dir("watch-cancel-hist");
    let cfg = watch_config(port);
    let mut watch = spawn_watch(&args_of(&watch_args(&guard, &out, &[])), &cfg, &history);
    watch.ready();
    std::fs::write(guard.join("hang.txt"), b"hang").unwrap();
    // The request must be in flight before the interrupt: wait for the
    // holder's accept, not for a sleep.
    std::thread::sleep(Duration::from_millis(2000));
    in_flight_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("the watched file never started a request");
    let out = watch.kill(libc::SIGINT);
    out.assert_code(130);
    holder.join().ok();

    let runs = run_dirs(&history);
    assert_eq!(runs.len(), 1, "the interrupted run leaves a trace");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(runs[0].join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["generation"]["status"], "cancelled");
}

#[cfg(unix)]
#[test]
fn clipboard_delivery_is_a_valid_watch_plan_on_a_terminal() {
    // base_url that is never contacted: a dry-run plan needs no server.
    let cfg = settings_config(
        "[profiles.test]\nprovider = \"srv\"\nmodel = \"m\"\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:1\"",
    );
    let guard = temp_dir("watch-tty");
    let out = run_full_tty(
        &[
            "watch",
            guard.to_str().unwrap(),
            "--dry-run",
            "--",
            "ocr",
            "--profile",
            "test",
            "--copy",
        ],
        &[],
        cfg,
    );
    out.assert_code(0);
    assert!(out.stdout().contains("clipboard"), "{}", out.stdout());
}

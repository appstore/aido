//! Watch daemons the UI started: a registry of in-flight guards. Each
//! daemon gets its own thread with a current-thread runtime (the run
//! path underneath is `Rc` single-threaded, like every UI run), streams
//! its activity over a broadcast channel, and stops cooperatively —
//! between files, never mid-run. Only watches this server started are
//! here; a CLI `aido watch` is another process the UI cannot see, and
//! that boundary is deliberate.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use clap::Parser as _;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::{broadcast, watch as cancel_watch};

use crate::cli::WatchArgs;
use crate::domain::{AppError, AppResult};
use crate::watch::{self, WatchEvent};

use super::invoke::{self, RunRequest};

/// What the UI may ask a watch to do. The task flags are the run
/// whitelist's own; delivery is always `--out-dir` under the guarded
/// directory (a subdirectory never re-triggers the scan), because a
/// watched file's stdout has no audience in a browser.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchRequest {
    /// The directory to guard, on the server's disk.
    pub dir: String,
    pub task: String,
    pub prompt: Option<String>,
    pub profile: Option<String>,
    pub model: Option<String>,
    pub to: Option<String>,
    pub voice: Option<String>,
    pub speed: Option<f64>,
    pub count: Option<u64>,
    pub size: Option<String>,
    #[serde(default)]
    pub no_split: bool,
    pub timeout_secs: Option<u64>,
    pub total_timeout_secs: Option<u64>,
    /// Where results land: a name under DIR (default "out").
    pub out_subdir: Option<String>,
    /// Poll interval override, seconds (the CLI's `--interval`).
    pub interval_secs: Option<f64>,
    /// Size-stability window override, milliseconds.
    pub stable_ms: Option<u64>,
    /// Process the files already in DIR when the daemon starts.
    #[serde(default)]
    pub include_existing: bool,
}

/// One SSE frame off a daemon, mirroring [`WatchEvent`] with the names
/// and timestamps a dashboard reads.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum WatchFrame {
    Started {
        at: String,
        dir: String,
        task: String,
        out_dir: String,
        interval_ms: u64,
        stable_ms: u64,
    },
    FileDone {
        at: String,
        file: String,
        task: String,
    },
    FileFailed {
        at: String,
        file: String,
        task: String,
        reason: String,
    },
    DirUnreadable {
        at: String,
        dir: String,
        error: String,
    },
    DirReadable {
        at: String,
        dir: String,
    },
    Stopped {
        at: String,
        reason: Option<String>,
    },
    /// The subscriber fell behind a fast daemon; the counters in the
    /// list keep the whole truth.
    Lagged {
        at: String,
    },
}

/// The synthetic frame a lagged subscriber receives (mirrors the runs'
/// stream behavior).
pub(super) fn lagged_frame() -> WatchFrame {
    WatchFrame::Lagged {
        at: crate::app::now_iso(),
    }
}

/// The mutable half of a daemon's identity: counters and the last
/// verdict, updated by the daemon thread and read by the list.
#[derive(Default)]
struct WatchInfo {
    processed: u32,
    failed: u32,
    last_file: Option<String>,
    last_error: Option<String>,
    status: &'static str, // "running" | "stopping" | "stopped"
    stop_reason: Option<String>,
}

struct WatchHandle {
    cancel: cancel_watch::Sender<bool>,
    /// None once the daemon has ended: the channel closes, which is the
    /// SSE stream's natural end.
    events: Option<broadcast::Sender<WatchFrame>>,
    info: Arc<Mutex<WatchInfo>>,
}

/// The fixed half of the list answer: what the daemon was started as.
struct WatchMeta {
    dir: String,
    task: String,
    out_dir: String,
    interval_ms: u64,
    stable_ms: u64,
    include_existing: bool,
    started_at: String,
}

#[derive(Default)]
pub struct Watches {
    inner: Mutex<HashMap<String, (WatchMeta, WatchHandle)>>,
}

/// A validated watch, ready to start or to describe: the `--quiet` Cli
/// the daemon runs under, the watch args (task argv carrying the
/// resolved `--out-dir`), and the precheck's proof.
pub struct PreparedWatch {
    cli: crate::cli::Cli,
    args: WatchArgs,
    setup: watch::WatchSetup,
    out_dir: PathBuf,
    interval_ms: u64,
    stable_ms: u64,
}

impl Watches {
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Validate a watch exactly the way the CLI would — same probe, same
    /// messages — without starting anything. Blocking (it reads the
    /// guarded directory and builds a probe plan); callers keep it off
    /// the async workers.
    pub fn prepare(request: &WatchRequest) -> AppResult<PreparedWatch> {
        invoke::check_task_name(&request.task)?;
        let dir = PathBuf::from(request.dir.trim());
        if dir.as_os_str().is_empty() {
            return Err(AppError::usage("name the directory to watch"));
        }
        // Delivery lands in a subdirectory of the guarded directory: a
        // single component (never a path), so outputs can neither escape
        // nor land in the guarded root and re-trigger the scan.
        let subdir = request.out_subdir.as_deref().unwrap_or("out");
        let subdir = safe_component("out_subdir", subdir)?;
        let guard = lexical_absolute(&dir);
        let out_dir = guard.join(subdir);

        // The task argv is the run whitelist's own builder, plus the
        // one delivery flag a watch must carry.
        let mut argv = invoke::argv_for_task(&RunRequest {
            task: request.task.clone(),
            prompt: request.prompt.clone(),
            profile: request.profile.clone(),
            model: request.model.clone(),
            to: request.to.clone(),
            voice: request.voice.clone(),
            speed: request.speed,
            count: request.count,
            size: request.size.clone(),
            no_split: request.no_split,
            timeout_secs: request.timeout_secs,
            total_timeout_secs: request.total_timeout_secs,
            out_dir: None,
            out_file: None,
            texts: Vec::new(),
        })?;
        argv.push(format!("--out-dir={}", out_dir.to_string_lossy()).into());
        let args = WatchArgs {
            dir,
            interval: request.interval_secs,
            stable_ms: request.stable_ms,
            include_existing: request.include_existing,
            task_argv: argv,
            parent_argv: Vec::new(),
        };
        // The daemon's own run surface: quiet (the server's terminal is
        // not the audience), everything else default.
        let cli = crate::cli::Cli::try_parse_from(["aido", "--quiet"])
            .map_err(|e| AppError::usage(e.to_string()))?;
        let setup = watch::precheck(&cli, &args)?;
        let (interval_ms, stable_ms) = (
            setup.interval.as_millis() as u64,
            setup.stable.as_millis() as u64,
        );
        Ok(PreparedWatch {
            cli,
            args,
            setup,
            out_dir,
            interval_ms,
            stable_ms,
        })
    }

    /// The dashboard's dry-run: the probe plan each arriving file would
    /// run, described — zero requests.
    pub fn preview(prepared: &PreparedWatch) -> serde_json::Value {
        serde_json::json!({
            "text": crate::plan::describe(&prepared.setup.probe.plan),
            "task": prepared.setup.probe.task_name,
            "dir": prepared.args.dir.display().to_string(),
            "out_dir": prepared.out_dir.display().to_string(),
            "interval_ms": prepared.interval_ms,
            "stable_ms": prepared.stable_ms,
        })
    }

    /// Start a validated watch on its own thread. The id exists before
    /// the daemon runs, so a stop can always find it.
    pub fn start(self: &Arc<Self>, prepared: PreparedWatch) -> String {
        let id = format!("w{}", crate::history::stamp_now());
        let (events, _) = broadcast::channel(256);
        let (cancel, cancelled) = cancel_watch::channel(false);
        let info = Arc::new(Mutex::new(WatchInfo {
            status: "running",
            ..WatchInfo::default()
        }));
        let meta = WatchMeta {
            dir: prepared.args.dir.display().to_string(),
            task: prepared.setup.probe.task_name.clone(),
            out_dir: prepared.out_dir.display().to_string(),
            interval_ms: prepared.interval_ms,
            stable_ms: prepared.stable_ms,
            include_existing: prepared.args.include_existing,
            started_at: crate::app::now_iso(),
        };
        self.inner.lock().unwrap().insert(
            id.clone(),
            (
                meta,
                WatchHandle {
                    cancel,
                    events: Some(events.clone()),
                    info: info.clone(),
                },
            ),
        );

        let watches = self.clone();
        let thread_id = id.clone();
        // The thread owns its own clones; the caller keeps `info` for
        // the spawn-failed branch below.
        let info_thread = info.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("aido-ui-watch-{thread_id}"))
            .spawn(move || {
                let watches = watches.clone();
                let id = thread_id.clone();
                let info = info_thread;
                let finish = {
                    // The guard owns what it finalizes: independent Arc
                    // clones, so it outlives the locals below.
                    struct Finish(Arc<Watches>, String, Arc<Mutex<WatchInfo>>);
                    impl Drop for Finish {
                        fn drop(&mut self) {
                            let stopped = {
                                let mut info = self.2.lock().unwrap();
                                let was = info.status;
                                if was != "stopped" {
                                    info.status = "stopped";
                                    if info.stop_reason.is_none() {
                                        info.stop_reason =
                                            Some("the daemon ended unexpectedly".into());
                                    }
                                }
                                was != "stopped"
                            };
                            if stopped {
                                let _ = self.0.emit_frame(
                                    &self.1,
                                    WatchFrame::Stopped {
                                        at: crate::app::now_iso(),
                                        reason: None,
                                    },
                                );
                            }
                            if let Some((_, handle)) = self.0.inner.lock().unwrap().get_mut(&self.1)
                            {
                                handle.events = None;
                            }
                        }
                    }
                    Finish(watches.clone(), id.clone(), info.clone())
                };
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                let local = tokio::task::LocalSet::new();
                let info2 = info.clone();
                let events2 = events.clone();
                let meta_task = prepared.setup.probe.task_name.clone();
                local.block_on(&runtime, async move {
                    let state: Arc<std::sync::Mutex<crate::app::RunState>> = Arc::default();
                    let _ = events2.send(WatchFrame::Started {
                        at: crate::app::now_iso(),
                        dir: prepared.args.dir.display().to_string(),
                        task: meta_task,
                        out_dir: prepared.out_dir.display().to_string(),
                        interval_ms: prepared.interval_ms,
                        stable_ms: prepared.stable_ms,
                    });
                    let mut emit = |event: WatchEvent| {
                        let frame = match event {
                            WatchEvent::FileDone { path, task } => {
                                if let Ok(mut info) = info2.lock() {
                                    info.processed += 1;
                                    info.last_file = Some(file_name(&path));
                                    info.last_error = None;
                                }
                                WatchFrame::FileDone {
                                    at: crate::app::now_iso(),
                                    file: file_name(&path),
                                    task,
                                }
                            }
                            WatchEvent::FileFailed { path, task, reason } => {
                                if let Ok(mut info) = info2.lock() {
                                    info.failed += 1;
                                    info.last_file = Some(file_name(&path));
                                    info.last_error = Some(reason.clone());
                                }
                                WatchFrame::FileFailed {
                                    at: crate::app::now_iso(),
                                    file: file_name(&path),
                                    task,
                                    reason,
                                }
                            }
                            WatchEvent::DirUnreadable { dir, error } => WatchFrame::DirUnreadable {
                                at: crate::app::now_iso(),
                                dir,
                                error,
                            },
                            WatchEvent::DirReadable { dir } => WatchFrame::DirReadable {
                                at: crate::app::now_iso(),
                                dir,
                            },
                            WatchEvent::Stopped => return,
                        };
                        let _ = events2.send(frame);
                    };
                    let stop = async {
                        let mut cancelled = cancelled;
                        while cancelled.changed().await.is_ok() {
                            if *cancelled.borrow() {
                                return;
                            }
                        }
                    };
                    watch::serve(
                        &prepared.cli,
                        &prepared.args,
                        prepared.setup,
                        &state,
                        &mut emit,
                        stop,
                    )
                    .await;
                });
                // The clean ending: serve returned (stop requested).
                if let Ok(mut info) = info.lock() {
                    info.status = "stopped";
                }
                drop(finish);
            })
            .is_ok();
        if !spawned {
            // Nothing started; the entry must not claim otherwise.
            self.inner.lock().unwrap().remove(&id);
            if let Ok(mut info) = info.lock() {
                info.status = "stopped";
                info.stop_reason = Some("cannot spawn the daemon thread".into());
            }
        }
        id
    }

    fn emit_frame(&self, id: &str, frame: WatchFrame) -> Option<broadcast::Receiver<WatchFrame>> {
        let handle = self
            .inner
            .lock()
            .unwrap()
            .get(id)
            .map(|(_, h)| h.events.clone());
        if let Some(Some(sender)) = handle {
            let _ = sender.send(frame);
            Some(sender.subscribe())
        } else {
            None
        }
    }

    /// The dashboard's list: newest first, stopped daemons included (a
    /// stopped guard is still a fact about this server's session).
    pub fn list(&self) -> Vec<serde_json::Value> {
        let inner = self.inner.lock().unwrap();
        let mut rows: Vec<(String, &WatchMeta, &WatchHandle)> = inner
            .iter()
            .map(|(id, (meta, handle))| (id.clone(), meta, handle))
            .collect();
        rows.sort_by(|a, b| b.0.cmp(&a.0));
        rows.into_iter()
            .map(|(id, meta, handle)| {
                let info = handle.info.lock().unwrap();
                serde_json::json!({
                    "id": id,
                    "dir": meta.dir,
                    "task": meta.task,
                    "out_dir": meta.out_dir,
                    "interval_ms": meta.interval_ms,
                    "stable_ms": meta.stable_ms,
                    "include_existing": meta.include_existing,
                    "started_at": meta.started_at,
                    "status": info.status,
                    "processed": info.processed,
                    "failed": info.failed,
                    "last_file": info.last_file,
                    "last_error": info.last_error,
                    "stop_reason": info.stop_reason,
                })
            })
            .collect()
    }

    /// Subscribe to a live daemon's activity. The channel closes when
    /// the daemon ends (after its `stopped` frame); no replay, exactly
    /// like the runs' streams.
    pub fn subscribe(&self, id: &str) -> Option<broadcast::Receiver<WatchFrame>> {
        self.inner
            .lock()
            .unwrap()
            .get(id)
            .and_then(|(_, handle)| handle.events.as_ref().map(|e| e.subscribe()))
    }

    /// Ask a daemon to stop. It takes effect between files — the current
    /// file, if any, finishes first.
    pub fn stop(&self, id: &str) -> bool {
        match self.inner.lock().unwrap().get_mut(id) {
            Some((_, handle)) => {
                let _ = handle.cancel.send_if_modified(|asked| {
                    let first = !*asked;
                    *asked = true;
                    first
                });
                if let Ok(mut info) = handle.info.lock() {
                    if info.status == "running" {
                        info.status = "stopping";
                    }
                }
                true
            }
            None => false,
        }
    }
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// A single path component for a user-named subdirectory — the same
/// rule the run whitelist applies to delivery names.
fn safe_component(field: &str, name: &str) -> AppResult<String> {
    let name = name.trim();
    let bad = |why: &str| {
        AppError::usage(format!("'{name}' is not a usable {field} ({why}); it names a directory under the watch directory"))
    };
    if name.is_empty() {
        return Err(bad("empty"));
    }
    if name.len() > 128
        || name.contains(['/', '\\'])
        || name == ".."
        || name.starts_with('.')
        || name.contains('\0')
    {
        return Err(bad("one path component, not starting with '.'"));
    }
    Ok(name.to_string())
}

/// Lexically absolute form (watch's own `absolute` is private): the
/// guarded directory exists by the time precheck runs, but the out-dir
/// comparison wants the same normalization for the path we build.
fn lexical_absolute(path: &std::path::Path) -> PathBuf {
    if let Ok(p) = path.canonicalize() {
        return p;
    }
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

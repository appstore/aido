//! Turning an HTTP request into a run: parse the request, land the
//! uploaded files, and re-enter the CLI's own parsing — the argv below
//! is the command the user could have typed, because `cli::normalize`
//! is the one parser every entry shares (watch re-enters it per
//! arriving file; the ui server re-enters it per request).

use std::path::{Path, PathBuf};

use clap::Parser as _;
use serde::Deserialize;

use crate::cli::{self, Cli, Normalized, SourceSpec};
use crate::domain::{AppError, AppResult};
use crate::input::InputEnv;
use crate::plan::{ExecutionPlan, TerminalInfo};

/// What the UI may ask a run to do — a deliberate whitelist. Delivery
/// destinations (`-o`, `--copy`, `--out-dir`) are absent on purpose: the
/// browser is the destination. Unknown fields are rejected so the
/// contract stays honest.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRequest {
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
    /// Literal text material, in order, alongside the files.
    #[serde(default)]
    pub texts: Vec<String>,
}

/// One parsed invocation: the same triple `app::dispatch` feeds
/// `run_task` with.
pub struct Invocation {
    pub cli: Cli,
    pub task_name: String,
    pub specs: Vec<SourceSpec>,
}

/// Uploaded material, landed on disk under original names: gather reads
/// files, so an upload becomes an ordinary file input (typed by its
/// content, like every aido input). The directory lives until the plan
/// is built — gather has read the bytes by then.
pub struct Landed {
    dir: PathBuf,
}

impl Drop for Landed {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub fn land_uploads(files: Vec<(String, Vec<u8>)>) -> AppResult<(Landed, Vec<PathBuf>)> {
    let dir = std::env::temp_dir().join(format!(
        "aido-ui-upload-{}-{}",
        std::process::id(),
        crate::history::stamp_now()
    ));
    std::fs::create_dir_all(&dir)
        .map_err(|e| AppError::usage(format!("cannot land the uploaded files: {e}")))?;
    let mut paths = Vec::new();
    for (index, (name, bytes)) in files.into_iter().enumerate() {
        // Only the final component survives: a browser may send a whole
        // path, and the name chooses prompts and artifact stems.
        let name = Path::new(&name)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| format!("upload-{}", index + 1));
        let path = dir.join(&name);
        std::fs::write(&path, bytes)
            .map_err(|e| AppError::usage(format!("cannot land {name}: {e}")))?;
        paths.push(path);
    }
    Ok((Landed { dir }, paths))
}

/// The argv a user could have typed: task first, flags (combined
/// `--flag=value` form, so values that start with '-' stay values),
/// then the material in order.
fn argv_for(request: &RunRequest, files: &[PathBuf]) -> Vec<std::ffi::OsString> {
    let mut argv: Vec<std::ffi::OsString> = vec![request.task.clone().into()];
    let flag = |argv: &mut Vec<std::ffi::OsString>, name: &str, value: &str| {
        argv.push(format!("--{name}={value}").into());
    };
    if let Some(value) = &request.prompt {
        flag(&mut argv, "prompt", value);
    }
    if let Some(value) = &request.profile {
        flag(&mut argv, "profile", value);
    }
    if let Some(value) = &request.model {
        flag(&mut argv, "model", value);
    }
    if let Some(value) = &request.to {
        flag(&mut argv, "to", value);
    }
    if let Some(value) = &request.voice {
        flag(&mut argv, "voice", value);
    }
    if let Some(value) = request.speed {
        flag(&mut argv, "speed", &value.to_string());
    }
    if let Some(value) = request.count {
        flag(&mut argv, "count", &value.to_string());
    }
    if let Some(value) = &request.size {
        flag(&mut argv, "size", value);
    }
    if request.no_split {
        argv.push("--no-split".into());
    }
    if let Some(value) = request.timeout_secs {
        flag(&mut argv, "timeout", &value.to_string());
    }
    if let Some(value) = request.total_timeout_secs {
        flag(&mut argv, "total-timeout", &value.to_string());
    }
    for text in &request.texts {
        argv.push("--text".into());
        argv.push(text.into());
    }
    for file in files {
        argv.push(file.as_os_str().to_os_string());
    }
    argv
}

pub fn parse(request: &RunRequest, files: &[PathBuf]) -> AppResult<Invocation> {
    let argv = argv_for(request, files);
    let normalized = cli::normalize(argv).map_err(|e| AppError::usage(format!("{e}")))?;
    let Normalized::Single { task, specs, argv } = &normalized else {
        return Err(AppError::usage(
            "the web UI runs one task at a time (chains arrive in a later version)",
        ));
    };
    let cli =
        Cli::try_parse_from(std::iter::once(std::ffi::OsString::from("aido")).chain(argv.clone()))
            .map_err(|e| AppError::usage(e.to_string()))?;
    let task_name = task
        .clone()
        .or_else(|| cli.task.clone())
        .ok_or_else(|| AppError::usage("name a task (see the task picker)"))?;
    Ok(Invocation {
        cli,
        task_name,
        specs: specs.clone(),
    })
}

/// Build the plan with the server's environment: no terminal, no stdin,
/// no clipboard. Every plan is buffered (a non-tty stdout forces it), no
/// spinner starts, nothing reaches the server's own stdio — the SSE
/// stream is the UI's terminal. Validation is the CLI's own: a bad
/// combination fails here, in the response, with the same message the
/// terminal would print.
pub fn build_plan(invocation: &Invocation) -> AppResult<ExecutionPlan> {
    let Invocation {
        cli,
        task_name,
        specs,
    } = invocation;
    crate::app::require_ask_prompt(cli, task_name)?;
    let task = crate::tasks::get(task_name).map_err(|e| AppError::usage(format!("{e:#}")))?;
    let cfg = crate::config::load().map_err(|e| AppError::usage(format!("{e:#}")))?;
    let terminal = TerminalInfo {
        stdin: false,
        stdout: false,
        stderr: false,
    };
    let mut empty = std::io::empty();
    let mut probe = || false;
    let mut no_clipboard = || {
        Err(anyhow::anyhow!(
            "the clipboard is not available to the web UI; paste it as a file"
        ))
    };
    let mut env = InputEnv::custom(&mut empty, &mut probe, &mut no_clipboard);
    crate::plan::build(cli, &task, specs, &cfg, terminal, &mut env)
}

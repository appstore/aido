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
/// destinations are narrowed, not open: `out_dir`/`out_file` name a
/// component under aido's dedicated deliveries directory (see
/// [`delivery_path`]), never a free-form path — the browser stays the
/// default destination, and the server's disk is only ever touched
/// inside that one root. Unknown fields are rejected so the contract
/// stays honest.
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
    /// `--out-dir` as a name under the deliveries directory.
    pub out_dir: Option<String>,
    /// `-o` as a file name under the deliveries directory.
    pub out_file: Option<String>,
    /// Literal text material, in order, alongside the files.
    #[serde(default)]
    pub texts: Vec<String>,
}

/// The one directory server-side deliveries may land in: loopback UI or
/// not, a browser request must never choose where files are written.
pub fn deliveries_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("AIDO_DELIVERY_DIR") {
        if !dir.trim().is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::data_local_dir().map(|d| d.join("aido").join("deliveries"))
}

/// Resolve a requested delivery name into its one true path under
/// [`deliveries_dir`]: a single path component (no separators, no `..`,
/// no leading dot), so the request cannot escape the root. The name
/// lands on disk verbatim — it is the file's name, not an id to
/// sanitize.
fn delivery_path(field: &str, name: &str) -> AppResult<PathBuf> {
    let name = name.trim();
    let bad = |why: &str| {
        AppError::usage(format!(
            "'{name}' is not a usable {field} name ({why}); it names a file under \
             the deliveries directory"
        ))
    };
    if name.is_empty() {
        return Err(bad("empty"));
    }
    if name.len() > 128 {
        return Err(bad("at most 128 bytes"));
    }
    if name.contains(['/', '\\']) || name == ".." || name.starts_with('.') || name.contains('\0') {
        return Err(bad("one path component, not starting with '.'"));
    }
    let Some(root) = deliveries_dir() else {
        return Err(AppError::usage(
            "cannot determine the deliveries directory on this platform",
        ));
    };
    Ok(root.join(name))
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
    // A process-global counter makes the directory exclusive by
    // construction: two requests in the same millisecond must never
    // share one (the first finisher's cleanup would delete the other's
    // files mid-gather) — the same collision discipline new_run_id
    // applies to history ids.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "aido-ui-upload-{}-{}-{n}",
        std::process::id(),
        crate::history::stamp_now()
    ));
    std::fs::create_dir(&dir)
        .map_err(|e| AppError::usage(format!("cannot land the uploaded files: {e}")))?;
    let mut paths = Vec::new();
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (index, (name, bytes)) in files.into_iter().enumerate() {
        // Only the final component survives: a browser may send a whole
        // path, and the name chooses prompts and artifact stems. A
        // repeated name gets a `-2`/`-3` suffix — a silent overwrite
        // would drop one upload's content while both specs point at the
        // same path (the batch naming's own dedup rule).
        let original = Path::new(&name)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| format!("upload-{}", index + 1));
        let mut name = original.clone();
        let mut nth = 1;
        while !used.insert(name.clone()) {
            nth += 1;
            name = match original.rsplit_once('.') {
                Some((stem, ext)) => format!("{stem}-{nth}.{ext}"),
                None => format!("{original}-{nth}"),
            };
        }
        let path = dir.join(&name);
        std::fs::write(&path, bytes)
            .map_err(|e| AppError::usage(format!("cannot land {name}: {e}")))?;
        paths.push(path);
    }
    Ok((Landed { dir }, paths))
}

/// The argv a user could have typed: task first, flags (combined
/// `--flag=value` form, so values that start with '-' stay values),
/// then the material in order. Delivery names resolve to their true
/// paths here — one place, so the preview and the run agree.
fn argv_for(request: &RunRequest, files: &[PathBuf]) -> AppResult<Vec<std::ffi::OsString>> {
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
    if let Some(name) = &request.out_dir {
        let path = delivery_path("out_dir", name)?;
        flag(&mut argv, "out-dir", &path.to_string_lossy());
    }
    if let Some(name) = &request.out_file {
        let path = delivery_path("out_file", name)?;
        flag(&mut argv, "output", &path.to_string_lossy());
    }
    for text in &request.texts {
        argv.push("--text".into());
        argv.push(text.into());
    }
    for file in files {
        argv.push(file.as_os_str().to_os_string());
    }
    Ok(argv)
}

/// The task flags of a request as plain argv, without material and
/// without delivery resolution — the watch dashboard composes its own
/// task invocation from the same whitelist.
pub(super) fn argv_for_task(request: &RunRequest) -> AppResult<Vec<std::ffi::OsString>> {
    argv_for(request, &[])
}

pub fn parse(request: &RunRequest, files: &[PathBuf]) -> AppResult<Invocation> {
    check_task_name(&request.task)?;
    let argv = argv_for(request, files)?;
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

/// A task name that starts with '-' would be eaten as a flag by the
/// normalizer and surface as a bizarre ask run; refuse it with the CLI's
/// own unknown-task wording instead (an empty name is the same problem).
pub(super) fn check_task_name(name: &str) -> AppResult<()> {
    if name.trim().is_empty() {
        return Err(AppError::usage("name a task (see the task picker)"));
    }
    if name.starts_with('-') {
        return Err(AppError::usage(format!("unknown task '{name}'")));
    }
    Ok(())
}

/// Build the plan with the server's environment: no terminal, no stdin,
/// no clipboard — and quiet, so the spinner's tty check cannot draw on
/// the SERVER's terminal (aido ui is itself started from one) and the
/// gather notes stay off stderr. Progress reaches the UI through the
/// event sink, which quiet does not gate. Every plan is buffered (a
/// non-tty stdout forces it); the SSE stream is the UI's terminal.
/// Validation is the CLI's own: a bad combination fails here, in the
/// response, with the same message the terminal would print.
pub fn build_plan(invocation: &Invocation) -> AppResult<ExecutionPlan> {
    let Invocation {
        cli,
        task_name,
        specs,
    } = invocation;
    let mut cli = cli.clone();
    cli.quiet = true;
    crate::app::require_ask_prompt(&cli, task_name)?;
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
    let plan = match crate::plan::build(&cli, &task, specs, &cfg, terminal, &mut env) {
        Ok(plan) => Ok(plan),
        // A per-part batch hard-requires `--out-dir`, which the UI's
        // request whitelist deliberately does not offer — the browser is
        // the destination and the artifacts live in history. The check
        // only wants the flag present: the UI path never delivers, so a
        // placeholder directory (never created, never written) satisfies
        // it and "一次拖两张图" works exactly like the CLI's grid.
        Err(e) if e.message.contains("use --out-dir to collect") => {
            let mut cli = cli.clone();
            cli.out_dir = Some(
                std::env::temp_dir().join(format!("aido-ui-batch-{}", crate::history::stamp_now())),
            );
            crate::plan::build(&cli, &task, specs, &cfg, terminal, &mut env)
        }
        Err(e) => Err(e),
    }?;
    // An `-o` target that already exists is knowable here (the plan
    // names the file exactly); it fails as usage in the response, the
    // same message and exit-2 semantics a CLI run would print.
    crate::output::precheck_file_targets(&plan.destinations, cli.overwrite)?;
    Ok(plan)
}

/// One chain stage as the UI sends it — the same whitelist as a single
/// run, minus material: only stage 1 takes material (the chain's own
/// files and texts), every later stage reads the junction.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageRequest {
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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainRequest {
    pub stages: Vec<StageRequest>,
    #[serde(default)]
    pub texts: Vec<String>,
}

impl StageRequest {
    /// Reuse the single-run argv builder: a stage IS one task invocation
    /// with its own flags; only material differs (stage 1's comes from
    /// the chain).
    fn as_run_request(&self, texts: Vec<String>) -> RunRequest {
        RunRequest {
            task: self.task.clone(),
            prompt: self.prompt.clone(),
            profile: self.profile.clone(),
            model: self.model.clone(),
            to: self.to.clone(),
            voice: self.voice.clone(),
            speed: self.speed,
            count: self.count,
            size: self.size.clone(),
            no_split: self.no_split,
            timeout_secs: self.timeout_secs,
            total_timeout_secs: self.total_timeout_secs,
            // Chains deliver nothing server-side in v1: the last stage
            // hands the browser its artifacts through history.
            out_dir: None,
            out_file: None,
            texts,
        }
    }
}

/// The `--then` form of the chain — plain shell tokens, no spec-string
/// quoting to get wrong (the chain sugar's grammar has no escapes; a
/// prompt with quotes in it would be unbuildable there).
fn chain_argv(request: &ChainRequest, files: &[PathBuf]) -> AppResult<Vec<std::ffi::OsString>> {
    if request.stages.len() < 2 {
        return Err(AppError::usage(
            "a chain needs at least two stages (add one with the ＋ button)",
        ));
    }
    let mut argv: Vec<std::ffi::OsString> = Vec::new();
    for (index, stage) in request.stages.iter().enumerate() {
        if index > 0 {
            argv.push("--then".into());
        }
        // Stage 1 carries the chain's material; texts land in its
        // segment so they join the files in order.
        let texts = if index == 0 {
            request.texts.clone()
        } else {
            Vec::new()
        };
        let stage_files: &[PathBuf] = if index == 0 { files } else { &[] };
        argv.extend(argv_for(&stage.as_run_request(texts), stage_files)?);
    }
    Ok(argv)
}

/// A chain ready to plan or run, plus whether history will record it —
/// the same judgment `run_chain` makes from the merged run surface.
pub struct ChainPrepared {
    pub chain: crate::chain::PreparedChain,
    pub record_history: bool,
}

/// Parse and prepare the chain through the CLI's own path: normalize
/// splits the `--then` argv into stages, `parse_syntax` applies the
/// stage rules, `prepare` resolves routes and runs the junction type
/// checks — zero requests, so every misuse answers here, in the
/// response, with the message the terminal would print.
pub fn parse_chain(request: &ChainRequest, files: &[PathBuf]) -> AppResult<ChainPrepared> {
    for (index, stage) in request.stages.iter().enumerate() {
        if let Err(e) = check_task_name(&stage.task) {
            return Err(AppError::usage(format!(
                "stage {}: {}",
                index + 1,
                e.message
            )));
        }
    }
    let argv = chain_argv(request, files)?;
    let normalized = cli::normalize(argv).map_err(|e| AppError::usage(format!("{e}")))?;
    let Normalized::Chain { stages } = normalized else {
        return Err(AppError::usage(
            "a chain needs at least two stages joined by --then",
        ));
    };
    let syntax = crate::chain::parse_syntax(stages).map_err(|e| match e {
        crate::chain::ChainParseError::Usage(e) => e,
        // The UI never asks a stage for --help; reaching for it is a
        // bug, not a display request.
        crate::chain::ChainParseError::Display(_) => {
            AppError::usage("a chain stage asked for --help, which the web UI never sends")
        }
    })?;
    let cfg = crate::config::load().map_err(|e| AppError::usage(format!("{e:#}")))?;
    let chain = crate::chain::prepare(syntax, &cfg)?;
    let record_history = !chain.run_cli.no_history
        && cfg
            .settings
            .history_keep
            .unwrap_or(crate::history::DEFAULT_KEEP)
            > 0;
    Ok(ChainPrepared {
        chain,
        record_history,
    })
}

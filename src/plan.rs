//! The execution plan: one immutable object describing everything a run
//! will do, built and validated before any request is sent.
//!
//! Validation order (contract §step 4): task parameters → input types and
//! counts → adapter capability → model capability → artifact types and
//! counts → encodings → output destinations. `--dry-run` prints the plan
//! (sanitized) and exits without touching network, history or clipboard.

use crate::cli::{Cli, OutputFormat, SourceSpec};
use crate::config::resolve::{self, ParamSource, Resolved};
use crate::config::Config;
use crate::domain::{AppError, AppResult, Destination, InputPart, MediaKind, RunSummary};
use crate::input::{self, InputEnv};
use crate::processors::{self, RequestStep};
use crate::tasks::{ProcessorKind, Task};
use std::time::Duration;

/// Terminal-ness injected so plans are testable without a tty.
#[derive(Debug, Clone, Copy)]
pub struct TerminalInfo {
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
}

impl TerminalInfo {
    pub fn real() -> Self {
        use std::io::IsTerminal;
        Self {
            stdin: std::io::stdin().is_terminal(),
            stdout: std::io::stdout().is_terminal(),
            stderr: std::io::stderr().is_terminal(),
        }
    }
}

/// How the reply reaches stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    /// Print deltas as they arrive.
    Live,
    /// Collect and print once complete.
    Buffered,
}

#[derive(Debug)]
pub struct ExecutionPlan {
    pub task: Task,
    pub resolved: Resolved,
    /// The task's fixed instruction (already includes typed param effects
    /// like --to).
    pub instruction: String,
    /// This run's -p requirement, kept separate from the instruction.
    pub requirement: Option<String>,
    pub inputs: Vec<InputPart>,
    pub steps: Vec<RequestStep>,
    /// The processing strategy actually selected for this run (the task's
    /// choice, overridden by `--no-split`).
    pub processor: ProcessorKind,
    pub format: Option<OutputFormat>,
    pub destinations: Vec<Destination>,
    pub overwrite: bool,
    /// Expected artifact counts per kind once the service answers.
    pub expected_counts: Vec<(MediaKind, Option<u64>)>,
    pub transport_stream: bool,
    pub delivery: DeliveryMode,
    pub timeout: Duration,
    pub total_timeout: Option<Duration>,
    pub record_history: bool,
    pub quiet: bool,
    pub json: bool,
    /// Where each merged parameter came from, for dry-run.
    pub param_sources: Vec<(String, String, ParamSource)>,
    pub credentials_available: Option<bool>,
    pub terminal: TerminalInfo,
}

/// Old `-o` values that meant modes, not files.
const OLD_OUTPUT_MODES: &[(&str, &str)] = &[
    ("clipboard", "use `--copy`"),
    ("both", "use `--copy --stdout`"),
    (
        "stdout",
        "use `--stdout` (or omit it: stdout is the default)",
    ),
];

pub fn build(
    cli: &Cli,
    task: &Task,
    specs: &[SourceSpec],
    cfg: &Config,
    terminal: TerminalInfo,
    env: &mut InputEnv<'_>,
) -> AppResult<ExecutionPlan> {
    // --- task parameters -------------------------------------------------
    validate_task_params(cli, task)?;
    let mut resolved =
        resolve::resolve(cli, cfg, task).map_err(|e| AppError::usage(format!("{e:#}")))?;
    apply_param_options(cli, task, &mut resolved)?;
    let instruction = compose_instruction(cli, task)?;
    let requirement = cli.prompt.clone().filter(|p| !p.trim().is_empty());

    // --- inputs -----------------------------------------------------------
    let inputs = input::gather(
        specs,
        task.requires_material,
        cfg.settings.input_bytes,
        cli.dry_run,
        env,
    )
    .map_err(|e| AppError::usage(e.to_string()))?;
    validate_inputs(task, &resolved, &inputs)?;
    // Adapter capability: the edge-tts protocol has no instruction channel.
    // Refusing at plan time (not just at send time) keeps --dry-run honest
    // about a plan that could never execute.
    if resolved.adapter == crate::api::Adapter::EdgeTts {
        let from_task = !instruction.trim().is_empty();
        let from_prompt = requirement.as_deref().is_some_and(|p| !p.trim().is_empty());
        if from_task || from_prompt {
            let source = match (from_task, from_prompt) {
                (true, true) => "the task's fixed instruction and -p",
                (true, false) => "the task's fixed instruction",
                _ => "-p",
            };
            return Err(AppError::usage(format!(
                "{}; {} would have nowhere to go — drop it, or use a provider \
                 whose speech route has one",
                crate::api::EDGE_NO_INSTRUCTION_CHANNEL,
                source
            )));
        }
    }
    let processor = select_processor(cli, task);
    let steps = processors::plan_steps(&inputs, processor, cli.quiet)
        .map_err(|e| AppError::usage(e.to_string()))?;

    // --- artifacts and encodings -------------------------------------------
    validate_outputs(cli, &mut resolved, &steps, terminal)?;

    // --- destinations -------------------------------------------------------
    let destinations = resolve_destinations(cli, &resolved.produce, terminal)?;

    // --- transport and limits ------------------------------------------------
    // `--stream` demands a streaming adapter and is refused otherwise;
    // `--no-stream` forces buffering and is always valid — on an adapter
    // that never streams it is simply a no-op.
    if cli.stream && !resolved.adapter.streams() {
        return Err(AppError::usage(format!(
            "adapter '{}' does not support --stream; use --no-stream or omit the flag",
            resolved.adapter
        )));
    }
    let transport_stream = if cli.no_stream {
        false
    } else {
        resolved.adapter.streams()
    };
    let stdout_text =
        destinations.contains(&Destination::Stdout) && resolved.produce == [MediaKind::Text];
    let delivery = if cli.stream {
        DeliveryMode::Live
    } else if cli.no_stream || !stdout_text || cli.json || cfg.settings.stream == Some(false) {
        DeliveryMode::Buffered
    } else if terminal.stdout {
        DeliveryMode::Live
    } else {
        DeliveryMode::Buffered
    };

    let timeout = Duration::from_secs(cli.timeout.or(cfg.settings.timeout_secs).unwrap_or(120));
    let total_timeout = cli
        .total_timeout
        .or(cfg.settings.total_timeout_secs)
        .map(Duration::from_secs);

    // Expected artifact counts: image generation honors --count.
    let mut expected_counts = Vec::new();
    for kind in &resolved.produce {
        let n = match kind {
            MediaKind::Image => resolved.options.get("n").and_then(|v| v.as_u64()),
            _ => None,
        };
        expected_counts.push((*kind, n));
    }

    // Credential reference only: never the value. Adapters that own their
    // endpoint (edge-tts) take no credentials, so their plans report
    // "none required" instead of pointing at a key that is never sent.
    let credentials_available = if resolved.adapter == crate::api::Adapter::EdgeTts {
        None
    } else {
        resolved.api_key_env.as_ref().map(|name| {
            std::env::var(name)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
        })
    };

    let param_sources = describe_param_sources(cli, task, &resolved);

    Ok(ExecutionPlan {
        task: task.clone(),
        resolved,
        instruction,
        requirement,
        inputs,
        steps,
        processor,
        format: cli.format,
        destinations,
        overwrite: cli.overwrite,
        expected_counts,
        transport_stream,
        delivery,
        timeout,
        total_timeout,
        record_history: !cli.no_history && cfg.settings.history_keep.unwrap_or(50) > 0,
        quiet: cli.quiet,
        json: cli.json,
        param_sources,
        credentials_available,
        terminal,
    })
}

fn validate_task_params(cli: &Cli, task: &Task) -> AppResult<()> {
    let given: Vec<(&str, bool)> = vec![
        ("to", cli.to.is_some()),
        ("voice", cli.voice.is_some()),
        ("speed", cli.speed.is_some()),
        ("count", cli.count.is_some()),
        ("size", cli.size.is_some()),
    ];
    for (name, present) in given {
        if present && !task.accepts_param(name) {
            return Err(AppError::usage(format!(
                "task '{}' does not accept --{name} (its parameters: {})",
                task.name,
                if task.params.is_empty() {
                    "none".into()
                } else {
                    task.params
                        .iter()
                        .map(|p| format!("--{}", p.name()))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            )));
        }
    }
    if let Some(speed) = cli.speed {
        if !(0.25..=4.0).contains(&speed) {
            return Err(AppError::usage("--speed must be between 0.25 and 4"));
        }
    }
    if let Some(count) = cli.count {
        if !(1..=10).contains(&count) {
            return Err(AppError::usage("--count must be between 1 and 10"));
        }
    }
    if let Some(size) = &cli.size {
        let valid = [
            "auto",
            "1024x1024",
            "1536x1024",
            "1024x1536",
            "1792x1024",
            "1024x1792",
        ];
        if !valid.contains(&size.as_str()) {
            return Err(AppError::usage(format!(
                "--size must be one of: {}",
                valid.join(", ")
            )));
        }
    }
    if let Some(to) = &cli.to {
        if to.trim().is_empty() {
            return Err(AppError::usage("--to must name a language (or \"auto\")"));
        }
    }
    Ok(())
}

/// Typed parameters become adapter options (or instruction suffixes).
fn apply_param_options(cli: &Cli, task: &Task, resolved: &mut Resolved) -> AppResult<()> {
    let mut set = |key: &str, value: serde_json::Value| {
        resolved.options.insert(key.to_string(), value);
    };
    if task.accepts_param("voice") {
        if let Some(voice) = &cli.voice {
            set("voice", serde_json::Value::String(voice.clone()));
        } else if let Some(default) = task.default_param("voice") {
            set("voice", default.clone());
        }
    }
    if task.accepts_param("speed") {
        if let Some(speed) = cli.speed {
            set("speed", serde_json::json!(speed));
        }
    }
    if task.accepts_param("count") {
        if let Some(count) = cli.count {
            set("n", serde_json::json!(count));
        }
    }
    if task.accepts_param("size") {
        if let Some(size) = &cli.size {
            set("size", serde_json::Value::String(size.clone()));
        }
    }
    resolved
        .adapter
        .validate_options(&resolved.options)
        .map_err(AppError::from)?;
    Ok(())
}

/// The fixed instruction with typed parameter effects folded in.
fn compose_instruction(cli: &Cli, task: &Task) -> AppResult<String> {
    let mut instruction = task.instruction.trim().to_string();
    if task.accepts_param("to") {
        let to = cli
            .to
            .clone()
            .or_else(|| {
                task.default_param("to")
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
            })
            .unwrap_or_else(|| "auto".to_string());
        let target = if to.eq_ignore_ascii_case("auto") {
            "Simplified Chinese if the text is English; otherwise English".to_string()
        } else {
            to
        };
        if !instruction.is_empty() {
            instruction.push_str("\n\n");
        }
        instruction.push_str(&format!("Target language: {target}."));
    }
    Ok(instruction)
}

fn validate_inputs(task: &Task, resolved: &Resolved, inputs: &[InputPart]) -> AppResult<()> {
    if !task.requires_material && inputs.is_empty() {
        return Ok(());
    }
    if inputs.is_empty() {
        return Err(AppError::usage(format!(
            "task '{}' requires material; give files, `-` (stdin), --text or --paste",
            task.name
        )));
    }
    if let Some(max) = task.max_inputs {
        if inputs.len() > max {
            return Err(AppError::usage(format!(
                "task '{}' accepts at most {max} input(s), got {}",
                task.name,
                inputs.len()
            )));
        }
    }
    for part in inputs {
        if let Some(allowed) = &resolved.allowed_inputs {
            if !allowed.contains(&part.kind) {
                return Err(AppError::usage(format!(
                    "input '{}' is {}, which this task/profile does not accept \
                     (allowed: {})",
                    part.name,
                    part.kind,
                    allowed
                        .iter()
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        } else if !resolved.adapter.inputs().contains(&part.kind) {
            return Err(AppError::usage(format!(
                "adapter '{}' does not accept input type '{}' (input '{}')",
                resolved.adapter, part.kind, part.name
            )));
        }
    }
    for required in &task.required_types {
        if !inputs.iter().any(|p| p.kind == *required) {
            return Err(AppError::usage(format!(
                "task '{}' requires {required} input; none of the material is {required}",
                task.name
            )));
        }
    }
    Ok(())
}

fn select_processor(cli: &Cli, task: &Task) -> ProcessorKind {
    if cli.no_split {
        ProcessorKind::Single
    } else {
        task.processor
    }
}

fn validate_outputs(
    cli: &Cli,
    resolved: &mut Resolved,
    steps: &[RequestStep],
    terminal: TerminalInfo,
) -> AppResult<()> {
    let media_kinds: Vec<MediaKind> = resolved
        .produce
        .iter()
        .copied()
        .filter(|k| *k != MediaKind::Text)
        .collect();
    // One --format cannot name encodings for several media kinds.
    if cli.format.is_some() && media_kinds.len() > 1 {
        return Err(AppError::usage(
            "--format is ambiguous when producing several media kinds; configure \
             each kind's encoding in the task or provider options",
        ));
    }
    if let Some(format) = cli.format {
        let ok = match resolved.produce.as_slice() {
            [MediaKind::Image] => matches!(
                format,
                OutputFormat::Png | OutputFormat::Jpeg | OutputFormat::Webp
            ),
            [MediaKind::Audio] => matches!(
                format,
                OutputFormat::Mp3
                    | OutputFormat::Opus
                    | OutputFormat::Aac
                    | OutputFormat::Flac
                    | OutputFormat::Wav
                    | OutputFormat::Pcm
            ),
            _ => false,
        };
        if !ok {
            return Err(AppError::usage(format!(
                "--format {format} does not match the produced type(s) [{}]",
                resolved
                    .produce
                    .iter()
                    .map(|k| k.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            )));
        }
        resolved.options.insert(
            "format".into(),
            serde_json::Value::String(format.to_string()),
        );
    }
    // Media output needs somewhere to go.
    if !media_kinds.is_empty() {
        let has_file_target = cli.output.is_some() || cli.out_dir.is_some();
        if !has_file_target && terminal.stdout {
            return Err(AppError::usage(
                "binary output needs -o FILE or --out-dir, or a stdout pipe",
            ));
        }
    }
    if resolved.produce.len() > 1 && cli.output.is_some() {
        return Err(AppError::usage(
            "several output kinds cannot go to a single -o FILE; use --out-dir",
        ));
    }
    // Multiple images from one request need a directory.
    if resolved
        .options
        .get("n")
        .and_then(|v| v.as_u64())
        .is_some_and(|n| n > 1)
        && cli.out_dir.is_none()
    {
        return Err(AppError::usage("multiple images require --out-dir"));
    }
    // A media file's extension states its encoding; a mismatch with the
    // requested encoding is a usage error, before any request.
    if let Some(path) = &cli.output {
        if let Some(extension) = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
        {
            for kind in &media_kinds {
                let requested = resolved
                    .options
                    .get("format")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| cli.format.map(|f| f.to_string()))
                    .unwrap_or_else(|| match kind {
                        MediaKind::Audio => "mp3".to_string(),
                        _ => "png".to_string(),
                    });
                let compatible = extension == requested
                    || (extension == "jpg" && requested == "jpeg")
                    || (extension == "ogg" && requested == "opus");
                if !compatible {
                    return Err(AppError::usage(format!(
                        "output encoding is '{requested}', but the file is named '.{extension}'; \
                         pass --format to match or rename the target"
                    )));
                }
            }
        }
    }
    // Slice runs produce one text artifact from several requests: that is
    // fine, the merge gate handles it.
    let _ = steps;
    Ok(())
}

fn resolve_destinations(
    cli: &Cli,
    produce: &[MediaKind],
    terminal: TerminalInfo,
) -> AppResult<Vec<Destination>> {
    let mut destinations: Vec<Destination> = Vec::new();
    if let Some(path) = &cli.output {
        if path.as_os_str() == "-" {
            destinations.push(Destination::Stdout);
        } else {
            let is_bare_name = path.parent().is_none_or(|p| p.as_os_str().is_empty());
            if is_bare_name {
                if let Some(name) = path.to_str() {
                    if let Some((_, guidance)) =
                        OLD_OUTPUT_MODES.iter().find(|(mode, _)| *mode == name)
                    {
                        return Err(AppError::usage(format!(
                            "`-o {name}` used to select an output mode; {guidance} \
                             (for a file literally named '{name}', write `-o ./{name}`)"
                        )));
                    }
                }
            }
            destinations.push(Destination::File { path: path.clone() });
        }
    }
    if let Some(dir) = &cli.out_dir {
        destinations.push(Destination::Directory { path: dir.clone() });
    }
    if cli.stdout && !destinations.contains(&Destination::Stdout) {
        destinations.push(Destination::Stdout);
    }
    if cli.json {
        // --json is itself the explicit stdout target.
        if destinations.contains(&Destination::Stdout) {
            return Err(AppError::usage(
                "--json cannot combine with --stdout or `-o -` (two stdout contracts)",
            ));
        }
        destinations.insert(0, Destination::Stdout);
    }
    if cli.copy {
        if produce.contains(&MediaKind::Audio) {
            return Err(AppError::usage(
                "audio cannot go to the clipboard; use -o FILE or pipe stdout",
            ));
        }
        if produce.len() > 1 {
            return Err(AppError::usage(
                "the clipboard takes one artifact; use --out-dir for mixed output",
            ));
        }
        destinations.push(Destination::Clipboard);
    }
    if destinations.is_empty() {
        // Default destination: stdout.
        destinations.push(Destination::Stdout);
    }
    let _ = terminal;
    Ok(destinations)
}

fn describe_param_sources(
    cli: &Cli,
    task: &Task,
    resolved: &Resolved,
) -> Vec<(String, String, ParamSource)> {
    let mut out = Vec::new();
    out.push((
        "model".into(),
        resolved.model.clone(),
        resolved.model_source,
    ));
    if let Some(max) = cli.max_tokens {
        out.push(("max_tokens".into(), max.to_string(), ParamSource::Cli));
    }
    if let Some(to) = &cli.to {
        out.push(("to".into(), to.clone(), ParamSource::Cli));
    } else if task.accepts_param("to") {
        out.push((
            "to".into(),
            task.default_param("to")
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| "auto".into()),
            ParamSource::Task,
        ));
    }
    out
}

/// Render the sanitized plan for `--dry-run` and debugging.
pub fn describe(plan: &ExecutionPlan) -> String {
    let mut out = String::new();
    let r = &plan.resolved;
    out.push_str(&format!(
        "task:        {} {} (operation {})\n",
        plan.task.name,
        if plan.task.builtin {
            "(builtin)"
        } else {
            "(user)"
        },
        plan.task.operation
    ));
    out.push_str(&format!("profile:     {}\n", r.profile_name));
    let shown_url = r
        .base_url
        .as_deref()
        .map(redact_url)
        .unwrap_or_else(|| "(endpoint owned by the edge-tts adapter)".to_string());
    out.push_str(&format!(
        "provider:    {} → {} (route: {})\n",
        r.provider_name, shown_url, r.adapter
    ));
    out.push_str(&format!("model:       {}\n", r.model));
    if !plan.instruction.is_empty() {
        out.push_str(&format!("instruction: {}\n", first_line(&plan.instruction)));
    }
    if let Some(req) = &plan.requirement {
        out.push_str(&format!("requirement: {}\n", first_line(req)));
    }
    out.push_str("material:\n");
    if plan.inputs.is_empty() {
        out.push_str("  (none — the instruction alone drives this run)\n");
    }
    for part in &plan.inputs {
        out.push_str(&format!(
            "  {}. {}  {}  {}  [{}]\n",
            part.id + 1,
            part.name,
            part.kind,
            part.source,
            human_bytes(match &part.content {
                crate::domain::InputContent::Text(s) => s.len() as u64,
                crate::domain::InputContent::Media(b) => b.len() as u64,
            })
        ));
    }
    out.push_str(&format!(
        "processing:  {}\n",
        match plan.processor {
            ProcessorKind::Single => "single request".to_string(),
            ProcessorKind::OcrTiles => {
                let mut parts = Vec::new();
                for step in &plan.steps {
                    if step.label != "all material" {
                        parts.push(step.label.clone());
                    }
                }
                if parts.is_empty() {
                    "ocr-tiles (no image needs slicing)".to_string()
                } else {
                    format!("ocr-tiles — {}", parts.join("; "))
                }
            }
        }
    ));
    out.push_str(&format!(
        "produce:     {}\n",
        plan.resolved
            .produce
            .iter()
            .map(|k| k.to_string())
            .collect::<Vec<_>>()
            .join(",")
    ));
    if let Some(format) = plan.format {
        out.push_str(&format!("format:      {format}\n"));
    }
    out.push_str("parameters:\n");
    for (name, value, source) in &plan.param_sources {
        out.push_str(&format!("  {name} = {value}  ({})\n", source.as_str()));
    }
    out.push_str(&format!(
        "transport:   {} (delivery: {})\n",
        if plan.transport_stream {
            "streaming"
        } else {
            "buffered"
        },
        match plan.delivery {
            DeliveryMode::Live => "live",
            DeliveryMode::Buffered => "buffered",
        }
    ));
    out.push_str("destinations:\n");
    for d in &plan.destinations {
        out.push_str(&format!("  - {d}\n"));
    }
    match plan.credentials_available {
        Some(true) => out.push_str(&format!(
            "credentials: {} is set\n",
            r.api_key_env.as_deref().unwrap_or("AIDO_API_KEY")
        )),
        Some(false) => out.push_str(&format!(
            "credentials: {} is NOT set — the request would fail\n",
            r.api_key_env.as_deref().unwrap_or("AIDO_API_KEY")
        )),
        None => out.push_str("credentials: none required\n"),
    }
    out.push_str("history:     no request is sent, nothing is recorded\n");
    out
}

fn first_line(s: &str) -> String {
    let line = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if line.chars().count() > 72 {
        format!("{}...", line.chars().take(72).collect::<String>())
    } else {
        line.to_string()
    }
}

fn human_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

/// Strip potentially sensitive query values from URLs shown in reports.
fn redact_url(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut u) => {
            let redacted: Vec<(String, String)> = u
                .query_pairs()
                .map(|(k, _)| (k.to_string(), "…".to_string()))
                .collect();
            if redacted.is_empty() {
                u.set_query(None);
            } else {
                let pairs: Vec<String> = redacted
                    .into_iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                u.set_query(Some(&pairs.join("&")));
            }
            u.into()
        }
        Err(_) => url.to_string(),
    }
}

/// Summary for the run record, derived from the plan.
pub fn summarize(plan: &ExecutionPlan) -> RunSummary {
    RunSummary {
        task: Some(plan.task.name.clone()),
        profile: Some(plan.resolved.profile_name.clone()),
        provider: Some(plan.resolved.provider_name.clone()),
        model: Some(plan.resolved.model.clone()),
        adapter: Some(plan.resolved.adapter.to_string()),
        inputs: plan
            .inputs
            .iter()
            .map(|p| crate::domain::InputSummary {
                name: p.name.clone(),
                kind: p.kind,
                source: p.source.to_string(),
                bytes: match &p.content {
                    crate::domain::InputContent::Text(s) => s.len() as u64,
                    crate::domain::InputContent::Media(b) => b.len() as u64,
                },
            })
            .collect(),
        processor: Some(
            match plan.processor {
                ProcessorKind::Single => "single",
                ProcessorKind::OcrTiles => "ocr-tiles",
            }
            .to_string(),
        ),
    }
}

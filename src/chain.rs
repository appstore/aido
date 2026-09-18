//! Task chains: one run, several ordered stages — the `--then` primitive
//! and the `chain "a | b"` sugar, which [`crate::cli`] reduces to the same
//! stage list.
//!
//! This module turns per-stage argv into real plans and runs the
//! plan-time contract: a chain that cannot work fails here, before any
//! request is sent (exit 2, zero requests). Every junction hands off
//! exactly one text artifact, so every stage before the last must produce
//! exactly text; only the last stage owns destinations, the JSON report
//! and live stdout.

use crate::cli::{Cli, SourceSpec, StageArgv};
use crate::config::resolve::{self, Resolved};
use crate::config::Config;
use crate::domain::{
    first_line, AppError, AppResult, Artifact, Destination, GenerationStatus, InputContent,
    InputPart, InputSource, MediaKind, Provenance, RunSummary,
};
use crate::input::{self, InputEnv};
use crate::plan::{self, StageRole, TerminalInfo};
use crate::runner::{self, RunOutput};
use crate::tasks::{self, ProcessorKind, Task};
use clap::Parser as _;
use std::ffi::OsString;

/// One chain stage past parsing: the task definition, the stage's own
/// clap surface, its input specs (stage 1 only — later stages read the
/// previous stage's output) and the resolved route and parameters.
pub struct StageParsed {
    pub task: Task,
    pub cli: Cli,
    pub specs: Vec<SourceSpec>,
    pub resolved: Resolved,
}

/// A chain ready to plan or run. `run_cli` is the last stage's clap
/// surface after run-level flags merged into it: it answers `--dry-run`,
/// `--json`, `--quiet` and every delivery flag for the run as a whole.
pub struct PreparedChain {
    pub stages: Vec<StageParsed>,
    pub run_cli: Cli,
}

impl PreparedChain {
    /// The label history and reports show for the whole chain.
    pub fn task_label(&self) -> String {
        self.stages
            .iter()
            .map(|s| s.task.name.clone())
            .collect::<Vec<_>>()
            .join("|")
    }
}

/// Parse and validate every stage: clap per stage (errors prefixed with
/// the stage number), the stage-level rejections, run-level flag merging
/// and the junction type checks. Zero requests, zero side effects.
pub fn parse(stages: Vec<StageArgv>, cfg: &Config) -> AppResult<PreparedChain> {
    if stages.len() < 2 {
        return Err(AppError::usage("a chain needs at least two stages"));
    }
    let mut parsed: Vec<StageParsed> = Vec::new();
    for (i, stage) in stages.into_iter().enumerate() {
        let index = i + 1;
        let name = stage.task.clone().ok_or_else(|| {
            AppError::usage(format!(
                "chain stage {index} must name a task (its first word, as in \
                 `aido ocr shot.png --then translate`)"
            ))
        })?;
        if matches!(
            name.as_str(),
            "tasks"
                | "profiles"
                | "config"
                | "history"
                | "run"
                | "last"
                | "help"
                | "version"
                | "__hold"
                | "chain"
        ) || name == crate::cli::LAST_TASK
        {
            return Err(AppError::usage(format!("'{name}' cannot be a chain stage")));
        }
        let cli = match Cli::try_parse_from(
            std::iter::once(OsString::from("aido")).chain(stage.argv.iter().cloned()),
        ) {
            Ok(cli) => cli,
            Err(e) => {
                if e.use_stderr() {
                    return Err(AppError::usage(format!("stage {index} ({name}): {e}")));
                }
                // `--help`/`--version` inside a stage: print like clap
                // itself would and stop here — nothing else was asked for.
                let _ = e.print();
                std::process::exit(0);
            }
        };
        if i > 0 && !stage.specs.is_empty() {
            return Err(AppError::usage(format!(
                "chain stage {index} takes no input material; it reads the previous \
                 stage's output"
            )));
        }
        if name == "ask" && cli.prompt.as_deref().is_none_or(str::is_empty) {
            return Err(AppError::usage(format!(
                "stage {index} (ask) needs -p with the instruction, e.g. \
                 aido chain \"ask -p '...' | tts\""
            )));
        }
        let task = tasks::get(&name).map_err(|e| AppError::usage(format!("{e:#}")))?;
        let resolved = resolve::resolve(&cli, cfg, &task)
            .map_err(|e| AppError::usage(format!("stage {index} ({name}): {e:#}")))?;
        parsed.push(StageParsed {
            task,
            cli,
            specs: stage.specs,
            resolved,
        });
    }
    merge_run_flags(&mut parsed)?;
    validate_chain_types(&parsed)?;
    let run_cli = parsed.last().expect("n >= 2").cli.clone();
    Ok(PreparedChain {
        stages: parsed,
        run_cli,
    })
}

/// Run-level flags found on a non-last stage address the run as a whole:
/// merge them into the last stage's surface. Equal values pass; a
/// conflicting value is a usage error — one run has one destination set,
/// one report format, one time budget.
fn merge_run_flags(stages: &mut [StageParsed]) -> AppResult<()> {
    let n = stages.len();
    for k in 0..n - 1 {
        let stage = k + 1;
        let src = stages[k].cli.clone();
        let dst = &mut stages[n - 1].cli;
        merge_opt(&mut dst.output, &src.output, "--output", stage)?;
        merge_opt(&mut dst.out_dir, &src.out_dir, "--out-dir", stage)?;
        merge_opt(
            &mut dst.total_timeout,
            &src.total_timeout,
            "--total-timeout",
            stage,
        )?;
        merge_opt(&mut dst.format, &src.format, "--format", stage)?;
        if !src.produce.is_empty() {
            match dst.produce.is_empty() {
                true => dst.produce = src.produce.clone(),
                false => {
                    if dst.produce != src.produce {
                        return Err(AppError::usage(format!(
                            "run-level flag --produce is given twice with different \
                             values (stage {stage} vs the last stage)"
                        )));
                    }
                }
            }
        }
        if (src.stream && dst.no_stream) || (src.no_stream && dst.stream) {
            return Err(AppError::usage(format!(
                "--stream and --no-stream conflict across stages (stage {stage} vs \
                 the last stage); a run streams or buffers, not both"
            )));
        }
        dst.stream |= src.stream;
        dst.no_stream |= src.no_stream;
        dst.copy |= src.copy;
        dst.stdout |= src.stdout;
        dst.json |= src.json;
        dst.overwrite |= src.overwrite;
        dst.quiet |= src.quiet;
        dst.dry_run |= src.dry_run;
        dst.no_history |= src.no_history;
    }
    Ok(())
}

fn merge_opt<T: PartialEq + Clone>(
    dst: &mut Option<T>,
    src: &Option<T>,
    name: &str,
    stage: usize,
) -> AppResult<()> {
    let Some(value) = src else {
        return Ok(());
    };
    match dst {
        Some(existing) if existing != value => Err(AppError::usage(format!(
            "run-level flag {name} is given twice with different values (stage \
             {stage} vs the last stage)"
        ))),
        _ => {
            *dst = Some(value.clone());
            Ok(())
        }
    }
}

/// The junction contract, checked in order for every adjacent pair: the
/// upstream stage produces exactly one text artifact; the downstream
/// stage accepts text; its required kinds are met. A violation stops the
/// chain before any request exists.
fn validate_chain_types(stages: &[StageParsed]) -> AppResult<()> {
    for k in 0..stages.len() - 1 {
        let up = &stages[k];
        let down = &stages[k + 1];
        let junction = format!(
            "stage {} ({}) → stage {} ({})",
            k + 1,
            up.task.name,
            k + 2,
            down.task.name
        );
        if up.resolved.produce.as_slice() != [MediaKind::Text] {
            return Err(AppError::usage(format!(
                "{junction}: every stage before the last must produce exactly text \
                 to hand off one artifact, but {} produces {}",
                up.task.name,
                kinds(&up.resolved.produce)
            )));
        }
        if !plan::kind_accepted(&down.resolved, MediaKind::Text) {
            return Err(AppError::usage(format!(
                "{junction}: {} does not accept text input (allowed: {})",
                down.task.name,
                allowed_kinds(&down.resolved)
            )));
        }
        for required in &down.task.required_types {
            if !up.resolved.produce.contains(required) {
                return Err(AppError::usage(format!(
                    "{junction}: {} requires {} input; the previous stage produces {}",
                    down.task.name,
                    required,
                    kinds(&up.resolved.produce)
                )));
            }
        }
    }
    Ok(())
}

fn kinds(kinds: &[MediaKind]) -> String {
    kinds
        .iter()
        .map(|k| k.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

fn allowed_kinds(resolved: &Resolved) -> String {
    match &resolved.allowed_inputs {
        Some(list) => kinds(list),
        None => kinds(resolved.adapter.inputs()),
    }
}

fn processor_name(kind: ProcessorKind) -> &'static str {
    match kind {
        ProcessorKind::Single => "single",
        ProcessorKind::OcrTiles => "ocr-tiles",
        ProcessorKind::ChunkJoin => "chunk-join",
        ProcessorKind::ChunkReduce => "chunk-reduce",
    }
}

/// What a finished chain hands to the recorder: the last executed stage's
/// plan (the delivery and `unsatisfied` contract), the merged runner
/// output over every stage, one summary per stage, and how many trailing
/// artifacts are the final stage's deliverables.
pub struct ChainRun {
    pub plan: plan::ExecutionPlan,
    pub output: RunOutput,
    pub stage_summaries: Vec<RunSummary>,
    pub last_stage_len: usize,
}

/// Run the chain stage by stage. Stage 1 gathers its own material; every
/// later stage consumes the previous stage's single text artifact. The
/// first stage that fails ends the chain: artifacts already produced
/// travel in the returned output (history keeps them), the failure
/// carries the stage's name, and no later stage sends anything.
pub async fn execute(
    chain: &PreparedChain,
    cfg: &Config,
    terminal: TerminalInfo,
    env: &mut InputEnv<'_>,
    on_started: &mut dyn FnMut(RunSummary),
) -> AppResult<ChainRun> {
    let n = chain.stages.len();
    let mut all_artifacts: Vec<Artifact> = Vec::new();
    // The stage that just finished: the only artifacts the next junction
    // may hand off (the run collection keeps everything, the junction
    // sees one stage).
    let mut prev_artifacts: Vec<Artifact> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut summaries: Vec<RunSummary> = Vec::new();
    let mut request_offset = 0usize;
    let mut last_plan: Option<plan::ExecutionPlan> = None;
    let mut last_len = 0usize;
    // The last executed stage's streaming/step facts, for the merged
    // output: only the final stage can have streamed, and only its step
    // counts describe "the run".
    let mut last_meta = (false, 0u64, 0usize, 0usize);

    for k in 0..n {
        let stage = &chain.stages[k];
        let is_last = k + 1 == n;
        let inputs: Vec<InputPart> = if k == 0 {
            let mut notes = Vec::new();
            let inputs = input::gather_with_notes(
                &stage.specs,
                stage.task.requires_material,
                cfg.settings.input_bytes,
                stage.cli.dry_run,
                env,
                &mut notes,
            )
            .map_err(|e| AppError::usage(format!("{e:#}")))?;
            if !stage.cli.quiet {
                for note in &notes {
                    eprintln!("note: {note}");
                }
            }
            inputs
        } else {
            vec![handoff(&prev_artifacts, k, &stage.task.name)?]
        };
        let role = if is_last {
            StageRole::Terminal
        } else {
            StageRole::Intermediate
        };
        let mut stage_plan =
            plan::build_with_inputs(&stage.cli, &stage.task, inputs, cfg, terminal, role)?;
        stage_plan.stage_label = Some(format!("chain {}/{}", k + 1, n));
        if k == 0 {
            // The interrupted-run placeholder needs a record of a started
            // run before the first request goes out.
            on_started(plan::summarize(&stage_plan));
        }
        let mut out = runner::execute(&stage_plan).await?;
        // Re-base provenance onto the run-global request numbering so a
        // chain's manifests name every request without collisions.
        retag(&mut out.artifacts, request_offset);
        request_offset += stage_plan.steps.len();
        if !is_last {
            rename_intermediates(&mut out.artifacts, &stage.task.name);
            for w in out.warnings.iter_mut() {
                *w = format!("stage {}: {w}", k + 1);
            }
        }
        let stage_len = out.artifacts.len();
        let failed = out.status != GenerationStatus::Complete || out.failure.is_some();
        last_meta = (
            out.live_stdout,
            out.live_chars,
            out.steps_done,
            out.steps_total,
        );
        all_artifacts.append(&mut prev_artifacts);
        prev_artifacts = out.artifacts;
        warnings.extend(out.warnings);
        summaries.push(plan::summarize(&stage_plan));
        last_plan = Some(stage_plan);
        let stage_failure = out.failure;
        last_len = stage_len;
        if failed {
            all_artifacts.append(&mut prev_artifacts);
            // The chain stops here: no later stage runs, the upstream
            // artifacts stay real and travel as the record, and the exit
            // code is the failed stage's own classification (a service
            // error exits 3; a finished-but-truncated generation, 4).
            let reason = format!("stage {}/{} ({}) failed", k + 1, n, stage.task.name);
            let failure = match stage_failure {
                Some(e) => AppError::new(e.kind, format!("{reason}: {}", e.chain())),
                None => AppError::generation(reason.clone()),
            };
            warnings.push(format!(
                "{reason}; the earlier stages' artifacts are kept in history"
            ));
            return Ok(ChainRun {
                plan: last_plan.expect("the failed stage built a plan"),
                output: RunOutput {
                    artifacts: all_artifacts,
                    status: GenerationStatus::Incomplete {
                        reason: reason.clone(),
                    },
                    warnings,
                    live_stdout: last_meta.0,
                    live_chars: last_meta.1,
                    failed_parts: Vec::new(),
                    parts_total: 0,
                    steps_done: last_meta.2,
                    steps_total: last_meta.3,
                    failure: Some(failure),
                },
                stage_summaries: summaries,
                last_stage_len: last_len,
            });
        }
    }
    all_artifacts.append(&mut prev_artifacts);
    let plan = last_plan.expect("a chain has at least two stages");
    Ok(ChainRun {
        plan,
        output: RunOutput {
            artifacts: all_artifacts,
            status: GenerationStatus::Complete,
            warnings,
            live_stdout: last_meta.0,
            live_chars: last_meta.1,
            failed_parts: Vec::new(),
            parts_total: 0,
            steps_done: last_meta.2,
            steps_total: last_meta.3,
            // Every stage completed: no failure left to carry.
            failure: None,
        },
        stage_summaries: summaries,
        last_stage_len: last_len,
    })
}

/// The single text artifact crossing a junction, as the next stage's one
/// input part. The junction type check makes the shape a certainty for a
/// planned chain; this guards the runtime edge — an upstream stage that
/// finished with an unexpected artifact set must not hand garbage on.
fn handoff(artifacts: &[Artifact], k: usize, task_name: &str) -> AppResult<InputPart> {
    let texts: Vec<&Artifact> = artifacts
        .iter()
        .filter(|a| a.kind == MediaKind::Text)
        .collect();
    if texts.len() != 1 {
        return Err(AppError::generation(format!(
            "stage {} ({}) needs exactly one text artifact from stage {k}; got {}",
            k + 1,
            task_name,
            texts.len()
        )));
    }
    let text = texts[0]
        .text()
        .ok_or_else(|| AppError::generation("the previous stage's text is not valid UTF-8"))?;
    Ok(InputPart {
        id: 0,
        source: InputSource::Stage { index: k - 1 },
        name: format!("stage-{k}-output"),
        kind: MediaKind::Text,
        unknown_kind: false,
        mime: "text/plain".into(),
        content: InputContent::Text(text.to_string()),
        unit: None,
    })
}

/// Re-base one stage's provenance onto the run-global request numbering,
/// so a chain record and its manifests name every request without
/// collisions.
fn retag(artifacts: &mut [Artifact], offset: usize) {
    for artifact in artifacts.iter_mut() {
        artifact.provenance = match &artifact.provenance {
            Provenance::Request { index } => Provenance::Request {
                index: index + offset,
            },
            Provenance::Merged { requests } => Provenance::Merged {
                requests: requests.iter().map(|r| r + offset).collect(),
            },
            Provenance::Restored => Provenance::Restored,
        };
    }
}

/// Name an intermediate stage's artifacts after the stage (`ocr`,
/// `translate-2`), so a chain's `--out-dir` manifest and history keep one
/// readable file per stage without collisions. The last stage keeps the
/// ordinary naming — its artifacts are the deliverables.
fn rename_intermediates(artifacts: &mut [Artifact], task_name: &str) {
    let mut count = 0usize;
    for artifact in artifacts.iter_mut() {
        count += 1;
        artifact.id = if count == 1 {
            task_name.to_string()
        } else {
            format!("{task_name}-{count}")
        };
    }
}

/// The `--dry-run` preview, one block per stage. Stage 1 builds its real
/// plan (material gather included, under dry-run rules); later stages
/// render from task + resolved route — their material only exists once
/// the previous stage has run.
pub fn describe_chain(chain: &PreparedChain, cfg: &Config) -> AppResult<String> {
    let terminal = TerminalInfo::real();
    let mut env = InputEnv::real();
    let first = &chain.stages[0];
    let mut cli1 = first.cli.clone();
    cli1.dry_run = true; // never read the clipboard for a preview
    let plan1 = plan::build(&cli1, &first.task, &first.specs, cfg, terminal, &mut env)?;
    if plan1.per_part && real_batch(&plan1) {
        return Err(AppError::usage(format!(
            "stage 1 ({}) would process {} unit(s) one request each; a chain stage \
             hands off a single result — run the batch form on its own (v1 keeps \
             chains and per-part batches apart)",
            first.task.name,
            batch_units(&plan1)
        )));
    }
    let n = chain.stages.len();
    let mut out = format!("chain: {}\n", chain.task_label());
    for (k, stage) in chain.stages.iter().enumerate() {
        let is_first = k == 0;
        let is_last = k + 1 == n;
        out.push_str(&format!(
            "\nstage {}/{n}: {} ({}, operation {})\n",
            k + 1,
            stage.task.name,
            if stage.task.builtin {
                "builtin"
            } else {
                "user"
            },
            stage.task.operation
        ));
        let r = &stage.resolved;
        out.push_str(&format!("profile:     {}\n", r.profile_name));
        let shown_url = r
            .base_url
            .as_deref()
            .map(plan::redact_url)
            .unwrap_or_else(|| "(endpoint owned by the edge-tts adapter)".to_string());
        out.push_str(&format!(
            "provider:    {} → {} (route: {})\n",
            r.provider_name, shown_url, r.adapter
        ));
        if let Some(warning) =
            crate::api::cleartext_key_warning(r.base_url.as_deref(), plan::api_key_present(r))
        {
            out.push_str(&format!("warning:     {warning}\n"));
        }
        out.push_str(&format!("model:       {}\n", r.model));
        let instruction = plan::compose_instruction(&stage.cli, &stage.task)?;
        if !instruction.is_empty() {
            out.push_str(&format!(
                "instruction: {}\n",
                first_line(&instruction, plan::DRY_RUN_LINE_MAX)
            ));
        }
        if let Some(req) = stage.cli.prompt.as_deref().filter(|p| !p.trim().is_empty()) {
            out.push_str(&format!(
                "requirement: {}\n",
                first_line(req, plan::DRY_RUN_LINE_MAX)
            ));
        }
        if is_first {
            out.push_str("material:\n");
            if plan1.inputs.is_empty() {
                out.push_str("  (none — the instruction alone drives this run)\n");
            }
            for part in &plan1.inputs {
                let kind = if part.unknown_kind {
                    "unknown (decided at runtime)"
                } else {
                    part.kind.as_str()
                };
                let bytes = match &part.content {
                    crate::domain::InputContent::Text(s) => s.len() as u64,
                    crate::domain::InputContent::Media(b) => b.len() as u64,
                };
                out.push_str(&format!(
                    "  {}. {}  {}  {}  [{}]\n",
                    part.id + 1,
                    part.name,
                    kind,
                    part.source,
                    plan::human_bytes(bytes)
                ));
            }
        } else {
            out.push_str(&format!("material:    stage {k} output (text)\n"));
        }
        if is_first {
            out.push_str(&format!(
                "processing:  {} ({} request{})\n",
                processor_name(plan1.processor),
                plan1.steps.len(),
                if plan1.steps.len() == 1 { "" } else { "s" }
            ));
        } else {
            out.push_str(&format!(
                "processing:  {}\n",
                processor_name(stage.task.processor)
            ));
        }
        out.push_str(&format!("produce:     {}\n", kinds(&r.produce)));
        out.push_str("parameters:\n");
        for (name, value, source) in
            plan::describe_param_sources(&stage.cli, &stage.task, &stage.resolved)
        {
            out.push_str(&format!("  {name} = {value}  ({})\n", source.as_str()));
        }
        if is_last {
            let destinations = plan::resolve_destinations(&stage.cli, &r.produce, terminal)?;
            let transport_stream = !stage.cli.no_stream && r.adapter.streams();
            let stdout_text = destinations.contains(&Destination::Stdout)
                && r.produce.as_slice() == [MediaKind::Text];
            let delivery = if stage.cli.stream
                || (transport_stream
                    && stdout_text
                    && !stage.cli.json
                    && cfg.settings.stream != Some(false)
                    && terminal.stdout)
            {
                "live"
            } else {
                "buffered"
            };
            out.push_str(&format!(
                "transport:   {} (delivery: {delivery})\n",
                if transport_stream {
                    "streaming"
                } else {
                    "buffered"
                }
            ));
            out.push_str("destinations:\n");
            for d in &destinations {
                out.push_str(&format!("  - {d}\n"));
            }
            if r.adapter != crate::api::Adapter::EdgeTts {
                match crate::config::effective_key_env(r.api_key_env.as_deref()) {
                    Some(name) => {
                        let via_fallback = r.api_key_env.as_deref() != Some(name);
                        out.push_str(&format!(
                            "credentials: {name}{} is set\n",
                            if via_fallback { " (fallback)" } else { "" }
                        ));
                    }
                    None => out.push_str(&format!(
                        "credentials: {} is NOT set — the request would fail\n",
                        r.api_key_env
                            .as_deref()
                            .unwrap_or(crate::config::DEFAULT_KEY_ENV)
                    )),
                }
            } else {
                out.push_str("credentials: none required\n");
            }
        } else {
            out.push_str("transport:   buffered (delivery: none — intermediate stage)\n");
            out.push_str(&format!(
                "→ feeds stage {}: {}\n",
                k + 2,
                chain.stages[k + 1].task.name
            ));
        }
    }
    out.push_str("\nhistory:     no request is sent, nothing is recorded\n");
    Ok(out)
}

/// Whether stage 1's plan is a real per-part batch (more than one unit):
/// single-file inputs of per-part tasks are ordinary runs and fine in a
/// chain; a directory, glob or multi-page document is not.
fn real_batch(plan: &plan::ExecutionPlan) -> bool {
    batch_units(plan) > 1
}

fn batch_units(plan: &plan::ExecutionPlan) -> usize {
    let mut ids: Vec<usize> = plan.steps.iter().filter_map(|s| s.part).collect();
    ids.sort_unstable();
    ids.dedup();
    ids.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage(task: &str, produce: Vec<MediaKind>, allowed: Option<Vec<MediaKind>>) -> StageParsed {
        // A minimal stage: only the fields the junction checks read.
        let resolved = Resolved {
            profile_name: "default".into(),
            provider_name: "p".into(),
            adapter: crate::api::Adapter::Chat,
            base_url: None,
            api_key_env: None,
            model: "m".into(),
            model_source: crate::config::resolve::ParamSource::Default,
            max_tokens: None,
            max_tokens_source: crate::config::resolve::ParamSource::Default,
            temperature: None,
            temperature_source: crate::config::resolve::ParamSource::Default,
            options: Default::default(),
            allowed_inputs: allowed,
            required_inputs: Vec::new(),
            produce: produce.clone(),
        };
        let task = Task {
            name: task.into(),
            operation: crate::tasks::Operation::Generate,
            instruction: String::new(),
            profile: None,
            input_types: None,
            required_types: Vec::new(),
            max_inputs: None,
            output_types: produce,
            requires_material: true,
            processor: ProcessorKind::Single,
            per_part: false,
            params: Vec::new(),
            defaults: Default::default(),
            options: Default::default(),
            builtin: true,
        };
        let cli = Cli::try_parse_from(["aido", "--__task", task.name.as_str()]).unwrap();
        StageParsed {
            task,
            cli,
            specs: Vec::new(),
            resolved,
        }
    }

    #[test]
    fn text_junctions_pass_and_media_ones_fail() {
        let mut stages = vec![
            stage("ocr", vec![MediaKind::Text], None),
            stage("tts", vec![MediaKind::Audio], Some(vec![MediaKind::Text])),
        ];
        validate_chain_types(&stages).unwrap();

        // A downstream stage that refuses text kills the junction.
        stages[1].resolved.allowed_inputs = Some(vec![MediaKind::Audio]);
        let err = validate_chain_types(&stages).unwrap_err();
        assert!(err.message.contains("does not accept text input"), "{err}");
        assert_eq!(err.kind, crate::domain::ErrorKind::Usage);
    }

    #[test]
    fn non_text_handoff_is_rejected_before_the_last_stage() {
        let stages = vec![
            stage("image", vec![MediaKind::Image], None),
            stage("tts", vec![MediaKind::Audio], Some(vec![MediaKind::Text])),
        ];
        let err = validate_chain_types(&stages).unwrap_err();
        assert!(err.message.contains("exactly text"), "{err}");
        assert!(err.message.contains("→ stage 2 (tts)"), "{err}");
    }

    #[test]
    fn required_types_must_be_produced_upstream() {
        // The downstream stage accepts text at large (no allow-list) but
        // declares audio as required: the missing kind is its own error.
        let mut down = stage("transcribe", vec![MediaKind::Text], None);
        down.task.required_types = vec![MediaKind::Audio];
        let stages = vec![stage("ocr", vec![MediaKind::Text], None), down];
        let err = validate_chain_types(&stages).unwrap_err();
        assert!(err.message.contains("requires audio input"), "{err}");
    }

    #[test]
    fn run_level_conflicts_are_usage_errors() {
        let mut stages = vec![
            stage("ocr", vec![MediaKind::Text], None),
            stage("tts", vec![MediaKind::Audio], Some(vec![MediaKind::Text])),
        ];
        stages[0].cli.output = Some("/tmp/a.png".into());
        stages[1].cli.output = Some("/tmp/b.mp3".into());
        let err = merge_run_flags(&mut stages).unwrap_err();
        assert!(err.message.contains("--output"), "{err}");

        // Same value on both stages passes.
        stages[1].cli.output = Some("/tmp/a.png".into());
        merge_run_flags(&mut stages).unwrap();
        assert_eq!(stages[1].cli.output.as_deref(), Some("/tmp/a.png".as_ref()));
    }

    #[test]
    fn stream_pair_conflicts_across_stages() {
        let mut stages = vec![
            stage("ocr", vec![MediaKind::Text], None),
            stage("tts", vec![MediaKind::Audio], Some(vec![MediaKind::Text])),
        ];
        stages[0].cli.no_stream = true;
        stages[1].cli.stream = true;
        let err = merge_run_flags(&mut stages).unwrap_err();
        assert!(err.message.contains("--stream"), "{err}");
    }

    #[test]
    fn run_level_flags_merge_into_the_last_stage() {
        let mut stages = vec![
            stage("ocr", vec![MediaKind::Text], None),
            stage("tts", vec![MediaKind::Audio], Some(vec![MediaKind::Text])),
        ];
        stages[0].cli.json = true;
        stages[0].cli.quiet = true;
        stages[0].cli.total_timeout = Some(30);
        merge_run_flags(&mut stages).unwrap();
        let last = &stages[1].cli;
        assert!(last.json && last.quiet);
        assert_eq!(last.total_timeout, Some(30));
    }
}

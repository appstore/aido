//! Task chains: one run, several ordered stages — the `--then` primitive
//! and the `chain "a | b"` sugar, which [`crate::cli`] reduces to the same
//! stage list.
//!
//! This module owns the whole chain lifecycle: per-stage clap parsing and
//! the plan-time contract (a chain that cannot work fails here, before
//! any request is sent — exit 2, zero requests), the stage-by-stage
//! executor, and the `--dry-run` preview. Every junction hands off
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
            let shown = if name == crate::cli::LAST_TASK {
                "last"
            } else {
                name.as_str()
            };
            return Err(AppError::usage(format!(
                "'{shown}' cannot be a chain stage"
            )));
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
    // Run-level flags move to the last stage BEFORE the plan checks: the
    // checks read `-o`/`--out-dir`/`--stream`, so a chain whose delivery
    // flags sit in the outer argv (or on an earlier `--then` stage) must
    // be judged on the merged shape — the same shape the run executes.
    merge_run_flags(&mut parsed)?;
    let n = parsed.len();
    let (rest, last_stage) = parsed.split_at_mut(n - 1);
    for (i, stage) in rest.iter_mut().enumerate() {
        if let Err(e) = plan::preflight_stage(&stage.cli, &stage.task, cfg, false) {
            let name = stage.task.name.clone();
            return Err(AppError::usage(format!(
                "stage {} ({}): {}",
                i + 1,
                name,
                e.chain_inline()
            )));
        }
    }
    if let Err(e) = plan::preflight_stage(&last_stage[0].cli, &last_stage[0].task, cfg, true) {
        let name = last_stage[0].task.name.clone();
        return Err(AppError::usage(format!(
            "stage {n} ({name}): {}",
            e.chain_inline()
        )));
    }
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
/// one report format, one time budget. `--produce`/`--format` are
/// refused outright: a stage's outputs are fixed by the junction
/// contract, so on a non-last stage they have no legal meaning, and
/// merging them would silently reshape the last stage's request after
/// earlier stages paid.
///
/// Two shapes of flag, two fates. Delivery choices (`-o`, `--out-dir`,
/// `--copy`, `--stdout`, `--json`, `--overwrite`) merge into the last
/// stage and are stripped from the source stage: they must not leak into
/// an earlier stage's plan (a leftover `--out-dir` would let a real
/// per-part batch run as an intermediate and pay for results no junction
/// can hand off). Run-mode choices (`--quiet`, `--dry-run`,
/// `--no-history`, `--stream`/`--no-stream`, `--total-timeout`) describe
/// every stage, so the merged value propagates back to all of them.
fn merge_run_flags(stages: &mut [StageParsed]) -> AppResult<()> {
    let n = stages.len();
    for k in 0..n - 1 {
        let stage = k + 1;
        let src = stages[k].cli.clone();
        if !src.produce.is_empty() {
            return Err(AppError::usage(format!(
                "stage {stage}: --produce belongs to a single stage; write it on \
                 the last stage only"
            )));
        }
        if src.format.is_some() {
            return Err(AppError::usage(format!(
                "stage {stage}: --format belongs to a single stage; write it on \
                 the last stage only"
            )));
        }
        let dst = &mut stages[n - 1].cli;
        merge_opt(&mut dst.output, &src.output, "--output", stage)?;
        merge_opt(&mut dst.out_dir, &src.out_dir, "--out-dir", stage)?;
        merge_opt(
            &mut dst.total_timeout,
            &src.total_timeout,
            "--total-timeout",
            stage,
        )?;
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
        let src_cli = &mut stages[k].cli;
        src_cli.output = None;
        src_cli.out_dir = None;
        src_cli.copy = false;
        src_cli.stdout = false;
        src_cli.json = false;
        src_cli.overwrite = false;
    }
    // Clap only sees conflicts per stage; merging can recombine two
    // stages' choices into a combination the single-run surface refuses.
    let last = &stages[n - 1].cli;
    if last.output.is_some() && last.out_dir.is_some() {
        return Err(AppError::usage(
            "-o FILE and --out-dir are two delivery contracts for one run; pick one",
        ));
    }
    if last.stdout && last.json {
        return Err(AppError::usage(
            "--json cannot combine with --stdout (two stdout contracts)",
        ));
    }
    // Propagate the run-mode choices to every stage, so both chain
    // spellings run the same plans: `--quiet` silences each stage's
    // spinner, `--no-stream` buffers every request, `--no-history`
    // records nothing even when a mid-chain stage's plan travels into the
    // failure record, and `--total-timeout` is one budget per stage (each
    // gets whatever remains).
    let run = stages[n - 1].cli.clone();
    for stage in &mut stages[..n - 1] {
        let cli = &mut stage.cli;
        cli.quiet |= run.quiet;
        cli.dry_run |= run.dry_run;
        cli.no_history |= run.no_history;
        cli.stream |= run.stream;
        cli.no_stream |= run.no_stream;
        cli.total_timeout = run.total_timeout;
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
/// output over every stage, one summary per stage, and where the final
/// stage's deliverables begin inside the artifact list (the record's
/// `last_stage_len` is the artifacts after it).
pub struct ChainRun {
    pub plan: plan::ExecutionPlan,
    pub output: RunOutput,
    pub stage_summaries: Vec<RunSummary>,
    pub deliverable_start: usize,
}

/// Run the chain stage by stage. Stage 1 gathers its own material; every
/// later stage consumes the previous stage's single text artifact. The
/// first stage that fails ends the chain: artifacts already produced
/// travel in the returned output (history keeps them), the failure
/// carries the stage's name, and no later stage sends anything.
///
/// `on_started` fires once before the first request (the interrupted-run
/// placeholder); `on_progress` fires after each completed upstream stage
/// with everything paid for so far, so a Ctrl+C mid-chain can record the
/// earlier stages' artifacts instead of dropping them with the future.
pub async fn execute(
    chain: &PreparedChain,
    cfg: &Config,
    terminal: TerminalInfo,
    env: &mut InputEnv<'_>,
    on_started: &mut dyn FnMut(RunSummary),
    on_progress: &mut dyn FnMut(&[Artifact], &[RunSummary]),
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
    // The last executed stage's streaming/step facts, for the merged
    // output: only the final stage can have streamed, and only its step
    // counts describe "the run".
    let mut last_meta = (false, 0u64, 0usize, 0usize);
    // The chain's own clock: `--total-timeout` caps the whole run, so
    // every stage gets whatever remains of the budget, not a fresh one.
    let started = std::time::Instant::now();

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
            // Defensive: the junction check below stops a mis-shaped
            // stage before this point. If it ever fires anyway, the
            // upstream artifacts travel as the record instead of being
            // lost with a bare error.
            match handoff(&prev_artifacts, k, &stage.task.name) {
                Ok(part) => vec![part],
                Err(e) => {
                    return Ok(stopped_chain(
                        last_plan.expect("stage 1 built a plan"),
                        all_artifacts,
                        prev_artifacts,
                        warnings,
                        last_meta,
                        summaries,
                        format!("stage {}/{} handoff failed: {}", k + 1, n, e.chain()),
                        e,
                    ));
                }
            }
        };
        let role = if is_last {
            StageRole::Terminal
        } else {
            StageRole::Intermediate
        };
        let mut stage_plan =
            match plan::build_with_inputs(&stage.cli, &stage.task, inputs, cfg, terminal, role) {
                Ok(plan) => plan,
                // A later stage's plan can depend on the previous stage's
                // real output (chunk sizes, artifact counts): failing here
                // is a chain failure — the upstream artifacts stay real and
                // travel as the record instead of being lost with a bare
                // error. Stage 1 has nothing paid yet and propagates.
                Err(e) if k > 0 => {
                    let detail = format!(
                        "stage {}/{} ({}) failed: {}",
                        k + 1,
                        n,
                        stage.task.name,
                        e.chain()
                    );
                    return Ok(stopped_chain(
                        last_plan.expect("stage 1 built a plan"),
                        all_artifacts,
                        prev_artifacts,
                        warnings,
                        last_meta,
                        summaries,
                        detail,
                        AppError::new(
                            e.kind,
                            format!("stage {}/{} ({}): {}", k + 1, n, stage.task.name, e.chain()),
                        ),
                    ));
                }
                Err(e) => return Err(e),
            };
        stage_plan.stage_label = Some(format!("chain {}/{}", k + 1, n));
        if let Some(budget) = stage_plan.total_timeout {
            stage_plan.total_timeout = Some(budget.saturating_sub(started.elapsed()));
        }
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
            rename_intermediates(&mut out.artifacts, &stage.task.name, k);
            for w in out.warnings.iter_mut() {
                *w = format!("stage {}: {w}", k + 1);
            }
        }
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
        if failed {
            // The chain stops here: no later stage runs, the upstream
            // artifacts stay real and travel as the record, and the exit
            // code is the failed stage's own classification (a service
            // error exits 3; a finished-but-truncated generation, 4).
            // The reason carries the underlying cause (the service
            // message, the truncation) everywhere the short reason goes —
            // stderr, --json, and the history record.
            let base = format!("stage {}/{} ({}) failed", k + 1, n, stage.task.name);
            let detail = match (&out.status, &stage_failure) {
                (_, Some(e)) => format!("{base}: {}", e.chain()),
                (GenerationStatus::Incomplete { reason }, None) => {
                    format!("{base}: {reason}")
                }
                _ => base.clone(),
            };
            let failure = match stage_failure {
                Some(e) => AppError::new(e.kind, detail.clone()),
                None => AppError::generation(detail.clone()),
            };
            return Ok(stopped_chain(
                last_plan.expect("the failed stage built a plan"),
                all_artifacts,
                prev_artifacts,
                warnings,
                last_meta,
                summaries,
                detail,
                failure,
            ));
        }
        // Snapshot for the Ctrl+C placeholder: every stage up to here
        // completed, so its artifacts are paid for. Only upstream stages
        // snapshot — after the final stage there is no await point left
        // inside the chain, so a cancel can no longer land there.
        if !is_last {
            let mut kept = all_artifacts.clone();
            kept.extend(prev_artifacts.iter().cloned());
            on_progress(&kept, &summaries);
        }
        if !is_last {
            // A stage can finish "cleanly" with the wrong shape — an
            // empty reply produces no artifact at all. The junction needs
            // exactly one text artifact; stopping here keeps the
            // upstream artifacts and refuses delivery, instead of letting
            // an upstream artifact pose as this stage's result (or the
            // next junction failing with the upstream work dropped).
            let texts = prev_artifacts
                .iter()
                .filter(|a| a.kind == MediaKind::Text)
                .count();
            if texts != 1 {
                let detail = format!(
                    "stage {}/{} ({}) produced {texts} text artifact(s); a chain \
                     junction hands off exactly one",
                    k + 1,
                    n,
                    stage.task.name
                );
                return Ok(stopped_chain(
                    last_plan.expect("the stage built a plan"),
                    all_artifacts,
                    prev_artifacts,
                    warnings,
                    last_meta,
                    summaries,
                    detail.clone(),
                    AppError::generation(detail),
                ));
            }
        }
    }
    // The final stage's artifacts start here; everything before rides
    // into --out-dir (and its manifest) only. Recorded even when the last
    // stage produced nothing (start == len): the empty deliverable then
    // fails the last stage's own unsatisfied check downstream.
    let deliverable_start = all_artifacts.len();
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
        deliverable_start,
    })
}

/// The chain-stopped-here outcome, shared by "a stage's run failed" and
/// "a later stage's plan failed": the reason names the stage, the failure
/// keeps its own class (the exit code), and the upstream artifacts —
/// including whatever the failed stage produced — travel as the record
/// for history to keep.
#[allow(clippy::too_many_arguments)]
fn stopped_chain(
    plan: plan::ExecutionPlan,
    mut artifacts: Vec<Artifact>,
    stage_artifacts: Vec<Artifact>,
    mut warnings: Vec<String>,
    meta: (bool, u64, usize, usize),
    summaries: Vec<RunSummary>,
    reason: String,
    failure: AppError,
) -> ChainRun {
    let stage_len = stage_artifacts.len();
    artifacts.extend(stage_artifacts);
    // The promise needs history to be on: with --no-history (or a zero
    // keep budget) nothing is written, and the warning must not claim it.
    if plan.record_history {
        warnings.push(format!(
            "{reason}; the earlier stages' artifacts are kept in history"
        ));
    }
    // The failed stage's own artifacts are the trailing slice (empty when
    // it produced nothing). An incomplete record is never restored for
    // delivery, so the value only describes the record.
    let deliverable_start = artifacts.len() - stage_len;
    ChainRun {
        plan,
        output: RunOutput {
            artifacts,
            status: GenerationStatus::Incomplete {
                reason: reason.clone(),
            },
            warnings,
            live_stdout: meta.0,
            live_chars: meta.1,
            failed_parts: Vec::new(),
            parts_total: 0,
            steps_done: meta.2,
            steps_total: meta.3,
            failure: Some(failure),
        },
        stage_summaries: summaries,
        deliverable_start,
    }
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

/// Name an intermediate stage's artifacts after a stage-namespaced stem
/// (`stage-1-summarize`, `stage-2-summarize`): the namespace keeps them
/// from colliding with the last stage's ordinary ids (`text`, …) or with
/// each other — a collision would be a silent on-disk overwrite, since
/// file names, not raw ids, are what history and `--out-dir` write. The
/// last stage keeps the ordinary naming — its artifacts are the
/// deliverables.
fn rename_intermediates(artifacts: &mut [Artifact], task_name: &str, stage: usize) {
    let base = format!("stage-{}-{task_name}", stage + 1);
    let mut count = 0usize;
    for artifact in artifacts.iter_mut() {
        count += 1;
        artifact.id = if count == 1 {
            base.clone()
        } else {
            format!("{base}-{count}")
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

    // Stage 1's real plan build doubles as the batch gate: an
    // intermediate stage that would run a per-part batch is refused at
    // plan time (plan_from), zero requests, before anything is previewed.
    let plan1 = plan::build(&cli1, &first.task, &first.specs, cfg, terminal, &mut env)?;
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
            // Mirror the plan's processor selection: --no-split on the
            // stage forces Single there too.
            let processor = if stage.cli.no_split {
                ProcessorKind::Single
            } else {
                stage.task.processor
            };
            out.push_str(&format!("processing:  {}\n", processor_name(processor)));
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

    #[test]
    fn delivery_flags_leave_the_source_stage_and_mode_flags_reach_every_stage() {
        let mut stages = vec![
            stage("ocr", vec![MediaKind::Text], None),
            stage("summarize", vec![MediaKind::Text], None),
            stage("tts", vec![MediaKind::Audio], Some(vec![MediaKind::Text])),
        ];
        // A delivery flag and a run-mode flag on stage 1 (the --then
        // shape): the delivery flag belongs to the last stage alone — a
        // leftover --out-dir would let a real per-part batch run as an
        // intermediate — while the run-mode flag describes every stage.
        stages[0].cli.out_dir = Some("/tmp/x".into());
        stages[0].cli.no_stream = true;
        merge_run_flags(&mut stages).unwrap();
        assert!(
            stages[0].cli.out_dir.is_none() && stages[1].cli.out_dir.is_none(),
            "delivery flags stripped from non-last stages"
        );
        assert_eq!(stages[2].cli.out_dir.as_deref(), Some("/tmp/x".as_ref()));
        assert!(
            stages[0].cli.no_stream && stages[1].cli.no_stream && stages[2].cli.no_stream,
            "run-mode flags propagate to every stage"
        );
    }
}

//! The command-line surface (contract §2.1–2.3) and the argv normalizer
//! that makes `aido <TASK> [INPUT...] [OPTIONS]` parseable by clap.
//!
//! The normalizer runs first: it knows every declared flag and its arity,
//! finds the first free (non-flag, non-value) token, and decides what it
//! is — a task name, `run`, `ask`, a management command, or a file. It
//! collects the ordered input slots (positional files, `-`, `--paste`,
//! `--text`) in argv order, then hands clap an argv containing only flags.

use crate::domain::MediaKind;
use anyhow::{anyhow, bail, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::ffi::OsString;
use std::path::PathBuf;

/// The domain type stays free of clap; the CLI layer teaches it to clap.
impl clap::ValueEnum for MediaKind {
    fn value_variants<'a>() -> &'a [Self] {
        &[MediaKind::Text, MediaKind::Image, MediaKind::Audio]
    }
    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(clap::builder::PossibleValue::new(self.as_str()))
    }
}

/// Names owned by management commands and helpers; never treated as tasks
/// (`aido run NAME` reaches a custom task with such a name).
pub const RESERVED_WORDS: &[&str] = &[
    "tasks", "profiles", "config", "history", "run", "ask", "last", "help", "version", "__hold",
    "chain",
];

/// The recovery pseudo-task behind `aido last`.
pub const LAST_TASK: &str = "__last";

/// One material slot in argv order, before anything is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceSpec {
    File(PathBuf),
    /// A glob pattern that reached aido unexpanded (quoted on Unix, always
    /// on Windows); expanded into files at gather time, never here.
    Glob(String),
    Stdin,
    Paste,
    Text(String),
}

/// One chain stage's argv after normalization: the stage's task, its
/// ordered input specs (stage 1 only — later stages read the previous
/// stage's output), and the flags left for clap.
#[derive(Debug, Clone)]
pub struct StageArgv {
    pub task: Option<String>,
    pub specs: Vec<SourceSpec>,
    pub argv: Vec<OsString>,
}

/// argv after normalization: either one task run, or a task chain already
/// split into per-stage argv (`--then` markers, or the `chain "a | b"`
/// sugar, both reduced to the same stage list here).
#[derive(Debug)]
pub enum Normalized {
    Single {
        task: Option<String>,
        specs: Vec<SourceSpec>,
        argv: Vec<OsString>,
    },
    Chain {
        stages: Vec<StageArgv>,
    },
}

impl Normalized {
    /// Whether the run asked for `--json`, judged on the normalized argv.
    /// The raw argv scan answers this for tokens at the top level, but a
    /// chain spec is one shell token — `chain "a | b --json"` carries the
    /// flag where only the tokenizer can see it, and the run would print
    /// its report as JSON while a parse error printed as plain text. This
    /// walks the same stage lists the run itself will parse, so the error
    /// contract and the report format cannot disagree. (After parsing
    /// succeeds, `cli.json` is the authority.)
    pub fn wants_json(&self) -> bool {
        match self {
            Normalized::Single { argv, .. } => wants_json_in(argv),
            Normalized::Chain { stages } => stages.iter().any(|stage| wants_json_in(&stage.argv)),
        }
    }
}

/// Flags and their value arity — the normalizer's single source of truth.
/// `takes_value` must match the clap schema below.
const FLAGS: &[(&str, bool)] = &[
    ("--prompt", true),
    ("--text", true),
    ("--task", true),
    ("--output", true),
    ("--out-dir", true),
    ("--produce", true),
    ("--format", true),
    ("--profile", true),
    ("--model", true),
    ("--max-tokens", true),
    ("--temperature", true),
    ("--option", true),
    ("--timeout", true),
    ("--total-timeout", true),
    ("--to", true),
    ("--voice", true),
    ("--speed", true),
    ("--count", true),
    ("--size", true),
    ("--copy", false),
    ("--stdout", false),
    ("--json", false),
    ("--overwrite", false),
    ("--quiet", false),
    ("--stream", false),
    ("--no-stream", false),
    ("--no-split", false),
    ("--dry-run", false),
    ("--no-history", false),
    ("--paste", false),
    ("--help", false),
    ("--version", false),
];

/// Pre-2.0 flags that must fail with guidance instead of silently changing
/// meaning: `--save FILE` used to compose with default stdout, which the
/// new destination rules cannot express as an alias.
const REMOVED_FLAGS: &[(&str, &str)] = &[
    (
        "--save",
        "use `-o FILE` (exactly one artifact; add --stdout to also print)",
    ),
    ("--save-dir", "use `--out-dir DIR`"),
    (
        "--input-mode",
        "input types are the task's contract now (see `aido tasks show <TASK>`)",
    ),
    (
        "--output-mode",
        "use `--produce TYPES` for requested generated types",
    ),
    (
        "--adapter",
        "set the route on a provider in the config (see `aido config init`)",
    ),
    ("--base-url", "set base_url on a provider in the config"),
    (
        "--api-key",
        "set api_key_env on a provider and export that variable",
    ),
    ("--preset", "use `aido <TASK>` or `aido run <TASK>`"),
    ("--init", "use `aido config init`"),
    ("--list-presets", "use `aido tasks list`"),
    ("--no-spinner", "use `--quiet`"),
];

/// A material slot while scanning; free tokens become files/stdin once the
/// task token has been identified and removed.
#[derive(Debug)]
enum Slot {
    Free(OsString),
    /// A free token that followed `--`: a literal path, never a pattern
    /// to expand (`-` still reads stdin, as at every other position).
    Literal(OsString),
    Text(String),
    Paste,
}

fn flag_arity(token: &str) -> Option<bool> {
    if let Some((name, _)) = token.split_once('=') {
        if name.len() > 2 && name.starts_with("--") {
            return Some(match FLAGS.iter().find(|(f, _)| *f == name) {
                Some((_, takes_value)) => *takes_value,
                // An unknown `--x=y` is still a flag: hand it to clap so
                // the typo gets "unexpected argument", not "cannot read
                // the file".
                None => false,
            });
        }
        // -p=… / -m=… / -o=… are attached short values.
        if SHORT_VALUE_FLAGS.iter().any(|(s, _)| *s == name) {
            return Some(true);
        }
        return None;
    }
    if let Some((_, v)) = FLAGS.iter().find(|(f, _)| *f == token) {
        return Some(*v);
    }
    for (short, _) in SHORT_VALUE_FLAGS {
        if token == *short {
            return Some(true);
        }
        if token.starts_with(short) && token.len() > 2 {
            return Some(true); // attached value: -pfoo
        }
    }
    if token.starts_with('-') && token != "-" {
        return Some(false); // boolean short or cluster
    }
    None // not a flag
}

/// Whether raw argv asks for the JSON report — decided before clap parses.
///
/// A normalize error or a clap parse error fires before any `Cli` exists,
/// so `app::run` must judge the `--json` error contract from raw argv.
/// The scan has to be arity-aware: a token equal to `--json` that is
/// merely the value of a value-taking flag (`aido ask -p "--json"`) is
/// not a request. That is why it lives here, beside the normalizer: the
/// `FLAGS`/`SHORT_VALUE_FLAGS` arity tables are the single source of
/// truth, and this scan mirrors exactly which tokens the normalizer
/// consumes as values.
pub(crate) fn argv_wants_json() -> bool {
    wants_json_in(&std::env::args_os().skip(1).collect::<Vec<_>>())
}

/// The argv scan behind [`argv_wants_json`], over an explicit slice so
/// tests can drive it: walk the tokens, let a separated value flag
/// swallow its value, stop at `--`, and look for a real `--json`.
fn wants_json_in(argv: &[OsString]) -> bool {
    let mut iter = argv.iter();
    while let Some(token) = iter.next() {
        // Non-UTF-8 argv entries can only be file paths (or a consumed
        // value); either way they are never the `--json` flag.
        let Some(t) = token.to_str() else { continue };
        if t == "--" {
            // Everything past the separator is a literal path.
            return false;
        }
        if t == "--json" {
            return true;
        }
        if flag_arity(t) == Some(true) && !value_attached(t) {
            // A separated value flag: the next token is its value and can
            // never be a flag — the normalizer consumes it the same way
            // (`-p --` is a prompt of "--", not a separator).
            iter.next();
        }
    }
    false
}

/// A value flag whose value rides inside the token itself — `--flag=v`,
/// `-pv`, `-p=v` — so it consumes no further argv token.
fn value_attached(t: &str) -> bool {
    t.contains('=') || short_attached(t).is_some()
}

/// Rewrite argv into its normalized form: a single task run, or a task
/// chain — `--then` markers, or the `chain "a | b"` sugar — already split
/// into per-stage argv. Both chain spellings reduce to the same stage
/// list here, so the rest of the program sees one shape.
pub fn normalize(argv: Vec<OsString>) -> Result<Normalized> {
    // The clipboard holder child is spawned as `aido __hold SECS [--image]`:
    // already clap-shaped, and never a task run — pass it straight through so
    // the normalizer's task discovery cannot reject it (RESERVED_WORDS lists
    // it precisely so it can never be a task name).
    if argv.first().and_then(|t| t.to_str()) == Some("__hold") {
        return Ok(Normalized::Single {
            task: None,
            specs: Vec::new(),
            argv,
        });
    }
    if first_free_is_chain(&argv) {
        return normalize_chain(argv);
    }
    if let Some(segments) = split_on_then(&argv) {
        let mut stages = Vec::new();
        for segment in segments {
            stages.push(normalize_stage(segment)?);
        }
        if stages[0].task.is_none() {
            bail!(
                "a chain must start with a task: aido <TASK> [INPUT...] --then <TASK> \
                 [...] (a leading -p selects ask)"
            );
        }
        return Ok(Normalized::Chain { stages });
    }
    normalize_stage(argv).map(|s| Normalized::Single {
        task: s.task,
        specs: s.specs,
        argv: s.argv,
    })
}

/// Rewrite one stage's argv — a complete single-run command line — into
/// its normalized form. The chain paths call this per stage; the body is
/// the historical single-run normalizer, unchanged.
fn normalize_stage(argv: Vec<OsString>) -> Result<StageArgv> {
    let mut slots: Vec<Slot> = Vec::new();
    let mut rest: Vec<OsString> = Vec::new();
    let mut prompt_seen = false;
    let mut after_separator = false;
    // Free tokens seen before `--`; only these can name a task.
    let mut free_pre: Vec<OsString> = Vec::new();
    // Everything after `--`; management commands must keep these literals.
    let mut post_separator: Vec<OsString> = Vec::new();

    let mut iter = argv.into_iter().peekable();
    while let Some(token) = iter.next() {
        let text = token.to_str().map(|s| s.to_string());
        if !after_separator {
            if let Some(t) = &text {
                if t == "--" {
                    after_separator = true;
                    continue;
                }
                if let Some((flag, guidance)) = REMOVED_FLAGS
                    .iter()
                    .find(|(f, _)| t == *f || t.starts_with(&format!("{f}=")))
                {
                    bail!("'{flag}' is no longer accepted: {guidance}");
                }
                match flag_arity(t) {
                    Some(true) => {
                        // Resolve an attached short (-pfoo/-mfoo/-ofile) to
                        // its long form so every check below sees one
                        // spelling.
                        let attached = short_attached(t);
                        if t == "--prompt"
                            || t.starts_with("--prompt=")
                            || t == "-p"
                            || attached
                                .as_ref()
                                .is_some_and(|(long, _)| *long == "--prompt")
                        {
                            prompt_seen = true;
                        }
                        if t == "--text" || t.starts_with("--text=") {
                            let value = match t.split_once('=') {
                                Some((_, v)) => v.to_string(),
                                None => take_value("--text", &mut iter)?,
                            };
                            slots.push(Slot::Text(value));
                        } else if let Some((long, value)) = attached {
                            // A separated leading-dash value would parse as
                            // a flag to clap; the combined form keeps it a
                            // value — the same rule as the generic branch.
                            if value.starts_with('-') && value != "-" {
                                rest.push(OsString::from(format!("{long}={value}")));
                            } else {
                                rest.push(OsString::from(long));
                                rest.push(OsString::from(value));
                            }
                        } else if t == "--prompt" || t.starts_with("--prompt=") {
                            let value = match t.strip_prefix("--prompt=") {
                                Some(v) => v.to_string(),
                                None => take_value("--prompt", &mut iter)?,
                            };
                            // Same rule as the generic branch: a separated
                            // leading-dash value would parse as a flag, so
                            // it stays attached.
                            if value.starts_with('-') && value != "-" {
                                rest.push(OsString::from(format!("--prompt={value}")));
                            } else {
                                rest.push(OsString::from("--prompt"));
                                rest.push(OsString::from(value));
                            }
                        } else {
                            // `--flag=value` carries its own value; only the
                            // separated form consumes the next token.
                            if !t.contains('=') {
                                let Some(value) = iter.next() else {
                                    bail!("{} requires a value", t);
                                };
                                // `-p VALUE` is the prompt's short spelling:
                                // the same UTF-8 rule as `--prompt`, with the
                                // flag named (clap would reject the raw bytes,
                                // but as a bare "invalid UTF-8" that names
                                // nothing).
                                if t == "-p" {
                                    text_value("-p/--prompt", &value)?;
                                }
                                // A negative number would look like a flag
                                // to clap; the combined form keeps it a
                                // value.
                                let negative = value
                                    .to_str()
                                    .is_some_and(|v| v.starts_with('-') && v != "-");
                                if negative {
                                    let mut combined = token.clone();
                                    combined.push("=");
                                    combined.push(&value);
                                    rest.push(combined);
                                } else {
                                    rest.push(token.clone());
                                    rest.push(value);
                                }
                            } else {
                                rest.push(token.clone());
                            }
                        }
                        continue;
                    }
                    Some(false) => {
                        if t == "--paste" {
                            slots.push(Slot::Paste);
                        } else {
                            rest.push(token.clone());
                        }
                        continue;
                    }
                    None => {} // free token
                }
            } else {
                // A non-UTF-8 entry shaped like a text flag is that flag
                // with a bad value, not a file: `--text=<bytes>`,
                // `--prompt=<bytes>` and `-p<bytes>` must fail the way
                // their separated spellings do instead of quietly
                // becoming an input named after the flag (their UTF-8
                // lookalikes are flags, never files). Every other
                // non-UTF-8 argv entry is a file path.
                if let Some(flag) = text_flag_shape(token.as_encoded_bytes()) {
                    return Err(invalid_text_value(flag));
                }
                free_pre.push(token.clone());
                slots.push(Slot::Free(token));
                continue;
            }
        }
        // A free token (or anything after `--`).
        if after_separator {
            post_separator.push(token.clone());
            slots.push(Slot::Literal(token));
        } else {
            free_pre.push(token.clone());
            slots.push(Slot::Free(token));
        }
    }

    let first = free_pre.first().and_then(|t| t.to_str());

    // `aido help` / `aido version` are words, not tasks: show the real
    // thing instead of "unknown task 'help' (did you mean 'help'?)".
    if matches!(first, Some("help") | Some("version")) {
        let flag = if first == Some("help") {
            "--help"
        } else {
            "--version"
        };
        return Ok(StageArgv {
            task: None,
            specs: Vec::new(),
            argv: vec![OsString::from(flag)],
        });
    }

    // Management commands go to clap as subcommands: the words themselves
    // must stay in the argv (flags may precede them, which clap accepts).
    // Explicit material is never silently dropped — the same rule the
    // `last` branch applies; positional/file arguments are clap's to
    // reject (at `usize::MAX` only `--text`/`--paste` survive into specs).
    if let Some(word) = first {
        if matches!(word, "tasks" | "profiles" | "config" | "history") {
            if prompt_seen {
                bail!("`-p` has no effect on management commands; pass the instruction to a task run instead");
            }
            let specs = specs_from(slots, usize::MAX);
            if !specs.is_empty() {
                bail!(
                    "material flags (--text, --paste) have no effect on management \
                     commands; pass material to a task run instead"
                );
            }
            rest.extend(free_pre.clone());
            rest.extend(post_separator);
            return Ok(StageArgv {
                task: None,
                specs,
                argv: rest,
            });
        }
    }
    if let Some("last") = first {
        // `last` redelivers a recorded run; it takes no fresh material.
        if !specs_from(slots, 1).is_empty() {
            bail!(
                "`aido last` takes no input; it redelivers the most recent \
                 completed run (output flags like -o/--out-dir still apply)"
            );
        }
        return Ok(StageArgv {
            task: Some(LAST_TASK.to_string()),
            specs: Vec::new(),
            argv: {
                let mut argv = vec![OsString::from("--__task"), OsString::from(LAST_TASK)];
                argv.extend(rest);
                argv
            },
        });
    }

    // `run NAME` — NAME is the next free token.
    if first == Some("run") {
        let second = free_pre.get(1).and_then(|t| t.to_str());
        let Some(name) = second else {
            bail!("run requires a task name: aido run <TASK> [INPUT...] [OPTIONS]");
        };
        let mut argv = vec![OsString::from("--__task"), OsString::from(name)];
        argv.extend(rest);
        return Ok(StageArgv {
            task: Some(name.to_string()),
            specs: specs_from(slots, 2),
            argv,
        });
    }

    let task = if let Some(word) = first {
        if word == "ask" {
            Some("ask".to_string())
        } else {
            // Loaded lazily: `aido --help` must not parse user task files
            // (a broken one would print a warning on a pure help request).
            let all_tasks = crate::tasks::load_all()?;
            if all_tasks.contains_key(word) {
                Some(word.to_string())
            } else if prompt_seen {
                // -p selects `ask`; positionals are files.
                None
            } else if looks_like_path(word) {
                bail!(
                    "file input requires a task or -p, e.g. `aido ocr {word}` or \
                     `aido -p \"<instructions>\" {word}` (see `aido tasks list`)"
                );
            } else {
                let mut names: Vec<&str> = all_tasks.keys().map(String::as_str).collect();
                names.extend(RESERVED_WORDS);
                names.sort_unstable();
                names.dedup();
                let hint = crate::tasks::closest(word, &names)
                    .map(|best| format!(" (did you mean '{best}'?)"))
                    .unwrap_or_default();
                bail!(
                    "unknown task '{word}'; available: {}{hint} \
                     (or pass -p \"<instruction>\" for an ad-hoc run)",
                    names.join(", ")
                );
            }
        }
    } else {
        None
    };

    // Without a task and without -p, file input has nothing to drive it.
    if task.is_none() && !prompt_seen && !slots.is_empty() {
        let example = slots
            .iter()
            .find_map(|s| match s {
                Slot::Free(p) | Slot::Literal(p) => p.to_str().map(|s| s.to_string()),
                _ => None,
            })
            .unwrap_or_else(|| "input.txt".to_string());
        bail!(
            "file input requires a task or -p, e.g. `aido ocr {example}` or \
             `aido -p \"<instructions>\" {example}` (see `aido tasks list`)"
        );
    }
    if task.is_none() && prompt_seen {
        return Ok(StageArgv {
            task: Some("ask".to_string()),
            specs: specs_from(slots, 0),
            argv: {
                let mut argv = vec![OsString::from("--__task"), OsString::from("ask")];
                argv.extend(rest);
                argv
            },
        });
    }

    match &task {
        Some(t) => {
            let mut argv = vec![OsString::from("--__task"), OsString::from(t)];
            argv.extend(rest);
            Ok(StageArgv {
                specs: specs_from(slots, 1),
                task,
                argv,
            })
        }
        None => Ok(StageArgv {
            task: None,
            specs: specs_from(slots, 0),
            argv: rest,
        }),
    }
}

// ---------------------------------------------------------------------------
// Task chains (`--then`, and the `chain "a | b"` sugar)
// ---------------------------------------------------------------------------

/// Flags that may appear OUTSIDE the chain spec string: they address the
/// run as a whole and merge into the last stage (the chain module's
/// `merge_run_flags`). `--produce`/`--format` are deliberately absent —
/// outside the last stage they are rejected, because every junction hands
/// off exactly one text artifact and no stage before the last may reshape
/// what it produces.
const CHAIN_OUTER_FLAGS: &[&str] = &[
    "--output",
    "--out-dir",
    "--copy",
    "--stdout",
    "--json",
    "--overwrite",
    "--quiet",
    "--stream",
    "--no-stream",
    "--dry-run",
    "--no-history",
    "--total-timeout",
    "--help",
    "--version",
];

/// Whether argv's first free token — the normalizer's notion: arity-aware,
/// `--`-terminated — is the chain word.
fn first_free_is_chain(argv: &[OsString]) -> bool {
    let mut iter = argv.iter();
    while let Some(token) = iter.next() {
        let Some(t) = token.to_str() else { continue };
        if t == "--" {
            // Past the separator everything is literal material.
            return false;
        }
        match flag_arity(t) {
            Some(true) if !value_attached(t) => {
                iter.next(); // the flag's value can never be the chain word
            }
            Some(_) => {}
            None => return t == "chain",
        }
    }
    false
}

/// Split argv on top-level `--then` markers; None when none is present.
/// The scan mirrors the normalizer's grammar: a value flag swallows its
/// value (the `--then` in `-p --then x` is a prompt, not a marker), and
/// everything after `--` is literal material that never splits.
fn split_on_then(argv: &[OsString]) -> Option<Vec<Vec<OsString>>> {
    let mut segments: Vec<Vec<OsString>> = vec![Vec::new()];
    let mut after_separator = false;
    let mut iter = argv.iter();
    while let Some(token) = iter.next() {
        let text = token.to_str();
        if !after_separator {
            match text {
                Some("--") => {
                    after_separator = true;
                    segments
                        .last_mut()
                        .expect("segments never empty")
                        .push(token.clone());
                    continue;
                }
                Some("--then") => {
                    segments.push(Vec::new());
                    continue;
                }
                // A value flag swallows its value: the `--then` in
                // `-p --then x` is a prompt, not a marker.
                Some(t) if flag_arity(t) == Some(true) && !value_attached(t) => {
                    segments
                        .last_mut()
                        .expect("segments never empty")
                        .push(token.clone());
                    if let Some(value) = iter.next() {
                        segments
                            .last_mut()
                            .expect("segments never empty")
                            .push(value.clone());
                    }
                    continue;
                }
                _ => {}
            }
        }
        segments
            .last_mut()
            .expect("segments never empty")
            .push(token.clone());
    }
    (segments.len() > 1).then_some(segments)
}

/// `aido chain "SPEC" [INPUT...] [OUTER_FLAGS...]`: split the spec into
/// stages, attach material to stage 1 and outer flags to the last stage
/// (position-independently — both address the run, not a stage), and
/// normalize each stage exactly as a standalone command line.
fn normalize_chain(argv: Vec<OsString>) -> Result<Normalized> {
    let mut spec: Option<String> = None;
    let mut material: Vec<OsString> = Vec::new();
    let mut outer: Vec<OsString> = Vec::new();
    let mut after_separator = false;
    let mut free_seen = 0usize;
    let mut iter = argv.into_iter().peekable();
    while let Some(token) = iter.next() {
        let text = token.to_str().map(|s| s.to_string());
        if after_separator {
            material.push(token);
            continue;
        }
        if let Some(t) = &text {
            if t == "--" {
                after_separator = true;
                material.push(token);
                continue;
            }
            if t == "--then" {
                bail!("--then and chain cannot mix: pick one spelling");
            }
            if let Some((flag, guidance)) = REMOVED_FLAGS
                .iter()
                .find(|(f, _)| t == *f || t.starts_with(&format!("{f}=")))
            {
                bail!("'{flag}' is no longer accepted: {guidance}");
            }
            // Material flags feed stage 1 wherever they appear.
            if t == "--text" || t.starts_with("--text=") {
                material.push(token.clone());
                if t == "--text" {
                    let value = take_value("--text", &mut iter)?;
                    material.push(OsString::from(value));
                }
                continue;
            }
            if t == "--paste" {
                material.push(token);
                continue;
            }
            // A flag token: run-level flags ride to the last stage; a
            // stage-level one outside the spec has no home.
            if t.starts_with('-') && t != "-" {
                let Some(long) = flag_long_name(t) else {
                    // Unknown flags reach clap through the last stage, so a
                    // typo keeps its usual "unexpected argument" treatment.
                    outer.push(token);
                    continue;
                };
                if CHAIN_OUTER_FLAGS.contains(&long) {
                    outer.push(token.clone());
                    if flag_arity(t) == Some(true) && !value_attached(t) {
                        let value = iter
                            .next()
                            .ok_or_else(|| anyhow!("{long} requires a value"))?;
                        outer.push(value);
                    }
                } else {
                    bail!(
                        "'{long}' configures one stage, not the run: put it inside the \
                         chain spec string, on the stage it configures (or use the \
                         --then form)"
                    );
                }
                continue;
            }
        } else if let Some(flag) = text_flag_shape(token.as_encoded_bytes()) {
            return Err(invalid_text_value(flag));
        }
        // A free token: the chain word, the spec, or material.
        free_seen += 1;
        match free_seen {
            1 => {
                // first_free_is_chain skips tokens it cannot read as
                // UTF-8, so a non-UTF-8 entry can occupy this slot. It is
                // a file the user meant to run a task on — say so instead
                // of silently swallowing it and misreading the spec.
                if text.as_deref() != Some("chain") {
                    bail!(
                        "file input requires a task or -p, e.g. \
                         `aido ocr <FILE> --then <TASK>` (see `aido tasks list`)"
                    );
                }
            }
            2 => {
                spec =
                    Some(text.ok_or_else(|| anyhow!("the chain spec must be valid UTF-8 text"))?);
            }
            _ => material.push(token),
        }
    }
    let Some(spec) = spec else {
        // `aido chain --help` (no spec yet): hand the outer flags to clap
        // so help and version keep working — the natural way to discover
        // the chain grammar.
        if outer.iter().any(|t| {
            matches!(
                t.to_str(),
                Some("--help") | Some("-h") | Some("--version") | Some("-V")
            )
        }) {
            return Ok(Normalized::Single {
                task: None,
                specs: Vec::new(),
                argv: outer,
            });
        }
        bail!(
            "chain requires a spec string: aido chain \"TASK [FLAGS] | TASK [FLAGS]\" \
             [INPUT...] [OPTIONS]"
        )
    };
    let mut stages_tokens = tokenize_chain_spec(&spec)?;
    if stages_tokens.len() < 2 {
        let single = stages_tokens.pop().unwrap_or_default().join(" ");
        bail!("a chain needs at least two stages; run this task directly: aido {single}");
    }
    let n = stages_tokens.len();
    let mut stages = Vec::new();
    for (i, tokens) in stages_tokens.into_iter().enumerate() {
        let mut segment: Vec<OsString> = tokens.into_iter().map(OsString::from).collect();
        if i == 0 {
            segment.extend(material.iter().cloned());
        }
        if i + 1 == n {
            segment.extend(outer.iter().cloned());
        }
        stages.push(normalize_stage(segment)?);
    }
    Ok(Normalized::Chain { stages })
}

/// Split a chain spec into stages of shell-like tokens. One scan makes
/// both cuts: `|` (outside quotes) ends a stage, whitespace (outside
/// quotes) ends a token. Quotes are `'...'` and `"..."`; there are no
/// escapes — inside quotes every character is literal, which is what
/// keeps one spelling valid on every platform. `''` contributes an empty
/// argument, as in a shell.
fn tokenize_chain_spec(spec: &str) -> Result<Vec<Vec<String>>> {
    let mut stages: Vec<Vec<String>> = vec![Vec::new()];
    let mut token = String::new();
    let mut started = false; // the token holds content or a quoted empty
    let mut quote: Option<char> = None;
    for ch in spec.chars() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                token.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                started = true;
            }
            '|' => {
                if started {
                    stages
                        .last_mut()
                        .expect("stages never empty")
                        .push(std::mem::take(&mut token));
                }
                started = false;
                stages.push(Vec::new());
            }
            c if c.is_whitespace() => {
                if started {
                    stages
                        .last_mut()
                        .expect("stages never empty")
                        .push(std::mem::take(&mut token));
                    started = false;
                }
            }
            c => {
                token.push(c);
                started = true;
            }
        }
    }
    if let Some(q) = quote {
        // `stages.len()` is the 1-based number of the stage being scanned
        // when the spec ran out of characters.
        bail!(
            "chain spec stage {} ends inside an unclosed {q} quote; chain specs have \
             no escapes — when a stage argument needs both quote kinds, use the \
             --then form, whose arguments are ordinary shell tokens",
            stages.len()
        );
    }
    if started {
        stages.last_mut().expect("stages never empty").push(token);
    }
    for (i, stage) in stages.iter().enumerate() {
        if stage.is_empty() {
            bail!("chain spec stage {} is empty", i + 1);
        }
    }
    Ok(stages)
}

/// The canonical long form of a flag token, for chain classification:
/// `--out-dir`, `-o`, `-ofile`, `-o=file` and `--flag=v` all resolve to
/// their FLAGS-table name. None for non-flags and unknown flags.
fn flag_long_name(token: &str) -> Option<&'static str> {
    let bare = token.split_once('=').map(|(name, _)| name).unwrap_or(token);
    if let Some(("__task", _)) = FLAGS.iter().find(|(f, _)| *f == bare) {
        return None; // internal flag, never user-writable
    }
    if let Some((name, _)) = FLAGS.iter().find(|(f, _)| *f == bare) {
        return Some(name);
    }
    for (short, long) in SHORT_VALUE_FLAGS {
        if bare == *short {
            return Some(long);
        }
        if bare.starts_with(short) && bare.len() > 2 && short_attached(token).is_some() {
            return Some(long);
        }
    }
    None
}

/// A word that names no task but looks like a file path is input rather
/// than a typo — point at the task requirement instead of "unknown task".
/// Keep diagnostics independent of filesystem state and automount probes.
fn looks_like_path(word: &str) -> bool {
    word.contains('/')
        || (cfg!(windows) && word.contains('\\'))
        || word.contains('.')
        || word.starts_with('~')
        || has_glob_metachars(word)
}

/// Short flags that take a value, and the long flag each stands for.
const SHORT_VALUE_FLAGS: &[(&str, &str)] =
    &[("-p", "--prompt"), ("-m", "--model"), ("-o", "--output")];

/// An attached short value and the long flag it stands for: `-pfoo` →
/// `("--prompt", "foo")`, `-mfoo` → `("--model", "foo")`, `-ofile` →
/// `("--output", "file")`. The `=` form (`-p=foo`) resolves the same way.
fn short_attached(token: &str) -> Option<(&'static str, String)> {
    for (short, long) in SHORT_VALUE_FLAGS {
        let Some(value) = token.strip_prefix(short) else {
            continue;
        };
        if let Some(v) = value.strip_prefix('=') {
            if !v.is_empty() {
                return Some((long, v.to_string()));
            }
        } else if !value.is_empty() {
            return Some((long, value.to_string()));
        }
    }
    None
}

/// The error every text-flag spelling raises for a non-UTF-8 value:
/// refusing beats the lossy fallback, which would hand the model U+FFFD
/// in place of the user's bytes with no warning. File paths never take
/// this path — they keep their raw `OsString` end to end.
fn invalid_text_value(flag: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{flag} requires a valid UTF-8 value (a lossy conversion would \
         silently corrupt the text)"
    )
}

/// Validate a text-flag value (`--text`, `--prompt`/`-p`): the words the
/// model must see verbatim, so the bytes have to be UTF-8.
fn text_value(flag: &str, value: &OsString) -> Result<String> {
    value
        .to_str()
        .map(|v| v.to_string())
        .ok_or_else(|| invalid_text_value(flag))
}

/// Take the next argv token as a separated text-flag value: present, and
/// valid UTF-8 ([`text_value`]).
fn take_value(
    flag: &str,
    iter: &mut std::iter::Peekable<std::vec::IntoIter<OsString>>,
) -> Result<String> {
    let Some(value) = iter.next() else {
        bail!("{flag} requires a value");
    };
    text_value(flag, &value)
}

/// The text flag a non-UTF-8 argv entry spells, if any: `--text=<bytes>`
/// and `--prompt=<bytes>` combined, `-p<bytes>` (and `-p=<bytes>`)
/// attached.
fn text_flag_shape(bytes: &[u8]) -> Option<&'static str> {
    for (prefix, flag) in [("--text=", "--text"), ("--prompt=", "--prompt")] {
        if bytes.starts_with(prefix.as_bytes()) && bytes.len() > prefix.len() {
            return Some(flag);
        }
    }
    (bytes.starts_with(b"-p") && bytes.len() > 2).then_some("-p/--prompt")
}

/// Turn slots into specs, skipping the first `skip` free tokens (the task
/// token and friends) while keeping text/paste slots in place. `skip`
/// beyond the slot count drops every free token. A free token with glob
/// metacharacters becomes a `Glob` spec for gather-time expansion; tokens
/// that followed `--` stay literal files.
fn specs_from(slots: Vec<Slot>, skip: usize) -> Vec<SourceSpec> {
    let mut skipped = 0;
    let mut specs = Vec::new();
    for slot in slots {
        let (token, literal) = match slot {
            Slot::Text(value) => {
                specs.push(SourceSpec::Text(value));
                continue;
            }
            Slot::Paste => {
                specs.push(SourceSpec::Paste);
                continue;
            }
            Slot::Free(token) => (token, false),
            Slot::Literal(token) => (token, true),
        };
        if skipped < skip {
            skipped += 1;
            continue;
        }
        if token == "-" {
            specs.push(SourceSpec::Stdin);
        } else if !literal && token.to_str().is_some_and(has_glob_metachars) {
            specs.push(SourceSpec::Glob(token.to_string_lossy().into_owned()));
        } else {
            specs.push(SourceSpec::File(PathBuf::from(token)));
        }
    }
    specs
}

/// True when a token may be an unexpanded pattern: the shell either was
/// not asked (`*` stayed quoted) or cannot expand at all (Windows). A
/// literal filename that happens to contain these characters still wins
/// if it exists — that is checked at gather time, where the filesystem
/// is available.
fn has_glob_metachars(token: &str) -> bool {
    token.contains('*') || token.contains('?') || token.contains('[')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Mp3,
    Opus,
    Aac,
    Flac,
    Wav,
    Pcm,
    Png,
    Jpeg,
    Webp,
}

impl std::fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Mp3 => "mp3",
            Self::Opus => "opus",
            Self::Aac => "aac",
            Self::Flac => "flac",
            Self::Wav => "wav",
            Self::Pcm => "pcm",
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Webp => "webp",
        };
        f.write_str(s)
    }
}

/// Run a task over material and deliver the result.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "aido",
    version,
    about = "Send material to an AI task, deliver the result",
    override_usage = "aido <TASK> [INPUT...] [OPTIONS]\n    aido run <TASK> [INPUT...] [OPTIONS]\n    aido ask [INPUT...] -p <INSTRUCTION> [OPTIONS]\n    aido -p <INSTRUCTION> [INPUT...] [OPTIONS]\n    aido <TASK> [INPUT...] --then <TASK> [...] [OPTIONS]\n    aido chain \"TASK [FLAGS] | TASK [FLAGS]\" [INPUT...] [OPTIONS]",
    after_help = "INPUT is one or more of:\n  FILE            input file(s) in the order given\n  GLOB            pattern aido expands itself (quote it when the shell must not):\n                  matches in sorted order; no match is an error\n  DIR             directory: its files one level deep, sorted;\n                  dotfiles are skipped, subdirectories are refused\n                  (use a glob to descend)\n  PDF             each page's embedded images and text become material,\n                  in page order (vector-only PDFs need the pdfium build)\n  XLSX            each non-empty sheet becomes one text part, as a table\n  -               read stdin at this position (at most once)\n  --text TEXT     literal text material (repeatable)\n  --paste         read the clipboard at this position\n  -p TEXT         instruction for this run (not material)\n\nTask chains (--then, or the chain sugar) run several stages in one\n  run: stage parameters (--to, --voice, -p, --profile, ...) go on\n  their own stage; run-level ones (-o, --json, --dry-run, ...) on the\n  chain. Only the last stage delivers; every stage's artifact lands in\n  the record. When a stage argument needs shell expansion or both\n  quote kinds, use --then: its arguments are ordinary shell tokens.\n\nManagement: aido tasks|profiles|config|history ... and aido last\n\nExamples:\n  aido ocr screenshot.png --copy\n  aido ocr \"shots/*.png\" --copy\n  aido ocr shots/\n  git diff | aido code-review -\n  aido translate article.md --to zh-CN\n  aido tts --text \"你好\" -o hello.mp3\n  aido image --text \"a dog\" --count 2 --out-dir dogs/\n  aido ask a.png b.png -p \"比较两图\"\n  aido ask picture-book.pdf -p \"把这本绘本讲成旁白和对话\" | aido tts -o story.mp3\n  aido chain \"ocr | translate --to zh-CN | tts\" shot.png -o brief.mp3 --copy\n  aido ocr screenshot.png --dry-run"
)]
pub struct Cli {
    /// Task name, set by the normalizer (use `aido run NAME` explicitly)
    #[arg(long = "__task", hide = true)]
    pub task: Option<String>,

    /// Instruction for this run; for `ask` it is the task itself
    #[arg(short = 'p', long = "prompt", value_name = "INSTRUCTION")]
    pub prompt: Option<String>,

    /// Save exactly one artifact to FILE ("-" for stdout)
    #[arg(short = 'o', long, value_name = "FILE", conflicts_with = "out_dir")]
    pub output: Option<PathBuf>,

    /// Save the full artifact set plus a manifest into DIR
    #[arg(long, value_name = "DIR")]
    pub out_dir: Option<PathBuf>,

    /// Write a single text or image artifact to the clipboard
    #[arg(short = 'c', long)]
    pub copy: bool,

    /// Write the result body (or a single media artifact) to stdout
    #[arg(long, conflicts_with = "json")]
    pub stdout: bool,

    /// Content types to request (normally the task's choice)
    #[arg(long, value_enum, value_delimiter = ',')]
    pub produce: Vec<MediaKind>,

    /// Encoding for a single media output type
    #[arg(long, value_name = "FORMAT")]
    pub format: Option<OutputFormat>,

    /// Print a versioned run report on stdout instead of the body
    #[arg(long)]
    pub json: bool,

    /// Replace an existing output file instead of failing
    #[arg(long)]
    pub overwrite: bool,

    /// Hide progress and success notes (errors still print)
    #[arg(long)]
    pub quiet: bool,

    /// Config profile to use
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,

    /// Model name (overrides only the profile's model)
    #[arg(short = 'm', long)]
    pub model: Option<String>,

    /// Max completion tokens (default: not sent)
    #[arg(long)]
    pub max_tokens: Option<u64>,

    /// Sampling temperature
    #[arg(long)]
    pub temperature: Option<f64>,

    /// Adapter extension option KEY=VALUE
    #[arg(long = "option", value_name = "KEY=VALUE")]
    pub options: Vec<String>,

    /// Header-wait and network-idle timeout in seconds (default 120)
    #[arg(long, value_name = "SECS")]
    pub timeout: Option<u64>,

    /// Cap the whole run's duration (default: no cap)
    #[arg(long, value_name = "SECS")]
    pub total_timeout: Option<u64>,

    /// Deliver stdout in real time as it is generated
    #[arg(long, overrides_with = "no_stream")]
    pub stream: bool,

    /// Buffer the reply instead of streaming it
    #[arg(long, overrides_with = "stream")]
    pub no_stream: bool,

    /// Show the execution plan and exit; no request is sent
    #[arg(long)]
    pub dry_run: bool,

    /// Do not record this run in history
    #[arg(long)]
    pub no_history: bool,

    /// Target language (translate)
    #[arg(long, value_name = "LANG")]
    pub to: Option<String>,

    /// Voice (tts)
    #[arg(long)]
    pub voice: Option<String>,

    /// Speech speed 0.25..=4 (tts)
    #[arg(long)]
    pub speed: Option<f64>,

    /// Number of images 1..=10 (image)
    #[arg(long)]
    pub count: Option<u64>,

    /// Image size WxH (image)
    #[arg(long, value_name = "WxH")]
    pub size: Option<String>,

    /// Send inputs whole instead of slicing/chunking (ocr, summarize, translate)
    #[arg(long)]
    pub no_split: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Commands {
    /// List available tasks
    Tasks {
        #[command(subcommand)]
        cmd: TasksCmd,
    },
    /// List configured profiles
    Profiles,
    /// Create or inspect the configuration
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Browse past runs
    History {
        #[command(subcommand)]
        cmd: HistoryCmd,
    },
    /// Internal: hold clipboard contents in the background (Linux)
    #[command(name = "__hold", hide = true)]
    Hold {
        #[arg(long)]
        image: bool,
        /// Seconds to keep the clipboard alive
        secs: u64,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum TasksCmd {
    /// List all tasks
    List,
    /// Show one task's definition
    Show { task: String },
}

#[derive(Debug, Clone, Subcommand)]
pub enum ConfigCmd {
    /// Write a sample config file
    Init,
    /// Validate the current config
    Check,
}

#[derive(Debug, Clone, Subcommand)]
pub enum HistoryCmd {
    /// List recorded runs, oldest first (newest last); the index
    /// addresses `show` (1 = newest)
    List,
    /// Redeliver a recorded run
    Show {
        /// Which run: an index from `history list` (1 = newest), a run id,
        /// or a unique prefix of one
        #[arg(value_name = "RUN")]
        target: String,
        // Delivery flags (-o/--output, --out-dir, --copy, --stdout, --json,
        // --overwrite, --quiet) must live ONLY on the top-level `Cli`: the
        // normalizer collects every flag before the management words, so
        // `aido history show 1 --copy` reaches clap as
        // `[--copy, history, show, 1]` and `Cli` assigns `--copy`. A
        // duplicate declared here could never be assigned — it would parse
        // as `None`/`false` while looking real. app.rs reads the flags
        // straight off `Cli`.
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    /// The single-run view the historical tests are written against:
    /// unwraps [`Normalized::Single`]. A chain invocation panics here —
    /// chain tests use [`stages_of`].
    fn normalize(args: Vec<OsString>) -> Result<StageArgv> {
        match super::normalize(args) {
            Ok(Normalized::Single { task, specs, argv }) => Ok(StageArgv { task, specs, argv }),
            Ok(Normalized::Chain { .. }) => panic!("expected a single-run normalization"),
            Err(e) => Err(e),
        }
    }

    /// The chain view: unwraps [`Normalized::Chain`] into its stages.
    fn stages_of(args: &[&str]) -> Vec<StageArgv> {
        match super::normalize(os(args)).unwrap() {
            Normalized::Chain { stages } => stages,
            Normalized::Single { .. } => panic!("expected a chain normalization"),
        }
    }

    fn task_of(args: &[&str]) -> String {
        normalize(os(args)).unwrap().task.unwrap()
    }

    #[test]
    fn task_can_come_before_or_after_flags() {
        assert_eq!(task_of(&["--profile", "local", "ocr", "scan.png"]), "ocr");
        assert_eq!(task_of(&["--copy", "ocr", "scan.png"]), "ocr");
        assert_eq!(task_of(&["ocr", "--profile", "local", "scan.png"]), "ocr");
    }

    #[test]
    fn run_takes_the_next_free_token() {
        assert_eq!(task_of(&["run", "last", "input.txt"]), "last");
        assert_eq!(task_of(&["run", "--profile", "x", "ocr"]), "ocr");
        assert!(normalize(os(&["run"])).is_err());
    }

    #[test]
    fn ask_via_flag_or_word() {
        assert_eq!(task_of(&["-p", "hi", "notes.md"]), "ask");
        assert_eq!(task_of(&["ask", "notes.md", "-p", "hi"]), "ask");
        assert_eq!(task_of(&["--prompt=hi"]), "ask");
        assert_eq!(task_of(&["-phi"]), "ask");
    }

    #[test]
    fn specs_keep_cross_type_order() {
        let n = normalize(os(&[
            "ask",
            "--text",
            "图一",
            "a.png",
            "--text",
            "图二",
            "b.png",
            "-p",
            "比较两图",
        ]))
        .unwrap();
        assert_eq!(
            n.specs,
            vec![
                SourceSpec::Text("图一".into()),
                SourceSpec::File(PathBuf::from("a.png")),
                SourceSpec::Text("图二".into()),
                SourceSpec::File(PathBuf::from("b.png")),
            ]
        );
    }

    #[test]
    fn dash_and_paste_are_slots() {
        let n = normalize(os(&["code-review", "-", "CONTRIBUTING.md"])).unwrap();
        assert_eq!(
            n.specs,
            vec![
                SourceSpec::Stdin,
                SourceSpec::File(PathBuf::from("CONTRIBUTING.md"))
            ]
        );
        let n = normalize(os(&["ocr", "--paste"])).unwrap();
        assert_eq!(n.specs, vec![SourceSpec::Paste]);
    }

    #[test]
    fn separator_makes_everything_a_file() {
        let n = normalize(os(&["ocr", "--", "./-strange-name.png"])).unwrap();
        assert_eq!(
            n.specs,
            vec![SourceSpec::File(PathBuf::from("./-strange-name.png"))]
        );
    }

    #[test]
    fn glob_tokens_become_glob_specs() {
        let n = normalize(os(&["ocr", "shots/*.png"])).unwrap();
        assert_eq!(n.specs, vec![SourceSpec::Glob("shots/*.png".into())]);
        // `?` and `[` are metacharacters too.
        let n = normalize(os(&["ocr", "shot-?.png"])).unwrap();
        assert_eq!(n.specs, vec![SourceSpec::Glob("shot-?.png".into())]);
        // A directory argument parses as a plain File; expansion is
        // gather's job, where the filesystem lives.
        let n = normalize(os(&["ocr", "shots/"])).unwrap();
        assert_eq!(n.specs, vec![SourceSpec::File(PathBuf::from("shots/"))]);
        // `--text` values are literal text, never patterns.
        let n = normalize(os(&["ask", "--text", "a*b"])).unwrap();
        assert_eq!(n.specs, vec![SourceSpec::Text("a*b".into())]);
    }

    #[test]
    fn separator_tokens_stay_literal_files() {
        let n = normalize(os(&["ocr", "--", "shots/*.png"])).unwrap();
        assert_eq!(
            n.specs,
            vec![SourceSpec::File(PathBuf::from("shots/*.png"))]
        );
        // `-` stays stdin even after `--`, as at every other position.
        let n = normalize(os(&["ocr", "--", "-"])).unwrap();
        assert_eq!(n.specs, vec![SourceSpec::Stdin]);
        // The ask path produces the same Glob spec.
        let n = normalize(os(&["-p", "hi", "shots/*.png"])).unwrap();
        assert_eq!(n.specs, vec![SourceSpec::Glob("shots/*.png".into())]);
    }

    #[test]
    fn path_hints_are_lexical() {
        for word in [
            "/missing",
            "dir/file",
            "dir/",
            ".",
            "..",
            ".hidden",
            "missing-file.txt",
            "~",
            "~user",
            "*",
            "file?",
            "file[abc]",
            "file[",
        ] {
            assert!(looks_like_path(word), "{word:?}");
        }
        for word in ["", "transalte", "missing-file", "name~", "name]"] {
            assert!(!looks_like_path(word), "{word:?}");
        }
        // F64: even an existing bare directory is not a lexical path hint;
        // callers can use `./src` to request the file-input diagnostic.
        assert!(!looks_like_path("src"));
        assert_eq!(looks_like_path(r"dir\file"), cfg!(windows));
    }

    #[test]
    fn lexical_path_hints_select_file_input_guidance() {
        for word in ["missing-file.txt", "~user", "*", "file?", "file["] {
            let err = normalize(os(&[word])).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("requires a task"), "{word:?}: {msg}");
            assert!(!msg.contains("unknown task"), "{word:?}: {msg}");
        }
        let err = normalize(os(&["missing-file"])).unwrap_err();
        assert!(err.to_string().contains("unknown task"), "{err}");
    }

    #[test]
    fn files_without_task_or_prompt_are_an_error() {
        let err = normalize(os(&["notes.txt"])).unwrap_err();
        assert!(err.to_string().contains("requires a task"));
    }

    #[test]
    fn unknown_task_without_prompt_lists_candidates() {
        let err = normalize(os(&["transalte"])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown task"), "{msg}");
        assert!(msg.contains("translate"), "{msg}");
    }

    #[test]
    fn prompt_like_first_word_with_p_becomes_ask_file() {
        // -p present: a non-task word in the task slot is a file, not an error
        let n = normalize(os(&["-p", "润色这段话", "笔记.txt"])).unwrap();
        assert_eq!(n.task.as_deref(), Some("ask"));
        assert_eq!(n.specs, vec![SourceSpec::File(PathBuf::from("笔记.txt"))]);
    }

    #[test]
    fn removed_flags_error_with_guidance() {
        for flag in [
            "--save",
            "--save-dir",
            "--input-mode",
            "--adapter",
            "--base-url",
            "--api-key",
            "--preset",
            "--init",
            "--list-presets",
            "--no-spinner",
            "--output-mode",
        ] {
            let err = normalize(os(&[flag, "x", "ask", "-p", "hi"])).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("no longer accepted"), "{flag}: {msg}");
        }
    }

    #[test]
    fn last_is_a_pseudo_task_keeping_flags() {
        let n = normalize(os(&["--copy", "last"])).unwrap();
        assert_eq!(n.task.as_deref(), Some(LAST_TASK));
        assert!(n.argv.contains(&OsString::from("--copy")));
        let n = normalize(os(&["last", "--out-dir", "d"])).unwrap();
        assert_eq!(n.task.as_deref(), Some(LAST_TASK));
    }

    #[test]
    fn management_subcommands_pass_through() {
        let n = normalize(os(&["tasks", "list"])).unwrap();
        assert_eq!(n.task, None);
        assert_eq!(n.argv[0], OsString::from("tasks"));
        let n = normalize(os(&["history", "show", "x"])).unwrap();
        assert_eq!(n.argv[1], OsString::from("show"));
        let n = normalize(os(&["config", "init"])).unwrap();
        assert_eq!(n.argv[0], OsString::from("config"));
        let n = normalize(os(&["profiles"])).unwrap();
        assert_eq!(n.argv[0], OsString::from("profiles"));
    }

    #[test]
    fn hold_passes_through_untouched() {
        // The clipboard holder child is spawned as `aido __hold SECS
        // [--image]` with the word always first; task discovery would
        // reject it ("unknown task '__hold'"), so argv must reach clap
        // exactly as spawned.
        let n = normalize(os(&["__hold", "5", "--image"])).unwrap();
        assert_eq!(n.task, None);
        assert!(n.specs.is_empty());
        assert_eq!(n.argv, os(&["__hold", "5", "--image"]));
    }

    #[test]
    fn flag_values_are_not_mistaken_for_tasks() {
        assert_eq!(task_of(&["--profile", "local", "ocr", "a.png"]), "ocr");
        assert_eq!(
            task_of(&["--to", "zh-CN", "translate", "a.md"]),
            "translate"
        );
    }

    #[test]
    fn bare_argv_yields_no_task() {
        let n = normalize(os(&[])).unwrap();
        assert_eq!(n.task, None);
    }

    #[test]
    fn attached_short_values_map_to_their_own_flags() {
        // -mVALUE must reach --model, never collapse into --prompt.
        let n = normalize(os(&["summarize", "-mmodel-x", "--text", "hi"])).unwrap();
        let i = n.argv.iter().position(|a| a == "--model").unwrap();
        assert_eq!(n.argv[i + 1], OsString::from("model-x"));
        assert!(
            !n.argv.contains(&OsString::from("--prompt")),
            "{:?}",
            n.argv
        );
        // -oVALUE reaches --output.
        let n = normalize(os(&["ask", "-phi", "-ofile.mp3"])).unwrap();
        let i = n.argv.iter().position(|a| a == "--output").unwrap();
        assert_eq!(n.argv[i + 1], OsString::from("file.mp3"));
        let i = n.argv.iter().position(|a| a == "--prompt").unwrap();
        assert_eq!(n.argv[i + 1], OsString::from("hi"));
    }

    #[test]
    fn attached_short_equals_form_resolves() {
        let n = normalize(os(&["summarize", "-m=model-x"])).unwrap();
        let i = n.argv.iter().position(|a| a == "--model").unwrap();
        assert_eq!(n.argv[i + 1], OsString::from("model-x"));
    }

    #[test]
    fn text_flag_accepts_the_equals_form() {
        let n = normalize(os(&["ask", "--text=hi"])).unwrap();
        assert_eq!(n.specs, vec![SourceSpec::Text("hi".into())]);
    }

    #[test]
    fn value_flags_require_their_values() {
        let err = normalize(os(&["ask", "--text"])).unwrap_err();
        assert!(err.to_string().contains("--text requires a value"), "{err}");
        let err = normalize(os(&["ask", "-p"])).unwrap_err();
        assert!(err.to_string().contains("requires a value"), "{err}");
    }

    #[test]
    fn unknown_flag_with_value_is_not_a_file() {
        let n = normalize(os(&["summarize", "--text", "hi", "--typo=x"])).unwrap();
        // The token stays in the clap argv (which rejects it); it never
        // becomes an input file.
        assert!(n.argv.contains(&OsString::from("--typo=x")), "{:?}", n.argv);
        assert!(n.specs.iter().all(|s| !matches!(s, SourceSpec::File(_))));
    }

    #[test]
    fn negative_numbers_stay_flag_values() {
        let n = normalize(os(&["ask", "-p", "hi", "--temperature", "-0.5"])).unwrap();
        assert!(
            n.argv.contains(&OsString::from("--temperature=-0.5")),
            "{:?}",
            n.argv
        );
    }

    /// Normalize + clap, exactly what app.rs does with real argv: the
    /// strongest assertion a spelling works is that the real parser
    /// accepts it and keeps the value.
    fn parse_cli(args: &[&str]) -> Cli {
        let n = normalize(os(args)).unwrap();
        Cli::try_parse_from(std::iter::once(OsString::from("aido")).chain(n.argv.clone()))
            .unwrap_or_else(|e| panic!("{args:?}: {e}; argv: {:?}", n.argv))
    }

    #[test]
    fn leading_dash_prompt_values_parse_end_to_end() {
        // Every spelling of a prompt that starts with '-' must reach clap
        // with the value still attached, or clap rejects it as a flag.
        for args in [
            &["ask", "--prompt", "-x"][..],
            &["ask", "--prompt=-x"][..],
            &["ask", "-p", "-x"][..],
            &["ask", "-p-x"][..],
            &["ask", "-p=-x"][..],
        ] {
            let cli = parse_cli(args);
            assert_eq!(cli.prompt.as_deref(), Some("-x"), "{args:?}");
            assert_eq!(cli.task.as_deref(), Some("ask"), "{args:?}");
        }
    }

    #[test]
    fn leading_dash_attached_short_values_parse_end_to_end() {
        // -m-x / -o-out.txt keep the dash inside the combined long form.
        let cli = parse_cli(&["ask", "-m-x", "-p", "hi"]);
        assert_eq!(cli.model.as_deref(), Some("-x"));
        let cli = parse_cli(&["ask", "-o-out.txt", "-p", "hi"]);
        assert_eq!(cli.output, Some(PathBuf::from("-out.txt")));
    }

    #[test]
    fn non_dash_and_lone_dash_values_stay_separated() {
        // Without a leading dash the two-token form is unchanged; the
        // lone `-` is a value (stdin elsewhere), never a flag.
        for args in [&["ask", "-phi"][..], &["ask", "--prompt", "hi"][..]] {
            let n = normalize(os(args)).unwrap();
            let i = n.argv.iter().position(|a| a == "--prompt").unwrap();
            assert_eq!(n.argv[i + 1], OsString::from("hi"), "{args:?}");
        }
        for args in [&["ask", "--prompt", "-"][..], &["ask", "--prompt=-"][..]] {
            let cli = parse_cli(args);
            assert_eq!(cli.prompt.as_deref(), Some("-"), "{args:?}");
        }
    }

    #[test]
    fn help_and_version_are_words_not_tasks() {
        let n = normalize(os(&["help"])).unwrap();
        assert_eq!(n.argv, vec![OsString::from("--help")]);
        let n = normalize(os(&["version"])).unwrap();
        assert_eq!(n.argv, vec![OsString::from("--version")]);
    }

    #[test]
    fn last_rejects_input_material() {
        let err = normalize(os(&["last", "notes.txt"])).unwrap_err();
        assert!(err.to_string().contains("takes no input"), "{err}");
        let n = normalize(os(&["last", "--copy"])).unwrap();
        assert_eq!(n.task.as_deref(), Some(LAST_TASK));
    }

    #[test]
    fn prompt_has_no_effect_on_management_commands() {
        let err = normalize(os(&["-p", "hi", "tasks", "list"])).unwrap_err();
        assert!(err.to_string().contains("no effect"), "{err}");
    }

    #[test]
    fn material_flags_have_no_effect_on_management_commands() {
        // Every management word, both material flags, both spellings of
        // --text: the material must be refused, not silently dropped.
        for args in [
            &["tasks", "list", "--text", "x"][..],
            &["profiles", "--paste"][..],
            &["config", "check", "--text", "x"][..],
            &["history", "show", "1", "--paste"][..],
            &["tasks", "list", "--text=x"][..],
        ] {
            let err = normalize(os(args)).unwrap_err();
            assert!(
                err.to_string().contains("no effect on management commands"),
                "{args:?}: {err}"
            );
        }
    }

    #[test]
    fn management_passthrough_without_material() {
        // No material, no refusal: the words reach clap untouched.
        for args in [&["tasks", "list"][..], &["history", "show", "1"][..]] {
            let n = normalize(os(args)).unwrap();
            assert!(n.task.is_none(), "{args:?}");
            assert!(n.specs.is_empty(), "{args:?}");
            assert!(
                n.argv.contains(&OsString::from(args[0])),
                "{args:?}: {:?}",
                n.argv
            );
        }
    }

    #[test]
    fn management_keeps_post_separator_literals() {
        let n = normalize(os(&["history", "show", "--", "--weird-id"])).unwrap();
        assert!(
            n.argv.contains(&OsString::from("--weird-id")),
            "{:?}",
            n.argv
        );
    }

    #[test]
    fn wants_json_scan_mirrors_flag_arity() {
        // A "--json" that is a value-taking flag's value is not a request.
        assert!(!wants_json_in(&os(&["ask", "-p", "--json"])));
        assert!(!wants_json_in(&os(&["summarize", "--text", "--json"])));
        assert!(!wants_json_in(&os(&["--prompt=--json"])));
        assert!(!wants_json_in(&os(&["-p=--json"])));
        assert!(!wants_json_in(&os(&["-p--json"])));
        assert!(!wants_json_in(&os(&["-m", "--json"])));
        assert!(!wants_json_in(&os(&["-o", "--json"])));
        assert!(!wants_json_in(&os(&["--model", "--json"])));
        // Past the separator everything is a literal path.
        assert!(!wants_json_in(&os(&["--", "--json"])));
        assert!(!wants_json_in(&os(&["ask", "--", "--json"])));
        // A real --json flag, wherever it stands.
        assert!(wants_json_in(&os(&["--json"])));
        assert!(wants_json_in(&os(&["--json", "--bogus"])));
        assert!(wants_json_in(&os(&["ask", "--json", "-p", "hi"])));
        assert!(wants_json_in(&os(&["--text", "hi", "--json"])));
        // `-p` swallows the separator as its value, so a later --json
        // still parses as the flag — the normalizer consumes it too.
        assert!(wants_json_in(&os(&["-p", "--", "--json"])));
    }

    #[cfg(unix)]
    #[test]
    fn wants_json_scan_handles_non_utf8_tokens() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![0xff, 0xfe]);
        // A non-UTF-8 token is a file path, never a flag: the scan goes on.
        assert!(wants_json_in(&[bad.clone(), OsString::from("--json")]));
        // A value flag still swallows it; the skip works on OsStrings.
        assert!(wants_json_in(&[
            OsString::from("-p"),
            bad,
            OsString::from("--json")
        ]));
    }

    #[cfg(unix)]
    #[test]
    fn text_flag_values_must_be_valid_utf8() {
        use std::os::unix::ffi::OsStringExt;
        // "你好" in GBK: real bytes a Chinese Windows shell would pass, and
        // invalid UTF-8 — a lossy conversion would hand the model two
        // U+FFFD instead of the words the user typed.
        let gbk = OsString::from_vec(vec![0xc4, 0xe3, 0xba, 0xc3]);
        // Every separated spelling refuses the bytes and names its flag.
        for (flag, argv) in [
            (
                "--text",
                vec![OsString::from("ask"), OsString::from("--text"), gbk.clone()],
            ),
            (
                "--prompt",
                vec![
                    OsString::from("ask"),
                    OsString::from("--prompt"),
                    gbk.clone(),
                ],
            ),
            (
                "-p/--prompt",
                vec![OsString::from("ask"), OsString::from("-p"), gbk.clone()],
            ),
        ] {
            let err = normalize(argv).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains(flag), "{flag}: {msg}");
            assert!(msg.contains("valid UTF-8"), "{flag}: {msg}");
        }
        // Combined spellings embed the bytes in the token itself; the same
        // refusal fires instead of the token quietly becoming a file named
        // after the flag.
        for (flag, token) in [
            ("--text", concat_bytes(b"--text=", &gbk)),
            ("--prompt", concat_bytes(b"--prompt=", &gbk)),
            ("-p/--prompt", concat_bytes(b"-p", &gbk)),
            ("-p/--prompt", concat_bytes(b"-p=", &gbk)),
        ] {
            let err = normalize(vec![OsString::from("ask"), token]).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains(flag), "{flag}: {msg}");
            assert!(msg.contains("valid UTF-8"), "{flag}: {msg}");
        }
    }

    #[cfg(unix)]
    fn concat_bytes(prefix: &[u8], value: &std::ffi::OsStr) -> OsString {
        use std::os::unix::ffi::OsStringExt;
        let mut bytes = prefix.to_vec();
        bytes.extend_from_slice(value.as_encoded_bytes());
        OsString::from_vec(bytes)
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_survive_but_never_become_text() {
        use std::os::unix::ffi::OsStringExt;
        let bad_name = OsString::from_vec(vec![0xff, 0xfe, 0x2e, 0x70, 0x6e, 0x67]);
        // A non-UTF-8 token that is not flag-shaped keeps its raw bytes as
        // a file path — the exact contrast that makes refusing the text
        // spellings right.
        let n = normalize(vec![
            OsString::from("ask"),
            OsString::from("-p"),
            OsString::from("hi"),
            bad_name.clone(),
        ])
        .unwrap();
        assert_eq!(n.specs[0], SourceSpec::File(PathBuf::from(&bad_name)));
        // After `--` even a flag-shaped token is a literal file.
        let token = concat_bytes(b"--text=", &OsString::from_vec(vec![0xc4, 0xe3]));
        let n = normalize(vec![
            OsString::from("ocr"),
            OsString::from("--"),
            token.clone(),
        ])
        .unwrap();
        assert_eq!(n.specs[0], SourceSpec::File(PathBuf::from(&token)));
    }

    #[test]
    fn then_form_splits_into_stages() {
        let stages = stages_of(&[
            "ocr",
            "a.png",
            "--then",
            "translate",
            "--to",
            "zh-CN",
            "--then",
            "tts",
            "-o",
            "out.mp3",
        ]);
        assert_eq!(stages.len(), 3);
        assert_eq!(stages[0].task.as_deref(), Some("ocr"));
        assert_eq!(
            stages[0].specs,
            vec![SourceSpec::File(PathBuf::from("a.png"))]
        );
        assert_eq!(stages[1].task.as_deref(), Some("translate"));
        assert!(stages[1]
            .argv
            .windows(2)
            .any(|w| w == [OsString::from("--to"), OsString::from("zh-CN")]));
        assert_eq!(stages[2].task.as_deref(), Some("tts"));
        assert!(stages[2].argv.contains(&OsString::from("-o")));
    }

    #[test]
    fn then_prompt_value_swallows_the_marker() {
        // `-p --then` is a prompt of "--then", not a stage boundary; the
        // next real marker splits.
        let stages = stages_of(&["ask", "-p", "--then", "--then", "tts"]);
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0].task.as_deref(), Some("ask"));
        let argv = stages[0]
            .argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            argv.iter().any(|a| a.contains("--then")),
            "the prompt's value must survive: {argv:?}"
        );
        assert_eq!(stages[1].task.as_deref(), Some("tts"));
    }

    #[test]
    fn then_after_separator_is_literal() {
        // After `--` nothing splits: a file literally named "--then" is
        // material, and the separator's stage simply ends the argv's
        // marker interpretation (put later stages before `--`).
        let n = normalize(os(&["ocr", "--", "--then", "f.png", "--then", "tts"])).unwrap();
        assert_eq!(
            n.specs,
            vec![
                SourceSpec::File(PathBuf::from("--then")),
                SourceSpec::File(PathBuf::from("f.png")),
                SourceSpec::File(PathBuf::from("--then")),
                SourceSpec::File(PathBuf::from("tts")),
            ]
        );
        // A separator inside the last stage is fine: its material stays
        // literal while the marker before it still split.
        let stages = stages_of(&["ocr", "a.png", "--then", "tts", "--", "--then"]);
        assert_eq!(stages.len(), 2);
        assert_eq!(
            stages[1].specs,
            vec![SourceSpec::File(PathBuf::from("--then"))]
        );
    }

    #[test]
    fn then_first_stage_without_a_task_is_rejected() {
        let err = super::normalize(os(&["--then", "tts"])).unwrap_err();
        assert!(err.to_string().contains("must start with a task"), "{err}");
    }

    #[test]
    fn chain_sugar_desugars_material_and_outer_flags() {
        let stages = stages_of(&[
            "chain",
            "ocr | translate --to zh-CN | tts",
            "shot.png",
            "-o",
            "brief.mp3",
        ]);
        assert_eq!(stages.len(), 3);
        assert_eq!(stages[0].task.as_deref(), Some("ocr"));
        assert_eq!(
            stages[0].specs,
            vec![SourceSpec::File(PathBuf::from("shot.png"))]
        );
        assert_eq!(stages[1].task.as_deref(), Some("translate"));
        assert!(stages[1]
            .argv
            .windows(2)
            .any(|w| w == [OsString::from("--to"), OsString::from("zh-CN")]));
        assert_eq!(stages[2].task.as_deref(), Some("tts"));
        let argv = stages[2]
            .argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(argv.contains(&"-o".to_string()) && argv.contains(&"brief.mp3".to_string()));
    }

    #[test]
    fn chain_outer_flags_are_position_independent() {
        // Flags before the chain word land on the last stage too.
        let stages = stages_of(&["--json", "chain", "ocr|tts", "shot.png", "--copy"]);
        assert_eq!(stages.len(), 2);
        for flag in ["--json", "--copy"] {
            assert!(
                stages[1].argv.contains(&OsString::from(flag)),
                "last stage missing {flag}: {:?}",
                stages[1].argv
            );
        }
        assert!(stages[0].specs.len() == 1);
    }

    #[test]
    fn chain_stage_level_flag_outside_the_spec_is_rejected() {
        let err = super::normalize(os(&["chain", "ocr|tts", "--to", "zh-CN"])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--to"), "{msg}");
        assert!(msg.contains("inside the chain spec"), "{msg}");
    }

    #[test]
    fn chain_needs_two_stages() {
        let err = super::normalize(os(&["chain", "ocr", "shot.png"])).unwrap_err();
        assert!(err.to_string().contains("at least two stages"), "{err}");
    }

    #[test]
    fn chain_empty_stage_is_named() {
        let err = super::normalize(os(&["chain", "ocr || tts"])).unwrap_err();
        assert!(err.to_string().contains("stage 2 is empty"), "{err}");
    }

    #[test]
    fn chain_unclosed_quote_points_at_then() {
        let err = super::normalize(os(&["chain", "ocr | 'tts"])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unclosed"), "{msg}");
        assert!(msg.contains("--then"), "{msg}");
    }

    #[test]
    fn chain_unclosed_quote_names_the_stage() {
        // The double-quote variant: the error names the 1-based stage the
        // spec died in, like the empty-stage error does.
        let err = tokenize_chain_spec("ocr | translate \"zh").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("stage 2"), "{msg}");
        assert!(msg.contains("unclosed"), "{msg}");
    }

    #[test]
    fn chain_and_then_cannot_mix() {
        let err = super::normalize(os(&["chain", "ocr|tts", "--then", "ask"])).unwrap_err();
        assert!(err.to_string().contains("cannot mix"), "{err}");
    }

    #[test]
    fn chain_material_flags_reach_stage1() {
        let stages = stages_of(&["chain", "ask -p '总结' | tts", "--text", "你好"]);
        assert_eq!(stages[0].task.as_deref(), Some("ask"));
        assert!(stages[0].specs.contains(&SourceSpec::Text("你好".into())));
    }

    #[test]
    fn chain_material_after_separator_stays_literal() {
        let stages = stages_of(&["chain", "ocr|tts", "--", "shots/*.png"]);
        assert_eq!(
            stages[0].specs,
            vec![SourceSpec::File(PathBuf::from("shots/*.png"))]
        );
    }

    #[test]
    fn chain_spec_unknown_task_gets_did_you_mean() {
        let err = super::normalize(os(&["chain", "ocr|transalte"])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown task 'transalte'"), "{msg}");
        assert!(msg.contains("translate"), "{msg}");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_token_before_the_chain_word_is_not_swallowed() {
        use std::os::unix::ffi::OsStringExt;
        // A non-UTF-8 free token ahead of "chain" means the chain word
        // was never first: refuse as file-without-task instead of
        // silently eating the bytes and misreading the spec.
        let bad = OsString::from_vec(vec![0xff, 0xfe]);
        let argv = vec![bad, OsString::from("chain"), OsString::from("ocr|tts")];
        let err = super::normalize(argv).unwrap_err();
        assert!(err.to_string().contains("requires a task"), "{err}");
    }

    #[test]
    fn tokenizer_cuts_segments_and_tokens_quote_aware() {
        let stages = tokenize_chain_spec("ocr |  translate  --to 'zh-CN' |tts").unwrap();
        assert_eq!(
            stages,
            vec![
                vec!["ocr".to_string()],
                vec![
                    "translate".to_string(),
                    "--to".to_string(),
                    "zh-CN".to_string()
                ],
                vec!["tts".to_string()],
            ]
        );
        // `|` inside quotes is literal; a quoted empty is an argument.
        let stages = tokenize_chain_spec("ask -p 'a|b' ''|tts").unwrap();
        assert_eq!(
            stages,
            vec![
                vec![
                    "ask".to_string(),
                    "-p".to_string(),
                    "a|b".to_string(),
                    "".to_string(),
                ],
                vec!["tts".to_string()],
            ]
        );
        // Double quotes survive into the token for clap to see verbatim.
        let stages = tokenize_chain_spec("ask -p \"总结 这页\"|tts").unwrap();
        assert_eq!(stages[0][2], "总结 这页");
    }
}

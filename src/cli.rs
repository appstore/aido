//! The command-line surface (contract §2.1–2.3) and the argv normalizer
//! that makes `aido <TASK> [INPUT...] [OPTIONS]` parseable by clap.
//!
//! The normalizer runs first: it knows every declared flag and its arity,
//! finds the first free (non-flag, non-value) token, and decides what it
//! is — a task name, `run`, `ask`, a management command, or a file. It
//! collects the ordered input slots (positional files, `-`, `--paste`,
//! `--text`) in argv order, then hands clap an argv containing only flags.

use crate::domain::MediaKind;
use anyhow::{bail, Result};
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

/// argv after normalization: a task (or the recovery pseudo-task), the
/// ordered input specs, and the remaining flags for clap.
#[derive(Debug)]
pub struct Normalized {
    pub task: Option<String>,
    pub specs: Vec<SourceSpec>,
    pub argv: Vec<OsString>,
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

/// Rewrite argv into its normalized form.
pub fn normalize(argv: Vec<OsString>) -> Result<Normalized> {
    // The clipboard holder child is spawned as `aido __hold SECS [--image]`:
    // already clap-shaped, and never a task run — pass it straight through so
    // the normalizer's task discovery cannot reject it (RESERVED_WORDS lists
    // it precisely so it can never be a task name).
    if argv.first().and_then(|t| t.to_str()) == Some("__hold") {
        return Ok(Normalized {
            task: None,
            specs: Vec::new(),
            argv,
        });
    }
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
        return Ok(Normalized {
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
            return Ok(Normalized {
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
        return Ok(Normalized {
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
        return Ok(Normalized {
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
        return Ok(Normalized {
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
            Ok(Normalized {
                specs: specs_from(slots, 1),
                task,
                argv,
            })
        }
        None => Ok(Normalized {
            task: None,
            specs: specs_from(slots, 0),
            argv: rest,
        }),
    }
}

/// A word that names no task but looks like a file path is input rather
/// than a typo — point at the task requirement instead of "unknown task".
fn looks_like_path(word: &str) -> bool {
    word.contains('/')
        || word.contains('\\')
        || word.contains('.')
        || std::path::Path::new(word).exists()
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
#[derive(Debug, Parser)]
#[command(
    name = "aido",
    version,
    about = "Send material to an AI task, deliver the result",
    override_usage = "aido <TASK> [INPUT...] [OPTIONS]\n    aido run <TASK> [INPUT...] [OPTIONS]\n    aido ask [INPUT...] -p <INSTRUCTION> [OPTIONS]\n    aido -p <INSTRUCTION> [INPUT...] [OPTIONS]",
    after_help = "INPUT is one or more of:\n  FILE            input file(s) in the order given\n  GLOB            pattern aido expands itself (quote it when the shell must not):\n                  matches in sorted order; no match is an error\n  DIR             directory: its files one level deep, sorted;\n                  dotfiles are skipped, subdirectories are refused\n                  (use a glob to descend)\n  PDF             each page's embedded images and text become material,\n                  in page order (vector-only PDFs need the pdfium build)\n  XLSX            each non-empty sheet becomes one text part, as a table\n  -               read stdin at this position (at most once)\n  --text TEXT     literal text material (repeatable)\n  --paste         read the clipboard at this position\n  -p TEXT         instruction for this run (not material)\n\nManagement: aido tasks|profiles|config|history ... and aido last\n\nExamples:\n  aido ocr screenshot.png --copy\n  aido ocr \"shots/*.png\" --copy\n  aido ocr shots/\n  git diff | aido code-review -\n  aido translate article.md --to zh-CN\n  aido tts --text \"你好\" -o hello.mp3\n  aido image --text \"a dog\" --count 2 --out-dir dogs/\n  aido ask a.png b.png -p \"比较两图\"\n  aido ask picture-book.pdf -p \"把这本绘本讲成旁白和对话\" | aido tts -o story.mp3\n  aido ocr screenshot.png --dry-run"
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

#[derive(Debug, Subcommand)]
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

#[derive(Debug, Subcommand)]
pub enum TasksCmd {
    /// List all tasks
    List,
    /// Show one task's definition
    Show { task: String },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCmd {
    /// Write a sample config file
    Init,
    /// Validate the current config
    Check,
}

#[derive(Debug, Subcommand)]
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
}

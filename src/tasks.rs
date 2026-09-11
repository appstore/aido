//! Task definitions — the "what to do" layer.
//!
//! A task names an operation, its input/output contracts and its processing
//! strategy. It never stores addresses or credentials: those belong to
//! providers and profiles, selected when the task runs.

use crate::domain::MediaKind;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;

/// The operation selects the protocol route inside the chosen provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    /// Text generation (chat completions / responses).
    Generate,
    /// Text-to-speech.
    Speech,
    /// Audio transcription.
    Transcribe,
    /// Image generation.
    Image,
}

impl Operation {
    /// Every operation, for contexts that mean "unrestricted".
    pub const ALL: [Operation; 4] = [Self::Generate, Self::Speech, Self::Transcribe, Self::Image];

    /// Parse an operation name (used to validate provider route keys).
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "generate" => Some(Self::Generate),
            "speech" => Some(Self::Speech),
            "transcribe" => Some(Self::Transcribe),
            "image" => Some(Self::Image),
            _ => None,
        }
    }

    /// The adapter name used when a provider route is not configured
    /// explicitly; also the key into `[providers.X.routes]`.
    pub fn default_route(self) -> &'static str {
        match self {
            Self::Generate => "openai-chat",
            Self::Speech => "openai-speech",
            Self::Transcribe => "openai-transcription",
            Self::Image => "openai-images",
        }
    }
}

impl std::fmt::Display for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Generate => "generate",
            Self::Speech => "speech",
            Self::Transcribe => "transcribe",
            Self::Image => "image",
        })
    }
}

/// How a task's material is turned into requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ProcessorKind {
    /// One request carrying all input.
    #[default]
    Single,
    /// OCR strategy: tall images are sliced vertically, requests are
    /// per-slice, replies merge at slice boundaries.
    OcrTiles,
    /// Text strategy: oversized text is chunked at paragraph boundaries,
    /// one request per chunk, replies joined in order.
    ChunkMapReduce,
}

/// Typed CLI parameters a task accepts (`--to`, `--voice`, ...); validated
/// and mapped to protocol options by the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskParam {
    To,
    Voice,
    Speed,
    Count,
    Size,
}

impl TaskParam {
    pub fn name(self) -> &'static str {
        match self {
            Self::To => "to",
            Self::Voice => "voice",
            Self::Speed => "speed",
            Self::Count => "count",
            Self::Size => "size",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "to" => Self::To,
            "voice" => Self::Voice,
            "speed" => Self::Speed,
            "count" => Self::Count,
            "size" => Self::Size,
            _ => return None,
        })
    }

    /// The adapter option this parameter maps to for an operation.
    pub fn maps_to(self) -> &'static str {
        match self {
            Self::To => "__instruction_suffix", // handled in the plan, not an option
            Self::Voice => "voice",
            Self::Speed => "speed",
            Self::Count => "n",
            Self::Size => "size",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskFile {
    operation: Operation,
    #[serde(default)]
    instruction: String,
    profile: Option<String>,
    /// Allowed input types; omit for everything the adapter supports.
    input_types: Option<Vec<MediaKind>>,
    #[serde(default)]
    required_types: Vec<MediaKind>,
    /// Cap on the number of input parts (e.g. exactly-one audio routes
    /// declare `max_inputs = 1`).
    max_inputs: Option<usize>,
    #[serde(default)]
    output_types: Vec<MediaKind>,
    /// Whether the task needs material at all (`ask` does not).
    #[serde(default = "default_true")]
    requires_material: bool,
    #[serde(default)]
    processor: ProcessorKind,
    /// Batch mode: plan the declared processor once per file part instead
    /// of one sequence over all material, one artifact per part.
    #[serde(default)]
    per_part: bool,
    #[serde(default)]
    params: Vec<String>,
    #[serde(default)]
    defaults: BTreeMap<String, toml::Value>,
    #[serde(default)]
    options: BTreeMap<String, toml::Value>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone)]
pub struct Task {
    pub name: String,
    pub operation: Operation,
    pub instruction: String,
    pub profile: Option<String>,
    pub input_types: Option<Vec<MediaKind>>,
    pub required_types: Vec<MediaKind>,
    pub max_inputs: Option<usize>,
    pub output_types: Vec<MediaKind>,
    pub requires_material: bool,
    pub processor: ProcessorKind,
    pub per_part: bool,
    pub params: Vec<TaskParam>,
    pub defaults: BTreeMap<String, serde_json::Value>,
    pub options: BTreeMap<String, serde_json::Value>,
    pub builtin: bool,
}

impl Task {
    /// Whether the task accepts the typed parameter `name`.
    pub fn accepts_param(&self, name: &str) -> bool {
        self.params.iter().any(|p| p.name() == name)
    }

    pub fn default_param(&self, name: &str) -> Option<&serde_json::Value> {
        self.defaults.get(name)
    }
}

const BUILTIN: &[(&str, &str)] = &[
    ("ask", include_str!("../tasks/ask.toml")),
    ("code-review", include_str!("../tasks/code-review.toml")),
    ("ocr", include_str!("../tasks/ocr.toml")),
    ("summarize", include_str!("../tasks/summarize.toml")),
    ("transcribe", include_str!("../tasks/transcribe.toml")),
    ("translate", include_str!("../tasks/translate.toml")),
    ("tts", include_str!("../tasks/tts.toml")),
    ("image", include_str!("../tasks/image.toml")),
];

pub fn tasks_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("AIDO_TASKS_DIR") {
        if !p.trim().is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    dirs::config_dir().map(|d| d.join("aido").join("tasks"))
}

/// Load every task: built-ins first, user files overriding by name.
pub fn load_all() -> Result<BTreeMap<String, Task>> {
    static CACHE: OnceLock<Result<BTreeMap<String, Task>, String>> = OnceLock::new();
    CACHE
        .get_or_init(|| load_all_uncached().map_err(|e| e.to_string()))
        .clone()
        .map_err(anyhow::Error::msg)
}

fn load_all_uncached() -> Result<BTreeMap<String, Task>> {
    let mut map = BTreeMap::new();
    for (name, src) in BUILTIN {
        let task = parse_task(name, src, true)?;
        map.insert((*name).to_string(), task);
    }
    if let Some(dir) = tasks_dir() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                match std::fs::read_to_string(&path)
                    .context("cannot read the task file")
                    .and_then(|s| parse_task(stem, &s, false))
                {
                    Ok(task) => {
                        map.insert(stem.to_string(), task);
                    }
                    Err(e) => eprintln!(
                        "warning: ignoring invalid task file {}: {e:#}",
                        path.display()
                    ),
                }
            }
        }
    }
    Ok(map)
}

fn parse_task(name: &str, src: &str, builtin: bool) -> Result<Task> {
    let file: TaskFile =
        toml::from_str(src).with_context(|| format!("invalid task definition for '{name}'"))?;
    if file.output_types.is_empty() {
        bail!("task '{name}': output_types must not be empty");
    }
    let mut params = Vec::new();
    for raw in &file.params {
        let Some(p) = TaskParam::from_name(raw) else {
            bail!("task '{name}': unknown parameter '{raw}'");
        };
        params.push(p);
    }
    let defaults = toml_to_json_map(&file.defaults, name, "defaults")?;
    let options = toml_to_json_map(&file.options, name, "options")?;
    Ok(Task {
        name: name.to_string(),
        operation: file.operation,
        instruction: file.instruction,
        profile: file.profile,
        input_types: file.input_types,
        required_types: file.required_types,
        max_inputs: file.max_inputs,
        output_types: file.output_types,
        requires_material: file.requires_material,
        processor: file.processor,
        per_part: file.per_part,
        params,
        defaults,
        options,
        builtin,
    })
}

fn toml_to_json_map(
    map: &BTreeMap<String, toml::Value>,
    task: &str,
    table: &str,
) -> Result<BTreeMap<String, serde_json::Value>> {
    map.iter()
        .map(|(k, v)| {
            let value = serde_json::to_value(v)
                .with_context(|| format!("task '{task}': invalid value in [{table}] for '{k}'"))?;
            Ok((k.clone(), value))
        })
        .collect()
}

/// Look up a task by name, suggesting the closest on a miss.
pub fn get(name: &str) -> Result<Task> {
    let all = load_all()?;
    match all.get(name) {
        Some(t) => Ok(t.clone()),
        None => {
            let names: Vec<&str> = all.keys().map(String::as_str).collect();
            let hint = closest(name, &names)
                .map(|best| format!(" (did you mean '{best}'?)"))
                .unwrap_or_default();
            bail!(
                "unknown task '{name}'; available: {}{hint} (see `aido tasks list`)",
                names.join(", ")
            )
        }
    }
}

/// The candidate closest to `word` within edit distance 2.
pub fn closest<'a>(word: &str, candidates: &[&'a str]) -> Option<&'a str> {
    candidates
        .iter()
        .map(|c| (edit_distance(word, c), *c))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_tasks_load_and_are_consistent() {
        let all = load_all().unwrap();
        for name in [
            "ask",
            "code-review",
            "ocr",
            "summarize",
            "transcribe",
            "translate",
            "tts",
            "image",
        ] {
            let task = all.get(name).unwrap_or_else(|| panic!("missing {name}"));
            assert!(!task.output_types.is_empty());
        }
        let ocr = &all["ocr"];
        assert_eq!(ocr.processor, ProcessorKind::OcrTiles);
        assert!(ocr.required_types.contains(&MediaKind::Image));
        let tts = &all["tts"];
        assert_eq!(tts.operation, Operation::Speech);
        // tts is useless without material to speak
        assert!(tts.requires_material);
        // ask runs without material
        assert!(!all["ask"].requires_material);
        // transcribe allows exactly one audio
        let tr = &all["transcribe"];
        assert_eq!(tr.max_inputs, Some(1));
    }

    #[test]
    fn unknown_fields_are_rejected_with_context() {
        let err = parse_task(
            "x",
            "operation = 'generate'\noutput_types = ['text']\nsistem = 'x'\n",
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("x"));
    }

    #[test]
    fn closest_suggests() {
        assert_eq!(
            closest("transalte", &["translate", "ocr"]),
            Some("translate")
        );
    }
}

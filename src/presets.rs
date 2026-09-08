use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug, Clone, Deserialize)]
pub struct Preset {
    pub system: String,
}

const BUILTIN: &[(&str, &str)] = &[
    ("code-review", include_str!("../presets/code-review.toml")),
    ("ocr", include_str!("../presets/ocr.toml")),
    ("summarize", include_str!("../presets/summarize.toml")),
    ("translate", include_str!("../presets/translate.toml")),
];

pub fn user_preset_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("AIDO_PRESETS_DIR") {
        if !p.trim().is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    dirs::config_dir().map(|d| d.join("aido").join("presets"))
}

pub fn load_all() -> Result<BTreeMap<String, Preset>> {
    // Several code paths (action dispatch, --preset, list) need the presets,
    // and the scan prints warnings for invalid files — load once per process.
    static CACHE: OnceLock<Result<BTreeMap<String, Preset>, String>> = OnceLock::new();
    CACHE
        .get_or_init(|| load_all_uncached().map_err(|e| e.to_string()))
        .clone()
        .map_err(anyhow::Error::msg)
}

fn load_all_uncached() -> Result<BTreeMap<String, Preset>> {
    let mut map = BTreeMap::new();
    for (name, src) in BUILTIN {
        let preset: Preset = toml::from_str(src)?;
        map.insert((*name).to_string(), preset);
    }
    // User presets with the same name override the built-ins.
    if let Some(dir) = user_preset_dir() {
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
                    .ok()
                    .and_then(|s| toml::from_str(&s).ok())
                {
                    Some(p) => {
                        map.insert(stem.to_string(), p);
                    }
                    None => eprintln!("warning: ignoring invalid preset file: {}", path.display()),
                }
            }
        }
    }
    Ok(map)
}

/// The candidate closest to `word` within edit distance 2, for
/// did-you-mean hints on unknown actions.
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

pub fn list() -> Result<()> {
    let all = load_all()?;
    for (name, preset) in &all {
        println!("{name:<14} {}", first_line(&preset.system));
    }
    eprintln!("\nrun an action: aido <NAME> [flags...], e.g. aido ocr --copy");
    if let Some(dir) = user_preset_dir() {
        eprintln!("custom presets: drop NAME.toml into {}", dir.display());
    }
    Ok(())
}

fn first_line(s: &str) -> String {
    let line = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if line.chars().count() > 64 {
        let head: String = line.chars().take(64).collect();
        format!("{head}...")
    } else {
        line.to_string()
    }
}

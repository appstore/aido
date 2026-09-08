use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

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

pub fn list() -> Result<()> {
    let all = load_all()?;
    for (name, preset) in &all {
        println!("{name:<14} {}", first_line(&preset.system));
    }
    if let Some(dir) = user_preset_dir() {
        eprintln!("\ncustom presets: drop NAME.toml into {}", dir.display());
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

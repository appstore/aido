use crate::api::{Adapter, MediaMode, Modes};
use crate::cli::{Cli, OutputMode};
use crate::history;
use crate::presets::Preset;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub default_profile: Option<String>,
    pub profiles: BTreeMap<String, Profile>,
    pub settings: Settings,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub adapter: Option<Adapter>,
    pub input_modes: Option<Vec<MediaMode>>,
    pub required_inputs: Option<Vec<MediaMode>>,
    pub output_modes: Option<Vec<MediaMode>>,
    pub options: BTreeMap<String, serde_json::Value>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub output: Option<OutputMode>,
    pub timeout_secs: Option<u64>,
    pub hold_secs: Option<u64>,
    /// Stream the reply as it is generated (SSE); on by default
    pub stream: Option<bool>,
    /// How many results to keep on disk (0 disables history)
    pub history_keep: Option<usize>,
}

/// Effective values after merging CLI flags, environment variables,
/// the selected preset's API overrides, the selected profile and
/// global defaults.
#[derive(Debug)]
pub struct Resolved {
    pub adapter: Adapter,
    pub modes: Modes,
    pub options: BTreeMap<String, serde_json::Value>,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub output: OutputMode,
    pub timeout_secs: u64,
    pub hold_secs: u64,
    pub history_keep: usize,
    pub stream: bool,
}

pub fn config_path() -> Option<PathBuf> {
    env_config_path().or_else(default_config_path)
}

fn env_config_path() -> Option<PathBuf> {
    std::env::var("AIDO_CONFIG")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
}

fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("aido").join("config.toml"))
}

pub fn load() -> Result<Config> {
    // An explicitly given AIDO_CONFIG must exist; a typo in the path
    // should be an error, not a silent fall back to the built-in defaults.
    if let Some(path) = env_config_path() {
        if !path.exists() {
            bail!("AIDO_CONFIG points to a missing file: {}", path.display());
        }
        return parse_config(&path);
    }
    let Some(path) = default_config_path() else {
        return Ok(Config::default());
    };
    if !path.exists() {
        return Ok(Config::default());
    }
    parse_config(&path)
}

fn parse_config(path: &Path) -> Result<Config> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    let cfg: Config = toml::from_str(&raw).with_context(|| {
        format!(
            "failed to parse config {} (run `aido --init` for a valid sample)",
            path.display()
        )
    })?;
    Ok(cfg)
}

/// `preset` (the action's TOML) acts as a profile scoped to one action:
/// its overrides come right after CLI flags, ahead of env vars and the
/// config profile, so `aido ocr` can pin a vision model even in shells
/// where OPENAI_* vars are exported for other tools.
pub fn resolve(cli: &Cli, cfg: &Config, preset: Option<&Preset>) -> Result<Resolved> {
    let profile_name = cli
        .profile
        .clone()
        .or_else(|| preset.and_then(|p| p.profile.clone()))
        .or_else(|| env_nonempty("AIDO_PROFILE"))
        .or_else(|| cfg.default_profile.clone())
        .unwrap_or_else(|| "default".to_string());

    let empty = Profile::default();
    let profile = match cfg.profiles.get(&profile_name) {
        Some(p) => p,
        None => {
            let explicit = cli.profile.is_some()
                || preset.is_some_and(|p| p.profile.is_some())
                || env_nonempty("AIDO_PROFILE").is_some()
                || cfg.default_profile.is_some();
            if explicit {
                let available = if cfg.profiles.is_empty() {
                    "(none defined)".to_string()
                } else {
                    cfg.profiles.keys().cloned().collect::<Vec<_>>().join(", ")
                };
                bail!("profile '{profile_name}' not found in config; available: {available}");
            }
            &empty
        }
    };

    let env_base = env_nonempty("AIDO_BASE_URL").or_else(|| env_nonempty("OPENAI_BASE_URL"));
    let env_key = env_nonempty("AIDO_API_KEY").or_else(|| env_nonempty("OPENAI_API_KEY"));
    let env_model = env_nonempty("AIDO_MODEL");
    let env_max_tokens = parse_env_number::<u64>("AIDO_MAX_TOKENS")?;
    let env_temperature = parse_env_number::<f64>("AIDO_TEMPERATURE")?;

    let raw_base = cli
        .base_url
        .clone()
        .or_else(|| preset.and_then(|p| p.base_url.clone()))
        .or(env_base)
        .or(profile.base_url.clone())
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());

    let api_key = cli
        .api_key
        .clone()
        .or_else(|| preset.and_then(|p| p.api_key.clone()))
        .or(env_key)
        .or(profile.api_key.clone())
        .filter(|k| !k.trim().is_empty());

    let env_adapter = env_nonempty("AIDO_ADAPTER")
        .map(|value| serde_json::from_value::<Adapter>(serde_json::Value::String(value)))
        .transpose()
        .context("invalid AIDO_ADAPTER")?;
    let adapter = cli
        .adapter
        .or_else(|| preset.and_then(|p| p.adapter))
        .or(env_adapter)
        .or(profile.adapter)
        .unwrap_or_default();
    let modes = Modes {
        inputs: if cli.input_mode.is_empty() {
            preset
                .and_then(|p| p.input_modes.clone())
                .or_else(|| profile.input_modes.clone())
        } else {
            Some(cli.input_mode.clone())
        },
        required: preset
            .and_then(|p| p.required_inputs.clone())
            .or_else(|| profile.required_inputs.clone())
            .unwrap_or_default(),
        outputs: if cli.output_mode.is_empty() {
            preset
                .and_then(|p| p.output_modes.clone())
                .or_else(|| profile.output_modes.clone())
                .unwrap_or_else(|| adapter.default_outputs())
        } else {
            cli.output_mode.clone()
        },
    };
    modes.validate(adapter)?;
    let mut options = profile.options.clone();
    if let Some(preset) = preset {
        options.extend(preset.options.clone());
    }
    for option in &cli.options {
        let (key, value) = option
            .split_once('=')
            .context("--option requires KEY=VALUE")?;
        let value =
            serde_json::from_str(value).unwrap_or_else(|_| serde_json::Value::String(value.into()));
        options.insert(key.into(), value);
    }
    adapter.validate_options(&options)?;
    if adapter == Adapter::Responses
        && !options.is_empty()
        && !modes.outputs.contains(&MediaMode::Image)
    {
        bail!("image options require --output-mode image (or text,image)");
    }

    let model = cli
        .model
        .clone()
        .or_else(|| preset.and_then(|p| p.model.clone()))
        .or(env_model)
        .or(profile.model.clone())
        .unwrap_or_else(|| adapter.default_model().to_string());

    let max_tokens = match cli
        .max_tokens
        .or_else(|| preset.and_then(|p| p.max_tokens))
        .or(env_max_tokens)
        .or(profile.max_tokens)
    {
        Some(0) => None, // explicit "don't send"
        Some(t) => Some(t),
        // Generous enough not to clip dense OCR output or long rewrites;
        // low enough for any vision-capable server to accept.
        None => Some(8192),
    };

    let output = if cli.copy {
        OutputMode::Clipboard
    } else {
        cli.output
            .or(cfg.settings.output)
            .unwrap_or(OutputMode::Stdout)
    };

    let base_url = crate::api::normalize_base_url(&raw_base)?;

    Ok(Resolved {
        adapter,
        modes,
        options,
        base_url,
        api_key,
        model,
        max_tokens,
        temperature: cli
            .temperature
            .or_else(|| preset.and_then(|p| p.temperature))
            .or(env_temperature)
            .or(profile.temperature),
        output,
        timeout_secs: cli.timeout.or(cfg.settings.timeout_secs).unwrap_or(120),
        hold_secs: cfg.settings.hold_secs.unwrap_or(45),
        history_keep: cfg.settings.history_keep.unwrap_or(history::DEFAULT_KEEP),
        // CLI pair --stream / --no-stream: last one given wins; neither
        // falls back to settings.stream, whose own default is streaming.
        stream: if cli.no_stream {
            false
        } else if cli.stream {
            true
        } else {
            cfg.settings.stream.unwrap_or(true)
        },
    })
}

pub fn init() -> Result<()> {
    let Some(path) = config_path() else {
        bail!("cannot determine the config directory for this platform");
    };
    if path.exists() {
        bail!("config already exists: {}", path.display());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&path, SAMPLE_CONFIG)
        .with_context(|| format!("failed to write {}", path.display()))?;
    println!("sample config written to: {}", path.display());
    println!("edit it to add your providers; set API keys via AIDO_API_KEY / OPENAI_API_KEY");
    Ok(())
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn parse_env_number<T>(key: &str) -> Result<Option<T>>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    env_nonempty(key)
        .map(|v| {
            v.parse::<T>()
                .with_context(|| format!("invalid value for {key}: '{v}'"))
        })
        .transpose()
}

const SAMPLE_CONFIG: &str = r#"# aido configuration
# Precedence: CLI flags > preset > environment variables > profile > defaults.
# A preset (see `aido list`) may pin base_url / model / api_key / max_tokens /
# temperature for its own action; those beat env vars and the profile, while
# CLI flags beat the preset.

# Profile used when --profile / AIDO_PROFILE is not given.
default_profile = "default"

[settings]
# output = "stdout"        # stdout | clipboard | both
# stream = true            # SSE streaming (the default); false = one buffered request
# timeout_secs = 120
# hold_secs = 45           # Linux: seconds to keep the clipboard alive after writing
# history_keep = 50        # results kept on disk; 0 disables, see `aido last`

[profiles.default]
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"
# adapter = "openai-chat"   # default; or openai-responses
# api_key = "sk-..."       # prefer env vars: OPENAI_API_KEY / AIDO_API_KEY

# Use with: echo hello | aido tts --profile speech --save hello.mp3
# [profiles.speech]
# adapter = "openai-speech"
# model = "tts-1"
# [profiles.speech.options]
# voice = "alloy"
# format = "mp3"

# Use with: aido transcribe meeting.wav --profile transcription
# [profiles.transcription]
# adapter = "openai-transcription"
# model = "whisper-1"

# Use with: aido image --text "a dog" --profile images --save dog.png
# [profiles.images]
# adapter = "openai-images"
# model = "gpt-image-1"

# Local LLM (vLLM / SGLang / llama.cpp / Ollama / LM Studio)
# [profiles.local]
# base_url = "http://localhost:30000"
# model = "qwen3"

# [profiles.zhipu]
# base_url = "https://open.bigmodel.cn/api/paas/v4"
# model = "glm-4.6"
# api_key = "..."
"#;

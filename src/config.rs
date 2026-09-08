use crate::cli::{Cli, OutputMode};
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
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub output: Option<OutputMode>,
    pub timeout_secs: Option<u64>,
    pub hold_secs: Option<u64>,
}

/// Effective values after merging CLI flags, environment variables,
/// the selected profile and global defaults.
#[derive(Debug)]
pub struct Resolved {
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f32>,
    pub output: OutputMode,
    pub timeout_secs: u64,
    pub hold_secs: u64,
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

pub fn resolve(cli: &Cli, cfg: &Config) -> Result<Resolved> {
    let profile_name = cli
        .profile
        .clone()
        .or_else(|| env_nonempty("AIDO_PROFILE"))
        .or_else(|| cfg.default_profile.clone())
        .unwrap_or_else(|| "default".to_string());

    let empty = Profile::default();
    let profile = match cfg.profiles.get(&profile_name) {
        Some(p) => p,
        None => {
            let explicit = cli.profile.is_some()
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
    let env_temperature = parse_env_number::<f32>("AIDO_TEMPERATURE")?;

    let raw_base = cli
        .base_url
        .clone()
        .or(env_base)
        .or(profile.base_url.clone())
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());

    let api_key = cli
        .api_key
        .clone()
        .or(env_key)
        .or(profile.api_key.clone())
        .filter(|k| !k.trim().is_empty());

    let model = cli
        .model
        .clone()
        .or(env_model)
        .or(profile.model.clone())
        .unwrap_or_else(|| "gpt-4o-mini".to_string());

    let max_tokens = match cli.max_tokens.or(env_max_tokens).or(profile.max_tokens) {
        Some(0) => None, // explicit "don't send"
        Some(t) => Some(t),
        None => Some(4096),
    };

    let output = if cli.copy {
        OutputMode::Clipboard
    } else {
        cli.output
            .or(cfg.settings.output)
            .unwrap_or(OutputMode::Stdout)
    };

    let base_url = crate::openai::normalize_base_url(&raw_base);
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        bail!("base URL must start with http:// or https:// (got '{raw_base}')");
    }

    Ok(Resolved {
        base_url,
        api_key,
        model,
        max_tokens,
        temperature: cli.temperature.or(env_temperature).or(profile.temperature),
        output,
        timeout_secs: cli.timeout.or(cfg.settings.timeout_secs).unwrap_or(120),
        hold_secs: cfg.settings.hold_secs.unwrap_or(45),
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
# Precedence: CLI flags > environment variables > profile > defaults.

# Profile used when --profile / AIDO_PROFILE is not given.
default_profile = "default"

[settings]
# output = "stdout"        # stdout | clipboard | both
# timeout_secs = 120
# hold_secs = 45           # Linux: seconds to keep the clipboard alive after writing

[profiles.default]
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"
# api_key = "sk-..."       # prefer env vars: OPENAI_API_KEY / AIDO_API_KEY

# Local LLM (vLLM / SGLang / llama.cpp / Ollama / LM Studio)
# [profiles.local]
# base_url = "http://localhost:30000"
# model = "qwen3"

# [profiles.zhipu]
# base_url = "https://open.bigmodel.cn/api/paas/v4"
# model = "glm-4.6"
# api_key = "..."
"#;

//! Configuration.
//!
//! Providers own connections (address, credential reference, operation →
//! adapter routes). Profiles own model choice and generation defaults, and
//! reference a provider. Tasks reference profiles by name. Credentials are
//! named by environment variable here and resolved only when a request is
//! about to be sent.

pub mod resolve;

pub use resolve::{resolve, ParamSource, Resolved};

use crate::api::Adapter;
use crate::domain::MediaKind;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub default_profile: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
    #[serde(default)]
    pub providers: BTreeMap<String, Provider>,
    #[serde(default)]
    pub settings: Settings,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub base_url: Option<String>,
    /// Environment variable holding the API key for this provider.
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub routes: BTreeMap<String, Adapter>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Operations this profile is intended for (capability check).
    pub operations: Option<Vec<crate::tasks::Operation>>,
    pub input_types: Option<Vec<MediaKind>>,
    pub output_types: Option<Vec<MediaKind>>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    #[serde(default)]
    pub options: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub stream: Option<bool>,
    pub timeout_secs: Option<u64>,
    pub total_timeout_secs: Option<u64>,
    pub hold_secs: Option<u64>,
    pub history_keep: Option<usize>,
    pub history_bytes: Option<u64>,
    /// Total input budget for one run in bytes (default 128 MiB).
    pub input_bytes: Option<u64>,
}

/// Pre-2.0 globals that no longer apply; kept explicit so the user hears
/// about the change instead of the value being used silently.
pub const DEPRECATED_ENV_VARS: &[&str] = &[
    "AIDO_MODEL",
    "AIDO_BASE_URL",
    "AIDO_ADAPTER",
    "AIDO_MAX_TOKENS",
    "AIDO_TEMPERATURE",
];

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
    // An explicitly given AIDO_CONFIG must exist; a typo should be an
    // error, not a silent fall back to defaults.
    if let Some(path) = env_config_path() {
        if !path.exists() {
            bail!("AIDO_CONFIG points to a missing file: {}", path.display());
        }
        return parse_config(&path);
    }
    let Some(path) = default_config_path() else {
        return Ok(default_config());
    };
    if !path.exists() {
        return Ok(default_config());
    }
    parse_config(&path)
}

fn parse_config(path: &Path) -> Result<Config> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    let cfg: Config = toml::from_str(&raw).with_context(|| {
        format!(
            "failed to parse config {} (run `aido config init` for a valid sample)",
            path.display()
        )
    })?;
    Ok(cfg)
}

/// A config with the built-in default provider: official OpenAI, key from
/// AIDO_API_KEY / OPENAI_API_KEY. Keeps zero-config usage working.
pub fn default_config() -> Config {
    let mut cfg = Config::default();
    cfg.providers.insert(
        "openai".into(),
        Provider {
            base_url: Some("https://api.openai.com/v1".into()),
            api_key_env: Some("AIDO_API_KEY".into()),
            routes: BTreeMap::new(),
        },
    );
    cfg.profiles.insert(
        "default".into(),
        Profile {
            provider: Some("openai".into()),
            ..Default::default()
        },
    );
    cfg
}

/// Resolve the credential value at send time (never earlier: dry-run and
/// reports only ever see the variable name).
pub fn resolve_api_key(provider: &Provider) -> Option<String> {
    let name = provider.api_key_env.as_deref().unwrap_or("AIDO_API_KEY");
    let key = std::env::var(name).ok().filter(|k| !k.trim().is_empty());
    key.or_else(|| {
        // The default provider also accepts the conventional OpenAI name.
        if name == "AIDO_API_KEY" {
            std::env::var("OPENAI_API_KEY")
                .ok()
                .filter(|k| !k.trim().is_empty())
        } else {
            None
        }
    })
}

/// Validate a whole config: used by `config check`.
pub fn check(cfg: &Config) -> Vec<String> {
    let mut issues = Vec::new();
    for (name, profile) in &cfg.profiles {
        let Some(provider_name) = &profile.provider else {
            issues.push(format!("profile '{name}': missing provider"));
            continue;
        };
        match cfg.providers.get(provider_name) {
            None => issues.push(format!(
                "profile '{name}' references unknown provider '{provider_name}'"
            )),
            Some(provider) => {
                if provider.base_url.is_none() {
                    issues.push(format!(
                        "provider '{provider_name}' (used by '{name}'): missing base_url"
                    ));
                }
            }
        }
        if profile.model.is_none() {
            issues.push(format!(
                "profile '{name}': no model set; the adapter default would be used"
            ));
        }
    }
    if let Some(name) = &cfg.default_profile {
        if !cfg.profiles.contains_key(name) && name != "default" {
            issues.push(format!(
                "default_profile '{name}' is not defined in [profiles]"
            ));
        }
    }
    issues
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
    println!("\nnext steps:");
    println!("  1. edit the provider base_url and api_key_env");
    println!("  2. export the key variable it names");
    println!("  3. validate with: aido config check");
    Ok(())
}

const SAMPLE_CONFIG: &str = r#"# aido configuration.
# Providers own connections; profiles own model choice; tasks reference
# profiles. Credentials are environment variable names, never values.

default_profile = "default"

[settings]
# stream = true            # live stdout delivery (terminal); pipes are buffered
# timeout_secs = 120       # header wait + network idle limit
# hold_secs = 45           # Linux: keep the clipboard alive this long
# history_keep = 50        # runs kept on disk; 0 disables history
# history_bytes = 536870912 # total history budget (512 MiB)

# The zero-config default: official OpenAI, key from AIDO_API_KEY or
# OPENAI_API_KEY.
[providers.openai]
base_url = "https://api.openai.com/v1"
api_key_env = "AIDO_API_KEY"

[profiles.default]
provider = "openai"
model = "YOUR_MODEL"

# A local vision setup (vLLM / SGLang / llama.cpp / Ollama / LM Studio):
# [providers.local]
# base_url = "http://localhost:30000"
# # no api_key_env: local servers usually need no auth
#
# [profiles.vision]
# provider = "local"
# model = "qwen3-vl"
# operations = ["generate"]
# input_types = ["text", "image"]
# output_types = ["text"]
#
# Then: aido ocr shot.png --profile vision

# Speech / transcription / images use the same provider with explicit
# routes when your server needs a different protocol per operation:
# [providers.cloud]
# base_url = "https://example.invalid/v1"
# api_key_env = "MY_AI_API_KEY"
#
# [providers.cloud.routes]
# generate = "openai-chat"       # or openai-responses
# speech = "openai-speech"
# transcribe = "openai-transcription"
# image = "openai-images"
#
# [profiles.speech]
# provider = "cloud"
# model = "tts-1"
# operations = ["speech"]
# output_types = ["audio"]
# [profiles.speech.options]
# voice = "alloy"
# format = "mp3"
#
# [profiles.transcription]
# provider = "cloud"
# model = "whisper-1"
# operations = ["transcribe"]
# output_types = ["text"]
#
# [profiles.images]
# provider = "cloud"
# model = "gpt-image-1"
# operations = ["image"]
# output_types = ["image"]
#
# Then: aido tts --text "hello" -o hello.mp3 --profile speech
"#;

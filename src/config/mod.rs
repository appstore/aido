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
use crate::tasks::Operation;
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

/// Clipboard hold duration when `settings.hold_secs` is unset: on Linux
/// the clipboard is kept alive this long after a copy.
pub const DEFAULT_HOLD_SECS: u64 = 45;

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
    let cfg: Config = toml::from_str(&raw).map_err(|e| {
        let mut message = format!(
            "failed to parse config {} (run `aido config init` for a valid sample)",
            path.display()
        );
        // Pre-2.0 configs nested connection fields under [profiles.*] and
        // used --input-mode/--output-mode style keys; name the migration
        // instead of a bare unknown-field list.
        if ["input_modes", "output_mode", "api_key ="]
            .iter()
            .any(|marker| raw.contains(marker))
        {
            message.push_str(
                "\n\nthis looks like a pre-2.0 config: base_url/api_key/adapter moved \
                 from [profiles.*] to [providers.*] (with [providers.X.routes] for \
                 per-operation adapters), input_modes/output_mode are gone (tasks \
                 declare their contracts), and presets are now tasks \
                 (`aido tasks list`)",
            );
        }
        anyhow::anyhow!("{message}: {e}")
    })?;
    Ok(cfg)
}

/// A config with the built-in default provider: official OpenAI, key from
/// AIDO_API_KEY / OPENAI_API_KEY. Keeps zero-config usage working. Speech
/// routes to the keyless Edge TTS adapter — the one operation that can run
/// without any credentials, so the zero-config default makes it work
/// instead of failing on a missing API key.
pub fn default_config() -> Config {
    let mut cfg = Config::default();
    cfg.providers.insert(
        "openai".into(),
        Provider {
            base_url: Some("https://api.openai.com/v1".into()),
            api_key_env: Some("AIDO_API_KEY".into()),
            // The edge-tts adapter owns its endpoint and ignores this
            // provider's connection entirely.
            routes: BTreeMap::from([("speech".to_string(), crate::api::Adapter::EdgeTts)]),
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

/// Validate a whole config: used by `aido config check`.
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
                // The edge-tts adapter owns its endpoint; base_url is only
                // required when some operation the profile allows resolves
                // to an adapter that does not own its endpoint. Unrouted
                // operations fall back to conventional adapters, so an
                // edge-only route table does not cover them.
                let allowed = profile.operations.as_deref().unwrap_or(&Operation::ALL);
                let needs_base = allowed
                    .iter()
                    .any(|&op| resolve::effective_adapter(op, provider) != Adapter::EdgeTts);
                if needs_base && provider.base_url.is_none() {
                    issues.push(format!(
                        "provider '{provider_name}' (used by '{name}'): missing base_url"
                    ));
                }
                for key in provider.routes.keys() {
                    if crate::tasks::Operation::from_name(key).is_none() {
                        issues.push(format!(
                            "provider '{provider_name}': route '{key}' is not an \
                             operation (generate, speech, transcribe, image) and \
                             would never be used"
                        ));
                    }
                }
            }
        }
        // `config init` writes `model = "YOUR_MODEL"`; a profile still
        // carrying that placeholder (or an empty model) fails at run time,
        // so check must flag it instead of reporting ok.
        match profile.model.as_deref().map(str::trim) {
            None => issues.push(format!(
                "profile '{name}': no model set; the adapter default would be used"
            )),
            Some(m) if m.is_empty() || m == "YOUR_MODEL" => issues.push(format!(
                "profile '{name}': no model configured; set model in the config \
                 ('YOUR_MODEL' is the config init placeholder)"
            )),
            Some(_) => {}
        }
    }
    // The profile resolution will actually use: an explicit
    // default_profile, else the implicit "default". The built-in default
    // profile exists only when no providers are configured at all — the
    // same rule resolution applies at run time, so `config check` and a
    // real run never disagree.
    let default_name = cfg
        .default_profile
        .clone()
        .unwrap_or_else(|| "default".to_string());
    let resolvable = cfg.profiles.contains_key(&default_name)
        || (default_name == "default" && cfg.providers.is_empty());
    if !resolvable {
        issues.push(format!(
            "default profile '{default_name}' is not defined in [profiles]; \
             set default_profile or add [profiles.{default_name}]"
        ));
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
# stream = false           # force buffered stdout even on a terminal (default: live on a tty)
# timeout_secs = 120       # header wait + network idle limit
# hold_secs = 45           # Linux: keep the clipboard alive this long
# history_keep = 50        # runs kept on disk; 0 disables history
# history_bytes = 536870912 # total history budget (512 MiB)

# The zero-config default: official OpenAI, key from AIDO_API_KEY or
# OPENAI_API_KEY.
[providers.openai]
base_url = "https://api.openai.com/v1"
api_key_env = "AIDO_API_KEY"

# Speech routes to the keyless Edge Read Aloud protocol (Microsoft's
# unofficial endpoint; the text is sent there, no API key). Delete the
# speech line to use OpenAI's speech API, which needs the key above.
[providers.openai.routes]
speech = "edge-tts"

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

# Free speech synthesis via Microsoft Edge's Read Aloud protocol (no API
# key, no base_url — the adapter owns its endpoint). Unofficial interface:
# Microsoft rotates its DRM constants, so keep the kothok-edge-tts
# dependency current.
# [providers.edge]
# routes = { speech = "edge-tts" }
#
# [profiles.edge]
# provider = "edge"
# model = "edge"
# operations = ["speech"]
# output_types = ["audio"]
# [profiles.edge.options]
# voice = "zh-CN-XiaoxiaoNeural"   # default; speed 0.25..4 via --speed
#
# Then: aido tts --text "你好" -o hello.mp3 --profile edge
"#;

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

/// The `model` value `config init` writes as a stand-in. `check()` flags
/// it so a fresh sample never reports ok, and the sample interpolates it
/// from this one constant — check and init cannot drift apart.
pub(crate) const MODEL_PLACEHOLDER: &str = "YOUR_MODEL";

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
    /// Local model directory for adapters that load local models
    /// (local-asr); unused by the others.
    #[serde(default)]
    pub model_dir: Option<String>,
    /// Silero VAD model file; required by the local-asr adapter.
    #[serde(default)]
    pub vad: Option<String>,
    /// Punctuation model directory; optional (local-asr).
    #[serde(default)]
    pub punct: Option<String>,
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
    /// Watch mode: how often the guarded directory is listed.
    pub watch_interval_ms: Option<u64>,
    /// Watch mode: how long a file's size must stay put before the task
    /// runs on it.
    pub watch_stable_ms: Option<u64>,
}

/// Clipboard hold duration when `settings.hold_secs` is unset: on Linux
/// the clipboard is kept alive this long after a copy.
pub const DEFAULT_HOLD_SECS: u64 = 45;

/// Watch mode defaults: list the guarded directory every second, and let
/// a file's size settle for half a second before running the task on it.
pub const DEFAULT_WATCH_INTERVAL_MS: u64 = 1000;
pub const DEFAULT_WATCH_STABLE_MS: u64 = 500;

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

/// The built-in default provider's credential variable, and the
/// conventional OpenAI name it also accepts: an existing
/// OPENAI_API_KEY keeps zero-config usage working without any
/// aido-specific setup.
pub(crate) const DEFAULT_KEY_ENV: &str = "AIDO_API_KEY";
pub(crate) const OPENAI_KEY_ENV: &str = "OPENAI_API_KEY";

/// The env var a run would actually read the key from: the provider's
/// `api_key_env` — or, when that names the default provider's key, the
/// conventional OpenAI name as a fallback. Returns the effective name
/// when that variable is set and non-empty; None when it (and any
/// fallback) is unset, or the provider names no variable at all. The
/// dry-run's credential line, its cleartext warning and the runner's
/// send-time resolution all share this judgment, so the three cannot
/// disagree about whether a request would carry a key.
pub(crate) fn effective_key_env(api_key_env: Option<&str>) -> Option<&str> {
    let set = |name: &str| std::env::var(name).is_ok_and(|v| !v.trim().is_empty());
    let name = api_key_env?;
    if set(name) {
        return Some(name);
    }
    // Only the default provider's key falls back to the conventional
    // OpenAI name; a custom variable is the provider's only credential.
    (name == DEFAULT_KEY_ENV && set(OPENAI_KEY_ENV)).then_some(OPENAI_KEY_ENV)
}

/// A config with the built-in default provider: official OpenAI, key from
/// AIDO_API_KEY / OPENAI_API_KEY. Keeps zero-config usage working. With the
/// edge-tts feature (the default build) speech routes to the keyless Edge
/// TTS adapter — the one operation that can run without any credentials.
/// Without the feature there is no speech route: speech falls back to the
/// conventional openai-speech adapter, so zero-config tts honestly fails on
/// the missing API key instead of silently sending text to Microsoft.
pub fn default_config() -> Config {
    // The edge-tts adapter owns its endpoint and ignores this provider's
    // connection entirely (feature off: no route at all).
    #[cfg(feature = "edge-tts")]
    let routes = BTreeMap::from([("speech".to_string(), crate::api::Adapter::EdgeTts)]);
    #[cfg(not(feature = "edge-tts"))]
    let routes: BTreeMap<String, crate::api::Adapter> = BTreeMap::new();
    let mut cfg = Config::default();
    cfg.providers.insert(
        "openai".into(),
        Provider {
            base_url: Some("https://api.openai.com/v1".into()),
            api_key_env: Some(DEFAULT_KEY_ENV.into()),
            routes,
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
    // The built-in providers, for the same `openai` fallback resolve()
    // applies at run time: a profile naming `openai` runs against the
    // built-in provider when the user defined no such provider, so check
    // cannot disagree with a real run about it.
    let effective = default_config();
    for (name, profile) in &cfg.profiles {
        let Some(provider_name) = &profile.provider else {
            issues.push(format!("profile '{name}': missing provider"));
            continue;
        };
        let provider = cfg.providers.get(provider_name).or_else(|| {
            (provider_name == "openai")
                .then(|| effective.providers.get("openai"))
                .flatten()
        });
        match provider {
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
                let needs_base = allowed.iter().any(|&op| {
                    !matches!(
                        resolve::effective_adapter(op, provider),
                        Adapter::EdgeTts | Adapter::LocalAsr
                    )
                });
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
                // A route naming edge-tts can never run in a binary built
                // without the feature; say so here, while the user is
                // editing the config, instead of at send time.
                #[cfg(not(feature = "edge-tts"))]
                for (key, adapter) in &provider.routes {
                    if *adapter == Adapter::EdgeTts {
                        issues.push(format!(
                            "provider '{provider_name}': route '{key}' names the \
                             edge-tts adapter, which this binary does not include \
                             (rebuild with --features edge-tts)"
                        ));
                    }
                }
                // The local-asr adapter's model files: with the feature
                // compiled in, run the same detect and cheap checks a run
                // performs (no model load) — absent files or a broken
                // directory surface here, while the user is editing the
                // config. Only when one of the profile's own operations
                // resolves to the adapter (the filter `needs_base` uses) —
                // a generate-only profile sharing this provider is never
                // asked for model files its runs would not load.
                let uses_local_asr = allowed
                    .iter()
                    .any(|&op| resolve::effective_adapter(op, provider) == Adapter::LocalAsr);
                if uses_local_asr {
                    #[cfg(feature = "local-asr")]
                    {
                        let family = profile.options.get("family").and_then(|v| v.as_str());
                        let models = crate::api::LocalModels {
                            asr: profile.model_dir.clone(),
                            vad: profile.vad.clone(),
                            punct: profile.punct.clone(),
                        };
                        if let Err(error) = crate::api::local_asr::check_model(&models, family) {
                            issues.push(format!("profile '{name}': {error:#}"));
                        }
                    }
                    #[cfg(not(feature = "local-asr"))]
                    {
                        issues.push(format!(
                            "provider '{provider_name}' has a local-asr route, \
                             but this binary does not include the local-asr \
                             adapter (rebuild with --features local-asr)"
                        ));
                    }
                }
            }
        }
        // A model-less profile runs on the adapter's default model
        // (resolve() falls back to it), so check must not fail it — a
        // stderr note says as much. Only the placeholder/empty model is a
        // real issue: `config init` writes it and it would never be what
        // the user wants.
        match profile.model.as_deref().map(str::trim) {
            None => eprintln!(
                "note: profile '{name}' has no model set; the adapter default will be used"
            ),
            Some(m) if m.is_empty() || m == MODEL_PLACEHOLDER => issues.push(format!(
                "profile '{name}': no model configured; set model in the config \
                 ('{MODEL_PLACEHOLDER}' is the config init placeholder)"
            )),
            Some(_) => {}
        }
    }
    // The profile resolution will actually use: an explicit
    // default_profile, else the implicit "default". The built-in default
    // profile exists exactly when the user defined no profiles at all —
    // the same rule resolution applies at run time, so `config check` and
    // a real run never disagree.
    let default_name = cfg
        .default_profile
        .clone()
        .unwrap_or_else(|| "default".to_string());
    let resolvable = cfg.profiles.contains_key(&default_name)
        || (default_name == "default" && cfg.profiles.is_empty());
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
    std::fs::write(&path, sample_config())
        .with_context(|| format!("failed to write {}", path.display()))?;
    println!("sample config written to: {}", path.display());
    println!("\nnext steps:");
    println!("  1. edit the provider base_url and api_key_env");
    println!("  2. export the key variable it names");
    println!("  3. validate with: aido config check");
    Ok(())
}

/// The sample's speech-route segment. With the edge-tts feature (the
/// default build) it keeps `aido tts` keyless via the Edge Read Aloud
/// protocol; a binary built without the feature cannot run that route, so
/// the segment ships commented out with a pointer at the feature — speech
/// then stays on OpenAI's speech API, which needs the key above.
#[cfg(feature = "edge-tts")]
const SAMPLE_SPEECH_ROUTE: &str = r#"
# Speech routes to the keyless Edge Read Aloud protocol (Microsoft's
# unofficial endpoint; the text is sent there, no API key). Delete the
# speech line to use OpenAI's speech API, which needs the key above.
[providers.openai.routes]
speech = "edge-tts"
"#;

#[cfg(not(feature = "edge-tts"))]
const SAMPLE_SPEECH_ROUTE: &str = r#"
# This build excludes the edge-tts adapter, so the keyless speech route
# below stays commented out (rebuild with --features edge-tts to enable
# it); speech then uses OpenAI's speech API, which needs the key above.
# [providers.openai.routes]
# speech = "edge-tts"
"#;

/// The sample `config init` writes: head, the feature-gated speech-route
/// segment, then the tail. `concat!` cannot join consts on this toolchain,
/// so the join happens here at runtime — init runs once, and both halves
/// stay plain readable raw strings. The tail names the placeholder model
/// through `{MODEL_PLACEHOLDER}`, filled from the same constant `check()`
/// flags, so the two can never disagree.
fn sample_config() -> String {
    format!("{SAMPLE_CONFIG_HEAD}{SAMPLE_SPEECH_ROUTE}{SAMPLE_CONFIG_TAIL}")
        .replace("{MODEL_PLACEHOLDER}", MODEL_PLACEHOLDER)
}

const SAMPLE_CONFIG_HEAD: &str = r#"# aido configuration.
# Providers own connections; profiles own model choice; tasks reference
# profiles. Credentials are environment variable names, never values.

default_profile = "default"

[settings]
# stream = false           # force buffered stdout even on a terminal (default: live on a tty)
# timeout_secs = 120       # header wait + network idle limit
# hold_secs = 45           # Linux: keep the clipboard alive this long
# history_keep = 50        # runs kept on disk; 0 disables history
# history_bytes = 536870912 # total history budget (512 MiB)
# watch_interval_ms = 1000 # watch mode: directory poll interval
# watch_stable_ms = 500    # watch mode: size-stability window before a file runs

# The zero-config default: official OpenAI, key from AIDO_API_KEY or
# OPENAI_API_KEY.
[providers.openai]
base_url = "https://api.openai.com/v1"
api_key_env = "AIDO_API_KEY"
"#;

const SAMPLE_CONFIG_TAIL: &str = r#"
[profiles.default]
provider = "openai"
model = "{MODEL_PLACEHOLDER}"

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

#[cfg(test)]
mod tests;

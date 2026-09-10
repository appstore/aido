//! Selection and merging: which profile, provider, adapter and parameters
//! a run uses, with the origin of every value (for `--dry-run`).

use super::{Config, Provider};
use crate::api::Adapter;
use crate::cli::Cli;
use crate::domain::MediaKind;
use crate::tasks::{Operation, Task};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;

/// Where a merged parameter came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamSource {
    Cli,
    Task,
    Profile,
    Default,
}

impl ParamSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Task => "task",
            Self::Profile => "profile",
            Self::Default => "default",
        }
    }
}

#[derive(Debug)]
pub struct Sourced<T> {
    pub value: T,
    pub source: ParamSource,
}

/// Everything a run needs to talk to a service, plus the input/output
/// capability envelope after intersecting task and profile constraints.
#[derive(Debug)]
pub struct Resolved {
    pub profile_name: String,
    pub provider_name: String,
    pub adapter: Adapter,
    pub base_url: String,
    pub api_key_env: Option<String>,
    pub model: String,
    pub model_source: ParamSource,
    /// None = "do not send a token limit".
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub options: BTreeMap<String, serde_json::Value>,
    /// Input types allowed after task ∩ profile; None = all.
    pub allowed_inputs: Option<Vec<MediaKind>>,
    pub required_inputs: Vec<MediaKind>,
    /// Requested generated types (task output, possibly --produce).
    pub produce: Vec<MediaKind>,
}

/// Pick the profile: CLI → task default → AIDO_PROFILE → config default.
fn select_profile_name(cli: &Cli, task: &Task, cfg: &Config) -> Result<String> {
    if let Some(name) = &cli.profile {
        return Ok(name.clone());
    }
    if let Some(name) = &task.profile {
        return Ok(name.clone());
    }
    if let Some(name) = env_nonempty("AIDO_PROFILE") {
        return Ok(name);
    }
    Ok(cfg
        .default_profile
        .clone()
        .unwrap_or_else(|| "default".to_string()))
}

pub fn resolve(cli: &Cli, cfg: &Config, task: &Task) -> Result<Resolved> {
    let profile_name = select_profile_name(cli, task, cfg)?;
    let effective = super::default_config();
    // A config that predates schema 2 may carry no providers at all: the
    // built-in defaults still let `--help`-level usage work.
    let profile = cfg.profiles.get(&profile_name).or_else(|| {
        if profile_name == "default" && cfg.providers.is_empty() {
            effective.profiles.get("default")
        } else {
            None
        }
    });
    let Some(profile) = profile else {
        let available: Vec<String> = cfg
            .profiles
            .keys()
            .cloned()
            .chain(
                (cfg.profiles.is_empty() && profile_name != "default")
                    .then(|| "default".to_string()),
            )
            .collect();
        bail!(
            "profile '{profile_name}' not found; available: {} (see `aido profiles list`)",
            if available.is_empty() {
                "(none defined)".to_string()
            } else {
                available.join(", ")
            }
        );
    };

    let provider_name = profile.provider.clone().unwrap_or_else(|| "openai".into());
    let provider: Provider = cfg
        .providers
        .get(&provider_name)
        .cloned()
        .or_else(|| {
            (provider_name == "openai")
                .then(|| effective.providers.get("openai").cloned())
                .flatten()
        })
        .with_context(|| {
            format!(
                "profile '{profile_name}' references provider '{provider_name}', \
                 which is not defined in the config"
            )
        })?;
    let Some(raw_base) = provider
        .base_url
        .clone()
        .or_else(|| (provider_name == "openai").then(|| "https://api.openai.com/v1".into()))
    else {
        bail!("provider '{provider_name}' has no base_url");
    };

    // The operation's route: explicit provider route, else the operation's
    // conventional adapter.
    let adapter = match provider.routes.get(&task.operation.to_string()) {
        Some(adapter) => *adapter,
        None => match task.operation {
            Operation::Generate => Adapter::Chat,
            Operation::Speech => Adapter::Speech,
            Operation::Transcribe => Adapter::Transcription,
            Operation::Image => Adapter::Images,
        },
    };

    if let Some(ops) = &profile.operations {
        if !ops.contains(&task.operation) {
            bail!(
                "profile '{profile_name}' does not declare the '{}' operation \
                 (needed by task '{}'); pick another profile or extend the profile",
                task.operation,
                task.name
            );
        }
    }

    // Model: explicit --model only overrides the profile's model — it never
    // switches provider or inherits another service's credentials.
    let (model, model_source) = if let Some(m) = &cli.model {
        (m.clone(), ParamSource::Cli)
    } else if let Some(m) = &profile.model {
        (m.clone(), ParamSource::Profile)
    } else {
        (adapter.default_model().to_string(), ParamSource::Default)
    };

    // Capability envelope: constraints intersect, they are not overridden.
    let allowed_inputs = match (&task.input_types, &profile.input_types) {
        (Some(t), Some(p)) => {
            let i: Vec<MediaKind> = t.iter().filter(|k| p.contains(k)).copied().collect();
            Some(i)
        }
        (Some(t), None) => Some(t.clone()),
        (None, Some(p)) => Some(p.clone()),
        (None, None) => None,
    };
    if let Some(allowed) = &allowed_inputs {
        for mode in allowed {
            if !adapter.inputs().contains(mode) {
                bail!(
                    "adapter '{adapter}' (route for task '{}') does not support \
                     input type '{mode}'",
                    task.name
                );
            }
        }
    }
    for mode in &task.required_types {
        if let Some(allowed) = &allowed_inputs {
            if !allowed.contains(mode) {
                bail!(
                    "task '{}' requires '{mode}' input, but the selected profile \
                     or task restricts inputs to [{}]",
                    task.name,
                    allowed
                        .iter()
                        .map(|m| m.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                );
            }
        }
        if !adapter.inputs().contains(mode) {
            bail!(
                "adapter '{adapter}' does not support the required input '{mode}' \
                 for task '{}'",
                task.name
            );
        }
    }

    // Requested outputs: task declaration, possibly narrowed by profile and
    // overridden by --produce (which must stay within adapter support).
    let mut produce = if cli.produce.is_empty() {
        let mut out = task.output_types.clone();
        if let Some(p) = &profile.output_types {
            out.retain(|k| p.contains(k));
        }
        out
    } else {
        cli.produce.clone()
    };
    produce.dedup();
    if produce.is_empty() {
        bail!("the requested output types are empty after applying the profile");
    }
    for mode in &produce {
        if !adapter.outputs().contains(mode) {
            bail!(
                "adapter '{adapter}' does not support output type '{mode}' \
                 (requested for task '{}')",
                task.name
            );
        }
    }

    // Generation parameters: CLI → profile → program (tasks declare no
    // generation defaults).
    let max_tokens = cli
        .max_tokens
        .map(|v| (v, ParamSource::Cli))
        .or_else(|| profile.max_tokens.map(|v| (v, ParamSource::Profile)));
    let temperature = cli
        .temperature
        .map(|v| (v, ParamSource::Cli))
        .or_else(|| profile.temperature.map(|v| (v, ParamSource::Profile)));
    // v2 default: no token limit is sent unless someone sets one.
    let max_tokens = match max_tokens {
        Some((0, _)) => None, // v1's explicit "0 = omit" sentinel
        Some((v, _)) => Some(v),
        None => None,
    };

    // Options: profile defaults, then task defaults, then --option.
    let mut options: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for (k, v) in &profile.options {
        options.insert(
            k.clone(),
            serde_json::to_value(v).context("invalid profile option value")?,
        );
    }
    for (k, v) in &task.options {
        options.insert(
            k.clone(),
            serde_json::to_value(v).context("invalid task option value")?,
        );
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

    let base_url = crate::api::normalize_base_url(&raw_base)?;

    Ok(Resolved {
        profile_name,
        provider_name,
        adapter,
        base_url,
        api_key_env: provider.api_key_env.clone(),
        model,
        model_source,
        max_tokens,
        temperature: temperature.map(|(v, _)| v),
        options,
        allowed_inputs,
        required_inputs: task.required_types.clone(),
        produce,
    })
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

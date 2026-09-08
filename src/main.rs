mod cli;
mod clipboard;
mod config;
mod input;
mod openai;
mod output;
mod presets;
mod spinner;

use anyhow::{anyhow, bail, Result};
use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    restore_sigpipe();

    let cli = parse_cli()?;

    // Internal: detached child that keeps the Linux clipboard alive.
    if let Some(cli::Commands::Hold { secs }) = &cli.command {
        return run_hold(*secs);
    }
    if let Some(cli::Commands::List) = &cli.command {
        return presets::list();
    }
    if cli.init {
        return config::init();
    }
    if cli.list_presets {
        return presets::list();
    }

    let config = config::load()?;
    let resolved = config::resolve(&cli, &config)?;

    let system = if let Some(name) = &cli.preset {
        let all = presets::load_all()?;
        match all.get(name) {
            Some(p) => p.system.clone(),
            None => {
                let names: Vec<&str> = all.keys().map(String::as_str).collect();
                let hint = presets::closest(name, &names)
                    .map(|best| format!(" (did you mean '{best}'?)"))
                    .unwrap_or_default();
                bail!(
                    "unknown preset '{name}'; available: {}{hint} (see `aido list`)",
                    names.join(", ")
                )
            }
        }
    } else {
        cli.prompt.clone().unwrap_or_default()
    };

    let user = input::gather()?;
    let request = openai::ChatRequest {
        model: resolved.model.clone(),
        messages: openai::build_messages(Some(&system), &user),
        max_tokens: resolved.max_tokens,
        temperature: resolved.temperature,
    };

    let client = openai::Client::new(&resolved)?;
    let spinner = if cli.no_spinner {
        spinner::Spinner::disabled()
    } else {
        spinner::Spinner::start(&format!("asking {}...", resolved.model))
    };
    let reply = client.chat(&request).await;
    spinner.stop();
    let reply = reply?;

    if reply.trim().is_empty() {
        if matches!(
            resolved.output,
            cli::OutputMode::Clipboard | cli::OutputMode::Both
        ) {
            // Writing an empty string would destroy whatever the user copied
            // (the OCR input itself, typically) — fail instead.
            bail!("model returned empty content; clipboard left untouched");
        }
        eprintln!("warning: model returned empty content");
    }
    output::emit(&reply, resolved.output, resolved.hold_secs)?;
    Ok(())
}

/// Names owned by real subcommands (including clap's built-in `help`);
/// they are never treated as preset actions.
const RESERVED_ACTIONS: &[&str] = &["list", "help", "__hold"];

/// Old versions accepted any positional text as the prompt, so text with
/// whitespace or non-ASCII characters in the action slot is that old habit
/// rather than a misspelled action name — it should be pointed at -p.
fn looks_like_prompt(word: &str) -> bool {
    word.chars().any(|c| c.is_whitespace() || !c.is_ascii())
}

/// The first argument (when not a flag) is an action: `aido ocr ...` becomes
/// `aido --preset ocr ...`. The rewrite happens before clap because preset
/// names are only known at runtime; anything else in that slot is an error.
fn parse_cli() -> Result<cli::Cli> {
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if let Some(first) = args.first().and_then(|a| a.to_str()) {
        if !first.is_empty() && !first.starts_with('-') && !RESERVED_ACTIONS.contains(&first) {
            let all = presets::load_all()?;
            if all.contains_key(first) {
                args.insert(0, std::ffi::OsString::from("--preset"));
            } else {
                let mut actions: Vec<&str> = all.keys().map(String::as_str).collect();
                actions.extend(["list", "help"]);
                actions.sort_unstable();
                let hint = presets::closest(first, &actions)
                    .map(|best| format!(" (did you mean '{best}'?)"))
                    .unwrap_or_default();
                let prompt_hint = if looks_like_prompt(first) {
                    "; for an ad-hoc prompt, use -p/--prompt"
                } else {
                    ""
                };
                bail!(
                    "unknown action '{first}'; available: {}{hint}{prompt_hint}",
                    actions.join(", ")
                );
            }
        }
    }
    Ok(cli::Cli::parse_from(
        std::iter::once(std::ffi::OsString::from("aido")).chain(args),
    ))
}

/// `aido | head` should die silently like any other unix tool, not panic.
#[cfg(unix)]
fn restore_sigpipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn restore_sigpipe() {}

fn run_hold(secs: u64) -> Result<()> {
    use std::io::Read;
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    // Errors propagate to stderr (the child inherits it), so a failed hold
    // is visible instead of silently losing the clipboard contents.
    let mut cb =
        arboard::Clipboard::new().map_err(|e| anyhow!("cannot access the clipboard: {e}"))?;
    cb.set_text(text)
        .map_err(|e| anyhow!("failed to write clipboard: {e}"))?;
    std::thread::sleep(std::time::Duration::from_secs(secs));
    Ok(())
}

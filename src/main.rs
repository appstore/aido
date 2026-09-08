mod clipboard;
mod cli;
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

    let cli = cli::Cli::parse();

    // Internal: detached child that keeps the Linux clipboard alive.
    if let Some(cli::Commands::Hold { secs }) = &cli.command {
        return run_hold(*secs);
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
                let names: Vec<String> = all.keys().cloned().collect();
                bail!("unknown preset '{name}'; available: {} (see --list-presets)", names.join(", "))
            }
        }
    } else {
        cli.prompt_pos.clone().or_else(|| cli.prompt.clone()).unwrap_or_default()
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
        if matches!(resolved.output, cli::OutputMode::Clipboard | cli::OutputMode::Both) {
            // Writing an empty string would destroy whatever the user copied
            // (the OCR input itself, typically) — fail instead.
            bail!("model returned empty content; clipboard left untouched");
        }
        eprintln!("warning: model returned empty content");
    }
    output::emit(&reply, resolved.output, resolved.hold_secs)?;
    Ok(())
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
    let mut cb = arboard::Clipboard::new()
        .map_err(|e| anyhow!("cannot access the clipboard: {e}"))?;
    cb.set_text(text).map_err(|e| anyhow!("failed to write clipboard: {e}"))?;
    std::thread::sleep(std::time::Duration::from_secs(secs));
    Ok(())
}

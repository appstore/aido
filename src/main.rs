mod cli;
mod clipboard;
mod config;
mod history;
mod input;
mod openai;
mod output;
mod presets;
mod spinner;
mod split;

use anyhow::{anyhow, bail, Result};
use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    restore_sigpipe();

    let cli = parse_cli()?;
    require_action_for_files(&cli)?;

    // Internal: detached child that keeps the Linux clipboard alive.
    if let Some(cli::Commands::Hold { secs }) = &cli.command {
        return run_hold(*secs);
    }
    if let Some(cli::Commands::List) = &cli.command {
        return presets::list();
    }
    if let Some(cli::Commands::Last { copy }) = &cli.command {
        // Parent flags must reach the subcommand: `aido -c last`,
        // `aido --save f last` etc. would otherwise be silently ignored.
        // `aido last` is the recovery command, so a broken config only
        // drops the preference hints instead of failing the run.
        let cfg = config::load().ok();
        let wants_clipboard = |m: Option<cli::OutputMode>| {
            matches!(m, Some(cli::OutputMode::Clipboard | cli::OutputMode::Both))
        };
        let copy = *copy
            || cli.copy
            || wants_clipboard(cli.output)
            || wants_clipboard(cfg.as_ref().and_then(|c| c.settings.output));
        let hold_secs = cfg
            .as_ref()
            .and_then(|c| c.settings.hold_secs)
            .unwrap_or(45);
        return run_last(copy, hold_secs, cli.save.as_deref());
    }
    if cli.init {
        return config::init();
    }
    if cli.list_presets {
        return presets::list();
    }

    let config = config::load()?;

    // The preset is loaded before resolving so its API overrides take part
    // in the merge (CLI flags > preset > env vars > profile > defaults).
    let preset = match &cli.preset {
        Some(name) => Some(presets::get(name)?),
        None => None,
    };
    let resolved = config::resolve(&cli, &config, preset.as_ref())?;

    let system = match &preset {
        Some(p) => p.system.clone(),
        None => cli.prompt.clone().unwrap_or_default(),
    };

    let user = input::gather(&cli.files)?;
    // Tall images are sliced into several requests; with nothing to slice
    // this is a single batch and behaves exactly as before.
    let batches = split::expand(user, !cli.no_split)?;

    let client = openai::Client::new(&resolved)?;
    let mut replies: Vec<String> = Vec::with_capacity(batches.len());
    for (i, batch) in batches.iter().enumerate() {
        let messages = openai::build_messages(Some(&system), batch)?;
        let request = openai::ChatRequest {
            model: resolved.model.clone(),
            messages,
            max_tokens: resolved.max_tokens,
            temperature: resolved.temperature,
        };
        let spinner = if cli.no_spinner {
            spinner::Spinner::disabled()
        } else if batches.len() > 1 {
            spinner::Spinner::start(&format!(
                "asking {} ({}/{} slices)...",
                resolved.model,
                i + 1,
                batches.len()
            ))
        } else {
            spinner::Spinner::start(&format!("asking {}...", resolved.model))
        };
        let reply = client.chat(&request).await;
        spinner.stop();
        replies.push(reply?);
    }
    let reply = if replies.len() > 1 {
        replies.join("\n")
    } else {
        replies.into_iter().next().unwrap_or_default()
    };

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
    history::record(&reply, resolved.history_keep);
    output::emit(
        &reply,
        resolved.output,
        resolved.hold_secs,
        cli.save.as_deref(),
    )?;
    Ok(())
}

/// Names owned by real subcommands (including clap's built-in `help`);
/// they are never treated as preset actions.
const RESERVED_ACTIONS: &[&str] = &["list", "help", "last", "__hold"];

/// Old versions accepted any positional text as the prompt, so text with
/// whitespace or non-ASCII characters in the action slot is that old habit
/// rather than a misspelled action name — it should be pointed at -p.
fn looks_like_prompt(word: &str) -> bool {
    word.chars().any(|c| c.is_whitespace() || !c.is_ascii())
}

/// A word that names no action but looks like a file path (a separator, an
/// extension, or an existing file) is file input rather than a typo.
fn looks_like_path(word: &str) -> bool {
    word.contains('/')
        || word.contains('\\')
        || word.contains('.')
        || std::path::Path::new(word).exists()
}

/// Files are explicit input and only mean something with an instruction
/// behind them, so a bare `aido notes.txt` is an error; an action or a
/// -p prompt must name what to do with the file.
fn require_action_for_files(cli: &cli::Cli) -> Result<()> {
    if cli.files.is_empty() || cli.preset.is_some() || cli.prompt.is_some() {
        return Ok(());
    }
    let example = cli.files[0].display();
    // `aido --copy ocr` lands here: the misplaced action name parses as a
    // file, so point out that actions must come first.
    let all = presets::load_all()?;
    if let Some(name) = cli
        .files
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
        .find(|n| all.contains_key(*n))
    {
        bail!(
            "'{name}' is an action name; actions must be the first argument, e.g. `aido {name} ...`"
        );
    }
    bail!(
        "file input requires an action or -p/--prompt, e.g. `aido ocr {example}` \
         or `aido -p \"<instructions>\" {example}` (see `aido list`)"
    );
}

/// The first argument (when not a flag) is an action: `aido ocr ...` becomes
/// `aido --preset ocr ...`. The rewrite happens before clap because preset
/// names are only known at runtime; anything else in that slot is either left
/// for clap as file input (which then requires an action or -p) or reported
/// as an unknown action.
fn parse_cli() -> Result<cli::Cli> {
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if let Some(first) = args.first().and_then(|a| a.to_str()) {
        if !first.is_empty() && !first.starts_with('-') && !RESERVED_ACTIONS.contains(&first) {
            let all = presets::load_all()?;
            if all.contains_key(first) {
                args.insert(0, std::ffi::OsString::from("--preset"));
            } else if looks_like_path(first) {
                // File input: left for clap to parse as FILEs; the required
                // action or -p is enforced after parsing.
            } else {
                let mut actions: Vec<&str> = all.keys().map(String::as_str).collect();
                actions.extend(["list", "help", "last"]);
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

/// `aido last`: re-print (or re-copy) the most recent history entry, so a
/// clipboard lost to a later copy doesn't mean paying for the model again.
fn run_last(copy: bool, hold_secs: u64, save: Option<&std::path::Path>) -> Result<()> {
    let Some(text) = history::last()? else {
        bail!(
            "no saved results yet; every non-empty result is kept on disk \
               (settings.history_keep, 0 disables)"
        );
    };
    if !text.trim().is_empty() {
        if let Some(path) = save {
            output::save_to_file(&text, path)?;
        }
    }
    if copy {
        // Same trailing-newline trim as a normal clipboard write.
        let text = text.trim_end();
        clipboard::write_text(text, hold_secs)?;
        eprintln!("✓ copied {} chars to clipboard", text.chars().count());
    } else {
        println!("{text}");
    }
    Ok(())
}

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

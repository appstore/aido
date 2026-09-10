mod api;
mod cli;
mod clipboard;
mod config;
mod history;
mod input;
mod output;
mod presets;
mod spinner;
mod split;

use anyhow::{anyhow, bail, Result};
use clap::Parser;
use std::io::Write as _;

#[tokio::main]
async fn main() -> Result<()> {
    restore_sigpipe();

    let cli = parse_cli()?;
    require_action_for_files(&cli)?;

    // Internal: detached child that keeps the Linux clipboard alive.
    if let Some(cli::Commands::Hold { secs, image }) = &cli.command {
        return run_hold(*secs, *image);
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
        return run_last(
            copy,
            hold_secs,
            cli.save.as_deref(),
            cli.save_dir.as_deref(),
        );
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

    let user = match &cli.text {
        Some(text) => input::UserContent {
            text: Some(text.clone()),
            ..Default::default()
        },
        None => input::gather(&cli.files)?,
    };
    resolved.modes.validate_input(&user, resolved.adapter)?;
    output::validate_destination(
        &resolved.modes.outputs,
        resolved.output,
        cli.save.as_deref(),
        cli.save_dir.as_deref(),
    )?;
    if resolved
        .options
        .get("n")
        .and_then(|v| v.as_u64())
        .is_some_and(|n| n > 1)
        && cli.save_dir.is_none()
    {
        bail!("multiple images require --save-dir");
    }
    if resolved.adapter == api::Adapter::Speech {
        if let Some(path) = cli.save.as_deref() {
            output::validate_extension(
                resolved
                    .options
                    .get("format")
                    .and_then(|v| v.as_str())
                    .unwrap_or("mp3"),
                path,
            )?;
        }
    }
    if cli.stream && !resolved.adapter.streams() {
        bail!(
            "adapter '{}' does not support --stream; use --no-stream or omit the flag",
            resolved.adapter
        );
    }
    // Tall images are sliced into several requests; with nothing to slice
    // this is a single batch and behaves exactly as before.
    let batches = split::expand(
        user,
        !cli.no_split
            && resolved
                .modes
                .outputs
                .iter()
                .all(|m| *m == api::MediaMode::Text),
    )?;

    let client = api::Client::new(&resolved)?;
    // Streaming is the default for every output mode: a clipboard-only
    // run streams silently — the clipboard has no "partial" state — so
    // long generations are bounded by idle gaps, not a total request
    // timeout. Deltas print live only where stdout carries the reply.
    let stream = resolved.stream && resolved.adapter.streams();
    let live = resolved.modes.outputs.contains(&api::MediaMode::Text)
        && matches!(
            resolved.output,
            cli::OutputMode::Stdout | cli::OutputMode::Both
        );

    let mut combined = api::GenerateResult::default();
    let mut replies: Vec<String> = Vec::with_capacity(batches.len());
    for (i, batch) in batches.iter().enumerate() {
        let request = api::GenerateRequest {
            system: Some(&system),
            user: batch,
            model: &resolved.model,
            max_tokens: resolved.max_tokens,
            temperature: resolved.temperature,
            outputs: &resolved.modes.outputs,
            options: &resolved.options,
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
        let reply = if stream {
            // Batches are separated by the same "\n" the buffered path
            // builds with join("\n"); the spinner yields to the live
            // output at the first token. A clipboard-only run prints
            // nothing and keeps its spinner until the end.
            if live && i > 0 {
                println!();
                let _ = std::io::stdout().flush();
            }
            let mut spinner = Some(spinner);
            // A clipboard-only run has no live output; the spinner line
            // instead counts the chars received so far.
            let mut seen = 0u64;
            let reply = client
                .generate_stream(&request, |delta| {
                    if live {
                        if let Some(s) = spinner.take() {
                            s.stop();
                        }
                        print!("{delta}");
                        let _ = std::io::stdout().flush();
                    } else if let Some(s) = spinner.as_ref() {
                        seen += delta.chars().count() as u64;
                        s.set_progress(seen);
                    }
                })
                .await;
            if let Some(s) = spinner.take() {
                s.stop();
            }
            reply
        } else {
            let reply = client.generate(&request).await;
            spinner.stop();
            reply
        };
        let reply = reply?;
        for warning in reply.diagnostics() {
            eprintln!("warning: {warning}");
        }
        if reply.status != api::CompletionStatus::Complete {
            combined.status = reply.status;
        }
        combined.warnings.extend(reply.warnings);
        combined.artifacts.extend(reply.artifacts);
        replies.push(reply.text);
    }
    let reply = if replies.len() > 1 {
        replies.join("\n")
    } else {
        replies.into_iter().next().unwrap_or_default()
    };

    if reply.trim().is_empty() && combined.artifacts.is_empty() {
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
    combined.text = reply;
    for mode in &resolved.modes.outputs {
        if *mode != api::MediaMode::Text && !combined.artifacts.iter().any(|a| a.mode == *mode) {
            bail!("response did not produce the requested '{mode}' output");
        }
    }
    if !combined.artifacts.is_empty() || cli.save_dir.is_some() {
        history::record_result(&combined, resolved.history_keep);
        // Keep the full response in history, but render only requested modalities.
        if !resolved.modes.outputs.contains(&api::MediaMode::Text) {
            combined.text.clear();
        }
        combined
            .artifacts
            .retain(|a| resolved.modes.outputs.contains(&a.mode));
        return output::emit_result(
            &combined,
            resolved.output,
            resolved.hold_secs,
            cli.save.as_deref(),
            cli.save_dir.as_deref(),
            stream && live,
        );
    }
    let reply = combined.text;
    history::record(&reply, resolved.history_keep);
    if stream && live {
        // The deltas already went to stdout: just close the line, then run
        // the clipboard / --save half of emit.
        if !reply.is_empty() {
            println!();
        }
        output::finish(
            &reply,
            resolved.output,
            resolved.hold_secs,
            cli.save.as_deref(),
        )?;
    } else {
        output::emit(
            &reply,
            resolved.output,
            resolved.hold_secs,
            cli.save.as_deref(),
        )?;
    }
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
    if cli.files.is_empty()
        || cli.preset.is_some()
        || cli.prompt.is_some()
        || cli.profile.is_some()
        || cli.adapter.is_some()
    {
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
fn run_last(
    copy: bool,
    hold_secs: u64,
    save: Option<&std::path::Path>,
    save_dir: Option<&std::path::Path>,
) -> Result<()> {
    let Some(result) = history::last_result()? else {
        bail!("no saved results yet; every non-empty result is kept on disk (settings.history_keep, 0 disables)");
    };
    for warning in result.diagnostics() {
        eprintln!("warning: {warning}");
    }
    output::emit_result(
        &result,
        if copy {
            cli::OutputMode::Clipboard
        } else {
            cli::OutputMode::Stdout
        },
        hold_secs,
        save,
        save_dir,
        false,
    )
}

fn run_hold(secs: u64, image: bool) -> Result<()> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::io::stdin().read_to_end(&mut bytes)?;
    let mut cb =
        arboard::Clipboard::new().map_err(|e| anyhow!("cannot access the clipboard: {e}"))?;
    if image {
        clipboard::set_image(&mut cb, &bytes)?;
    } else {
        cb.set_text(String::from_utf8(bytes)?)
            .map_err(|e| anyhow!("failed to write clipboard: {e}"))?;
    }
    std::thread::sleep(std::time::Duration::from_secs(secs));
    Ok(())
}

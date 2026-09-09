use crate::cli::OutputMode;
use crate::clipboard;
use anyhow::{Context, Result};
use std::path::Path;

pub fn emit(result: &str, mode: OutputMode, hold_secs: u64, save: Option<&Path>) -> Result<()> {
    if matches!(mode, OutputMode::Stdout | OutputMode::Both) {
        println!("{result}");
    }
    finish(result, mode, hold_secs, save)
}

/// The clipboard / `--save` half of [`emit`], for callers that already
/// streamed the result to stdout themselves.
pub fn finish(result: &str, mode: OutputMode, hold_secs: u64, save: Option<&Path>) -> Result<()> {
    if let Some(path) = save {
        // An empty reply saves nothing (main already warned): an empty file
        // would read as saved content.
        if !result.trim().is_empty() {
            save_to_file(result, path)?;
        }
    }
    if matches!(mode, OutputMode::Clipboard | OutputMode::Both) {
        // A trailing newline in the clipboard is almost never wanted when pasting.
        let text = result.trim_end();
        clipboard::write_text(text, hold_secs)?;
        eprintln!("copied {} chars to clipboard", text.chars().count());
    }
    Ok(())
}

/// Write the result to an explicit `--save` path, creating parent dirs.
pub fn save_to_file(result: &str, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
    }
    std::fs::write(path, result).with_context(|| format!("failed to write {}", path.display()))?;
    eprintln!("saved result to {}", path.display());
    Ok(())
}

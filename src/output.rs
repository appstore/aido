use crate::cli::OutputMode;
use crate::clipboard;
use anyhow::Result;

pub fn emit(result: &str, mode: OutputMode, hold_secs: u64) -> Result<()> {
    if matches!(mode, OutputMode::Stdout | OutputMode::Both) {
        println!("{result}");
    }
    if matches!(mode, OutputMode::Clipboard | OutputMode::Both) {
        // A trailing newline in the clipboard is almost never wanted when pasting.
        let text = result.trim_end();
        clipboard::write_text(text, hold_secs)?;
        eprintln!("✓ copied {} chars to clipboard", text.chars().count());
    }
    Ok(())
}

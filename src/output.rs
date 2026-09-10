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

/// Validate the intended sink before making a potentially expensive request.
pub fn validate_destination(
    modes: &[crate::api::MediaMode],
    output: OutputMode,
    save: Option<&Path>,
    save_dir: Option<&Path>,
) -> Result<()> {
    use crate::api::MediaMode;
    use std::io::IsTerminal;
    if modes.contains(&MediaMode::Audio)
        && matches!(output, OutputMode::Clipboard | OutputMode::Both)
    {
        anyhow::bail!("audio clipboard output is not supported; use --save or pipe stdout");
    }
    if modes.contains(&MediaMode::Image)
        && modes.contains(&MediaMode::Text)
        && matches!(output, OutputMode::Clipboard | OutputMode::Both)
    {
        anyhow::bail!("mixed text/image clipboard output is ambiguous; use --save-dir");
    }
    if modes.iter().any(|m| *m != MediaMode::Text)
        && matches!(output, OutputMode::Stdout | OutputMode::Both)
        && save.is_none()
        && save_dir.is_none()
        && std::io::stdout().is_terminal()
    {
        anyhow::bail!("binary output needs --save / --save-dir or a stdout pipe");
    }
    if modes.contains(&MediaMode::Text)
        && modes.iter().any(|m| *m != MediaMode::Text)
        && save_dir.is_none()
    {
        anyhow::bail!("mixed output modes require --save-dir");
    }
    Ok(())
}

pub fn emit_result(
    result: &crate::api::GenerateResult,
    mode: OutputMode,
    hold_secs: u64,
    save: Option<&Path>,
    save_dir: Option<&Path>,
    text_streamed: bool,
) -> Result<()> {
    use crate::api::MediaMode;
    use anyhow::bail;
    use std::io::{IsTerminal, Write};
    for artifact in &result.artifacts {
        artifact.validate()?;
    }
    if result.artifacts.is_empty() && save_dir.is_none() {
        return if text_streamed {
            finish(&result.text, mode, hold_secs, save)
        } else {
            emit(&result.text, mode, hold_secs, save)
        };
    }
    let has_text = !result.text.is_empty();
    let count = result.artifacts.len() + usize::from(has_text);
    if save.is_some() && count != 1 {
        bail!("response has {count} outputs; use --save-dir instead of --save");
    }
    let copy = matches!(mode, OutputMode::Clipboard | OutputMode::Both);
    if copy
        && !result.artifacts.is_empty()
        && (count != 1 || result.artifacts[0].mode != MediaMode::Image)
    {
        bail!("clipboard supports one text or image output; use --save-dir for this response");
    }
    let stdout = matches!(mode, OutputMode::Stdout | OutputMode::Both);
    if stdout
        && save.is_none()
        && save_dir.is_none()
        && !result.artifacts.is_empty()
        && (count != 1 || std::io::stdout().is_terminal())
    {
        bail!("binary or mixed output needs --save / --save-dir, or pipe a single artifact");
    }
    if let Some(dir) = save_dir {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        if has_text {
            save_to_file(&result.text, &dir.join("text.txt"))?;
        }
        for (index, artifact) in result.artifacts.iter().enumerate() {
            save_bytes(
                &artifact.bytes,
                &dir.join(format!(
                    "{}-{}.{}",
                    artifact.mode,
                    index + 1,
                    artifact.format
                )),
            )?;
        }
    } else if let Some(path) = save {
        if let Some(artifact) = result.artifacts.first() {
            validate_extension(&artifact.format, path)?;
            save_bytes(&artifact.bytes, path)?;
        }
    }
    if copy {
        if let Some(artifact) = result.artifacts.first() {
            clipboard::write_image(&artifact.bytes, hold_secs)?;
            eprintln!("copied image to clipboard");
        } else {
            clipboard::write_text(result.text.trim_end(), hold_secs)?;
        }
    }
    if stdout {
        if has_text && !text_streamed {
            println!("{}", result.text);
        } else if has_text && text_streamed {
            println!();
        }
        if save.is_none() && save_dir.is_none() {
            if let Some(artifact) = result.artifacts.first() {
                std::io::stdout().write_all(&artifact.bytes)?;
            }
        }
    }
    Ok(())
}

fn save_bytes(bytes: &[u8], path: &Path) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))?;
    eprintln!("saved result to {}", path.display());
    Ok(())
}

pub fn validate_extension(format: &str, path: &Path) -> Result<()> {
    if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
        let extension = extension.to_ascii_lowercase();
        if extension != format
            && !(extension == "jpg" && format == "jpeg")
            && !(extension == "ogg" && format == "opus")
        {
            anyhow::bail!("output format is '{format}', but --save has extension '{extension}'; select --option format=... or use the matching extension");
        }
    }
    Ok(())
}

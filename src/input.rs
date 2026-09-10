//! Material collection: ordered `SourceSpec`s become ordered `InputPart`s.
//!
//! Decision table (contract §2.2):
//!
//! | explicit material | stdin non-terminal | behavior                          |
//! |-------------------|-------------------|-----------------------------------|
//! | none              | yes               | read stdin; empty is an error     |
//! | contains `-`      | yes               | read stdin at the `-` position    |
//! | none              | no                | clipboard (or instruction-only)   |
//! | some, no `-`      | yes               | error: consume the pipe with `-`  |
//! | some              | no                | explicit material; `-` reads EOF  |
//!
//! Nothing is ever silently dropped or re-sourced: an empty file, empty
//! stdin or an empty clipboard is an error at the position it occurred.

use crate::cli::SourceSpec;
use crate::domain::{InputContent, InputPart, InputSource, MediaKind};
use anyhow::{bail, Context, Result};
use std::io::{IsTerminal, Read};
use std::path::Path;

/// One material slot fully read into memory.
const MAX_PART_BYTES: u64 = 32 * 1024 * 1024;
/// Budget for the whole run's material (settings.input_bytes overrides).
const DEFAULT_TOTAL_BYTES: u64 = 128 * 1024 * 1024;

/// Injectable environment so tests never touch the real clipboard or tty.
pub struct InputEnv<'a> {
    pub stdin_is_terminal: bool,
    stdin: &'a mut dyn Read,
    clipboard: &'a mut (dyn FnMut() -> Result<crate::clipboard::ClipboardContent> + 'a),
}

impl InputEnv<'static> {
    pub fn real() -> Self {
        // Leaks nothing: stdin and the clipboard live for the process.
        Self {
            stdin_is_terminal: std::io::stdin().is_terminal(),
            stdin: Box::leak(Box::new(std::io::stdin())),
            clipboard: Box::leak(Box::new(crate::clipboard::read)),
        }
    }
}

impl<'a> InputEnv<'a> {
    pub fn custom(
        stdin: &'a mut dyn Read,
        stdin_is_terminal: bool,
        clipboard: &'a mut dyn FnMut() -> Result<crate::clipboard::ClipboardContent>,
    ) -> Self {
        Self {
            stdin_is_terminal,
            stdin,
            clipboard,
        }
    }

    fn read_stdin(&mut self) -> &mut dyn Read {
        self.stdin
    }

    fn read_clipboard(&mut self) -> Result<crate::clipboard::ClipboardContent> {
        (self.clipboard)()
    }
}

/// Read every spec into an ordered list of parts. `requires_material`
/// decides whether a task with no specs and a terminal stdin may run on
/// the instruction alone. Under `dry_run` the clipboard is never touched:
/// paste slots become clearly-labeled placeholders so a plan can be
/// checked without reading (or depending on) desktop state.
pub fn gather(
    specs: &[SourceSpec],
    requires_material: bool,
    total_limit: Option<u64>,
    dry_run: bool,
    env: &mut InputEnv<'_>,
) -> Result<Vec<InputPart>> {
    if specs
        .iter()
        .filter(|s| matches!(s, SourceSpec::Stdin))
        .count()
        > 1
    {
        bail!("stdin can be read once; `-` appears multiple times");
    }
    if specs
        .iter()
        .filter(|s| matches!(s, SourceSpec::Paste))
        .count()
        > 1
    {
        bail!("the clipboard can be read once; --paste appears multiple times");
    }

    let limit = total_limit.unwrap_or(DEFAULT_TOTAL_BYTES);

    if specs.is_empty() {
        if !env.stdin_is_terminal {
            let bytes = read_limited(env.stdin, MAX_PART_BYTES, "stdin")?;
            if bytes.is_empty() {
                bail!("stdin is empty; nothing to send (the clipboard is never a fallback)");
            }
            return Ok(vec![classify("stdin", bytes, InputSource::Stdin, 0)?]);
        }
        if requires_material {
            if dry_run {
                return Ok(vec![dry_run_clipboard_part(0)]);
            }
            let content = env
                .read_clipboard()
                .map_err(|e| crate::domain::AppError::usage(format!("clipboard: {e}")))?;
            return Ok(vec![clipboard_part(content, 0)?]);
        }
        // No material at all: the instruction alone drives the run.
        return Ok(Vec::new());
    }

    if !env.stdin_is_terminal && !specs.iter().any(|s| matches!(s, SourceSpec::Stdin)) {
        bail!(
            "stdin is piped but not consumed; add `-` where the piped data belongs \
             (e.g. `aido code-review -`), or redirect stdin from the terminal"
        );
    }

    let mut parts = Vec::new();
    let mut total: u64 = 0;
    for spec in specs {
        let part = match spec {
            SourceSpec::File(path) => {
                let bytes = read_file(path, MAX_PART_BYTES, limit.saturating_sub(total))?;
                let origin = path.display().to_string();
                classify(&origin, bytes, InputSource::File(path.clone()), parts.len())?
            }
            SourceSpec::Stdin => {
                let bytes = read_limited(env.read_stdin(), MAX_PART_BYTES, "stdin")?;
                if bytes.is_empty() {
                    bail!("stdin is empty; nothing to send");
                }
                classify("stdin", bytes, InputSource::Stdin, parts.len())?
            }
            SourceSpec::Paste => {
                if dry_run {
                    parts.push(dry_run_clipboard_part(parts.len()));
                    continue;
                }
                let content = env
                    .read_clipboard()
                    .map_err(|e| crate::domain::AppError::usage(format!("clipboard: {e}")))?;
                clipboard_part(content, parts.len())?
            }
            SourceSpec::Text(value) => {
                if value.trim().is_empty() {
                    bail!("--text is empty or whitespace-only");
                }
                InputPart {
                    id: parts.len(),
                    source: InputSource::Literal,
                    name: format!("--text #{}", parts.len() + 1),
                    kind: MediaKind::Text,
                    mime: "text/plain".into(),
                    content: InputContent::Text(value.clone()),
                }
            }
        };
        total = total.saturating_add(part_size(&part) as u64);
        if total > limit {
            bail!("inputs exceed the {} MB total limit", limit / (1024 * 1024));
        }
        parts.push(part);
    }
    Ok(parts)
}

fn part_size(part: &InputPart) -> usize {
    match &part.content {
        InputContent::Text(s) => s.len(),
        InputContent::Media(b) => b.len(),
    }
}

/// A stand-in for clipboard material under `--dry-run`: the plan can be
/// checked without reading (or requiring) desktop clipboard state. The
/// real run still reads the clipboard and still fails on an empty one.
fn dry_run_clipboard_part(id: usize) -> InputPart {
    InputPart {
        id,
        source: InputSource::Clipboard,
        name: "clipboard (not read under --dry-run)".into(),
        kind: MediaKind::Text,
        mime: "text/plain".into(),
        content: InputContent::Text(String::new()),
    }
}

fn clipboard_part(content: crate::clipboard::ClipboardContent, id: usize) -> Result<InputPart> {
    match content {
        crate::clipboard::ClipboardContent::Text(t) => {
            if t.trim().is_empty() {
                bail!("the clipboard is empty; nothing to send");
            }
            Ok(InputPart {
                id,
                source: InputSource::Clipboard,
                name: "clipboard".into(),
                kind: MediaKind::Text,
                mime: "text/plain".into(),
                content: InputContent::Text(t),
            })
        }
        crate::clipboard::ClipboardContent::Png(png) => {
            if png.is_empty() {
                bail!("the clipboard is empty; nothing to send");
            }
            Ok(InputPart {
                id,
                source: InputSource::Clipboard,
                name: "clipboard.png".into(),
                kind: MediaKind::Image,
                mime: "image/png".into(),
                content: InputContent::Media(png),
            })
        }
    }
}

fn read_file(path: &Path, max: u64, remaining: u64) -> Result<Vec<u8>> {
    if path.is_dir() {
        bail!("'{}' is a directory, not a file", path.display());
    }
    let meta =
        std::fs::metadata(path).with_context(|| format!("cannot read '{}'", path.display()))?;
    if meta.len() > max {
        bail!(
            "'{}' is {} MB; refusing input files over {} MB (they are fully loaded into memory)",
            path.display(),
            meta.len() / (1024 * 1024),
            max / (1024 * 1024)
        );
    }
    if meta.len() > remaining {
        bail!("inputs exceed the total input limit");
    }
    std::fs::read(path).with_context(|| format!("cannot read '{}'", path.display()))
}

fn read_limited(reader: &mut dyn Read, max: u64, origin: &str) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    reader
        .take(max + 1)
        .read_to_end(&mut buf)
        .with_context(|| format!("failed to read {origin}"))?;
    if buf.len() as u64 > max {
        bail!("{origin} exceeds the 32 MB input limit");
    }
    Ok(buf)
}

/// Classify raw bytes by content, keeping the original bytes and MIME.
fn classify(origin: &str, bytes: Vec<u8>, source: InputSource, id: usize) -> Result<InputPart> {
    let name = match &source {
        InputSource::File(p) => p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(origin)
            .to_string(),
        _ => origin.to_string(),
    };
    if bytes.is_empty() {
        bail!("'{origin}' is empty; nothing to send");
    }
    if let Some(mime) = image_mime(&bytes) {
        return Ok(InputPart {
            id,
            source,
            name,
            kind: MediaKind::Image,
            mime: mime.into(),
            content: InputContent::Media(bytes),
        });
    }
    if let Some((mime, _)) = audio_type(&bytes) {
        return Ok(InputPart {
            id,
            source,
            name,
            kind: MediaKind::Audio,
            mime: mime.into(),
            content: InputContent::Media(bytes),
        });
    }
    match String::from_utf8(bytes) {
        Ok(text) => {
            if text.trim().is_empty() {
                bail!("'{origin}' is empty or whitespace-only; nothing to send");
            }
            Ok(InputPart {
                id,
                source,
                name,
                kind: MediaKind::Text,
                mime: "text/plain".into(),
                content: InputContent::Text(text),
            })
        }
        Err(_) => bail!("'{origin}' is neither valid UTF-8 text nor a supported image/audio file"),
    }
}

fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

fn audio_type(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE") {
        Some(("audio/wav", "wav"))
    } else if bytes.starts_with(b"ID3")
        || (bytes.len() > 1 && bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0)
    {
        Some(("audio/mpeg", "mp3"))
    } else if bytes.starts_with(b"fLaC") {
        Some(("audio/flac", "flac"))
    } else if bytes.starts_with(b"OggS") {
        Some(("audio/ogg", "ogg"))
    } else if bytes.get(4..8) == Some(b"ftyp") {
        Some(("audio/mp4", "m4a"))
    } else if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        Some(("audio/webm", "webm"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::ClipboardContent;
    use std::io::Cursor;

    fn env(stdin_data: &'static [u8], terminal: bool) -> InputEnv<'static> {
        // The cursor and closure are leaked here on purpose: test-scoped,
        // tiny, and it keeps every call site a one-liner.
        InputEnv {
            stdin: Box::leak(Box::new(Cursor::new(stdin_data))),
            stdin_is_terminal: terminal,
            clipboard: Box::leak(Box::new(|| Ok(ClipboardContent::Text("clip".into())))),
        }
    }

    fn file(p: &str) -> SourceSpec {
        SourceSpec::File(p.into())
    }

    #[test]
    fn piped_stdin_alone_is_material() {
        let mut e = env(b"hello\n", false);
        let parts = gather(&[], true, None, false, &mut e).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].text(), Some("hello\n"));
        assert_eq!(parts[0].source, InputSource::Stdin);
    }

    #[test]
    fn empty_piped_stdin_is_an_error_without_clipboard_fallback() {
        let mut e = env(b"", false);
        let err = gather(&[], true, None, false, &mut e).unwrap_err();
        assert!(err.to_string().contains("stdin is empty"));
    }

    #[test]
    fn unconsumed_pipe_with_explicit_material_is_an_error() {
        let mut e = env(b"pipe data\n", false);
        let err = gather(&[file("a.txt")], true, None, false, &mut e).unwrap_err();
        assert!(err.to_string().contains("add `-`"));
    }

    #[test]
    fn dash_reads_stdin_at_its_position() {
        let dir = std::env::temp_dir().join(format!("aido-input-dash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        std::fs::write(&a, b"from file\n").unwrap();
        let mut e = env(b"pipe\n", false);
        let parts = gather(
            &[SourceSpec::File(a.clone()), SourceSpec::Stdin],
            true,
            None,
            false,
            &mut e,
        )
        .unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].text(), Some("from file\n"));
        assert_eq!(parts[1].text(), Some("pipe\n"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn terminal_stdin_falls_back_to_clipboard_only_without_specs() {
        let mut e = env(b"", true);
        let parts = gather(&[], true, None, false, &mut e).unwrap();
        assert_eq!(parts[0].text(), Some("clip"));
        assert_eq!(parts[0].source, InputSource::Clipboard);
    }

    #[test]
    fn no_material_task_runs_on_instruction_alone() {
        let mut e = env(b"", true);
        let parts = gather(&[], false, None, false, &mut e).unwrap();
        assert!(parts.is_empty());
    }

    #[test]
    fn empty_file_is_an_error_not_a_skip() {
        let dir = std::env::temp_dir().join(format!("aido-input-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let empty = dir.join("empty.txt");
        std::fs::write(&empty, b"").unwrap();
        let mut e = env(b"", true);
        let err = gather(
            &[SourceSpec::File(empty.clone())],
            true,
            None,
            false,
            &mut e,
        )
        .unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
        let real = dir.join("real.txt");
        std::fs::write(&real, b"real\n").unwrap();
        let mut e = env(b"", true);
        let parts = gather(&[SourceSpec::File(real.clone())], true, None, false, &mut e).unwrap();
        assert_eq!(parts.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn original_image_bytes_are_kept() {
        let img = image::RgbaImage::from_pixel(3, 3, image::Rgba([1, 2, 3, 255]));
        let mut jpg = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut jpg),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        let dir = std::env::temp_dir().join(format!("aido-input-jpg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shot.jpg");
        std::fs::write(&path, &jpg).unwrap();
        let mut e = env(b"", true);
        let parts = gather(&[SourceSpec::File(path)], true, None, false, &mut e).unwrap();
        assert_eq!(parts[0].kind, MediaKind::Image);
        assert_eq!(parts[0].mime, "image/jpeg");
        assert_eq!(parts[0].content, InputContent::Media(jpg));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn duplicate_stdin_and_paste_rejected() {
        let mut e = env(b"x", false);
        let err = gather(
            &[SourceSpec::Stdin, SourceSpec::Stdin],
            true,
            None,
            false,
            &mut e,
        );
        assert!(err.is_err());
        let mut e = env(b"", true);
        let err = gather(
            &[SourceSpec::Paste, SourceSpec::Paste],
            true,
            None,
            false,
            &mut e,
        );
        assert!(err.is_err());
    }

    #[test]
    fn paste_is_material_and_consumes_the_pipe_check() {
        // paste + piped stdin without `-` is still "unconsumed pipe"
        let mut e = env(b"pipe\n", false);
        let err = gather(&[SourceSpec::Paste], true, None, false, &mut e).unwrap_err();
        assert!(err.to_string().contains("add `-`"));
    }

    #[test]
    fn dry_run_never_touches_the_clipboard() {
        // The closure would panic if called; the placeholder keeps the
        // plan checkable without desktop clipboard state.
        let mut e = InputEnv {
            stdin: Box::leak(Box::new(Cursor::new(b""))),
            stdin_is_terminal: true,
            clipboard: Box::leak(Box::new(|| panic!("clipboard read under --dry-run"))),
        };
        let parts = gather(&[SourceSpec::Paste], true, None, true, &mut e).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].source, InputSource::Clipboard);
        assert!(parts[0].name.contains("dry-run"), "{}", parts[0].name);

        let mut e = InputEnv {
            stdin: Box::leak(Box::new(Cursor::new(b""))),
            stdin_is_terminal: true,
            clipboard: Box::leak(Box::new(|| panic!("clipboard read under --dry-run"))),
        };
        let parts = gather(&[], true, None, true, &mut e).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].source, InputSource::Clipboard);
    }
}

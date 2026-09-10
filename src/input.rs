use crate::clipboard::{self, ClipboardContent};
use anyhow::{bail, Context, Result};
use std::io::{IsTerminal, Read};
use std::path::PathBuf;

/// Everything gathered from one input source: the user text (if any) plus
/// images, already normalized to PNG, and audio files with their media type.
#[derive(Default)]
pub struct UserContent {
    pub audios: Vec<AudioInput>,
    pub text: Option<String>,
    pub images: Vec<Vec<u8>>,
}

/// Explicit file paths win over piped stdin; stdin wins over the clipboard.
/// Binary PNG/JPEG/WebP input (file or stdin) is treated as an image so
/// `aido ocr shot.png` works headless.
pub fn gather(files: &[PathBuf]) -> Result<UserContent> {
    if !files.is_empty() {
        return gather_from_files(files);
    }

    if !std::io::stdin().is_terminal() {
        let mut buf = Vec::new();
        std::io::stdin()
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut buf)
            .context("failed to read stdin")?;
        if buf.len() as u64 > MAX_FILE_BYTES {
            bail!("stdin exceeds the 32 MB input limit");
        }
        if !buf.is_empty() {
            let content = classify_bytes("stdin", buf)?;
            if content.text.is_some() || !content.images.is_empty() || !content.audios.is_empty() {
                return Ok(content);
            }
            // whitespace-only stdin: fall back to the clipboard
        }
    }

    match clipboard::read()? {
        ClipboardContent::Text(t) => Ok(UserContent {
            audios: Vec::new(),
            text: Some(t),
            images: Vec::new(),
        }),
        ClipboardContent::Png(p) => Ok(UserContent {
            audios: Vec::new(),
            text: None,
            images: vec![p],
        }),
    }
}

/// Whole files are read into memory and images are base64-encoded into a
/// single request, so oversized files fail fast instead of exhausting memory.
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// Read each file and classify it by content (not extension): PNG image,
/// JPEG/WebP images (re-encoded as PNG), supported audio containers, or UTF-8 text.
fn gather_from_files(files: &[PathBuf]) -> Result<UserContent> {
    let mut texts: Vec<(PathBuf, String)> = Vec::new();
    let mut images: Vec<Vec<u8>> = Vec::new();
    let mut audios = Vec::new();
    for path in files {
        if path.is_dir() {
            bail!("'{}' is a directory, not a file", path.display());
        }
        let size = std::fs::metadata(path)
            .with_context(|| format!("cannot read '{}'", path.display()))?
            .len();
        if size > MAX_FILE_BYTES {
            bail!(
                "'{}' is {} MB; refusing input files over {} MB (they are fully loaded into memory)",
                path.display(),
                size / (1024 * 1024),
                MAX_FILE_BYTES / (1024 * 1024)
            );
        }
        let buf =
            std::fs::read(path).with_context(|| format!("cannot read '{}'", path.display()))?;
        let origin = format!("'{}'", path.display());
        let content = classify_bytes(&origin, buf)?;
        if content.text.is_none() && content.images.is_empty() && content.audios.is_empty() {
            // Whitespace-only: keep going with the other files, but say so
            // instead of silently dropping this one.
            eprintln!("warning: {origin} is empty or whitespace-only; skipped");
            continue;
        }
        audios.extend(content.audios);
        match content.text {
            Some(text) => texts.push((path.clone(), text)),
            None => images.extend(content.images),
        }
    }

    if texts.is_empty() && images.is_empty() && audios.is_empty() {
        bail!("the given files contain no text, image or audio content");
    }

    // With several text files, label each one so the model can tell them
    // apart; a lone file is passed through untouched.
    let text = match texts.len() {
        0 => None,
        1 => Some(texts.pop().expect("len checked above").1),
        _ => Some(
            texts
                .into_iter()
                .map(|(path, text)| format!("--- {} ---\n\n{text}", path.display()))
                .collect::<Vec<_>>()
                .join("\n\n"),
        ),
    };
    Ok(UserContent {
        text,
        images,
        audios,
    })
}

/// Classify raw bytes by magic number: PNG passes through, JPEG is re-encoded
/// as PNG, supported audio containers keep their bytes; other data must be UTF-8 text. Whitespace-only text yields an
/// empty content for the caller to fall back on.
fn classify_bytes(origin: &str, buf: Vec<u8>) -> Result<UserContent> {
    if buf.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok(UserContent {
            audios: Vec::new(),
            text: None,
            images: vec![buf],
        });
    }
    if buf.starts_with(&[0xFF, 0xD8, 0xFF])
        || (buf.starts_with(b"RIFF") && buf.get(8..12) == Some(b"WEBP"))
    {
        let img = image::load_from_memory(&buf)
            .with_context(|| format!("failed to decode the image from {origin}"))?;
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .context("failed to re-encode image as PNG")?;
        return Ok(UserContent {
            audios: Vec::new(),
            text: None,
            images: vec![png],
        });
    }
    if let Some((mime, extension)) = audio_type(&buf) {
        return Ok(UserContent {
            audios: vec![AudioInput {
                bytes: buf,
                mime: mime.into(),
                filename: format!("input.{extension}"),
            }],
            ..Default::default()
        });
    }
    match String::from_utf8(buf) {
        Ok(text) => Ok(UserContent {
            audios: Vec::new(),
            text: (!text.trim().is_empty()).then_some(text),
            images: Vec::new(),
        }),
        Err(_) => bail!("{origin} is neither valid UTF-8 text nor a supported image/audio file"),
    }
}

pub struct AudioInput {
    pub bytes: Vec<u8>,
    pub mime: String,
    pub filename: String,
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

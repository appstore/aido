use crate::clipboard::{self, ClipboardContent};
use anyhow::{bail, Context, Result};
use std::io::{IsTerminal, Read};

pub enum UserContent {
    Text(String),
    Png(Vec<u8>),
}

/// Piped stdin wins over the clipboard; binary PNG/JPEG on stdin is treated
/// as an image so `aido < shot.png --preset ocr` works headless.
pub fn gather() -> Result<UserContent> {
    if !std::io::stdin().is_terminal() {
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .context("failed to read stdin")?;
        if !buf.is_empty() {
            if buf.starts_with(b"\x89PNG\r\n\x1a\n") {
                return Ok(UserContent::Png(buf));
            }
            if buf.starts_with(&[0xFF, 0xD8, 0xFF]) {
                let img =
                    image::load_from_memory(&buf).context("failed to decode JPEG from stdin")?;
                let mut png = Vec::new();
                img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                    .context("failed to re-encode image as PNG")?;
                return Ok(UserContent::Png(png));
            }
            match String::from_utf8(buf) {
                Ok(text) if !text.trim().is_empty() => return Ok(UserContent::Text(text)),
                Ok(_) => {} // empty stdin: fall back to the clipboard
                Err(_) => bail!("stdin is neither valid UTF-8 text nor a PNG/JPEG image"),
            }
        }
    }

    match clipboard::read()? {
        ClipboardContent::Text(t) => Ok(UserContent::Text(t)),
        ClipboardContent::Png(p) => Ok(UserContent::Png(p)),
    }
}

use anyhow::{anyhow, bail, Context, Result};

pub enum ClipboardContent {
    Text(String),
    Png(Vec<u8>),
}

pub fn read() -> Result<ClipboardContent> {
    let mut cb = arboard::Clipboard::new().map_err(|e| {
        anyhow!("cannot access the clipboard: {e}; hint: pipe text instead, e.g. `echo hello | aido`")
    })?;
    // A clipboard holding an image makes get_text() fail on most platforms,
    // so a text error is only fatal if the image read fails too; keep it for
    // the error message rather than reporting a plain "empty clipboard".
    let text_err = match cb.get_text() {
        Ok(text) if !text.trim().is_empty() => return Ok(ClipboardContent::Text(text)),
        Ok(_) => None, // whitespace-only text: try the image before giving up
        Err(e) => Some(e.to_string()),
    };
    match cb.get_image() {
        Ok(img) => Ok(ClipboardContent::Png(rgba_to_png(img)?)),
        Err(img_err) => {
            let detail = match text_err {
                Some(te) => format!("text read failed: {te}; image read failed: {img_err}"),
                None => format!("only whitespace text; image read failed: {img_err}"),
            };
            bail!(
                "clipboard has no usable text or image ({detail}); hint: copy something first, or pipe text via stdin"
            )
        }
    }
}

fn rgba_to_png(img: arboard::ImageData) -> Result<Vec<u8>> {
    let (w, h) = (img.width as u32, img.height as u32);
    let buf = image::RgbaImage::from_raw(w, h, img.bytes.into_owned())
        .context("clipboard image has invalid dimensions")?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(buf)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .context("failed to encode clipboard image as PNG")?;
    Ok(png)
}

pub fn write_text(text: &str, hold_secs: u64) -> Result<()> {
    {
        let mut cb = arboard::Clipboard::new().map_err(|e| anyhow!("cannot access the clipboard: {e}"))?;
        cb.set_text(text.to_string())
            .map_err(|e| anyhow!("failed to write clipboard: {e}"))?;
    }
    #[cfg(target_os = "linux")]
    spawn_holder(text, hold_secs);
    #[cfg(not(target_os = "linux"))]
    let _ = hold_secs;
    Ok(())
}

/// On X11 (and Wayland data-control, which serves pastes lazily too) the
/// clipboard dies with the process that wrote it. A detached child re-owns
/// the clipboard and holds it for a few seconds, mirroring what `xclip` /
/// `wl-copy` do internally. The child inherits stderr so a failed hold is
/// reported instead of silently dropping the contents.
#[cfg(target_os = "linux")]
fn spawn_holder(text: &str, hold_secs: u64) {
    use std::io::Write;
    use std::process::{Command, Stdio};

    if hold_secs == 0 {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let Ok(mut child) = Command::new(exe)
        .arg("__hold")
        .arg(hold_secs.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
    else {
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
}

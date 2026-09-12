use anyhow::{anyhow, bail, Context, Result};

pub enum ClipboardContent {
    Text(String),
    Png(Vec<u8>),
}

pub fn read() -> Result<ClipboardContent> {
    let mut cb = arboard::Clipboard::new().map_err(|e| {
        anyhow!(
            "cannot access the clipboard: {e}; hint: pipe text instead, e.g. `echo hello | aido`"
        )
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
        let mut cb =
            arboard::Clipboard::new().map_err(|e| anyhow!("cannot access the clipboard: {e}"))?;
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
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
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

/// Decode clipboard-bound image bytes to RGBA. The declared dimensions
/// are read from the container header first — the same
/// decompression-bomb guard the input side uses — so bytes reaching the
/// clipboard (freshly generated or restored from history) cannot OOM
/// this decode.
fn decode_for_clipboard(bytes: &[u8]) -> Result<image::RgbaImage> {
    let (w, h) = crate::api::image_dimensions(bytes)
        .context("failed to read the clipboard image dimensions")?;
    crate::api::ensure_decode_size("clipboard image", w, h)?;
    Ok(image::load_from_memory(bytes)
        .context("failed to decode clipboard image")?
        .into_rgba8())
}

pub fn set_image(cb: &mut arboard::Clipboard, bytes: &[u8]) -> Result<()> {
    let rgba = decode_for_clipboard(bytes)?;
    cb.set_image(arboard::ImageData {
        width: rgba.width() as usize,
        height: rgba.height() as usize,
        bytes: std::borrow::Cow::Owned(rgba.into_raw()),
    })
    .context("failed to write image to clipboard")
}

pub fn write_image(bytes: &[u8], hold_secs: u64) -> Result<()> {
    let mut cb = arboard::Clipboard::new().context("cannot access the clipboard")?;
    set_image(&mut cb, bytes)?;
    #[cfg(target_os = "linux")]
    {
        use std::io::Write;
        use std::process::{Command, Stdio};
        if hold_secs > 0 {
            let mut child = Command::new(std::env::current_exe()?)
                .arg("__hold")
                .arg(hold_secs.to_string())
                .arg("--image")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()?;
            child
                .stdin
                .take()
                .context("clipboard holder stdin unavailable")?
                .write_all(bytes)?;
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = hold_secs;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_jpeg() -> Vec<u8> {
        let img = image::GrayImage::from_pixel(2, 2, image::Luma([128]));
        let mut jpg = Vec::new();
        image::DynamicImage::ImageLuma8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut jpg),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        jpg
    }

    /// A real 2×2 JPEG whose SOF0 segment is patched to declare `w`×`h`:
    /// the decompression-bomb shape — a few hundred bytes claiming a huge
    /// canvas.
    fn jpeg_declaring(w: u32, h: u32) -> Vec<u8> {
        let mut jpg = tiny_jpeg();
        // Layout after the FF C0 marker: length(2), precision(1), height
        // (2 BE), width (2 BE).
        let sof = jpg
            .windows(2)
            .position(|p| p == [0xFF, 0xC0])
            .expect("encoder wrote a SOF0 marker");
        jpg[sof + 5..sof + 7].copy_from_slice(&(h as u16).to_be_bytes());
        jpg[sof + 7..sof + 9].copy_from_slice(&(w as u16).to_be_bytes());
        jpg
    }

    #[test]
    fn clipboard_decode_refuses_bomb_shaped_bytes() {
        let err = decode_for_clipboard(&jpeg_declaring(20_000, 20_000)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("refusing to decode"), "{msg}");
        assert!(msg.contains("400 MP"), "{msg}");
    }

    #[test]
    fn clipboard_decode_returns_rgba_for_small_images() {
        let rgba = decode_for_clipboard(&tiny_jpeg()).unwrap();
        assert_eq!(rgba.dimensions(), (2, 2));
    }
}

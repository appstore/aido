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
    spawn_holder(text.as_bytes(), hold_secs, false);
    #[cfg(not(target_os = "linux"))]
    let _ = hold_secs;
    Ok(())
}

/// On X11 (and Wayland data-control, which serves pastes lazily too) the
/// clipboard dies with the process that wrote it. A detached child re-owns
/// the clipboard and holds it for a few seconds, mirroring what `xclip` /
/// `wl-copy` do internally. The child inherits stderr, and a hold that
/// fails on this side is reported too — as a warning, never a delivery
/// error: the clipboard write has already succeeded, so the delivery stays
/// a success and only the contents' lifetime past this process is at risk.
#[cfg(target_os = "linux")]
fn spawn_holder(payload: &[u8], hold_secs: u64, image: bool) {
    if hold_secs == 0 {
        return;
    }
    let held = match std::env::current_exe() {
        Ok(exe) => hold_via(&exe, payload, hold_secs, image),
        Err(e) => Err(anyhow::Error::new(e).context("cannot locate the aido binary")),
    };
    if let Err(e) = held {
        eprintln!("{}", hold_warning(&e));
    }
}

/// Spawn `<exe> __hold SECS [--image]`, hand it `payload` on stdin, and
/// drop the pipe so the child can take over the clipboard. The binary path
/// is a parameter rather than always `current_exe()` so tests can drive
/// the failure path with a path that cannot be spawned.
#[cfg(target_os = "linux")]
fn hold_via(exe: &std::path::Path, payload: &[u8], hold_secs: u64, image: bool) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(exe);
    cmd.arg("__hold").arg(hold_secs.to_string());
    if image {
        cmd.arg("--image");
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .context("cannot start the holder child")?;
    child
        .stdin
        .take()
        .context("holder child has no stdin")?
        .write_all(payload)
        .context("the holder child did not receive the contents")?;
    Ok(())
}

/// The stderr line a failed hold produces. Kept as its own function so the
/// exact wording the user sees is assertable in tests.
#[cfg(target_os = "linux")]
fn hold_warning(err: &anyhow::Error) -> String {
    format!("warning: clipboard contents may not outlive this process (hold failed: {err:#})")
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
    spawn_holder(bytes, hold_secs, true);
    #[cfg(not(target_os = "linux"))]
    let _ = hold_secs;
    Ok(())
}

#[cfg(test)]
mod tests;

use crate::input::UserContent;
use anyhow::{Context, Result};
use image::DynamicImage;

/// Vision backends shrink oversized images before tokenizing them: the
/// OpenAI convention caps the long edge at 2048 px (with a further squeeze
/// of the short edge), and Qwen-VL-class servers enforce a total pixel
/// budget (`max_pixels`). A scrolling screenshot is several times taller
/// than that, so a whole-image request arrives blurred and the model skips
/// lines — the "long screenshot loses text" failure. Slicing keeps every
/// request inside those limits.
const SPLIT_ABOVE: u32 = 3072;
const MAX_SLICE_HEIGHT: u32 = 2000;
/// Wide images get proportionally shorter slices so each stays under a
/// pixel budget in the same ballpark as common server limits.
const MAX_SLICE_PIXELS: u32 = 4_000_000;
const MIN_SLICE_HEIGHT: u32 = 1024;
/// Don't leave (or bother creating) a tail slice thinner than this.
const MIN_TAIL: u32 = 128;
/// A seam with no quiet row nearby cuts through content; re-showing a thin
/// band lets the split line survive whole in at least one slice.
const HARD_CUT_OVERLAP: u32 = 32;

const SLICE_NOTE: &str = "This image is one slice of a taller image that was \
     split so its text stays legible; process only what is visible in this slice.";

/// Turn one user input into the list of requests to send: tall images are
/// sliced vertically, each slice becoming its own request so neither the
/// image resolution nor the reply length can overflow the model's limits.
/// The slices' replies are meant to be concatenated in order by the caller.
/// Anything that needs no slicing stays exactly as it was (a single
/// request with all images, today's behavior).
pub fn expand(user: UserContent, enabled: bool) -> Result<Vec<UserContent>> {
    if !enabled || user.images.is_empty() || !user.audios.is_empty() {
        return Ok(vec![user]);
    }

    let mut untouched: Vec<Vec<u8>> = Vec::new();
    let mut sliced: Vec<Vec<Vec<u8>>> = Vec::new();
    for png in &user.images {
        match slice_if_tall(png)? {
            Some(chunks) => sliced.push(chunks),
            None => untouched.push(png.clone()),
        }
    }
    if sliced.is_empty() {
        return Ok(vec![user]);
    }

    // One request per slice, in original order; images that needed no
    // slicing travel with the first request.
    let mut text = user.text;
    let mut batches = Vec::new();
    for chunks in sliced {
        for chunk in chunks {
            let first = batches.is_empty();
            let mut images = vec![chunk];
            if first {
                images.extend(untouched.iter().cloned());
            }
            batches.push(UserContent {
                audios: Vec::new(),
                text: if first {
                    text.take()
                } else {
                    Some(SLICE_NOTE.to_string())
                },
                images,
            });
        }
    }
    Ok(batches)
}

fn needs_splitting(w: u32, h: u32) -> bool {
    // h <= SPLIT_ABOVE keeps ordinary screenshots (phone screens included)
    // whole; h <= w keeps landscape photos whole — server-side shrinking is
    // only fatal when the text lines get thin, i.e. tall strips.
    h > SPLIT_ABOVE && h > w
}

fn slice_height_for(w: u32) -> u32 {
    MAX_SLICE_HEIGHT.min((MAX_SLICE_PIXELS / w.max(1)).max(MIN_SLICE_HEIGHT))
}

fn slice_if_tall(png: &[u8]) -> Result<Option<Vec<Vec<u8>>>> {
    // Every image here is PNG (input::gather guarantees it by magic number),
    // so the IHDR — always the first chunk — carries the dimensions without
    // paying for a full decode in the common not-tall case.
    let Some((w, h)) = png_dimensions(png) else {
        return Ok(None);
    };
    if !needs_splitting(w, h) {
        return Ok(None);
    }

    let img =
        image::load_from_memory(png).with_context(|| "failed to decode tall image for slicing")?;
    let rgba = img.to_rgba8();
    let gray = image::imageops::grayscale(&rgba);
    let energies = row_energies(&gray);
    let quiet = quiet_threshold(&energies);

    let slice_h = slice_height_for(w);
    let slack = slice_h / 4;
    let mut cuts: Vec<(u32, bool)> = Vec::new();
    let mut pos: u32 = 0;
    while h - pos > slice_h + MIN_TAIL {
        let target = pos + slice_h;
        let lo = target.saturating_sub(slack).max(pos + 1);
        let hi = (target + slack).min(h - MIN_TAIL);
        if lo >= hi {
            break;
        }
        let (y, hard) = pick_seam(&energies, lo as usize, hi as usize, target as usize, quiet);
        cuts.push((y as u32, hard));
        pos = y as u32;
    }

    let mut chunks = Vec::with_capacity(cuts.len() + 1);
    let mut start: u32 = 0;
    for (y, hard) in cuts {
        chunks.push(encode_slice(&rgba, start, y)?);
        start = y.saturating_sub(if hard { HARD_CUT_OVERLAP } else { 0 });
    }
    chunks.push(encode_slice(&rgba, start, h)?);
    eprintln!(
        "note: tall image ({w}\u{d7}{h}) split into {} slices for legibility",
        chunks.len()
    );
    Ok(Some(chunks))
}

/// Read the size out of the PNG IHDR (the mandatory first chunk) without
/// decoding the stream.
fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(png[20..24].try_into().ok()?);
    Some((w, h))
}

/// Horizontal contrast per row: text lights a row up, blank and smoothly
/// fading rows stay dark. The derivative is horizontal so vertical color
/// gradients don't register as content.
fn row_energies(gray: &image::GrayImage) -> Vec<u32> {
    let (w, h) = gray.dimensions();
    let raw = gray.as_raw();
    let mut energies = Vec::with_capacity(h as usize);
    for y in 0..h as usize {
        let row = &raw[y * w as usize..(y + 1) * w as usize];
        energies.push(row.windows(2).map(|p| p[0].abs_diff(p[1]) as u32).sum());
    }
    energies
}

/// A row counts as blank when its contrast sits far below the typical row.
/// `min(median/16, p20)` keeps the bar adaptive: dense-text images admit
/// nothing (every cut falls back to the overlap), mostly-blank images admit
/// only genuinely smooth rows, and noisy (JPEG-origin) images still get
/// their blank rows recognized.
fn quiet_threshold(energies: &[u32]) -> u32 {
    let mut sorted = energies.to_vec();
    sorted.sort_unstable();
    let at = |p: usize| sorted[p.min(sorted.len() - 1)];
    let median = at(sorted.len() / 2);
    let p20 = at(sorted.len() / 5);
    (median / 16).min(p20).max(1)
}

/// Choose the cut row within `[lo, hi]`: the quiet row closest to `target`
/// when one exists, otherwise the quietest row — a "hard" cut the caller
/// compensates with a small overlap.
fn pick_seam(energies: &[u32], lo: usize, hi: usize, target: usize, quiet: u32) -> (usize, bool) {
    let mut nearest_quiet: Option<(usize, usize)> = None; // (distance, row)
    let mut calmest = lo;
    for y in lo..=hi {
        let e = energies[y];
        if e < energies[calmest] {
            calmest = y;
        }
        if e <= quiet {
            let d = y.abs_diff(target);
            if nearest_quiet.is_none_or(|(bd, _)| d < bd) {
                nearest_quiet = Some((d, y));
            }
        }
    }
    match nearest_quiet {
        Some((_, y)) => (y, false),
        None => (calmest, true),
    }
}

fn encode_slice(rgba: &image::RgbaImage, start: u32, end: u32) -> Result<Vec<u8>> {
    let (w, _) = rgba.dimensions();
    let crop = image::imageops::crop_imm(rgba, 0, start, w, end - start).to_image();
    let mut png = Vec::new();
    DynamicImage::ImageRgba8(crop)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .context("failed to encode image slice as PNG")?;
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::{expand, needs_splitting, png_dimensions, slice_height_for, HARD_CUT_OVERLAP};
    use crate::input::UserContent;

    fn solid_png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([255, 255, 255, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        png
    }

    /// Alternating black/white pixels: every row looks like a line of text.
    fn striped_png(w: u32, h: u32) -> Vec<u8> {
        let mut img = image::RgbaImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let c = if (x + y) % 2 == 0 { 0 } else { 255 };
                img.put_pixel(x, y, image::Rgba([c, c, c, 255]));
            }
        }
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        png
    }

    /// Text-like content with a blank band, i.e. the shape of a real
    /// scrolling screenshot with spacing between blocks.
    fn band_png(w: u32, h: u32, blank: std::ops::Range<u32>) -> Vec<u8> {
        let mut img = image::RgbaImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let c = if blank.contains(&y) || (x + y) % 2 == 1 {
                    255
                } else {
                    0
                };
                img.put_pixel(x, y, image::Rgba([c, c, c, 255]));
            }
        }
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        png
    }

    fn dims(png: &[u8]) -> (u32, u32) {
        let img = image::load_from_memory(png).unwrap();
        use image::GenericImageView as _;
        img.dimensions()
    }

    #[test]
    fn split_gates() {
        assert!(needs_splitting(100, 3200)); // tall strip
        assert!(!needs_splitting(100, 3072)); // at the threshold
        assert!(!needs_splitting(100, 2400)); // phone screenshot
        assert!(!needs_splitting(3200, 3100)); // wide, not tall
        assert_eq!(slice_height_for(1080), 2000);
        assert_eq!(slice_height_for(3000), 1333); // pixel budget binds
        assert_eq!(slice_height_for(6000), 1024); // floor binds
    }

    #[test]
    fn short_images_pass_through_untouched() {
        let png = solid_png(100, 500);
        let user = UserContent {
            audios: Vec::new(),
            text: Some("hi".into()),
            images: vec![png.clone()],
        };
        let batches = expand(user, true).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].images, vec![png]);
        assert_eq!(batches[0].text.as_deref(), Some("hi"));
    }

    #[test]
    fn disabled_passes_everything_through() {
        let user = UserContent {
            audios: Vec::new(),
            text: None,
            images: vec![striped_png(100, 3200)],
        };
        let batches = expand(user, false).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].images.len(), 1);
    }

    #[test]
    fn seams_prefer_quiet_rows() {
        // 100x3300, blank band at rows 1500..1700; the target seam is 2000,
        // so the cut should land on the blank row closest to it (1699) and
        // the two slices should tile the image exactly.
        let user = UserContent {
            audios: Vec::new(),
            text: None,
            images: vec![band_png(100, 3300, 1500..1700)],
        };
        let batches = expand(user, true).unwrap();
        assert_eq!(batches.len(), 2);
        let (w1, h1) = dims(&batches[0].images[0]);
        let (w2, h2) = dims(&batches[1].images[0]);
        assert_eq!((w1, w2), (100, 100));
        assert_eq!(h1, 1699);
        assert_eq!(h2, 1601);
        // later slices explain themselves instead of reusing the first text
        assert!(batches[1].text.as_deref().unwrap().contains("slice"));
    }

    #[test]
    fn hard_cuts_overlap() {
        // No blank rows anywhere: the fallback cut re-shows a thin band so
        // the line it cuts through survives whole in one of the slices.
        let user = UserContent {
            audios: Vec::new(),
            text: None,
            images: vec![striped_png(100, 3200)],
        };
        let batches = expand(user, true).unwrap();
        assert_eq!(batches.len(), 2);
        let (_, h1) = dims(&batches[0].images[0]);
        let (_, h2) = dims(&batches[1].images[0]);
        assert_eq!(h1 + h2, 3200 + HARD_CUT_OVERLAP);
    }

    #[test]
    fn untouched_images_travel_with_the_first_slice() {
        let user = UserContent {
            audios: Vec::new(),
            text: None,
            images: vec![solid_png(100, 500), striped_png(100, 3200)],
        };
        let batches = expand(user, true).unwrap();
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].images.len(), 2); // slice + the short image
        assert_eq!(batches[1].images.len(), 1);
    }

    #[test]
    fn ihdr_dimensions() {
        assert_eq!(png_dimensions(&solid_png(64, 33)), Some((64, 33)));
        assert_eq!(png_dimensions(b"not a png"), None);
    }
}

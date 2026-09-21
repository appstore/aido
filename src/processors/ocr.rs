//! The OCR tiling strategy.
//!
//! Vision backends shrink oversized images before tokenizing them (the
//! OpenAI convention caps the long edge at 2048 px; Qwen-VL-class servers
//! enforce a total pixel budget), so a scrolling screenshot arrives
//! blurred and the model skips lines. Slicing keeps every request inside
//! those limits; slice replies merge at their boundaries, removing only
//! lines the overlap bands genuinely duplicated.

use super::{carry_guard, step_material, synthetic_text, RequestStep, StepRole};
use crate::api::{ensure_decode_size, image_dimensions};
use crate::domain::{InputPart, MediaKind};
use anyhow::{Context, Result};
use image::DynamicImage;

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

/// Lines the merge gate holds back at a slice boundary while deciding
/// whether the next slice repeats them.
const MERGE_WINDOW: usize = 3;

pub const SLICE_NOTE: &str = "This image is one slice of a taller image that was \
     split so its text stays legible; process only what is visible in this slice.";

/// Plan the request sequence: unsliced material rides with every slice's
/// request in command-line order while it fits the carry budget
/// ([`carry_guard`]); past it, the material travels with the first request
/// only. Each tall image's slices follow in order, and every slice request
/// carries the task's instruction (the runner re-attaches it). `quiet`
/// suppresses the split note on stderr.
pub fn plan_steps(inputs: &[InputPart], quiet: bool) -> Result<Vec<RequestStep>> {
    let mut untouched: Vec<InputPart> = Vec::new();
    // (source part, slices, per-slice hard flags): flag i marks the
    // boundary after slice i, whose overlap band slice i+1 re-shows.
    let mut sliced: Vec<(&InputPart, Vec<InputPart>, Vec<bool>)> = Vec::new();
    for part in inputs {
        if part.kind == MediaKind::Image {
            if let Some((chunks, hard_flags)) = slice_if_tall(part, quiet)? {
                sliced.push((part, chunks, hard_flags));
                continue;
            }
        }
        untouched.push(part.clone());
    }
    if sliced.is_empty() {
        return Ok(vec![RequestStep {
            index: 0,
            inputs: inputs.to_vec(),
            label: "all material".into(),
            hard_cut_end: false,
            part: None,
            artifact_stem: None,
            role: StepRole::Map,
        }]);
    }
    let split: Vec<&InputPart> = sliced.iter().map(|(part, _, _)| *part).collect();
    let carry_every = carry_guard(&untouched, quiet);

    let mut steps = Vec::new();
    for (source, chunks, hard_flags) in &sliced {
        let part = *source;
        let total = chunks.len();
        for (i, chunk) in chunks.iter().enumerate() {
            let mut step_inputs = Vec::new();
            if !steps.is_empty() {
                step_inputs.push(synthetic_text(usize::MAX, "slice note", SLICE_NOTE));
            }
            // Real material keeps the command-line order, this slice
            // standing in at its source part's position. Unsliced material
            // rides with every slice while it fits the budget; past it,
            // only the run's first request carries it.
            let carry = steps.is_empty() || carry_every;
            step_inputs.extend(step_material(inputs, part, chunk.clone(), &split, carry));
            steps.push(RequestStep {
                index: steps.len(),
                inputs: step_inputs,
                label: format!("slice {}/{} of {}", i + 1, total, part.name),
                // The merge gate may only compare at a boundary that
                // re-shows an overlap band: cut i's hardness belongs to
                // the step whose bottom edge it cuts, and a tail slice
                // has no next step at all.
                hard_cut_end: hard_flags[i],
                part: None,
                artifact_stem: None,
                role: StepRole::Map,
            });
        }
    }
    Ok(steps)
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

fn slice_if_tall(part: &InputPart, quiet: bool) -> Result<Option<(Vec<InputPart>, Vec<bool>)>> {
    let bytes = match &part.content {
        crate::domain::InputContent::Media(b) => b,
        _ => anyhow::bail!("'{}' is not an image", part.name),
    };
    // Dimensions come from the container header alone (PNG IHDR, JPEG SOF,
    // WebP VP8X), so the decompression-bomb guard runs before any pixel is
    // decoded — and an image that needs no slicing is not decoded here at
    // all; its full decode, if any, happens once at the adapter boundary.
    let (w, h) = image_dimensions(bytes)
        .with_context(|| format!("cannot read the dimensions of image '{}'", part.name))?;
    ensure_decode_size(&part.name, w, h)?;
    if !needs_splitting(w, h) {
        return Ok(None);
    }

    // Decode the original bytes once — the intermediate PNG the adapter
    // boundary needs would cost a second full decode plus a re-encode.
    let img = image::load_from_memory(bytes)
        .with_context(|| format!("failed to decode tall image '{}'", part.name))?;
    let rgba = img.to_rgba8();
    let gray = image::imageops::grayscale(&rgba);
    let energies = row_energies(&gray);
    let quiet_row = quiet_threshold(&energies);

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
        let (y, hard) = pick_seam(
            &energies,
            lo as usize,
            hi as usize,
            target as usize,
            quiet_row,
        );
        cuts.push((y as u32, hard));
        pos = y as u32;
    }

    // Flag i marks the boundary between slice i and slice i+1 — exactly
    // the cut whose overlap band slice i+1 re-shows at its top. The last
    // slice has no next step, so its flag is false.
    let hard_flags: Vec<bool> = cuts.iter().map(|(_, hard)| *hard).chain([false]).collect();
    let mut chunks = Vec::with_capacity(cuts.len() + 1);
    let mut start: u32 = 0;
    for (y, hard) in cuts {
        chunks.push(encode_slice(part, &rgba, start, y)?);
        start = y.saturating_sub(if hard { HARD_CUT_OVERLAP } else { 0 });
    }
    chunks.push(encode_slice(part, &rgba, start, h)?);
    if !quiet {
        eprintln!(
            "note: tall image '{}' ({w}\u{d7}{h}) split into {} slices for legibility",
            part.name,
            chunks.len()
        );
    }
    Ok(Some((chunks, hard_flags)))
}

fn encode_slice(
    part: &InputPart,
    rgba: &image::RgbaImage,
    start: u32,
    end: u32,
) -> Result<InputPart> {
    let (w, _) = rgba.dimensions();
    let crop = image::imageops::crop_imm(rgba, 0, start, w, end - start).to_image();
    let mut png = Vec::new();
    DynamicImage::ImageRgba8(crop)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .context("failed to encode image slice as PNG")?;
    Ok(InputPart {
        id: part.id,
        source: part.source.clone(),
        name: format!("{} [slice {}..{}]", part.name, start, end),
        kind: MediaKind::Image,
        unknown_kind: false,
        mime: "image/png".into(),
        content: crate::domain::InputContent::Media(png),
        // A slice is its source part's material, so it stays in the
        // source's sub-document unit.
        unit: part.unit.clone(),
    })
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

// ---------------------------------------------------------------------------
// Boundary merge gate
// ---------------------------------------------------------------------------

/// Joins slice replies line by line, so live and buffered delivery end up
/// byte-identical. At hard-cut boundaries (where the overlap band re-shows
/// content) the newest `MERGE_WINDOW` lines of a slice are held back until
/// the next slice's head is known; only lines that genuinely repeat are
/// removed. Quiet boundaries and normal output pass straight through.
pub struct BoundaryGate {
    emit: Box<dyn FnMut(&str)>,
    /// Completed lines of the current slice, not yet emitted (≤ window).
    held: Vec<String>,
    /// Text after the last newline of the current slice.
    tail: String,
    /// When comparing: the next slice's head lines buffered for the match.
    incoming: Vec<String>,
    comparing: bool,
    finished: bool,
}

impl BoundaryGate {
    pub fn new(emit: impl FnMut(&str) + 'static) -> Self {
        Self {
            emit: Box::new(emit),
            held: Vec::new(),
            tail: String::new(),
            incoming: Vec::new(),
            comparing: false,
            finished: false,
        }
    }

    fn out(&mut self, text: &str) {
        if !text.is_empty() {
            (self.emit)(text);
        }
    }

    /// Feed a delta of the current slice's reply.
    pub fn push_delta(&mut self, delta: &str) {
        if self.finished {
            return;
        }
        self.tail.push_str(delta);
        while let Some(pos) = self.tail.find('\n') {
            let line: String = self.tail.drain(..=pos).collect();
            let line = line.trim_end_matches('\n').to_string();
            self.line(line);
        }
    }

    fn line(&mut self, line: String) {
        if self.comparing {
            self.incoming.push(line);
            let matched = self.match_len();
            // A longer match may still align against a different window of
            // held lines, so the decision only waits for the window to
            // fill — or for every held line to have matched.
            if self.incoming.len() >= MERGE_WINDOW || matched == self.held.len() {
                self.resolve_compare();
            }
            return;
        }
        if self.held.len() >= MERGE_WINDOW {
            let oldest = self.held.remove(0);
            self.out(&oldest);
            self.out("\n");
        }
        self.held.push(line);
    }

    /// Longest k where the k newest held lines equal the first k incoming
    /// lines — the overlap band's genuine duplication.
    fn match_len(&self) -> usize {
        let max = self.held.len().min(self.incoming.len());
        (0..=max)
            .rev()
            .find(|&k| {
                let held_tail = &self.held[self.held.len() - k..];
                &self.incoming[..k] == held_tail
            })
            .unwrap_or(0)
    }

    fn resolve_compare(&mut self) {
        self.comparing = false;
        let matched = self.match_len();
        // The held lines are the first occurrence of the content: emit all
        // of them; the duplicated head of the incoming slice is dropped.
        let held = std::mem::take(&mut self.held);
        for line in held {
            self.out(&line);
            self.out("\n");
        }
        let incoming = std::mem::take(&mut self.incoming);
        for line in incoming.into_iter().skip(matched) {
            self.line(line);
        }
    }

    /// The current slice's reply is complete. `hard` marks that its bottom
    /// edge was cut through content with an overlap band.
    pub fn slice_end(&mut self, hard: bool) {
        if self.finished {
            return;
        }
        if self.comparing {
            self.resolve_compare();
        }
        // Terminate the partial line: slices join on line boundaries.
        if !self.tail.is_empty() {
            let line = std::mem::take(&mut self.tail);
            self.line(line);
        }
        if hard {
            self.comparing = true;
            self.incoming.clear();
        } else {
            self.flush_held();
        }
    }

    /// No further slices: emit everything.
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        if self.comparing {
            self.resolve_compare();
        }
        self.flush_held();
        if !self.tail.is_empty() {
            let tail = std::mem::take(&mut self.tail);
            self.out(&tail);
        }
        self.finished = true;
    }

    fn flush_held(&mut self) {
        let held = std::mem::take(&mut self.held);
        for line in held {
            self.out(&line);
            self.out("\n");
        }
    }
}

#[cfg(test)]
mod tests;

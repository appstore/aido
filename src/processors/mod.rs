//! Processing strategies: how ordered material becomes requests.
//!
//! `single` sends everything in one request. `ocr-tiles` slices tall
//! images so their text stays legible, merges slice replies at their
//! boundaries, and is the strategy OCR tasks declare. `chunk-join` splits
//! oversized text at paragraph boundaries, one request per chunk with the
//! previous chunk's tail carried along as context, and joins the replies
//! in order; `chunk-reduce` maps the same chunks and then sends one more
//! reduce request that consolidates the chunk replies into a single
//! final result under the task's instruction. On top of any of these,
//! `per-part` (a task-level `per_part = true` flag) turns a multi-file
//! run into one request sequence per file — see [`perpart`].

pub mod chunk;
pub mod ocr;
pub mod perpart;

use crate::domain::{InputContent, InputPart, MediaKind};
use crate::tasks::ProcessorKind;
use anyhow::Result;

/// A step's role in its strategy. Map steps turn one piece of material
/// into a reply (the ordinary case); a reduce step consolidates the map
/// replies of its group into the run's final text, so its material does
/// not exist at planning time — the runner fills it in from the map
/// replies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StepRole {
    #[default]
    Map,
    Reduce,
}

/// One planned request. `hard_cut_end` marks that the boundary to the
/// next step (if any) cuts through content with an overlap band, so the
/// merge gate looks for duplicated lines there. In a per-part batch,
/// `part` tags the input part the step belongs to and `artifact_stem`
/// names the artifact on the part's first step (`None` outside a batch).
#[derive(Debug)]
pub struct RequestStep {
    pub index: usize,
    pub inputs: Vec<InputPart>,
    /// Human label for progress, e.g. "slice 2/3 of long.png".
    pub label: String,
    pub hard_cut_end: bool,
    /// The tagged input part's id in a per-part batch.
    pub part: Option<usize>,
    /// The artifact name stem for this part, carried by the part's first
    /// step (`a.png` → `a`).
    pub artifact_stem: Option<String>,
    /// Map or reduce; see [`StepRole`]. A reduce step's `inputs` are
    /// empty placeholders until the runner fills them from the map
    /// replies.
    pub role: StepRole,
}

/// Decide the request sequence for the chosen processor. `quiet`
/// suppresses side-channel notes (stderr).
pub fn plan_steps(
    inputs: &[InputPart],
    kind: ProcessorKind,
    quiet: bool,
) -> Result<Vec<RequestStep>> {
    dispatch(inputs, kind, quiet)
}

fn dispatch(inputs: &[InputPart], kind: ProcessorKind, quiet: bool) -> Result<Vec<RequestStep>> {
    match kind {
        ProcessorKind::Single => Ok(vec![RequestStep {
            index: 0,
            inputs: inputs.to_vec(),
            label: "all material".into(),
            hard_cut_end: false,
            part: None,
            artifact_stem: None,
            role: StepRole::Map,
        }]),
        ProcessorKind::OcrTiles => ocr::plan_steps(inputs, quiet),
        ProcessorKind::ChunkJoin => chunk::plan_steps(inputs, quiet),
        ProcessorKind::ChunkReduce => chunk::plan_steps_reduce(inputs, quiet),
    }
}

/// A text part synthesized by a processor (slice notes).
pub(crate) fn synthetic_text(id: usize, name: &str, text: &str) -> InputPart {
    InputPart {
        id,
        source: crate::domain::InputSource::Literal,
        name: name.into(),
        kind: MediaKind::Text,
        unknown_kind: false,
        mime: "text/plain".into(),
        content: InputContent::Text(text.into()),
        unit: None,
    }
}

/// Character budget for unsliced material riding with every split request
/// (chunk or image slice): half a chunk's target. Repeating more than this
/// in each request costs more tokens than the context is worth.
pub(crate) const MAX_CARRY_CHARS: usize = chunk::TARGET_CHUNK_CHARS / 2;

/// Decide whether unsliced material may ride with every split request
/// (`true`) or must fall back to traveling with the first request only.
/// The fallback explains itself with one stderr note unless `quiet`.
pub(crate) fn carry_guard(untouched: &[InputPart], quiet: bool) -> bool {
    let chars = text_chars(untouched);
    if chars <= MAX_CARRY_CHARS {
        return true;
    }
    if !quiet {
        eprintln!(
            "note: context material ({chars} chars) exceeds half the chunk budget; \
             it travels with the first request only"
        );
    }
    false
}

/// Total text characters across parts — the measure the carry budget is
/// stated in (media parts carry no text and add nothing).
pub(crate) fn text_chars(parts: &[InputPart]) -> usize {
    parts
        .iter()
        .map(|p| p.text().unwrap_or_default().chars().count())
        .sum()
}

/// One split step's real material, in command-line order: `piece` stands
/// in at its source part's position (a chunk replaces the oversized text,
/// a slice the tall image), the other split parts' pieces stay out — each
/// travels in its own steps — and unsliced material rides along unless
/// `carry` is false, the budget fallback that leaves it on the first
/// request. The source part and the split parts are matched by `id`
/// (unique within a run: parts are numbered at gather time), not by
/// pointer, so a cloned `InputPart` still matches — a pointer comparison
/// would silently drop the piece if a `.clone()` ever slipped between
/// slicing and calling.
pub(crate) fn step_material(
    inputs: &[InputPart],
    source: &InputPart,
    piece: InputPart,
    split: &[&InputPart],
    carry: bool,
) -> Vec<InputPart> {
    let mut piece = Some(piece);
    let mut out = Vec::with_capacity(inputs.len());
    for input in inputs {
        if input.id == source.id {
            // The source's id is unique among `inputs`, so its piece slots
            // in exactly once.
            out.push(piece.take().unwrap());
        } else if carry && !split.iter().any(|p| p.id == input.id) {
            out.push(input.clone());
        }
    }
    // The id contract violated (a source id absent from `inputs`) would
    // silently drop the piece from the request; make it loud in debug.
    debug_assert!(
        piece.is_none(),
        "step_material: the source part ({}) was not found in inputs",
        source.id
    );
    out
}

#[cfg(test)]
mod tests;

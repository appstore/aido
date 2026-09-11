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
fn text_chars(parts: &[InputPart]) -> usize {
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
/// request.
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
        if std::ptr::eq(input, source) {
            // `source` is one of `inputs`, so its piece slots in exactly once.
            out.push(piece.take().unwrap());
        } else if carry && !split.iter().any(|p| std::ptr::eq(*p, input)) {
            out.push(input.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{InputContent, InputSource};

    fn part(id: usize, name: &str) -> InputPart {
        InputPart {
            id,
            source: InputSource::Literal,
            name: name.into(),
            kind: MediaKind::Text,
            unknown_kind: false,
            mime: "text/plain".into(),
            content: InputContent::Text(format!("body of {name}")),
        }
    }

    fn names(parts: &[InputPart]) -> Vec<&str> {
        parts.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn material_keeps_command_line_order_with_the_piece_in_place() {
        let inputs = [part(0, "glossary"), part(1, "book"), part(2, "notes")];
        let piece = part(1, "book [chunk 1/2]");
        let split = vec![&inputs[1]];
        let material = step_material(&inputs, &inputs[1], piece, &split, true);
        assert_eq!(
            names(&material),
            vec!["glossary", "book [chunk 1/2]", "notes"]
        );
    }

    #[test]
    fn other_split_parts_stay_out_of_a_step() {
        let inputs = [part(0, "glossary"), part(1, "a"), part(2, "b")];
        let piece = part(1, "a [chunk 1/2]");
        let split = vec![&inputs[1], &inputs[2]];
        let material = step_material(&inputs, &inputs[1], piece, &split, true);
        assert_eq!(names(&material), vec!["glossary", "a [chunk 1/2]"]);
    }

    #[test]
    fn no_carry_leaves_only_the_piece() {
        let inputs = [part(0, "glossary"), part(1, "book")];
        let piece = part(1, "book [chunk 2/2]");
        let split = vec![&inputs[1]];
        let material = step_material(&inputs, &inputs[1], piece, &split, false);
        assert_eq!(names(&material), vec!["book [chunk 2/2]"]);
    }

    #[test]
    fn carry_budget_counts_text_chars_and_announces_the_fallback() {
        let small = vec![part(0, "glossary")];
        assert!(carry_guard(&small, true), "small material may ride along");
        // Just over half the chunk target: too big to repeat per request.
        let big = vec![part(0, &"词".repeat(MAX_CARRY_CHARS + 1))];
        assert!(!carry_guard(&big, true), "quiet suppresses only the note");
    }
}

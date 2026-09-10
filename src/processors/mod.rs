//! Processing strategies: how ordered material becomes requests.
//!
//! `single` sends everything in one request. `ocr-tiles` slices tall
//! images so their text stays legible, merges slice replies at their
//! boundaries, and is the strategy OCR tasks declare. `chunk-map-reduce`
//! splits oversized text at paragraph boundaries, one request per chunk
//! with the previous chunk's tail carried along as context, and joins the
//! replies in order.

pub mod chunk;
pub mod ocr;

use crate::domain::{InputContent, InputPart, MediaKind};
use crate::tasks::ProcessorKind;
use anyhow::Result;

/// One planned request. `hard_cut_end` marks that the boundary to the
/// next step (if any) cuts through content with an overlap band, so the
/// merge gate looks for duplicated lines there.
#[derive(Debug)]
pub struct RequestStep {
    pub index: usize,
    pub inputs: Vec<InputPart>,
    /// Human label for progress, e.g. "slice 2/3 of long.png".
    pub label: String,
    pub hard_cut_end: bool,
}

/// Decide the request sequence for the chosen processor. `quiet`
/// suppresses side-channel notes (stderr).
pub fn plan_steps(
    inputs: &[InputPart],
    kind: ProcessorKind,
    quiet: bool,
) -> Result<Vec<RequestStep>> {
    match kind {
        ProcessorKind::Single => Ok(vec![RequestStep {
            index: 0,
            inputs: inputs.to_vec(),
            label: "all material".into(),
            hard_cut_end: false,
        }]),
        ProcessorKind::OcrTiles => ocr::plan_steps(inputs, quiet),
        ProcessorKind::ChunkMapReduce => chunk::plan_steps(inputs, quiet),
    }
}

/// A text part synthesized by a processor (slice notes).
pub(crate) fn synthetic_text(id: usize, name: &str, text: &str) -> InputPart {
    InputPart {
        id,
        source: crate::domain::InputSource::Literal,
        name: name.into(),
        kind: MediaKind::Text,
        mime: "text/plain".into(),
        content: InputContent::Text(text.into()),
    }
}

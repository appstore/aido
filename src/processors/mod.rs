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

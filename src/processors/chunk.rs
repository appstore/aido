//! The chunk strategies for long text: `chunk-join` and `chunk-reduce`.
//!
//! Text that outgrows the model's context window — or, for tasks whose
//! reply scales with the input like translation, its output budget —
//! fails as one request. Both strategies split oversized text parts at
//! paragraph boundaries and send one request per chunk (the runner
//! attaches the task's instruction to every request). They differ in how
//! the per-chunk replies become the result: `chunk-join` (translate)
//! joins them in order — a translation's pieces are the final text —
//! while `chunk-reduce` (summarize) sends one more request whose material
//! is the collected chunk replies, so the task's instruction is applied
//! once more to the whole document and the reply is a single consolidated
//! result, not one section per chunk.
//!
//! Continuity across a cut comes from *context carry*, not output overlap:
//! every chunk after the first also sees a short excerpt of the previous
//! chunk's tail, marked "context only". An overlap band re-shown as
//! processable text would have to be deduplicated from the replies, and
//! exact-match dedup only works when the reply is a deterministic
//! transcription (OCR) — a translation renders the same source differently
//! each time. With context carry the outputs never repeat, so the join
//! step of `chunk-join` is a plain join with a paragraph break.

use super::{carry_guard, step_material, synthetic_text, RequestStep, StepRole};
use crate::domain::{InputContent, InputPart, MediaKind};
use anyhow::Result;

/// Packing target per chunk, in characters — not bytes, which would make
/// CJK chunks three times smaller than Latin ones for the same token
/// budget (CJK is roughly one token per char; Latin roughly four chars).
pub(crate) const TARGET_CHUNK_CHARS: usize = 4000;
/// A tail chunk smaller than this folds into the previous chunk instead.
const MIN_TAIL_CHARS: usize = 500;
/// Context excerpt from the previous chunk, shown to every later chunk.
const CONTEXT_CHARS: usize = 400;

pub const CHUNK_NOTE: &str = "This text is one chunk of a longer document, split so each \
     request stays within the context window; the results of all chunks are joined in \
     order. Process only the chunk text.";

const CHUNK_REDUCE_NOTE: &str = "This text is one chunk of a longer document, split so each \
     request stays within the context window; the results of all chunks are consolidated \
     into one final result. Process only the chunk text.";

/// The reduce request's material opens with this explanation; the task's
/// original instruction rides along as the request's instruction channel,
/// so the model knows both what to do and that the pieces are one
/// document.
pub const REDUCE_NOTE: &str = "This material is the per-chunk results of one longer \
     document; each chunk was already processed under the same instruction, and the \
     '--- result i of N ---' markers separate the sections. Produce the single final \
     result for the whole document, as the instruction asks.";

const CONTEXT_NOTE: &str = "Context from the end of the previous chunk, for continuity \
     only — do not process or output it:";

/// Plan the chunk-join sequence: unsliced material rides with every
/// chunk's request in command-line order while it fits the carry budget,
/// each oversized text part's chunks follow in order, and `quiet`
/// suppresses the split note on stderr.
pub fn plan_steps(inputs: &[InputPart], quiet: bool) -> Result<Vec<RequestStep>> {
    plan_map_steps(inputs, quiet, CHUNK_NOTE)
}

/// Plan the chunk-reduce sequence: the same map steps as the join
/// strategy, plus one final reduce step whose material is the map
/// replies. That material does not exist at planning time — the step
/// carries empty inputs and the runner fills them in — so the plan is
/// honest only together with the runner's reduce handling. A single chunk
/// is already the whole document and takes no reduce step.
pub fn plan_steps_reduce(inputs: &[InputPart], quiet: bool) -> Result<Vec<RequestStep>> {
    let mut steps = plan_map_steps(inputs, quiet, CHUNK_REDUCE_NOTE)?;
    if steps.len() > 1 {
        let maps = steps.len();
        steps.push(RequestStep {
            index: steps.len(),
            // Placeholders: the map replies become the material at run
            // time, built by `reduce_inputs`.
            inputs: Vec::new(),
            label: format!("consolidate {maps} chunks"),
            hard_cut_end: false,
            part: None,
            artifact_stem: None,
            role: StepRole::Reduce,
        });
    }
    Ok(steps)
}

/// The map phase shared by both strategies: unsliced material rides with
/// every chunk's request — in command-line order, the chunk standing in
/// at its source part's position — while it fits the carry budget
/// ([`carry_guard`]); past it, the material travels with the first
/// request only. Each oversized text part's chunks follow in order, and
/// every step after the first explains itself with `note`. `quiet`
/// suppresses the split notes on stderr.
fn plan_map_steps(inputs: &[InputPart], quiet: bool, note: &str) -> Result<Vec<RequestStep>> {
    let mut untouched: Vec<InputPart> = Vec::new();
    let mut chunked: Vec<(&InputPart, Vec<String>)> = Vec::new();
    for part in inputs {
        if part.kind == MediaKind::Text {
            if let Some(chunks) = chunk_if_long(part) {
                if !quiet {
                    eprintln!(
                        "note: long text '{}' ({} chars) split into {} chunks",
                        part.name,
                        char_len(part.text().unwrap_or_default()),
                        chunks.len()
                    );
                }
                chunked.push((part, chunks));
                continue;
            }
        }
        untouched.push(part.clone());
    }
    if chunked.is_empty() {
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
    let split: Vec<&InputPart> = chunked.iter().map(|(part, _)| *part).collect();
    let carry_every = carry_guard(&untouched, quiet);

    let mut steps = Vec::new();
    for (source, chunks) in &chunked {
        let part = *source;
        let total = chunks.len();
        for (i, chunk_text) in chunks.iter().enumerate() {
            let mut step_inputs = Vec::new();
            if !steps.is_empty() {
                step_inputs.push(synthetic_text(usize::MAX, "chunk note", note));
            }
            if i > 0 {
                let context = context_tail(&chunks[i - 1]);
                step_inputs.push(synthetic_text(
                    usize::MAX,
                    "previous chunk context",
                    &format!("{CONTEXT_NOTE}\n\n{context}"),
                ));
            }
            // Real material keeps the command-line order, this chunk
            // standing in at its source part's position. Unsliced material
            // rides with every chunk while it fits the budget; past it,
            // only the run's first request carries it.
            let carry = steps.is_empty() || carry_every;
            let chunk = chunk_part(part, i, total, chunk_text);
            step_inputs.extend(step_material(inputs, part, chunk, &split, carry));
            steps.push(RequestStep {
                index: steps.len(),
                inputs: step_inputs,
                label: format!("chunk {}/{} of {}", i + 1, total, part.name),
                // Context carry keeps the outputs duplication-free; there
                // is no overlap band, so the join never needs a dedup gate.
                hard_cut_end: false,
                part: None,
                artifact_stem: None,
                role: StepRole::Map,
            });
        }
    }
    Ok(steps)
}

/// Build the reduce request's material from the collected map replies: a
/// consolidation note plus one labeled section per reply, so the model
/// sees distinct sections of one document instead of one anonymous wall
/// of text. Each reply carries the index of the request that produced it;
/// the material itself ignores it (a reply's place in the sequence is its
/// position), so the runner can also use the pair to give a kept
/// intermediate reply the right provenance when the run fails.
pub fn reduce_inputs(sections: &[(usize, String)]) -> Vec<InputPart> {
    let total = sections.len();
    let mut parts = vec![synthetic_text(
        usize::MAX,
        "consolidation note",
        REDUCE_NOTE,
    )];
    for (i, (_, text)) in sections.iter().enumerate() {
        parts.push(synthetic_text(
            usize::MAX,
            &format!("chunk {}/{} result", i + 1, total),
            &format!("--- result {} of {total} ---\n\n{}", i + 1, text.trim()),
        ));
    }
    parts
}

fn chunk_if_long(part: &InputPart) -> Option<Vec<String>> {
    let text = part.text()?;
    if char_len(text) <= TARGET_CHUNK_CHARS {
        return None;
    }
    let chunks = split_text(text);
    // Whitespace-only text yields no chunks at all, and the tail fold can
    // leave a single chunk: both travel whole in the ordinary single
    // request path instead of staging a run with zero or one request.
    if chunks.len() > 1 {
        Some(chunks)
    } else {
        None
    }
}

fn chunk_part(part: &InputPart, i: usize, total: usize, text: &str) -> InputPart {
    InputPart {
        id: part.id,
        source: part.source.clone(),
        name: format!("{} [chunk {}/{}]", part.name, i + 1, total),
        kind: MediaKind::Text,
        unknown_kind: false,
        mime: part.mime.clone(),
        content: InputContent::Text(text.into()),
        // A chunk is its source part's material, so it stays in the
        // source's sub-document unit.
        unit: part.unit.clone(),
    }
}

/// The last `CONTEXT_CHARS` characters of a chunk, on a char boundary.
/// A cut that straddles a sentence is exactly what the excerpt is for.
fn context_tail(text: &str) -> String {
    let total = char_len(text);
    if total <= CONTEXT_CHARS {
        return text.to_owned();
    }
    text.chars().skip(total - CONTEXT_CHARS).collect()
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

/// Split into chunks: paragraphs become units; a paragraph over the target
/// is re-split into sentence groups; a single sentence over the target is
/// hard-cut at the char boundary. Units then pack greedily up to the
/// target, and a too-small tail chunk folds back into its predecessor.
fn split_text(text: &str) -> Vec<String> {
    pack_units(units(text))
}

fn units(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for para in paragraphs(text) {
        if char_len(&para) <= TARGET_CHUNK_CHARS {
            out.push(para);
            continue;
        }
        let mut group = String::new();
        let mut group_len = 0;
        for sentence in split_sentences(&para) {
            let len = char_len(&sentence);
            if group_len > 0 && group_len + 2 + len > TARGET_CHUNK_CHARS {
                out.push(std::mem::take(&mut group));
                group_len = 0;
            }
            if len > TARGET_CHUNK_CHARS {
                if group_len > 0 {
                    out.push(std::mem::take(&mut group));
                    group_len = 0;
                }
                out.extend(hard_pieces(&sentence, TARGET_CHUNK_CHARS));
                continue;
            }
            if group_len > 0 {
                group.push_str("\n\n");
                group_len += 2;
            }
            group.push_str(&sentence);
            group_len += len;
        }
        if group_len > 0 {
            out.push(group);
        }
    }
    out
}

/// Paragraphs separated by blank (whitespace-only) lines; interior line
/// breaks survive, so a unit re-joined with a blank line reads like the
/// original document.
fn paragraphs(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else {
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(line);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Sentence cut points: terminal punctuation (CJK and Latin), `.` when not
/// followed by an alphanumeric (so "3.5" and "e.g" hold together), and the
/// sentence ends even mid-paragraph. A terminator followed by another
/// terminator stays glued ("...", "……", "?!") so ellipses survive as one
/// piece instead of shattering into blank-line-separated fragments. Only
/// oversized paragraphs ever reach this, so an occasional early cut costs
/// nothing.
fn split_sentences(para: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut prev: Option<char> = None;
    for (i, c) in para.char_indices() {
        if let Some(p) = prev {
            let boundary = (matches!(p, '。' | '！' | '？' | '；' | '…' | '!' | '?' | ';')
                || (p == '.' && !c.is_alphanumeric()))
                && !matches!(c, '.' | '…' | '!' | '?' | ';' | '。' | '！' | '？' | '；');
            if boundary {
                let sentence = para[start..i].trim();
                if !sentence.is_empty() {
                    out.push(sentence.to_owned());
                }
                start = i;
            }
        }
        prev = Some(c);
    }
    let rest = para[start..].trim();
    if !rest.is_empty() {
        out.push(rest.to_owned());
    }
    out
}

fn hard_pieces(text: &str, target: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut len = 0;
    for c in text.chars() {
        if len == target {
            out.push(std::mem::take(&mut current));
            len = 0;
        }
        current.push(c);
        len += 1;
    }
    if len > 0 {
        out.push(current);
    }
    out
}

fn pack_units(units: Vec<String>) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut len = 0;
    for unit in units {
        let unit_len = char_len(&unit);
        if !current.is_empty() && len + 2 + unit_len > TARGET_CHUNK_CHARS {
            chunks.push(current.join("\n\n"));
            current.clear();
            len = 0;
        }
        if !current.is_empty() {
            len += 2;
        }
        current.push(unit);
        len += unit_len;
    }
    if !current.is_empty() {
        chunks.push(current.join("\n\n"));
    }
    // A tail chunk smaller than MIN_TAIL_CHARS is not worth a request; it
    // may push its predecessor to TARGET + MIN_TAIL_CHARS at worst — the
    // target is a token budget, not a hard server limit.
    if chunks.len() > 1 && char_len(chunks.last().unwrap()) < MIN_TAIL_CHARS {
        let tail = chunks.pop().unwrap();
        let prev = chunks.last_mut().unwrap();
        prev.push_str("\n\n");
        prev.push_str(&tail);
    }
    chunks
}

// ---------------------------------------------------------------------------
// Chunk join gate
// ---------------------------------------------------------------------------

/// Joins chunk replies with one paragraph break, so live and buffered
/// delivery end up byte-identical. A reply may open with — or end with —
/// the newlines its chunk boundary makes natural, so the tail of the
/// emitted stream is tracked and the head of every slice after the first
/// is buffered until real content shows up; whatever arrives, exactly one
/// blank line ends up between the chunks.
pub struct ChunkGate {
    emit: Box<dyn FnMut(&str)>,
    /// Start of the next slice's reply, buffered while it is only newlines.
    head: String,
    /// True after a slice ends, until the next slice's content is out.
    awaiting_head: bool,
    /// Newlines the emitted stream currently ends with (capped at 2).
    end_newlines: u8,
    started: bool,
    finished: bool,
}

impl ChunkGate {
    pub fn new(emit: impl FnMut(&str) + 'static) -> Self {
        Self {
            emit: Box::new(emit),
            head: String::new(),
            awaiting_head: false,
            end_newlines: 0,
            started: false,
            finished: false,
        }
    }

    fn out(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.started = true;
        let trailing = text.chars().rev().take_while(|&c| c == '\n').count();
        let all_newlines = trailing == text.chars().count();
        self.end_newlines = if all_newlines {
            (self.end_newlines as usize + trailing).min(2) as u8
        } else {
            trailing.min(2) as u8
        };
        (self.emit)(text);
    }

    pub fn push_delta(&mut self, delta: &str) {
        if self.finished {
            return;
        }
        if self.awaiting_head {
            self.head.push_str(delta);
            let trimmed = self.head.trim_start_matches(['\r', '\n']);
            if trimmed.is_empty() {
                return;
            }
            let body = trimmed.to_string();
            self.head.clear();
            self.awaiting_head = false;
            if self.started {
                match self.end_newlines {
                    0 => self.out("\n\n"),
                    1 => self.out("\n"),
                    _ => {}
                }
            }
            self.out(&body);
            return;
        }
        self.out(delta);
    }

    /// The current slice's reply is complete.
    pub fn slice_end(&mut self) {
        if self.finished {
            return;
        }
        if self.awaiting_head {
            // The slice produced only blank output; stay on the boundary.
            self.head.clear();
            return;
        }
        self.awaiting_head = true;
    }

    pub fn finish(&mut self) {
        self.finished = true;
    }
}

#[cfg(test)]
mod tests;

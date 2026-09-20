//! The per-part batch wrapper: one request sequence per batch unit.
//!
//! Multi-file runs (`aido ocr a.png b.png`) used to share a single
//! request sequence and a single merged artifact, so batch results could
//! not be told apart or fail apart. With a task declaring
//! `per_part = true`, this wrapper plans the task's own strategy once per
//! unit and tags every step with its unit, so the runner can keep one
//! artifact (and one failure) per unit.
//!
//! A unit is what one request answers for. A part without a unit key
//! ([`InputPart::unit`]) is a unit of its own — the ordinary
//! one-file-one-request case. The parts materialized from one
//! sub-document share a key (a PDF page's image and text layer, a
//! workbook sheet), so a mixed page plans as ONE request: the page's text
//! layer rides as context for its image instead of becoming a separate,
//! semantically wrong "extract the text from the image" request. Literal
//! `--text`, stdin and clipboard parts are shared context that rides with
//! every unit's material — a glossary must not become a request of its
//! own. Fewer than two units is not a batch, and the strategy plans
//! exactly as before.
//!
//! A unit's steps come from planning the task's strategy over the shared
//! context plus all the unit's parts, with the unit's first part primary:
//! it tags the steps and names the artifact, and the strategies' own
//! carry mechanism moves the rest — under `ocr-tiles` the primary image
//! slices exactly as a standalone tall image would, while its page's text
//! rides every slice's request (or the unit's first request alone, past
//! the carry budget — the strategies' built-in fallback).
//!
//! Sharing never drops material, so a shared text past the carry budget
//! ([`MAX_CARRY_CHARS`]) only earns one stderr note per plan: every
//! unit's requests still carry it, and the note warns about the cost.
//!
//! Input-extension contract (#35): when glob / directory / URL inputs
//! land, they count as batch units through the same rule — material that
//! carries a file name. A URL that downloads to a temp file or a
//! dedicated `InputSource::Url` variant only needs a sensible name; no
//! change here.

use super::{dispatch, text_chars, RequestStep, MAX_CARRY_CHARS};
use crate::domain::{InputPart, InputSource, MediaKind};
use crate::tasks::ProcessorKind;
use anyhow::Result;

/// Plan the request sequence for a per-part batch: the inner strategy
/// runs once per unit over the shared material plus that unit's parts,
/// and every step is tagged with the unit's first part's id. `quiet`
/// suppresses the inner strategies' notes (stderr).
pub fn plan_steps(
    inputs: &[InputPart],
    inner: ProcessorKind,
    quiet: bool,
) -> Result<Vec<RequestStep>> {
    let owned: Vec<&InputPart> = inputs
        .iter()
        .filter(|p| matches!(p.source, InputSource::File(_)))
        .collect();
    let units = group_units(&owned);
    if units.len() < 2 {
        return dispatch(inputs, inner, quiet);
    }
    let shared: Vec<InputPart> = inputs
        .iter()
        .filter(|p| !matches!(p.source, InputSource::File(_)))
        .cloned()
        .collect();
    let stems = unique_stems(&units);
    warn_shared_budget(&shared, quiet);

    let mut steps = Vec::new();
    for (unit, stem) in units.iter().zip(&stems) {
        let primary = unit[0];
        let mut material = shared.clone();
        material.extend(unit.iter().map(|p| (*p).clone()));
        let inner_steps = dispatch(&material, inner, quiet)?;
        let single = inner_steps.len() == 1;
        for (j, mut step) in inner_steps.into_iter().enumerate() {
            step.index = steps.len();
            step.part = Some(primary.id);
            // An unlabeled whole-material step names the unit's primary
            // part instead, so progress reads "asking … — story-p1-1".
            if single && step.label == "all material" {
                step.label = primary.name.clone();
            }
            if j == 0 {
                step.artifact_stem = Some(stem.clone());
            }
            steps.push(step);
        }
    }
    Ok(steps)
}

/// Group the batch units in input order: a part without a unit key is a
/// unit of its own (one ordinary file), while parts sharing a
/// sub-document key form one unit (a page's image and its text layer),
/// parts keeping their input order inside it. A unit's first part is its
/// primary: it tags the unit's steps and names its artifact.
fn group_units<'a>(parts: &[&'a InputPart]) -> Vec<Vec<&'a InputPart>> {
    let mut index: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut units: Vec<Vec<&'a InputPart>> = Vec::new();
    for part in parts {
        match part.unit.as_deref() {
            Some(key) => {
                let next = units.len();
                let slot = index.entry(key).or_insert(next);
                if *slot == next {
                    units.push(Vec::new());
                }
                units[*slot].push(part);
            }
            None => units.push(vec![part]),
        }
    }
    units
}

/// Unlike the chunk strategies' carry guard, shared material is never
/// dropped: a glossary must not become a request of its own, so every
/// part's request keeps carrying it. Past the carry budget
/// ([`MAX_CARRY_CHARS`]) that repetition may crowd the model's context
/// window, which is worth one note per plan (`quiet` suppresses it).
fn warn_shared_budget(shared: &[InputPart], quiet: bool) {
    let chars = text_chars(shared);
    let images = shared.iter().filter(|p| p.kind == MediaKind::Image).count();
    if quiet {
        return;
    }
    if chars > MAX_CARRY_CHARS {
        eprintln!(
            "note: shared context material ({chars} chars) rides with every \
             part's request and may exceed the model's context window"
        );
    }
    // Images were never part of the char count, but a shared image costs
    // the same way — one per request. A piped document is the loud case:
    // its pages materialize to shared images (stdin is context, never a
    // unit), while the same document as a file processes page by page.
    if images > 0 {
        if images == 1 {
            eprintln!(
                "note: 1 shared image rides with every part's request; \
                 give a document as a file to process it page by page instead"
            );
        } else {
            eprintln!(
                "note: {images} shared images ride with every part's request; \
                 give a document as a file to process it page by page instead"
            );
        }
    }
}

/// Output stems (from each unit's primary part's name, `a.png` → `a`),
/// unique across the batch in input order: the first unit keeps the bare
/// stem, later collisions get `-2`, `-3`, … Dedup runs on the sanitized,
/// case-folded form — that is the name that actually lands in the output
/// directory (`a b.png` and `a-b.png` would otherwise sanitize to the
/// same file), and case-insensitive filesystems would treat `A.txt` and
/// `a.txt` as one.
fn unique_stems(units: &[Vec<&InputPart>]) -> Vec<String> {
    let mut used = std::collections::BTreeSet::new();
    let mut stems = Vec::with_capacity(units.len());
    for unit in units {
        let base = crate::output::sanitize_stem(&stem_of(unit[0]));
        let mut candidate = base.clone();
        let mut n = 1;
        while !used.insert(candidate.to_lowercase()) {
            n += 1;
            candidate = format!("{base}-{n}");
        }
        stems.push(candidate);
    }
    stems
}

/// A part's name is a single file name (never a path), so the stem is
/// everything before the last dot; a dotfile or extension-less name is
/// its own stem.
fn stem_of(part: &InputPart) -> String {
    match part.name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => part.name.clone(),
    }
}

#[cfg(test)]
mod tests;

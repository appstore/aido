//! The per-part batch wrapper: one request sequence per input file.
//!
//! Multi-file runs (`aido ocr a.png b.png`) used to share a single
//! request sequence and a single merged artifact, so batch results could
//! not be told apart or fail apart. With a task declaring
//! `per_part = true`, this wrapper plans the task's own strategy once per
//! file part and tags every step with its part, so the runner can keep
//! one artifact (and one failure) per file.
//!
//! The unit of batching is any material with a file name of its own
//! (`InputSource::File`); literal `--text`, stdin and clipboard parts are
//! shared context that rides with every part's first request — a
//! glossary must not become a request of its own. Fewer than two file
//! parts is not a batch, and the strategy plans exactly as before.
//!
//! Input-extension contract (#35): when glob / directory / URL inputs
//! land, they count as batch units through the same rule — material that
//! carries a file name. A URL that downloads to a temp file or a
//! dedicated `InputSource::Url` variant only needs a sensible name; no
//! change here.

use super::{dispatch, RequestStep};
use crate::domain::{InputPart, InputSource};
use crate::tasks::ProcessorKind;
use anyhow::Result;

/// Plan the request sequence for a per-part batch: the inner strategy
/// runs once per file part over the shared material plus that part, and
/// every step is tagged with the part's id. `quiet` suppresses the inner
/// strategies' notes (stderr).
pub fn plan_steps(
    inputs: &[InputPart],
    inner: ProcessorKind,
    quiet: bool,
) -> Result<Vec<RequestStep>> {
    let units: Vec<&InputPart> = inputs
        .iter()
        .filter(|p| matches!(p.source, InputSource::File(_)))
        .collect();
    if units.len() < 2 {
        return dispatch(inputs, inner, quiet);
    }
    let shared: Vec<InputPart> = inputs
        .iter()
        .filter(|p| !matches!(p.source, InputSource::File(_)))
        .cloned()
        .collect();
    let stems = unique_stems(&units);

    let mut steps = Vec::new();
    for (part, stem) in units.iter().zip(&stems) {
        let mut material = shared.clone();
        material.push((*part).clone());
        let inner_steps = dispatch(&material, inner, quiet)?;
        let single = inner_steps.len() == 1;
        for (j, mut step) in inner_steps.into_iter().enumerate() {
            step.index = steps.len();
            step.part = Some(part.id);
            // An unlabeled whole-material step names the part instead, so
            // progress reads "asking … — a.png".
            if single && step.label == "all material" {
                step.label = part.name.clone();
            }
            if j == 0 {
                step.artifact_stem = Some(stem.clone());
            }
            steps.push(step);
        }
    }
    Ok(steps)
}

/// Output stems (`a.png` → `a`), unique across the batch in input order:
/// the first part keeps the bare stem, later collisions get `-2`, `-3`, …
/// Dedup runs on the sanitized, case-folded form — that is the name that
/// actually lands in the output directory (`a b.png` and `a-b.png` would
/// otherwise sanitize to the same file), and case-insensitive filesystems
/// would treat `A.txt` and `a.txt` as one.
fn unique_stems(units: &[&InputPart]) -> Vec<String> {
    let mut used = std::collections::BTreeSet::new();
    let mut stems = Vec::with_capacity(units.len());
    for part in units {
        let base = crate::output::sanitize_stem(&stem_of(part));
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
mod tests {
    use super::*;
    use crate::domain::{InputContent, MediaKind};

    fn file_part(id: usize, path: &str) -> InputPart {
        let name = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path);
        InputPart {
            id,
            source: InputSource::File(std::path::PathBuf::from(path)),
            name: name.into(),
            kind: MediaKind::Text,
            unknown_kind: false,
            mime: "text/plain".into(),
            content: InputContent::Text(format!("material of {name}")),
        }
    }

    fn literal_part(id: usize, name: &str) -> InputPart {
        InputPart {
            id,
            source: InputSource::Literal,
            name: name.into(),
            kind: MediaKind::Text,
            unknown_kind: false,
            mime: "text/plain".into(),
            content: InputContent::Text(format!("shared {name}")),
        }
    }

    #[test]
    fn fewer_than_two_files_is_not_a_batch() {
        let inputs = [literal_part(0, "--text #1"), file_part(1, "a.md")];
        let steps = plan_steps(&inputs, ProcessorKind::Single, true).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].part, None);
        assert_eq!(steps[0].artifact_stem, None);
        assert_eq!(steps[0].label, "all material");
    }

    #[test]
    fn one_request_sequence_per_file_with_shared_material_first() {
        let inputs = [
            literal_part(0, "glossary"),
            file_part(1, "a.md"),
            file_part(2, "b.md"),
        ];
        let steps = plan_steps(&inputs, ProcessorKind::Single, true).unwrap();
        assert_eq!(steps.len(), 2);
        for (step, id) in steps.iter().zip([1, 2]) {
            assert_eq!(step.part, Some(id));
            // Shared material rides with the part, not beside it.
            assert_eq!(step.inputs.len(), 2);
            assert_eq!(step.inputs[0].name, "glossary");
        }
        assert_eq!(steps[0].label, "a.md");
        assert_eq!(steps[1].label, "b.md");
    }

    #[test]
    fn stems_are_input_order_unique() {
        let inputs = [
            file_part(0, "dir-a/a.png"),
            file_part(1, "dir-b/a.png"),
            file_part(2, "a-2.png"),
            file_part(3, "a.png"),
        ];
        let steps = plan_steps(&inputs, ProcessorKind::Single, true).unwrap();
        let stems: Vec<String> = steps
            .iter()
            .filter_map(|s| s.artifact_stem.clone())
            .collect();
        assert_eq!(stems, ["a", "a-2", "a-2-2", "a-3"]);
        // Every step of a single-request part is its part's first step,
        // so all of them carry the stem; a later step of a multi-request
        // part carries none (covered by the batch integration tests).
        for step in &steps {
            assert!(step.artifact_stem.is_some(), "{:?}", step);
        }
    }

    #[test]
    fn dotfiles_and_extensionless_names_are_their_own_stem() {
        let inputs = [file_part(0, ".aido"), file_part(1, "Makefile")];
        let steps = plan_steps(&inputs, ProcessorKind::Single, true).unwrap();
        // Stems carry their sanitized form — the name that will land in
        // the output directory.
        assert_eq!(steps[0].artifact_stem, Some("aido".into()));
        assert_eq!(steps[1].artifact_stem, Some("Makefile".into()));
    }

    #[test]
    fn stems_collide_on_their_sanitized_form() {
        // Both sanitize to `a-b`; the second must not overwrite the first.
        let inputs = [file_part(0, "a b.png"), file_part(1, "a-b.png")];
        let steps = plan_steps(&inputs, ProcessorKind::Single, true).unwrap();
        assert_eq!(steps[0].artifact_stem, Some("a-b".into()));
        assert_eq!(steps[1].artifact_stem, Some("a-b-2".into()));
    }

    #[test]
    fn a_reduce_step_belongs_to_its_part() {
        // A long text chunks into three, so each part plans three map
        // steps plus its own reduce step — the consolidation happens per
        // part, not once over every part's replies.
        let long = |id: usize, path: &str| InputPart {
            id,
            source: InputSource::File(std::path::PathBuf::from(path)),
            name: path.into(),
            kind: MediaKind::Text,
            unknown_kind: false,
            mime: "text/plain".into(),
            content: InputContent::Text("字".repeat(9000)),
        };
        let inputs = [long(1, "a.md"), long(2, "b.md")];
        let steps = plan_steps(&inputs, ProcessorKind::ChunkReduce, true).unwrap();
        assert_eq!(steps.len(), 8, "3 map + 1 reduce per part");
        for (reduce, id) in [(&steps[3], 1), (&steps[7], 2)] {
            assert_eq!(reduce.role, crate::processors::StepRole::Reduce);
            assert_eq!(reduce.part, Some(id));
        }
        assert!(steps[..3]
            .iter()
            .chain(&steps[4..7])
            .all(|s| s.role == crate::processors::StepRole::Map));
    }
}

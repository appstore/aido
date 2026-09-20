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
        unit: None,
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
fn cloned_source_and_split_parts_still_match_by_id() {
    // Matching is by part id, not pointer: a caller that clones the
    // source or the split parts before calling (pointer equality would
    // then never fire) still sees the piece slot in at the source's
    // position and the split parts stay out under carry.
    let inputs = [part(0, "glossary"), part(1, "book"), part(2, "notes")];
    let source = inputs[1].clone();
    let split_owned = [inputs[1].clone(), inputs[2].clone()];
    let split: Vec<&InputPart> = split_owned.iter().collect();
    let piece = part(1, "book [chunk 1/2]");
    let material = step_material(&inputs, &source, piece, &split, true);
    assert_eq!(
        names(&material),
        vec!["glossary", "book [chunk 1/2]"],
        "the piece replaces the cloned source's position; cloned split \
             parts are still excluded"
    );
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

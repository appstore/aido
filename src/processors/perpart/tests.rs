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
        unit: None,
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
        unit: None,
    }
}

/// A small real PNG: the ocr strategy reads image dimensions at plan
/// time, so placeholder bytes would be refused.
fn solid_png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba([255, 255, 255, 255]));
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    png
}

/// One materialized page part: image or text layer of one PDF page,
/// sharing the page's unit key.
fn page_part(id: usize, name: &str, unit: &str, png: Option<(u32, u32)>, text: &str) -> InputPart {
    let (kind, mime, content) = match png {
        Some((w, h)) => (
            MediaKind::Image,
            "image/png",
            InputContent::Media(solid_png(w, h)),
        ),
        None => (
            MediaKind::Text,
            "text/plain",
            InputContent::Text(text.into()),
        ),
    };
    InputPart {
        id,
        source: InputSource::File("story.pdf".into()),
        name: name.into(),
        kind,
        unknown_kind: false,
        mime: mime.into(),
        content,
        unit: Some(unit.into()),
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
fn a_shared_stdin_image_rides_every_part() {
    // A piped image is shared context (stdin is never a unit), so a
    // two-file batch carries it in every request — the multiplying
    // case the stderr note in warn_shared_budget names.
    let stdin_image = InputPart {
        id: 0,
        source: InputSource::Stdin,
        name: "stdin-p1".into(),
        kind: MediaKind::Image,
        unknown_kind: false,
        mime: "image/png".into(),
        content: InputContent::Media(solid_png(2, 2)),
        unit: None,
    };
    let inputs = [stdin_image, file_part(1, "a.png"), file_part(2, "b.png")];
    let steps = plan_steps(&inputs, ProcessorKind::Single, true).unwrap();
    assert_eq!(steps.len(), 2);
    assert!(steps
        .iter()
        .all(|s| s.inputs.iter().any(|p| p.name == "stdin-p1")));
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
fn a_page_unit_plans_its_image_and_text_as_one_request() {
    // [image(unit A), text(unit A), image(unit B)] is two units, not
    // three parts: the page's text layer rides with its image instead
    // of becoming a paid "extract the text from the image" request of
    // its own. Stems come from each unit's primary (image) part.
    let inputs = [
        page_part(0, "story-p1-1", "story.pdf#p1", Some((2, 2)), ""),
        page_part(1, "story-p1-2", "story.pdf#p1", None, "hello"),
        page_part(2, "story-p2", "story.pdf#p2", Some((2, 2)), ""),
    ];
    let steps = plan_steps(&inputs, ProcessorKind::OcrTiles, true).unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].part, Some(0));
    assert_eq!(steps[1].part, Some(2));
    assert_eq!(steps[0].artifact_stem.as_deref(), Some("story-p1-1"));
    assert_eq!(steps[1].artifact_stem.as_deref(), Some("story-p2"));
    let first: Vec<&str> = steps[0].inputs.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(first, ["story-p1-1", "story-p1-2"], "the text rides along");
    let second: Vec<&str> = steps[1].inputs.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(second, ["story-p2"], "no stray material across units");
    // Unlabeled whole-material steps read as their unit's primary part.
    assert_eq!(steps[0].label, "story-p1-1");
    assert_eq!(steps[1].label, "story-p2");
}

#[test]
fn a_unit_text_rides_every_slice_of_its_page_image() {
    // When the primary image slices, the unit's text keeps riding
    // every slice request — the strategies' own carry mechanism, with
    // its built-in fallback to the unit's first request past the
    // carry budget. Steps stay tagged with the unit's primary part.
    let inputs = [
        page_part(0, "story-p1-1", "story.pdf#p1", Some((64, 3200)), ""),
        page_part(1, "story-p1-2", "story.pdf#p1", None, "see headers only"),
        page_part(2, "story-p2", "story.pdf#p2", Some((2, 2)), ""),
    ];
    let steps = plan_steps(&inputs, ProcessorKind::OcrTiles, true).unwrap();
    assert_eq!(
        steps.len(),
        3,
        "two slices for page 1, one request for page 2"
    );
    assert!(steps[0].label.contains("slice 1/2"), "{}", steps[0].label);
    assert!(steps[1].label.contains("slice 2/2"), "{}", steps[1].label);
    for step in &steps[..2] {
        assert_eq!(step.part, Some(0));
        assert!(
            step.inputs.iter().any(|p| p.name == "story-p1-2"),
            "the page's text rides its image's slices"
        );
    }
    assert_eq!(steps[0].artifact_stem.as_deref(), Some("story-p1-1"));
    assert_eq!(
        steps[1].artifact_stem, None,
        "only the unit's first step carries the stem"
    );
    assert_eq!(steps[2].part, Some(2));
    assert_eq!(steps[2].artifact_stem.as_deref(), Some("story-p2"));
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
        unit: None,
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

#[test]
fn a_single_chunk_part_plans_no_reduce_step_next_to_a_chunked_part() {
    // per_part plus chunk-reduce is a supported combination: the long
    // file's part ends in its own reduce step, while the single-chunk
    // file's part is one map step whose reply is the part's whole
    // result — a chunk that is already the whole document takes no
    // reduce step, so the runner must not collect its reply as
    // intermediate material either.
    let part = |id: usize, path: &str, text: String| InputPart {
        id,
        source: InputSource::File(std::path::PathBuf::from(path)),
        name: path.into(),
        kind: MediaKind::Text,
        unknown_kind: false,
        mime: "text/plain".into(),
        content: InputContent::Text(text),
        unit: None,
    };
    let inputs = [
        part(1, "long.md", "字".repeat(9000)),
        part(2, "short.md", "one short line".into()),
    ];
    let steps = plan_steps(&inputs, ProcessorKind::ChunkReduce, true).unwrap();
    // 3 map + 1 reduce for the long file, then 1 map for the short one.
    assert_eq!(steps.len(), 5);
    assert!(steps[..4].iter().all(|s| s.part == Some(1)));
    assert!(steps[..3]
        .iter()
        .all(|s| s.role == crate::processors::StepRole::Map));
    assert_eq!(steps[3].role, crate::processors::StepRole::Reduce);
    assert_eq!(steps[3].part, Some(1));
    assert_eq!(steps[4].part, Some(2));
    assert_eq!(steps[4].role, crate::processors::StepRole::Map);
    assert!(!steps
        .iter()
        .any(|s| s.part == Some(2) && s.role == crate::processors::StepRole::Reduce));
}

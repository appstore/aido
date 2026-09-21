use super::*;
use crate::domain::{InputContent, InputSource};

fn solid_png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba([255, 255, 255, 255]));
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    png
}

/// Alternating black/white pixels: every row looks like a line of text.
fn striped_png(w: u32, h: u32) -> Vec<u8> {
    let mut img = image::RgbaImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let c = if (x + y) % 2 == 0 { 0 } else { 255 };
            img.put_pixel(x, y, image::Rgba([c, c, c, 255]));
        }
    }
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    png
}

fn image_part(id: usize, png: Vec<u8>) -> InputPart {
    InputPart {
        id,
        source: InputSource::File("long.png".into()),
        name: "long.png".into(),
        kind: MediaKind::Image,
        unknown_kind: false,
        mime: "image/png".into(),
        content: InputContent::Media(png),
        unit: None,
    }
}

fn text_part(text: &str) -> InputPart {
    InputPart {
        id: 9,
        source: InputSource::Literal,
        name: "notes".into(),
        kind: MediaKind::Text,
        unknown_kind: false,
        mime: "text/plain".into(),
        content: InputContent::Text(text.into()),
        unit: None,
    }
}

fn dims(png: &[u8]) -> (u32, u32) {
    let img = image::load_from_memory(png).unwrap();
    use image::GenericImageView as _;
    img.dimensions()
}

/// A real 2×2 JPEG whose SOF0 segment is patched to declare `w`×`h`:
/// the decompression-bomb shape — a few hundred bytes claiming a huge
/// canvas. The header read sees the patched size; a full decode would
/// have to honor it.
fn jpeg_declaring(w: u32, h: u32) -> Vec<u8> {
    let img = image::GrayImage::from_pixel(2, 2, image::Luma([128]));
    let mut jpg = Vec::new();
    image::DynamicImage::ImageLuma8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut jpg),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    // Baseline JPEGs carry one SOF0 (FF C0); byte stuffing means FF in
    // entropy data is never followed by C0, so the first hit is it.
    // Layout after the marker: length(2), precision(1), height(2 BE),
    // width(2 BE).
    let sof = jpg
        .windows(2)
        .position(|p| p == [0xFF, 0xC0])
        .expect("encoder wrote a SOF0 marker");
    jpg[sof + 5..sof + 7].copy_from_slice(&(h as u16).to_be_bytes());
    jpg[sof + 7..sof + 9].copy_from_slice(&(w as u16).to_be_bytes());
    jpg
}

fn jpeg_part(jpg: Vec<u8>) -> InputPart {
    InputPart {
        id: 0,
        source: InputSource::File("long.jpg".into()),
        name: "long.jpg".into(),
        kind: MediaKind::Image,
        unknown_kind: false,
        mime: "image/jpeg".into(),
        content: InputContent::Media(jpg),
        unit: None,
    }
}

/// A real tall JPEG (unlike [`jpeg_declaring`], the pixels match the
/// header), so the slicing path actually decodes it.
fn tall_jpeg(w: u32, h: u32) -> Vec<u8> {
    let img = image::GrayImage::from_pixel(w, h, image::Luma([128]));
    let mut jpg = Vec::new();
    image::DynamicImage::ImageLuma8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut jpg),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    jpg
}

#[test]
fn decompression_bomb_jpegs_are_refused_before_any_decode() {
    // Square (would not even be split) and tall (would be sliced):
    // both declare over 200 MP, and the header-only guard must refuse
    // them before a multi-gigabyte decode — tall or not.
    for (w, h) in [(20_000, 20_000), (4_000, 60_000)] {
        let err = plan_steps(&[jpeg_part(jpeg_declaring(w, h))], true).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("refusing to decode"), "{msg}");
        assert!(msg.contains(&format!("{w}\u{d7}{h}")), "{msg}");
    }
}

#[test]
fn small_jpeg_needing_no_split_is_untouched_at_plan_time() {
    // Below the split threshold the JPEG must not be decoded (and
    // re-encoded) here at all — the step carries the original bytes,
    // and only the adapter boundary decodes them.
    let jpg = jpeg_declaring(2, 2);
    let steps = plan_steps(&[jpeg_part(jpg.clone())], true).unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].label, "all material");
    assert_eq!(steps[0].inputs[0].content, InputContent::Media(jpg));
}

#[test]
fn tall_jpeg_slices_reach_the_adapter_boundary_as_png() {
    // The tiler decodes the original JPEG directly, but what it emits
    // must still be PNG — the only encoding the chat/responses routes
    // send.
    let steps = plan_steps(&[jpeg_part(tall_jpeg(100, 3200))], true).unwrap();
    assert_eq!(steps.len(), 2);
    for step in &steps {
        let slice = step
            .inputs
            .iter()
            .find(|p| p.name.contains("[slice "))
            .expect("every step carries a slice");
        assert_eq!(slice.mime, "image/png");
        match &slice.content {
            InputContent::Media(b) => assert!(b.starts_with(b"\x89PNG")),
            _ => panic!("slice is not media"),
        }
    }
}

#[test]
fn split_gates() {
    assert!(needs_splitting(100, 3200)); // tall strip
    assert!(!needs_splitting(100, 3072)); // at the threshold
    assert!(!needs_splitting(100, 2400)); // phone screenshot
    assert!(!needs_splitting(3200, 3100)); // wide, not tall
    assert_eq!(slice_height_for(1080), 2000);
    assert_eq!(slice_height_for(3000), 1333); // pixel budget binds
    assert_eq!(slice_height_for(6000), 1024); // floor binds
}

#[test]
fn short_images_pass_through_untouched() {
    let steps = plan_steps(&[image_part(0, solid_png(100, 500))], true).unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].inputs.len(), 1);
    assert_eq!(steps[0].label, "all material");
}

#[test]
fn tall_image_produces_sequential_slices_that_tile_it() {
    let steps = plan_steps(&[image_part(0, solid_png(100, 3200))], true).unwrap();
    assert_eq!(steps.len(), 2);
    assert!(steps[0].label.contains("slice 1/2"));
    let (w1, h1) = dims(match &steps[0].inputs[0].content {
        InputContent::Media(b) => b,
        _ => panic!(),
    });
    let (w2, h2) = dims(match &steps[1].inputs[1].content {
        InputContent::Media(b) => b,
        _ => panic!(),
    });
    assert_eq!((w1, w2), (100, 100));
    assert_eq!(h1, 2000);
    assert_eq!(h2, 1200);
}

#[test]
fn unsliced_material_travels_with_every_slice_in_order() {
    let inputs = vec![
        image_part(0, solid_png(100, 500)),
        image_part(1, striped_png(100, 3200)),
    ];
    let steps = plan_steps(&inputs, true).unwrap();
    assert_eq!(steps.len(), 2);
    for (i, step) in steps.iter().enumerate() {
        let names: Vec<&str> = step.inputs.iter().map(|p| p.name.as_str()).collect();
        let short = names.iter().position(|n| *n == "long.png").unwrap();
        let slice = names.iter().position(|n| n.contains("[slice ")).unwrap();
        assert!(short < slice, "step {i}: {names:?}");
    }
    // Later slices explain themselves with the note, ahead of the
    // carried material.
    assert!(steps[1].inputs[0]
        .text()
        .is_some_and(|t| t.contains("slice")));
}

#[test]
fn text_material_rides_with_every_slice_until_over_budget() {
    let small = vec![
        image_part(0, striped_png(100, 3200)),
        text_part("see headers only"),
    ];
    let steps = plan_steps(&small, true).unwrap();
    assert_eq!(steps.len(), 2);
    for step in &steps {
        assert!(
            step.inputs.iter().any(|p| p.name == "notes"),
            "every slice request must see the text material"
        );
    }
    // Over the carry budget: the text rides with the first request only.
    let big = "词".repeat(super::super::MAX_CARRY_CHARS + 1);
    let over = vec![text_part(&big), image_part(0, striped_png(100, 3200))];
    let steps = plan_steps(&over, true).unwrap();
    assert!(steps[0].inputs.iter().any(|p| p.name == "notes"));
    assert!(!steps[1].inputs.iter().any(|p| p.name == "notes"));
}

#[test]
fn hard_boundary_dedups_repeated_overlap_lines() {
    let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = log.clone();
    let mut gate = BoundaryGate::new(move |t: &str| sink.borrow_mut().push(t.to_string()));
    gate.push_delta("alpha\nbeta\ngamma\n");
    gate.slice_end(true);
    gate.push_delta("beta\ngamma\ndelta\n");
    gate.finish();
    let joined: String = log.borrow().concat();
    assert_eq!(joined, "alpha\nbeta\ngamma\ndelta\n");
}

#[test]
fn quiet_boundary_joins_with_one_newline() {
    let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = log.clone();
    let mut gate = BoundaryGate::new(move |t: &str| sink.borrow_mut().push(t.to_string()));
    gate.push_delta("first half");
    gate.slice_end(false);
    gate.push_delta("second half");
    gate.finish();
    let joined: String = log.borrow().concat();
    assert_eq!(joined, "first half\nsecond half");
}

#[test]
fn no_match_at_hard_boundary_keeps_everything() {
    let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = log.clone();
    let mut gate = BoundaryGate::new(move |t: &str| sink.borrow_mut().push(t.to_string()));
    gate.push_delta("one\ntwo\nthree\n");
    gate.slice_end(true);
    gate.push_delta("four\nfive\n");
    gate.finish();
    let joined: String = log.borrow().concat();
    assert_eq!(joined, "one\ntwo\nthree\nfour\nfive\n");
}

#[test]
fn partial_match_only_removes_matched_lines() {
    let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = log.clone();
    let mut gate = BoundaryGate::new(move |t: &str| sink.borrow_mut().push(t.to_string()));
    gate.push_delta("a\nb\nc\n");
    gate.slice_end(true);
    gate.push_delta("c\nd\n");
    gate.finish();
    let joined: String = log.borrow().concat();
    assert_eq!(joined, "a\nb\nc\nd\n");
}

#[test]
fn long_repetition_far_from_boundary_survives() {
    // The window is small: lines repeated far apart are content, not
    // overlap artifacts, and must not be removed.
    let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = log.clone();
    let mut gate = BoundaryGate::new(move |t: &str| sink.borrow_mut().push(t.to_string()));
    let mut first = String::new();
    for i in 0..20 {
        first.push_str(&format!("line {i}\n"));
    }
    gate.push_delta(&first);
    gate.slice_end(true);
    gate.push_delta("line 19\nline 0\nline 1\n");
    gate.finish();
    let joined: String = log.borrow().concat();
    assert!(joined.contains("line 19\nline 0\nline 1\n"));
    assert!(joined.contains("line 18\n"));
}

#[test]
fn empty_slices_and_finish_idempotence() {
    let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = log.clone();
    let mut gate = BoundaryGate::new(move |t: &str| sink.borrow_mut().push(t.to_string()));
    gate.slice_end(false);
    gate.push_delta("");
    gate.finish();
    gate.finish();
    assert!(log.borrow().is_empty());
}

#[test]
fn mixed_seams_flag_only_the_hard_boundaries() {
    // Top half solid (quiet seam lands on the target), bottom half
    // striped (no quiet row: the seam is hard). Only the middle
    // boundary may tell the merge gate to look for duplicated lines —
    // gating the quiet one would delete real text on a coincidence.
    let mut img = image::RgbaImage::new(100, 4400);
    for y in 0..4400u32 {
        for x in 0..100u32 {
            let c = if y >= 2200 && (x + y) % 2 == 0 {
                0
            } else {
                255
            };
            img.put_pixel(x, y, image::Rgba([c, c, c, 255]));
        }
    }
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    let steps = plan_steps(&[image_part(0, png)], true).unwrap();
    assert_eq!(steps.len(), 3, "two cuts expected");
    assert!(!steps[0].hard_cut_end, "quiet boundary must not gate");
    assert!(steps[1].hard_cut_end, "hard boundary must gate");
    assert!(!steps[2].hard_cut_end, "the tail slice has no next step");
}

#[test]
fn slice_labels_name_their_own_image() {
    let small = image_part(0, solid_png(100, 500));
    let tall = InputPart {
        name: "other.png".into(),
        ..image_part(1, striped_png(100, 3200))
    };
    let steps = plan_steps(&[small, tall], true).unwrap();
    let labels: Vec<&str> = steps.iter().map(|s| s.label.as_str()).collect();
    assert!(labels.iter().all(|l| l.contains("other.png")), "{labels:?}");
}

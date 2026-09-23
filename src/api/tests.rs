use super::*;
use crate::domain::{InputContent, InputSource};

fn text(name: &str, source: InputSource, s: &str) -> InputPart {
    InputPart {
        id: 0,
        source,
        name: name.into(),
        kind: MediaKind::Text,
        unknown_kind: false,
        mime: "text/plain".into(),
        content: InputContent::Text(s.into()),
        unit: None,
    }
}

fn image_part(name: &str, mime: &str, bytes: Vec<u8>) -> InputPart {
    InputPart {
        id: 0,
        source: InputSource::File(name.into()),
        name: name.into(),
        kind: MediaKind::Image,
        unknown_kind: false,
        mime: mime.into(),
        content: InputContent::Media(bytes),
        unit: None,
    }
}

fn tiny_jpeg() -> Vec<u8> {
    let img = image::GrayImage::from_pixel(2, 2, image::Luma([128]));
    let mut jpg = Vec::new();
    image::DynamicImage::ImageLuma8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut jpg),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    jpg
}

/// A real 2×2 JPEG whose SOF0 segment is patched to declare `w`×`h`:
/// the decompression-bomb shape — a few hundred bytes claiming a huge
/// canvas.
fn jpeg_declaring(w: u32, h: u32) -> Vec<u8> {
    let mut jpg = tiny_jpeg();
    // Layout after the FF C0 marker: length(2), precision(1), height
    // (2 BE), width (2 BE).
    let sof = jpg
        .windows(2)
        .position(|p| p == [0xFF, 0xC0])
        .expect("encoder wrote a SOF0 marker");
    jpg[sof + 5..sof + 7].copy_from_slice(&(h as u16).to_be_bytes());
    jpg[sof + 7..sof + 9].copy_from_slice(&(w as u16).to_be_bytes());
    jpg
}

#[test]
fn image_dimensions_reads_headers_without_decoding() {
    let img = image::RgbaImage::from_pixel(3, 4, image::Rgba([0, 0, 0, 255]));
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    assert_eq!(image_dimensions(&png).unwrap(), (3, 4));
    assert_eq!(
        image_dimensions(&jpeg_declaring(640, 480)).unwrap(),
        (640, 480)
    );
    assert!(image_dimensions(b"not an image").is_err());
}

#[test]
fn image_as_png_refuses_images_over_the_decode_limit() {
    let part = image_part("huge.jpg", "image/jpeg", jpeg_declaring(20_000, 20_000));
    let err = image_as_png(&part).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("refusing to decode"), "{msg}");
    assert!(msg.contains("400 MP"), "{msg}");
    assert!(msg.contains("200 MP"), "{msg}");
}

#[test]
fn image_as_png_reencodes_small_jpegs_unchallenged() {
    // A legitimate tiny JPEG passes the guard and comes back as PNG.
    let part = image_part("shot.jpg", "image/jpeg", tiny_jpeg());
    let png = image_as_png(&part).unwrap();
    assert!(png.starts_with(b"\x89PNG"));
    use image::GenericImageView as _;
    assert_eq!(image::load_from_memory(&png).unwrap().dimensions(), (2, 2));
}

#[test]
fn generated_image_bombs_are_refused_before_the_validation_decode() {
    let err = media::image_bytes(jpeg_declaring(20_000, 20_000)).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("refusing to decode"), "{msg}");
    assert!(msg.contains("400 MP"), "{msg}");
}

#[test]
fn generated_small_images_still_pass_the_guard() {
    let art = media::image_bytes(tiny_jpeg()).unwrap();
    assert_eq!(art.kind, MediaKind::Image);
    assert_eq!(art.mime, "image/jpeg");
    assert_eq!(art.format, "jpeg");
    assert!(!art.bytes.is_empty());
}

#[test]
fn single_text_stays_raw() {
    let parts = vec![text("a.txt", InputSource::File("a.txt".into()), "alpha\n")];
    assert_eq!(labeled_texts(&parts), vec!["alpha\n"]);
}

#[test]
fn multiple_texts_are_labeled_in_order() {
    let parts = vec![
        text("a.txt", InputSource::File("a.txt".into()), "alpha\n"),
        text("--text #1", InputSource::Literal, "literal\n"),
        text("stdin", InputSource::Stdin, "piped\n"),
    ];
    let labeled = labeled_texts(&parts);
    assert!(labeled[0].contains("--- a.txt ---"));
    assert_eq!(labeled[1], "literal\n");
    assert!(labeled[2].contains("piped\n"));
}

#[test]
fn merged_text_joins_with_blank_line() {
    let parts = vec![
        text("a.txt", InputSource::File("a.txt".into()), "alpha"),
        text("b.txt", InputSource::File("b.txt".into()), "beta"),
    ];
    assert!(merged_text(&parts).unwrap().contains("\n\n"));
}

#[test]
fn instruction_channel_joins_both_parts() {
    let req = GenerateRequest {
        instruction: Some("be brief"),
        requirement: Some("in english"),
        inputs: &[],
        model: "m",
        max_tokens: None,
        temperature: None,
        outputs: &[],
        options: &Default::default(),
    };
    assert_eq!(req.instruction_channel(), "be brief\n\nin english");
}

#[test]
fn local_asr_options_validate_seconds_and_threads_separately() {
    let opts = |pairs: &[(&str, serde_json::Value)]| {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect::<std::collections::BTreeMap<String, serde_json::Value>>()
    };
    let adapter = Adapter::LocalAsr;
    // A realistic decode budget in seconds passes — the default (3600)
    // must be settable explicitly too.
    assert!(adapter
        .validate_options(&opts(&[("max_audio_secs", serde_json::json!(600))]))
        .is_ok());
    assert!(adapter
        .validate_options(&opts(&[("max_audio_secs", serde_json::json!(7200))]))
        .is_ok());
    // The day cap holds for the budget, while threads keep their own range.
    let err = adapter
        .validate_options(&opts(&[("max_audio_secs", serde_json::json!(86_401))]))
        .unwrap_err();
    assert!(format!("{err:#}").contains("1..=86400"), "{err:#}");
    let err = adapter
        .validate_options(&opts(&[("threads", serde_json::json!(97))]))
        .unwrap_err();
    assert!(format!("{err:#}").contains("1..=96"), "{err:#}");
}

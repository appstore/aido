use super::*;
use lopdf::{dictionary, Stream};

fn tiny_jpeg() -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([220, 120, 40, 255]));
    let mut jpeg = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut jpeg),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    jpeg
}

/// A minimal picture book: one page per JPEG, the first page also
/// showing a text line when `with_text`. `draw` controls whether the
/// page content actually references its image.
fn book(jpegs: &[Vec<u8>], with_text: bool, draw: bool) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let pages_id = doc.add_object(dictionary! {
        "Type" => "Pages",
        "Kids" => Object::Array(Vec::new()),
        "Count" => Object::Integer(0),
    });
    for (i, jpeg) in jpegs.iter().enumerate() {
        let image_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => Object::Integer(4),
                "Height" => Object::Integer(4),
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => Object::Integer(8),
                "Filter" => "DCTDecode",
            },
            jpeg.clone(),
        ));
        let mut content = String::new();
        if draw {
            content.push_str(&format!("q 100 0 0 100 0 0 cm /Im{i} Do Q\n"));
        }
        if with_text && i == 0 {
            content.push_str("BT /F0 12 Tf 10 20 Td (hello) Tj ET\n");
        }
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => Object::Array(vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(100),
                Object::Integer(100),
            ]),
            "Resources" => Object::Dictionary(dictionary! {
                "XObject" => Object::Dictionary(dictionary! {
                    format!("Im{i}") => Object::Reference(image_id),
                }),
                "Font" => Object::Dictionary(dictionary! {
                    "F0" => Object::Reference(font_id),
                }),
            }),
            "Contents" => Object::Reference(content_id),
        });
        if let Some(Object::Dictionary(pages)) = doc.objects.get_mut(&pages_id) {
            if let Ok(Object::Array(kids)) = pages.get_mut(b"Kids") {
                kids.push(Object::Reference(page_id));
            }
            pages.set("Count", (i + 1) as i64);
        }
    }
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(catalog_id));
    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    out
}

fn expand_with_notes(bytes: &[u8]) -> Result<(Vec<InputPart>, Vec<String>)> {
    let mut budget = Budget::new();
    let parts = expand(
        "story.pdf",
        bytes,
        &InputSource::File("story.pdf".into()),
        0,
        0,
        &mut budget,
    )?;
    Ok((parts, budget.notes))
}

/// Most tests only care about the parts; the notes have their own
/// tests.
fn expand_bytes(bytes: &[u8]) -> Result<Vec<InputPart>> {
    expand_with_notes(bytes).map(|(parts, _)| parts)
}

fn expand_budget(bytes: &[u8], budget: &mut Budget) -> Result<Vec<InputPart>> {
    expand(
        "story.pdf",
        bytes,
        &InputSource::File("story.pdf".into()),
        7,
        20,
        budget,
    )
}

#[test]
fn extraction_admits_images_at_byte_and_part_boundaries() {
    let jpeg = tiny_jpeg();
    let bytes = book(&[jpeg.clone(), jpeg.clone(), jpeg.clone()], false, true);
    let mut budget = Budget::with_byte_cap(jpeg.len() * 2 - 1);
    let err = expand_budget(&bytes, &mut budget).unwrap_err();
    assert!(err.to_string().contains("MB of material"), "{err}");
    assert_eq!((budget.bytes, budget.parts), (jpeg.len(), 1));

    let bytes = book(&[jpeg.clone(), jpeg.clone()], false, true);
    let mut budget = Budget::with_byte_cap(jpeg.len() * 2);
    let parts = expand_budget(&bytes, &mut budget).unwrap();
    assert_eq!((budget.bytes, budget.parts), (jpeg.len() * 2, 2));
    assert_eq!(parts.iter().map(|p| p.id).collect::<Vec<_>>(), [20, 21]);
    assert_eq!(parts[1].unit.as_deref(), Some("story.pdf#7#p2"));

    let mut budget = Budget::new();
    budget.parts = super::super::MAX_PARTS_PER_DOCUMENT - 1;
    let err = expand_budget(&bytes, &mut budget).unwrap_err();
    assert!(err.to_string().contains("4096 parts"), "{err}");
    assert_eq!(budget.bytes, jpeg.len());
    assert_eq!(budget.parts, super::super::MAX_PARTS_PER_DOCUMENT);
}

#[test]
fn image_walk_stops_before_decoding_the_next_image_on_refusal() {
    // Three ordinary tiny images, not an oversized or malformed PDF.
    // Scope's visited set proves that refusal stops decoding, rather
    // than merely rejecting a pre-collected page Vec at delivery time.
    let jpeg = tiny_jpeg();
    let doc = Document::load_mem(&book(
        &[jpeg.clone(), jpeg.clone(), jpeg.clone()],
        false,
        true,
    ))
    .unwrap();
    let images: Vec<_> = doc
        .objects
        .iter()
        .filter_map(|(&id, object)| {
            let stream = object.as_stream().ok()?;
            matches!(stream.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image")
                .then(|| (format!("Im{}", id.0).into_bytes(), id, stream))
        })
        .collect();
    let order: Vec<_> = images.iter().map(|(name, _, _)| name.clone()).collect();
    for part_limited in [false, true] {
        let mut budget = Budget::with_byte_cap(if part_limited {
            jpeg.len() * 3
        } else {
            jpeg.len()
        });
        if part_limited {
            budget.parts = super::super::MAX_PARTS_PER_DOCUMENT - 1;
        }
        let mut scope = Scope::default();
        let mut calls = 0;
        let err = collect_scope(&doc, &images, &order, 0, &mut scope, &mut |bytes, _| {
            calls += 1;
            budget.admit("story.pdf", bytes.len())
        })
        .unwrap_err();
        assert!(err.to_string().contains(if part_limited {
            "4096 parts"
        } else {
            "MB of material"
        }));
        assert_eq!(calls, 2);
        assert_eq!(scope.decoded.len(), 2);
        assert!(!scope.decoded.contains(&images[2].1));
        assert_eq!(budget.bytes, jpeg.len());
    }
}

#[test]
fn text_collection_stops_at_admission_before_merge() {
    let bytes = text_only_book(3);
    let doc = Document::load_mem(&bytes).unwrap();
    let first = doc
        .extract_text_with_limit(&[1], MAX_CONTENT_BYTES)
        .unwrap();
    let mut budget = Budget::with_byte_cap(first.len());
    let err = expand_budget(&bytes, &mut budget).unwrap_err();
    assert!(err.to_string().contains("MB of material"), "{err}");
    // Only first-page raw text has been admitted, no merge glue or
    // later text; the provisional part remains ONE.
    assert_eq!((budget.bytes, budget.parts), (first.len(), 1));
}

#[test]
fn merged_text_budgets_all_glue_and_only_one_final_part() {
    let bytes = text_only_book(3);
    let expected = expand_bytes(&bytes)
        .unwrap()
        .remove(0)
        .text()
        .unwrap()
        .to_owned();
    let doc = Document::load_mem(&bytes).unwrap();
    let raw: usize = (1..=3)
        .map(|page| {
            doc.extract_text_with_limit(&[page], MAX_CONTENT_BYTES)
                .unwrap()
                .len()
        })
        .sum();
    assert_eq!(expected.len() - raw, 3 * "----- page 1 -----\n\n".len() + 4);
    let mut budget = Budget::with_byte_cap(expected.len() - 1);
    let err = expand_budget(&bytes, &mut budget).unwrap_err();
    assert!(err.to_string().contains("MB of material"), "{err}");
    assert_eq!((budget.bytes, budget.parts), (raw, 1));

    let mut budget = Budget::with_byte_cap(expected.len() + 5);
    budget.admit("prior", 5).unwrap();
    budget.parts = super::super::MAX_PARTS_PER_DOCUMENT - 1;
    let parts = expand_budget(&bytes, &mut budget).unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].text(), Some(expected.as_str()));
    assert_eq!(parts[0].unit.as_deref(), Some("story.pdf#7"));
    assert_eq!(budget.bytes, expected.len() + 5);
    assert_eq!(budget.parts, super::super::MAX_PARTS_PER_DOCUMENT);
}

#[test]
fn late_image_converts_prior_text_parts_without_charging_markers() {
    let mut doc = Document::load_mem(&text_only_book(3)).unwrap();
    let image_doc = Document::load_mem(&book(&[tiny_jpeg()], false, true)).unwrap();
    let image = image_doc
        .objects
        .values()
        .find(|o| {
            o.as_stream().is_ok_and(
                |s| matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image"),
            )
        })
        .unwrap()
        .clone();
    let image_id = doc.add_object(image);
    let content = doc.add_object(Stream::new(dictionary! {}, b"/Im Do".to_vec()));
    let page_id = doc.get_pages()[&3];
    let page = doc.get_object_mut(page_id).unwrap().as_dict_mut().unwrap();
    page.set("Contents", content);
    page.set(
        "Resources",
        dictionary! { "XObject" => dictionary! { "Im" => image_id } },
    );
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    let raw: usize = (1..=2)
        .map(|page| {
            doc.extract_text_with_limit(&[page], MAX_CONTENT_BYTES)
                .unwrap()
                .len()
        })
        .sum();
    let size = raw + tiny_jpeg().len();
    let mut budget = Budget::with_byte_cap(size);
    let parts = expand_budget(&bytes, &mut budget).unwrap();
    assert_eq!((budget.bytes, budget.parts), (size, 3));
    assert_eq!(
        parts.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        ["story-p1", "story-p2", "story-p3"]
    );
    assert!(parts[..2]
        .iter()
        .all(|p| !p.text().unwrap().contains("-----")));
    assert_eq!(parts[2].kind, MediaKind::Image);

    let mut budget = Budget::new();
    budget.parts = super::super::MAX_PARTS_PER_DOCUMENT - 2;
    let err = expand_budget(&bytes, &mut budget).unwrap_err();
    assert!(err.to_string().contains("4096 parts"), "{err}");
    // Both text pages fit as one provisional part; the late image's
    // conversion fails atomically, before any image payload is retained.
    assert_eq!(budget.bytes, raw);
    assert_eq!(budget.parts, super::super::MAX_PARTS_PER_DOCUMENT - 1);
}

#[test]
fn jpeg_pages_pass_through_in_order() {
    let jpegs = vec![tiny_jpeg(), tiny_jpeg()];
    let parts = expand_bytes(&book(&jpegs, false, true)).unwrap();
    assert_eq!(
        parts.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        ["story-p1", "story-p2"]
    );
    assert!(parts.iter().all(|p| p.kind == MediaKind::Image));
    assert!(parts.iter().all(|p| p.mime == "image/jpeg"));
    for (part, jpeg) in parts.iter().zip(&jpegs) {
        assert_eq!(part.text(), None);
        assert!(match &part.content {
            InputContent::Media(bytes) => bytes == jpeg,
            _ => false,
        });
    }
}

#[test]
fn text_rides_with_its_page_and_names_disambiguate() {
    let parts = expand_bytes(&book(&[tiny_jpeg()], true, true)).unwrap();
    assert_eq!(
        parts.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        ["story-p1-1", "story-p1-2"]
    );
    assert_eq!(parts[0].kind, MediaKind::Image);
    assert_eq!(parts[1].kind, MediaKind::Text);
    assert!(
        parts[1].text().is_some_and(|t| t.contains("hello")),
        "{:?}",
        parts[1].text()
    );
}

/// A text-layer-only book: `pages` pages whose content shows a text
/// line and no image at all.
fn text_only_book(pages: usize) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let pages_id = doc.add_object(dictionary! {
        "Type" => "Pages",
        "Kids" => Object::Array(Vec::new()),
        "Count" => Object::Integer(0),
    });
    for i in 0..pages {
        let content = format!("BT /F0 12 Tf 10 20 Td (page {i} words) Tj ET\n");
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => Object::Array(vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(100),
                Object::Integer(100),
            ]),
            "Resources" => Object::Dictionary(dictionary! {
                "Font" => Object::Dictionary(dictionary! {
                    "F0" => Object::Reference(font_id),
                }),
            }),
            "Contents" => Object::Reference(content_id),
        });
        if let Some(Object::Dictionary(pages)) = doc.objects.get_mut(&pages_id) {
            if let Ok(Object::Array(kids)) = pages.get_mut(b"Kids") {
                kids.push(Object::Reference(page_id));
            }
            pages.set("Count", (i + 1) as i64);
        }
    }
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(catalog_id));
    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    out
}

#[test]
fn a_text_only_document_is_one_part_with_page_markers() {
    // No image anywhere: the whole document is ONE text part (the
    // chunk strategies keep cross-page context), its pages separated
    // by markers, not one per-page request per page.
    let parts = expand_bytes(&text_only_book(3)).unwrap();
    assert_eq!(parts.len(), 1, "{parts:?}");
    assert_eq!(parts[0].name, "story");
    assert_eq!(parts[0].kind, MediaKind::Text);
    let text = parts[0].text().unwrap();
    assert!(text.contains("page 0 words"), "{text}");
    assert!(text.contains("page 2 words"), "{text}");
    assert!(text.contains("----- page 1 -----"), "{text}");
    assert_eq!(parts[0].unit.as_deref(), Some("story.pdf#0"));
}

#[test]
fn a_single_page_text_document_has_no_page_marker() {
    let parts = expand_bytes(&text_only_book(1)).unwrap();
    let text = parts[0].text().unwrap();
    assert!(text.contains("page 0 words"), "{text}");
    assert!(!text.contains("-----"), "{text}");
}

#[test]
fn a_text_document_with_a_skipped_image_still_merges() {
    // Page 1 shows text; page 2's only image is JPEG 2000 (skipped).
    // No image anywhere materialized, so the delivered material is
    // text and the merge applies — while the skip stays a note.
    let mut doc = Document::with_version("1.5");
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let jpx_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => Object::Integer(4),
            "Height" => Object::Integer(4),
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => Object::Integer(8),
            "Filter" => "JPXDecode",
        },
        b"not really jp2".to_vec(),
    ));
    let pages_id = doc.add_object(dictionary! {
        "Type" => "Pages",
        "Kids" => Object::Array(Vec::new()),
        "Count" => Object::Integer(0),
    });
    let content1_id = doc.add_object(Stream::new(
        dictionary! {},
        b"BT /F0 12 Tf 10 20 Td (hello) Tj ET\n".to_vec(),
    ));
    let page1_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => Object::Reference(pages_id),
        "MediaBox" => Object::Array(vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Integer(100),
            Object::Integer(100),
        ]),
        "Resources" => Object::Dictionary(dictionary! {
            "Font" => Object::Dictionary(dictionary! {
                "F0" => Object::Reference(font_id),
            }),
        }),
        "Contents" => Object::Reference(content1_id),
    });
    let content2_id = doc.add_object(Stream::new(dictionary! {}, b"q /Im9 Do Q\n".to_vec()));
    let page2_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => Object::Reference(pages_id),
        "MediaBox" => Object::Array(vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Integer(100),
            Object::Integer(100),
        ]),
        "Resources" => Object::Dictionary(dictionary! {
            "XObject" => Object::Dictionary(dictionary! {
                "Im9" => Object::Reference(jpx_id),
            }),
        }),
        "Contents" => Object::Reference(content2_id),
    });
    if let Some(Object::Dictionary(pages)) = doc.objects.get_mut(&pages_id) {
        pages.set(
            "Kids",
            Object::Array(vec![
                Object::Reference(page1_id),
                Object::Reference(page2_id),
            ]),
        );
        pages.set("Count", Object::Integer(2));
    }
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(catalog_id));
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();

    let (parts, notes) = expand_with_notes(&bytes).unwrap();
    assert_eq!(parts.len(), 1, "{parts:?}");
    assert_eq!(parts[0].kind, MediaKind::Text);
    assert!(parts[0].text().unwrap().contains("hello"));
    assert!(
        notes
            .iter()
            .any(|n| n.contains("page 2 contributed nothing")),
        "{notes:?}"
    );
}

// Without the feature an undrawn image leaves the document empty, so
// the vector-PDF refusal fires; with it the page rasterizes instead.
#[cfg(not(feature = "pdfium"))]
#[test]
fn undrawn_images_do_not_become_material() {
    // The XObject sits in resources but no /Do ever draws it.
    let parts = expand_bytes(&book(&[tiny_jpeg()], false, false));
    let err = parts.unwrap_err();
    assert!(err.to_string().contains("pdfium"), "{err}");
}

#[cfg(feature = "pdfium")]
#[test]
fn undrawn_images_do_not_become_material_even_under_pdfium() {
    // The undrawn XObject must not leak into the material: the page
    // rasterizes to a single rendered PNG instead.
    let parts = expand_bytes(&book(&[tiny_jpeg()], false, false)).unwrap();
    assert_eq!(parts.len(), 1, "{parts:?}");
    assert_eq!(parts[0].kind, MediaKind::Image);
    assert_eq!(parts[0].mime, "image/png");
}

/// A one-page document whose content draws a 2×1 Flate bitmap with
/// the given `/ColorSpace`; shared by the plain-RGB and ICCBased
/// decode tests. `colorspace` may add objects (an ICC profile stream)
/// and reference them.
fn flate_bitmap_document(colorspace: impl FnOnce(&mut Document) -> Object) -> Vec<u8> {
    use std::io::Write as _;
    // 2×1 bitmap, one dark pixel and one light.
    let raw = vec![10u8, 20, 30, 200, 210, 220];
    // PDF FlateDecode is the zlib wrapper, not raw deflate.
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&raw).unwrap();
    let flate = encoder.finish().unwrap();
    let mut doc = Document::with_version("1.5");
    let colorspace = colorspace(&mut doc);
    let pages_id = doc.add_object(dictionary! {
        "Type" => "Pages", "Kids" => Object::Array(Vec::new()), "Count" => Object::Integer(0),
    });
    let image_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => Object::Integer(2),
            "Height" => Object::Integer(1),
            "ColorSpace" => colorspace,
            "BitsPerComponent" => Object::Integer(8),
            "Filter" => "FlateDecode",
        },
        flate,
    ));
    let content_id = doc.add_object(Stream::new(dictionary! {}, b"q /Im0 Do Q".to_vec()));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => Object::Reference(pages_id),
        "MediaBox" => Object::Array(vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(100),
                Object::Integer(100),
            ]),
        "Resources" => Object::Dictionary(dictionary! {
            "XObject" => Object::Dictionary(dictionary! {
                "Im0" => Object::Reference(image_id),
            }),
        }),
        "Contents" => Object::Reference(content_id),
    });
    if let Some(Object::Dictionary(pages)) = doc.objects.get_mut(&pages_id) {
        pages.set("Kids", Object::Array(vec![Object::Reference(page_id)]));
        pages.set("Count", Object::Integer(1));
    }
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(catalog_id));
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn flate_bitmaps_reencode_as_png() {
    let bytes = flate_bitmap_document(|_| Object::Name(b"DeviceRGB".to_vec()));
    let parts = expand_bytes(&bytes).unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].mime, "image/png");
    let (w, h) = crate::api::image_dimensions(match &parts[0].content {
        InputContent::Media(bytes) => bytes,
        _ => panic!("expected image bytes"),
    })
    .unwrap();
    assert_eq!((w, h), (2, 1));
}

#[test]
fn an_iccbased_color_space_decodes_like_device_rgb() {
    // [/ICCBased <stream with /N 3>] carries the same samples RGB
    // does; the profile stream itself is never decoded.
    let bytes = flate_bitmap_document(|doc| {
        let icc_id = doc.add_object(Stream::new(
            dictionary! { "N" => Object::Integer(3) },
            b"not a real profile, only the /N matters".to_vec(),
        ));
        Object::Array(vec![
            Object::Name(b"ICCBased".to_vec()),
            Object::Reference(icc_id),
        ])
    });
    let parts = expand_bytes(&bytes).unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].mime, "image/png");
    let (w, h) = crate::api::image_dimensions(match &parts[0].content {
        InputContent::Media(bytes) => bytes,
        _ => panic!("expected image bytes"),
    })
    .unwrap();
    assert_eq!((w, h), (2, 1));
}

#[test]
fn cmyk_reencode_inverts_to_rgb() {
    // Two CMYK pixels: uninked paper, then a mid-strength ink. The
    // formula encode_png documents must come out of the PNG bytes.
    let raw = vec![0u8, 0, 0, 0, 25, 51, 77, 26];
    let png = encode_png(2, 1, 4, &raw).unwrap();
    let pixels = image::load_from_memory(&png).unwrap().to_rgb8().into_raw();
    let expected = |c: u8, k: u8| ((255 - c) as u32 * (255 - k) as u32 / 255) as u8;
    assert_eq!(
        pixels,
        vec![
            255,
            255,
            255, // (0,0,0,0): no ink, no black — paper white
            expected(25, 26),
            expected(51, 26),
            expected(77, 26),
        ]
    );
}

#[test]
fn a_pages_own_referenced_resources_beat_inherited_ones() {
    // Both levels declare /Im0; the page's own (a real image) must win
    // over the Pages node's (a form whose empty descent would have
    // left the page with nothing). The merge used to walk lopdf's
    // inherited list forward, letting the ancestor overwrite the page.
    let jpeg = tiny_jpeg();
    let mut doc = Document::with_version("1.5");
    let image_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => Object::Integer(4),
            "Height" => Object::Integer(4),
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => Object::Integer(8),
            "Filter" => "DCTDecode",
        },
        jpeg,
    ));
    let form_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "BBox" => Object::Array(vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(1),
                Object::Integer(1),
            ]),
        },
        Vec::new(),
    ));
    let page_resources_id = doc.add_object(dictionary! {
        "XObject" => Object::Dictionary(dictionary! {
            "Im0" => Object::Reference(image_id),
        }),
    });
    let pages_resources_id = doc.add_object(dictionary! {
        "XObject" => Object::Dictionary(dictionary! {
            "Im0" => Object::Reference(form_id),
        }),
    });
    let pages_id = doc.add_object(dictionary! {
        "Type" => "Pages",
        "Kids" => Object::Array(Vec::new()),
        "Count" => Object::Integer(0),
        "Resources" => Object::Reference(pages_resources_id),
    });
    let content_id = doc.add_object(Stream::new(dictionary! {}, b"q /Im0 Do Q".to_vec()));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => Object::Reference(pages_id),
        "Resources" => Object::Reference(page_resources_id),
        "Contents" => Object::Reference(content_id),
    });
    if let Some(Object::Dictionary(pages)) = doc.objects.get_mut(&pages_id) {
        pages.set("Kids", Object::Array(vec![Object::Reference(page_id)]));
        pages.set("Count", Object::Integer(1));
    }
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(catalog_id));
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();

    let parts = expand_bytes(&bytes).unwrap();
    assert_eq!(parts.len(), 1, "{parts:?}");
    assert_eq!(parts[0].mime, "image/jpeg");
}

#[test]
fn an_image_drawn_twice_materializes_once() {
    let jpeg = tiny_jpeg();
    let mut doc = Document::with_version("1.5");
    let image_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => Object::Integer(4),
            "Height" => Object::Integer(4),
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => Object::Integer(8),
            "Filter" => "DCTDecode",
        },
        jpeg,
    ));
    let pages_id = doc.add_object(dictionary! {
        "Type" => "Pages", "Kids" => Object::Array(Vec::new()), "Count" => Object::Integer(0),
    });
    let content_id = doc.add_object(Stream::new(
        dictionary! {},
        b"q /Im0 Do Q /Im0 Do Q".to_vec(),
    ));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => Object::Reference(pages_id),
        "Resources" => Object::Dictionary(dictionary! {
            "XObject" => Object::Dictionary(dictionary! {
                "Im0" => Object::Reference(image_id),
            }),
        }),
        "Contents" => Object::Reference(content_id),
    });
    if let Some(Object::Dictionary(pages)) = doc.objects.get_mut(&pages_id) {
        pages.set("Kids", Object::Array(vec![Object::Reference(page_id)]));
        pages.set("Count", Object::Integer(1));
    }
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(catalog_id));
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();

    let parts = expand_bytes(&bytes).unwrap();
    assert_eq!(parts.len(), 1, "{parts:?}");
}

/// A two-page book: page 1 carries a real JPEG, page 2's only image
/// is JPEG 2000 — the shape where a confident-looking subset used to
/// ship in silence.
fn partial_book() -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.add_object(dictionary! {
        "Type" => "Pages", "Kids" => Object::Array(Vec::new()), "Count" => Object::Integer(0),
    });
    for (page, (filter, data)) in [
        ("DCTDecode", tiny_jpeg()),
        ("JPXDecode", b"not really jp2".to_vec()),
    ]
    .into_iter()
    .enumerate()
    {
        let image_id = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => Object::Integer(4),
                "Height" => Object::Integer(4),
                "ColorSpace" => "DeviceRGB",
                "BitsPerComponent" => Object::Integer(8),
                "Filter" => filter,
            },
            data,
        ));
        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            format!("q /Im{page} Do Q\n").into_bytes(),
        ));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => Object::Array(vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(100),
                Object::Integer(100),
            ]),
            "Resources" => Object::Dictionary(dictionary! {
                "XObject" => Object::Dictionary(dictionary! {
                    format!("Im{page}") => Object::Reference(image_id),
                }),
            }),
            "Contents" => Object::Reference(content_id),
        });
        if let Some(Object::Dictionary(pages)) = doc.objects.get_mut(&pages_id) {
            if let Ok(Object::Array(kids)) = pages.get_mut(b"Kids") {
                kids.push(Object::Reference(page_id));
            }
            pages.set("Count", (page + 1) as i64);
        }
    }
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(catalog_id));
    let mut out = Vec::new();
    doc.save_to(&mut out).unwrap();
    out
}

#[test]
fn a_partially_lost_page_is_a_note_not_silence() {
    let (parts, notes) = expand_with_notes(&partial_book()).unwrap();
    // Page 1 still ships, exactly as before.
    assert_eq!(parts.len(), 1, "{parts:?}");
    assert_eq!(parts[0].name, "story-p1");
    // Page 2 says what it lost: once for the skipped image, once for
    // the page that ended up with nothing.
    assert!(
        notes
            .iter()
            .any(|n| n.contains("page 2") && n.contains("skipped") && n.contains("JPEG 2000")),
        "{notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|n| n.contains("page 2 contributed nothing")),
        "{notes:?}"
    );
}

#[test]
fn a_whole_document_materializing_normally_stays_silent() {
    // No skips, no empty pages: no notes, so stderr stays quiet for
    // the ordinary picture book.
    let (_, notes) = expand_with_notes(&book(&[tiny_jpeg(), tiny_jpeg()], false, true)).unwrap();
    assert!(notes.is_empty(), "{notes:?}");
}

#[test]
fn an_image_inside_a_form_xobject_materializes() {
    // Producers that wrap page content in Form XObjects (Office and
    // CAD exports) used to lose their images: only the page's
    // top-level image names were followed. The form is also drawn
    // twice — its image must materialize once.
    let jpeg = tiny_jpeg();
    let mut doc = Document::with_version("1.5");
    let image_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => Object::Integer(4),
            "Height" => Object::Integer(4),
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => Object::Integer(8),
            "Filter" => "DCTDecode",
        },
        jpeg,
    ));
    let form_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "BBox" => Object::Array(vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(100),
                Object::Integer(100),
            ]),
            "Resources" => Object::Dictionary(dictionary! {
                "XObject" => Object::Dictionary(dictionary! {
                    "ImF" => Object::Reference(image_id),
                }),
            }),
        },
        b"q /ImF Do Q".to_vec(),
    ));
    let pages_id = doc.add_object(dictionary! {
        "Type" => "Pages", "Kids" => Object::Array(Vec::new()), "Count" => Object::Integer(0),
    });
    let content_id = doc.add_object(Stream::new(
        dictionary! {},
        b"q /Fm0 Do Q /Fm0 Do Q".to_vec(),
    ));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => Object::Reference(pages_id),
        "MediaBox" => Object::Array(vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Integer(100),
            Object::Integer(100),
        ]),
        "Resources" => Object::Dictionary(dictionary! {
            "XObject" => Object::Dictionary(dictionary! {
                "Fm0" => Object::Reference(form_id),
            }),
        }),
        "Contents" => Object::Reference(content_id),
    });
    if let Some(Object::Dictionary(pages)) = doc.objects.get_mut(&pages_id) {
        pages.set("Kids", Object::Array(vec![Object::Reference(page_id)]));
        pages.set("Count", Object::Integer(1));
    }
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(catalog_id));
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();

    let (parts, notes) = expand_with_notes(&bytes).unwrap();
    assert_eq!(parts.len(), 1, "{parts:?}");
    assert_eq!(parts[0].name, "story-p1");
    assert_eq!(parts[0].mime, "image/jpeg");
    assert!(notes.is_empty(), "{notes:?}");
}

/// A chain of `levels` nested forms with the image at the very
/// bottom: the page draws Fm0, Fm0 draws Fm1, and so on down.
fn nested_form_chain(levels: usize) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let image_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => Object::Integer(4),
            "Height" => Object::Integer(4),
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => Object::Integer(8),
            "Filter" => "DCTDecode",
        },
        tiny_jpeg(),
    ));
    // Deepest form first: each form's content draws its child.
    let mut child_ref = Object::Reference(image_id);
    let mut child_name = "Im".to_string();
    for level in (0..levels).rev() {
        let mut form_dict = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Form",
            "BBox" => Object::Array(vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(100),
                Object::Integer(100),
            ]),
        };
        form_dict.set(
            "Resources",
            Object::Dictionary(dictionary! {
                "XObject" => Object::Dictionary(dictionary! {
                    child_name.as_str() => child_ref,
                }),
            }),
        );
        let form_id = doc.add_object(Stream::new(
            form_dict,
            format!("q /{child_name} Do Q").into_bytes(),
        ));
        child_ref = Object::Reference(form_id);
        child_name = format!("Fm{level}");
    }
    let pages_id = doc.add_object(dictionary! {
        "Type" => "Pages", "Kids" => Object::Array(Vec::new()), "Count" => Object::Integer(0),
    });
    let content_id = doc.add_object(Stream::new(dictionary! {}, b"q /Fm0 Do Q".to_vec()));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => Object::Reference(pages_id),
        "MediaBox" => Object::Array(vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Integer(100),
            Object::Integer(100),
        ]),
        "Resources" => Object::Dictionary(dictionary! {
            "XObject" => Object::Dictionary(dictionary! {
                "Fm0" => child_ref,
            }),
        }),
        "Contents" => Object::Reference(content_id),
    });
    if let Some(Object::Dictionary(pages)) = doc.objects.get_mut(&pages_id) {
        pages.set("Kids", Object::Array(vec![Object::Reference(page_id)]));
        pages.set("Count", Object::Integer(1));
    }
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(catalog_id));
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

#[test]
fn form_nesting_descends_four_levels_and_stops() {
    // Four levels of nesting still reach the image at the bottom...
    let parts = expand_bytes(&nested_form_chain(4)).unwrap();
    assert_eq!(parts.len(), 1, "{parts:?}");
    assert_eq!(parts[0].mime, "image/jpeg");
    // ...five hide it: the descent stops, and the document's usual
    // nothing-extractable paths take over.
    let bytes = nested_form_chain(5);
    #[cfg(not(feature = "pdfium"))]
    {
        let err = expand_bytes(&bytes).unwrap_err();
        assert!(
            err.to_string().contains("no supported images or text"),
            "{err}"
        );
    }
    #[cfg(feature = "pdfium")]
    {
        let parts = expand_bytes(&bytes).unwrap();
        assert_eq!(parts.len(), 1, "{parts:?}");
        assert_eq!(parts[0].mime, "image/png");
    }
}

/// A one-page document whose only image carries an array-form
/// `/DecodeParms` with a predictor — undecodable for aido, so the
/// page has nothing extractable.
/// Which shape the /DecodeParms takes in the fixture. (The indirect
/// variant's test asserts the refusal, which only the no-renderer
/// build produces; under pdfium the page renders instead.)
#[cfg_attr(feature = "pdfium", allow(dead_code))]
enum ParmsShape {
    InlineArray,
    Indirect,
}

/// A one-page PDF whose lone image is a predicted flate bitmap, with
/// the /DecodeParms in whichever shape the caller wants exercised:
/// the array form (per-filter parameters) or an indirect reference to
/// the parameter dictionary — both hide the predictor from lopdf's
/// undo, which reads only an inline dictionary.
fn predicted_image_document(parms_shape: ParmsShape) -> Vec<u8> {
    use std::io::Write as _;
    let raw = vec![10u8, 20, 30, 200, 210, 220, 1, 1, 1, 2, 2, 2]; // 2 rows: 4 data bytes + 1 filter byte each
    let flate = {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&raw).unwrap();
        encoder.finish().unwrap()
    };
    let mut doc = Document::with_version("1.5");
    let decode_parms = match parms_shape {
        ParmsShape::InlineArray => Object::Array(vec![Object::Dictionary(dictionary! {
            "Predictor" => Object::Integer(15),
            "Colors" => Object::Integer(3),
            "Columns" => Object::Integer(4),
            "BitsPerComponent" => Object::Integer(8),
        })]),
        ParmsShape::Indirect => {
            let id = doc.add_object(dictionary! {
                "Predictor" => Object::Integer(15),
                "Colors" => Object::Integer(3),
                "Columns" => Object::Integer(4),
                "BitsPerComponent" => Object::Integer(8),
            });
            Object::Reference(id)
        }
    };
    let image_id = doc.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => Object::Integer(4),
            "Height" => Object::Integer(1),
            "ColorSpace" => "DeviceRGB",
            "BitsPerComponent" => Object::Integer(8),
            "Filter" => "FlateDecode",
            "DecodeParms" => decode_parms,
        },
        flate,
    ));
    let pages_id = doc.add_object(dictionary! {
        "Type" => "Pages", "Kids" => Object::Array(Vec::new()), "Count" => Object::Integer(0),
    });
    let content_id = doc.add_object(Stream::new(dictionary! {}, b"q /Im0 Do Q".to_vec()));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => Object::Reference(pages_id),
        "Resources" => Object::Dictionary(dictionary! {
            "XObject" => Object::Dictionary(dictionary! {
                "Im0" => Object::Reference(image_id),
            }),
        }),
        "Contents" => Object::Reference(content_id),
    });
    if let Some(Object::Dictionary(pages)) = doc.objects.get_mut(&pages_id) {
        pages.set("Kids", Object::Array(vec![Object::Reference(page_id)]));
        pages.set("Count", Object::Integer(1));
    }
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(catalog_id));
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

#[cfg(not(feature = "pdfium"))]
#[test]
fn a_predicted_image_in_array_form_is_skipped_not_guessed_at() {
    let err = expand_bytes(&predicted_image_document(ParmsShape::InlineArray)).unwrap_err();
    assert!(
        err.to_string().contains("no supported images or text"),
        "{err}"
    );
}

#[cfg(not(feature = "pdfium"))]
#[test]
fn a_predicted_image_behind_an_indirect_parms_reference_is_skipped_too() {
    let err = expand_bytes(&predicted_image_document(ParmsShape::Indirect)).unwrap_err();
    assert!(
        err.to_string().contains("no supported images or text"),
        "{err}"
    );
}

#[cfg(feature = "pdfium")]
#[test]
fn a_predicted_image_in_array_form_is_skipped_and_the_page_renders() {
    // No guessed-at pixels: the skipped image contributes nothing and
    // the page's true appearance comes from the render.
    let parts = expand_bytes(&predicted_image_document(ParmsShape::InlineArray)).unwrap();
    assert_eq!(parts.len(), 1, "{parts:?}");
    assert_eq!(parts[0].kind, MediaKind::Image);
    assert_eq!(parts[0].mime, "image/png");
}

#[test]
fn a_corrupt_pdf_is_an_error_naming_the_file() {
    let err = expand_bytes(b"%PDF-1.7 not really a pdf").unwrap_err();
    assert!(err.to_string().contains("story.pdf"), "{err}");
}

/// A valid document whose single page has no images and no text.
fn blank_document() -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.add_object(dictionary! {
        "Type" => "Pages", "Kids" => Object::Array(Vec::new()), "Count" => Object::Integer(0),
    });
    let content_id = doc.add_object(Stream::new(dictionary! {}, Vec::new()));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => Object::Reference(pages_id),
        "Contents" => Object::Reference(content_id),
    });
    if let Some(Object::Dictionary(pages)) = doc.objects.get_mut(&pages_id) {
        pages.set("Kids", Object::Array(vec![Object::Reference(page_id)]));
        pages.set("Count", Object::Integer(1));
    }
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(catalog_id));
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

// Without the feature the blank page has nothing to give, so the run
// fails with guidance; with it the page rasterizes instead.
#[cfg(not(feature = "pdfium"))]
#[test]
fn blank_but_valid_pages_still_fail_with_guidance() {
    let err = expand_bytes(&blank_document()).unwrap_err();
    assert!(
        err.to_string().contains("no supported images or text"),
        "{err}"
    );
}

#[cfg(feature = "pdfium")]
#[test]
fn blank_but_valid_pages_render_as_one_png_under_pdfium() {
    let parts = expand_bytes(&blank_document()).unwrap();
    assert_eq!(parts.len(), 1, "{parts:?}");
    assert_eq!(parts[0].name, "story-p1");
    assert_eq!(parts[0].kind, MediaKind::Image);
    assert_eq!(parts[0].mime, "image/png");
    let bytes = match &parts[0].content {
        InputContent::Media(bytes) => bytes,
        _ => panic!("expected image bytes"),
    };
    let (w, h) = crate::api::image_dimensions(bytes).unwrap();
    assert!(w > 0 && h > 0, "{w}x{h}");
    // The page is blank, so every pixel must be the opaque white the
    // renderer fills first — any other value means the bitmap came
    // back uninitialized or with swapped channels.
    let pixels = image::load_from_memory(bytes)
        .unwrap()
        .to_rgba8()
        .into_raw();
    assert!(
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| *p == [255, 255, 255, 255]),
        "a blank page did not render to plain white"
    );
}

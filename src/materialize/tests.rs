use super::test_support::{zip_fixture, zip_with_entries};
use super::*;

fn file_source(name: &str) -> InputSource {
    InputSource::File(std::path::PathBuf::from(name))
}

#[test]
fn non_documents_are_not_recognized() {
    // Text, images and audio keep the classification path.
    assert!(expand("a.txt", b"hello", &file_source("a.txt"), 0, 0)
        .unwrap()
        .is_none());
    // PNG magic.
    let png = [0x89u8, b'P', b'N', b'G'];
    assert!(expand("a.png", &png, &file_source("a.png"), 0, 0)
        .unwrap()
        .is_none());
    // Empty bytes.
    assert!(expand("a", b"", &file_source("a"), 0, 0).unwrap().is_none());
}

#[test]
fn a_bare_zip_is_not_a_document() {
    // PK magic but no parseable central directory with OOXML markers:
    // not recognized, classification takes over.
    assert!(
        expand("a.zip", b"PK\x03\x04garbage", &file_source("a.zip"), 0, 0)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_zip_with_other_entries_is_not_a_document() {
    let bytes = zip_with_entries(&[b"readme.txt", b"data/bin"]);
    assert!(expand("a.zip", &bytes, &file_source("a.zip"), 0, 0)
        .unwrap()
        .is_none());
}

#[test]
fn a_word_document_materializes_through_anydoc() {
    // docx used to be refused with guidance; it converts now — one
    // markdown part carrying the document's provenance.
    let bytes =
        test_support::docx_fixture("<w:body><w:p><w:r><w:t>Chapter One</w:t></w:r></w:p></w:body>");
    let Some((parts, _)) = expand("notes.docx", &bytes, &file_source("notes.docx"), 0, 0).unwrap()
    else {
        panic!("a docx is a document");
    };
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].mime, "text/markdown");
    assert!(parts[0].text().unwrap().contains("Chapter One"));
}

#[test]
fn a_powerpoint_document_materializes_through_anydoc() {
    let bytes = test_support::pptx_fixture("Hello Slide");
    let Some((parts, _)) =
        expand("slides.pptx", &bytes, &file_source("slides.pptx"), 0, 0).unwrap()
    else {
        panic!("a pptx is a document");
    };
    assert_eq!(parts.len(), 1);
    assert!(parts[0].text().unwrap().contains("Hello Slide"));
}

#[test]
fn an_opendocument_package_materializes_through_anydoc() {
    // The dispatcher only says "ODF" (the root content.xml marker);
    // the package mimetype is what makes it a text document.
    let bytes = test_support::odt_fixture("Once upon a time.");
    let Some((parts, _)) = expand("tale.odt", &bytes, &file_source("tale.odt"), 0, 0).unwrap()
    else {
        panic!("an odt is a document");
    };
    assert!(parts[0].text().unwrap().contains("Once upon a time."));
}

#[test]
fn an_epub_materializes_through_anydoc() {
    // The OCF container descriptor (META-INF/container.xml) is the
    // EPUB marker; the spine's chapters ride into one markdown part.
    let bytes = test_support::epub_fixture("Once upon a time.");
    let Some((parts, _)) = expand("tale.epub", &bytes, &file_source("tale.epub"), 0, 0).unwrap()
    else {
        panic!("an epub is a document");
    };
    assert!(parts[0].text().unwrap().contains("Once upon a time."));
}

#[test]
fn a_binary_workbook_routes_to_the_converter() {
    // .xlsb carries xl/workbook.bin instead of workbook.xml, so the
    // calamine branch cannot take it. This anchors two contracts at
    // once: the marker routes into the converter, and a converter
    // failure surfaces as an anyhow error whose message carries the
    // file name (the `cannot convert '{origin}': …` lead-in built by
    // document::conversion_error — the real-content counterpart is
    // document::tests::a_binary_workbook_converts). If anydoc ever
    // changes its error text, this is the test that says so.
    let bytes = test_support::real_zip(&[
        ("xl/workbook.bin", "not a workbook".into()),
        ("[Content_Types].xml", "also not".into()),
    ]);
    let err = expand("book.xlsb", &bytes, &file_source("book.xlsb"), 0, 0).unwrap_err();
    assert!(err.to_string().contains("book.xlsb"), "{err}");
}

#[test]
fn rtf_materializes_through_anydoc() {
    let Some((parts, _)) = expand(
        "tale.rtf",
        br#"{\rtf1\ansi Once upon a time.}"#,
        &file_source("tale.rtf"),
        0,
        0,
    )
    .unwrap() else {
        panic!("rtf is a document");
    };
    assert!(parts[0].text().unwrap().contains("Once upon a time."));
}

#[test]
fn a_compound_file_routes_to_the_converter() {
    // Legacy doc/ppt/xls share the compound-file magic; garbage
    // behind it makes the converter's refusal name the file — proof
    // of routing, not of classification.
    let mut bytes = CFB_MAGIC.to_vec();
    bytes.extend_from_slice(b"not a compound file");
    let err = expand("legacy.doc", &bytes, &file_source("legacy.doc"), 0, 0).unwrap_err();
    assert!(err.to_string().contains("legacy.doc"), "{err}");
}

#[test]
fn csv_by_extension_becomes_a_markdown_table() {
    // CSV has no signature; the extension names it, case-insensitively.
    for name in ["data.csv", "DATA.CSV"] {
        let Some((parts, _)) = expand(
            name,
            b"city,sales\nBeijing,1200\n",
            &file_source(name),
            0,
            0,
        )
        .unwrap() else {
            panic!("a .csv is a document");
        };
        assert!(parts[0].text().unwrap().contains("Beijing"), "{name}");
    }
}

#[test]
fn a_csv_the_converter_cannot_table_falls_back_to_text() {
    // A conversion failure — here an empty file, which tables to
    // nothing — falls back to classification, exactly the pre-anydoc
    // behavior for such a file.
    assert!(expand("junk.csv", b"", &file_source("junk.csv"), 0, 0)
        .unwrap()
        .is_none());
    // So does a table past the budget: the plain text the file
    // already is stays under one single-file input's worth, the
    // inflated table does not.
    let rows = "a,b\n".repeat(4_000_000); // ~16 MB → ~36 MB as a table
    assert!(
        expand("huge.csv", rows.as_bytes(), &file_source("huge.csv"), 0, 0)
            .unwrap()
            .is_none()
    );
}

#[test]
fn an_office_package_declaring_over_512_mb_is_refused_before_reading() {
    // The declared gate stands in front of the converter too: a
    // Word-shaped zip claiming more than the ceiling is refused
    // even though its entries hold nothing.
    let bomb = zip_fixture(&[(b"word/document.xml", 600 * 1024 * 1024)], &[]);
    let err = expand("notes.docx", &bomb, &file_source("notes.docx"), 0, 0).unwrap_err();
    assert!(
        err.to_string().contains("declares more than 512 MB"),
        "{err}"
    );
    // The same gate fronts the workbook loader: a normal declaration
    // passes and runs on into the loader, which fails on the fake
    // content — proof the gate did not swallow it.
    let small = zip_fixture(&[(b"xl/workbook.xml", 1024)], &[]);
    let err = expand("book.xlsx", &small, &file_source("book.xlsx"), 0, 0).unwrap_err();
    assert!(err.to_string().contains("as a workbook"), "{err}");
}

#[test]
fn budget_bounds_one_documents_expansion() {
    let mut budget = Budget::new();
    let mut parts = Vec::new();
    let mut overflow = None;
    for i in 0..=MAX_PARTS_PER_DOCUMENT {
        let built = part(
            file_source("x.pdf"),
            format!("x-p{i}"),
            MediaKind::Text,
            "text/plain",
            InputContent::Text("x".into()),
            None,
        );
        if let Err(e) = push(&mut budget, &mut parts, "x.pdf", built) {
            overflow = Some(e.to_string());
            break;
        }
    }
    assert_eq!(parts.len(), MAX_PARTS_PER_DOCUMENT);
    assert!(
        overflow
            .as_deref()
            .is_some_and(|m| m.contains("more than 4096 parts")),
        "{overflow:?}"
    );
    // Ids stay contiguous from zero within the document.
    assert_eq!(parts.first().unwrap().id, 0);
    assert_eq!(parts.last().unwrap().id, MAX_PARTS_PER_DOCUMENT - 1);

    // One part alone can trip the byte bound.
    let mut budget = Budget::new();
    let mut parts = Vec::new();
    let huge = part(
        file_source("x.pdf"),
        "big".into(),
        MediaKind::Image,
        "image/png",
        InputContent::Media(vec![0u8; MAX_DOCUMENT_BYTES + 1]),
        None,
    );
    let err = push(&mut budget, &mut parts, "x.pdf", huge).unwrap_err();
    assert!(err.to_string().contains("MB of material"), "{err}");
    assert!(parts.is_empty());
}

#[test]
fn document_stems_are_sanitized() {
    assert_eq!(doc_stem(&file_source("story.pdf"), "story.pdf"), "story");
    assert_eq!(
        doc_stem(&file_source("my.scan.pdf"), "my.scan.pdf"),
        "my-scan"
    );
    assert_eq!(doc_stem(&file_source("绘本.pdf"), "绘本.pdf"), "绘本");
    assert_eq!(doc_stem(&InputSource::Stdin, "stdin"), "stdin");
    // A stem of only odd characters sanitizes to something non-empty
    // (or "artifact"), never breaks downstream naming.
    assert!(!doc_stem(&file_source("...pdf"), "...pdf").is_empty());
}

#[test]
fn notes_dedupe_and_collapse_past_the_cap() {
    let mut budget = Budget::new();
    budget.note("same skip twice".into());
    budget.note("same skip twice".into());
    budget.note("another".into());
    assert_eq!(budget.notes, ["same skip twice", "another"]);

    let mut budget = Budget::new();
    for i in 0..MAX_NOTES {
        budget.note(format!("note {i}"));
    }
    budget.note("one past the cap".into());
    budget.note("another past the cap".into());
    budget.note("another past the cap".into());
    assert_eq!(budget.notes.len(), MAX_NOTES + 1, "{:?}", budget.notes);
    assert_eq!(budget.notes.last().unwrap(), "… 2 more");
}

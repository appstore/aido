use super::*;
use crate::materialize::test_support::{docx_fixture, real_zip};
use crate::materialize::Budget;

fn para(text: &str) -> String {
    format!("<w:p><w:r><w:t>{text}</w:t></w:r></w:p>")
}

fn expand_bytes(bytes: &[u8], format: Option<anydoc::Format>) -> Vec<InputPart> {
    expand(
        "story.docx",
        bytes,
        &InputSource::File("story.docx".into()),
        0,
        0,
        &mut Budget::new(),
        format,
    )
    .unwrap()
}

#[test]
fn a_minimal_docx_becomes_one_markdown_part() {
    let body = format!(
        "<w:body>{}{}</w:body>",
        // A Word heading is a paragraph whose style names Heading1;
        // a converter that honors it renders a markdown heading.
        r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Chapter One</w:t></w:r></w:p>"#,
        para("It was a dark and stormy night."),
    );
    let parts = expand_bytes(&docx_fixture(&body), None);
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].name, "story");
    assert_eq!(parts[0].mime, "text/markdown");
    assert_eq!(parts[0].unit.as_deref(), Some("story.docx#0"));
    let text = parts[0].text().unwrap();
    assert!(text.contains("Chapter One"), "{text}");
    assert!(text.contains("It was a dark and stormy night."), "{text}");
}

#[test]
fn a_docx_table_survives_as_a_markdown_table() {
    let body = format!(
        "<w:body><w:tbl><w:tr><w:tc>{}</w:tc><w:tc>{}</w:tc></w:tr><w:tr><w:tc>{}</w:tc><w:tc>{}</w:tc></w:tr></w:tbl></w:body>",
        para("city"),
        para("sales"),
        para("Beijing"),
        para("1200"),
    );
    let parts = expand_bytes(&docx_fixture(&body), None);
    let text = parts[0].text().unwrap();
    assert!(text.contains("city"), "{text}");
    assert!(text.contains("1200"), "{text}");
}

#[test]
fn rtf_converts_without_a_container() {
    // RTF is plain text with a signature — no ZIP, no gates.
    let rtf = br#"{\rtf1\ansi Once upon a time.}"#;
    let text = expand(
        "tale.rtf",
        rtf,
        &InputSource::File("tale.rtf".into()),
        3,
        7,
        &mut Budget::new(),
        None,
    )
    .unwrap();
    assert_eq!(text.len(), 1);
    assert_eq!(text[0].id, 7);
    assert!(text[0].text().unwrap().contains("Once upon a time."));
}

#[test]
fn csv_converts_to_a_markdown_table() {
    // CSV carries no signature: the dispatcher names it explicitly.
    let csv = b"city,sales\nBeijing,1200\n";
    let parts = expand_bytes(csv, Some(anydoc::Format::Csv));
    let text = parts[0].text().unwrap();
    assert!(text.contains("Beijing"), "{text}");
    assert!(text.contains("1200"), "{text}");
}

#[test]
fn an_empty_document_is_an_error_naming_the_file() {
    // A valid package whose body holds nothing readable.
    let body = "<w:body><w:sectPr/></w:body>";
    let err = expand(
        "blank.docx",
        &docx_fixture(body),
        &InputSource::File("blank.docx".into()),
        0,
        0,
        &mut Budget::new(),
        None,
    )
    .unwrap_err();
    assert!(err.to_string().contains("blank.docx"), "{err}");
}

#[test]
fn a_malformed_package_is_an_error_not_a_crash() {
    // The dispatcher routed it here as a docx, but the body part is
    // missing: the converter refuses, the message names the file.
    let broken = real_zip(&[("[Content_Types].xml", "not really".into())]);
    let err = expand(
        "broken.docx",
        &broken,
        &InputSource::File("broken.docx".into()),
        0,
        0,
        &mut Budget::new(),
        Some(anydoc::Format::Docx),
    )
    .unwrap_err();
    assert!(err.to_string().contains("broken.docx"), "{err}");
}

#[test]
fn a_document_past_the_budget_is_refused() {
    // ~64 MB of text compresses to a few kilobytes: conversion
    // succeeds, the document budget refuses the materialized size.
    let line = "It was a dark and stormy night, and the rain fell in torrents.\n";
    let body = format!("<w:body>{}</w:body>", para(&line.repeat(1_000_000)));
    let err = match expand(
        "huge.docx",
        &docx_fixture(&body),
        &InputSource::File("huge.docx".into()),
        0,
        0,
        &mut Budget::new(),
        None,
    ) {
        // Panic without Debug-printing the 64 MB part the Ok value
        // carries.
        Ok(parts) => panic!(
            "a {} MB document should be refused",
            parts[0].text().unwrap().len() / (1024 * 1024)
        ),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("MB of material"),
        "{}",
        err.to_string()
    );
}

/// A real Word 97 compound file, committed under
/// `tests/fixtures/documents/` (see its README): the binary OLE
/// container cannot be assembled from parts the way a ZIP package
/// can. CJK body text must survive the legacy format into markdown.
#[test]
fn a_legacy_word_document_converts() {
    let parts = expand(
        "legacy.doc",
        include_bytes!("../../../tests/fixtures/documents/sample.doc"),
        &InputSource::File("legacy.doc".into()),
        0,
        0,
        &mut Budget::new(),
        None,
    )
    .unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].name, "legacy");
    assert_eq!(parts[0].mime, "text/markdown");
    assert_eq!(parts[0].text().unwrap(), "Office preview 中文文档\n");
}

/// A real BIFF8 workbook in a compound file, riding anydoc's Excel
/// format into a markdown table. Dispatch goes through the
/// compound-file magic with `None` — detection is anydoc's job.
#[test]
fn a_legacy_excel_workbook_converts() {
    let parts = expand(
        "legacy.xls",
        include_bytes!("../../../tests/fixtures/documents/sample.xls"),
        &InputSource::File("legacy.xls".into()),
        0,
        0,
        &mut Budget::new(),
        None,
    )
    .unwrap();
    assert_eq!(parts.len(), 1);
    let text = parts[0].text().unwrap();
    assert!(text.contains("Office preview 中文文档"), "{text}");
}

/// A real BIFF12 workbook, committed under `tests/fixtures/documents/`
/// (see its README): the binary workbook flavor no open-source writer
/// can produce. The Excel format must ride its grid into a markdown
/// table end to end.
#[test]
fn a_binary_workbook_converts() {
    let parts = expand(
        "legacy.xlsb",
        include_bytes!("../../../tests/fixtures/documents/sample.xlsb"),
        &InputSource::File("legacy.xlsb".into()),
        0,
        0,
        &mut Budget::new(),
        Some(anydoc::Format::Excel),
    )
    .unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].name, "legacy");
    assert_eq!(parts[0].mime, "text/markdown");
    assert_eq!(
        parts[0].text().unwrap(),
        "| hello | world |\n| --- | --- |\n| 1 | 2 |\n"
    );
}

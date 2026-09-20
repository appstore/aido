//! Word, PowerPoint, legacy binary Office, OpenDocument, RTF and CSV
//! documents convert through the anydoc backend: bytes in, one
//! GitHub-Flavored-Markdown text part out — the shape every text
//! consumer already takes. Markdown is the point: a document's structure
//! (headings, tables, lists) survives into the exact representation the
//! chunk strategies and LLM tasks read best.
//!
//! One part for the whole document, like a text-only PDF: anydoc's model
//! is a single merged body with no sheet or slide boundary to split on,
//! and a merged part keeps a per-part task's chunks in context across the
//! document instead of paying isolated requests. Workbooks are the
//! exception and stay with the calamine loader (`xlsx.rs`), which splits
//! sheets into per-sheet parts and bounds the used grid while streaming —
//! structure anydoc's whole-document model cannot express.
//!
//! ZIP-based containers are gated before conversion (declared sizes and
//! a measured pass over every entry, `security.rs`); anydoc's own
//! resource limits backstop what those gates see. Conversion errors
//! become aido's anyhow messages: encryption says so, a resource limit
//! names the limit that spoke, everything else carries the converter's
//! detail with the file named first.

use super::{doc_stem, part, push, Budget};
use crate::domain::{InputContent, InputPart, InputSource, MediaKind};
use anyhow::{bail, Result};

/// Convert one document through anydoc and emit it as a single markdown
/// text part. `format` pre-names signature-less or already-detected
/// formats (`Some(anydoc::Format::Csv)`, a ZIP package routed by its
/// central directory); `None` lets anydoc detect from the content.
pub(super) fn expand(
    origin: &str,
    bytes: &[u8],
    source: &InputSource,
    document: usize,
    start_id: usize,
    budget: &mut Budget,
    format: Option<anydoc::Format>,
) -> Result<Vec<InputPart>> {
    let markdown = anydoc::to_markdown_bytes(bytes, format)
        .map_err(|error| conversion_error(origin, error))?;
    if markdown.trim().is_empty() {
        bail!("'{origin}' produced no readable content");
    }
    let mut parts = Vec::new();
    push(
        budget,
        &mut parts,
        origin,
        part(
            source.clone(),
            doc_stem(source, origin),
            MediaKind::Text,
            "text/markdown",
            InputContent::Text(markdown),
            // One part for the whole document, keyed like a text-only
            // PDF: grouping is a no-op at this size, but the key stays
            // truthful, and the document number keeps two specs of one
            // file apart.
            Some(format!("{origin}#{document}")),
        ),
    )?;
    for part in &mut parts {
        part.id += start_id;
    }
    Ok(parts)
}

/// anydoc's error vocabulary, translated. The converter's own resource
/// limits are the second line of defense behind the materialize gates,
/// so which limit spoke is worth naming; encryption is its own story the
/// way it is on the PDF path; everything else keeps the converter's
/// detail under one common lead-in.
fn conversion_error(origin: &str, error: anydoc::ConvertError) -> anyhow::Error {
    match error {
        anydoc::ConvertError::Encrypted => {
            anyhow::anyhow!(
                "'{origin}' is encrypted or password-protected; its content cannot be read"
            )
        }
        anydoc::ConvertError::ResourceLimit { limit, detail } => {
            anyhow::anyhow!("'{origin}' exceeds the converter's {limit} limit: {detail}")
        }
        other => anyhow::anyhow!("cannot convert '{origin}': {other}"),
    }
}

#[cfg(test)]
mod tests {
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
            include_bytes!("../../tests/fixtures/documents/sample.doc"),
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
            include_bytes!("../../tests/fixtures/documents/sample.xls"),
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
            include_bytes!("../../tests/fixtures/documents/sample.xlsb"),
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
}

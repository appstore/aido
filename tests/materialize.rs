//! Document materialization through the real binary: a PDF input reaches
//! the plan as its page parts and a workbook as its sheets — the whole
//! gather → plan path, under `--dry-run` so nothing leaves the machine.

mod support;

use lopdf::{dictionary, Object, Stream};
use support::*;

/// An isolated config with a profile the plan can resolve; `--dry-run`
/// never contacts the URL.
fn dry_run_config() -> std::path::PathBuf {
    settings_config(
        "default_profile = \"test\"\n\
         [profiles.test]\nprovider = \"srv\"\nmodel = \"test-model\"\n\
         [providers.srv]\nbase_url = \"http://127.0.0.1:9/unreachable\"\n",
    )
}

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

/// Two pages, each an embedded JPEG; the first page also shows text.
fn picture_book() -> Vec<u8> {
    picture_book_pages(2)
}

/// The same book with `pages` pages: page 1 carries a text layer, the
/// rest are image only.
fn picture_book_pages(pages: usize) -> Vec<u8> {
    let mut doc = lopdf::Document::with_version("1.5");
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
        let mut content = format!("q 100 0 0 100 0 0 cm /Im{i} Do Q\n");
        if i == 0 {
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

/// A minimal ZIP-shaped buffer whose central directory lists `entries` —
/// (name, declared uncompressed size) pairs — with an optional trailing
/// ZIP comment. Same shape the detection scan expects; see the unit-test
/// twin in src/materialize/mod.rs for the field-by-field layout.
fn zip_fixture(entries: &[(&[u8], u32)], comment: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"PK\x03\x04");
    bytes.extend_from_slice(&[0u8; 26]);
    let cd_offset = bytes.len() as u32;
    let cd_start = bytes.len();
    for (name, declared) in entries {
        bytes.extend_from_slice(b"PK\x01\x02");
        bytes.extend_from_slice(&[0u8; 20]);
        bytes.extend_from_slice(&declared.to_le_bytes()); // uncompressed size at +24
        bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 12]);
        bytes.extend_from_slice(name);
    }
    let cd_size = (bytes.len() - cd_start) as u32;
    bytes.extend_from_slice(b"PK\x05\x06");
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    bytes.extend_from_slice(&cd_size.to_le_bytes());
    bytes.extend_from_slice(&cd_offset.to_le_bytes());
    bytes.extend_from_slice(&(comment.len() as u16).to_le_bytes());
    bytes.extend_from_slice(comment);
    bytes
}

// The package builders below (real_zip, docx_fixture, pptx_fixture,
// epub_fixture) deliberately mirror their twins in
// `src/materialize/mod.rs` `test_support`: integration tests are a
// separate crate and cannot see that `pub(crate)` module. "Twin" means
// shape-equivalent — the same entries with the same XML payloads (only
// the XML prolog's trailing whitespace may differ) — so keep each pair
// in sync by hand: a one-sided change would quietly test different
// shapes on the two layers.

/// A real ZIP (zip crate, deflate) holding the given text entries — the
/// minimal honest package shape the anydoc converter parses. Twin of
/// `test_support::real_zip`.
fn real_zip(entries: &[(&str, String)]) -> Vec<u8> {
    use std::io::Write as _;
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, content) in entries {
            zip.start_file(name, options).unwrap();
            zip.write_all(content.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }
    buf.into_inner()
}

/// A minimal but real .docx whose body holds one paragraph of `text`.
/// Twin of `test_support::docx_fixture`.
fn docx_fixture(text: &str) -> Vec<u8> {
    real_zip(&[
        (
            "[Content_Types].xml",
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
</Types>"
                .into(),
        ),
        (
            "_rels/.rels",
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>\
</Relationships>"
                .into(),
        ),
        (
            "word/document.xml",
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
<w:body><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:body></w:document>"
            ),
        ),
    ])
}

/// A minimal but real .pptx: one slide whose shape holds `text`. Twin of
/// `test_support::pptx_fixture`.
fn pptx_fixture(text: &str) -> Vec<u8> {
    real_zip(&[
        (
            "[Content_Types].xml",
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Override PartName=\"/ppt/presentation.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml\"/>\
<Override PartName=\"/ppt/slides/slide1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slide+xml\"/>\
</Types>"
                .into(),
        ),
        (
            "_rels/.rels",
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"ppt/presentation.xml\"/>\
</Relationships>"
                .into(),
        ),
        (
            "ppt/presentation.xml",
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<p:presentation xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
<p:sldIdLst><p:sldId id=\"256\" r:id=\"rId1\"/></p:sldIdLst></p:presentation>"
                .into(),
        ),
        (
            "ppt/_rels/presentation.xml.rels",
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide\" Target=\"slides/slide1.xml\"/>\
</Relationships>"
                .into(),
        ),
        (
            "ppt/slides/slide1.xml",
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<p:sld xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\">\
<p:cSld><p:spTree>\
<p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>\
<p:sp><p:nvSpPr><p:cNvPr id=\"2\" name=\"Title 1\"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>\
<p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp>\
</p:spTree></p:cSld></p:sld>"
            ),
        ),
    ])
}

/// Page 1 is a real picture with a text layer; page 2's only image is
/// JPEG 2000 — the shape where a confident-looking subset used to ship
/// in silence.
fn partial_book() -> Vec<u8> {
    let mut doc = lopdf::Document::with_version("1.5");
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
        let mut content = format!("q 100 0 0 100 0 0 cm /Im{page} Do Q\n");
        if page == 0 {
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
                    format!("Im{page}") => Object::Reference(image_id),
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
fn pdf_pages_and_text_reach_the_plan_in_order() {
    let pdf = temp_file("story.pdf", &picture_book());
    let cfg = dry_run_config();
    let out = run_null_stdin(
        &[
            "ask",
            pdf.to_str().unwrap(),
            "-p",
            "讲这个故事",
            "--dry-run",
        ],
        &[],
        &cfg,
    );
    out.assert_code(0);
    // Page order on the plan: both pages, image and text parts named.
    let stdout = out.stdout();
    assert!(stdout.contains("story-p1-1"), "{stdout}");
    assert!(stdout.contains("story-p1-2"), "{stdout}");
    assert!(stdout.contains("story-p2"), "{stdout}");
    assert!(stdout.find("story-p1-1").unwrap() < stdout.find("story-p2").unwrap());
    assert!(stdout.contains("text"), "{stdout}");
}

/// A text-layer-only PDF (no images at all) is ONE text part for the whole
/// document: per-part text tasks keep cross-page context through the chunk
/// strategies instead of paying one page-isolated request per page.
#[test]
fn a_text_only_pdf_is_one_part_with_page_markers() {
    let mut doc = lopdf::Document::with_version("1.5");
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
    for i in 0..3 {
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
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();

    let pdf = temp_file("paper.pdf", &bytes);
    let cfg = dry_run_config();
    // translate is per-part with text-only inputs: three pages must plan
    // as ONE request (not a batch), so no --out-dir is demanded.
    let out = run_null_stdin(
        &["translate", pdf.to_str().unwrap(), "--dry-run"],
        &[],
        &cfg,
    );
    out.assert_code(0);
    let stdout = out.stdout();
    // One material part named after the document, no per-page parts —
    // and no batch, so stdout delivery stands and --out-dir is never
    // demanded. ("per-part batch" names the task's planning strategy in
    // the report; a batch that demands --out-dir shows as exit 2 above.)
    assert!(stdout.contains("paper"), "{stdout}");
    assert!(
        stdout.contains("1. aido-test-file") || stdout.contains("material:"),
        "the merged part is listed: {stdout}"
    );
    assert!(!stdout.contains("paper-p1"), "no per-page parts: {stdout}");
    assert!(!stdout.contains("paper-p2"), "no per-page parts: {stdout}");
    assert!(!stdout.contains("--out-dir"), "{stdout}");
}

#[test]
fn workbook_sheets_reach_the_plan_as_text() {
    let mut book = rust_xlsxwriter::Workbook::new();
    let sheet = book.add_worksheet().set_name("销售").unwrap();
    sheet.write(0, 0, "城市").unwrap();
    sheet.write(1, 0, "北京").unwrap();
    let xlsx = temp_file("report.xlsx", &book.save_to_buffer().unwrap());
    let cfg = dry_run_config();
    let out = run_null_stdin(
        &["ask", xlsx.to_str().unwrap(), "-p", "总结", "--dry-run"],
        &[],
        &cfg,
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("report-销售"), "{stdout}");
    assert!(stdout.contains("single request"), "{stdout}");
}

/// A Word document converts through the anydoc backend now: one markdown
/// text part named after the file, planned like any text input.
#[test]
fn a_word_document_reaches_the_plan_as_markdown() {
    let docx = temp_file("notes.docx", &docx_fixture("Meeting notes from Tuesday"));
    let cfg = dry_run_config();
    let out = run_null_stdin(
        &["ask", docx.to_str().unwrap(), "-p", "总结", "--dry-run"],
        &[],
        &cfg,
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("notes"), "{stdout}");
    assert!(stdout.contains("single request"), "{stdout}");
}

#[test]
fn a_powerpoint_document_reaches_the_plan_as_markdown() {
    let pptx = temp_file("slides.pptx", &pptx_fixture("Quarterly review"));
    let cfg = dry_run_config();
    let out = run_null_stdin(
        &["ask", pptx.to_str().unwrap(), "-p", "总结", "--dry-run"],
        &[],
        &cfg,
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("slides"), "{stdout}");
    assert!(stdout.contains("single request"), "{stdout}");
}

/// CSV has no content signature: the extension names it, and the table
/// conversion rides the same markdown-part path as the Office formats.
#[test]
fn a_csv_file_reaches_the_plan_as_a_markdown_table() {
    let csv = temp_file("data.csv", b"city,sales\nBeijing,1200\n");
    let cfg = dry_run_config();
    let out = run_null_stdin(
        &["ask", csv.to_str().unwrap(), "-p", "总结", "--dry-run"],
        &[],
        &cfg,
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("data"), "{stdout}");
    assert!(stdout.contains("single request"), "{stdout}");
}

/// A minimal but real .epub: OCF container descriptor, one package
/// document, one xhtml chapter whose body holds `text`. Twin of
/// `test_support::epub_fixture`.
fn epub_fixture(text: &str) -> Vec<u8> {
    real_zip(&[
        ("mimetype", "application/epub+zip".into()),
        (
            "META-INF/container.xml",
            "<?xml version=\"1.0\"?>\
<container version=\"1.0\" xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\">\
<rootfiles><rootfile full-path=\"content.opf\" media-type=\"application/oebps-package+xml\"/></rootfiles></container>"
                .into(),
        ),
        (
            "content.opf",
            "<?xml version=\"1.0\"?>\
<package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\"><metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:title>Book</dc:title><dc:identifier id=\"id\">urn:uuid:aido</dc:identifier><dc:language>en</dc:language></metadata>\
<manifest><item id=\"ch1\" href=\"ch1.xhtml\" media-type=\"application/xhtml+xml\"/></manifest>\
<spine><itemref idref=\"ch1\"/></spine></package>"
                .into(),
        ),
        (
            "ch1.xhtml",
            format!(
                "<?xml version=\"1.0\"?><html xmlns=\"http://www.w3.org/1999/xhtml\"><body><p>{text}</p></body></html>"
            ),
        ),
    ])
}

#[test]
fn an_epub_reaches_the_plan_as_markdown() {
    let epub = temp_file("tale.epub", &epub_fixture("A very memorable chapter"));
    let cfg = dry_run_config();
    let out = run_null_stdin(
        &["ask", epub.to_str().unwrap(), "-p", "总结", "--dry-run"],
        &[],
        &cfg,
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("tale"), "{stdout}");
    assert!(stdout.contains("single request"), "{stdout}");
}

/// The committed legacy binary fixtures (see fixtures/documents/README.md)
/// are real compound files: the CFB magic routes them through the
/// converter and their text reaches the plan like any text input.
#[test]
fn a_legacy_word_document_reaches_the_plan_as_markdown() {
    let doc = temp_file(
        "legacy.doc",
        include_bytes!("fixtures/documents/sample.doc"),
    );
    let cfg = dry_run_config();
    let out = run_null_stdin(
        &["ask", doc.to_str().unwrap(), "-p", "总结", "--dry-run"],
        &[],
        &cfg,
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("legacy"), "{stdout}");
    assert!(stdout.contains("single request"), "{stdout}");
}

#[test]
fn a_legacy_excel_workbook_reaches_the_plan_as_markdown() {
    let xls = temp_file(
        "legacy.xls",
        include_bytes!("fixtures/documents/sample.xls"),
    );
    let cfg = dry_run_config();
    let out = run_null_stdin(
        &["ask", xls.to_str().unwrap(), "-p", "总结", "--dry-run"],
        &[],
        &cfg,
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("legacy"), "{stdout}");
    assert!(stdout.contains("single request"), "{stdout}");
}

/// The binary workbook flavor (BIFF12 in an OPC package): the
/// xl/workbook.bin marker routes it through the converter and the grid
/// reaches the plan as a markdown table.
#[test]
fn a_binary_workbook_reaches_the_plan_as_markdown() {
    let xlsb = temp_file(
        "legacy.xlsb",
        include_bytes!("fixtures/documents/sample.xlsb"),
    );
    let cfg = dry_run_config();
    let out = run_null_stdin(
        &["ask", xlsb.to_str().unwrap(), "-p", "总结", "--dry-run"],
        &[],
        &cfg,
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("legacy"), "{stdout}");
    assert!(stdout.contains("single request"), "{stdout}");
}

#[test]
fn a_pdf_via_stdin_materializes_like_a_file() {
    let pdf = picture_book();
    let cfg = dry_run_config();
    let out = run(
        &["ask", "-", "-p", "讲这个故事", "--dry-run"],
        &pdf,
        &[("AIDO_CONFIG", cfg.to_str().unwrap())],
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("stdin-p1"), "{stdout}");
    assert!(stdout.contains("stdin-p2"), "{stdout}");
}

/// The per-part batch is the page count, not the part count: page 1 is
/// image + text layer and page 2 image only, so the book holds three
/// parts but must plan exactly two requests — the text layer rides with
/// its page's image instead of buying a separate "extract the text from
/// the image" request.
#[test]
fn a_mixed_page_pdf_plans_one_request_per_page_not_per_part() {
    let pdf = temp_file("book.pdf", &picture_book());
    let cfg = dry_run_config();
    // Two units are a batch: one result per page needs --out-dir.
    let out = run_null_stdin(&["ocr", pdf.to_str().unwrap(), "--dry-run"], &[], &cfg);
    out.assert_code(2);
    assert!(out.stderr().contains("--out-dir"), "{}", out.stderr());

    let out_dir = temp_dir("book-out");
    let out = run_null_stdin(
        &[
            "ocr",
            pdf.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
            "--dry-run",
        ],
        &[],
        &cfg,
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("per-part batch"), "{stdout}");
    let line = stdout
        .lines()
        .find(|l| l.starts_with("processing:"))
        .unwrap();
    // Exactly two planned requests, labeled by each page's primary part;
    // the text layer (-p1-2) never plans one of its own.
    assert_eq!(line.matches("; ").count(), 1, "{line}");
    assert!(line.contains("-p1-1; "), "{line}");
    assert!(line.trim_end().ends_with("-p2"), "{line}");
    assert!(!line.contains("-p1-2"), "{line}");
}

#[test]
fn a_one_page_pdf_plans_a_single_request_without_out_dir() {
    // One unit is not a batch: the run keeps the plain single-request
    // plan and stdout delivery, no matter how many parts the page holds.
    let pdf = temp_file("single.pdf", &picture_book_pages(1));
    let cfg = dry_run_config();
    let out = run_null_stdin(&["ocr", pdf.to_str().unwrap(), "--dry-run"], &[], &cfg);
    out.assert_code(0);
    let stdout = out.stdout();
    // The strategy's single-step shape: no slice/chunk labels follow.
    assert!(stdout.contains("(no image needs slicing)"), "{stdout}");
}

/// Partial material speaks up on stderr, the way the processors' notes
/// do: page 2 lost its only image to an unsupported codec, and the run
/// must say so while still succeeding with page 1. `--quiet` silences
/// the note, never the run.
#[test]
fn materialization_notes_reach_stderr_and_quiet_silences_them() {
    let pdf = temp_file("partial.pdf", &partial_book());
    let cfg = dry_run_config();
    let out = run_null_stdin(
        &[
            "ask",
            pdf.to_str().unwrap(),
            "-p",
            "讲这个故事",
            "--dry-run",
        ],
        &[],
        &cfg,
    );
    out.assert_code(0);
    let stdout = out.stdout();
    assert!(stdout.contains("partial-p1"), "{stdout}");
    let err = out.stderr();
    assert!(err.contains("note: page 2: image skipped"), "{err}");
    assert!(err.contains("JPEG 2000 is not supported"), "{err}");
    assert!(err.contains("note: page 2 contributed nothing"), "{err}");

    let out = run_null_stdin(
        &[
            "ask",
            pdf.to_str().unwrap(),
            "-p",
            "讲这个故事",
            "--dry-run",
            "--quiet",
        ],
        &[],
        &cfg,
    );
    out.assert_code(0);
    assert!(
        !out.stderr().contains("note:"),
        "--quiet suppresses the note: {}",
        out.stderr()
    );
}

/// A workbook-shaped zip whose central directory declares more than the
/// decompression ceiling is refused before any entry data is read; a
/// normal declaration reaches the loader untouched.
#[test]
fn a_workbook_declaring_over_512mb_is_refused() {
    let bomb = temp_file(
        "bomb.xlsx",
        &zip_fixture(&[(b"xl/workbook.xml", 600 * 1024 * 1024)], &[]),
    );
    let cfg = dry_run_config();
    let out = run_null_stdin(
        &["ask", bomb.to_str().unwrap(), "-p", "hi", "--dry-run"],
        &[],
        &cfg,
    );
    // Usage errors exit 2.
    out.assert_code(2);
    let err = out.stderr();
    assert!(err.contains("declares more than 512 MB"), "{err}");
    assert!(err.contains("bomb.xlsx"), "{err}");

    let small = temp_file(
        "small.xlsx",
        &zip_fixture(&[(b"xl/workbook.xml", 1024)], &[]),
    );
    let out = run_null_stdin(
        &["ask", small.to_str().unwrap(), "-p", "hi", "--dry-run"],
        &[],
        &cfg,
    );
    // The gate passed it on: the loader fails on the fake workbook
    // content instead of the declared-size refusal.
    out.assert_code(2);
    let err = out.stderr();
    assert!(!err.contains("declares more than 512 MB"), "{err}");
    assert!(err.contains("as a workbook"), "{err}");
}

/// Piped material is never exempt from the run's total limit — and the
/// limit bites on the *materialized* bytes, which are bigger than the
/// file's. The same workbook from a path under a generous budget passes.
#[test]
fn stdin_materialization_is_charged_to_the_total_limit() {
    let mut book = rust_xlsxwriter::Workbook::new();
    let sheet = book.add_worksheet().set_name("s").unwrap();
    sheet.write(0, 0, "x".repeat(400)).unwrap();
    let bytes = book.save_to_buffer().unwrap();
    let budget_cfg = settings_config("[settings]\nhistory_keep = 0\ninput_bytes = 64\n");

    // Via stdin with a tiny budget: the materialized markdown exceeds it.
    let out = run(
        &["ask", "-", "-p", "总结", "--dry-run"],
        &bytes,
        &[("AIDO_CONFIG", budget_cfg.to_str().unwrap())],
    );
    out.assert_code(2);
    let err = out.stderr();
    assert!(err.contains("inputs exceed the"), "{err}");
    assert!(err.contains("total limit"), "{err}");

    // The same workbook from a path with the default (generous) budget.
    let xlsx = temp_file("wide.xlsx", &bytes);
    let cfg = dry_run_config();
    let out = run_null_stdin(
        &["ask", xlsx.to_str().unwrap(), "-p", "总结", "--dry-run"],
        &[],
        &cfg,
    );
    out.assert_code(0);
    assert!(out.stdout().contains("wide-s"), "{}", out.stdout());
}

#[test]
fn the_same_file_passed_twice_keeps_its_page_units_distinct() {
    // Two explicit specs of one document are two books to process, not
    // one merged unit set: the per-page units carry a per-spec document
    // number, so the plan shows one request per page per occurrence.
    let pdf = temp_file("book.pdf", &picture_book());
    let cfg = dry_run_config();
    let out_dir = temp_dir("book-twice-out");
    let out = run_null_stdin(
        &[
            "ocr",
            pdf.to_str().unwrap(),
            pdf.to_str().unwrap(),
            "--out-dir",
            out_dir.to_str().unwrap(),
            "--dry-run",
        ],
        &[],
        &cfg,
    );
    out.assert_code(0);
    let stdout = out.stdout();
    // 2 pages x 2 occurrences = 4 units, the second occurrence's units
    // ordered after the first's — never merged into two.
    let processing = stdout
        .lines()
        .find(|line| line.starts_with("processing:"))
        .unwrap_or("");
    // Unit labels, in plan order: the first occurrence's two pages, then
    // the second occurrence's — four units, never merged into two.
    let suffixes: Vec<String> = processing
        .split(" — ")
        .last()
        .unwrap_or_default()
        .split("; ")
        .map(|label| {
            label
                .split_once("-book-")
                .map(|(_, rest)| rest.to_string())
                .unwrap_or_else(|| label.to_string())
        })
        .collect();
    assert_eq!(suffixes, ["p1-1", "p2", "p1-1", "p2"], "{processing}");
}

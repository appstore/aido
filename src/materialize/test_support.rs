/// A minimal ZIP-shaped buffer whose central directory lists exactly
/// `entries` — (name, declared uncompressed size) pairs, which the
/// detection scan and the declared-size gate read but never
/// decompress — with an optional ZIP comment trailing the EOCD. Field
/// offsets follow the appnote: uncompressed size at +24, name_len at
/// +28, extra at +30, comment at +32, fixed fields through +46.
pub(crate) fn zip_fixture(entries: &[(&[u8], u32)], comment: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"PK\x03\x04"); // local header stand-in
    bytes.extend_from_slice(&[0u8; 26]);
    let cd_offset = bytes.len() as u32;
    let cd_start = bytes.len();
    for (name, declared) in entries {
        bytes.extend_from_slice(b"PK\x01\x02");
        bytes.extend_from_slice(&[0u8; 20]); // made-by .. last-modified
        bytes.extend_from_slice(&declared.to_le_bytes()); // uncompressed size at +24
        bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes()); // extra_len
        bytes.extend_from_slice(&0u16.to_le_bytes()); // comment_len
        bytes.extend_from_slice(&[0u8; 12]); // disk no .. local header offset
        bytes.extend_from_slice(name);
    }
    let cd_size = (bytes.len() - cd_start) as u32;
    bytes.extend_from_slice(b"PK\x05\x06");
    bytes.extend_from_slice(&0u16.to_le_bytes()); // disk number
    bytes.extend_from_slice(&0u16.to_le_bytes()); // cd disk number
    bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    bytes.extend_from_slice(&cd_size.to_le_bytes());
    bytes.extend_from_slice(&cd_offset.to_le_bytes());
    bytes.extend_from_slice(&(comment.len() as u16).to_le_bytes());
    bytes.extend_from_slice(comment);
    bytes
}

/// The shape most detection tests want: zero declared sizes, no
/// comment.
pub(crate) fn zip_with_entries(names: &[&[u8]]) -> Vec<u8> {
    zip_fixture(
        &names.iter().map(|name| (*name, 0u32)).collect::<Vec<_>>(),
        &[],
    )
}

/// A real ZIP (zip crate, deflate) holding the given text entries —
/// the shape every honest test document takes, when the bytes under
/// the central directory must actually parse.
pub(crate) fn real_zip(entries: &[(&str, String)]) -> Vec<u8> {
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

/// A minimal but real .docx wrapping `body_xml` (the children of a
/// `<w:body>`): package content types, the office-document
/// relationship, and one body part. Enough for the converter to
/// parse, small enough to read in one glance.
pub(crate) fn docx_fixture(body_xml: &str) -> Vec<u8> {
    real_zip(&[
        ("[Content_Types].xml", DOCX_CONTENT_TYPES.into()),
        ("_rels/.rels", DOCX_RELS.into()),
        (
            "word/document.xml",
            format!("{XML_DECL}<w:document {W_NS}>{body_xml}</w:document>"),
        ),
    ])
}

const XML_DECL: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#;
const W_NS: &str = "xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"";

const DOCX_CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;

const DOCX_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;

/// A minimal but real .pptx: one slide whose shape holds `slide_text`.
pub(crate) fn pptx_fixture(slide_text: &str) -> Vec<u8> {
    real_zip(&[
        (
            "[Content_Types].xml",
            format!(
                "{XML_DECL}\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Override PartName=\"/ppt/presentation.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml\"/>\
<Override PartName=\"/ppt/slides/slide1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slide+xml\"/>\
</Types>"
            ),
        ),
        ("_rels/.rels", PPTX_RELS.into()),
        (
            "ppt/presentation.xml",
            format!(
                "{XML_DECL}\
<p:presentation xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
<p:sldIdLst><p:sldId id=\"256\" r:id=\"rId1\"/></p:sldIdLst>\
</p:presentation>"
            ),
        ),
        (
            "ppt/_rels/presentation.xml.rels",
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/></Relationships>"#
                .into(),
        ),
        (
            "ppt/slides/slide1.xml",
            format!(
                "{XML_DECL}\
<p:sld xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\" xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\">\
<p:cSld><p:spTree>\
<p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>\
<p:sp><p:nvSpPr><p:cNvPr id=\"2\" name=\"Title 1\"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>\
<p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:t>{slide_text}</a:t></a:r></a:p></p:txBody></p:sp>\
</p:spTree></p:cSld></p:sld>"
            ),
        ),
    ])
}

const PPTX_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/></Relationships>"#;

/// A minimal but real .odt whose body holds one paragraph. The
/// mimetype entry is the identity the ODF spec designates; the
/// manifest lists the parts a strict reader asks for.
pub(crate) fn odt_fixture(paragraph: &str) -> Vec<u8> {
    real_zip(&[
        (
            "mimetype",
            "application/vnd.oasis.opendocument.text".into(),
        ),
        ("META-INF/manifest.xml", ODF_MANIFEST.into()),
        (
            "content.xml",
            format!(
                "{}\
<office:document-content xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\" office:version=\"1.2\">\
<office:body><office:text><text:p>{paragraph}</text:p></office:text></office:body>\
</office:document-content>",
                r#"<?xml version="1.0" encoding="UTF-8"?>"#
            ),
        ),
    ])
}

const ODF_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.2"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.text"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/></manifest:manifest>"#;

/// A minimal but real .epub: OCF container descriptor, one package
/// document, one xhtml chapter whose body holds the text. The same
/// shape anydoc's own tests convert.
pub(crate) fn epub_fixture(chapter_text: &str) -> Vec<u8> {
    real_zip(&[
        (
            "mimetype",
            "application/epub+zip".into(),
        ),
        (
            "META-INF/container.xml",
            r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#
                .into(),
        ),
        (
            "content.opf",
            r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Book</dc:title><dc:identifier id="id">urn:uuid:aido</dc:identifier><dc:language>en</dc:language></metadata><manifest><item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="ch1"/></spine></package>"#
                .into(),
        ),
        (
            "ch1.xhtml",
            format!(
                r#"<?xml version="1.0"?><html xmlns="http://www.w3.org/1999/xhtml"><body><p>{chapter_text}</p></body></html>"#
            ),
        ),
    ])
}

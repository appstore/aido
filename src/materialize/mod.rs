//! Document materialization: recognized document containers expand into
//! ordinary text/image parts at gather time, so everything downstream —
//! adapters, processors, task definitions, the capability envelope — sees
//! nothing new. A picture-book PDF becomes its page images and a workbook
//! becomes one text part per sheet, exactly the material a task may
//! already take from files, stdin and the clipboard.
//!
//! Detection is by content, not extension — the same policy [`classify`]
//! applies to images and audio. A `%PDF-` header (searched through the
//! first KiB, as readers may see leading junk from scanners and mail
//! gateways) is a PDF; a ZIP archive whose central directory lists
//! `xl/workbook.xml` is an OOXML workbook; `word/document.xml`,
//! `ppt/presentation.xml`, `xl/workbook.bin`, a root `content.xml` or
//! `META-INF/container.xml` is a Word, PowerPoint, binary-workbook,
//! OpenDocument or EPUB package; a compound-file or `{\rtf` signature is
//! a legacy Office document or rich text. Word, PowerPoint, OpenDocument,
//! EPUB, legacy Office, RTF and CSV convert through the anydoc backend
//! ([`document`]) behind the container gates in [`security`]; a workbook
//! keeps its dedicated loader ([`xlsx`]), which splits sheets into
//! per-sheet parts and bounds the used grid while streaming. Anything
//! else keeps the content-classification path: a random ZIP is "neither
//! valid UTF-8 text nor a supported image/audio file", exactly as before
//! this module existed.
//!
//! Decomposition is bounded like reading: one document yields at most one
//! single-file input's worth of material ([`Budget`]), whatever its pages
//! or sheets claim to contain — hostile containers must not turn a small
//! file into an unbounded expansion.
//!
//! Part names matter beyond labels: in a per-part batch the artifact stem
//! derives from the part's name (`story-p3` → `story-p3.txt`), so page and
//! sheet components are sanitized here — dots would truncate the stem, and
//! duplicate names would collide in `--out-dir`.
//!
//! [`classify`]: crate::input::classify

use crate::domain::{InputContent, InputPart, InputSource, MediaKind};
use anyhow::{bail, Result};
use security::zip_lists_entry;
use std::collections::HashSet;

pub(crate) mod document;
pub(crate) mod pdf;
pub(crate) mod pdf_render;
pub(crate) mod security;
pub(crate) mod xlsx;

/// One document expands to at most this many parts — the same ceiling one
/// glob spec has in [`crate::input`].
pub(super) const MAX_PARTS_PER_DOCUMENT: usize = 4096;
/// One document expands to at most this many materialized bytes — one
/// [`crate::input`] single-file cap's worth.
pub(super) const MAX_DOCUMENT_BYTES: usize = 32 * 1024 * 1024;
/// Distinct notes one document may carry before the rest collapse into a
/// single "… N more" note — notes explain partial material, they must not
/// become their own flood.
const MAX_NOTES: usize = 32;

/// The expansion guard one document's loader carries: every part it emits
/// is admitted here, so a hostile container is refused instead of growing
/// without bound before the run's total budget can speak. The budget also
/// carries the run's notes channel: partial material (a page that yielded
/// nothing, an image in a codec aido cannot carry) is explained there so
/// a confident-looking subset never ships silently.
pub(super) struct Budget {
    bytes: usize,
    /// The aggregate materialized-byte ceiling. Production uses
    /// [`MAX_DOCUMENT_BYTES`]; tests inject a smaller cap so the
    /// admission boundary is exercised without materializing 32 MB.
    byte_cap: usize,
    parts: usize,
    notes: Vec<String>,
    /// Every message ever noted, so repeats stay deduped even after the
    /// visible notes hit the cap.
    seen: HashSet<String>,
    notes_dropped: usize,
}

impl Budget {
    pub(super) fn new() -> Self {
        Self::with_byte_cap(MAX_DOCUMENT_BYTES)
    }

    /// Production uses the document cap; unit tests can use a small cap.
    fn with_byte_cap(cap: usize) -> Self {
        Self {
            bytes: 0,
            byte_cap: cap,
            parts: 0,
            notes: Vec::new(),
            seen: HashSet::new(),
            notes_dropped: 0,
        }
    }

    /// The render fallback replaces extraction wholesale: notes about
    /// images it rendered around no longer describe the delivered material.
    /// (Only the pdfium build renders; elsewhere this is dead weight.)
    #[cfg_attr(not(feature = "pdfium"), allow(dead_code))]
    pub(super) fn clear_notes(&mut self) {
        self.notes.clear();
    }

    pub(super) fn admit(&mut self, origin: &str, size: usize) -> Result<()> {
        self.admit_material(origin, size, 1)
    }

    /// Admit bytes and final parts independently: PDF text can grow one
    /// merged part, then become page parts if a supported image appears.
    /// Failed admission leaves both counters unchanged.
    fn admit_material(&mut self, origin: &str, size: usize, parts: usize) -> Result<()> {
        let next_parts = self.parts.saturating_add(parts);
        if next_parts > MAX_PARTS_PER_DOCUMENT {
            bail!("'{origin}' expands to more than {MAX_PARTS_PER_DOCUMENT} parts; split it into smaller documents");
        }
        let next_bytes = self.bytes.saturating_add(size);
        if next_bytes > self.byte_cap {
            bail!(
                "'{origin}' expands to more than {} MB of material; split it into smaller documents",
                self.byte_cap / (1024 * 1024)
            );
        }
        self.parts = next_parts;
        self.bytes = next_bytes;
        Ok(())
    }

    /// Record one note about partial material. Identical messages
    /// collapse into the first; past [`MAX_NOTES`] distinct notes the rest
    /// share one running "… N more" note.
    pub(super) fn note(&mut self, message: String) {
        if !self.seen.insert(message.clone()) {
            return;
        }
        if self.notes.len() < MAX_NOTES {
            self.notes.push(message);
        } else {
            self.notes_dropped += 1;
            if self.notes.last().is_some_and(|n| n.starts_with("… ")) {
                self.notes.pop();
            }
            self.notes.push(format!("… {} more", self.notes_dropped));
        }
    }
}

/// Expand recognized document bytes into ordered parts; `Ok(None)` means
/// "not a recognized document" and the caller falls back to content
/// classification. Parts take consecutive ids from `start_id` — contiguity
/// is load-bearing downstream (batch bookkeeping keys off part ids). The
/// second tuple element carries the
/// loader's notes about partial material (skipped images, pages that
/// yielded nothing) for the caller to surface on a side channel; a
/// document that materializes whole yields none.
pub(crate) fn expand(
    origin: &str,
    bytes: &[u8],
    source: &InputSource,
    document: usize,
    start_id: usize,
) -> Result<Option<(Vec<InputPart>, Vec<String>)>> {
    let mut budget = Budget::new();
    // ISO 32000 says readers should search the first bytes for the header:
    // scan-to-PDF pipelines and mail gateways prepend junk to real PDFs,
    // and lopdf reads the xref from the end, so it opens such files fine.
    let pdf_header = bytes.starts_with(b"%PDF-")
        || bytes[..bytes.len().min(1024)]
            .windows(5)
            .any(|window| window == b"%PDF-");
    if pdf_header {
        let parts = pdf::expand(origin, bytes, source, document, start_id, &mut budget)?;
        return Ok(Some((parts, budget.notes)));
    }
    if bytes.starts_with(b"PK\x03\x04") {
        if zip_lists_entry(bytes, b"xl/workbook.xml") {
            // The declared decompressed sizes are a cheap first gate
            // against a deflate bomb riding in a small file — one a
            // lying header still passes, since a header costs nothing
            // to write; the xlsx loader's bounded second pass measures
            // what the eagerly-loaded entries really decompress to.
            security::gate_declared(origin, bytes)?;
            let parts = xlsx::expand(origin, bytes, source, document, start_id, &mut budget)?;
            return Ok(Some((parts, budget.notes)));
        }
        // Word, PowerPoint, OpenDocument, EPUB and the binary workbook
        // (.xlsb) convert through the anydoc backend. The same two gates
        // bound what conversion may decompress: anydoc parses the
        // container with its own reader, and a lying header must not buy
        // it unbounded work.
        let office = if zip_lists_entry(bytes, b"word/document.xml") {
            Some(Some(anydoc::Format::Docx))
        } else if zip_lists_entry(bytes, b"ppt/presentation.xml") {
            Some(Some(anydoc::Format::Pptx))
        } else if zip_lists_entry(bytes, b"content.xml") {
            // OpenDocument: odt/ods/odp share the package shape, so the
            // marker only says "ODF" — anydoc tells them apart by the
            // package mimetype.
            Some(None)
        } else if zip_lists_entry(bytes, b"xl/workbook.bin") {
            // The binary workbook flavor (.xlsb): same OPC package as a
            // workbook but BIFF12 parts, so the workbook.xml scan above
            // never matches. The per-sheet calamine loader cannot read
            // it; the converter can.
            Some(Some(anydoc::Format::Excel))
        } else if zip_lists_entry(bytes, b"META-INF/container.xml") {
            // EPUB: the OCF container descriptor is the package's
            // identity (the mimetype entry is its mandatory carrier, but
            // detection falls back to the descriptor).
            Some(Some(anydoc::Format::Epub))
        } else {
            None
        };
        if let Some(format) = office {
            security::gate_declared(origin, bytes)?;
            security::verify_all(origin, bytes, security::MAX_OOXML_DECOMPRESSED, "document")?;
            let parts = document::expand(
                origin,
                bytes,
                source,
                document,
                start_id,
                &mut budget,
                format,
            )?;
            return Ok(Some((parts, budget.notes)));
        }
        // A ZIP that is none of these (or whose central directory
        // cannot be parsed) is not ours to judge.
    }
    // Legacy binary Office (doc/ppt/xls share the compound-file
    // container; anydoc tells them apart by their OLE stream names) and
    // RTF convert through anydoc too. Compound files store their sectors
    // uncompressed and RTF is plain text — no deflate surface, so no
    // ZIP gates; anydoc's own resource limits apply.
    if bytes.starts_with(&CFB_MAGIC) || bytes.starts_with(b"{\\rtf") {
        let parts = document::expand(origin, bytes, source, document, start_id, &mut budget, None)?;
        return Ok(Some((parts, budget.notes)));
    }
    // CSV carries no signature: the extension names it, and only a
    // converter success changes anything. A failure (content anydoc
    // cannot table, or material past the budget) falls back to
    // classification, so a .csv keeps behaving like the plain text it
    // is — one text part — whenever the table conversion cannot
    // improve on that.
    if names_csv(origin) {
        match document::expand(
            origin,
            bytes,
            source,
            document,
            start_id,
            &mut budget,
            Some(anydoc::Format::Csv),
        ) {
            Ok(parts) => return Ok(Some((parts, budget.notes))),
            Err(_) => return Ok(None),
        }
    }
    Ok(None)
}

/// The compound-file magic every legacy Office binary starts with.
const CFB_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

/// CSV is the one dispatch on extension — it has no signature to sniff.
/// Stdin and clipboard origins have no extension and stay text.
fn names_csv(origin: &str) -> bool {
    std::path::Path::new(origin)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("csv"))
}

/// The label stem every part of one document shares: the source file's
/// stem, or the origin ("stdin") otherwise. Sanitized so the per-part
/// artifact stem derived from it survives intact — dots would truncate
/// it, slashes would invent directories.
pub(crate) fn doc_stem(source: &InputSource, origin: &str) -> String {
    let raw = match source {
        InputSource::File(p) => p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| origin.to_string()),
        _ => origin.to_string(),
    };
    crate::output::sanitize_stem(&raw)
}

/// Build one materialized part: an ordinary text/image part that carries
/// the document as its provenance. `unit` is the sub-document key every
/// part of one page/sheet shares (see [`InputPart::unit`]), so a per-part
/// task plans the unit's parts as one request.
pub(super) fn part(
    source: InputSource,
    name: String,
    kind: MediaKind,
    mime: &str,
    content: InputContent,
    unit: Option<String>,
) -> InputPart {
    InputPart {
        id: 0,
        source,
        name,
        kind,
        unknown_kind: false,
        mime: mime.to_string(),
        content,
        unit,
    }
}

/// Charge a built part to the document's budget and append it, assigning
/// the next contiguous id.
pub(super) fn push(
    budget: &mut Budget,
    parts: &mut Vec<InputPart>,
    origin: &str,
    part: InputPart,
) -> Result<()> {
    budget.admit(origin, size_of(&part.content))?;
    append(parts, part);
    Ok(())
}

/// Append material already admitted during extraction, without charging it twice.
fn append(parts: &mut Vec<InputPart>, mut part: InputPart) {
    part.id = start_id_of(parts);
    parts.push(part);
}

fn start_id_of(parts: &[InputPart]) -> usize {
    parts.last().map_or(0, |last| last.id + 1)
}

fn size_of(content: &InputContent) -> usize {
    match content {
        InputContent::Text(s) => s.len(),
        InputContent::Media(b) => b.len(),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
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
}

#[cfg(test)]
mod tests {
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
        let bytes = test_support::docx_fixture(
            "<w:body><w:p><w:r><w:t>Chapter One</w:t></w:r></w:p></w:body>",
        );
        let Some((parts, _)) =
            expand("notes.docx", &bytes, &file_source("notes.docx"), 0, 0).unwrap()
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
        let Some((parts, _)) =
            expand("tale.epub", &bytes, &file_source("tale.epub"), 0, 0).unwrap()
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
}

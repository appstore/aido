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
//! `xl/workbook.xml` is an OOXML workbook; `word/document.xml` and
//! `ppt/presentation.xml` are Word and PowerPoint files, refused with
//! guidance until loaders exist. Anything else keeps the
//! content-classification path: a random ZIP is "neither valid UTF-8
//! text nor a supported image/audio file", exactly as before this module
//! existed.
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
use std::collections::HashSet;

pub(crate) mod pdf;
pub(crate) mod pdf_render;
pub(crate) mod xlsx;

/// One document expands to at most this many parts — the same ceiling one
/// glob spec has in [`crate::input`].
pub(super) const MAX_PARTS_PER_DOCUMENT: usize = 4096;
/// One document expands to at most this many materialized bytes — one
/// [`crate::input`] single-file cap's worth.
pub(super) const MAX_DOCUMENT_BYTES: usize = 32 * 1024 * 1024;
/// OOXML entries may declare at most this much decompressed content: a
/// deflate bomb inside a ≤32 MB file would otherwise stream gigabytes
/// through the workbook loader.
const MAX_OOXML_DECOMPRESSED: u64 = 512 * 1024 * 1024;
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
    parts: usize,
    notes: Vec<String>,
    /// Every message ever noted, so repeats stay deduped even after the
    /// visible notes hit the cap.
    seen: HashSet<String>,
    notes_dropped: usize,
}

impl Budget {
    pub(super) fn new() -> Self {
        Self {
            bytes: 0,
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
        self.parts += 1;
        if self.parts > MAX_PARTS_PER_DOCUMENT {
            bail!("'{origin}' expands to more than {MAX_PARTS_PER_DOCUMENT} parts; split it into smaller documents");
        }
        self.bytes = self.bytes.saturating_add(size);
        if self.bytes > MAX_DOCUMENT_BYTES {
            bail!(
                "'{origin}' expands to more than {} MB of material; split it into smaller documents",
                MAX_DOCUMENT_BYTES / (1024 * 1024)
            );
        }
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
            if zip_declared_total(bytes) > MAX_OOXML_DECOMPRESSED
                || zip_declared_max(bytes) > MAX_OOXML_DECOMPRESSED
            {
                bail!(
                    "'{origin}' declares more than {} MB of decompressed content; refusing",
                    MAX_OOXML_DECOMPRESSED / (1024 * 1024)
                );
            }
            let parts = xlsx::expand(origin, bytes, source, document, start_id, &mut budget)?;
            return Ok(Some((parts, budget.notes)));
        }
        if zip_lists_entry(bytes, b"word/document.xml") {
            bail!(
                "'{origin}' is a Word document; docx input is not supported yet — \
                 convert it to plain text or markdown first"
            );
        }
        if zip_lists_entry(bytes, b"ppt/presentation.xml") {
            bail!(
                "'{origin}' is a PowerPoint presentation; pptx input is not \
                 supported yet — convert it to images or plain text first"
            );
        }
        // A ZIP that is neither OOXML kind (or whose central directory
        // cannot be parsed) is not ours to judge.
    }
    Ok(None)
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
    mut part: InputPart,
) -> Result<()> {
    part.id = start_id_of(parts);
    budget.admit(origin, size_of(&part.content))?;
    parts.push(part);
    Ok(())
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

/// Walk the ZIP central directory, handing each entry's name and its
/// declared uncompressed size to `visit`. Returns false when the EOCD
/// cannot be found or the directory cannot be parsed — callers treat that
/// as "not ours to judge" and classification describes the bytes instead.
/// ZIP64 is out of scope: inputs are capped at 32 MB, far below the ZIP64
/// threshold; a declared 0xFFFFFFFF (the ZIP64 "unknown" marker) is
/// passed through for the visitor to judge.
fn zip_walk(bytes: &[u8], mut visit: impl FnMut(&[u8], u64)) -> bool {
    const EOCD: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
    const EOCD_LEN: usize = 22;
    if bytes.len() < EOCD_LEN {
        return false;
    }
    // The EOCD sits at the very end unless a ZIP comment (up to 65_535
    // bytes) follows it; scan backwards for the signature.
    let floor = bytes.len().saturating_sub(EOCD_LEN + 65_535);
    let Some(eocd) = (floor..=bytes.len() - EOCD_LEN)
        .rev()
        .find(|&i| bytes[i..].starts_with(&EOCD))
    else {
        return false;
    };
    let u16le = |b: &[u8]| u16::from_le_bytes([b[0], b[1]]);
    let u32le = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let entries = u16le(&bytes[eocd + 10..]);
    // Central directory offset + size, each clamped to the buffer so a
    // corrupt directory degrades to "not found" instead of a panic.
    let cd_offset = (u32le(&bytes[eocd + 16..]) as usize).min(bytes.len());
    let cd_end = cd_offset
        .saturating_add(u32le(&bytes[eocd + 12..]) as usize)
        .min(bytes.len());
    // Walk the file headers: PK\x01\x02, the fixed 42-byte field block,
    // then the name, extra field and comment.
    let mut p = cd_offset;
    for _ in 0..entries {
        if p + 46 > cd_end || !bytes[p..].starts_with(b"PK\x01\x02") {
            return false;
        }
        let name_len = u16le(&bytes[p + 28..]) as usize;
        let extra_len = u16le(&bytes[p + 30..]) as usize;
        let comment_len = u16le(&bytes[p + 32..]) as usize;
        let name_start = p + 46;
        if name_start + name_len > bytes.len() {
            return false;
        }
        visit(
            &bytes[name_start..name_start + name_len],
            u64::from(u32le(&bytes[p + 24..])),
        );
        p = name_start + name_len + extra_len + comment_len;
    }
    true
}

/// The exact-name scan the OOXML detection branch uses.
fn zip_lists_entry(bytes: &[u8], needle: &[u8]) -> bool {
    let mut found = false;
    zip_walk(bytes, |name, _| {
        if name == needle {
            found = true;
        }
    });
    found
}

/// The decompressed size the central directory declares in total.
/// Declared sizes can lie — a header costs nothing to write — so this is
/// a cheap first gate against deflate bombs, not the defense: the
/// loader's per-sheet cell bound is what actually contains one. A
/// declared 0xFFFFFFFF (the ZIP64 "unknown" marker) counts as unknown
/// and stays out of the sum.
fn zip_declared_total(bytes: &[u8]) -> u64 {
    let mut total = 0;
    zip_walk(bytes, |_, declared| {
        if declared != u64::from(u32::MAX) {
            total += declared;
        }
    });
    total
}

/// The largest single entry's declared uncompressed size, unknown markers
/// excluded (see [`zip_declared_total`]).
fn zip_declared_max(bytes: &[u8]) -> u64 {
    let mut max = 0;
    zip_walk(bytes, |_, declared| {
        if declared != u64::from(u32::MAX) {
            max = max.max(declared);
        }
    });
    max
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
    fn docx_is_refused_with_guidance() {
        let bytes = zip_with_entries(&[b"[Content_Types].xml", b"word/document.xml"]);
        let err = expand("notes.docx", &bytes, &file_source("notes.docx"), 0, 0).unwrap_err();
        assert!(
            err.to_string().contains("docx input is not supported"),
            "{err}"
        );
    }

    #[test]
    fn pptx_is_refused_with_guidance() {
        let bytes = zip_with_entries(&[b"[Content_Types].xml", b"ppt/presentation.xml"]);
        let err = expand("slides.pptx", &bytes, &file_source("slides.pptx"), 0, 0).unwrap_err();
        assert!(
            err.to_string().contains("pptx input is not supported"),
            "{err}"
        );
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
    fn zip_scan_survives_corrupt_directories() {
        // Signature only, no room for the fields the scan reads.
        assert!(!zip_lists_entry(b"PK\x05\x06", b"xl/workbook.xml"));
        assert!(!zip_lists_entry(&[], b"xl/workbook.xml"));
        // An EOCD pointing into the void.
        let mut bytes = zip_with_entries(&[b"xl/workbook.xml"]);
        let n = bytes.len();
        bytes[n - 8..].copy_from_slice(&[0xffu8; 8]); // cd_offset/cd_size garbage
        assert!(!zip_lists_entry(&bytes, b"xl/workbook.xml"));
    }

    #[test]
    fn a_workbook_declaring_over_512_mb_is_refused_before_reading() {
        // The gate reads declarations, never entry data: a workbook-shaped
        // zip claiming more than the ceiling is refused even though the
        // fixture's entry holds nothing.
        let bomb = zip_fixture(&[(b"xl/workbook.xml", 600 * 1024 * 1024)], &[]);
        let err = expand("book.xlsx", &bomb, &file_source("book.xlsx"), 0, 0).unwrap_err();
        assert!(
            err.to_string().contains("declares more than 512 MB"),
            "{err}"
        );
        // A normal declaration passes the gate and runs on into the
        // workbook loader, which fails on the fixture's fake content —
        // proof the gate did not swallow it.
        let small = zip_fixture(&[(b"xl/workbook.xml", 1024)], &[]);
        let err = expand("book.xlsx", &small, &file_source("book.xlsx"), 0, 0).unwrap_err();
        assert!(err.to_string().contains("as a workbook"), "{err}");
    }

    #[test]
    fn zip64_unknown_sizes_stay_out_of_the_declared_sums() {
        // 0xFFFFFFFF is the ZIP64 "unknown" marker: neither counted in the
        // total nor judged as an oversized single entry.
        let bytes = zip_fixture(&[(b"a", u32::MAX), (b"b", 10)], &[]);
        assert_eq!(zip_declared_total(&bytes), 10);
        assert_eq!(zip_declared_max(&bytes), 10);
    }

    #[test]
    fn a_zip_comment_after_the_eocd_hides_nothing() {
        // The backwards scan must find the EOCD under a trailing comment;
        // the walker reads the directory behind it normally.
        let bytes = zip_fixture(&[(b"xl/workbook.xml", 7)], b"packed by hand, with feeling");
        assert!(zip_lists_entry(&bytes, b"xl/workbook.xml"));
        assert_eq!(zip_declared_total(&bytes), 7);
        assert_eq!(zip_declared_max(&bytes), 7);
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

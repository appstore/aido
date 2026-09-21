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
pub(crate) mod test_support;
#[cfg(test)]
mod tests;

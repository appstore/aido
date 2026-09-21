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
mod tests;

//! PDF materialization: a document becomes its pages, in page order —
//! each page's embedded raster images (the dominant content of scanned
//! or picture-book PDFs) plus the page's text layer when it has one.
//! A document that is text and nothing else is instead ONE text part
//! for the whole document (pages separated by markers), so per-part
//! text tasks keep cross-page context through the chunk strategies
//! instead of paying page-isolated requests.
//!
//! Embedded images ride through untouched when the PDF already stores
//! them as JPEG (`DCTDecode` — no re-encode, no quality loss); raw or
//! Flate-compressed bitmaps are re-encoded as PNG, with the same
//! decompression-bomb policy the rest of aido applies to images. Images
//! wrapped in Form XObjects (how Office and CAD exports draw pictures)
//! are followed through the form's own resources, a few levels deep, so
//! a wrapped page materializes like a flat one. Codecs aido cannot
//! decode (JPEG 2000, CCITT fax, JBIG2) skip their image rather than
//! fail the run. Text comes from lopdf's extractor with each font's
//! declared encoding (`/ToUnicode` included) and a per-page decompression
//! cap; a page whose text cannot be decoded contributes no text — never
//! garbage — and its image carries the information instead.
//!
//! A subset that ships must never look like the whole document: a skipped
//! image, or a page that yields nothing while its neighbors contribute,
//! becomes a note on the budget's notes channel, which the plan prints to
//! stderr. A wholly blank document stays the extraction path's error to
//! give (or the renderer's job to cover) — no notes there.
//!
//! A PDF that yields neither images nor text is either vector-drawn or
//! a render no tool can see into: with the `pdfium` feature aido renders
//! its pages; without it the run fails naming that build flag.

use super::{append, doc_stem, part, Budget};
use crate::domain::{InputContent, InputPart, InputSource, MediaKind};
use anyhow::{bail, Context, Result};
use lopdf::{Document, Object};
use std::collections::{HashMap, HashSet};

/// Materialized images stay page-sized: bigger declared canvases are
/// refused before any bitmap allocation, mirroring the adapter's decode
/// guard at a tighter bound (a page is never a camera photo).
const MAX_IMAGE_PIXELS: u64 = 64_000_000;
/// Cap for one page's decompressed content stream.
const MAX_CONTENT_BYTES: usize = 16 * 1024 * 1024;
/// The page cap keeps per-page work linear-ish in practice: text
/// extraction re-walks the page tree per page, so an absurd page count is
/// a slow-run vector even with no material to decode. The same ceiling
/// one glob spec has.
const MAX_PAGES: usize = super::MAX_PARTS_PER_DOCUMENT;
/// How deep aido follows Form XObjects: producers that wrap page content
/// in forms nest a level or two in practice, so four is a generous
/// ceiling that still stops hostile recursion.
const MAX_FORM_DEPTH: usize = 4;

pub(super) fn expand(
    origin: &str,
    bytes: &[u8],
    source: &InputSource,
    document: usize,
    start_id: usize,
    budget: &mut Budget,
) -> Result<Vec<InputPart>> {
    // Object and cross-reference streams are decompressed eagerly during
    // loading, before any of the per-page caps below get a say; unbounded,
    // a kilobyte bomb owns the process right there (lopdf's default is no
    // cap). One stream exceeding this is a hostile document, not a book.
    let options = lopdf::LoadOptions {
        max_decompressed_size: Some(MAX_CONTENT_BYTES),
        ..Default::default()
    };
    let doc = Document::load_mem_with_options(bytes, options)
        .with_context(|| format!("cannot open '{origin}' as a PDF (is it password-protected?)"))?;
    let stem = doc_stem(source, origin);
    let pages = doc.get_pages();
    if pages.len() > MAX_PAGES {
        bail!(
            "'{origin}' has {} pages; refusing documents over {MAX_PAGES} pages",
            pages.len()
        );
    }
    let encrypted = doc.trailer.get(b"Encrypt").is_ok();
    let mut parts: Vec<InputPart> = Vec::new();
    // Images skipped per page (page, reason), and pages that ended up
    // with nothing to contribute — the notes channel's raw material.
    let mut skips: Vec<(u32, &'static str)> = Vec::new();
    let mut empty_pages: Vec<u32> = Vec::new();
    // Retained payloads are admitted before extracting the next item.
    // The loaded PDF and one image/page decoder's working memory remain
    // separate from this aggregate extracted-material bound.
    let mut pages_items: Vec<Vec<(MediaKind, &'static str, InputContent)>> =
        Vec::with_capacity(pages.len());
    let mut any_image = false;
    let mut text_pages = 0usize;
    for (page, page_id) in &pages {
        let mut items: Vec<(MediaKind, &'static str, InputContent)> = Vec::new();
        page_images(&doc, *page_id, *page, &mut skips, &mut |bytes, mime| {
            // Until this first supported image, text reserved ONE part.
            // Now all preceding nonempty text pages become final parts.
            let prior_text_parts = if any_image {
                0
            } else {
                text_pages.saturating_sub(1)
            };
            budget.admit_material(origin, bytes.len(), 1 + prior_text_parts)?;
            any_image = true;
            items.push((MediaKind::Image, mime, InputContent::Media(bytes)));
            Ok(())
        })?;
        // One page at a time: the extractor returns text chunks, so only a
        // single-page request maps its result to that page. A page whose
        // text fails (undecodable font, over-limit stream) yields no text
        // — never garbage — and its image carries the information instead.
        if let Ok(text) = doc.extract_text_with_limit(&[*page], MAX_CONTENT_BYTES) {
            if !text.trim().is_empty() {
                budget.admit_material(
                    origin,
                    text.len(),
                    usize::from(any_image || text_pages == 0),
                )?;
                text_pages += 1;
                items.push((MediaKind::Text, "text/plain", InputContent::Text(text)));
            }
        }
        if items.is_empty() {
            empty_pages.push(*page);
        }
        pages_items.push(items);
    }
    let has_text = pages_items
        .iter()
        .any(|items| items.iter().any(|(k, _, _)| *k == MediaKind::Text));
    if any_image {
        for ((page, _), items) in pages.iter().zip(pages_items) {
            // One unit key per page: the page's images and its text layer
            // plan as ONE per-part request together — the text layer rides
            // as context for its page's image, never as a separate
            // "extract the text from the image" request of its own.
            // The document number keeps two specs of the same file
            // separate: `ocr book.pdf book.pdf` is two explicit units,
            // never one.
            let unit = format!("{origin}#{document}#p{page}");
            // One part on a page keeps the bare page name; several get a
            // number, so `story-p3` and `story-p3-2` never fight over one
            // artifact stem in a per-part batch.
            let plural = items.len() > 1;
            for (i, (kind, mime, content)) in items.into_iter().enumerate() {
                let name = if plural {
                    format!("{stem}-p{page}-{}", i + 1)
                } else {
                    format!("{stem}-p{page}")
                };
                append(
                    &mut parts,
                    part(
                        source.clone(),
                        name,
                        kind,
                        mime,
                        content,
                        Some(unit.clone()),
                    ),
                );
            }
        }
    } else if has_text {
        // A document that is text and nothing else is ONE text part, its
        // pages separated by markers — not one part per page. A per-part
        // text task (translate) would otherwise pay a page-isolated
        // request per page with no context across page breaks, exactly
        // what its chunk strategy exists to avoid; merged, the chunker
        // splits at paragraph boundaries and carries context between
        // chunks. One unit for the whole document, like a workbook sheet.
        // Markers exist only in the text-only shape. Charging them earlier
        // could reject a valid mixed PDF whose first image appears late.
        // Admit ALL glue before any concatenation, including separators.
        let mut glue = text_pages.saturating_sub(1) * 2;
        if pages.len() > 1 {
            for ((page, _), items) in pages.iter().zip(&pages_items) {
                if !items.is_empty() {
                    glue += format!("----- page {page} -----\n\n").len();
                }
            }
        }
        budget.admit_material(origin, glue, 0)?;
        let mut text = String::new();
        // Consume rather than borrow the chunks: reuse the first buffer
        // and release subsequent page buffers as they are appended, rather
        // than keeping a second full aggregate alive until the merge ends.
        for ((page, _), items) in pages.iter().zip(pages_items) {
            for (_, _, content) in items {
                if let InputContent::Text(mut t) = content {
                    let marker = if pages.len() > 1 {
                        format!("----- page {page} -----\n\n")
                    } else {
                        String::new()
                    };
                    if text.is_empty() {
                        t.insert_str(0, &marker);
                        text = t;
                    } else {
                        text.reserve_exact(2 + marker.len() + t.len());
                        text.push_str("\n\n");
                        text.push_str(&marker);
                        text.push_str(&t);
                    }
                }
            }
        }
        // The single merged part was budgeted piecewise at extraction;
        // appending it must not charge the whole aggregate a second time.
        append(
            &mut parts,
            part(
                source.clone(),
                stem.clone(),
                MediaKind::Text,
                "text/plain",
                InputContent::Text(text),
                Some(format!("{origin}#{document}")),
            ),
        );
    }
    partial_material_notes(budget, &skips, &empty_pages, pages.len());
    if parts.is_empty() {
        #[cfg(feature = "pdfium")]
        {
            // The renders replaced extraction wholesale: notes about
            // images the renderer covered no longer describe what the
            // user is getting.
            budget.clear_notes();
            // One page at a time, admitted before the next is
            // rasterized: the whole document never sits in memory, so
            // a hostile page count is refused by the budget after the
            // first page past the bound.
            let render = super::pdf_render::render_pages(bytes, |page, png| {
                let name = format!("{stem}-p{page}");
                let built = part(
                    source.clone(),
                    name,
                    MediaKind::Image,
                    "image/png",
                    InputContent::Media(png),
                    Some(format!("{origin}#{document}#p{page}")),
                );
                super::push(budget, &mut parts, origin, built)
            });
            if let Err(render_err) = render {
                // The encryption hint rides on both nothing-came-out
                // paths: here as context for the render failure, and on
                // the no-renderer build as the lead explanation.
                let why = if encrypted {
                    " — the file is encrypted, which hides its content from extraction"
                } else {
                    ""
                };
                bail!(
                    "'{origin}' contains no extractable content and rendering failed: \
                     {render_err}{why}"
                );
            }
        }
        #[cfg(not(feature = "pdfium"))]
        {
            // The likeliest stories behind "nothing came out": vector-drawn
            // pages (no raster to extract, no text layer), images in codecs
            // aido cannot carry, or content an owner password hides. Name
            // them and the way out; a release-binary user cannot rebuild.
            let why = if encrypted {
                "the file is encrypted, and its content is hidden from extraction"
            } else {
                "it is likely drawn as vectors, or its images use codecs aido cannot carry"
            };
            bail!(
                "'{origin}' has no supported images or text: {why} — convert its pages \
                 to images (e.g. `pdftoppm -png '{origin}'`) or use a build made with \
                 `--features pdfium`"
            );
        }
    }
    // push() numbered the parts within the document; shift into the run's
    // contiguous id sequence at the position this document was given.
    for part in &mut parts {
        part.id += start_id;
    }
    Ok(parts)
}

/// Turn the collected skips and empty pages into notes: skips aggregate
/// by reason (a single occurrence names its page, several count
/// themselves), and pages that contributed nothing are named together —
/// but only while other pages did contribute, since a wholly blank
/// document is the extraction error's (or the renderer's) story to tell,
/// not a note. A document that materializes whole, with zero skips,
/// stays silent.
fn partial_material_notes(
    budget: &mut Budget,
    skips: &[(u32, &'static str)],
    empty_pages: &[u32],
    page_count: usize,
) {
    let mut reasons: Vec<(&'static str, Vec<u32>)> = Vec::new();
    for &(page, reason) in skips {
        match reasons.iter_mut().find(|(r, _)| *r == reason) {
            Some((_, pages)) => pages.push(page),
            None => reasons.push((reason, vec![page])),
        }
    }
    for (reason, pages) in &reasons {
        if pages.len() == 1 {
            budget.note(format!("page {}: image skipped ({reason})", pages[0]));
        } else {
            budget.note(format!("{} images skipped ({reason})", pages.len()));
        }
    }
    if !empty_pages.is_empty() && empty_pages.len() < page_count {
        let listed = empty_pages
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let s = if empty_pages.len() == 1 { "" } else { "s" };
        budget.note(format!(
            "page{s} {listed} contributed nothing (no supported images, no text)"
        ));
    }
}

/// A page's images as material bytes, in draw order (`/Do` operands); a
/// content stream that does not parse loses its draw order, so every
/// image XObject gets its chance in resource order instead. A drawn Form
/// XObject's own content stream is parsed for its `/Do` names and
/// descended into through the form's own `/Resources` (forms inherit
/// nothing from the page), so images wrapped in forms materialize like
/// flat ones. An object drawn repeatedly (borders, shadows, a form
/// stamped twice) is materialized once — the dedupe spans the whole page,
/// nested scopes included. Undecodable images contribute nothing but a
/// skip reason — the remaining material still ships.
///
/// Each decoded image is handed to `emit` one at a time instead of
/// returning the page's `Vec` whole, so the caller admits every image to
/// the budget before the next image is decoded — a page's material never
/// accumulates behind the budget's back. An `Err` from `emit` (the
/// budget's refusal) stops the walk and propagates.
fn page_images(
    doc: &Document,
    page_id: lopdf::ObjectId,
    page: u32,
    skips: &mut Vec<(u32, &'static str)>,
    emit: &mut impl FnMut(Vec<u8>, &'static str) -> Result<()>,
) -> Result<()> {
    let xobjects = xobject_streams(doc, page_id, b"XObject");
    let content = doc
        .get_page_content_with_limit(page_id, MAX_CONTENT_BYTES)
        .ok()
        .and_then(|bytes| lopdf::content::Content::decode(&bytes).ok());
    let order = match content {
        Some(parsed) => do_names(&parsed),
        None => xobjects.iter().map(|(name, _, _)| name.clone()).collect(),
    };
    let mut scope = Scope::default();
    collect_scope(doc, &xobjects, &order, 0, &mut scope, emit)?;
    skips.extend(scope.skips.drain(..).map(|reason| (page, reason)));
    Ok(())
}

/// What one page's drawing walk accumulates: per-image skip reasons and
/// the images/forms already visited (an object reached through two
/// scopes — or a form drawn twice — counts once, on first visit). The
/// material itself is not held: each decoded image goes straight to the
/// caller's `emit`, so nothing page-sized piles up behind the budget.
#[derive(Default)]
struct Scope {
    decoded: HashSet<lopdf::ObjectId>,
    seen_forms: HashSet<lopdf::ObjectId>,
    skips: Vec<&'static str>,
}

/// `/Do` operands of one parsed content stream, in draw order.
fn do_names(content: &lopdf::content::Content) -> Vec<Vec<u8>> {
    content
        .operations
        .iter()
        .filter(|op| op.operator == "Do")
        .filter_map(|op| op.operands.first())
        .filter_map(|o| o.as_name().ok())
        .map(<[u8]>::to_vec)
        .collect()
}

/// One drawing scope's XObjects, drawn in `order`, each decoded image
/// handed to `emit` as it is produced; Form XObjects recurse into their
/// own resources down to [`MAX_FORM_DEPTH`] levels. `decoded` is shared
/// across the whole page, so an image reached through two scopes — or a
/// form drawn twice — materializes once. Names with no entry in
/// `xobjects` (a missing or non-stream resource) are skipped, as before.
fn collect_scope(
    doc: &Document,
    xobjects: &[(Vec<u8>, lopdf::ObjectId, &lopdf::Stream)],
    order: &[Vec<u8>],
    depth: usize,
    scope: &mut Scope,
    emit: &mut impl FnMut(Vec<u8>, &'static str) -> Result<()>,
) -> Result<()> {
    let by_name: HashMap<&[u8], usize> = xobjects
        .iter()
        .enumerate()
        .map(|(index, (name, _, _))| (&name[..], index))
        .collect();
    for name in order {
        let Some((id, stream)) = by_name.get(name.as_slice()).map(|&index| {
            let (_, id, stream) = xobjects[index];
            (id, stream)
        }) else {
            continue;
        };
        // A form owns a content stream of its own: follow it through the
        // form's own /Resources, which inherit nothing from the page. A
        // form already expanded is not re-parsed — its images are deduped
        // below anyway, and without this memo a form stamped B times (or
        // nested B deep) multiplies into unbounded parse work from a
        // kilobyte of PDF.
        if is_form(stream) {
            if !scope.seen_forms.insert(id) {
                continue;
            }
            if depth < MAX_FORM_DEPTH {
                let inner = form_xobjects(doc, stream);
                let inner_order = form_draw_order(stream, &inner);
                collect_scope(doc, &inner, &inner_order, depth + 1, scope, emit)?;
            }
            continue;
        }
        if !scope.decoded.insert(id) {
            continue;
        }
        match decode_image(stream, doc) {
            Ok(Some((bytes, mime))) => emit(bytes, mime)?,
            Ok(None) => {} // neither image nor form: not ours to judge
            Err(reason) => scope.skips.push(reason),
        }
    }
    Ok(())
}

/// Whether the XObject is a Form — the wrapper kind whose content stream
/// aido descends into.
fn is_form(stream: &lopdf::Stream) -> bool {
    matches!(stream.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Form")
}

/// The form's own `/Resources /XObject` entries; forms do not inherit the
/// page's resources, so only this dictionary is consulted. References are
/// resolved like every other resource lookup.
fn form_xobjects<'a>(
    doc: &'a Document,
    form: &'a lopdf::Stream,
) -> Vec<(Vec<u8>, lopdf::ObjectId, &'a lopdf::Stream)> {
    let xobjects = form
        .dict
        .get_deref(b"Resources", doc)
        .ok()
        .and_then(|resources| resources.as_dict().ok())
        .and_then(|resources| resources.get_deref(b"XObject", doc).ok());
    let Some(Object::Dictionary(dict)) = xobjects else {
        return Vec::new();
    };
    dict.iter()
        .filter_map(|(name, object)| match doc.dereference(object) {
            Ok((Some(id), Object::Stream(stream))) => Some((name.clone(), id, stream)),
            _ => None,
        })
        .collect()
}

/// The form's content stream as `/Do` names in draw order, decompressed
/// under the same cap a page's content gets; a stream that does not
/// decompress or parse falls back to the form's resource order, the way
/// an unparseable page content does.
fn form_draw_order(
    form: &lopdf::Stream,
    xobjects: &[(Vec<u8>, lopdf::ObjectId, &lopdf::Stream)],
) -> Vec<Vec<u8>> {
    form.decompressed_content_with_limit(MAX_CONTENT_BYTES)
        .ok()
        .and_then(|bytes| lopdf::content::Content::decode(&bytes).ok())
        .map(|parsed| do_names(&parsed))
        .unwrap_or_else(|| xobjects.iter().map(|(name, _, _)| name.clone()).collect())
}

/// One named resource dictionary merged across the page tree, nearest to
/// the page wins: lopdf collects the page's own (referenced) dictionary
/// first and the ancestors after it, so walking them in reverse puts the
/// farthest ancestor down first and the page's own last — a page's
/// `/Resources` replaces what it inherits, per the spec. A page-inline
/// dictionary (rare) still wins over everything. Order is insertion
/// order — part order must not depend on hash iteration.
fn resource_entries<'a>(
    doc: &'a Document,
    page_id: lopdf::ObjectId,
    key: &[u8],
) -> Vec<(Vec<u8>, &'a Object)> {
    let Ok((inline, inherited)) = doc.get_page_resources(page_id) else {
        return Vec::new();
    };
    let mut dicts: Vec<&lopdf::Dictionary> = inherited
        .iter()
        .rev()
        .filter_map(|id| match doc.get_object(*id) {
            Ok(Object::Dictionary(d)) => Some(d),
            _ => None,
        })
        .collect();
    if let Some(d) = inline {
        dicts.push(d);
    }
    let mut entries: Vec<(Vec<u8>, &Object)> = Vec::new();
    for dict in dicts {
        let Ok(resolved) = dict.get_deref(key, doc) else {
            continue;
        };
        let Object::Dictionary(inner) = resolved else {
            continue;
        };
        for (name, object) in inner.iter() {
            match entries.iter_mut().find(|(n, _)| n == name) {
                Some(slot) => slot.1 = object,
                None => entries.push((name.clone(), object)),
            }
        }
    }
    entries
}

/// The scope's stream XObjects keyed for drawing: entries whose object
/// cannot be resolved to an indirect stream (invalid PDF, in practice)
/// are left out, as they always were — an unnamed stream cannot be deduped
/// or decoded safely.
fn xobject_streams<'a>(
    doc: &'a Document,
    page_id: lopdf::ObjectId,
    key: &[u8],
) -> Vec<(Vec<u8>, lopdf::ObjectId, &'a lopdf::Stream)> {
    resource_entries(doc, page_id, key)
        .into_iter()
        .filter_map(|(name, object)| match doc.dereference(object) {
            Ok((Some(id), Object::Stream(stream))) => Some((name, id, stream)),
            _ => None,
        })
        .collect()
}

/// Decode one image XObject into material bytes. `Ok(None)` says "not an
/// image at all" — nothing to judge; `Err(reason)` says the image was
/// skipped because aido cannot carry it, with the reason the notes
/// channel aggregates. A skip is never an error: a page's remaining
/// material still ships.
fn decode_image(
    stream: &lopdf::Stream,
    doc: &Document,
) -> Result<Option<(Vec<u8>, &'static str)>, &'static str> {
    let dict = &stream.dict;
    if !matches!(dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") {
        return Ok(None);
    }
    // A /Decode array remaps component values (inversions, swaps); the
    // byte-level passthrough and plain re-encode cannot honor it.
    if dict.get(b"Decode").is_ok() {
        return Err("a /Decode array is not supported");
    }
    let (Ok(width), Ok(height)) = (
        dict.get(b"Width").and_then(Object::as_i64),
        dict.get(b"Height").and_then(Object::as_i64),
    ) else {
        return Err("the canvas size is missing or invalid");
    };
    if width <= 0 || height <= 0 || width > u32::MAX as i64 || height > u32::MAX as i64 {
        return Err("the canvas size is missing or invalid");
    }
    let (width, height) = (width as u32, height as u32);
    if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        return Err("the canvas exceeds the 64 MP cap");
    }
    let filters = stream.filters().unwrap_or_default();
    match filters.last().copied().unwrap_or(b"") {
        // The scanned-book case: the JPEG is the stream, byte for byte.
        b"DCTDecode" => {
            if filters.len() > 1 {
                // A filter chain ending in DCT (flate-wrapped JPEG) has no
                // partial-decompress path; rare enough to skip.
                return Err("a flate-wrapped JPEG is not supported");
            }
            // Adobe-inverted CMYK JPEGs decode with swapped channels —
            // refuse the colors rather than ship wrong ones.
            if components(dict, doc) == Some(4) {
                return Err("a CMYK JPEG is not supported");
            }
            if !stream.content.starts_with(&[0xff, 0xd8]) {
                return Err("the JPEG stream is malformed");
            }
            Ok(Some((stream.content.clone(), "image/jpeg")))
        }
        b"" | b"FlateDecode" => {
            // Array-form /DecodeParms (per-filter parameters — what many
            // exporters write) is invisible to lopdf's predictor undo,
            // which only reads a dictionary; a predicted image decoded
            // without its undo is plausible-looking noise. Skip it, the
            // way the uncarrable codecs are skipped. Dictionary-form
            // parameters work and stay enabled.
            if has_hidden_predictor(dict, doc) {
                return Err("a /DecodeParms predictor aido cannot undo is present");
            }
            let components = components(dict, doc).ok_or("its color space is not supported")?;
            if dict
                .get(b"BitsPerComponent")
                .and_then(Object::as_i64)
                // The spec gives images no default here; 8 is what files
                // that omit the key turn out to mean in practice, and any
                // packed-width lie still fails the exact size check below.
                .unwrap_or(8)
                != 8
            {
                return Err("only 8 bits per component is supported");
            }
            let expected = width as usize * height as usize * components;
            let raw = if filters.is_empty() {
                stream.content.clone()
            } else {
                // Slack for encoding variance and a predictor's filter
                // byte per row — nothing more; multipliers would hand a
                // hostile stream a large legal allocation.
                stream
                    .decompressed_content_with_limit(
                        expected
                            .saturating_add(height as usize)
                            .saturating_add(65_536),
                    )
                    .map_err(|_| "the stream does not decompress within the cap")?
            };
            if raw.len() < expected {
                return Err("the stream is shorter than its declared size");
            }
            Ok(Some((
                encode_png(width, height, components, &raw[..expected])
                    .map_err(|_| "PNG re-encoding failed")?,
                "image/png",
            )))
        }
        // JPEG 2000, CCITT fax, JBIG2, LZW, run-length: not carried, each
        // under its own name so the notes can say what was lost.
        other => Err(match other {
            b"JPXDecode" => "JPEG 2000 is not supported",
            b"CCITTFaxDecode" => "CCITT fax is not supported",
            b"JBIG2Decode" => "JBIG2 is not supported",
            b"LZWDecode" => "LZW is not supported",
            b"RunLengthDecode" => "a RunLength filter is not supported",
            _ => "its image filter is not supported",
        }),
    }
}

/// Whether an array-form `/DecodeParms` carries a predictor (`>= 2`):
/// per-filter parameters that lopdf's dictionary-form undo cannot see.
fn has_hidden_predictor(dict: &lopdf::Dictionary, doc: &Document) -> bool {
    // lopdf's predictor undo reads only an INLINE dictionary form; an
    // array (per-filter parameters) or an indirect reference hides the
    // predictor from it, and decoding without the undo ships
    // plausible-looking noise. Only an inline dictionary form decodes.
    fn predicted(parms: &lopdf::Dictionary) -> bool {
        matches!(
            parms.get(b"Predictor").and_then(Object::as_i64),
            Ok(predictor) if predictor >= 2
        )
    }
    let Ok(parms) = dict.get(b"DecodeParms") else {
        return false;
    };
    match parms {
        Object::Dictionary(d) => predicted(d),
        Object::Array(items) => items.iter().any(|item| {
            doc.dereference(item)
                .ok()
                .and_then(|(_, object)| object.as_dict().ok())
                .is_some_and(predicted)
        }),
        Object::Reference(_) => doc
            .dereference(parms)
            .ok()
            .and_then(|(_, object)| object.as_dict().ok())
            .is_some_and(predicted),
        _ => false,
    }
}

/// Samples per pixel from the color space, or `None` for the palette and
/// spot-color families aido does not carry.
fn components(dict: &lopdf::Dictionary, doc: &Document) -> Option<usize> {
    let space = dict.get_deref(b"ColorSpace", doc).ok()?;
    match space {
        Object::Name(n) => match &n[..] {
            b"DeviceGray" | b"CalGray" | b"G" => Some(1),
            b"DeviceRGB" | b"CalRGB" | b"RGB" => Some(3),
            b"DeviceCMYK" | b"CMYK" => Some(4),
            _ => None,
        },
        Object::Array(items) => match items.first() {
            // [/ICCBased N-stream]
            Some(Object::Name(n)) if n == b"ICCBased" => items
                .get(1)
                .and_then(|o| doc.dereference(o).ok())
                .and_then(|(_, o)| o.as_stream().ok())
                .and_then(|s| s.dict.get(b"N").and_then(Object::as_i64).ok())
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| matches!(n, 1 | 3 | 4)),
            _ => None,
        },
        _ => None,
    }
}

/// Raw row-major samples as PNG; CMYK inverts to RGB on the way.
fn encode_png(width: u32, height: u32, components: usize, raw: &[u8]) -> Result<Vec<u8>> {
    let image = match components {
        1 => image::DynamicImage::ImageLuma8(
            image::GrayImage::from_raw(width, height, raw.to_vec())
                .context("raw image buffer size mismatch")?,
        ),
        3 => image::DynamicImage::ImageRgb8(
            image::RgbImage::from_raw(width, height, raw.to_vec())
                .context("raw image buffer size mismatch")?,
        ),
        4 => {
            let mut rgb = Vec::with_capacity(raw.len() / 4 * 3);
            for [c, m, y, k] in raw.as_chunks::<4>().0 {
                // R = 255·(1-C)·(1-K), on 0..255 samples.
                rgb.push(((255 - c) as u32 * (255 - k) as u32 / 255) as u8);
                rgb.push(((255 - m) as u32 * (255 - k) as u32 / 255) as u8);
                rgb.push(((255 - y) as u32 * (255 - k) as u32 / 255) as u8);
            }
            image::DynamicImage::ImageRgb8(
                image::RgbImage::from_raw(width, height, rgb)
                    .context("raw image buffer size mismatch")?,
            )
        }
        _ => bail!("unsupported sample count {components}"),
    };
    let mut png = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .context("failed to encode a materialized page image")?;
    Ok(png)
}

#[cfg(test)]
mod tests {
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
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
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
        let (_, notes) =
            expand_with_notes(&book(&[tiny_jpeg(), tiny_jpeg()], false, true)).unwrap();
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
}

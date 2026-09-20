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
mod tests;

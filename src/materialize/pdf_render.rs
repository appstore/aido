//! Page rendering through pdfium, compiled only under the `pdfium`
//! feature. This is the fallback for PDFs whose content is drawn as
//! vectors — nothing aido can extract as images or text — where the only
//! honest option is rasterizing the page exactly as a viewer would.
//!
//! The pdfium C API is linked statically (see the `build` script): there
//! is no runtime library to load, so a feature build fails at link time,
//! never at run time. Only the dozen entry points a rasterization needs
//! are declared; `FPDF_InitLibrary` (the legacy one-argument form) is
//! still exported by every pdfium build in circulation.

#![cfg(feature = "pdfium")]

use anyhow::{bail, Context, Result};
use std::os::raw::{c_char, c_float, c_int, c_void};
use std::sync::Once;

type FpdfDocument = c_void;
type FpdfPage = c_void;
type FpdfBitmap = c_void;

/// Fill with opaque white first, so a blank page renders white, not black.
/// `WHITE` is an 8888-ARGB `FPDF_DWORD`; pdfium's headers type that as
/// `unsigned long` on some ABIs, but the documented value is a 32-bit
/// color, and `u32` zero-extends safely everywhere.
const WHITE: u32 = 0xFFFF_FFFF;
/// Longest side of a rendered page, in pixels: bound both the bitmap and
/// the token cost of the image downstream.
const MAX_SIDE: u32 = 2000;
const DPI: f32 = 150.0;

extern "C" {
    fn FPDF_InitLibrary();
    // `FPDF_DestroyLibrary` is deliberately not declared: tearing the
    // library down and initializing it again within one process segfaults
    // the next render (verified against a real static archive), so it is
    // initialized once, below, and left standing for the life of the
    // process — the way Chromium itself embeds pdfium.
    fn FPDF_LoadMemDocument64(
        data: *const u8,
        size: usize,
        password: *const c_char,
    ) -> *mut FpdfDocument;
    fn FPDF_CloseDocument(document: *mut FpdfDocument);
    fn FPDF_GetPageCount(document: *mut FpdfDocument) -> c_int;
    fn FPDF_LoadPage(document: *mut FpdfDocument, page_index: c_int) -> *mut FpdfPage;
    fn FPDF_ClosePage(page: *mut FpdfPage);
    fn FPDF_GetPageWidthF(page: *mut FpdfPage) -> c_float;
    fn FPDF_GetPageHeightF(page: *mut FpdfPage) -> c_float;
    fn FPDFBitmap_Create(width: c_int, height: c_int, alpha: c_int) -> *mut FpdfBitmap;
    fn FPDFBitmap_FillRect(
        bitmap: *mut FpdfBitmap,
        left: c_int,
        top: c_int,
        width: c_int,
        height: c_int,
        color: u32,
        blend_mode: c_int,
    ) -> c_int;
    fn FPDF_RenderPageBitmap(
        bitmap: *mut FpdfBitmap,
        page: *mut FpdfPage,
        start_x: c_int,
        start_y: c_int,
        size_x: c_int,
        size_y: c_int,
        rotate: c_int,
        flags: c_int,
    );
    fn FPDFBitmap_GetBuffer(bitmap: *mut FpdfBitmap) -> *mut u8;
    fn FPDFBitmap_GetStride(bitmap: *mut FpdfBitmap) -> c_int;
    fn FPDFBitmap_Destroy(bitmap: *mut FpdfBitmap);
}

/// pdfium's global state (font cache, codec registry) must exist for
/// every render but must not be re-created mid-process, so it is set up
/// exactly once, whether the run materializes one PDF or a hundred.
fn init_library() {
    static INIT: Once = Once::new();
    INIT.call_once(|| unsafe { FPDF_InitLibrary() });
}

/// Render every page to PNG bytes, numbered from 1, handing each to
/// `emit` before the next is rasterized — the caller admits each render
/// to its document budget as it arrives, so a hostile page count is
/// refused after the first page past the bound instead of after the
/// whole document has been rendered into memory. A document whose pages
/// cannot be rendered fails the run: the caller has already established
/// there is nothing else to extract, so a partial book would be silent
/// data loss.
pub(super) fn render_pages(
    bytes: &[u8],
    mut emit: impl FnMut(u32, Vec<u8>) -> Result<()>,
) -> Result<()> {
    init_library();
    let document = unsafe { FPDF_LoadMemDocument64(bytes.as_ptr(), bytes.len(), std::ptr::null()) };
    if document.is_null() {
        bail!("pdfium could not open the document");
    }
    let outcome = (|| {
        let count = unsafe { FPDF_GetPageCount(document) };
        if count <= 0 {
            bail!("the document has no pages");
        }
        for index in 0..count {
            emit(index as u32 + 1, render_page(document, index)?)?;
        }
        Ok(())
    })();
    unsafe { FPDF_CloseDocument(document) };
    outcome
}

fn render_page(document: *mut FpdfDocument, index: c_int) -> Result<Vec<u8>> {
    unsafe {
        let page = FPDF_LoadPage(document, index);
        if page.is_null() {
            bail!("pdfium could not load page {}", index + 1);
        }
        let outcome = rasterize(page);
        FPDF_ClosePage(page);
        outcome
    }
}

fn rasterize(page: *mut FpdfPage) -> Result<Vec<u8>> {
    unsafe {
        let (width_pt, height_pt) = (FPDF_GetPageWidthF(page), FPDF_GetPageHeightF(page));
        if !(width_pt > 0.0 && height_pt > 0.0) {
            bail!("pdfium reports a degenerate page size");
        }
        let scale = (DPI / 72.0).min(MAX_SIDE as f32 / width_pt.max(height_pt));
        let (width, height) = (
            (width_pt * scale).round().max(1.0) as c_int,
            (height_pt * scale).round().max(1.0) as c_int,
        );
        let bitmap = FPDFBitmap_Create(width, height, 1);
        if bitmap.is_null() {
            bail!("pdfium could not allocate a {width}×{height} bitmap");
        }
        // FPDFBitmap_Create does not initialize the buffer: the fill must
        // succeed or the page renders over uninitialized memory.
        if FPDFBitmap_FillRect(bitmap, 0, 0, width, height, WHITE, 0) == 0 {
            FPDFBitmap_Destroy(bitmap);
            bail!("pdfium could not clear the page bitmap");
        }
        FPDF_RenderPageBitmap(bitmap, page, 0, 0, width, height, 0, 0);
        let stride = FPDFBitmap_GetStride(bitmap);
        let buffer = FPDFBitmap_GetBuffer(bitmap);
        let outcome = if stride <= 0 || buffer.is_null() {
            bail!("pdfium returned no bitmap buffer");
        } else {
            // BGRA rows (with padding the stride may add) → RGBA bytes.
            let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
            for y in 0..height as usize {
                let row = &*std::ptr::slice_from_raw_parts(
                    buffer.add(y * stride as usize),
                    width as usize * 4,
                );
                for &[b, g, r, a] in row.as_chunks::<4>().0 {
                    rgba.extend_from_slice(&[r, g, b, a]);
                }
            }
            encode(width as u32, height as u32, rgba)
        };
        FPDFBitmap_Destroy(bitmap);
        outcome
    }
}

fn encode(width: u32, height: u32, rgba: Vec<u8>) -> Result<Vec<u8>> {
    let image =
        image::RgbaImage::from_raw(width, height, rgba).context("rendered bitmap size mismatch")?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .context("failed to encode a rendered page")?;
    Ok(png)
}

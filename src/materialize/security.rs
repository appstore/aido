//! Container safety gates shared by the materialize loaders: everything
//! that hands a ZIP-shaped container to a parser passes through here, so
//! a small hostile file cannot buy unbounded decompression anywhere.
//!
//! Two layers, both cheap for honest files. The declared gate reads the
//! central directory's uncompressed sizes — a header costs nothing to
//! write, so it is a first filter, not the defense. The measured passes
//! stream every (or selected) entry through a `take` bound and drop the
//! bytes, so what a loader is about to pay for is measured on the real
//! inflate, whatever the directory claimed. There is no timeout and none
//! is needed for correctness: the passes are CPU-only, the loaders they
//! guard are bounded by these gates and the document budget, and a sync
//! pipeline cannot forcibly abandon a thread anyway.

use anyhow::{bail, Context, Result};
use std::io::{Cursor, Read};
use zip::ZipArchive;

/// OOXML/ODF entries may declare at most this much decompressed content:
/// a deflate bomb inside a ≤32 MB file would otherwise stream gigabytes
/// through a loader.
pub(super) const MAX_OOXML_DECOMPRESSED: u64 = 512 * 1024 * 1024;

/// The declared-size gate a ZIP-based loader runs before opening its
/// parser: one entry, or the directory's whole sum, claiming past the
/// ceiling is refused without reading any entry data.
pub(super) fn gate_declared(origin: &str, bytes: &[u8]) -> Result<()> {
    if zip_declared_total(bytes) > MAX_OOXML_DECOMPRESSED
        || zip_declared_max(bytes) > MAX_OOXML_DECOMPRESSED
    {
        bail!(
            "'{origin}' declares more than {} MB of decompressed content; refusing",
            MAX_OOXML_DECOMPRESSED / (1024 * 1024)
        );
    }
    Ok(())
}

/// One bounded pass over each named entry, individually capped at
/// `ceiling`: a loader that eagerly decompresses specific entries whole
/// (the workbook loader's shared strings, styles and directory) is
/// measured before it can allocate. `container` names the file kind in
/// the open-failure context, so callers keep their own vocabulary
/// ("workbook", "document").
pub(super) fn verify_entries(
    origin: &str,
    bytes: &[u8],
    ceiling: u64,
    entries: &[&str],
    container: &str,
) -> Result<()> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .with_context(|| format!("cannot open '{origin}' as a {container}"))?;
    for name in entries {
        let Ok(mut entry) = archive.by_name(name) else {
            continue;
        };
        if drain_limited(&mut entry, ceiling)? > ceiling {
            bail!(
                "'{origin}': entry '{name}' actually decompresses past {} MB; refusing",
                ceiling / (1024 * 1024)
            );
        }
    }
    Ok(())
}

/// One cumulative bounded pass over EVERY entry: a loader that parses the
/// container with its own reader (the anydoc converter) is measured in
/// full before it runs, so a lying declared size that slips past
/// [`gate_declared`] still cannot buy it more than `ceiling` bytes of
/// real inflate — across all entries together.
pub(super) fn verify_all(origin: &str, bytes: &[u8], ceiling: u64, container: &str) -> Result<()> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .with_context(|| format!("cannot open '{origin}' as a {container}"))?;
    let mut remaining = ceiling;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("cannot read entry {index} of '{origin}'"))?;
        let read = drain_limited(&mut entry, remaining)?;
        if read > remaining {
            bail!(
                "'{origin}': its entries actually decompress past {} MB; refusing",
                ceiling / (1024 * 1024)
            );
        }
        remaining -= read;
    }
    Ok(())
}

/// Read (and discard) up to `limit + 1` bytes, returning how much actually
/// streamed. `take` bounds how much decompression happens at all; the
/// bytes are dropped, so the pass costs CPU only.
fn drain_limited(entry: &mut impl Read, limit: u64) -> Result<u64> {
    let mut limited = entry.take(limit + 1);
    let mut sink = [0u8; 64 * 1024];
    let mut read = 0u64;
    loop {
        let n = limited.read(&mut sink)?;
        if n == 0 {
            return Ok(read);
        }
        read += n as u64;
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

/// The exact-name scan the detection branches use.
pub(super) fn zip_lists_entry(bytes: &[u8], needle: &[u8]) -> bool {
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
/// measured passes above are what actually contains one. A declared
/// 0xFFFFFFFF (the ZIP64 "unknown" marker) counts as unknown and stays
/// out of the sum.
pub(super) fn zip_declared_total(bytes: &[u8]) -> u64 {
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
pub(super) fn zip_declared_max(bytes: &[u8]) -> u64 {
    let mut max = 0;
    zip_walk(bytes, |_, declared| {
        if declared != u64::from(u32::MAX) {
            max = max.max(declared);
        }
    });
    max
}

#[cfg(test)]
mod tests;

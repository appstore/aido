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
mod tests {
    use crate::materialize::test_support::{zip_fixture, zip_with_entries};

    use super::*;

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
    fn the_declared_gate_refuses_past_the_ceiling() {
        let bomb = zip_fixture(&[(b"word/document.xml", 600 * 1024 * 1024)], &[]);
        let err = gate_declared("notes.docx", &bomb).unwrap_err();
        assert!(
            err.to_string().contains("declares more than 512 MB"),
            "{err}"
        );
        // Under the ceiling (and with unknown markers excluded) it passes.
        let fine = zip_fixture(&[(b"word/document.xml", u32::MAX), (b"b", 10)], &[]);
        gate_declared("notes.docx", &fine).unwrap();
    }

    /// A real ZIP whose named entry actually decompresses to at least
    /// `decompresses_to` bytes of zeros while the central directory
    /// declares `declared` — the genuine shape of a lying deflate bomb,
    /// built with zip + the deflate stream doing real work.
    fn lying_zip(name: &str, declared: u32, decompresses_to: usize) -> Vec<u8> {
        use std::io::Write as _;
        let mut entry = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut entry);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file(name, options).unwrap();
            let zeros = vec![0u8; 64 * 1024];
            for _ in 0..(decompresses_to / zeros.len()).max(1) {
                zip.write_all(&zeros).unwrap();
            }
            zip.finish().unwrap();
        }
        let mut bytes = entry.into_inner();
        // Rewrite the central directory's uncompressed-size field for the
        // entry to the lie; the local header keeps the truth (zip reads
        // sizes from the central directory only).
        // EOCD layout: cd_size at +12..16, cd_offset at +16..20.
        let tail = bytes.split_off(bytes.len() - 22);
        let eocd = bytes.len();
        let cd_offset = u32::from_le_bytes([
            bytes[eocd - 6],
            bytes[eocd - 5],
            bytes[eocd - 4],
            bytes[eocd - 3],
        ]) as usize;
        let mut p = cd_offset;
        while p + 46 <= eocd {
            if bytes[p..].starts_with(b"PK\x01\x02") {
                let name_len = u16::from_le_bytes([bytes[p + 28], bytes[p + 29]]) as usize;
                if &bytes[p + 46..p + 46 + name_len] == name.as_bytes() {
                    bytes[p + 24..p + 28].copy_from_slice(&declared.to_le_bytes());
                }
                let extra_len = u16::from_le_bytes([bytes[p + 30], bytes[p + 31]]) as usize;
                let comment_len = u16::from_le_bytes([bytes[p + 32], bytes[p + 33]]) as usize;
                p += 46 + name_len + extra_len + comment_len;
            } else {
                break;
            }
        }
        bytes.extend_from_slice(&tail);
        bytes
    }

    #[test]
    fn a_lying_declared_size_is_refused_by_the_measured_pass() {
        // The directory declares 1 KiB — far under every gate — while the
        // entry really inflates past the small ceiling: the bounded pass
        // must refuse before any loader sees the bytes.
        let bomb = lying_zip("word/document.xml", 1024, 512 * 1024);
        let err = verify_all("bomb.docx", &bomb, 256 * 1024, "document").unwrap_err();
        // The message rounds a sub-megabyte ceiling to "0 MB"; the load-
        // bearing part is the refusal and the file's name.
        assert!(err.to_string().contains("actually decompress"), "{err}");
        assert!(err.to_string().contains("bomb.docx"), "{err}");
    }

    #[test]
    fn the_measured_pass_is_cumulative_across_entries() {
        // Two honest entries, each far under the ceiling alone, together
        // past it: the cumulative bound catches what a per-entry check
        // would miss.
        let bytes = real_zip(&[
            ("a.xml", &vec![0u8; 96 * 1024]),
            ("b.xml", &vec![0u8; 96 * 1024]),
        ]);
        let err = verify_all("sum.docx", &bytes, 128 * 1024, "document").unwrap_err();
        assert!(err.to_string().contains("actually decompress"), "{err}");
        // Both entries together under the ceiling pass.
        verify_all("ok.docx", &bytes, 256 * 1024, "document").unwrap();
    }

    /// A real ZIP (zip crate, deflate) holding the given text entries —
    /// the shape every honest test document takes.
    fn real_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write as _;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (name, content) in entries {
                zip.start_file(name, options).unwrap();
                zip.write_all(content).unwrap();
            }
            zip.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn verify_entries_skips_missing_and_measures_present_entries() {
        // A named entry that is absent is skipped — an honest loader only
        // measures what it will eagerly load. Present entries are measured
        // for real: one inflating past the ceiling is refused with the
        // entry's name, one under it passes.
        let small = real_zip(&[("xl/workbook.xml", b"tiny")]);
        verify_entries(
            "x.xlsx",
            &small,
            1024,
            &["xl/sharedStrings.xml"],
            "workbook",
        )
        .unwrap();
        let bomb = lying_zip("xl/sharedStrings.xml", 1024, 256 * 1024);
        let err = verify_entries(
            "x.xlsx",
            &bomb,
            128 * 1024,
            &["xl/sharedStrings.xml"],
            "workbook",
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("actually decompresses past"), "{message}");
        assert!(message.contains("xl/sharedStrings.xml"), "{message}");
        assert!(message.contains("x.xlsx"), "{message}");
    }
}

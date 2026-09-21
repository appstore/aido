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

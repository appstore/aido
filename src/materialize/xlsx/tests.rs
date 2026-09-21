use super::*;
use rust_xlsxwriter::Workbook;

/// A real (tiny) ZIP whose `xl/sharedStrings.xml` entry actually
/// decompresses to `decompresses_to` bytes, while its central
/// directory declares `declared` (the declaration the detection-time
/// gate reads — a plain lie, or `u32::MAX`, the ZIP64 unknown marker).
/// Built with zip + flate2, so the stream is a genuine deflate bomb's
/// shape: a few bytes that inflate far past what the header claims.
fn lying_workbook(declared: u32, decompresses_to: usize) -> Vec<u8> {
    use std::io::Write as _;
    let mut entry = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut entry);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("xl/workbook.xml", options).unwrap();
        zip.write_all(b"not a real workbook, only the scan matters")
            .unwrap();
        zip.start_file("xl/sharedStrings.xml", options).unwrap();
        // 600 MB of zeros compress to a few kilobytes; the declared
        // size says 1 KiB. The entry is written for real, so the
        // bounded pass measures a genuine inflate, not a header.
        let zeros = vec![0u8; 64 * 1024];
        for _ in 0..(decompresses_to / zeros.len()).max(1) {
            zip.write_all(&zeros).unwrap();
        }
        zip.finish().unwrap();
    }
    let mut bytes = entry.into_inner();
    // Rewrite the central directory's uncompressed-size fields for
    // `xl/sharedStrings.xml` to the lie; the local header keeps the
    // truth (zip reads sizes from the central directory only).
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
            if &bytes[p + 46..p + 46 + name_len] == b"xl/sharedStrings.xml" {
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
fn a_lying_declared_size_is_refused_by_the_actual_decompression() {
    // The central directory declares 1 KiB — far under every gate —
    // while the sharedStrings entry really inflates past the ceiling:
    // the bounded pass must refuse before calamine ever opens it.
    let bomb = lying_workbook(1024, 600 * 1024 * 1024);
    let err = expand(
        "bomb.xlsx",
        &bomb,
        &InputSource::File("bomb.xlsx".into()),
        0,
        0,
        &mut Budget::new(),
    )
    .unwrap_err();
    let message = err.to_string();
    assert!(message.contains("actually decompresses past"), "{message}");
    assert!(message.contains("xl/sharedStrings.xml"), "{message}");
    assert!(message.contains("bomb.xlsx"), "{message}");
}

#[test]
fn a_zip64_unknown_marker_is_refused_by_the_actual_decompression() {
    // 0xFFFFFFFF (ZIP64 unknown) stays out of every declared sum; the
    // real stream still gets measured and refused.
    let bomb = lying_workbook(u32::MAX, 600 * 1024 * 1024);
    let err = expand(
        "bomb.xlsx",
        &bomb,
        &InputSource::File("bomb.xlsx".into()),
        0,
        0,
        &mut Budget::new(),
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("actually decompresses past"),
        "{err}"
    );
}

#[test]
fn an_honest_workbook_passes_the_bounded_pass() {
    // The fixtures the other tests use are real workbooks with real
    // declarations; they must keep materializing unchanged.
    let bytes = fixture(|book| {
        book.add_worksheet()
            .set_name("s")
            .unwrap()
            .write(0, 0, 1)
            .unwrap();
    });
    let parts = expand(
        "ok.xlsx",
        &bytes,
        &InputSource::File("ok.xlsx".into()),
        0,
        0,
        &mut Budget::new(),
    )
    .unwrap();
    assert_eq!(parts.len(), 1);
}

fn fixture(f: impl FnOnce(&mut Workbook)) -> Vec<u8> {
    let mut book = Workbook::new();
    f(&mut book);
    book.save_to_buffer().unwrap()
}

fn expand_bytes(bytes: &[u8]) -> Vec<InputPart> {
    expand(
        "book.xlsx",
        bytes,
        &InputSource::File("book.xlsx".into()),
        0,
        0,
        &mut Budget::new(),
    )
    .unwrap()
}

#[test]
fn each_sheet_becomes_one_markdown_part_in_order() {
    let bytes = fixture(|book| {
        let sheet = book.add_worksheet().set_name("销售").unwrap();
        sheet.write(0, 0, "城市").unwrap();
        sheet.write(0, 1, "销售额").unwrap();
        sheet.write(1, 0, "北京").unwrap();
        sheet.write_string(1, 1, "1,200").unwrap();
        let notes = book.add_worksheet().set_name("notes").unwrap();
        notes.write(0, 0, "备注").unwrap();
        notes.write(0, 1, "无").unwrap();
    });
    let parts = expand_bytes(&bytes);
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].name, "book-销售");
    assert_eq!(parts[1].name, "book-notes");
    assert_eq!(
        parts[0].text(),
        Some("| 城市 | 销售额 |\n| --- | --- |\n| 北京 | 1,200 |\n")
    );
}

#[test]
fn ids_are_contiguous_from_start() {
    let bytes = fixture(|book| {
        book.add_worksheet()
            .set_name("a")
            .unwrap()
            .write(0, 0, 1)
            .unwrap();
        book.add_worksheet()
            .set_name("b")
            .unwrap()
            .write(0, 0, 2)
            .unwrap();
    });
    let parts = expand(
        "x.xlsx",
        &bytes,
        &InputSource::File("x.xlsx".into()),
        0,
        3,
        &mut Budget::new(),
    )
    .unwrap();
    assert_eq!(parts.iter().map(|p| p.id).collect::<Vec<_>>(), [3, 4]);
}

#[test]
fn empty_and_headerless_sheets_are_skipped() {
    let bytes = fixture(|book| {
        book.add_worksheet().set_name("empty").unwrap();
        // A lone far-corner cell is the sheet's whole used grid.
        let sheet = book.add_worksheet().set_name("sparse").unwrap();
        sheet.write(2, 1, "x").unwrap();
        book.add_worksheet()
            .set_name("real")
            .unwrap()
            .write(0, 0, "v")
            .unwrap();
    });
    let parts = expand_bytes(&bytes);
    assert_eq!(
        parts.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        ["book-sparse", "book-real"]
    );
    assert_eq!(parts[0].text(), Some("| x |\n| --- |\n"));
}

#[test]
fn a_spread_out_grid_is_refused_before_any_dense_allocation() {
    // One real cell, but the used grid it implies is enormous — the
    // shape a hostile workbook takes (a cell at the far corner). The
    // coordinates are the Excel maximum, so the writer accepts them.
    let bytes = fixture(|book| {
        let sheet = book.add_worksheet().set_name("bomb").unwrap();
        sheet.write(1_048_575, 16_383, "x").unwrap();
    });
    let err = expand(
        "bomb.xlsx",
        &bytes,
        &InputSource::File("bomb.xlsx".into()),
        0,
        0,
        &mut Budget::new(),
    )
    .unwrap_err();
    let message = err.to_string();
    assert!(message.contains("spans"), "{message}");
    assert!(message.contains("bomb.xlsx"), "{message}");
}

#[test]
fn colliding_sheet_names_get_distinct_parts() {
    let bytes = fixture(|book| {
        book.add_worksheet()
            .set_name("sales.east")
            .unwrap()
            .write(0, 0, 1)
            .unwrap();
        book.add_worksheet()
            .set_name("sales-east")
            .unwrap()
            .write(0, 0, 2)
            .unwrap();
    });
    let parts = expand_bytes(&bytes);
    assert_eq!(
        parts.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        ["book-sales-east", "book-sales-east-2"]
    );
}

#[test]
fn all_empty_sheets_are_an_error_naming_the_file() {
    let bytes = fixture(|book| {
        book.add_worksheet().set_name("empty").unwrap();
    });
    let err = expand(
        "blank.xlsx",
        &bytes,
        &InputSource::File("blank.xlsx".into()),
        0,
        0,
        &mut Budget::new(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("blank.xlsx"), "{err}");
}

#[test]
fn dates_render_as_iso_and_pipes_are_escaped() {
    let bytes = fixture(|book| {
        let sheet = book.add_worksheet().set_name("s").unwrap();
        sheet.write(0, 0, "a|b").unwrap();
        // A date cell in a real workbook always carries a date number
        // format — that format is what tells calamine it is a date.
        let format = rust_xlsxwriter::Format::new().set_num_format("yyyy-mm-dd hh:mm:ss");
        let dt = rust_xlsxwriter::ExcelDateTime::parse_from_str("2024-03-01 08:00:00").unwrap();
        sheet.write_datetime_with_format(1, 0, dt, &format).unwrap();
    });
    let parts = expand_bytes(&bytes);
    let text = parts[0].text().unwrap();
    assert!(text.contains("a\\|b"), "{text}");
    assert!(text.contains("2024-03-01T08:00:00"), "{text}");
}

#[test]
fn numbers_and_bools_survive() {
    let bytes = fixture(|book| {
        let sheet = book.add_worksheet().set_name("s").unwrap();
        sheet.write(0, 0, 1).unwrap();
        sheet.write(0, 1, 2.5).unwrap();
        sheet.write(0, 2, true).unwrap();
    });
    assert_eq!(
        expand_bytes(&bytes)[0].text(),
        Some("| 1 | 2.5 | true |\n| --- | --- | --- |\n")
    );
}

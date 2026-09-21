//! OOXML workbooks (`.xlsx`): every non-empty sheet becomes one text part
//! holding a markdown table, in workbook order. The model reads structure
//! better from a table in text than it ever could from bytes, and text
//! rides the `text` input type every generate task already accepts.
//!
//! Sheets are read through calamine's streaming cells reader with a bound
//! on the used grid: a workbook's cells sit at arbitrary coordinates, and
//! a dense grid over hostile ones (a single cell at the last spreadsheet
//! corner) would allocate gigabytes before any budget could speak. Dates
//! render as `YYYY-MM-DDTHH:MM:SS` (the chrono feature; a cell is only a
//! date to calamine when its number format says so, as real workbooks
//! always do); durations as their ISO form; everything else through the
//! cell value's `Display`. A huge sheet is refused by the same bound or
//! by the document budget — chunked tasks (`summarize`, `translate`)
//! remain the way to work with genuinely large documents.
//!
//! The per-sheet cell bound cannot contain what `Xlsx::new` itself loads:
//! calamine eagerly decompresses `xl/sharedStrings.xml`, `xl/styles.xml`
//! and `xl/workbook.xml` whole at open time, and the zip crate's deflate
//! reader never caps a lying declared size. [`verify_decompression`] pays
//! one bounded pass over those entries first, so a workbook whose entries
//! actually decompress past the ceiling is refused before the loader can
//! allocate.

use super::{doc_stem, part, push, Budget};
use crate::domain::{InputContent, InputPart, InputSource, MediaKind};
use anyhow::{bail, Context, Result};
use calamine::{Cell, Data, DataRef, Range, Reader, Xlsx};
use std::collections::HashSet;
use std::io::Cursor;

/// A sheet's used grid is refused beyond this many cells: four million is
/// far above any sheet meant for reading, and bounds the dense range (and
/// the markdown built from it) to a working-set size.
const MAX_SHEET_CELLS: u64 = 4_000_000;

/// The decompressed size one eagerly-loaded workbook entry may actually
/// reach. The detection-time gate (`crate::materialize::expand`) judges
/// declared sizes, which cost nothing to write; this bound is measured on
/// the real stream. Shared strings are the entry a hostile workbook hides
/// its bomb in — every distinct string is loaded before any sheet is read.
const MAX_LOADED_ENTRY_BYTES: u64 = super::security::MAX_OOXML_DECOMPRESSED;

/// The entries calamine decompresses whole while opening the workbook.
const EAGER_ENTRIES: [&str; 3] = ["xl/sharedStrings.xml", "xl/styles.xml", "xl/workbook.xml"];

/// The per-sheet cell bound cannot contain what `Xlsx::new` itself loads:
/// calamine eagerly decompresses `xl/sharedStrings.xml`, `xl/styles.xml`
/// and `xl/workbook.xml` whole at open time, and the zip crate's deflate
/// reader never caps a lying declared size. [`super::security::verify_entries`]
/// pays one bounded pass over those entries first — each streamed through
/// a `take` bound, the bytes dropped — so a workbook whose entries
/// actually decompress past the ceiling is refused before the loader can
/// allocate.
fn verify_decompression(origin: &str, bytes: &[u8]) -> Result<()> {
    super::security::verify_entries(
        origin,
        bytes,
        MAX_LOADED_ENTRY_BYTES,
        &EAGER_ENTRIES,
        "workbook",
    )
}

pub(super) fn expand(
    origin: &str,
    bytes: &[u8],
    source: &InputSource,
    document: usize,
    start_id: usize,
    budget: &mut Budget,
) -> Result<Vec<InputPart>> {
    verify_decompression(origin, bytes)?;
    let mut workbook = Xlsx::new(Cursor::new(bytes))
        .with_context(|| format!("cannot open '{origin}' as a workbook"))?;
    let stem = doc_stem(source, origin);
    let mut parts: Vec<InputPart> = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    for sheet in workbook.sheet_names() {
        let range = bounded_range(&mut workbook, &sheet, origin)?;
        let Some(markdown) = sheet_markdown(&range) else {
            continue;
        };
        // Distinct sheet names can sanitize to the same artifact stem
        // ("sales.east" and "sales-east"); number the repeats so a
        // per-part batch never writes two sheets to one artifact.
        let mut name = format!("{stem}-{}", crate::output::sanitize_stem(&sheet));
        let mut n = 2;
        while !used.insert(name.clone()) {
            name = format!("{stem}-{}-{n}", crate::output::sanitize_stem(&sheet));
            n += 1;
        }
        push(
            budget,
            &mut parts,
            origin,
            part(
                source.clone(),
                name,
                MediaKind::Text,
                "text/markdown",
                InputContent::Text(markdown),
                // A single-part unit: grouping is a no-op, but the key
                // stays truthful (the raw sheet name keeps distinct sheets
                // distinct even where their sanitized names collide, and
                // the document number keeps two specs of one file apart).
                Some(format!("{origin}#{document}#{sheet}")),
            ),
        )?;
    }
    if parts.is_empty() {
        bail!("'{origin}' has no non-empty sheets; nothing to send");
    }
    for part in &mut parts {
        part.id += start_id;
    }
    Ok(parts)
}

/// One sheet's cells, read in stream order with the used grid measured
/// from the cells that really exist: a dense range over hostile
/// coordinates (a lone cell at the last spreadsheet corner) would abort
/// the process on allocation before any budget could speak. The reader's
/// own declared dimensions are not trusted for the same reason. The cell
/// count itself is bounded in the loop: a sheet of millions of
/// same-position cells would otherwise pile up in the vector before the
/// span check ever ran.
fn bounded_range(
    workbook: &mut Xlsx<Cursor<&[u8]>>,
    sheet: &str,
    origin: &str,
) -> Result<Range<Data>> {
    let mut reader = workbook
        .worksheet_cells_reader(sheet)
        .with_context(|| format!("cannot read sheet '{sheet}' of '{origin}'"))?;
    let mut cells: Vec<Cell<DataRef>> = Vec::new();
    let mut end = (0u32, 0u32);
    while let Some(cell) = reader
        .next_cell()
        .with_context(|| format!("cannot read sheet '{sheet}' of '{origin}'"))?
    {
        let (row, col) = cell.get_position();
        end = (end.0.max(row), end.1.max(col));
        cells.push(cell);
        if cells.len() > MAX_SHEET_CELLS as usize {
            bail!(
                "sheet '{sheet}' of '{origin}' holds more than {MAX_SHEET_CELLS} cells; \
                 refusing sheets over {MAX_SHEET_CELLS} (they are fully loaded into memory)"
            );
        }
    }
    let span = u64::from(end.0).saturating_add(1) * u64::from(end.1).saturating_add(1);
    if span > MAX_SHEET_CELLS {
        bail!(
            "sheet '{sheet}' of '{origin}' spans {span} cells; refusing sheets over \
             {MAX_SHEET_CELLS} (they are fully loaded into memory)"
        );
    }
    Ok(Range::from_sparse(
        cells
            .into_iter()
            .map(|cell| {
                let (row, col) = cell.get_position();
                Cell::new((row, col), Data::from(cell.get_value().clone()))
            })
            .collect(),
    ))
}

/// The sheet as a markdown table, or `None` when no cell holds a value.
/// The first surviving row becomes the header row (markdown requires a
/// separator under it); trailing empty cells are trimmed per row and rows
/// that trim to nothing are dropped.
fn sheet_markdown(range: &Range<Data>) -> Option<String> {
    let rows: Vec<Vec<String>> = range
        .rows()
        .map(|row| {
            let mut cells: Vec<String> = row.iter().map(cell_text).collect();
            while cells.last().is_some_and(String::is_empty) {
                cells.pop();
            }
            cells
        })
        .filter(|row| !row.is_empty())
        .collect();
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    if width == 0 {
        return None;
    }
    let mut out = String::new();
    for (i, row) in rows.iter().enumerate() {
        out.push('|');
        for cell in row {
            out.push(' ');
            out.push_str(cell);
            out.push_str(" |");
        }
        for _ in row.len()..width {
            out.push_str(" |");
        }
        out.push('\n');
        if i == 0 {
            out.push('|');
            for _ in 0..width {
                out.push_str(" --- |");
            }
            out.push('\n');
        }
    }
    Some(out)
}

/// One cell's display text; `|` and newlines are escaped so a cell value
/// can never break the table's structure.
fn cell_text(cell: &Data) -> String {
    let text = match cell {
        Data::Empty => String::new(),
        Data::DateTime(dt) => match dt.as_datetime() {
            Some(datetime) => datetime.format("%Y-%m-%dT%H:%M:%S").to_string(),
            None => match dt.as_duration() {
                Some(duration) => duration.to_string(),
                None => dt.to_string(),
            },
        },
        other => other.to_string(),
    };
    text.replace('|', "\\|").replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests;

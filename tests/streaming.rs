//! Streaming reads, and where they do something the dense path cannot.
//!
//! Two properties are under test. The streamed rows must agree with the dense range cell for cell,
//! since the two are alternative views of the same file. And a sheet whose cells are far apart —
//! which the dense path refuses as an implausible allocation — must read in constant space.

mod common;

use cellar::{open_workbook, Data, Error, Reader, Sheets};
use std::io::{BufReader, Write};
use std::path::PathBuf;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join(name)
}

/// Every cell a sheet holds, as `(row, column, value)`, in row-major order.
fn streamed(workbook: &mut Sheets<BufReader<std::fs::File>>, name: &str) -> Vec<(u32, u32, Data)> {
    let mut rows = workbook.worksheet_rows(name).unwrap();
    let mut cells = Vec::new();
    while let Some(row) = rows.next_row().unwrap() {
        for (column, value) in row.iter() {
            cells.push((row.index(), column, value.clone()));
        }
    }
    cells
}

/// The same cells read from the dense range.
fn dense(workbook: &mut Sheets<BufReader<std::fs::File>>, name: &str) -> Vec<(u32, u32, Data)> {
    workbook
        .worksheet_range(name)
        .unwrap()
        .cells()
        .filter(|(_, _, value)| !value.is_empty())
        .map(|(row, column, value)| (row, column, value.clone()))
        .collect()
}

/// The two views are alternative shapes for the same data, so they must not disagree.
#[test]
fn streaming_agrees_with_the_dense_range() {
    let mut workbook = open_workbook(fixture("chanora.xlsx")).unwrap();
    let names = workbook.sheet_names().to_vec();
    assert!(names.len() > 1);

    for name in names {
        let from_rows = streamed(&mut workbook, &name);
        let from_range = dense(&mut workbook, &name);
        assert_eq!(
            from_rows.len(),
            from_range.len(),
            "{name}: different number of populated cells"
        );
        assert_eq!(from_rows, from_range, "{name}: cells differ");
    }
}

#[test]
fn streaming_agrees_on_ods_too() {
    let mut workbook = open_workbook(fixture("sample.ods")).unwrap();
    for name in workbook.sheet_names().to_vec() {
        assert_eq!(
            streamed(&mut workbook, &name),
            dense(&mut workbook, &name),
            "{name}"
        );
    }
}

/// A row the file declares with no values is still a row, and carries its own index.
#[test]
fn blank_rows_are_yielded_with_their_index() {
    let path = crate::common::scratch_file("cellar-blank-row.xlsx");
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData>
<row r="1"><c r="A1"><v>1</v></c></row>
<row r="2"/>
<row r="3"><c r="B3"><v>2</v></c></row>
</sheetData>
</worksheet>"#;
    let workbook = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheets><sheet name="S" sheetId="1"/></sheets>
</workbook>"#;
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    for (name, body) in [
        ("xl/workbook.xml", workbook),
        ("xl/worksheets/sheet1.xml", sheet),
    ] {
        writer.start_file(name, options).unwrap();
        writer.write_all(body.as_bytes()).unwrap();
    }
    writer.finish().unwrap();

    let mut workbook = open_workbook(&path).unwrap();
    let mut rows = workbook.worksheet_rows("S").unwrap();

    let first = rows.next_row().unwrap().unwrap();
    assert_eq!((first.index(), first.get(0)), (0, Some(&Data::Int(1))));

    let second = rows.next_row().unwrap().unwrap();
    assert_eq!(second.index(), 1);
    assert!(second.is_empty());

    let third = rows.next_row().unwrap().unwrap();
    assert_eq!((third.index(), third.get(1)), (2, Some(&Data::Int(2))));
    assert!(rows.next_row().unwrap().is_none());

    crate::common::cleanup(&path);
}

/// A row holding one value far to the right costs one entry, not one per skipped column.
#[test]
fn a_row_costs_its_values_not_its_width() {
    let mut workbook = open_workbook(fixture("chanora.xlsx")).unwrap();
    let mut rows = workbook.worksheet_rows("Identifications").unwrap();
    let row = rows.next_row().unwrap().unwrap();

    assert_eq!(row.len(), row.iter().count());
    assert!(
        row.len() <= 21,
        "a 21-column sheet cannot have a row of {} entries",
        row.len()
    );
}

/// Writes a workbook whose two cells sit at opposite corners of the grid.
///
/// The rectangle between them is seventeen billion cells: a dense range would have to allocate all
/// of it, and the reader refuses rather than trying. Streaming has no such obligation.
fn write_sparse_workbook(path: &std::path::Path) {
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<dimension ref="A1:XFD1048576"/>
<sheetData>
<row r="1"><c r="A1"><v>1</v></c></row>
<row r="1048576"><c r="XFD1048576"><v>2</v></c></row>
</sheetData>
</worksheet>"#;
    let workbook = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheets><sheet name="S" sheetId="1"/></sheets>
</workbook>"#;

    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let mut writer = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    for (name, body) in [
        ("xl/workbook.xml", workbook),
        ("xl/worksheets/sheet1.xml", sheet),
    ] {
        writer.start_file(name, options).unwrap();
        writer.write_all(body.as_bytes()).unwrap();
    }
    writer.finish().unwrap();
}

/// The point of the streaming API: reading a sheet the dense path cannot represent.
#[test]
fn a_sheet_too_large_to_materialise_still_streams() {
    let path = crate::common::scratch_file("cellar-sparse-streaming.xlsx");
    write_sparse_workbook(&path);

    let mut workbook = open_workbook(&path).unwrap();

    // The dense path refuses, because the rectangle between those two cells is 17,179,869,184
    // cells and it would have to hold every one of them.
    let dense_error = workbook.worksheet_range("S").unwrap_err();
    assert!(
        matches!(dense_error, Error::Format(_)),
        "expected the dense path to refuse, got {dense_error}"
    );

    // Streaming reads both values without allocating the rectangle, so memory is a function of
    // the row, not of how far apart the cells are.
    let cells = streamed(&mut workbook, "S");
    assert_eq!(
        cells,
        vec![(0, 0, Data::Int(1)), (1_048_575, 16_383, Data::Int(2)),]
    );

    crate::common::cleanup(&path);
}

/// The header setting moves the dense range's start, so it has to move the stream's too.
#[test]
fn streaming_honours_the_header_row() {
    let mut workbook = open_workbook(fixture("chanora.xlsx")).unwrap();

    let first = streamed(&mut workbook, "Summary")[0].clone();
    assert_eq!(first.2, Data::String("License".into()));

    workbook.with_header_row(cellar::HeaderRow::Row(2));
    let shifted = streamed(&mut workbook, "Summary");
    assert_eq!(
        shifted[0].2,
        Data::String("MIT License".into()),
        "streaming should start where the dense path starts"
    );
    assert!(shifted.len() < 30);
}

/// Reading the same sheet twice in a row works: the cursor holds the decompressed part, and the
/// next stream replaces it rather than colliding with it.
#[test]
fn a_sheet_can_be_streamed_again() {
    let mut workbook = open_workbook(fixture("sample.ods")).unwrap();
    let first = streamed(&mut workbook, "Sheet1");
    let second = streamed(&mut workbook, "Sheet1");
    assert_eq!(first, second);
    assert!(!first.is_empty());
}

/// A window's shape follows its reference, not the values inside it.
#[test]
fn a_window_has_the_shape_that_was_asked_for() {
    let mut workbook = open_workbook(fixture("chanora.xlsx")).unwrap();
    let window = workbook.worksheet_range_ref("Summary", "B2:D4").unwrap();

    assert_eq!(window.start(), (1, 1));
    assert_eq!(window.end(), (4, 4));
    assert_eq!(window.get_size(), (3, 3));

    // And it agrees with the dense view of the same region.
    let full = workbook.worksheet_range("Summary").unwrap();
    for (row, column) in [(1, 1), (1, 2), (3, 3)] {
        assert_eq!(
            window.get((row, column)).cloned().unwrap_or(Data::Empty),
            full.get((row, column)).cloned().unwrap_or(Data::Empty),
            "cell ({row}, {column})"
        );
    }
}

/// A window must agree with the same region of the dense range, including where it holds nothing.
#[test]
fn a_window_agrees_with_the_dense_range() {
    let mut workbook = open_workbook(fixture("chanora.xlsx")).unwrap();
    let window = workbook
        .worksheet_range_ref("Identifications", "B2:F40")
        .unwrap();
    let full = workbook.worksheet_range("Identifications").unwrap();

    assert_eq!(window.get_size(), (39, 5));
    for row in window.start().0..window.end().0 {
        for column in window.start().1..window.end().1 {
            assert_eq!(
                window.get((row, column)).cloned().unwrap_or(Data::Empty),
                full.get((row, column)).cloned().unwrap_or(Data::Empty),
                "cell ({row}, {column}) differs from the dense range"
            );
        }
    }
}

/// Writes a one-sheet workbook whose sheet part is `body`, compressed as asked for.
///
/// Real workbooks are deflated, so tests that care about stopping partway through a part have to
/// say so: on a stored part there is nothing to stop.
fn write_sheet(path: &std::path::Path, body: &str, method: zip::CompressionMethod) {
    let workbook = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheets><sheet name="S" sheetId="1"/></sheets>
</workbook>"#;

    let options = zip::write::SimpleFileOptions::default().compression_method(method);
    let mut writer = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    for (name, text) in [
        ("xl/workbook.xml", workbook),
        ("xl/worksheets/sheet1.xml", body),
    ] {
        writer.start_file(name, options).unwrap();
        writer.write_all(text.as_bytes()).unwrap();
    }
    writer.finish().unwrap();
}

/// A part whose rows do not follow the format's ordering must not be cut short.
///
/// A window read stops once more rows have closed than the window's last row, which is sound while
/// rows carry ascending, distinct indexes. This part repeats rows 1 to 98, so the closes pile up
/// while the row the window asks for is still to come; trusting the count alone would drop it. The
/// cut is checked against the parser, and a part that disagrees is taken whole.
#[test]
fn a_part_listing_its_rows_out_of_order_is_not_cut_short() {
    let path = crate::common::scratch_file("cellar-repeated-rows.xlsx");
    let mut body = String::from(
        r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<dimension ref="A1:A99"/>
<sheetData>"#,
    );
    for _ in 0..2 {
        for row in 1..=98 {
            body.push_str(&format!(
                r#"<row r="{row}"><c r="A{row}"><v>{row}</v></c></row>"#
            ));
        }
    }
    body.push_str(r#"<row r="99"><c r="A99"><v>99</v></c></row>"#);
    body.push_str("</sheetData></worksheet>");
    write_sheet(&path, &body, zip::CompressionMethod::Stored);

    let mut workbook = open_workbook(&path).unwrap();
    let window = workbook.worksheet_range_ref("S", "A99").unwrap();
    crate::common::cleanup(&path);

    // A window is anchored where it was asked for, so the cell it holds is at row 98, not row 0.
    assert_eq!(window.start(), (98, 0));
    assert_eq!(window.get((98, 0)), Some(&Data::Int(99)));
}

/// Text that looks like the end of a row must not be mistaken for one.
///
/// The stop counts occurrences of `</row>` in the bytes, which a comment can contain. The count
/// alone would then cut the part in the middle of that comment, losing every row after it ??so the
/// cut is confirmed against the parser, which knows a comment when it reads one.
#[test]
fn a_comment_that_looks_like_a_row_close_does_not_truncate_the_sheet() {
    let path = crate::common::scratch_file("cellar-comment-rows.xlsx");
    let mut body = String::from(
        r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<dimension ref="A1:A10"/>
<sheetData>"#,
    );
    for row in 1..=5 {
        body.push_str(&format!(
            r#"<row r="{row}"><c r="A{row}"><v>{row}</v></c></row>"#
        ));
    }
    // Six of them, so the count passes row 9's worth while still inside the comment.
    body.push_str("<!-- ");
    for _ in 0..6 {
        body.push_str("</row> ");
    }
    body.push_str("-->");
    for row in 6..=10 {
        body.push_str(&format!(
            r#"<row r="{row}"><c r="A{row}"><v>{row}</v></c></row>"#
        ));
    }
    body.push_str("</sheetData></worksheet>");
    write_sheet(&path, &body, zip::CompressionMethod::Stored);

    let mut workbook = open_workbook(&path).unwrap();
    let window = workbook.worksheet_range_ref("S", "A10").unwrap();
    let whole = workbook.worksheet_range("S").unwrap();
    crate::common::cleanup(&path);

    assert_eq!(window.get((9, 0)), Some(&Data::Int(10)));
    assert_eq!(whole.get((9, 0)), Some(&Data::Int(10)));
}

/// Rows that close themselves produce no close tag, so the stop can never fire. The read must then
/// simply take the part whole and still be right.
#[test]
fn a_sheet_of_self_closing_rows_still_reads_its_window() {
    let path = crate::common::scratch_file("cellar-self-closing-rows.xlsx");
    let body = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<dimension ref="A1:A5"/>
<sheetData>
<row r="1"/><row r="2"/><row r="3"><c r="A3"><v>3</v></c></row><row r="4"/>
<row r="5"><c r="A5"><v>5</v></c></row>
</sheetData></worksheet>"#;
    write_sheet(&path, body, zip::CompressionMethod::Stored);

    let mut workbook = open_workbook(&path).unwrap();
    let window = workbook.worksheet_range_ref("S", "A5").unwrap();
    let whole = workbook.worksheet_range("S").unwrap();
    let rows = streamed(&mut workbook, "S");
    crate::common::cleanup(&path);

    assert_eq!(window.get((4, 0)), Some(&Data::Int(5)));
    assert_eq!(whole.get((4, 0)), Some(&Data::Int(5)));
    // The empty rows are still rows, and the ones with values land where they belong.
    assert_eq!(
        rows,
        vec![(2, 0, Data::Int(3)), (4, 0, Data::Int(5))],
        "self-closing rows should be skipped over, not collapsed"
    );
}

/// Stopping partway through a compressed part must not damage the container, and a window deep in
/// the sheet must still be right.
///
/// The part here deflates, as real workbooks do, and its text is well past one chunk, so the stop
/// is a real mid-stream stop rather than a whole-part read.
#[test]
fn a_deflated_part_stops_early_and_can_be_read_again() {
    const ROWS: u32 = 300;
    const COLS: u32 = 20;
    let path = crate::common::scratch_file("cellar-deflated-window.xlsx");

    let mut body = String::from(
        r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<dimension ref="A1:T300"/>
<sheetData>"#,
    );
    for row in 1..=ROWS {
        body.push_str(&format!(r#"<row r="{row}">"#));
        for col in 0..COLS {
            body.push_str(&format!(
                r#"<c r="{}{row}"><v>{}</v></c>"#,
                cellar::refs::col_to_letters(col),
                row * COLS + col
            ));
        }
        body.push_str("</row>");
    }
    body.push_str("</sheetData></worksheet>");
    write_sheet(&path, &body, zip::CompressionMethod::Deflated);

    let mut workbook = open_workbook(&path).unwrap();
    // The first read stops mid-stream. The second has to reopen the part and do it again.
    let top = workbook.worksheet_range_ref("S", "A1").unwrap();
    assert_eq!(top.get((0, 0)), Some(&Data::Int(COLS as i64)));

    let deep = workbook.worksheet_range_ref("S", "F250:G251").unwrap();
    let whole = workbook.worksheet_range("S").unwrap();
    crate::common::cleanup(&path);

    assert_eq!(deep.start(), (249, 5));
    assert_eq!(deep.get((249, 5)), Some(&Data::Int(250 * COLS as i64 + 5)));
    assert_eq!(whole.get((249, 5)), Some(&Data::Int(250 * COLS as i64 + 5)));
}

/// Corner order in the reference does not matter.
#[test]
fn a_window_accepts_its_corners_either_way_round() {
    let mut workbook = open_workbook(fixture("sample.ods")).unwrap();
    let forward = workbook.worksheet_range_ref("Sheet1", "A1:C3").unwrap();
    let backward = workbook.worksheet_range_ref("Sheet1", "C3:A1").unwrap();
    assert_eq!(forward, backward);
    assert_eq!(forward.get_size(), (3, 3));
}

/// A reference that is not a range is rejected rather than guessed at.
#[test]
fn a_bad_window_reference_is_rejected() {
    let mut workbook = open_workbook(fixture("sample.ods")).unwrap();
    for reference in ["", "garbage", "1:2", "A", "A0:A1", "A1:B"] {
        assert!(
            matches!(
                workbook.worksheet_range_ref("Sheet1", reference),
                Err(Error::Format(_))
            ),
            "{reference:?} should have been rejected"
        );
    }
}

/// A window is the answer for a sheet too large to materialise: it reads the region asked for and
/// leaves the rest alone.
#[test]
fn a_window_reads_from_a_sheet_that_cannot_be_materialised() {
    let path = crate::common::scratch_file("cellar-window-sparse.xlsx");
    write_sparse_workbook(&path);

    let mut workbook = open_workbook(&path).unwrap();
    assert!(workbook.worksheet_range("S").is_err());

    // The far corner, reached by asking for exactly the cell it sits in.
    let corner = workbook.worksheet_range_ref("S", "XFD1048576").unwrap();
    assert_eq!(corner.get_size(), (1, 1));
    assert_eq!(corner.get((1_048_575, 16_383)), Some(&Data::Int(2)));

    // A window holding nothing still has the shape that was asked for.
    let empty = workbook.worksheet_range_ref("S", "B2:C3").unwrap();
    assert_eq!(empty.get_size(), (2, 2));
    assert_eq!(empty.used_cell_count(), 0);

    crate::common::cleanup(&path);
}

/// A window names exact rows, so the header setting must not move it.
#[test]
fn the_header_setting_does_not_move_a_window() {
    let mut workbook = open_workbook(fixture("chanora.xlsx")).unwrap();
    let plain = workbook.worksheet_range_ref("Summary", "A1:B2").unwrap();

    workbook.with_header_row(cellar::HeaderRow::Row(3));
    let shifted = workbook.worksheet_range_ref("Summary", "A1:B2").unwrap();
    assert_eq!(plain, shifted);
}

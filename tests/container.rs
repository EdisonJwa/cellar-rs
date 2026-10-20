//! The archive backend's contract, as the format readers rely on it.
//!
//! The container is the only part of the read path that is a dependency rather than this
//! crate's own code, so its behaviour is pinned here: the error class a caller sees for a
//! damaged archive, the tolerance for part names spelled differently by different producers,
//! and the fact that detection still works on the real fixtures.

use cellar::{Data, Error, Format, Reader};

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name)
}

/// The committed `sample.ods` was zipped on Windows and stores its entries with backslash
/// separators, which the ZIP specification does not allow — it requires `/`.
///
/// It is kept as-is because it is a genuine example of what tools emit, and because the reader
/// tolerates it: names are normalised before lookup, so the parts are still found. This test is
/// what stops that tolerance from being dropped as "unnecessary" during a refactor.
#[test]
fn a_windows_zipped_archive_is_still_readable() {
    let bytes = std::fs::read(fixture("sample.ods")).unwrap();
    // Confirm the fixture really is the non-conforming kind, or the test proves nothing.
    let archive = zipcore::Archive::new(std::io::Cursor::new(bytes.clone())).unwrap();
    let has_backslash = archive.entries().iter().any(|e| e.name().contains('\\'));
    assert!(
        has_backslash,
        "the fixture no longer has backslash separators; this test needs a different one"
    );

    let mut workbook = cellar::open_ods_from_rs(std::io::Cursor::new(bytes)).unwrap();
    let range = workbook.worksheet_range("Sheet1").unwrap();
    assert!(
        range.used_cell_count() > 0,
        "the parts should still be found despite the separators"
    );
    assert_eq!(range.get((0, 0)), Some(&Data::String("Name".into())));
}

/// A workbook package with no relationship metadata still reads.
///
/// `_rels/.rels` and matching relationship ids are how a strict reader navigates the package.
/// This crate resolves the conventional part path directly, so a package that omits them still
/// opens — which is worth pinning, since it is the kind of tolerance that gets "simplified" away.
#[test]
fn a_package_without_relationship_metadata_still_reads() {
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData><row r="1"><c r="A1"><v>11</v></c></row></sheetData>
</worksheet>"#;
    // No `_rels/.rels`, no `xl/_rels/workbook.xml.rels`, and no `r:id` on the sheet.
    let workbook = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheets><sheet name="S" sheetId="1"/></sheets>
</workbook>"#;
    let stored = zipcore::FileOptions::stored();
    let mut w = zipcore::ZipWriter::new(std::io::Cursor::new(Vec::new())).unwrap();
    w.add_file("xl/workbook.xml", workbook.as_bytes(), &stored)
        .unwrap();
    w.add_file("xl/worksheets/sheet1.xml", sheet.as_bytes(), &stored)
        .unwrap();
    let bytes = w.finish().unwrap().into_inner();

    let mut wb = cellar::open_xlsx_from_rs(std::io::Cursor::new(bytes)).unwrap();
    assert_eq!(
        wb.worksheet_range("S").unwrap().get((0, 0)),
        Some(&Data::Int(11))
    );
}

/// Bytes that start like a ZIP but carry no directory must be reported as a damaged container
/// rather than misread or panicked on.
#[test]
fn a_garbage_archive_is_a_container_error() {
    let junk = b"PK\x03\x04 not really a zip archive at all".to_vec();
    match cellar::open_xlsx_from_rs(std::io::Cursor::new(junk)) {
        Err(Error::Container(_)) => {}
        Err(other) => panic!("expected a container error, got {other:?}"),
        Ok(_) => panic!("a header with no directory must not open"),
    }
}

/// A truncated archive is the same class of failure, and is reachable from a real file.
#[test]
fn a_truncated_archive_is_a_container_error() {
    let bytes = std::fs::read(fixture("sample.ods")).unwrap();
    let truncated = bytes[..bytes.len() / 2].to_vec();
    match cellar::open_ods_from_rs(std::io::Cursor::new(truncated)) {
        Err(Error::Container(_)) => {}
        Err(other) => panic!("expected a container error, got {other:?}"),
        Ok(_) => panic!("a truncated archive must not open"),
    }
}

/// Part names are matched case-insensitively, because producers disagree about their case and
/// a workbook is worth reading whichever spelling it used.
#[test]
fn part_names_are_matched_case_insensitively() {
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData><row r="1"><c r="A1"><v>7</v></c></row></sheetData>
</worksheet>"#;
    let workbook = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheets><sheet name="S" sheetId="1"/></sheets>
</workbook>"#;

    let stored = zipcore::FileOptions::stored();
    let mut writer = zipcore::ZipWriter::new(std::io::Cursor::new(Vec::new())).unwrap();
    for (name, body) in [
        ("XL/Workbook.xml", workbook),
        ("XL/Worksheets/Sheet1.xml", sheet),
    ] {
        writer.add_file(name, body.as_bytes(), &stored).unwrap();
    }
    let bytes = writer.finish().unwrap().into_inner();

    let mut workbook = cellar::open_xlsx_from_rs(std::io::Cursor::new(bytes)).unwrap();
    let range = workbook.worksheet_range("S").unwrap();
    assert_eq!(range.get((0, 0)), Some(&Data::Int(7)));
}

/// Detection still distinguishes the three formats by content through the new backend.
#[test]
fn detection_still_distinguishes_the_formats() {
    for (name, expected) in [
        ("chanora.xlsx", Format::Xlsx),
        ("rollmilkice.xlsx", Format::Xlsx),
        ("sample.ods", Format::Ods),
    ] {
        let workbook = cellar::open_workbook(fixture(name)).unwrap();
        assert_eq!(workbook.format(), expected, "{name}");
    }

    // An OLE container that is not a workbook is rejected rather than misread as one. The
    // fixture is a Word 97 document, which shares `.xls`'s container format exactly.
    assert!(
        cellar::open_workbook(fixture("word97.doc")).is_err(),
        "a Word document must not open as a workbook"
    );
}

/// A deflated part is read through the streaming entry reader, which is the path a windowed
/// read depends on stopping early. This checks the payload actually inflates.
#[test]
fn a_deflated_part_reads_correctly() {
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData><row r="1"><c r="A1"><v>42</v></c></row></sheetData>
</worksheet>"#;
    let workbook = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheets><sheet name="S" sheetId="1"/></sheets>
</workbook>"#;

    let deflated = zipcore::FileOptions::deflate(6);
    let mut writer = zipcore::ZipWriter::new(std::io::Cursor::new(Vec::new())).unwrap();
    for (name, body) in [
        ("xl/workbook.xml", workbook),
        ("xl/worksheets/sheet1.xml", sheet),
    ] {
        writer.add_file(name, body.as_bytes(), &deflated).unwrap();
    }
    let bytes = writer.finish().unwrap().into_inner();

    let mut workbook = cellar::open_xlsx_from_rs(std::io::Cursor::new(bytes)).unwrap();
    assert_eq!(
        workbook.worksheet_range("S").unwrap().get((0, 0)),
        Some(&Data::Int(42))
    );
}

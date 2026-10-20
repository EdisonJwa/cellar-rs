//! Sheet visibility.
//!
//! A workbook distinguishes visible, hidden and very-hidden sheets, and a caller listing a
//! workbook needs to tell them apart: a hidden sheet is still data, but it is not what the author
//! intended to present. Reporting every sheet as visible makes a workbook look like it holds
//! content the user never sees.

use cellar::{Reader, SheetVisibility};

fn ods_package(content: &str) -> Vec<u8> {
    let stored = zipcore::FileOptions::stored();
    let mut w = zipcore::ZipWriter::new(std::io::Cursor::new(Vec::new())).unwrap();
    w.add_file(
        "mimetype",
        b"application/vnd.oasis.opendocument.spreadsheet",
        &stored,
    )
    .unwrap();
    w.add_file("content.xml", content.as_bytes(), &stored)
        .unwrap();
    w.finish().unwrap().into_inner()
}

fn ods_doc(tables: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
<office:body><office:spreadsheet>{tables}</office:spreadsheet></office:body>
</office:document-content>"#
    )
}

fn xlsx_with_states(states: &[(&str, Option<&str>)]) -> Vec<u8> {
    let mut workbook_xml = String::from(
        r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheets>"#,
    );
    for (i, (name, state)) in states.iter().enumerate() {
        match state {
            Some(s) => workbook_xml.push_str(&format!(
                r#"<sheet name="{name}" sheetId="{}" state="{s}"/>"#,
                i + 1
            )),
            None => {
                workbook_xml.push_str(&format!(r#"<sheet name="{name}" sheetId="{}"/>"#, i + 1))
            }
        }
    }
    workbook_xml.push_str("</sheets></workbook>");

    let empty = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData/></worksheet>"#;
    let stored = zipcore::FileOptions::stored();
    let mut w = zipcore::ZipWriter::new(std::io::Cursor::new(Vec::new())).unwrap();
    w.add_file("xl/workbook.xml", workbook_xml.as_bytes(), &stored)
        .unwrap();
    for i in 1..=states.len() {
        w.add_file(
            &format!("xl/worksheets/sheet{i}.xml"),
            empty.as_bytes(),
            &stored,
        )
        .unwrap();
    }
    w.finish().unwrap().into_inner()
}

#[test]
fn visibility_predicate_and_display() {
    assert!(SheetVisibility::Visible.is_visible());
    assert!(!SheetVisibility::Hidden.is_visible());
    assert!(!SheetVisibility::VeryHidden.is_visible());
    assert_eq!(SheetVisibility::Visible.to_string(), "visible");
    assert_eq!(SheetVisibility::Hidden.to_string(), "hidden");
    assert_eq!(SheetVisibility::VeryHidden.to_string(), "veryHidden");
    assert_eq!(SheetVisibility::default(), SheetVisibility::Visible);
}

#[test]
fn xlsx_reports_hidden_and_very_hidden_sheets() {
    let bytes = xlsx_with_states(&[
        ("Shown", None),
        ("Hidden1", Some("hidden")),
        ("VeryHidden1", Some("veryHidden")),
    ]);
    let wb = cellar::open_xlsx_from_rs(std::io::Cursor::new(bytes)).unwrap();
    assert_eq!(wb.sheet_names(), ["Shown", "Hidden1", "VeryHidden1"]);
    assert_eq!(wb.sheet_visibility(0), SheetVisibility::Visible);
    assert_eq!(wb.sheet_visibility(1), SheetVisibility::Hidden);
    assert_eq!(wb.sheet_visibility(2), SheetVisibility::VeryHidden);
}

#[test]
fn ods_reports_hidden_tables() {
    let bytes = ods_package(&ods_doc(
        r#"<table:table table:name="Shown"/>
           <table:table table:name="Hidden1" table:visibility="hidden"/>
           <table:table table:name="Collapsed" table:visibility="collapse"/>
           <table:table table:name="Filtered" table:visibility="filter"/>
           <table:table table:name="Explicit" table:visibility="visible"/>"#,
    ));
    let wb = cellar::open_ods_from_rs(std::io::Cursor::new(bytes)).unwrap();
    assert_eq!(
        wb.sheet_names(),
        ["Shown", "Hidden1", "Collapsed", "Filtered", "Explicit"]
    );
    assert_eq!(wb.sheet_visibility(0), SheetVisibility::Visible);
    assert_eq!(wb.sheet_visibility(1), SheetVisibility::Hidden, "hidden");
    // A collapsed or filtered sheet is not being shown either.
    assert_eq!(wb.sheet_visibility(2), SheetVisibility::Hidden, "collapse");
    assert_eq!(wb.sheet_visibility(3), SheetVisibility::Hidden, "filter");
    assert_eq!(wb.sheet_visibility(4), SheetVisibility::Visible, "visible");
}

/// Every format that does not record visibility reports visible, which keeps a caller's filter
/// from dropping data it cannot classify.
#[test]
fn an_out_of_range_index_is_visible() {
    let bytes = xlsx_with_states(&[("Only", None)]);
    let wb = cellar::open_xlsx_from_rs(std::io::Cursor::new(bytes)).unwrap();
    assert_eq!(wb.sheet_visibility(99), SheetVisibility::Visible);
}

/// The real fixtures parse their visibility without error, and report at least one sheet.
#[test]
fn real_fixtures_report_visibility() {
    for name in ["chanora.xlsx", "rollmilkice.xlsx", "sample.ods"] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name);
        let wb = cellar::open_workbook(&path).unwrap();
        assert!(!wb.sheet_names().is_empty(), "{name}");
        for index in 0..wb.sheet_names().len() {
            // Any of the three states is acceptable; what matters is that it is answerable.
            let visibility = wb.sheet_visibility(index);
            println!("{name}[{index}] = {visibility}");
        }
    }
}

/// A hidden sheet's cells are still readable: hiding is presentation, not access control.
#[test]
fn a_hidden_sheet_is_still_readable() {
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData><row r="1"><c r="A1"><v>5</v></c></row></sheetData>
</worksheet>"#;
    let workbook = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheets><sheet name="Secret" sheetId="1" state="hidden"/></sheets>
</workbook>"#;
    let stored = zipcore::FileOptions::stored();
    let mut w = zipcore::ZipWriter::new(std::io::Cursor::new(Vec::new())).unwrap();
    w.add_file("xl/workbook.xml", workbook.as_bytes(), &stored)
        .unwrap();
    w.add_file("xl/worksheets/sheet1.xml", sheet.as_bytes(), &stored)
        .unwrap();
    let bytes = w.finish().unwrap().into_inner();

    let mut wb = cellar::open_xlsx_from_rs(std::io::Cursor::new(bytes)).unwrap();
    assert_eq!(wb.sheet_visibility(0), SheetVisibility::Hidden);
    assert_eq!(
        wb.worksheet_range("Secret").unwrap().get((0, 0)),
        Some(&cellar::Data::Int(5)),
        "a hidden sheet's data is still readable"
    );
}

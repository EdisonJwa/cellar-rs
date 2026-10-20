//! Builds the fixtures the comparison needs.
//!
//! Three kinds are generated:
//!
//! * A large synthetic workbook in each format, so throughput is measured on a sheet whose size
//!   is controlled rather than on whatever the shipped fixtures happen to hold.
//! * A forward-slash rewrite of the shipped `.ods`, because that fixture was zipped on Windows
//!   and carries backslash separators ??non-conforming, and `calamine` cannot read it.
//! * Nothing else: the real `.xlsx` fixtures are used as they are.

use std::io::Read;

pub const ROWS: u32 = 4_000;
pub const COLS: u32 = 50;

fn col_letters(mut col: u32) -> String {
    let mut out = Vec::new();
    loop {
        out.push(b'A' + (col % 26) as u8);
        if col < 26 {
            break;
        }
        col = col / 26 - 1;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

/// A worksheet part with `ROWS x COLS` numeric cells and a declared `<dimension>`.
fn xlsx_sheet() -> String {
    let mut s = String::with_capacity(4 << 20);
    s.push_str(
        r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
    );
    s.push_str(&format!(
        r#"<dimension ref="A1:{}{}"/><sheetData>"#,
        col_letters(COLS - 1),
        ROWS
    ));
    for r in 1..=ROWS {
        s.push_str(&format!(r#"<row r="{r}">"#));
        for c in 0..COLS {
            let v = r as f64 * 1.5 + c as f64;
            s.push_str(&format!(
                r#"<c r="{}{}"><v>{v}</v></c>"#,
                col_letters(c),
                r
            ));
        }
        s.push_str("</row>");
    }
    s.push_str("</sheetData></worksheet>");
    s
}

/// The content part of an ODS document with `ROWS x COLS` numeric cells.
fn ods_content() -> String {
    let mut body = String::with_capacity(4 << 20);
    for r in 0..ROWS {
        body.push_str("<table:table-row>");
        for c in 0..COLS {
            let v = (r + 1) as f64 * 1.5 + c as f64;
            body.push_str(&format!(
                r#"<table:table-cell office:value-type="float" office:value="{v}"/>"#
            ));
        }
        body.push_str("</table:table-row>");
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
<office:body><office:spreadsheet><table:table table:name="S">{body}</table:table>
</office:spreadsheet></office:body></office:document-content>"#
    )
}

const ODS_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.3">
  <manifest:file-entry manifest:full-path="/" manifest:version="1.3" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/>
  <manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>
</manifest:manifest>"#;

/// Writes an ODS package with forward-slash entry names, deflated.
pub fn write_ods(path: &std::path::Path) {
    let stored = zipcore::FileOptions::stored();
    let deflated = zipcore::FileOptions::deflate(6);
    let mut w = zipcore::ZipWriter::new(std::fs::File::create(path).unwrap()).unwrap();
    // `mimetype` must be first and stored, as the format requires.
    w.add_file(
        "mimetype",
        b"application/vnd.oasis.opendocument.spreadsheet",
        &stored,
    )
    .unwrap();
    w.add_file("content.xml", ods_content().as_bytes(), &deflated)
        .unwrap();
    w.add_file("META-INF/manifest.xml", ODS_MANIFEST.as_bytes(), &deflated)
        .unwrap();
    w.finish().unwrap();
}

/// Writes an OOXML workbook with forward-slash entry names, deflated.
pub fn write_xlsx(path: &std::path::Path) {
    let workbook = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
          xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets><sheet name="S" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#;
    let types = r#"<?xml version="1.0"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="xml" ContentType="application/xml"/>
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#;
    // The package-level relationship is what a strict reader follows to find the workbook.
    // `gridxcel-rs` resolves the conventional path directly, which is why it needs nothing here.
    let package_rels = r#"<?xml version="1.0"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;
    let rels = r#"<?xml version="1.0"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#;

    let deflated = zipcore::FileOptions::deflate(6);
    let mut w = zipcore::ZipWriter::new(std::fs::File::create(path).unwrap()).unwrap();
    for (name, body) in [
        ("[Content_Types].xml", types),
        ("_rels/.rels", package_rels),
        ("xl/workbook.xml", workbook),
        ("xl/_rels/workbook.xml.rels", rels),
    ] {
        w.add_file(name, body.as_bytes(), &deflated).unwrap();
    }
    w.add_file("xl/worksheets/sheet1.xml", xlsx_sheet().as_bytes(), &deflated)
        .unwrap();
    w.finish().unwrap();
}

/// Rewrites an existing archive's entry names to use `/` instead of `\`.
///
/// The shipped `sample.ods` was zipped on Windows and stores `META-INF\manifest.xml`. The ZIP
/// specification requires `/`, and a reader that takes the names literally cannot find the parts.
/// Rather than changing the committed fixture ??which is a genuine example of what tools emit ??/// the comparison uses a conforming copy.
pub fn rewrite_slashes(src: &std::path::Path, dst: &std::path::Path) {
    let bytes = std::fs::read(src).unwrap();
    let mut archive = zipcore::Archive::new(std::io::Cursor::new(bytes)).unwrap();
    let stored = zipcore::FileOptions::stored();
    let mut w = zipcore::ZipWriter::new(std::fs::File::create(dst).unwrap()).unwrap();
    let count = archive.len();
    for i in 0..count {
        let name = archive.entries()[i].name().replace('\\', "/");
        let mut buf = Vec::new();
        archive
            .by_index(i)
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();
        w.add_file(&name, &buf, &stored).unwrap();
    }
    w.finish().unwrap();
}

/// Whether an archive's entry names all use `/`.
pub fn names_are_conforming(path: &std::path::Path) -> bool {
    let bytes = std::fs::read(path).unwrap();
    match zipcore::Archive::new(std::io::Cursor::new(bytes)) {
        Ok(a) => a.entries().iter().all(|e| !e.name().contains('\\')),
        Err(_) => false,
    }
}

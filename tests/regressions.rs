//! Regression tests for defects found in review.
//!
//! Each test pins a specific failure that was once present, so a change that reintroduces it
//! fails here rather than in the field. The fixtures are built in the test rather than shipped,
//! because every one of these is a malformed or adversarial file that no real suite carries.

use cellar::{Data, Error, Reader};

// ============================================================ ODS helpers

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

fn ods_doc(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:body><office:spreadsheet>
    <table:table table:name="S">{body}</table:table>
  </office:spreadsheet></office:body>
</office:document-content>"#
    )
}

fn open_ods(body: &str) -> cellar::Ods<std::io::Cursor<Vec<u8>>> {
    cellar::open_ods_from_rs(std::io::Cursor::new(ods_package(&ods_doc(body)))).unwrap()
}

// =========================================================== xlsx helpers

fn xlsx_package(sheet: &str, extras: &[(&str, &str)]) -> Vec<u8> {
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
    for (name, body) in extras {
        w.add_file(name, body.as_bytes(), &stored).unwrap();
    }
    w.finish().unwrap().into_inner()
}

// ================================================================== ODS

/// A row repeat that would push the row cursor past the grid must be refused.
///
/// This was a reachable arithmetic-overflow panic: empty row repeats are free, so about 120 KB
/// of markup drove the row counter to `u32::MAX` and the next repeat's `row + offset` overflowed.
/// It broke the crate's "a malformed file never panics" guarantee.
#[test]
fn ods_row_repeat_past_the_grid_is_refused_not_panicked() {
    const MAX_ROWS: u32 = 1_048_576;
    // 4096 * 1048576 == 2^32, so the cursor reaches the top of the index space.
    let padding: String =
        format!(r#"<table:table-row table:number-rows-repeated="{MAX_ROWS}"/>"#).repeat(4096);
    let body = format!(
        r#"{padding}<table:table-row table:number-rows-repeated="2">
             <table:table-cell office:value-type="float" office:value="1"/>
           </table:table-row>"#
    );

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut wb = open_ods(&body);
        wb.worksheet_range("S")
    }));
    assert!(outcome.is_ok(), "a malformed file must never panic");
    assert!(
        matches!(outcome.unwrap(), Err(Error::Format(_))),
        "the out-of-grid row repeat should be a format error"
    );
}

/// A cell placed past the grid's width is refused rather than silently dropped.
#[test]
fn ods_column_repeat_past_the_grid_is_refused() {
    let padding: String =
        r#"<table:table-cell table:number-columns-repeated="16384"/>"#.repeat(262_144);
    let body = format!(
        r#"<table:table-row>{padding}
             <table:table-cell office:value-type="float" office:value="1"/>
           </table:table-row>"#
    );
    let mut wb = open_ods(&body);
    assert!(matches!(wb.worksheet_range("S"), Err(Error::Format(_))));
}

/// A formula read must report the same budget refusal the value read does, not an empty range.
#[test]
fn ods_formula_read_does_not_swallow_a_budget_error() {
    const MAX_ROWS: u32 = 1_048_576;
    const MAX_COLS: u32 = 16_384;
    let body = format!(
        r#"<table:table-row>
             <table:table-cell office:value-type="float" office:value="1" table:formula="of:=1"/>
           </table:table-row>
           <table:table-row table:number-rows-repeated="{}"/>
           <table:table-row>
             <table:table-cell table:number-columns-repeated="{}"/>
             <table:table-cell office:value-type="float" office:value="2" table:formula="of:=2"/>
           </table:table-row>"#,
        MAX_ROWS - 2,
        MAX_COLS - 1
    );
    let mut wb = open_ods(&body);
    let values = wb.worksheet_range("S");
    let formulas = wb.worksheet_formula("S");
    assert!(matches!(values, Err(Error::Format(_))));
    assert!(
        matches!(formulas, Err(Error::Format(_))),
        "the formula read must refuse, not return an empty range"
    );
}

/// Text inside `<text:list>` is part of the cell's content, not decoration to be dropped.
#[test]
fn ods_list_content_is_not_lost() {
    let body = r#"<table:table-row>
        <table:table-cell office:value-type="string"><text:p>plain</text:p></table:table-cell>
        <table:table-cell office:value-type="string"><text:list><text:list-item>
            <text:p>in a list</text:p></text:list-item></text:list></table:table-cell>
      </table:table-row>"#;
    let mut wb = open_ods(body);
    let range = wb.worksheet_range("S").unwrap();
    assert_eq!(
        range.get((0, 1)).cloned().unwrap_or(Data::Empty),
        Data::String("in a list".into())
    );
}

/// Producers spell a boolean in more than one way; all of them are booleans.
#[test]
fn ods_boolean_spellings_all_read_as_booleans() {
    let body = r#"<table:table-row>
        <table:table-cell office:value-type="boolean" office:boolean-value="1"/>
        <table:table-cell office:value-type="boolean" office:boolean-value="TRUE"/>
        <table:table-cell office:value-type="boolean" office:boolean-value="true"/>
        <table:table-cell office:value-type="boolean" office:boolean-value="0"/>
        <table:table-cell office:value-type="boolean" office:boolean-value="false"/>
      </table:table-row>"#;
    let mut wb = open_ods(body);
    let range = wb.worksheet_range("S").unwrap();
    let cell = |c: u32| range.get((0, c)).cloned().unwrap_or(Data::Empty);
    assert_eq!(cell(0), Data::Bool(true), "\"1\"");
    assert_eq!(cell(1), Data::Bool(true), "\"TRUE\"");
    assert_eq!(cell(2), Data::Bool(true), "\"true\"");
    assert_eq!(cell(3), Data::Bool(false), "\"0\"");
    assert_eq!(cell(4), Data::Bool(false), "\"false\"");
}

// ================================================================= xlsx

/// A formula read must not depend on the shared string table.
///
/// It used to load and parse that table unconditionally, so a damaged `sharedStrings.xml` failed
/// a read that never needed it ??and a workbook with millions of strings paid to inflate them.
#[test]
fn xlsx_formula_read_does_not_need_the_shared_strings() {
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData><row r="1"><c r="A1"><f>1+1</f><v>2</v></c></row></sheetData>
</worksheet>"#;
    let bytes = xlsx_package(sheet, &[("xl/sharedStrings.xml", "<sst><si><t>unclosed")]);

    let mut wb = cellar::open_xlsx_from_rs(std::io::Cursor::new(bytes)).unwrap();
    let formulas = wb.worksheet_formula("S");
    assert!(
        formulas.is_ok(),
        "a formula read must not fail because the string table is damaged: {:?}",
        formulas.err()
    );
}

/// An error-typed cell with an empty value is empty, not an `Unknown("")` error.
#[test]
fn xlsx_empty_error_value_is_empty() {
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData><row r="1"><c r="A1" t="e"><v></v></c><c r="B1"><v>1</v></c></row></sheetData>
</worksheet>"#;
    let bytes = xlsx_package(sheet, &[]);
    let mut wb = cellar::open_xlsx_from_rs(std::io::Cursor::new(bytes)).unwrap();
    let range = wb.worksheet_range("S").unwrap();
    // An empty value contributes no cell, so the range is just B1 and A1 is outside it.
    assert_eq!(range.get((0, 0)), None);
    assert_eq!(range.get((0, 1)), Some(&Data::Int(1)));
}

/// The dense and streaming views must agree that a row outside the grid is impossible.
///
/// The dense path refused such a row while streaming handed back an addressable index, so two
/// views of one sheet contradicted each other.
#[test]
fn xlsx_streaming_and_dense_agree_on_an_impossible_row() {
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData><row r="4294967295"><c r="A4294967295"><v>1</v></c></row></sheetData>
</worksheet>"#;
    let bytes = xlsx_package(sheet, &[]);

    let mut dense_wb = cellar::open_xlsx_from_rs(std::io::Cursor::new(bytes.clone())).unwrap();
    let dense = dense_wb.worksheet_range("S");

    let mut stream_wb = cellar::open_xlsx_from_rs(std::io::Cursor::new(bytes)).unwrap();
    let mut rows = stream_wb.worksheet_rows("S").unwrap();
    let streamed = rows.next_row();

    assert!(
        matches!(dense, Err(Error::Format(_))),
        "dense should refuse"
    );
    assert!(
        matches!(streamed, Err(Error::Format(_))),
        "streaming should refuse too, not yield index 4294967294"
    );
}

/// A cell whose value is an integer is reported as one, and one beyond 2^53 as a float.
#[test]
fn xlsx_integer_and_float_are_distinguished() {
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData><row r="1">
  <c r="A1"><v>42</v></c>
  <c r="B1"><v>-7</v></c>
  <c r="C1"><v>1.5</v></c>
  <c r="D1"><v>2.0</v></c>
  <c r="E1"><v>1e20</v></c>
  <c r="F1"><v>9007199254740993</v></c>
</row></sheetData>
</worksheet>"#;
    let bytes = xlsx_package(sheet, &[]);
    let mut wb = cellar::open_xlsx_from_rs(std::io::Cursor::new(bytes)).unwrap();
    let range = wb.worksheet_range("S").unwrap();
    let cell = |c: u32| range.get((0, c)).cloned().unwrap_or(Data::Empty);
    assert_eq!(cell(0), Data::Int(42));
    assert_eq!(cell(1), Data::Int(-7));
    assert_eq!(cell(2), Data::Float(1.5));
    assert_eq!(cell(3), Data::Int(2), "2.0 is stored identically to 2");
    assert!(matches!(cell(4), Data::Float(_)), "1e20 is not an i64");
    // Past 2^53 an integer is not exactly representable, so it is reported as a float.
    assert!(
        matches!(cell(5), Data::Float(_)),
        "9007199254740993 exceeds i64's exact float range"
    );
}

// ================================================================== CFB

/// A compound file large enough to need a DIFAT extension sector must open.
///
/// The DIFAT extension loop started at slot 1 instead of slot 0, dropping the FAT sector listed
/// first and ingesting the next-DIFAT pointer as if it were a FAT entry. The table came out one
/// sector short, so every 512-byte-sector workbook above about 6.8 MiB failed with
/// "stream chain points outside the FAT".
#[test]
fn cfb_large_file_with_a_difat_extension_opens() {
    const SECTOR: usize = 512;
    const FREESECT: u32 = 0xFFFF_FFFF;
    const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
    const FATSECT: u32 = 0xFFFF_FFFD;
    const MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

    fn rec(op: u16, body: &[u8]) -> Vec<u8> {
        let mut o = op.to_le_bytes().to_vec();
        o.extend_from_slice(&(body.len() as u16).to_le_bytes());
        o.extend_from_slice(body);
        o
    }
    fn bof(sub: u16) -> Vec<u8> {
        let mut b = 0x0600u16.to_le_bytes().to_vec();
        b.extend_from_slice(&sub.to_le_bytes());
        b.extend_from_slice(&[0u8; 12]);
        rec(0x0809, &b)
    }
    fn dirent(name: &str, kind: u8, child: u32, start: u32, size: u64) -> [u8; 128] {
        let mut e = [0u8; 128];
        for (i, u) in name.encode_utf16().enumerate() {
            let at = i * 2;
            if at + 2 > 64 {
                break;
            }
            e[at..at + 2].copy_from_slice(&u.to_le_bytes());
        }
        let nl = ((name.encode_utf16().count() + 1) * 2) as u16;
        e[0x40..0x42].copy_from_slice(&nl.to_le_bytes());
        e[0x42] = kind;
        e[0x43] = 1;
        e[0x44..0x48].copy_from_slice(&FREESECT.to_le_bytes());
        e[0x48..0x4C].copy_from_slice(&FREESECT.to_le_bytes());
        e[0x4C..0x50].copy_from_slice(&child.to_le_bytes());
        e[0x74..0x78].copy_from_slice(&start.to_le_bytes());
        e[0x78..0x80].copy_from_slice(&size.to_le_bytes());
        e
    }

    // A globals substream naming one sheet, then padded past 6.8 MiB.
    let mut g = bof(0x0005);
    let at = g.len();
    let mut bs = 0u32.to_le_bytes().to_vec();
    bs.extend_from_slice(&0u16.to_le_bytes());
    bs.push(6);
    bs.push(0);
    bs.extend_from_slice(b"Sheet1");
    g.extend(rec(0x0085, &bs));
    g.extend(rec(0x000A, &[]));
    let off = g.len() as u32;
    g[at + 4..at + 8].copy_from_slice(&off.to_le_bytes());
    let mut s = bof(0x0010);
    let mut n = 0u16.to_le_bytes().to_vec();
    n.extend_from_slice(&0u16.to_le_bytes());
    n.extend_from_slice(&0u16.to_le_bytes());
    n.extend_from_slice(&1.0f64.to_le_bytes());
    s.extend(rec(0x0203, &n));
    s.extend(rec(0x000A, &[]));
    g.extend(s);
    // 13888 data sectors forces 110 FAT sectors, which needs one DIFAT extension.
    g.resize(13_888 * SECTOR, 0);

    let d = g.len() / SECTOR;
    let f = 110usize;
    let mut fat = vec![FREESECT; f * 128];
    for slot in fat.iter_mut().take(f) {
        *slot = FATSECT;
    }
    fat[f] = ENDOFCHAIN;
    fat[f + 1] = ENDOFCHAIN;
    let data_start = (f + 2) as u32;
    for i in 0..d {
        let s = data_start as usize + i;
        fat[s] = if i + 1 == d {
            ENDOFCHAIN
        } else {
            (s + 1) as u32
        };
    }

    let mut h = vec![0u8; SECTOR];
    h[..8].copy_from_slice(&MAGIC);
    h[0x18..0x1A].copy_from_slice(&0x003Eu16.to_le_bytes());
    h[0x1A..0x1C].copy_from_slice(&3u16.to_le_bytes());
    h[0x1C..0x1E].copy_from_slice(&0xFFFEu16.to_le_bytes());
    h[0x1E..0x20].copy_from_slice(&9u16.to_le_bytes());
    h[0x20..0x22].copy_from_slice(&6u16.to_le_bytes());
    h[0x2C..0x30].copy_from_slice(&(f as u32).to_le_bytes());
    h[0x30..0x34].copy_from_slice(&((f + 1) as u32).to_le_bytes());
    h[0x38..0x3C].copy_from_slice(&4096u32.to_le_bytes());
    h[0x3C..0x40].copy_from_slice(&ENDOFCHAIN.to_le_bytes());
    h[0x44..0x48].copy_from_slice(&(f as u32).to_le_bytes());
    h[0x48..0x4C].copy_from_slice(&1u32.to_le_bytes());
    for i in 0..109 {
        let at = 0x4C + i * 4;
        h[at..at + 4].copy_from_slice(&(i as u32).to_le_bytes());
    }

    // The extension sector lists the 110th FAT sector in slot 0; the last slot is the pointer.
    let mut difat = vec![FREESECT; 128];
    difat[0] = 109;
    difat[127] = ENDOFCHAIN;

    let mut dir = Vec::new();
    dir.extend_from_slice(&dirent("Root Entry", 5, 1, ENDOFCHAIN, 0));
    dir.extend_from_slice(&dirent("Workbook", 2, FREESECT, data_start, g.len() as u64));
    dir.resize(SECTOR, 0);

    let mut out = h;
    for v in &fat {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for v in &difat {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&dir);
    out.extend_from_slice(&g);

    let mut wb = cellar::open_xls_from_rs(std::io::Cursor::new(out))
        .expect("a file with a DIFAT extension must open");
    assert_eq!(wb.sheet_names(), ["Sheet1"]);
    assert_eq!(
        wb.worksheet_range("Sheet1").unwrap().get((0, 0)),
        Some(&Data::Int(1))
    );
}

/// A hole in the directory array must not hide the workbook stream.
///
/// Directory entries address one another by slot index, so an unused slot still has to occupy a
/// position. Dropping it shifted every later index, and the workbook became unreachable.
#[test]
fn cfb_directory_hole_does_not_hide_the_workbook() {
    const SECTOR: usize = 512;
    const FREESECT: u32 = 0xFFFF_FFFF;
    const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
    const FATSECT: u32 = 0xFFFF_FFFD;
    const MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

    fn rec(op: u16, body: &[u8]) -> Vec<u8> {
        let mut o = op.to_le_bytes().to_vec();
        o.extend_from_slice(&(body.len() as u16).to_le_bytes());
        o.extend_from_slice(body);
        o
    }
    fn bof(sub: u16) -> Vec<u8> {
        let mut b = 0x0600u16.to_le_bytes().to_vec();
        b.extend_from_slice(&sub.to_le_bytes());
        b.extend_from_slice(&[0u8; 12]);
        rec(0x0809, &b)
    }
    fn dirent(name: &str, kind: u8, child: u32, start: u32, size: u64) -> [u8; 128] {
        let mut e = [0u8; 128];
        for (i, u) in name.encode_utf16().enumerate() {
            let at = i * 2;
            if at + 2 > 64 {
                break;
            }
            e[at..at + 2].copy_from_slice(&u.to_le_bytes());
        }
        let nl = ((name.encode_utf16().count() + 1) * 2) as u16;
        e[0x40..0x42].copy_from_slice(&nl.to_le_bytes());
        e[0x42] = kind;
        e[0x43] = 1;
        e[0x44..0x48].copy_from_slice(&FREESECT.to_le_bytes());
        e[0x48..0x4C].copy_from_slice(&FREESECT.to_le_bytes());
        e[0x4C..0x50].copy_from_slice(&child.to_le_bytes());
        e[0x74..0x78].copy_from_slice(&start.to_le_bytes());
        e[0x78..0x80].copy_from_slice(&size.to_le_bytes());
        e
    }

    let mut g = bof(0x0005);
    let at = g.len();
    let mut bs = 0u32.to_le_bytes().to_vec();
    bs.extend_from_slice(&0u16.to_le_bytes());
    bs.push(6);
    bs.push(0);
    bs.extend_from_slice(b"Sheet1");
    g.extend(rec(0x0085, &bs));
    g.extend(rec(0x000A, &[]));
    let off = g.len() as u32;
    g[at + 4..at + 8].copy_from_slice(&off.to_le_bytes());
    let mut s = bof(0x0010);
    let mut n = 0u16.to_le_bytes().to_vec();
    n.extend_from_slice(&0u16.to_le_bytes());
    n.extend_from_slice(&0u16.to_le_bytes());
    n.extend_from_slice(&7.0f64.to_le_bytes());
    s.extend(rec(0x0203, &n));
    s.extend(rec(0x000A, &[]));
    g.extend(s);
    if g.len() < 4096 {
        g.resize(4096, 0);
    }

    let d = g.len().div_ceil(SECTOR);
    let f = ((d + 1).div_ceil(127)).max(1);
    let mut fat = vec![FREESECT; f * 128];
    for slot in fat.iter_mut().take(f) {
        *slot = FATSECT;
    }
    fat[f] = ENDOFCHAIN;
    let data_start = (f + 1) as u32;
    for i in 0..d {
        let s = data_start as usize + i;
        fat[s] = if i + 1 == d {
            ENDOFCHAIN
        } else {
            (s + 1) as u32
        };
    }

    let mut h = vec![0u8; SECTOR];
    h[..8].copy_from_slice(&MAGIC);
    h[0x18..0x1A].copy_from_slice(&0x003Eu16.to_le_bytes());
    h[0x1A..0x1C].copy_from_slice(&3u16.to_le_bytes());
    h[0x1C..0x1E].copy_from_slice(&0xFFFEu16.to_le_bytes());
    h[0x1E..0x20].copy_from_slice(&9u16.to_le_bytes());
    h[0x20..0x22].copy_from_slice(&6u16.to_le_bytes());
    h[0x2C..0x30].copy_from_slice(&(f as u32).to_le_bytes());
    h[0x30..0x34].copy_from_slice(&(f as u32).to_le_bytes());
    h[0x38..0x3C].copy_from_slice(&4096u32.to_le_bytes());
    h[0x3C..0x40].copy_from_slice(&ENDOFCHAIN.to_le_bytes());
    h[0x44..0x48].copy_from_slice(&ENDOFCHAIN.to_le_bytes());
    for slot in h[0x4C..0x200].as_chunks_mut::<4>().0 {
        slot.copy_from_slice(&FREESECT.to_le_bytes());
    }
    for i in 0..f {
        let at = 0x4C + i * 4;
        h[at..at + 4].copy_from_slice(&(i as u32).to_le_bytes());
    }

    // Slot 0 is the root, slot 1 is deliberately unused, slot 2 is the workbook. The root's
    // child index points at slot 2, which only resolves if the hole keeps its position.
    let mut dir = Vec::new();
    dir.extend_from_slice(&dirent("Root Entry", 5, 2, ENDOFCHAIN, 0));
    dir.extend_from_slice(&[0u8; 128]);
    dir.extend_from_slice(&dirent("Workbook", 2, FREESECT, data_start, g.len() as u64));
    dir.resize(SECTOR, 0);

    let mut out = h;
    for v in &fat {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&dir);
    let mut data = g;
    data.resize(d * SECTOR, 0);
    out.extend_from_slice(&data);

    let mut wb = cellar::open_xls_from_rs(std::io::Cursor::new(out))
        .expect("a directory hole must not hide the workbook");
    assert_eq!(wb.sheet_names(), ["Sheet1"]);
    assert_eq!(
        wb.worksheet_range("Sheet1").unwrap().get((0, 0)),
        Some(&Data::Int(7))
    );
}

/// A `DIMENSIONS` record appearing after cells must not discard them.
///
/// The dense reader replaced its builder whenever it saw one, so a second `DIMENSIONS` ??which
/// BIFF permits anywhere in the substream ??silently dropped the sheet's prefix.
#[test]
fn xls_interior_dimensions_does_not_drop_cells() {
    const SECTOR: usize = 512;
    const FREESECT: u32 = 0xFFFF_FFFF;
    const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
    const FATSECT: u32 = 0xFFFF_FFFD;
    const MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

    fn rec(op: u16, body: &[u8]) -> Vec<u8> {
        let mut o = op.to_le_bytes().to_vec();
        o.extend_from_slice(&(body.len() as u16).to_le_bytes());
        o.extend_from_slice(body);
        o
    }
    fn bof(sub: u16) -> Vec<u8> {
        let mut b = 0x0600u16.to_le_bytes().to_vec();
        b.extend_from_slice(&sub.to_le_bytes());
        b.extend_from_slice(&[0u8; 12]);
        rec(0x0809, &b)
    }
    fn number(row: u16, col: u16, v: f64) -> Vec<u8> {
        let mut b = row.to_le_bytes().to_vec();
        b.extend_from_slice(&col.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&v.to_le_bytes());
        rec(0x0203, &b)
    }
    fn dimensions(first_row: u32, last_row: u32, first_col: u16, last_col: u16) -> Vec<u8> {
        let mut b = first_row.to_le_bytes().to_vec();
        b.extend_from_slice(&last_row.to_le_bytes());
        b.extend_from_slice(&first_col.to_le_bytes());
        b.extend_from_slice(&last_col.to_le_bytes());
        rec(0x0200, &b)
    }
    fn dirent(name: &str, kind: u8, child: u32, start: u32, size: u64) -> [u8; 128] {
        let mut e = [0u8; 128];
        for (i, u) in name.encode_utf16().enumerate() {
            let at = i * 2;
            if at + 2 > 64 {
                break;
            }
            e[at..at + 2].copy_from_slice(&u.to_le_bytes());
        }
        let nl = ((name.encode_utf16().count() + 1) * 2) as u16;
        e[0x40..0x42].copy_from_slice(&nl.to_le_bytes());
        e[0x42] = kind;
        e[0x43] = 1;
        e[0x44..0x48].copy_from_slice(&FREESECT.to_le_bytes());
        e[0x48..0x4C].copy_from_slice(&FREESECT.to_le_bytes());
        e[0x4C..0x50].copy_from_slice(&child.to_le_bytes());
        e[0x74..0x78].copy_from_slice(&start.to_le_bytes());
        e[0x78..0x80].copy_from_slice(&size.to_le_bytes());
        e
    }

    let build = |with_interior: bool| -> Vec<u8> {
        let mut g = bof(0x0005);
        let at = g.len();
        let mut bs = 0u32.to_le_bytes().to_vec();
        bs.extend_from_slice(&0u16.to_le_bytes());
        bs.push(6);
        bs.push(0);
        bs.extend_from_slice(b"Sheet1");
        g.extend(rec(0x0085, &bs));
        g.extend(rec(0x000A, &[]));
        let off = g.len() as u32;
        g[at + 4..at + 8].copy_from_slice(&off.to_le_bytes());

        let mut s = bof(0x0010);
        s.extend(number(0, 0, 1.0));
        if with_interior {
            s.extend(dimensions(0, 2, 0, 1));
        }
        s.extend(number(1, 0, 2.0));
        s.extend(rec(0x000A, &[]));
        g.extend(s);
        if g.len() < 4096 {
            g.resize(4096, 0);
        }

        let d = g.len().div_ceil(SECTOR);
        let f = ((d + 1).div_ceil(127)).max(1);
        let mut fat = vec![FREESECT; f * 128];
        for slot in fat.iter_mut().take(f) {
            *slot = FATSECT;
        }
        fat[f] = ENDOFCHAIN;
        let data_start = (f + 1) as u32;
        for i in 0..d {
            let s = data_start as usize + i;
            fat[s] = if i + 1 == d {
                ENDOFCHAIN
            } else {
                (s + 1) as u32
            };
        }

        let mut h = vec![0u8; SECTOR];
        h[..8].copy_from_slice(&MAGIC);
        h[0x18..0x1A].copy_from_slice(&0x003Eu16.to_le_bytes());
        h[0x1A..0x1C].copy_from_slice(&3u16.to_le_bytes());
        h[0x1C..0x1E].copy_from_slice(&0xFFFEu16.to_le_bytes());
        h[0x1E..0x20].copy_from_slice(&9u16.to_le_bytes());
        h[0x20..0x22].copy_from_slice(&6u16.to_le_bytes());
        h[0x2C..0x30].copy_from_slice(&(f as u32).to_le_bytes());
        h[0x30..0x34].copy_from_slice(&(f as u32).to_le_bytes());
        h[0x38..0x3C].copy_from_slice(&4096u32.to_le_bytes());
        h[0x3C..0x40].copy_from_slice(&ENDOFCHAIN.to_le_bytes());
        h[0x44..0x48].copy_from_slice(&ENDOFCHAIN.to_le_bytes());
        for slot in h[0x4C..0x200].as_chunks_mut::<4>().0 {
            slot.copy_from_slice(&FREESECT.to_le_bytes());
        }
        for i in 0..f {
            let at = 0x4C + i * 4;
            h[at..at + 4].copy_from_slice(&(i as u32).to_le_bytes());
        }

        let mut dir = Vec::new();
        dir.extend_from_slice(&dirent("Root Entry", 5, 1, ENDOFCHAIN, 0));
        dir.extend_from_slice(&dirent("Workbook", 2, FREESECT, data_start, g.len() as u64));
        dir.resize(SECTOR, 0);

        let mut out = h;
        for v in &fat {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&dir);
        let mut data = g;
        data.resize(d * SECTOR, 0);
        out.extend_from_slice(&data);
        out
    };

    let read = |bytes: Vec<u8>| {
        let mut wb = cellar::open_xls_from_rs(std::io::Cursor::new(bytes)).unwrap();
        wb.worksheet_range("Sheet1").unwrap()
    };

    let baseline = read(build(false));
    let with_dims = read(build(true));
    assert_eq!(baseline.get_size(), (2, 1));
    assert_eq!(
        with_dims.get_size(),
        (2, 1),
        "an interior DIMENSIONS must not drop the cells before it"
    );
    assert_eq!(with_dims.get((0, 0)), Some(&Data::Int(1)));
    assert_eq!(with_dims.get((1, 0)), Some(&Data::Int(2)));
}

/// A file that is not a BIFF stream at all must be rejected, not opened as an empty workbook.
#[test]
fn xls_rejects_bytes_that_are_not_a_workbook() {
    let junk = b"this is definitely not a BIFF workbook, not even close".to_vec();
    match cellar::open_xls_from_rs(std::io::Cursor::new(junk)) {
        Err(_) => {}
        Ok(wb) => panic!(
            "expected a rejection, but it opened with sheets {:?}",
            wb.sheet_names()
        ),
    }
}

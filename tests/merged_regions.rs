//! Merged-cell regions, which every spreadsheet format records differently.
//!
//! A merged cell's value sits in its top-left position; the rest of the region is blank in the
//! grid. Without the region a caller cannot tell "merged" from "empty", so each reader has to
//! report it rather than leave the caller to guess from shape alone.

use cellar::{Data, MergedRegion, Reader};

// ------------------------------------------------------------------ helpers

fn xlsx_with_merges(merges: &[&str]) -> Vec<u8> {
    let mut sheet = String::from(
        r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData>
<row r="1"><c r="A1" t="inlineStr"><is><t>title</t></is></c><c r="B1"><v>1</v></c></row>
<row r="2"><c r="A2"><v>2</v></c></row>
</sheetData>"#,
    );
    if !merges.is_empty() {
        sheet.push_str("<mergeCells>");
        for m in merges {
            sheet.push_str(&format!(r#"<mergeCell ref="{m}"/>"#));
        }
        sheet.push_str("</mergeCells>");
    }
    sheet.push_str("</worksheet>");

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
    w.finish().unwrap().into_inner()
}

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

/// A BIFF8 workbook whose sheet carries one `MERGEDCELLS` record.
fn xls_with_merge(first: (u16, u16), last: (u16, u16)) -> Vec<u8> {
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
    let mut mc = 1u16.to_le_bytes().to_vec();
    mc.extend_from_slice(&first.0.to_le_bytes());
    mc.extend_from_slice(&last.0.to_le_bytes());
    mc.extend_from_slice(&first.1.to_le_bytes());
    mc.extend_from_slice(&last.1.to_le_bytes());
    s.extend(rec(0x00E5, &mc));
    s.extend(rec(0x000A, &[]));
    g.extend(s);

    // A compound file stores streams under 4096 bytes in the mini-stream, which this fixture
    // does not build. Padding past that threshold keeps the workbook on the ordinary FAT,
    // which is what a real `.xls` of any size uses and what this test is exercising.
    if g.len() < 4096 {
        g.resize(4096, 0);
    }

    let d = g.len().div_ceil(SECTOR);
    let f = ((d + 1).div_ceil(127)).max(1);
    let mut fat = vec![FREESECT; f * 128];
    for x in &mut fat[..f] {
        *x = FATSECT;
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
}

fn ods_with_span(span_cols: u32, span_rows: u32) -> Vec<u8> {
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
<office:body><office:spreadsheet><table:table table:name="S">
  <table:table-row>
    <table:table-cell office:value-type="string" table:number-columns-spanned="{span_cols}"
      table:number-rows-spanned="{span_rows}"><text:p>merged</text:p></table:table-cell>
  </table:table-row>
</table:table></office:spreadsheet></office:body></office:document-content>"#
    );
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

// -------------------------------------------------------------------- tests

#[test]
fn region_arithmetic_is_inclusive() {
    let r = MergedRegion::new((1, 1), (3, 3));
    assert_eq!(r.rows(), 3);
    assert_eq!(r.columns(), 3);
    assert!(r.contains((1, 1)));
    assert!(r.contains((3, 3)));
    assert!(!r.contains((4, 3)));
    assert_eq!(r.to_a1(), "B2:D4");
    // Corners may be given either way round.
    assert_eq!(MergedRegion::new((3, 3), (1, 1)), r);
}

#[test]
fn xlsx_reports_merged_regions() {
    let bytes = xlsx_with_merges(&["A1:B2", "D4:E6"]);
    let mut wb = cellar::open_xlsx_from_rs(std::io::Cursor::new(bytes)).unwrap();
    let regions = wb.worksheet_merged_regions("S").unwrap();
    assert_eq!(
        regions,
        vec![
            MergedRegion::new((0, 0), (1, 1)),
            MergedRegion::new((3, 3), (5, 4)),
        ]
    );
    // The value lives in the anchor and the covered positions are blank.
    let range = wb.worksheet_range("S").unwrap();
    assert_eq!(
        range.get((0, 0)),
        Some(&Data::String("title".into())),
        "the anchor holds the value"
    );
    assert_eq!(range.get((1, 1)), Some(&Data::Empty), "covered is blank");
}

#[test]
fn xlsx_without_merges_reports_nothing() {
    let bytes = xlsx_with_merges(&[]);
    let mut wb = cellar::open_xlsx_from_rs(std::io::Cursor::new(bytes)).unwrap();
    assert!(wb.worksheet_merged_regions("S").unwrap().is_empty());
}

#[test]
fn xls_reports_merged_regions() {
    let bytes = xls_with_merge((2, 1), (4, 3));
    let mut wb = cellar::open_xls_from_rs(std::io::Cursor::new(bytes)).unwrap();
    let regions = wb.worksheet_merged_regions("Sheet1").unwrap();
    assert_eq!(regions, vec![MergedRegion::new((2, 1), (4, 3))]);
    assert_eq!(regions[0].to_a1(), "B3:D5");
}

#[test]
fn ods_reports_merged_regions() {
    let bytes = ods_with_span(3, 2);
    let mut wb = cellar::open_ods_from_rs(std::io::Cursor::new(bytes)).unwrap();
    let regions = wb.worksheet_merged_regions("S").unwrap();
    assert_eq!(regions, vec![MergedRegion::new((0, 0), (1, 2))]);
    assert_eq!(regions[0].to_a1(), "A1:C2");
}

#[test]
fn ods_without_spans_reports_nothing() {
    let bytes = ods_with_span(1, 1);
    let mut wb = cellar::open_ods_from_rs(std::io::Cursor::new(bytes)).unwrap();
    assert!(wb.worksheet_merged_regions("S").unwrap().is_empty());
}

/// An unknown sheet is the same `NotFound` the value reads give.
#[test]
fn merged_regions_reject_an_unknown_sheet() {
    let bytes = xlsx_with_merges(&["A1:B2"]);
    let mut wb = cellar::open_xlsx_from_rs(std::io::Cursor::new(bytes)).unwrap();
    assert!(matches!(
        wb.worksheet_merged_regions("nope"),
        Err(cellar::Error::NotFound(_))
    ));
}

/// The merged regions of the real fixtures parse without error, which is what keeps the
/// element walk honest on files this crate did not build.
#[test]
fn real_fixtures_report_their_merges() {
    for name in ["chanora.xlsx", "rollmilkice.xlsx", "sample.ods"] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name);
        let mut wb = cellar::open_workbook(&path).unwrap();
        for sheet in wb.sheet_names().to_vec() {
            let regions = wb.worksheet_merged_regions(&sheet).unwrap();
            for r in &regions {
                assert!(
                    r.start.0 <= r.end.0 && r.start.1 <= r.end.1,
                    "{name}/{sheet}"
                );
                assert!(r.rows() >= 1 && r.columns() >= 1, "{name}/{sheet}");
                assert!(r.end.0 < 1_048_576 && r.end.1 < 16_384, "{name}/{sheet}");
            }
            println!("{name}/{sheet}: {} merged regions", regions.len());
        }
    }
}

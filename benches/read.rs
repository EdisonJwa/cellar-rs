//! Read throughput, so the performance claims in the README stay checkable.
//!
//! Run with `cargo bench`. The fixtures are the same real workbooks the tests use.

use criterion::{criterion_group, criterion_main, Criterion};
use gridxcel_rs::{open_workbook, Reader};
use std::hint::black_box;

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join(name)
}

/// Opening only reads metadata, so this should not scale with sheet contents.
fn bench_open(c: &mut Criterion) {
    let mut group = c.benchmark_group("open");
    group.bench_function("xlsx", |b| {
        b.iter(|| {
            let workbook = open_workbook(fixture("chanora.xlsx")).unwrap();
            black_box(workbook.sheet_names().len())
        })
    });
    group.bench_function("ods", |b| {
        b.iter(|| {
            let workbook = open_workbook(fixture("sample.ods")).unwrap();
            black_box(workbook.sheet_names().len())
        })
    });
    group.finish();
}

/// The full parse of a nine-sheet workbook, shared string table included.
fn bench_read_all_sheets(c: &mut Criterion) {
    c.bench_function("read_all_sheets/chanora.xlsx", |b| {
        b.iter(|| {
            let mut workbook = open_workbook(fixture("chanora.xlsx")).unwrap();
            let mut cells = 0usize;
            for name in workbook.sheet_names().to_vec() {
                cells += workbook.worksheet_range(&name).unwrap().used_cell_count();
            }
            black_box(cells)
        })
    });
}

/// A single dense sheet, where the sparse-to-dense conversion shows up.
fn bench_one_sheet(c: &mut Criterion) {
    c.bench_function("one_sheet/identifications", |b| {
        b.iter(|| {
            let mut workbook = open_workbook(fixture("chanora.xlsx")).unwrap();
            black_box(
                workbook
                    .worksheet_range("Identifications")
                    .unwrap()
                    .used_cell_count(),
            )
        })
    });
}

/// The serde path: one column plan per range, then one value per field per row.
///
/// Fields are owned strings because the row deserializer requires `DeserializeOwned`; a borrowed
/// `&str` field would not compile.
#[cfg(feature = "serde")]
fn bench_deserialize(c: &mut Criterion) {
    use gridxcel_rs::RangeDeserializerBuilder;
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct Identification {
        #[serde(rename = "Identified By")]
        identified_by: String,
        #[serde(rename = "File path")]
        path: String,
        #[serde(rename = "Component license category")]
        category: String,
        #[serde(rename = "Hits")]
        hits: i64,
    }

    c.bench_function("deserialize/chanora.xlsx", |b| {
        b.iter(|| {
            let mut workbook = open_workbook(fixture("chanora.xlsx")).unwrap();
            let range = workbook.worksheet_range("Identifications").unwrap();
            // Rows whose fields are blank fail the strict `i64`; a benchmark cares about
            // throughput, not about the fixture's quirks.
            let count = RangeDeserializerBuilder::new()
                .from_range::<Identification>(&range)
                .unwrap()
                .filter(Result::is_ok)
                .count();
            black_box(count)
        })
    });
}

const WINDOW_ROWS: u32 = 2000;
const WINDOW_COLS: u32 = 50;

/// Reads a window out of a large sheet, which is where skipping pays.
///
/// The parser is told which rows and columns the caller wants, so a row outside the window costs a
/// tag scan instead of decoding every cell in it. Without that, a window read would still walk the
/// whole sheet and the difference would vanish.
fn bench_window(c: &mut Criterion) {
    use std::io::Write;

    let path = std::env::temp_dir().join("gridxcel-bench-window.xlsx");
    // Deflated, as a real workbook is. On a stored part there is no decompression to stop, so the
    // saving a window read makes would not show up at all.
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    writer.start_file("xl/workbook.xml", options).unwrap();
    writer
        .write_all(
            br#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheets><sheet name="S" sheetId="1"/></sheets>
</workbook>"#,
        )
        .unwrap();
    writer
        .start_file("xl/worksheets/sheet1.xml", options)
        .unwrap();
    writer
        .write_all(
            format!(
                r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<dimension ref="A1:{}{}"/>
<sheetData>"#,
                gridxcel_rs::refs::col_to_letters(WINDOW_COLS - 1),
                WINDOW_ROWS
            )
            .as_bytes(),
        )
        .unwrap();
    for row in 1..=WINDOW_ROWS {
        writer
            .write_all(format!(r#"<row r="{row}">"#).as_bytes())
            .unwrap();
        for col in 0..WINDOW_COLS {
            writer
                .write_all(
                    format!(
                        r#"<c r="{}{row}"><v>{}</v></c>"#,
                        gridxcel_rs::refs::col_to_letters(col),
                        row * WINDOW_COLS + col
                    )
                    .as_bytes(),
                )
                .unwrap();
        }
        writer.write_all(b"</row>").unwrap();
    }
    writer.write_all(b"</sheetData></worksheet>").unwrap();
    writer.finish().unwrap();

    eprintln!(
        "sheet is {WINDOW_ROWS} x {WINDOW_COLS} = {} cells",
        u64::from(WINDOW_ROWS) * u64::from(WINDOW_COLS)
    );

    let mut group = c.benchmark_group("window");
    group.bench_function("one_cell/A1", |b| {
        b.iter(|| {
            let mut workbook = open_workbook(&path).unwrap();
            black_box(workbook.worksheet_range_ref("S", "A1").unwrap().get_size())
        })
    });
    group.bench_function("middle/B900:C910", |b| {
        b.iter(|| {
            let mut workbook = open_workbook(&path).unwrap();
            black_box(
                workbook
                    .worksheet_range_ref("S", "B900:C910")
                    .unwrap()
                    .used_cell_count(),
            )
        })
    });
    group.bench_function("whole_sheet", |b| {
        b.iter(|| {
            let mut workbook = open_workbook(&path).unwrap();
            black_box(workbook.worksheet_range("S").unwrap().used_cell_count())
        })
    });
    group.finish();

    let _ = std::fs::remove_file(&path);
}

criterion_group!(
    benches,
    bench_open,
    bench_read_all_sheets,
    bench_one_sheet,
    bench_window
);

#[cfg(feature = "serde")]
criterion_group!(serde_benches, bench_deserialize);

#[cfg(feature = "serde")]
criterion_main!(benches, serde_benches);

#[cfg(not(feature = "serde"))]
criterion_main!(benches);

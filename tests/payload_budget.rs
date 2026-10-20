//! The cell budget must account for what a cell *holds*, not only for its slot.
//!
//! `size_of::<Data>()` is a fixed 32 bytes whether a cell is `Empty` or a `String` owning a
//! megabyte, so a budget derived from the slot size cannot see string payloads. A workbook of
//! cells all naming one shared string therefore passed the budget while allocating far more heap
//! than it allowed: a 49,522-byte file measured at 23 MB of live heap, and at a large enough
//! shape the amplification is unbounded relative to the file.
//!
//! Findings are pinned against a counting allocator, because the difference between "bounded" and
//! "bounded in the units that happen to be counted" is invisible to any other kind of test.

use cellar::Reader;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

mod common;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: every method forwards to the system allocator and keeps statistics that cannot affect
// what is returned.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        System.dealloc(ptr, layout);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

static MEASURING: Mutex<()> = Mutex::new(());

fn reset() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}

fn peak() -> usize {
    PEAK.load(Ordering::Relaxed)
}

fn col(mut c: u32) -> String {
    let mut out = Vec::new();
    loop {
        out.push(b'A' + (c % 26) as u8);
        if c < 26 {
            break;
        }
        c = c / 26 - 1;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

/// A workbook of `rows x cols` cells, every one naming the same shared string of `text_len` bytes.
///
/// All cells share one SST entry, so the file costs one string plus the cell tags while the reader
/// materialises a clone per cell — which is exactly the amplification under test.
fn string_heavy_workbook(rows: u32, cols: u32, text_len: usize) -> Vec<u8> {
    let text = "x".repeat(text_len);

    let mut sst = String::with_capacity(text_len + 128);
    sst.push_str(
        r#"<?xml version="1.0"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
    );
    sst.push_str("<si><t>");
    sst.push_str(&text);
    sst.push_str("</t></si></sst>");

    let mut sheet = String::with_capacity(rows as usize * cols as usize * 26);
    sheet.push_str(
        r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#,
    );
    for r in 1..=rows {
        sheet.push_str(&format!(r#"<row r="{r}">"#));
        for c in 0..cols {
            sheet.push_str(&format!(r#"<c r="{}{}" t="s"><v>0</v></c>"#, col(c), r));
        }
        sheet.push_str("</row>");
    }
    sheet.push_str("</sheetData></worksheet>");

    let workbook = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheets><sheet name="S" sheetId="1"/></sheets>
</workbook>"#;

    let deflated = zipcore::FileOptions::deflate(6);
    let mut w = zipcore::ZipWriter::new(std::io::Cursor::new(Vec::new())).unwrap();
    w.add_file("xl/workbook.xml", workbook.as_bytes(), &deflated)
        .unwrap();
    w.add_file("xl/sharedStrings.xml", sst.as_bytes(), &deflated)
        .unwrap();
    w.add_file("xl/worksheets/sheet1.xml", sheet.as_bytes(), &deflated)
        .unwrap();
    w.finish().unwrap().into_inner()
}

/// A sheet whose total string payload is far beyond the payload budget must be refused, and the
/// refusal must happen before the payload is allocated.
///
/// 60,000 cells of 20 KiB each is 1.2 GB of text against 1.9 MB of slots, so a slot-only budget
/// admits it. The file that describes it is 157 KB.
#[test]
fn a_sheet_whose_text_exceeds_the_payload_budget_is_refused() {
    let _guard = MEASURING.lock().unwrap_or_else(|p| p.into_inner());
    let (rows, cols, text_len) = (600u32, 100u32, 20 * 1024usize);
    let payload = rows as usize * cols as usize * text_len;

    let bytes = string_heavy_workbook(rows, cols, text_len);
    let file_len = bytes.len();

    reset();
    let mut wb = cellar::open_xlsx_from_rs(std::io::Cursor::new(bytes)).unwrap();
    let result = wb.worksheet_range("S");
    let allocated = peak();

    println!(
        "file {file_len} bytes, payload {:.2} GB, peak {:.1} MB",
        payload as f64 / 1e9,
        allocated as f64 / 1e6
    );

    assert!(
        result.is_err(),
        "1.2 GB of cell text must be refused by the payload budget"
    );
    // The payload budget is 256 MiB; the refusal must land near it rather than after the whole
    // 1.2 GB has been cloned.
    assert!(
        allocated < 400 << 20,
        "refusal must precede the allocation, peak was {allocated} bytes"
    );
}

/// A sheet whose text is within the budget still reads, and every cell keeps its content.
///
/// This is the guard against the fix being so tight it rejects real workbooks.
#[test]
fn text_within_the_payload_budget_still_reads() {
    let _guard = MEASURING.lock().unwrap_or_else(|p| p.into_inner());
    let (rows, cols, text_len) = (200u32, 100u32, 1024usize);
    let bytes = string_heavy_workbook(rows, cols, text_len);

    reset();
    let mut wb = cellar::open_xlsx_from_rs(std::io::Cursor::new(bytes)).unwrap();
    let range = wb.worksheet_range("S").unwrap();

    assert_eq!(
        range.used_cell_count(),
        rows as usize * cols as usize,
        "every cell should read"
    );
    // The text survives the budget intact: a truncated cell would be worse than a refused file.
    let first = range.get((0, 0)).unwrap().as_string().unwrap();
    assert_eq!(first.len(), text_len);
    assert!(first.chars().all(|c| c == 'x'));

    let allocated = peak();
    println!("peak {:.1} MB for 20 MB of text", allocated as f64 / 1e6);
    assert!(
        allocated < 128 << 20,
        "a 20 MB sheet should not peak above 128 MB, got {allocated}"
    );
}

/// The real workbook fixtures stay inside the payload budget, so the new cap rejects nothing
/// a producer actually writes.
#[test]
fn real_fixtures_stay_within_the_payload_budget() {
    for name in ["chanora.xlsx", "rollmilkice.xlsx", "sample.ods"] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name);
        let mut wb = cellar::open_workbook(&path).unwrap();
        let mut total = 0usize;
        for sheet in wb.sheet_names().to_vec() {
            let range = wb.worksheet_range(&sheet).unwrap();
            for (_, _, value) in range.used_cells() {
                if let Some(text) = value.as_string() {
                    total += text.len();
                }
            }
        }
        println!("{name}: {:.2} MB of text", total as f64 / 1e6);
        assert!(
            total < 256 << 20,
            "{name} exceeds the payload budget at {total} bytes"
        );
    }
}

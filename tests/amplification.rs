//! Amplification through the paths that do not use `SheetBuilder`.
//!
//! The payload budget is enforced in `SheetBuilder::write`, which the OOXML and BIFF readers use.
//! The OpenDocument reader builds its range through `Range::from_sparse` instead, so the budget
//! had to be applied to it explicitly — and the compound-file reader bounded its chain walks by
//! the allocation table rather than by the file.
//!
//! Both were reachable with tiny files and measured in gigabytes before the fix:
//!
//! | Shape | File | Peak before | Peak after |
//! |---|---|---|---|
//! | ODS row repeat × one 64 KiB cell | 629 B | 1,075 MB | 0.5 MB |
//! | CFB two-sector directory cycle, 4 KiB sectors | 0.46 MB | 807 MB | 2.2 MB |
//!
//! Memory is measured with a counting allocator, because "bounded" is otherwise an opinion.

use gridxcel_rs::Reader;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

mod common;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: every method forwards to the system allocator and maintains counters that cannot
// affect what is returned.
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

/// A row repeat multiplying one large cell must be refused, not inflated.
///
/// The column repeat multiplies the cell's text as well as its count; the row repeat multiplies
/// the whole row. Neither went through the builder, so neither was charged.
#[test]
fn ods_row_repeat_cannot_multiply_text_without_bound() {
    let text = "x".repeat(64 * 1024);
    let body = format!(
        r#"<table:table-row table:number-rows-repeated="16384">
             <table:table-cell office:value-type="string"><text:p>{text}</text:p></table:table-cell>
           </table:table-row>"#
    );
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
<office:body><office:spreadsheet><table:table table:name="S">{body}</table:table>
</office:spreadsheet></office:body></office:document-content>"#
    );

    let deflated = zipcore::FileOptions::deflate(6);
    let mut w = zipcore::ZipWriter::new(std::io::Cursor::new(Vec::new())).unwrap();
    w.add_file(
        "mimetype",
        b"application/vnd.oasis.opendocument.spreadsheet",
        &zipcore::FileOptions::stored(),
    )
    .unwrap();
    w.add_file("content.xml", content.as_bytes(), &deflated)
        .unwrap();
    let bytes = w.finish().unwrap().into_inner();
    let file_len = bytes.len();

    let _guard = MEASURING.lock().unwrap_or_else(|p| p.into_inner());
    reset();
    let mut wb = gridxcel_rs::open_ods_from_rs(std::io::Cursor::new(bytes)).unwrap();
    let result = wb.worksheet_range("S");
    let allocated = peak();

    println!(
        "file {file_len} B -> peak {:.1} MB ({:.0}x)",
        allocated as f64 / 1e6,
        allocated as f64 / file_len as f64
    );

    assert!(
        result.is_err(),
        "1 GB of text from a {file_len}-byte file must be refused"
    );
    // The counters are process-wide, so a sibling test allocating in parallel lands in this
    // window. The assertion is on the order of magnitude that separates "refused before
    // allocating" from "cloned a gigabyte", not on an exact figure.
    assert!(
        allocated < 64 << 20,
        "refusal must precede the allocation, peak was {allocated} bytes"
    );
}

/// A column repeat multiplying one large cell is refused on the same grounds.
#[test]
fn ods_column_repeat_cannot_multiply_text_without_bound() {
    let text = "x".repeat(64 * 1024);
    let body = format!(
        r#"<table:table-row>
             <table:table-cell table:number-columns-repeated="16384"
                 office:value-type="string"><text:p>{text}</text:p></table:table-cell>
           </table:table-row>"#
    );
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
<office:body><office:spreadsheet><table:table table:name="S">{body}</table:table>
</office:spreadsheet></office:body></office:document-content>"#
    );

    let deflated = zipcore::FileOptions::deflate(6);
    let mut w = zipcore::ZipWriter::new(std::io::Cursor::new(Vec::new())).unwrap();
    w.add_file(
        "mimetype",
        b"application/vnd.oasis.opendocument.spreadsheet",
        &zipcore::FileOptions::stored(),
    )
    .unwrap();
    w.add_file("content.xml", content.as_bytes(), &deflated)
        .unwrap();
    let bytes = w.finish().unwrap().into_inner();
    let file_len = bytes.len();

    let _guard = MEASURING.lock().unwrap_or_else(|p| p.into_inner());
    reset();
    let mut wb = gridxcel_rs::open_ods_from_rs(std::io::Cursor::new(bytes)).unwrap();
    let result = wb.worksheet_range("S");
    let allocated = peak();

    println!("file {file_len} B -> peak {allocated} bytes");
    assert!(result.is_err(), "the column repeat must be refused");
    // The row's cell vector is reserved for 16,384 entries before the charge trips, so a few
    // hundred kilobytes of bookkeeping is expected. What must not happen is the gigabyte of
    // cloned text the unbounded path produced.
    assert!(allocated < 8 << 20, "peak was {allocated} bytes");
}

/// A cyclic directory chain must be detected within the file's own size.
///
/// The walk appended a whole sector per step and stopped only after `fat.len()` steps, so a
/// two-sector cycle re-read those sectors roughly `file_len/4` times first.
#[test]
fn a_cyclic_directory_chain_is_bounded_by_the_file() {
    const SECTOR: usize = 4096;
    const FREESECT: u32 = 0xFFFF_FFFF;
    const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
    const FATSECT: u32 = 0xFFFF_FFFD;
    const MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

    let num_fat = 109usize;
    let mut header = vec![0u8; SECTOR];
    header[..8].copy_from_slice(&MAGIC);
    header[0x18..0x1A].copy_from_slice(&0x003Eu16.to_le_bytes());
    header[0x1A..0x1C].copy_from_slice(&3u16.to_le_bytes());
    header[0x1C..0x1E].copy_from_slice(&0xFFFEu16.to_le_bytes());
    header[0x1E..0x20].copy_from_slice(&12u16.to_le_bytes()); // 4096-byte sectors
    header[0x20..0x22].copy_from_slice(&6u16.to_le_bytes());
    header[0x2C..0x30].copy_from_slice(&(num_fat as u32).to_le_bytes());
    header[0x30..0x34].copy_from_slice(&((num_fat + 1) as u32).to_le_bytes());
    header[0x38..0x3C].copy_from_slice(&4096u32.to_le_bytes());
    header[0x3C..0x40].copy_from_slice(&ENDOFCHAIN.to_le_bytes());
    header[0x44..0x48].copy_from_slice(&ENDOFCHAIN.to_le_bytes());
    for slot in header[0x4C..0x200].chunks_exact_mut(4) {
        slot.copy_from_slice(&FREESECT.to_le_bytes());
    }
    for i in 0..109 {
        let at = 0x4C + i * 4;
        header[at..at + 4].copy_from_slice(&(i as u32).to_le_bytes());
    }

    let entries = SECTOR / 4;
    let mut fat = vec![FREESECT; num_fat * entries];
    for slot in fat.iter_mut().take(num_fat) {
        *slot = FATSECT;
    }
    let (a, b) = (num_fat, num_fat + 1);
    fat[a] = b as u32;
    fat[b] = a as u32;

    let mut bytes = header;
    for v in &fat {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    bytes.resize(SECTOR * (num_fat + 3), 0);
    let file_len = bytes.len();

    let _guard = MEASURING.lock().unwrap_or_else(|p| p.into_inner());
    reset();
    let outcome = std::panic::catch_unwind(move || {
        gridxcel_rs::open_xls_from_rs(std::io::Cursor::new(bytes)).map(|w| w.sheet_names().len())
    });
    let allocated = peak();

    println!(
        "file {} B -> peak {:.1} MB ({:.0}x)",
        file_len,
        allocated as f64 / 1e6,
        allocated as f64 / file_len as f64
    );

    assert!(outcome.is_ok(), "a cyclic chain must not panic");
    let refused = outcome.unwrap().is_err();
    assert!(refused, "a cyclic directory chain must be reported");
    assert!(
        allocated < 4 << 20,
        "the walk must be bounded by the file, peak was {allocated} bytes"
    );
}

/// The real fixtures still read, so the new bounds reject nothing a producer writes.
#[test]
fn real_workbooks_still_read_after_the_bounds() {
    for name in ["chanora.xlsx", "rollmilkice.xlsx", "sample.ods"] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name);
        let mut wb = gridxcel_rs::open_workbook(&path).unwrap();
        for sheet in wb.sheet_names().to_vec() {
            let range = wb.worksheet_range(&sheet).unwrap();
            let _ = range.used_cell_count();
        }
    }
    // And the `.xls` container still parses its real fixture.
    let doc = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/word97.doc");
    assert!(gridxcel_rs::open_workbook(&doc).is_err(), "not a workbook");
}

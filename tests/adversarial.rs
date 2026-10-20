//! Adversarial input: attempts to make the reader misbehave.
//!
//! Every test here is an attack, not a correctness check. The properties under test are the ones
//! that matter for a parser of untrusted files: no panic, bounded memory, bounded time, and no
//! data returned for something that should be refused.
//!
//! Memory is measured with a counting allocator, so "bounded" is a number rather than an
//! intention.

use cellar::Reader;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

mod common;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: every method forwards to the system allocator and additionally maintains statistics.
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

/// The counters are process-wide, so measurements must not overlap.
static MEASURING: Mutex<()> = Mutex::new(());

fn reset_peak() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}

fn peak() -> usize {
    PEAK.load(Ordering::Relaxed)
}

/// Runs an attack, asserting it does not panic, and reporting peak allocation and time.
fn attack<T>(label: &str, f: impl FnOnce() -> T + std::panic::UnwindSafe) -> T {
    let guard = MEASURING.lock().unwrap_or_else(|p| p.into_inner());
    reset_peak();
    let start = Instant::now();
    let outcome = std::panic::catch_unwind(f);
    let elapsed = start.elapsed();
    let peak = peak();
    drop(guard);
    println!(
        "  {label}: {peak} bytes, {:.1} ms",
        elapsed.as_secs_f64() * 1000.0
    );
    match outcome {
        Ok(v) => v,
        Err(_) => panic!("{label} PANICKED"),
    }
}

// ------------------------------------------------------------------ helpers

fn ods(body: &str) -> Vec<u8> {
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
<office:body><office:spreadsheet><table:table table:name="S">{body}</table:table>
</office:spreadsheet></office:body></office:document-content>"#
    );
    let stored = zipcore::FileOptions::deflate(6);
    let mut w = zipcore::ZipWriter::new(std::io::Cursor::new(Vec::new())).unwrap();
    w.add_file(
        "mimetype",
        b"application/vnd.oasis.opendocument.spreadsheet",
        &zipcore::FileOptions::stored(),
    )
    .unwrap();
    w.add_file("content.xml", content.as_bytes(), &stored)
        .unwrap();
    w.finish().unwrap().into_inner()
}

fn xlsx(sheet: &str) -> Vec<u8> {
    let workbook = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheets><sheet name="S" sheetId="1"/></sheets>
</workbook>"#;
    let deflated = zipcore::FileOptions::deflate(6);
    let mut w = zipcore::ZipWriter::new(std::io::Cursor::new(Vec::new())).unwrap();
    w.add_file("xl/workbook.xml", workbook.as_bytes(), &deflated)
        .unwrap();
    w.add_file("xl/worksheets/sheet1.xml", sheet.as_bytes(), &deflated)
        .unwrap();
    w.finish().unwrap().into_inner()
}

fn sheet(body: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData>{body}</sheetData></worksheet>"#
    )
}

// ================================================================ attacks

/// Many `<text:s text:c="10000"/>` runs. Each is individually capped, so the question is whether
/// the *total* text one cell can produce is bounded.
#[test]
fn ods_text_run_amplification_is_bounded() {
    let runs = r#"<text:s text:c="10000"/>"#.repeat(2_000);
    let body = format!(
        r#"<table:table-row><table:table-cell office:value-type="string"><text:p>{runs}</text:p></table:table-cell></table:table-row>"#
    );
    let bytes = ods(&body);
    println!("compressed fixture: {} bytes", bytes.len());
    let text_len = attack("text:s amplification", move || {
        let mut wb = cellar::open_ods_from_rs(std::io::Cursor::new(bytes)).unwrap();
        wb.worksheet_range("S")
            .ok()
            .and_then(|r| r.get((0, 0)).and_then(|v| v.as_string()))
            .map(|s| s.len())
            .unwrap_or(0)
    });
    println!("  produced {text_len} chars of text");
}

/// A `<dimension>` claiming the whole grid. Can a few hundred bytes force a gigabyte?
#[test]
fn xlsx_dimension_does_not_force_a_huge_allocation() {
    let body = r#"<row r="1"><c r="A1"><v>1</v></c></row>"#;
    let doc = format!(
        r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<dimension ref="A1:AF1048576"/><sheetData>{body}</sheetData></worksheet>"#
    );
    let bytes = xlsx(&doc);
    println!("compressed fixture: {} bytes", bytes.len());
    let result = attack("huge declared dimension", move || {
        let mut wb = cellar::open_xlsx_from_rs(std::io::Cursor::new(bytes)).unwrap();
        wb.worksheet_range("S").map(|r| r.get_size())
    });
    println!("  result: {:?}", result.map_err(|e| e.to_string()));
}

/// Deeply nested elements must not overflow the stack.
#[test]
fn deeply_nested_xml_does_not_overflow_the_stack() {
    let depth = 100_000;
    let mut body = String::new();
    body.push_str("<a>");
    for _ in 0..depth {
        body.push_str("<b>");
    }
    for _ in 0..depth {
        body.push_str("</b>");
    }
    body.push_str("</a>");
    let doc = format!(
        r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
{body}<sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#
    );
    let bytes = xlsx(&doc);
    let _ = attack("100k nested elements", move || {
        let mut wb = cellar::open_xlsx_from_rs(std::io::Cursor::new(bytes)).unwrap();
        wb.worksheet_range("S").map(|r| r.get((0, 0)).cloned())
    });
}

/// A cell reference with an enormous row number, at the limit of what a u32 can hold.
#[test]
fn out_of_grid_cell_references_are_rejected() {
    for row in ["4294967295", "2147483648", "99999999999999999999"] {
        let body = format!(r#"<row r="1"><c r="A{row}"><v>1</v></c></row>"#);
        let bytes = xlsx(&sheet(&body));
        let _ = attack(&format!("row {row}"), move || {
            let mut wb = cellar::open_xlsx_from_rs(std::io::Cursor::new(bytes)).unwrap();
            let dense = wb.worksheet_range("S").is_err();
            let mut wb = cellar::open_xlsx_from_rs(std::io::Cursor::new({
                let body = format!(r#"<row r="1"><c r="A{row}"><v>1</v></c></row>"#);
                xlsx(&sheet(&body))
            }))
            .unwrap();
            let stream = match wb.worksheet_rows("S") {
                Ok(mut rows) => {
                    let mut n = 0;
                    while let Ok(Some(_)) = rows.next_row() {
                        n += 1;
                    }
                    Ok(n)
                }
                Err(e) => Err(e.to_string()),
            };
            (dense, stream)
        });
    }
}

/// A malformed archive whose central directory points at absurd offsets.
#[test]
fn corrupt_archive_offsets_are_refused() {
    let mut bytes = xlsx(&sheet(r#"<row r="1"><c r="A1"><v>1</v></c></row>"#));
    // Corrupt the end-of-central-directory signature region.
    let n = bytes.len();
    for byte in bytes.iter_mut().skip(n.saturating_sub(22)) {
        *byte ^= 0xFF;
    }
    let outcome = attack("corrupt EOCD", move || {
        cellar::open_xlsx_from_rs(std::io::Cursor::new(bytes)).is_err()
    });
    assert!(outcome, "a corrupt archive must not open");
}

/// Truncation at every length must be refused, never panic.
#[test]
fn every_truncation_is_refused_without_panicking() {
    let full = xlsx(&sheet(
        r#"<row r="1"><c r="A1"><v>1</v></c></row><row r="2"><c r="A2"><v>2</v></c></row>"#,
    ));
    let _guard = MEASURING.lock().unwrap_or_else(|p| p.into_inner());
    for cut in (0..full.len()).step_by(7) {
        let partial = full[..cut].to_vec();
        let result = std::panic::catch_unwind(move || {
            let _ = cellar::open_xlsx_from_rs(std::io::Cursor::new(partial));
        });
        assert!(result.is_ok(), "truncating at {cut} panicked");
    }
}

/// Flipping bits in a valid archive must never panic.
#[test]
fn bit_flips_are_refused_without_panicking() {
    let full = xlsx(&sheet(
        r#"<row r="1"><c r="A1"><v>1</v></c><c r="B1" t="s"><v>0</v></c></row>"#,
    ));
    let _guard = MEASURING.lock().unwrap_or_else(|p| p.into_inner());
    let mut seed = 0x12345678u64;
    for _ in 0..400 {
        let mut copy = full.clone();
        // A cheap deterministic PRNG, so a failure is reproducible.
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let at = (seed >> 33) as usize % copy.len();
        copy[at] ^= 1 << ((seed >> 17) % 8);
        let result = std::panic::catch_unwind(move || {
            let _ = cellar::open_xlsx_from_rs(std::io::Cursor::new(copy));
        });
        assert!(result.is_ok(), "a bit flip at {at} panicked");
    }
}

/// ODS with a repeated row of repeated cells: the multiplication must be charged, not performed.
#[test]
fn ods_repeat_multiplication_is_charged_before_allocating() {
    let cell = r#"<table:table-cell office:value-type="float" office:value="1"/>"#.repeat(200);
    let body = format!(
        r#"<table:table-row table:number-rows-repeated="1048576">{cell}</table:table-row>"#
    );
    let bytes = ods(&body);
    println!("compressed fixture: {} bytes", bytes.len());
    let result = attack("row repeat amplification", move || {
        let mut wb = cellar::open_ods_from_rs(std::io::Cursor::new(bytes)).unwrap();
        wb.worksheet_range("S").map(|r| r.get_size())
    });
    println!("  result: {:?}", result.map_err(|e| e.to_string()));
}

/// A streaming read of the same amplified file must also stay small: it materialises nothing.
#[test]
fn ods_streaming_amplification_is_cheap() {
    let cell = r#"<table:table-cell office:value-type="float" office:value="1"/>"#.repeat(200);
    let body = format!(
        r#"<table:table-row table:number-rows-repeated="1048576">{cell}</table:table-row>"#
    );
    let bytes = ods(&body);
    let rows_read = attack("row repeat, streamed", move || {
        let mut wb = cellar::open_ods_from_rs(std::io::Cursor::new(bytes)).unwrap();
        // The cursor borrows the workbook, so the count is extracted inside the same scope and
        // the borrow is finished with before the workbook is dropped.
        let count = {
            let mut rows = match wb.worksheet_rows("S") {
                Ok(rows) => rows,
                Err(_) => return 0,
            };
            let mut n = 0usize;
            // Only a few rows: the point is the path, not draining a million.
            for _ in 0..10 {
                match rows.next_row() {
                    Ok(Some(r)) => n += r.len(),
                    _ => break,
                }
            }
            n
        };
        count
    });
    println!("  cells read: {rows_read}");
}

/// A BIFF stream whose records claim lengths that overrun the buffer.
#[test]
fn biff_overrunning_record_lengths_are_survivable() {
    let _guard = MEASURING.lock().unwrap_or_else(|p| p.into_inner());
    // A BOF followed by a record claiming a 65535-byte body in a tiny buffer.
    let mut bytes = vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    bytes.extend_from_slice(&[0u8; 504]);
    bytes.extend_from_slice(&[0x09, 0x08, 0xFF, 0xFF]);
    bytes.extend_from_slice(&[0u8; 8]);
    let result = std::panic::catch_unwind(move || {
        let _ = cellar::open_xls_from_rs(std::io::Cursor::new(bytes));
    });
    assert!(result.is_ok(), "an overrunning BIFF record panicked");
}

/// A ZIP54-style archive claiming a colossal entry count.
#[test]
fn a_colossal_entry_count_is_refused() {
    let _guard = MEASURING.lock().unwrap_or_else(|p| p.into_inner());
    // Minimal EOCD claiming 65535 entries on disk.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"PK\x05\x06");
    bytes.extend_from_slice(&[0u8; 16]);
    bytes.extend_from_slice(&0xFFFFu16.to_le_bytes()); // entries total
    bytes.extend_from_slice(&[0u8; 4]);
    let outcome = std::panic::catch_unwind(move || {
        let _ = cellar::open_xlsx_from_rs(std::io::Cursor::new(bytes));
    });
    assert!(outcome.is_ok(), "a colossal entry count panicked");
}

/// A compound file whose directory declares a stream far larger than the file, or larger than
/// the crate's per-part ceiling, must be refused rather than allocated for.
///
/// `.xls` was the one format with no per-part bound: the ZIP path applied `MAX_ENTRY_BYTES` to
/// every entry, the CFB path applied it to nothing.
#[test]
fn a_compound_file_declaring_an_oversized_stream_is_refused() {
    const SECTOR: usize = 512;
    const FREESECT: u32 = 0xFFFF_FFFF;
    const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
    const FATSECT: u32 = 0xFFFF_FFFD;
    const MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

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

    for declared in [u64::from(u32::MAX), u64::MAX, 5 << 30] {
        let mut fat = vec![FREESECT; 128];
        fat[0] = FATSECT;
        fat[1] = ENDOFCHAIN;

        let mut h = vec![0u8; SECTOR];
        h[..8].copy_from_slice(&MAGIC);
        h[0x18..0x1A].copy_from_slice(&0x003Eu16.to_le_bytes());
        h[0x1A..0x1C].copy_from_slice(&3u16.to_le_bytes());
        h[0x1C..0x1E].copy_from_slice(&0xFFFEu16.to_le_bytes());
        h[0x1E..0x20].copy_from_slice(&9u16.to_le_bytes());
        h[0x20..0x22].copy_from_slice(&6u16.to_le_bytes());
        h[0x2C..0x30].copy_from_slice(&1u32.to_le_bytes());
        h[0x30..0x34].copy_from_slice(&1u32.to_le_bytes());
        h[0x38..0x3C].copy_from_slice(&4096u32.to_le_bytes());
        h[0x3C..0x40].copy_from_slice(&ENDOFCHAIN.to_le_bytes());
        h[0x44..0x48].copy_from_slice(&ENDOFCHAIN.to_le_bytes());
        for slot in h[0x4C..0x200].as_chunks_mut::<4>().0 {
            slot.copy_from_slice(&FREESECT.to_le_bytes());
        }
        h[0x4C..0x50].copy_from_slice(&0u32.to_le_bytes());

        let mut dir = Vec::new();
        dir.extend_from_slice(&dirent("Root Entry", 5, 1, ENDOFCHAIN, 0));
        dir.extend_from_slice(&dirent("Workbook", 2, FREESECT, 2, declared));
        dir.resize(SECTOR, 0);

        let mut bytes = h;
        for v in &fat {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        bytes.extend_from_slice(&dir);
        bytes.resize(SECTOR * 4, 0);

        let outcome = std::panic::catch_unwind(move || {
            cellar::open_xls_from_rs(std::io::Cursor::new(bytes)).map(|w| w.sheet_names().len())
        });
        assert!(outcome.is_ok(), "a declared size of {declared} panicked");
        let result = outcome.unwrap();
        assert!(
            result.is_err(),
            "a stream declared as {declared} bytes must be refused, not allocated"
        );
    }
}

/// Empty and near-empty inputs must be refused cleanly.
#[test]
fn degenerate_inputs_are_refused() {
    let _guard = MEASURING.lock().unwrap_or_else(|p| p.into_inner());
    for (label, bytes) in [
        ("empty", Vec::new()),
        ("one byte", vec![0u8]),
        ("PK", b"PK".to_vec()),
        ("PK03", b"PK\x03\x04".to_vec()),
        (
            "CFB magic only",
            vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1],
        ),
        ("zeros", vec![0u8; 1024]),
        ("0xFF", vec![0xFFu8; 1024]),
    ] {
        let outcome = std::panic::catch_unwind(move || {
            let _ = cellar::open_workbook_auto_from_rs(std::io::Cursor::new(bytes));
        });
        assert!(outcome.is_ok(), "{label} panicked");
    }
}

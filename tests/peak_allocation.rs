//! Guards the memory the reader uses while building a sheet.
//!
//! A dense range costs far more than the file it came from, so how the sheet is assembled decides
//! whether a large workbook is readable at all. Collecting the cells into a sparse list and then
//! converting it holds the sheet twice at the moment of peak use — a little over twice the cost of
//! the finished range, plus the list itself.
//!
//! This measures peak allocation with a counting allocator rather than asserting a design, because
//! that difference is invisible to a correctness test and easy to reintroduce.
//!
//! The bound is deliberately loose enough to survive a dependency's internal buffer changing, but
//! tight enough that reverting to the sparse-then-dense conversion fails it.

mod common;

use cellar::{Data, Reader};
use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: every method forwards to the system allocator and additionally keeps two counters that
// are only ever used as statistics. No allocation is served from anywhere else, and the counters
// cannot affect what is returned.
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

fn reset_peak() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}

fn peak() -> usize {
    PEAK.load(Ordering::Relaxed)
}

/// The counters are process-wide, so a test that measures them must not run alongside another test
/// that allocates: a sibling's allocations land in the middle of the measurement.
static MEASURING: Mutex<()> = Mutex::new(());

fn sole_measurer() -> MutexGuard<'static, ()> {
    MEASURING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

const ROWS: u32 = 2000;
const COLS: u32 = 50;
/// Cells in the sheet.
const CELLS: usize = ROWS as usize * COLS as usize;

/// Writes a plain workbook of filled numeric cells, with the extent declared as OOXML declares it.
///
/// Deflated, as a real workbook is: a stored part has no decompression to pay for, which is exactly
/// the cost these measurements are about.
fn write_workbook(path: &std::path::Path) {
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut writer = zip::ZipWriter::new(std::fs::File::create(path).unwrap());

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
    let last_column = cellar::refs::col_to_letters(COLS - 1);
    writer
        .write_all(
            format!(
                r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<dimension ref="A1:{last_column}{ROWS}"/>
<sheetData>"#
            )
            .as_bytes(),
        )
        .unwrap();
    for row in 1..=ROWS {
        writer
            .write_all(format!(r#"<row r="{row}">"#).as_bytes())
            .unwrap();
        for col in 0..COLS {
            writer
                .write_all(
                    format!(
                        r#"<c r="{}{row}"><v>{}</v></c>"#,
                        cellar::refs::col_to_letters(col),
                        row * COLS + col
                    )
                    .as_bytes(),
                )
                .unwrap();
        }
        writer.write_all(b"</row>").unwrap();
    }
    writer.write_all(b"</sheetData></worksheet>").unwrap();
    writer.finish().unwrap();
}

#[test]
fn reading_a_sheet_does_not_hold_it_twice() {
    let _measuring = sole_measurer();
    let path = crate::common::scratch_file("cellar-peak-allocation.xlsx");
    write_workbook(&path);

    // Open and read once so that one-off costs ??the shared string table, the archive index ??are
    // behind us and the measurement covers assembling the sheet.
    {
        let mut workbook = cellar::open_workbook(&path).unwrap();
        assert_eq!(
            workbook.worksheet_range("S").unwrap().used_cell_count(),
            CELLS
        );
    }

    reset_peak();
    let mut workbook = cellar::open_workbook(&path).unwrap();
    let range = workbook.worksheet_range("S").unwrap();
    let measured = peak();
    let dense = CELLS * std::mem::size_of::<cellar::Data>();

    assert_eq!(range.used_cell_count(), CELLS);
    crate::common::cleanup(&path);

    // Assembling the sheet must not cost several times the range it produces. Holding the cells
    // as a sparse list as well, and then cloning that list, comes to well over five times.
    assert!(
        measured < dense * 3,
        "peak allocation was {measured} bytes for a {dense} byte range ({:.1}x); \
         the sheet is being held more than once",
        measured as f64 / dense as f64
    );
}

/// Writes a workbook whose two cells sit at opposite corners of the grid.
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

/// A window read must stop reading the sheet once it holds the window.
///
/// The sheet part here is a few megabytes. Reading a window at the top must not hold it all ??a
/// window read that reads the part whole allocates about the size of the file and throws nearly all
/// of it away.
#[test]
fn a_window_read_does_not_pay_for_the_rest_of_the_sheet() {
    let _measuring = sole_measurer();
    let path = crate::common::scratch_file("cellar-window-peak.xlsx");
    write_workbook(&path);
    let file_size = std::fs::metadata(&path).unwrap().len();

    // Warm up so one-off costs are behind the measurement.
    {
        let mut workbook = cellar::open_workbook(&path).unwrap();
        let _ = workbook.worksheet_range_ref("S", "A1").unwrap();
    }

    reset_peak();
    let mut workbook = cellar::open_workbook(&path).unwrap();
    let window = workbook.worksheet_range_ref("S", "A1").unwrap();
    let measured = peak();
    crate::common::cleanup(&path);

    // The fixture numbers cells `row * COLS + col` with rows counted from one.
    assert_eq!(window.get((0, 0)), Some(&Data::Int(COLS as i64)));
    // Measured at 87 KB, most of it the inflater's own window. The bound leaves room for a
    // dependency's buffers to change while still failing if the read stops stopping: taking the
    // 2.9 MB part whole costs about the size of the part.
    assert!(
        measured < 256 << 10,
        "a window read allocated {measured} bytes from a {file_size} byte file, so it read the \
         rest of the sheet instead of stopping at the window"
    );
}

/// Streaming must cost what the rows cost, not what the rectangle between them would have cost.
///
/// The rectangle here is 17,179,869,184 cells ??over half a terabyte if it were materialised,
/// which is why the dense path refuses it. Streaming reads both values instead.
#[test]
fn streaming_a_sparse_sheet_costs_a_constant_amount() {
    let _measuring = sole_measurer();
    let path = crate::common::scratch_file("cellar-sparse-peak.xlsx");
    write_sparse_workbook(&path);

    reset_peak();
    let mut workbook = cellar::open_workbook(&path).unwrap();
    let mut cells = Vec::new();
    let mut rows = workbook.worksheet_rows("S").unwrap();
    while let Some(row) = rows.next_row().unwrap() {
        for (column, value) in row.iter() {
            cells.push((row.index(), column, value.clone()));
        }
    }
    let measured = peak();
    crate::common::cleanup(&path);

    assert_eq!(cells.len(), 2);
    assert!(
        measured < 1 << 20,
        "streaming a two-cell sheet allocated {measured} bytes; the implied rectangle would be \
         over 500 GB, so this must not scale with it"
    );
}

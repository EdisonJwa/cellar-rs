//! Compares `cellar` against `calamine` on the same files and the same operations.
//!
//! Both are pure-Rust spreadsheet readers with the same interface shape, so this measures what a
//! caller actually does: open a workbook, read a whole sheet, stream it, and read one window.
//! Each is best-of-N to reduce scheduler noise.
//!
//! Fixtures are generated rather than shipped, so the sheet size is controlled: a 4000 x 50
//! numeric sheet in each format. The shipped `.ods` is also measured, separately, because it
//! exposes a robustness difference worth recording.
//!
//! Run with `cargo run --release` from this directory.

mod fixtures;

use std::time::{Duration, Instant};

fn best_of(iterations: usize, mut f: impl FnMut() -> u64) -> (Duration, u64) {
    let mut best = Duration::MAX;
    let mut counted = 0u64;
    for _ in 0..iterations {
        let start = Instant::now();
        let cells = f();
        let elapsed = start.elapsed();
        if elapsed < best {
            best = elapsed;
            counted = cells;
        }
    }
    (best, counted)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn run_cellar(path: &std::path::Path, sheet: &str) -> Vec<(&'static str, Duration, u64)> {
    use cellar::Reader;
    let mut out = Vec::new();

    let (e, c) = best_of(25, || {
        let mut wb = cellar::open_workbook(path).unwrap();
        std::hint::black_box(wb.sheet_names().len());
        std::hint::black_box(wb.worksheet_range(sheet).unwrap().used_cell_count() as u64)
    });
    out.push(("open + first sheet", e, c));

    let (e, c) = best_of(20, || {
        let mut wb = cellar::open_workbook(path).unwrap();
        std::hint::black_box(wb.worksheet_range(sheet).unwrap().as_slice().len() as u64)
    });
    out.push(("read whole sheet", e, c));

    let (e, c) = best_of(20, || {
        let mut wb = cellar::open_workbook(path).unwrap();
        let mut rows = wb.worksheet_rows(sheet).unwrap();
        let mut n = 0u64;
        while let Some(row) = rows.next_row().unwrap() {
            n += row.len() as u64;
        }
        std::hint::black_box(n)
    });
    out.push(("stream all rows", e, c));

    let (e, c) = best_of(25, || {
        let mut wb = cellar::open_workbook(path).unwrap();
        std::hint::black_box(wb.worksheet_range_ref(sheet, "B2:F40").unwrap().as_slice().len() as u64)
    });
    out.push(("read window B2:F40", e, c));

    out
}

fn run_calamine(path: &std::path::Path, sheet: &str) -> Result<Vec<(&'static str, Duration, u64)>, String> {
    use calamine::{open_workbook_auto, Data, Reader};
    let mut out = Vec::new();

    let (e, c) = best_of(25, || {
        let mut wb = open_workbook_auto(path).unwrap();
        std::hint::black_box(wb.sheet_names().len());
        std::hint::black_box(wb.worksheet_range(sheet).unwrap().used_cells().count() as u64)
    });
    out.push(("open + first sheet", e, c));

    let (e, c) = best_of(20, || {
        let mut wb = open_workbook_auto(path).unwrap();
        let range = wb.worksheet_range(sheet).unwrap();
        let (h, w) = range.get_size();
        std::hint::black_box((h * w) as u64)
    });
    out.push(("read whole sheet", e, c));

    // calamine has no row-streaming API, so the nearest equivalent is a dense read walked row
    // by row. Reported so the gap is visible rather than hidden by omission.
    let (e, c) = best_of(20, || {
        let mut wb = open_workbook_auto(path).unwrap();
        let range = wb.worksheet_range(sheet).unwrap();
        let mut n = 0u64;
        for row in range.rows() {
            n += row.iter().filter(|c| **c != Data::Empty).count() as u64;
        }
        std::hint::black_box(n)
    });
    out.push(("stream all rows", e, c));

    // calamine has no windowed read: it materialises the sheet and slices it, so this pays for
    // the whole sheet. Reported rather than omitted, and labelled as such.
    let (e, c) = best_of(25, || {
        let mut wb = open_workbook_auto(path).unwrap();
        let range = wb.worksheet_range(sheet).unwrap();
        let mut n = 0u64;
        for row in 1..40usize {
            for col in 1..6usize {
                if range.get((row, col)) != Some(&Data::Empty) {
                    n += 1;
                }
            }
        }
        std::hint::black_box(n)
    });
    out.push(("read window B2:F40", e, c));

    // Recorded so the difference is visible in the table rather than inferred.
    let (e, c) = best_of(25, || {
        let mut wb = open_workbook_auto(path).unwrap();
        let range = wb.worksheet_range(sheet).unwrap();
        std::hint::black_box(range.used_cells().count() as u64)
    });
    out.push(("(materialise, no window)", e, c));

    Ok(out)
}

fn compare(label: &str, path: &std::path::Path, sheet: &str) {
    println!("=== {label}");

    let ours = run_cellar(path, sheet);
    let theirs = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_calamine(path, sheet)
    })) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            println!("  cellar reads this file; calamine fails: {e}\n");
            return;
        }
        Err(_) => {
            println!("  cellar reads this file; calamine panics on it\n");
            return;
        }
    };

    println!(
        "  {:<24} {:>11} {:>11} {:>9}",
        "operation", "cellar", "calamine", "ratio"
    );
    for (i, a) in ours.iter().enumerate() {
        // The last calamine row is its materialise-only measurement and has no cellar
        // counterpart, so it is printed on its own rather than paired with something else.
        let b = theirs.get(i);
        match b {
            Some(b) => {
                let ratio = ms(b.1) / ms(a.1).max(1e-9);
                let note = if a.2 != 0 && b.2 != 0 && a.2 != b.2 {
                    format!("  (cells {} vs {})", a.2, b.2)
                } else {
                    String::new()
                };
                println!(
                    "  {:<24} {:>8.3} ms {:>8.3} ms {:>8.2}x{}",
                    a.0,
                    ms(a.1),
                    ms(b.1),
                    ratio,
                    note
                );
            }
            None => println!("  {:<24} {:>8.3} ms", a.0, ms(a.1)),
        }
    }
    if let Some(extra) = theirs.get(ours.len()) {
        println!(
            "  {:<24} {:>22.3} ms   (cellar has no equivalent: it never materialises)",
            extra.0,
            ms(extra.1)
        );
    }
    println!();
}

fn main() {
    // Fixtures go beside the crate rather than in the system temp directory, which is not
    // reliably writable on every platform this runs on.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    let xlsx = dir.join("cellar-compare.xlsx");
    let ods = dir.join("cellar-compare.ods");
    let shipped_ods = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/data/sample.ods");
    let conforming_ods = dir.join("cellar-compare-shipped.ods");
    let real_xlsx = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/data/chanora.xlsx");

    fixtures::write_xlsx(&xlsx);
    fixtures::write_ods(&ods);
    fixtures::rewrite_slashes(&shipped_ods, &conforming_ods);

    println!(
        "fixture entry names conform to the ZIP spec (all '/'):\n  \
         synthetic .xlsx: {}\n  synthetic .ods:  {}\n  shipped .ods:    {}  <- committed fixture\n",
        fixtures::names_are_conforming(&xlsx),
        fixtures::names_are_conforming(&ods),
        fixtures::names_are_conforming(&shipped_ods),
    );

    let cells = fixtures::ROWS as u64 * fixtures::COLS as u64;
    println!(
        "cellar vs calamine - best of N, release build\n\
         synthetic sheet: {} x {} = {cells} numeric cells\n",
        fixtures::ROWS,
        fixtures::COLS
    );

    compare("synthetic .xlsx (4000 x 50)", &xlsx, "S");
    compare("synthetic .ods (4000 x 50)", &ods, "S");
    compare("real chanora.xlsx (Identifications)", &real_xlsx, "Identifications");
    compare("shipped sample.ods (forward-slash copy)", &conforming_ods, "Sheet1");

    // Recorded separately: the committed fixture is a real example of a Windows-produced
    // Recorded separately: the committed fixture is a real example of a Windows-produced
    // archive with non-conforming entry names.
    println!("=== shipped sample.ods (as committed, backslash entry names)");
    let cellar_ok = {
        use cellar::Reader;
        let mut wb = cellar::open_workbook(&shipped_ods).unwrap();
        wb.worksheet_range("Sheet1").is_ok()
    };
    println!("  cellar reads it: {cellar_ok}");
    match std::panic::catch_unwind(|| run_calamine(&shipped_ods, "Sheet1")) {
        Ok(Ok(_)) => println!("  calamine reads it too"),
        Ok(Err(e)) => println!("  calamine fails: {e}"),
        Err(_) => println!("  calamine panics on it"),
    }
    println!();
}

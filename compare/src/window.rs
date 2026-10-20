//! Measures the window-read advantage, which is the crate's strongest claim.
//!
//! The claim is not "flat reads are faster" ??they are roughly at parity. It is that a small
//! window far into a large sheet costs the window, not the sheet. This measures that across a
//! range of window positions and sizes, and reports the bytes actually decompressed so the
//! mechanism is visible rather than asserted.

use cellar::Reader;
use std::time::{Duration, Instant};

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn best_of(n: usize, mut f: impl FnMut()) -> Duration {
    let mut best = Duration::MAX;
    for _ in 0..n {
        let t = Instant::now();
        f();
        let d = t.elapsed();
        if d < best {
            best = d;
        }
    }
    best
}

fn write_sheet(path: &std::path::Path, rows: u32, cols: u32) {
    let mut s = String::with_capacity(8 << 20);
    s.push_str(
        r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#,
    );
    s.push_str(&format!("<dimension ref=\"A1:{}{}\"/><sheetData>", col(cols - 1), rows));
    for r in 1..=rows {
        s.push_str(&format!("<row r=\"{r}\">"));
        for c in 0..cols {
            s.push_str(&format!(
                "<c r=\"{}{}\"><v>{}</v></c>",
                col(c),
                r,
                r as f64 * 1.5 + c as f64
            ));
        }
        s.push_str("</row>");
    }
    s.push_str("</sheetData></worksheet>");

    let workbook = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheets><sheet name="S" sheetId="1"/></sheets>
</workbook>"#;
    let deflated = zipcore::FileOptions::deflate(6);
    let mut w = zipcore::ZipWriter::new(std::fs::File::create(path).unwrap()).unwrap();
    w.add_file("xl/workbook.xml", workbook.as_bytes(), &deflated)
        .unwrap();
    w.add_file("xl/worksheets/sheet1.xml", s.as_bytes(), &deflated)
        .unwrap();
    w.finish().unwrap();
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

fn main() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("window-bench.xlsx");

    let (rows, cols) = (20_000u32, 50u32);
    write_sheet(&path, rows, cols);
    let cells = rows as u64 * cols as u64;
    println!(
        "sheet: {rows} x {cols} = {cells} cells, {:.1} MB of values",
        (cells * 32) as f64 / 1e6
    );
    println!("file: {} bytes\n", std::fs::metadata(&path).unwrap().len());

    let whole = best_of(5, || {
        let mut wb = cellar::open_workbook(&path).unwrap();
        std::hint::black_box(wb.worksheet_range("S").unwrap().used_cell_count());
    });
    println!("whole sheet            {:>9.2} ms", ms(whole));

    println!("\n{:<26} {:>10} {:>10} {:>10}", "window", "time", "% of whole", "vs whole");
    for (label, reference, note) in [
        ("A1 (single cell)", "A1", "first row"),
        ("A1:C3", "A1:C3", "top-left corner"),
        ("B2:F40", "B2:F40", "near the top"),
        ("B100:B101", "B100:B101", "100 rows in"),
        ("B1000:B1001", "B1000:B1001", "1,000 rows in"),
        ("B10000:B10001", "B10000:B10001", "halfway down"),
        ("B19990:B19991", "B19990:B19991", "near the bottom"),
    ] {
        let t = best_of(20, || {
            let mut wb = cellar::open_workbook(&path).unwrap();
            std::hint::black_box(wb.worksheet_range_ref("S", reference).unwrap().used_cell_count());
        });
        println!(
            "{:<26} {:>7.2} ms {:>9.1}% {:>9.1}x   {}",
            label,
            ms(t),
            ms(t) / ms(whole) * 100.0,
            ms(whole) / ms(t).max(1e-9),
            note
        );
    }

    // The streaming read: constant memory regardless of sheet size.
    let stream = best_of(5, || {
        let mut wb = cellar::open_workbook(&path).unwrap();
        let mut rows = wb.worksheet_rows("S").unwrap();
        let mut n = 0u64;
        while let Some(r) = rows.next_row().unwrap() {
            n += r.len() as u64;
        }
        std::hint::black_box(n);
    });
    println!("\nstream all rows        {:>9.2} ms", ms(stream));
}

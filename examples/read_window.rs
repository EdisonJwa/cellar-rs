//! Read one region of a sheet.
//!
//! `worksheet_range_ref` returns exactly the rectangle named by an A1-style reference. Positions
//! inside it that hold nothing come back as `Data::Empty`, so the result's shape follows the
//! reference rather than the data — which is what distinguishes it from `worksheet_range`.
//!
//! The cost is the window, not the sheet: in OOXML the part is read only as far as the window's
//! last row, so a window near the top of a large sheet is nearly free.
//!
//! Run with:
//!
//! ```text
//! cargo run --example read_window -- tests/data/chanora.xlsx Summary B2:D6
//! ```

use cellar::{open_workbook, Data, Reader};
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| {
        eprintln!("usage: read_window <workbook> <sheet> <range>");
        eprintln!();
        eprintln!("Try:");
        eprintln!("  cargo run --example read_window -- tests/data/chanora.xlsx Summary B2:D6");
        std::process::exit(2);
    });
    let sheet = args.next().unwrap_or_else(|| "Summary".into());
    let reference = args.next().unwrap_or_else(|| "B2:D6".into());

    let mut workbook = open_workbook(&path)?;

    // Time the window against reading the whole sheet, so the difference is visible rather than
    // taken on faith.
    let window_start = Instant::now();
    let window = workbook.worksheet_range_ref(&sheet, &reference)?;
    let window_time = window_start.elapsed();

    let whole_start = Instant::now();
    let whole = workbook.worksheet_range(&sheet)?;
    let whole_time = whole_start.elapsed();

    println!("{sheet}!{reference}");
    println!("  window:      {:?}", window_time);
    println!("  whole sheet: {:?}", whole_time);
    if window_time.as_nanos() > 0 {
        let ratio = whole_time.as_secs_f64() / window_time.as_secs_f64().max(1e-9);
        println!(
            "  ratio:       {ratio:.2}x  ({} cells against {})",
            window.used_cell_count(),
            whole.used_cell_count()
        );
    }
    // A window's advantage is proportional to how much of the sheet it skips, so on a small
    // sheet it can be no faster at all — the fixed cost of opening dominates.
    println!("  note:        the saving scales with what the window skips, so a small");
    println!("               sheet shows little of it");

    println!("\n  the window's shape follows the reference, not the data:");
    println!(
        "  start {:?}, end {:?}, size {:?}",
        window.start(),
        window.end(),
        window.get_size()
    );

    for row in window.rows() {
        let rendered: Vec<String> = row.iter().map(cell).collect();
        println!("  {}", rendered.join(" | "));
    }
    Ok(())
}

fn cell(value: &Data) -> String {
    match value {
        Data::Empty => "·".into(),
        other => other.as_string().unwrap_or_else(|| format!("{other:?}")),
    }
}

//! Stream a sheet's rows without materialising it.
//!
//! `worksheet_rows` hands back one row at a time, so memory is the widest row rather than the
//! sheet's rectangle. Use it for a large or sparse sheet, which the dense read would either
//! refuse or waste gigabytes on.
//!
//! Run with:
//!
//! ```text
//! cargo run --example stream_rows -- tests/data/chanora.xlsx Identifications
//! ```

use cellar::{open_workbook, Reader};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| {
        eprintln!("usage: stream_rows <workbook> [sheet]");
        eprintln!();
        eprintln!("Try:");
        eprintln!("  cargo run --example stream_rows -- tests/data/chanora.xlsx Identifications");
        std::process::exit(2);
    });

    let mut workbook = open_workbook(&path)?;
    // Default to the first sheet when none is named.
    let sheet = args
        .next()
        .or_else(|| workbook.sheet_names().first().cloned())
        .ok_or("the workbook has no sheets")?;

    println!("{sheet}");
    println!("  row   cells  first values");

    let mut rows = workbook.worksheet_rows(&sheet)?;
    let mut total_cells = 0usize;
    let mut row_count = 0usize;
    let mut widest = 0usize;

    while let Some(row) = rows.next_row()? {
        row_count += 1;
        total_cells += row.len();
        widest = widest.max(row.len());

        // Print the first few rows and a summary afterwards, so a large sheet does not scroll.
        if row_count <= 10 {
            let sample: Vec<String> = row
                .iter()
                .take(4)
                .map(|(column, value)| {
                    let text = value.as_string().unwrap_or_default();
                    format!("{column}={}", truncate(&text, 18))
                })
                .collect();
            println!(
                "  {:>4}  {:>5}  {}",
                row.index(),
                row.len(),
                sample.join(" ")
            );
        }
    }

    println!();
    println!("  rows:        {row_count}");
    println!("  cells:       {total_cells}");
    println!("  widest row:  {widest} cells");
    println!("  memory:      the widest row, not the sheet");
    Ok(())
}

fn truncate(text: &str, max: usize) -> String {
    let flat = text.replace('\n', " ");
    if flat.chars().count() <= max {
        flat
    } else {
        let mut out: String = flat.chars().take(max).collect();
        out.push('…');
        out
    }
}

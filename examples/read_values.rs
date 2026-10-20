//! Read every cell of every sheet.
//!
//! The straightforward path: open a workbook, walk its sheets, and print what each holds. This
//! materialises one sheet at a time as a dense rectangle, which is the right shape for anything
//! that fits comfortably in memory.
//!
//! Run with:
//!
//! ```text
//! cargo run --example read_values -- tests/data/chanora.xlsx
//! ```

use gridxcel_rs::{open_workbook, Data, Reader};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: read_values <workbook>");
        eprintln!();
        eprintln!("Try one of the bundled fixtures:");
        eprintln!("  cargo run --example read_values -- tests/data/chanora.xlsx");
        eprintln!("  cargo run --example read_values -- tests/data/sample.ods");
        std::process::exit(2);
    });

    let mut workbook = open_workbook(&path)?;

    println!("{}", path);
    println!("  format: {:?}", workbook.format());
    println!("  sheets: {}", workbook.sheet_names().len());

    for name in workbook.sheet_names().to_vec() {
        let range = workbook.worksheet_range(&name)?;
        let (rows, cols) = range.get_size();
        println!("\n{name}  ({rows} rows x {cols} columns)");

        for row in range.rows() {
            let rendered: Vec<String> = row.iter().map(describe).collect();
            // Trim trailing empties so the output stays readable.
            let width = rendered
                .iter()
                .rposition(|c| !c.is_empty())
                .map(|i| i + 1)
                .unwrap_or(0);
            if width == 0 {
                continue;
            }
            println!("  {}", rendered[..width].join(" | "));
        }
    }

    Ok(())
}

/// A cell as text, with the type visible for anything that a spreadsheet would render differently.
fn describe(value: &Data) -> String {
    match value {
        Data::Empty => String::new(),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => f.to_string(),
        Data::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Data::String(s) => s.replace('\n', " "),
        // A serial needs its epoch to mean anything, so show what it converts to instead.
        Data::DateTime(dt) => dt
            .to_ymd_hms()
            .map(|v| v.to_string())
            .unwrap_or_else(|| format!("serial {}", dt.as_f64())),
        Data::DateTimeIso(s) => s.clone(),
        Data::DurationIso(s) => s.clone(),
        Data::Error(e) => e.to_string(),
    }
}

//! Dumps the first rows of every sheet in a workbook.
//!
//! Run with `cargo run --example dump -- <file> [rows]`.

use cellar::{open_workbook, HeaderRow, Reader};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: dump <workbook> [rows]");
            std::process::exit(2);
        }
    };
    let limit: usize = std::env::args()
        .nth(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);

    let mut workbook = open_workbook(&path)?;
    println!("file: {path}");
    println!("format: {:?}", workbook.format());
    println!("sheets: {:?}", workbook.sheet_names());
    if !workbook.defined_names().is_empty() {
        println!("defined names: {:?}", workbook.defined_names());
    }
    if let HeaderRow::Row(n) = workbook.header_row() {
        println!("header row: {n}");
    }

    for (index, name) in workbook.sheet_names().to_vec().into_iter().enumerate() {
        let visibility = workbook.sheet_visibility(index);
        let range = workbook.worksheet_range(&name)?;
        if visibility.is_visible() {
            println!("\n=== {name} {:?}", range.get_size());
        } else {
            println!("\n=== {name} {:?} ({visibility})", range.get_size());
        }
        println!("used cells: {}", range.used_cell_count());
        let merged = workbook.worksheet_merged_regions(&name)?;
        if !merged.is_empty() {
            let shown: Vec<String> = merged.iter().take(8).map(|m| m.to_a1()).collect();
            let more = if merged.len() > shown.len() {
                format!(", … {} more", merged.len() - shown.len())
            } else {
                String::new()
            };
            println!("merged: {}{more}", shown.join(" "));
        }
        for row in range.rows().take(limit) {
            println!("{row:?}");
        }
    }
    Ok(())
}

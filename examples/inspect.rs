//! Read formulas, merged regions and sheet metadata.
//!
//! Values and formulas are read separately, because a sheet usually needs one or the other. The
//! cached result sits next to the formula in the file, so values are available without asking for
//! formula text.
//!
//! Run with:
//!
//! ```text
//! cargo run --example inspect -- tests/data/sample.ods
//! ```

use gridxcel_rs::{open_workbook, Error, Reader};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: inspect <workbook>");
        eprintln!();
        eprintln!("Try:");
        eprintln!("  cargo run --example inspect -- tests/data/sample.ods");
        eprintln!("  cargo run --example inspect -- tests/data/chanora.xlsx");
        std::process::exit(2);
    });

    let mut workbook = open_workbook(&path)?;

    println!("{}", path);
    println!("  format: {:?}", workbook.format());

    if !workbook.defined_names().is_empty() {
        println!("\ndefined names:");
        for (name, target) in workbook.defined_names() {
            println!("  {name} -> {target}");
        }
    }

    for (index, name) in workbook.sheet_names().to_vec().into_iter().enumerate() {
        let visibility = workbook.sheet_visibility(index);
        let range = workbook.worksheet_range(&name)?;

        println!("\n{name}  {:?}", range.get_size());
        if !visibility.is_visible() {
            println!("  visibility: {visibility}");
        }

        // Merged regions tell a "merged" cell apart from an empty one.
        let merged = workbook.worksheet_merged_regions(&name)?;
        if merged.is_empty() {
            println!("  merged: none");
        } else {
            println!("  merged: {} region(s)", merged.len());
            for region in merged.iter().take(5) {
                let anchor = range.value_or_empty(region.start);
                println!(
                    "    {region}  {}x{} cells, anchor = {}",
                    region.rows(),
                    region.columns(),
                    anchor.as_string().unwrap_or_else(|| "·".into())
                );
            }
            if merged.len() > 5 {
                println!("    ??{} more", merged.len() - 5);
            }
        }

        // `.xls` stores formulas as compiled token streams rather than text, and says so rather
        // than returning nothing.
        match workbook.worksheet_formula(&name) {
            Ok(formulas) if formulas.used_cell_count() > 0 => {
                println!("  formulas: {}", formulas.used_cell_count());
                for (row, column, text) in formulas.used_cells().take(5) {
                    println!("    ({row}, {column}) = {text}");
                }
            }
            Ok(_) => println!("  formulas: none"),
            Err(Error::Unsupported(why)) => println!("  formulas: unavailable ??{why}"),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

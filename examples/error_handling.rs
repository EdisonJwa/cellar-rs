//! Handle failures by kind.
//!
//! Failures are typed rather than stringly-message'd, so a caller can act on the difference
//! between "this needs a password", "this file is damaged" and "this is a valid file using
//! something the reader does not implement". A malformed file never panics; it always arrives as
//! an `Err`.
//!
//! Run with any file at all — the point is that every input produces a useful answer:
//!
//! ```text
//! cargo run --example error_handling -- tests/data/chanora.xlsx
//! cargo run --example error_handling -- tests/data/word97.doc
//! cargo run --example error_handling -- README.md
//! ```

use gridxcel_rs::{open_workbook, Error, Reader};
use std::path::Path;

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: error_handling <file>");
        eprintln!();
        eprintln!("Try:");
        eprintln!("  cargo run --example error_handling -- tests/data/chanora.xlsx");
        eprintln!("  cargo run --example error_handling -- tests/data/word97.doc");
        eprintln!("  cargo run --example error_handling -- README.md");
        std::process::exit(2);
    });

    println!("{}", path);
    match open_workbook(&path) {
        Ok(mut workbook) => {
            println!("  opened as {:?}", workbook.format());
            let names = workbook.sheet_names().to_vec();
            println!("  {} sheet(s): {:?}", names.len(), names);
            // A missing sheet is a caller error, not corruption, and says so.
            if let Err(e) = workbook.worksheet_range("No Such Sheet") {
                println!("  asking for a missing sheet -> {e}");
            }
        }
        Err(Error::Io(e)) => {
            // The file could not be read at all.
            println!("  io error: {e}");
            println!("  -> the path is wrong or unreadable, not a problem with the contents");
        }
        Err(Error::Encrypted) => {
            println!("  encrypted");
            println!("  -> decryption is out of scope; a password would be required");
        }
        Err(Error::Unsupported(what)) => {
            // A valid file using something this reader deliberately does not implement.
            println!("  unsupported: {what}");
            println!("  -> the file is fine; this reader does not handle that feature");
        }
        Err(Error::NotFound(what)) => {
            println!("  not found: {what}");
        }
        Err(e) if e.is_corrupt() => {
            // Damage, as distinct from a limitation.
            println!("  damaged: {e}");
            println!("  -> the contents are not a spreadsheet this reader can parse");
        }
        Err(e) => println!("  failed: {e}"),
    }

    // The same distinction is available without matching, when the only question is "is this
    // file damaged or merely unsupported".
    if Path::new(&path).exists() {
        if let Err(e) = open_workbook(&path) {
            println!("\n  is_corrupt() = {}", e.is_corrupt());
        }
    }
}

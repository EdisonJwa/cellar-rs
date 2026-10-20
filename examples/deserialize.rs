//! Deserialize rows into typed records.
//!
//! Requires the `serde` feature:
//!
//! ```text
//! cargo run --example deserialize --features serde -- tests/data/chanora.xlsx Identifications
//! ```
//!
//! Fields are matched to columns **by header name**, so a struct works even when the sheet's
//! columns are in a different order, and extra columns are ignored.

#[cfg(feature = "serde")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use gridxcel_rs::{open_workbook, RangeDeserializerBuilder, Reader};
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct Record {
        /// Matched to the column whose header reads "Identified By".
        #[serde(rename = "Identified By")]
        identified_by: String,
        #[serde(rename = "File path")]
        path: String,
        /// A column that may hold blanks or text; anything unusable becomes `None`.
        #[serde(
            rename = "Hits",
            default,
            deserialize_with = "gridxcel_rs::de::deserialize_as_i64_or_none"
        )]
        hits: Option<i64>,
    }

    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: deserialize <workbook> [sheet]");
        eprintln!();
        eprintln!(
            "  cargo run --example deserialize --features serde -- \
             tests/data/chanora.xlsx Identifications"
        );
        std::process::exit(2);
    });

    let mut workbook = open_workbook(&path)?;
    let sheet = std::env::args()
        .nth(2)
        .or_else(|| workbook.sheet_names().first().cloned())
        .ok_or("the workbook has no sheets")?;

    let range = workbook.worksheet_range(&sheet)?;
    println!("{sheet}: {:?}", range.get_size());

    // Without headers, columns are matched by position.
    let mut reader = RangeDeserializerBuilder::new().from_range::<Record>(&range)?;
    println!("  {} columns per record", reader.width());

    let mut shown = 0;
    let mut ok = 0usize;
    for (index, record) in reader.by_ref().enumerate() {
        match record {
            Ok(record) => {
                ok += 1;
                if shown < 5 {
                    println!(
                        "  [{index}] by={:?} path={:?} hits={:?}",
                        truncate(&record.identified_by, 22),
                        truncate(&record.path, 34),
                        record.hits
                    );
                    shown += 1;
                }
            }
            Err(e) => {
                // One bad row does not stop the iteration; the error names the row.
                if shown < 5 {
                    println!("  [{index}] skipped: {e}");
                    shown += 1;
                }
            }
        }
    }
    println!("  {ok} record(s) deserialized");
    Ok(())
}

#[cfg(feature = "serde")]
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(max).collect();
        out.push('…');
        out
    }
}

#[cfg(not(feature = "serde"))]
fn main() {
    eprintln!("This example needs the `serde` feature:");
    eprintln!();
    eprintln!(
        "  cargo run --example deserialize --features serde -- \
         tests/data/chanora.xlsx Identifications"
    );
    std::process::exit(2);
}

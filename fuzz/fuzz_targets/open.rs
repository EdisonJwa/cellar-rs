//! Fuzzes the whole read path with arbitrary bytes.
//!
//! This is the target that matters most for this crate: every format here parses binary input
//! that a caller did not write, so the properties under test are "never panics", "never hangs"
//! and "never allocates beyond the documented bounds" — not "reads the file correctly".
//!
//! Run with `cargo +nightly fuzz run open`.

#![no_main]

use gridxcel_rs::Reader;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Detection plus all three readers: the container, the XML tokenizer and the BIFF record
    // stream are all reachable from here.
    let Ok(mut workbook) = gridxcel_rs::open_workbook_auto_from_rs(std::io::Cursor::new(data.to_vec()))
    else {
        return;
    };

    // Opening succeeded, so exercise the metadata and then every sheet.
    let names = workbook.sheet_names().to_vec();
    for name in &names {
        if let Ok(range) = workbook.worksheet_range(name) {
            // Touch the cells so the values are actually materialised and compared.
            let _ = range
                .used_cells()
                .map(|(r, c, v)| (r, c, v.as_string()))
                .count();
        }
        // `.xls` reports unsupported here by design; the error path must not panic either.
        let _ = workbook.worksheet_formula(name);

        // A window decides for itself where the part is cut, and a streaming read decides where it
        // stops, so both reach parser paths the dense read never does: a part truncated at a row
        // boundary, and a cut rejected because the rows did not arrive in the order it assumed.
        for reference in ["A1", "A1:B2", "B2:A1", "A1:XFD1048576", "ZZZ99999:AAAA100000"] {
            if let Ok(range) = workbook.worksheet_range_ref(name, reference) {
                let _ = range.used_cell_count();
            }
        }
        if let Ok(mut rows) = workbook.worksheet_rows(name) {
            // A few rows, not the sheet: the point is the path, and a fuzzer should not spend its
            // budget draining whatever it happened to build.
            for _ in 0..4 {
                match rows.next_row() {
                    Ok(Some(row)) => {
                        let _ = row.iter().count();
                    }
                    Ok(None) | Err(_) => break,
                }
            }
        }
    }
    let _ = workbook.defined_names().len();

    // Each concrete reader is also driven directly, so a file whose detection disagrees with its
    // contents still gets parsed by the format that claims it.
    let bytes = data.to_vec();
    if let Ok(mut xlsx) = gridxcel_rs::open_xlsx_from_rs(std::io::Cursor::new(bytes.clone())) {
        let _ = xlsx.worksheets();
    }
    if let Ok(mut xls) = gridxcel_rs::open_xls_from_rs(std::io::Cursor::new(bytes.clone())) {
        let _ = xls.worksheets();
    }
    if let Ok(mut ods) = gridxcel_rs::open_ods_from_rs(std::io::Cursor::new(bytes)) {
        let _ = ods.worksheets();
    }
});

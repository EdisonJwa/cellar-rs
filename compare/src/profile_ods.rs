//! Where does a whole-sheet read actually spend its time?
//!
//! A benchmark that reports only a total cannot say which phase to attack, and guessing at the
//! answer is how this project's own documentation once named the wrong ODS bottleneck. This
//! splits a read into phases that can be measured independently.
//!
//! ```text
//! cargo run --release --bin profile_ods -- <workbook> [sheet]
//! ```
//!
//! - **open** — archive open plus the metadata walk. `.ods` inflates `content.xml` and scans it
//!   for table names here, while `.xlsx` reads only the workbook part, so the two formats are not
//!   directly comparable on this line.
//! - **parse cells** — the streaming path: every cell decoded, no rectangle built.
//! - **assemble** — the distance between a dense read and a streaming one: what it costs to turn
//!   parsed rows into the rectangle the caller asked for.

use gridxcel_rs::Reader;
use std::time::{Duration, Instant};

fn best_of<F: FnMut() -> usize>(runs: usize, mut f: F) -> (Duration, usize) {
    let mut best = Duration::MAX;
    let mut last = 0;
    for _ in 0..runs {
        let t = Instant::now();
        last = f();
        let e = t.elapsed();
        if e < best {
            best = e;
        }
    }
    (best, last)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| {
        eprintln!("usage: profile_ods <workbook> [sheet]");
        eprintln!();
        eprintln!(
            "  cargo run --release --bin profile_ods -- target/fixtures/gridxcel-compare.ods S"
        );
        std::process::exit(2);
    });

    // Default to the first sheet, so the tool works on any workbook without arguments.
    let sheet = match args.next() {
        Some(s) => s,
        None => {
            let wb = gridxcel_rs::open_workbook(&path).expect("open");
            let names = wb.sheet_names().to_vec();
            names
                .first()
                .cloned()
                .expect("the workbook has no sheets")
        }
    };

    let runs: usize = std::env::var("RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);

    let (dense, cells) = best_of(runs, || {
        let mut wb = gridxcel_rs::open_workbook(&path).expect("open");
        wb.worksheet_range(&sheet)
            .expect("read")
            .used_cell_count()
    });

    let (open, _) = best_of(runs, || {
        let wb = gridxcel_rs::open_workbook(&path).expect("open");
        wb.sheet_names().len()
    });

    let (stream, streamed) = best_of(runs, || {
        let mut wb = gridxcel_rs::open_workbook(&path).expect("open");
        let mut rows = wb.worksheet_rows(&sheet).expect("rows");
        let mut n = 0;
        while let Some(row) = rows.next_row().expect("row") {
            n += row.len();
        }
        n
    });

    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let parse = (ms(stream) - ms(open)).max(0.0);
    let assemble = (ms(dense) - ms(stream)).max(0.0);

    println!("{path}");
    println!("  sheet {sheet:?}, best of {runs}");
    println!("  {cells} used cells, {streamed} streamed\n");
    println!("  open        {:7.2} ms", ms(open));
    println!("  parse cells {parse:7.2} ms   XML scan + value decode");
    println!("  assemble    {assemble:7.2} ms   parsed rows -> dense rectangle");
    println!("  ----------  {:7.2} ms   dense read", ms(dense));
    println!();
    println!(
        "  parse is {:.0}% of the dense read, assembly {:.0}%",
        parse / ms(dense) * 100.0,
        assemble / ms(dense) * 100.0
    );
}

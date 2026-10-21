//! A fair comparison under a noisy machine.
//!
//! This box runs Docker, a vendor utility and a browser between measurements, so absolute
//! milliseconds drift by more than the differences being measured. Interleaving the two readers
//! and comparing them *within one run* removes that drift: whatever the machine is doing, both
//! pay for it.
//!
//! ```text
//! cargo run --release --bin ab -- <workbook> <sheet> [rounds]
//! ```

use std::time::{Duration, Instant};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| {
        eprintln!("usage: ab <workbook> <sheet> [rounds]");
        std::process::exit(2);
    });
    let sheet = args.next().unwrap_or_else(|| "S".to_string());
    let rounds: usize = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);

    let mut ours = Vec::new();
    let mut theirs = Vec::new();

    for _ in 0..rounds {
        // Interleaved: ours, then theirs, one after the other, every round.
        let t = Instant::now();
        {
            use gridxcel_rs::Reader;
            let mut wb = gridxcel_rs::open_workbook(&path).expect("open");
            std::hint::black_box(
                wb.worksheet_range(&sheet)
                    .expect("read")
                    .used_cell_count(),
            );
        }
        ours.push(t.elapsed());

        let t = Instant::now();
        {
            use calamine::Reader as _;
            let mut wb = calamine::open_workbook_auto(&path).expect("open");
            std::hint::black_box(
                wb.worksheet_range(&sheet)
                    .expect("read")
                    .used_cells()
                    .count(),
            );
        }
        theirs.push(t.elapsed());
    }

    let med = |v: &mut Vec<Duration>| {
        v.sort_unstable();
        v[v.len() / 2]
    };
    let min = |v: &[Duration]| *v.iter().min().expect("non-empty");

    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let (om, tm) = (med(&mut ours), med(&mut theirs));

    println!("{path}  sheet {sheet:?}  {rounds} interleaved rounds\n");
    println!("  gridxcel-rs   median {:7.2} ms   best {:7.2} ms", ms(om), ms(min(&ours)));
    println!("  calamine      median {:7.2} ms   best {:7.2} ms", ms(tm), ms(min(&theirs)));
    println!();
    println!(
        "  median ratio  {:.2}x   (>1 means gridxcel-rs is faster)",
        ms(tm) / ms(om)
    );
    println!(
        "  best ratio    {:.2}x",
        ms(min(&theirs)) / ms(min(&ours))
    );
}

# gridxcel-rs

[![CI](https://github.com/EdisonJwa/gridxcel-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/EdisonJwa/gridxcel-rs/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/gridxcel-rs.svg)](https://crates.io/crates/gridxcel-rs)
[![docs.rs](https://docs.rs/gridxcel-rs/badge.svg)](https://docs.rs/gridxcel-rs)
[![license](https://img.shields.io/crates/l/gridxcel-rs.svg)](LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue.svg)](Cargo.toml)

**A pure-Rust reader for Excel and OpenDocument spreadsheet files.**

No `unsafe`. One dependency. Reads `.xlsx`, `.xlsm`, `.xls`, `.ods` from untrusted files without
panicking, and answers a question about forty cells on a two-million-row sheet in under a
millisecond.

```toml
[dependencies]
gridxcel-rs = "0.1"
```

```rust
use gridxcel_rs::{open_workbook, Reader};

let mut workbook = open_workbook("report.xlsx")?;
let range = workbook.worksheet_range("Sheet1")?;

for row in range.rows() {
    for cell in row {
        print!("{}\t", cell.as_string().unwrap_or_default());
    }
    println!();
}
# Ok::<(), gridxcel_rs::Error>(())
```

---

## Why this exists

The `zip` crate's reader is the single point of failure for every pure-Rust spreadsheet reader. On
2026-10-16 its maintainer yanked six major version lines in three minutes:

```
zip 9.0.3   live          zip 6.0.0   yanked
zip 9.0.2   yanked        zip 5.1.1   yanked
zip 9.0.0   yanked        zip 4.6.1   yanked
zip 8.6.0   yanked        zip 3.0.0   yanked
zip 7.4.0   yanked        zip 2.6.1   yanked
```

`calamine` pins `zip = "^8.6"`, and `8.6.0` was the only `8.6.x` ever published. With every
version satisfying `^8.6` yanked, a fresh `cargo build` could not resolve its own dependency tree
— taking the ecosystem down over something no format reader had done wrong.

So here the archive container sits behind one internal trait, served by
[`zipcore`](https://docs.rs/zipcore) — this project's own ZIP reader, which parses the container
itself. The OLE compound-file reader (`.xls`) and the XML tokenizer (`.xlsx`, `.ods`) are written
from scratch in this crate too. **The entire parsing stack is this project's own code.**

## What it reads

| Format | Extensions | Container |
|---|---|---|
| OOXML workbook | `.xlsx` `.xlsm` `.xltx` `.xlam` | ZIP + XML |
| OpenDocument spreadsheet | `.ods` | ZIP + XML |
| Legacy BIFF8 workbook | `.xls` (Excel 97–2003) | OLE compound file |

Values, cached formula results, formula text, cell errors, dates in both epochs, merged regions,
sheet visibility, defined names and sheet metadata all come out as typed data. The crate is
read-only.

Format detection is by **content**, not extension, so a renamed file still reads correctly.

## Quick start

### Read a sheet

```rust
use gridxcel_rs::{open_workbook, Reader};

let mut workbook = open_workbook("sales.xlsx")?;

for name in workbook.sheet_names().to_vec() {
    let range = workbook.worksheet_range(&name)?;
    println!("{name}: {:?}", range.get_size());

    for row in range.rows() {
        for cell in row {
            print!("{}\t", cell.as_string().unwrap_or_default());
        }
        println!();
    }
}
# Ok::<(), gridxcel_rs::Error>(())
```

### Cell values

A cell is a `Data`, which distinguishes the cases a spreadsheet actually has:

```rust
use gridxcel_rs::{CellError, Data, ExcelDateTime};

Data::Int(42)                                       // a whole number
Data::Float(1.5)                                    // a fractional number
Data::String("text".into())
Data::Bool(true)
Data::DateTime(ExcelDateTime::new(45321.5, false))  // a serial, with its epoch
Data::DateTimeIso("2024-03-01T13:45:30".into())     // ODS stores dates as text
Data::DurationIso("PT1H30M".into())                 // ODS time values
Data::Error(CellError::Div0)                        // #DIV/0! and friends
Data::Empty                                         // no value
```

Excel has no integer type, so an integral value is reported as `Data::Int`. That is what a reader
wants for an ID or a count column, and loses nothing: the file stores `2` and `2.0` identically.

Forgiving accessors mean you rarely match by hand:

```rust
# let value = gridxcel_rs::Data::Float(2.5);
value.as_string();   // Some(String) for anything renderable
value.as_f64();      // numbers, booleans, date serials, and numeric text
value.as_i64();
value.as_bool();
value.as_date();     // YmdHms, from either date representation
value.as_duration(); // days, from a serial or an ISO-8601 duration
```

### Dates

A serial is held **with its epoch**, so a 1904-system workbook is never silently reinterpreted
under the 1900 system. Excel's fabricated 1900-02-29 is handled the way every mainstream reader
handles it: serial 60 converts to 1900-03-01.

```rust
use gridxcel_rs::ExcelDateTime;

let dt = ExcelDateTime::new(45321.5, false);
assert_eq!(dt.to_ymd_hms().unwrap().to_string(), "2024-01-30 12:00:00");
assert_eq!(dt.to_unix_timestamp(), Some(1_706_616_000));

// With the `dates` feature:
// let naive = dt.to_naive_datetime();
```

### Stream a large sheet

`worksheet_range` builds the sheet's dense rectangle, which for a large or sparse sheet is the
wrong shape: a file whose cells sit at `A1` and `XFD1048576` implies **17,179,869,184 cells**, and
that path refuses rather than try to allocate them.

`worksheet_rows` reads the same sheet without materialising anything:

```rust
use gridxcel_rs::{open_workbook, Reader};

let mut workbook = open_workbook("huge.xlsx")?;
let mut rows = workbook.worksheet_rows("Sheet1")?;

while let Some(row) = rows.next_row()? {
    // Only the populated cells of this row are visited.
    for (column, value) in row.iter() {
        println!("({}, {column}) = {value:?}", row.index());
    }
}
# Ok::<(), gridxcel_rs::Error>(())
```

- **Memory is the widest row, not the area of the sheet.** Row buffers are reused, so a long sheet
  allocates nothing per row.
- **A row carries only the cells that hold values**, so one value in column 16000 costs one entry
  rather than 16,001. `row.get(column)` binary-searches.
- **Rows the file declares with no values are still rows**, with their own index, so a gap cannot
  be mistaken for the end of the data.
- **Rows already parsed are never re-materialised.** An ODS row that `table:number-rows-repeated`
  says occurs a million times is re-emitted from one buffered row.

Both views are checked against each other cell for cell on every real fixture, so they cannot
drift apart.

### Read one region

`worksheet_range_ref` returns exactly the rectangle named by an A1-style reference:

```rust
# use gridxcel_rs::{open_workbook, Reader};
# let mut workbook = open_workbook("tests/data/chanora.xlsx")?;
let totals = workbook.worksheet_range_ref("Identifications", "B2:F40")?;
assert_eq!(totals.get_size(), (39, 5));
# Ok::<(), gridxcel_rs::Error>(())
```

Its shape follows the reference rather than the data, so positions inside the window that hold
nothing come back as `Data::Empty`. That is what distinguishes it from `worksheet_range`. The
corners may be given either way round.

Cost is the window, not the sheet: in OOXML the part is read only as far as the window's last row,
so decompression stops early. On a 20,000 × 50 sheet:

| Window | Time | vs a whole-sheet read |
|---|---|---|
| `A1` (one cell) | 0.19 ms | **877× faster** |
| `B2:F40` | 0.46 ms | 365× faster |
| `B10000:B10001` | 67 ms | 2.5× faster |
| `B19990:B19991` | 137 ms | 1.2× faster |

The saving scales with what a window skips, which is why the table lists both ends. A window near
the top of a sheet — headers, a summary block, a preview — is essentially free. A window at the
bottom is not: bytes before it are not seekable inside a deflated part, so the reader still scans
up to that row. What is constant is that rows *below* the window are never decompressed.

### Formulas

Values and formulas are read separately, because a sheet usually needs one or the other:

```rust
# use gridxcel_rs::{open_workbook, Reader};
# let mut workbook = open_workbook("report.xlsx")?;
let values   = workbook.worksheet_range("Sheet1")?;
let formulas = workbook.worksheet_formula("Sheet1")?;
# Ok::<(), gridxcel_rs::Error>(())
```

Shared formulas — where Excel writes the text once and references it from the rest of a
filled-down column — are expanded into real formula text per cell:

```
master at C2:  =SUM($B2:D2)
C3:            =SUM($B3:D3)
D2:            =SUM($B2:E2)
```

Absolute (`$`) parts are preserved, string literals and quoted sheet names are left alone, and
things that merely *look* like references are not rewritten: `LOG10(...)`, `1E5`, defined names
and structured table references (`Table1[Column1]`) all survive intact.

### Merged cells

A merged cell keeps its value in its top-left position and leaves the rest of the region blank, so
values alone cannot tell "merged" from "empty":

```rust
# use gridxcel_rs::{open_workbook, Reader};
# let mut workbook = open_workbook("report.xlsx")?;
for region in workbook.worksheet_merged_regions("Sheet1")? {
    println!("{region} covers {}x{}", region.rows(), region.columns());
    let anchor = workbook.worksheet_range("Sheet1")?.value_or_empty(region.start);
    println!("  {anchor:?}");
}
# Ok::<(), gridxcel_rs::Error>(())
```

Corners are **inclusive**, matching how every format states a merge — which differs from `Range`'s
exclusive bottom-right. A merge over `A1:B2` reports `start == (0, 0)` and `end == (1, 1)`.
`to_a1()` renders it back, and `contains()` says whether a cell falls inside.

### Sheet visibility

A sheet is visible, hidden, or very hidden. Hiding is presentation, not access control: a hidden
sheet's cells read normally.

```rust
# use gridxcel_rs::{open_workbook, Reader};
# let mut workbook = open_workbook("report.xlsx")?;
for (index, name) in workbook.sheet_names().to_vec().iter().enumerate() {
    let visibility = workbook.sheet_visibility(index);
    if visibility.is_visible() {
        println!("{name}");
    } else {
        println!("{name} ({visibility})");   // "hidden" or "veryHidden"
    }
}
# Ok::<(), gridxcel_rs::Error>(())
```

### Deserialize rows (feature `serde`)

```toml
gridxcel-rs = { version = "0.1", features = ["serde"] }
```

Fields are matched to columns **by header name**, so a struct works even when the sheet's columns
are in a different order, and extra columns are ignored.

```rust
use gridxcel_rs::{open_workbook, RangeDeserializerBuilder, Reader};
use serde::Deserialize;

#[derive(Deserialize)]
struct Record {
    metric: String,
    value: f64,
    #[serde(default, deserialize_with = "gridxcel_rs::de::deserialize_as_f64_or_none")]
    noise: Option<f64>,
}

let mut workbook = open_workbook("data.xlsx")?;
let range = workbook.worksheet_range("Sheet1")?;

let records: Vec<Record> = RangeDeserializerBuilder::new()
    .from_range(&range)?
    .collect::<Result<Vec<_>, _>>()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

Blanks become `None` for `Option` fields. Without headers, columns are matched by position.

### Header row

```rust
# use gridxcel_rs::{open_workbook, HeaderRow, Reader};
# let mut workbook = open_workbook("report.xlsx")?;
workbook.with_header_row(HeaderRow::Row(2));
let range = workbook.worksheet_range("Sheet1")?;
# Ok::<(), gridxcel_rs::Error>(())
```

Trimming keeps coordinates absolute, so after asking for row 2 the range starts at `(1, 0)`.
Iterating `rows()` is unaffected.

### Sheet metadata

```rust
# use gridxcel_rs::{open_workbook, Reader};
# let mut workbook = open_workbook("report.xlsx")?;
workbook.sheet_names();          // &[String], in workbook order
workbook.defined_names();        // &[(String, String)] of name and target
workbook.sheet_index("Sheet1");  // Option<usize>
workbook.worksheet_range_at(0);  // by position instead of name
# Ok::<(), gridxcel_rs::Error>(())
```

### Error handling

Failures are typed rather than stringly-message'd, and a malformed file never panics:

```rust
use gridxcel_rs::Error;

match gridxcel_rs::open_workbook(path) {
    Err(Error::Encrypted)        => { /* needs a password */ }
    Err(Error::Unsupported(m))   => { /* valid file, unimplemented feature */ }
    Err(Error::NotFound(name))   => { /* no such sheet */ }
    Err(e) if e.is_corrupt()     => { /* damaged or not a spreadsheet */ }
    other => { /* ... */ }
}
```

## Features

Nothing is enabled by default.

| Feature | Enables | Pulls in |
|---|---|---|
| `serde` | `RangeDeserializerBuilder`, row → typed records | `serde` |
| `dates` | `ExcelDateTime::to_naive_*` | `chrono` |
| `codepage` | Correct legacy `.xls` text via `encoding_rs` | `encoding_rs` |

```toml
gridxcel-rs = { version = "0.1", features = ["serde", "dates", "codepage"] }
```

### Legacy `.xls` text and the `codepage` feature

BIFF stores each string as UTF-16 or "compressed" as one byte per character in the workbook's code
page. Which one is recorded per string, but the code page itself lives in a separate record, so
decoding waits until the globals substream has been walked.

Without `codepage`, a compressed byte becomes its Latin-1 code point. That is exact for ASCII and
for the ISO-8859 pages whose high range agrees with Latin-1, but **wrong** for the Windows code
pages between `0x80` and `0x9F` — where the euro sign, the typographic quotes and the en dash live.
Ordinary Western European text hits that band.

The feature closes it, covering the Windows pages (874, 1250–1258), the ISO-8859 pages, KOI8-R/U,
IBM866 and Macintosh. A code page it does not know falls back to Latin-1 rather than guessing.

## Examples

```text
cargo run --example read_values    -- tests/data/chanora.xlsx
cargo run --example stream_rows    -- tests/data/chanora.xlsx Identifications
cargo run --example read_window    -- tests/data/chanora.xlsx Identifications B2:F40
cargo run --example inspect        -- tests/data/sample.ods
cargo run --example error_handling -- tests/data/word97.doc
cargo run --example deserialize --features serde -- tests/data/chanora.xlsx Identifications
```

## Performance

Against [`calamine`](https://crates.io/crates/calamine) on the same files doing the same work. The
comparison lives in `compare/`, a separate workspace so `calamine` is never a dependency of this
crate: `cd compare && cargo run --release`.

A 4000 × 50 numeric sheet (200,000 cells), best of N:

| Workload | gridxcel-rs | calamine |
|---|---|---|
| `.xlsx` open + first sheet | 31 ms | 40 ms |
| `.xlsx` read whole sheet | 31 ms | 39 ms |
| `.xlsx` stream rows | 29 ms | 40 ms |
| `.xlsx` read window `B2:F40` | **0.46 ms** | 39 ms |
| `.ods` read whole sheet | 71 ms | 39 ms |
| real `chanora.xlsx`, whole sheet | 1.4 ms | 1.5 ms |
| real `chanora.xlsx`, window | 0.30 ms | 1.4 ms |

`calamine` has no windowed read — it materialises the sheet and slices it, so it pays 39 ms to
answer a question about 195 cells. That is a design difference, not a micro-optimisation.

### Where this crate is actually ahead

Stated narrowly, because a list of advantages that includes parity is not a list of advantages.

**Structural, and not copyable without a redesign:**

1. **A window costs the window, not the sheet.** Decompression is abandoned once the window's rows
   have gone by, and that cut is *verified against the parser* rather than trusted — so a part with
   out-of-order rows or a `</row>` inside a comment is read whole instead of truncated.
2. **One sheet view costs one allocation.** Cells are written into their final positions as they
   are parsed, so a dense read never holds the sheet twice. Measured at 6.1 MB peak against
   16.6 MB for the sparse-then-dense shape, and a test fails if that reverts.
3. **Streaming materialises nothing.** A sheet whose rectangle is 17 billion cells reads in
   constant space, through the same decoder the dense path uses.
4. **The container is ours.** `zipcore`: no `unsafe`, ZIP64, CRC verification, three dependencies
   (`crc32fast`, `miniz_oxide`, `getrandom`) instead of eight.

**Parity, not a moat.** Flat whole-sheet reads are ~1.3× faster on a synthetic sheet and
indistinguishable on a real one. Opening is metadata-only, which is table stakes.

**Behind, and says so.** `.ods` whole-sheet reads are ~1.4× slower than `calamine`. The cause is
the format, not the code: ODS states each cell as a full element with its attributes, so the same
200,000 cells occupy 13.3 MB against 5.9 MB in OOXML. Per megabyte this parser is already ahead
(5.0 ms/MB against `calamine`'s 5.25); there is simply 2.26× as much to walk. Assembly into a
dense rectangle is 12% of the read, so it is not where the time goes.

Two restructurings were tried and reverted, both measured with `compare`'s interleaved `ab` tool:
resuming at a recorded offset instead of re-walking tables before the wanted one (no change beyond
noise), and driving the dense read through `SheetBuilder` rather than a sparse list (5× slower,
because a cell-at-a-time write loses to one conversion when the column count grows row by row).
Streaming remains the answer for an `.ods` too large to materialise, where it is well ahead of a
dense read.

## Hardening

A reader takes untrusted input, so the usual resource traps are closed rather than documented away.

- **Decompression bombs.** No archive entry is decompressed past 4 GiB, and the limit applies to
  bytes actually produced, not to a declared length a crafted header can understate.
- **One per-part ceiling, everywhere.** The same 4 GiB bound covers compound-file streams, which
  previously had no equivalent limit — `.xls` was the one format that could allocate without bound.
- **The cell budget counts payloads, not just slots.** A `Data` slot is 32 bytes whether it is
  empty or owns a megabyte of text. Cells all naming one shared string passed the slot budget while
  allocating far more heap than it allowed — a 49 KB file measured at 23 MB, and unbounded at a
  larger shape. Text is now charged as it is stored, against a separate 256 MiB budget, and refusal
  lands before the allocation.
- **Repeat-count amplification.** ODS and BIFF express padding as a repeat count, so a few hundred
  bytes can stand for millions of cells. The budget is spent *as each cell is produced*, a row is
  capped at the grid width, and a repeat that would carry an index past the grid is **refused**
  rather than saturated.
- **Text amplification.** `<text:s text:c="N"/>` is a few bytes standing for N characters. Each
  element is capped and a cell's total text is capped; without the second bound, 656 bytes expanded
  to 40 MB.
- **Declared extents reserve a bounded amount.** `A1:AF1048576` is ~100 bytes naming exactly the
  grid's 33.5 million cells — a 1 GiB allocation before a cell is read. A hint now reserves at most
  64 MiB, and the buffer still grows to the true extent.
- **Archive entry counts.** More than 65,536 parts is rejected, and the index grows as entries are
  read rather than preallocated from a file-supplied count.
- **XML entity expansion.** DTDs are skipped rather than resolved, and there are no external entity
  fetches, so billion-laughs and XXE have nothing to work with. The entity decoder is linear, not
  quadratic.
- **Dense sheets.** Materialising a sheet is capped by a 1 GiB budget, and going over returns a
  clear error instead of an allocation the process cannot survive.
- **Cyclic allocation chains.** Every OLE sector chain walk is bounded and cycle-checked.
- **Directory slots keep their positions.** Compound-file entries address one another by slot
  index, so an unused slot is retained rather than compacted away — otherwise one deleted stream
  would shift every later index and hide the workbook.

`unsafe` is forbidden crate-wide. The read path is fuzzed for panics, hangs and unbounded
allocation rather than for correctness. See [SECURITY.md](SECURITY.md).

## Tests

```
cargo test --all-features
```

**206 tests** across ten suites. Four exist because of defects found in review, and each is a
guard rather than a demonstration:

| Suite | What it pins |
|---|---|
| `adversarial` | Attacks, not correctness: truncation at every offset, 400 bit flips, 100k-deep nesting, amplification, oversized declared sizes. Memory measured with a counting allocator. |
| `regressions` | One test per defect found in review, each carrying a note on why the input is dangerous. |
| `payload_budget` | The cell budget accounts for what a cell holds, not only its slot. |
| `entities` | The entity decoder stays linear; text that only looks like an entity survives. |
| `merged_regions`, `visibility`, `container` | Reader-level features and the container contract. |
| `streaming`, `peak_allocation`, `real_files` | The three sheet views held against each other, and their memory bounds. |

The fixtures are deliberately not self-generated where a real file was available:

- `tests/data/chanora.xlsx` — a nine-sheet report from a real scan tool.
- `tests/data/rollmilkice.xlsx` — a smaller real export.
- `tests/data/word97.doc` — a genuine OLE compound file, used to validate the container reader's
  FAT, mini-FAT and directory-tree walking.
- `tests/data/sample.ods` — exercises repeat counts, every ODS value type, and `text:s` runs. Note
  it was zipped on Windows and stores backslash separators, which the ZIP spec does not allow; it
  is kept as-is because it is a genuine example of what tools emit, and the reader tolerates it.

`.xls` is tested against a constructed fixture rather than a real file: a structurally valid BIFF8
workbook assembled in-memory, with a real FAT and directory, covering every cell record type.

## Design

```
src/
  container.rs   the swappable archive trait, ZIP backend (zipcore), format sniffing
  cfb.rs         OLE compound file reader, from scratch (used by .xls)
  xml.rs         XML pull parser, from scratch
  refs.rs        A1 reference parsing and rendering (public)
  formula.rs     shared-formula reference shifting
  datatype.rs    Data, CellError, ExcelDateTime
  range.rs       the dense Range<T> and the sheet builder
  rows.rs        streaming rows and windowed reads
  reader.rs      the Reader trait, header handling, merged regions, visibility
  de.rs          serde row deserialization (feature `serde`)
  open.rs        format detection and the open_* entry points
  formats/
    xlsx.rs      OOXML
    ods.rs       OpenDocument
    xls.rs       BIFF8
```

Three properties are worth calling out:

- **Sheets are parsed on demand.** Opening a workbook reads metadata only. The shared string table
  is not touched until a sheet needs it, so a workbook with millions of strings you never read costs
  nothing. `.xls` is the exception, because its sheet substreams are addressed by offset in a stream
  that has to be read whole.
- **Everything is bounds-checked.** No slicing without a length check, no unchecked arithmetic on
  file-supplied values, no panics on malformed input.
- **Coordinates are absolute.** A range knows where it sits in the sheet, so a cell keeps its
  identity across trimming and merging.

## Deliberate limitations

Stated plainly, because knowing where a reader stops matters as much as where it works.

- **Read-only.** Writing is not implemented.
- **No XLSB.** The MS-XLSB binary format is not supported; detected and rejected with
  `Error::Unsupported` naming it.
- **No Excel 5.0/95 (BIFF5/BIFF7).** A `.xls` predating Excel 97 is rejected with
  `Error::Unsupported` rather than misread.
- **`.xls` formula text is not recovered.** BIFF stores formulas as compiled token streams;
  decompiling them is not implemented. `worksheet_formula` returns `Error::Unsupported`. Cached
  **values** are fully available.
- **`.xls` defined names are not extracted.** A `Lbl` record's value is a compiled token stream, so
  the names could be listed but not what they refer to.
- **An ODS `<text:list>` inside a cell** is flattened into the cell's text rather than modelled.
- **ODS `META-INF/manifest.xml` is not read**, so an ODF-encrypted package surfaces as an XML error
  rather than `Error::Encrypted`.
- **Code pages outside the single-byte set are not decoded.** A double-byte page such as Shift_JIS
  cannot express its text one byte per character, so such a workbook writes UTF-16 instead.
- **Row structs cannot use `#[serde(flatten)]` or `#[serde(deny_unknown_fields)]`.** Both require a
  keyed visitor, and rows are read positionally so no field-name key is built per row.
- **Deserialized fields must be owned.** The row deserializer requires `DeserializeOwned`.
- **Dates convert assuming UTC.** An Excel serial is a wall-clock value with no zone attached.
- **No caching layer.** `worksheet_range` re-parses the sheet part on every call.
- **No formatting, charts, images or pivot definitions.**
- **A sheet is materialised as a dense rectangle.** Memory is proportional to the bounding box, not
  to the cells filled in. Use `worksheet_rows` when that matters.

## MSRV

Rust **1.88**.

## License

MIT. See [LICENSE](LICENSE).

## Contributing

Issues and pull requests are welcome. Before sending a change:

```
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo package
```

All four run in CI on Linux, Windows and macOS across five feature sets. A change that fixes a
parser bug should come with a fixture that would have caught it — `tests/regressions.rs` is where
those live.

# gridxcel-rs

[![CI](https://github.com/EdisonJwa/gridxcel-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/EdisonJwa/gridxcel-rs/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/gridxcel-rs.svg)](https://crates.io/crates/gridxcel-rs)
[![docs.rs](https://docs.rs/gridxcel-rs/badge.svg)](https://docs.rs/gridxcel-rs)
[![license](https://img.shields.io/crates/l/gridxcel-rs.svg)](LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-blue.svg)](Cargo.toml)

**A pure-Rust reader for Excel and OpenDocument spreadsheet files.**

No `unsafe`. One dependency. Reads `.xlsx`, `.xlsm`, `.xls` and `.ods` from untrusted files without
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

## Contents

- [Formats](#formats) · [Reading a sheet](#reading-a-sheet) · [Three ways to read](#three-ways-to-read)
- [Cell values](#cell-values) · [Dates](#dates) · [Formulas](#formulas) · [Merged cells](#merged-cells)
- [Sheet metadata](#sheet-metadata) · [Deserializing rows](#deserializing-rows) · [Errors](#errors)
- [Feature flags](#feature-flags) · [Examples](#examples) · [Performance](#performance)
- [Design](#design) · [Limitations](#limitations) · [Contributing](#contributing)

## Formats

| Format | Extensions | Container |
|---|---|---|
| OOXML workbook | `.xlsx` `.xlsm` `.xltx` `.xlam` | ZIP + XML |
| OpenDocument spreadsheet | `.ods` | ZIP + XML |
| Legacy BIFF8 workbook | `.xls` (Excel 97–2003) | OLE compound file |

Values, cached formula results, formula text, cell errors, dates in both epochs, merged regions,
sheet visibility and defined names come out as typed data. The crate is read-only.

Format detection is by **content**, not extension, so a renamed file still reads correctly.

## Reading a sheet

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

`worksheet_range` returns a `Range<Data>`: a dense rectangle that remembers where it sits in the
sheet. Positions inside it that hold nothing come back as `Data::Empty`.

## Three ways to read

Which one you want depends on the shape of the sheet.

**`worksheet_range`** builds the whole rectangle. Convenient, and costs the sheet's bounding box
whether the cells inside are full or not.

**`worksheet_range_ref`** returns exactly one rectangle, named in A1 notation. In OOXML the part is
read only as far as the window's last row, so the cost is the window rather than the sheet.

```rust
# use gridxcel_rs::{open_workbook, Reader};
# let mut workbook = open_workbook("tests/data/chanora.xlsx")?;
let totals = workbook.worksheet_range_ref("Identifications", "B2:F40")?;
assert_eq!(totals.get_size(), (39, 5));
# Ok::<(), gridxcel_rs::Error>(())
```

Its shape follows the reference rather than the data, which is what distinguishes it from
`worksheet_range`. Corners may be given either way round.

**`worksheet_rows`** streams. Memory is the widest row rather than the rectangle, which is what
matters when a sheet is large or sparse enough that its rectangle is the problem.

```rust
use gridxcel_rs::{open_workbook, Reader};

let mut workbook = open_workbook("huge.xlsx")?;
let mut rows = workbook.worksheet_rows("Sheet1")?;

while let Some(row) = rows.next_row()? {
    for (column, value) in row.iter() {
        println!("({}, {column}) = {value:?}", row.index());
    }
}
# Ok::<(), gridxcel_rs::Error>(())
```

- **A row carries only the cells that hold values**, so one value in column 16000 costs one entry
  rather than 16,001. `row.get(column)` binary-searches.
- **Rows the file declares with no values are still rows**, with their own index, so a gap cannot be
  mistaken for the end of the data.
- **Buffers are reused**, so a long sheet allocates nothing per row.

A sheet whose cells sit at `A1` and `XFD1048576` implies 17,179,869,184 cells. The dense path
refuses that; the streaming path reads it in constant space.

## Cell values

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

## Dates

A serial is held **with its epoch**, so a 1904-system workbook is never silently reinterpreted under
the 1900 system. Excel's fabricated 1900-02-29 is handled the way every mainstream reader handles
it: serial 60 converts to 1900-03-01.

```rust
use gridxcel_rs::ExcelDateTime;

let dt = ExcelDateTime::new(45321.5, false);
assert_eq!(dt.to_ymd_hms().unwrap().to_string(), "2024-01-30 12:00:00");
assert_eq!(dt.to_unix_timestamp(), Some(1_706_616_000));

// With the `dates` feature:
// let naive = dt.to_naive_datetime();
```

## Formulas

Values and formulas are read separately, because a sheet usually needs one or the other. The cached
result sits next to the formula in the file, so values are available without asking for formula
text.

```rust
# use gridxcel_rs::{open_workbook, Reader};
# let mut workbook = open_workbook("tests/data/sample.ods")?;
let values = workbook.worksheet_range("Sheet1")?;
let formulas = workbook.worksheet_formula("Sheet1")?;
# Ok::<(), gridxcel_rs::Error>(())
```

Shared formulas — where Excel writes the text once and references it from the rest of a filled-down
column — are expanded into real formula text per cell:

```
master at C2:  =SUM($B2:D2)
C3:            =SUM($B3:D3)
D2:            =SUM($B2:E2)
```

Absolute (`$`) parts are preserved, string literals and quoted sheet names are left alone, and
things that merely *look* like references are not rewritten: `LOG10(...)`, `1E5`, defined names and
structured table references (`Table1[Column1]`) all survive intact.

## Merged cells

A merged cell keeps its value in its top-left position and leaves the rest of the region blank, so
values alone cannot tell "merged" from "empty":

```rust
# use gridxcel_rs::{open_workbook, Reader};
# let mut workbook = open_workbook("tests/data/chanora.xlsx")?;
for region in workbook.worksheet_merged_regions("Identifications")? {
    println!("{region} covers {}x{}", region.rows(), region.columns());
}
# Ok::<(), gridxcel_rs::Error>(())
```

Corners are **inclusive**, matching how every format states a merge — which differs from `Range`'s
exclusive bottom-right. A merge over `A1:B2` reports `start == (0, 0)` and `end == (1, 1)`.
`to_a1()` renders it back, and `contains()` says whether a cell falls inside.

## Sheet metadata

```rust
# use gridxcel_rs::{open_workbook, Reader};
# let mut workbook = open_workbook("tests/data/chanora.xlsx")?;
workbook.sheet_names();          // &[String], in workbook order
workbook.defined_names();        // &[(String, String)] of name and target
workbook.sheet_index("Sheet1");  // Option<usize>
workbook.worksheet_range_at(0);  // by position instead of name
# Ok::<(), gridxcel_rs::Error>(())
```

A sheet is visible, hidden, or very hidden. Hiding is presentation, not access control: a hidden
sheet's cells read normally.

```rust
# use gridxcel_rs::{open_workbook, Reader};
# let mut workbook = open_workbook("tests/data/chanora.xlsx")?;
for index in 0..workbook.sheet_names().len() {
    let visibility = workbook.sheet_visibility(index);
    if !visibility.is_visible() {
        println!("sheet {index} is {visibility}");   // "hidden" or "veryHidden"
    }
}
# Ok::<(), gridxcel_rs::Error>(())
```

### Header rows

```rust
# use gridxcel_rs::{open_workbook, HeaderRow, Reader};
# let mut workbook = open_workbook("tests/data/chanora.xlsx")?;
workbook.with_header_row(HeaderRow::Row(2));
let range = workbook.worksheet_range("Summary")?;
assert_eq!(range.start(), (1, 0));
# Ok::<(), gridxcel_rs::Error>(())
```

Trimming keeps coordinates absolute, so after asking for row 2 the range starts at `(1, 0)`.

## Deserializing rows

Requires the `serde` feature.

```toml
gridxcel-rs = { version = "0.1", features = ["serde"] }
```

Fields are matched to columns **by header name**, so a struct works even when the sheet's columns are
in a different order, and extra columns are ignored.

```rust
# #[cfg(feature = "serde")]
# {
use gridxcel_rs::{open_workbook, RangeDeserializerBuilder, Reader};
use serde::Deserialize;

#[derive(Deserialize)]
struct Record {
    metric: String,
    value: f64,
    #[serde(default, deserialize_with = "gridxcel_rs::de::deserialize_as_f64_or_none")]
    noise: Option<f64>,
}

let mut workbook = open_workbook("tests/data/chanora.xlsx")?;
let range = workbook.worksheet_range("Identifications")?;
let records: Vec<Record> = RangeDeserializerBuilder::new()
    .from_range(&range)?
    .collect::<Result<Vec<_>, _>>()?;
# }
# Ok::<(), Box<dyn std::error::Error>>(())
```

Blanks become `None` for `Option` fields. Without headers, columns are matched by position.

## Errors

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

## Feature flags

Nothing is enabled by default.

| Feature | Enables | Pulls in |
|---|---|---|
| `serde` | `RangeDeserializerBuilder`, row → typed records | `serde` |
| `dates` | `ExcelDateTime::to_naive_*` | `chrono` |
| `codepage` | Correct legacy `.xls` text via `encoding_rs` | `encoding_rs` |

```toml
gridxcel-rs = { version = "0.1", features = ["serde", "dates", "codepage"] }
```

### Why `codepage` exists

BIFF stores each string as UTF-16, or "compressed" as one byte per character in the workbook's code
page. Which one is recorded per string, but the code page itself lives in a separate record.

Without the feature, a compressed byte becomes its Latin-1 code point. That is exact for ASCII and
for the ISO-8859 pages whose high range agrees with Latin-1, but **wrong** for the Windows code
pages between `0x80` and `0x9F` — where the euro sign, the typographic quotes and the en dash live.
Ordinary Western European text hits that band.

The feature covers the Windows pages (874, 1250–1258), the ISO-8859 pages, KOI8-R/U, IBM866 and
Macintosh. A code page it does not know falls back to Latin-1 rather than guessing.

## Examples

```
cargo run --example read_values    -- tests/data/chanora.xlsx
cargo run --example dump           -- tests/data/sample.ods 5
cargo run --example stream_rows    -- tests/data/chanora.xlsx Identifications
cargo run --example read_window    -- tests/data/chanora.xlsx Identifications B2:F40
cargo run --example inspect        -- tests/data/sample.ods
cargo run --example error_handling -- tests/data/word97.doc
cargo run --example deserialize --features serde -- tests/data/chanora.xlsx Identifications
```

## Performance

Numbers below come from `compare/`, a separate workspace that depends on both this crate and
[`calamine`](https://crates.io/crates/calamine) so the two can be measured on identical work:

```
cd compare
cargo run --release --bin ab -- target/fixtures/gridxcel-compare.xlsx S 15
```

`ab` interleaves the two readers and reports medians, because absolute milliseconds on a busy
machine drift by more than the differences being measured.

Synthetic 4000 × 50 sheet, 200,000 numeric cells:

| Workload | gridxcel-rs | calamine |
|---|---|---|
| `.xlsx` read whole sheet | 49 ms | 67 ms |
| `.xlsx` stream rows | 44 ms | 67 ms |
| `.xlsx` read window `B2:F40` | **0.54 ms** | 43 ms |
| `.ods` read whole sheet | 83 ms | 56 ms |
| `.ods` read window `B2:F40` | **19 ms** | 51 ms |

### What is faster, and why

**A window costs the window, not the sheet.** Rows outside it are never decompressed: in OOXML the
read abandons the deflate stream once the window's rows have closed. That cut is verified against
the parser rather than trusted, so a part with out-of-order rows or a `</row>` inside a comment is
read whole instead of truncated. A question about 195 cells costs 0.54 ms against 43 ms.

**A dense read of an OOXML sheet holds it once.** Cells are written into their final positions as
they are parsed, so the sheet is never present twice. Measured at 6.1 MB peak against 16.6 MB for
the sparse-then-dense shape, and a test fails if that regresses.

**Streaming materialises nothing**, through the same decoder the dense path uses.

### What is slower

`.ods` whole-sheet reads are ~1.4× slower than `calamine`. The cause is the format rather than the
code: ODS states each cell as a full element with its attributes, so the same 200,000 cells occupy
13.3 MB against 5.9 MB in OOXML. Per megabyte this parser is ahead (5.0 ms/MB against 5.25); there
is simply 2.26× as much to walk. Building the dense rectangle is 12% of the read, so it is not where
the time goes.

Two restructurings were tried and reverted, both measured with `ab`: resuming at a recorded offset
instead of re-walking the tables before the wanted one (no change beyond noise), and driving the
dense read through the row builder rather than a sparse list (5× slower). For an `.ods` too large to
materialise, use `worksheet_rows` — streaming is 1.5× faster than the dense read on `.xlsx` and
competitive on `.ods`.

## Design

```
src/
  container.rs   the swappable archive trait, ZIP backend (zipcore), format sniffing
  cfb.rs         OLE compound file reader (used by .xls)
  xml.rs         XML pull parser
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

The archive container sits behind one internal trait, `container::Archive`, so the format readers
never talk to a ZIP file directly and the backend can be replaced without touching them. It is served
by [`zipcore`](https://docs.rs/zipcore), a from-scratch ZIP reader in a sibling crate. The
compound-file reader and the XML tokenizer are in this crate, which means the whole parsing stack
can be audited as one body of code.

Three properties are worth calling out:

- **Sheets are parsed on demand.** Opening a workbook reads metadata only. The shared string table is
  not touched until a sheet needs it, so a workbook with millions of strings you never read costs
  nothing. `.xls` is the exception, because its sheet substreams are addressed by offset in a stream
  that has to be read whole.
- **Everything is bounds-checked.** No slicing without a length check, no unchecked arithmetic on
  file-supplied values, no panics on malformed input. `unsafe` is forbidden crate-wide.
- **Coordinates are absolute.** A range knows where it sits in the sheet, so a cell keeps its
  identity across trimming and merging.

### Reading untrusted files

A reader takes input someone else wrote, so the resource traps are closed rather than documented
away. The full list is in [SECURITY.md](SECURITY.md); in short:

| Bound | Value |
|---|---|
| Bytes decompressed per archive entry or compound stream | 4 GiB, counted as produced |
| Cell text per sheet | 256 MiB, charged before allocation |
| Declared extent reservation | 64 MiB |
| Text per cell (ODS) | 1 MiB |
| Materialised sheet | 1 GiB |
| Archive entries | 65,536 |

Repeat counts and `<text:s>` runs are charged as each cell is produced, so a few hundred bytes of
markup cannot stand for millions of cells. DTDs are skipped rather than resolved, so XXE and
billion-laughs have nothing to work with.

## Tests

```
cargo test --all-features
```

232 tests across 12 suites, plus 22 doc-tests that compile every sample in this file. Several exist because of defects found in review, and each of those is a
guard rather than a demonstration.

| Suite | What it pins |
|---|---|
| `adversarial` | Attacks, not correctness: truncation at every offset, 400 bit flips, 100k-deep nesting, amplification, oversized declared sizes. Memory measured with a counting allocator. |
| `amplification` | Repeat counts and chain walks cannot multiply bytes; the paths that bypass the builder's budget are charged explicitly. |
| `regressions` | One test per defect found in review, each carrying a note on why the input is dangerous. |
| `payload_budget` | The cell budget accounts for what a cell holds, not only its slot. |
| `entities` | The entity decoder stays linear; text that only looks like an entity survives. |
| `zipcore_upgrade` | The container backend's behaviour is unchanged across a major version bump. |
| `merged_regions`, `visibility`, `container` | Reader-level features and the container contract. |
| `streaming`, `peak_allocation`, `real_files` | The three sheet views held against each other, and their memory bounds. |

Fixtures are real files where a real file was available:

- `tests/data/chanora.xlsx` — a nine-sheet report from a real scan tool.
- `tests/data/rollmilkice.xlsx` — a smaller real export.
- `tests/data/word97.doc` — a genuine OLE compound file, used to validate the container reader's
  FAT, mini-FAT and directory-tree walking.
- `tests/data/sample.ods` — exercises repeat counts, every ODS value type, and `text:s` runs. It was
  zipped on Windows and stores backslash separators, which the ZIP spec does not allow; it is kept
  as-is because it is a genuine example of what tools emit, and the reader tolerates it.

`.xls` is tested against a constructed fixture rather than a real file: a structurally valid BIFF8
workbook assembled in memory, with a real FAT and directory, covering every cell record type.

## Limitations

Knowing where a reader stops matters as much as where it works.

- **Read-only.** Writing is not implemented.
- **No XLSB.** Detected and rejected with `Error::Unsupported` naming it.
- **No Excel 5.0/95 (BIFF5/BIFF7).** A `.xls` predating Excel 97 is rejected rather than misread.
- **`.xls` formula text is not recovered.** BIFF stores formulas as compiled token streams;
  decompiling them is not implemented, and `worksheet_formula` returns `Error::Unsupported`. Cached
  **values** are fully available.
- **`.xls` defined names are not extracted.** A `Lbl` record's value is a token stream, so the names
  could be listed but not what they refer to.
- **An ODS `<text:list>` inside a cell** is flattened into the cell's text rather than modelled.
- **ODS `META-INF/manifest.xml` is not read**, so an ODF-encrypted package surfaces as an XML error
  rather than `Error::Encrypted`.
- **Double-byte code pages are not decoded.** A page such as Shift_JIS cannot express its text one
  byte per character, so such a workbook writes UTF-16 instead.
- **Row structs cannot use `#[serde(flatten)]` or `#[serde(deny_unknown_fields)]`.** Both require a
  keyed visitor, and rows are read positionally.
- **Deserialized fields must be owned.** The row deserializer requires `DeserializeOwned`.
- **Dates convert assuming UTC.** An Excel serial is a wall-clock value with no zone attached.
- **No caching layer.** `worksheet_range` re-parses the sheet part on every call.
- **No formatting, charts, images or pivot definitions.**
- **A dense read is proportional to the bounding box**, not to the cells filled in. Use
  `worksheet_rows` when that matters.

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

All four run in CI on Linux, Windows and macOS across five feature sets. A change that fixes a parser
bug should come with a fixture that would have caught it — `tests/regressions.rs` is where those
live.

The full process is in [CONTRIBUTING.md](CONTRIBUTING.md), and the project follows the
[code of conduct](CODE_OF_CONDUCT.md).

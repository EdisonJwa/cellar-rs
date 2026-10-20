# cellar

A pure-Rust reader for Excel and OpenDocument spreadsheet files.

`cellar` reads the three formats that cover essentially every spreadsheet in the wild:

| Format | Extensions | Container |
|---|---|---|
| OOXML workbook | `.xlsx` `.xlsm` `.xltx` `.xlam` | ZIP + XML |
| OpenDocument spreadsheet | `.ods` | ZIP + XML |
| Legacy BIFF8 workbook | `.xls` (Excel 97–2003) | OLE compound file |

Values, cached formula results, formula text, cell errors, dates in both epochs, defined
names, and sheet metadata all round out of the file as typed data. The crate is read-only.

## Why it exists

The `zip` crate's reader is the single point of failure for every pure-Rust spreadsheet
reader. On 2026-10-16 the maintainer yanked six major version lines in the space of three
minutes:

```
zip 9.0.3   live          zip 6.0.0   yanked
zip 9.0.2   yanked        zip 5.1.1   yanked
zip 9.0.0   yanked        zip 4.6.1   yanked
zip 8.6.0   yanked        zip 3.0.0   yanked
zip 7.4.0   yanked        zip 2.6.1   yanked
```

`calamine` pins `zip = "^8.6"`, and `8.6.0` was the only `8.6.x` ever published. With every
version satisfying `^8.6` yanked, a fresh `cargo build` of `calamine` cannot resolve its own
dependency tree — which takes the whole ecosystem down with it, not because of anything
wrong with calamine.

Only `zip 9.0.3` survived. Relying on it would mean being one publish away from the same
outage. So in `cellar` the archive container sits behind a single internal trait, and it is
served by [`zipcore`](https://docs.rs/zipcore) — this project's own from-scratch ZIP reader,
which parses the container itself and has no `zip` dependency at all:

```rust
pub(crate) trait Archive {
    fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>>;
    fn contains(&self, name: &str) -> bool;
    fn names(&self) -> Vec<String>;
}
```

Swapping the backend is writing one more implementation of this trait — no format reader
changes, because none of them know what a ZIP file is. The OLE compound file reader used for
`.xls` and the XML tokenizer used for the other two formats are **already** written from scratch
in this crate, as neither is available from a container library. With this swap the only
dependencies left by default are `zipcore`'s own two: `miniz_oxide` and `crc32fast`.

## Installation

```toml
[dependencies]
cellar = "0.1"

# Optional. Chrono conversions for dates, serde-based row deserialization, and correct code page
# handling for legacy `.xls` text.
# cellar = { version = "0.1", features = ["dates", "serde", "codepage"] }
```

`zipcore` is the only non-optional dependency. Nothing is enabled by default.

## Reading values

```rust
use cellar::{open_workbook, Reader};

let mut workbook = open_workbook("report.xlsx")?;

for name in workbook.sheet_names().to_vec() {
    let range = workbook.worksheet_range(&name)?;
    println!("{name}: {:?}", range.get_size());

    for row in range.rows() {
        for cell in row {
            if let Some(text) = cell.as_string() {
                print!("{text}\t");
            }
        }
        println!();
    }
}
```

Format detection is by content, not by extension, so a renamed file still reads correctly.
If you know the format, skip the detection:

```rust
use cellar::{Reader, Xlsx, Xls, Ods};

let mut xlsx = Xlsx::new(std::io::Cursor::new(bytes))?;
let mut xls  = cellar::open_xls("legacy.xls")?;
let mut ods  = cellar::open_ods("sheet.ods")?;
```

## Cell values

A cell is a [`Data`], which distinguishes the cases a spreadsheet actually has:

```rust
use cellar::{Data, CellError, ExcelDateTime};

Data::Int(42)                                   // a whole number
Data::Float(1.5)                                // a fractional number
Data::String("text".into())
Data::Bool(true)
Data::DateTime(ExcelDateTime::new(45321.5, false))  // a serial, with its epoch
Data::DateTimeIso("2024-03-01T13:45:30".into())     // ODS stores dates as text
Data::DurationIso("PT1H30M".into())                 // ODS time values
Data::Error(CellError::Div0)                        // #DIV/0! and friends
Data::Empty                                         // no value
```

Excel has no integer type, so an integral serial is reported as `Data::Int`. That is what a
reader wants for an ID or count column, and loses nothing: the file stores `2` and `2.0`
identically.

`Data` has forgiving accessors, so you rarely need to match by hand:

```rust
let value = range.get((0, 0)).unwrap();
value.as_string();     // Some(String) for anything renderable
value.as_f64();        // numbers, booleans, date serials, and numeric text
value.as_i64();
value.as_bool();
value.as_date();       // YmdHms, from either date representation
value.as_duration();   // days, from a serial or an ISO-8601 duration
```

Dates are held as a serial **plus the workbook's epoch**, so a 1904-system file is never
silently reinterpreted under the 1900 system. The 1900 system's fabricated 1900-02-29 is
handled the way every mainstream reader handles it: serial 60 converts to 1900-03-01.

```rust
use cellar::ExcelDateTime;

let dt = ExcelDateTime::new(45321.5, false);
assert_eq!(dt.to_ymd_hms().unwrap().to_string(), "2024-01-30 12:00:00");
assert_eq!(dt.to_unix_timestamp(), Some(1_706_616_000));

// With the `dates` feature enabled:
let naive = dt.to_naive_datetime();
```

## Legacy `.xls` text

BIFF stores each string one of two ways: as UTF-16, or "compressed" as one byte per character read
in the workbook's code page. Which one is used is recorded per string, but the code page itself
lives in a separate record, so decoding waits until the globals substream has been walked rather
than assuming the records arrive in a particular order.

Without the `codepage` feature a compressed byte becomes its Latin-1 code point. That is exact for
ASCII, and exact for the ISO-8859 pages whose high range agrees with Latin-1, but **wrong** for the
Windows code pages between `0x80` and `0x9F` — which is where the euro sign, the typographic quotes
and the en dash live. Ordinary Western European text hits that band.

The feature closes it by decoding through `encoding_rs`:

```toml
cellar = { version = "0.1", features = ["codepage"] }
```

It covers the Windows pages (874, 1250–1258), the ISO-8859 pages, KOI8-R/U, IBM866 and Macintosh,
and is off by default so the default build stays at one dependency. A code page it does not know
falls back to Latin-1 rather than guessing.

## Streaming rows

`worksheet_range` builds the sheet's dense rectangle, which for a large or sparse sheet is the
wrong shape: a file whose cells sit at `A1` and `XFD1048576` implies **17,179,869,184 cells**, and
that path refuses rather than try to allocate them.

`worksheet_rows` reads the same sheet without materialising anything:

```rust
use cellar::{open_workbook, Reader};

let mut workbook = open_workbook("huge.xlsx")?;
let mut rows = workbook.worksheet_rows("Sheet1")?;
while let Some(row) = rows.next_row()? {
    // Only the populated cells of this row are visited.
    for (column, value) in row.iter() {
        println!("({}, {column}) = {value:?}", row.index());
    }
}
```

- **Memory is the widest row, not the area of the sheet.** The row buffers are reused, so a long
  sheet allocates nothing per row.
- **A row carries only the cells that hold values**, so one value in column 16000 costs one entry
  rather than 16001. `row.get(column)` binary-searches, since columns are guaranteed ascending.
- **Rows the file declares with no values are still rows**, with their own index, so a gap cannot
  be mistaken for the end of the data.
- **Rows already parsed are never re-materialised.** An OpenDocument row that
  `table:number-rows-repeated` says occurs a million times is re-emitted from one buffered row.
  The dense path has to *reject* the same file, because it would have to hold every copy.

The two views are checked against each other cell for cell on every real fixture, so they cannot
drift apart.

`.xls` streams too, and for the same reason as the rest: the format allows a sheet of 65,536 x 256
cells, so a workbook holding two values can still imply 537 MB of them. What it does not save is the
record stream itself — a BIFF workbook addresses its sheets by offset, so the stream is in memory
either way. Both reads decode records through one shared routine, which is what keeps them agreeing
about what a cell holds.

Because the streaming read is how a window is served on this format, `worksheet_range_ref` works on
`.xls` as well: it reads rows and places the ones the window holds.

## Reading one region

`worksheet_range_ref` takes an A1-style range and returns exactly that rectangle:

```rust
let totals = workbook.worksheet_range_ref("Sheet1", "B2:D10")?;
assert_eq!(totals.get_size(), (9, 3));
```

Its shape follows the reference rather than the data, so positions inside the window that hold
nothing come back as `Data::Empty`. That is what distinguishes it from `worksheet_range`, whose
shape is the extent of the values. The corners may be given either way round.

The parser is told which rows and columns are wanted, so anything outside the window is **skipped
rather than decoded**: a row costs a tag scan instead of a reference parse, a style lookup and a
number parse for every cell in it. In OOXML the part itself is also read only as far as the window
reaches — once more rows have closed than the window's last row, decompression stops. That part holds
one sheet; OpenDocument's content part holds every sheet at once, so there it is read whole and only
the scanning is saved. On a 2000 x 50 sheet, deflated as a real workbook is:

| Read | Time |
|---|---|
| `worksheet_range_ref("S", "A1")` | ~113 µs |
| `worksheet_range_ref("S", "B900:C910")` | ~12.1 ms |
| `worksheet_range("S")` (whole sheet) | ~35.0 ms |

A window on the first rows of a sheet costs a few hundredths of reading it, and one a thousand rows
in costs about what reading up to that row costs; neither pays for the rows below it. The same sheet
shows it in memory: a window read peaks at 87 KB against its 2.9 MB part, most of that being the
inflater's own window. On a stored part — where there is no decompression to stop — the same read
peaks at 29 KB.

The stopping point is found by counting `</row>` closes, which is sound while rows carry ascending,
distinct indexes as the format requires — so the window's row has necessarily gone by. That
assumption is not simply trusted: the cut is checked against the parser, and a part that disagrees is
read whole rather than cut short. Counting bytes cannot tell a comment from a row close, so this is
also what keeps a sheet whose comments look like `</row>` from being cut in half.

A window still has to reach the row it names, and the bytes before it are not seekable, so a window
deep into a sheet costs about a scan up to that row.

The header row setting does **not** move a window. It exists to trim a whole-sheet read, and an
explicit window already says which rows are wanted.

## Formulas

Values and formulas are read separately, because a sheet usually needs one or the other.
Excel stores the cached result next to the formula, so values are always available even
without asking for formula text.

```rust
let values   = workbook.worksheet_range("Sheet1")?;
let formulas = workbook.worksheet_formula("Sheet1")?;

assert_eq!(formulas.get((4, 2)), Some(&"SUM(B2:B3)".to_string()));
```

Shared formulas — where Excel writes the text once and references it from the rest of a
filled-down column — are expanded into real formula text per cell, by shifting each relative
reference by that cell's offset:

```
master at C2:  =SUM($B2:D2)
C3:            =SUM($B3:D3)
D2:            =SUM($B2:E2)
```

Absolute (`$`) parts are preserved, string literals and quoted sheet names are left alone,
and things that merely *look* like references are not rewritten: `LOG10(...)`, `1E5`, defined
names and structured table references (`Table1[Column1]`) all survive intact.

## Deserializing rows (feature `serde`)

With the `serde` feature, a range deserializes into typed records. Fields are matched to
columns **by header name**, so a struct works even when the sheet's columns are in a
different order, and extra columns are ignored.

```rust
use cellar::{open_workbook, RangeDeserializerBuilder, Reader};
use serde::Deserialize;

#[derive(Deserialize)]
struct Record {
    metric: String,
    value: f64,
    #[serde(default, deserialize_with = "cellar::de::deserialize_as_f64_or_none")]
    noise: Option<f64>,
}

let mut workbook = open_workbook("data.xlsx")?;
let range = workbook.worksheet_range("Sheet1")?;

let records: Vec<Record> = RangeDeserializerBuilder::new()
    .from_range(&range)?
    .collect::<Result<Vec<_>, _>>()?;
```

Blanks become `None` for `Option` fields. Without headers, columns are matched by position.

## The header row

```rust
use cellar::{HeaderRow, Reader};

workbook.with_header_row(HeaderRow::Row(2));
let range = workbook.worksheet_range("Sheet1")?;
```

Trimming keeps coordinates absolute, so after asking for row 2 the range starts at `(1, 0)`.
Iterating `rows()` is unaffected.

## Sheet metadata

```rust
workbook.sheet_names();          // &[String], in workbook order
workbook.defined_names();        // &[(String, String)] of name and target
workbook.sheet_index("Sheet1");  // Option<usize>
workbook.worksheet_range_at(0);  // by position instead of name
```

A sheet is visible, hidden, or very hidden, and the difference is worth reporting: a hidden
sheet is still data, but it is not part of what the author meant to present. Hiding is
presentation, not access control — a hidden sheet's cells read normally.

```rust
for (index, name) in workbook.sheet_names().to_vec().iter().enumerate() {
    let visibility = workbook.sheet_visibility(index);
    if visibility.is_visible() {
        println!("{name}");
    } else {
        println!("{name} ({visibility})");   // "hidden" or "veryHidden"
    }
}
```

OOXML records this as the `state` attribute on `<sheet>`, BIFF8 in the low two bits of a
`BOUNDSHEET` record's state word, and OpenDocument as `table:visibility` — where a collapsed or
filtered table is reported as hidden rather than visible. A format that records nothing reports
visible, so a caller filtering by visibility keeps everything rather than dropping data it could
not classify.

## Merged cells

A merged cell keeps its value in the top-left position and leaves the rest of the region blank
in the grid, so the values alone cannot tell "merged" from "empty". The regions are reported
separately, in the order the file records them:

```rust
use cellar::{open_workbook, Reader};

let mut workbook = open_workbook("report.xlsx")?;
for region in workbook.worksheet_merged_regions("Sheet1")? {
    println!("{region} covers {}x{} cells", region.rows(), region.columns());
    let anchor = workbook.worksheet_range("Sheet1")?.value_or_empty(region.start);
    println!("  value: {anchor:?}");
}
```

`MergedRegion`'s corners are **inclusive**, matching how every format states a merge, which
differs from `Range`'s exclusive bottom-right. That is why a merge over `A1:B2` reports
`start == (0, 0)` and `end == (1, 1)`. `to_a1()` renders it back as `A1:B2`, and `contains`
answers whether a cell falls inside.

Each format carries the information differently — OOXML in a `<mergeCells>` element, BIFF8 in
`MERGEDCELLS` records, and OpenDocument as `number-columns-spanned` / `number-rows-spanned`
attributes on the anchor cell — so all three readers override the method rather than relying on
a shared assumption about the file's shape.

## Errors

Failures are typed rather than stringly-message'd, and a malformed file never panics:

```rust
use cellar::Error;

match cellar::open_workbook(path) {
    Err(Error::Encrypted)        => { /* needs a password */ }
    Err(Error::Unsupported(m))   => { /* valid file, unimplemented feature */ }
    Err(Error::NotFound(name))   => { /* no such sheet */ }
    Err(e) if e.is_corrupt()     => { /* damaged or not a spreadsheet */ }
    other => { /* ... */ }
}
```

## Hardening

A reader takes untrusted input, so the usual resource traps are closed rather than documented
away:

- **Decompression bombs.** No archive entry is decompressed past 4 GiB, and the limit is applied
  to the bytes actually produced, not to a declared length that a crafted header can understate.
- **Archive entry counts.** An archive that declares more than 65536 parts is rejected, and the
  index is grown as entries are read rather than preallocated from a count the file supplies.
- **Repeat-count amplification.** OpenDocument and BIFF express padding as a repeat count, so a
  few hundred bytes can stand for millions of cells. The cell budget is spent *as each cell is
  produced* and a row is capped at the grid width, so a small file cannot demand a huge
  allocation before the limit is consulted. A repeat that would carry a row or column index past
  the grid is **refused** rather than saturated, which is what keeps the index arithmetic from
  wrapping on a file that claims `2^32` rows.
- **Text amplification.** `<text:s text:c="N"/>` is a few bytes standing for N characters. Each
  element is capped, and a cell's *total* text is capped too: without the second bound, 656 bytes
  of repeated runs expanded to 40 MB of text. A cell now stops at 1 MiB.
- **Declared extents reserve a bounded amount.** A `<dimension ref="A1:AF1048576"/>` is about a
  hundred bytes and names exactly the grid's 33.5 million cells — a 1 GiB allocation decided
  before a single cell is read. A hint now reserves at most 64 MiB; the buffer still grows to the
  real extent as cells arrive, so a sheet that genuinely fills that rectangle is read correctly.
- **One per-part ceiling, applied everywhere.** No archive entry and no compound-file stream may
  exceed 4 GiB. `.xls` previously had no equivalent of the ZIP path's limit, so it was the one
  format that could be made to allocate without bound.
- **Short chains are reported, not silently truncated.** A compound-file stream whose sector
  chain ends before its declared size is a damaged file; returning the short buffer would surface
  much later as an unexplained end-of-stream from a record parser.
- **The cell budget counts payloads, not just slots.** A `Data` slot is 32 bytes whether it is
  empty or owns a megabyte of text, so a budget derived from the slot size cannot see what the
  cells hold. Cells all naming one shared string passed the slot budget while allocating far more
  heap than it allowed — a 49 KB file measured at 23 MB, and at a larger shape unbounded. Text is
  now charged as it is stored, against a separate 256 MiB budget, and the refusal happens before
  the allocation.
- **XML entity expansion.** DTDs are skipped rather than resolved, and there are no external
  entity fetches, so billion-laughs and XXE have nothing to work with.
- **Dense sheets.** A range is dense, so a file with cells at `A1` and `XFD1048576` implies
  seventeen billion cells. Materialising a sheet is capped by a 1 GiB budget, and going over
  returns a clear error instead of an allocation the process cannot survive.
- **Cyclic allocation chains.** Every OLE sector chain walk is bounded and cycle-checked, because
  a malformed file can point a sector at itself.
- **Directory slots keep their positions.** Compound-file entries address one another by slot
  index, so an unused slot is retained as a placeholder rather than compacted away — otherwise
  one deleted stream would shift every later index and hide the workbook.
- **DIFAT extensions are walked whole.** A file needing more FAT sectors than the header can list
  puts the remainder in extension sectors; every slot of those is read, so a workbook past
  roughly 6.8 MiB is not silently truncated to a table that ends early.
- **Counts read from the file.** String-table sizes, record lengths and repeat counts are all
  clamped before they are used to allocate.

`unsafe` is forbidden crate-wide. The read path is fuzzed for panics, hangs and unbounded
allocation rather than for correctness — see [Operational readiness](#operational-readiness).
The defects found in review are pinned by `tests/regressions.rs`, so a reintroduction fails the
build rather than the next user.

## Operational readiness

| Gate | Where |
|---|---|
| Formatting | `cargo fmt --all --check` |
| Lints | `cargo clippy --all-targets --all-features -- -D warnings` |
| Tests | `cargo test` over the default, `serde`, `serde,dates` and `codepage` feature sets, on Linux, Windows and macOS |
| Docs | `cargo doc --all-features` with `-D warnings` |
| MSRV | `cargo build --lib` on the declared 1.88 |
| Supply chain | `cargo deny check` — see below |
| Fuzzing | a 60-second session per pull request, plus a scheduled run |

`.github/workflows/ci.yml` runs all of these. Two of them are worth explaining.

**`cargo deny` is not incidental here.** `deny.toml` sets `yanked = "deny"`, turning "a dependency
I rely on was yanked" into a red build. This crate exists because that failure went unnoticed by
every consumer of a format reader until their builds broke, so it is checked rather than assumed.
The workflow also runs weekly on a schedule, so a newly published advisory or a fresh yank
surfaces even when nothing in the repository changed.

**Fuzzing targets the whole read path**, not one format: `fuzz/fuzz_targets/open.rs` feeds
arbitrary bytes through format detection, all three readers, the XML tokenizer, the container
parsers and the serde layer, asserting that none of them panic. Run it with:

```sh
cargo +nightly fuzz run open
```

## Performance

`cargo bench` reports:

| Benchmark | Time |
|---|---|
| `open/xlsx` | ~156 µs |
| `open/ods` | ~103 µs |
| `read_all_sheets/chanora.xlsx` | ~3.1 ms |
| `one_sheet/identifications` | ~2.6 ms |
| `deserialize/chanora.xlsx` | ~2.8 ms |
| `window/one_cell` on a 100k-cell sheet | ~113 µs |
| `window/middle` on the same sheet | ~12.1 ms |
| `window/whole_sheet` on the same sheet | ~35.0 ms |

Opening is metadata-only, which is why it does not scale with sheet contents.

### Against `calamine`

`compare/` measures both readers on the same files doing the same work. It is a separate
workspace so `calamine` is never a dependency of this crate. Run it with
`cd compare && cargo run --release`.
On a 4000 × 50 numeric sheet (200,000 cells), best of N:

| Workload | cellar | calamine | |
|---|---|---|---|
| `.xlsx` open + first sheet | 31 ms | 40 ms | 1.3× faster |
| `.xlsx` read whole sheet | 31 ms | 39 ms | 1.3× faster |
| `.xlsx` stream rows | 29 ms | 40 ms | 1.4× faster |
| `.xlsx` read window `B2:F40` | **0.46 ms** | 39 ms | **85× faster** |
| `.ods` open + first sheet | 70 ms | 39 ms | 1.8× slower |
| `.ods` read whole sheet | 71 ms | 39 ms | 1.8× slower |
| real `chanora.xlsx`, whole sheet | 1.4 ms | 1.5 ms | 1.03× faster |
| real `chanora.xlsx`, window | 0.30 ms | 1.4 ms | 4.6× faster |

The window result is the one that matters most and the one to read carefully. `calamine` has no
windowed read — it materialises the sheet and slices it, so it pays the full 39 ms to answer a
question about 195 cells. `cellar` stops decompressing once the window's rows have gone by. That
is a design difference, not a micro-optimisation.

### How far the window advantage actually goes

The advantage is proportional to how much of the sheet a window skips, so it is worth stating
where it holds rather than quoting the best case alone. On a 20,000 × 50 sheet (1,000,000 cells,
3.9 MB deflated, 166 ms to materialise in full):

| Window | Time | Share of a whole-sheet read | Speedup |
|---|---|---|---|
| `A1` (single cell) | 0.19 ms | 0.1% | **877×** |
| `A1:C3` | 0.20 ms | 0.1% | 819× |
| `B2:F40` | 0.46 ms | 0.3% | 365× |
| `B100:B101` | 0.78 ms | 0.5% | 213× |
| `B1000:B1001` | 6.67 ms | 4.0% | 25× |
| `B10000:B10001` | 67.3 ms | 40.5% | 2.5× |
| `B19990:B19991` | 136.6 ms | 82.1% | 1.2× |

Two things follow, and both are worth being plain about:

- **A window near the top of a sheet is essentially free**, which is the case that matters for
  the common access pattern: headers, a summary block, a preview of the first rows.
- **A window near the bottom is not.** The bytes before it are not seekable inside a deflated
  part, so the reader still scans up to that row. At the last row of this sheet a window saves
  almost nothing. Claiming "constant time" for any window would be false; what is constant is
  that the rows *below* the window are never decompressed.

The `.ods` gap is real and in calamine's favour: this crate's OpenDocument reader assembles the
sheet through a sparse list and holds it twice, and the format gives no declared extent to
pre-size a buffer from. Its own window read is still 2.3× faster than calamine's
materialise-and-slice, because content.xml is read once and only the scanning is saved.

Two robustness differences also fell out of building the comparison, both recorded by it:

- **Entry-name separators.** The committed `tests/data/sample.ods` was zipped on Windows and
  stores `META-INF\manifest.xml`. The ZIP specification requires `/`; `cellar` folds the
  separators in `normalize_name`, `calamine` cannot find the part. The fixture is kept as-is
  because it is a genuine example of what tools emit.
- **Part resolution.** `calamine` requires `_rels/.rels` and matching relationship ids to reach
  the workbook; `cellar` resolves the conventional part path directly, so a package missing its
  relationship metadata still reads. The comparison's synthetic `.xlsx` carries both forms so
  the timing is like-for-like.

Two properties of the design are load-bearing for throughput and memory, and both were arrived
at by measurement rather than assumption:

- **Cells are converted from sparse to dense in a single pass.** The conversion detects that the
  input already arrives in order — which it does for every format read here — and skips the sort
  it would otherwise need.
- **In OOXML and BIFF, cells are written straight into their final positions as they are parsed.**
  Both formats declare the sheet's extent up front, in `<dimension>` and `DIMENSIONS` respectively,
  so the dense buffer is allocated once and the sparse list never exists. A declared extent that is
  missing, stale, or wrong is grown or trimmed to fit, costing correctness nothing.
- **The serde layer computes a column plan once per range**, not once per row. Field names are
  never materialised, because a row is read positionally using the plan.

### Memory

A dense range costs far more than the file it came from, so how a sheet is assembled decides
whether a large workbook is readable at all. `tests/peak_allocation.rs` measures peak allocation
with a counting allocator, because the difference is invisible to a correctness test and easy to
reintroduce.

Reading a 100,000-cell sheet whose range is 3.2 MB, deflated as a real workbook is:

| | Peak allocation |
|---|---|
| Collecting cells as a sparse list, then converting | 16.6 MB |
| Writing cells directly into the range | 6.1 MB |

The remainder is the range itself plus the decompressed sheet part, which is the floor without
streaming the XML. The test fails if the peak exceeds three times the range, so reverting to the
two-step conversion is caught rather than measured again.

OpenDocument has no equivalent extent declaration, so `.ods` still assembles a sheet through the
sparse list. It is the one format where the peak is still roughly twice the range.

`worksheet_rows` sidesteps all of this: it materialises nothing, so its cost is the widest row.
`tests/peak_allocation.rs` reads a two-cell sheet whose rectangle is 17 billion cells — over 500 GB
if held — and asserts the whole read stays under a megabyte.

A window read is bounded the same way, by stopping. On that same sheet `worksheet_range_ref("S",
"A1")` peaks at 87 KB — most of it the inflater's window — rather than at the 2.9 MB part it comes
from. The test pins it below 256 KB: taking the part whole costs about its size, so the two cannot be
confused.

## Where this crate is actually ahead

Stated narrowly, because a list of advantages that includes parity is not a list of advantages.

**It keeps.** These are structural, and a competitor cannot copy them without a redesign:

1. **A window costs the window, not the sheet.** Reading `B2:F40` of a 20,000-row sheet takes
   0.46 ms against 166 ms to materialise it — because decompression is abandoned once the
   window's rows have gone by, and the cut is verified against the parser rather than trusted.
   The advantage scales with what a window skips: 877× at the top of a sheet, 1.2× at the
   bottom, and the table above says which is which. No other pure-Rust reader here offers the
   operation at all.
2. **One sheet view costs one allocation.** Cells are written into their final positions as they
   are parsed, so a dense read never holds the sheet twice. Measured at 6.1 MB peak against
   16.6 MB for the sparse-then-dense shape, and the test fails if that reverts.
3. **Streaming materialises nothing.** `worksheet_rows` reads a sheet whose rectangle is 17
   billion cells in constant space, which the dense path correctly refuses. Reached through the
   same decoder as the dense read, so the two cannot disagree about what a cell holds — a
   property the test suite checks cell for cell.
4. **The container is ours.** `zipcore` is this project's own ZIP reader: no `unsafe`, ZIP64,
   CRC verification, two dependencies instead of eight. That was the reason the crate exists.

**It is at parity, and should not be sold as ahead.** Flat whole-sheet reads are 1.3× faster on
a synthetic sheet and indistinguishable on a real one — that is not a moat. Opening is metadata
only, which is table stakes.

**It is behind, and says so.** `.ods` whole-sheet reads are ~1.8× slower than `calamine`
because the format gives no declared extent and the sheet is assembled through a sparse list.
Fixing it needs an ODS-specific two-pass reader, not a tweak.

Running the comparison is the point: `compare/` exists so these numbers can be re-derived rather
than believed.

## Deliberate limitations

Stated plainly, because knowing where a reader stops matters as much as where it works:

- **Read-only.** Writing is not implemented.
- **No XLSB.** The MS-XLSB binary format is not supported. An XLSB package is detected and
  rejected with `Error::Unsupported` naming it, rather than failing with a confusing missing-part
  error. Adding it means one more module against the same `Reader` trait.
- **No Excel 5.0/95 (BIFF5/BIFF7).** A `.xls` predating Excel 97 is rejected with
  `Error::Unsupported` rather than misread.
- **`.xls` formula text is not recovered.** BIFF stores formulas as compiled token streams rather
  than text; decompiling them is not implemented. `worksheet_formula` returns
  `Error::Unsupported` for `.xls`. Cached **values** are fully available.
- **Code pages outside the single-byte set are not decoded.** See
  [Legacy `.xls` text](#legacy-xls-text). A double-byte page such as Shift_JIS is deliberately
  absent: it cannot express its text one byte per character, so such a workbook writes UTF-16
  instead, and the compressed strings it does contain are ASCII, which Latin-1 reads correctly.
- **An `.xls` workbook's defined names** are not extracted. A `Lbl` record's value is a compiled
  token stream, the same as a formula's, so the names could be listed but not what they refer to —
  and a name without its target is worse than no name at all.
- **Row structs cannot use `#[serde(flatten)]` or `#[serde(deny_unknown_fields)]`.** Both require a
  keyed visitor, and rows are read positionally so that no field-name key has to be built per row.
  `HashMap` targets still work, since those genuinely need keys.
- **Deserialized fields must be owned.** The row deserializer requires `DeserializeOwned`, so a
  field cannot be a borrowed `&str`.
- **Dates convert assuming UTC.** An Excel serial is a wall-clock value with no zone attached, so
  `to_unix_timestamp` interprets it as UTC. Converting and then rendering in local time will shift
  it; prefer `to_ymd_hms` or the `chrono` `Naive*` conversions when the value is a wall clock.
- **No caching layer.** `worksheet_range` re-parses the sheet part on every call. Read a sheet once
  and keep the range rather than calling it in a loop.
- **No formatting, charts, images or pivot definitions.** Values, formulas, names and dates.
- **A sheet is materialised as a dense rectangle.** Memory is proportional to the bounding box, not
  to the cells that are filled in, and `.ods` also holds the sheet twice while assembling it — see
  [Memory](#memory). Use [`worksheet_rows`](#streaming-rows) when that matters, which it does for
  any sheet large enough to be worth worrying about.

## Design


```
src/
  container.rs   the swappable archive trait, ZIP backend, format sniffing
  cfb.rs         OLE compound file reader, from scratch (used by .xls)
  xml.rs         XML pull parser, from scratch
  refs.rs        A1 reference parsing and rendering (public)
  formula.rs     shared-formula reference shifting
  datatype.rs    Data, CellError, ExcelDateTime
  range.rs       the dense Range<T>
  reader.rs      the Reader trait and header handling
  de.rs          serde row deserialization (feature `serde`)
  open.rs        format detection and the open_* entry points
  formats/
    xlsx.rs      OOXML
    ods.rs       OpenDocument
    xls.rs       BIFF8
```

Three properties are worth calling out:

- **Sheets are parsed on demand.** Opening a workbook reads metadata only. The shared string
  table is not touched until a sheet needs it, so a workbook with millions of strings you
  never read costs nothing. `.xls` is the exception, because its sheet substreams are
  addressed by offset in a stream that has to be read whole anyway.
- **Everything is bounds-checked.** No slicing without a length check, no unchecked
  arithmetic on file-supplied values, no panics on malformed input.
- **Coordinates are absolute.** A range knows where it sits in the sheet, so a cell keeps its
  identity across trimming and merging.

## Tests

```
cargo test --features serde
```

203 tests: unit tests per module, integration tests that read real workbooks, tests that hold the
streaming, windowed and dense views of the same sheet against each other, and three that measure peak
allocation. The window path has its own edges pinned, because it decides where to stop reading: a
comment that looks like a row close, rows listed out of order, rows that close themselves, and a
deflated part stopped mid-stream.

Four suites exist because of defects found in review, and each one is a guard rather than a
demonstration:

- `tests/adversarial.rs` — attacks, not correctness checks: amplification, truncation at every
  offset, 400 deterministic bit flips, deep nesting, out-of-grid references, and declared sizes
  that would allocate a gigabyte. Memory is measured with a counting allocator, so "bounded" is a
  number.
- `tests/regressions.rs` — one test per defect found in review, each carrying a note on why the
  input is dangerous.
- `tests/entities.rs` — the entity decoder stays linear, and text that only looks like an entity
  survives.
- `tests/visibility.rs`, `tests/merged_regions.rs`, `tests/container.rs` — the reader-level
  features and the container contract.

The fixtures are deliberately not self-generated where a real file was available:

- `tests/data/chanora.xlsx` — a nine-sheet report from a real scan tool, covering shared
  strings, integers, multi-line licence text and long strings.
- `tests/data/rollmilkice.xlsx` — a smaller real export.
- `tests/data/word97.doc` — a genuine OLE compound file used to validate the container
  reader's FAT, mini-FAT and directory-tree walking (`.xls` uses the same container).
- `tests/data/sample.ods` — built from `tests/data/ods-src/` to exercise column/row repeat
  counts, every ODS value type, and `text:s` / `text:line-break` runs. Regenerate it by
  zipping that directory's contents.

`.xls` is the one format tested against a constructed fixture rather than a real file: a
structurally valid BIFF8 workbook is assembled in-memory by a test helper, with a real FAT
and directory, covering every cell record type the reader handles. Building it that way is
what caught a real bug — a skipped format-index field in the shared-string cell record that
happened to read back correctly for zero-valued cells and wrongly for everything else.

The fixture also declares code page 1252 and holds a compressed string containing byte `0x80`,
so the test fails unless the reader picks the code page up from the file rather than assuming
it. That assertion is the reason the `codepage` feature is trusted rather than hoped for.

Three of the hardening limits are covered by regression tests rather than left to inspection.
Those build ODS packages in memory with a few hundred bytes of markup that would otherwise
expand to millions of cells or hundreds of megabytes, and assert that reading them fails
cleanly instead of allocating — including one that asserts opening a workbook with such a body
succeeds, which is what pins the metadata-only guarantee for ODS.

## License

MIT. See [LICENSE](LICENSE).

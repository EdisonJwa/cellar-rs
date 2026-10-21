# Changelog

All notable changes to this project are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.2] - 2026-10-21

Documentation only. No code change.

### Changed

- **The README is rewritten.** The previous one opened with an account of the `zip` crate being
  yanked from crates.io and what `calamine` pins, which introduced this crate by describing a
  neighbouring project's bad week. It now opens on what the crate does and how to start, with one
  section per task.
- Every performance figure was re-measured. The old table quoted `.ods` numbers from before the
  parser work landed (71 ms against a measured 83 ms), and `.xlsx` whole-sheet 31 ms against 49.
- Test count corrected from 206 to 232 across twelve suites, with the doc-test count stated
  separately.

### Added

- Doc-tests for the front-page sample and the error-handling match, the two README snippets that
  had none behind them. 22 doc-tests now compile every example in the file.

## [0.1.1] - 2026-10-21

A metadata-only patch. No API change.

### Fixed

- The crate description read "pure Rust" where the README and documentation say "pure-Rust".


## [0.1.0] - 2026-10-20

The first release. Read-only, and complete for the three formats it covers.

### Added

**Reading**

- **OOXML (`.xlsx`, `.xlsm`, `.xltx`, `.xlam`)**, **OpenDocument (`.ods`)** and **legacy BIFF8
  (`.xls`)** support, detected by content rather than extension.
- **`Data`**, a cell value that distinguishes the cases a spreadsheet actually has: integer, float,
  text, boolean, Excel serial date with its epoch, ISO date and duration text, in-cell error, and
  empty. Forgiving accessors (`as_f64`, `as_i64`, `as_string`, `as_date`, `as_duration`, …).
- **`Range<T>`**, a dense rectangle that keeps absolute coordinates, so a cell keeps its identity
  across trimming and merging.
- **`worksheet_rows`** — streaming reads that materialise nothing. A sheet whose cells sit at `A1`
  and `XFD1048576` implies 17 billion cells; the dense path refuses it and this reads it in
  constant space.
- **`worksheet_range_ref`** — windowed reads that skip everything outside the window rather than
  decoding and discarding it. In OOXML decompression stops once the window's rows have closed, and
  that cut is verified against the parser instead of trusted.
- **`worksheet_formula`** — formula text, with shared formulas expanded per cell by shifting each
  relative reference.
- **`worksheet_merged_regions`** — merged-cell regions, read from `<mergeCells>`, `MERGEDCELLS`
  records, or `number-columns-spanned`/`number-rows-spanned` depending on the format.
- **`sheet_visibility`** — visible, hidden or very hidden, per sheet.
- **Defined names, sheet metadata, and header-row trimming** that keeps coordinates absolute.

**Features**

- **`serde`** — row deserialization matching columns to struct fields by header name, so a struct
  works when the sheet's columns are reordered. The column plan is computed once per range.
- **`codepage`** — correct legacy `.xls` text via `encoding_rs`, covering the Windows pages (874,
  1250–1258), the ISO-8859 pages, KOI8-R/U, IBM866 and Macintosh. Without it the `0x80`–`0x9F` band
  is read as Latin-1, which is wrong for exactly the characters Western European text uses.
- **`dates`** — `chrono` conversions.

**Tooling**

- `compare/` — a separate workspace measuring this crate against `calamine` on the same files, so
  the README's claims can be re-derived rather than believed.
- Six compiled examples covering every public entry point.
- The README's samples are doc-tests, so a snippet that stops building fails the suite.

### Fixed

Found in review, each with a regression test carrying a note on why the input is dangerous:

- **DIFAT extension sectors were parsed off by one.** Entry 0 was skipped and the next-DIFAT pointer
  ingested as a FAT sector, leaving the allocation table one sector short. Every 512-byte-sector
  workbook above roughly 6.8 MiB failed with "stream chain points outside the FAT".
- **Compound-file directory slots were compacted during parsing.** Entries address one another by
  slot index, so a single unused slot shifted every later index and could hide the workbook
  entirely. Unused slots are now retained as placeholders.
- **A mid-stream `DIMENSIONS` record discarded every cell decoded before it.** The builder was
  replaced rather than hinted; it is now adopted only while still empty.
- **`open_xls` accepted any bytes.** A non-BIFF input produced an empty, successful workbook. A
  leading `BOF` record is now required.
- **ODS repeat counts could overflow the row cursor.** Empty repeats are free, so ~120 KB of markup
  drove the cursor to `u32::MAX` and the next repeat panicked in debug (and wrapped silently in
  release), breaking the "never panics" guarantee. Out-of-grid repeats are refused instead.
- **ODS `<text:list>` content was dropped**, because only direct `<text:p>` children were read.
- **ODS booleans written as `1` or `TRUE` became empty**, because only literal `true`/`false` were
  accepted.
- **`worksheet_formula` swallowed a budget refusal** into an empty range, while the value read
  reported it.
- **A formula read depended on the shared string table.** A damaged `sharedStrings.xml` failed a
  read that never needed it, and a workbook with millions of strings paid to inflate them.
- **An error-typed cell with an empty value produced `Error(Unknown(""))`** instead of empty.
- **Dense and streaming disagreed about an impossible row index.** The dense path refused; streaming
  handed back an addressable one.
- **A row source pushing uneven columns and values could index past the end of `values`.** The
  invariant is now enforced rather than assumed.

### Security

- **The cell budget counted `Data` slots but not string payloads.** A slot is 32 bytes whether
  empty or owning a megabyte, so cells all naming one shared string passed the budget while
  allocating far more heap than it allowed — a 49 KB file measured at 23 MB, and unbounded at a
  larger shape. Text is now charged as it is stored, against a separate 256 MiB budget, and the
  refusal lands before the allocation.
- **`<text:s text:c="N"/>` could amplify without limit.** Each element was capped, the total per
  cell was not: 656 bytes expanded to 40 MB. A per-cell text budget closes it.
- **A `<dimension>` element could demand a gigabyte.** `A1:AF1048576` is about a hundred bytes
  naming exactly the grid's 33.5 million cells. A declared extent now reserves at most 64 MiB, and
  the buffer still grows to the true extent.
- **`.xls` had no per-part ceiling.** The ZIP path bounded every entry at 4 GiB; the compound-file
  path bounded nothing. It does now, and a stream whose chain ends before its declared size is
  reported where it happens.
- **The entity decoder was quadratic** on text full of stray ampersands. The scan is now bounded by
  the longest name an entity can have.
- **Header-row trimming copied the dense buffer**, doubling peak memory on exactly the large sheets
  the builder exists to keep single-copy.
- `unsafe` forbidden crate-wide.
- `tests/adversarial.rs` attacks the reader rather than testing it: truncation at every offset, 400
  deterministic bit flips, 100,000-deep nesting, amplification, and oversized declared sizes, with
  memory measured by a counting allocator.

### Notes

- The archive container is `zipcore`, this project's own ZIP reader, behind one internal trait. It
  is the only non-optional dependency, and it has no `unsafe` either.
- `.ods` whole-sheet reads are slower than the OOXML path because the format declares no extent, so
  the sheet is assembled through a sparse list and held twice while doing so. Measured, and stated
  in the README rather than glossed over.
- MSRV is **1.88**, which is where `slice::as_chunks` stabilised.

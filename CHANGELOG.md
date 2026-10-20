# Changelog

All notable changes to this project are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Nothing yet.

## [0.1.0] — 2026-10-20

The first release. Read-only, and complete for the three formats it covers.

### Added

- **OOXML (`.xlsx`, `.xlsm`, `.xltx`, `.xlam`)**, **OpenDocument (`.ods`)** and **legacy BIFF8
  (`.xls`)** reading, detected by content rather than extension.
- **`Data`**, a cell value that distinguishes the cases a spreadsheet actually has: integer,
  float, text, boolean, Excel serial date (with its epoch), ISO date and duration text, in-cell
  error, and empty. Forgiving accessors (`as_f64`, `as_i64`, `as_string`, `as_date`, …) so callers
  rarely match by hand.
- **`Range<T>`**, a dense rectangle that keeps absolute coordinates, so a cell keeps its identity
  across trimming and merging.
- **`worksheet_rows`** — streaming reads that materialise nothing. A sheet whose cells sit at
  `A1` and `XFD1048576` implies 17 billion cells; the dense path refuses it and this reads it in
  constant space.
- **`worksheet_range_ref`** — windowed reads that skip everything outside the window rather than
  decoding and discarding it. In OOXML decompression stops once the window's rows have closed,
  and that cut is verified against the parser instead of trusted.
- **`worksheet_formula`** — formula text, with shared formulas expanded per cell by shifting each
  relative reference. `.xls` reports `Unsupported`, because BIFF stores compiled token streams.
- **`worksheet_merged_regions`** — merged-cell regions, read from `<mergeCells>`, `MERGEDCELLS`
  records, or `number-columns-spanned`/`number-rows-spanned`, depending on the format.
- **`sheet_visibility`** — visible, hidden or very hidden, per sheet.
- **Defined names, sheet metadata, and header-row trimming** that keeps coordinates absolute.
- **`serde` row deserialization** behind a feature, matching columns to struct fields by header
  name so a struct works when the sheet's columns are reordered. The column plan is computed once
  per range, not once per row.
- **`codepage`** feature decoding compressed BIFF strings through `encoding_rs`, covering the
  Windows pages (874, 1250–1258), the ISO-8859 pages, KOI8-R/U, IBM866 and Macintosh. Without it
  the 0x80–0x9F band is read as Latin-1, which is wrong for exactly the characters Western
  European text uses.
- **`dates`** feature adding `chrono` conversions.

### Security

- `unsafe` forbidden crate-wide.
- No archive entry or compound-file stream exceeds 4 GiB, measured on the bytes produced rather
  than a declared length that a crafted header can understate.
- A cell's text is bounded at 1 MiB; without it, 656 bytes of `<text:s>` runs expanded to 40 MB.
- A declared `<dimension>` reserves at most 64 MiB; without it a ~100-byte element demanded a 1 GiB
  allocation.
- Repeat counts are charged against a cell budget as each cell is produced, and a repeat that
  would carry a row or column index past the grid is refused rather than saturated.
- Every allocation table walk is bounded and cycle checked.
- DTDs are skipped rather than resolved, so entity-expansion and external-entity attacks have
  nothing to work with, and the entity decoder is linear rather than quadratic.
- `tests/adversarial.rs` attacks the reader rather than testing it: truncation at every offset,
  400 deterministic bit flips, 100,000-deep nesting, out-of-grid references, amplification, and
  oversized declared sizes. Memory is measured with a counting allocator.

### Notes

- The archive container is `zipcore`, this project's own ZIP reader, behind one internal trait.
  It is the only non-optional dependency, and it has no `unsafe` either.
- `.ods` whole-sheet reads are slower than the OOXML path because the format declares no extent,
  so the sheet is assembled through a sparse list and held twice while doing so. Measured, and
  stated in the README rather than glossed over.

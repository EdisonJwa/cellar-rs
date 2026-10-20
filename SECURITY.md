# Security policy

`cellar` parses untrusted files. Every input is treated as hostile, and the failures that matter
are the ones a crafted workbook can cause: a panic, an unbounded allocation, a hang, or data
returned for something that should have been refused.

## Reporting a vulnerability

**Open a private security advisory** on the repository rather than a public issue:

<https://github.com/EdisonJwa/cellar-rs/security/advisories/new>

Please include the file that triggers it if you can — a malformed workbook is usually small enough
to attach. There is no bounty, but expect an initial response within a week.

## What is in scope

Anything reachable through the public API:

- `open_workbook`, `open_workbook_auto_from_rs` and the per-format constructors
- every `Reader` method: `worksheet_range`, `worksheet_rows`, `worksheet_range_ref`,
  `worksheet_formula`, `worksheet_merged_regions`, `sheet_visibility`
- the `serde` deserialization layer

The container parsers, the XML tokenizer and the BIFF record decoder are all in-tree, so a defect
in any of them is in scope. `zipcore` is a separate crate with its own policy.

## What the crate promises

These are enforced by tests, not intended. A report showing any of them false is a security bug.

### No panic on malformed input

`tests/adversarial.rs` truncates a valid archive at every offset, flips 400 bits
deterministically, nests elements 100,000 deep, feeds degenerate inputs, and drives row and column
indices past the grid. None may panic. The crate is `#![forbid(unsafe_code)]`, crate-wide.

### No allocation without a bound

| Bound | Value | Applies to |
|---|---|---|
| Per archive entry / compound stream | 4 GiB | measured on bytes **produced**, not a declared length |
| Cell text per sheet | 256 MiB | charged as text is stored, before the allocation |
| Declared `<dimension>` reservation | 64 MiB | the buffer still grows to the true extent |
| Text per cell (ODS) | 1 MiB | `<text:s>` runs, charged incrementally |
| Materialised sheet | 1 GiB | the dense rectangle |
| Archive entry count | 65,536 | the index grows as entries are read |

Memory is measured with a counting allocator, so these are numbers rather than intentions. Two of
the bounds exist because the first attempt at them was measured wrong: a `Data` slot is 32 bytes
whether empty or owning a megabyte, so a budget derived from slot size could not see string
payloads (a 49 KB file measured at 23 MB), and 656 bytes of `<text:s>` expanded to 40 MB before a
per-cell text budget existed.

### No unbounded work

Every loop over file data makes forward progress, every allocation-chain walk is cycle-checked and
step-bounded, and the entity decoder is linear in its input rather than quadratic.

### Inconsistencies are errors

A bad signature, a truncated stream, or a declared size the file cannot satisfy is reported rather
than returned as valid. A compound-file stream whose chain ends before its declared size is
reported where it happens, instead of surfacing later as an unexplained end-of-stream.

## What is deliberately not a vulnerability

- **Resource use on a legitimately large file.** A one-gigabyte sheet that really holds a gigabyte
  of cells will use about a gigabyte. The dense read refuses an implausible rectangle, and
  `worksheet_rows` streams when the shape is the problem.
- **Encrypted workbooks.** Detected and refused with `Error::Encrypted`. Decryption is out of
  scope, not a gap.
- **Formats named as unsupported.** XLSB, BIFF5/7 and ODS `manifest.xml` encryption are reported as
  unsupported rather than guessed at. A file refused for a documented reason is behaving as
  designed.
- **Hiding as access control.** `sheet_visibility` reports what the workbook says. Hiding a sheet is
  presentation; its cells are readable by design.
- **A window near the bottom of a sheet being slow.** Bytes before a window are not seekable inside
  a deflated part, so a window deep into a sheet costs a scan up to that row. Documented, measured,
  and inherent to the format rather than a defect.

## Supported versions

The crate is pre-1.0; fixes land on `main` and are released as the next `0.1.x`. There are no
maintained back-branches yet.

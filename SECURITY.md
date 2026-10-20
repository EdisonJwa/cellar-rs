# Security policy

`cellar` parses untrusted files. Every input is treated as hostile, and the failures that matter
are the ones a crafted workbook can cause: a panic, an unbounded allocation, a hang, or data
returned for something that should have been refused.

## Reporting a vulnerability

Open a private security advisory on the repository rather than a public issue. Please include the
file that triggers it if you can; a malformed workbook is usually small enough to attach.

There is no bounty. Expect an initial response within a week.

## What is covered

Reports are welcome for anything reachable through the public API:

- `open_workbook`, `open_workbook_auto_from_rs` and the per-format constructors
- every `Reader` method: `worksheet_range`, `worksheet_rows`, `worksheet_range_ref`,
  `worksheet_formula`, `worksheet_merged_regions`, `sheet_visibility`
- the `serde` deserialization layer

The container parsers, the XML tokenizer and the BIFF record decoder are all in-tree, so a defect
in any of them is in scope. `zipcore` is a separate crate with its own policy.

## What the crate already promises

These are enforced by tests rather than intended, and a report showing any of them is false is a
security bug:

- **No panic on malformed input.** `tests/adversarial.rs` truncates a valid archive at every
  offset, flips 400 bits, nests elements 100,000 deep, and feeds degenerate inputs. None may
  panic.
- **No allocation without a bound.** No archive entry or compound-file stream exceeds 4 GiB.
  Declared extents reserve at most 64 MiB. A cell's text stops at 1 MiB. Repeat counts are
  charged against a cell budget as each cell is produced. Memory is measured with a counting
  allocator, so these are numbers rather than intentions.
- **No unbounded work.** Every loop over file data makes progress, every chain walk is cycle
  checked and step bounded, and the entity decoder is linear in the length of its input.
- **`unsafe` is forbidden crate-wide**, enforced by `#![forbid(unsafe_code)]`.

## What is deliberately not a vulnerability

- **Resource use on a legitimately large file.** A one-gigabyte sheet that really holds a
  gigabyte of cells will use about a gigabyte. The dense read refuses a rectangle that is
  implausible rather than trying, and `worksheet_rows` streams when the shape is the problem.
- **Encrypted workbooks.** They are detected and refused with `Error::Encrypted`. Decryption is
  out of scope, not a gap.
- **Formats named as unsupported.** XLSB, BIFF5/7 and ODS `manifest.xml` encryption are reported
  as unsupported rather than guessed at. A file that is refused for a documented reason is
  behaving as designed.
- **Hiding as access control.** `sheet_visibility` reports what the workbook says. Hiding a sheet
  is presentation; its cells are readable by design.

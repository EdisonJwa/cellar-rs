# Contributing

Thanks for taking a look. This is a small, single-purpose crate, so the most useful contributions
are usually a failing input rather than a patch — a workbook that reads wrongly, or one that makes
the reader do something it promised not to.

## Reporting a bug in the parser

Please include **the file**, or the smallest fragment of it that still reproduces the problem. A
malformed workbook is usually small enough to attach. If the file is confidential, this is often
enough on its own:

- the format and the producer (`.xlsx` from Excel 2019, `.ods` from LibreOffice 7.6, …)
- the sheet and cell where the value is wrong
- what the reader returned, and what the producing application shows

For anything that looks security-relevant — a panic, a hang, or memory use out of proportion to the
file — please use the private advisory flow in [SECURITY.md](SECURITY.md) instead of a public issue.

## Before sending a pull request

```
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo package
```

All four run in CI on Linux, Windows and macOS across five feature sets. A change that fails one of
them will not be merged, and the CI run on the pull request will say which.

## What a good change looks like here

**A parser fix comes with a fixture.** `tests/regressions.rs` holds one test per defect that has
been found, and each carries a note explaining why its input is dangerous. A fix without a test is a
fix that can silently un-fix itself. Fixtures built by hand belong in the test file that uses them;
a real file that reproduces the bug is even better, and goes in `tests/data/`.

**Performance claims come with a measurement.** `compare/` measures this crate against
`calamine` on identical work, and its `ab` binary interleaves the two readers so that a busy machine
affects both equally. Absolute timings on a developer laptop drift by more than the differences
being measured — the last time this project trusted them, it published a table that was wrong in
three cells. If a change is meant to be faster, show the number from `ab`.

**A rejected idea is worth writing down.** Two restructurings of the `.ods` reader were tried,
measured, found slower, and reverted; the README says which and why, so the next person does not
spend the same afternoon on them. If you try something that does not work, a note in the pull
request is genuinely useful.

**Bounds are not optional.** The crate reads untrusted input. Any new allocation driven by a number
in the file needs a bound, and any new loop over file data needs to make forward progress. The
existing limits are listed in [SECURITY.md](SECURITY.md); if a change needs a new one, say so in the
description.

## Style

- Comments explain *why*, not *what*. The code already says what it does.
- A comment that would be obvious from the code is noise; a comment explaining a trade-off is not.
- Public items need documentation, because `#![warn(missing_docs)]` is on.
- `unsafe` is forbidden crate-wide and the build will fail if it appears.

## Scope

The crate is read-only, and deliberately so. Writing spreadsheets is a different problem with a
different set of hazards, and mixing them would make the security story harder to state.

Formats outside the three it covers — XLSB, BIFF5/BIFF7, and anything predating Excel 97 — are
rejected with a clear error rather than guessed at. Adding one is welcome if it can be done without
weakening that: a file refused for a documented reason is behaving correctly, and a file misread is
not.

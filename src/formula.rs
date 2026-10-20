//! Adjusting cell references inside a formula by a row/column offset.
//!
//! Excel stores a filled-down column of the same formula once, on the group's master cell,
//! and marks every other member with an offset instead of repeating the text. Recovering the
//! text for those cells means shifting each relative reference in the master by that offset
//! while leaving absolute (`$`) parts alone.
//!
//! This is a token-level rewrite, not a parse: it walks the formula and rewrites the spans that
//! look like references, skipping anything where a reference cannot occur.

use crate::refs::{col_from_letters, col_to_letters};

/// Largest row and column Excel's grid can address.
const MAX_ROW: i64 = 1_048_576;
const MAX_COL: i64 = 16_384;

/// Rewrites every relative A1-style reference in `formula` by `dr` rows and `dc` columns.
///
/// A reference that would fall outside the grid is left as written.
pub(crate) fn shift_formula(formula: &str, dr: i64, dc: i64) -> String {
    if (dr == 0 && dc == 0) || formula.is_empty() {
        return formula.to_owned();
    }
    let bytes = formula.as_bytes();
    let mut out = String::with_capacity(formula.len());
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'"' => i = copy_quoted(formula, bytes, i, b'"', &mut out),
            // A quoted sheet name such as 'My Sheet'!A1.
            b'\'' => i = copy_quoted(formula, bytes, i, b'\'', &mut out),
            // Structured table references and external workbook paths live in brackets; the
            // text inside is a column name, not a cell reference.
            b'[' => i = copy_bracketed(formula, bytes, i, &mut out),
            b'$' | b'A'..=b'Z' | b'a'..=b'z' => match try_reference(formula, bytes, i, dr, dc) {
                Some((end, shifted)) => {
                    out.push_str(&shifted);
                    i = end;
                }
                None => i = copy_char(formula, i, &mut out),
            },
            _ => i = copy_char(formula, i, &mut out),
        }
    }
    out
}

/// Copies one whole character, so multi-byte text survives untouched.
fn copy_char(formula: &str, at: usize, out: &mut String) -> usize {
    match formula[at..].chars().next() {
        Some(ch) => {
            out.push(ch);
            at + ch.len_utf8()
        }
        None => at + 1,
    }
}

/// Copies a single- or double-quoted span, treating a doubled quote as an escaped one.
fn copy_quoted(formula: &str, bytes: &[u8], start: usize, quote: u8, out: &mut String) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            i += 1;
            break;
        }
        i += 1;
    }
    out.push_str(&formula[start..i]);
    i
}

/// Copies a bracketed span, which holds a column name rather than a reference.
fn copy_bracketed(formula: &str, bytes: &[u8], start: usize, out: &mut String) -> usize {
    let mut i = start + 1;
    while i < bytes.len() && bytes[i] != b']' {
        i += 1;
    }
    if i < bytes.len() {
        i += 1;
    }
    out.push_str(&formula[start..i]);
    i
}

/// Tries to read a reference at `start`, returning its end offset and shifted text.
///
/// Guard rails matter here because formulas are full of things that resemble references:
/// `LOG10` looks like column `LOG` row `10`, and `1E5` looks like column `E` row `5`.
fn try_reference(
    formula: &str,
    bytes: &[u8],
    start: usize,
    dr: i64,
    dc: i64,
) -> Option<(usize, String)> {
    // Never match from the middle of an identifier, number or existing reference.
    // A leading `!` is fine, and in fact expected: `Sheet1!A1` is an ordinary reference.
    if start > 0 {
        let prev = bytes[start - 1];
        if prev.is_ascii_alphanumeric() || matches!(prev, b'_' | b'.' | b'$') {
            return None;
        }
    }

    let mut i = start;
    let col_absolute = bytes[i] == b'$';
    if col_absolute {
        i += 1;
    }

    let letters_start = i;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    let letters = &formula[letters_start..i];
    // Excel's widest column is XFD, so more than three letters is not a reference.
    if letters.is_empty() || letters.len() > 3 {
        return None;
    }

    let row_absolute = bytes.get(i) == Some(&b'$');
    if row_absolute {
        i += 1;
    }
    let digits_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let digits = &formula[digits_start..i];
    if digits.is_empty() || digits.len() > 7 {
        return None;
    }

    // `LOG10(` is a function call, and a trailing word character means we are inside a name.
    if let Some(&next) = bytes.get(i) {
        if next == b'(' || next.is_ascii_alphanumeric() || matches!(next, b'_' | b'.' | b'$') {
            return None;
        }
    }

    let col = col_from_letters(letters)?;
    let row: i64 = digits.parse().ok()?;
    if row < 1 {
        return None;
    }

    let new_row = if row_absolute { row } else { row + dr };
    let new_col = if col_absolute {
        i64::from(col)
    } else {
        i64::from(col) + dc
    };
    // Anything pushed off the grid is left as authored rather than becoming an invented `#REF!`.
    if !(1..=MAX_ROW).contains(&new_row) || !(0..MAX_COL).contains(&new_col) {
        return None;
    }

    let mut shifted = String::with_capacity(8);
    if col_absolute {
        shifted.push('$');
    }
    shifted.push_str(&col_to_letters(new_col as u32));
    if row_absolute {
        shifted.push('$');
    }
    shifted.push_str(&new_row.to_string());
    Some((i, shifted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shifts_relative_references() {
        assert_eq!(shift_formula("B1+C1", 3, 0), "B4+C4");
        assert_eq!(shift_formula("SUM(B1:C1)", 1, 1), "SUM(C2:D2)");
        assert_eq!(shift_formula("A1", 0, 1), "B1");
        assert_eq!(shift_formula("A1", 1, 0), "A2");
    }

    #[test]
    fn respects_absolute_anchors() {
        assert_eq!(shift_formula("$A$1+B2", 2, 2), "$A$1+D4");
        assert_eq!(shift_formula("$A1+A$1", 1, 1), "$A2+B$1");
        assert_eq!(shift_formula("A$5", 3, 0), "A$5");
    }

    /// The worked example from the README, so the two cannot drift apart.
    #[test]
    fn expands_a_filled_column_and_row() {
        let master = "SUM($B2:D2)";
        assert_eq!(shift_formula(master, 1, 0), "SUM($B3:D3)");
        assert_eq!(shift_formula(master, 0, 1), "SUM($B2:E2)");
        assert_eq!(shift_formula(master, 2, 2), "SUM($B4:F4)");
    }

    #[test]
    fn leaves_non_references_alone() {
        // A function name that looks like column LOG row 10.
        assert_eq!(shift_formula("LOG10(A1)", 1, 0), "LOG10(A2)");
        // Scientific notation.
        assert_eq!(shift_formula("1E5+A1", 1, 0), "1E5+A2");
        assert_eq!(shift_formula("1.5E-3+A1", 1, 0), "1.5E-3+A2");
        // A defined name.
        assert_eq!(shift_formula("MyName+A1", 1, 0), "MyName+A2");
    }

    #[test]
    fn leaves_quoted_text_alone() {
        assert_eq!(shift_formula("\"A1\"&B1", 1, 0), "\"A1\"&B2");
        assert_eq!(shift_formula("'Sheet A1'!B1", 1, 0), "'Sheet A1'!B2");
        assert_eq!(
            shift_formula("\"say \"\"A1\"\"\"&C3", 1, 0),
            "\"say \"\"A1\"\"\"&C4"
        );
    }

    #[test]
    fn leaves_structured_references_alone() {
        assert_eq!(
            shift_formula("SUM(Table1[Column1])+A1", 1, 0),
            "SUM(Table1[Column1])+A2"
        );
    }

    #[test]
    fn drops_off_grid_references() {
        // A1 shifted up by one row is off the grid, so it stays as written.
        assert_eq!(shift_formula("A1+B2", -1, 0), "A1+B1");
        assert_eq!(shift_formula("A1+B2", 0, -1), "A1+A2");
    }

    #[test]
    fn identity_is_free() {
        assert_eq!(shift_formula("SUM(A1:B2)", 0, 0), "SUM(A1:B2)");
    }
}

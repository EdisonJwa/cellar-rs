//! A1-style cell reference handling.
//!
//! These are the conversions between the way spreadsheets name cells (`A1`, `$B$2`,
//! `Sheet1!C3`) and the zero-based `(row, column)` pairs the rest of this crate uses. They are
//! public because any caller that builds or inspects ranges needs them.

/// Converts a base-26 column reference (`A`, `Z`, `AA`) to a zero-based column index.
///
/// Returns `None` for anything that is not one to seven letters.
pub fn col_from_letters(letters: &str) -> Option<u32> {
    if letters.is_empty() || letters.len() > 7 {
        return None;
    }
    let mut col: u32 = 0;
    for ch in letters.chars() {
        let v = ch.to_ascii_uppercase() as u32;
        if !(b'A' as u32..=b'Z' as u32).contains(&v) {
            return None;
        }
        col = col.checked_mul(26)?.checked_add(v - b'A' as u32 + 1)?;
    }
    Some(col - 1)
}

/// Converts a zero-based column index to its letters (`0` -> `A`, `26` -> `AA`).
pub fn col_to_letters(mut col: u32) -> String {
    let mut out = Vec::new();
    loop {
        out.push(b'A' + (col % 26) as u8);
        if col < 26 {
            break;
        }
        col = col / 26 - 1;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// Parses an A1-style cell reference into a zero-based `(row, column)`.
///
/// Absolute markers (`$`) and a leading sheet qualifier are ignored, so all of
/// `A1`, `$A$1`, `Sheet1!A1` and `'My Sheet'!$B$2` resolve.
pub fn parse_cell_ref(s: &str) -> Option<(u32, u32)> {
    // Drop a sheet qualifier, taking the last one so a quoted name containing '!' still works.
    let cell = match s.rfind('!') {
        Some(i) => &s[i + 1..],
        None => s,
    };
    let cell = cell.trim();
    let cell = cell.strip_prefix('$').unwrap_or(cell);
    let split = cell.find(|c: char| !c.is_ascii_alphabetic())?;
    let (letters, rest) = cell.split_at(split);
    let rest = rest.strip_prefix('$').unwrap_or(rest);
    let col = col_from_letters(letters)?;
    let row: u32 = rest.parse().ok()?;
    if row == 0 {
        return None;
    }
    Some((row - 1, col))
}

/// Parses an A1-style range reference into its two corners: `(top-left, bottom-right)`.
///
/// A single cell is treated as a one-cell range.
pub fn parse_range_ref(s: &str) -> Option<((u32, u32), (u32, u32))> {
    match s.split_once(':') {
        Some((a, b)) => Some((parse_cell_ref(a)?, parse_cell_ref(b)?)),
        None => parse_cell_ref(s).map(|c| (c, c)),
    }
}

/// Renders a zero-based cell position as an A1-style reference.
pub fn cell_ref(row: u32, col: u32) -> String {
    format!("{}{}", col_to_letters(col), row + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_letters_round_trip() {
        assert_eq!(col_from_letters("A"), Some(0));
        assert_eq!(col_from_letters("Z"), Some(25));
        assert_eq!(col_from_letters("AA"), Some(26));
        assert_eq!(col_from_letters("AZ"), Some(51));
        assert_eq!(col_from_letters("BA"), Some(52));
        assert_eq!(col_from_letters("XFD"), Some(16383));
        for col in [0u32, 1, 25, 26, 51, 52, 701, 702, 16383] {
            assert_eq!(col_from_letters(&col_to_letters(col)), Some(col));
        }
        assert_eq!(col_from_letters(""), None);
        assert_eq!(col_from_letters("A1"), None);
    }

    #[test]
    fn cell_refs_parse() {
        assert_eq!(parse_cell_ref("A1"), Some((0, 0)));
        assert_eq!(parse_cell_ref("B2"), Some((1, 1)));
        assert_eq!(parse_cell_ref("$AA$10"), Some((9, 26)));
        assert_eq!(parse_cell_ref("Sheet1!C3"), Some((2, 2)));
        assert_eq!(parse_cell_ref("'My Sheet'!D4"), Some((3, 3)));
        assert_eq!(parse_cell_ref("XFD1048576"), Some((1048575, 16383)));
        assert_eq!(parse_cell_ref("A0"), None);
        assert_eq!(parse_cell_ref("A"), None);
        assert_eq!(parse_cell_ref("1"), None);
        assert_eq!(parse_cell_ref(""), None);
    }

    #[test]
    fn cell_refs_render() {
        assert_eq!(cell_ref(0, 0), "A1");
        assert_eq!(cell_ref(9, 26), "AA10");
        assert_eq!(cell_ref(1048575, 16383), "XFD1048576");
    }

    #[test]
    fn range_refs_parse() {
        assert_eq!(parse_range_ref("A1:B2"), Some(((0, 0), (1, 1))));
        assert_eq!(parse_range_ref("C3"), Some(((2, 2), (2, 2))));
    }
}

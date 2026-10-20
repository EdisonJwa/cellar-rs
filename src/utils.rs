use crate::error::{Error, Result};

/// A bounds-checked forward cursor over a byte slice.
///
/// Used by the binary format readers (the OLE container and BIFF records), where every read has
/// to be validated against a length that came from the file itself.
#[derive(Debug, Clone)]
pub struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Cursor { data, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub fn read_u8(&mut self) -> Result<u8> {
        let b = *self.data.get(self.pos).ok_or(Error::Eof)?;
        self.pos += 1;
        Ok(b)
    }

    pub fn read_u16(&mut self) -> Result<u16> {
        let bytes = self.read_bytes(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    pub fn read_u32(&mut self) -> Result<u32> {
        let bytes = self.read_bytes(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_u64(&mut self) -> Result<u64> {
        let bytes = self.read_bytes(8)?;
        let mut buf = [0u8; 8];
        buf.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(buf))
    }

    pub fn read_f64(&mut self) -> Result<f64> {
        self.read_u64().map(f64::from_bits)
    }

    pub fn read_bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(Error::Eof);
        }
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }
}

/// The built-in number formats from ECMA-376 §18.8.30, by id.
///
/// Ids in `27..=36` and `50..=58` are locale-specific date/time formats that have no canonical
/// code string; [`is_builtin_date_format`] recognises them instead.
pub fn builtin_format(id: u16) -> Option<&'static str> {
    Some(match id {
        0 => "General",
        1 => "0",
        2 => "0.00",
        3 => "#,##0",
        4 => "#,##0.00",
        9 => "0%",
        10 => "0.00%",
        11 => "0.00E+00",
        12 => "# ?/?",
        13 => "# ??/??",
        14 => "mm-dd-yy",
        15 => "d-mmm-yy",
        16 => "d-mmm",
        17 => "mmm-yy",
        18 => "h:mm AM/PM",
        19 => "h:mm:ss AM/PM",
        20 => "h:mm",
        21 => "h:mm:ss",
        22 => "m/d/yy h:mm",
        37 => "#,##0 ;(#,##0)",
        38 => "#,##0 ;[Red](#,##0)",
        39 => "#,##0.00;(#,##0.00)",
        40 => "#,##0.00;[Red](#,##0.00)",
        45 => "mm:ss",
        46 => "[h]:mm:ss",
        47 => "mmss.0",
        48 => "##0.0E+0",
        49 => "@",
        _ => return None,
    })
}

/// Whether a built-in format id is a date or time, including the locale-specific ranges.
pub fn is_builtin_date_format(id: u16) -> bool {
    matches!(id, 14..=22 | 27..=36 | 45..=47 | 50..=58)
}

/// Whether a number format code renders a date or time.
///
/// The decision is made on the tokens that survive stripping quoted literals, bracketed
/// colour/locale/condition prefixes and escaped or skipped characters — so `[Red]0.00` is
/// numeric while `[$-409]m/d/yy` is a date.
pub fn is_date_format(code: &str) -> bool {
    let mut in_quote = false;
    let mut in_bracket = false;
    let mut bracket = String::new();
    let mut chars = code.chars();
    while let Some(c) = chars.next() {
        if in_quote {
            if c == '"' {
                in_quote = false;
            }
            continue;
        }
        if in_bracket {
            match c {
                ']' => {
                    in_bracket = false;
                    // `[h]`, `[mm]` and `[ss]` are elapsed-time indicators rather than
                    // decorative prefixes, so they do count as a time format.
                    if !bracket.is_empty()
                        && bracket
                            .chars()
                            .all(|b| matches!(b, 'h' | 'H' | 'm' | 'M' | 's' | 'S'))
                    {
                        return true;
                    }
                    bracket.clear();
                }
                _ => bracket.push(c),
            }
            continue;
        }
        match c {
            '"' => in_quote = true,
            // A backslash escapes the next character; `_` sets a skip width and `*` repeats a
            // fill character. None of those payloads can be a date token.
            '\\' | '_' | '*' => {
                chars.next();
            }
            '[' => in_bracket = true,
            'y' | 'Y' | 'd' | 'D' | 'h' | 'H' | 's' | 'S' | 'm' | 'M' => return true,
            _ => {}
        }
    }
    false
}

/// Resolves a number format id plus any custom code to "is this a date/time?".
pub fn format_is_date(num_fmt_id: u16, custom_code: Option<&str>) -> bool {
    if let Some(code) = custom_code {
        return is_date_format(code);
    }
    match builtin_format(num_fmt_id) {
        Some(code) => is_date_format(code),
        None => is_builtin_date_format(num_fmt_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_formats_detected() {
        for code in [
            "mm-dd-yy",
            "d-mmm-yy",
            "h:mm:ss",
            "m/d/yy h:mm",
            "yyyy-mm-dd",
            "[h]:mm:ss",
            "[$-409]m/d/yy",
            "mmm",
            "h:mm AM/PM",
            "yyyy\"年\"m\"月\"",
        ] {
            assert!(is_date_format(code), "{code} should be a date");
        }
        for code in [
            "General",
            "0",
            "#,##0.00",
            "#,##0 ;(#,##0)",
            "[Red]0.00",
            "0.00E+00",
            "\"$\"#,##0.00",
            "0.0%",
            "@",
            "#,##0_);[Red](#,##0)",
            "\"days\"0",
        ] {
            assert!(!is_date_format(code), "{code} should not be a date");
        }
    }

    #[test]
    fn builtin_date_ids() {
        assert!(is_builtin_date_format(14));
        assert!(is_builtin_date_format(22));
        assert!(is_builtin_date_format(45));
        assert!(is_builtin_date_format(50));
        assert!(!is_builtin_date_format(0));
        assert!(!is_builtin_date_format(49));
        assert!(format_is_date(14, None));
        assert!(!format_is_date(0, None));
        assert!(format_is_date(999, Some("yyyy-mm-dd")));
    }

    #[test]
    fn cursor_reads_within_bounds() {
        let data = [1u8, 2, 0, 0, 0, 0, 0, 0];
        let mut cursor = Cursor::new(&data);
        assert_eq!(cursor.read_u8().unwrap(), 1);
        assert_eq!(cursor.read_u16().unwrap(), 2);
        assert_eq!(cursor.remaining(), 5);
        assert_eq!(cursor.read_u32().unwrap(), 0);
        assert_eq!(cursor.remaining(), 1);
        // Not enough left for another field.
        assert!(cursor.read_u16().is_err());
        assert!(Cursor::new(&data[..1]).read_u16().is_err());
        assert!(Cursor::new(&[]).read_u8().is_err());
    }
}

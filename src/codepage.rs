//! Decoding the one-byte-per-character form of a BIFF string.
//!
//! BIFF stores every string one of two ways: as UTF-16, one unit per character, or "compressed" as
//! one byte per character to be read in the workbook's code page. The compressed form is what this
//! module handles.
//!
//! Without the `codepage` feature the byte is taken as its Latin-1 code point. That is exact for
//! ASCII, which is the overwhelming majority of compressed strings, and exact for the ISO-8859
//! pages whose high range happens to agree with Latin-1. It is *wrong* for the Windows code pages
//! in the 0x80..=0x9F band, which is where the euro sign and the typographic quotes and dashes
//! live — ordinary text in a Western European workbook.
//!
//! The `codepage` feature closes that by decoding through `encoding_rs`.

/// Maps a byte of a compressed BIFF string to the character it stands for.
pub(crate) type ByteMap = [char; 256];

/// Builds the mapping for a workbook's declared code page.
///
/// A code page this crate does not know about falls back to Latin-1 rather than guessing.
pub(crate) fn byte_map(_codepage: u16) -> ByteMap {
    let mut map = ['\0'; 256];
    for (byte, slot) in map.iter_mut().enumerate() {
        *slot = char::from(byte as u8);
    }

    #[cfg(feature = "codepage")]
    if let Some(encoding) = encoding_for(_codepage) {
        for (byte, slot) in map.iter_mut().enumerate() {
            *slot = decode_one(encoding, byte as u8);
        }
    }

    map
}

/// The encoding for a code page, for the ones whose high range differs from Latin-1.
///
/// Only single-byte code pages are listed. A double-byte code page such as Shift_JIS cannot
/// express its text through the compressed form at all — one byte per character cannot hold a
/// two-byte character — so such a workbook writes UTF-16 instead, and a compressed string in it is
/// ASCII. Latin-1 already handles that correctly, so those code pages are deliberately absent.
#[cfg(feature = "codepage")]
fn encoding_for(codepage: u16) -> Option<&'static encoding_rs::Encoding> {
    use encoding_rs::*;
    Some(match codepage {
        // Windows code pages.
        874 => WINDOWS_874,
        1250 => WINDOWS_1250,
        1251 => WINDOWS_1251,
        // What a BIFF8 file means when it declares nothing.
        0 | 1252 => WINDOWS_1252,
        1253 => WINDOWS_1253,
        1254 => WINDOWS_1254,
        1255 => WINDOWS_1255,
        1256 => WINDOWS_1256,
        1257 => WINDOWS_1257,
        1258 => WINDOWS_1258,
        10000 => MACINTOSH,
        20866 => KOI8_R,
        21866 => KOI8_U,
        866 => IBM866,
        // ISO code pages. The encoding standard folds a few of these onto the Windows code page
        // that supersedes them, which is why 28591 and 28599 resolve to windows-1252 and -1254.
        28591 => WINDOWS_1252,
        28592 => ISO_8859_2,
        28593 => ISO_8859_3,
        28594 => ISO_8859_4,
        28595 => ISO_8859_5,
        28596 => ISO_8859_6,
        28597 => ISO_8859_7,
        28598 => ISO_8859_8,
        28599 => WINDOWS_1254,
        28600 => ISO_8859_10,
        28601 => WINDOWS_874,
        28603 => ISO_8859_13,
        28604 => ISO_8859_14,
        28605 => ISO_8859_15,
        28606 => ISO_8859_16,
        _ => return None,
    })
}

/// Decodes a single byte.
///
/// A byte the code page leaves undefined, or one that would need a following byte, keeps its
/// Latin-1 interpretation rather than becoming a replacement character: that is lossless, and it
/// lets a caller see that the file held no character there.
#[cfg(feature = "codepage")]
fn decode_one(encoding: &'static encoding_rs::Encoding, byte: u8) -> char {
    let single = [byte];
    let (decoded, had_errors) = encoding.decode_without_bom_handling(&single);
    match (decoded.chars().next(), had_errors) {
        (Some(ch), false) => ch,
        _ => char::from(byte),
    }
}

/// Appends a character to a UTF-16 buffer, splitting it into a surrogate pair if it is outside the
/// basic plane.
pub(crate) fn push_char(units: &mut Vec<u16>, ch: char) {
    let code = ch as u32;
    if code <= 0xFFFF {
        units.push(code as u16);
    } else {
        let offset = code - 0x1_0000;
        units.push(0xD800 + (offset >> 10) as u16);
        units.push(0xDC00 + (offset & 0x3FF) as u16);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_stable_in_every_code_page() {
        let map = byte_map(1252);
        for byte in 0x20u8..0x7F {
            assert_eq!(map[byte as usize], char::from(byte));
        }
    }

    #[test]
    fn unknown_code_pages_fall_back_to_latin1() {
        // A double-byte code page is not decoded byte-wise; Latin-1 keeps ASCII intact.
        let map = byte_map(932);
        assert_eq!(map[b'A' as usize], 'A');
        assert_eq!(map[0xE9], char::from(0xE9u8));
    }

    #[test]
    fn push_char_handles_both_planes() {
        let mut units = Vec::new();
        push_char(&mut units, 'A');
        assert_eq!(units, vec![0x41]);
        units.clear();
        push_char(&mut units, '\u{1F600}');
        assert_eq!(String::from_utf16_lossy(&units), "\u{1F600}");
    }

    /// The bug the `codepage` feature exists to fix: the 0x80..=0x9F band of a Windows code page
    /// does not agree with Latin-1.
    #[cfg(feature = "codepage")]
    #[test]
    fn windows_code_pages_are_decoded_not_taken_as_latin1() {
        let map = byte_map(1252);
        assert_eq!(
            map[0x80], '\u{20AC}',
            "0x80 is the euro sign in windows-1252"
        );
        assert_eq!(map[0x92], '\u{2019}', "0x92 is a right single quote");
        assert_eq!(map[0x96], '\u{2013}', "0x96 is an en dash");
        // Outside the differing band the two agree.
        assert_eq!(map[0xE9], '\u{00E9}');
    }

    #[cfg(feature = "codepage")]
    #[test]
    fn other_code_pages_decode_too() {
        // Cyrillic and Central European pages differ from Latin-1 across the whole high range.
        assert_eq!(byte_map(1251)[0xC0], '\u{0410}', "0xC0 is Cyrillic A");
        assert_eq!(byte_map(1250)[0xA5], '\u{0104}', "0xA5 is A-ogonek");
        // An undefined byte keeps its Latin-1 reading rather than becoming U+FFFD.
        assert_eq!(byte_map(1252)[0x81], char::from(0x81u8));
    }
}

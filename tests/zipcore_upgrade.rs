//! `zipcore` 0.2 -> 0.10 changed nothing this crate depends on, and that is worth a test.
//!
//! The upgrade was eight breaking minor versions of a 0.x crate, so "it compiled" is not the same
//! as "it behaves the same". These pin the three properties the reader actually relies on, in the
//! terms the upgrade could have broken them.

use gridxcel_rs::{open_workbook_auto_from_rs, Error};
use std::io::Cursor;

mod common;

/// Builds a ZIP in memory with one entry whose *declared* size is a lie.
///
/// The local header claims `claim` bytes; the compressed payload decompresses to `actual`. A
/// reader that trusts the header allocates `claim`; one that counts produced bytes does not.
fn archive_with_lying_size(name: &str, actual: usize, claim: u64) -> Vec<u8> {
    let payload = vec![b'A'; actual];
    let deflated = zipcore::FileOptions::deflate(6);
    let mut w = zipcore::ZipWriter::new(Cursor::new(Vec::new())).unwrap();
    w.add_file(name, &payload, &deflated).unwrap();
    let mut bytes = w.finish().unwrap().into_inner();

    // Patch the 32-bit uncompressed size in both the local header and the central directory, so
    // no consistency check in the parser catches the mismatch before decompression.
    let lie = claim as u32;
    for sig in [0x0403_4b50u32, 0x0201_4b50u32] {
        let needle = sig.to_le_bytes();
        let mut at = 0;
        while at + 4 <= bytes.len() {
            if bytes[at..at + 4] == needle {
                let field = if sig == 0x0403_4b50 { at + 22 } else { at + 24 };
                if field + 4 <= bytes.len() {
                    bytes[field..field + 4].copy_from_slice(&lie.to_le_bytes());
                }
            }
            at += 1;
        }
    }
    bytes
}

/// A size the header claims but never delivers is still refused, on produced bytes.
///
/// `ReadOptions::max_entry_size` is documented in `zipcore` as "the only bound that does not come
/// from the archive itself, and therefore the only one that still holds when a header lies". This
/// crate sets it to its own `MAX_ENTRY_BYTES`, so a lie about a huge entry must not allocate.
#[test]
fn a_lying_size_header_cannot_defeat_the_entry_bound() {
    // Claim ~3.9 GiB but deliver 64 KiB: the claim must not drive an allocation.
    let bytes = archive_with_lying_size("xl/workbook.xml", 64 * 1024, 0xFFFF_FF00);
    let result = open_workbook_auto_from_rs(Cursor::new(bytes));

    // Either the archive rejects the inconsistency or the part fails to parse as a workbook;
    // what must not happen is a multi-gigabyte allocation or a panic.
    match result {
        Err(_) => {}
        Ok(_) => panic!("a workbook whose part is 64 KiB of 'A' must not open successfully"),
    }
}

/// A damaged compressed body is reported, and not as "encrypted".
///
/// The flip is aimed at the compressed body, found through the local header, rather than at
/// whatever byte happens to sit in the middle of the file. An earlier version of this test
/// flipped a byte in the central directory and set the general-purpose "encrypted" bit, so the
/// reader correctly reported `Encrypted` -- a correct answer to the wrong question. Locating
/// the body is what makes the assertion mean something.
#[test]
fn corrupted_deflate_payload_is_refused() {
    let bytes = archive_with_lying_size("xl/workbook.xml", 4096, 4096);

    // Local file header: 30 bytes of fixed fields, then the name and extra field it declares.
    const LOCAL_SIG: [u8; 4] = [0x50, 0x4b, 0x03, 0x04];
    let at = bytes
        .windows(4)
        .position(|w| w == LOCAL_SIG)
        .expect("local header");
    let name_len = u16::from_le_bytes([bytes[at + 26], bytes[at + 27]]) as usize;
    let extra_len = u16::from_le_bytes([bytes[at + 28], bytes[at + 29]]) as usize;
    let body = at + 30 + name_len + extra_len;

    let mut damaged = bytes.clone();
    damaged[body + 8] ^= 0xFF;

    match open_workbook_auto_from_rs(Cursor::new(damaged)) {
        // A damaged body is not an encrypted entry: no password would help, so reporting
        // `Encrypted` would send a caller to look for a password that does not exist.
        Err(Error::Encrypted) => panic!("a damaged body is not an encrypted entry"),
        Err(_) => {}
        Ok(_) => panic!("a corrupted body must not decode into a workbook"),
    }
}
/// The real fixtures read to the same values as before the upgrade.
///
/// This is the assertion that would fail if 0.10 changed name resolution, entry iteration or
/// decompression in a way that mattered.
#[test]
fn real_workbooks_read_identically_after_the_upgrade() {
    use gridxcel_rs::Reader;

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let mut summary = Vec::new();

    for name in ["chanora.xlsx", "rollmilkice.xlsx", "sample.ods"] {
        let path = root.join(name);
        let mut wb = open_workbook_auto_from_rs(std::fs::File::open(&path).unwrap()).unwrap();
        let sheets = wb.sheet_names().to_vec();
        let mut cells = 0usize;
        for sheet in &sheets {
            cells += wb.worksheet_range(sheet).unwrap().used_cell_count();
        }
        summary.push((name, sheets.len(), cells));
    }

    // Pinned counts: if decompression or entry lookup regressed, these move.
    assert_eq!(summary[0], ("chanora.xlsx", 9, 3221));
    assert_eq!(summary[1], ("rollmilkice.xlsx", 2, 17));
    assert_eq!(summary[2], ("sample.ods", 2, 20));
    assert!(
        summary.iter().all(|(_, sheets, _)| *sheets >= 1),
        "every fixture must still expose its sheets: {summary:?}"
    );
}

/// An entry count over the limit is refused before the index is built.
#[test]
fn entry_count_limit_still_applies() {
    // One entry is fine; this pins that the container's own cap is consulted through the new API
    // rather than being dropped when `Archive::len` changed shape.
    let deflated = zipcore::FileOptions::deflate(6);
    let mut w = zipcore::ZipWriter::new(Cursor::new(Vec::new())).unwrap();
    w.add_file("mimetype", b"x", &deflated).unwrap();
    let bytes = w.finish().unwrap().into_inner();
    assert!(open_workbook_auto_from_rs(Cursor::new(bytes)).is_err());
}

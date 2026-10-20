//! The archive layer: how bytes get out of a container and into a format reader.
//!
//! Every format reader talks to the [`Archive`] trait and never to a concrete container.
//! That is deliberate. The ZIP side is served by [`zipcore`](https://docs.rs/zipcore), a
//! from-scratch ZIP reader in this project: swapping it again means another implementation of
//! this one trait and nothing else. `.xls` files are not ZIP containers at all, so they are
//! served by [`crate::cfb`], which is likewise implemented here rather than borrowed.

use crate::error::{Error, Result};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};

/// Largest single entry this crate will decompress.
///
/// A sheet part of a genuinely enormous workbook can reach a gigabyte, so the ceiling is set
/// well above that. It still stops the classic decompression bomb, where a few kilobytes of
/// archive expand to hundreds of gigabytes.
pub const MAX_ENTRY_BYTES: u64 = 4 << 30;

/// Largest number of parts this crate will index in one archive.
///
/// A real workbook has tens of parts; an enormous one has a few thousand. The bound exists
/// because the entry count comes from the file, and indexing it is an allocation driven by
/// attacker-controlled data.
const MAX_ENTRIES: usize = 1 << 16;

/// Largest buffer reserved up front from a size the file declares.
///
/// Reserving the declared size outright would let a few bytes of header demand a multi-gigabyte
/// allocation, so the reservation is capped and the buffer grows past it if the part really is
/// that big. Above this the doubling on the way up is not worth the risk of trusting the header.
pub(crate) const MAX_RESERVE: u64 = 8 << 20;

/// Random access to the named parts of a container.
pub(crate) trait Archive {
    /// The contents of `name`, or `None` when the container has no such part.
    fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>>;

    /// Opens `name` for incremental reading.
    ///
    /// Reading a part whole is what most callers want, but a reader that already knows it needs
    /// only the start of a part can stop early and save the rest of the work. Dropping the reader
    /// is what stops it.
    ///
    /// The default reads the part whole and hands over the bytes: correct, and simply unable to
    /// stop early. A container that can decompress incrementally overrides it.
    fn open(&mut self, name: &str) -> Result<Option<Box<dyn Read + '_>>> {
        Ok(self
            .read(name)?
            .map(|bytes| Box::new(std::io::Cursor::new(bytes)) as Box<dyn Read + '_>))
    }

    /// Whether the container has a part called `name`.
    fn contains(&self, name: &str) -> bool;

    /// Every part name, in container order.
    fn names(&self) -> Vec<String>;
}

/// Normalises a part name so lookups survive the inconsistencies producers exhibit:
/// leading slashes, backslash separators and differing case.
fn normalize_name(name: &str) -> String {
    let trimmed = name.trim_start_matches(['/', '\\']);
    let mut out = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        out.push(match ch {
            '\\' => '/',
            c => c.to_ascii_lowercase(),
        });
    }
    out
}

/// Reads one chunk of a part into `out`, refusing to go past `limit`.
///
/// Every reader of a part goes through here, so the whole-part read and the early-stopping one are
/// bounded the same way rather than each having to remember. The bound is on the bytes actually
/// produced rather than on the size the file declares, because a crafted header can understate
/// that — which is how a few kilobytes of archive become hundreds of gigabytes of expansion.
///
/// Returns how many bytes this call added, zero at the end of the part, so a caller reading only a
/// prefix can stop wherever it likes.
pub(crate) fn read_part(
    reader: &mut impl Read,
    buf: &mut [u8],
    out: &mut Vec<u8>,
    limit: u64,
) -> Result<usize> {
    loop {
        match reader.read(buf) {
            Ok(n) => {
                if out.len() as u64 + n as u64 > limit {
                    return Err(Error::container(format!(
                        "part exceeds the {limit} byte limit"
                    )));
                }
                out.extend_from_slice(&buf[..n]);
                return Ok(n);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::Io(e)),
        }
    }
}

/// Reads a part whole, up to [`MAX_ENTRY_BYTES`].
///
/// `declared` is used only to reserve, and only up to [`MAX_RESERVE`], since a header can overstate
/// as easily as understate.
fn read_limited(reader: &mut impl Read, declared: u64) -> Result<Vec<u8>> {
    let reserve = declared.min(MAX_ENTRY_BYTES).min(MAX_RESERVE) as usize;
    let mut out = Vec::with_capacity(reserve);
    let mut buf = [0u8; 64 * 1024];
    while read_part(reader, &mut buf, &mut out, MAX_ENTRY_BYTES)? != 0 {}
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{read_part, MAX_ENTRY_BYTES};
    use std::io::Read;

    /// The limit is on the bytes produced, so a part that declares little and yields much is
    /// refused rather than allowed to grow without bound. Exercised with a small limit because the
    /// real one is four gigabytes and reaching it would take four gigabytes.
    #[test]
    fn a_part_that_produces_more_than_its_limit_is_refused() {
        let mut buf = [0u8; 8];
        let mut out = Vec::new();

        // A reader that would produce a byte forever if it were asked to.
        let mut endless = std::io::repeat(b'x');
        let error = loop {
            if let Err(error) = read_part(&mut endless, &mut buf, &mut out, 32) {
                break error;
            }
        };
        assert!(
            error.to_string().contains("byte limit"),
            "expected the limit to be reported, got {error}"
        );
        assert_eq!(out.len(), 32, "exactly the limit should have been read");

        // The same content fits when it is under the limit.
        let mut small = std::io::repeat(b'x').take(32);
        let mut fits = Vec::new();
        while read_part(&mut small, &mut buf, &mut fits, MAX_ENTRY_BYTES).unwrap() != 0 {}
        assert_eq!(fits.len(), 32);
    }
}

/// A ZIP container, backed by `zipcore`.
///
/// The format readers only see [`Archive`], so the backend is free to change; this is the one
/// place in the crate that knows what a ZIP file is.
pub(crate) struct ZipContainer<RS: Read + Seek> {
    archive: zipcore::Archive<RS>,
    /// Normalised part name to entry index, so lookups do not scan the directory.
    ///
    /// `zipcore` resolves names by exact match, while producers spell the same part several
    /// ways. Normalising once, at open, keeps the tolerant behaviour the readers rely on.
    index: HashMap<String, usize>,
}

impl<RS: Read + Seek> ZipContainer<RS> {
    /// Opens a ZIP container.
    pub(crate) fn new(reader: RS) -> Result<Self> {
        // `zipcore` caps decompressed bytes per entry from its own default; the crate's own
        // ceiling is applied by `read_part`/`read_limited` below, so the two agree by
        // construction rather than by coincidence.
        let options = zipcore::ReadOptions {
            max_entry_size: Some(MAX_ENTRY_BYTES),
            ..zipcore::ReadOptions::default()
        };
        let archive =
            zipcore::Archive::new_with_options(reader, options).map_err(container_error)?;
        let count = archive.len();
        if count > MAX_ENTRIES {
            return Err(Error::container(format!(
                "archive declares {count} entries, over the {MAX_ENTRIES} limit"
            )));
        }
        // Preallocating from the declared count would let the file drive the allocation, so the
        // map grows as entries are actually read instead.
        let mut index = HashMap::with_capacity(count.min(1024));
        for (i, entry) in archive.entries().iter().enumerate() {
            index.entry(normalize_name(entry.name())).or_insert(i);
        }
        Ok(ZipContainer { archive, index })
    }

    /// Reclaims the underlying reader, used by format detection to avoid re-opening a file.
    pub(crate) fn into_inner(self) -> RS {
        self.archive.into_inner()
    }

    /// The index of a part, tolerating the spelling differences `normalize_name` folds.
    fn lookup(&self, name: &str) -> Result<Option<usize>> {
        let key = normalize_name(name);
        if let Some(&i) = self.index.get(&key) {
            return Ok(Some(i));
        }
        // The part name may differ only by case from what the archive records, which
        // `normalize_name` already folds; a miss here means the archive lacks it.
        Ok(None)
    }
}

impl<RS: Read + Seek> Archive for ZipContainer<RS> {
    fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
        let Some(i) = self.lookup(name)? else {
            return Ok(None);
        };
        // The directory stays readable for an encrypted workbook, so opening the entry is what
        // reports a password being required.
        let mut file = self.archive.by_index(i).map_err(entry_error)?;
        if file.entry().uncompressed_size() > MAX_ENTRY_BYTES {
            return Err(Error::container(format!(
                "part {name} declares {} bytes, over the {MAX_ENTRY_BYTES} byte limit",
                file.entry().uncompressed_size()
            )));
        }
        let declared = file.entry().uncompressed_size();
        read_limited(&mut file, declared).map(Some)
    }

    fn open(&mut self, name: &str) -> Result<Option<Box<dyn Read + '_>>> {
        let Some(i) = self.lookup(name)? else {
            return Ok(None);
        };
        let file = self.archive.by_index(i).map_err(entry_error)?;
        if file.entry().uncompressed_size() > MAX_ENTRY_BYTES {
            return Err(Error::container(format!(
                "part {name} declares {} bytes, over the {MAX_ENTRY_BYTES} byte limit",
                file.entry().uncompressed_size()
            )));
        }
        Ok(Some(Box::new(file)))
    }

    fn contains(&self, name: &str) -> bool {
        self.index.contains_key(&normalize_name(name))
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<(usize, String)> = self
            .index
            .iter()
            .map(|(name, &i)| (i, name.clone()))
            .collect();
        names.sort_unstable();
        names.into_iter().map(|(_, name)| name).collect()
    }
}

/// Maps a failure to open the archive itself onto the crate's error type.
fn container_error(error: zipcore::Error) -> Error {
    match error {
        zipcore::Error::InvalidSignature => {
            Error::container("not a zip archive: no end-of-central-directory record")
        }
        zipcore::Error::Io(e) => Error::Io(e),
        other => Error::container(other.to_string()),
    }
}

/// Maps a failure to open one entry onto the crate's error type.
///
/// The distinctions matter to callers: an encrypted workbook is not corruption, and a
/// compression method this build cannot read is a limitation rather than damage.
fn entry_error(error: zipcore::Error) -> Error {
    match error {
        zipcore::Error::Encrypted { .. } => Error::Encrypted,
        zipcore::Error::UnsupportedCompression { entry, method } => Error::unsupported(format!(
            "part {entry} uses compression method {method}, which this build cannot read"
        )),
        zipcore::Error::Unsupported(what) => {
            Error::unsupported(format!("zip feature not supported: {what}"))
        }
        zipcore::Error::Io(e) => Error::Io(e),
        other => Error::container(other.to_string()),
    }
}

/// A container we know how to read.
///
/// Non-exhaustive because more formats are planned; adding one must not be a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Format {
    /// OOXML workbook: `.xlsx`, `.xlsm`, `.xltx`, `.xlam`.
    Xlsx,
    /// OpenDocument spreadsheet: `.ods`.
    Ods,
    /// Legacy BIFF workbook: `.xls`.
    Xls,
}

/// The BIFF record stream starts with a BOF record.
const BIFF_BOF: [u8; 2] = [0x09, 0x08];
/// OLE compound file magic.
pub(crate) const CFB_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

/// Identifies a workbook from its leading bytes.
///
/// A ZIP prefix only narrows things to "some OOXML or OpenDocument package"; the caller must
/// look inside the archive to choose. `None` means the bytes match nothing this crate reads.
pub(crate) fn sniff(head: &[u8]) -> Option<Format> {
    if head.starts_with(&CFB_MAGIC) {
        return Some(Format::Xls);
    }
    if head.starts_with(&BIFF_BOF) {
        return Some(Format::Xls);
    }
    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        // Refined by the caller once the archive directory can be read.
        return Some(Format::Xlsx);
    }
    None
}

/// Picks between the two ZIP-based formats by looking for a part unique to each.
pub(crate) fn sniff_zip<RS: Read + Seek>(reader: RS) -> Result<(Format, RS)> {
    let container = ZipContainer::new(reader)?;
    let format = if container.contains("xl/workbook.xml") {
        Format::Xlsx
    } else if container.contains("content.xml") {
        Format::Ods
    } else if container.contains("xl/workbook.bin") {
        // An XLSB package is shaped like a workbook but stores its parts in the binary format.
        // Saying so beats letting it fail later with a missing-part error.
        return Err(Error::unsupported(
            "XLSB package: only the XML-based OOXML formats are supported",
        ));
    } else if container.contains("[Content_Types].xml") {
        return Err(Error::unsupported(
            "OOXML package with no spreadsheet part; it is a different kind of Office document",
        ));
    } else if container.contains("mimetype") {
        Format::Ods
    } else {
        return Err(Error::format(
            "zip archive is neither an OOXML workbook nor an OpenDocument spreadsheet",
        ));
    };
    let mut reader = container.into_inner();
    reader.seek(SeekFrom::Start(0))?;
    Ok((format, reader))
}

/// Reads up to `buf.len()` bytes, returning how many were read.
pub(crate) fn read_head(reader: &mut impl Read, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Ok(filled)
}

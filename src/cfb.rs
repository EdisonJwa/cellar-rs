//! A from-scratch reader for the OLE Compound File Binary container (MS-CFB).
//!
//! `.xls` workbooks predate ZIP packaging: they live inside a small FAT filesystem with its
//! own directory tree, stream allocation table and a second "mini" allocation table for
//! streams under 4096 bytes. Nothing here comes from a crate, because the container is
//! specific to this format family.
//!
//! The design streams from the underlying reader: the allocation tables and directory are
//! loaded eagerly (they are small), while stream contents are pulled on demand. Every chain
//! walk is bounded and cycle-checked, because a malformed file can point a sector at itself.

use crate::container::{Archive, MAX_ENTRY_BYTES, MAX_RESERVE};
use crate::error::{Error, Result};
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};

const SIGNATURE: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
/// A sector that is not in use.
const FREESECT: u32 = 0xFFFF_FFFF;
/// The last sector of a chain.
const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
/// The largest valid sector number.
const MAXREGSECT: u32 = 0xFFFF_FFFA;

/// Object type of a directory entry.
const TYPE_STORAGE: u8 = 1;
const TYPE_STREAM: u8 = 2;
const TYPE_ROOT: u8 = 5;

/// One node of the directory tree.
#[derive(Debug, Clone)]
pub(crate) struct DirEntry {
    pub name: String,
    pub kind: u8,
    pub start: u32,
    pub size: u64,
    left: u32,
    right: u32,
    child: u32,
}

/// A parsed compound file.
pub(crate) struct Cfb<RS> {
    reader: RS,
    sector_size: usize,
    mini_sector_size: usize,
    /// Next-sector pointers for the main allocation table.
    fat: Vec<u32>,
    /// Next-sector pointers for the mini allocation table.
    mini_fat: Vec<u32>,
    dir: Vec<DirEntry>,
    /// The pool that small streams are carved out of.
    mini_stream: Vec<u8>,
    mini_cutoff: u32,
    /// Sectors the file actually holds, the bound every chain walk is held to.
    ///
    /// A walk bounded only by the length of the allocation table can iterate about `file_len/4`
    /// times, appending a whole sector each time — a two-sector cycle in a 0.46 MB file reached an
    /// 807 MB buffer before the cycle was noticed. The file's own length is the honest bound.
    total_sectors: u64,
    /// Normalised name to directory index, for lookups.
    index: HashSet<(String, u8)>,
}

impl<RS: Read + Seek> Cfb<RS> {
    /// Parses a compound file container.
    pub(crate) fn new(mut reader: RS) -> Result<Self> {
        let total_len = reader.seek(SeekFrom::End(0))?;
        reader.seek(SeekFrom::Start(0))?;

        let mut header = [0u8; 512];
        reader
            .read_exact(&mut header)
            .map_err(|_| Error::format("file is too small to be an OLE compound file"))?;
        if header[..8] != SIGNATURE {
            return Err(Error::format("not an OLE compound file"));
        }

        let sector_shift = u16::from_le_bytes([header[0x1E], header[0x1F]]);
        if sector_shift != 9 && sector_shift != 12 {
            return Err(Error::format(format!(
                "unsupported sector shift {sector_shift}"
            )));
        }
        // A 4096-byte sector file still has a 512-byte header, padded out to one sector.
        let sector_size = 1usize << sector_shift;
        let mini_shift = u16::from_le_bytes([header[0x20], header[0x21]]);
        if !(2..=sector_shift).contains(&mini_shift) {
            return Err(Error::format(format!(
                "unsupported mini sector shift {mini_shift}"
            )));
        }
        let mini_sector_size = 1usize << mini_shift;
        let version = u16::from_le_bytes([header[0x1A], header[0x1B]]);

        let num_fat_sectors = u32::from_le_bytes(header[0x2C..0x30].try_into().unwrap());
        let first_dir_sector = u32::from_le_bytes(header[0x30..0x34].try_into().unwrap());
        let mini_cutoff = u32::from_le_bytes(header[0x38..0x3C].try_into().unwrap());
        let first_mini_fat = u32::from_le_bytes(header[0x3C..0x40].try_into().unwrap());
        let num_mini_fat = u32::from_le_bytes(header[0x40..0x44].try_into().unwrap());
        let first_difat = u32::from_le_bytes(header[0x44..0x48].try_into().unwrap());
        let num_difat = u32::from_le_bytes(header[0x48..0x4C].try_into().unwrap());

        let total_sectors = total_len / sector_size as u64;
        let mut cfb = Cfb {
            reader,
            sector_size,
            mini_sector_size,
            fat: Vec::new(),
            mini_fat: Vec::new(),
            dir: Vec::new(),
            mini_stream: Vec::new(),
            mini_cutoff: if mini_cutoff == 0 { 4096 } else { mini_cutoff },
            total_sectors,
            index: HashSet::new(),
        };

        let difat = cfb.build_difat(first_difat, num_difat, &header[0x4C..0x200], total_sectors)?;
        if num_fat_sectors as u64 > total_sectors + 1 {
            return Err(Error::format("FAT sector count exceeds file size"));
        }
        cfb.build_fat(&difat, num_fat_sectors, total_sectors)?;
        cfb.build_mini_fat(first_mini_fat, num_mini_fat)?;
        cfb.read_directory(first_dir_sector)?;

        // The root entry is slot zero by specification, and describes where small streams live.
        if let Some(root) = cfb.dir.first().filter(|e| e.kind == TYPE_ROOT).cloned() {
            if root.size > 0 && root.start <= MAXREGSECT {
                cfb.mini_stream = cfb.read_fat_stream(root.start, root.size)?;
            }
        }

        if version != 3 && version != 4 {
            return Err(Error::format(format!(
                "unsupported compound file version {version}"
            )));
        }

        // Only real, named streams go in the lookup index; placeholders hold a slot but name
        // nothing.
        for entry in &cfb.dir {
            if entry.kind != 0 && !entry.name.is_empty() {
                cfb.index.insert((normalize(&entry.name), entry.kind));
            }
        }
        Ok(cfb)
    }

    /// Gathers the list of FAT sectors from the header and any extension sectors.
    fn build_difat(
        &mut self,
        first_difat: u32,
        num_difat: u32,
        header_difat: &[u8],
        total_sectors: u64,
    ) -> Result<Vec<u32>> {
        let mut difat = Vec::new();
        for chunk in header_difat.as_chunks::<4>().0 {
            let v = u32::from_le_bytes(*chunk);
            if v != FREESECT && v != ENDOFCHAIN {
                difat.push(v);
            }
        }
        let per_sector = self.sector_size / 4;
        if per_sector < 2 {
            return Err(Error::format("sector size too small for a DIFAT"));
        }
        let mut sector = first_difat;
        let mut seen = 0u32;
        let limit = num_difat.saturating_add(1).min(total_sectors as u32 + 1);
        while sector <= MAXREGSECT && seen < limit {
            let bytes = self.read_sector(sector)?;
            // A DIFAT extension sector holds FAT sector numbers in every slot but the last,
            // which is the pointer to the next extension sector. Reading from slot 1 would
            // drop the sector listed in slot 0 and add the pointer as if it were a FAT sector,
            // leaving the table one entry short.
            for i in 0..per_sector - 1 {
                let v = u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
                if v != FREESECT && v != ENDOFCHAIN {
                    difat.push(v);
                }
            }
            let next = u32::from_le_bytes(
                bytes[(per_sector - 1) * 4..per_sector * 4]
                    .try_into()
                    .unwrap(),
            );
            sector = next;
            seen += 1;
        }
        if difat.len() as u64 > total_sectors + 1 {
            return Err(Error::format(
                "DIFAT lists more sectors than the file holds",
            ));
        }
        Ok(difat)
    }

    /// Loads the main allocation table.
    fn build_fat(
        &mut self,
        difat: &[u32],
        num_fat_sectors: u32,
        _total_sectors: u64,
    ) -> Result<()> {
        // A header that claims more FAT sectors than the DIFAT lists cannot be satisfied, and
        // proceeding would leave the table short: every sector beyond it then reads as "outside
        // the FAT", far from the actual cause. Reporting it here keeps the diagnosis local.
        if (difat.len() as u64) < u64::from(num_fat_sectors) {
            return Err(Error::format(format!(
                "header declares {num_fat_sectors} FAT sectors but the DIFAT lists {}",
                difat.len()
            )));
        }
        let entries_per_sector = self.sector_size / 4;
        self.fat = Vec::with_capacity(difat.len() * entries_per_sector);
        for (i, &sector) in difat.iter().enumerate() {
            if u32::try_from(i)
                .map(|n| n >= num_fat_sectors)
                .unwrap_or(true)
            {
                break;
            }
            let bytes = self.read_sector(sector)?;
            for chunk in bytes.as_chunks::<4>().0 {
                self.fat.push(u32::from_le_bytes(*chunk));
            }
        }
        Ok(())
    }

    /// Loads the mini allocation table, which lives in its own FAT chain.
    fn build_mini_fat(&mut self, first: u32, count: u32) -> Result<()> {
        if first > MAXREGSECT || count == 0 {
            return Ok(());
        }
        let bytes = self.read_fat_stream(first, u64::from(count) * self.sector_size as u64)?;
        self.mini_fat = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect();
        Ok(())
    }

    /// Loads the directory stream and parses its 128-byte entries.
    ///
    /// Every slot in the stream becomes an element of `dir`, including unused ones, because
    /// `left`/`right`/`child` are indices into the *stream* and are resolved against this
    /// vector. Skipping a slot would shift every later index and make the tree walk land on the
    /// wrong entries — which is exactly what a file with a deleted stream looks like.
    fn read_directory(&mut self, first_dir_sector: u32) -> Result<()> {
        let mut raw = Vec::new();
        let mut sector = first_dir_sector;
        let mut steps = 0usize;
        while sector <= MAXREGSECT {
            raw.extend_from_slice(&self.read_sector(sector)?);
            sector = *self
                .fat
                .get(sector as usize)
                .ok_or_else(|| Error::format("directory chain points outside the FAT"))?;
            steps += 1;
            // Bounded by the file, not by the allocation table: a cycle can be revisited many
            // times before a table-length bound trips, and each visit appends a sector.
            if steps as u64 > self.total_sectors + 1 {
                return Err(Error::format("cyclic directory chain"));
            }
        }
        for chunk in raw.as_chunks::<128>().0 {
            let kind = chunk[0x42];
            if kind == 0 {
                // An unused slot still occupies a position, and the indices depend on it.
                self.dir.push(DirEntry {
                    name: String::new(),
                    kind: 0,
                    start: 0,
                    size: 0,
                    left: FREESECT,
                    right: FREESECT,
                    child: FREESECT,
                });
                continue;
            }
            let name_len = u16::from_le_bytes([chunk[0x40], chunk[0x41]]) as usize;
            let name = if (2..=64).contains(&name_len) {
                let utf16: Vec<u16> = chunk[..name_len - 2]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(*c))
                    .collect();
                // Trailing NULs occasionally pad short names.
                let end = utf16.iter().position(|&c| c == 0).unwrap_or(utf16.len());
                String::from_utf16_lossy(&utf16[..end])
            } else {
                // A present entry with an unusable name still holds a slot; keep the position.
                self.dir.push(DirEntry {
                    name: String::new(),
                    kind,
                    start: 0,
                    size: 0,
                    left: FREESECT,
                    right: FREESECT,
                    child: FREESECT,
                });
                continue;
            };
            let size = u64::from_le_bytes(chunk[0x78..0x80].try_into().unwrap());
            self.dir.push(DirEntry {
                name,
                kind,
                start: u32::from_le_bytes(chunk[0x74..0x78].try_into().unwrap()),
                size,
                left: u32::from_le_bytes(chunk[0x44..0x48].try_into().unwrap()),
                right: u32::from_le_bytes(chunk[0x48..0x4C].try_into().unwrap()),
                child: u32::from_le_bytes(chunk[0x4C..0x50].try_into().unwrap()),
            });
        }
        // Version 3 files only use the low 32 bits of the size field.
        if self.sector_size == 512 {
            for entry in &mut self.dir {
                if entry.kind != TYPE_ROOT {
                    entry.size &= 0xFFFF_FFFF;
                }
            }
        }
        Ok(())
    }

    fn read_sector(&mut self, sector: u32) -> Result<Vec<u8>> {
        let offset = (u64::from(sector) + 1) * self.sector_size as u64;
        self.reader.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; self.sector_size];
        match self.reader.read_exact(&mut buf) {
            Ok(()) => Ok(buf),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(Error::Eof),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// Walks a chain in the main allocation table.
    fn read_fat_stream(&mut self, start: u32, size: u64) -> Result<Vec<u8>> {
        let fat = self.fat.clone();
        // Reserved from the declared size, capped, so a large workbook stream does not double its
        // way up while a header that overstates its size cannot demand the allocation.
        let mut out: Vec<u8> = Vec::with_capacity(size.min(MAX_RESERVE) as usize);
        let mut sector = start;
        let mut steps = 0usize;
        while sector <= MAXREGSECT && (out.len() as u64) < size {
            let bytes = self.read_sector(sector)?;
            let need = (size - out.len() as u64) as usize;
            out.extend_from_slice(&bytes[..need.min(bytes.len())]);
            sector = *fat
                .get(sector as usize)
                .ok_or_else(|| Error::format("stream chain points outside the FAT"))?;
            steps += 1;
            // Bounded by the file as well as the table: a chain cannot legitimately be longer
            // than the sectors the file holds, and a cycle would otherwise spin until the table
            // length is reached.
            if steps as u64 > self.total_sectors + 1 || steps > fat.len() + 1 {
                return Err(Error::format("cyclic stream chain"));
            }
        }
        out.truncate(size as usize);
        Ok(out)
    }

    /// Walks a chain in the mini allocation table, which indexes the mini stream.
    fn read_mini_stream(&self, start: u32, size: u64) -> Result<Vec<u8>> {
        let mut out: Vec<u8> = Vec::new();
        let mut sector = start;
        let mut steps = 0usize;
        while sector <= MAXREGSECT && (out.len() as u64) < size {
            let offset = sector as usize * self.mini_sector_size;
            if offset >= self.mini_stream.len() {
                break;
            }
            let end = (offset + self.mini_sector_size).min(self.mini_stream.len());
            let chunk = &self.mini_stream[offset..end];
            let need = (size - out.len() as u64) as usize;
            out.extend_from_slice(&chunk[..need.min(chunk.len())]);
            sector = *self
                .mini_fat
                .get(sector as usize)
                .ok_or_else(|| Error::format("mini stream chain points outside the mini FAT"))?;
            steps += 1;
            if steps > self.mini_fat.len() + 1 {
                return Err(Error::format("cyclic mini stream chain"));
            }
        }
        out.truncate(size as usize);
        Ok(out)
    }

    /// Reads the full contents of a directory entry.
    ///
    /// The size is capped by [`MAX_ENTRY_BYTES`], the same ceiling the ZIP path applies. Without
    /// it `.xls` was the one format that could be made to allocate without bound: the directory
    /// declares a stream's length, and a crafted file can declare a very large one.
    pub(crate) fn read_entry(&mut self, entry: &DirEntry) -> Result<Vec<u8>> {
        if entry.size == 0 || entry.start > MAXREGSECT {
            return Ok(Vec::new());
        }
        if entry.size > MAX_ENTRY_BYTES {
            return Err(Error::container(format!(
                "stream {} declares {} bytes, over the {MAX_ENTRY_BYTES} byte limit",
                entry.name, entry.size
            )));
        }
        let read = if entry.kind == TYPE_STREAM && entry.size < u64::from(self.mini_cutoff) {
            self.read_mini_stream(entry.start, entry.size)?
        } else {
            self.read_fat_stream(entry.start, entry.size)?
        };
        // A chain shorter than the declared size is a damaged file. Returning the short buffer
        // would surface much later as an unexplained end-of-stream from a record parser, so the
        // failure is reported where it happens.
        if (read.len() as u64) < entry.size {
            return Err(Error::format(format!(
                "stream {} is truncated: {} of {} bytes are present",
                entry.name,
                read.len(),
                entry.size
            )));
        }
        Ok(read)
    }

    /// Every stream name in the container, for diagnostics.
    pub(crate) fn stream_names(&self) -> Vec<String> {
        self.dir
            .iter()
            .filter(|e| e.kind == TYPE_STREAM)
            .map(|e| e.name.clone())
            .collect()
    }

    /// Finds a stream by name, searching the directory tree from the root entry.
    ///
    /// Names inside a compound file are case-insensitive, and a workbook may sit in either
    /// the top-level storage or a nested one, so the whole reachable tree is searched.
    pub(crate) fn find_stream(&self, name: &str) -> Option<DirEntry> {
        let target = normalize(name);
        let root = self.dir.first()?;
        let mut stack = vec![root.child];
        let mut visited = HashSet::new();
        while let Some(node) = stack.pop() {
            if node > MAXREGSECT || !visited.insert(node) {
                continue;
            }
            let entry = match self.dir.get(node as usize) {
                Some(e) => e,
                None => continue,
            };
            if entry.kind == TYPE_STREAM && normalize(&entry.name) == target {
                return Some(entry.clone());
            }
            stack.push(entry.left);
            stack.push(entry.right);
            if entry.kind == TYPE_STORAGE {
                stack.push(entry.child);
            }
        }
        None
    }
}

/// Compound file names compare case-insensitively and are stored uppercase by convention.
fn normalize(name: &str) -> String {
    name.trim_matches('\u{0}').to_ascii_uppercase()
}

impl<RS: Read + Seek> Archive for Cfb<RS> {
    fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
        match self.find_stream(name) {
            Some(entry) => self.read_entry(&entry).map(Some),
            None => Ok(None),
        }
    }

    fn contains(&self, name: &str) -> bool {
        self.index.contains(&(normalize(name), TYPE_STREAM)) || self.find_stream(name).is_some()
    }

    fn names(&self) -> Vec<String> {
        self.stream_names()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::BufReader;

    /// The container is validated against a genuine OLE file rather than a synthetic one, so
    /// the FAT, mini-FAT and directory-tree walking are exercised the way real writers lay
    /// them out. This fixture is a Word 97 document, which uses the same container as `.xls`.
    fn open_word_fixture() -> Cfb<BufReader<File>> {
        let path = format!("{}/tests/data/word97.doc", env!("CARGO_MANIFEST_DIR"));
        Cfb::new(BufReader::new(
            File::open(&path).unwrap_or_else(|e| panic!("missing fixture {path}: {e}")),
        ))
        .expect("compound file should parse")
    }

    #[test]
    fn parses_a_real_compound_file() {
        let cfb = open_word_fixture();
        let names = cfb.stream_names();
        assert!(
            names.iter().any(|n| n == "WordDocument"),
            "expected a WordDocument stream, found {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "1Table" || n == "0Table"),
            "expected a table stream, found {names:?}"
        );
    }

    #[test]
    fn reads_stream_contents() {
        let mut cfb = open_word_fixture();
        let entry = cfb
            .find_stream("WordDocument")
            .expect("WordDocument stream should be locatable");
        assert!(entry.size > 0, "stream should declare a size");
        let bytes = cfb.read_entry(&entry).expect("stream should read");
        assert_eq!(
            bytes.len() as u64,
            entry.size,
            "read {} bytes but the directory declares {}",
            bytes.len(),
            entry.size
        );
        // A Word 97 document stream opens with an FIB whose wIdent is 0xA5EC.
        assert_eq!(&bytes[..2], &[0xEC, 0xA5]);
    }

    #[test]
    fn lookups_are_case_insensitive() {
        let cfb = open_word_fixture();
        assert!(cfb.find_stream("worddocument").is_some());
        assert!(cfb.find_stream("WORDDOCUMENT").is_some());
        assert!(cfb.find_stream("nope").is_none());
    }

    #[test]
    fn rejects_a_non_compound_file() {
        let bytes = b"not a compound file at all, not even close............".to_vec();
        assert!(Cfb::new(std::io::Cursor::new(bytes)).is_err());
    }
}

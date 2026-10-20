//! Format detection and the convenience entry points.

use crate::container::{read_head, sniff, sniff_zip, Format};
use crate::datatype::Data;
use crate::error::{Error, Result};
use crate::formats::ods::Ods;
use crate::formats::xls::Xls;
use crate::formats::xlsx::Xlsx;
use crate::range::Range;
use crate::reader::{HeaderRow, Reader, SheetVisibility};
use crate::rows::SheetRows;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

macro_rules! delegate {
    ($this:expr, $method:ident ( $( $arg:expr ),* )) => {
        match $this {
            Sheets::Xlsx(workbook) => workbook.$method($( $arg ),*),
            Sheets::Xls(workbook) => workbook.$method($( $arg ),*),
            Sheets::Ods(workbook) => workbook.$method($( $arg ),*),
        }
    };
}

/// A workbook of whichever format the file turned out to be.
///
/// This is what [`open_workbook`] hands back when the format is not known ahead of time.
/// Each method forwards to the concrete reader.
///
/// Non-exhaustive because more formats are planned; adding one must not be a breaking change.
#[non_exhaustive]
pub enum Sheets<RS: Read + Seek> {
    /// An OOXML workbook.
    Xlsx(Xlsx<RS>),
    /// A legacy BIFF8 workbook.
    Xls(Xls<RS>),
    /// An OpenDocument spreadsheet.
    Ods(Ods<RS>),
}

impl<RS: Read + Seek> Sheets<RS> {
    /// Which format the file actually was.
    pub fn format(&self) -> Format {
        match self {
            Sheets::Xlsx(_) => Format::Xlsx,
            Sheets::Xls(_) => Format::Xls,
            Sheets::Ods(_) => Format::Ods,
        }
    }

    /// Borrows the OOXML reader, if this is one.
    pub fn as_xlsx(&self) -> Option<&Xlsx<RS>> {
        match self {
            Sheets::Xlsx(w) => Some(w),
            _ => None,
        }
    }

    /// Borrows the legacy reader, if this is one.
    pub fn as_xls(&self) -> Option<&Xls<RS>> {
        match self {
            Sheets::Xls(w) => Some(w),
            _ => None,
        }
    }

    /// Borrows the OpenDocument reader, if this is one.
    pub fn as_ods(&self) -> Option<&Ods<RS>> {
        match self {
            Sheets::Ods(w) => Some(w),
            _ => None,
        }
    }
}

impl<RS: Read + Seek> Reader<RS> for Sheets<RS> {
    fn new(reader: RS) -> Result<Self> {
        open_workbook_auto_from_rs(reader)
    }

    fn with_header_row(&mut self, header_row: HeaderRow) -> &mut Self {
        match self {
            Sheets::Xlsx(w) => {
                w.with_header_row(header_row);
            }
            Sheets::Xls(w) => {
                w.with_header_row(header_row);
            }
            Sheets::Ods(w) => {
                w.with_header_row(header_row);
            }
        }
        self
    }

    fn header_row(&self) -> HeaderRow {
        delegate!(self, header_row())
    }

    fn sheet_names(&self) -> &[String] {
        delegate!(self, sheet_names())
    }

    fn sheet_visibility(&self, index: usize) -> SheetVisibility {
        delegate!(self, sheet_visibility(index))
    }

    fn defined_names(&self) -> &[(String, String)] {
        delegate!(self, defined_names())
    }

    fn worksheet_range(&mut self, name: &str) -> Result<Range<Data>> {
        delegate!(self, worksheet_range(name))
    }

    fn worksheet_rows(&mut self, name: &str) -> Result<SheetRows<'_>> {
        delegate!(self, worksheet_rows(name))
    }

    /// Forwarded rather than left to the default, so the format's own skipping is used. The default
    /// would read the whole sheet and throw most of it away.
    fn worksheet_range_ref(&mut self, name: &str, reference: &str) -> Result<Range<Data>> {
        delegate!(self, worksheet_range_ref(name, reference))
    }

    fn worksheet_formula(&mut self, name: &str) -> Result<Range<String>> {
        delegate!(self, worksheet_formula(name))
    }

    /// Forwarded rather than left to the default, so each format's own merge metadata is used.
    fn worksheet_merged_regions(&mut self, name: &str) -> Result<Vec<crate::reader::MergedRegion>> {
        delegate!(self, worksheet_merged_regions(name))
    }
}

/// Opens a workbook, working out the format from the file's contents.
///
/// Detection is by signature rather than by file extension, so a workbook that has been
/// renamed is still read correctly.
pub fn open_workbook_auto_from_rs<RS: Read + Seek>(mut reader: RS) -> Result<Sheets<RS>> {
    let mut head = [0u8; 8];
    let filled = read_head(&mut reader, &mut head)?;
    reader.seek(SeekFrom::Start(0))?;

    match sniff(&head[..filled]) {
        Some(Format::Xls) => Ok(Sheets::Xls(Xls::new(reader)?)),
        // A ZIP prefix only says "some OOXML or OpenDocument package"; the archive directory
        // has to be read to tell those apart.
        Some(_) => {
            let (format, mut reader) = sniff_zip(reader)?;
            reader.seek(SeekFrom::Start(0))?;
            match format {
                Format::Ods => Ok(Sheets::Ods(Ods::new(reader)?)),
                _ => Ok(Sheets::Xlsx(Xlsx::new(reader)?)),
            }
        }
        None => Err(Error::format(
            "unrecognised spreadsheet format: not OOXML, OpenDocument or BIFF",
        )),
    }
}

/// Opens a workbook from a path, working out the format from its contents.
pub fn open_workbook_auto<P: AsRef<Path>>(path: P) -> Result<Sheets<BufReader<File>>> {
    let file = File::open(path)?;
    open_workbook_auto_from_rs(BufReader::new(file))
}

/// Alias for [`open_workbook_auto`].
pub fn open_workbook<P: AsRef<Path>>(path: P) -> Result<Sheets<BufReader<File>>> {
    open_workbook_auto(path)
}

/// Opens an OOXML workbook from a path.
pub fn open_xlsx<P: AsRef<Path>>(path: P) -> Result<Xlsx<BufReader<File>>> {
    Xlsx::new(BufReader::new(File::open(path)?))
}

/// Opens an OOXML workbook from any seekable source.
pub fn open_xlsx_from_rs<RS: Read + Seek>(reader: RS) -> Result<Xlsx<RS>> {
    Xlsx::new(reader)
}

/// Opens a legacy BIFF8 workbook from a path.
pub fn open_xls<P: AsRef<Path>>(path: P) -> Result<Xls<BufReader<File>>> {
    Xls::new(BufReader::new(File::open(path)?))
}

/// Opens a legacy BIFF8 workbook from any seekable source.
pub fn open_xls_from_rs<RS: Read + Seek>(reader: RS) -> Result<Xls<RS>> {
    Xls::new(reader)
}

/// Opens an OpenDocument spreadsheet from a path.
pub fn open_ods<P: AsRef<Path>>(path: P) -> Result<Ods<BufReader<File>>> {
    Ods::new(BufReader::new(File::open(path)?))
}

/// Opens an OpenDocument spreadsheet from any seekable source.
pub fn open_ods_from_rs<RS: Read + Seek>(reader: RS) -> Result<Ods<RS>> {
    Ods::new(reader)
}

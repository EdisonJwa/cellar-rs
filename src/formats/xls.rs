//! Legacy BIFF8 workbook (`.xls`, Excel 97-2003) reading.
//!
//! A `.xls` file is a BIFF record stream inside an OLE compound file, so this module sits on
//! top of [`crate::cfb`]. The stream opens with a "globals" substream that carries the sheet
//! directory, the shared string table and the format table, followed by one substream per
//! sheet. Sheets are addressed by absolute byte offset, which is why the directory is read
//! before any cell data.
//!
//! Two details drive most of the complexity here. Records are capped at 8224 bytes, so long
//! strings are split across `CONTINUE` records and the character encoding can change at each
//! break. And a formula's cached result is an eight-byte union whose most significant two bytes
//! decide whether it holds a number, a boolean, an error, or nothing at all with the text in a
//! following record.

use crate::cfb::{Cfb, DirEntry};
use crate::codepage::{byte_map, push_char, ByteMap};
use crate::container::{read_head, CFB_MAGIC, MAX_ENTRY_BYTES};
use crate::datatype::{CellError, Data, ExcelDateTime};
use crate::error::{Error, Result};
use crate::range::{Extent, Range, SheetBuilder};
use crate::reader::{apply_header_row, HeaderRow, MergedRegion, Reader, SheetVisibility};
use crate::rows::{skip_header_row, RowSource, SheetRows};
use crate::utils::{format_is_date, Cursor};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};

/// Record opcodes this reader acts on.
mod op {
    pub const EOF: u16 = 0x000A;
    pub const BOF: u16 = 0x0809;
    pub const CONTINUE: u16 = 0x003C;
    pub const BOUNDSHEET: u16 = 0x0085;
    pub const SST: u16 = 0x00FC;
    pub const XF: u16 = 0x00E0;
    pub const FORMAT: u16 = 0x041E;
    pub const DATEMODE: u16 = 0x0022;
    pub const CODEPAGE: u16 = 0x0042;
    pub const FILEPASS: u16 = 0x002F;
    pub const LABEL: u16 = 0x0204;
    pub const LABELSST: u16 = 0x00FD;
    pub const NUMBER: u16 = 0x0203;
    pub const RK: u16 = 0x027E;
    pub const MULRK: u16 = 0x00BD;
    pub const BOOLERR: u16 = 0x0205;
    pub const MULBLANK: u16 = 0x00BE;
    pub const MERGEDCELLS: u16 = 0x00E5;
    pub const BLANK: u16 = 0x0201;
    pub const DIMENSIONS: u16 = 0x0200;
    pub const ROW: u16 = 0x0208;
    pub const FORMULA: u16 = 0x0006;
    pub const STRING: u16 = 0x0207;
}

/// BIFF8 is version `0x0600`; anything lower is the pre-Unicode Excel 5.0/95 format.
const BIFF8_VERSION: u16 = 0x0600;

/// The code page a BIFF8 file means when it declares none.
const DEFAULT_CODEPAGE: u16 = 1252;

/// Where the record stream comes from.
enum Source<RS> {
    /// An OLE container plus the directory entry of the workbook stream.
    Compound(Box<Cfb<RS>>, Box<DirEntry>),
    /// A bare BIFF stream with no container, which a few tools emit.
    Raw(Vec<u8>),
}

/// Streams that mark an OLE container as holding an encrypted Office package.
const ENCRYPTION_INFO: &str = "EncryptionInfo";
const ENCRYPTED_PACKAGE: &str = "EncryptedPackage";

/// A reader for legacy BIFF8 workbooks.
///
/// Unlike the OOXML and OpenDocument readers this one parses the whole record stream up front,
/// because sheet substreams are addressed by offset in a stream that has to be read as a whole
/// anyway.
pub struct Xls<RS: Read + Seek> {
    source: Option<Source<RS>>,
    stream: Option<Vec<u8>>,
    names: Vec<String>,
    offsets: Vec<u32>,
    /// Visibility per sheet, parallel to `names`.
    visibilities: Vec<SheetVisibility>,
    xf_date: Vec<bool>,
    strings: Vec<String>,
    is_1904: bool,
    /// Decoding table for compressed strings, built from the workbook's code page. Boxed because
    /// it is a kilobyte, and inlining it would make every one of these readers that much larger.
    byte_map: Box<ByteMap>,
    header: HeaderRow,
}

/// A cell decoded from the record stream, wherever it belongs on the sheet.
struct Cell {
    row: u32,
    column: u32,
    value: Data,
}

/// What the record at a position in a sheet substream turned out to hold.
enum Step {
    /// One record's cells, and the position after the record. A run record such as `MULRK` fills a
    /// whole span in one go, so this is a list rather than a single cell.
    Cells(usize),
    /// A row the substream declares here, and the position after the record. Declaring a row is not
    /// the same as the row holding nothing — its cells may follow.
    Row(u32, usize),
    /// The extent the sheet declares, and the position after the record.
    Extent(Extent, usize),
    /// A record holding none of the above, and the position after it.
    Other(usize),
    /// The end of the substream.
    End,
}

/// Adds a decoded cell to a result, unless it holds nothing.
///
/// Leaving the empty ones out is what lets the two reads report the same populated cells.
fn push_cell(out: &mut Vec<Cell>, row: u32, column: u32, value: Data) {
    if !value.is_empty() {
        out.push(Cell { row, column, value });
    }
}

impl<RS: Read + Seek> Xls<RS> {
    /// Materialises the workbook record stream.
    fn ensure_stream(&mut self) -> Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        let stream = match self.source.as_mut() {
            Some(Source::Compound(cfb, entry)) => cfb.read_entry(entry)?,
            Some(Source::Raw(buf)) => std::mem::take(buf),
            None => return Err(Error::format("workbook stream is unavailable")),
        };
        self.stream = Some(stream);
        Ok(())
    }

    /// Reads the globals substream: sheets, strings, formats and the date system.
    ///
    /// Nothing that holds a compressed string is decoded here. The code page can appear anywhere
    /// among these records, so every string is kept as bytes and decoded at the end. Assuming an
    /// order would work for the files Excel writes and silently mistranslate the ones that differ.
    fn parse_globals(&mut self) -> Result<()> {
        let data = self.stream.as_deref().ok_or(Error::Eof)?;
        let mut pos = 0usize;
        let mut xf_ifmt: Vec<u16> = Vec::new();
        let mut raw_names: Vec<RawString> = Vec::new();
        let mut raw_formats: Vec<(u16, RawString)> = Vec::new();
        let mut sst: Option<(Vec<u8>, Vec<usize>)> = None;
        let mut codepage: u16 = DEFAULT_CODEPAGE;
        // A BIFF stream opens with a BOF record. Requiring it is what separates "a workbook with
        // no sheets" from "bytes that were never a workbook": without the check any input
        // produced an empty, successful workbook.
        let mut saw_bof = false;

        while let Some((opcode, body, next)) = read_record(data, pos) {
            match opcode {
                op::EOF => break,
                op::BOF => {
                    check_biff_version(body)?;
                    saw_bof = true;
                }
                op::FILEPASS => return Err(Error::Encrypted),
                op::BOUNDSHEET => {
                    let (offset, name, visibility) = parse_boundsheet(body)?;
                    self.offsets.push(offset);
                    self.visibilities.push(visibility);
                    raw_names.push(name);
                }
                op::DATEMODE => {
                    self.is_1904 = body.len() >= 2 && u16::from_le_bytes([body[0], body[1]]) != 0;
                }
                op::CODEPAGE => {
                    if let Some(declared) = read_codepage(body) {
                        codepage = declared;
                    }
                }
                op::XF => {
                    // `ifmt` is the third field of an XF record.
                    xf_ifmt.push(if body.len() >= 4 {
                        u16::from_le_bytes([body[2], body[3]])
                    } else {
                        0
                    });
                }
                op::FORMAT => {
                    if let Some(entry) = parse_format(body) {
                        raw_formats.push(entry);
                    }
                }
                op::SST => {
                    let (buffer, boundaries, after) = gather_continues(data, next, body.to_vec());
                    sst = Some((buffer, boundaries));
                    pos = after;
                    continue;
                }
                op::CONTINUE => {}
                _ => {}
            }
            pos = next;
        }

        if !saw_bof {
            return Err(Error::format(
                "not a BIFF workbook: the stream does not open with a BOF record",
            ));
        }

        let map = byte_map(codepage);
        self.names = raw_names.iter().map(|name| name.decode(&map)).collect();
        let formats: HashMap<u16, String> = raw_formats
            .iter()
            .map(|(id, code)| (*id, code.decode(&map)))
            .collect();
        if let Some((buffer, boundaries)) = sst {
            self.strings = parse_sst(&buffer, &boundaries, &map)?;
        }
        // Written through the existing box rather than replacing it, so the table built at
        // construction is reused instead of being allocated twice.
        *self.byte_map = map;

        self.xf_date = xf_ifmt
            .iter()
            .map(|id| format_is_date(*id, formats.get(id).map(String::as_str)))
            .collect();
        Ok(())
    }

    /// Decodes the record at `pos`, adding the cells it holds to `out`.
    ///
    /// Both reads go through here, so a record cannot be decoded one way when the sheet is built and
    /// another way when it is streamed. That sharing is what makes the two agree about what a cell
    /// holds without either of them having to be trusted to.
    fn decode(&self, data: &[u8], pos: usize, out: &mut Vec<Cell>) -> Result<Step> {
        out.clear();
        let Some((opcode, body, next)) = read_record(data, pos) else {
            return Ok(Step::End);
        };
        let after = match opcode {
            op::EOF => return Ok(Step::End),
            op::DIMENSIONS => {
                return Ok(match parse_dimensions(body) {
                    Some(extent) => Step::Extent(extent, next),
                    None => Step::Other(next),
                });
            }
            op::ROW => {
                // The row's number is the record's first field.
                if body.len() < 2 {
                    return Ok(Step::Other(next));
                }
                let row = u32::from(u16::from_le_bytes([body[0], body[1]]));
                return Ok(Step::Row(row, next));
            }
            op::LABELSST => {
                let mut c = Cursor::new(body);
                let row = u32::from(c.read_u16()?);
                let column = u32::from(c.read_u16()?);
                let _ixfe = c.read_u16()?;
                let index = c.read_u32()? as usize;
                // An index the table does not hold leaves the cell as empty as any other.
                let value = match self.strings.get(index) {
                    Some(text) => Data::String(text.clone()),
                    None => Data::Empty,
                };
                push_cell(out, row, column, value);
                next
            }
            op::LABEL => {
                let mut c = Cursor::new(body);
                let row = u32::from(c.read_u16()?);
                let column = u32::from(c.read_u16()?);
                let _ixfe = c.read_u16()?;
                let (buffer, boundaries, after) = gather_continues(data, next, body.to_vec());
                let mut reader = UnitReader::new(&buffer, &boundaries, &self.byte_map);
                reader.skip(6)?;
                // A string that will not decode leaves the cell empty rather than failing the read.
                let value = match reader.read_string() {
                    Ok(text) => Data::String(text),
                    Err(_) => Data::Empty,
                };
                push_cell(out, row, column, value);
                after
            }
            op::NUMBER => {
                let mut c = Cursor::new(body);
                let row = u32::from(c.read_u16()?);
                let column = u32::from(c.read_u16()?);
                let ixfe = c.read_u16()? as usize;
                let value = c.read_f64()?;
                push_cell(out, row, column, self.number_data(value, ixfe));
                next
            }
            op::RK => {
                let mut c = Cursor::new(body);
                let row = u32::from(c.read_u16()?);
                let column = u32::from(c.read_u16()?);
                let ixfe = c.read_u16()? as usize;
                let value = decode_rk(c.read_u32()?);
                push_cell(out, row, column, self.number_data(value, ixfe));
                next
            }
            op::MULRK => {
                let mut c = Cursor::new(body);
                let row = u32::from(c.read_u16()?);
                let first = u32::from(c.read_u16()?);
                // The body ends with the last column index, so it is not part of the run.
                let mut index = 0u32;
                while c.remaining() > 2 {
                    let ixfe = match c.read_u16() {
                        Ok(v) => v as usize,
                        Err(_) => break,
                    };
                    let rk = match c.read_u32() {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    push_cell(
                        out,
                        row,
                        first + index,
                        self.number_data(decode_rk(rk), ixfe),
                    );
                    index += 1;
                }
                next
            }
            op::BOOLERR => {
                let mut c = Cursor::new(body);
                let row = u32::from(c.read_u16()?);
                let column = u32::from(c.read_u16()?);
                let _ixfe = c.read_u16()?;
                let value = c.read_u8()?;
                let is_error = c.read_u8()? != 0;
                let data = if is_error {
                    Data::Error(CellError::from_biff(value))
                } else {
                    Data::Bool(value != 0)
                };
                push_cell(out, row, column, data);
                next
            }
            op::FORMULA => {
                let mut c = Cursor::new(body);
                let row = u32::from(c.read_u16()?);
                let column = u32::from(c.read_u16()?);
                let ixfe = c.read_u16()? as usize;
                let result = c.read_bytes(8)?;
                let (value, wants_string) = self.formula_result(result, ixfe);
                let mut after = next;
                if wants_string {
                    // The text is in the record that follows, when it is there at all.
                    if let Some((op::STRING, sbody, snext)) = read_record(data, next) {
                        let (buffer, boundaries, end) =
                            gather_continues(data, snext, sbody.to_vec());
                        let mut reader = UnitReader::new(&buffer, &boundaries, &self.byte_map);
                        let text = match reader.read_string() {
                            Ok(text) => Data::String(text),
                            Err(_) => Data::Empty,
                        };
                        push_cell(out, row, column, text);
                        after = end;
                    } else {
                        push_cell(out, row, column, value);
                    }
                } else {
                    push_cell(out, row, column, value);
                }
                after
            }
            // Blank cells, continuations and anything unrecognised hold nothing.
            op::BLANK | op::MULBLANK | op::CONTINUE => return Ok(Step::Other(next)),
            _ => return Ok(Step::Other(next)),
        };
        Ok(Step::Cells(after))
    }

    /// Parses a sheet substream into a dense range.
    ///
    /// Cells are written straight into their final positions rather than collected into a sparse
    /// list first, which halves peak memory on a large sheet. The substream opens with a
    /// `DIMENSIONS` record stating the sheet's extent, which sizes that buffer in advance.
    fn parse_sheet(&self, offset: u32) -> Result<SheetBuilder<Data>> {
        let data = self.stream.as_deref().ok_or(Error::Eof)?;
        let start = offset as usize;
        if start >= data.len() {
            return Err(Error::format(format!(
                "sheet substream offset {start} is past the end of the workbook"
            )));
        }
        let mut builder = SheetBuilder::new(None);
        let mut cells = Vec::new();
        let mut pos = start;
        // `DIMENSIONS` is only a sizing hint, and BIFF permits it anywhere in the substream.
        // Adopting a later one as a *fresh* builder would discard every cell already decoded,
        // so a hint is only taken while the builder is still empty.
        let mut wrote_cell = false;

        loop {
            match self.decode(data, pos, &mut cells)? {
                Step::End => break,
                Step::Cells(after) => {
                    for cell in cells.drain(..) {
                        builder.write(cell.row, cell.column, cell.value)?;
                    }
                    wrote_cell = true;
                    pos = after;
                }
                Step::Extent(extent, after) => {
                    // Rebuilding is only safe while the builder holds nothing: it has no way to
                    // re-home existing cells, so once a cell has been written the extent stays
                    // a hint the buffer simply grows past.
                    if !wrote_cell {
                        builder = SheetBuilder::new(Some(extent));
                    }
                    pos = after;
                }
                Step::Row(_, after) | Step::Other(after) => pos = after,
            }
        }
        Ok(builder)
    }

    /// Applies a cell format to a raw number, promoting it to a date when the format says so.
    fn number_data(&self, value: f64, ixfe: usize) -> Data {
        if self.xf_date.get(ixfe).copied().unwrap_or(false) {
            return Data::DateTime(ExcelDateTime::new(value, self.is_1904));
        }
        if value.is_finite() && value == value.trunc() && value.abs() <= 9_007_199_254_740_992.0 {
            Data::Int(value as i64)
        } else {
            Data::Float(value)
        }
    }

    /// Decodes a formula's cached result, reporting whether its text follows separately.
    fn formula_result(&self, result: &[u8], ixfe: usize) -> (Data, bool) {
        let mut tail = [0u8; 8];
        let len = result.len().min(8);
        tail[..len].copy_from_slice(&result[..len]);
        if tail[6] == 0xFF && tail[7] == 0xFF {
            match tail[0] {
                // The value is a string, held in the record that follows.
                0 => (Data::Empty, true),
                1 => (Data::Bool(tail[2] != 0), false),
                2 => (Data::Error(CellError::from_biff(tail[2])), false),
                // Blank, meaning the cell holds a formula but no cached value.
                _ => (Data::Empty, false),
            }
        } else {
            let value = f64::from_le_bytes(tail);
            (self.number_data(value, ixfe), false)
        }
    }
}

/// Streams one BIFF8 sheet's rows.
///
/// The record stream is in memory whatever the read does, because a BIFF workbook addresses its
/// sheets by offset. What streaming avoids is the sheet's rectangle: the format allows 65536 by 256
/// cells, which is 537 MB of values for a sheet that may hold two of them.
struct XlsRows<'a, RS: Read + Seek> {
    xls: &'a Xls<RS>,
    data: &'a [u8],
    /// The next record to read.
    pos: usize,
    /// A row the substream has declared and not yet shown a cell for. It is held rather than
    /// returned, because the cells it declares may follow and they belong to the same row.
    declared: Option<u32>,
    /// Cells decoded from the record in hand.
    cells: Vec<Cell>,
    done: bool,
}

impl<RS: Read + Seek> RowSource for XlsRows<'_, RS> {
    fn next_row(&mut self, columns: &mut Vec<u32>, values: &mut Vec<Data>) -> Result<Option<u32>> {
        columns.clear();
        values.clear();
        if self.done {
            return Ok(None);
        }
        // The row being filled. Everything before a row's first cell has been walked already.
        let mut open: Option<u32> = None;
        loop {
            let at = self.pos;
            match self.xls.decode(self.data, at, &mut self.cells)? {
                Step::End => {
                    self.done = true;
                    return Ok(open.or_else(|| self.declared.take()));
                }
                Step::Other(after) | Step::Extent(_, after) => self.pos = after,
                Step::Row(row, after) => {
                    if open == Some(row) || self.declared == Some(row) {
                        // Already in hand, from a cell or from this same declaration.
                        self.pos = after;
                    } else if open.is_none() && self.declared.is_none() {
                        self.declared = Some(row);
                        self.pos = after;
                    } else {
                        // A row is in hand, so this declaration is for a later one.
                        self.pos = at;
                        match open.or_else(|| self.declared.take()) {
                            Some(held) => return Ok(Some(held)),
                            None => continue,
                        }
                    }
                }
                Step::Cells(after) => {
                    // One record's cells share a row, so the row is settled once.
                    let Some(row) = self.cells.first().map(|cell| cell.row) else {
                        self.pos = after;
                        continue;
                    };
                    if self.declared == Some(row) {
                        // The row holds a cell after all, so it was not empty.
                        self.declared = None;
                    }
                    if open.is_none() && self.declared.is_none() {
                        open = Some(row);
                    }
                    if open == Some(row) {
                        for cell in self.cells.drain(..) {
                            match columns.last() {
                                // One cell written twice: the later record wins, as it does in the
                                // dense read.
                                Some(&last) if last == cell.column => {
                                    if let Some(slot) = values.last_mut() {
                                        *slot = cell.value;
                                    }
                                }
                                _ => {
                                    columns.push(cell.column);
                                    values.push(cell.value);
                                }
                            }
                        }
                        self.pos = after;
                    } else {
                        self.pos = at;
                        match open.or_else(|| self.declared.take()) {
                            Some(held) => return Ok(Some(held)),
                            None => continue,
                        }
                    }
                }
            }
        }
    }
}

impl<RS: Read + Seek> Reader<RS> for Xls<RS> {
    fn new(reader: RS) -> Result<Self> {
        let mut reader = reader;
        let mut head = [0u8; 8];
        let filled = read_head(&mut reader, &mut head)?;
        reader.seek(SeekFrom::Start(0))?;

        let source = if filled == head.len() && head == CFB_MAGIC {
            let cfb = Cfb::new(reader)?;
            // An encrypted Office file is not a ZIP at all: the whole package is wrapped in an
            // OLE container holding these two streams, so it arrives down this path. Detecting
            // them turns a confusing "no Workbook stream" into a usable diagnosis.
            if cfb.find_stream(ENCRYPTION_INFO).is_some()
                || cfb.find_stream(ENCRYPTED_PACKAGE).is_some()
            {
                return Err(Error::Encrypted);
            }
            let entry = cfb
                .find_stream("Workbook")
                .or_else(|| cfb.find_stream("Book"))
                .ok_or_else(|| Error::missing("Workbook stream inside the OLE container"))?;
            Source::Compound(Box::new(cfb), Box::new(entry))
        } else {
            // A bare BIFF stream with no container is slurped whole, so the same ceiling the
            // container paths apply has to be applied here too — otherwise this is the one way
            // to make the reader allocate without bound.
            let mut buffer = Vec::new();
            let read = std::io::copy(&mut reader.take(MAX_ENTRY_BYTES + 1), &mut buffer)?;
            if read > MAX_ENTRY_BYTES {
                return Err(Error::container(format!(
                    "bare record stream exceeds the {MAX_ENTRY_BYTES} byte limit"
                )));
            }
            Source::Raw(buffer)
        };

        let mut workbook = Xls {
            source: Some(source),
            stream: None,
            names: Vec::new(),
            offsets: Vec::new(),
            visibilities: Vec::new(),
            xf_date: Vec::new(),
            strings: Vec::new(),
            is_1904: false,
            // Replaced by the real one once the globals substream reports its code page.
            byte_map: Box::new(byte_map(DEFAULT_CODEPAGE)),
            header: HeaderRow::First,
        };
        workbook.ensure_stream()?;
        workbook.parse_globals()?;
        Ok(workbook)
    }

    fn with_header_row(&mut self, header_row: HeaderRow) -> &mut Self {
        self.header = header_row;
        self
    }

    fn header_row(&self) -> HeaderRow {
        self.header
    }

    fn sheet_names(&self) -> &[String] {
        &self.names
    }

    fn sheet_visibility(&self, index: usize) -> SheetVisibility {
        self.visibilities.get(index).copied().unwrap_or_default()
    }

    fn defined_names(&self) -> &[(String, String)] {
        &[]
    }

    fn worksheet_range(&mut self, name: &str) -> Result<Range<Data>> {
        let index = self
            .sheet_index(name)
            .ok_or_else(|| Error::NotFound(format!("sheet {name:?}")))?;
        let offset = self.offsets[index];
        let builder = self.parse_sheet(offset)?;
        Ok(apply_header_row(builder.finish(), self.header))
    }

    fn worksheet_rows(&mut self, name: &str) -> Result<SheetRows<'_>> {
        let index = self
            .sheet_index(name)
            .ok_or_else(|| Error::NotFound(format!("sheet {name:?}")))?;
        let start = self.offsets[index] as usize;
        let data = self.stream.as_deref().ok_or(Error::Eof)?;
        if start >= data.len() {
            return Err(Error::format(format!(
                "sheet substream offset {start} is past the end of the workbook"
            )));
        }
        let header = self.header;
        let mut rows = SheetRows::new(XlsRows {
            xls: &*self,
            data,
            pos: start,
            declared: None,
            cells: Vec::new(),
            done: false,
        });
        // The dense path trims its range to the header row, so streaming starts in the same place.
        skip_header_row(&mut rows, header)?;
        Ok(rows)
    }

    fn worksheet_formula(&mut self, _name: &str) -> Result<Range<String>> {
        // Formulas are stored as compiled token streams, not text. Recovering the text means
        // decompiling those tokens, which this reader does not do; the cached *values* are
        // still available through `worksheet_range`.
        Err(Error::unsupported(
            "formula text is not recovered from BIFF8 workbooks",
        ))
    }

    /// Reads `MERGEDCELLS` records from the sheet substream.
    ///
    /// The record lists runs of merged cells: a first row and count of covered rows, then a
    /// count of ranges, each a first column and last column within that row span.
    fn worksheet_merged_regions(&mut self, name: &str) -> Result<Vec<MergedRegion>> {
        let index = self
            .sheet_index(name)
            .ok_or_else(|| Error::NotFound(format!("sheet {name:?}")))?;
        let start = self.offsets[index] as usize;
        let data = self.stream.as_deref().ok_or(Error::Eof)?;
        if start >= data.len() {
            return Err(Error::format(format!(
                "sheet substream offset {start} is past the end of the workbook"
            )));
        }
        let mut out = Vec::new();
        let mut pos = start;
        while let Some((opcode, body, next)) = read_record(data, pos) {
            match opcode {
                op::EOF => break,
                op::MERGEDCELLS => parse_merged_cells(body, &mut out),
                _ => {}
            }
            pos = next;
        }
        Ok(out)
    }
}

/// Expands a `MERGEDCELLS` record into regions.
///
/// The record is a count followed by that many `(first row, last row, first column, last
/// column)` quadruples, all inclusive. Every field is read through [`Cursor`], so a truncated
/// record contributes the ranges it does hold rather than panicking or aborting the sheet.
fn parse_merged_cells(body: &[u8], out: &mut Vec<MergedRegion>) {
    let mut c = Cursor::new(body);
    let Ok(count) = c.read_u16() else {
        return;
    };
    out.reserve(usize::from(count));
    for _ in 0..count {
        let (Ok(first_row), Ok(last_row), Ok(first_col), Ok(last_col)) =
            (c.read_u16(), c.read_u16(), c.read_u16(), c.read_u16())
        else {
            return;
        };
        out.push(MergedRegion::new(
            (u32::from(first_row), u32::from(first_col)),
            (u32::from(last_row), u32::from(last_col)),
        ));
    }
}

/// Reads a `DIMENSIONS` record, which states the extent the sheet's cells occupy.
///
/// Only a hint, like OOXML's `dimension`: files exist with a stale or empty record, and the
/// builder grows or trims to the real extent either way.
fn parse_dimensions(body: &[u8]) -> Option<Extent> {
    let mut c = Cursor::new(body);
    let first_row = c.read_u32().ok()?;
    // Both bounds are exclusive at the far end.
    let last_row = c.read_u32().ok()?;
    let first_col = u32::from(c.read_u16().ok()?);
    let last_col = u32::from(c.read_u16().ok()?);
    Some(Extent {
        origin: (first_row, first_col),
        rows: last_row.checked_sub(first_row)?,
        cols: last_col.checked_sub(first_col)?,
    })
}

/// Reads a `CODEPAGE` record, if it is long enough to carry one.
fn read_codepage(body: &[u8]) -> Option<u16> {
    if body.len() >= 2 {
        Some(u16::from_le_bytes([body[0], body[1]]))
    } else {
        None
    }
}

/// Rejects record streams from the pre-Unicode Excel 5.0/95 formats.
fn check_biff_version(body: &[u8]) -> Result<()> {
    if body.len() >= 2 {
        let version = u16::from_le_bytes([body[0], body[1]]);
        if version < BIFF8_VERSION {
            return Err(Error::unsupported(format!(
                "BIFF{version:#06x} workbook; only BIFF8 (Excel 97-2003) is supported"
            )));
        }
    }
    Ok(())
}

/// Reads one BIFF record, returning its opcode, body and the offset that follows it.
fn read_record(data: &[u8], pos: usize) -> Option<(u16, &[u8], usize)> {
    if pos + 4 > data.len() {
        return None;
    }
    let opcode = u16::from_le_bytes([data[pos], data[pos + 1]]);
    let len = usize::from(u16::from_le_bytes([data[pos + 2], data[pos + 3]]));
    let start = pos + 4;
    let end = start.checked_add(len)?;
    if end > data.len() {
        return None;
    }
    Some((opcode, &data[start..end], end))
}

/// Collects a record's body together with every `CONTINUE` body that follows it.
///
/// The returned offsets mark where each continuation begins, which is what lets the string
/// reader notice that the encoding may have changed at that point.
fn gather_continues(
    data: &[u8],
    mut pos: usize,
    mut buffer: Vec<u8>,
) -> (Vec<u8>, Vec<usize>, usize) {
    let mut boundaries = Vec::new();
    while let Some((op::CONTINUE, body, next)) = read_record(data, pos) {
        boundaries.push(buffer.len());
        buffer.extend_from_slice(body);
        pos = next;
    }
    (buffer, boundaries, pos)
}

/// Reads BIFF8 strings whose length is split across continuation records.
struct UnitReader<'a> {
    data: &'a [u8],
    pos: usize,
    boundaries: &'a [usize],
    /// How a compressed string's bytes map to characters, from the workbook's code page.
    map: &'a ByteMap,
}

impl<'a> UnitReader<'a> {
    fn new(data: &'a [u8], boundaries: &'a [usize], map: &'a ByteMap) -> Self {
        UnitReader {
            data,
            pos: 0,
            boundaries,
            map,
        }
    }

    fn skip(&mut self, n: usize) -> Result<()> {
        if self.pos + n > self.data.len() {
            return Err(Error::Eof);
        }
        self.pos += n;
        Ok(())
    }

    fn read_u8(&mut self) -> Result<u8> {
        let b = *self.data.get(self.pos).ok_or(Error::Eof)?;
        self.pos += 1;
        Ok(b)
    }

    fn read_u16(&mut self) -> Result<u16> {
        if self.pos + 2 > self.data.len() {
            return Err(Error::Eof);
        }
        let v = u16::from_le_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }

    fn read_u32(&mut self) -> Result<u32> {
        if self.pos + 4 > self.data.len() {
            return Err(Error::Eof);
        }
        let v = u32::from_le_bytes([
            self.data[self.pos],
            self.data[self.pos + 1],
            self.data[self.pos + 2],
            self.data[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(v)
    }

    fn at_boundary(&self) -> bool {
        self.boundaries.binary_search(&self.pos).is_ok()
    }

    /// The next continuation offset strictly after the cursor, or the end of the data.
    fn next_boundary(&self) -> usize {
        let from = match self.boundaries.binary_search(&self.pos) {
            Ok(i) => i + 1,
            Err(i) => i,
        };
        self.boundaries
            .get(from)
            .copied()
            .unwrap_or(self.data.len())
    }

    /// Reads `count` characters, honouring an encoding change at every continuation break.
    ///
    /// Units are accumulated across the whole run and decoded once, so a surrogate pair that
    /// straddles a record boundary still decodes to a single character.
    ///
    /// The compressed form is one byte per character. That is exact for the single-byte code
    /// pages and for ASCII, which is what these files overwhelmingly contain; a double-byte
    /// code page's compressed text is not decoded.
    fn read_chars(&mut self, count: u32, mut high: bool, out: &mut String) -> Result<()> {
        let mut remaining = count as usize;
        let mut units: Vec<u16> = Vec::with_capacity(remaining.min(4096));
        while remaining > 0 {
            if self.at_boundary() {
                let flags = *self.data.get(self.pos).ok_or(Error::Eof)?;
                self.pos += 1;
                high = flags & 0x01 != 0;
                continue;
            }
            let limit = self.next_boundary().min(self.data.len());
            let width = if high { 2 } else { 1 };
            let available = limit.saturating_sub(self.pos) / width;
            if available == 0 {
                return Err(Error::Eof);
            }
            let take = remaining.min(available);
            if high {
                units.reserve(take);
                for i in 0..take {
                    let at = self.pos + i * 2;
                    units.push(u16::from_le_bytes([self.data[at], self.data[at + 1]]));
                }
            } else {
                for offset in 0..take {
                    push_char(&mut units, self.map[self.data[self.pos + offset] as usize]);
                }
            }
            self.pos += take * width;
            remaining -= take;
        }
        out.push_str(&String::from_utf16_lossy(&units));
        Ok(())
    }

    /// Reads an `XLUnicodeRichExtendedString` whose length is a 16-bit field.
    fn read_string(&mut self) -> Result<String> {
        let count = u32::from(self.read_u16()?);
        let flags = self.read_u8()?;
        let high = flags & 0x01 != 0;
        let rich = flags & 0x08 != 0;
        let extended = flags & 0x04 != 0;
        // Both counts precede the characters, so they must be read before them.
        let runs = if rich { u32::from(self.read_u16()?) } else { 0 };
        let extended_len = if extended { self.read_u32()? } else { 0 };

        let mut out = String::new();
        self.read_chars(count, high, &mut out)?;
        self.skip(runs as usize * 4)?;
        self.skip(extended_len as usize)?;
        Ok(out)
    }
}

/// A string field kept as bytes until the workbook's code page is known.
///
/// BIFF writes a string either as UTF-16 or as one byte per character in the workbook's code page.
/// Which one is recorded per string, but the code page is a separate record, so decoding has to
/// wait until the globals substream has been walked.
struct RawString {
    bytes: Vec<u8>,
    /// True when the string is UTF-16, which needs no code page.
    wide: bool,
}

impl RawString {
    fn decode(&self, map: &ByteMap) -> String {
        if self.wide {
            String::from_utf16_lossy(&decode_units(&self.bytes))
        } else {
            self.bytes.iter().map(|byte| map[*byte as usize]).collect()
        }
    }
}

/// Reads UTF-16 little-endian units from a byte string.
fn decode_units(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect()
}

/// Splits a `BOUNDSHEET` record into the substream offset and the sheet name.
fn parse_boundsheet(body: &[u8]) -> Result<(u32, RawString, SheetVisibility)> {
    let mut c = Cursor::new(body);
    let offset = c.read_u32()?;
    // The low two bits are the sheet's visibility: 0 visible, 1 hidden, 2 very hidden.
    let state = c.read_u16()?;
    let visibility = match state & 0x03 {
        0x01 => SheetVisibility::Hidden,
        0x02 => SheetVisibility::VeryHidden,
        _ => SheetVisibility::Visible,
    };
    let count = usize::from(c.read_u8()?);
    let flags = c.read_u8()?;
    let wide = flags & 0x01 != 0;
    let bytes = c.read_bytes(count * if wide { 2 } else { 1 })?.to_vec();
    Ok((offset, RawString { bytes, wide }, visibility))
}

/// Splits a `FORMAT` record into its id and code.
fn parse_format(body: &[u8]) -> Option<(u16, RawString)> {
    let mut c = Cursor::new(body);
    let id = c.read_u16().ok()?;
    let count = usize::from(c.read_u16().ok()?);
    let flags = c.read_u8().ok()?;
    let wide = flags & 0x01 != 0;
    let bytes = c
        .read_bytes(count * if wide { 2 } else { 1 })
        .ok()?
        .to_vec();
    Some((id, RawString { bytes, wide }))
}

/// Parses the shared string table.
fn parse_sst(buffer: &[u8], boundaries: &[usize], map: &ByteMap) -> Result<Vec<String>> {
    let mut reader = UnitReader::new(buffer, boundaries, map);
    let _total = reader.read_u32()?;
    let unique = reader.read_u32()?;
    // The declared count is attacker-controlled, so the reservation is capped; the loop below
    // stops as soon as the buffer runs out regardless.
    let mut out = Vec::with_capacity((unique as usize).min(1_000_000));
    for _ in 0..unique {
        if reader.pos >= buffer.len() {
            break;
        }
        match reader.read_string() {
            Ok(text) => out.push(text),
            Err(_) => break,
        }
    }
    Ok(out)
}

/// Expands a BIFF `RK` value.
///
/// The top 30 bits are either a signed integer or the high half of an IEEE double whose low
/// 34 bits are zero; two flag bits say which, and whether the result is scaled by 100.
fn decode_rk(rk: u32) -> f64 {
    let is_integer = rk & 0x02 != 0;
    let scaled = rk & 0x01 != 0;
    let value = if is_integer {
        f64::from((rk as i32) >> 2)
    } else {
        f64::from_bits(u64::from(rk & 0xFFFF_FFFC) << 32)
    };
    if scaled {
        value / 100.0
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_rk_integers() {
        let integer = |v: i32| ((v << 2) as u32) | 0b10;
        assert_eq!(decode_rk(integer(1)), 1.0);
        assert_eq!(decode_rk(integer(7)), 7.0);
        assert_eq!(decode_rk(integer(-3)), -3.0);
        // The low bit means the value is scaled down by 100.
        assert_eq!(decode_rk(((100 << 2) as u32) | 0b11), 1.0);
    }

    #[test]
    fn decodes_rk_doubles() {
        // A double is stored as its high 32 bits, which is why only values whose low 34 bits
        // are clear survive the round trip.
        let double = |v: f64| ((v.to_bits() >> 32) as u32) & 0xFFFF_FFFC;
        assert_eq!(decode_rk(double(1.5)), 1.5);
        assert_eq!(decode_rk(double(2.5)), 2.5);
        assert_eq!(decode_rk(double(12.75)), 12.75);
    }

    #[test]
    fn reads_records_and_stops_at_the_end() {
        let data = [0xFC, 0x00, 0x02, 0x00, 0xAA, 0xBB, 0xFF, 0xFF];
        let (opcode, body, next) = read_record(&data, 0).unwrap();
        assert_eq!(opcode, op::SST);
        assert_eq!(body, &[0xAA, 0xBB]);
        assert_eq!(next, 6);
        // A truncated header yields nothing rather than panicking.
        assert!(read_record(&data, 7).is_none());
        // A length that overruns the buffer is rejected.
        assert!(read_record(&[0x00, 0x00, 0xFF, 0xFF, 0x00], 0).is_none());
    }

    #[test]
    fn reads_boundsheet_names() {
        let mut body = Vec::new();
        body.extend_from_slice(&0x1234u32.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.push(5);
        body.push(0x01);
        for unit in "Sheet".encode_utf16() {
            body.extend_from_slice(&unit.to_le_bytes());
        }
        let (offset, name, visibility) = parse_boundsheet(&body).unwrap();
        assert_eq!(name.decode(&byte_map(1252)), "Sheet");
        assert_eq!(offset, 0x1234);
        assert_eq!(visibility, SheetVisibility::Visible, "state 0 is visible");
    }

    /// The low two bits of the BOUNDSHEET state word are the visibility.
    #[test]
    fn boundsheet_visibility_is_decoded() {
        let build = |state: u16| {
            let mut body = Vec::new();
            body.extend_from_slice(&0u32.to_le_bytes());
            body.extend_from_slice(&state.to_le_bytes());
            body.push(1);
            body.push(0x01);
            body.extend_from_slice(&u16::from(b'S').to_le_bytes());
            body
        };
        assert_eq!(
            parse_boundsheet(&build(0)).unwrap().2,
            SheetVisibility::Visible
        );
        assert_eq!(
            parse_boundsheet(&build(1)).unwrap().2,
            SheetVisibility::Hidden
        );
        assert_eq!(
            parse_boundsheet(&build(2)).unwrap().2,
            SheetVisibility::VeryHidden
        );
    }

    #[test]
    fn reads_compressed_strings_across_continuations() {
        // "AB" as a compressed string: count, flags, 'A', then a continuation that begins
        // with the encoding byte again before 'B'.
        let mut buffer = vec![2, 0x00, 0x00, b'A'];
        let boundaries = vec![buffer.len()];
        buffer.push(0x00);
        buffer.push(b'B');
        let map = byte_map(1252);
        let mut reader = UnitReader::new(&buffer, &boundaries, &map);
        assert_eq!(reader.read_string().unwrap(), "AB");
    }

    #[test]
    fn reads_wide_strings() {
        let mut buffer = vec![2, 0x00, 0x01];
        for unit in "Hi".encode_utf16() {
            buffer.extend_from_slice(&unit.to_le_bytes());
        }
        let map = byte_map(1252);
        let mut reader = UnitReader::new(&buffer, &[], &map);
        assert_eq!(reader.read_string().unwrap(), "Hi");
    }

    /// A compressed string is read in the workbook's code page, not as Latin-1.
    #[test]
    fn compressed_strings_use_the_byte_map() {
        // Two characters: 0x80 and 0xE9. In Latin-1 the first is a control character; in the code
        // pages that matter here it is the euro sign.
        let buffer = vec![2, 0x00, 0x00, 0x80, 0xE9];
        let map = byte_map(1252);
        let mut reader = UnitReader::new(&buffer, &[], &map);
        let decoded = reader.read_string().unwrap();

        assert_eq!(decoded.chars().nth(1), Some('\u{00E9}'), "0xE9 is e-acute");
        #[cfg(feature = "codepage")]
        assert_eq!(
            decoded.chars().next(),
            Some('\u{20AC}'),
            "0x80 is the euro sign"
        );
        #[cfg(not(feature = "codepage"))]
        assert_eq!(decoded.chars().next(), Some('\u{0080}'));
    }

    #[test]
    fn sst_parses_both_encodings() {
        let mut buffer = 2u32.to_le_bytes().to_vec();
        buffer.extend_from_slice(&2u32.to_le_bytes());
        buffer.extend(fixture::string("hello"));
        buffer.extend(fixture::string("wélcome"));
        let strings = parse_sst(&buffer, &[], &byte_map(1252)).unwrap();
        assert_eq!(strings, vec!["hello".to_string(), "wélcome".to_string()]);
    }

    /// Builds a minimal but structurally valid BIFF8 workbook inside an OLE container.
    ///
    /// Testing the legacy path needs a file to read, and none can be shipped for this format
    /// without licensing concerns, so one is constructed here instead: a real compound file
    /// with a real FAT and directory, wrapping a real record stream. That exercises the offset
    /// arithmetic, the shared string table and every cell record type the reader claims to
    /// handle, rather than assuming how they are laid out.
    mod fixture {
        use super::CFB_MAGIC;

        const FREESECT: u32 = 0xFFFF_FFFF;
        const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
        const FATSECT: u32 = 0xFFFF_FFFD;
        const SECTOR: usize = 512;

        fn record(op: u16, body: &[u8]) -> Vec<u8> {
            let mut out = Vec::with_capacity(body.len() + 4);
            out.extend_from_slice(&op.to_le_bytes());
            out.extend_from_slice(&(body.len() as u16).to_le_bytes());
            out.extend_from_slice(body);
            out
        }

        /// A BIFF8 string with a 16-bit length, choosing the compressed form when it fits.
        pub fn string(s: &str) -> Vec<u8> {
            let mut out = Vec::new();
            let count = s.chars().count() as u16;
            out.extend_from_slice(&count.to_le_bytes());
            if s.is_ascii() {
                out.push(0x00);
                out.extend_from_slice(s.as_bytes());
            } else {
                out.push(0x01);
                for unit in s.encode_utf16() {
                    out.extend_from_slice(&unit.to_le_bytes());
                }
            }
            out
        }

        /// A BIFF8 string with an 8-bit length, as sheet names use.
        fn short_string(s: &str) -> Vec<u8> {
            let mut out = Vec::new();
            out.push(s.chars().count() as u8);
            if s.is_ascii() {
                out.push(0x00);
                out.extend_from_slice(s.as_bytes());
            } else {
                out.push(0x01);
                for unit in s.encode_utf16() {
                    out.extend_from_slice(&unit.to_le_bytes());
                }
            }
            out
        }

        /// A compressed string written byte for byte, so a specific code page byte can be tested.
        fn compressed(bytes: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
            out.push(0x00);
            out.extend_from_slice(bytes);
            out
        }

        fn bof(substream: u16) -> Vec<u8> {
            let mut body = Vec::new();
            body.extend_from_slice(&0x0600u16.to_le_bytes());
            body.extend_from_slice(&substream.to_le_bytes());
            body.extend_from_slice(&[0u8; 12]);
            record(0x0809, &body)
        }

        /// An XF record of the BIFF8 length, with the format id where the reader expects it.
        fn xf(ifmt: u16) -> Vec<u8> {
            let mut body = vec![0u8; 20];
            body[2..4].copy_from_slice(&ifmt.to_le_bytes());
            record(0x00E0, &body)
        }

        fn cell_header(row: u16, col: u16, ixfe: u16) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&row.to_le_bytes());
            out.extend_from_slice(&col.to_le_bytes());
            out.extend_from_slice(&ixfe.to_le_bytes());
            out
        }

        fn rk_value(value: i32) -> u32 {
            ((value << 2) as u32) | 0b10
        }

        /// The workbook as a flat record stream, plus the sheet's substream offset.
        pub fn workbook_stream() -> Vec<u8> {
            let mut globals = Vec::new();
            globals.extend(bof(0x0005));
            globals.extend(record(0x0022, &0u16.to_le_bytes()));
            globals.extend(record(0x0042, &1252u16.to_le_bytes()));

            let mut format = 164u16.to_le_bytes().to_vec();
            format.extend(string("yyyy-mm-dd"));
            globals.extend(record(0x041E, &format));

            // Format 0 is General, format 1 is the date format declared above.
            globals.extend(xf(0));
            globals.extend(xf(164));

            let boundsheet_at = globals.len();
            let mut boundsheet = 0u32.to_le_bytes().to_vec();
            boundsheet.extend_from_slice(&0u16.to_le_bytes());
            boundsheet.extend(short_string("Sheet1"));
            globals.extend(record(0x0085, &boundsheet));

            let mut sst = 3u32.to_le_bytes().to_vec();
            sst.extend_from_slice(&3u32.to_le_bytes());
            sst.extend(string("hello"));
            sst.extend(string("wélcome"));
            // A compressed string whose first byte is 0x80, which is a control character in
            // Latin-1 but the euro sign in the code page this workbook declares.
            sst.extend(compressed(&[0x80, b'!']));
            globals.extend(record(0x00FC, &sst));
            globals.extend(record(0x000A, &[]));

            let sheet_offset = globals.len() as u32;
            globals[boundsheet_at + 4..boundsheet_at + 8]
                .copy_from_slice(&sheet_offset.to_le_bytes());

            let mut sheet = bof(0x0010);

            let mut labelsst = cell_header(0, 0, 0);
            labelsst.extend_from_slice(&0u32.to_le_bytes());
            sheet.extend(record(0x00FD, &labelsst));

            // A date: format index 1 says the number is a date.
            let mut date = cell_header(1, 0, 1);
            date.extend_from_slice(&45321.5f64.to_le_bytes());
            sheet.extend(record(0x0203, &date));

            let mut number = cell_header(2, 0, 0);
            number.extend_from_slice(&42.0f64.to_le_bytes());
            sheet.extend(record(0x0203, &number));

            let mut rk = cell_header(3, 0, 0);
            rk.extend_from_slice(&(((3.5f64.to_bits() >> 32) as u32) & 0xFFFF_FFFC).to_le_bytes());
            sheet.extend(record(0x027E, &rk));

            let mut boolean = cell_header(4, 0, 0);
            boolean.extend_from_slice(&[1, 0]);
            sheet.extend(record(0x0205, &boolean));

            let mut error = cell_header(5, 0, 0);
            error.extend_from_slice(&[0x07, 1]);
            sheet.extend(record(0x0205, &error));

            let mut mulrk = Vec::new();
            mulrk.extend_from_slice(&6u16.to_le_bytes());
            mulrk.extend_from_slice(&0u16.to_le_bytes());
            for value in [10i32, 20, 30] {
                mulrk.extend_from_slice(&0u16.to_le_bytes());
                mulrk.extend_from_slice(&rk_value(value).to_le_bytes());
            }
            mulrk.extend_from_slice(&2u16.to_le_bytes());
            sheet.extend(record(0x00BD, &mulrk));

            // A formula whose cached result is a string, carried in a following record.
            let mut formula = cell_header(7, 0, 0);
            formula.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0xFF, 0xFF]);
            formula.extend_from_slice(&0u16.to_le_bytes());
            formula.extend_from_slice(&0u32.to_le_bytes());
            sheet.extend(record(0x0006, &formula));
            sheet.extend(record(0x0207, &string("from formula")));

            let mut label = cell_header(8, 0, 0);
            label.extend(string("plain"));
            sheet.extend(record(0x0204, &label));

            let mut wide = cell_header(9, 0, 0);
            wide.extend_from_slice(&1u32.to_le_bytes());
            sheet.extend(record(0x00FD, &wide));

            // The compressed string, which has to be read in the workbook's code page.
            let mut coded = cell_header(10, 0, 0);
            coded.extend_from_slice(&2u32.to_le_bytes());
            sheet.extend(record(0x00FD, &coded));

            sheet.extend(record(0x000A, &[]));

            globals.extend(sheet);
            globals
        }

        /// The globals substream that finds one sheet, wrapping `sheet`.
        fn wrap_sheet(sheet: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend(bof(0x0005));
            out.extend(record(0x0042, &1252u16.to_le_bytes()));
            let boundsheet_at = out.len();
            let mut boundsheet = 0u32.to_le_bytes().to_vec();
            boundsheet.extend_from_slice(&0u16.to_le_bytes());
            boundsheet.extend(short_string("Sheet1"));
            out.extend(record(0x0085, &boundsheet));
            out.extend(record(0x000A, &[]));

            let sheet_offset = out.len() as u32;
            out[boundsheet_at + 4..boundsheet_at + 8].copy_from_slice(&sheet_offset.to_le_bytes());
            out.extend_from_slice(sheet);
            out
        }

        /// A row record for `row`: sixteen bytes with the row's number first.
        fn row_record(row: u16) -> Vec<u8> {
            let mut body = row.to_le_bytes().to_vec();
            body.resize(16, 0);
            record(0x0208, &body)
        }

        /// A sheet that declares rows and leaves them empty.
        ///
        /// A row exists here because the file says so rather than because a cell was written to it,
        /// which is the only way an empty row can be told apart from the end of the data.
        pub fn workbook_stream_with_declared_rows() -> Vec<u8> {
            let mut sheet = bof(0x0010);
            sheet.extend(row_record(0));
            let mut first = cell_header(1, 0, 0);
            first.extend_from_slice(&1.0f64.to_le_bytes());
            sheet.extend(record(0x0203, &first));
            sheet.extend(row_record(2));
            sheet.extend(row_record(5));
            let mut second = cell_header(6, 0, 0);
            second.extend_from_slice(&2.0f64.to_le_bytes());
            sheet.extend(record(0x0203, &second));
            sheet.extend(record(0x000A, &[]));
            wrap_sheet(&sheet)
        }

        /// A sheet whose two cells sit at opposite corners of the grid BIFF can address.
        ///
        /// The rectangle between them is 65536 by 256 — 537 MB of values — which is what the dense
        /// read would have to hold and what streaming does not.
        pub fn workbook_stream_across_the_grid() -> Vec<u8> {
            let mut sheet = bof(0x0010);
            let mut first = cell_header(0, 0, 0);
            first.extend_from_slice(&1.0f64.to_le_bytes());
            sheet.extend(record(0x0203, &first));
            let mut last = cell_header(65535, 255, 0);
            last.extend_from_slice(&2.0f64.to_le_bytes());
            sheet.extend(record(0x0203, &last));
            sheet.extend(record(0x000A, &[]));
            wrap_sheet(&sheet)
        }

        fn name_utf16(name: &str) -> [u8; 64] {
            let mut buffer = [0u8; 64];
            for (i, unit) in name.encode_utf16().enumerate() {
                let at = i * 2;
                if at + 2 > 64 {
                    break;
                }
                buffer[at..at + 2].copy_from_slice(&unit.to_le_bytes());
            }
            buffer
        }

        fn directory_entry(name: &str, kind: u8, child: u32, start: u32, size: u64) -> [u8; 128] {
            let mut entry = [0u8; 128];
            entry[..64].copy_from_slice(&name_utf16(name));
            let name_len = ((name.chars().count() + 1) * 2) as u16;
            entry[0x40..0x42].copy_from_slice(&name_len.to_le_bytes());
            entry[0x42] = kind;
            entry[0x43] = 1;
            entry[0x44..0x48].copy_from_slice(&FREESECT.to_le_bytes());
            entry[0x48..0x4C].copy_from_slice(&FREESECT.to_le_bytes());
            entry[0x4C..0x50].copy_from_slice(&child.to_le_bytes());
            entry[0x74..0x78].copy_from_slice(&start.to_le_bytes());
            entry[0x78..0x80].copy_from_slice(&size.to_le_bytes());
            entry
        }

        /// Wraps a record stream in an OLE container with a single FAT sector.
        pub fn wrap(stream: &[u8]) -> Vec<u8> {
            // The reader takes the main allocation table rather than the mini stream for
            // streams of 4096 bytes or more, so the stream is padded past that threshold.
            let data_sectors = stream.len().div_ceil(SECTOR).max(9);
            let data_at = 2u32;
            let data_len = data_sectors * SECTOR;

            let mut fat = vec![FREESECT; SECTOR / 4];
            fat[0] = FATSECT;
            fat[1] = ENDOFCHAIN;
            for i in 0..data_sectors {
                let sector = data_at as usize + i;
                fat[sector] = if i + 1 == data_sectors {
                    ENDOFCHAIN
                } else {
                    (sector + 1) as u32
                };
            }

            let mut header = vec![0u8; SECTOR];
            header[..8].copy_from_slice(&CFB_MAGIC);
            header[0x18..0x1A].copy_from_slice(&0x003Eu16.to_le_bytes());
            header[0x1A..0x1C].copy_from_slice(&0x0003u16.to_le_bytes());
            header[0x1C..0x1E].copy_from_slice(&0xFFFEu16.to_le_bytes());
            header[0x1E..0x20].copy_from_slice(&9u16.to_le_bytes());
            header[0x20..0x22].copy_from_slice(&6u16.to_le_bytes());
            header[0x2C..0x30].copy_from_slice(&1u32.to_le_bytes());
            header[0x30..0x34].copy_from_slice(&1u32.to_le_bytes());
            header[0x38..0x3C].copy_from_slice(&4096u32.to_le_bytes());
            header[0x3C..0x40].copy_from_slice(&ENDOFCHAIN.to_le_bytes());
            header[0x44..0x48].copy_from_slice(&ENDOFCHAIN.to_le_bytes());
            for slot in header[0x4C..0x200].chunks_exact_mut(4) {
                slot.copy_from_slice(&FREESECT.to_le_bytes());
            }
            header[0x4C..0x50].copy_from_slice(&0u32.to_le_bytes());

            let mut directory = vec![0u8; SECTOR];
            directory[..128].copy_from_slice(&directory_entry("Root Entry", 5, 1, ENDOFCHAIN, 0));
            directory[128..256].copy_from_slice(&directory_entry(
                "Workbook",
                2,
                FREESECT,
                data_at,
                data_len as u64,
            ));

            let mut data = stream.to_vec();
            data.resize(data_len, 0);

            let mut out = header;
            out.extend_from_slice(
                &fat.iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect::<Vec<u8>>(),
            );
            out.extend_from_slice(&directory);
            out.extend_from_slice(&data);
            out
        }
    }

    fn open_fixture() -> Xls<std::io::Cursor<Vec<u8>>> {
        open_stream(fixture::workbook_stream())
    }

    fn open_stream(stream: Vec<u8>) -> Xls<std::io::Cursor<Vec<u8>>> {
        let bytes = fixture::wrap(&stream);
        Xls::new(std::io::Cursor::new(bytes)).expect("fixture workbook should open")
    }

    /// Every populated cell of a sheet, as `(row, column, value)`, in row-major order.
    fn streamed(workbook: &mut Xls<std::io::Cursor<Vec<u8>>>) -> Vec<(u32, u32, Data)> {
        let mut cells = Vec::new();
        let mut rows = workbook.worksheet_rows("Sheet1").unwrap();
        while let Some(row) = rows.next_row().unwrap() {
            for (column, value) in row.iter() {
                cells.push((row.index(), column, value.clone()));
            }
        }
        cells
    }

    /// The same cells, read by building the sheet's rectangle.
    fn dense(workbook: &mut Xls<std::io::Cursor<Vec<u8>>>) -> Vec<(u32, u32, Data)> {
        workbook
            .worksheet_range("Sheet1")
            .unwrap()
            .cells()
            .filter(|(_, _, value)| !value.is_empty())
            .map(|(row, column, value)| (row, column, value.clone()))
            .collect()
    }

    /// The two reads are alternative shapes for the same records, so they must not disagree.
    ///
    /// The fixture holds every cell record the reader claims to handle, so this compares the two
    /// decoders on each of them rather than on whichever one a test happened to reach.
    #[test]
    fn streaming_agrees_with_the_dense_range() {
        let mut workbook = open_fixture();
        assert_eq!(streamed(&mut workbook), dense(&mut workbook));
    }

    /// A row the substream declares and leaves empty is still a row.
    #[test]
    fn declared_rows_appear_even_when_they_hold_nothing() {
        let mut workbook = open_stream(fixture::workbook_stream_with_declared_rows());
        let mut rows = workbook.worksheet_rows("Sheet1").unwrap();
        let mut seen = Vec::new();
        while let Some(row) = rows.next_row().unwrap() {
            seen.push((row.index(), row.get(0).cloned()));
        }
        assert_eq!(
            seen,
            vec![
                (0, None),
                (1, Some(Data::Int(1))),
                (2, None),
                (5, None),
                (6, Some(Data::Int(2))),
            ]
        );
    }

    /// Cells at opposite corners of the addressable grid must not cost the rectangle between them.
    ///
    /// That rectangle is 65536 by 256 cells, so this is also the case where the dense read is out of
    /// reach: reading it would ask for half a gigabyte of values to report two.
    #[test]
    fn a_sheet_wide_apart_streams_without_its_rectangle() {
        let mut workbook = open_stream(fixture::workbook_stream_across_the_grid());
        let mut rows = workbook.worksheet_rows("Sheet1").unwrap();

        let first = rows.next_row().unwrap().unwrap();
        assert_eq!((first.index(), first.get(0)), (0, Some(&Data::Int(1))));

        let last = rows.next_row().unwrap().unwrap();
        assert_eq!((last.index(), last.get(255)), (65535, Some(&Data::Int(2))));
        assert!(rows.next_row().unwrap().is_none());
    }

    /// A window works on this format too, which it does by reading rows and placing the ones the
    /// window holds — the same shape as the streaming read, so the two cannot drift apart.
    #[test]
    fn a_window_reads_a_legacy_sheet() {
        let mut workbook = open_fixture();
        let window = workbook.worksheet_range_ref("Sheet1", "A2:A4").unwrap();
        let whole = workbook.worksheet_range("Sheet1").unwrap();

        assert_eq!(window.start(), (1, 0));
        assert_eq!(window.get_size(), (3, 1));
        for row in 1..4 {
            assert_eq!(
                window.get((row, 0)),
                whole.get((row, 0)),
                "row {row} differs from the dense range"
            );
        }
    }

    #[test]
    fn reads_a_biff8_workbook() {
        let mut workbook = open_fixture();
        assert_eq!(workbook.sheet_names(), ["Sheet1".to_string()]);

        let range = workbook.worksheet_range("Sheet1").unwrap();
        let column: Vec<Data> = range.rows().map(|row| row[0].clone()).collect();

        assert_eq!(column.len(), 11);
        // A shared-string reference.
        assert_eq!(column[0], Data::String("hello".into()));
        // Format index 1 is a date format, so this becomes a date-time rather than a number.
        assert_eq!(
            column[1],
            Data::DateTime(ExcelDateTime::new(45321.5, false))
        );
        assert_eq!(column[2], Data::Int(42));
        assert_eq!(column[3], Data::Float(3.5));
        assert_eq!(column[4], Data::Bool(true));
        assert_eq!(column[5], Data::Error(CellError::Div0));
        assert_eq!(column[6], Data::Int(10));
        assert_eq!(column[7], Data::String("from formula".into()));
        assert_eq!(column[8], Data::String("plain".into()));
        // A wide (UTF-16) entry in the shared string table.
        assert_eq!(column[9], Data::String("wélcome".into()));
        // A compressed entry, decoded in the code page the workbook declares (1252), which the
        // reader has to pick up from a record of its own rather than assume.
        #[cfg(feature = "codepage")]
        assert_eq!(column[10], Data::String("\u{20AC}!".into()));
        #[cfg(not(feature = "codepage"))]
        assert_eq!(column[10], Data::String("\u{0080}!".into()));
    }

    #[test]
    fn reads_the_date_as_a_calendar_date() {
        let mut workbook = open_fixture();
        let range = workbook.worksheet_range("Sheet1").unwrap();
        let value = range.get((1, 0)).unwrap().as_date().unwrap();
        assert_eq!((value.year, value.month, value.day), (2024, 1, 30));
        assert_eq!((value.hour, value.minute), (12, 0));
    }

    #[test]
    fn multi_cell_records_expand_across_columns() {
        let mut workbook = open_fixture();
        let range = workbook.worksheet_range("Sheet1").unwrap();
        let row: Vec<Data> = range.rows().nth(6).unwrap().to_vec();
        assert_eq!(row[0], Data::Int(10));
        assert_eq!(row[1], Data::Int(20));
        assert_eq!(row[2], Data::Int(30));
    }

    #[test]
    fn formula_text_is_reported_as_unsupported() {
        let mut workbook = open_fixture();
        assert!(matches!(
            workbook.worksheet_formula("Sheet1"),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn missing_sheets_are_reported() {
        let mut workbook = open_fixture();
        assert!(matches!(
            workbook.worksheet_range("Nope"),
            Err(Error::NotFound(_))
        ));
    }
}

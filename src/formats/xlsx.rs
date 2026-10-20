//! OOXML workbook (`.xlsx`, `.xlsm`, `.xltx`, `.xlam`) reading.

use crate::container::{read_part, Archive, ZipContainer, MAX_ENTRY_BYTES};
use crate::datatype::{CellError, Data, ExcelDateTime};
use crate::error::{Error, Result};
use crate::formula::shift_formula;
use crate::range::{Extent, Range, SheetBuilder, MAX_AXIS};
use crate::reader::{apply_header_row, HeaderRow, MergedRegion, Reader, SheetVisibility};
use crate::refs::{parse_cell_ref, parse_range_ref};
use crate::rows::{read_window, skip_header_row, RowSource, SheetRows, Window};
use crate::utils::format_is_date;
use crate::xml::{decode_ooxml_escapes, local_name, Event, XmlParser};
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{Read, Seek};

/// Which cell formats render as dates, indexed by the workbook's format list.
#[derive(Debug, Default, Clone)]
struct Styles {
    date_xf: Vec<bool>,
}

/// A sheet as declared in the workbook part.
struct RawSheet {
    name: String,
    rid: Option<String>,
    sheet_id: u32,
    visibility: SheetVisibility,
}

/// A formula as written in a worksheet part.
#[derive(Debug, Clone, Default)]
struct FormulaRecord {
    text: String,
    /// The `t` attribute: empty when the cell has no formula at all.
    kind: String,
    /// The shared-formula group this cell belongs to.
    si: Option<u32>,
}

const CHARTSHEET_REL: &str = "/chartsheet";
const DIALOGSHEET_REL: &str = "/dialogsheet";

/// A reader for OOXML workbooks.
///
/// Sheets are parsed on demand, so opening a workbook only reads its metadata and the shared
/// string table is not touched until a sheet that needs it is read.
pub struct Xlsx<RS: Read + Seek> {
    archive: ZipContainer<RS>,
    names: Vec<String>,
    /// Visibility per sheet, parallel to `names`.
    visibilities: Vec<SheetVisibility>,
    /// Part path per sheet, parallel to `names`.
    parts: Vec<String>,
    defined: Vec<(String, String)>,
    is_1904: bool,
    /// Loaded on first use, because a workbook can carry millions of strings.
    strings: Option<Vec<String>>,
    styles: Styles,
    /// The sheet part currently being streamed.
    ///
    /// A streaming reader has to borrow the part it is reading, and the part is decompressed on
    /// demand, so the decompressed bytes are held here for the reader's lifetime. Only one sheet
    /// is streamed at a time, and this is reused for the next one.
    stream_xml: Option<Vec<u8>>,
    header: HeaderRow,
}

impl<RS: Read + Seek> Xlsx<RS> {
    /// Reads a part, trying each candidate path in turn.
    fn read_part(&mut self, candidates: &[&str]) -> Result<Option<Vec<u8>>> {
        for name in candidates {
            if let Some(bytes) = self.archive.read(name)? {
                return Ok(Some(bytes));
            }
        }
        Ok(None)
    }

    /// Reads a part, falling back to any entry with a matching file name.
    ///
    /// Producers occasionally place parts at non-standard paths, and a workbook is worth
    /// reading if the file is simply somewhere else in the package.
    fn read_part_fuzzy(&mut self, candidates: &[&str], file_name: &str) -> Result<Option<Vec<u8>>> {
        if let Some(bytes) = self.read_part(candidates)? {
            return Ok(Some(bytes));
        }
        let wanted = file_name.to_ascii_lowercase();
        let found = self
            .archive
            .names()
            .into_iter()
            .find(|n| n.rsplit('/').next() == Some(wanted.as_str()));
        match found {
            Some(name) => self.archive.read(&name),
            None => Ok(None),
        }
    }

    /// Caches the shared string table, if it has not been read yet.
    fn load_shared_strings(&mut self) -> Result<()> {
        if self.strings.is_some() {
            return Ok(());
        }
        let parsed = match self.read_part_fuzzy(&["xl/sharedStrings.xml"], "sharedstrings.xml")? {
            Some(xml) => parse_shared_strings(&xml)?,
            None => Vec::new(),
        };
        self.strings = Some(parsed);
        Ok(())
    }

    /// Reads and parses one worksheet part.
    fn read_sheet_xml(&mut self, index: usize) -> Result<Vec<u8>> {
        let part = self.parts[index].clone();
        self.archive
            .read(&part)?
            .ok_or_else(|| Error::missing(part))
    }

    /// Turns a parsed sheet into a dense range.
    ///
    /// The builder already holds the cells in their final positions, so nothing is copied here.
    fn range_of(parsed: ParsedSheet, header: HeaderRow) -> Result<Range<Data>> {
        Ok(apply_header_row(parsed.builder.finish(), header))
    }

    /// Reads a sheet part only as far as the row a window ends on.
    ///
    /// A reader holding the region it was asked for has no use for the rest of the part, and
    /// stopping the decompression is what makes a window read cost the window rather than the
    /// sheet. The truncation point is checked against the parser before it is used, so a part that
    /// does not follow the format's row ordering is read whole rather than cut short.
    fn read_prefix(&mut self, index: usize, last_row: u32) -> Result<Vec<u8>> {
        let part = self.parts[index].clone();
        let mut input = self
            .archive
            .open(&part)?
            .ok_or_else(|| Error::missing(part.clone()))?;

        let mut out = Vec::new();
        // The aim is to hold as little of the part as possible, so start small: a window on the
        // first rows of a large sheet should not need a chunk sized for the whole sheet.
        let mut buf = [0u8; PREFIX_CHUNK];
        let mut closes = RowCloses::default();
        loop {
            if read_part(&mut input, &mut buf, &mut out, MAX_ENTRY_BYTES)? == 0 {
                return Ok(out);
            }
            let Some(end) = closes.feed(&out, last_row) else {
                continue;
            };
            // Knowing a row has gone by from the number of closes alone assumes rows are listed in
            // order, which the format requires but a reader should not simply trust. One walk of
            // the prefix settles it; if it disagrees the part is taken whole rather than trusted.
            if prefix_through_row(&out[..end], last_row).is_some() {
                // Cut at the row's end rather than where the chunk stopped, so what is left is a
                // document that stops cleanly instead of one with a torn tag.
                out.truncate(end);
                return Ok(out);
            }
            while read_part(&mut input, &mut buf, &mut out, MAX_ENTRY_BYTES)? != 0 {}
            return Ok(out);
        }
    }

    /// Streams one sheet's rows, skipping anything outside `window` when there is one.
    ///
    /// Cells are decoded by the same code the dense path uses, so the two cannot disagree about
    /// what a cell holds. The `dimension` element is ignored, since a buffer sized from the sheet's
    /// extent is exactly what streaming exists to avoid.
    fn rows_with(&mut self, name: &str, window: Option<Window>) -> Result<SheetRows<'_>> {
        let index = self
            .sheet_index(name)
            .ok_or_else(|| Error::NotFound(format!("sheet {name:?}")))?;
        let header = self.header;
        // A window read stops the part as soon as it covers the window, so it never holds the rest
        // of the sheet. A whole-sheet read has no such point and takes the part as it comes.
        let xml = match window {
            Some(window) => self.read_prefix(index, window.rows.1)?,
            None => self.read_sheet_xml(index)?,
        };
        self.load_shared_strings()?;
        self.stream_xml = Some(xml);

        let xml = self.stream_xml.as_deref().unwrap_or(&[]);
        let mut rows = SheetRows::new(XlsxRows {
            parser: XmlParser::new(xml),
            ctx: SheetCtx {
                strings: self.strings.as_deref().unwrap_or(&[]),
                date_xf: &self.styles.date_xf,
                is_1904: self.is_1904,
            },
            cursor: RowCursor::default(),
            window,
            exhausted: false,
        });
        // A whole-sheet read is trimmed to the header row to match the dense path. An explicit
        // window is not: the caller named exact bounds, and silently shifting them would be a
        // surprise.
        if window.is_none() {
            skip_header_row(&mut rows, header)?;
        }
        Ok(rows)
    }
}

impl<RS: Read + Seek> Reader<RS> for Xlsx<RS> {
    fn new(reader: RS) -> Result<Self> {
        let mut archive = ZipContainer::new(reader)?;
        let workbook = {
            let mut found = archive
                .read("xl/workbook.xml")?
                .or_else(|| archive.read("workbook.xml").ok().flatten());
            if found.is_none() {
                if let Some(name) = archive
                    .names()
                    .into_iter()
                    .find(|n| n.ends_with("/workbook.xml"))
                {
                    found = archive.read(&name)?;
                }
            }
            found.ok_or_else(|| Error::missing("workbook.xml"))?
        };
        let WorkbookMeta {
            is_1904,
            sheets: raw_sheets,
            defined,
        } = parse_workbook(&workbook)?;

        let rels = match archive.read("xl/_rels/workbook.xml.rels")? {
            Some(xml) => parse_rels(&xml)?,
            None => HashMap::new(),
        };
        let styles = match archive.read("xl/styles.xml")? {
            Some(xml) => parse_styles(&xml)?,
            None => Styles::default(),
        };

        let mut names = Vec::with_capacity(raw_sheets.len());
        let mut visibilities = Vec::with_capacity(raw_sheets.len());
        let mut parts = Vec::with_capacity(raw_sheets.len());
        for sheet in raw_sheets {
            let rel = sheet.rid.as_ref().and_then(|rid| rels.get(rid));
            // Chart and dialog sheets carry no cells; listing them as empty sheets would be
            // misleading, so they are dropped and `sheet_names` reports only real sheets.
            if let Some((_, kind)) = rel {
                if kind.ends_with(CHARTSHEET_REL) || kind.ends_with(DIALOGSHEET_REL) {
                    continue;
                }
            }
            let part = rel
                .map(|(target, _)| resolve_part(target))
                .unwrap_or_else(|| format!("xl/worksheets/sheet{}.xml", sheet.sheet_id));
            names.push(sheet.name);
            visibilities.push(sheet.visibility);
            parts.push(part);
        }

        Ok(Xlsx {
            archive,
            names,
            visibilities,
            parts,
            defined,
            is_1904,
            strings: None,
            styles,
            stream_xml: None,
            header: HeaderRow::First,
        })
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
        &self.defined
    }

    fn worksheet_range(&mut self, name: &str) -> Result<Range<Data>> {
        let index = self
            .sheet_index(name)
            .ok_or_else(|| Error::NotFound(format!("sheet {name:?}")))?;
        let xml = self.read_sheet_xml(index)?;
        self.load_shared_strings()?;
        let parsed = parse_sheet(
            &xml,
            &SheetCtx {
                strings: self.strings.as_deref().unwrap_or(&[]),
                date_xf: &self.styles.date_xf,
                is_1904: self.is_1904,
            },
            false,
        )?;
        Self::range_of(parsed, self.header)
    }

    fn worksheet_rows(&mut self, name: &str) -> Result<SheetRows<'_>> {
        self.rows_with(name, None)
    }

    /// Reads a window, telling the parser about it so rows and cells outside it are not decoded.
    fn worksheet_range_ref(&mut self, name: &str, reference: &str) -> Result<Range<Data>> {
        let window = Window::parse(reference)?;
        let mut rows = self.rows_with(name, Some(window))?;
        read_window(&mut rows, window)
    }

    fn worksheet_formula(&mut self, name: &str) -> Result<Range<String>> {
        let index = self
            .sheet_index(name)
            .ok_or_else(|| Error::NotFound(format!("sheet {name:?}")))?;
        let xml = self.read_sheet_xml(index)?;
        // The shared string table is deliberately not loaded here. Formula text never refers to
        // it, so a caller reading structure should not pay to inflate millions of strings, nor
        // fail because that table is damaged while the formulas themselves are intact.
        let parsed = parse_sheet(
            &xml,
            &SheetCtx {
                strings: &[],
                date_xf: &self.styles.date_xf,
                is_1904: self.is_1904,
            },
            true,
        )?;
        let formulas = expand_shared_formulas(&parsed.formulas);
        Ok(apply_header_row(Range::from_sparse(formulas)?, self.header))
    }

    /// Reads `<mergeCells>` from the sheet part.
    ///
    /// The shared string table is deliberately not loaded: a merge region is a reference, not a
    /// value, so a caller asking about structure should not pay for text it never reads.
    fn worksheet_merged_regions(&mut self, name: &str) -> Result<Vec<MergedRegion>> {
        let index = self
            .sheet_index(name)
            .ok_or_else(|| Error::NotFound(format!("sheet {name:?}")))?;
        let xml = self.read_sheet_xml(index)?;
        Ok(parse_merged_regions(&xml))
    }
}

/// Collects the sheet's `<mergeCell ref="A1:B2"/>` entries.
///
/// Merge metadata is small and independent of the cells, so this is a separate, shallow walk of
/// the part rather than something the value reader has to carry along.
fn parse_merged_regions(xml: &[u8]) -> Vec<MergedRegion> {
    let mut parser = XmlParser::new(xml);
    let mut out = Vec::new();
    loop {
        match parser.next_event() {
            Ok(Event::Start(el)) => match el.local_name() {
                // Containers worth descending into; everything else is a subtree with no
                // merge metadata in it, and skipping it keeps this proportional to the record.
                "worksheet" | "sheetData" | "cols" | "mergeCells" => {}
                "mergeCell" => {
                    if let Some(region) = el.attr_raw("ref").and_then(parse_region) {
                        out.push(region);
                    }
                }
                _ => {
                    if parser.skip_element().is_err() {
                        break;
                    }
                }
            },
            Ok(Event::Eof) | Err(_) => break,
            Ok(_) => {}
        }
    }
    out
}

/// Turns an A1-style range reference into a region with inclusive corners.
fn parse_region(reference: &str) -> Option<MergedRegion> {
    let (a, b) = parse_range_ref(reference)?;
    Some(MergedRegion::new(a, b))
}

/// Context a worksheet parse needs to turn raw text into values.
struct SheetCtx<'a> {
    strings: &'a [String],
    date_xf: &'a [bool],
    is_1904: bool,
}

impl SheetCtx<'_> {
    /// Whether the cell's style renders as a date or time.
    fn uses_date(&self, xf: Option<usize>) -> bool {
        xf.and_then(|i| self.date_xf.get(i).copied())
            .unwrap_or(false)
    }
}

#[derive(Default)]
struct ParsedSheet {
    /// Cells go straight into their final positions as they are read.
    ///
    /// Collecting them into a sparse list first and converting afterwards would hold the sheet
    /// twice at the moment of peak memory use, which is the whole reason this is a builder.
    builder: SheetBuilder<Data>,
    formulas: Vec<(u32, u32, FormulaRecord)>,
}

/// Turns a `<dimension ref="A1:U389"/>` reference into an extent.
///
/// The declared extent is only a hint and is sometimes stale, so anything unparseable is ignored
/// rather than treated as an error; the real extent is discovered as the sheet is read.
fn parse_extent(reference: &str) -> Option<Extent> {
    let ((start_row, start_col), (end_row, end_col)) = parse_range_ref(reference)?;
    Some(Extent {
        origin: (start_row, start_col),
        rows: end_row.checked_sub(start_row)?.checked_add(1)?,
        cols: end_col.checked_sub(start_col)?.checked_add(1)?,
    })
}

/// How much of a sheet part arrives between looks for the end of the window's last row.
const PREFIX_CHUNK: usize = 16 * 1024;

/// The bytes that end a row.
const ROW_CLOSE: &[u8] = b"</row>";

/// Counts the rows that have closed in a sheet part as its bytes arrive.
///
/// Rows are listed in ascending order with distinct indexes, so once more than `last_row` of them
/// have closed, the row a window ends on has necessarily gone by. Counting closes costs one pass
/// over the bytes and decodes nothing, which is what lets a window read stop early without paying
/// for a parse of everything before it.
#[derive(Default)]
struct RowCloses {
    /// How much of the part has been looked at.
    scanned: usize,
    /// How many closes have been counted.
    closings: u32,
}

impl RowCloses {
    /// The offset just past the close that puts `last_row` behind us, if the bytes so far hold it.
    ///
    /// `prefix` is the part read so far, **not** the bytes that just arrived: offsets already
    /// counted are remembered against it, and passing disjoint pieces instead would lose closes
    /// between them. Bytes already looked at are not looked at again, so feeding a growing prefix
    /// costs one pass over the part in total.
    fn feed(&mut self, prefix: &[u8], last_row: u32) -> Option<usize> {
        let counted = self.scanned;
        // A close can straddle two arrivals, so searching starts just before what was counted and
        // a match is counted only if it ends beyond it.
        let mut at = counted.saturating_sub(ROW_CLOSE.len() - 1);
        while let Some(offset) = find_bytes(&prefix[at..], ROW_CLOSE) {
            at += offset;
            let end = at + ROW_CLOSE.len();
            if end > counted {
                self.closings += 1;
                if self.closings > last_row {
                    // Moving past the close that fired keeps a second call looking for a later one
                    // rather than answering with the same offset again.
                    self.scanned = end;
                    return Some(end);
                }
            }
            at = end;
        }
        self.scanned = prefix.len();
        None
    }
}

/// The offset of `needle` within `hay`.
fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let last = hay.len().checked_sub(needle.len())?;
    let first = needle[0];
    (0..=last).find(|&at| hay[at] == first && hay[at..at + needle.len()] == *needle)
}

/// The offset just past the row a window ends on, if a prefix already holds it.
///
/// Returning the offset rather than a yes or no matters: a prefix arrives in chunks, so it usually
/// ends partway through the following tag, and cutting it there would leave the parser an
/// unterminated element. Cutting at the row's end leaves a document that simply stops.
///
/// The prefix is walked with the same cursor the real read uses, so what counts as a row here is
/// what counts as a row there.
fn prefix_through_row(prefix: &[u8], last_row: u32) -> Option<usize> {
    let mut parser = XmlParser::new(prefix);
    let mut cursor = RowCursor::default();
    loop {
        match parser.next_event() {
            Ok(Event::Start(el)) => match el.local_name() {
                // Structural elements, and nothing to skip.
                "worksheet" | "sheetData" | "dimension" => {}
                "row" => cursor.start_row(el.attr_raw("r")),
                _ => {
                    // A prefix can end in the middle of an element. That is not a fault here: it
                    // means the region asked for has not arrived yet.
                    if parser.skip_element().is_err() {
                        return None;
                    }
                }
            },
            Ok(Event::End(name)) => {
                if local_name(name) == "row" && cursor.row >= last_row {
                    return Some(parser.position());
                }
            }
            Ok(Event::Text(_)) => {}
            Ok(Event::Eof) | Err(_) => return None,
        }
    }
}

/// Tracks which row and column a cell belongs to.
///
/// A cell may omit its reference, in which case it follows the previous one, and a row may omit
/// its index, in which case it follows the previous row. Both the dense and the streaming reader
/// use this, so the two cannot disagree about where a cell sits.
#[derive(Default)]
struct RowCursor {
    row: u32,
    seen_row: bool,
    next_col: u32,
}

impl RowCursor {
    /// Begins a row, taking its index from `r` when it has one.
    fn start_row(&mut self, reference: Option<&str>) {
        self.row = match reference.and_then(|v| v.parse::<u32>().ok()) {
            Some(r) if r >= 1 => r - 1,
            _ => {
                if self.seen_row {
                    self.row.saturating_add(1)
                } else {
                    0
                }
            }
        };
        self.seen_row = true;
        self.next_col = 0;
    }

    /// Places a cell within the current row, taking its column from `r` when it has one.
    fn place(&mut self, reference: Option<&str>) -> u32 {
        let column = reference
            .and_then(parse_cell_ref)
            .map(|(_, c)| c)
            .unwrap_or(self.next_col);
        self.next_col = column.saturating_add(1);
        column
    }
}

/// Streams one worksheet part, one row at a time.
struct XlsxRows<'a> {
    parser: XmlParser<'a>,
    ctx: SheetCtx<'a>,
    cursor: RowCursor,
    /// When set, rows and cells outside it are skipped without being decoded.
    window: Option<Window>,
    exhausted: bool,
}

impl XlsxRows<'_> {
    /// Whether a row's cells are worth decoding.
    fn wants_row(&self, row: u32) -> bool {
        self.window.is_none_or(|window| window.holds_row(row))
    }

    /// Whether a column's value is worth decoding.
    fn wants_column(&self, column: u32) -> bool {
        self.window.is_none_or(|window| window.holds_col(column))
    }
}

impl RowSource for XlsxRows<'_> {
    fn next_row(&mut self, columns: &mut Vec<u32>, values: &mut Vec<Data>) -> Result<Option<u32>> {
        columns.clear();
        values.clear();
        if self.exhausted {
            return Ok(None);
        }
        loop {
            match self.parser.next_event()? {
                Event::Eof => {
                    self.exhausted = true;
                    return Ok(None);
                }
                Event::Start(el) => match el.local_name() {
                    // Structural elements, including `dimension`, which streaming has no use for.
                    "worksheet" | "sheetData" | "dimension" => {}
                    "row" => {
                        self.cursor.start_row(el.attr_raw("r"));
                        // The dense path refuses a cell outside any plausible grid; streaming
                        // reports the same failure rather than handing back a row index the
                        // other view of the sheet calls impossible.
                        if self.cursor.row >= MAX_AXIS {
                            return Err(Error::format(format!(
                                "row {} is outside any spreadsheet grid",
                                self.cursor.row
                            )));
                        }
                        if !self.wants_row(self.cursor.row) {
                            // Skipping the element costs a tag scan. Decoding its cells would mean
                            // parsing a reference, a style index and a value for each one.
                            self.parser.skip_element()?;
                        }
                    }
                    "c" => {
                        let column = self.cursor.place(el.attr_raw("r"));
                        if column >= MAX_AXIS {
                            return Err(Error::format(format!(
                                "cell at ({}, {column}) is outside any spreadsheet grid",
                                self.cursor.row
                            )));
                        }
                        if self.wants_column(column) {
                            let xf = el.attr_raw("s").and_then(|v| v.parse::<usize>().ok());
                            let ty = el.attr_raw("t").unwrap_or("n");
                            let (value, _formula) =
                                parse_cell(&mut self.parser, ty, xf, &self.ctx)?;
                            if !value.is_empty() {
                                columns.push(column);
                                values.push(value);
                            }
                        } else {
                            self.parser.skip_element()?;
                        }
                    }
                    _ => self.parser.skip_element()?,
                },
                // Only the row's own end can arrive here: cells and skipped elements are consumed
                // whole by the arms above.
                Event::End(name) => {
                    if local_name(name) == "row" {
                        return Ok(Some(self.cursor.row));
                    }
                }
                Event::Text(_) => {}
            }
        }
    }
}

/// Parses one worksheet part.
fn parse_sheet(xml: &[u8], ctx: &SheetCtx<'_>, want_formulas: bool) -> Result<ParsedSheet> {
    let mut parser = XmlParser::new(xml);
    let mut out = ParsedSheet::default();
    let mut cursor = RowCursor::default();

    loop {
        match parser.next_event()? {
            Event::Eof => break,
            Event::Start(el) => match el.local_name() {
                // Containers to descend into rather than skip.
                "worksheet" | "sheetData" => {}
                "dimension" => {
                    // The extent is declared before any cell, so sizing the buffer here means the
                    // sheet never has to be held as a sparse list at all.
                    if let Some(extent) = el.attr_raw("ref").and_then(parse_extent) {
                        out.builder = SheetBuilder::new(Some(extent));
                    }
                }
                "row" => cursor.start_row(el.attr_raw("r")),
                "c" => {
                    let column = cursor.place(el.attr_raw("r"));
                    let xf = el.attr_raw("s").and_then(|v| v.parse::<usize>().ok());
                    let ty = el.attr_raw("t").unwrap_or("n");
                    let (value, formula) = parse_cell(&mut parser, ty, xf, ctx)?;
                    if !value.is_empty() && !want_formulas {
                        out.builder.write(cursor.row, column, value)?;
                    }
                    // `kind` is empty when the cell carries no `<f>` at all.
                    if want_formulas && !formula.kind.is_empty() {
                        out.formulas.push((cursor.row, column, formula));
                    }
                }
                _ => parser.skip_element()?,
            },
            Event::End(_) | Event::Text(_) => {}
        }
    }
    Ok(out)
}

/// Reads a single `<c>` element, which the caller has just opened.
fn parse_cell<'a>(
    parser: &mut XmlParser<'a>,
    ty: &str,
    xf: Option<usize>,
    ctx: &SheetCtx<'_>,
) -> Result<(Data, FormulaRecord)> {
    // A cell's numeric value is the overwhelmingly common case and the text is parsed and
    // dropped immediately, so it is borrowed when the bytes allow rather than copied into a
    // `String` per cell.
    let mut raw_value: Option<Cow<'a, str>> = None;
    let mut inline: Option<String> = None;
    let mut formula = FormulaRecord::default();

    loop {
        match parser.next_event()? {
            Event::Start(el) => match el.local_name() {
                "v" => raw_value = Some(parser.inner_text_cow()?),
                "f" => {
                    formula.kind = el.attr_raw("t").unwrap_or("normal").to_owned();
                    formula.si = el.attr_raw("si").and_then(|v| v.parse::<u32>().ok());
                    formula.text = parser.inner_text()?;
                }
                "is" => inline = Some(read_rich_text(parser)?),
                _ => parser.skip_element()?,
            },
            Event::End(_) => break,
            Event::Text(_) => {}
            Event::Eof => return Err(Error::xml("cell is not terminated")),
        }
    }

    let value = raw_value.as_deref().map(str::trim);

    let data = match ty {
        "s" => value
            .and_then(|s| s.parse::<usize>().ok())
            .and_then(|i| ctx.strings.get(i))
            .map(|s| Data::String(s.clone()))
            .unwrap_or(Data::Empty),
        "inlineStr" => inline.map(Data::String).unwrap_or(Data::Empty),
        "str" => match value {
            // The cached result of a formula that returns text.
            Some(s) if !s.is_empty() => Data::String(decode_ooxml_escapes(s).into_owned()),
            _ => Data::Empty,
        },
        "b" => match value {
            Some("1") | Some("true") | Some("TRUE") => Data::Bool(true),
            Some("0") | Some("false") | Some("FALSE") => Data::Bool(false),
            _ => Data::Empty,
        },
        "e" => match value {
            Some(s) if !s.is_empty() => Data::Error(CellError::from_ooxml(s)),
            _ => Data::Empty,
        },
        "d" => match value {
            Some(s) if !s.is_empty() => Data::DateTimeIso(s.to_owned()),
            _ => Data::Empty,
        },
        _ => number_value(value, xf, ctx),
    };

    Ok((data, formula))
}

/// Turns a numeric `<v>` into a value, honouring a date number format.
///
/// The integer path is checked first and parses straight to `i64`. Most numeric cells in a real
/// workbook are whole numbers, and for those this skips both an `f64` parse and the
/// integrality test that would only confirm what the digits already said. `i64` handles every
/// integer Excel can represent exactly: past 2^53 a double cannot, and those values fall
/// through to the `f64` path, which is where a fractional or rounded value belongs.
fn number_value(raw: Option<&str>, xf: Option<usize>, ctx: &SheetCtx<'_>) -> Data {
    let Some(text) = raw else {
        return Data::Empty;
    };
    if text.is_empty() {
        return Data::Empty;
    }

    // An integer written without a decimal point or exponent, which is the common case.
    let bytes = text.as_bytes();
    let (negative, digits) = match bytes.first() {
        Some(b'-') => (true, &bytes[1..]),
        Some(b'+') => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    if !digits.is_empty() && digits.iter().all(u8::is_ascii_digit) {
        // Parsed through `i64` only when it fits; a 19-digit cell overflows and falls through.
        if let Some(mut value) = std::str::from_utf8(digits)
            .ok()
            .and_then(|d| d.parse::<i64>().ok())
        {
            if negative {
                value = -value;
            }
            let as_float = value as f64;
            if ctx.uses_date(xf) {
                return Data::DateTime(ExcelDateTime::new(as_float, ctx.is_1904));
            }
            // Beyond 2^53 an `i64` is not exactly representable as a double, so such a cell is
            // reported as a float: that is what the file's own type can hold.
            if value.unsigned_abs() <= 9_007_199_254_740_992 {
                return Data::Int(value);
            }
            return Data::Float(as_float);
        }
    }

    let Ok(f) = text.parse::<f64>() else {
        // Some producers leave an error string in a cell typed as numeric.
        return if text.starts_with('#') {
            Data::Error(CellError::from_ooxml(text))
        } else {
            Data::Empty
        };
    };
    if ctx.uses_date(xf) {
        return Data::DateTime(ExcelDateTime::new(f, ctx.is_1904));
    }
    // Excel has no integer type, so an integral value is reported as an integer. That is what
    // a reader wants for an ID or a count column, and loses no information: the file stores
    // `2` and `2.0` identically.
    if f.is_finite() && f == f.trunc() && f.abs() <= 9_007_199_254_740_992.0 {
        Data::Int(f as i64)
    } else {
        Data::Float(f)
    }
}

/// Reads `<is>` or `<si>` content, joining rich-text runs and skipping phonetic hints.
fn read_rich_text(parser: &mut XmlParser<'_>) -> Result<String> {
    let mut out = String::new();
    let mut depth = 1usize;
    loop {
        match parser.next_event()? {
            Event::Start(el) => match el.local_name() {
                "t" => {
                    let text = parser.inner_text()?;
                    out.push_str(&decode_ooxml_escapes(&text));
                }
                // Phonetic guide text is metadata, not cell content.
                "rPh" => parser.skip_element()?,
                _ => depth += 1,
            },
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    return Ok(out);
                }
            }
            Event::Text(_) => {}
            Event::Eof => return Err(Error::xml("string is not terminated")),
        }
    }
}

/// Parses the shared string table.
fn parse_shared_strings(xml: &[u8]) -> Result<Vec<String>> {
    let mut parser = XmlParser::new(xml);
    let mut out = Vec::new();
    loop {
        match parser.next_event()? {
            Event::Eof => break,
            Event::Start(el) => match el.local_name() {
                "sst" => {
                    if let Some(n) = el
                        .attr_raw("uniqueCount")
                        .and_then(|v| v.parse::<usize>().ok())
                    {
                        // Reserved in bounded chunks so a bogus count cannot allocate wildly.
                        out.reserve(n.min(4_000_000));
                    }
                }
                "si" => out.push(read_rich_text(&mut parser)?),
                _ => parser.skip_element()?,
            },
            Event::End(_) | Event::Text(_) => {}
        }
    }
    Ok(out)
}

/// What the workbook part declares.
struct WorkbookMeta {
    is_1904: bool,
    sheets: Vec<RawSheet>,
    defined: Vec<(String, String)>,
}

/// Parses the workbook part for the date system, sheet list and defined names.
fn parse_workbook(xml: &[u8]) -> Result<WorkbookMeta> {
    let mut parser = XmlParser::new(xml);
    let mut is_1904 = false;
    let mut sheets = Vec::new();
    let mut defined = Vec::new();

    loop {
        match parser.next_event()? {
            Event::Eof => break,
            Event::Start(el) => match el.local_name() {
                "workbook" | "sheets" | "definedNames" => {}
                "workbookPr" => {
                    is_1904 = matches!(el.attr_raw("date1904"), Some("1") | Some("true"));
                }
                "sheet" => {
                    let name = el.attr("name").map(|c| c.into_owned()).unwrap_or_default();
                    let rid = el
                        .attr_raw("r:id")
                        .map(str::to_owned)
                        .or_else(|| el.attr_local("id").map(|c| c.into_owned()));
                    let sheet_id = el
                        .attr_raw("sheetId")
                        .and_then(|v| v.parse::<u32>().ok())
                        .unwrap_or(sheets.len() as u32 + 1);
                    sheets.push(RawSheet {
                        name,
                        rid,
                        sheet_id,
                        visibility: SheetVisibility::from_state(el.attr_raw("state")),
                    });
                }
                "definedName" => {
                    let name = el.attr("name").map(|c| c.into_owned()).unwrap_or_default();
                    let text = parser.inner_text()?;
                    if !name.is_empty() {
                        defined.push((name, text));
                    }
                }
                _ => parser.skip_element()?,
            },
            Event::End(_) | Event::Text(_) => {}
        }
    }
    Ok(WorkbookMeta {
        is_1904,
        sheets,
        defined,
    })
}

/// Parses a relationship part into `id -> (target, type)`.
fn parse_rels(xml: &[u8]) -> Result<HashMap<String, (String, String)>> {
    let mut parser = XmlParser::new(xml);
    let mut out = HashMap::new();
    loop {
        match parser.next_event()? {
            Event::Eof => break,
            Event::Start(el) => match el.local_name() {
                "Relationships" => {}
                "Relationship" => {
                    if let (Some(id), Some(target)) = (el.attr_raw("Id"), el.attr("Target")) {
                        let kind = el.attr_string("Type").unwrap_or_default();
                        out.insert(id.to_owned(), (target.into_owned(), kind));
                    }
                }
                _ => parser.skip_element()?,
            },
            Event::End(_) | Event::Text(_) => {}
        }
    }
    Ok(out)
}

/// Parses the styles part, recording which cell formats render as dates.
fn parse_styles(xml: &[u8]) -> Result<Styles> {
    let mut parser = XmlParser::new(xml);
    let mut custom: HashMap<u32, String> = HashMap::new();
    let mut date_xf = Vec::new();
    let mut in_cell_xfs = false;

    loop {
        match parser.next_event()? {
            Event::Eof => break,
            Event::Start(el) => match el.local_name() {
                "styleSheet" | "numFmts" => {}
                "cellXfs" => in_cell_xfs = true,
                // `numFmts` precedes `cellXfs` in the schema, so the custom codes are already
                // known by the time the formats are classified.
                "numFmt" => {
                    if let (Some(id), Some(code)) = (el.attr_raw("numFmtId"), el.attr("formatCode"))
                    {
                        if let Ok(id) = id.parse::<u32>() {
                            custom.insert(id, code.into_owned());
                        }
                    }
                }
                "xf" if in_cell_xfs => {
                    let id = el
                        .attr_raw("numFmtId")
                        .and_then(|v| v.parse::<u32>().ok())
                        .unwrap_or(0);
                    let code = custom.get(&id).map(String::as_str);
                    date_xf.push(format_is_date(id.min(u32::from(u16::MAX)) as u16, code));
                }
                _ => parser.skip_element()?,
            },
            Event::End(el) => {
                if local_name(el) == "cellXfs" {
                    in_cell_xfs = false;
                }
            }
            Event::Text(_) => {}
        }
    }
    Ok(Styles { date_xf })
}

/// Turns a relationship target into an absolute part path inside the package.
fn resolve_part(target: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in target.split(['/', '\\']) {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    let joined = segments.join("/");
    if joined.starts_with("xl/") {
        joined
    } else {
        format!("xl/{joined}")
    }
}

/// Fills in the cells that share a formula instead of repeating its text.
///
/// Excel writes a filled-down column's formula once, on the group's master cell, and stores an
/// offset on every other member. Without this pass a reader would see a blank formula on nearly
/// every cell of such a column.
fn expand_shared_formulas(records: &[(u32, u32, FormulaRecord)]) -> Vec<(u32, u32, String)> {
    // First pass, so a member is satisfied even if it appears before its master.
    let mut masters: HashMap<u32, (u32, u32, String)> = HashMap::new();
    for (row, col, record) in records {
        if record.kind == "shared" && !record.text.is_empty() {
            if let Some(si) = record.si {
                masters
                    .entry(si)
                    .or_insert_with(|| (*row, *col, record.text.clone()));
            }
        }
    }

    let mut out = Vec::with_capacity(records.len());
    for (row, col, record) in records {
        let text = match record.kind.as_str() {
            "shared" if record.text.is_empty() => match record.si.and_then(|si| masters.get(&si)) {
                Some((anchor_row, anchor_col, master)) => shift_formula(
                    master,
                    i64::from(*row) - i64::from(*anchor_row),
                    i64::from(*col) - i64::from(*anchor_col),
                ),
                None => continue,
            },
            _ => record.text.clone(),
        };
        if !text.is_empty() {
            out.push((*row, *col, text));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Builds the smallest workbook that reaches the sheet reader: a workbook part naming one
    /// sheet, and that sheet. No relationships and no shared strings, so the reader falls back to
    /// the conventional part path and reads its cells directly.
    fn package(sheet: &str) -> Vec<u8> {
        let workbook = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheets><sheet name="S" sheetId="1"/></sheets>
</workbook>"#;
        let stored = zipcore::FileOptions::stored();
        let mut writer = zipcore::ZipWriter::new(Cursor::new(Vec::new())).unwrap();
        for (name, body) in [
            ("xl/workbook.xml", workbook),
            ("xl/worksheets/sheet1.xml", sheet),
        ] {
            writer.add_file(name, body.as_bytes(), &stored).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    /// Two rows by two columns, with whatever extent is given.
    fn sheet_with_dimension(dimension: Option<&str>) -> String {
        let dimension = match dimension {
            Some(reference) => format!(r#"<dimension ref="{reference}"/>"#),
            None => String::new(),
        };
        format!(
            r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  {dimension}
  <sheetData>
    <row r="1"><c r="A1"><v>1</v></c><c r="B1"><v>2</v></c></row>
    <row r="2"><c r="A2"><v>3</v></c><c r="B2"><v>4</v></c></row>
  </sheetData>
</worksheet>"#
        )
    }

    fn read(dimension: Option<&str>) -> Range<Data> {
        let bytes = package(&sheet_with_dimension(dimension));
        let mut workbook = Xlsx::new(Cursor::new(bytes)).unwrap();
        workbook.worksheet_range("S").unwrap()
    }

    fn expected() -> Vec<Data> {
        vec![Data::Int(1), Data::Int(2), Data::Int(3), Data::Int(4)]
    }

    #[test]
    fn reads_extents() {
        assert_eq!(
            parse_extent("A1:U389"),
            Some(Extent {
                origin: (0, 0),
                rows: 389,
                cols: 21
            })
        );
        assert_eq!(
            parse_extent("B2"),
            Some(Extent {
                origin: (1, 1),
                rows: 1,
                cols: 1
            })
        );
        // Anything else is ignored rather than rejected: the element is only a hint.
        assert_eq!(parse_extent("garbage"), None);
        assert_eq!(parse_extent(""), None);
        assert_eq!(parse_extent("1:2"), None);
    }

    /// The declared extent sizes the dense buffer, so the value has to be right whichever way the
    /// declaration is wrong. A stale extent is common in real files.
    #[test]
    fn a_declared_extent_never_changes_what_is_read() {
        for dimension in [
            // Exact, so the buffer is allocated once and nothing is trimmed.
            Some("A1:B2"),
            // Absent, so the buffer starts small and grows.
            None,
            // Too small, so growth must not drop the cells that fall outside it.
            Some("A1"),
            // Too large, so the buffer must be trimmed back to what was written.
            Some("A1:Z99"),
            // An origin past the cells, so leading rows and columns have to be trimmed away.
            Some("C3:Z99"),
        ] {
            let range = read(dimension);
            assert_eq!(
                range.get_size(),
                (2, 2),
                "dimension {dimension:?} changed the shape"
            );
            assert_eq!(range.as_slice(), expected(), "dimension {dimension:?}");
        }
    }

    /// The search is hand-rolled, so its edges are pinned: a needle longer than the haystack, one
    /// at either end, and a repeated first byte that must not stop the search early.
    #[test]
    fn the_byte_search_finds_what_it_should() {
        assert_eq!(find_bytes(b"abcd</row>", ROW_CLOSE), Some(4));
        assert_eq!(find_bytes(b"</row>", ROW_CLOSE), Some(0));
        assert_eq!(find_bytes(b"</ro", ROW_CLOSE), None);
        assert_eq!(find_bytes(b"", ROW_CLOSE), None);
        assert_eq!(find_bytes(b"<</row>", ROW_CLOSE), Some(1));
        assert_eq!(find_bytes(b"</row></row>", ROW_CLOSE), Some(0));
    }

    /// A close split across two arrivals must be counted exactly once — neither lost nor repeated.
    /// Chunking is what creates the case, and a sheet only reaches it when it is larger than one
    /// chunk, so it is pinned here rather than left to a file that happens to be big enough.
    #[test]
    fn a_row_close_split_across_arrivals_is_counted_once() {
        let text = b"</row></row></row>";
        for split in 0..=text.len() {
            // The prefix grows, which is how the reader feeds it: every byte that was handed over
            // before arrives again with more after it, and none of them may be counted twice.
            assert!(RowCloses::default()
                .feed(&text[..split], u32::MAX)
                .is_none());

            let mut closes = RowCloses::default();
            closes.feed(&text[..split], u32::MAX);
            closes.feed(text, u32::MAX);
            assert_eq!(closes.closings, 3, "split at {split} miscounted");

            // The third close is the one that puts row 2 behind us and it ends the text, so that is
            // where the cut must land whichever arrival carried it.
            let mut closes = RowCloses::default();
            let end = closes
                .feed(&text[..split], 2)
                .or_else(|| closes.feed(text, 2))
                .unwrap_or_else(|| panic!("split at {split} never fired"));
            assert_eq!(end, text.len(), "split at {split} cut in the wrong place");
        }
    }
}

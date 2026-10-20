//! OpenDocument spreadsheet (`.ods`) reading.
//!
//! An ODS package keeps the whole document in `content.xml`, so that part is decompressed once
//! and held, then walked to find the requested table. Walking matters more than it looks:
//! OpenDocument expresses empty padding with repeat counts rather than repeated elements, so a
//! row of three filled cells followed by a thousand empty ones is a handful of bytes. Those
//! repeats are resolved into positions without materialising the empty cells.

use crate::container::{Archive, ZipContainer};
use crate::datatype::Data;
use crate::error::{Error, Result};
use crate::range::{HeapPayload, Range, MAX_PAYLOAD_BYTES};
use crate::reader::{apply_header_row, HeaderRow, MergedRegion, Reader, SheetVisibility};
use crate::rows::{read_window, skip_header_row, RowSource, SheetRows, Window};
use crate::xml::{local_name, Element, Event, XmlParser};
use std::io::{Read, Seek};

/// Excel's grid limits, used to bound what a repeat count can expand to.
const MAX_ROWS: u32 = 1_048_576;
const MAX_COLS: u32 = 16_384;
/// Ceiling on the cells one table may expand to before the range builder sees them.
///
/// This is the **dense** budget: it bounds the sparse list that `worksheet_range` builds before
/// converting it to a rectangle. The streaming path does not use it, and deliberately so — it
/// materialises nothing, so the only bound it needs is the width of a single row ([`MAX_COLS`],
/// enforced in `parse_row`). A file whose table would exceed this limit is therefore refused by
/// `worksheet_range` and read successfully by `worksheet_rows`. That is the intended difference
/// between the two views, not an inconsistency between them: one asks for a rectangle, the other
/// does not.
const MAX_CELLS: usize = 16_777_216;

/// A reader for OpenDocument spreadsheets.
pub struct Ods<RS: Read + Seek> {
    archive: ZipContainer<RS>,
    names: Vec<String>,
    /// Visibility per sheet, parallel to `names`.
    visibilities: Vec<SheetVisibility>,
    defined: Vec<(String, String)>,
    /// The decompressed content part, reused across sheets.
    content: Option<Vec<u8>>,
    header: HeaderRow,
}

impl<RS: Read + Seek> Ods<RS> {
    /// Streams one table, skipping anything outside `window` when there is one.
    fn rows_with(&mut self, name: &str, window: Option<Window>) -> Result<SheetRows<'_>> {
        if !self.names.iter().any(|n| n == name) {
            return Err(Error::NotFound(format!("sheet {name:?}")));
        }
        // Read before borrowing the content: the borrow lasts as long as the cursor does.
        let header = self.header;
        let content = self.content()?;
        let mut rows = SheetRows::new(OdsRows {
            parser: XmlParser::new(content),
            // Owned rather than borrowed: the cursor outlives this call, and a sheet name is a
            // small thing to copy once.
            wanted: name.to_owned(),
            inside: false,
            table_depth: 0,
            index: 0,
            buffered: Vec::new(),
            pending: 0,
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

    fn content(&mut self) -> Result<&[u8]> {
        if self.content.is_none() {
            let bytes = self
                .archive
                .read("content.xml")?
                .ok_or_else(|| Error::missing("content.xml"))?;
            self.content = Some(bytes);
        }
        Ok(self.content.as_deref().unwrap_or(&[]))
    }
}

impl<RS: Read + Seek> Reader<RS> for Ods<RS> {
    fn new(reader: RS) -> Result<Self> {
        let mut archive = ZipContainer::new(reader)?;
        let content = archive
            .read("content.xml")?
            .ok_or_else(|| Error::missing("content.xml"))?;
        let (names, visibilities, defined) = scan_tables(&content)?;
        Ok(Ods {
            archive,
            names,
            visibilities,
            defined,
            content: Some(content),
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
        if !self.names.iter().any(|n| n == name) {
            return Err(Error::NotFound(format!("sheet {name:?}")));
        }
        let parsed = {
            let content = self.content()?;
            parse_table(content, name, false)?
        };
        Ok(apply_header_row(
            Range::from_sparse(parsed.cells)?,
            self.header,
        ))
    }

    fn worksheet_rows(&mut self, name: &str) -> Result<SheetRows<'_>> {
        self.rows_with(name, None)
    }

    /// Reads a window, telling the parser about it so rows outside it are skipped entirely.
    fn worksheet_range_ref(&mut self, name: &str, reference: &str) -> Result<Range<Data>> {
        let window = Window::parse(reference)?;
        let mut rows = self.rows_with(name, Some(window))?;
        read_window(&mut rows, window)
    }

    fn worksheet_formula(&mut self, name: &str) -> Result<Range<String>> {
        if !self.names.iter().any(|n| n == name) {
            return Err(Error::NotFound(format!("sheet {name:?}")));
        }
        let parsed = {
            let content = self.content()?;
            parse_table(content, name, true)?
        };
        Ok(apply_header_row(
            range_of_strings(parsed.formulas)?,
            self.header,
        ))
    }

    /// Collects `table:number-columns-spanned` / `table:number-rows-spanned` regions.
    ///
    /// OpenDocument does not have a separate merge record: a merge is expressed on its anchor
    /// cell by the two span attributes, and the cells it covers appear as
    /// `table:covered-table-cell`. So the regions have to be found while walking the rows,
    /// which is why this shares the table walk rather than reading a separate element.
    fn worksheet_merged_regions(&mut self, name: &str) -> Result<Vec<MergedRegion>> {
        if !self.names.iter().any(|n| n == name) {
            return Err(Error::NotFound(format!("sheet {name:?}")));
        }
        let content = self.content()?;
        Ok(scan_merged_regions(content, name))
    }
}

/// Walks one table and collects the regions its span attributes declare.
fn scan_merged_regions(xml: &[u8], wanted: &str) -> Vec<MergedRegion> {
    let mut parser = XmlParser::new(xml);
    let mut out = Vec::new();
    let mut inside = false;
    let mut table_depth = 0usize;
    let mut row: u32 = 0;
    let mut col: u32 = 0;

    loop {
        match parser.next_event() {
            Ok(Event::Start(el)) => {
                let local = el.local_name();
                if !inside {
                    if local == "table" {
                        let name = el
                            .attr_local("name")
                            .map(|c| c.into_owned())
                            .unwrap_or_default();
                        if name == wanted {
                            inside = true;
                            table_depth = 1;
                        } else if parser.skip_element().is_err() {
                            break;
                        }
                    }
                    continue;
                }
                match local {
                    "table" => table_depth += 1,
                    "table-row" => {
                        // The row index advances *after* the row's cells are recorded, so a
                        // span recorded here is anchored at this row rather than the next one.
                        col = 0;
                    }
                    "table-cell" | "covered-table-cell" => {
                        let span_cols = read_repeat(&el, "number-columns-spanned", MAX_COLS);
                        let span_rows = read_repeat(&el, "number-rows-spanned", MAX_ROWS);
                        if span_cols > 1 || span_rows > 1 {
                            out.push(MergedRegion::new(
                                (row, col),
                                (
                                    row.saturating_add(span_rows - 1),
                                    col.saturating_add(span_cols - 1),
                                ),
                            ));
                        }
                        let repeat = read_repeat(&el, "number-columns-repeated", MAX_COLS);
                        col = col.saturating_add(repeat);
                        if parser.skip_element().is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(el)) => {
                let local = crate::xml::local_name(el);
                if inside && local == "table" {
                    table_depth = table_depth.saturating_sub(1);
                    if table_depth == 0 {
                        break;
                    }
                } else if inside && local == "table-row" {
                    // Close of a row: the next one starts below it.
                    row = row.saturating_add(1);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            Ok(Event::Text(_)) => {}
        }
    }
    out
}

/// A table's name and visibility, plus the defined names declared alongside it.
type TableScan = (Vec<String>, Vec<SheetVisibility>, Vec<(String, String)>);

/// One row's filled cells, and its formulas when they were requested.
type RowCells = (Vec<(u32, Data)>, Vec<(u32, String)>);

/// Streams one table, one row at a time.
///
/// A repeated row is re-emitted from the buffered cells rather than copied into a sheet, so a row
/// the file declares a million times over costs one row of memory and a million cheap
/// iterations. The dense path has to reject that same row, because it would have to materialise
/// every copy.
struct OdsRows<'a> {
    parser: XmlParser<'a>,
    wanted: String,
    /// Whether the wanted table has been found.
    inside: bool,
    table_depth: usize,
    /// The index the next yielded row will carry.
    index: u32,
    /// Cells of the row being repeated.
    buffered: Vec<(u32, Data)>,
    /// Copies of `buffered` still to emit after the current one.
    pending: u32,
    /// When set, rows and cells outside it are skipped.
    window: Option<Window>,
    exhausted: bool,
}

impl OdsRows<'_> {
    /// Copies the buffered row's cells into the caller's buffers.
    fn emit_buffered(&self, columns: &mut Vec<u32>, values: &mut Vec<Data>) {
        for (column, value) in &self.buffered {
            if self.wants_column(*column) {
                columns.push(*column);
                values.push(value.clone());
            }
        }
    }

    /// Whether a column's value is wanted, given the window if there is one.
    fn wants_column(&self, column: u32) -> bool {
        self.window.is_none_or(|window| window.holds_col(column))
    }
}

impl RowSource for OdsRows<'_> {
    fn next_row(&mut self, columns: &mut Vec<u32>, values: &mut Vec<Data>) -> Result<Option<u32>> {
        columns.clear();
        values.clear();

        // Outstanding copies of the row already parsed.
        if self.pending > 0 {
            self.pending -= 1;
            self.emit_buffered(columns, values);
            let index = self.index;
            self.index += 1;
            return Ok(Some(index));
        }
        if self.exhausted {
            return Ok(None);
        }

        loop {
            match self.parser.next_event()? {
                Event::Eof => {
                    self.exhausted = true;
                    return Ok(None);
                }
                Event::Start(el) => {
                    let local = el.local_name();
                    if !self.inside {
                        if local == "table" {
                            let name = el
                                .attr_local("name")
                                .map(|c| c.into_owned())
                                .unwrap_or_default();
                            if name == self.wanted {
                                self.inside = true;
                                self.table_depth = 1;
                            } else {
                                self.parser.skip_element()?;
                            }
                        }
                        // Everything before the table is small, so descending is cheap.
                        continue;
                    }
                    match local {
                        "table" => self.table_depth += 1,
                        "table-row" => {
                            let repeated = read_repeat(&el, "number-rows-repeated", MAX_ROWS);
                            let last = self.index.saturating_add(repeated - 1);
                            if let Some(window) = self.window {
                                if window.ends_before(self.index) {
                                    // This row and everything after it is past the window.
                                    self.exhausted = true;
                                    return Ok(None);
                                }
                                if last < window.rows.0 {
                                    // The whole run sits before the window. Padding runs like this
                                    // are why skipping is worth having: the alternative is parsing
                                    // a row that is about to be discarded, a million times over.
                                    self.parser.skip_element()?;
                                    self.index = last.saturating_add(1);
                                    continue;
                                }
                            }

                            // A per-row budget, not a per-sheet one: nothing is materialised
                            // here, so only a single row needs bounding. That row can still be
                            // enormous — the grid is 16,384 cells wide and a cell's text may be
                            // a megabyte — so the payload budget applies to it as well, reset
                            // for each row because the previous one has been handed out and
                            // dropped by the time the next is read.
                            let mut remaining = MAX_CELLS;
                            let mut row_payload = 0u64;
                            let (cells, _) = parse_row(
                                &mut self.parser,
                                false,
                                &mut remaining,
                                &mut row_payload,
                            )?;

                            if repeated == 1 {
                                // The common case: hand the cells straight over.
                                for (column, value) in cells {
                                    if self.wants_column(column) {
                                        columns.push(column);
                                        values.push(value);
                                    }
                                }
                            } else {
                                self.buffered = cells;
                                self.pending = repeated - 1;
                                self.emit_buffered(columns, values);
                            }
                            // One row is emitted per call, so the counter advances by one here and
                            // by one for each outstanding copy. Advancing by the whole repeat now
                            // would leave the copies numbered past the end of their own run.
                            let index = self.index;
                            self.index = self.index.saturating_add(1);
                            return Ok(Some(index));
                        }
                        _ => {}
                    }
                }
                Event::End(el) => {
                    if self.inside && local_name(el) == "table" {
                        self.table_depth = self.table_depth.saturating_sub(1);
                        if self.table_depth == 0 {
                            self.exhausted = true;
                            return Ok(None);
                        }
                    }
                }
                Event::Text(_) => {}
            }
        }
    }
}

/// Collects table names and defined names from the content part.
///
/// Table bodies are skipped rather than descended into. OpenDocument puts defined names after
/// the tables, at the spreadsheet level, so nothing inside a table is needed here — and a table
/// body is the overwhelming bulk of the file. Walking it would make opening a workbook cost as
/// much as reading one.
fn scan_tables(xml: &[u8]) -> Result<TableScan> {
    let mut parser = XmlParser::new(xml);
    let mut names = Vec::new();
    let mut visibilities = Vec::new();
    let mut defined = Vec::new();

    loop {
        match parser.next_event()? {
            Event::Eof => break,
            Event::Start(el) => match el.local_name() {
                "table" => {
                    let name = el
                        .attr_local("name")
                        .map(|c| c.into_owned())
                        .unwrap_or_default();
                    visibilities.push(SheetVisibility::from_oodo(
                        el.attr_local("visibility").as_deref(),
                    ));
                    names.push(name);
                    parser.skip_element()?;
                }
                "named-range" => {
                    if let (Some(name), Some(range)) =
                        (el.attr_local("name"), el.attr_local("cell-range-address"))
                    {
                        defined.push((name.into_owned(), range.into_owned()));
                    }
                }
                _ => {}
            },
            Event::End(_) | Event::Text(_) => {}
        }
    }
    Ok((names, visibilities, defined))
}

#[derive(Default)]
struct ParsedTable {
    cells: Vec<(u32, u32, Data)>,
    formulas: Vec<(u32, u32, String)>,
}

/// Walks the content part and materialises one named table.
fn parse_table(xml: &[u8], wanted: &str, want_formulas: bool) -> Result<ParsedTable> {
    let mut parser = XmlParser::new(xml);
    let mut out = ParsedTable::default();
    let mut inside = false;
    let mut table_depth = 0usize;
    let mut row: u32 = 0;
    // Cell budget left for this table, spent as cells are produced.
    let mut remaining = MAX_CELLS;
    // Heap bytes of cell text produced so far, charged against `MAX_PAYLOAD_BYTES`.
    //
    // ODS builds its range through `Range::from_sparse` rather than `SheetBuilder`, so the
    // payload budget that the builder enforces does not apply here unless it is applied
    // explicitly. It has to be: a repeat count multiplies text as well as cells.
    let mut payload_bytes: u64 = 0;

    loop {
        match parser.next_event()? {
            Event::Eof => break,
            Event::Start(el) => {
                let local = el.local_name();
                if !inside {
                    if local == "table" {
                        let name = el
                            .attr_local("name")
                            .map(|c| c.into_owned())
                            .unwrap_or_default();
                        if name == wanted {
                            inside = true;
                            table_depth = 1;
                        } else {
                            parser.skip_element()?;
                        }
                    }
                    // Everything before the table — settings, styles, font declarations — is
                    // small, so descending is cheap and avoids hard-coding the container chain.
                    continue;
                }

                match local {
                    "table" => table_depth += 1,
                    "table-row" => {
                        let repeat = read_repeat(&el, "number-rows-repeated", MAX_ROWS);
                        // A repeat that starts beyond the grid cannot name a real row, and a
                        // file claiming one is malformed rather than merely large. Checking here
                        // keeps the row arithmetic below from wrapping.
                        if row.saturating_add(repeat) > MAX_ROWS {
                            return Err(Error::format(format!(
                                "table {wanted:?} declares rows past the {MAX_ROWS} row limit"
                            )));
                        }
                        let (cells, formulas) = parse_row(
                            &mut parser,
                            want_formulas,
                            &mut remaining,
                            &mut payload_bytes,
                        )?;
                        // The row's own cells were charged as they were produced. Repeating the
                        // row multiplies them, so charge the extra copies before the loop below
                        // rather than after it, so the multiplication cannot allocate first.
                        let extra = cells
                            .len()
                            .saturating_mul(repeat.saturating_sub(1) as usize);
                        remaining = remaining.checked_sub(extra).ok_or_else(|| {
                            Error::format(format!(
                                "table {wanted:?} expands past the {MAX_CELLS} cell limit"
                            ))
                        })?;
                        // Cell *count* is not the only thing a repeat multiplies: each copy clones
                        // its cell's text, so a repeat is also a payload multiplier. Charging the
                        // count alone let a 629-byte file expand to a gigabyte of text, because
                        // this path never goes through `SheetBuilder`, which is where the payload
                        // budget is otherwise applied.
                        let payload: u64 = cells
                            .iter()
                            .map(|(_, value)| value.heap_payload())
                            .sum::<u64>()
                            .saturating_mul(u64::from(repeat));
                        payload_bytes = payload_bytes.saturating_add(payload);
                        if payload_bytes > MAX_PAYLOAD_BYTES {
                            return Err(Error::format(format!(
                                "table {wanted:?} cell text exceeds the \
                                 {MAX_PAYLOAD_BYTES} byte limit"
                            )));
                        }
                        // A repeat of one is the ordinary case and needs no copy at all: the
                        // parsed cells are moved into place. Only a genuine repeat has to
                        // duplicate them, and that is the rarer shape.
                        if repeat == 1 {
                            out.cells.reserve(cells.len());
                            for (col, value) in cells {
                                out.cells.push((row, col, value));
                            }
                        } else {
                            out.cells.reserve(cells.len() * repeat as usize);
                            for offset in 0..repeat {
                                // `row + repeat` was bounded above, so this cannot overflow.
                                for (col, value) in &cells {
                                    out.cells.push((row + offset, *col, value.clone()));
                                }
                            }
                        }
                        if want_formulas {
                            for (col, formula) in formulas {
                                out.formulas.push((row, col, formula));
                            }
                        }
                        row += repeat;
                    }
                    _ => {}
                }
            }
            Event::End(el) => {
                if inside && crate::xml::local_name(el) == "table" {
                    table_depth = table_depth.saturating_sub(1);
                    if table_depth == 0 {
                        return Ok(out);
                    }
                }
            }
            Event::Text(_) => {}
        }
    }
    if inside {
        Ok(out)
    } else {
        Err(Error::NotFound(format!("table {wanted:?}")))
    }
}

/// Reads one `<table:table-row>`, returning its filled cells and formulas.
///
/// `remaining` is the table's leftover cell budget, spent as cells are produced. OpenDocument
/// expresses padding as a repeat count, so a few hundred bytes of markup can stand for millions
/// of cells; charging the budget here, as each cell is pushed, is what stops that from becoming
/// an allocation the process cannot satisfy. Checking only once the row was built would be too
/// late to help.
fn parse_row(
    parser: &mut XmlParser<'_>,
    want_formulas: bool,
    remaining: &mut usize,
    payload_bytes: &mut u64,
) -> Result<RowCells> {
    let mut cells = Vec::new();
    let mut formulas = Vec::new();
    let mut col: u32 = 0;

    loop {
        match parser.next_event()? {
            Event::Eof => return Err(Error::xml("row is not terminated")),
            Event::Start(el) => match el.local_name() {
                "table-cell" | "covered-table-cell" => {
                    let repeat = read_repeat(&el, "number-columns-repeated", MAX_COLS);
                    // The column cursor never runs past the grid: a run that would is a
                    // malformed file, not a wide sheet, and letting it advance would place the
                    // value outside any addressable cell.
                    if col.saturating_add(repeat) > MAX_COLS {
                        return Err(Error::format(format!(
                            "row declares columns past the {MAX_COLS} column limit"
                        )));
                    }
                    let start_col = col;
                    let (value, formula) = read_cell(parser, &el)?;
                    col += repeat;
                    if !value.is_empty() {
                        let count = repeat as usize;
                        // A row cannot hold more filled cells than the grid is wide, so this is
                        // the tightest bound available and it is checked before anything is
                        // pushed. Without it a row of a thousand repeated cells would run the
                        // table's whole budget before the earlier check could object.
                        if cells.len() + count > MAX_COLS as usize {
                            return Err(Error::format(format!(
                                "row expands past the {MAX_COLS} column limit"
                            )));
                        }
                        *remaining = remaining.checked_sub(count).ok_or_else(|| {
                            Error::format(format!("table expands past the {MAX_CELLS} cell limit"))
                        })?;
                        // The column repeat clones this cell's text, so it multiplies payload as
                        // well as cell count. Charged here because this is the loop that
                        // duplicates, and a 64 KiB cell repeated 16,384 times is a gigabyte.
                        let payload = value.heap_payload().saturating_mul(u64::from(repeat));
                        *payload_bytes = payload_bytes.saturating_add(payload);
                        if *payload_bytes > MAX_PAYLOAD_BYTES {
                            return Err(Error::format(format!(
                                "table cell text exceeds the {MAX_PAYLOAD_BYTES} byte limit"
                            )));
                        }
                        for c in start_col..start_col.saturating_add(repeat) {
                            cells.push((c, value.clone()));
                        }
                    }
                    if want_formulas && !formula.is_empty() {
                        formulas.push((start_col, formula));
                    }
                }
                _ => parser.skip_element()?,
            },
            Event::End(_) => return Ok((cells, formulas)),
            Event::Text(_) => {}
        }
    }
}

/// Reads a `<table:table-cell>`, gathering its paragraph text.
///
/// A cell's attributes are read by borrowing: a numeric cell carries no text, so copying
/// `value-type` into a `String` per cell would be one allocation for a value that is about to
/// be matched against a `&str` and dropped.
fn read_cell(parser: &mut XmlParser<'_>, el: &Element<'_>) -> Result<(Data, String)> {
    let value_type = el.attr_local("value-type");
    let formula = match el.attr_local("formula") {
        Some(raw) => normalise_formula(&raw),
        None => String::new(),
    };

    // Most cells hold no paragraph at all; the vector is only built once one is found.
    let mut paragraphs: Vec<String> = Vec::new();
    loop {
        match parser.next_event()? {
            Event::Eof => return Err(Error::xml("cell is not terminated")),
            Event::Start(child) => match child.local_name() {
                // Headings appear in cells too, and read like paragraphs.
                "p" | "h" => paragraphs.push(read_text_container(parser)?),
                // A list inside a cell is text, not decoration: producers that write a
                // multi-line cell as a list would otherwise lose it entirely. Each item's
                // paragraphs become lines of the cell.
                "list" => read_list_paragraphs(parser, &mut paragraphs)?,
                // A comment attached to the cell is not cell content.
                "annotation" => parser.skip_element()?,
                _ => parser.skip_element()?,
            },
            Event::End(_) => break,
            Event::Text(_) => {}
        }
    }

    // The overwhelmingly common cell holds no paragraph, so the join is skipped rather than
    // building an empty `String` through it.
    let text = match paragraphs.len() {
        0 => String::new(),
        1 => paragraphs.pop().unwrap_or_default(),
        _ => paragraphs.join("\n"),
    };
    Ok((cell_value(el, value_type.as_deref(), text), formula))
}

/// Collects the paragraphs of a `<text:list>`, appending them to `out`.
///
/// A list item holds one or more paragraphs; nested lists are flattened the same way rather than
/// dropped, because the cell's text is the only thing being recovered here — the list structure
/// itself is presentation, which this reader does not model.
fn read_list_paragraphs(parser: &mut XmlParser<'_>, out: &mut Vec<String>) -> Result<()> {
    let mut depth = 1usize;
    loop {
        match parser.next_event()? {
            Event::Eof => return Err(Error::xml("list is not terminated")),
            Event::Start(el) => match el.local_name() {
                "p" | "h" => out.push(read_text_container(parser)?),
                // Descend into items and nested lists so their paragraphs are found.
                "list" | "list-item" | "list-header" => depth += 1,
                "annotation" => parser.skip_element()?,
                _ => parser.skip_element()?,
            },
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    return Ok(());
                }
            }
            Event::Text(_) => {}
        }
    }
}

/// Turns a cell's declared value type into a value.
fn cell_value(el: &Element<'_>, value_type: Option<&str>, text: String) -> Data {
    match value_type {
        Some("float" | "percentage" | "currency") => {
            match el.attr_local("value").and_then(|v| v.parse::<f64>().ok()) {
                Some(f)
                    if f.is_finite() && f == f.trunc() && f.abs() <= 9_007_199_254_740_992.0 =>
                {
                    Data::Int(f as i64)
                }
                Some(f) => Data::Float(f),
                // A number with no `office:value` still has its rendered text.
                None if text.is_empty() => Data::Empty,
                None => Data::String(text),
            }
        }
        Some("date") => match el.attr_local("date-value") {
            Some(v) => Data::DateTimeIso(v.into_owned()),
            None => fallback_string(text),
        },
        Some("time") => match el.attr_local("time-value") {
            Some(v) => Data::DurationIso(v.into_owned()),
            None => fallback_string(text),
        },
        Some("boolean") => match el.attr_local("boolean-value").as_deref() {
            // Producers disagree on how a boolean is spelled; the truthy and falsy sets are
            // matched case-insensitively so `1`/`TRUE`/`true` all read as a boolean rather than
            // silently disappearing.
            Some(value) => match value.trim().to_ascii_lowercase().as_str() {
                "true" | "1" => Data::Bool(true),
                "false" | "0" => Data::Bool(false),
                _ => fallback_string(text),
            },
            None => fallback_string(text),
        },
        Some("string") => {
            if text.is_empty() {
                el.attr_local("string-value")
                    .filter(|v| !v.is_empty())
                    .map(|v| Data::String(v.into_owned()))
                    .unwrap_or(Data::Empty)
            } else {
                Data::String(text)
            }
        }
        _ => fallback_string(text),
    }
}

fn fallback_string(text: String) -> Data {
    if text.is_empty() {
        Data::Empty
    } else {
        Data::String(text)
    }
}

/// Largest text one cell may produce, counted in characters.
///
/// OpenDocument writes a run of spaces as `<text:s text:c="N"/>`, which is a handful of bytes
/// standing for N characters. Each element is capped at [`MAX_TEXT_RUN`], but the *total* a cell
/// can reach was previously unbounded: 656 bytes of repeated `<text:s/>` expanded to 40 MB of
/// text, and a larger file would have gone further. A real cell never approaches this.
const MAX_CELL_TEXT: usize = 1 << 20;

/// Largest single `<text:s/>` run.
const MAX_TEXT_RUN: usize = 10_000;

/// Reads a text container, expanding the elements OpenDocument uses instead of characters.
///
/// The budget is spent as characters are produced rather than checked once at the end, so a file
/// that would amplify cannot allocate first and be refused afterwards.
fn read_text_container(parser: &mut XmlParser<'_>) -> Result<String> {
    let mut out = String::new();
    let mut depth = 1usize;
    loop {
        match parser.next_event()? {
            Event::Eof => return Err(Error::xml("paragraph is not terminated")),
            Event::Start(el) => match el.local_name() {
                // `<text:s/>` stands for a run of spaces, and one bare space by default.
                "s" => {
                    let count = el
                        .attr_local("c")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(1)
                        .min(MAX_TEXT_RUN);
                    if out.len() + count > MAX_CELL_TEXT {
                        return Err(Error::format(format!(
                            "cell text exceeds the {MAX_CELL_TEXT} character limit"
                        )));
                    }
                    out.extend(std::iter::repeat_n(' ', count));
                    parser.skip_element()?;
                }
                "tab" => {
                    out.push('\t');
                    parser.skip_element()?;
                }
                "line-break" => {
                    out.push('\n');
                    parser.skip_element()?;
                }
                // A note inside a paragraph is not part of the text.
                "annotation" => parser.skip_element()?,
                _ => depth += 1,
            },
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    return Ok(out);
                }
            }
            Event::Text(t) => {
                if out.len() + t.len() > MAX_CELL_TEXT {
                    return Err(Error::format(format!(
                        "cell text exceeds the {MAX_CELL_TEXT} character limit"
                    )));
                }
                if t.contains('&') {
                    crate::xml::unescape_into(&mut out, t);
                } else {
                    out.push_str(t);
                }
            }
        }
    }
}

/// Turns `of:=SUM([.A1:.B2])` into `SUM([.A1:.B2])`.
///
/// The leading token is a namespace prefix rather than formula text, but it is only stripped
/// when a `=` actually follows it, because `=` is optional and a formula can legitimately
/// contain a colon of its own.
fn normalise_formula(raw: &str) -> String {
    let mut body = raw.trim();
    if let Some((prefix, rest)) = body.split_once(':') {
        if rest.starts_with('=')
            && !prefix.is_empty()
            && prefix.chars().all(|c| c.is_ascii_alphanumeric())
        {
            body = &rest[1..];
            return body.to_owned();
        }
    }
    body.strip_prefix('=').unwrap_or(body).to_owned()
}

/// Reads a repeat attribute, clamped so a crafted count cannot drive a huge allocation.
fn read_repeat(el: &Element<'_>, attribute: &str, max: u32) -> u32 {
    el.attr_local(attribute)
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(1)
        .min(max)
}

/// Builds a string range from sparse positions.
///
/// Returns the error rather than an empty range: a sheet too large to materialise is the same
/// refusal the value read reports, and a formula read must not turn it into a silently empty
/// result the caller cannot distinguish from a sheet with no formulas.
fn range_of_strings(cells: Vec<(u32, u32, String)>) -> Result<Range<String>> {
    Range::from_sparse(cells)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Wraps a content part in the minimum container an ODS needs.
    fn package(content: &str) -> Vec<u8> {
        let stored = zipcore::FileOptions::stored();
        let mut writer = zipcore::ZipWriter::new(Cursor::new(Vec::new())).unwrap();
        writer
            .add_file(
                "mimetype",
                b"application/vnd.oasis.opendocument.spreadsheet",
                &stored,
            )
            .unwrap();
        writer
            .add_file("content.xml", content.as_bytes(), &stored)
            .unwrap();
        writer.finish().unwrap().into_inner()
    }

    fn document(body: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:body><office:spreadsheet>
    <table:table table:name="S">{body}</table:table>
  </office:spreadsheet></office:body>
</office:document-content>"#
        )
    }

    fn open(body: &str) -> Result<Ods<Cursor<Vec<u8>>>> {
        Ods::new(Cursor::new(package(&document(body))))
    }

    /// Opens the package and reads its single table, which is where the guards live.
    fn read(body: &str) -> Result<Range<Data>> {
        let mut workbook = open(body)?;
        workbook.worksheet_range("S")
    }

    #[test]
    fn reads_a_package_built_here() {
        let body = r#"<table:table-row>
            <table:table-cell office:value-type="float" office:value="1.5"/>
        </table:table-row>"#;
        let range = read(body).unwrap();
        assert_eq!(range.get((0, 0)), Some(&Data::Float(1.5)));
        assert_eq!(open(body).unwrap().sheet_names(), ["S"]);
    }

    /// A row is capped at the grid width, so a small file cannot demand a huge allocation by
    /// repeating one cell many times.
    #[test]
    fn a_row_cannot_exceed_the_column_limit() {
        let cell = |repeat: u32| {
            format!(
                r#"<table:table-cell office:value-type="float" office:value="1" table:number-columns-repeated="{repeat}"/>"#
            )
        };
        // Two cells each standing for the full grid width cannot both fit.
        let body = format!(
            "<table:table-row>{}{}</table:table-row>",
            cell(MAX_COLS),
            cell(MAX_COLS)
        );
        assert!(matches!(read(&body), Err(Error::Format(_))));
    }

    /// Repeating a row multiplies its cells, and the multiplication is charged before it is
    /// performed.
    #[test]
    fn a_repeated_row_cannot_exceed_the_cell_budget() {
        let cells: String = (0..100)
            .map(|_| {
                r#"<table:table-cell office:value-type="float" office:value="1"/>"#.to_string()
            })
            .collect();
        let body = format!(
            r#"<table:table-row table:number-rows-repeated="{MAX_ROWS}">{cells}</table:table-row>"#
        );
        assert!(matches!(read(&body), Err(Error::Format(_))));
    }

    /// Empty padding is the reason repeat counts exist, and it must stay free: a full-width run
    /// of empty cells stores nothing while still advancing the column it lands on.
    #[test]
    fn empty_repeats_store_nothing() {
        let body = format!(
            r#"<table:table-row>
                 <table:table-cell table:number-columns-repeated="{MAX_COLS}"/>
               </table:table-row>
               <table:table-row>
                 <table:table-cell table:number-columns-repeated="3"/>
                 <table:table-cell office:value-type="string"><text:p>after</text:p></table:table-cell>
               </table:table-row>"#
        );
        let range = read(&body).unwrap();
        assert_eq!(range.get((1, 3)), Some(&Data::String("after".into())));
        assert_eq!(range.used_cell_count(), 1);
    }

    /// Opening must not depend on the size of the tables, only on the metadata around them.
    ///
    /// The body here would be rejected if it were parsed, because the repeated row overruns the
    /// cell budget. Opening only reads names, so it succeeds; reading the table does not.
    #[test]
    fn opening_does_not_parse_table_bodies() {
        let cells: String = (0..100)
            .map(|_| {
                r#"<table:table-cell office:value-type="float" office:value="1"/>"#.to_string()
            })
            .collect();
        let body = format!(
            r#"<table:table-row table:number-rows-repeated="{MAX_ROWS}">{cells}</table:table-row>"#
        );
        let mut workbook = open(&body).expect("opening should not read the table body");
        assert_eq!(workbook.sheet_names(), ["S"]);
        assert!(workbook.worksheet_range("S").is_err());
    }
}

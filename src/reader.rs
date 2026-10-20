//! The shape every format reader presents to callers.

use crate::datatype::Data;
use crate::error::Result;
use crate::range::Range;
use crate::rows::{read_window, SheetRows, Window};
use std::io::{Read, Seek};

/// Where a worksheet's header row sits.
///
/// Asking for a header row trims the returned range so it begins at that row, which keeps the
/// header itself available as the range's first row for deserialization.
///
/// Trimming does *not* renumber anything: the range keeps its absolute coordinates, so after
/// asking for row 2 the range starts at `(1, 0)` and `Range::get((0, 0))` is out of bounds.
/// Iterating `rows()` is unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HeaderRow {
    /// The first row of the sheet.
    #[default]
    First,
    /// A 1-based row number, matching how the format itself counts rows.
    Row(u32),
}

impl HeaderRow {
    /// How many leading rows to discard before the range should start.
    pub(crate) fn skip_rows(&self) -> u32 {
        match self {
            HeaderRow::First => 0,
            HeaderRow::Row(n) => n.saturating_sub(1),
        }
    }
}

/// Trims the leading `header` rows off a range.
///
/// The rows are dropped in place. Slicing and copying instead (`inner[offset..].to_vec()`) would
/// hold both the whole sheet and its trimmed copy at the moment of peak memory, doubling the peak
/// on exactly the large sheets this reader exists to handle — and defeating the one thing the
/// dense builder guarantees.
pub(crate) fn apply_header_row<T: Clone>(range: Range<T>, header: HeaderRow) -> Range<T> {
    let skip = header.skip_rows();
    if skip == 0 {
        return range;
    }
    let (start, end) = (range.start(), range.end());
    if start.0 + skip >= end.0 {
        return Range::new((end.0, start.1), (end.0, end.1), Vec::new());
    }
    let width = range.width() as usize;
    let mut inner = range.into_inner();
    let offset = (skip as usize * width).min(inner.len());
    // `drain` shifts the survivors down in place, so the allocation is reused rather than added
    // to while both copies are live.
    inner.drain(..offset);
    Range::new((start.0 + skip, start.1), end, inner)
}

/// A rectangular region of a sheet, as recorded by the format's merge metadata.
///
/// Both corners are **inclusive**, which is how every spreadsheet format states a merge. That
/// differs from [`Range`], whose top-left is inclusive and bottom-right exclusive, so a merge
/// covering `A1:B2` reports `start == (0, 0)` and `end == (1, 1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MergedRegion {
    /// Inclusive top-left cell.
    pub start: (u32, u32),
    /// Inclusive bottom-right cell.
    pub end: (u32, u32),
}

impl MergedRegion {
    /// Builds a region from its inclusive corners, ordering them if they were given either way.
    pub fn new(a: (u32, u32), b: (u32, u32)) -> Self {
        MergedRegion {
            start: (a.0.min(b.0), a.1.min(b.1)),
            end: (a.0.max(b.0), a.1.max(b.1)),
        }
    }

    /// Number of rows the region spans, which is at least one.
    pub fn rows(&self) -> u32 {
        self.end.0.saturating_sub(self.start.0).saturating_add(1)
    }

    /// Number of columns the region spans, which is at least one.
    pub fn columns(&self) -> u32 {
        self.end.1.saturating_sub(self.start.1).saturating_add(1)
    }

    /// Whether a cell falls inside the region.
    pub fn contains(&self, cell: (u32, u32)) -> bool {
        cell.0 >= self.start.0
            && cell.0 <= self.end.0
            && cell.1 >= self.start.1
            && cell.1 <= self.end.1
    }

    /// The region as an A1-style reference, such as `B2:D10`.
    pub fn to_a1(&self) -> String {
        format!(
            "{}:{}",
            crate::refs::cell_ref(self.start.0, self.start.1),
            crate::refs::cell_ref(self.end.0, self.end.1)
        )
    }
}

impl std::fmt::Display for MergedRegion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_a1())
    }
}

/// Whether a sheet is shown when the workbook is opened.
///
/// Spreadsheets distinguish three states, and the difference matters to a caller listing a
/// workbook: a hidden sheet is still data, but it is not part of what the author intended to
/// present. A reader that reports hidden sheets as ordinary ones makes a workbook look like it
/// holds content the user never sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SheetVisibility {
    /// Visible when the workbook is opened.
    #[default]
    Visible,
    /// Hidden, but the reader may still be reading it.
    Hidden,
    /// Hidden and not reachable through the application's own unhide command.
    VeryHidden,
}

impl SheetVisibility {
    /// Whether the sheet is shown when the workbook opens.
    pub fn is_visible(&self) -> bool {
        matches!(self, SheetVisibility::Visible)
    }

    /// Builds from the OOXML/BIFF state word: `visible`, `hidden`, `veryHidden`.
    pub(crate) fn from_state(state: Option<&str>) -> Self {
        match state {
            Some("hidden") => SheetVisibility::Hidden,
            Some("veryHidden") | Some("veryhidden") => SheetVisibility::VeryHidden,
            _ => SheetVisibility::Visible,
        }
    }

    /// Builds from OpenDocument's `table:visibility` attribute: `visible`, `hidden`,
    /// `collapse` or `filter`.
    ///
    /// `collapse` and `filter` describe a sheet that is not being shown — a filtered or
    /// collapsed group — so they are reported as hidden rather than silently treated as visible.
    pub(crate) fn from_oodo(visibility: Option<&str>) -> Self {
        match visibility {
            Some("hidden") | Some("collapse") | Some("filter") => SheetVisibility::Hidden,
            _ => SheetVisibility::Visible,
        }
    }
}

impl std::fmt::Display for SheetVisibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SheetVisibility::Visible => "visible",
            SheetVisibility::Hidden => "hidden",
            SheetVisibility::VeryHidden => "veryHidden",
        })
    }
}

/// Reading a workbook from a seekable source.
///
/// Implemented by [`crate::Xlsx`], [`crate::Xls`], [`crate::Ods`] and the auto-detecting
/// [`crate::Sheets`]. Worksheets are read on demand, so opening a workbook is cheap and
/// memory use is proportional to the sheets actually touched.
pub trait Reader<RS>: Sized
where
    RS: Read + Seek,
{
    /// Opens a workbook from a reader.
    fn new(reader: RS) -> Result<Self>;

    /// Sets where the header row is, trimming returned ranges to start there.
    fn with_header_row(&mut self, header_row: HeaderRow) -> &mut Self;

    /// The header row currently configured.
    fn header_row(&self) -> HeaderRow;

    /// Sheet names, in workbook order.
    fn sheet_names(&self) -> &[String];

    /// The visibility of the sheet at a workbook-order position.
    ///
    /// Defaults to visible for a format that does not record it, which is the safe answer: a
    /// caller filtering by visibility then keeps everything rather than dropping data.
    fn sheet_visibility(&self, index: usize) -> SheetVisibility {
        let _ = index;
        SheetVisibility::Visible
    }

    /// Workbook-level defined names as `(name, formula)`.
    fn defined_names(&self) -> &[(String, String)];

    /// The values of one sheet, as a dense range.
    ///
    /// This materialises the sheet's whole bounding rectangle. For a large or sparse sheet prefer
    /// [`Reader::worksheet_rows`], which reads the same values without allocating one.
    fn worksheet_range(&mut self, name: &str) -> Result<Range<Data>>;

    /// Streams one sheet's rows without materialising it.
    ///
    /// Memory is proportional to the widest row rather than to the sheet, so a sheet whose cells
    /// sit far apart — which [`Reader::worksheet_range`] would refuse as implausibly large — reads
    /// here in constant space.
    fn worksheet_rows(&mut self, name: &str) -> Result<SheetRows<'_>>;

    /// Reads one rectangular window of a sheet, as an A1-style range reference.
    ///
    /// ```no_run
    /// # use cellar::{open_workbook, Reader};
    /// # fn main() -> Result<(), cellar::Error> {
    /// let mut workbook = open_workbook("huge.xlsx")?;
    /// // Only this window is materialised, and only the rows it needs are decoded.
    /// let totals = workbook.worksheet_range_ref("Sheet1", "B2:D10")?;
    /// assert_eq!(totals.get_size(), (9, 3));
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// The result is exactly the requested window, so its shape follows the reference rather than
    /// the data: positions inside it that hold nothing come back as [`Data::Empty`]. This is what
    /// distinguishes it from [`Reader::worksheet_range`], whose shape is the extent of the values.
    ///
    /// A format that can skip rows and cells outside the window does so, which makes the cost of
    /// this a function of the window instead of the sheet.
    ///
    /// The header row setting does not apply here. It exists to trim a whole-sheet read, and an
    /// explicit window already says which rows the caller wants.
    fn worksheet_range_ref(&mut self, name: &str, reference: &str) -> Result<Range<Data>> {
        let window = Window::parse(reference)?;
        let mut rows = self.worksheet_rows(name)?;
        read_window(&mut rows, window)
    }

    /// The formulas of one sheet, as a dense range of the text that follows `=`.
    fn worksheet_formula(&mut self, name: &str) -> Result<Range<String>>;

    /// The merged-cell regions of one sheet, in the order the file records them.
    ///
    /// A merged cell's value lives in its top-left cell; the positions it covers are blank in
    /// the grid, so without this a caller cannot tell "merged" from "empty" and cannot render
    /// the value across the region it actually occupies.
    ///
    /// The default returns no regions, which is the honest answer for a format that carries no
    /// merge metadata. All three readers here override it.
    fn worksheet_merged_regions(&mut self, name: &str) -> Result<Vec<MergedRegion>> {
        let _ = name;
        Ok(Vec::new())
    }

    /// The position of a sheet in workbook order.
    fn sheet_index(&self, name: &str) -> Option<usize> {
        self.sheet_names().iter().position(|s| s == name)
    }

    /// The values of the sheet at a workbook-order position.
    fn worksheet_range_at(&mut self, index: usize) -> Result<Range<Data>> {
        let name = self
            .sheet_names()
            .get(index)
            .cloned()
            .ok_or_else(|| crate::Error::NotFound(format!("sheet at index {index}")))?;
        self.worksheet_range(&name)
    }

    /// The formulas of the sheet at a workbook-order position.
    fn worksheet_formula_at(&mut self, index: usize) -> Result<Range<String>> {
        let name = self
            .sheet_names()
            .get(index)
            .cloned()
            .ok_or_else(|| crate::Error::NotFound(format!("sheet at index {index}")))?;
        self.worksheet_formula(&name)
    }

    /// Every sheet's values, in workbook order.
    fn worksheets(&mut self) -> Result<Vec<(String, Range<Data>)>> {
        let names = self.sheet_names().to_vec();
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let range = self.worksheet_range(&name)?;
            out.push((name, range));
        }
        Ok(out)
    }
}

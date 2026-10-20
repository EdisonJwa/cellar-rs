//! Streaming over a sheet's rows without materialising it.
//!
//! The dense [`Range`](crate::Range) is the convenient shape, but building one costs the sheet's
//! whole bounding rectangle in memory. A file whose cells sit at `A1` and `XFD1048576` implies
//! seventeen billion cells — an allocation no process can serve, and one a reader should never
//! need in order to hand back two values.
//!
//! This is the alternative: rows are produced one at a time into a buffer that is reused, so
//! memory is proportional to the widest row rather than to the sheet. A row carries only the cells
//! that hold something, so a single value in column 16000 costs one entry rather than 16001.
//!
//! ```no_run
//! use cellar::{open_workbook, Reader};
//!
//! # fn main() -> Result<(), cellar::Error> {
//! let mut workbook = open_workbook("huge.xlsx")?;
//! let mut rows = workbook.worksheet_rows("Sheet1")?;
//! while let Some(row) = rows.next_row()? {
//!     for (column, value) in row.iter() {
//!         // Only the populated cells of this row are visited.
//!         println!("({}, {column}) = {value:?}", row.index());
//!     }
//! }
//! # Ok(())
//! # }
//! ```

use crate::datatype::Data;
use crate::error::{Error, Result};
use crate::range::{max_range_cells, Range};
use crate::reader::HeaderRow;
use crate::refs::parse_range_ref;

/// One row of a sheet, holding only the cells that have values.
///
/// Columns are in ascending order, so [`Row::get`] can answer a lookup without scanning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Row<'a> {
    index: u32,
    columns: &'a [u32],
    values: &'a [Data],
}

impl<'a> Row<'a> {
    pub(crate) fn new(index: u32, columns: &'a [u32], values: &'a [Data]) -> Self {
        debug_assert_eq!(columns.len(), values.len());
        Row {
            index,
            columns,
            values,
        }
    }

    /// The row's zero-based position in the sheet.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// How many cells in this row hold a value.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether the row holds no values.
    ///
    /// A blank row still appears: it is part of the sheet's shape, and skipping it silently would
    /// leave a caller unable to tell a gap from the end of the data.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The columns holding values, in ascending order.
    pub fn columns(&self) -> &'a [u32] {
        self.columns
    }

    /// The values, paired positionally with [`Row::columns`].
    pub fn values(&self) -> &'a [Data] {
        self.values
    }

    /// The value at a column, if this row has one.
    pub fn get(&self, column: u32) -> Option<&'a Data> {
        self.columns
            .binary_search(&column)
            .ok()
            .map(|at| &self.values[at])
    }

    /// Iterates the populated cells as `(column, value)`.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &'a Data)> + '_ {
        self.columns.iter().copied().zip(self.values.iter())
    }

    /// Iterates the populated cells as `(absolute_row, column, value)`.
    pub fn cells(&self) -> impl Iterator<Item = (u32, u32, &'a Data)> + '_ {
        self.iter()
            .map(move |(column, value)| (self.index, column, value))
    }
}

/// Fills a reusable buffer with the next row.
///
/// Implemented once per format. Taking the buffers as arguments rather than owning them is what
/// lets the caller hand the same allocation back on every call.
pub(crate) trait RowSource {
    /// Clears and fills `columns` and `values` with the next row, returning its index, or `None`
    /// once the sheet is exhausted.
    fn next_row(&mut self, columns: &mut Vec<u32>, values: &mut Vec<Data>) -> Result<Option<u32>>;
}

/// A cursor over one sheet's rows.
///
/// Created by [`Reader::worksheet_rows`](crate::Reader::worksheet_rows). Rows borrow the cursor,
/// so the next one cannot be read until the current one is finished with — which is what keeps the
/// buffer reusable rather than allocating per row.
pub struct SheetRows<'a> {
    source: Box<dyn RowSource + 'a>,
    columns: Vec<u32>,
    values: Vec<Data>,
}

impl<'a> SheetRows<'a> {
    pub(crate) fn new(source: impl RowSource + 'a) -> Self {
        SheetRows {
            source: Box::new(source),
            columns: Vec::new(),
            values: Vec::new(),
        }
    }

    /// Advances to the next row, or `None` at the end of the sheet.
    pub fn next_row(&mut self) -> Result<Option<Row<'_>>> {
        // The buffers are borrowed mutably to fill and then immutably to read, which the borrow
        // checker allows because the two borrows are of separate fields.
        let index = match self.source.next_row(&mut self.columns, &mut self.values)? {
            Some(index) => index,
            None => return Ok(None),
        };

        // `Row::get` binary-searches `columns` and indexes `values` with the result, so the two
        // must be the same length or that index can fall outside `values`. Every source here
        // pushes them in lockstep, but the invariant is the kind a new source could break
        // silently, so it is checked rather than assumed: truncating both to the shorter length
        // is the only reading of a mismatched row that cannot panic.
        if self.columns.len() != self.values.len() {
            let keep = self.columns.len().min(self.values.len());
            self.columns.truncate(keep);
            self.values.truncate(keep);
        }

        // Rows promise ascending columns so that lookups can binary-search them. Every format
        // here writes them in order, so this costs one comparison pass; a file that does not gets
        // its row ordered rather than a silently broken lookup.
        if !self.columns.is_sorted() {
            let columns = std::mem::take(&mut self.columns);
            let values = std::mem::take(&mut self.values);
            let mut paired: Vec<(u32, Data)> = columns.into_iter().zip(values).collect();
            paired.sort_by_key(|(column, _)| *column);
            self.columns.reserve(paired.len());
            self.values.reserve(paired.len());
            for (column, value) in paired {
                self.columns.push(column);
                self.values.push(value);
            }
        }

        debug_assert_eq!(self.columns.len(), self.values.len());
        Ok(Some(Row::new(index, &self.columns, &self.values)))
    }
}

/// A rectangular window of a sheet, with inclusive bounds.
///
/// A format reader that can skip work it does not need takes one of these, so that reading a small
/// window out of a large sheet costs the window rather than the sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Window {
    pub rows: (u32, u32),
    pub cols: (u32, u32),
}

impl Window {
    /// Parses an A1-style range reference such as `B2:D10`.
    ///
    /// The corners are ordered, so `D10:B2` means the same window as `B2:D10`.
    pub(crate) fn parse(reference: &str) -> Result<Self> {
        let (a, b) = parse_range_ref(reference)
            .ok_or_else(|| Error::format(format!("not a cell range: {reference:?}")))?;
        Ok(Window {
            rows: (a.0.min(b.0), a.0.max(b.0)),
            cols: (a.1.min(b.1), a.1.max(b.1)),
        })
    }

    /// Whether a row index falls inside the window.
    pub(crate) fn holds_row(&self, row: u32) -> bool {
        row >= self.rows.0 && row <= self.rows.1
    }

    /// Whether a row index falls beyond the window, so that nothing later can.
    pub(crate) fn ends_before(&self, row: u32) -> bool {
        row > self.rows.1
    }

    /// Whether a column index falls inside the window.
    pub(crate) fn holds_col(&self, column: u32) -> bool {
        column >= self.cols.0 && column <= self.cols.1
    }

    /// The window's shape as `(rows, columns)`.
    fn size(&self) -> (u64, u64) {
        (
            u64::from(self.rows.1 - self.rows.0) + 1,
            u64::from(self.cols.1 - self.cols.0) + 1,
        )
    }
}

/// Reads a window of a sheet into a range of exactly that shape.
///
/// Positions inside the window that hold nothing come back as [`Data::Empty`], because the caller
/// asked for a rectangle and its shape should follow the reference rather than whatever the file
/// happens to contain.
///
/// Rows arrive in order, so the read stops once the window is behind it instead of walking the rest
/// of the sheet.
pub(crate) fn read_window(rows: &mut SheetRows<'_>, window: Window) -> Result<Range<Data>> {
    let (height, width) = window.size();
    let cells = height
        .checked_mul(width)
        .ok_or_else(|| Error::format("window dimensions overflow"))?;
    if cells > max_range_cells::<Data>() {
        return Err(Error::format(format!(
            "refusing to materialise a {height}x{width} window ({cells} cells)"
        )));
    }

    let mut inner = vec![Data::Empty; cells as usize];
    while let Some(row) = rows.next_row()? {
        let index = row.index();
        if window.ends_before(index) {
            break;
        }
        if !window.holds_row(index) {
            continue;
        }
        for (column, value) in row.iter() {
            if !window.holds_col(column) {
                continue;
            }
            let at = (index - window.rows.0) as usize * width as usize
                + (column - window.cols.0) as usize;
            inner[at] = value.clone();
        }
    }

    Ok(Range::new(
        (window.rows.0, window.cols.0),
        (window.rows.1 + 1, window.cols.1 + 1),
        inner,
    ))
}

/// Moves a cursor past the rows a header setting excludes.
///
/// The dense path trims its range to start at the header row, so streaming has to start in the
/// same place. Without this the two would disagree about which row comes first on the same sheet.
pub(crate) fn skip_header_row(rows: &mut SheetRows<'_>, header: HeaderRow) -> Result<()> {
    for _ in 0..header.skip_rows() {
        if rows.next_row()?.is_none() {
            break;
        }
    }
    Ok(())
}

impl std::fmt::Debug for SheetRows<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SheetRows")
            .field("buffered", &self.values.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(Vec<Vec<(u32, i32)>>, usize);

    impl RowSource for Fixed {
        fn next_row(
            &mut self,
            columns: &mut Vec<u32>,
            values: &mut Vec<Data>,
        ) -> Result<Option<u32>> {
            columns.clear();
            values.clear();
            if self.1 >= self.0.len() {
                return Ok(None);
            }
            let index = self.1;
            for (column, value) in &self.0[self.1] {
                columns.push(*column);
                values.push(Data::Int(i64::from(*value)));
            }
            self.1 += 1;
            Ok(Some(index as u32))
        }
    }

    fn rows() -> SheetRows<'static> {
        SheetRows::new(Fixed(
            vec![vec![(0, 1), (2, 3)], vec![], vec![(16000, 9)]],
            0,
        ))
    }

    #[test]
    fn yields_rows_in_order_including_blank_ones() {
        let mut rows = rows();
        let first = rows.next_row().unwrap().unwrap();
        assert_eq!(first.index(), 0);
        assert_eq!(first.columns(), &[0, 2]);
        assert_eq!(first.values(), &[Data::Int(1), Data::Int(3)]);

        let second = rows.next_row().unwrap().unwrap();
        assert_eq!(second.index(), 1);
        assert!(second.is_empty());

        let third = rows.next_row().unwrap().unwrap();
        assert_eq!(third.index(), 2);
        assert_eq!(third.len(), 1);
        assert_eq!(third.get(16000), Some(&Data::Int(9)));
        assert_eq!(third.get(0), None);

        assert!(rows.next_row().unwrap().is_none());
    }

    #[test]
    fn lookups_use_the_column_pairs() {
        let mut rows = rows();
        let row = rows.next_row().unwrap().unwrap();
        assert_eq!(row.get(0), Some(&Data::Int(1)));
        assert_eq!(row.get(1), None);
        assert_eq!(row.get(2), Some(&Data::Int(3)));
        assert_eq!(
            row.iter().map(|(c, v)| (c, v.clone())).collect::<Vec<_>>(),
            vec![(0, Data::Int(1)), (2, Data::Int(3))]
        );
        assert_eq!(row.cells().count(), 2);
    }

    /// The buffer is reused across rows, so a long sheet does not allocate per row.
    #[test]
    fn the_buffer_is_reused() {
        let mut rows = SheetRows::new(Fixed(vec![vec![(0, 1), (2, 3)], vec![(1, 4), (9, 5)]], 0));

        let first = rows.next_row().unwrap().unwrap();
        let address = first.columns().as_ptr();
        assert_eq!(first.columns(), &[0, 2]);

        let second = rows.next_row().unwrap().unwrap();
        assert_eq!(second.columns(), &[1, 9]);
        assert_eq!(
            second.columns().as_ptr(),
            address,
            "the second row should be filled into the buffer the first row used"
        );
    }

    #[test]
    fn a_header_row_is_skipped_by_advancing_the_cursor() {
        let mut rows = SheetRows::new(Fixed(vec![vec![(0, 1)], vec![(0, 2)], vec![(0, 3)]], 0));
        skip_header_row(&mut rows, HeaderRow::Row(2)).unwrap();
        let row = rows.next_row().unwrap().unwrap();
        assert_eq!(row.index(), 1);
        assert_eq!(row.get(0), Some(&Data::Int(2)));
    }

    #[test]
    fn skipping_past_the_end_is_not_an_error() {
        let mut rows = SheetRows::new(Fixed(vec![vec![(0, 1)]], 0));
        skip_header_row(&mut rows, HeaderRow::Row(99)).unwrap();
        assert!(rows.next_row().unwrap().is_none());
    }

    /// A row source that pushes fewer values than columns must not make `Row::get` index past
    /// the end of `values`.
    ///
    /// `Row::get` binary-searches `columns` and uses the result to index `values`, so a source
    /// that broke the lockstep would be a latent out-of-bounds. No source here does, which is
    /// exactly why the guard is worth pinning: it protects a future one.
    #[test]
    fn a_row_with_uneven_columns_and_values_is_truncated_not_trusted() {
        struct Uneven;

        impl RowSource for Uneven {
            fn next_row(
                &mut self,
                columns: &mut Vec<u32>,
                values: &mut Vec<Data>,
            ) -> Result<Option<u32>> {
                columns.clear();
                values.clear();
                // Three columns, one value: the mismatch a buggy source would produce.
                columns.extend([0, 5, 9]);
                values.push(Data::Int(1));
                Ok(Some(0))
            }
        }

        let mut rows = SheetRows::new(Uneven);
        let row = rows.next_row().unwrap().unwrap();
        assert_eq!(row.len(), 1, "the row keeps only what it can pair");
        assert_eq!(row.get(0), Some(&Data::Int(1)));
        // The dropped columns must not be reachable, which is what would panic without the guard.
        assert_eq!(row.get(5), None);
        assert_eq!(row.get(9), None);
    }

    /// Values arriving without columns is the mirror case, and is also truncated.
    #[test]
    fn a_row_with_more_values_than_columns_is_truncated() {
        struct Extra;

        impl RowSource for Extra {
            fn next_row(
                &mut self,
                columns: &mut Vec<u32>,
                values: &mut Vec<Data>,
            ) -> Result<Option<u32>> {
                columns.clear();
                values.clear();
                columns.push(2);
                values.extend([Data::Int(7), Data::Int(8), Data::Int(9)]);
                Ok(Some(0))
            }
        }

        let mut rows = SheetRows::new(Extra);
        let row = rows.next_row().unwrap().unwrap();
        assert_eq!(row.len(), 1);
        assert_eq!(row.get(2), Some(&Data::Int(7)));
    }
}

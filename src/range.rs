use crate::datatype::Data;
use crate::error::{Error, Result};

/// Splits a `Range<T>`'s index space from its storage: the parser fills a sparse list of
/// cells, and `Range` flattens that into the dense rectangle a caller wants to walk.
#[derive(Debug, Clone, PartialEq)]
pub struct Range<T> {
    /// Inclusive top-left, as zero-based `(row, column)`.
    start: (u32, u32),
    /// Exclusive bottom-right, as zero-based `(row, column)`.
    end: (u32, u32),
    /// Row-major cells, `width * height` of them.
    inner: Vec<T>,
}

impl<T> Range<T> {
    /// Builds a range from explicit bounds and row-major storage.
    ///
    /// `inner` must hold exactly `(end.0 - start.0) * (end.1 - start.1)` cells. The length is
    /// checked in debug builds and trusted in release builds.
    pub fn new(start: (u32, u32), end: (u32, u32), inner: Vec<T>) -> Self {
        debug_assert_eq!(
            inner.len() as u64,
            u64::from(end.0.saturating_sub(start.0)) * u64::from(end.1.saturating_sub(start.1)),
            "Range storage does not match its bounds"
        );
        Range { start, end, inner }
    }

    /// The inclusive top-left corner, as zero-based `(row, column)`.
    pub fn start(&self) -> (u32, u32) {
        self.start
    }

    /// The exclusive bottom-right corner, as zero-based `(row, column)`.
    pub fn end(&self) -> (u32, u32) {
        self.end
    }

    /// Number of rows.
    pub fn height(&self) -> u32 {
        self.end.0.saturating_sub(self.start.0)
    }

    /// Number of columns.
    pub fn width(&self) -> u32 {
        self.end.1.saturating_sub(self.start.1)
    }

    /// `(height, width)`.
    pub fn get_size(&self) -> (u32, u32) {
        (self.height(), self.width())
    }

    /// True when the range holds no cells at all.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// The row-major backing slice.
    pub fn as_slice(&self) -> &[T] {
        &self.inner
    }

    /// Consumes the range, returning its row-major storage.
    pub fn into_inner(self) -> Vec<T> {
        self.inner
    }

    /// The cell at an absolute zero-based `(row, column)`, if it is inside the range.
    pub fn get(&self, at: (u32, u32)) -> Option<&T> {
        let (r, c) = at;
        if r < self.start.0 || r >= self.end.0 || c < self.start.1 || c >= self.end.1 {
            return None;
        }
        let idx = (r - self.start.0) as usize * self.width() as usize + (c - self.start.1) as usize;
        self.inner.get(idx)
    }

    /// Mutable counterpart of [`Range::get`].
    pub fn get_mut(&mut self, at: (u32, u32)) -> Option<&mut T> {
        let (r, c) = at;
        if r < self.start.0 || r >= self.end.0 || c < self.start.1 || c >= self.end.1 {
            return None;
        }
        let idx = (r - self.start.0) as usize * self.width() as usize + (c - self.start.1) as usize;
        self.inner.get_mut(idx)
    }

    /// Alias for [`Range::get`], kept for familiarity.
    pub fn get_value(&self, at: (u32, u32)) -> Option<&T> {
        self.get(at)
    }

    /// Iterates rows as slices, each of length `width`.
    pub fn rows(&self) -> impl Iterator<Item = &[T]> {
        let width = self.width() as usize;
        self.inner.chunks(width.max(1))
    }

    /// Iterates rows mutably.
    pub fn rows_mut(&mut self) -> impl Iterator<Item = &mut [T]> {
        let width = self.width() as usize;
        self.inner.chunks_mut(width.max(1))
    }

    /// Iterates every cell in row-major order.
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.inner.iter()
    }

    /// Iterates `(absolute_row, absolute_column, &value)` for every cell.
    pub fn cells(&self) -> impl Iterator<Item = (u32, u32, &T)> + '_ {
        let start = self.start;
        let width = self.width();
        self.inner.iter().enumerate().map(move |(i, v)| {
            let i = i as u32;
            (start.0 + i / width.max(1), start.1 + i % width.max(1), v)
        })
    }

    /// The first row, which callers commonly treat as column headers.
    pub fn header_row(&self) -> Option<&[T]> {
        self.rows().next()
    }
}

impl<T: Default + Clone> Range<T> {
    /// Builds a range from a sparse list of absolute coordinates.
    ///
    /// The result is the dense bounding rectangle of `cells`, with every unlisted position
    /// filled by `T::default()`. Fails rather than allocating if the rectangle is
    /// implausibly large, which is what stops a two-cell file claiming to span four
    /// billion rows from exhausting memory.
    pub fn from_sparse(mut cells: Vec<(u32, u32, T)>) -> Result<Self> {
        if cells.is_empty() {
            return Ok(Range {
                start: (0, 0),
                end: (0, 0),
                inner: Vec::new(),
            });
        }
        // One pass finds the bounds and detects whether the input already arrives in order,
        // which it does for every format this crate reads. Sorting unconditionally would move
        // every cell — megabytes of them on a large sheet — for nothing in the common case.
        let mut min_col = cells[0].1;
        let mut max_col = cells[0].1;
        let mut ordered = true;
        let mut previous = (cells[0].0, cells[0].1);
        for (r, c, _) in &cells {
            min_col = min_col.min(*c);
            max_col = max_col.max(*c);
            if (*r, *c) < previous {
                ordered = false;
            }
            previous = (*r, *c);
        }
        if !ordered {
            cells.sort_unstable_by_key(|(r, c, _)| (*r, *c));
        }
        let start = (cells[0].0, min_col);
        // `+ 1` is the exclusive far corner, so a cell at the very top of the index space has
        // no representable rectangle. Refusing is the only honest answer; wrapping would put
        // the far corner before the near one and produce a nonsense range.
        let end = (
            cells
                .last()
                .map(|(r, _, _)| *r)
                .unwrap_or(0)
                .checked_add(1)
                .ok_or_else(|| Error::format("sheet extends past the addressable grid"))?,
            max_col
                .checked_add(1)
                .ok_or_else(|| Error::format("sheet extends past the addressable grid"))?,
        );
        let height = u64::from(end.0 - start.0);
        let width = u64::from(end.1 - start.1);
        let len = height
            .checked_mul(width)
            .ok_or_else(|| Error::format("sheet dimensions overflow"))?;
        let budget = max_range_cells::<T>();
        if len > budget {
            return Err(Error::format(format!(
                "refusing to materialise a {height}x{width} sheet ({len} cells, limit {budget}); \
                 the file's cells span an implausibly large rectangle"
            )));
        }
        let mut inner = vec![T::default(); len as usize];
        for (r, c, v) in cells {
            let idx = (r - start.0) as usize * width as usize + (c - start.1) as usize;
            inner[idx] = v;
        }
        Ok(Range { start, end, inner })
    }
}

impl<T: PartialEq + Default> Range<T> {
    /// Iterates `(absolute_row, absolute_column, &value)` for cells that are not default.
    pub fn used_cells(&self) -> impl Iterator<Item = (u32, u32, &T)> + '_ {
        self.cells().filter(|(_, _, v)| **v != T::default())
    }

    /// Counts cells that differ from `T::default()`.
    pub fn used_cell_count(&self) -> usize {
        self.inner.iter().filter(|v| **v != T::default()).count()
    }
}

/// Memory budget for a single materialised sheet.
///
/// A range is dense, so a file whose cells sit at `A1` and `XFD1048576` implies seventeen
/// billion cells. Deriving the ceiling from the element size means a caller gets a clear
/// error instead of an allocation the process cannot survive.
const MAX_RANGE_BYTES: u64 = 1 << 30;

pub(crate) fn max_range_cells<T>() -> u64 {
    let size = std::mem::size_of::<T>().max(1) as u64;
    MAX_RANGE_BYTES / size
}

/// A sheet extent a file declares, used to size the dense buffer before any cell is read.
///
/// Declared extents are hints: both OOXML and BIFF carry one, and both are occasionally stale. A
/// builder sizes itself from one when it can, and grows or trims if the file disagrees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Extent {
    /// The inclusive top-left the extent starts at.
    pub origin: (u32, u32),
    pub rows: u32,
    pub cols: u32,
}

/// Smallest buffer a builder starts with when no usable extent was declared.
const MIN_AXIS: u64 = 16;

/// Highest row or column index a cell may carry.
///
/// Well beyond any real grid, but it keeps the bounds arithmetic away from `u32` overflow while
/// leaving the cell budget to reject anything genuinely absurd. Every read path applies it, so
/// the dense and streaming views of a sheet agree about what is addressable.
pub(crate) const MAX_AXIS: u32 = 1 << 31;

/// Builds a range by writing cells straight into their final positions.
///
/// [`Range::from_sparse`] holds the cells twice — once as the sparse list, once as the dense range
/// built from it — so peak memory is the sum of the two, a little over twice the size of the
/// finished range. When the file declares the sheet's extent, as OOXML does, the dense buffer can
/// be allocated once and filled directly and the sparse list never exists at all.
///
/// Writes may arrive in any order. The buffer is grown when a cell falls outside it and trimmed to
/// the extent actually written when it is finished, so a declared extent that turns out to be
/// wrong costs neither correctness nor memory beyond what it claimed.
#[derive(Default)]
pub(crate) struct SheetBuilder<T> {
    /// Absolute position of buffer slot zero.
    origin: (u32, u32),
    /// Allocated size of the buffer.
    rows: u32,
    cols: u32,
    inner: Vec<T>,
    /// Inclusive extent actually written.
    min: (u32, u32),
    max: (u32, u32),
    wrote: bool,
}

/// Largest buffer reserved from a declared extent, in bytes.
///
/// A `<dimension ref="A1:AF1048576"/>` element is about a hundred bytes and declares exactly the
/// grid's 33,554,432 cells — which, at 32 bytes each, is a one-gigabyte allocation decided before
/// a single cell has been read. The cell budget permits it because the rectangle is legal; what
/// was missing is a cap on how much a *hint* may reserve up front. The buffer still grows to the
/// real extent as cells arrive, so a sheet that genuinely fills that rectangle is read correctly
/// — it simply pays for the memory as it proves it needs it.
pub(crate) const MAX_EXTENT_RESERVE: u64 = 64 << 20;

impl<T: Default + Clone> SheetBuilder<T> {
    /// Creates a builder, optionally pre-sized from a declared extent.
    pub(crate) fn new(hint: Option<Extent>) -> Self {
        let mut builder = SheetBuilder {
            origin: (0, 0),
            rows: 0,
            cols: 0,
            inner: Vec::new(),
            min: (0, 0),
            max: (0, 0),
            wrote: false,
        };
        if let Some(extent) = hint {
            let cells = u64::from(extent.rows) * u64::from(extent.cols);
            let bytes = cells.saturating_mul(std::mem::size_of::<T>().max(1) as u64);
            // The declared extent is only a hint, so it is trusted only as far as it can be
            // checked cheaply: it must describe a real rectangle, fit the cell budget, and not
            // reserve more than `MAX_EXTENT_RESERVE` before any cell has been seen. Anything
            // larger is ignored, and the buffer grows to the truth instead.
            if extent.rows > 0
                && extent.cols > 0
                && cells <= max_range_cells::<T>()
                && bytes <= MAX_EXTENT_RESERVE
            {
                builder.origin = extent.origin;
                builder.rows = extent.rows;
                builder.cols = extent.cols;
                builder.inner = vec![T::default(); cells as usize];
            }
        }
        builder
    }

    /// Whether a position lies inside the allocated buffer.
    fn covers(&self, row: u32, col: u32) -> bool {
        let (r, c) = (u64::from(row), u64::from(col));
        let (or, oc) = (u64::from(self.origin.0), u64::from(self.origin.1));
        r >= or && r < or + u64::from(self.rows) && c >= oc && c < oc + u64::from(self.cols)
    }

    /// Places a value, growing the buffer first if the position falls outside it.
    pub(crate) fn write(&mut self, row: u32, col: u32, value: T) -> Result<()> {
        if row >= MAX_AXIS || col >= MAX_AXIS {
            return Err(Error::format(format!(
                "cell at ({row}, {col}) is outside any spreadsheet grid"
            )));
        }
        if !self.covers(row, col) {
            self.grow(row, col)?;
        }
        if self.wrote {
            self.min = (self.min.0.min(row), self.min.1.min(col));
            self.max = (self.max.0.max(row), self.max.1.max(col));
        } else {
            self.min = (row, col);
            self.max = (row, col);
            self.wrote = true;
        }
        let index = (u64::from(row - self.origin.0) * u64::from(self.cols)
            + u64::from(col - self.origin.1)) as usize;
        self.inner[index] = value;
        Ok(())
    }

    /// Grows and re-lays-out the buffer to cover a position.
    fn grow(&mut self, row: u32, col: u32) -> Result<()> {
        let origin = (self.origin.0.min(row), self.origin.1.min(col));
        let needed_rows = (u64::from(self.origin.0) + u64::from(self.rows)).max(u64::from(row) + 1)
            - u64::from(origin.0);
        let needed_cols = (u64::from(self.origin.1) + u64::from(self.cols)).max(u64::from(col) + 1)
            - u64::from(origin.1);
        // Grow geometrically, so a sheet whose extent was not declared does not re-lay-out the
        // whole buffer on every row.
        let rows = needed_rows.max(u64::from(self.rows) * 2).max(MIN_AXIS);
        let cols = needed_cols.max(u64::from(self.cols) * 2).max(MIN_AXIS);
        let cells = rows * cols;
        let budget = max_range_cells::<T>();
        if cells > budget {
            return Err(Error::format(format!(
                "refusing to materialise a {rows}x{cols} sheet ({cells} cells, limit {budget}); \
                 the file's cells span an implausibly large rectangle"
            )));
        }
        let (rows, cols) = (rows as u32, cols as u32);

        let mut inner = vec![T::default(); cells as usize];
        if self.cols > 0 {
            let old_cols = self.cols as usize;
            let row_shift = (self.origin.0 - origin.0) as usize;
            let col_shift = (self.origin.1 - origin.1) as usize;
            for (index, value) in std::mem::take(&mut self.inner).into_iter().enumerate() {
                let shifted =
                    (index / old_cols + row_shift) * cols as usize + (index % old_cols + col_shift);
                inner[shifted] = value;
            }
        }
        self.inner = inner;
        self.origin = origin;
        self.rows = rows;
        self.cols = cols;
        Ok(())
    }

    /// Trims the buffer to the extent written and returns the range.
    pub(crate) fn finish(mut self) -> Range<T> {
        if !self.wrote {
            return Range::new((0, 0), (0, 0), Vec::new());
        }
        let rows = self.max.0 - self.min.0 + 1;
        let cols = self.max.1 - self.min.1 + 1;
        if self.origin != self.min || self.rows != rows || self.cols != cols {
            // Drop the rows and columns outside the extent written. `retain` keeps the survivors
            // in order in a single in-place pass, which leaves exactly the compacted row-major
            // layout, so trimming a stale declared extent costs no second allocation.
            let buffer_cols = self.cols as usize;
            let row_offset = (self.min.0 - self.origin.0) as usize;
            let col_offset = (self.min.1 - self.origin.1) as usize;
            let keep_rows = rows as usize;
            let keep_cols = cols as usize;
            let mut index = 0usize;
            self.inner.retain(|_| {
                let row = index / buffer_cols;
                let col = index % buffer_cols;
                index += 1;
                row >= row_offset
                    && row < row_offset + keep_rows
                    && col >= col_offset
                    && col < col_offset + keep_cols
            });
        }
        Range::new(self.min, (self.max.0 + 1, self.max.1 + 1), self.inner)
    }
}

impl Range<Data> {
    /// The value at an absolute coordinate, or [`Data::Empty`] when outside the range.
    pub fn value_or_empty(&self, at: (u32, u32)) -> Data {
        self.get(at).cloned().unwrap_or(Data::Empty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sparse(cells: &[(u32, u32, i32)]) -> Range<i32> {
        Range::from_sparse(cells.iter().map(|(r, c, v)| (*r, *c, *v)).collect()).unwrap()
    }

    #[test]
    fn sparse_input_becomes_a_dense_rectangle() {
        let range = sparse(&[(1, 1, 5), (0, 0, 7)]);
        assert_eq!(range.start(), (0, 0));
        assert_eq!(range.end(), (2, 2));
        assert_eq!(range.get_size(), (2, 2));
        assert_eq!(range.as_slice(), &[7, 0, 0, 5]);
    }

    #[test]
    fn sparse_input_keeps_its_origin() {
        let range = sparse(&[(3, 2, 1), (4, 4, 2)]);
        assert_eq!(range.start(), (3, 2));
        assert_eq!(range.end(), (5, 5));
        assert_eq!(range.get_size(), (2, 3));
        assert_eq!(range.get((3, 2)), Some(&1));
        assert_eq!(range.get((4, 4)), Some(&2));
        // Inside the rectangle but not supplied.
        assert_eq!(range.get((3, 4)), Some(&0));
        // Outside the rectangle.
        assert_eq!(range.get((2, 2)), None);
        assert_eq!(range.get((5, 4)), None);
    }

    #[test]
    fn empty_sparse_input_is_an_empty_range() {
        let range = Range::<i32>::from_sparse(Vec::new()).unwrap();
        assert!(range.is_empty());
        assert_eq!(range.get_size(), (0, 0));
        assert_eq!(range.rows().count(), 0);
    }

    #[test]
    fn an_implausible_rectangle_is_refused() {
        // Two cells that between them claim Excel's entire grid must not be materialised.
        let result = Range::from_sparse(vec![
            (0u32, 0u32, Data::Int(1)),
            (1_048_575, 16_383, Data::Int(2)),
        ]);
        assert!(matches!(result, Err(Error::Format(_))));
    }

    #[test]
    fn rows_and_cells_walk_in_order() {
        let range = sparse(&[(0, 0, 1), (0, 1, 2), (1, 0, 3), (1, 1, 4)]);
        let rows: Vec<Vec<i32>> = range.rows().map(|r| r.to_vec()).collect();
        assert_eq!(rows, vec![vec![1, 2], vec![3, 4]]);
        let cells: Vec<(u32, u32, i32)> = range.cells().map(|(r, c, v)| (r, c, *v)).collect();
        assert_eq!(cells, vec![(0, 0, 1), (0, 1, 2), (1, 0, 3), (1, 1, 4)]);
        assert_eq!(range.header_row(), Some(&[1, 2][..]));
    }

    #[test]
    fn used_cells_skips_defaults() {
        let range = sparse(&[(0, 0, 1), (0, 2, 0), (1, 1, 2)]);
        let used: Vec<(u32, u32, i32)> = range.used_cells().map(|(r, c, v)| (r, c, *v)).collect();
        assert_eq!(used, vec![(0, 0, 1), (1, 1, 2)]);
        assert_eq!(range.used_cell_count(), 2);
    }

    #[test]
    fn get_mut_updates_in_place() {
        let mut range = sparse(&[(0, 0, 1)]);
        *range.get_mut((0, 0)).unwrap() = 9;
        assert_eq!(range.get((0, 0)), Some(&9));
    }

    #[test]
    fn constructing_with_explicit_bounds() {
        let range = Range::new((0, 0), (3, 2), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(range.height(), 3);
        assert_eq!(range.width(), 2);
        assert_eq!(range.get((0, 1)), Some(&2));
        assert_eq!(range.get((2, 1)), Some(&6));
        assert_eq!(range.get((3, 0)), None);
        assert_eq!(range.into_inner(), vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn value_or_empty_covers_positions_outside_the_rectangle() {
        let range = Range::from_sparse(vec![(0u32, 0u32, Data::Int(1))]).unwrap();
        assert_eq!(range.value_or_empty((0, 0)), Data::Int(1));
        assert_eq!(range.value_or_empty((5, 5)), Data::Empty);
    }

    /// Cell sets chosen to cover the awkward cases: empty, single, holes, ragged rows, an origin
    /// away from the top-left, and extents that are one cell wide or one cell tall.
    fn sample_cell_sets() -> Vec<Vec<(u32, u32, i32)>> {
        vec![
            vec![],
            vec![(0, 0, 1)],
            vec![(5, 7, 1)],
            vec![(0, 0, 1), (0, 1, 2), (1, 0, 3), (1, 1, 4)],
            vec![(3, 2, 1), (4, 4, 2)],
            vec![(0, 0, 1), (0, 3, 2)],
            vec![(0, 0, 1), (0, 1, 2), (0, 2, 3), (1, 0, 4)],
            vec![(0, 0, 1), (2, 2, 2), (4, 4, 3)],
            vec![(1, 1, 1), (1, 2, 2), (2, 1, 3), (2, 2, 4)],
            vec![(100, 100, 1), (100, 101, 2), (101, 100, 3)],
        ]
    }

    fn hinted(origin: (u32, u32), rows: u32, cols: u32) -> Option<Extent> {
        Some(Extent { origin, rows, cols })
    }

    fn build(cells: &[(u32, u32, i32)], hint: Option<Extent>) -> Range<i32> {
        let mut builder = SheetBuilder::new(hint);
        for (row, col, value) in cells {
            builder.write(*row, *col, *value).unwrap();
        }
        builder.finish()
    }

    /// The builder must agree with the sparse conversion exactly, whatever extent it was given.
    ///
    /// This is the test that makes the builder safe to prefer: it exercises sizing, growing and
    /// trimming against the reference implementation on the same inputs.
    #[test]
    fn the_builder_agrees_with_the_sparse_conversion() {
        for cells in sample_cell_sets() {
            let expected = Range::from_sparse(cells.clone()).unwrap();
            for hint in [
                None,
                hinted((0, 0), 1, 1),
                hinted((0, 0), 4, 4),
                hinted((0, 0), 1000, 1000),
                hinted((0, 0), 2, 100),
                hinted((0, 0), 100, 2),
                hinted((5, 5), 1, 1),
                hinted((99, 99), 3, 3),
                hinted((0, 0), 0, 0),
            ] {
                assert_eq!(
                    build(&cells, hint),
                    expected,
                    "cells {cells:?} with hint {hint:?}"
                );
            }
        }
    }

    /// A declared extent that is too small must not drop cells.
    #[test]
    fn the_builder_grows_past_a_small_extent() {
        let cells: Vec<(u32, u32, i32)> = (0..8)
            .flat_map(|r| (0..8).map(move |c| (r, c, (r * 8 + c) as i32)))
            .collect();
        let range = build(&cells, hinted((0, 0), 2, 2));
        assert_eq!(range.get_size(), (8, 8));
        assert_eq!(range.get((7, 7)), Some(&63));
    }

    /// A declared extent that is too large is trimmed back to what was written.
    #[test]
    fn the_builder_trims_a_large_extent() {
        let range = build(&[(1, 1, 7)], hinted((0, 0), 500, 500));
        assert_eq!(range.start(), (1, 1));
        assert_eq!(range.end(), (2, 2));
        assert_eq!(range.get_size(), (1, 1));
        assert_eq!(range.as_slice(), &[7]);
    }

    /// An extent whose origin sits past the first cell written trims its leading rows and columns.
    #[test]
    fn the_builder_trims_leading_rows_and_columns() {
        let range = build(&[(0, 0, 1), (3, 3, 2)], hinted((0, 0), 10, 10));
        assert_eq!(range.start(), (0, 0));
        assert_eq!(range.get_size(), (4, 4));
        assert_eq!(range.get((0, 0)), Some(&1));
        assert_eq!(range.get((3, 3)), Some(&2));
        assert_eq!(range.get((1, 1)), Some(&0));
    }

    /// A stale extent that starts past the written cells still yields the right rectangle.
    #[test]
    fn the_builder_handles_an_origin_past_the_cells() {
        let range = build(&[(0, 0, 1)], hinted((5, 5), 4, 4));
        assert_eq!(range.start(), (0, 0));
        assert_eq!(range.get_size(), (1, 1));
        assert_eq!(range.as_slice(), &[1]);
    }

    /// A later write to the same position wins, matching the sparse conversion.
    #[test]
    fn the_builder_keeps_the_last_write() {
        let range = build(&[(0, 0, 1), (0, 0, 2)], None);
        assert_eq!(range.as_slice(), &[2]);
    }

    #[test]
    fn an_empty_builder_gives_an_empty_range() {
        let range = build(&[], None);
        assert!(range.is_empty());
        assert_eq!(range.get_size(), (0, 0));
    }

    /// A cell outside any plausible grid is refused rather than reaching the bounds arithmetic.
    #[test]
    fn the_builder_refuses_an_impossible_position() {
        let mut builder = SheetBuilder::<i32>::new(None);
        assert!(builder.write(u32::MAX, 0, 1).is_err());
        let mut builder = SheetBuilder::<i32>::new(None);
        assert!(builder.write(0, u32::MAX, 1).is_err());
    }

    /// A rectangle beyond the budget is refused rather than allocated.
    #[test]
    fn the_builder_refuses_an_implausible_rectangle() {
        let mut builder = SheetBuilder::<Data>::new(None);
        assert!(builder.write(0, 0, Data::Int(1)).is_ok());
        assert!(builder.write(1_048_575, 16_383, Data::Int(2)).is_err());
    }

    /// A declared extent larger than the budget is ignored, not fatal: the file may be wrong.
    #[test]
    fn an_oversized_declared_extent_is_ignored() {
        let mut builder = SheetBuilder::<Data>::new(hinted((0, 0), u32::MAX, u32::MAX));
        builder.write(0, 0, Data::Int(1)).unwrap();
        let range = builder.finish();
        assert_eq!(range.get_size(), (1, 1));
    }
}

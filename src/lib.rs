//! Reading Excel and OpenDocument spreadsheets, in pure Rust.
//!
//! No `unsafe`. One dependency. Reads `.xlsx`, `.xlsm`, `.xls` and `.ods` from untrusted files
//! without panicking, and answers a question about forty cells on a two-million-row sheet in under
//! a millisecond.
//!
//! The archive container is served by [`zipcore`](https://docs.rs/zipcore), this project's own
//! from-scratch ZIP reader, behind a single internal trait. The OLE compound-file reader used for
//! `.xls` and the XML tokenizer used for the two XML-based formats are implemented here too, so
//! the whole parsing stack is this project's own code.
//!
//! # Formats
//!
//! | Format | Extensions | Container |
//! |---|---|---|
//! | OOXML workbook | `.xlsx` `.xlsm` `.xltx` `.xlam` | ZIP + XML |
//! | OpenDocument spreadsheet | `.ods` | ZIP + XML |
//! | Legacy BIFF8 workbook | `.xls` (Excel 97–2003) | OLE compound file |
//!
//! Detection is by content rather than extension, so a renamed file still reads correctly.
//!
//! # Reading a sheet
//!
//! [`Reader::worksheet_range`] returns a dense [`Range`], which is the convenient shape and costs
//! the sheet's whole bounding rectangle.
//!
//! ```
//! use cellar::{open_workbook, Reader};
//!
//! let mut workbook = open_workbook("tests/data/chanora.xlsx")?;
//! for name in workbook.sheet_names().to_vec() {
//!     let range = workbook.worksheet_range(&name)?;
//!     for row in range.rows() {
//!         for cell in row {
//!             if let Some(text) = cell.as_string() {
//!                 print!("{text}\t");
//!             }
//!         }
//!         println!();
//!     }
//! }
//! # Ok::<(), cellar::Error>(())
//! ```
//!
//! # The two other shapes
//!
//! [`Reader::worksheet_rows`] streams a sheet instead, materialising nothing: memory is the widest
//! row rather than the rectangle, which matters when a sheet is large or sparse enough that its
//! rectangle would be the problem.
//!
//! ```
//! use cellar::{open_workbook, Reader};
//!
//! let mut workbook = open_workbook("tests/data/sample.ods")?;
//! let mut rows = workbook.worksheet_rows("Sheet1")?;
//! while let Some(row) = rows.next_row()? {
//!     println!("row {}: {} cell(s)", row.index(), row.len());
//! }
//! # Ok::<(), cellar::Error>(())
//! ```
//!
//! [`Reader::worksheet_range_ref`] returns exactly one rectangle, and in OOXML stops decompressing
//! once the window's rows have closed — so its cost is the window rather than the sheet.
//!
//! ```
//! use cellar::{open_workbook, Reader};
//!
//! let mut workbook = open_workbook("tests/data/chanora.xlsx")?;
//! let window = workbook.worksheet_range_ref("Identifications", "B2:F40")?;
//! assert_eq!(window.get_size(), (39, 5));
//! # Ok::<(), cellar::Error>(())
//! ```
//!
//! # Cell values
//!
//! A cell is a [`Data`], which distinguishes the cases a spreadsheet actually has — integer,
//! float, text, boolean, a date serial with its epoch, ISO date and duration text, an in-cell
//! error, and empty. Forgiving accessors mean callers rarely match by hand.
//!
//! ```
//! use cellar::Data;
//!
//! let value = Data::Float(2.5);
//! assert_eq!(value.as_f64(), Some(2.5));
//! assert_eq!(value.as_string().as_deref(), Some("2.5"));
//! assert_eq!(Data::Int(42).as_i64(), Some(42));
//! assert!(Data::Empty.is_empty());
//! ```
//!
//! # Errors
//!
//! Failures are typed, and a malformed file never panics.
//!
//! ```
//! use cellar::{open_workbook, Reader};
//!
//! let mut workbook = open_workbook("tests/data/chanora.xlsx")?;
//! match workbook.worksheet_range("No Such Sheet") {
//!     Err(cellar::Error::NotFound(name)) => println!("no sheet called {name}"),
//!     _ => unreachable!(),
//! }
//! # Ok::<(), cellar::Error>(())
//! ```
//!
//! # Design
//!
//! Every reader talks to an internal archive abstraction rather than to a container directly.
//! That keeps the container swappable and keeps format logic free of packaging concerns.

#![forbid(unsafe_code)]

mod cfb;
mod codepage;
mod container;
mod datatype;
#[cfg(feature = "serde")]
pub mod de;
mod error;
mod formats;
mod formula;
mod open;
mod range;
mod reader;
pub mod refs;
mod rows;
mod utils;
mod xml;

pub use container::Format;
pub use datatype::{CellError, Data, DataType, ExcelDateTime, YmdHms};
#[cfg(feature = "serde")]
pub use de::{RangeDeserializer, RangeDeserializerBuilder};
pub use error::{Error, Result};
pub use formats::ods::Ods;
pub use formats::xls::Xls;
pub use formats::xlsx::Xlsx;
pub use open::{
    open_ods, open_ods_from_rs, open_workbook, open_workbook_auto, open_workbook_auto_from_rs,
    open_xls, open_xls_from_rs, open_xlsx, open_xlsx_from_rs, Sheets,
};
pub use range::Range;
pub use reader::{HeaderRow, MergedRegion, Reader, SheetVisibility};
pub use rows::{Row, SheetRows};

/// The README's samples, compiled and run by `cargo test --doc`.
///
/// A README whose examples do not build is worse than no README, so every sample a reader is
/// likely to copy lives here as a doc-test against the real fixtures. The module is compiled only
/// while collecting doc-tests, so it costs the library nothing.
#[cfg(doctest)]
pub mod readme_examples {
    /// Read a sheet: open, list, iterate.
    ///
    /// ```
    /// use cellar::{open_workbook, Reader};
    ///
    /// let mut workbook = open_workbook("tests/data/chanora.xlsx")?;
    ///
    /// for name in workbook.sheet_names().to_vec() {
    ///     let range = workbook.worksheet_range(&name)?;
    ///     assert!(range.get_size().0 > 0 || range.is_empty());
    /// }
    /// # Ok::<(), cellar::Error>(())
    /// ```
    pub struct ReadASheet;

    /// Cell values and their forgiving accessors.
    ///
    /// ```
    /// use cellar::{CellError, Data, ExcelDateTime};
    ///
    /// let value = Data::Float(2.5);
    /// assert_eq!(value.as_f64(), Some(2.5));
    /// assert_eq!(value.as_string().as_deref(), Some("2.5"));
    ///
    /// assert_eq!(Data::Int(42).as_i64(), Some(42));
    /// assert_eq!(Data::Bool(true).as_bool(), Some(true));
    /// assert!(Data::Error(CellError::Div0).is_error());
    /// assert!(Data::Empty.is_empty());
    /// assert_eq!(
    ///     Data::DateTime(ExcelDateTime::new(45321.5, false))
    ///         .as_date()
    ///         .unwrap()
    ///         .to_string(),
    ///     "2024-01-30 12:00:00"
    /// );
    /// ```
    pub struct CellValues;

    /// Dates carry their epoch, so a 1904 workbook is never misread as 1900.
    ///
    /// ```
    /// use cellar::ExcelDateTime;
    ///
    /// let dt = ExcelDateTime::new(45321.5, false);
    /// assert_eq!(dt.to_ymd_hms().unwrap().to_string(), "2024-01-30 12:00:00");
    /// assert_eq!(dt.to_unix_timestamp(), Some(1_706_616_000));
    /// ```
    pub struct Dates;

    /// Stream a sheet's rows without materialising it.
    ///
    /// ```
    /// use cellar::{open_workbook, Reader};
    ///
    /// let mut workbook = open_workbook("tests/data/sample.ods")?;
    /// let mut rows = workbook.worksheet_rows("Sheet1")?;
    ///
    /// let mut seen = 0;
    /// while let Some(row) = rows.next_row()? {
    ///     for (_column, _value) in row.iter() {
    ///         seen += 1;
    ///     }
    /// }
    /// assert!(seen > 0);
    /// # Ok::<(), cellar::Error>(())
    /// ```
    pub struct StreamRows;

    /// Read one region; its shape follows the reference.
    ///
    /// ```
    /// use cellar::{open_workbook, Reader};
    ///
    /// let mut workbook = open_workbook("tests/data/chanora.xlsx")?;
    /// let totals = workbook.worksheet_range_ref("Identifications", "B2:F40")?;
    /// assert_eq!(totals.get_size(), (39, 5));
    /// # Ok::<(), cellar::Error>(())
    /// ```
    pub struct ReadRegion;

    /// Merged regions, whose corners are inclusive.
    ///
    /// ```
    /// use cellar::{open_workbook, Reader};
    ///
    /// let mut workbook = open_workbook("tests/data/chanora.xlsx")?;
    /// for region in workbook.worksheet_merged_regions("Identifications")? {
    ///     assert!(region.rows() >= 1 && region.columns() >= 1);
    ///     assert!(region.start.0 <= region.end.0 && region.start.1 <= region.end.1);
    /// }
    /// # Ok::<(), cellar::Error>(())
    /// ```
    pub struct MergedCells;

    /// Sheet visibility, per sheet.
    ///
    /// ```
    /// use cellar::{open_workbook, Reader};
    ///
    /// let mut workbook = open_workbook("tests/data/chanora.xlsx")?;
    /// for index in 0..workbook.sheet_names().len() {
    ///     let visibility = workbook.sheet_visibility(index);
    ///     let _ = visibility.is_visible();
    /// }
    /// # Ok::<(), cellar::Error>(())
    /// ```
    pub struct SheetVisibilityOf;

    /// Sheet metadata and lookup by position.
    ///
    /// ```
    /// use cellar::{open_workbook, Reader};
    ///
    /// let mut workbook = open_workbook("tests/data/chanora.xlsx")?;
    /// let names = workbook.sheet_names().to_vec();
    /// assert!(!names.is_empty());
    /// assert_eq!(workbook.sheet_index(&names[0]), Some(0));
    /// assert!(workbook.worksheet_range_at(0).is_ok());
    /// let _ = workbook.defined_names();
    /// # Ok::<(), cellar::Error>(())
    /// ```
    pub struct SheetMetadata;

    /// Formulas are read separately from values.
    ///
    /// ```
    /// use cellar::{open_workbook, Reader};
    ///
    /// let mut workbook = open_workbook("tests/data/sample.ods")?;
    /// let _values = workbook.worksheet_range("Sheet1")?;
    /// let formulas = workbook.worksheet_formula("Sheet1")?;
    /// assert!(formulas.used_cell_count() > 0);
    /// # Ok::<(), cellar::Error>(())
    /// ```
    pub struct Formulas;

    /// Error handling by kind.
    ///
    /// ```
    /// use cellar::{Error, Reader};
    ///
    /// // A missing sheet is a caller error, not corruption.
    /// let mut workbook = cellar::open_workbook("tests/data/chanora.xlsx")?;
    /// match workbook.worksheet_range("No Such Sheet") {
    ///     Err(Error::NotFound(_)) => {}
    ///     other => panic!("expected NotFound, got {other:?}"),
    /// }
    ///
    /// // Bytes that are not a spreadsheet are corruption, and say so.
    /// let junk = std::io::Cursor::new(b"not a workbook".to_vec());
    /// match cellar::open_workbook_auto_from_rs(junk) {
    ///     Err(e) => assert!(e.is_corrupt()),
    ///     Ok(_) => panic!("junk must not open"),
    /// }
    /// # Ok::<(), cellar::Error>(())
    /// ```
    pub struct ErrorHandling;

    /// Header-row trimming keeps coordinates absolute.
    ///
    /// ```
    /// use cellar::{open_workbook, HeaderRow, Reader};
    ///
    /// let mut workbook = open_workbook("tests/data/chanora.xlsx")?;
    /// workbook.with_header_row(HeaderRow::Row(2));
    /// let range = workbook.worksheet_range("Summary")?;
    /// assert_eq!(range.start(), (1, 0));
    /// # Ok::<(), cellar::Error>(())
    /// ```
    pub struct HeaderRowTrim;

    /// Deserializing rows by header name (feature `serde`).
    ///
    /// ```
    /// # #[cfg(feature = "serde")]
    /// # {
    /// use cellar::{open_workbook, RangeDeserializerBuilder, Reader};
    /// use serde::Deserialize;
    ///
    /// #[derive(Deserialize)]
    /// struct Record {
    ///     #[serde(rename = "File path")]
    ///     path: String,
    /// }
    ///
    /// let mut workbook = open_workbook("tests/data/chanora.xlsx")?;
    /// let range = workbook.worksheet_range("Identifications")?;
    /// let records: Vec<Record> = RangeDeserializerBuilder::new()
    ///     .from_range(&range)?
    ///     .collect::<Result<Vec<_>, _>>()?;
    /// assert!(!records.is_empty());
    /// assert!(!records[0].path.is_empty());
    /// # }
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub struct Deserialize;
}

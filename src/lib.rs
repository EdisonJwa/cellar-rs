//! Reading Excel and OpenDocument spreadsheets, in pure Rust.
//!
//! `cellar` reads the three spreadsheet formats that cover essentially every file in the
//! wild, with no dependency on the `zip` crate: the archive container is served by
//! [`zipcore`](https://docs.rs/zipcore), this project's own from-scratch ZIP reader, behind a
//! single internal trait.
//!
//! - OOXML workbooks: `.xlsx`, `.xlsm`, `.xltx`, `.xlam`
//! - OpenDocument spreadsheets: `.ods`
//! - Legacy BIFF8 workbooks: `.xls` (Excel 97-2003)
//!
//! This crate contains no `unsafe` code, and that is enforced rather than intended: the attribute
//! below fails the build if any is introduced. It applies to the library only, because a test that
//! measures peak allocation needs to install a global allocator.
//!
//! A sheet can be read two ways. [`Reader::worksheet_range`] returns a dense
//! [`Range`], which is the convenient shape and costs the sheet's whole bounding rectangle.
//! [`Reader::worksheet_rows`] streams rows instead, materialising nothing, which is the one to use
//! when a sheet is large or sparse enough that its rectangle would be the problem.
//!
//! # Example
//!
//! ```no_run
//! use cellar::{open_workbook, Data, Reader};
//!
//! # fn main() -> Result<(), cellar::Error> {
//! let mut workbook = open_workbook("report.xlsx")?;
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
//! # Ok(())
//! # }
//! ```
//!
//! # Design
//!
//! Every reader talks to an internal archive abstraction rather than to a container directly.
//! That keeps the container swappable and keeps format logic free of packaging concerns.
//! The compound file reader used for `.xls` and the XML tokenizer used for the two XML-based
//! formats are both implemented in this crate.

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

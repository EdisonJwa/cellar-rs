//! Deserializing worksheet rows into Rust types.
//!
//! This mirrors the shape calamine users expect: a builder produces an iterator over a range
//! that yields `Result<T>` for each row, with the sheet's header row supplying field names.
//!
//! Values are matched to struct fields by name when headers are available, so a struct works
//! even when the sheet's columns are in a different order. Without headers, columns are matched
//! by position.

use crate::datatype::Data;
use crate::error::{Error, Result};
use crate::range::Range;
use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;

impl de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Error::Msg(msg.to_string())
    }
}

/// Generates the scalar methods whose only job is to defer to the row's first cell.
macro_rules! delegate_to_first_cell {
    ($($method:ident),* $(,)?) => {
        $(
            fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
                self.first().$method(visitor)
            }
        )*
    };
}

/// Adapts a single cell to a deserializer.
struct DataDeserializer<'a> {
    value: &'a Data,
}

impl<'a> DataDeserializer<'a> {
    fn new(value: &'a Data) -> Self {
        DataDeserializer { value }
    }
}

impl<'de, 'a> de::Deserializer<'de> for DataDeserializer<'a> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.value {
            Data::Int(i) => visitor.visit_i64(*i),
            Data::Float(f) => visitor.visit_f64(*f),
            Data::String(s) => visitor.visit_str(s),
            Data::Bool(b) => visitor.visit_bool(*b),
            // A serial is exposed as a Unix timestamp, which is the portable interpretation.
            Data::DateTime(dt) => match dt.to_unix_timestamp() {
                Some(seconds) => visitor.visit_i64(seconds),
                None => visitor.visit_unit(),
            },
            Data::DateTimeIso(s) | Data::DurationIso(s) => visitor.visit_str(s),
            Data::Error(e) => Err(de::Error::custom(e)),
            Data::Empty => visitor.visit_unit(),
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.value {
            Data::Empty => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.value.as_bool() {
            Some(b) => visitor.visit_bool(b),
            None => Err(unexpected(self.value, "a boolean")),
        }
    }

    fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.value.as_i64() {
            Some(i) => visitor.visit_i64(i),
            None => Err(unexpected(self.value, "an integer")),
        }
    }

    fn deserialize_u64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.value.as_i64() {
            Some(i) if i >= 0 => visitor.visit_u64(i as u64),
            _ => Err(unexpected(self.value, "an unsigned integer")),
        }
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.value.as_f64() {
            Some(f) => visitor.visit_f64(f),
            None => Err(unexpected(self.value, "a number")),
        }
    }

    /// A blank cell reads as an empty string rather than failing, because trailing blanks are
    /// extremely common inside a range's rectangle. `Option` still sees a blank as absent,
    /// because `deserialize_option` intercepts it first.
    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.value.as_string() {
            Some(s) => visitor.visit_string(s),
            None => visitor.visit_string(String::new()),
        }
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_str(visitor)
    }

    serde::forward_to_deserialize_any! {
        char bytes byte_buf unit unit_struct newtype_struct seq tuple tuple_struct
        map struct enum identifier ignored_any
        i8 i16 i32 u8 u16 u32 f32
    }
}

fn unexpected(value: &Data, expected: &str) -> Error {
    Error::Msg(format!("expected {expected} but found {value:?}"))
}

/// Plans already computed for this range, so the header lookup runs once per range rather than
/// once per row. Keyed by the struct's field list, whose address is stable per type.
#[derive(Default)]
struct PlanCache {
    /// For a struct: the column to read for each field, in field order.
    structs: HashMap<usize, Vec<usize>>,
    /// For a map: every column with its header name.
    map: Option<Vec<(String, usize)>>,
}

/// Presents one row either as a sequence of cells or as a map keyed by field name.
struct RowDeserializer<'a> {
    cells: &'a [Data],
    headers: Option<&'a [String]>,
    plans: &'a RefCell<PlanCache>,
}

impl<'a> RowDeserializer<'a> {
    fn first(&self) -> DataDeserializer<'a> {
        DataDeserializer::new(self.cells.first().unwrap_or(&Data::Empty))
    }

    /// The column to read for each struct field, in field order.
    ///
    /// Fields are matched to columns by header name, so a struct works even when the sheet's
    /// columns are in a different order. A field with no column is reported by name, which is a
    /// more useful message than serde's own "missing field".
    ///
    /// The result is cached: building it costs a scan of the headers per field, and serde only
    /// reveals the field list from inside `deserialize_struct`, which runs once per row. Without
    /// the cache that scan would be paid a million times over on a large sheet.
    fn struct_plan(&self, fields: &'static [&'static str]) -> Result<Vec<usize>> {
        let key = fields.as_ptr() as usize;
        if let Some(cached) = self.plans.borrow().structs.get(&key).cloned() {
            return Ok(cached);
        }

        let mut plan = Vec::with_capacity(fields.len());
        for field in fields {
            let column = match self.headers {
                Some(headers) => headers
                    .iter()
                    .position(|header| header.trim().eq_ignore_ascii_case(field))
                    .ok_or_else(|| {
                        Error::Msg(format!("no column named `{field}` in the header row"))
                    })?,
                None => {
                    if plan.len() >= self.cells.len() {
                        return Err(Error::Msg(format!(
                            "row has {} columns but field `{field}` needs column {}",
                            self.cells.len(),
                            plan.len() + 1
                        )));
                    }
                    plan.len()
                }
            };
            plan.push(column);
        }

        self.plans.borrow_mut().structs.insert(key, plan.clone());
        Ok(plan)
    }

    /// Builds the plan for a map: one entry per column.
    fn map_plan(&self) -> Vec<(String, usize)> {
        if let Some(cached) = self.plans.borrow().map.clone() {
            return cached;
        }
        let plan: Vec<(String, usize)> = match self.headers {
            Some(headers) => headers
                .iter()
                .enumerate()
                .map(|(i, h)| (h.clone(), i))
                .collect(),
            None => (0..self.cells.len()).map(|i| (i.to_string(), i)).collect(),
        };
        self.plans.borrow_mut().map = Some(plan.clone());
        plan
    }
}

impl<'de, 'a> de::Deserializer<'de> for RowDeserializer<'a> {
    type Error = Error;

    /// A bare row is a sequence of cells.
    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_seq(RowSeqAccess {
            cells: self.cells.iter(),
        })
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_any(visitor)
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        self.deserialize_any(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_any(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let plan = self.map_plan();
        visitor.visit_map(PlanMapAccess {
            cells: self.cells,
            plan,
            index: 0,
        })
    }

    /// A struct is read positionally, one value per field in declaration order.
    ///
    /// Positional rather than keyed deliberately: yielding field names would mean handing serde a
    /// freshly built key for every field of every row, which on a large sheet is millions of
    /// pointless allocations. The plan already knows which column each field wants, so the names
    /// never need to be materialised.
    ///
    /// The cost is that `#[serde(flatten)]` and `#[serde(deny_unknown_fields)]` are not supported
    /// on a row struct, since both require a keyed visitor.
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        let plan = self.struct_plan(fields)?;
        visitor.visit_seq(PlanSeqAccess {
            cells: self.cells,
            plan,
            index: 0,
        })
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_some(self)
    }

    // Deserializing a row as a scalar reads its first cell, which is what lets a single-column
    // sheet collect straight into a `Vec<f64>`.
    delegate_to_first_cell!(
        deserialize_bool,
        deserialize_i8,
        deserialize_i16,
        deserialize_i32,
        deserialize_i64,
        deserialize_u8,
        deserialize_u16,
        deserialize_u32,
        deserialize_u64,
        deserialize_f32,
        deserialize_f64,
        deserialize_char,
        deserialize_str,
        deserialize_string,
        deserialize_bytes,
        deserialize_byte_buf,
        deserialize_unit,
        deserialize_identifier,
        deserialize_ignored_any,
    );

    serde::forward_to_deserialize_any! {
        unit_struct newtype_struct enum
    }
}

struct RowSeqAccess<'a> {
    cells: std::slice::Iter<'a, Data>,
}

impl<'de, 'a> SeqAccess<'de> for RowSeqAccess<'a> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>> {
        match self.cells.next() {
            Some(value) => seed.deserialize(DataDeserializer::new(value)).map(Some),
            None => Ok(None),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.cells.len())
    }
}

/// Yields one cell per planned column, in field order.
struct PlanSeqAccess<'a> {
    cells: &'a [Data],
    plan: Vec<usize>,
    index: usize,
}

impl<'de, 'a> SeqAccess<'de> for PlanSeqAccess<'a> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>> {
        let Some(column) = self.plan.get(self.index).copied() else {
            return Ok(None);
        };
        self.index += 1;
        let value = self.cells.get(column).unwrap_or(&Data::Empty);
        seed.deserialize(DataDeserializer::new(value)).map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.plan.len())
    }
}

/// Yields `(key, value)` pairs in a fixed order decided up front.
struct PlanMapAccess<'a> {
    cells: &'a [Data],
    plan: Vec<(String, usize)>,
    index: usize,
}

impl<'de, 'a> MapAccess<'de> for PlanMapAccess<'a> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>> {
        match self.plan.get(self.index) {
            Some((name, _)) => {
                let name = name.clone();
                seed.deserialize(StringDeserializer(name)).map(Some)
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value> {
        let column = match self.plan.get(self.index) {
            Some((_, column)) => *column,
            None => return Err(Error::Msg("value requested before its key".into())),
        };
        self.index += 1;
        let value = self.cells.get(column).unwrap_or(&Data::Empty);
        seed.deserialize(DataDeserializer::new(value))
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.plan.len())
    }
}

/// A string key, used for map keys and struct field names.
struct StringDeserializer(String);

impl<'de> de::Deserializer<'de> for StringDeserializer {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_string(self.0)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes byte_buf option
        unit unit_struct newtype_struct seq tuple tuple_struct map struct enum identifier
        ignored_any
    }
}

/// Configures how a range is turned into a stream of records.
///
/// ```
/// use cellar::{Data, Range, RangeDeserializerBuilder};
/// use serde::Deserialize;
///
/// #[derive(Deserialize)]
/// struct Row {
///     name: String,
///     value: f64,
/// }
///
/// # fn main() -> Result<(), cellar::Error> {
/// let range = Range::from_sparse(vec![
///     (0, 0, Data::String("name".into())),
///     (0, 1, Data::String("value".into())),
///     (1, 0, Data::String("alpha".into())),
///     (1, 1, Data::Float(1.5)),
/// ])?;
///
/// let rows: Vec<Row> = RangeDeserializerBuilder::new()
///     .from_range(&range)?
///     .collect::<Result<Vec<_>, _>>()?;
/// assert_eq!(rows[0].name, "alpha");
/// assert_eq!(rows[0].value, 1.5);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Default)]
pub struct RangeDeserializerBuilder {
    headers: Option<Vec<String>>,
    has_headers: bool,
}

impl RangeDeserializerBuilder {
    /// Creates a builder that treats the range's first row as headers.
    pub fn new() -> Self {
        RangeDeserializerBuilder {
            headers: None,
            has_headers: true,
        }
    }

    /// Uses these names instead of a header row, leaving every row as data.
    pub fn with_headers(headers: &[&str]) -> Self {
        RangeDeserializerBuilder {
            headers: Some(headers.iter().map(|h| (*h).to_string()).collect()),
            has_headers: true,
        }
    }

    /// Whether the range's first row holds the headers. On by default.
    pub fn has_headers(&mut self, has_headers: bool) -> &mut Self {
        self.has_headers = has_headers;
        self
    }

    /// Starts deserializing records from `range`.
    ///
    /// The returned iterator borrows the range, not the builder, so the builder need not outlive
    /// the iteration.
    pub fn from_range<'r, T: DeserializeOwned>(
        &self,
        range: &'r Range<Data>,
    ) -> Result<RangeDeserializer<'r, T>> {
        let rows: Vec<&[Data]> = range.rows().collect();

        let (headers, rows) = if let Some(provided) = self.headers.clone() {
            (Some(provided), rows)
        } else if self.has_headers {
            match rows.split_first() {
                // The header row supplies the names and is not itself yielded as a record.
                Some((first, rest)) => (
                    Some(
                        first
                            .iter()
                            .map(|cell| cell.as_string().unwrap_or_default())
                            .collect(),
                    ),
                    rest.to_vec(),
                ),
                None => (None, Vec::new()),
            }
        } else {
            (None, rows)
        };

        let width = range.width() as usize;
        Ok(RangeDeserializer {
            headers,
            rows,
            width,
            index: 0,
            plans: RefCell::new(PlanCache::default()),
            marker: PhantomData,
        })
    }
}

/// Iterator over the records of a range.
pub struct RangeDeserializer<'a, T> {
    headers: Option<Vec<String>>,
    rows: Vec<&'a [Data]>,
    /// The range's width, kept so callers can size buffers ahead of iteration.
    width: usize,
    index: usize,
    /// Column plans, computed once and reused for every row.
    plans: RefCell<PlanCache>,
    marker: PhantomData<T>,
}

impl<'a, T> RangeDeserializer<'a, T> {
    /// The number of columns each record spans.
    pub fn width(&self) -> usize {
        self.width
    }
}

impl<'a, T: DeserializeOwned> Iterator for RangeDeserializer<'a, T> {
    type Item = Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        let row = *self.rows.get(self.index)?;
        self.index += 1;
        Some(T::deserialize(RowDeserializer {
            cells: row,
            headers: self.headers.as_deref(),
            plans: &self.plans,
        }))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.rows.len().saturating_sub(self.index);
        (remaining, Some(remaining))
    }
}

impl<'a, T: DeserializeOwned> ExactSizeIterator for RangeDeserializer<'a, T> {}

/// Deserializes a cell as an `Option<f64>`, turning anything unusable into `None`.
///
/// Intended for `#[serde(deserialize_with = "...")]` on a column that is numeric in intent but
/// may hold blanks or text in practice.
pub fn deserialize_as_f64_or_none<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<f64>, D::Error>
where
    D: de::Deserializer<'de>,
{
    let value = Data::deserialize(deserializer)?;
    Ok(value.as_f64().filter(|f| f.is_finite()))
}

/// Deserializes a cell as an `Option<i64>`, turning anything unusable into `None`.
pub fn deserialize_as_i64_or_none<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<i64>, D::Error>
where
    D: de::Deserializer<'de>,
{
    let value = Data::deserialize(deserializer)?;
    Ok(value.as_i64())
}

/// Deserializes a cell from whatever the sheet holds.
///
/// This is what the helpers above use to reach a cell through the row deserializer, and it also
/// lets a struct field be typed as [`Data`] to keep the raw value.
impl<'de> Deserialize<'de> for Data {
    fn deserialize<D: de::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct DataVisitor;

        impl<'de> Visitor<'de> for DataVisitor {
            type Value = Data;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a spreadsheet cell value")
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Data, E> {
                Ok(Data::Int(v))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Data, E> {
                Ok(Data::Int(v as i64))
            }

            fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Data, E> {
                Ok(Data::Float(v))
            }

            fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Data, E> {
                Ok(Data::Bool(v))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Data, E> {
                Ok(Data::String(v.to_owned()))
            }

            fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Data, E> {
                Ok(Data::String(v))
            }

            fn visit_unit<E: de::Error>(self) -> std::result::Result<Data, E> {
                Ok(Data::Empty)
            }

            fn visit_none<E: de::Error>(self) -> std::result::Result<Data, E> {
                Ok(Data::Empty)
            }

            fn visit_some<D: de::Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> std::result::Result<Data, D::Error> {
                Data::deserialize(deserializer)
            }
        }

        deserializer.deserialize_any(DataVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn sample() -> Range<Data> {
        let cells = vec![
            (0u32, 0u32, Data::String("name".into())),
            (0, 1, Data::String("count".into())),
            (0, 2, Data::String("ratio".into())),
            (0, 3, Data::String("active".into())),
            (1, 0, Data::String("alpha".into())),
            (1, 1, Data::Int(3)),
            (1, 2, Data::Float(1.5)),
            (1, 3, Data::Bool(true)),
            (2, 0, Data::String("beta".into())),
            (2, 1, Data::Int(4)),
            (2, 2, Data::Float(2.5)),
            (2, 3, Data::Bool(false)),
        ];
        Range::from_sparse(cells).unwrap()
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Record {
        name: String,
        count: i64,
        ratio: f64,
        active: bool,
    }

    fn records<T: DeserializeOwned>(
        builder: &RangeDeserializerBuilder,
        range: &Range<Data>,
    ) -> Vec<Result<T>> {
        builder.from_range::<T>(range).unwrap().collect()
    }

    #[test]
    fn deserializes_named_fields() {
        let range = sample();
        let rows: Vec<Record> = records(&RangeDeserializerBuilder::new(), &range)
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                Record {
                    name: "alpha".into(),
                    count: 3,
                    ratio: 1.5,
                    active: true
                },
                Record {
                    name: "beta".into(),
                    count: 4,
                    ratio: 2.5,
                    active: false
                },
            ]
        );
    }

    #[test]
    fn fields_are_matched_by_name_not_position() {
        let range = sample();
        #[derive(Debug, Deserialize)]
        struct Reordered {
            ratio: f64,
            name: String,
        }
        let rows: Vec<Reordered> = records(&RangeDeserializerBuilder::new(), &range)
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows[0].name, "alpha");
        assert_eq!(rows[0].ratio, 1.5);
    }

    #[test]
    fn extra_columns_are_ignored() {
        let range = sample();
        #[derive(Debug, Deserialize)]
        struct Subset {
            name: String,
        }
        let rows: Vec<Subset> = records(&RangeDeserializerBuilder::new(), &range)
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].name, "beta");
    }

    #[test]
    fn a_missing_column_is_an_error() {
        let range = sample();
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        struct Missing {
            name: String,
            absent: String,
        }
        let rows: Vec<Result<Missing>> = records(&RangeDeserializerBuilder::new(), &range);
        assert!(rows[0].is_err());
    }

    #[test]
    fn explicit_headers_leave_every_row_as_data() {
        let range = sample();
        let builder = RangeDeserializerBuilder::with_headers(&["name", "count", "ratio", "active"]);
        let rows: Vec<Result<Record>> = records(&builder, &range);
        // No row is consumed, so the header row is present and cannot be read as a record.
        assert_eq!(rows.len(), 3);
        assert!(rows[0].is_err());
        assert_eq!(rows[1].as_ref().unwrap().name, "alpha");
        assert_eq!(rows[2].as_ref().unwrap().name, "beta");
    }

    #[test]
    fn positional_mapping_without_headers() {
        let range = sample();
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        struct Tuple(String, i64, f64, bool);
        let mut builder = RangeDeserializerBuilder::new();
        builder.has_headers(false);
        let rows: Vec<Result<Tuple>> = records(&builder, &range);
        assert_eq!(rows.len(), 3, "the header row is data when headers are off");
        // The header row's text cannot be read as a tuple of numbers.
        assert!(rows[0].is_err());
        assert_eq!(rows[1].as_ref().unwrap().1, 3);
    }

    #[test]
    fn sequences_and_maps() {
        let range = sample();

        let rows: Vec<Vec<Data>> = records(&RangeDeserializerBuilder::new(), &range)
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Data::String("alpha".into()));
        assert_eq!(rows[0][1], Data::Int(3));

        let maps: Vec<std::collections::HashMap<String, Data>> =
            records(&RangeDeserializerBuilder::new(), &range)
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
        assert_eq!(maps[0].get("name"), Some(&Data::String("alpha".into())));
        assert_eq!(maps[0].get("count"), Some(&Data::Int(3)));
    }

    #[test]
    fn options_treat_blanks_as_absent() {
        let range = Range::from_sparse(vec![
            (0u32, 0u32, Data::String("a".into())),
            (0, 1, Data::String("b".into())),
            (1, 0, Data::Int(1)),
        ])
        .unwrap();

        #[derive(Debug, Deserialize)]
        struct Row {
            a: Option<i64>,
            b: Option<String>,
        }
        let rows: Vec<Row> = records(&RangeDeserializerBuilder::new(), &range)
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows[0].a, Some(1));
        assert_eq!(rows[0].b, None);
    }

    #[test]
    fn text_columns_parse_into_numbers() {
        let range = Range::from_sparse(vec![
            (0u32, 0u32, Data::String("v".into())),
            (1, 0, Data::String("42.5".into())),
        ])
        .unwrap();
        #[derive(Debug, Deserialize)]
        struct Row {
            v: f64,
        }
        let rows: Vec<Row> = records(&RangeDeserializerBuilder::new(), &range)
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows[0].v, 42.5);
    }

    #[test]
    fn a_single_column_sheet_becomes_a_vec_of_scalars() {
        let range = Range::from_sparse(vec![
            (0u32, 0u32, Data::Int(1)),
            (1, 0, Data::Int(2)),
            (2, 0, Data::Int(3)),
        ])
        .unwrap();
        let mut builder = RangeDeserializerBuilder::new();
        builder.has_headers(false);
        let values: Vec<i64> = builder
            .from_range::<i64>(&range)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(values, vec![1, 2, 3]);
    }

    #[test]
    fn an_empty_range_yields_nothing() {
        let range = Range::<Data>::from_sparse(Vec::new()).unwrap();
        let rows: Vec<Result<Record>> = records(&RangeDeserializerBuilder::new(), &range);
        assert!(rows.is_empty());
    }

    #[test]
    fn helpers_turn_unusable_values_into_none() {
        #[derive(Debug, Deserialize)]
        struct Row {
            #[serde(default, deserialize_with = "deserialize_as_f64_or_none")]
            v: Option<f64>,
        }
        let range = Range::from_sparse(vec![
            (0u32, 0u32, Data::String("v".into())),
            (1, 0, Data::String("not a number".into())),
            (2, 0, Data::Float(2.5)),
        ])
        .unwrap();
        let rows: Vec<Row> = records(&RangeDeserializerBuilder::new(), &range)
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows[0].v, None);
        assert_eq!(rows[1].v, Some(2.5));
    }

    #[test]
    fn the_iterator_reports_its_length() {
        let range = sample();
        let deserializer = RangeDeserializerBuilder::new()
            .from_range::<Record>(&range)
            .unwrap();
        assert_eq!(deserializer.len(), 2);
        assert_eq!(deserializer.width(), 4);
    }
}

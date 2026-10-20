use std::fmt;

/// An error value stored *inside* a cell, such as `#DIV/0!`.
///
/// This is data, not a failure: reading a sheet full of `#N/A` is a success.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CellError {
    /// `#DIV/0!` — division by zero.
    Div0,
    /// `#N/A` — a value is not available.
    NA,
    /// `#NAME?` — an unrecognised name.
    Name,
    /// `#NULL!` — an empty intersection of two ranges.
    Null,
    /// `#NUM!` — a number a function cannot use.
    Num,
    /// `#REF!` — a reference to a cell that no longer exists.
    Ref,
    /// `#VALUE!` — the wrong type of argument.
    Value,
    /// `#GETTING_DATA` — a value still being fetched.
    GettingData,
    /// An error code this crate does not recognise, kept verbatim.
    Unknown(String),
}

impl fmt::Display for CellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            CellError::Div0 => "#DIV/0!",
            CellError::NA => "#N/A",
            CellError::Name => "#NAME?",
            CellError::Null => "#NULL!",
            CellError::Num => "#NUM!",
            CellError::Ref => "#REF!",
            CellError::Value => "#VALUE!",
            CellError::GettingData => "#GETTING_DATA",
            CellError::Unknown(s) => s.as_str(),
        };
        f.write_str(s)
    }
}

impl CellError {
    /// Parses the textual form used by OOXML (`<v>#REF!</v>` with `t="e"`).
    pub(crate) fn from_ooxml(s: &str) -> Self {
        match s {
            "#DIV/0!" => CellError::Div0,
            "#N/A" => CellError::NA,
            "#NAME?" => CellError::Name,
            "#NULL!" => CellError::Null,
            "#NUM!" => CellError::Num,
            "#REF!" => CellError::Ref,
            "#VALUE!" => CellError::Value,
            "#GETTING_DATA" => CellError::GettingData,
            other => CellError::Unknown(other.to_owned()),
        }
    }

    /// Maps a BIFF error byte (`0x00`=NULL, `0x07`=DIV/0!, `0x0F`=VALUE!, `0x17`=REF!,
    /// `0x1D`=NAME?, `0x24`=NUM!, `0x2A`=N/A) to a [`CellError`].
    pub(crate) fn from_biff(code: u8) -> Self {
        match code {
            0x00 => CellError::Null,
            0x07 => CellError::Div0,
            0x0F => CellError::Value,
            0x17 => CellError::Ref,
            0x1D => CellError::Name,
            0x24 => CellError::Num,
            0x2A => CellError::NA,
            other => CellError::Unknown(format!("#ERR{other:02X}")),
        }
    }
}

/// A date/time held as an Excel serial number.
///
/// The serial is measured in days since the workbook's epoch, with the fractional
/// part carrying the time of day. Excel has two epochs and the choice is per-workbook:
/// the default 1900 system and the legacy Mac 1904 system. Keeping the flag here means a
/// value read from a file is never silently reinterpreted under the wrong epoch.
///
/// The 1900 system also contains Excel's notorious leap-year bug: it treats 1900 as a leap
/// year so that serial 60 names the non-existent 1900-02-29. Serial 60 therefore converts
/// to 1900-03-01, which is what every mainstream reader does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExcelDateTime {
    value: f64,
    is_1904: bool,
}

/// A civil date and time broken out of an Excel serial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YmdHms {
    /// Proleptic Gregorian year.
    pub year: i32,
    /// Month, `1..=12`.
    pub month: u8,
    /// Day of month, `1..=31`.
    pub day: u8,
    /// Hour, `0..=23`.
    pub hour: u8,
    /// Minute, `0..=59`.
    pub minute: u8,
    /// Second, `0..=59`.
    pub second: u8,
    /// Millisecond, `0..=999`.
    pub millisecond: u16,
}

impl fmt::Display for YmdHms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )?;
        if self.millisecond > 0 {
            write!(f, ".{:03}", self.millisecond)?;
        }
        Ok(())
    }
}

/// Days between the Unix epoch and the start of the 1904 date system.
const DAYS_1904_TO_UNIX: f64 = 24107.0;
/// Serial of 1970-01-01 in the 1900 system.
const SERIAL_1900_TO_UNIX: f64 = 25569.0;
/// Same, for serials at or after 1900-03-01. Serials below 60 are one day out, because
/// Excel still counts the 1900-02-29 that never happened.
const SERIAL_1900_TO_UNIX_EARLY: f64 = 25568.0;
/// First serial after Excel's fabricated 1900-02-29.
const SERIAL_AFTER_FAKE_LEAP_DAY: f64 = 61.0;

impl ExcelDateTime {
    /// Builds a value from a raw serial number.
    pub const fn new(value: f64, is_1904: bool) -> Self {
        ExcelDateTime { value, is_1904 }
    }

    /// The raw serial number, exactly as stored in the file.
    pub const fn as_f64(&self) -> f64 {
        self.value
    }

    /// Whether this workbook uses the 1904 epoch.
    pub const fn is_1904(&self) -> bool {
        self.is_1904
    }

    /// Whether the serial carries a time-of-day component.
    pub fn has_time(&self) -> bool {
        self.value.is_finite() && (self.value - self.value.trunc()).abs() > f64::EPSILON
    }

    /// Whether the serial is a whole number of days (i.e. a pure date).
    pub fn is_pure_date(&self) -> bool {
        self.value.is_finite() && !self.has_time() && self.value >= 1.0
    }

    /// Converts to days-since-Unix-epoch plus milliseconds-since-midnight.
    fn to_unix_parts(self) -> Option<(i64, u32)> {
        if !self.value.is_finite() {
            return None;
        }
        let offset = if self.is_1904 {
            DAYS_1904_TO_UNIX
        } else if self.value < SERIAL_AFTER_FAKE_LEAP_DAY {
            SERIAL_1900_TO_UNIX_EARLY
        } else {
            SERIAL_1900_TO_UNIX
        };
        let unix_days = self.value - offset;
        if !(-62_000_000.0..=62_000_000.0).contains(&unix_days) {
            return None;
        }
        let mut days = unix_days.floor();
        let mut ms = ((unix_days - days) * 86_400_000.0).round() as i64;
        if ms >= 86_400_000 {
            ms -= 86_400_000;
            days += 1.0;
        }
        if ms < 0 {
            ms += 86_400_000;
            days -= 1.0;
        }
        Some((days as i64, ms as u32))
    }

    /// Breaks the serial into a civil date and time.
    ///
    /// Returns `None` for non-finite or out-of-range serials.
    pub fn to_ymd_hms(&self) -> Option<YmdHms> {
        let (days, ms) = self.to_unix_parts()?;
        let (year, month, day) = civil_from_days(days);
        let total = ms;
        Some(YmdHms {
            year,
            month,
            day,
            hour: (total / 3_600_000) as u8,
            minute: ((total / 60_000) % 60) as u8,
            second: ((total / 1_000) % 60) as u8,
            millisecond: (total % 1_000) as u16,
        })
    }

    /// Seconds since the Unix epoch, truncated.
    pub fn to_unix_timestamp(&self) -> Option<i64> {
        let (days, ms) = self.to_unix_parts()?;
        days.checked_mul(86_400)?.checked_add((ms / 1000) as i64)
    }

    /// Converts to `chrono` types. Requires the `dates` feature.
    #[cfg(feature = "dates")]
    pub fn to_naive_datetime(&self) -> Option<chrono::NaiveDateTime> {
        let parts = self.to_ymd_hms()?;
        chrono::NaiveDate::from_ymd_opt(parts.year, parts.month as u32, parts.day as u32)?
            .and_hms_milli_opt(
                parts.hour as u32,
                parts.minute as u32,
                parts.second as u32,
                parts.millisecond as u32,
            )
    }

    /// Converts to a `chrono` date, discarding any time component. Requires the `dates` feature.
    #[cfg(feature = "dates")]
    pub fn to_naive_date(&self) -> Option<chrono::NaiveDate> {
        let parts = self.to_ymd_hms()?;
        chrono::NaiveDate::from_ymd_opt(parts.year, parts.month as u32, parts.day as u32)
    }

    /// Converts to a `chrono` time, discarding the date. Requires the `dates` feature.
    #[cfg(feature = "dates")]
    pub fn to_naive_time(&self) -> Option<chrono::NaiveTime> {
        let parts = self.to_ymd_hms()?;
        chrono::NaiveTime::from_hms_milli_opt(
            parts.hour as u32,
            parts.minute as u32,
            parts.second as u32,
            parts.millisecond as u32,
        )
    }
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to a proleptic Gregorian date.
pub(crate) fn civil_from_days(z: i64) -> (i32, u8, u8) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    ((y + i64::from(m <= 2)) as i32, m, d)
}

/// The kind of value a cell holds, without borrowing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    /// A whole number. See [`Data::Int`].
    Int,
    /// A fractional number. See [`Data::Float`].
    Float,
    /// Text. See [`Data::String`].
    String,
    /// A boolean. See [`Data::Bool`].
    Bool,
    /// A date and/or time serial. See [`Data::DateTime`].
    DateTime,
    /// An ISO-8601 date/time held as text. See [`Data::DateTimeIso`].
    DateTimeIso,
    /// An ISO-8601 duration held as text. See [`Data::DurationIso`].
    DurationIso,
    /// An in-cell error. See [`Data::Error`].
    Error,
    /// No value. See [`Data::Empty`].
    Empty,
}

/// A single cell value.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Data {
    /// A whole number. Only produced when the source is unambiguous about integrality.
    Int(i64),
    /// A floating point number, including values that are mathematically integral.
    Float(f64),
    /// Text.
    String(String),
    /// A boolean.
    Bool(bool),
    /// A date and/or time stored as an Excel serial.
    DateTime(ExcelDateTime),
    /// An ISO-8601 date/time string that the source format stored as text
    /// (used by ODS `office:date-value`).
    DateTimeIso(String),
    /// An ISO-8601 duration string (used by ODS `office:time-value`).
    DurationIso(String),
    /// An in-cell error such as `#N/A`.
    Error(CellError),
    /// No value. Also used to pad a range out to its full rectangle.
    #[default]
    Empty,
}

impl Data {
    /// The discriminant, for matching without cloning the payload.
    pub fn data_type(&self) -> DataType {
        match self {
            Data::Int(_) => DataType::Int,
            Data::Float(_) => DataType::Float,
            Data::String(_) => DataType::String,
            Data::Bool(_) => DataType::Bool,
            Data::DateTime(_) => DataType::DateTime,
            Data::DateTimeIso(_) => DataType::DateTimeIso,
            Data::DurationIso(_) => DataType::DurationIso,
            Data::Error(_) => DataType::Error,
            Data::Empty => DataType::Empty,
        }
    }

    /// Whether the cell holds no value.
    pub fn is_empty(&self) -> bool {
        matches!(self, Data::Empty)
    }

    /// Whether the cell holds text.
    pub fn is_string(&self) -> bool {
        matches!(self, Data::String(_))
    }

    /// Whether the cell holds a whole number.
    pub fn is_int(&self) -> bool {
        matches!(self, Data::Int(_))
    }

    /// Whether the cell holds a fractional number.
    pub fn is_float(&self) -> bool {
        matches!(self, Data::Float(_))
    }

    /// Whether the cell holds a boolean.
    pub fn is_bool(&self) -> bool {
        matches!(self, Data::Bool(_))
    }

    /// Whether the cell holds a date or time, in either of the two representations.
    pub fn is_datetime(&self) -> bool {
        matches!(self, Data::DateTime(_) | Data::DateTimeIso(_))
    }

    /// Whether the cell holds an in-cell error such as `#N/A`.
    pub fn is_error(&self) -> bool {
        matches!(self, Data::Error(_))
    }

    /// The value as text. Numbers are rendered the way a spreadsheet would show them,
    /// which means integral floats lose their `.0`.
    pub fn as_string(&self) -> Option<String> {
        match self {
            Data::Int(i) => Some(i.to_string()),
            Data::Float(f) => Some(format_float(*f)),
            Data::String(s) => Some(s.clone()),
            Data::Bool(b) => Some(if *b { "TRUE".into() } else { "FALSE".into() }),
            Data::DateTime(dt) => Some(dt.to_ymd_hms().map(|v| v.to_string()).unwrap_or_default()),
            Data::DateTimeIso(s) | Data::DurationIso(s) => Some(s.clone()),
            Data::Error(e) => Some(e.to_string()),
            Data::Empty => None,
        }
    }

    /// The value as a float. Strings are parsed; dates yield their serial; booleans yield 0/1.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Data::Int(i) => Some(*i as f64),
            Data::Float(f) => Some(*f),
            Data::Bool(b) => Some(u8::from(*b) as f64),
            Data::DateTime(dt) => Some(dt.as_f64()),
            Data::String(s) => s.trim().parse::<f64>().ok(),
            Data::DateTimeIso(s) => s.parse::<f64>().ok(),
            Data::DurationIso(_) | Data::Error(_) | Data::Empty => None,
        }
    }

    /// The value as an integer. Text is parsed, and numerics are truncated toward zero.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Data::Int(i) => Some(*i),
            Data::Float(f) if f.is_finite() => Some(*f as i64),
            Data::Bool(b) => Some(i64::from(*b)),
            Data::String(s) => s.trim().parse::<i64>().ok(),
            _ => self.as_f64().map(|f| f as i64),
        }
    }

    /// The value as a boolean. Numbers are non-zero, and text is `TRUE` or `FALSE`.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Data::Bool(b) => Some(*b),
            Data::Int(i) => Some(*i != 0),
            Data::Float(f) => Some(*f != 0.0),
            Data::String(s) => match s.trim().to_ascii_uppercase().as_str() {
                "TRUE" => Some(true),
                "FALSE" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    /// The Excel serial, if this cell is a date/time.
    pub fn as_datetime(&self) -> Option<ExcelDateTime> {
        match self {
            Data::DateTime(dt) => Some(*dt),
            _ => None,
        }
    }

    /// The date portion, if this cell holds a date or an ISO date string.
    pub fn as_date(&self) -> Option<YmdHms> {
        match self {
            Data::DateTime(dt) => dt.to_ymd_hms(),
            Data::DateTimeIso(s) => parse_iso_datetime(s),
            _ => None,
        }
    }

    /// The time-of-day portion as a serial fraction.
    pub fn as_time(&self) -> Option<f64> {
        match self {
            Data::DateTime(dt) => Some(dt.as_f64().rem_euclid(1.0)),
            Data::DurationIso(s) => parse_iso_duration(s),
            _ => None,
        }
    }

    /// The duration in days, from an ISO-8601 duration string.
    pub fn as_duration(&self) -> Option<f64> {
        match self {
            Data::DateTime(dt) => Some(dt.as_f64()),
            Data::DurationIso(s) => parse_iso_duration(s),
            _ => None,
        }
    }
}

/// Renders a float the way a spreadsheet does: `2` rather than `2.0`, but `2.5` unchanged.
pub(crate) fn format_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if f == f.trunc() && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

/// Parses the subset of ISO-8601 date/date-time that ODS produces.
fn parse_iso_datetime(s: &str) -> Option<YmdHms> {
    let s = s.trim();
    let (date, rest) = match s.split_once(['T', ' ']) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut it = date.split('-');
    let year = it.next()?.parse::<i32>().ok()?;
    let month = it.next()?.parse::<u8>().ok()?;
    let day = it.next()?.parse::<u8>().ok()?;
    let (mut hour, mut minute, mut second, mut millisecond) = (0u8, 0u8, 0u8, 0u16);
    if let Some(t) = rest {
        let t = t.trim_end_matches('Z');
        let mut parts = t.split(':');
        hour = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        minute = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        if let Some(sec) = parts.next() {
            match sec.split_once('.') {
                Some((s, frac)) => {
                    second = s.parse().unwrap_or(0);
                    let frac: String = frac.chars().take(3).collect();
                    let padded = format!("{frac:0<3}");
                    millisecond = padded.parse().unwrap_or(0);
                }
                None => second = sec.parse().unwrap_or(0),
            }
        }
    }
    Some(YmdHms {
        year,
        month,
        day,
        hour,
        minute,
        second,
        millisecond,
    })
}

/// Parses `PnYnMnDTnHnMnS` into a number of days. Only the day-and-below components
/// can be represented, so years and months are converted at 365 and 30 days.
fn parse_iso_duration(s: &str) -> Option<f64> {
    let s = s.trim().strip_prefix('P')?;
    let (date_part, time_part) = match s.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut days = 0.0f64;
    let mut num = String::new();
    for ch in date_part.chars() {
        if ch.is_ascii_digit() || ch == '.' || ch == '-' {
            num.push(ch);
            continue;
        }
        let v: f64 = num.parse().ok()?;
        num.clear();
        days += match ch {
            'Y' => v * 365.0,
            'M' => v * 30.0,
            'W' => v * 7.0,
            'D' => v,
            _ => return None,
        };
    }
    if let Some(t) = time_part {
        for ch in t.chars() {
            if ch.is_ascii_digit() || ch == '.' || ch == '-' {
                num.push(ch);
                continue;
            }
            let v: f64 = num.parse().ok()?;
            num.clear();
            days += match ch {
                'H' => v / 24.0,
                'M' => v / 1440.0,
                'S' => v / 86_400.0,
                _ => return None,
            };
        }
    }
    Some(days)
}

impl From<i64> for Data {
    fn from(v: i64) -> Self {
        Data::Int(v)
    }
}

impl From<i32> for Data {
    fn from(v: i32) -> Self {
        Data::Int(v as i64)
    }
}

impl From<u64> for Data {
    fn from(v: u64) -> Self {
        Data::Int(v as i64)
    }
}

impl From<f64> for Data {
    fn from(v: f64) -> Self {
        Data::Float(v)
    }
}

impl From<bool> for Data {
    fn from(v: bool) -> Self {
        Data::Bool(v)
    }
}

impl From<String> for Data {
    fn from(v: String) -> Self {
        Data::String(v)
    }
}

impl From<&str> for Data {
    fn from(v: &str) -> Self {
        Data::String(v.to_owned())
    }
}

impl From<ExcelDateTime> for Data {
    fn from(v: ExcelDateTime) -> Self {
        Data::DateTime(v)
    }
}

impl From<CellError> for Data {
    fn from(v: CellError) -> Self {
        Data::Error(v)
    }
}

impl<T: Into<Data>> From<Option<T>> for Data {
    fn from(v: Option<T>) -> Self {
        match v {
            Some(v) => v.into(),
            None => Data::Empty,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ymd(serial: f64, is_1904: bool) -> (i32, u8, u8) {
        let v = ExcelDateTime::new(serial, is_1904).to_ymd_hms().unwrap();
        (v.year, v.month, v.day)
    }

    #[test]
    fn converts_the_1900_date_system() {
        assert_eq!(ymd(1.0, false), (1900, 1, 1));
        assert_eq!(ymd(2.0, false), (1900, 1, 2));
        assert_eq!(ymd(59.0, false), (1900, 2, 28));
        // Excel counts a 1900-02-29 that never existed, so serial 60 lands on 1900-03-01 and
        // serial 61 does too. This matches every mainstream reader.
        assert_eq!(ymd(60.0, false), (1900, 3, 1));
        assert_eq!(ymd(61.0, false), (1900, 3, 1));
        assert_eq!(ymd(45292.0, false), (2024, 1, 1));
        assert_eq!(ymd(45321.0, false), (2024, 1, 30));
        assert_eq!(ymd(36526.0, false), (2000, 1, 1));
    }

    #[test]
    fn converts_the_1904_date_system() {
        assert_eq!(ymd(0.0, true), (1904, 1, 1));
        assert_eq!(ymd(1.0, true), (1904, 1, 2));
        // 1904 is a leap year, so four years on is 1461 days.
        assert_eq!(ymd(1461.0, true), (1908, 1, 1));
        // The two date systems are 1462 days apart: 45292 - 1462.
        assert_eq!(ymd(43830.0, true), (2024, 1, 1));
        assert_eq!(
            ExcelDateTime::new(43830.0, true).to_unix_timestamp(),
            ExcelDateTime::new(45292.0, false).to_unix_timestamp()
        );
    }

    #[test]
    fn splits_the_time_of_day() {
        let value = ExcelDateTime::new(45321.5, false).to_ymd_hms().unwrap();
        assert_eq!((value.hour, value.minute, value.second), (12, 0, 0));
        assert_eq!(value.to_string(), "2024-01-30 12:00:00");

        let value = ExcelDateTime::new(45321.0 + 13.75 / 24.0, false)
            .to_ymd_hms()
            .unwrap();
        assert_eq!((value.hour, value.minute), (13, 45));
    }

    #[test]
    fn rounding_a_time_near_midnight_rolls_the_day() {
        // A fraction a hair under one day must not spill into the next date oddly: 23:59:59.9
        // rounds to 23:59:59.900, still inside the same day.
        let value = ExcelDateTime::new(45321.0 + 0.999_999, false)
            .to_ymd_hms()
            .unwrap();
        assert_eq!((value.year, value.month, value.day), (2024, 1, 30));
        assert_eq!(value.hour, 23);
    }

    #[test]
    fn has_time_and_pure_date() {
        assert!(ExcelDateTime::new(45321.5, false).has_time());
        assert!(!ExcelDateTime::new(45321.0, false).has_time());
        assert!(ExcelDateTime::new(45321.0, false).is_pure_date());
        assert!(!ExcelDateTime::new(0.5, false).is_pure_date());
    }

    #[test]
    fn non_finite_serials_do_not_convert() {
        for serial in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e300] {
            assert!(ExcelDateTime::new(serial, false).to_ymd_hms().is_none());
            assert!(ExcelDateTime::new(serial, false)
                .to_unix_timestamp()
                .is_none());
        }
    }

    #[test]
    fn converts_to_unix_timestamps() {
        // 2024-01-01T00:00:00Z
        assert_eq!(
            ExcelDateTime::new(45292.0, false).to_unix_timestamp(),
            Some(1_704_067_200)
        );
        // 1904-01-01T00:00:00Z
        assert_eq!(
            ExcelDateTime::new(0.0, true).to_unix_timestamp(),
            Some(-2_082_844_800)
        );
    }

    /// The value the README quotes, so the two cannot drift apart.
    #[test]
    fn converts_a_serial_with_a_time_of_day_to_a_unix_timestamp() {
        assert_eq!(
            ExcelDateTime::new(45321.5, false).to_unix_timestamp(),
            Some(1_706_616_000)
        );
    }

    #[test]
    fn data_accessors() {
        assert_eq!(Data::Int(42).as_string().as_deref(), Some("42"));
        assert_eq!(Data::Float(2.0).as_string().as_deref(), Some("2"));
        assert_eq!(Data::Float(2.5).as_string().as_deref(), Some("2.5"));
        assert_eq!(Data::Bool(true).as_string().as_deref(), Some("TRUE"));
        assert_eq!(Data::String("x".into()).as_string().as_deref(), Some("x"));
        assert_eq!(
            Data::Error(CellError::Div0).as_string().as_deref(),
            Some("#DIV/0!")
        );
        assert_eq!(Data::Empty.as_string(), None);

        assert_eq!(Data::String(" 3.5 ".into()).as_f64(), Some(3.5));
        assert_eq!(Data::Bool(true).as_f64(), Some(1.0));
        assert_eq!(Data::Float(2.9).as_i64(), Some(2));
        assert_eq!(Data::String("TRUE".into()).as_bool(), Some(true));
        assert_eq!(Data::String("nope".into()).as_bool(), None);
        assert_eq!(Data::Int(0).as_bool(), Some(false));
    }

    #[test]
    fn iso_dates_and_durations_parse() {
        let date = Data::DateTimeIso("2024-03-02T13:45:30".into())
            .as_date()
            .unwrap();
        assert_eq!((date.year, date.month, date.day), (2024, 3, 2));
        assert_eq!((date.hour, date.minute, date.second), (13, 45, 30));

        let date = Data::DateTimeIso("2024-03-02".into()).as_date().unwrap();
        assert_eq!((date.hour, date.minute), (0, 0));

        assert_eq!(
            Data::DurationIso("PT1H30M".into()).as_duration(),
            Some(0.0625)
        );
        assert_eq!(Data::DurationIso("P1D".into()).as_duration(), Some(1.0));
        assert_eq!(
            Data::DurationIso("PT30S".into()).as_duration(),
            Some(30.0 / 86400.0)
        );
        assert_eq!(Data::DurationIso("garbage".into()).as_duration(), None);
    }

    #[test]
    fn cell_errors_round_trip_through_their_text() {
        for (text, expected) in [
            ("#DIV/0!", CellError::Div0),
            ("#N/A", CellError::NA),
            ("#NAME?", CellError::Name),
            ("#NULL!", CellError::Null),
            ("#NUM!", CellError::Num),
            ("#REF!", CellError::Ref),
            ("#VALUE!", CellError::Value),
            ("#GETTING_DATA", CellError::GettingData),
        ] {
            assert_eq!(CellError::from_ooxml(text), expected);
            assert_eq!(expected.to_string(), text);
        }
        assert_eq!(
            CellError::from_ooxml("#WEIRD!"),
            CellError::Unknown("#WEIRD!".into())
        );
    }

    #[test]
    fn biff_error_codes_map() {
        assert_eq!(CellError::from_biff(0x07), CellError::Div0);
        assert_eq!(CellError::from_biff(0x2A), CellError::NA);
        assert_eq!(CellError::from_biff(0x00), CellError::Null);
    }

    #[test]
    fn data_types_are_reported() {
        assert_eq!(Data::Int(1).data_type(), DataType::Int);
        assert_eq!(Data::Float(1.0).data_type(), DataType::Float);
        assert_eq!(Data::Empty.data_type(), DataType::Empty);
        assert!(Data::Empty.is_empty());
        assert!(Data::Error(CellError::Ref).is_error());
        assert!(Data::DateTimeIso("x".into()).is_datetime());
    }

    #[test]
    fn conversions_from_primitives() {
        assert_eq!(Data::from(1i64), Data::Int(1));
        assert_eq!(Data::from(1.5f64), Data::Float(1.5));
        assert_eq!(Data::from("a"), Data::String("a".into()));
        assert_eq!(Data::from(true), Data::Bool(true));
        assert_eq!(Data::from(None::<i64>), Data::Empty);
        assert_eq!(Data::from(Some(2i64)), Data::Int(2));
    }
}

use std::fmt;
use std::io;

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong while reading a spreadsheet.
#[derive(Debug)]
pub enum Error {
    /// The underlying reader failed.
    Io(io::Error),
    /// The file is not a container this crate recognises, or its header is damaged.
    Format(String),
    /// The archive container itself could not be read: bad central directory,
    /// truncation, or a failed checksum.
    Container(String),
    /// A structural problem in one of the XML parts.
    Xml(String),
    /// The file is valid but uses something this reader deliberately does not implement.
    Unsupported(String),
    /// The workbook is encrypted.
    Encrypted,
    /// A part that the format requires was not present in the container.
    MissingPart(String),
    /// A sheet, defined name or range the caller asked for does not exist.
    NotFound(String),
    /// Reached the end of a binary stream while a record was still being read.
    Eof,
    /// Free-form error used by the serde layer.
    Msg(String),
}

impl Error {
    pub(crate) fn format<S: Into<String>>(msg: S) -> Self {
        Error::Format(msg.into())
    }

    pub(crate) fn container<S: Into<String>>(msg: S) -> Self {
        Error::Container(msg.into())
    }

    pub(crate) fn xml<S: Into<String>>(msg: S) -> Self {
        Error::Xml(msg.into())
    }

    pub(crate) fn missing<S: Into<String>>(part: S) -> Self {
        Error::MissingPart(part.into())
    }

    pub(crate) fn unsupported<S: Into<String>>(msg: S) -> Self {
        Error::Unsupported(msg.into())
    }

    /// True when the failure is caused by the file's contents rather than by I/O.
    pub fn is_corrupt(&self) -> bool {
        matches!(
            self,
            Error::Format(_)
                | Error::Container(_)
                | Error::Xml(_)
                | Error::MissingPart(_)
                | Error::Eof
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Format(m) => write!(f, "malformed spreadsheet: {m}"),
            Error::Container(m) => write!(f, "malformed container: {m}"),
            Error::Xml(m) => write!(f, "malformed xml: {m}"),
            Error::Unsupported(m) => write!(f, "unsupported feature: {m}"),
            Error::Encrypted => write!(f, "workbook is encrypted; a password is required"),
            Error::MissingPart(m) => write!(f, "missing required part: {m}"),
            Error::NotFound(m) => write!(f, "not found: {m}"),
            Error::Eof => write!(f, "unexpected end of stream"),
            Error::Msg(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        match e {
            Error::Io(e) => e,
            other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
        }
    }
}

impl From<String> for Error {
    fn from(m: String) -> Self {
        Error::Msg(m)
    }
}

impl From<&str> for Error {
    fn from(m: &str) -> Self {
        Error::Msg(m.to_owned())
    }
}

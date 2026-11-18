//! Values, rows and errors shared by both database backends.
//!
//! Statements bind [`Value`]s, built with [`params!`](crate::database::params),
//! and read [`Row`]s of them. A NULL parameter keeps the type of the Rust value
//! it came from (`None::<i64>` binds an integer NULL), so PostgreSQL can type
//! `?1 IS NULL OR column = ?1` without casts.
use std::fmt;

/// The storage class of a value, also the type of a NULL parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Integer,
    Real,
    Text,
    Blob,
}

/// One parameter or column value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null(Kind),
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    pub fn kind(&self) -> Kind {
        match self {
            Self::Null(kind) => *kind,
            Self::Integer(_) => Kind::Integer,
            Self::Real(_) => Kind::Real,
            Self::Text(_) => Kind::Text,
            Self::Blob(_) => Kind::Blob,
        }
    }

    fn describe(&self) -> &'static str {
        match self {
            Self::Null(_) => "NULL",
            Self::Integer(_) => "an integer",
            Self::Real(_) => "a real",
            Self::Text(_) => "text",
            Self::Blob(_) => "a blob",
        }
    }
}

/// A Rust value that binds as a statement parameter.
pub trait ToValue {
    /// The type a `None` of this value binds as.
    const KIND: Kind;
    fn to_value(&self) -> Value;
}

impl<T: ToValue + ?Sized> ToValue for &T {
    const KIND: Kind = T::KIND;
    fn to_value(&self) -> Value {
        (**self).to_value()
    }
}

impl<T: ToValue> ToValue for Option<T> {
    const KIND: Kind = T::KIND;
    fn to_value(&self) -> Value {
        match self {
            Some(value) => value.to_value(),
            None => Value::Null(T::KIND),
        }
    }
}

macro_rules! lossless_integer {
    ($($type:ty),*) => {$(
        impl ToValue for $type {
            const KIND: Kind = Kind::Integer;
            fn to_value(&self) -> Value {
                Value::Integer(i64::from(*self))
            }
        }
    )*};
}
lossless_integer!(i8, i16, i32, i64, u8, u16, u32, bool);

/// Values past `i64::MAX` bind as `i64::MAX`; both databases store signed
/// 64-bit integers.
macro_rules! saturating_integer {
    ($($type:ty),*) => {$(
        impl ToValue for $type {
            const KIND: Kind = Kind::Integer;
            fn to_value(&self) -> Value {
                Value::Integer(i64::try_from(*self).unwrap_or(i64::MAX))
            }
        }
    )*};
}
saturating_integer!(u64, usize);

impl ToValue for f64 {
    const KIND: Kind = Kind::Real;
    fn to_value(&self) -> Value {
        Value::Real(*self)
    }
}

impl ToValue for str {
    const KIND: Kind = Kind::Text;
    fn to_value(&self) -> Value {
        Value::Text(self.to_owned())
    }
}

impl ToValue for String {
    const KIND: Kind = Kind::Text;
    fn to_value(&self) -> Value {
        Value::Text(self.clone())
    }
}

impl ToValue for [u8] {
    const KIND: Kind = Kind::Blob;
    fn to_value(&self) -> Value {
        Value::Blob(self.to_vec())
    }
}

impl ToValue for Vec<u8> {
    const KIND: Kind = Kind::Blob;
    fn to_value(&self) -> Value {
        Value::Blob(self.clone())
    }
}

/// Statement parameters: `params![name, port, None::<String>]`.
macro_rules! params {
    ($($value:expr),* $(,)?) => {
        &[$($crate::database::ToValue::to_value(&$value)),*] as &[$crate::database::Value]
    };
}
pub(crate) use params;

/// A Rust value a column converts to.
pub trait FromValue: Sized {
    fn from_value(value: &Value) -> Result<Self, String>;
}

fn mismatch<T>(value: &Value, wanted: &str) -> Result<T, String> {
    Err(format!("expected {wanted}, found {}", value.describe()))
}

macro_rules! from_integer {
    ($($type:ty),*) => {$(
        impl FromValue for $type {
            fn from_value(value: &Value) -> Result<Self, String> {
                match value {
                    Value::Integer(integer) => <$type>::try_from(*integer)
                        .map_err(|_| format!("integer {integer} is out of range for {}", stringify!($type))),
                    other => mismatch(other, "an integer"),
                }
            }
        }
    )*};
}
from_integer!(i8, i16, i32, i64, u8, u16, u32, u64, usize);

impl FromValue for bool {
    fn from_value(value: &Value) -> Result<Self, String> {
        match value {
            Value::Integer(integer) => Ok(*integer != 0),
            other => mismatch(other, "an integer"),
        }
    }
}

impl FromValue for f64 {
    fn from_value(value: &Value) -> Result<Self, String> {
        match value {
            Value::Real(real) => Ok(*real),
            Value::Integer(integer) => Ok(*integer as f64),
            other => mismatch(other, "a number"),
        }
    }
}

impl FromValue for String {
    fn from_value(value: &Value) -> Result<Self, String> {
        match value {
            Value::Text(text) => Ok(text.clone()),
            other => mismatch(other, "text"),
        }
    }
}

impl FromValue for Vec<u8> {
    fn from_value(value: &Value) -> Result<Self, String> {
        match value {
            Value::Blob(bytes) => Ok(bytes.clone()),
            Value::Text(text) => Ok(text.as_bytes().to_vec()),
            other => mismatch(other, "a blob"),
        }
    }
}

impl<T: FromValue> FromValue for Option<T> {
    fn from_value(value: &Value) -> Result<Self, String> {
        match value {
            Value::Null(_) => Ok(None),
            value => T::from_value(value).map(Some),
        }
    }
}

/// A column position.
pub trait RowIndex {
    fn index(self) -> usize;
}

impl RowIndex for usize {
    fn index(self) -> usize {
        self
    }
}

/// One result row.
#[derive(Debug, Clone, PartialEq)]
pub struct Row(pub(crate) Vec<Value>);

impl Row {
    /// Column `index` (from 0) as `T`.
    pub fn get<I: RowIndex, T: FromValue>(&self, index: I) -> Result<T> {
        let index = index.index();
        let value = self
            .0
            .get(index)
            .ok_or_else(|| Error::Conversion(format!("no column {index}")))?;
        T::from_value(value)
            .map_err(|message| Error::Conversion(format!("column {index}: {message}")))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn values(&self) -> &[Value] {
        &self.0
    }
}

/// Every failure of a statement.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// `query_row` found no row.
    NoRows,
    Sqlite(rusqlite::Error),
    Postgres(tokio_postgres::Error),
    /// A column did not convert to the requested type.
    Conversion(String),
    /// Anything else, such as a lost connection or a timeout.
    Other(String),
}

impl Error {
    pub(crate) fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRows => f.write_str("query returned no rows"),
            Self::Sqlite(error) => error.fmt(f),
            Self::Postgres(error) => match error.as_db_error() {
                Some(db) => {
                    write!(f, "{}: {}", db.severity(), db.message())?;
                    if let Some(detail) = db.detail() {
                        write!(f, " ({detail})")?;
                    }
                    Ok(())
                }
                None => error.fmt(f),
            },
            Self::Conversion(message) | Self::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        match error {
            rusqlite::Error::QueryReturnedNoRows => Self::NoRows,
            error => Self::Sqlite(error),
        }
    }
}

impl From<tokio_postgres::Error> for Error {
    fn from(error: tokio_postgres::Error) -> Self {
        Self::Postgres(error)
    }
}

impl From<Error> for std::io::Error {
    fn from(error: Error) -> Self {
        std::io::Error::other(error.to_string())
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// `query_row(...).optional()`: no row is `None` rather than an error.
pub trait OptionalExtension<T> {
    fn optional(self) -> Result<Option<T>>;
}

impl<T> OptionalExtension<T> for Result<T> {
    fn optional(self) -> Result<Option<T>> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(Error::NoRows) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

impl rusqlite::ToSql for Value {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        use rusqlite::types::{ToSqlOutput, Value as Owned, ValueRef};
        Ok(match self {
            Self::Null(_) => ToSqlOutput::Owned(Owned::Null),
            Self::Integer(integer) => ToSqlOutput::Owned(Owned::Integer(*integer)),
            Self::Real(real) => ToSqlOutput::Owned(Owned::Real(*real)),
            Self::Text(text) => ToSqlOutput::Borrowed(ValueRef::Text(text.as_bytes())),
            Self::Blob(bytes) => ToSqlOutput::Borrowed(ValueRef::Blob(bytes)),
        })
    }
}

impl Value {
    pub(crate) fn from_sqlite(value: rusqlite::types::ValueRef<'_>) -> Result<Self> {
        use rusqlite::types::ValueRef;
        Ok(match value {
            ValueRef::Null => Self::Null(Kind::Text),
            ValueRef::Integer(integer) => Self::Integer(integer),
            ValueRef::Real(real) => Self::Real(real),
            ValueRef::Text(bytes) => Self::Text(
                String::from_utf8(bytes.to_vec())
                    .map_err(|error| Error::Conversion(format!("invalid UTF-8 text: {error}")))?,
            ),
            ValueRef::Blob(bytes) => Self::Blob(bytes.to_vec()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_keep_the_type_of_their_nulls() {
        let none: Option<&str> = None;
        let values = params![1u16, "a", none, None::<i64>, true, Some(vec![1u8])];
        assert_eq!(
            values,
            [
                Value::Integer(1),
                Value::Text("a".into()),
                Value::Null(Kind::Text),
                Value::Null(Kind::Integer),
                Value::Integer(1),
                Value::Blob(vec![1]),
            ]
        );
        assert_eq!(params![u64::MAX], [Value::Integer(i64::MAX)]);
    }

    #[test]
    fn columns_convert_with_range_checks() {
        let row = Row(vec![
            Value::Integer(70000),
            Value::Null(Kind::Integer),
            Value::Text("x".into()),
        ]);
        assert_eq!(row.get::<_, i64>(0).unwrap(), 70000);
        assert!(row.get::<_, u16>(0).is_err());
        assert_eq!(row.get::<_, Option<i64>>(1).unwrap(), None);
        assert!(row.get::<_, i64>(1).is_err());
        assert_eq!(row.get::<_, String>(2).unwrap(), "x");
        assert!(row.get::<_, String>(3).is_err());
        assert!(matches!(Err::<(), _>(Error::NoRows).optional(), Ok(None)));
    }
}

//! One representation for every value a driver can produce.
//!
//! The property that matters most here is that an unknown type can never crash
//! the client: anything a driver cannot decode becomes [`Value::Opaque`],
//! carrying whatever textual form it managed to obtain.

use time::{Date, OffsetDateTime, PrimitiveDateTime, Time};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// Kept as text so that arbitrary-precision numerics survive the trip.
    Decimal(String),
    Text(String),
    Bytes(Vec<u8>),
    Date(Date),
    Time(Time),
    /// A timestamp without a time zone. PostgreSQL `timestamp`, BigQuery
    /// `DATETIME`.
    Timestamp(PrimitiveDateTime),
    /// A timestamp with a time zone. PostgreSQL `timestamptz`, BigQuery
    /// `TIMESTAMP`.
    TimestampTz(OffsetDateTime),
    Json(serde_json::Value),
    Array(Vec<Value>),
    /// BigQuery `RECORD`, PostgreSQL composite types.
    Struct(Vec<(String, Value)>),
    /// A type this driver could not decode. Never a failure.
    Opaque {
        type_name: String,
        text: String,
    },
}

impl Value {
    #[must_use]
    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Whether the value should be laid out as a number, i.e. right-aligned
    /// with digits lined up.
    #[must_use]
    pub const fn is_numeric(&self) -> bool {
        matches!(self, Self::Int(_) | Self::Float(_) | Self::Decimal(_))
    }

    /// Whether the value has internal structure worth opening in a detail view
    /// rather than reading inline.
    #[must_use]
    pub const fn is_composite(&self) -> bool {
        matches!(self, Self::Json(_) | Self::Array(_) | Self::Struct(_))
    }

    /// A short name for the shape of the value, for diagnostics and tests.
    #[must_use]
    pub fn type_name(&self) -> &str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "bool",
            Self::Int(_) => "int",
            Self::Float(_) => "float",
            Self::Decimal(_) => "decimal",
            Self::Text(_) => "text",
            Self::Bytes(_) => "bytes",
            Self::Date(_) => "date",
            Self::Time(_) => "time",
            Self::Timestamp(_) => "timestamp",
            Self::TimestampTz(_) => "timestamptz",
            Self::Json(_) => "json",
            Self::Array(_) => "array",
            Self::Struct(_) => "struct",
            Self::Opaque { type_name, .. } => type_name,
        }
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Self::Text(v.to_owned())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Self::Text(v)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Self::Null, Into::into)
    }
}

#[cfg(test)]
mod tests {
    use time::macros::{date, datetime, time};

    use super::*;

    #[test]
    fn classification_drives_layout() {
        assert!(Value::Int(1).is_numeric());
        assert!(Value::Decimal("1.0".into()).is_numeric());
        assert!(!Value::Text("1".into()).is_numeric());

        assert!(Value::Array(vec![]).is_composite());
        assert!(!Value::Bytes(vec![]).is_composite());
    }

    #[test]
    fn an_unknown_type_reports_its_own_name() {
        let v = Value::Opaque {
            type_name: "geometry".into(),
            text: "POINT(0 0)".into(),
        };
        assert_eq!(v.type_name(), "geometry");
        assert!(!v.is_null());
    }

    #[test]
    fn none_becomes_null() {
        let v: Value = Option::<i64>::None.into();
        assert!(v.is_null());

        let v: Value = Some(3_i64).into();
        assert_eq!(v, Value::Int(3));
    }

    #[test]
    fn timestamps_keep_their_zone_distinction() {
        // A driver that collapses these two loses whether the value was
        // anchored to an instant or to a wall clock.
        let naive = Value::Timestamp(datetime!(2026-01-02 10:00));
        let zoned = Value::TimestampTz(datetime!(2026-01-02 10:00 UTC));
        assert_ne!(naive.type_name(), zoned.type_name());
    }

    #[test]
    fn dates_and_times_are_separate_variants() {
        assert_eq!(Value::Date(date!(2026 - 01 - 02)).type_name(), "date");
        assert_eq!(Value::Time(time!(10:00:00)).type_name(), "time");
    }
}

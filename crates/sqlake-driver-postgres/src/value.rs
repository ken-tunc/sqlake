//! Turning what the wire carries into [`Value`], without ever failing.
//!
//! Every column arrives as a [`RawValue`]: the type the server declared and
//! the bytes it sent. Nothing is decoded during the read, so a type this
//! driver has never heard of cannot fail a query — it becomes
//! [`Value::Opaque`] and is displayed as text.
//!
//! That is the whole point. PostgreSQL is an extension ecosystem: PostGIS,
//! `citext`, `ltree`, enums, domains, composite types, and whatever the last
//! person to run a migration invented. A client that decodes strictly and
//! errors on the rest is a client that cannot open half the tables it is
//! pointed at.

use postgres_protocol::types;
use sqlake_core::value::Value;
use time::{Date, Duration, PrimitiveDateTime, Time};
use tokio_postgres::types::{FromSql, Type};

/// A column value exactly as it came off the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawValue {
    ty: Type,
    /// `None` is SQL `NULL`, which is not the same as an empty byte string.
    bytes: Option<Vec<u8>>,
}

impl RawValue {
    #[must_use]
    pub const fn new(ty: Type, bytes: Option<Vec<u8>>) -> Self {
        Self { ty, bytes }
    }

    /// The declared type, whether or not this driver can decode it.
    #[must_use]
    pub const fn ty(&self) -> &Type {
        &self.ty
    }

    /// Decode into the shared value model. Cannot fail: see the module note.
    #[must_use]
    pub fn decode(&self) -> Value {
        let Some(bytes) = self.bytes.as_deref() else {
            return Value::Null;
        };
        decode(&self.ty, bytes).unwrap_or_else(|| self.opaque(bytes))
    }

    /// What a value looks like when the driver has no idea what it is.
    ///
    /// Many unknown types — enums, domains over text, `citext`, `ltree`, XML —
    /// send their text form as their binary form, so trying UTF-8 first
    /// displays them correctly by accident. Everything else is shown the way
    /// `psql` shows a `bytea`, which at least says *something* true about the
    /// bytes rather than a replacement character per byte.
    ///
    /// Valid UTF-8 is not enough on its own to call something text: the binary
    /// form of `interval`, `inet`, `money`, `timetz` and every array is mostly
    /// bytes below `0x80`, so it decodes cleanly into a string of NULs and
    /// control characters. That is not readable and it is not true — those go
    /// to hex with the rest.
    fn opaque(&self, bytes: &[u8]) -> Value {
        let text = match std::str::from_utf8(bytes) {
            Ok(text) if !text.chars().any(is_unprintable) => text.to_owned(),
            _ => {
                let mut hex = String::with_capacity(2 + bytes.len() * 2);
                hex.push_str("\\x");
                for byte in bytes {
                    use std::fmt::Write as _;
                    let _ = write!(hex, "{byte:02x}");
                }
                hex
            }
        };
        Value::Opaque {
            type_name: self.ty.name().to_owned(),
            text,
        }
    }
}

/// What disqualifies a byte string from being shown as text.
///
/// Tabs and newlines do not: they are ordinary contents of an XML document or
/// a domain over `text`, and the front-end already has a glyph for them.
fn is_unprintable(ch: char) -> bool {
    ch.is_control() && !matches!(ch, '\n' | '\r' | '\t')
}

impl<'a> FromSql<'a> for RawValue {
    /// Everything. Refusing a type here is what this whole module exists to
    /// avoid: the refusal would arrive as a failed query rather than as one
    /// odd-looking cell.
    fn accepts(_: &Type) -> bool {
        true
    }

    fn from_sql(
        ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Self::new(ty.clone(), Some(raw.to_vec())))
    }

    fn from_sql_null(ty: &Type) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Self::new(ty.clone(), None))
    }
}

/// `None` means "not a type this driver decodes", which is not an error.
fn decode(ty: &Type, bytes: &[u8]) -> Option<Value> {
    let value = match *ty {
        Type::BOOL => Value::Bool(types::bool_from_sql(bytes).ok()?),
        Type::INT2 => Value::Int(types::int2_from_sql(bytes).ok()?.into()),
        Type::INT4 => Value::Int(types::int4_from_sql(bytes).ok()?.into()),
        Type::INT8 => Value::Int(types::int8_from_sql(bytes).ok()?),
        Type::OID => Value::Int(types::oid_from_sql(bytes).ok()?.into()),
        Type::FLOAT4 => Value::Float(widen(types::float4_from_sql(bytes).ok()?)),
        Type::FLOAT8 => Value::Float(types::float8_from_sql(bytes).ok()?),
        Type::NUMERIC => Value::Decimal(numeric(bytes)?),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => {
            Value::Text(types::text_from_sql(bytes).ok()?.to_owned())
        }
        Type::BYTEA => Value::Bytes(types::bytea_from_sql(bytes).to_vec()),
        Type::UUID => Value::Text(uuid(types::uuid_from_sql(bytes).ok()?)),
        Type::JSON | Type::JSONB => Value::Json(json(ty, bytes)?),
        Type::DATE => Value::Date(date(types::date_from_sql(bytes).ok()?)?),
        Type::TIME => Value::Time(time_of_day(types::time_from_sql(bytes).ok()?)?),
        Type::TIMESTAMP => Value::Timestamp(timestamp(types::timestamp_from_sql(bytes).ok()?)?),
        Type::TIMESTAMPTZ => {
            Value::TimestampTz(timestamp(types::timestamp_from_sql(bytes).ok()?)?.assume_utc())
        }
        _ => return None,
    };
    Some(value)
}

/// A `real` as the `double precision` that means the same thing *to a reader*.
///
/// `f64::from(1.1f32)` is 1.100000023841858, and the front-end formats an
/// `f64` with the shortest text that round-trips as an `f64` — so widening
/// directly turns every `real` column into a wall of noise digits that were
/// never in the value. Going via the `f32`'s own shortest form keeps the
/// number `psql` prints.
fn widen(value: f32) -> f64 {
    value.to_string().parse().unwrap_or_else(|_| value.into())
}

/// PostgreSQL counts from 2000-01-01, not from the Unix epoch.
const PG_EPOCH: Date = time::macros::date!(2000 - 01 - 01);

fn date(days: i32) -> Option<Date> {
    PG_EPOCH.checked_add(Duration::days(days.into()))
}

/// Microseconds since midnight.
///
/// PostgreSQL also allows `24:00:00`, which `time::Time` does not represent;
/// that one value comes back as [`Value::Opaque`] rather than as midnight of a
/// day nobody named.
fn time_of_day(micros: i64) -> Option<Time> {
    let micros = u64::try_from(micros).ok()?;
    let seconds = micros / 1_000_000;
    Time::from_hms_micro(
        u8::try_from(seconds / 3600).ok()?,
        u8::try_from((seconds % 3600) / 60).ok()?,
        u8::try_from(seconds % 60).ok()?,
        u32::try_from(micros % 1_000_000).ok()?,
    )
    .ok()
}

fn timestamp(micros: i64) -> Option<PrimitiveDateTime> {
    let epoch = PrimitiveDateTime::new(PG_EPOCH, Time::MIDNIGHT);
    epoch.checked_add(Duration::microseconds(micros))
}

fn uuid(bytes: [u8; 16]) -> String {
    let mut text = String::with_capacity(36);
    for (i, byte) in bytes.iter().enumerate() {
        use std::fmt::Write as _;
        if matches!(i, 4 | 6 | 8 | 10) {
            text.push('-');
        }
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// `jsonb` puts a format version byte in front of the text; `json` does not.
fn json(ty: &Type, bytes: &[u8]) -> Option<serde_json::Value> {
    let text = if *ty == Type::JSONB {
        match bytes.split_first() {
            // Version 1 is the only one there has ever been. A different one
            // means a server newer than this code, and guessing at its layout
            // would be worse than showing the bytes.
            Some((1, rest)) => rest,
            _ => return None,
        }
    } else {
        bytes
    };
    serde_json::from_slice(text).ok()
}

/// Decode `numeric` into its decimal text.
///
/// Kept as text because that is the only lossless place to put it: `f64` is
/// wrong for money, and every fixed-width decimal type has a precision this
/// one can exceed. The wire format is a sign, a base-10000 digit array, the
/// position of the decimal point, and how many fractional digits to display.
fn numeric(mut bytes: &[u8]) -> Option<String> {
    use std::fmt::Write as _;

    fn i16_at(bytes: &mut &[u8]) -> Option<i16> {
        let (head, rest) = bytes.split_at_checked(2)?;
        *bytes = rest;
        Some(i16::from_be_bytes([head[0], head[1]]))
    }

    let ndigits = i16_at(&mut bytes)?;
    let weight = i16_at(&mut bytes)?;
    let sign = i16_at(&mut bytes)? as u16;
    let dscale = i16_at(&mut bytes)?;

    // NaN and the infinities are values a numeric column can actually hold,
    // and none of them has digits to read.
    match sign {
        0x4000 | 0x0000 => {}
        0xC000 => return Some("NaN".to_owned()),
        0xD000 => return Some("Infinity".to_owned()),
        0xF000 => return Some("-Infinity".to_owned()),
        _ => return None,
    }

    let digits: Vec<i16> = (0..ndigits)
        .map(|_| i16_at(&mut bytes))
        .collect::<Option<_>>()?;

    // The digits are groups of four, and `weight` says which group sits just
    // left of the decimal point. A negative weight means the number is
    // smaller than one and has no whole part at all — which is where the
    // first version of this went wrong, reading 0.01 as 100.01.
    let mut text = String::new();
    if sign == 0x4000 {
        text.push('-');
    }
    if weight < 0 {
        text.push('0');
    } else {
        for group in 0..=weight {
            let digit = digits.get(group as usize).copied().unwrap_or(0);
            if group == 0 {
                let _ = write!(text, "{digit}");
            } else {
                // Only the first group is written as-is; the rest are padded,
                // because 1_0001 is two groups and 10001, not 11.
                let _ = write!(text, "{digit:04}");
            }
        }
    }

    if dscale > 0 {
        // Groups continue from just right of the point. A group index below
        // zero is a gap the sender did not transmit — 0.00000001 arrives as
        // one group with weight -2 — and reads as four zeros.
        let mut fraction = String::new();
        let mut group = i32::from(weight) + 1;
        while fraction.len() < dscale as usize {
            let digit = usize::try_from(group)
                .ok()
                .and_then(|g| digits.get(g).copied())
                .unwrap_or(0);
            let _ = write!(fraction, "{digit:04}");
            group += 1;
        }
        fraction.truncate(dscale as usize);
        text.push('.');
        text.push_str(&fraction);
    }

    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_postgres::types::{Kind, ToSql};

    /// Encode with the same library the server speaks, so the test is about
    /// this module rather than about a byte string somebody typed in.
    fn wire<T: ToSql>(ty: &Type, value: T) -> RawValue {
        let mut buf = bytes::BytesMut::new();
        value.to_sql(ty, &mut buf).expect("should encode");
        RawValue::new(ty.clone(), Some(buf.to_vec()))
    }

    fn raw(ty: Type, bytes: &[u8]) -> RawValue {
        RawValue::new(ty, Some(bytes.to_vec()))
    }

    #[test]
    fn null_is_not_an_empty_string() {
        assert_eq!(RawValue::new(Type::TEXT, None).decode(), Value::Null);
        assert_eq!(
            raw(Type::TEXT, b"").decode(),
            Value::Text(String::new()),
            "an empty text value is a value"
        );
    }

    #[test]
    fn the_ordinary_types_come_back_as_themselves() {
        assert_eq!(wire(&Type::BOOL, true).decode(), Value::Bool(true));
        assert_eq!(wire(&Type::INT2, 7i16).decode(), Value::Int(7));
        assert_eq!(wire(&Type::INT4, -42i32).decode(), Value::Int(-42));
        assert_eq!(wire(&Type::INT8, i64::MIN).decode(), Value::Int(i64::MIN));
        assert_eq!(wire(&Type::FLOAT8, 1.5f64).decode(), Value::Float(1.5));
        assert_eq!(
            wire(&Type::TEXT, "hello").decode(),
            Value::Text("hello".to_owned())
        );
        assert_eq!(
            wire(&Type::BYTEA, &b"\x00\xff"[..]).decode(),
            Value::Bytes(vec![0, 255])
        );
    }

    #[test]
    fn timestamps_count_from_2000_not_from_1970() {
        // The single most common way to be wrong by thirty years.
        let stamp = time::macros::datetime!(2026-08-15 12:34:56);
        assert_eq!(
            wire(&Type::TIMESTAMP, stamp).decode(),
            Value::Timestamp(stamp)
        );
        assert_eq!(
            wire(&Type::DATE, time::macros::date!(1999 - 12 - 31)).decode(),
            Value::Date(time::macros::date!(1999 - 12 - 31))
        );
        assert_eq!(
            wire(&Type::TIME, time::macros::time!(23:59:59.999999)).decode(),
            Value::Time(time::macros::time!(23:59:59.999999))
        );
        assert_eq!(
            wire(&Type::TIMESTAMPTZ, stamp.assume_utc()).decode(),
            Value::TimestampTz(stamp.assume_utc())
        );
    }

    #[test]
    fn json_and_jsonb_are_the_same_document() {
        let doc = serde_json::json!({"a": [1, 2], "b": null});
        assert_eq!(wire(&Type::JSON, &doc).decode(), Value::Json(doc.clone()));
        assert_eq!(wire(&Type::JSONB, &doc).decode(), Value::Json(doc));
    }

    #[test]
    fn a_jsonb_version_this_code_does_not_know_stays_opaque() {
        // Guessing at the layout of a format from a newer server is worse than
        // showing what arrived.
        let value = raw(Type::JSONB, b"\x02{}").decode();
        assert!(matches!(value, Value::Opaque { .. }), "{value:?}");
    }

    #[test]
    fn numerics_keep_every_digit() {
        // Reading these as f64 is how money loses a cent.
        let cases = [
            // Values under one, which is where the first version of this
            // read 0.01 as 100.01: the digit groups are the same either way
            // and only the weight says where the point goes.
            "0",
            "1",
            "-1",
            "0.01",
            "0.0001",
            "0.000000000000000001",
            "-0.5",
            // And over one, where a second group must be zero-padded: 10001
            // is two groups and is not 11.
            "12345.6789",
            "10000",
            "10001",
            "1234567890123456789",
            "-99999.99",
        ];
        for text in cases {
            let value: rust_decimal::Decimal = text.parse().expect("a numeric");
            assert_eq!(
                wire(&Type::NUMERIC, value).decode(),
                Value::Decimal(text.to_owned()),
                "{text}"
            );
        }
    }

    #[test]
    fn the_values_a_numeric_can_hold_that_are_not_numbers() {
        // sign words, with no digits behind them
        assert_eq!(
            raw(Type::NUMERIC, &[0, 0, 0, 0, 0xC0, 0x00, 0, 0]).decode(),
            Value::Decimal("NaN".to_owned())
        );
        assert_eq!(
            raw(Type::NUMERIC, &[0, 0, 0, 0, 0xD0, 0x00, 0, 0]).decode(),
            Value::Decimal("Infinity".to_owned())
        );
    }

    #[test]
    fn a_type_this_driver_never_heard_of_is_shown_not_refused() {
        // A `citext`, an enum, a domain: text in a type this code does not
        // list. Decoding is not allowed to fail, so it arrives readable.
        let odd = Type::new(
            "citext".to_owned(),
            17_000,
            Kind::Simple,
            "public".to_owned(),
        );
        assert_eq!(
            raw(odd.clone(), b"Hello").decode(),
            Value::Opaque {
                type_name: "citext".to_owned(),
                text: "Hello".to_owned(),
            }
        );

        // And one whose bytes are not text at all.
        let Value::Opaque { text, .. } = raw(odd, &[0x00, 0xff]).decode() else {
            panic!("expected opaque");
        };
        assert_eq!(text, "\\x00ff");
    }

    #[test]
    fn a_binary_builtin_goes_to_hex_rather_than_to_control_characters() {
        // `interval`, `inet`, `money`, `timetz` and every array are binary and
        // almost entirely below 0x80, so they are *valid UTF-8* — believing
        // that would show a cell of NULs where the hex at least describes what
        // arrived.
        let Value::Opaque { text, .. } = raw(Type::INTERVAL, &[0, 0, 0, 1, 0, 0, 0, 0]).decode()
        else {
            panic!("expected opaque");
        };
        assert_eq!(text, "\\x0000000100000000");

        // A newline is not that: it belongs to plenty of real text values.
        let Value::Opaque { text, .. } = raw(Type::XML, b"<a>\n</a>").decode() else {
            panic!("expected opaque");
        };
        assert_eq!(text, "<a>\n</a>");
    }

    #[test]
    fn a_real_keeps_the_digits_it_was_written_with() {
        // Widening straight to f64 makes 1.1 into 1.100000023841858, and the
        // front-end prints an f64 with every digit it needs to round-trip.
        assert_eq!(wire(&Type::FLOAT4, 1.1f32).decode(), Value::Float(1.1));
        assert_eq!(wire(&Type::FLOAT4, 0.1f32).decode(), Value::Float(0.1));
        assert_eq!(
            wire(&Type::FLOAT4, f32::INFINITY).decode(),
            Value::Float(f64::INFINITY)
        );
        let Value::Float(nan) = wire(&Type::FLOAT4, f32::NAN).decode() else {
            panic!("expected a float");
        };
        assert!(nan.is_nan());
    }

    #[test]
    fn a_known_type_with_impossible_bytes_is_opaque_rather_than_an_error() {
        // A four-byte integer that arrived with two bytes. Something is very
        // wrong, and taking the whole query down over one cell is worse.
        let value = raw(Type::INT4, &[0, 1]).decode();
        assert!(matches!(value, Value::Opaque { .. }), "{value:?}");
    }

    #[test]
    fn uuids_are_formatted_the_way_postgres_prints_them() {
        let bytes = [
            0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc,
            0xde, 0xf0,
        ];
        assert_eq!(
            raw(Type::UUID, &bytes).decode(),
            Value::Text("12345678-9abc-def0-1234-56789abcdef0".to_owned())
        );
    }
}

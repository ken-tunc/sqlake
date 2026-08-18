//! Decoding what `tabledata.list` sends.
//!
//! Everything arrives as JSON, and almost everything arrives as a JSON
//! *string*: an `INT64` is `"42"`, a `BOOL` is `"true"`, a `TIMESTAMP` is
//! `"1755432000.0"`. The type is not in the row at all — it is in the table's
//! schema, fetched separately — so decoding is always "this text, read as that
//! type".
//!
//! **A type this driver cannot read must never fail a preview.** Anything that
//! does not decode becomes [`Value::Opaque`] carrying the text it arrived as,
//! the same rule the PostgreSQL driver follows for an unknown OID. A table
//! with one `INTERVAL` column in it is still a table worth looking at.

use base64::Engine as _;
use gcp_bigquery_client::model::field_type::FieldType;
use gcp_bigquery_client::model::table_field_schema::TableFieldSchema;
use sqlake_core::value::Value;
use time::{Date, OffsetDateTime, PrimitiveDateTime, Time};

/// Read one cell against the field it belongs to.
///
/// `REPEATED` is checked before the type, because the mode is what decides the
/// shape: a repeated `STRING` arrives as an array of cells, not as a string.
#[must_use]
pub fn decode(json: Option<&serde_json::Value>, field: &TableFieldSchema) -> Value {
    let Some(json) = json else {
        return Value::Null;
    };
    if json.is_null() {
        return Value::Null;
    }
    if is_repeated(field) {
        return array(json, field);
    }
    scalar(Some(json), field)
}

fn is_repeated(field: &TableFieldSchema) -> bool {
    field.mode.as_deref() == Some("REPEATED")
}

/// `[{"v": …}, {"v": …}]` — one wrapper object per element.
fn array(json: &serde_json::Value, field: &TableFieldSchema) -> Value {
    let Some(elements) = json.as_array() else {
        return opaque(field, json);
    };
    Value::Array(
        elements
            .iter()
            .map(|element| scalar(unwrap_v(element), field))
            .collect(),
    )
}

/// `{"f": [{"v": …}, …]}` — the same envelope a row uses, one level down.
fn record(json: &serde_json::Value, field: &TableFieldSchema) -> Value {
    let fields = field.fields.as_deref().unwrap_or_default();
    let Some(cells) = json.get("f").and_then(|f| f.as_array()) else {
        return opaque(field, json);
    };
    // Walked over the schema and not over the cells, for the reason a row is:
    // the API sends the values in schema order and does not repeat the names,
    // so a record with fewer cells than fields still has to keep the names of
    // the ones it did send.
    Value::Struct(
        fields
            .iter()
            .enumerate()
            .map(|(index, field)| {
                let cell = cells.get(index).and_then(unwrap_v);
                (field.name.clone(), decode(cell, field))
            })
            .collect(),
    )
}

/// Every value is wrapped in `{"v": …}`, including the elements of an array.
fn unwrap_v(cell: &serde_json::Value) -> Option<&serde_json::Value> {
    cell.get("v").filter(|v| !v.is_null())
}

fn scalar(json: Option<&serde_json::Value>, field: &TableFieldSchema) -> Value {
    let Some(json) = json else {
        return Value::Null;
    };
    match field.r#type {
        FieldType::Record | FieldType::Struct => return record(json, field),
        // A `JSON` column arrives as a string holding a document, so it is
        // parsed rather than read.
        FieldType::Json => {
            let Some(text) = json.as_str() else {
                return opaque(field, json);
            };
            // The document as it arrived, not `json.to_string()`: that would
            // re-quote and re-escape it, so a document that failed to parse
            // would be shown as something it never was.
            return serde_json::from_str(text)
                .map_or_else(|_| opaque_text(field, text), Value::Json);
        }
        _ => {}
    }

    let Some(text) = json.as_str() else {
        // Not a string, and not one of the two shapes above: the API has sent
        // something this decoder does not model. Its own JSON is the most
        // faithful text available for it.
        return opaque(field, json);
    };

    match field.r#type {
        FieldType::String => Value::Text(text.to_owned()),
        FieldType::Integer | FieldType::Int64 => parsed(text, field, |t| t.parse().map(Value::Int)),
        FieldType::Float | FieldType::Float64 => {
            parsed(text, field, |t| t.parse().map(Value::Float))
        }
        // Kept as text: `NUMERIC` is 38 digits and `BIGNUMERIC` is 77, and
        // both would lose their tail as an `f64`.
        FieldType::Numeric | FieldType::Bignumeric => Value::Decimal(text.to_owned()),
        FieldType::Boolean | FieldType::Bool => match text {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => opaque_text(field, text),
        },
        FieldType::Bytes => base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_or_else(|_| opaque_text(field, text), Value::Bytes),
        FieldType::Date => parsed(text, field, |t| Date::parse(t, DATE).map(Value::Date)),
        FieldType::Time => time_of(text).unwrap_or_else(|| opaque_text(field, text)),
        FieldType::Datetime => datetime_of(text).unwrap_or_else(|| opaque_text(field, text)),
        FieldType::Timestamp => timestamp_of(text).unwrap_or_else(|| opaque_text(field, text)),
        // Known, and deliberately not modelled. `GEOGRAPHY` is WKT and
        // `INTERVAL` is a duration literal; both are readable as they stand,
        // and `Opaque` keeps the type name beside the text where `Text` would
        // claim they are strings.
        FieldType::Geography | FieldType::Interval => opaque_text(field, text),
        // Reached only by the arms handled above; listed so that a new variant
        // in the crate stops this compiling.
        FieldType::Record | FieldType::Struct | FieldType::Json => opaque_text(field, text),
    }
}

fn parsed<E>(
    text: &str,
    field: &TableFieldSchema,
    read: impl Fn(&str) -> Result<Value, E>,
) -> Value {
    read(text).unwrap_or_else(|_| opaque_text(field, text))
}

const DATE: &[time::format_description::BorrowedFormatItem<'_>] =
    time::macros::format_description!("[year]-[month]-[day]");
const TIME: &[time::format_description::BorrowedFormatItem<'_>] =
    time::macros::format_description!("[hour]:[minute]:[second]");
const TIME_SUB: &[time::format_description::BorrowedFormatItem<'_>] =
    time::macros::format_description!("[hour]:[minute]:[second].[subsecond]");
const DATETIME: &[time::format_description::BorrowedFormatItem<'_>] =
    time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");
const DATETIME_SUB: &[time::format_description::BorrowedFormatItem<'_>] =
    time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond]");

/// The fraction is present only when the value has one, and `time` has no way
/// to describe an optional component — so both spellings are tried.
fn time_of(text: &str) -> Option<Value> {
    Time::parse(text, TIME_SUB)
        .or_else(|_| Time::parse(text, TIME))
        .map(Value::Time)
        .ok()
}

fn datetime_of(text: &str) -> Option<Value> {
    PrimitiveDateTime::parse(text, DATETIME_SUB)
        .or_else(|_| PrimitiveDateTime::parse(text, DATETIME))
        .map(Value::Timestamp)
        .ok()
}

/// Seconds since the epoch: `"1755432000.123456"`, and also `"1.755432E9"`,
/// which is how a JSON writer renders the same double.
///
/// The API can send microseconds as an integer instead, which would make all
/// of this unnecessary — but `formatOptions.useInt64Timestamp` is a nested
/// parameter, and `gcp-bigquery-client` puts its query parameters through
/// `serde_urlencoded`, which refuses to serialise a nested struct. Setting it
/// fails the request before it leaves the machine, so the double is what there
/// is to read.
fn timestamp_of(text: &str) -> Option<Value> {
    OffsetDateTime::from_unix_timestamp_nanos(nanos_since_epoch(text)?)
        .map(Value::TimestampTz)
        .ok()
}

/// A decimal number of seconds, scaled to nanoseconds by moving its point.
///
/// Digit by digit rather than through an `f64`, because a mantissa runs out
/// around the microsecond at present-day epochs — which is exactly the digit
/// BigQuery keeps.
fn nanos_since_epoch(text: &str) -> Option<i128> {
    let (mantissa, exponent) = match text.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i32>().ok()?),
        None => (text, 0),
    };
    // The sign travels with the digits rather than being pulled out: joining
    // `-0` to `5` gives `-05`, which parses to -5. Reading the whole part on
    // its own is what loses it — `-0` is zero, and every instant in the second
    // before the epoch has that whole part.
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits: i128 = format!("{whole}{fraction}").parse().ok()?;

    // Where the point ends up once the fraction and the exponent have both
    // moved it, counted in powers of ten away from nanoseconds.
    let shift = 9 - i32::try_from(fraction.len()).ok()? + exponent;
    let scale = 10_i128.checked_pow(shift.unsigned_abs())?;
    let nanos = if shift >= 0 {
        digits.checked_mul(scale)?
    } else {
        // Below a nanosecond, which BigQuery does not keep and the model
        // cannot hold.
        digits / scale
    };
    Some(nanos)
}

fn opaque(field: &TableFieldSchema, json: &serde_json::Value) -> Value {
    opaque_text(field, &json.to_string())
}

fn opaque_text(field: &TableFieldSchema, text: &str) -> Value {
    Value::Opaque {
        type_name: type_name(field).to_owned(),
        text: text.to_owned(),
    }
}

/// BigQuery's own spelling, which is what a user reads in the console and in
/// the DDL.
#[must_use]
pub fn type_name(field: &TableFieldSchema) -> &'static str {
    // Both spellings out of one match, and neither of them built with
    // `format!`: the name is `&'static str` so that a column's type costs
    // nothing per row, and pairing them here is what stops a type added to one
    // list from going missing in the other.
    let (base, array) = match field.r#type {
        FieldType::String => ("STRING", "ARRAY<STRING>"),
        FieldType::Bytes => ("BYTES", "ARRAY<BYTES>"),
        FieldType::Integer | FieldType::Int64 => ("INT64", "ARRAY<INT64>"),
        FieldType::Float | FieldType::Float64 => ("FLOAT64", "ARRAY<FLOAT64>"),
        FieldType::Numeric => ("NUMERIC", "ARRAY<NUMERIC>"),
        FieldType::Bignumeric => ("BIGNUMERIC", "ARRAY<BIGNUMERIC>"),
        FieldType::Boolean | FieldType::Bool => ("BOOL", "ARRAY<BOOL>"),
        FieldType::Timestamp => ("TIMESTAMP", "ARRAY<TIMESTAMP>"),
        FieldType::Date => ("DATE", "ARRAY<DATE>"),
        FieldType::Time => ("TIME", "ARRAY<TIME>"),
        FieldType::Datetime => ("DATETIME", "ARRAY<DATETIME>"),
        FieldType::Record | FieldType::Struct => ("STRUCT", "ARRAY<STRUCT>"),
        FieldType::Geography => ("GEOGRAPHY", "ARRAY<GEOGRAPHY>"),
        FieldType::Json => ("JSON", "ARRAY<JSON>"),
        FieldType::Interval => ("INTERVAL", "ARRAY<INTERVAL>"),
    };
    if is_repeated(field) { array } else { base }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, ty: FieldType) -> TableFieldSchema {
        TableFieldSchema::new(name, ty)
    }

    fn repeated_field(name: &str, ty: FieldType) -> TableFieldSchema {
        TableFieldSchema {
            mode: Some("REPEATED".to_owned()),
            ..field(name, ty)
        }
    }

    fn read(text: &str, field: &TableFieldSchema) -> Value {
        decode(Some(&serde_json::json!(text)), field)
    }

    #[test]
    fn numbers_arrive_as_strings_and_leave_as_numbers() {
        assert_eq!(read("42", &field("n", FieldType::Integer)), Value::Int(42));
        assert_eq!(
            read("3.5", &field("n", FieldType::Float64)),
            Value::Float(3.5)
        );
    }

    #[test]
    fn a_numeric_keeps_every_digit_it_arrived_with() {
        // 38 digits of precision, which is more than an `f64` has. Read as one
        // it would come back as 1.2345678901234568e28.
        let big = "12345678901234567890123456789.123456789";
        assert_eq!(
            read(big, &field("n", FieldType::Numeric)),
            Value::Decimal(big.to_owned())
        );
    }

    #[test]
    fn a_timestamp_keeps_its_microseconds() {
        // A value chosen because an `f64` gets it wrong: at a present-day
        // epoch the mantissa runs out around here, and `text.parse::<f64>() *
        // 1e9` gives 1_755_432_000_999_998_976 — a microsecond short of what
        // arrived. Most values do survive that route, which is why picking one
        // that does would have been a test of nothing.
        let Value::TimestampTz(at) = read("1755432000.999999", &field("t", FieldType::Timestamp))
        else {
            panic!("not a timestamp");
        };
        assert_eq!(at.unix_timestamp_nanos(), 1_755_432_000_999_999_000);
    }

    #[test]
    fn a_timestamp_in_exponent_form_is_the_same_instant() {
        // How a JSON writer renders a double once it is large enough, which is
        // every present-day epoch. Split at the dot and read as a fraction it
        // would land in 1970.
        assert_eq!(
            nanos_since_epoch("1.755432000123456E9"),
            Some(1_755_432_000_123_456_000)
        );
        assert_eq!(
            nanos_since_epoch("1.755432E9"),
            Some(1_755_432_000_000_000_000)
        );
        assert_eq!(nanos_since_epoch("-1.5E3"), Some(-1_500_000_000_000));
    }

    #[test]
    fn a_timestamp_before_the_epoch_keeps_its_direction() {
        // The fraction is a magnitude, not a signed part: `-1.5` seconds is
        // half a second *before* -1, not after it.
        let Value::TimestampTz(at) = read("-1.5", &field("t", FieldType::Timestamp)) else {
            panic!("not a timestamp");
        };
        assert_eq!(at.unix_timestamp_nanos(), -1_500_000_000);
        // The sign has to come from the text: this one's whole part is zero,
        // and `-0` parses to a number that is not negative.
        assert_eq!(nanos_since_epoch("-0.5"), Some(-500_000_000));
    }

    #[test]
    fn a_timestamp_that_is_not_a_number_is_shown_rather_than_guessed_at() {
        assert_eq!(nanos_since_epoch("yesterday"), None);
        assert_eq!(nanos_since_epoch("1.5abc"), None);
        assert_eq!(nanos_since_epoch(""), None);
        assert_eq!(
            read("yesterday", &field("t", FieldType::Timestamp)),
            Value::Opaque {
                type_name: "TIMESTAMP".to_owned(),
                text: "yesterday".to_owned(),
            }
        );
    }

    #[test]
    fn dates_and_times_parse_with_or_without_a_fraction() {
        assert_eq!(
            read("2026-08-17", &field("d", FieldType::Date)),
            Value::Date(time::macros::date!(2026 - 08 - 17))
        );
        assert_eq!(
            read("12:34:56", &field("t", FieldType::Time)),
            Value::Time(time::macros::time!(12:34:56))
        );
        assert_eq!(
            read("12:34:56.789", &field("t", FieldType::Time)),
            Value::Time(time::macros::time!(12:34:56.789))
        );
        assert_eq!(
            read("2026-08-17T12:34:56.789", &field("d", FieldType::Datetime)),
            Value::Timestamp(time::macros::datetime!(2026-08-17 12:34:56.789))
        );
    }

    #[test]
    fn bytes_arrive_base64_and_leave_as_bytes() {
        // The grid draws these as hex; left encoded, the cell would show the
        // encoding rather than the value.
        assert_eq!(
            read("AP8=", &field("b", FieldType::Bytes)),
            Value::Bytes(vec![0x00, 0xff])
        );
    }

    #[test]
    fn a_repeated_field_is_an_array_of_its_own_type() {
        let json = serde_json::json!([{ "v": "1" }, { "v": "2" }]);
        assert_eq!(
            decode(Some(&json), &repeated_field("ns", FieldType::Integer)),
            Value::Array(vec![Value::Int(1), Value::Int(2)])
        );
    }

    #[test]
    fn a_record_keeps_the_names_the_schema_gave_it() {
        // The row carries values in schema order and no names at all, so a
        // struct that lost the schema would be a list of anonymous cells.
        let mut user = field("user", FieldType::Record);
        user.fields = Some(vec![
            field("id", FieldType::Integer),
            field("email", FieldType::String),
        ]);
        let json = serde_json::json!({ "f": [{ "v": "7" }, { "v": "a@example.com" }] });
        assert_eq!(
            decode(Some(&json), &user),
            Value::Struct(vec![
                ("id".to_owned(), Value::Int(7)),
                ("email".to_owned(), Value::Text("a@example.com".to_owned())),
            ])
        );
    }

    #[test]
    fn a_record_shorter_than_its_schema_keeps_the_names_it_did_not_get() {
        // Cells are positional inside a record too, so reading them against
        // themselves would drop the trailing names entirely — the detail view
        // would show a struct with a field missing rather than a null one.
        let mut user = field("user", FieldType::Record);
        user.fields = Some(vec![
            field("id", FieldType::Integer),
            field("email", FieldType::String),
        ]);
        let json = serde_json::json!({ "f": [{ "v": "7" }] });
        assert_eq!(
            decode(Some(&json), &user),
            Value::Struct(vec![
                ("id".to_owned(), Value::Int(7)),
                ("email".to_owned(), Value::Null),
            ])
        );
    }

    #[test]
    fn a_repeated_record_is_an_array_of_structs() {
        let mut tags = repeated_field("tags", FieldType::Record);
        tags.fields = Some(vec![field("k", FieldType::String)]);
        let json = serde_json::json!([
            { "v": { "f": [{ "v": "a" }] } },
            { "v": { "f": [{ "v": "b" }] } },
        ]);
        assert_eq!(
            decode(Some(&json), &tags),
            Value::Array(vec![
                Value::Struct(vec![("k".to_owned(), Value::Text("a".to_owned()))]),
                Value::Struct(vec![("k".to_owned(), Value::Text("b".to_owned()))]),
            ])
        );
    }

    #[test]
    fn a_null_is_a_null_at_every_depth() {
        assert_eq!(decode(None, &field("n", FieldType::Integer)), Value::Null);
        assert_eq!(
            decode(
                Some(&serde_json::Value::Null),
                &field("n", FieldType::Integer)
            ),
            Value::Null
        );
        let mut user = field("user", FieldType::Record);
        user.fields = Some(vec![field("id", FieldType::Integer)]);
        let json = serde_json::json!({ "f": [{ "v": null }] });
        assert_eq!(
            decode(Some(&json), &user),
            Value::Struct(vec![("id".to_owned(), Value::Null)])
        );
    }

    #[test]
    fn a_type_this_driver_cannot_read_is_shown_rather_than_refused() {
        // The rule the whole module exists for: one `INTERVAL` column must not
        // cost the user the other twelve.
        assert_eq!(
            read("0-0 1 0:0:0", &field("i", FieldType::Interval)),
            Value::Opaque {
                type_name: "INTERVAL".to_owned(),
                text: "0-0 1 0:0:0".to_owned(),
            }
        );
    }

    #[test]
    fn a_value_that_does_not_parse_is_shown_rather_than_refused() {
        assert_eq!(
            read("not a number", &field("n", FieldType::Integer)),
            Value::Opaque {
                type_name: "INT64".to_owned(),
                text: "not a number".to_owned(),
            }
        );
    }

    #[test]
    fn a_json_column_arrives_as_a_document() {
        assert_eq!(
            read(r#"{"a":[1,2]}"#, &field("doc", FieldType::Json)),
            Value::Json(serde_json::json!({ "a": [1, 2] }))
        );
    }

    #[test]
    fn a_repeated_column_says_so_in_its_type_name() {
        assert_eq!(type_name(&field("n", FieldType::Integer)), "INT64");
        assert_eq!(
            type_name(&repeated_field("ns", FieldType::Integer)),
            "ARRAY<INT64>"
        );
    }
}

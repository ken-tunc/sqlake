//! `Value` as the JSON that loses least.
//!
//! Here rather than in either front-end because both want it and neither may
//! depend on the other: `sqlake-api` writes it onto a socket, and `sqlake-tui`
//! writes it onto the clipboard when somebody copies cells as JSON. Two copies
//! would be two answers to one question, and the one that drifted would be the
//! one nobody was reading at the time.
//!
//! It does not break the rule that keeps display decisions out of this crate.
//! There are no widths here, no glyphs and no elision — JSON is what a value
//! *is* once it leaves the process, and the abbreviating is the front-ends'.
//!
//! Not the JSON that looks best, and emphatically not what the TUI draws. The
//! terminal's rules are all wrong here: `∅` does not parse as null, a document
//! collapsed to `{2 keys}` destroys exactly what was asked for, and a newline
//! shown as `␊` corrupts the text. That the two front-ends want opposite
//! renderings of one `PagedResult` is what makes them peers rather than one
//! reusing the other's formatter.
//!
//! Where JSON cannot hold a value — bytes, a type the driver did not decode,
//! a float that is not finite, an integer a double would round, a formatter
//! that cannot render its input — the cell becomes a one-key object whose key
//! starts with `$`. The alternative — a bare string — is worse in the way that
//! matters: an agent cannot tell it from a column that really is text, so it
//! quotes base64 back as if it were the value. A `$` key can collide with a
//! real key inside a `Json` column, which is why the marker is a whole object
//! and never a field added to one.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Map, Value as Json};
use sqlake_core::value::Value;
use time::format_description::BorrowedFormatItem;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;

/// The key of the object wrapping a value JSON cannot hold.
const BYTES: &str = "$base64";
const OPAQUE: &str = "$opaque";
const NONFINITE: &str = "$float";
const WIDE_INT: &str = "$int";
const ERROR: &str = "$error";

/// The largest magnitude a JSON number survives once a parser reads it as a
/// double, which most do.
const EXACT_INT: i64 = (1 << 53) - 1;

#[must_use]
pub fn to_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(n) => int(*n),
        Value::Float(f) => float(*f),
        // A JSON number is a double in most parsers, and an
        // arbitrary-precision numeric is exactly what the driver went to the
        // trouble of not rounding.
        Value::Decimal(text) => Json::String(text.clone()),
        Value::Text(text) => Json::String(text.clone()),
        Value::Bytes(bytes) => marker(BYTES, Json::String(BASE64.encode(bytes))),
        Value::Date(d) => stringify(d.format(DATE)),
        Value::Time(t) => stringify(t.format(time_of_day(t.nanosecond()))),
        Value::Timestamp(ts) => stringify(ts.format(datetime(ts.nanosecond()))),
        Value::TimestampTz(ts) => stringify(ts.format(&Rfc3339)),
        Value::Json(json) => json.clone(),
        // Structural, because decoding `RECORD` and `REPEATED` structurally
        // rather than flattening them is the whole reason the driver did.
        Value::Array(items) => Json::Array(items.iter().map(to_json).collect()),
        Value::Struct(fields) => Json::Object(
            fields
                .iter()
                .map(|(name, v)| (name.clone(), to_json(v)))
                .collect(),
        ),
        // Both halves, so an agent can see that the driver did not understand
        // the value rather than reading the fallback text as the value.
        Value::Opaque { type_name, text } => {
            let mut inner = Map::new();
            inner.insert("type".to_owned(), Json::String(type_name.clone()));
            inner.insert("text".to_owned(), Json::String(text.clone()));
            marker(OPAQUE, Json::Object(inner))
        }
    }
}

const DATE: &[BorrowedFormatItem<'_>] = format_description!("[year]-[month]-[day]");
const TIME: &[BorrowedFormatItem<'_>] = format_description!("[hour]:[minute]:[second]");
const TIME_SUB: &[BorrowedFormatItem<'_>] =
    format_description!("[hour]:[minute]:[second].[subsecond digits:6]");
const DATETIME: &[BorrowedFormatItem<'_>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");
const DATETIME_SUB: &[BorrowedFormatItem<'_>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]");

/// A fractional part is written only when there is one, which is what RFC 3339
/// does for the value next to these — a `timestamp` and a `timestamptz` in one
/// row should not disagree about whether whole seconds carry `.000000`.
fn time_of_day(nanosecond: u32) -> &'static [BorrowedFormatItem<'static>] {
    if nanosecond == 0 { TIME } else { TIME_SUB }
}

fn datetime(nanosecond: u32) -> &'static [BorrowedFormatItem<'static>] {
    if nanosecond == 0 {
        DATETIME
    } else {
        DATETIME_SUB
    }
}

fn marker(key: &str, value: Json) -> Json {
    let mut object = Map::new();
    object.insert(key.to_owned(), value);
    Json::Object(object)
}

/// Wrapped only past the point a double stops being exact, so the ordinary
/// integer stays a number an agent can compare and subtract.
///
/// The same reasoning as [`Value::Decimal`], which is stringified for being
/// wider than a double — but `bigserial` keys are common enough that paying
/// for them on every `42` would be the wrong trade. A marker rather than a
/// bare string because the value really is a number, and because silently
/// rounding a key to its neighbour is the failure this exists to prevent.
fn int(n: i64) -> Json {
    if (-EXACT_INT..=EXACT_INT).contains(&n) {
        Json::from(n)
    } else {
        marker(WIDE_INT, Json::String(n.to_string()))
    }
}

/// JSON has no NaN and no infinities, and `serde_json` turns one into `null` —
/// a float that overflowed reported as a value that was absent.
fn float(f: f64) -> Json {
    serde_json::Number::from_f64(f)
        .map(Json::Number)
        .unwrap_or_else(|| marker(NONFINITE, Json::String(f.to_string())))
}

/// Wrapped rather than returned bare, because a formatter's message is an
/// English sentence and a bare string is what a `text` column looks like — an
/// agent would read "the year component cannot be formatted" as the cell's
/// data. `Rfc3339` rejects any year outside `0..=9999`, so a BC timestamp
/// reaches this from a real driver rather than only in theory.
fn stringify(formatted: Result<String, time::error::Format>) -> Json {
    match formatted {
        Ok(text) => Json::String(text),
        Err(err) => marker(ERROR, Json::String(err.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::macros::{date, datetime, time};

    use super::*;

    #[test]
    fn a_json_document_survives_the_trip_intact() {
        // The whole reason a driver decodes `json` structurally. The TUI shows
        // this as `{2 keys}`, which is right on screen and destroys exactly
        // what an agent asked for.
        let document = json!({"a": [1, 2, {"b": null}], "c": "text"});
        assert_eq!(to_json(&Value::Json(document.clone())), document);
    }

    #[test]
    fn an_ordinary_integer_stays_a_number() {
        // The common case pays nothing: an agent can compare and subtract it
        // without unwrapping anything.
        assert_eq!(to_json(&Value::Int(42)), json!(42));
        assert_eq!(to_json(&Value::Int(-42)), json!(-42));
    }

    #[test]
    fn an_integer_a_double_would_round_is_wrapped() {
        // 2^53 + 1. As a JSON number this reads back as 2^53 in any parser
        // that uses a double, so a `bigserial` key silently becomes its
        // neighbour's — the quietest way to answer a question about the wrong
        // row.
        let key = 9_007_199_254_740_993_i64;
        assert_eq!(
            to_json(&Value::Int(key)),
            json!({"$int": "9007199254740993"})
        );
        assert_eq!(
            to_json(&Value::Int(i64::MIN)),
            json!({"$int": "-9223372036854775808"})
        );
    }

    #[test]
    fn the_boundary_is_where_a_double_stops_being_exact() {
        assert_eq!(to_json(&Value::Int(EXACT_INT)), json!(EXACT_INT));
        assert_eq!(to_json(&Value::Int(-EXACT_INT)), json!(-EXACT_INT));
        assert!(to_json(&Value::Int(EXACT_INT + 1)).get(WIDE_INT).is_some());
        assert!(to_json(&Value::Int(-EXACT_INT - 1)).get(WIDE_INT).is_some());
    }

    #[test]
    fn a_numeric_stays_text() {
        // A JSON number is a double in most parsers, and this one has more
        // digits than a double holds.
        let exact = "123456789012345678901234567890.123456789";
        assert_eq!(
            to_json(&Value::Decimal(exact.to_owned())),
            json!(exact),
            "the precision the driver preserved was rounded away"
        );
    }

    #[test]
    fn a_value_the_formatter_cannot_render_is_not_mistaken_for_text() {
        // `Rfc3339` rejects any year outside `0..=9999`, and a BC timestamp
        // reaches this from Postgres. Bare, the message reads as the column's
        // data: an agent quotes an English sentence back as the value.
        let bc = datetime!(-0044-03-15 01:02:03 UTC);
        let json = to_json(&Value::TimestampTz(bc));
        assert!(
            json.get(ERROR).is_some(),
            "a formatting failure was emitted as a value: {json}"
        );
    }

    #[test]
    fn bytes_say_that_they_are_base64() {
        assert_eq!(
            to_json(&Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef])),
            json!({"$base64": "3q2+7w=="})
        );
    }

    #[test]
    fn an_undecoded_value_keeps_both_halves() {
        // Without the type name an agent reads the fallback text as the value,
        // rather than as a report that the driver did not understand it.
        assert_eq!(
            to_json(&Value::Opaque {
                type_name: "geometry".to_owned(),
                text: "POINT(1 2)".to_owned(),
            }),
            json!({"$opaque": {"type": "geometry", "text": "POINT(1 2)"}})
        );
    }

    #[test]
    fn structure_stays_structure() {
        let value = Value::Struct(vec![
            ("id".to_owned(), Value::Int(1)),
            (
                "tags".to_owned(),
                Value::Array(vec![Value::Text("a".to_owned()), Value::Null]),
            ),
        ]);
        assert_eq!(to_json(&value), json!({"id": 1, "tags": ["a", null]}));
    }

    #[test]
    fn a_float_that_is_not_a_number_does_not_become_null() {
        // `serde_json` maps a non-finite float to null, which reports a value
        // that overflowed as one that was absent.
        assert_eq!(
            to_json(&Value::Float(f64::INFINITY)),
            json!({"$float": "inf"})
        );
        assert_eq!(to_json(&Value::Float(1.5)), json!(1.5));
    }

    #[test]
    fn instants_are_written_so_they_can_be_read_back() {
        assert_eq!(
            to_json(&Value::Date(date!(2024 - 01 - 15))),
            json!("2024-01-15")
        );
        assert_eq!(to_json(&Value::Time(time!(12:34:56))), json!("12:34:56"));
        assert_eq!(
            to_json(&Value::Time(time!(12:34:56.5))),
            json!("12:34:56.500000")
        );
        assert_eq!(
            to_json(&Value::Timestamp(datetime!(2024-01-15 12:34:56))),
            json!("2024-01-15T12:34:56")
        );
        assert_eq!(
            to_json(&Value::TimestampTz(datetime!(2024-01-15 12:34:56 UTC))),
            json!("2024-01-15T12:34:56Z")
        );
    }

    #[test]
    fn a_timestamp_and_a_timestamptz_agree_about_whole_seconds() {
        // Two columns of one row disagreeing about whether to write `.000000`
        // reads as a difference in the data.
        let (naive, aware) = (
            to_json(&Value::Timestamp(datetime!(2024-01-15 12:34:56))),
            to_json(&Value::TimestampTz(datetime!(2024-01-15 12:34:56 UTC))),
        );
        assert_eq!(
            naive.as_str().unwrap(),
            aware.as_str().unwrap().trim_end_matches('Z')
        );
    }

    #[test]
    fn null_is_null_and_not_a_glyph() {
        assert_eq!(to_json(&Value::Null), json!(null));
        assert_eq!(to_json(&Value::Text("∅".to_owned())), json!("∅"));
    }

    #[test]
    fn a_newline_stays_a_newline() {
        // The grid draws this as `␊`, which corrupts the text for anyone
        // reading the value rather than looking at it.
        assert_eq!(to_json(&Value::Text("a\nb".to_owned())), json!("a\nb"));
    }
}

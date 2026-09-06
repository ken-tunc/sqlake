//! Putting cells on the clipboard, through the terminal.
//!
//! OSC 52 and never a clipboard crate. The terminal may be at the far end of an
//! SSH connection, and a crate would put the text on the clipboard of the
//! machine the database is near rather than the one the person is at. The
//! terminal is the thing that knows where the person is.
//!
//! What is copied is the *value*, not the cell. The grid shortens, escapes
//! control characters and writes `∅` for null, and every one of those is wrong
//! in a paste — so this reads `PagedResult` directly, the way `sqlake-api`
//! does, and shares its JSON writer rather than answering the same question a
//! second time.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use sqlake_app::PagedResult;
use sqlake_app::json::to_json;
use sqlake_core::value::Value;

/// How the cells are written out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// One row per line, fields separated by commas.
    Csv,
    /// An array of arrays, matching what the agent surface sends.
    Json,
}

/// What most terminals will accept in one OSC 52 sequence.
///
/// The limit is real and varies: xterm's default `maxStringParmSize` is 1 MB of
/// escape sequence, tmux truncates its own buffer, and a sequence past whatever
/// the terminal allows is dropped in full rather than cut short. So the payload
/// is measured before it is sent and refused if it is over, because "nothing
/// arrived" is the failure mode this is avoiding and it is silent.
pub const MAX_PAYLOAD_BYTES: usize = 512 * 1024;

/// Why nothing was copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// There were no rows to take.
    Empty,
    /// The encoded payload is past what a terminal will take in one sequence.
    ///
    /// `bytes` is the *text*, not the base64 of it. The limit applies to the
    /// encoded form, but a third larger is not a number the person choosing
    /// how much to select can act on.
    TooLarge { bytes: usize },
}

/// The part of `area` that is actually there, or `None` when none of it is.
///
/// A selection is indexes into a result the store is free to replace with a
/// shorter one, so a rectangle can name rows or columns that have gone — and an
/// empty result names none at all. Clamping without this last part is how a
/// result with no rows still writes one row of empty fields onto the clipboard.
#[must_use]
pub fn clamped(
    rows: &PagedResult,
    (top, left, bottom, right): (usize, usize, usize, usize),
) -> Option<(usize, usize, usize, usize)> {
    let bottom = bottom.min(rows.row_count().checked_sub(1)?);
    let right = right.min(rows.columns().len().checked_sub(1)?);
    (top <= bottom && left <= right).then_some((top, left, bottom, right))
}

/// The text for a rectangle of cells, in `format`.
///
/// `(top, left, bottom, right)` inclusive, which is what `GridUi::selection`
/// produces — one cell is a rectangle of one.
#[must_use]
pub fn render(rows: &PagedResult, format: Format, area: (usize, usize, usize, usize)) -> String {
    let Some((top, left, bottom, right)) = clamped(rows, area) else {
        return String::new();
    };

    match format {
        Format::Csv => {
            let mut out = String::new();
            for row in top..=bottom {
                for col in left..=right {
                    if col > left {
                        out.push(',');
                    }
                    out.push_str(&csv_field(rows.value(row, col)));
                }
                out.push('\n');
            }
            out
        }
        Format::Json => {
            let document: Vec<Vec<serde_json::Value>> = (top..=bottom)
                .map(|row| {
                    (left..=right)
                        .map(|col| {
                            rows.value(row, col)
                                .map_or(serde_json::Value::Null, to_json)
                        })
                        .collect()
                })
                .collect();
            serde_json::to_string(&document).unwrap_or_default()
        }
    }
}

/// One CSV field.
///
/// A null is an empty field, which is RFC 4180's only way to say "no value" —
/// the alternative, the four characters `NULL`, is indistinguishable from a
/// string that says NULL, and a column of names containing one would come back
/// wrong. It is indistinguishable from an empty string instead, which is the
/// same trade every CSV export makes and the reason JSON is the format to reach
/// for when the difference matters.
fn csv_field(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    if matches!(value, Value::Null) {
        return String::new();
    }
    let text = match to_json(value) {
        serde_json::Value::String(s) => s,
        // Everything else is written as its JSON: a number as a number, and a
        // document as the document rather than as a count of its keys.
        other => other.to_string(),
    };
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text
    }
}

/// The OSC 52 sequence that puts `text` on the clipboard.
///
/// # Errors
///
/// If there is nothing to copy, or the payload is past what a terminal takes.
pub fn sequence(text: &str) -> Result<String, Refused> {
    if text.is_empty() {
        return Err(Refused::Empty);
    }
    let payload = BASE64.encode(text);
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(Refused::TooLarge { bytes: text.len() });
    }
    // `c` is the clipboard proper rather than the primary selection, and the
    // terminating BEL is what tmux and the Terminal.app lineage want; ST works
    // in xterm but not everywhere.
    Ok(format!("\u{1b}]52;c;{payload}\u{7}"))
}

#[cfg(test)]
mod tests {
    use sqlake_core::result::{Column, ResultSet, Row};

    use super::*;

    fn rows() -> PagedResult {
        PagedResult::new(&ResultSet::new(
            vec![
                Column::new("id", "int8", false),
                Column::new("name", "text", true),
                Column::new("note", "text", true),
            ],
            vec![
                Row(vec![
                    Value::Int(1),
                    Value::Text("ada".into()),
                    Value::Text("a,b".into()),
                ]),
                Row(vec![
                    Value::Int(2),
                    Value::Null,
                    Value::Text("say \"hi\"".into()),
                ]),
            ],
            None,
        ))
    }

    #[test]
    fn a_cell_is_a_rectangle_of_one() {
        assert_eq!(render(&rows(), Format::Csv, (0, 1, 0, 1)), "ada\n");
    }

    #[test]
    fn a_field_with_a_comma_or_a_quote_is_quoted() {
        assert_eq!(render(&rows(), Format::Csv, (0, 2, 0, 2)), "\"a,b\"\n");
        assert_eq!(
            render(&rows(), Format::Csv, (1, 2, 1, 2)),
            "\"say \"\"hi\"\"\"\n"
        );
    }

    #[test]
    fn a_null_is_an_empty_field_rather_than_the_word() {
        // `NULL` is indistinguishable from a string that says NULL, which a
        // column of names can contain.
        assert_eq!(render(&rows(), Format::Csv, (1, 1, 1, 1)), "\n");
    }

    #[test]
    fn json_keeps_the_difference_csv_cannot() {
        // The reason JSON is worth having: a null and an empty string are the
        // same CSV field and different JSON values.
        let json = render(&rows(), Format::Json, (1, 1, 1, 1));
        assert_eq!(json, "[[null]]");
    }

    #[test]
    fn a_rectangle_is_the_rows_cut_to_the_columns() {
        assert_eq!(render(&rows(), Format::Csv, (0, 0, 1, 1)), "1,ada\n2,\n");
    }

    #[test]
    fn what_is_copied_is_the_value_and_not_the_cell() {
        // The grid writes `∅` for a null and clamps a long value; both are
        // wrong in a paste, and both would appear here if this read the grid.
        let long = "x".repeat(crate::grid::MAX_CELL_CHARS * 2);
        let rows = PagedResult::new(&ResultSet::new(
            vec![Column::new("c", "text", false)],
            vec![Row(vec![Value::Text(long.clone())])],
            None,
        ));
        let copied = render(&rows, Format::Csv, (0, 0, 0, 0));
        assert_eq!(copied.trim_end(), long);
        assert!(!copied.contains('…'));
    }

    #[test]
    fn a_range_past_the_end_is_cut_to_what_is_there() {
        // A selection outlives a page that shrank under it, and asking for row
        // 99 of a two-row result must not panic or write blank rows.
        assert_eq!(
            render(&rows(), Format::Csv, (0, 0, 99, 99)),
            "1,ada,\"a,b\"\n2,,\"say \"\"hi\"\"\"\n"
        );
    }

    #[test]
    fn a_result_with_no_rows_copies_nothing_rather_than_a_row_of_commas() {
        // The rectangle for "everything" is built from `row_count() - 1`, which
        // is row zero when there are no rows at all. Rendering it wrote a row
        // of empty fields — a paste of `,,` and a message saying cells were
        // sent, for a table that has none.
        let empty = PagedResult::new(&ResultSet::new(
            vec![
                Column::new("a", "text", true),
                Column::new("b", "text", true),
            ],
            Vec::new(),
            None,
        ));
        assert_eq!(render(&empty, Format::Csv, (0, 0, 0, 1)), "");
        assert_eq!(render(&empty, Format::Json, (0, 0, 0, 1)), "");
    }

    #[test]
    fn a_rectangle_entirely_past_the_end_is_nothing_in_either_format() {
        assert_eq!(render(&rows(), Format::Csv, (9, 0, 9, 0)), "");
        // JSON went through the empty range and wrote `[]`, which is a
        // sequence sent and a clipboard replaced for a selection that named
        // nothing.
        assert_eq!(render(&rows(), Format::Json, (9, 0, 9, 0)), "");
    }

    #[test]
    fn the_sequence_is_osc_52_and_base64() {
        let seq = sequence("hi").expect("it encodes");
        assert!(seq.starts_with("\u{1b}]52;c;"), "{seq:?}");
        assert!(seq.ends_with('\u{7}'), "{seq:?}");
        assert!(seq.contains(&BASE64.encode("hi")));
    }

    #[test]
    fn nothing_to_copy_is_refused_rather_than_sent() {
        assert_eq!(sequence(""), Err(Refused::Empty));
    }

    #[test]
    fn a_payload_past_what_a_terminal_takes_is_refused() {
        // A sequence over the limit is dropped in full by the terminal, so
        // sending it and saying "copied" is the one outcome to avoid: nothing
        // arrives and nothing says so.
        let huge = "x".repeat(MAX_PAYLOAD_BYTES);
        // Reported as the text it came from: base64 is a third larger, and the
        // person deciding how much to select cannot act on the encoded number.
        assert_eq!(
            sequence(&huge),
            Err(Refused::TooLarge {
                bytes: MAX_PAYLOAD_BYTES
            })
        );
    }
}

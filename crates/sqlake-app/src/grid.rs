//! Turning a result set into something a grid can draw.
//!
//! The UI never sees a [`Value`]. It sees [`RenderedGrid`], which owns the
//! result set and formats cells on demand.
//!
//! The obvious implementation materialises `Vec<Vec<Cell>>`. At 200k rows by 60
//! columns that allocates twelve million strings in order to display thirty of
//! them, so cells are formatted on access instead and only column widths are
//! computed eagerly, from a sample.

use std::fmt::Write as _;
use std::sync::Arc;

use sqlake_core::result::ResultSet;
use sqlake_core::value::Value;
use unicode_width::UnicodeWidthStr;

/// Rows sampled to decide a column's natural width. Enough to be representative
/// of a page, cheap enough to do on every load.
const WIDTH_SAMPLE_ROWS: usize = 200;

/// Columns narrower than this are unreadable even when the data is narrow.
const MIN_WIDTH: u16 = 3;

/// Columns wider than this crowd out every other column. The user can still
/// widen one by dragging.
const MAX_WIDTH: u16 = 60;

/// Longest cell text kept before truncating. Guards against a single 1MB text
/// value being measured character by character on every frame.
const MAX_CELL_CHARS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// What a cell is, for styling. Classification happens here so that the theme
/// can stay in the TUI crate and the data stays here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    Null,
    Number,
    Text,
    /// JSON, arrays and structs: shown collapsed, opened in a detail view.
    Complex,
    /// A type the driver could not decode.
    Opaque,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub align: Align,
    pub kind: CellKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedColumn {
    pub name: String,
    pub type_name: String,
    /// Sampled from the data. The view may override it; see `GridUi`.
    pub natural_width: u16,
    pub align: Align,
}

/// A result set prepared for display.
#[derive(Debug, Clone)]
pub struct RenderedGrid {
    result: Arc<ResultSet>,
    columns: Vec<RenderedColumn>,
}

impl RenderedGrid {
    #[must_use]
    pub fn new(result: ResultSet) -> Self {
        let result = Arc::new(result);
        let columns = result
            .columns
            .iter()
            .enumerate()
            .map(|(i, col)| {
                let sampled = result
                    .rows
                    .iter()
                    .take(WIDTH_SAMPLE_ROWS)
                    .map(|row| display_width(&format_value(row.get(i))))
                    .max()
                    .unwrap_or(0);
                let header = display_width(&col.name);
                RenderedColumn {
                    name: col.name.clone(),
                    type_name: col.type_name.clone(),
                    natural_width: sampled.max(header).clamp(MIN_WIDTH, MAX_WIDTH),
                    align: column_align(&result, i),
                }
            })
            .collect();

        Self { result, columns }
    }

    #[must_use]
    pub fn columns(&self) -> &[RenderedColumn] {
        &self.columns
    }

    #[must_use]
    pub fn row_count(&self) -> usize {
        self.result.rows.len()
    }

    /// Rows in the underlying relation, when the driver knew.
    #[must_use]
    pub fn total_rows(&self) -> Option<u64> {
        self.result.total_rows
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.result.rows.is_empty()
    }

    /// Formatted on demand. Only cells that are actually drawn are ever built.
    #[must_use]
    pub fn cell(&self, row: usize, col: usize) -> Cell {
        let value = self.result.rows.get(row).and_then(|r| r.get(col));
        Cell {
            text: format_value(value),
            align: value.map_or(Align::Left, align_of),
            kind: value.map_or(CellKind::Null, kind_of),
        }
    }

    /// The underlying value, for the detail view and for copying. Deliberately
    /// separate from [`RenderedGrid::cell`] so that drawing code has no reason
    /// to reach for it.
    #[must_use]
    pub fn raw(&self, row: usize, col: usize) -> Option<&Value> {
        self.result.rows.get(row).and_then(|r| r.get(col))
    }
}

/// A column is right-aligned when its values are numbers. Decided from the
/// data rather than the declared type, because a driver's type names are its
/// own business.
fn column_align(result: &ResultSet, col: usize) -> Align {
    let mut saw_value = false;
    for row in result.rows.iter().take(WIDTH_SAMPLE_ROWS) {
        match row.get(col) {
            None | Some(Value::Null) => {}
            Some(v) => {
                if !v.is_numeric() {
                    return Align::Left;
                }
                saw_value = true;
            }
        }
    }
    if saw_value { Align::Right } else { Align::Left }
}

fn align_of(value: &Value) -> Align {
    if value.is_numeric() {
        Align::Right
    } else {
        Align::Left
    }
}

fn kind_of(value: &Value) -> CellKind {
    match value {
        Value::Null => CellKind::Null,
        Value::Opaque { .. } => CellKind::Opaque,
        v if v.is_numeric() => CellKind::Number,
        v if v.is_composite() => CellKind::Complex,
        _ => CellKind::Text,
    }
}

/// The glyph for a null. A blank cell is indistinguishable from an empty
/// string, which matters more often than it sounds.
pub const NULL_GLYPH: &str = "∅";

fn format_value(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return NULL_GLYPH.to_owned();
    };
    match value {
        Value::Null => NULL_GLYPH.to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => format_float(*f),
        Value::Decimal(s) => s.clone(),
        Value::Text(s) => sanitise(s),
        Value::Bytes(b) => format_bytes(b),
        Value::Date(d) => d.to_string(),
        Value::Time(t) => t.to_string(),
        Value::Timestamp(ts) => ts.to_string(),
        Value::TimestampTz(ts) => ts.to_string(),
        Value::Json(j) => sanitise(&j.to_string()),
        Value::Array(items) => format!("[{} items]", items.len()),
        Value::Struct(fields) => format!("{{{} fields}}", fields.len()),
        Value::Opaque { text, type_name } => {
            if text.is_empty() {
                format!("<{type_name}>")
            } else {
                sanitise(text)
            }
        }
    }
}

fn format_float(f: f64) -> String {
    if f.is_nan() {
        "NaN".to_owned()
    } else if f.is_infinite() {
        if f.is_sign_negative() { "-∞" } else { "∞" }.to_owned()
    } else {
        f.to_string()
    }
}

fn format_bytes(bytes: &[u8]) -> String {
    const SHOWN: usize = 8;
    let mut out = String::from("0x");
    for b in bytes.iter().take(SHOWN) {
        let _ = write!(out, "{b:02x}");
    }
    if bytes.len() > SHOWN {
        let _ = write!(out, "… ({} bytes)", bytes.len());
    }
    out
}

/// Replace control characters and clamp the length.
///
/// A raw newline or tab reaching the terminal breaks the grid apart, and a
/// megabyte-long text value must not be measured in full on every frame.
fn sanitise(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_CELL_CHARS));
    for (i, ch) in s.chars().enumerate() {
        if i >= MAX_CELL_CHARS {
            out.push('…');
            break;
        }
        match ch {
            '\n' | '\r' => out.push('␊'),
            '\t' => out.push('␉'),
            c if c.is_control() => out.push('·'),
            c => out.push(c),
        }
    }
    out
}

/// Terminal columns occupied, which is not the character count.
fn display_width(s: &str) -> u16 {
    u16::try_from(UnicodeWidthStr::width(s)).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use sqlake_core::result::{Column, ResultSet, Row};
    use time::macros::{date, datetime, time};

    use super::*;

    fn grid(columns: Vec<Column>, rows: Vec<Row>) -> RenderedGrid {
        RenderedGrid::new(ResultSet::new(columns, rows, None))
    }

    fn one(value: Value) -> RenderedGrid {
        grid(vec![Column::new("v", "any", true)], vec![Row(vec![value])])
    }

    fn text_of(value: Value) -> String {
        one(value).cell(0, 0).text
    }

    #[test]
    fn null_is_visible_and_distinct_from_an_empty_string() {
        assert_eq!(text_of(Value::Null), NULL_GLYPH);
        assert_eq!(text_of(Value::Text(String::new())), "");
    }

    #[test]
    fn a_missing_cell_reads_as_null_rather_than_panicking() {
        // Ragged rows should never take the process down.
        let g = grid(
            vec![
                Column::new("a", "int", false),
                Column::new("b", "int", true),
            ],
            vec![Row(vec![Value::Int(1)])],
        );
        assert_eq!(g.cell(0, 1).text, NULL_GLYPH);
        assert_eq!(g.cell(99, 0).text, NULL_GLYPH);
    }

    #[test]
    fn control_characters_never_reach_the_terminal() {
        let out = text_of(Value::Text("line one\nline two\tafter".into()));
        assert!(!out.contains('\n'), "{out:?}");
        assert!(!out.contains('\t'), "{out:?}");
        assert_eq!(out, "line one␊line two␉after");
    }

    #[test]
    fn very_long_text_is_truncated() {
        let out = text_of(Value::Text("x".repeat(10_000)));
        assert_eq!(out.chars().count(), MAX_CELL_CHARS + 1);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn width_is_measured_in_display_columns_not_characters() {
        // Seven CJK characters occupy fourteen terminal columns.
        let g = grid(
            vec![Column::new("s", "text", false)],
            vec![Row(vec![Value::Text("こんにちは世界".into())])],
        );
        assert_eq!(g.columns()[0].natural_width, 14);
    }

    #[test]
    fn fullwidth_latin_is_also_double_width() {
        let g = grid(
            vec![Column::new("s", "text", false)],
            vec![Row(vec![Value::Text("ＡＢＣ".into())])],
        );
        assert_eq!(g.columns()[0].natural_width, 6);
    }

    #[test]
    fn the_header_sets_a_floor_on_the_width() {
        let g = grid(
            vec![Column::new("a_very_long_column_name", "int", false)],
            vec![Row(vec![Value::Int(1)])],
        );
        assert_eq!(g.columns()[0].natural_width, 23);
    }

    #[test]
    fn widths_are_clamped_at_both_ends() {
        let narrow = grid(
            vec![Column::new("a", "int", false)],
            vec![Row(vec![Value::Int(1)])],
        );
        assert_eq!(narrow.columns()[0].natural_width, MIN_WIDTH);

        let huge = grid(
            vec![Column::new("a", "text", false)],
            vec![Row(vec![Value::Text("x".repeat(500))])],
        );
        assert_eq!(huge.columns()[0].natural_width, MAX_WIDTH);
    }

    #[test]
    fn only_sampled_rows_influence_the_width() {
        let mut rows: Vec<Row> = (0..WIDTH_SAMPLE_ROWS)
            .map(|_| Row(vec![Value::Text("short".into())]))
            .collect();
        rows.push(Row(vec![Value::Text("x".repeat(50))]));
        let g = grid(vec![Column::new("a", "text", false)], rows);
        assert_eq!(g.columns()[0].natural_width, 5);
    }

    #[test]
    fn numeric_columns_are_right_aligned() {
        let nums = grid(
            vec![Column::new("n", "int", true)],
            vec![Row(vec![Value::Int(1)]), Row(vec![Value::Null])],
        );
        assert_eq!(nums.columns()[0].align, Align::Right);

        // One non-numeric value is enough to make the column textual.
        let mixed = grid(
            vec![Column::new("n", "any", true)],
            vec![Row(vec![Value::Int(1)]), Row(vec![Value::Text("x".into())])],
        );
        assert_eq!(mixed.columns()[0].align, Align::Left);
    }

    #[test]
    fn an_all_null_column_is_left_aligned() {
        let g = grid(
            vec![Column::new("n", "int", true)],
            vec![Row(vec![Value::Null])],
        );
        assert_eq!(g.columns()[0].align, Align::Left);
    }

    #[test]
    fn composite_values_collapse_to_a_summary() {
        assert_eq!(
            text_of(Value::Array(vec![Value::Int(1), Value::Int(2)])),
            "[2 items]"
        );
        assert_eq!(
            text_of(Value::Struct(vec![("a".into(), Value::Int(1))])),
            "{1 fields}"
        );
        assert_eq!(one(Value::Array(vec![])).cell(0, 0).kind, CellKind::Complex);
    }

    #[test]
    fn unknown_types_show_their_text_or_their_name() {
        assert_eq!(
            text_of(Value::Opaque {
                type_name: "geometry".into(),
                text: "POINT(0 0)".into(),
            }),
            "POINT(0 0)"
        );
        assert_eq!(
            text_of(Value::Opaque {
                type_name: "unknown_oid_1".into(),
                text: String::new(),
            }),
            "<unknown_oid_1>"
        );
    }

    #[test]
    fn non_finite_floats_are_readable() {
        assert_eq!(text_of(Value::Float(f64::NAN)), "NaN");
        assert_eq!(text_of(Value::Float(f64::INFINITY)), "∞");
        assert_eq!(text_of(Value::Float(f64::NEG_INFINITY)), "-∞");
    }

    #[test]
    fn huge_decimals_keep_every_digit() {
        let s = "-99999999999999999999999999999999.99999999";
        assert_eq!(text_of(Value::Decimal(s.into())), s);
    }

    #[test]
    fn bytes_are_shown_as_a_prefix_with_a_length() {
        assert_eq!(text_of(Value::Bytes(vec![0x00, 0xff])), "0x00ff");
        let long = text_of(Value::Bytes((0..32).collect()));
        assert!(long.starts_with("0x0001"), "{long}");
        assert!(long.ends_with("(32 bytes)"), "{long}");
    }

    #[test]
    fn temporal_values_render_without_panicking() {
        assert_eq!(text_of(Value::Date(date!(2026 - 01 - 02))), "2026-01-02");
        assert_eq!(text_of(Value::Time(time!(10:30:00))), "10:30:00.0");
        assert!(text_of(Value::Timestamp(datetime!(2026-01-02 10:30))).starts_with("2026-01-02"));
        assert!(
            text_of(Value::TimestampTz(datetime!(2026-01-02 10:30 UTC))).starts_with("2026-01-02")
        );
    }

    #[test]
    fn cloning_a_grid_shares_the_result_set() {
        let g = one(Value::Int(1));
        let c = g.clone();
        assert!(Arc::ptr_eq(&g.result, &c.result));
    }

    #[test]
    fn raw_values_stay_reachable_for_copying() {
        let g = one(Value::Int(7));
        assert_eq!(g.raw(0, 0), Some(&Value::Int(7)));
        assert_eq!(g.raw(1, 0), None);
    }
}

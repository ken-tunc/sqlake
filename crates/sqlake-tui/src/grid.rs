//! Turning fetched rows into something a grid can draw.
//!
//! Drawing code never sees a [`Value`]. It sees [`RenderedGrid`], which holds
//! the rows and formats cells on demand.
//!
//! This lives in the terminal crate because every decision in it is a terminal
//! decision: widths in character cells, control characters replaced with
//! glyphs, long values elided, JSON collapsed to a summary. The agent surface
//! reads the same [`PagedResult`] and wants the opposite of all four —
//! collapsing a document to `{2 keys}` destroys exactly what it asked for.
//!
//! The obvious implementation materialises `Vec<Vec<Cell>>`. At 200k rows by 60
//! columns that allocates twelve million strings in order to display thirty of
//! them, so cells are formatted on access instead and only column widths are
//! computed eagerly, from a sample.

use std::fmt::Write as _;
use std::sync::Arc;

use sqlake_app::PagedResult;
use sqlake_core::result::Row;
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

/// Alignment is deliberately absent: it is a property of the *column*, not of
/// the value, and lives on [`RenderedColumn`].
///
/// Deciding it per value looks harmless and is not. A nullable integer column
/// would right-align its numbers and left-align its `∅`, because a null is not
/// numeric — so a perfectly ordinary column comes out ragged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
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

/// Rows prepared for display.
///
/// Cheap to build and cheap to hold: the rows stay behind the `Arc` they
/// arrived in, and only the column widths are computed up front.
#[derive(Debug, Clone)]
pub struct RenderedGrid {
    columns: Vec<RenderedColumn>,
    rows: Arc<PagedResult>,
}

impl RenderedGrid {
    #[must_use]
    pub fn new(rows: Arc<PagedResult>) -> Self {
        // The first page only, never the accumulated result: widths and
        // alignment are decided once, so that a page arriving later cannot
        // re-lay out the grid under the reader.
        let sample = rows.sample(WIDTH_SAMPLE_ROWS);
        let columns = rows
            .columns()
            .iter()
            .enumerate()
            .map(|(i, col)| {
                let sampled = sample
                    .iter()
                    .map(|row| display_width(&format_value(row.get(i))))
                    .max()
                    .unwrap_or(0);
                // Headers go through the same treatment as values. A driver's
                // column name is data too: a quoted identifier in PostgreSQL
                // can carry a newline, and one in a header splits the grid
                // apart exactly as one in a cell does.
                let name = sanitise(&col.name);
                let header = display_width(&name);
                RenderedColumn {
                    type_name: sanitise(&col.type_name),
                    name,
                    natural_width: sampled.max(header).clamp(MIN_WIDTH, MAX_WIDTH),
                    align: column_align(sample, i),
                }
            })
            .collect();

        Self { columns, rows }
    }

    /// Whether this grid was built from `rows`.
    ///
    /// Snapshots arrive for unrelated reasons — a spinner tick republishes
    /// everything — so the view keeps its grid and rebuilds only when the rows
    /// themselves change. Re-sampling widths on every frame would also make
    /// columns twitch as pages arrive.
    #[must_use]
    pub fn is_for(&self, rows: &Arc<PagedResult>) -> bool {
        Arc::ptr_eq(&self.rows, rows)
    }

    #[must_use]
    pub fn columns(&self) -> &[RenderedColumn] {
        &self.columns
    }

    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.row_count()
    }

    /// Rows in the underlying relation, when the driver knew.
    #[must_use]
    pub fn total_rows(&self) -> Option<u64> {
        self.rows.total_rows()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Formatted on demand. Only cells that are actually drawn are ever built.
    #[must_use]
    pub fn cell(&self, row: usize, col: usize) -> Cell {
        let value = self.rows.value(row, col);
        Cell {
            text: format_value(value),
            kind: value.map_or(CellKind::Null, kind_of),
        }
    }

    /// The underlying value, for the detail view and for copying. Deliberately
    /// separate from [`RenderedGrid::cell`] so that drawing code has no reason
    /// to reach for it.
    #[must_use]
    pub fn raw(&self, row: usize, col: usize) -> Option<&Value> {
        self.rows.value(row, col)
    }
}

/// A column is right-aligned when its values are numbers. Decided from the
/// data rather than the declared type, because a driver's type names are its
/// own business.
fn column_align(rows: &[Row], col: usize) -> Align {
    let mut saw_value = false;
    for row in rows {
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
        // PostgreSQL `numeric` goes to 131,072 digits. `raw` still hands the
        // whole thing to the detail view and to copying; the *cell* is clamped
        // like every other long value.
        Value::Decimal(s) => sanitise(s),
        Value::Text(s) => sanitise(s),
        Value::Bytes(b) => format_bytes(b),
        Value::Date(d) => d.to_string(),
        Value::Time(t) => t.to_string(),
        Value::Timestamp(ts) => ts.to_string(),
        Value::TimestampTz(ts) => ts.to_string(),
        Value::Json(j) => format_json(j),
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

/// Magnitudes outside this range are shown in exponent form.
///
/// Rust's `Display` for `f64` never uses one, so `1e300` renders as 301 digits
/// and `1e-300` as 302 — a cell wider than the terminal, carrying no
/// information a reader can use.
const FLOAT_EXP_ABOVE: f64 = 1e15;
const FLOAT_EXP_BELOW: f64 = 1e-5;

fn format_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_owned();
    }
    if f.is_infinite() {
        return if f.is_sign_negative() { "-∞" } else { "∞" }.to_owned();
    }
    let magnitude = f.abs();
    if magnitude != 0.0 && !(FLOAT_EXP_BELOW..FLOAT_EXP_ABOVE).contains(&magnitude) {
        format!("{f:e}")
    } else {
        f.to_string()
    }
}

/// Objects and arrays collapse to a count, which is what [`CellKind::Complex`]
/// promises and what the detail view is for.
///
/// Collapsing is not only about width: `to_string` on a megabyte of `jsonb`
/// allocates the whole document, and doing that for every visible cell on every
/// frame is exactly the cost this module exists to avoid.
fn format_json(json: &serde_json::Value) -> String {
    match json {
        serde_json::Value::Object(fields) => format!("{{{} keys}}", fields.len()),
        serde_json::Value::Array(items) => format!("[{} items]", items.len()),
        scalar => sanitise(&scalar.to_string()),
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

/// Whether a character rewrites the direction of the text that follows it.
///
/// These are the bidirectional overrides and isolates. Left in place, an
/// unterminated one — and a one-line grid cell never terminates anything —
/// reorders the rest of the line, so a value can paint itself over the column
/// beside it. `unicode-width` scores them zero, so they do not even show up in
/// the width calculation.
///
/// Deliberately **not** "every `Cf` character": ZWJ (U+200D) is `Cf` too, and
/// replacing it would take every emoji sequence apart. The directional *marks*
/// U+200E/U+200F are left alone as well — they nudge the ordering of the run
/// they sit in rather than overriding everything after them, and mixed-script
/// text uses them legitimately.
const fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Replace anything that would rewrite the terminal, and clamp the length.
///
/// A raw newline or tab breaks the grid apart, an escape sequence repaints it,
/// a bidi override reorders it, and a megabyte-long value must not be measured
/// in full on every frame.
pub(crate) fn sanitise(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_CELL_CHARS));
    for (i, ch) in s.chars().enumerate() {
        if i >= MAX_CELL_CHARS {
            out.push('…');
            break;
        }
        match ch {
            '\n' | '\r' => out.push('␊'),
            '\t' => out.push('␉'),
            // `is_control` is category Cc only, which is why the bidi test is
            // separate: U+202E is Cf and sails straight through it.
            c if c.is_control() || is_bidi_control(c) => out.push('·'),
            c => out.push(c),
        }
    }
    out
}

/// Terminal columns occupied, which is not the character count.
pub(crate) fn display_width(s: &str) -> u16 {
    u16::try_from(UnicodeWidthStr::width(s)).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use sqlake_core::result::{Column, ResultSet, Row};
    use time::macros::{date, datetime, time};

    use super::*;

    fn grid(columns: Vec<Column>, rows: Vec<Row>) -> RenderedGrid {
        paged(ResultSet::new(columns, rows, None))
    }

    fn paged(result: ResultSet) -> RenderedGrid {
        RenderedGrid::new(Arc::new(PagedResult::new(&result)))
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
    fn a_grid_is_recognised_as_belonging_to_its_rows() {
        let rows = Arc::new(PagedResult::new(&ResultSet::new(
            vec![Column::new("v", "any", true)],
            vec![Row(vec![Value::Int(1)])],
            None,
        )));
        let g = RenderedGrid::new(Arc::clone(&rows));
        assert!(g.is_for(&rows));

        // A snapshot arriving for an unrelated reason must not cost a rebuild,
        // and rebuilding would make the columns twitch as pages land.
        let other = Arc::new(PagedResult::new(&ResultSet::new(
            vec![Column::new("v", "any", true)],
            vec![Row(vec![Value::Int(1)])],
            None,
        )));
        assert!(!g.is_for(&other));
    }

    #[test]
    fn widths_survive_a_page_arriving() {
        let first = ResultSet::new(
            vec![Column::new("v", "text", false)],
            vec![Row(vec![Value::Text("short".into())])],
            Some(2),
        );
        let rows = PagedResult::new(&first);
        let width = RenderedGrid::new(Arc::new(rows.clone())).columns()[0].natural_width;

        let second = ResultSet::new(
            vec![Column::new("v", "text", false)],
            vec![Row(vec![Value::Text("a much longer value".into())])],
            None,
        );
        let grown = RenderedGrid::new(Arc::new(rows.append(&second).unwrap()));
        // A column that resized itself every time a page landed would be
        // unusable, so the sample is the first page and nothing else — even
        // though the second page is wider and the first is far short of
        // `WIDTH_SAMPLE_ROWS`.
        assert_eq!(grown.row_count(), 2);
        assert_eq!(grown.columns()[0].natural_width, width);
    }

    #[test]
    fn alignment_survives_a_page_arriving() {
        let numbers = ResultSet::new(
            vec![Column::new("v", "any", true)],
            vec![Row(vec![Value::Int(1)])],
            None,
        );
        let rows = PagedResult::new(&numbers);
        assert_eq!(
            RenderedGrid::new(Arc::new(rows.clone())).columns()[0].align,
            Align::Right
        );

        // A text value on page two must not flip the whole column to the left
        // under a reader who is looking at page one.
        let text = ResultSet::new(
            vec![Column::new("v", "any", true)],
            vec![Row(vec![Value::Text("x".into())])],
            None,
        );
        let grown = RenderedGrid::new(Arc::new(rows.append(&text).unwrap()));
        assert_eq!(grown.columns()[0].align, Align::Right);
    }

    #[test]
    fn raw_values_stay_reachable_for_copying() {
        let g = one(Value::Int(7));
        assert_eq!(g.raw(0, 0), Some(&Value::Int(7)));
        assert_eq!(g.raw(1, 0), None);
    }

    #[test]
    fn bidi_overrides_are_replaced_but_zero_width_joiners_survive() {
        // U+202E is category Cf, so `char::is_control` is false for it and it
        // sails through any check built on that alone.
        assert!(!'\u{202e}'.is_control());
        let reordered = text_of(Value::Text("total\u{202e}drawkcab".into()));
        assert!(!reordered.contains('\u{202e}'), "{reordered:?}");

        // The same category holds ZWJ, and replacing that would take every
        // emoji sequence apart.
        let family = text_of(Value::Text("👨\u{200d}👩\u{200d}👧".into()));
        assert!(family.contains('\u{200d}'), "{family:?}");
    }

    #[test]
    fn headers_are_sanitised_like_cells() {
        let g = grid(
            vec![Column::new("a\nb", "te\u{202e}xt", false)],
            vec![Row(vec![Value::Int(1)])],
        );
        let col = &g.columns()[0];
        assert_eq!(col.name, "a␊b");
        assert!(!col.type_name.contains('\u{202e}'), "{:?}", col.type_name);
    }

    #[test]
    fn extreme_magnitudes_use_an_exponent() {
        // Rust's Display for f64 never does, so these are 301 and 302
        // characters wide without the exponent form.
        assert_eq!(text_of(Value::Float(1e300)), "1e300");
        assert_eq!(text_of(Value::Float(1e-300)), "1e-300");
        // Ordinary numbers are left alone.
        assert_eq!(text_of(Value::Float(1.5)), "1.5");
        assert_eq!(text_of(Value::Float(0.0)), "0");
    }

    #[test]
    fn json_objects_and_arrays_collapse() {
        assert_eq!(
            text_of(Value::Json(serde_json::json!({"a": 1, "b": 2}))),
            "{2 keys}"
        );
        assert_eq!(
            text_of(Value::Json(serde_json::json!([1, 2, 3]))),
            "[3 items]"
        );
        // A scalar is small and worth reading in place.
        assert_eq!(text_of(Value::Json(serde_json::json!(42))), "42");
    }

    /// The fixtures were built to break naive formatting. Running the formatter
    /// against values written in this file only proves it handles values
    /// written in this file.
    mod against_the_fixtures {
        use sqlake_driver_mock::fixtures::catalog;

        use super::*;

        fn rendered(schema: &str, table: &str) -> RenderedGrid {
            let cat = catalog();
            let t = cat.table(schema, table).unwrap();
            paged(ResultSet::new(
                t.columns.clone(),
                t.all_rows(),
                t.total_rows(),
            ))
        }

        #[test]
        fn nothing_in_the_unicode_table_reaches_the_terminal_raw() {
            let g = rendered("public", "unicode");
            for row in 0..g.row_count() {
                for col in 0..g.columns().len() {
                    let text = g.cell(row, col).text;
                    assert!(
                        !text.chars().any(|c| c.is_control() || is_bidi_control(c)),
                        "row {row} col {col}: {text:?}"
                    );
                }
            }
        }

        #[test]
        fn column_widths_come_from_display_width_not_character_count() {
            let g = rendered("public", "unicode");
            let sample = &g.columns()[1];

            // The fixture's widest row on screen is fullwidth text that is
            // among its shortest in `char`s, so a sampler counting characters
            // lands on a different, narrower answer.
            let by_chars: usize = (0..g.row_count())
                .map(|r| g.cell(r, 1).text.chars().count())
                .max()
                .unwrap();
            assert!(
                usize::from(sample.natural_width) > by_chars,
                "width {} is not wider than the longest char count {by_chars}",
                sample.natural_width
            );
        }

        #[test]
        fn the_wide_table_asks_for_unequal_columns() {
            let g = rendered("public", "wide");
            let widths: Vec<u16> = g.columns().iter().map(|c| c.natural_width).collect();
            assert!(widths.iter().max() > widths.iter().min());
        }

        #[test]
        fn a_relation_without_a_total_renders() {
            let g = rendered("analytics", "unbounded");
            assert_eq!(g.total_rows(), None);
            assert!(g.row_count() > 0);
        }

        #[test]
        fn awkward_identifiers_survive_becoming_headers() {
            let g = rendered("public", "Mixed.Case");
            let names: Vec<&str> = g.columns().iter().map(|c| c.name.as_str()).collect();
            assert!(names.contains(&"Column With Spaces"), "{names:?}");
            assert!(names.contains(&"列名"), "{names:?}");
        }

        #[test]
        fn every_value_variant_formats_without_panicking() {
            let g = rendered("public", "types_showcase");
            for row in 0..g.row_count() {
                for col in 0..g.columns().len() {
                    assert!(!g.cell(row, col).text.is_empty(), "row {row} col {col}");
                }
            }
        }
    }
}

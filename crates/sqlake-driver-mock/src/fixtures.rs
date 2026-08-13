//! The fixture catalogue.
//!
//! This is not filler data. Every table here exists to break a specific naive
//! implementation, so that the failure shows up while the widget is being
//! written rather than when a real driver lands on top of it.

use sqlake_core::node::RelationKind;
use sqlake_core::result::{Column, Row};
use sqlake_core::value::Value;
use time::macros::{date, datetime, time};

/// Rows in the generated table. Large enough that materialising every row to
/// display thirty of them is obviously wrong.
pub const BIG_ROWS: u64 = 200_000;

#[derive(Debug)]
pub struct Catalog {
    pub schemas: Vec<Schema>,
}

#[derive(Debug)]
pub struct Schema {
    pub name: &'static str,
    pub tables: Vec<Table>,
}

#[derive(Debug)]
pub struct Table {
    pub name: &'static str,
    pub kind: RelationKind,
    pub columns: Vec<Column>,
    pub data: Data,
}

#[derive(Debug)]
pub enum Data {
    /// Rows held in memory.
    Rows(Vec<Row>),
    /// Rows held in memory whose count the engine will not report. A BigQuery
    /// preview and a PostgreSQL cursor without a `count(*)` both behave this
    /// way, so an unknown total is not an edge case — `ResultSet::total_rows`
    /// documents it as the common one.
    RowsWithoutTotal(Vec<Row>),
    /// Rows produced on demand, so that only the requested page is ever built.
    Generated { total: u64 },
}

impl Catalog {
    #[must_use]
    pub fn schema(&self, name: &str) -> Option<&Schema> {
        self.schemas.iter().find(|s| s.name == name)
    }

    #[must_use]
    pub fn table(&self, schema: &str, table: &str) -> Option<&Table> {
        self.schema(schema)?.tables.iter().find(|t| t.name == table)
    }

    /// Whether a node path names something that is actually here.
    ///
    /// Failure and latency injection is checked against this, so renaming a
    /// fixture is a panic rather than a test that quietly stops testing
    /// anything.
    #[must_use]
    pub fn resolves(&self, path: &[String]) -> bool {
        match crate::without_catalog(path) {
            // The root, which every configuration has.
            [] => true,
            [schema] => self.schema(schema).is_some(),
            [schema, table] => self.table(schema, table).is_some(),
            _ => false,
        }
    }
}

impl Table {
    /// `None` when the engine does not report a count. Callers that assume
    /// `Some` are the reason [`Data::RowsWithoutTotal`] exists.
    #[must_use]
    pub fn total_rows(&self) -> Option<u64> {
        match &self.data {
            Data::Rows(rows) => Some(rows.len() as u64),
            Data::RowsWithoutTotal(_) => None,
            Data::Generated { total } => Some(*total),
        }
    }

    /// One page, in storage order.
    #[must_use]
    pub fn page(&self, offset: u64, limit: u32) -> Vec<Row> {
        match &self.data {
            Data::Rows(rows) | Data::RowsWithoutTotal(rows) => rows
                .iter()
                .skip(usize::try_from(offset).unwrap_or(usize::MAX))
                .take(limit as usize)
                .cloned()
                .collect(),
            Data::Generated { total } => (offset..(*total).min(offset + u64::from(limit)))
                .map(generated_row)
                .collect(),
        }
    }

    /// Every row, which a sort needs. Only reached when the user actually
    /// sorts; the unsorted path above stays lazy.
    #[must_use]
    pub fn all_rows(&self) -> Vec<Row> {
        match &self.data {
            Data::Rows(rows) | Data::RowsWithoutTotal(rows) => rows.clone(),
            Data::Generated { total } => (0..*total).map(generated_row).collect(),
        }
    }
}

/// Deterministic, so tests and snapshots do not drift between runs.
fn generated_row(i: u64) -> Row {
    let bucket = ["alpha", "beta", "gamma", "delta"][(i % 4) as usize];
    Row(vec![
        Value::Int(i as i64),
        Value::Text(bucket.to_owned()),
        // A small irrational-looking spread without pulling in a RNG.
        Value::Float(((i * 2_654_435_761) % 10_000) as f64 / 100.0),
        Value::Bool(i.is_multiple_of(7)),
    ])
}

#[must_use]
pub fn catalog() -> Catalog {
    Catalog {
        schemas: vec![
            Schema {
                name: "public",
                tables: vec![
                    users(),
                    types_showcase(),
                    wide(),
                    big(),
                    unicode(),
                    empty(),
                    needs_quoting(),
                ],
            },
            Schema {
                name: "analytics",
                tables: vec![broken(), slow(), unbounded(), daily_summary()],
            },
            // Expanding this one fails. See Behaviour::fixture.
            //
            // It holds a relation anyway: failure lives in Behaviour, so under
            // Behaviour::instant() this schema expands normally, and an empty
            // table list would make it a node claiming children and then
            // producing none.
            Schema {
                name: "restricted",
                tables: vec![secrets()],
            },
        ],
    }
}

/// The ordinary case. Whatever renders correctly here is not yet proven.
fn users() -> Table {
    let rows = (1..=50_u64)
        .map(|i| {
            Row(vec![
                Value::Int(i as i64),
                Value::Text(format!("user{i:02}@example.com")),
                Value::Text(
                    ["Ada", "Grace", "Alan", "Barbara", "Edsger"][(i % 5) as usize].to_owned(),
                ),
                Value::Bool(!i.is_multiple_of(3)),
                Value::Decimal(format!("{}.{:02}", i * 7 % 100, i * 13 % 100)),
                Value::TimestampTz(
                    datetime!(2026-01-02 10:00 UTC) + time::Duration::minutes(i as i64 * 4),
                ),
                if i.is_multiple_of(6) {
                    Value::Null
                } else {
                    Value::Timestamp(datetime!(2026-08-01 09:30) + time::Duration::hours(i as i64))
                },
                if i.is_multiple_of(4) {
                    Value::Null
                } else {
                    Value::Text(format!("note for row {i}"))
                },
            ])
        })
        .collect();

    Table {
        name: "users",
        kind: RelationKind::Table,
        columns: vec![
            Column::new("id", "int8", false),
            Column::new("email", "text", false),
            Column::new("display_name", "text", false),
            Column::new("active", "bool", false),
            Column::new("score", "numeric", false),
            Column::new("created_at", "timestamptz", false),
            Column::new("last_seen", "timestamp", true),
            Column::new("notes", "text", true),
        ],
        data: Data::Rows(rows),
    }
}

/// One column per `Value` variant, so the formatter is exercised in full.
fn types_showcase() -> Table {
    let normal = Row(vec![
        Value::Bool(true),
        Value::Int(42),
        Value::Float(std::f64::consts::PI),
        Value::Decimal("12345.6789".into()),
        Value::Text("hello".into()),
        Value::Bytes(vec![0x00, 0x01, 0xfe, 0xff]),
        Value::Date(date!(2026 - 01 - 02)),
        Value::Time(time!(10:30:00)),
        Value::Timestamp(datetime!(2026-01-02 10:30)),
        Value::TimestampTz(datetime!(2026-01-02 10:30 UTC)),
        Value::Json(serde_json::json!({"a": 1, "b": [true, null]})),
        Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        Value::Struct(vec![
            ("name".into(), Value::Text("ada".into())),
            ("age".into(), Value::Int(36)),
        ]),
        Value::Opaque {
            type_name: "geometry".into(),
            text: "POINT(139.6917 35.6895)".into(),
        },
    ]);

    let extremes = Row(vec![
        Value::Bool(false),
        Value::Int(i64::MIN),
        Value::Float(f64::NEG_INFINITY),
        // Wider than any sensible column, on purpose.
        Value::Decimal("-99999999999999999999999999999999.99999999".into()),
        Value::Text("x".repeat(400)),
        Value::Bytes((0..64).collect()),
        Value::Date(date!(1970 - 01 - 01)),
        Value::Time(time!(23:59:59)),
        Value::Timestamp(datetime!(1970-01-01 00:00)),
        Value::TimestampTz(datetime!(1970-01-01 00:00 UTC)),
        Value::Json(serde_json::json!([])),
        Value::Array(Vec::new()),
        Value::Struct(Vec::new()),
        Value::Opaque {
            type_name: "unknown_oid_918273".into(),
            text: String::new(),
        },
    ]);

    let all_null = Row(vec![Value::Null; 14]);

    // NaN is what `compare`'s `partial_cmp` fallback exists for. Without a row
    // carrying one, that branch is unreachable from every fixture and a sort
    // that mishandles it cannot be caught.
    let nan = {
        let mut cells = vec![Value::Null; 14];
        cells[2] = Value::Float(f64::NAN);
        Row(cells)
    };

    Table {
        name: "types_showcase",
        kind: RelationKind::Table,
        columns: vec![
            Column::new("v_bool", "bool", true),
            Column::new("v_int", "int8", true),
            Column::new("v_float", "float8", true),
            Column::new("v_decimal", "numeric", true),
            Column::new("v_text", "text", true),
            Column::new("v_bytes", "bytea", true),
            Column::new("v_date", "date", true),
            Column::new("v_time", "time", true),
            Column::new("v_timestamp", "timestamp", true),
            Column::new("v_timestamptz", "timestamptz", true),
            Column::new("v_json", "jsonb", true),
            Column::new("v_array", "int8[]", true),
            Column::new("v_struct", "record", true),
            Column::new("v_opaque", "geometry", true),
        ],
        data: Data::Rows(vec![normal, extremes, all_null, nan]),
    }
}

/// Identifiers that break unquoted string interpolation.
///
/// `sqlake-core` ships `Ident`/`QuotedIdent` and a `quote_style` capability so
/// that forgetting to quote is a compile error rather than a query that dies on
/// the first upper-case name. If every fixture identifier were lower-case
/// ASCII, `format!("{schema}.{table}")` would pass the entire suite and break
/// on the first real catalogue.
fn needs_quoting() -> Table {
    let rows = vec![
        Row(vec![
            Value::Int(1),
            Value::Text("quoted".into()),
            Value::Text("ＦＵＬＬ".into()),
        ]),
        Row(vec![
            Value::Int(2),
            Value::Text("unquoted".into()),
            Value::Text("plain".into()),
        ]),
    ];

    Table {
        // A dot inside a single name: splitting a path on '.' instead of
        // keeping it segmented produces three segments here, not two.
        name: "Mixed.Case",
        kind: RelationKind::Table,
        columns: vec![
            // A reserved word, which needs quoting even in lower case.
            Column::new("select", "int8", false),
            Column::new("Column With Spaces", "text", false),
            // A non-ASCII header, so header width sampling has something to
            // chew on as well.
            Column::new("列名", "text", false),
        ],
        data: Data::Rows(rows),
    }
}

/// Sixty columns of deliberately unequal width: forces horizontal scrolling
/// *and* width negotiation.
///
/// The unequal part is load-bearing. If every column wanted the same width, a
/// layout that splits the viewport into sixty equal columns and one that
/// negotiates widths against a budget would produce identical output, and the
/// fixture could not tell the two apart.
fn wide() -> Table {
    const COLS: usize = 60;

    /// How much room column `c` asks for. A few columns want far more than
    /// their neighbours, so something has to be truncated.
    fn demand(c: usize) -> usize {
        if c.is_multiple_of(10) {
            44
        } else if c.is_multiple_of(5) {
            18
        } else {
            6
        }
    }

    let columns = (0..COLS)
        .map(|c| Column::new(format!("col_{c:02}"), "text", false))
        .collect();
    let rows = (0..20)
        .map(|r| {
            Row((0..COLS)
                .map(|c| {
                    let cell = format!("r{r}c{c}");
                    let pad = demand(c).saturating_sub(cell.len());
                    Value::Text(format!("{cell}{}", "-".repeat(pad)))
                })
                .collect())
        })
        .collect();

    Table {
        name: "wide",
        kind: RelationKind::Table,
        columns,
        data: Data::Rows(rows),
    }
}

/// Two hundred thousand rows, produced on demand.
fn big() -> Table {
    Table {
        name: "big",
        kind: RelationKind::Table,
        columns: vec![
            Column::new("id", "int8", false),
            Column::new("bucket", "text", false),
            Column::new("value", "float8", false),
            Column::new("flag", "bool", false),
        ],
        data: Data::Generated { total: BIG_ROWS },
    }
}

/// Display width is not character count. Getting this wrong corrupts every
/// column to the right of the mistake.
///
/// The rows are sized so that the widest row *by character count* and the
/// widest row *by display width* are two different rows — see
/// `char_count_and_display_width_disagree_about_the_widest_row`. Without that
/// property the table proves nothing: a sampler using `.chars().count()` and
/// one using `unicode-width` would compute the same column width, and both
/// would render it cleanly.
fn unicode() -> Table {
    let rows = vec![
        Row(vec![
            Value::Int(1),
            Value::Text("こんにちは世界".into()),
            Value::Text("cjk".into()),
        ]),
        Row(vec![
            Value::Int(2),
            // A ZWJ sequence: many code points, one glyph.
            Value::Text("👩‍💻 家族: 👨‍👩‍👧‍👦".into()),
            Value::Text("emoji".into()),
        ]),
        Row(vec![
            Value::Int(3),
            // Combining acute accent, not the precomposed character.
            Value::Text("e\u{0301}galite\u{0301}".into()),
            Value::Text("combining".into()),
        ]),
        Row(vec![
            Value::Int(4),
            // The widest row on screen, and one of the shortest in `char`s.
            // This is the row that has to decide the column width; if it does
            // not, the fixture is not testing anything.
            Value::Text("ＦＵＬＬＷＩＤＴＨ　ＣＯＬＵＭＮ".into()),
            Value::Text("fullwidth".into()),
        ]),
        Row(vec![
            Value::Int(5),
            // The longest row by `char` count, and no wider than its length on
            // screen — the decoy for a sampler that counts characters.
            Value::Text("line one\nline two\tafter tab".into()),
            Value::Text("control".into()),
        ]),
        Row(vec![
            Value::Int(6),
            // The control characters that actually do damage. A formatter that
            // escapes only `\n` and `\t` passes every other row here and then
            // lets a real `text` column repaint or clear the user's terminal.
            // U+202E reorders everything after it, which no amount of escaping
            // the whitespace will catch.
            Value::Text("\u{1b}[31mSGR\u{1b}[2J\u{0}\rCR\u{202e}reversed".into()),
            Value::Text("escape".into()),
        ]),
        Row(vec![
            Value::Int(7),
            Value::Text("مرحبا بالعالم".into()),
            Value::Text("rtl".into()),
        ]),
    ];

    Table {
        name: "unicode",
        kind: RelationKind::Table,
        columns: vec![
            Column::new("id", "int8", false),
            Column::new("sample", "text", false),
            Column::new("kind", "text", false),
        ],
        data: Data::Rows(rows),
    }
}

/// Columns but no rows: the empty state has to look deliberate.
fn empty() -> Table {
    Table {
        name: "empty",
        kind: RelationKind::Table,
        columns: vec![
            Column::new("id", "int8", false),
            Column::new("label", "text", true),
        ],
        data: Data::Rows(Vec::new()),
    }
}

/// Previewing this always fails. See `Behaviour::fixture`.
fn broken() -> Table {
    Table {
        name: "broken",
        kind: RelationKind::Table,
        columns: vec![Column::new("id", "int8", false)],
        data: Data::Rows(Vec::new()),
    }
}

/// Previewing this takes two seconds.
fn slow() -> Table {
    let rows = (1..=10_i64)
        .map(|i| Row(vec![Value::Int(i), Value::Text(format!("row {i}"))]))
        .collect();

    Table {
        name: "slow",
        kind: RelationKind::Table,
        columns: vec![
            Column::new("id", "int8", false),
            Column::new("label", "text", false),
        ],
        data: Data::Rows(rows),
    }
}

/// A relation whose row count the engine will not report.
///
/// Everything that divides by a total — the scrollbar thumb, "row 30 of N",
/// jump-to-last-page, whether "load more" is offered at all — has to work
/// without one, because a BigQuery preview never provides it.
fn unbounded() -> Table {
    let rows = (1..=120_i64)
        .map(|i| {
            Row(vec![
                Value::Int(i),
                Value::Text(format!("event {i}")),
                Value::TimestampTz(
                    datetime!(2026-08-01 00:00 UTC) + time::Duration::minutes(i * 17),
                ),
            ])
        })
        .collect();

    Table {
        name: "unbounded",
        kind: RelationKind::Table,
        columns: vec![
            Column::new("id", "int8", false),
            Column::new("label", "text", false),
            Column::new("at", "timestamptz", false),
        ],
        data: Data::RowsWithoutTotal(rows),
    }
}

/// Lives in the schema whose expansion fails, so that schema is not an empty
/// branch when failure injection is off.
fn secrets() -> Table {
    Table {
        name: "secrets",
        kind: RelationKind::Table,
        columns: vec![
            Column::new("id", "int8", false),
            Column::new("value", "text", false),
        ],
        data: Data::Rows(vec![Row(vec![
            Value::Int(1),
            Value::Text("you should not be reading this".into()),
        ])]),
    }
}

fn daily_summary() -> Table {
    let rows = (0..30_i64)
        .map(|i| {
            Row(vec![
                Value::Date(date!(2026 - 07 - 01) + time::Duration::days(i)),
                Value::Int(1000 + i * 37),
                Value::Decimal(format!("{}.{:02}", 100 + i, i * 3 % 100)),
            ])
        })
        .collect();

    Table {
        name: "daily_summary",
        kind: RelationKind::View,
        columns: vec![
            Column::new("day", "date", false),
            Column::new("events", "int8", false),
            Column::new("revenue", "numeric", false),
        ],
        data: Data::Rows(rows),
    }
}

#[cfg(test)]
mod tests {
    use sqlake_core::capability::QuoteStyle;
    use sqlake_core::ident::Ident;
    use unicode_width::UnicodeWidthStr;

    use super::*;

    #[test]
    fn every_row_matches_its_column_count() {
        for schema in &catalog().schemas {
            for table in &schema.tables {
                for (i, row) in table.page(0, 100).iter().enumerate() {
                    assert_eq!(
                        row.len(),
                        table.columns.len(),
                        "{}.{} row {i} has the wrong arity",
                        schema.name,
                        table.name
                    );
                }
            }
        }
    }

    #[test]
    fn the_big_table_pages_without_materialising() {
        let cat = catalog();
        let big = cat.table("public", "big").unwrap();
        assert_eq!(big.total_rows(), Some(BIG_ROWS));

        let page = big.page(199_990, 200);
        assert_eq!(page.len(), 10, "the last page is short, not wrapped");
        assert_eq!(page[0].get(0), Some(&Value::Int(199_990)));
    }

    #[test]
    fn generation_is_deterministic() {
        assert_eq!(generated_row(12_345), generated_row(12_345));
    }

    /// One value per `Value` variant.
    ///
    /// The match is exhaustive and has no wildcard, so adding a variant to
    /// `Value` stops this compiling until a sample is added here — and the
    /// caller then fails until the showcase carries a column for it. A
    /// hardcoded list of type names, which is what this replaced, stays green
    /// instead and leaves the new variant's formatter exercised by nothing.
    fn every_value_variant() -> Vec<Value> {
        let samples = vec![
            Value::Null,
            Value::Bool(true),
            Value::Int(0),
            Value::Float(0.0),
            Value::Decimal("0".into()),
            Value::Text(String::new()),
            Value::Bytes(Vec::new()),
            Value::Date(date!(2026 - 01 - 02)),
            Value::Time(time!(00:00:00)),
            Value::Timestamp(datetime!(2026-01-02 00:00)),
            Value::TimestampTz(datetime!(2026-01-02 00:00 UTC)),
            Value::Json(serde_json::Value::Null),
            Value::Array(Vec::new()),
            Value::Struct(Vec::new()),
            Value::Opaque {
                type_name: "probe".into(),
                text: String::new(),
            },
        ];

        for sample in &samples {
            let expected = match sample {
                Value::Null => "null",
                Value::Bool(_) => "bool",
                Value::Int(_) => "int",
                Value::Float(_) => "float",
                Value::Decimal(_) => "decimal",
                Value::Text(_) => "text",
                Value::Bytes(_) => "bytes",
                Value::Date(_) => "date",
                Value::Time(_) => "time",
                Value::Timestamp(_) => "timestamp",
                Value::TimestampTz(_) => "timestamptz",
                Value::Json(_) => "json",
                Value::Array(_) => "array",
                Value::Struct(_) => "struct",
                Value::Opaque { type_name, .. } => type_name.as_str(),
            };
            assert_eq!(sample.type_name(), expected);
        }

        samples
    }

    #[test]
    fn the_showcase_covers_every_value_variant() {
        let cat = catalog();
        let t = cat.table("public", "types_showcase").unwrap();
        let page = t.page(0, 1);
        let covered: Vec<_> = page[0].0.iter().map(Value::type_name).collect();

        for sample in every_value_variant() {
            // Null has no column of its own: every column is nullable and the
            // all-null row covers it.
            if sample.is_null() {
                continue;
            }
            // `Opaque` carries the driver's own type name, so the showcase
            // matches on the column it actually declares.
            let expected = if matches!(sample, Value::Opaque { .. }) {
                "geometry"
            } else {
                sample.type_name()
            };
            assert!(covered.contains(&expected), "{expected} is not covered");
        }
    }

    #[test]
    fn a_nan_reaches_the_fixtures() {
        let cat = catalog();
        let t = cat.table("public", "types_showcase").unwrap();
        assert!(
            t.all_rows()
                .iter()
                .any(|r| matches!(r.get(2), Some(Value::Float(f)) if f.is_nan())),
            "no row carries a NaN, so `compare`'s partial_cmp fallback is dead"
        );
    }

    #[test]
    fn the_empty_table_has_columns_but_no_rows() {
        let cat = catalog();
        let t = cat.table("public", "empty").unwrap();
        assert_eq!(t.total_rows(), Some(0));
        assert_eq!(t.columns.len(), 2);
    }

    #[test]
    fn one_relation_reports_no_total_at_all() {
        let cat = catalog();
        let t = cat.table("analytics", "unbounded").unwrap();
        assert_eq!(t.total_rows(), None);
        assert_eq!(t.page(0, 10).len(), 10, "it still pages");
    }

    #[test]
    fn char_count_and_display_width_disagree_about_the_widest_row() {
        let cat = catalog();
        let t = cat.table("public", "unicode").unwrap();
        let samples: Vec<String> = t
            .all_rows()
            .iter()
            .map(|r| match r.get(1) {
                Some(Value::Text(s)) => s.clone(),
                other => panic!("unexpected sample cell {other:?}"),
            })
            .collect();

        let widest_by_chars = samples.iter().max_by_key(|s| s.chars().count()).unwrap();
        let widest_by_width = samples.iter().max_by_key(|s| s.width()).unwrap();

        // The whole point of this fixture. If one row were widest under both
        // metrics, a sampler using `.chars().count()` and one using
        // `unicode-width` would lay the table out identically and neither
        // would be caught.
        assert_ne!(
            widest_by_chars, widest_by_width,
            "the unicode fixture no longer distinguishes char count from display width"
        );
        assert!(widest_by_width.width() > widest_by_chars.width());
    }

    #[test]
    fn the_unicode_fixture_carries_more_than_harmless_whitespace() {
        let cat = catalog();
        let t = cat.table("public", "unicode").unwrap();
        let all: String = t
            .all_rows()
            .iter()
            .filter_map(|r| match r.get(1) {
                Some(Value::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();

        // `\n` and `\t` are the two control characters that are harmless if
        // they leak. These are the ones that repaint the terminal or reorder
        // the line, and they are what the formatter has to be tested against.
        for (name, ch) in [
            ("ESC", '\u{1b}'),
            ("NUL", '\u{0}'),
            ("CR", '\r'),
            ("RLO", '\u{202e}'),
        ] {
            assert!(
                all.contains(ch),
                "{name} is missing from the unicode fixture"
            );
        }
    }

    #[test]
    fn the_wide_table_does_not_want_uniform_columns() {
        let cat = catalog();
        let t = cat.table("public", "wide").unwrap();
        let row = &t.page(0, 1)[0];
        let widths: Vec<usize> = row
            .0
            .iter()
            .map(|v| match v {
                Value::Text(s) => s.width(),
                other => panic!("unexpected cell {other:?}"),
            })
            .collect();

        let min = *widths.iter().min().unwrap();
        let max = *widths.iter().max().unwrap();
        // Uniform columns cannot distinguish an even split from a negotiated
        // layout, which is the only reason this table has sixty columns.
        assert!(
            max > min * 4,
            "wide has near-uniform columns ({min}..{max}); it proves nothing about width negotiation"
        );
    }

    #[test]
    fn some_identifiers_require_quoting() {
        fn bare(s: &str) -> bool {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        }

        let cat = catalog();
        let idents = cat.schemas.iter().flat_map(|s| {
            std::iter::once(s.name.to_owned()).chain(s.tables.iter().flat_map(|t| {
                std::iter::once(t.name.to_owned()).chain(t.columns.iter().map(|c| c.name.clone()))
            }))
        });

        let awkward: Vec<String> = idents.filter(|i| !bare(i)).collect();
        assert!(
            !awkward.is_empty(),
            "every identifier is bare lower-case ASCII, so unquoted interpolation passes the suite"
        );

        // And quoting one actually changes it, which is what the app layer
        // will rely on.
        let quoted = Ident::new("Mixed.Case").quote(QuoteStyle::DoubleQuote);
        assert_eq!(quoted.as_str(), "\"Mixed.Case\"");
    }
}

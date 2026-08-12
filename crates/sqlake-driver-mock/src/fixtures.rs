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
}

impl Table {
    #[must_use]
    pub fn total_rows(&self) -> u64 {
        match &self.data {
            Data::Rows(rows) => rows.len() as u64,
            Data::Generated { total } => *total,
        }
    }

    /// One page, in storage order.
    #[must_use]
    pub fn page(&self, offset: u64, limit: u32) -> Vec<Row> {
        match &self.data {
            Data::Rows(rows) => rows
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
            Data::Rows(rows) => rows.clone(),
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
                tables: vec![users(), types_showcase(), wide(), big(), unicode(), empty()],
            },
            Schema {
                name: "analytics",
                tables: vec![broken(), slow(), daily_summary()],
            },
            // Expanding this one always fails. See Behaviour::fixture.
            Schema {
                name: "restricted",
                tables: Vec::new(),
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
        data: Data::Rows(vec![normal, extremes, all_null]),
    }
}

/// Sixty columns: forces horizontal scrolling and width negotiation.
fn wide() -> Table {
    const COLS: usize = 60;
    let columns = (0..COLS)
        .map(|c| Column::new(format!("col_{c:02}"), "text", false))
        .collect();
    let rows = (0..20)
        .map(|r| {
            Row((0..COLS)
                .map(|c| Value::Text(format!("r{r}c{c}")))
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
fn unicode() -> Table {
    let rows = vec![
        Row(vec![
            Value::Int(1),
            Value::Text("こんにちは世界".into()),
            Value::Text("ascii".into()),
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
            Value::Text("ＦＵＬＬＷＩＤＴＨ".into()),
            Value::Text("fullwidth".into()),
        ]),
        Row(vec![
            Value::Int(5),
            // Control characters that must never reach the terminal raw.
            Value::Text("line one\nline two\tafter tab".into()),
            Value::Text("control".into()),
        ]),
        Row(vec![
            Value::Int(6),
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
        assert_eq!(big.total_rows(), BIG_ROWS);

        let page = big.page(199_990, 200);
        assert_eq!(page.len(), 10, "the last page is short, not wrapped");
        assert_eq!(page[0].get(0), Some(&Value::Int(199_990)));
    }

    #[test]
    fn generation_is_deterministic() {
        assert_eq!(generated_row(12_345), generated_row(12_345));
    }

    #[test]
    fn the_showcase_covers_every_value_variant() {
        let cat = catalog();
        let t = cat.table("public", "types_showcase").unwrap();
        let page = t.page(0, 1);
        let names: Vec<_> = page[0].0.iter().map(Value::type_name).collect();
        for expected in [
            "bool",
            "int",
            "float",
            "decimal",
            "text",
            "bytes",
            "date",
            "time",
            "timestamp",
            "timestamptz",
            "json",
            "array",
            "struct",
            "geometry",
        ] {
            assert!(names.contains(&expected), "{expected} is not covered");
        }
    }

    #[test]
    fn the_empty_table_has_columns_but_no_rows() {
        let cat = catalog();
        let t = cat.table("public", "empty").unwrap();
        assert_eq!(t.total_rows(), 0);
        assert_eq!(t.columns.len(), 2);
    }
}

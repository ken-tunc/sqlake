//! What a relation is, out of `pg_catalog`.
//!
//! One query per question, and each of them here as a constant so the SQL a
//! user's server sees is readable without running the client — the same rule
//! [`crate::catalog`] follows and for the same reason.
//!
//! Columns come from `pg_attribute` rather than from a prepared statement.
//! `preview` reads its columns off a result and marks them all nullable
//! because the wire protocol does not carry the answer; this is the call that
//! knows, and a `NOT NULL` nobody is shown is a constraint they do not know
//! they have.

use sqlake_core::detail::{ColumnDef, DetailSection, TableDetail};
use sqlake_core::driver::{DriverError, DriverResult};
use sqlake_core::node::TableRef;
use sqlake_core::result::{Column, ResultSet, Row};
use sqlake_core::value::Value;
use tokio_postgres::Client;

/// The relation itself: what kind it is, and its comment.
///
/// Matched on `nspname` and `relname` rather than resolved through the search
/// path: the caller already names the schema, so `to_regclass` would only add
/// a way for a name to resolve to a relation somewhere else.
const RELATION: &str = "\
    SELECT c.relkind, obj_description(c.oid, 'pg_class') \
    FROM pg_catalog.pg_class c \
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
    WHERE n.nspname = $1 AND c.relname = $2";

/// `attnum > 0` skips the system columns — `ctid`, `xmin` and the rest — which
/// exist on every table and are nobody's schema. `NOT attisdropped` skips
/// columns a `DROP COLUMN` left behind: PostgreSQL keeps the slot, and showing
/// it would show a column called `........pg.dropped.1........`.
const COLUMNS: &str = "\
    SELECT a.attname, \
           pg_catalog.format_type(a.atttypid, a.atttypmod), \
           NOT a.attnotnull, \
           pg_catalog.pg_get_expr(d.adbin, d.adrelid), \
           col_description(a.attrelid, a.attnum) \
    FROM pg_catalog.pg_attribute a \
    JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
    LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
    WHERE n.nspname = $1 AND c.relname = $2 \
      AND a.attnum > 0 AND NOT a.attisdropped \
    ORDER BY a.attnum";

/// One row per index, with the statement that would recreate it.
///
/// `pg_get_indexdef` rather than the columns and opclasses reassembled here:
/// it is what `\d` prints, and it already knows about expressions, partial
/// indexes, operator classes and `INCLUDE` — every one of which a hand-built
/// description would get subtly wrong.
const INDEXES: &str = "\
    SELECT ic.relname, \
           pg_catalog.pg_get_indexdef(i.indexrelid), \
           i.indisprimary, \
           i.indisunique \
    FROM pg_catalog.pg_index i \
    JOIN pg_catalog.pg_class c ON c.oid = i.indrelid \
    JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid \
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
    WHERE n.nspname = $1 AND c.relname = $2 \
    ORDER BY i.indisprimary DESC, ic.relname";

/// Triggers, without the ones nobody wrote.
///
/// `tgisinternal` is set on the triggers PostgreSQL creates to enforce foreign
/// keys — three per constraint — and listing them would bury a user's own
/// under bookkeeping they cannot edit and did not ask for. They are already
/// visible as the constraint they belong to.
const TRIGGERS: &str = "\
    SELECT t.tgname, pg_catalog.pg_get_triggerdef(t.oid) \
    FROM pg_catalog.pg_trigger t \
    JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
    WHERE n.nspname = $1 AND c.relname = $2 AND NOT t.tgisinternal \
    ORDER BY t.tgname";

/// Constraints, including the ones inherited from a parent table.
///
/// `contype` is a one-byte code; it is turned into a word here rather than
/// shown raw, because `c` and `f` are not something to make a reader look up.
const CONSTRAINTS: &str = "\
    SELECT con.conname, con.contype, pg_catalog.pg_get_constraintdef(con.oid) \
    FROM pg_catalog.pg_constraint con \
    JOIN pg_catalog.pg_class c ON c.oid = con.conrelid \
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
    WHERE n.nspname = $1 AND c.relname = $2 \
    ORDER BY con.contype, con.conname";

/// How the table is partitioned, and what its partitions are.
///
/// Two questions in one query because the answer to the second is meaningless
/// without the first: a list of children under a table that is not partitioned
/// is inheritance, which is a different thing.
///
/// `pg_get_expr(relpartbound)` is the child's bound — `FOR VALUES FROM … TO …`
/// — which is the fact somebody opening this pane is looking for.
const PARTITIONS: &str = "\
    SELECT pg_catalog.pg_get_partkeydef(c.oid), \
           child.relname, \
           pg_catalog.pg_get_expr(child.relpartbound, child.oid) \
    FROM pg_catalog.pg_class c \
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
    LEFT JOIN pg_catalog.pg_inherits inh ON inh.inhparent = c.oid \
    LEFT JOIN pg_catalog.pg_class child ON child.oid = inh.inhrelid \
    WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'p' \
    ORDER BY child.relname";

pub async fn describe(
    client: &Client,
    database: &str,
    table: &TableRef,
) -> DriverResult<TableDetail> {
    let (schema, name) = split(database, table)?;

    // Together: neither answer feeds the other, and a definition is two round
    // trips either way.
    let names: [&(dyn tokio_postgres::types::ToSql + Sync); 2] = [&schema, &name];
    let (relation, columns) = tokio::join!(
        client.query_opt(RELATION, &names),
        client.query(COLUMNS, &names),
    );
    let relation = relation
        .map_err(query)?
        .ok_or_else(|| DriverError::NotFound(table.to_string()))?;
    let columns = columns.map_err(query)?;

    let mut detail = TableDetail::new(
        table.clone(),
        crate::catalog::relation_kind(relation.get(0)),
        columns
            .iter()
            .map(|row| ColumnDef {
                name: row.get(0),
                // `format_type` is what `\d` prints: `character varying(20)`
                // rather than `varchar` and a modifier to reassemble.
                type_name: row.get(1),
                nullable: row.get(2),
                default: row.get(3),
                comment: row.get(4),
            })
            .collect(),
    );
    detail.comment = relation.get(1);

    // Four more, together. Each is one index scan and none feeds another, so
    // the cost of a definition is one round trip rather than five.
    let (indexes, triggers, constraints, partitions) = tokio::join!(
        client.query(INDEXES, &names),
        client.query(TRIGGERS, &names),
        client.query(CONSTRAINTS, &names),
        client.query(PARTITIONS, &names),
    );
    // A section with no rows is left out rather than shown empty: an "Indexes"
    // heading over nothing reads as a table that has none, which is true, and
    // as a driver that could not ask, which is not — and only one of those is
    // worth a heading.
    for section in [
        indexes_of(&indexes.map_err(query)?),
        triggers_of(&triggers.map_err(query)?),
        constraints_of(&constraints.map_err(query)?),
        partitioning_of(&partitions.map_err(query)?),
    ]
    .into_iter()
    .flatten()
    {
        detail.sections.push(section);
    }
    Ok(detail)
}

fn section(title: &str, columns: Vec<Column>, rows: Vec<Row>) -> Option<DetailSection> {
    (!rows.is_empty()).then(|| DetailSection {
        title: title.to_owned(),
        table: ResultSet::new(columns, rows, None),
    })
}

fn indexes_of(rows: &[tokio_postgres::Row]) -> Option<DetailSection> {
    section(
        "Indexes",
        vec![
            Column::new("name", "text", false),
            Column::new("kind", "text", false),
            Column::new("definition", "text", false),
        ],
        rows.iter()
            .map(|row| {
                let (primary, unique): (bool, bool) = (row.get(2), row.get(3));
                Row(vec![
                    Value::Text(row.get(0)),
                    // Primary before unique: a primary key is unique too, and
                    // saying so is less useful than saying which one it is.
                    Value::Text(
                        if primary {
                            "primary key"
                        } else if unique {
                            "unique"
                        } else {
                            "index"
                        }
                        .to_owned(),
                    ),
                    Value::Text(row.get(1)),
                ])
            })
            .collect(),
    )
}

fn triggers_of(rows: &[tokio_postgres::Row]) -> Option<DetailSection> {
    section(
        "Triggers",
        vec![
            Column::new("name", "text", false),
            Column::new("definition", "text", false),
        ],
        rows.iter()
            .map(|row| Row(vec![Value::Text(row.get(0)), Value::Text(row.get(1))]))
            .collect(),
    )
}

fn constraints_of(rows: &[tokio_postgres::Row]) -> Option<DetailSection> {
    section(
        "Constraints",
        vec![
            Column::new("name", "text", false),
            Column::new("kind", "text", false),
            Column::new("definition", "text", false),
        ],
        rows.iter()
            .map(|row| {
                Row(vec![
                    Value::Text(row.get(0)),
                    Value::Text(constraint_kind(row.get(1)).to_owned()),
                    Value::Text(row.get(2)),
                ])
            })
            .collect(),
    )
}

/// A partitioning section, or none at all for a table that is not partitioned.
///
/// The query answers no rows for an unpartitioned table — `relkind = 'p'` —
/// and one row with a null child for a partitioned one with no partitions yet,
/// which is a real state and worth showing: the key is set and nothing has
/// been created under it.
fn partitioning_of(rows: &[tokio_postgres::Row]) -> Option<DetailSection> {
    section(
        "Partitioning",
        vec![
            Column::new("key", "text", false),
            Column::new("partition", "text", true),
            Column::new("bounds", "text", true),
        ],
        rows.iter()
            .map(|row| {
                Row(vec![
                    Value::Text(row.get(0)),
                    row.get::<_, Option<String>>(1)
                        .map_or(Value::Null, Value::Text),
                    row.get::<_, Option<String>>(2)
                        .map_or(Value::Null, Value::Text),
                ])
            })
            .collect(),
    )
}

/// `contype` as a word.
///
/// Anything unrecognised keeps its letter rather than being called something
/// it is not: a new constraint kind in a future PostgreSQL is better shown as
/// `x` than as `check`.
fn constraint_kind(contype: i8) -> String {
    match u8::try_from(contype).map(char::from) {
        Ok('p') => "primary key".to_owned(),
        Ok('f') => "foreign key".to_owned(),
        Ok('u') => "unique".to_owned(),
        Ok('c') => "check".to_owned(),
        Ok('t') => "constraint trigger".to_owned(),
        Ok('x') => "exclusion".to_owned(),
        Ok(other) => other.to_string(),
        Err(_) => "unknown".to_owned(),
    }
}

/// The schema and relation names out of a path.
///
/// A path naming another database is refused rather than answered about this
/// one: PostgreSQL has no cross-database queries, so it needs another
/// connection, and dropping the segment instead would describe a same-named
/// relation here as though it were the one that was asked for. [`crate::preview`]
/// refuses it for the same reason.
fn split(database: &str, table: &TableRef) -> DriverResult<(String, String)> {
    match table.path.as_slice() {
        [db, schema, name] if db == database => Ok((schema.clone(), name.clone())),
        [db, _, _] => Err(DriverError::NotFound(format!(
            "{table} is in database `{db}`, and this connection is to `{database}`"
        ))),
        // Two segments are already about this connection, so there is nothing
        // to disagree with.
        [schema, name] => Ok((schema.clone(), name.clone())),
        _ => Err(DriverError::NotFound(format!(
            "`{table}` does not name a relation"
        ))),
    }
}

fn query(err: tokio_postgres::Error) -> DriverError {
    DriverError::query(crate::describe(&err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_with_or_without_its_database_names_the_same_relation() {
        // The tree gives three segments and an agent may give two, and both
        // mean the relation on this connection.
        let with = TableRef::new(["app", "public", "users"]);
        let without = TableRef::new(["public", "users"]);
        assert_eq!(
            split("app", &with).unwrap(),
            split("app", &without).unwrap()
        );
        assert_eq!(
            split("app", &with).unwrap(),
            ("public".to_owned(), "users".to_owned())
        );
    }

    #[test]
    fn a_path_naming_another_database_is_refused() {
        // Dropping the segment would answer about `app.public.users` under the
        // name `other.public.users`: the wrong relation, reported as the right
        // one. `preview` refuses the same path rather than paging it.
        let err = split("app", &TableRef::new(["other", "public", "users"])).unwrap_err();
        assert!(matches!(err, DriverError::NotFound(_)), "{err:?}");
    }

    #[test]
    fn a_path_that_names_no_relation_is_refused_before_a_query() {
        for path in [vec!["public"], vec![], vec!["a", "b", "c", "d"]] {
            assert!(
                split("app", &TableRef::new(path.clone())).is_err(),
                "{path:?}"
            );
        }
    }

    #[test]
    fn a_constraint_letter_becomes_a_word_or_stays_a_letter() {
        assert_eq!(constraint_kind(b'p' as i8), "primary key");
        assert_eq!(constraint_kind(b'f' as i8), "foreign key");
        assert_eq!(constraint_kind(b'c' as i8), "check");
        // A kind a future PostgreSQL adds is better shown as its letter than
        // as something it is not.
        assert_eq!(constraint_kind(b'z' as i8), "z");
    }

    #[test]
    fn an_empty_section_is_no_section() {
        // An "Indexes" heading over nothing reads as both "this table has
        // none" and "the driver could not ask", and only one of those is worth
        // a heading.
        assert!(
            section(
                "Indexes",
                vec![Column::new("name", "text", false)],
                Vec::new()
            )
            .is_none()
        );
        assert!(
            section(
                "Indexes",
                vec![Column::new("name", "text", false)],
                vec![Row(vec![Value::Text("users_pkey".to_owned())])],
            )
            .is_some()
        );
    }

    #[test]
    fn the_trigger_query_leaves_out_the_ones_nobody_wrote() {
        // PostgreSQL creates three internal triggers per foreign key. Listing
        // them buries a user's own under bookkeeping they cannot edit.
        assert!(TRIGGERS.contains("NOT t.tgisinternal"));
    }

    #[test]
    fn the_partition_query_asks_only_about_partitioned_tables() {
        // Without `relkind = 'p'` the same join answers with an inheritance
        // child list, which is a different thing under the same heading.
        assert!(PARTITIONS.contains("c.relkind = 'p'"));
    }

    #[test]
    fn the_column_query_leaves_out_what_is_not_a_column() {
        // Both filters are load-bearing and neither is obvious: without the
        // first, every table has six system columns; without the second, a
        // dropped one shows up under a name PostgreSQL invented.
        assert!(COLUMNS.contains("a.attnum > 0"));
        assert!(COLUMNS.contains("NOT a.attisdropped"));
    }
}

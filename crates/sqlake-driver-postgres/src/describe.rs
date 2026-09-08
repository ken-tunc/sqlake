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

use sqlake_core::detail::{ColumnDef, TableDetail};
use sqlake_core::driver::{DriverError, DriverResult};
use sqlake_core::node::TableRef;
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
    Ok(detail)
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
    fn the_column_query_leaves_out_what_is_not_a_column() {
        // Both filters are load-bearing and neither is obvious: without the
        // first, every table has six system columns; without the second, a
        // dropped one shows up under a name PostgreSQL invented.
        assert!(COLUMNS.contains("a.attnum > 0"));
        assert!(COLUMNS.contains("NOT a.attisdropped"));
    }
}

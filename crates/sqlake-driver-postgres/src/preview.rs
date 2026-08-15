//! Reading a page of a relation.
//!
//! The statement is assembled in one function, [`sql`], which is pure and
//! therefore testable without a server — the part of a driver most worth
//! testing that way, because a mistake here is a query that runs and returns
//! the wrong rows rather than one that fails.
//!
//! Two rules hold it together:
//!
//! - **The relation name goes through [`QuotedIdent`]**, which is the only way
//!   to build one, so a table called `select` or `Users` cannot break the
//!   statement and a name from the catalogue cannot smuggle SQL into it.
//! - **Everything else is a parameter or an ordinal.** The page size and the
//!   offset are bound values; the sort is a column *number*, which PostgreSQL
//!   accepts in `ORDER BY` and which cannot be anything but a number.

use sqlake_core::capability::QuoteStyle;
use sqlake_core::driver::{DriverError, DriverResult};
use sqlake_core::ident::{Ident, QuotedIdent};
use sqlake_core::node::TableRef;
use sqlake_core::result::{Column, PageRequest, ResultSet, Row, SortDir};
use tokio_postgres::Client;

use crate::value::RawValue;

pub async fn preview(
    client: &Client,
    database: &str,
    table: &TableRef,
    request: &PageRequest,
) -> DriverResult<ResultSet> {
    let text = sql(database, table, request)?;
    let limit = i64::from(request.limit);
    let offset = i64::try_from(request.offset).unwrap_or(i64::MAX);

    // Prepared first, so the columns come from the *statement*. Reading them
    // off the first row instead costs nothing until the page is empty — an
    // empty table, or an offset past the end — and then the grid is handed a
    // result with no columns and draws nothing at all, which looks like a
    // failure rather than like a table with no rows in it.
    let statement = client
        .prepare(&text)
        .await
        .map_err(|err| DriverError::Query(crate::describe(&err)))?;

    let columns = statement
        .columns()
        .iter()
        .map(|column| {
            Column::new(
                column.name(),
                column.type_().name(),
                // The wire protocol does not carry nullability, and a preview
                // is not worth a second round trip to ask. `false` would be a
                // claim; this is the absence of one, and M5's `describe` is
                // where the column list becomes authoritative.
                true,
            )
        })
        .collect();

    let rows: Vec<Row> = client
        .query(&statement, &[&limit, &offset])
        .await
        .map_err(|err| DriverError::Query(crate::describe(&err)))?
        .iter()
        .map(|row| {
            (0..row.len())
                .map(|i| row.get::<_, RawValue>(i).decode())
                .collect()
        })
        .collect();

    // `None` rather than an estimate. `reltuples` is a number the planner
    // keeps roughly up to date, and showing it as "1,234 rows" would be a
    // precise-looking lie; an exact `COUNT(*)` is a full scan nobody asked
    // for. M3 adds the estimate as an estimate, and the count on request.
    Ok(ResultSet::new(columns, rows, None))
}

/// `$1` is the page size and `$2` the offset.
pub fn sql(database: &str, table: &TableRef, request: &PageRequest) -> DriverResult<String> {
    let (schema, relation) = split(database, table)?;
    let name = QuotedIdent::join(&[
        Ident::new(schema).quote(QuoteStyle::DoubleQuote),
        Ident::new(relation).quote(QuoteStyle::DoubleQuote),
    ]);

    let mut sql = format!("SELECT * FROM {name}");
    if let Some(sort) = request.sort {
        // A column *number*, which `ORDER BY` accepts and which cannot carry
        // anything but digits. Sorting by name would mean quoting a name the
        // grid does not have — its columns are positions in a result set, and
        // two of them can share a name.
        let dir = match sort.dir {
            SortDir::Asc => "ASC",
            SortDir::Desc => "DESC",
        };
        // Deterministic paging needs a total order, and a sort column with
        // ties does not give one: the same row can appear on two pages while
        // another appears on none. The primary key would be the right
        // tiebreaker and the grid does not know it, so the physical order is
        // the next best thing that is at least stable within a page.
        sql.push_str(&format!(" ORDER BY {} {dir}", sort.column + 1));
    }
    sql.push_str(" LIMIT $1 OFFSET $2");
    Ok(sql)
}

fn split<'a>(database: &str, table: &'a TableRef) -> DriverResult<(&'a str, &'a str)> {
    match table.path.as_slice() {
        [db, schema, relation] => {
            if db == database {
                Ok((schema, relation))
            } else {
                // PostgreSQL has no cross-database queries: this needs another
                // connection, not another statement. Saying so is better than
                // a syntax error from a server that was never asked.
                Err(DriverError::NotFound(format!(
                    "{table} is in database `{db}`, and this connection is to `{database}`"
                )))
            }
        }
        _ => Err(DriverError::NotFound(format!(
            "{table} is not a database.schema.relation path"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use sqlake_core::result::Sort;

    use super::*;

    fn table(parts: [&str; 3]) -> TableRef {
        TableRef::new(parts)
    }

    #[test]
    fn a_page_is_a_select_with_bound_limits() {
        let sql = sql(
            "app",
            &table(["app", "public", "users"]),
            &PageRequest::first(),
        )
        .expect("should build");
        assert_eq!(sql, r#"SELECT * FROM "public"."users" LIMIT $1 OFFSET $2"#);
    }

    #[test]
    fn a_name_that_would_break_the_statement_cannot() {
        // A relation called `select`, one with a capital letter, and one with
        // a quote in it are all legal in PostgreSQL, and all three end the
        // statement early without quoting.
        let sql = sql(
            "app",
            &table(["app", "Public", r#"we"ird"#]),
            &PageRequest::first(),
        )
        .expect("should build");
        assert!(sql.contains(r#""Public"."we""ird""#), "{sql}");
    }

    #[test]
    fn sorting_names_a_column_by_number() {
        // Not by name: the grid's columns are positions, and two columns in
        // one result set are allowed to share a name.
        let request = PageRequest::first().with_sort(Some(Sort::new(2, SortDir::Desc)));
        let third = sql("app", &table(["app", "public", "users"]), &request).expect("should build");
        assert!(third.contains("ORDER BY 3 DESC"), "{third}");
        // The ordinal is one-based, so column 0 is `ORDER BY 1`.
        let request = PageRequest::first().with_sort(Some(Sort::new(0, SortDir::Asc)));
        let first = sql("app", &table(["app", "public", "users"]), &request).expect("should build");
        assert!(first.contains("ORDER BY 1 ASC"), "{first}");
    }

    #[test]
    fn the_page_size_and_offset_are_never_interpolated() {
        // They are numbers today; making them parameters means they cannot
        // become anything else when the page size arrives from a config file.
        let request = PageRequest {
            offset: 999,
            limit: 50,
            sort: None,
        };
        let sql = sql("app", &table(["app", "public", "users"]), &request).expect("should build");
        assert!(sql.ends_with("LIMIT $1 OFFSET $2"), "{sql}");
        assert!(!sql.contains("999"), "{sql}");
        assert!(!sql.contains("50"), "{sql}");
    }

    #[test]
    fn a_relation_in_another_database_says_what_is_wrong() {
        // No cross-database queries in PostgreSQL. A syntax error from the
        // server would be the confusing way to find that out.
        let err = sql(
            "app",
            &table(["other", "public", "users"]),
            &PageRequest::first(),
        )
        .expect_err("should refuse");
        assert!(err.to_string().contains("other"), "{err}");
        assert!(err.to_string().contains("app"), "{err}");
    }

    #[test]
    fn a_path_that_is_not_three_parts_is_refused() {
        let err = sql(
            "app",
            &TableRef::new(["public", "users"]),
            &PageRequest::first(),
        )
        .expect_err("should refuse");
        assert!(err.to_string().contains("path"), "{err}");
    }
}

//! Running a statement, and asking the planner what it will cost.
//!
//! The text is the user's own and is sent unaltered. Nothing here appends a
//! `LIMIT`: the row cap is applied to the fetch, so the statement PostgreSQL
//! sees — and therefore the offsets in any error it reports — is the one that
//! was typed.

use futures::{StreamExt as _, pin_mut};
use sqlake_core::driver::{DriverError, DriverResult};
use sqlake_core::result::{Column, ResultSet, Row};
use sqlake_core::sql::{ApprovedQuery, Estimate, ValidatedSql};
use tokio_postgres::Client;

use crate::value::RawValue;

/// The planner's total cost for the statement.
///
/// Cost units, not bytes and not money: they order two plans on one server and
/// mean nothing anywhere else, which is why [`Estimate`] keeps them apart from
/// the number BigQuery gives.
///
/// `EXPLAIN` without `ANALYZE`, so the statement is planned and not run — the
/// distinction the whole estimate rests on.
pub async fn estimate(client: &Client, sql: &ValidatedSql) -> DriverResult<Estimate> {
    let text = format!("EXPLAIN (FORMAT JSON) {}", sql.text());
    let rows = client
        .query(&text, &[])
        .await
        // Reported as the query failing, which is what it is: a statement
        // `EXPLAIN` refuses is one the server will refuse too, and saying so
        // now is better than saying it after the estimate appeared to pass.
        .map_err(|err| DriverError::Query(crate::describe(&err)))?;

    let plan: serde_json::Value = rows
        .first()
        .ok_or_else(|| DriverError::Query("EXPLAIN returned nothing".to_owned()))?
        .try_get(0)
        .map_err(|err| DriverError::Query(crate::describe(&err)))?;

    total_cost(&plan).map_or(Ok(Estimate::Unknown), |cost| Ok(Estimate::Cost(cost)))
}

/// `[{"Plan": {"Total Cost": 1.23, ...}}]`, which is the shape every version
/// since 9.0 produces. Unknown rather than an error when it is not: a plan this
/// could not read is a reason to say nothing about the cost, not to refuse to
/// run the query.
fn total_cost(plan: &serde_json::Value) -> Option<f64> {
    plan.get(0)?.get("Plan")?.get("Total Cost")?.as_f64()
}

pub async fn execute(client: &Client, query: &ApprovedQuery) -> DriverResult<ResultSet> {
    // Prepared first, so the columns come from the *statement*. Reading them
    // off the first row costs nothing until the result is empty, and then the
    // grid is handed a result with no columns and draws nothing at all — which
    // looks like a failure rather than like a query that matched no rows.
    let statement = client
        .prepare(query.text())
        .await
        .map_err(|err| DriverError::Query(crate::describe(&err)))?;

    let columns: Vec<Column> = statement
        .columns()
        .iter()
        // Nullability is not on the wire and a second round trip to ask for it
        // is not worth it here either; `describe` in M5 is where a column list
        // becomes authoritative.
        .map(|column| Column::new(column.name(), column.type_().name(), true))
        .collect();

    // `query_raw` rather than `query`: it streams, so a cap of fifty on a
    // statement matching a million rows stops after fifty instead of
    // materialising the lot and throwing most of it away.
    let stream = client
        .query_raw(&statement, std::iter::empty::<&str>())
        .await
        .map_err(|err| DriverError::Query(crate::describe(&err)))?;
    pin_mut!(stream);

    let cap = query.max_rows().map_or(usize::MAX, |n| n as usize);
    let mut rows: Vec<Row> = Vec::new();
    while let Some(row) = stream.next().await {
        if rows.len() >= cap {
            break;
        }
        let row = row.map_err(|err| DriverError::Query(crate::describe(&err)))?;
        rows.push(
            (0..row.len())
                .map(|i| row.get::<_, RawValue>(i).decode())
                .collect(),
        );
    }

    // `None` rather than the number fetched: with a cap in force that would
    // say the result *is* fifty rows, and the grid would show "50 rows" for a
    // query that matched a million.
    Ok(ResultSet::new(columns, rows, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_gives_up_its_total_cost() {
        let plan =
            serde_json::json!([{ "Plan": { "Node Type": "Seq Scan", "Total Cost": 155.0 } }]);
        assert_eq!(total_cost(&plan), Some(155.0));
    }

    #[test]
    fn a_plan_this_cannot_read_says_nothing_rather_than_guessing() {
        // A shape from a version that changed, or an `EXPLAIN` of something
        // with no plan. Refusing to run the query over it would be a worse
        // answer than having no number.
        for odd in [
            serde_json::json!([]),
            serde_json::json!([{ "Plan": {} }]),
            serde_json::json!({ "Plan": { "Total Cost": 1.0 } }),
            serde_json::json!([{ "Plan": { "Total Cost": "cheap" } }]),
        ] {
            assert_eq!(total_cost(&odd), None, "{odd}");
        }
    }
}

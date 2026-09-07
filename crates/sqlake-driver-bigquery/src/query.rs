//! Running a query, and the dry run that has to come first.
//!
//! This is the module `Capabilities::free_preview` exists to keep people out
//! of. A preview reads storage and is not billed; everything here is, which is
//! why the dry run is not an optimisation — it is the number somebody is shown
//! before agreeing to be charged.

use gcp_bigquery_client::Client;
use gcp_bigquery_client::model::query_request::QueryRequest;
use gcp_bigquery_client::model::query_response::QueryResponse;
use gcp_bigquery_client::model::table_field_schema::TableFieldSchema;
use sqlake_core::driver::{DriverError, DriverResult};
use sqlake_core::result::{Column, ResultSet, Row};
use sqlake_core::sql::{ApprovedQuery, Estimate, ValidatedSql};

use crate::error::{driver_error, listing_failed};
use crate::value;

/// The bytes the query would be billed for, without running it.
///
/// `dryRun` is answered by the query planner and costs nothing. What comes back
/// is a `QueryResponse` with no rows and `totalBytesProcessed` filled in, which
/// is the number `max_bytes_billed` is compared against.
pub async fn estimate(
    client: &Client,
    project: &str,
    location: Option<&str>,
    sql: &ValidatedSql,
) -> DriverResult<Estimate> {
    let mut request = QueryRequest::new(sql.text());
    request.dry_run = Some(true);
    request.location = location.map(ToOwned::to_owned);
    // A dry run reports the bytes a fresh execution would touch. Asking it to
    // consider the cache would let a query be estimated at nothing and then
    // billed in full the moment the cache entry expired between the estimate
    // and the answer.
    request.use_query_cache = Some(false);

    let response = client
        .job()
        .query(project, request)
        .await
        .map_err(listing_failed)?;

    // Unknown rather than an error when the field is missing or unreadable: a
    // number this could not parse is a reason to say nothing about the cost,
    // and the approval path treats "no number" as ungated rather than refused.
    Ok(bytes(&response).map_or(Estimate::Unknown, Estimate::Bytes))
}

fn bytes(response: &QueryResponse) -> Option<u64> {
    response.total_bytes_processed.as_ref()?.parse().ok()
}

/// Run it, and take back at most the rows that were asked for.
///
/// `maxResults` is a cap on the *response*, not on the statement: the job runs
/// and is billed for everything it scanned either way, so this is about how
/// much comes over the wire and not about what it costs. Rewriting the text to
/// add a `LIMIT` would change the offsets any error is reported at, and would
/// be wrong outright for a statement whose last clause is not a `SELECT`.
pub async fn execute(
    client: &Client,
    project: &str,
    location: Option<&str>,
    query: &ApprovedQuery,
) -> DriverResult<ResultSet> {
    let mut request = QueryRequest::new(query.text());
    request.location = location.map(ToOwned::to_owned);
    // Clamped rather than dropped: a cap too large for the field is still a
    // cap, and `and_then(try_from)` would turn it into no cap at all — the one
    // direction to fail in is the one that pulls the whole result over.
    request.max_results = query
        .max_rows()
        .map(|n| i32::try_from(n).unwrap_or(i32::MAX));

    let response = client
        .job()
        .query(project, request)
        .await
        .map_err(listing_failed)?;

    // A job that has not finished inside the synchronous call's own timeout
    // answers with no rows and `jobComplete: false`. Reporting that as an
    // empty result would be a query that quietly returned nothing — and so
    // would a response with no `jobComplete` at all, which is why anything but
    // an explicit `true` is treated as unfinished.
    if response.job_complete != Some(true) {
        return Err(driver_error(
            "the query is still running — a job that outlives the request needs polling, \
             which is not built yet",
        ));
    }
    if let Some(errors) = &response.errors
        && let Some(first) = errors.first()
    {
        return Err(DriverError::Query(first.message.clone().unwrap_or_else(
            || "the query failed and said nothing about why".to_owned(),
        )));
    }

    Ok(result_set(&response))
}

fn result_set(response: &QueryResponse) -> ResultSet {
    let empty: Vec<TableFieldSchema> = Vec::new();
    let fields = response
        .schema
        .as_ref()
        .and_then(|schema| schema.fields.as_deref())
        .unwrap_or(&empty);

    let columns: Vec<Column> = fields
        .iter()
        .map(|field| {
            Column::new(
                &field.name,
                value::type_name(field),
                // `REQUIRED` is the only mode that forbids a null; a field
                // with no mode at all defaults to nullable.
                field.mode.as_deref() != Some("REQUIRED"),
            )
        })
        .collect();

    let rows = response
        .rows
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|row| {
            let cells = row.columns.as_deref().unwrap_or_default();
            // Zipped against the schema rather than against the cells, so a
            // short row still lines up under the right headers — the same rule
            // `preview` follows, and for the same reason.
            Row(fields
                .iter()
                .enumerate()
                .map(|(index, field)| {
                    value::decode(cells.get(index).and_then(|cell| cell.value.as_ref()), field)
                })
                .collect())
        })
        .collect();

    // The job's own count, which is the number of rows the query matched
    // rather than the number that fitted in this response.
    let total = response
        .total_rows
        .as_ref()
        .and_then(|total| total.parse().ok());
    ResultSet::new(columns, rows, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(json: serde_json::Value) -> QueryResponse {
        serde_json::from_value(json).expect("a query response")
    }

    #[test]
    fn a_dry_run_gives_up_its_byte_count() {
        let r = response(serde_json::json!({ "totalBytesProcessed": "1048576" }));
        assert_eq!(bytes(&r), Some(1_048_576));
    }

    #[test]
    fn a_byte_count_that_cannot_be_read_is_unknown_rather_than_zero() {
        // Zero would be "this query is free", which is the one wrong answer:
        // it is the number the budget is compared against.
        assert_eq!(bytes(&response(serde_json::json!({}))), None);
        assert_eq!(
            bytes(&response(serde_json::json!({ "totalBytesProcessed": "" }))),
            None
        );
        assert_eq!(
            bytes(&response(
                serde_json::json!({ "totalBytesProcessed": "lots" })
            )),
            None
        );
    }

    #[test]
    fn a_result_keeps_its_columns_when_it_has_no_rows() {
        // A grid handed a result with no columns draws nothing at all, which
        // looks like a failure rather than like a query that matched nothing.
        let r = response(serde_json::json!({
            "schema": { "fields": [{ "name": "id", "type": "INTEGER", "mode": "REQUIRED" }] },
            "totalRows": "0",
        }));
        let out = result_set(&r);
        assert_eq!(out.column_count(), 1);
        assert_eq!(out.columns[0].name, "id");
        assert!(!out.columns[0].nullable);
        assert_eq!(out.row_count(), 0);
        assert_eq!(out.total_rows, Some(0));
    }

    #[test]
    fn a_short_row_still_lines_up_under_its_headers() {
        let r = response(serde_json::json!({
            "schema": { "fields": [
                { "name": "a", "type": "STRING" },
                { "name": "b", "type": "STRING" },
            ] },
            "rows": [{ "f": [{ "v": "one" }] }],
            "totalRows": "1",
        }));
        let out = result_set(&r);
        assert_eq!(out.row_count(), 1);
        assert_eq!(out.rows[0].len(), 2, "the missing cell has to be a value");
    }

    #[test]
    fn the_total_is_the_job_count_and_not_what_fitted_in_the_page() {
        let r = response(serde_json::json!({
            "schema": { "fields": [{ "name": "a", "type": "STRING" }] },
            "rows": [{ "f": [{ "v": "one" }] }],
            "totalRows": "9000",
        }));
        assert_eq!(result_set(&r).total_rows, Some(9_000));
    }
}

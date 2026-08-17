//! Reading a page of a table without running a query.
//!
//! `tabledata.list` is the whole point of this module and of
//! `Capabilities::free_preview`: it reads rows straight out of storage and is
//! not billed. `SELECT * FROM t LIMIT 200` would return the same rows and
//! charge for the columns it touched, on every click.
//!
//! What it does not send is the schema — a row is positional, with no names
//! and no types — so `tables.get`, also free, supplies those. The two calls do
//! not depend on each other and are made together.

use gcp_bigquery_client::Client;
use gcp_bigquery_client::tabledata::ListQueryParameters;
use sqlake_core::driver::{DriverError, DriverResult};
use sqlake_core::node::TableRef;
use sqlake_core::result::{Column, PageRequest, ResultSet, Row};

use crate::error::{driver_error, listing_failed};
use crate::value;

pub async fn preview(
    client: &Client,
    table: &TableRef,
    req: &PageRequest,
) -> DriverResult<ResultSet> {
    // `Capabilities::sortable_preview` is false, so a caller that reads it
    // never asks. One that does not — an agent sending a request straight in —
    // is refused rather than served an unsorted page it would present as
    // sorted. Honouring it would mean `ORDER BY`, which is a billed query and,
    // by design.md §4.2, needs an `ApprovedQuery` that does not exist yet.
    if req.sort.is_some() {
        return Err(DriverError::Unsupported(format!(
            "previewing {table} cannot be ordered: \
             reading it is free and sorting it would be a query"
        )));
    }

    let [project, dataset, name] = table.path.as_slice() else {
        return Err(driver_error(format!("`{table}` is not a BigQuery table")));
    };

    // Together, because neither answer feeds the other and a page is two round
    // trips to the other side of the planet either way.
    let (described, listed) = tokio::join!(
        client.table().get(project, dataset, name, None),
        client.tabledata().list(
            project,
            dataset,
            name,
            ListQueryParameters {
                start_index: Some(req.offset.to_string()),
                max_results: Some(req.limit),
                // Offsets rather than page tokens: the grid asks for a window
                // by number, and `startIndex` is what answers that. A token
                // only knows how to go forwards from where it was issued.
                page_token: None,
                selected_fields: None,
                format_options: None,
            },
        )
    );
    let described = described.map_err(listing_failed)?;
    let listed = listed.map_err(listing_failed)?;

    let schema = described.schema;
    let fields = schema.fields.as_deref().unwrap_or_default();
    let columns: Vec<Column> = fields
        .iter()
        .map(|field| {
            Column::new(
                &field.name,
                value::type_name(field),
                // `REQUIRED` is the only mode that forbids a null; `NULLABLE`
                // and `REPEATED` both allow one, and a field with no mode at
                // all defaults to nullable.
                field.mode.as_deref() != Some("REQUIRED"),
            )
        })
        .collect();

    let rows = listed
        .rows
        .unwrap_or_default()
        .iter()
        .map(|row| {
            let cells = row.columns.as_deref().unwrap_or_default();
            // Zipped against the schema, not against the cells: a row shorter
            // than the schema still has to line up under the right headers,
            // and one longer than it has values the header row cannot name.
            Row(fields
                .iter()
                .enumerate()
                .map(|(index, field)| {
                    let cell = cells.get(index).and_then(|cell| cell.value.as_ref());
                    value::decode(cell, field)
                })
                .collect())
        })
        .collect();

    Ok(ResultSet::new(
        columns,
        rows,
        total_rows(&described.num_rows),
    ))
}

/// The row count `tables.get` already carries, so the grid can show a position
/// in the relation without a `COUNT(*)` — which on BigQuery would be a job.
///
/// `None` rather than a guess when it is missing or unreadable: the shared
/// model documents an unknown total as the ordinary case, and a wrong one
/// would put the scrollbar in the wrong place for ever.
fn total_rows(num_rows: &Option<String>) -> Option<u64> {
    num_rows.as_ref()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_count_that_cannot_be_read_is_unknown_rather_than_zero() {
        assert_eq!(total_rows(&Some("1200".to_owned())), Some(1200));
        assert_eq!(total_rows(&None), None);
        assert_eq!(total_rows(&Some(String::new())), None);
        // A view reports no count at all, and a scrollbar told "0" would draw
        // a full thumb over rows that are there.
        assert_eq!(total_rows(&Some("not a number".to_owned())), None);
    }
}

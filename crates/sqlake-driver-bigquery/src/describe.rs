//! What a table is, from `tables.get`.
//!
//! The same call [`crate::preview`] makes for its schema, and free: BigQuery
//! charges for bytes a query scans, and metadata is not one. Which is why a
//! definition here costs nothing and a definition on PostgreSQL is two
//! catalogue queries — the same answer, reached by whatever each server makes
//! cheap.
//!
//! One limitation is the client's rather than this crate's, and is worth
//! knowing about before it is met: `gcp-bigquery-client` makes `schema` a
//! required field, so a `tables.get` answer that omits it — which Google does
//! for some external tables — fails to deserialise and arrives here as a
//! driver error rather than as a table with no columns.
//!
//! `describe` must agree with the page about nullability. Both read the same
//! `mode`, and a driver that disagreed with itself would be one where opening
//! the definition changed what a `NOT NULL` meant.

use gcp_bigquery_client::Client;
use gcp_bigquery_client::model::table::Table;
use sqlake_core::detail::{ColumnDef, TableDetail};
use sqlake_core::driver::DriverResult;
use sqlake_core::node::{RelationKind, TableRef};

use crate::error::{driver_error, listing_failed};
use crate::value;

pub async fn describe(client: &Client, table: &TableRef) -> DriverResult<TableDetail> {
    let [project, dataset, name] = table.path.as_slice() else {
        return Err(driver_error(format!("`{table}` is not a BigQuery table")));
    };
    let described = client
        .table()
        .get(project, dataset, name, None)
        .await
        .map_err(listing_failed)?;
    Ok(detail(table, &described))
}

/// Everything the metadata carries, in the shared shape.
///
/// Pure, so the mapping is testable against the JSON Google actually sends
/// rather than only against a server.
fn detail(table: &TableRef, described: &Table) -> TableDetail {
    let fields = described.schema.fields.as_deref().unwrap_or_default();

    let mut detail = TableDetail::new(
        table.clone(),
        kind_of(described.r#type.as_deref()),
        fields
            .iter()
            .map(|field| ColumnDef {
                name: field.name.clone(),
                type_name: value::type_name(field).to_owned(),
                // `REQUIRED` is the only mode that forbids a null; `NULLABLE`
                // and `REPEATED` both allow one, and no mode at all defaults
                // to nullable. The same reading `preview` makes, because a
                // driver that disagreed with itself would change what a
                // constraint meant depending on which pane you opened.
                nullable: field.mode.as_deref() != Some("REQUIRED"),
                // BigQuery has no column defaults in this shape, and inventing
                // an empty string for it would be a default nobody set.
                default: None,
                comment: field.description.clone(),
            })
            .collect(),
    );
    detail.comment = described.description.clone();
    detail.stats = stats(described);
    detail
}

/// The numbers worth showing, and only the ones that are there.
///
/// A row missing is how "this table does not report it" is said: a view has no
/// byte count, and `0 B` would be a claim about storage rather than an absence
/// of one.
fn stats(described: &Table) -> Vec<(String, String)> {
    let mut stats = Vec::new();
    if let Some(rows) = described.num_rows.as_ref().filter(|n| !n.is_empty()) {
        stats.push(("Rows".to_owned(), rows.clone()));
    }
    if let Some(bytes) = described.num_bytes.as_ref().filter(|n| !n.is_empty()) {
        stats.push(("Bytes".to_owned(), bytes.clone()));
    }
    if let Some(location) = described.location.as_ref().filter(|l| !l.is_empty()) {
        stats.push(("Location".to_owned(), location.clone()));
    }
    stats
}

/// `type` as the shared model sees it.
///
/// Named `type` on the wire and `r#type` in the client, which is worth saying
/// once: a grep for `tableType` finds the API's documentation and nothing in
/// this crate.
///
/// Anything unrecognised is a table, for the reason PostgreSQL's mapping gives:
/// a new type is far more likely to be something rows can be read from than
/// not, and the icon being wrong is smaller than the relation vanishing.
fn kind_of(table_type: Option<&str>) -> RelationKind {
    match table_type {
        Some("VIEW") => RelationKind::View,
        Some("MATERIALIZED_VIEW") => RelationKind::MaterializedView,
        Some("EXTERNAL") => RelationKind::External,
        _ => RelationKind::Table,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn described(json: serde_json::Value) -> Table {
        serde_json::from_value(json).expect("a table")
    }

    fn users() -> TableRef {
        TableRef::new(["p", "d", "users"])
    }

    #[test]
    fn a_required_field_is_not_nullable() {
        // The same reading `preview` makes. `tests/preview.rs` asserts it from
        // the other side, and the two have to agree: opening the definition
        // must not change what a constraint means.
        let detail = detail(
            &users(),
            &described(serde_json::json!({
                "tableReference": { "projectId": "p", "datasetId": "d", "tableId": "users" },
                "schema": { "fields": [
                    { "name": "id", "type": "INTEGER", "mode": "REQUIRED" },
                    { "name": "email", "type": "STRING" },
                    { "name": "tags", "type": "STRING", "mode": "REPEATED" },
                ]},
            })),
        );
        let nullable: Vec<bool> = detail.columns.iter().map(|c| c.nullable).collect();
        assert_eq!(nullable, [false, true, true]);
    }

    #[test]
    fn a_description_becomes_a_comment_and_an_absent_one_is_absent() {
        let detail = detail(
            &users(),
            &described(serde_json::json!({
                "tableReference": { "projectId": "p", "datasetId": "d", "tableId": "users" },
                "description": "everyone who signed up",
                "schema": { "fields": [
                    { "name": "id", "type": "INTEGER", "description": "the key" },
                    { "name": "email", "type": "STRING" },
                ]},
            })),
        );
        assert_eq!(detail.comment.as_deref(), Some("everyone who signed up"));
        assert_eq!(detail.columns[0].comment.as_deref(), Some("the key"));
        assert_eq!(detail.columns[1].comment, None);
    }

    #[test]
    fn a_number_this_table_does_not_report_is_left_out() {
        // `0 B` would be a claim about storage rather than an absence of one,
        // and a view genuinely has neither.
        let view = detail(
            &users(),
            &described(serde_json::json!({
                "tableReference": { "projectId": "p", "datasetId": "d", "tableId": "users" },
                "type": "VIEW",
                "schema": { "fields": [{ "name": "id", "type": "INTEGER" }] },
            })),
        );
        assert!(view.stats.is_empty(), "{:?}", view.stats);
        assert_eq!(view.kind, RelationKind::View);

        let table = detail(
            &users(),
            &described(serde_json::json!({
                "tableReference": { "projectId": "p", "datasetId": "d", "tableId": "users" },
                "numRows": "1200",
                "numBytes": "40960",
                "location": "asia-northeast1",
                "schema": { "fields": [{ "name": "id", "type": "INTEGER" }] },
            })),
        );
        let names: Vec<&str> = table.stats.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["Rows", "Bytes", "Location"]);
    }

    #[test]
    fn an_unknown_table_type_is_a_table() {
        assert_eq!(kind_of(None), RelationKind::Table);
        assert_eq!(kind_of(Some("SNAPSHOT")), RelationKind::Table);
        assert_eq!(kind_of(Some("VIEW")), RelationKind::View);
        assert_eq!(kind_of(Some("EXTERNAL")), RelationKind::External);
    }

    #[test]
    fn a_table_with_no_columns_still_describes() {
        // Not the same as no `schema` at all, which `gcp-bigquery-client`
        // refuses to deserialise before this code sees it — see the module
        // doc. An empty field list is reachable and is a relation with no
        // columns, not a failure.
        let detail = detail(
            &users(),
            &described(serde_json::json!({
                "tableReference": { "projectId": "p", "datasetId": "d", "tableId": "users" },
                "schema": { "fields": [] },
            })),
        );
        assert!(detail.columns.is_empty());
    }
}

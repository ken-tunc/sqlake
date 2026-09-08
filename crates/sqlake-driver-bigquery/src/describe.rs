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
use sqlake_core::detail::{ColumnDef, Ddl, DetailSection, TableDetail};
use sqlake_core::driver::DriverResult;
use sqlake_core::node::{RelationKind, TableRef};
use sqlake_core::result::{Column, ResultSet, Row};
use sqlake_core::value::Value;

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
    detail.sections.extend(partitioning(described));
    detail.sections.extend(clustering(described));
    detail.ddl = ddl(table, described);
    detail
}

/// How the table is partitioned, if it is.
///
/// The two kinds are one section rather than two: a table has one or the
/// other, and a reader looking for "how is this split up" should not have to
/// know which word BigQuery used for it.
fn partitioning(described: &Table) -> Option<DetailSection> {
    let mut rows = Vec::new();
    if let Some(time) = &described.time_partitioning {
        rows.push(Row(vec![
            Value::Text(format!("time ({})", time.r#type.to_lowercase())),
            // No field means the table is partitioned by ingestion time, which
            // is a real answer and not a missing one.
            time.field
                .clone()
                .map_or_else(|| Value::Text("_PARTITIONTIME".to_owned()), Value::Text),
            Value::Text(
                if time.require_partition_filter == Some(true) {
                    "required"
                } else {
                    "optional"
                }
                .to_owned(),
            ),
        ]));
    }
    if let Some(range) = &described.range_partitioning {
        rows.push(Row(vec![
            Value::Text("range".to_owned()),
            range.field.clone().map_or(Value::Null, Value::Text),
            // A filter requirement is a time-partitioning property; saying
            // "optional" here would answer a question this kind does not have.
            Value::Null,
        ]));
    }
    (!rows.is_empty()).then(|| DetailSection {
        title: "Partitioning".to_owned(),
        table: ResultSet::new(
            vec![
                Column::new("by", "text", false),
                Column::new("field", "text", true),
                Column::new("filter", "text", true),
            ],
            rows,
            None,
        ),
    })
}

/// The clustering columns, in the order they were declared.
///
/// The order is the whole content: clustering by `(a, b)` and by `(b, a)` sort
/// differently, and a set would lose exactly the fact somebody opened this to
/// see.
fn clustering(described: &Table) -> Option<DetailSection> {
    let fields = described.clustering.as_ref()?.fields.as_ref()?;
    let rows: Vec<Row> = fields
        .iter()
        .enumerate()
        .map(|(at, field)| Row(vec![Value::Int(at as i64 + 1), Value::Text(field.clone())]))
        .collect();
    (!rows.is_empty()).then(|| DetailSection {
        title: "Clustering".to_owned(),
        table: ResultSet::new(
            vec![
                Column::new("position", "int64", false),
                Column::new("field", "text", false),
            ],
            rows,
            None,
        ),
    })
}

/// A view's own query, which is the one thing here that was actually typed.
///
/// Only a view. A table's DDL lives in `INFORMATION_SCHEMA.TABLES.ddl`, and
/// reading it is a *query* — billed, with a ten-megabyte minimum, and issued
/// from a call that takes no `ApprovedQuery`. That would make `describe` the
/// one place in this client where SQL runs without the gate design.md §4.1 is
/// about, to fetch something nobody asked for. So a table has no DDL here and
/// M5's generated one is what it gets.
///
/// The `CREATE VIEW` around it is this client's and the query inside it is the
/// server's, so the whole is generated even though its middle was not.
fn ddl(table: &TableRef, described: &Table) -> Option<Ddl> {
    let view = described.view.as_ref()?;
    let [project, dataset, name] = table.path.as_slice() else {
        return None;
    };
    Some(Ddl::generated(format!(
        "CREATE VIEW `{project}.{dataset}.{name}` AS\n{}",
        view.query
    )))
}

/// The numbers worth showing, and only the ones that are there.
///
/// A field that is absent or empty is left out rather than shown as zero,
/// because "this table does not report it" and "this table has none" are
/// different answers.
///
/// A field reported *as* `"0"` is shown, and that is deliberate rather than
/// settled: an empty table's `Rows 0` is real, and whether a view's
/// `tables.get` omits these or answers zero is not something this driver has
/// been able to check against the API. Filtering zero to tidy up a view would
/// hide the empty table's count on a guess.
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
        // Absent and empty are both "it did not say", which is not the same
        // answer as zero.
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
    fn a_time_partitioned_table_says_what_it_is_partitioned_by() {
        let detail = detail(
            &users(),
            &described(serde_json::json!({
                "tableReference": { "projectId": "p", "datasetId": "d", "tableId": "users" },
                "schema": { "fields": [{ "name": "id", "type": "INTEGER" }] },
                "timePartitioning": { "type": "DAY", "field": "created_at",
                                      "requirePartitionFilter": true },
            })),
        );
        let rows = &detail
            .section("Partitioning")
            .expect("it is partitioned")
            .table
            .rows;
        assert_eq!(rows[0].get(0), Some(&Value::Text("time (day)".to_owned())));
        assert_eq!(rows[0].get(1), Some(&Value::Text("created_at".to_owned())));
        assert_eq!(rows[0].get(2), Some(&Value::Text("required".to_owned())));
    }

    #[test]
    fn partitioning_by_ingestion_time_has_a_field_after_all() {
        // No `field` means `_PARTITIONTIME`, which is a real answer rather
        // than a missing one — and a blank cell would read as the second.
        let detail = detail(
            &users(),
            &described(serde_json::json!({
                "tableReference": { "projectId": "p", "datasetId": "d", "tableId": "users" },
                "schema": { "fields": [{ "name": "id", "type": "INTEGER" }] },
                "timePartitioning": { "type": "DAY" },
            })),
        );
        let rows = &detail
            .section("Partitioning")
            .expect("it is partitioned")
            .table
            .rows;
        assert_eq!(
            rows[0].get(1),
            Some(&Value::Text("_PARTITIONTIME".to_owned()))
        );
    }

    #[test]
    fn clustering_keeps_the_order_it_was_declared_in() {
        // Clustering by `(a, b)` and by `(b, a)` sort differently, so the
        // order is the whole content.
        let detail = detail(
            &users(),
            &described(serde_json::json!({
                "tableReference": { "projectId": "p", "datasetId": "d", "tableId": "users" },
                "schema": { "fields": [{ "name": "id", "type": "INTEGER" }] },
                "clustering": { "fields": ["country", "city"] },
            })),
        );
        let rows = &detail
            .section("Clustering")
            .expect("it is clustered")
            .table
            .rows;
        let fields: Vec<_> = rows.iter().filter_map(|r| r.get(1)).collect();
        assert_eq!(
            fields,
            [
                &Value::Text("country".to_owned()),
                &Value::Text("city".to_owned())
            ]
        );
        assert_eq!(rows[0].get(0), Some(&Value::Int(1)));
    }

    #[test]
    fn a_plain_table_has_neither_section_and_no_ddl() {
        // An empty "Partitioning" would read as a table that is partitioned by
        // nothing, and a table's DDL is not fetched at all — see `ddl`.
        let detail = detail(
            &users(),
            &described(serde_json::json!({
                "tableReference": { "projectId": "p", "datasetId": "d", "tableId": "users" },
                "schema": { "fields": [{ "name": "id", "type": "INTEGER" }] },
            })),
        );
        assert!(detail.sections.is_empty(), "{:?}", detail.sections);
        assert!(detail.ddl.is_none());
    }

    #[test]
    fn a_views_own_query_comes_back_inside_a_create_view() {
        // The query is the server's and the wrapper is this client's, which is
        // why the whole thing is generated.
        let detail = detail(
            &users(),
            &described(serde_json::json!({
                "tableReference": { "projectId": "p", "datasetId": "d", "tableId": "users" },
                "type": "VIEW",
                "schema": { "fields": [{ "name": "id", "type": "INTEGER" }] },
                "view": { "query": "SELECT 1 AS id" },
            })),
        );
        let ddl = detail.ddl.expect("a view has one");
        assert!(
            ddl.text().starts_with("CREATE VIEW `p.d.users` AS"),
            "{}",
            ddl.text()
        );
        assert!(ddl.text().contains("SELECT 1 AS id"), "{}", ddl.text());
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

//! The BigQuery driver, through the shared suite.
//!
//! What this leg proves is the suite's own claim: that the application layer
//! can walk and page this driver exactly as it does the other two, and that
//! the `sortable_preview` it answers false is one the suite holds it to — a
//! refusal, where a driver that offers sorting has to reverse the rows.
//! Nothing else in the workspace could catch this driver claiming it can sort.
//! (`free_preview` is held elsewhere, by `tests/preview.rs` watching for a
//! query that is never issued; the shared suite does not look at it.)
//!
//! **It runs against a fixture, not against a server, and that was not the
//! first choice.** `goccy/bigquery-emulator` implements every endpoint this
//! driver uses, and its `tabledata.list` ignores `startIndex` and `maxResults`
//! outright — it answers `SELECT * FROM t` and returns the whole table. Three
//! of the suite's cases are precisely about those two parameters, so the
//! emulator cannot be put through it without carving them out, and a shared
//! suite with a driver-shaped hole in it is worth less than one that runs on a
//! fixture.
//!
//! So the fixture below serves the window it was asked for. That leaves it
//! testing this driver against a reading of the API rather than against an
//! implementation of it — which is why the wire quirks Google's own responses
//! have live in `tests/preview.rs` and `tests/catalog.rs`, written from what it
//! actually sends.

use std::sync::Arc;

use sqlake_conformance::Subject;
use sqlake_core::node::TableRef;
use wiremock::{Request, Respond, ResponseTemplate};

mod google;
use google::{Google, PROJECT, profile};

const DATASET: &str = "events";
const TABLE: &str = "users";

/// Five rows and three columns, whose first column sorts distinctly — the
/// shape the suite documents, and fewer than the whole page it asks for once.
const ROWS: usize = 5;

#[tokio::test]
async fn the_bigquery_driver_conforms() {
    let google = Google::start().await;
    google.issues_a_token().await;
    google
        .answers_datasets(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "kind": "bigquery#datasetList",
            "etag": "an-etag",
            "datasets": [{
                "kind": "bigquery#dataset",
                "id": format!("{PROJECT}:{DATASET}"),
                "datasetReference": { "projectId": PROJECT, "datasetId": DATASET },
            }],
        })))
        .await;
    google
        .answers_tables(
            DATASET,
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "kind": "bigquery#tableList",
                "tables": [{
                    "kind": "bigquery#table",
                    "id": format!("{PROJECT}:{DATASET}.{TABLE}"),
                    "tableReference": {
                        "projectId": PROJECT,
                        "datasetId": DATASET,
                        "tableId": TABLE,
                    },
                    "type": "TABLE",
                }],
            })),
        )
        .await;
    google
        .describes_table(
            DATASET,
            TABLE,
            serde_json::json!({
                "fields": [
                    { "name": "id", "type": "INTEGER", "mode": "REQUIRED" },
                    { "name": "email", "type": "STRING" },
                    { "name": "signed_up", "type": "DATE" },
                ],
            }),
            Some(&ROWS.to_string()),
        )
        .await;
    google.answers_rows(DATASET, TABLE, Window).await;

    // Everything else in the dataset is missing, which is what the suite's
    // last case asks about. Mounted after the table so the specific answer
    // wins where both would match.
    google.describes_any_table(not_found()).await;
    google.answers_any_rows(not_found()).await;

    sqlake_conformance::run(&Subject {
        driver: Arc::new(google.driver()),
        profile: profile(google.key_file()),
        relation: TableRef::new([PROJECT, DATASET, TABLE]),
        missing: TableRef::new([PROJECT, DATASET, "no_such_relation"]),
    })
    .await;
}

/// The rows `startIndex` and `maxResults` actually asked for.
///
/// A canned page would pass the suite's paging cases without meaning anything:
/// "the second page is not the first page again" is only a test if the fixture
/// could have got it wrong.
struct Window;

impl Respond for Window {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let param = |name: &str| {
            request
                .url
                .query_pairs()
                .find(|(key, _)| key == name)
                .and_then(|(_, value)| value.parse::<usize>().ok())
        };
        let start = param("startIndex").unwrap_or(0);
        let limit = param("maxResults").unwrap_or(ROWS);

        let rows: Vec<serde_json::Value> = (start..ROWS.min(start.saturating_add(limit)))
            .map(|row| {
                serde_json::json!({ "f": [
                    { "v": (row + 1).to_string() },
                    { "v": format!("{}@example.com", (b'a' + row as u8) as char) },
                    { "v": format!("2026-01-{:02}", row + 2) },
                ]})
            })
            .collect();

        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "kind": "bigquery#tableDataList",
            "totalRows": ROWS.to_string(),
            "rows": rows,
        }))
    }
}

fn not_found() -> ResponseTemplate {
    ResponseTemplate::new(404).set_body_json(serde_json::json!({
        "error": {
            "code": 404,
            "message": format!("Not found: Table {PROJECT}:{DATASET}.no_such_relation"),
            "errors": [],
            "status": "NOT_FOUND",
        }
    }))
}

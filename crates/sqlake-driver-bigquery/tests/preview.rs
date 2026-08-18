//! Reading a table, against the stub.
//!
//! The claim this file exists to hold is the one that costs money if it is
//! wrong: **previewing issues no query.** `jobs.insert` is how a query is run,
//! and every test here asserts the stub never saw one.

use sqlake_core::driver::{Driver as _, DriverError, Session};
use sqlake_core::node::TableRef;
use sqlake_core::result::{PageRequest, Sort, SortDir};
use sqlake_core::value::Value;
use wiremock::ResponseTemplate;

mod google;
use google::{Google, PROJECT, profile};

fn users() -> TableRef {
    TableRef::new([PROJECT, "events", "users"])
}

/// Connecting verifies the project, so the stub answers that before anything
/// here gets a session. Left to the unmatched-request path it would still
/// connect — on the decode-error forgiveness — and every test below would be
/// resting on the one gap this driver documents rather than on a real answer.
async fn connected(google: &Google) -> Box<dyn Session> {
    google
        .answers_datasets(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "kind": "bigquery#datasetList",
            "etag": "an-etag",
            "datasets": [{
                "kind": "bigquery#dataset",
                "id": format!("{PROJECT}:events"),
                "datasetReference": { "projectId": PROJECT, "datasetId": "events" },
            }],
        })))
        .await;
    google
        .driver()
        .connect(&profile(google.key_file()))
        .await
        .expect("should connect")
}

#[tokio::test]
async fn a_page_arrives_with_its_columns_and_no_query_is_issued() {
    let google = Google::start().await;
    google.issues_a_token().await;
    google
        .describes_table("events", "users", schema(), Some("1200"))
        .await;
    google
        .answers_rows(
            "events",
            "users",
            rows(&[&["1", "a@example.com"], &["2", "b@example.com"]]),
        )
        .await;
    let session = connected(&google).await;

    let page = session
        .preview(&users(), &PageRequest::first())
        .await
        .expect("should read");

    let columns: Vec<(&str, &str)> = page
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.type_name.as_str()))
        .collect();
    assert_eq!(columns, [("id", "INT64"), ("email", "STRING")]);
    // `REQUIRED` is the only mode that forbids a null. Reporting every column
    // as nullable would be the safe-looking answer and the wrong one: M5 draws
    // the definition from this, and a `NOT NULL` that is not shown is a
    // constraint the user does not know they have.
    let nullable: Vec<bool> = page.columns.iter().map(|c| c.nullable).collect();
    assert_eq!(nullable, [false, true]);
    assert_eq!(page.rows[0].get(0), Some(&Value::Int(1)));
    assert_eq!(
        page.rows[1].get(1),
        Some(&Value::Text("b@example.com".to_owned()))
    );
    // From `tables.get`, so the grid knows where it is in the relation without
    // a `COUNT(*)` — which on BigQuery is a job.
    assert_eq!(page.total_rows, Some(1200));

    assert_eq!(
        google.requests_ending_in("/jobs").await,
        0,
        "previewing must not run a query"
    );
    session.close().await;
}

#[tokio::test]
async fn a_page_is_asked_for_by_offset() {
    // `startIndex`, not a page token: the grid asks for a window by number,
    // and a token only knows how to go forwards from where it was issued.
    let google = Google::start().await;
    google.issues_a_token().await;
    google
        .describes_table("events", "users", schema(), Some("1200"))
        .await;
    google
        .answers_rows_at("events", "users", "400", "200", rows(&[&["400", "x"]]))
        .await;
    let session = connected(&google).await;

    let page = session
        .preview(
            &users(),
            &PageRequest {
                offset: 400,
                limit: 200,
                sort: None,
            },
        )
        .await
        .expect("should read");
    assert_eq!(page.rows[0].get(0), Some(&Value::Int(400)));
    session.close().await;
}

#[tokio::test]
async fn a_sort_is_refused_rather_than_ignored() {
    // `sortable_preview` is false, and this is what makes that answer true.
    // Serving the rows unsorted would look like the feature: the caller draws
    // an arrow over a page that is in storage order.
    let google = Google::start().await;
    google.issues_a_token().await;
    google
        .describes_table("events", "users", schema(), Some("2"))
        .await;
    google
        .answers_rows("events", "users", rows(&[&["1", "a@example.com"]]))
        .await;
    let session = connected(&google).await;

    let err = session
        .preview(
            &users(),
            &PageRequest::first().with_sort(Some(Sort::new(0, SortDir::Asc))),
        )
        .await
        .expect_err("should be refused");
    assert!(matches!(err, DriverError::Unsupported(_)), "{err:?}");
    assert_eq!(
        google.requests_ending_in("/data").await,
        0,
        "a request that cannot be served must not be sent"
    );
    session.close().await;
}

#[tokio::test]
async fn a_record_and_a_repeated_field_arrive_with_their_structure() {
    // The design's grid flattens these into dotted columns for display. The
    // driver does not: `sqlake-api` hands an agent the same `Value`, and it
    // wants the document rather than a screenful of `{2 keys}`.
    let google = Google::start().await;
    google.issues_a_token().await;
    google
        .describes_table(
            "events",
            "nested",
            serde_json::json!({
                "fields": [
                    {
                        "name": "user",
                        "type": "RECORD",
                        "mode": "NULLABLE",
                        "fields": [
                            { "name": "id", "type": "INTEGER", "mode": "NULLABLE" },
                            { "name": "name", "type": "STRING", "mode": "NULLABLE" },
                        ],
                    },
                    { "name": "tags", "type": "STRING", "mode": "REPEATED" },
                ],
            }),
            Some("1"),
        )
        .await;
    google
        .answers_rows(
            "events",
            "nested",
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "kind": "bigquery#tableDataList",
                "totalRows": "1",
                "rows": [{ "f": [
                    { "v": { "f": [{ "v": "7" }, { "v": "ken" }] } },
                    { "v": [{ "v": "a" }, { "v": "b" }] },
                ]}],
            })),
        )
        .await;
    let session = connected(&google).await;

    let page = session
        .preview(
            &TableRef::new([PROJECT, "events", "nested"]),
            &PageRequest::first(),
        )
        .await
        .expect("should read");

    assert_eq!(
        page.columns[1].type_name, "ARRAY<STRING>",
        "the header has to say it is repeated"
    );
    assert_eq!(
        page.rows[0].get(0),
        Some(&Value::Struct(vec![
            ("id".to_owned(), Value::Int(7)),
            ("name".to_owned(), Value::Text("ken".to_owned())),
        ]))
    );
    assert_eq!(
        page.rows[0].get(1),
        Some(&Value::Array(vec![
            Value::Text("a".to_owned()),
            Value::Text("b".to_owned()),
        ]))
    );
    session.close().await;
}

#[tokio::test]
async fn a_page_past_the_end_still_has_columns() {
    // What the conformance suite asks of every driver: the grid draws its
    // header from the columns, so a result with none of them reads as a failed
    // request rather than as the end of a table.
    let google = Google::start().await;
    google.issues_a_token().await;
    google
        .describes_table("events", "users", schema(), Some("2"))
        .await;
    google
        .answers_rows(
            "events",
            "users",
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "kind": "bigquery#tableDataList" })),
        )
        .await;
    let session = connected(&google).await;

    let page = session
        .preview(&users(), &PageRequest::first())
        .await
        .expect("should read");
    assert_eq!(page.row_count(), 0);
    assert_eq!(page.column_count(), 2);
    session.close().await;
}

#[tokio::test]
async fn a_row_shorter_than_the_schema_still_lines_up() {
    // Cells are positional and carry no names, so a short row read against
    // itself would slide every later value one column to the left — under a
    // header that belongs to something else.
    let google = Google::start().await;
    google.issues_a_token().await;
    google
        .describes_table("events", "users", schema(), Some("1"))
        .await;
    google
        .answers_rows("events", "users", rows(&[&["1"]]))
        .await;
    let session = connected(&google).await;

    let page = session
        .preview(&users(), &PageRequest::first())
        .await
        .expect("should read");
    assert_eq!(page.rows[0].len(), 2);
    assert_eq!(page.rows[0].get(1), Some(&Value::Null));
    session.close().await;
}

#[tokio::test]
async fn a_path_that_is_not_a_table_is_refused_before_anything_is_asked() {
    // A three-segment path is what the tree produces. Anything else is a
    // caller that built its own, and indexing into it would either panic or
    // send `/datasets//tables/` — a URL the API resolves against whatever the
    // credential defaults to.
    let google = Google::start().await;
    google.issues_a_token().await;
    let session = connected(&google).await;

    let before = google.requests_ending_in("/data").await;
    let err = session
        .preview(&TableRef::new(["events", "users"]), &PageRequest::first())
        .await
        .expect_err("not a table");
    assert!(err.to_string().contains("is not a BigQuery table"), "{err}");
    assert_eq!(google.requests_ending_in("/data").await, before);
    session.close().await;
}

#[tokio::test]
async fn a_table_that_is_not_there_is_an_error() {
    let google = Google::start().await;
    google.issues_a_token().await;
    google
        .describes_table_with(
            "events",
            "gone",
            ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": {
                    "code": 404,
                    "message": "Not found: Table analytics-prod:events.gone",
                    "errors": [],
                    "status": "NOT_FOUND",
                }
            })),
        )
        .await;
    google
        .answers_rows(
            "events",
            "gone",
            ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": {
                    "code": 404,
                    "message": "Not found: Table analytics-prod:events.gone",
                    "errors": [],
                    "status": "NOT_FOUND",
                }
            })),
        )
        .await;
    let session = connected(&google).await;

    let err = session
        .preview(
            &TableRef::new([PROJECT, "events", "gone"]),
            &PageRequest::first(),
        )
        .await
        .expect_err("should be refused");
    // Not `Unsupported`: that means the caller asked for something the driver
    // does not do, and reading a table is not that.
    assert!(!matches!(err, DriverError::Unsupported(_)), "{err:?}");
    assert!(err.to_string().contains("Not found"), "{err}");
    session.close().await;
}

// ── fixtures ───────────────────────────────────────────────────────────────

fn schema() -> serde_json::Value {
    serde_json::json!({
        "fields": [
            { "name": "id", "type": "INTEGER", "mode": "REQUIRED" },
            { "name": "email", "type": "STRING", "mode": "NULLABLE" },
        ],
    })
}

fn rows(values: &[&[&str]]) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "kind": "bigquery#tableDataList",
        "totalRows": values.len().to_string(),
        "rows": values.iter().map(|row| serde_json::json!({
            "f": row.iter().map(|v| serde_json::json!({ "v": v })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    }))
}

//! Walking the tree, against the stub.
//!
//! The property the conformance suite will check in T5 is that the tree has
//! exactly as many levels as `Capabilities` claims. What it cannot check
//! without a server is the part these tests are for: which URL each level
//! asks for, and what the driver does with the answers — including the two
//! answers that are easy to get wrong, an empty list and a second page.

use sqlake_core::driver::{Driver, Session};
use sqlake_core::node::{NodeKind, NodeRef, RelationKind};
use wiremock::ResponseTemplate;

mod google;
use google::{Google, PROJECT, profile};

#[tokio::test]
async fn the_tree_fills_to_three_levels() {
    let google = Google::start().await;
    google.issues_a_token().await;
    google
        .answers_datasets(datasets(&["events", "staging"], None))
        .await;
    google
        .answers_tables(
            "events",
            tables(&[("clicks", "TABLE"), ("daily", "VIEW"), ("feed", "EXTERNAL")]),
        )
        .await;
    let session = connected(&google).await;

    // The root is the project the profile named, not a list of every project
    // the credential can see.
    let projects = session.children(&NodeRef::root()).await.expect("projects");
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].label, PROJECT);
    assert_eq!(projects[0].node_ref.kind, NodeKind::Catalog);

    let datasets = session
        .children(&projects[0].node_ref)
        .await
        .expect("datasets");
    let names: Vec<&str> = datasets.iter().map(|d| d.label.as_str()).collect();
    assert_eq!(names, ["events", "staging"]);
    assert!(
        datasets
            .iter()
            .all(|d| d.node_ref.kind == NodeKind::Namespace)
    );

    let tables = session
        .children(&datasets[0].node_ref)
        .await
        .expect("tables");
    let kinds: Vec<(&str, Option<RelationKind>)> = tables
        .iter()
        .map(|t| (t.label.as_str(), t.relation_kind))
        .collect();
    assert_eq!(
        kinds,
        [
            ("clicks", Some(RelationKind::Table)),
            ("daily", Some(RelationKind::View)),
            ("feed", Some(RelationKind::External)),
        ]
    );

    // And a table is a leaf: its columns are `describe`'s answer in M5, not
    // another level of tree. Asserted as "asked Google nothing" rather than as
    // "came back empty", because a request that goes out and returns nothing
    // useful satisfies the second and is still a round trip per table the tree
    // draws.
    let before = google.requests_ending_in("").await;
    assert!(
        session
            .children(&tables[0].node_ref)
            .await
            .expect("a leaf")
            .is_empty()
    );
    assert_eq!(google.requests_ending_in("").await, before);
    session.close().await;
}

#[tokio::test]
async fn a_dataset_is_addressed_inside_its_own_project() {
    // The path carries `[project, dataset]`, and dropping the project would
    // still produce a URL — one that asks the API for a dataset in whichever
    // project the credential defaults to.
    let google = Google::start().await;
    google.issues_a_token().await;
    google.answers_datasets(datasets(&["events"], None)).await;
    google
        .answers_tables("events", tables(&[("clicks", "TABLE")]))
        .await;
    let session = connected(&google).await;

    let dataset = NodeRef::new(NodeKind::Namespace, [PROJECT, "events"]);
    let tables = session.children(&dataset).await.expect("tables");
    assert_eq!(
        tables[0].node_ref.path,
        [PROJECT, "events", "clicks"],
        "a relation's path has to be one `preview` will accept"
    );
    assert_eq!(
        google
            .requests_ending_in(&format!("/{PROJECT}/datasets/events/tables"))
            .await,
        1
    );
    session.close().await;
}

#[tokio::test]
async fn a_project_with_no_datasets_is_an_empty_branch_not_a_failure() {
    // The API omits the `datasets` key entirely rather than sending an empty
    // array, and the crate models it as a plain `Vec`. Reported as an error,
    // this would put a red node where a new project should show as open and
    // empty.
    let google = Google::start().await;
    google.issues_a_token().await;
    google
        .answers_datasets(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "kind": "bigquery#datasetList",
            "etag": "an-etag",
        })))
        .await;
    let session = connected(&google).await;

    let project = NodeRef::new(NodeKind::Catalog, [PROJECT]);
    assert!(
        session
            .children(&project)
            .await
            .expect("no datasets")
            .is_empty()
    );
    session.close().await;
}

#[tokio::test]
async fn a_dataset_with_no_tables_is_an_empty_branch_too() {
    let google = Google::start().await;
    google.issues_a_token().await;
    google.answers_datasets(datasets(&["events"], None)).await;
    google
        .answers_tables(
            "events",
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "kind": "bigquery#tableList" })),
        )
        .await;
    let session = connected(&google).await;

    let dataset = NodeRef::new(NodeKind::Namespace, [PROJECT, "events"]);
    assert!(
        session
            .children(&dataset)
            .await
            .expect("no tables")
            .is_empty()
    );
    session.close().await;
}

#[tokio::test]
async fn every_page_of_datasets_is_followed() {
    // A page is 50 by default. Stopping at the first one would hide most of
    // the datasets in any project worth browsing, and hide them silently:
    // there is nothing in the tree to say the list was cut off.
    let google = Google::start().await;
    google.issues_a_token().await;
    google
        .answers_datasets_at_page(None, datasets(&["a", "b"], Some("page-2")))
        .await;
    google
        .answers_datasets_at_page(Some("page-2"), datasets(&["c"], None))
        .await;
    let session = connected(&google).await;

    let project = NodeRef::new(NodeKind::Catalog, [PROJECT]);
    let found = session.children(&project).await.expect("datasets");
    let names: Vec<&str> = found.iter().map(|d| d.label.as_str()).collect();
    assert_eq!(names, ["a", "b", "c"]);
    session.close().await;
}

#[tokio::test]
async fn every_page_of_tables_is_followed_too() {
    // The one that bites sooner: a dataset holding a table per day passes 50
    // in under two months, and BigQuery's own public datasets are far past it.
    let google = Google::start().await;
    google.issues_a_token().await;
    google.answers_datasets(datasets(&["events"], None)).await;
    google
        .answers_tables_at_page(
            "events",
            None,
            tables_page(&[("clicks", "TABLE")], Some("page-2")),
        )
        .await;
    google
        .answers_tables_at_page(
            "events",
            Some("page-2"),
            tables_page(&[("impressions", "TABLE")], None),
        )
        .await;
    let session = connected(&google).await;

    let dataset = NodeRef::new(NodeKind::Namespace, [PROJECT, "events"]);
    let found = session.children(&dataset).await.expect("tables");
    let names: Vec<&str> = found.iter().map(|t| t.label.as_str()).collect();
    assert_eq!(names, ["clicks", "impressions"]);
    session.close().await;
}

#[tokio::test]
async fn a_refused_table_listing_says_why() {
    // Not the same path as a refused connection: that one is reported as a
    // `Connect` failure on the connection's own row, and this one has to carry
    // Google's words to the tree node the user clicked.
    let google = Google::start().await;
    google.issues_a_token().await;
    google.answers_datasets(datasets(&["events"], None)).await;
    google
        .answers_tables(
            "events",
            ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "error": {
                    "code": 403,
                    "message": "Access Denied: Dataset analytics-prod:events: \
                                User does not have bigquery.tables.list permission.",
                    "errors": [],
                    "status": "PERMISSION_DENIED",
                }
            })),
        )
        .await;
    let session = connected(&google).await;

    let dataset = NodeRef::new(NodeKind::Namespace, [PROJECT, "events"]);
    let err = session
        .children(&dataset)
        .await
        .expect_err("should be refused");
    let message = err.to_string();
    assert!(message.contains("bigquery.tables.list"), "{message}");
    assert!(message.contains("403"), "{message}");
    assert!(!message.contains("NestedResponseError"), "{message}");
    session.close().await;
}

#[tokio::test]
async fn a_refused_listing_says_why() {
    let google = Google::start().await;
    google.issues_a_token().await;
    google
        .answers_datasets(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "error": {
                "code": 403,
                "message": "Access Denied: Project analytics-prod: \
                            User does not have bigquery.datasets.list permission.",
                "errors": [],
                "status": "PERMISSION_DENIED",
            }
        })))
        .await;
    // Connecting has to get past the same call, so the driver is asked
    // directly rather than through a session.
    let err = google
        .driver()
        .connect(&profile(google.key_file()))
        .await
        .expect_err("should be refused");
    assert!(err.to_string().contains("bigquery.datasets.list"), "{err}");
}

// ── fixtures ───────────────────────────────────────────────────────────────

async fn connected(google: &Google) -> Box<dyn Session> {
    google
        .driver()
        .connect(&profile(google.key_file()))
        .await
        .expect("should connect")
}

fn datasets(names: &[&str], next: Option<&str>) -> ResponseTemplate {
    let mut body = serde_json::json!({
        "kind": "bigquery#datasetList",
        "etag": "an-etag",
        "datasets": names.iter().map(|name| serde_json::json!({
            "kind": "bigquery#dataset",
            "id": format!("{PROJECT}:{name}"),
            "datasetReference": { "projectId": PROJECT, "datasetId": name },
        })).collect::<Vec<_>>(),
    });
    if let Some(next) = next {
        body["nextPageToken"] = serde_json::json!(next);
    }
    ResponseTemplate::new(200).set_body_json(body)
}

fn tables(entries: &[(&str, &str)]) -> ResponseTemplate {
    tables_page(entries, None)
}

fn tables_page(entries: &[(&str, &str)], next: Option<&str>) -> ResponseTemplate {
    let mut body = serde_json::json!({
        "kind": "bigquery#tableList",
        "tables": entries.iter().map(|(name, kind)| serde_json::json!({
            "kind": "bigquery#table",
            "id": format!("{PROJECT}:events.{name}"),
            "tableReference": {
                "projectId": PROJECT,
                "datasetId": "events",
                "tableId": name,
            },
            "type": kind,
        })).collect::<Vec<_>>(),
    });
    if let Some(next) = next {
        body["nextPageToken"] = serde_json::json!(next);
    }
    ResponseTemplate::new(200).set_body_json(body)
}

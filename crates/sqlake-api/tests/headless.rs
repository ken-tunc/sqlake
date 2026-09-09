//! What a caller gets from a store it drove itself, with no terminal.
//!
//! Outside `src` because it is the crate's whole job seen from outside it:
//! connect, expand, preview, print — the three use cases A1 has, and nothing
//! it adds.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value as Json, json};
use sqlake_api::{Budget, Page, SessionInfo};
use sqlake_app::action::Action;
use sqlake_app::store::{Drivers, Store, Wiring};
use sqlake_core::id::{ConnId, ProfileId};
use sqlake_core::node::TableRef;
use sqlake_driver_mock::{Behaviour, MockDriver, MockProfiles};

const LIMIT: Duration = Duration::from_secs(5);

fn store() -> Store {
    Store::spawn(Wiring::new(
        Drivers::new().with(Arc::new(MockDriver::new(Behaviour::instant()))),
        Arc::new(MockProfiles::default()),
    ))
}

async fn previewed(store: &Store, conn: ConnId, table: &TableRef) -> Arc<sqlake_app::Snapshot> {
    store
        .dispatch_and_settle(
            Action::Connect {
                profile: ProfileId::parse("mock").expect("a usable id"),
                conn,
            },
            LIMIT,
            |s| s.connection_settled(conn),
        )
        .await
        .expect("the connection settles");
    store
        .dispatch_and_settle(
            Action::PreviewTable {
                conn,
                table: table.clone(),
            },
            LIMIT,
            |s| s.preview_settled(conn, table),
        )
        .await
        .expect("the preview settles")
}

/// One column per `Value` variant, so this covers the conversion in full
/// rather than the shapes somebody remembered to write a unit test for.
#[tokio::test]
async fn a_page_of_every_kind_of_value_serialises() {
    let store = store();
    let conn = ConnId::new();
    let table = TableRef::new(["public", "types_showcase"]);
    let snapshot = previewed(&store, conn, &table).await;

    let result = snapshot
        .preview(conn, &table)
        .and_then(|p| p.data.ready())
        .expect("rows");
    let page = Page::of(result, Budget::DEFAULT);
    let text = serde_json::to_string(&page).expect("serialises");
    let back: Json = serde_json::from_str(&text).expect("is JSON");

    let names: Vec<&str> = page.columns.iter().map(|c| c.name.as_str()).collect();
    let cell = |row: usize, column: &str| -> Json {
        let index = names.iter().position(|n| *n == column).expect(column);
        back["rows"][row][index].clone()
    };

    assert_eq!(cell(0, "v_bool"), json!(true));
    assert_eq!(cell(0, "v_int"), json!(42));
    assert_eq!(cell(0, "v_decimal"), json!("12345.6789"));
    assert_eq!(cell(0, "v_bytes"), json!({"$base64": "AAH+/w=="}));
    assert_eq!(cell(0, "v_date"), json!("2026-01-02"));
    assert_eq!(cell(0, "v_time"), json!("10:30:00"));
    assert_eq!(cell(0, "v_timestamptz"), json!("2026-01-02T10:30:00Z"));
    assert_eq!(cell(0, "v_json"), json!({"a": 1, "b": [true, null]}));
    assert_eq!(cell(0, "v_array"), json!([1, 2, 3]));
    assert_eq!(cell(0, "v_struct"), json!({"name": "ada", "age": 36}));
    assert_eq!(
        cell(0, "v_opaque"),
        json!({"$opaque": {"type": "geometry", "text": "POINT(139.6917 35.6895)"}})
    );

    // The rows the grid exists to survive: extremes, then nulls, then a NaN.
    assert_eq!(cell(1, "v_int"), json!({"$int": "-9223372036854775808"}));
    assert_eq!(cell(1, "v_float"), json!({"$float": "-inf"}));
    assert_eq!(cell(1, "v_array"), json!([]));
    assert_eq!(cell(2, "v_bool"), json!(null));
    assert_eq!(cell(3, "v_float"), json!({"$float": "NaN"}));

    assert_eq!(page.returned, page.rows.len());
    assert!(!page.truncated, "the fixture is smaller than the budget");
}

#[tokio::test]
async fn a_relation_wider_than_the_budget_says_what_it_left_out() {
    let store = store();
    let conn = ConnId::new();
    let table = TableRef::new(["public", "wide"]);
    let snapshot = previewed(&store, conn, &table).await;

    let result = snapshot
        .preview(conn, &table)
        .and_then(|p| p.data.ready())
        .expect("rows");
    let page = Page::of(
        result,
        Budget {
            max_rows: 2,
            max_columns: 3,
        },
    );

    assert_eq!(page.rows.len(), 2);
    assert!(page.truncated);
    assert!(page.loaded > page.returned);
    assert_eq!(page.columns.len(), 3);
    assert_eq!(
        page.omitted_columns.len(),
        result.columns().len() - 3,
        "every column left out is named"
    );
}

#[tokio::test]
async fn an_agent_can_see_what_it_may_connect_to_before_connecting() {
    let session = SessionInfo::from(&*store().snapshot());
    assert!(session.connections.is_empty());
    assert!(
        session.profiles.iter().any(|p| p.id == "mock"),
        "{:?}",
        session.profiles
    );
}

//! The mock, through the shared suite.
//!
//! It has no database behind it, so this is the run that says whether a case
//! is about drivers or about PostgreSQL: a case the mock cannot pass is a case
//! that assumes a real server.

use std::sync::Arc;

use sqlake_conformance::Subject;
use sqlake_core::node::TableRef;
use sqlake_driver_mock::{Behaviour, ESTIMATES, MockDriver, NO_SORT, mock_profile};

#[tokio::test]
async fn the_mock_driver_conforms() {
    sqlake_conformance::run(&subject(MockDriver::new(refusing()))).await;
}

/// And so does one that estimates.
///
/// The default set does not, so without this the branch every costing driver
/// takes — a number, compared to a budget — is written and never executed
/// anywhere CI can reach.
#[tokio::test]
async fn a_driver_that_estimates_conforms_too() {
    sqlake_conformance::run(&subject(
        MockDriver::new(refusing()).with_capabilities(ESTIMATES),
    ))
    .await;
}

/// And so does one that cannot sort a preview.
///
/// The other half of the suite's sort case, and the only way to run it until
/// BigQuery lands: without this the branch taken by every driver that answers
/// `sortable_preview` false is written and never executed.
#[tokio::test]
async fn a_driver_that_cannot_sort_conforms_too() {
    sqlake_conformance::run(&subject(
        MockDriver::new(refusing()).with_capabilities(NO_SORT),
    ))
    .await;
}

/// What the mock is told to refuse, so the suite has a statement the "server"
/// says no to.
const WRONG: &str = "sql_that_is_wrong";

/// A mock that says no to [`WRONG`], which is what stands in for a server
/// refusing a statement.
fn refusing() -> Behaviour {
    Behaviour {
        failing_sql: vec![WRONG.to_owned()],
        estimate_bytes: 4096,
        ..Behaviour::instant()
    }
}

fn subject(driver: MockDriver) -> Subject {
    Subject {
        driver: Arc::new(driver),
        profile: mock_profile("mock"),
        relation: TableRef::new(["public", "users"]),
        missing: TableRef::new(["public", "no_such_relation"]),
        query: "select * from public.users".to_owned(),
        // Over three lines, so the position the suite checks is about a line
        // rather than always the first one.
        broken_query: format!("select\n  {WRONG}\nfrom public.users"),
    }
}

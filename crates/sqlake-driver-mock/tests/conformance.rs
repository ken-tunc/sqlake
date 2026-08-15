//! The mock, through the shared suite.
//!
//! It has no database behind it, so this is the run that says whether a case
//! is about drivers or about PostgreSQL: a case the mock cannot pass is a case
//! that assumes a real server.

use std::sync::Arc;

use sqlake_conformance::Subject;
use sqlake_core::node::TableRef;
use sqlake_driver_mock::{Behaviour, MockDriver, NO_SORT, mock_profile};

#[tokio::test]
async fn the_mock_driver_conforms() {
    sqlake_conformance::run(&subject(MockDriver::new(Behaviour::instant()))).await;
}

/// And so does one that cannot sort a preview.
///
/// The other half of the suite's sort case, and the only way to run it until
/// BigQuery lands: without this the branch taken by every driver that answers
/// `sortable_preview` false is written and never executed.
#[tokio::test]
async fn a_driver_that_cannot_sort_conforms_too() {
    sqlake_conformance::run(&subject(
        MockDriver::new(Behaviour::instant()).with_capabilities(NO_SORT),
    ))
    .await;
}

fn subject(driver: MockDriver) -> Subject {
    Subject {
        driver: Arc::new(driver),
        profile: mock_profile("mock"),
        relation: TableRef::new(["public", "users"]),
        missing: TableRef::new(["public", "no_such_relation"]),
    }
}

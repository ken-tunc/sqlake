//! The mock, through the shared suite.
//!
//! It has no database behind it, so this is the run that says whether a case
//! is about drivers or about PostgreSQL: a case the mock cannot pass is a case
//! that assumes a real server.

use std::sync::Arc;

use sqlake_conformance::Subject;
use sqlake_core::node::TableRef;
use sqlake_driver_mock::{Behaviour, MockDriver, mock_profile};

#[tokio::test]
async fn the_mock_driver_conforms() {
    sqlake_conformance::run(&Subject {
        driver: Arc::new(MockDriver::new(Behaviour::instant())),
        profile: mock_profile("mock"),
        relation: TableRef::new(["public", "users"]),
        missing: TableRef::new(["public", "no_such_relation"]),
    })
    .await;
}

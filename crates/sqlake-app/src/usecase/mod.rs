//! Application operations, each with its input and output expressed as types.
//!
//! Named types rather than tuples or a generic map is the point: making
//! "before" and "after" distinct types is what turns a skipped step into a
//! compile error — the SQL pipeline (`RawSql` → `ValidatedSql` →
//! `ApprovedQuery`) is what it was written for.
//!
//! Dependencies are struct fields, so injecting the mock driver makes each use
//! case testable on its own.

use async_trait::async_trait;

use crate::error::AppResult;

pub mod connect;
pub mod describe_table;
pub mod estimate_query;
pub mod expand_node;
pub mod preview_table;
pub mod run_query;

pub use connect::{Connect, ConnectInput, ConnectOutput};
pub use describe_table::{DescribeTable, DescribeTableInput};
pub use estimate_query::{EstimateQuery, EstimateQueryInput};
pub use expand_node::{ExpandNode, ExpandNodeInput, ExpandNodeOutput};
pub use preview_table::{PreviewTable, PreviewTableInput, PreviewTableOutput};
pub use run_query::{RunApproved, RunQuery, RunQueryInput, RunQueryOutput};

#[async_trait]
pub trait UseCase {
    type Input;
    type Output;

    async fn execute(&self, input: Self::Input) -> AppResult<Self::Output>;
}

//! Application operations, each with its input and output expressed as types.
//!
//! Named types rather than tuples or a generic map is the point: making
//! "before" and "after" distinct types is what turns a skipped step into a
//! compile error later, when the SQL pipeline
//! (`RawSql` → … → `ApprovedQuery`) arrives in M4.
//!
//! Dependencies are struct fields, so injecting the mock driver makes each use
//! case testable on its own.

use async_trait::async_trait;

use crate::error::AppResult;

pub mod connect;
pub mod expand_node;
pub mod preview_table;

pub use connect::{Connect, ConnectInput, ConnectOutput};
pub use expand_node::{ExpandNode, ExpandNodeInput, ExpandNodeOutput};
pub use preview_table::{PreviewTable, PreviewTableInput, PreviewTableOutput};

#[async_trait]
pub trait UseCase {
    type Input;
    type Output;

    async fn execute(&self, input: Self::Input) -> AppResult<Self::Output>;
}

//! The agent surface's own view of `sqlake-app`.
//!
//! A peer of `sqlake-tui`, not a layer under it: the two render the same
//! `PagedResult` for opposite readers, and reaching for the terminal's
//! formatter here would be a bug rather than reuse.
//!
//! Nothing in this crate constructs a query or knows anything about a driver.
//! If either becomes necessary, that is a report that the interactive client is
//! missing a feature, not licence to write one here.

pub mod page;
pub mod protocol;
pub mod serve;
pub mod snapshot;
pub mod socket;

pub use page::{Budget, Column, Page};
pub use protocol::{
    Failure, FailureKind, Request, RequestKind, Response, ResponseKind, SortBy, schema,
};
pub use serve::{DEFAULT_TIMEOUT, Service};
pub use snapshot::{
    ColumnDefInfo, ConnectionInfo, DefinitionInfo, EstimateInfo, NodeInfo, NodeStatus,
    PositionInfo, ProfileInfo, QueryInfo, QueryState, SectionInfo, SessionInfo, StatInfo, Status,
};
pub use socket::{
    AMBIGUOUS, Backend, Client, DEFAULT_SESSION, Listener, ListenerHandle, choose, session_name,
    socket_path,
};
// The JSON a `Value` becomes is one answer, and it lives where both
// front-ends reach it. Re-exported so a caller of this crate need not know
// which layer settled it.
pub use sqlake_app::json::to_json;

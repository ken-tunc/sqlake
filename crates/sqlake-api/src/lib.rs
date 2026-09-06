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
pub mod value;

pub use page::{Budget, Column, Page};
pub use protocol::{Failure, Request, RequestKind, Response, SortBy, schema};
pub use serve::{DEFAULT_TIMEOUT, Service};
pub use snapshot::{ConnectionInfo, NodeInfo, NodeStatus, ProfileInfo, SessionInfo, Status};
pub use value::to_json;

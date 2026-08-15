//! The seam between the application and a database.
//!
//! This is the M0 subset. `describe`, `estimate` and `execute` are added by the
//! milestone that needs them (M5 and M4 respectively). Declaring them now would
//! force stub types into existence months before anything constructs one.

use async_trait::async_trait;
use thiserror::Error;

use crate::capability::{Capabilities, DriverKind};
use crate::node::{NodeRef, TableRef, TreeNode};
use crate::profile::ResolvedProfile;
use crate::result::{PageRequest, ResultSet};

pub type DriverResult<T> = Result<T, DriverError>;

#[derive(Debug, Error)]
pub enum DriverError {
    #[error("could not connect: {0}")]
    Connect(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("query failed: {0}")]
    Query(String),

    /// The driver does not implement this operation. Reaching this is a bug in
    /// the caller: [`Capabilities`] should have prevented the call.
    #[error("not supported by this driver: {0}")]
    Unsupported(String),

    #[error("cancelled")]
    Cancelled,

    #[error(transparent)]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}

impl DriverError {
    /// Whether retrying the same call could plausibly succeed. Used to decide
    /// between offering a retry and reporting a dead end.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Connect(_) | Self::Query(_) | Self::Other(_))
    }
}

#[async_trait]
pub trait Driver: Send + Sync + std::fmt::Debug {
    fn kind(&self) -> DriverKind;

    fn capabilities(&self) -> Capabilities;

    /// A driver is per *kind*, not per connection: one `Arc<dyn Driver>`
    /// serves every PostgreSQL profile there is, and what distinguishes two
    /// live connections is the [`ResolvedProfile`] each was opened with.
    async fn connect(&self, profile: &ResolvedProfile) -> DriverResult<Box<dyn Session>>;
}

/// A live connection. Owned by exactly one session actor, which serialises
/// access, so implementations need not be internally concurrent.
#[async_trait]
pub trait Session: Send + Sync + std::fmt::Debug {
    fn capabilities(&self) -> Capabilities;

    async fn children(&self, of: &NodeRef) -> DriverResult<Vec<TreeNode>>;

    /// Drivers whose [`Capabilities::free_preview`] is true must not issue a
    /// query here — that is the whole point of the flag.
    async fn preview(&self, table: &TableRef, req: &PageRequest) -> DriverResult<ResultSet>;

    async fn close(self: Box<Self>);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both traits must stay object-safe: the application holds
    /// `Box<dyn Session>` and `Arc<dyn Driver>`.
    #[allow(dead_code)]
    fn assert_object_safe(_: &dyn Driver, _: &dyn Session) {}

    #[test]
    fn errors_read_as_sentences() {
        assert_eq!(
            DriverError::NotFound("public.users".into()).to_string(),
            "not found: public.users"
        );
        assert_eq!(DriverError::Cancelled.to_string(), "cancelled");
    }

    #[test]
    fn only_transient_failures_are_retryable() {
        assert!(DriverError::Connect("refused".into()).is_retryable());
        assert!(!DriverError::NotFound("x".into()).is_retryable());
        assert!(!DriverError::Unsupported("triggers".into()).is_retryable());
        assert!(!DriverError::Cancelled.is_retryable());
    }

    #[test]
    fn foreign_errors_wrap_without_losing_their_message() {
        let io = std::io::Error::other("disk on fire");
        let err = DriverError::Other(Box::new(io));
        assert_eq!(err.to_string(), "disk on fire");
    }
}

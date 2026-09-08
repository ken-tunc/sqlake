//! The seam between the application and a database.
//!
//! The seam is complete: connecting, walking, reading, costing, running and
//! describing are what a driver has to do, and nothing above it needs anything
//! a driver cannot answer.

use async_trait::async_trait;
use thiserror::Error;

use crate::capability::{Capabilities, DriverKind};
use crate::detail::TableDetail;
use crate::node::{NodeRef, TableRef, TreeNode};
use crate::profile::ResolvedProfile;
use crate::result::{PageRequest, ResultSet};
use crate::sql::{ApprovedQuery, Estimate, Position, ValidatedSql};

pub type DriverResult<T> = Result<T, DriverError>;

#[derive(Debug, Error)]
pub enum DriverError {
    #[error("could not connect: {0}")]
    Connect(String),

    #[error("not found: {0}")]
    NotFound(String),

    /// The server refused or failed a statement.
    ///
    /// `at` is where in the statement, when the server said — one field with
    /// an `Option` rather than a second variant, because "did this query fail"
    /// is a question a caller should not have to ask twice.
    #[error("query failed: {message}")]
    Query {
        message: String,
        at: Option<Position>,
    },

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
    /// A query failure with nowhere to point, which is most of them: only a
    /// syntax error has a position, and only the driver can work out where.
    pub fn query(message: impl Into<String>) -> Self {
        Self::Query {
            message: message.into(),
            at: None,
        }
    }

    /// Whether retrying the same call could plausibly succeed. Used to decide
    /// between offering a retry and reporting a dead end.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Connect(_) | Self::Query { .. } | Self::Other(_))
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
    ///
    /// One whose [`Capabilities::sortable_preview`] is false must answer a
    /// `req` carrying a sort with [`DriverError::Unsupported`], not with the
    /// rows in its own order: a page that ignored the sort is indistinguishable
    /// from one that honoured it, and the caller would draw an arrow over it.
    async fn preview(&self, table: &TableRef, req: &PageRequest) -> DriverResult<ResultSet>;

    /// What running `sql` is expected to cost, without running it.
    ///
    /// A driver whose [`Capabilities::cost_estimate`] is false answers
    /// [`Estimate::Unknown`] rather than failing: "I cannot say" is the honest
    /// answer to a question that was fair to ask, and the caller has already
    /// been told to expect it.
    ///
    /// [`Capabilities::cost_estimate`]: crate::capability::Capabilities::cost_estimate
    async fn estimate(&self, sql: &ValidatedSql) -> DriverResult<Estimate>;

    /// What this relation is: its columns, and whatever else this driver keeps
    /// about it.
    ///
    /// Every driver answers the same shape and fills in only what it has.
    /// [`TableDetail::sections`] empty is an ordinary answer — BigQuery has no
    /// triggers to have — and [`Capabilities`] is what says in advance which
    /// of them to expect, so a caller is not left inferring absence from
    /// silence.
    ///
    /// This is the call that knows about nullability and defaults.
    /// [`Session::preview`] builds its columns from a result and cannot always
    /// know, so a front-end drawing a definition draws it from here.
    async fn describe(&self, table: &TableRef) -> DriverResult<TableDetail>;

    /// Run a query and return its rows.
    ///
    /// Takes an [`ApprovedQuery`] and nothing else, which is what makes "no
    /// query runs without being estimated first" a fact about the type system
    /// rather than a rule to remember. Building one requires an estimate, and
    /// the only estimates come from [`Session::estimate`].
    ///
    /// [`ApprovedQuery::max_rows`] is a cap on the *fetch*, not on the
    /// statement: a driver applies it to how many rows it pulls back, and
    /// never by rewriting the text.
    async fn execute(&self, query: &ApprovedQuery) -> DriverResult<ResultSet>;

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
        assert!(DriverError::query("deadlock").is_retryable());
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

//! What the UI asks the application to do.
//!
//! An `Action` is a raw intent — "this was clicked" — not a validated command.
//! The store turns one into a use case input, which is where the raw-to-checked
//! conversion happens.
//!
//! Every field here is an input somebody has to check, and the somebody is the
//! store rather than whichever front-end built it. A front-end takes a column
//! index from a grid it drew and a `TableRef` from a node its tree loaded, so
//! it cannot construct most bad actions; one sending these over a socket has
//! neither, and a check that lives in it is a check only it has.
//!
//! Everything here either touches data or performs I/O. Scrolling, selection,
//! column widths and split positions are *not* actions: they are handled inside
//! the render loop, because routing them through an async task adds a round
//! trip to every wheel tick. Nor is which of these a screen has open — see
//! the crate doc.

use std::fmt;

use sqlake_core::id::{ConnId, ProfileId, QueryId};
use sqlake_core::library::{NewTemplate, TemplateId};
use sqlake_core::node::{NodeRef, TableRef};

/// Identifies one long-running operation, so it can be shown and cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BusyId(u64);

impl BusyId {
    #[must_use]
    pub const fn new(n: u64) -> Self {
        Self(n)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Open a connection to a configured profile, under an id the caller
    /// chooses.
    ///
    /// Two connections may name the same profile: that is a second window onto
    /// the same database, not a mistake to deduplicate. So the profile does not
    /// identify the connection, and a caller that cannot see the result — one
    /// waiting on a snapshot for the connection it just opened — has nothing
    /// else to wait on. Reading the snapshot for "the one that was not there
    /// before" races every other caller on a shared session.
    Connect {
        profile: ProfileId,
        conn: ConnId,
    },
    Disconnect(ConnId),

    /// Expand or collapse a tree node, fetching its children if needed.
    ToggleNode {
        conn: ConnId,
        node: NodeRef,
    },

    /// Expand a tree node, leaving an already-expanded one alone.
    ///
    /// A toggle is a statement about a state the caller can see. A caller that
    /// cannot — one sending actions over a socket — would have to read the
    /// snapshot first and decide, and in a session a person is also clicking in
    /// the state can change between the read and the dispatch. So the
    /// idempotent form is its own action rather than a flag on `ToggleNode`.
    ExpandNode {
        conn: ConnId,
        node: NodeRef,
    },

    /// Fetch a relation, reusing what is already cached for it.
    PreviewTable {
        conn: ConnId,
        table: TableRef,
    },

    /// Fetch what a relation is, reusing what is cached for it.
    ///
    /// `refresh` asks again for something already answered. A definition is
    /// fetched once and never re-fetched by scrolling the way a preview is, so
    /// without this the only way out of a stale one is to close the
    /// connection.
    DescribeTable {
        conn: ConnId,
        table: TableRef,
        refresh: bool,
    },

    /// A front-end is no longer showing this definition anywhere.
    ForgetDefinition {
        conn: ConnId,
        table: TableRef,
    },

    /// Sort a preview by a column.
    ///
    /// The direction is not carried: the store holds the current sort and
    /// toggles it. Sending a direction computed by the view would race with a
    /// sort that is already in flight.
    SortPreview {
        conn: ConnId,
        table: TableRef,
        column: usize,
    },

    /// Fetch the next page into an existing preview.
    LoadMore {
        conn: ConnId,
        table: TableRef,
    },

    /// A front-end no longer has this preview open anywhere, so its cache
    /// entry and any page still in flight for it can go.
    ForgetPreview {
        conn: ConnId,
        table: TableRef,
    },

    /// Run a statement, under an id the caller chose.
    ///
    /// The id for the same reason `Connect` carries one: two runs of the same
    /// SQL are two different things with two different results, so there is no
    /// name to wait on afterwards.
    ///
    /// No budget: that is the user's own ceiling, and a front-end that could
    /// name its own would be one that could raise it.
    RunQuery {
        conn: ConnId,
        query: QueryId,
        sql: String,
        /// Rows to fetch back, or all of them.
        max_rows: Option<u32>,
        /// A ceiling of the caller's own, applied on top of the session's.
        ///
        /// Only ever downward, the way a page limit is: the budget exists to
        /// stop something spending more than its owner meant to, and a request
        /// that could raise it would be that protection asking permission from
        /// the party it protects against. An agent with a tighter budget than
        /// the person sharing the session is the case this is for.
        max_bytes: Option<u64>,
    },

    /// Cost a statement without running it.
    ///
    /// Its own action rather than a flag on `RunQuery`: what a caller wants
    /// here is the number *and no query*, and reaching it through the running
    /// path would mean asking for something and hoping the budget said no.
    EstimateQuery {
        conn: ConnId,
        query: QueryId,
        sql: String,
    },

    /// Run a query that stopped over the budget, because somebody said yes.
    ///
    /// Names only the query: what runs is the statement the refusal kept, not
    /// one rebuilt from a buffer that may have moved on since.
    ApproveQuery(QueryId),

    /// A front-end is no longer showing this query's result anywhere.
    ForgetQuery(QueryId),

    /// Cancel a running operation.
    Cancel(BusyId),

    /// Read the saved templates.
    ///
    /// Asked for rather than loaded at startup: a client that never opens the
    /// palette never needs them, and reading a file on the way to the first
    /// frame is a file read in front of somebody waiting for a screen.
    LoadTemplates,

    /// Save a statement under a name.
    SaveTemplate(NewTemplate),

    /// Overwrite one, keeping when it was written.
    ReplaceTemplate {
        id: TemplateId,
        with: NewTemplate,
    },

    DeleteTemplate(TemplateId),

    Quit,
}

impl fmt::Display for Action {
    /// Short forms for the log. Deliberately not user-facing text.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect { profile, conn } => {
                write!(f, "connect({profile} as {})", conn.short())
            }
            Self::Disconnect(id) => write!(f, "disconnect({})", id.short()),
            Self::ToggleNode { node, .. } => write!(f, "toggle({node})"),
            Self::ExpandNode { node, .. } => write!(f, "expand({node})"),
            Self::PreviewTable { table, .. } => write!(f, "preview({table})"),
            Self::DescribeTable { table, refresh, .. } => {
                write!(
                    f,
                    "describe({table}{})",
                    if *refresh { ", again" } else { "" }
                )
            }
            Self::ForgetDefinition { table, .. } => write!(f, "forget_definition({table})"),
            Self::SortPreview { table, column, .. } => write!(f, "sort({table}, col {column})"),
            Self::LoadMore { table, .. } => write!(f, "load_more({table})"),
            Self::ForgetPreview { table, .. } => write!(f, "forget_preview({table})"),
            Self::RunQuery { query, .. } => write!(f, "run({})", query.short()),
            Self::EstimateQuery { query, .. } => write!(f, "estimate({})", query.short()),
            Self::ApproveQuery(id) => write!(f, "approve({})", id.short()),
            Self::ForgetQuery(id) => write!(f, "forget_query({})", id.short()),
            Self::Cancel(id) => write!(f, "cancel({})", id.get()),
            Self::LoadTemplates => f.write_str("load_templates"),
            Self::SaveTemplate(template) => write!(f, "save_template({})", template.name),
            Self::ReplaceTemplate { id, .. } => write!(f, "replace_template({id})"),
            Self::DeleteTemplate(id) => write!(f, "delete_template({id})"),
            Self::Quit => f.write_str("quit"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_log_readably() {
        let a = Action::PreviewTable {
            conn: ConnId::new(),
            table: TableRef::new(["public", "users"]),
        };
        assert_eq!(a.to_string(), "preview(public.users)");
        assert_eq!(Action::Quit.to_string(), "quit");
    }

    #[test]
    fn sorting_carries_no_direction() {
        // If a direction were carried here, two fast clicks would race against
        // the sort already in flight. The store owns the current direction.
        let a = Action::SortPreview {
            conn: ConnId::new(),
            table: TableRef::new(["public", "users"]),
            column: 2,
        };
        assert_eq!(a.to_string(), "sort(public.users, col 2)");
    }
}

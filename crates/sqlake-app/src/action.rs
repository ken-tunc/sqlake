//! What the UI asks the application to do.
//!
//! An `Action` is a raw intent — "this was clicked" — not a validated command.
//! The store turns one into a use case input, which is where the raw-to-checked
//! conversion happens.
//!
//! Everything here either touches data or performs I/O. Scrolling, selection,
//! column widths and split positions are *not* actions: they are handled inside
//! the render loop, because routing them through an async task adds a round
//! trip to every wheel tick.

use std::fmt;

use sqlake_core::id::{ConnId, ProfileId, TabId};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ToastId(u64);

impl ToastId {
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
    /// Open a connection to a configured profile.
    ///
    /// Two connections may name the same profile: that is a second window onto
    /// the same database, not a mistake to deduplicate.
    Connect(ProfileId),
    Disconnect(ConnId),

    /// Expand or collapse a tree node, fetching its children if needed.
    ToggleNode {
        conn: ConnId,
        node: NodeRef,
    },

    /// Open a preview tab for a relation, reusing an existing tab if one is
    /// already showing it.
    PreviewTable {
        conn: ConnId,
        table: TableRef,
    },

    /// Sort a preview by a column.
    ///
    /// The direction is not carried: the store holds the current sort and
    /// toggles it. Sending a direction computed by the view would race with a
    /// sort that is already in flight.
    SortPreview {
        tab: TabId,
        column: usize,
    },

    /// Fetch the next page into an existing preview.
    LoadMore {
        tab: TabId,
    },

    SelectTab(TabId),
    CloseTab(TabId),

    /// Cancel a running operation.
    Cancel(BusyId),

    DismissToast(ToastId),

    Quit,
}

impl fmt::Display for Action {
    /// Short forms for the log. Deliberately not user-facing text.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(profile) => write!(f, "connect({profile})"),
            Self::Disconnect(id) => write!(f, "disconnect({})", id.short()),
            Self::ToggleNode { node, .. } => write!(f, "toggle({node})"),
            Self::PreviewTable { table, .. } => write!(f, "preview({table})"),
            Self::SortPreview { tab, column } => write!(f, "sort({tab}, col {column})"),
            Self::LoadMore { tab } => write!(f, "load_more({tab})"),
            Self::SelectTab(tab) => write!(f, "select_tab({tab})"),
            Self::CloseTab(tab) => write!(f, "close_tab({tab})"),
            Self::Cancel(id) => write!(f, "cancel({})", id.get()),
            Self::DismissToast(id) => write!(f, "dismiss_toast({})", id.get()),
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
            tab: TabId::new(1),
            column: 2,
        };
        assert_eq!(a.to_string(), "sort(#1, col 2)");
    }
}

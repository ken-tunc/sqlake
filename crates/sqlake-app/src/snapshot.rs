//! The immutable view of application state that the UI renders.
//!
//! Published on a watch channel and cloned freely, so every heavy field sits
//! behind an `Arc`. Nothing here describes appearance: scroll offsets, column
//! widths, selection and focus belong to `UiState` in the TUI crate.

use std::sync::Arc;
use std::time::Instant;

use sqlake_core::capability::{Capabilities, DriverKind};
use sqlake_core::id::{ConnId, ProfileId};
use sqlake_core::node::{NodeRef, TableRef};
use sqlake_core::profile::{ProfileColor, ProfileSummary};
use sqlake_core::result::Sort;

use crate::action::BusyId;
use crate::pages::PagedResult;
use crate::tree::{TreeView, VisibleNode};

/// Something that is fetched asynchronously.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadState<T> {
    /// Not requested yet.
    Idle,
    Loading,
    Ready(T),
    Failed(String),
}

impl<T> LoadState<T> {
    #[must_use]
    pub const fn is_loading(&self) -> bool {
        matches!(self, Self::Loading)
    }

    #[must_use]
    pub const fn ready(&self) -> Option<&T> {
        match self {
            Self::Ready(v) => Some(v),
            _ => None,
        }
    }

    #[must_use]
    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Failed(e) => Some(e),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnStatus {
    Connecting,
    Ready,
    Failed(String),
    Closed,
}

#[derive(Debug, Clone)]
pub struct ConnectionView {
    pub id: ConnId,
    /// The profile this connection came from.
    pub profile: ProfileId,
    pub name: String,
    /// The profile's colour, so a production connection does not look like a
    /// scratch one.
    pub color: Option<ProfileColor>,
    pub kind: DriverKind,
    pub status: ConnStatus,
    /// Known once the connection is open. Until then the UI has nothing to
    /// branch on, which is correct: there is nothing to show yet.
    pub capabilities: Option<Capabilities>,
}

impl ConnectionView {
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.status == ConnStatus::Ready
    }
}

/// A relation's data, as far as it has been fetched.
#[derive(Debug, Clone)]
pub struct PreviewView {
    pub conn: ConnId,
    pub table: TableRef,
    pub sort: Option<Sort>,
    /// Rows fetched so far. Paging appends, so this only grows.
    pub loaded_rows: usize,
    pub data: LoadState<Arc<PagedResult>>,
    /// A page that failed to extend `data`, without disturbing it.
    ///
    /// Set instead of turning `data` into `Failed`: the rows already fetched
    /// are still good, and replacing them with an error would lose them *and*
    /// leave the next request starting from the wrong offset. Cleared by the
    /// next request that reaches this preview.
    pub last_error: Option<String>,
}

/// What a busy item is waiting for.
///
/// Cancelling abandons a reply that will now never arrive, so something has to
/// know what that reply was going to be applied to. Without it the owner sits
/// in `Loading` for ever — and a tree node in `Loading` refuses to toggle, so
/// the node becomes permanently dead rather than merely stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusyOwner {
    Connection(ConnId),
    Node { conn: ConnId, node: NodeRef },
    Preview { conn: ConnId, table: TableRef },
}

/// A running, cancellable operation.
#[derive(Debug, Clone)]
pub struct BusyItem {
    pub id: BusyId,
    pub owner: BusyOwner,
    pub label: String,
    pub started_at: Instant,
}

impl BusyItem {
    #[must_use]
    pub fn elapsed_ms(&self) -> u128 {
        self.started_at.elapsed().as_millis()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Increments on every publication. Useful in logs and tests; the UI
    /// redraws on channel notification, not on this.
    pub rev: u64,
    /// Every configured profile, connected or not. Read once at startup, so
    /// this is the same `Arc` in every snapshot until a reload exists.
    pub profiles: Arc<Vec<ProfileSummary>>,
    pub connections: Vec<ConnectionView>,
    /// Every connection and its tree, in one flat list: a connection is a
    /// row like any other, and its objects are rows underneath it. Drawing is
    /// still a slice and an index — there is simply more than one root now.
    pub explorer: Arc<TreeView>,
    pub previews: Vec<PreviewView>,
    pub busy: Vec<BusyItem>,
    pub should_quit: bool,
}

impl Snapshot {
    #[must_use]
    pub fn connection(&self, id: ConnId) -> Option<&ConnectionView> {
        self.connections.iter().find(|c| c.id == id)
    }

    /// The rows belonging to one connection, without its own row.
    pub fn tree(&self, id: ConnId) -> impl Iterator<Item = &VisibleNode> {
        self.explorer
            .nodes
            .iter()
            .filter(move |node| node.conn == id && !node.node_ref.path.is_empty())
    }

    #[must_use]
    pub fn preview(&self, conn: ConnId, table: &TableRef) -> Option<&PreviewView> {
        self.previews
            .iter()
            .find(|p| p.conn == conn && &p.table == table)
    }

    #[must_use]
    pub fn is_busy(&self) -> bool {
        !self.busy.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(status: ConnStatus) -> ConnectionView {
        ConnectionView {
            id: ConnId::new(),
            profile: ProfileId::parse("mock").expect("a usable id"),
            name: "mock".into(),
            color: None,
            kind: DriverKind::Mock,
            status,
            capabilities: None,
        }
    }

    #[test]
    fn a_fresh_snapshot_shows_nothing_and_does_not_quit() {
        let s = Snapshot::default();
        assert!(s.connections.is_empty());
        assert!(s.previews.is_empty());
        assert!(!s.is_busy());
        assert!(!s.should_quit);
    }

    #[test]
    fn lookups_miss_cleanly() {
        let s = Snapshot::default();
        assert!(s.connection(ConnId::new()).is_none());
        assert!(
            s.preview(ConnId::new(), &TableRef::new(["public", "users"]))
                .is_none()
        );
        assert_eq!(s.tree(ConnId::new()).count(), 0);
    }

    #[test]
    fn only_an_open_connection_is_ready() {
        assert!(conn(ConnStatus::Ready).is_ready());
        assert!(!conn(ConnStatus::Connecting).is_ready());
        assert!(!conn(ConnStatus::Failed("nope".into())).is_ready());
        assert!(!conn(ConnStatus::Closed).is_ready());
    }

    #[test]
    fn load_state_distinguishes_never_asked_from_failed() {
        let idle: LoadState<u8> = LoadState::Idle;
        assert!(idle.ready().is_none());
        assert!(idle.error().is_none());
        assert!(!idle.is_loading());

        let failed: LoadState<u8> = LoadState::Failed("boom".into());
        assert_eq!(failed.error(), Some("boom"));
        assert!(failed.ready().is_none());

        assert_eq!(LoadState::Ready(7).ready(), Some(&7));
    }

    #[test]
    fn a_preview_resolves_by_connection_and_table() {
        let conn_id = ConnId::new();
        let table = TableRef::new(["public", "users"]);
        let s = Snapshot {
            previews: vec![PreviewView {
                conn: conn_id,
                table: table.clone(),
                sort: None,
                loaded_rows: 0,
                data: LoadState::Loading,
                last_error: None,
            }],
            ..Snapshot::default()
        };
        assert!(s.preview(conn_id, &table).unwrap().data.is_loading());
        // A different connection asking for the same table name is not this
        // preview: the two have nothing to do with each other.
        assert!(s.preview(ConnId::new(), &table).is_none());
    }
}

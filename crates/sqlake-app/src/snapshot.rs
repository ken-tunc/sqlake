//! The immutable view of application state that the UI renders.
//!
//! Published on a watch channel and cloned freely, so every heavy field sits
//! behind an `Arc`. Nothing here describes appearance: scroll offsets, column
//! widths, selection and focus belong to `UiState` in the TUI crate.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use sqlake_core::capability::{Capabilities, DriverKind};
use sqlake_core::id::{ConnId, ProfileId, TabId};
use sqlake_core::node::{NodeRef, TableRef};
use sqlake_core::profile::ProfileSummary;
use sqlake_core::result::Sort;

use crate::action::{BusyId, ToastId};
use crate::pages::PagedResult;
use crate::tree::TreeView;

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

#[derive(Debug, Clone)]
pub struct PreviewTab {
    pub table: TableRef,
    pub sort: Option<Sort>,
    /// Rows fetched so far. Paging appends, so this only grows.
    pub loaded_rows: usize,
    pub data: LoadState<Arc<PagedResult>>,
}

#[derive(Debug, Clone)]
pub enum TabContent {
    Preview(PreviewTab),
}

#[derive(Debug, Clone)]
pub struct TabView {
    pub id: TabId,
    pub conn: ConnId,
    pub title: String,
    pub content: TabContent,
}

impl TabView {
    #[must_use]
    pub fn preview(&self) -> Option<&PreviewTab> {
        let TabContent::Preview(p) = &self.content;
        Some(p)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub id: ToastId,
    pub text: String,
    pub severity: Severity,
    pub created_at: Instant,
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
    Tab(TabId),
}

/// A running operation, shown in the status bar with a way to stop it.
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
    pub trees: HashMap<ConnId, Arc<TreeView>>,
    pub tabs: Vec<TabView>,
    pub active_tab: Option<TabId>,
    pub busy: Vec<BusyItem>,
    pub toasts: Vec<Toast>,
    pub should_quit: bool,
}

impl Snapshot {
    #[must_use]
    pub fn connection(&self, id: ConnId) -> Option<&ConnectionView> {
        self.connections.iter().find(|c| c.id == id)
    }

    #[must_use]
    pub fn tree(&self, id: ConnId) -> Option<&TreeView> {
        self.trees.get(&id).map(Arc::as_ref)
    }

    #[must_use]
    pub fn tab(&self, id: TabId) -> Option<&TabView> {
        self.tabs.iter().find(|t| t.id == id)
    }

    #[must_use]
    pub fn active(&self) -> Option<&TabView> {
        self.active_tab.and_then(|id| self.tab(id))
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
            kind: DriverKind::Mock,
            status,
            capabilities: None,
        }
    }

    #[test]
    fn a_fresh_snapshot_shows_nothing_and_does_not_quit() {
        let s = Snapshot::default();
        assert!(s.connections.is_empty());
        assert!(s.active().is_none());
        assert!(!s.is_busy());
        assert!(!s.should_quit);
    }

    #[test]
    fn lookups_miss_cleanly() {
        let s = Snapshot::default();
        assert!(s.connection(ConnId::new()).is_none());
        assert!(s.tab(TabId::new(1)).is_none());
        assert!(s.tree(ConnId::new()).is_none());
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
    fn the_active_tab_resolves_through_the_tab_list() {
        let id = TabId::new(3);
        let s = Snapshot {
            tabs: vec![TabView {
                id,
                conn: ConnId::new(),
                title: "users".into(),
                content: TabContent::Preview(PreviewTab {
                    table: TableRef::new(["public", "users"]),
                    sort: None,
                    loaded_rows: 0,
                    data: LoadState::Loading,
                }),
            }],
            active_tab: Some(id),
            ..Snapshot::default()
        };
        assert_eq!(s.active().map(|t| t.title.as_str()), Some("users"));
        assert!(s.active().unwrap().preview().unwrap().data.is_loading());
    }

    #[test]
    fn a_dangling_active_tab_id_does_not_panic() {
        let s = Snapshot {
            active_tab: Some(TabId::new(9)),
            ..Snapshot::default()
        };
        assert!(s.active().is_none());
    }
}

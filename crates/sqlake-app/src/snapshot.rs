//! The immutable view of application state that the UI renders.
//!
//! Published on a watch channel and cloned freely, so every heavy field sits
//! behind an `Arc`. Nothing here describes appearance: scroll offsets, column
//! widths, selection and focus belong to `UiState` in the TUI crate.

use std::sync::Arc;
use std::time::Instant;

use sqlake_core::capability::{Capabilities, DriverKind};
use sqlake_core::detail::{Ddl, TableDetail};
use sqlake_core::id::{ConnId, ProfileId, QueryId};
use sqlake_core::node::{NodeRef, RelationKind, TableRef};
use sqlake_core::profile::{ProfileColor, ProfileSummary};
use sqlake_core::result::{Column, ResultSet, Row, Sort};
use sqlake_core::sql::{Estimate, OverBudget, Position};
use sqlake_core::value::Value;

use crate::action::BusyId;
use crate::pages::PagedResult;
use crate::store::Dispatched;
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
    /// Every object fetched for this connection, whether or not its row is
    /// currently open.
    ///
    /// Separate from [`Snapshot::explorer`] because expansion is one
    /// front-end's navigation state, and a caller with no screen has not
    /// collapsed anything. Reading the explorer instead would let a human
    /// closing a row turn an agent's answer into "this database is empty".
    pub tree: Arc<TreeView>,
}

impl ConnectionView {
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.status == ConnStatus::Ready
    }

    /// Whether this connection is one work can still be given to.
    ///
    /// Wider than [`Self::is_ready`]: a connection still opening will take the
    /// work by the time it is sent. Narrower than "there is a row here": a
    /// closed or failed connection keeps its row so the failure can be read,
    /// and counting it is how an affordance ends up offering something that
    /// cannot happen.
    #[must_use]
    pub fn is_live(&self) -> bool {
        matches!(self.status, ConnStatus::Connecting | ConnStatus::Ready)
    }

    /// Whether a preview of this connection can be ordered by a column.
    ///
    /// False while the capabilities are unknown, so the answer is one a caller
    /// can act on before the connection is open: nothing can be sorted yet
    /// either way, and an `Option` would only push the same decision outwards
    /// to be made differently in each front-end.
    #[must_use]
    pub fn can_sort_preview(&self) -> bool {
        self.capabilities.is_some_and(|c| c.sortable_preview)
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
    /// The relation has no more rows: the last page came back short.
    ///
    /// The store's to know, because it issued the request and knows the limit.
    /// A front-end inferring it from "the last request changed nothing" gets
    /// it wrong twice over — a cancelled page and a retry that failed the same
    /// way both change nothing either.
    pub exhausted: bool,
    /// Page requests that have finished, however they finished.
    ///
    /// The only way to tell "a request came back" from "no request was made",
    /// which the state a request leaves behind cannot say.
    pub attempts: u64,
    pub data: LoadState<Arc<PagedResult>>,
    /// A page that failed to extend `data`, without disturbing it.
    ///
    /// Set instead of turning `data` into `Failed`: the rows already fetched
    /// are still good, and replacing them with an error would lose them *and*
    /// leave the next request starting from the wrong offset. Cleared by the
    /// next request that reaches this preview.
    pub last_error: Option<String>,
}

/// One run of a statement, and whatever has come of it.
///
/// Keyed by a [`QueryId`] the caller chose rather than by its text: two runs of
/// the same SQL are two different things with two different answers, so there
/// is no name to look one up by.
#[derive(Debug, Clone)]
pub struct QueryView {
    pub id: QueryId,
    pub conn: ConnId,
    /// What was sent, or what would be. Kept so a result can be read next to
    /// the statement that produced it after the buffer has moved on.
    pub sql: String,
    /// What the server said it would cost, once it has been asked.
    pub estimate: Option<Estimate>,
    /// Waiting for somebody to say yes, and everything needed to ask.
    ///
    /// A field rather than a `LoadState` variant: the rows are genuinely not
    /// loading, and a front-end drawing a spinner over a question nobody has
    /// answered would be waiting for itself.
    pub needs_approval: Option<Arc<OverBudget>>,
    pub data: LoadState<Arc<PagedResult>>,
    /// Where the failure was, when the server said.
    ///
    /// Beside `data` rather than inside `LoadState::Failed`: every other thing
    /// that fails has nowhere to point, and widening the shared type for one
    /// of them would put an `Option` nobody reads on all of them.
    pub failed_at: Option<Position>,
}

impl QueryView {
    /// Whether this run is finished, however it finished — which is what a
    /// caller with no screen waits on.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        !self.data.is_loading()
    }
}

/// One section of a definition, with its rows in the shape a grid takes.
///
/// `PagedResult` rather than the `ResultSet` the driver answered with: the
/// grid has one input type and a definition is not a reason to give it a
/// second. Wrapped once here rather than per frame, since a section never
/// grows — there is no page two of an index list.
#[derive(Debug, Clone)]
pub struct SectionView {
    pub title: String,
    pub rows: Arc<PagedResult>,
}

/// What a relation is, as far as it has been fetched.
///
/// Cached by `(conn, table)` so that opening the tab twice does not ask the
/// server twice — and so an agent asking through `sqlake-api` reads what a
/// person already has open rather than fetching a second copy.
#[derive(Debug, Clone)]
pub struct DefinitionView {
    pub conn: ConnId,
    pub table: TableRef,
    pub data: LoadState<Arc<Definition>>,
}

/// The definition itself, in front-end shape.
#[derive(Debug, Clone)]
pub struct Definition {
    pub kind: RelationKind,
    pub comment: Option<String>,
    pub columns: Arc<PagedResult>,
    pub sections: Vec<SectionView>,
    pub ddl: Option<Ddl>,
    pub stats: Vec<(String, String)>,
}

impl Definition {
    /// The columns as a grid, and every other section beside them.
    ///
    /// Columns are a section like the rest once they are here, which is what
    /// lets a pane draw one list and one grid rather than a special case in
    /// front of a loop.
    #[must_use]
    pub fn of(detail: &TableDetail) -> Self {
        let columns = ResultSet::new(
            vec![
                Column::new("column", "text", false),
                Column::new("type", "text", false),
                Column::new("null", "text", false),
                Column::new("default", "text", true),
                Column::new("comment", "text", true),
            ],
            detail
                .columns
                .iter()
                .map(|column| {
                    Row(vec![
                        Value::Text(column.name.clone()),
                        Value::Text(column.type_name.clone()),
                        // A word rather than a boolean: `false` under a column
                        // headed `null` is two negatives to hold at once.
                        Value::Text(if column.nullable { "" } else { "not null" }.to_owned()),
                        column.default.clone().map_or(Value::Null, Value::Text),
                        column.comment.clone().map_or(Value::Null, Value::Text),
                    ])
                })
                .collect(),
            Some(detail.columns.len() as u64),
        );
        Self {
            kind: detail.kind,
            comment: detail.comment.clone(),
            columns: Arc::new(PagedResult::new(&columns)),
            sections: detail
                .sections
                .iter()
                .map(|section| SectionView {
                    title: section.title.clone(),
                    rows: Arc::new(PagedResult::new(&section.table)),
                })
                .collect(),
            ddl: detail.ddl.clone(),
            stats: detail.stats.clone(),
        }
    }

    /// Every section a pane can show, columns first.
    ///
    /// Columns lead because they are what somebody opened the pane for; the
    /// rest are in the order the driver gave them.
    #[must_use]
    pub fn titles(&self) -> Vec<&str> {
        std::iter::once("Columns")
            .chain(self.sections.iter().map(|s| s.title.as_str()))
            .collect()
    }

    /// The rows under the section at `index` in [`Definition::titles`].
    #[must_use]
    pub fn rows(&self, index: usize) -> Option<&Arc<PagedResult>> {
        match index.checked_sub(1) {
            None => Some(&self.columns),
            Some(at) => self.sections.get(at).map(|s| &s.rows),
        }
    }
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
    Definition { conn: ConnId, table: TableRef },
    Query(QueryId),
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
    /// How many actions the store has applied. See [`Snapshot::has_applied`].
    pub applied: u64,
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
    pub definitions: Vec<DefinitionView>,
    /// Newest last, the order they were started in.
    pub queries: Vec<QueryView>,
    pub busy: Vec<BusyItem>,
    pub should_quit: bool,
}

impl Snapshot {
    #[must_use]
    pub fn definition(&self, conn: ConnId, table: &TableRef) -> Option<&DefinitionView> {
        self.definitions
            .iter()
            .find(|d| d.conn == conn && &d.table == table)
    }

    #[must_use]
    pub fn query(&self, id: QueryId) -> Option<&QueryView> {
        self.queries.iter().find(|q| q.id == id)
    }

    #[must_use]
    pub fn connection(&self, id: ConnId) -> Option<&ConnectionView> {
        self.connections.iter().find(|c| c.id == id)
    }

    /// The rows belonging to one connection, without its own row.
    ///
    /// This is what the explorer draws, so a collapsed connection has none.
    /// For the objects themselves, use [`Snapshot::objects`].
    pub fn tree(&self, id: ConnId) -> impl Iterator<Item = &VisibleNode> {
        self.explorer
            .nodes
            .iter()
            .filter(move |node| node.conn == id && !node.node_ref.path.is_empty())
    }

    /// Every object loaded under one connection, regardless of what is open.
    pub fn objects(&self, id: ConnId) -> impl Iterator<Item = &VisibleNode> {
        self.connection(id)
            .into_iter()
            .flat_map(|conn| conn.tree.nodes.iter())
    }

    /// Whether the store has applied an action dispatched at this point in the
    /// queue.
    #[must_use]
    pub fn has_applied(&self, dispatched: Dispatched) -> bool {
        self.applied >= dispatched.ordinal()
    }

    /// Whether the store has finished opening this connection, successfully or
    /// not.
    ///
    /// Every `*_settled` predicate reads the same thing: whether the store has
    /// work in flight for the target. The state it left behind is deliberately
    /// not consulted, because every state is reachable both before a request
    /// and after one — a preview reads `Ready` while a page is being appended
    /// to it, and `Failed` both before a retry and after one that failed again.
    ///
    /// Which is why these mean nothing on their own: before the store has
    /// applied an action, nothing is in flight for it and everything looks
    /// settled. Pair them with [`Snapshot::has_applied`], which is what
    /// `Store::dispatch_and_settle` does.
    ///
    /// Settled is not succeeded. A caller reads the status it waited for.
    #[must_use]
    pub fn connection_settled(&self, id: ConnId) -> bool {
        !self
            .busy
            .iter()
            .any(|b| matches!(b.owner, BusyOwner::Connection(c) if c == id))
    }

    /// Whether the store has finished loading this node's children.
    #[must_use]
    pub fn node_settled(&self, conn: ConnId, node: &NodeRef) -> bool {
        !self.busy.iter().any(
            |b| matches!(&b.owner, BusyOwner::Node { conn: c, node: n } if *c == conn && n == node),
        )
    }

    /// Whether the store has finished fetching for this preview.
    #[must_use]
    pub fn preview_settled(&self, conn: ConnId, table: &TableRef) -> bool {
        !self.busy.iter().any(
            |b| matches!(&b.owner, BusyOwner::Preview { conn: c, table: t } if *c == conn && t == table),
        )
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
            tree: Arc::default(),
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
    fn a_connection_still_opening_is_live_and_a_finished_one_is_not() {
        assert!(conn(ConnStatus::Ready).is_live());
        assert!(conn(ConnStatus::Connecting).is_live());
        assert!(!conn(ConnStatus::Failed("nope".into())).is_live());
        assert!(!conn(ConnStatus::Closed).is_live());
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
                exhausted: false,
                attempts: 0,
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

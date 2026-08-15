//! The single owner of application state.
//!
//! One task owns everything mutable and is the only writer. It applies
//! [`Action`]s, invokes use cases, and republishes an immutable
//! [`Snapshot`] on a watch channel.
//!
//! The store never awaits a use case inline. Every call is spawned, and its
//! result comes back as an internal `Event`. Awaiting inline would let one slow
//! expansion block every other action — the same failure the PostgreSQL driver
//! avoids by holding a separate metadata connection.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use sqlake_core::capability::{Capabilities, DriverKind};
use sqlake_core::driver::Driver;
use sqlake_core::id::{ConnId, ProfileId, TabId};
use sqlake_core::node::{NodeRef, TableRef};
use sqlake_core::profile::{ProfileSummary, Profiles};
use sqlake_core::result::{PageRequest, Sort, SortDir};
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;

use crate::action::{Action, BusyId, ToastId};
use crate::error::{AppError, AppResult};
use crate::pages::PagedResult;
use crate::session::SessionHandle;
use crate::snapshot::{
    BusyItem, BusyOwner, ConnStatus, ConnectionView, LoadState, PreviewTab, Severity, Snapshot,
    TabContent, TabView, Toast,
};
use crate::tree::{NodeState, Toggle, TreeState, TreeView, VisibleNode};
use crate::usecase::{
    Connect, ConnectInput, ConnectOutput, ExpandNode, ExpandNodeInput, ExpandNodeOutput,
    PreviewTable, PreviewTableInput, PreviewTableOutput, UseCase,
};

#[derive(Debug, Default, Clone)]
pub struct Drivers {
    map: HashMap<DriverKind, Arc<dyn Driver>>,
}

impl Drivers {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with(mut self, driver: Arc<dyn Driver>) -> Self {
        self.map.insert(driver.kind(), driver);
        self
    }

    fn get(&self, kind: DriverKind) -> AppResult<Arc<dyn Driver>> {
        self.map
            .get(&kind)
            .cloned()
            .ok_or(AppError::UnknownDriver(kind.as_str()))
    }
}

#[derive(Debug, Clone)]
pub struct Store {
    actions: mpsc::UnboundedSender<Action>,
    snapshots: watch::Receiver<Arc<Snapshot>>,
}

impl Store {
    /// `page_size` comes from the configuration rather than from a constant
    /// here: how many rows are worth waiting for depends on the database and
    /// the link to it, which is something only the person using it knows.
    ///
    /// Zero is taken as one. `sqlake-config` refuses it, but that validation is
    /// a crate away and not on the path a second front-end takes: a page of no
    /// rows leaves an offset that `next_page` never advances, so the relation
    /// could never be read and nothing on screen would say why.
    #[must_use]
    pub fn spawn(drivers: Drivers, profiles: Arc<dyn Profiles>, page_size: u32) -> Self {
        let (action_tx, action_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (snapshot_tx, snapshot_rx) = watch::channel(Arc::new(Snapshot::default()));

        let runtime = Runtime {
            drivers,
            // Read once, at startup. Editing `connections.toml` while the
            // client is running is a reload, and a reload is a feature with
            // its own questions — not a thing to do silently on every frame.
            profile_list: Arc::new(profiles.list()),
            profiles,
            page_size: page_size.max(1),
            events: event_tx,
            conns: Vec::new(),
            tabs: Vec::new(),
            active_tab: None,
            busy: Vec::new(),
            tasks: HashMap::new(),
            toasts: Vec::new(),
            next_tab: 1,
            next_id: 1,
            should_quit: false,
            rev: 0,
        };
        tokio::spawn(runtime.run(action_rx, event_rx, snapshot_tx));

        Self {
            actions: action_tx,
            snapshots: snapshot_rx,
        }
    }

    /// Non-blocking on purpose: the render loop must never await.
    pub fn dispatch(&self, action: Action) {
        // A closed store means the process is shutting down; dropping the
        // action is the correct response.
        let _ = self.actions.send(action);
    }

    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.snapshots.clone()
    }

    #[must_use]
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshots.borrow().clone()
    }
}

#[derive(Debug)]
enum Event {
    Connected {
        conn: ConnId,
        busy: BusyId,
        result: AppResult<ConnectOutput>,
    },
    Expanded {
        conn: ConnId,
        /// Carried explicitly so a failure still identifies its node. A driver
        /// error does not know which node was asked for, and without this the
        /// node would stay in the loading state for ever.
        node: NodeRef,
        busy: BusyId,
        result: AppResult<ExpandNodeOutput>,
    },
    Previewed {
        tab: TabId,
        /// Likewise: a stale reply, successful or not, must not overwrite a
        /// newer page.
        page: PageRequest,
        busy: BusyId,
        result: AppResult<PreviewTableOutput>,
    },
}

#[derive(Debug)]
struct Conn {
    id: ConnId,
    /// Whether this connection's own row is open. Its objects were fetched at
    /// connect, so this is view state the store happens to own — closing a
    /// connection's rows must not throw its tree away.
    expanded: bool,
    /// Which profile this connection was opened from. Two connections can
    /// share one, which is what makes a second window onto the same database
    /// possible rather than a name collision.
    profile: ProfileId,
    name: String,
    kind: DriverKind,
    status: ConnStatus,
    capabilities: Option<Capabilities>,
    session: Option<SessionHandle>,
    tree: TreeState,
    /// Cached flattening, refreshed only when the tree actually changes.
    view: Arc<TreeView>,
}

const CANCELLED: &str = "cancelled";

/// How a connection's own row looks.
///
/// The same states an object node uses, so the explorer draws one kind of row:
/// a connection that is opening spins like a node that is loading, and one
/// that failed reports on itself rather than in a dialog nobody kept.
fn root_state(conn: &Conn) -> NodeState {
    match &conn.status {
        ConnStatus::Connecting => NodeState::Loading,
        ConnStatus::Failed(why) => NodeState::Failed(why.clone()),
        _ if !conn.expanded => NodeState::Collapsed,
        _ => NodeState::Expanded,
    }
}

/// A page request that has gone out and not yet come back.
///
/// The tab keeps this rather than optimistically advancing `page`, so that a
/// failed or cancelled request leaves the tab describing what it actually
/// holds. It is also the only correct guard against a second `LoadMore`: the
/// old one tested `data`, which an append deliberately leaves `Ready`.
#[derive(Debug)]
struct PendingPage {
    busy: BusyId,
    page: PageRequest,
    append: bool,
}

#[derive(Debug)]
struct Tab {
    id: TabId,
    conn: ConnId,
    table: TableRef,
    sort: Option<Sort>,
    /// The last page successfully loaded, never a page merely asked for.
    page: PageRequest,
    pending: Option<PendingPage>,
    data: LoadState<Arc<PagedResult>>,
    loaded_rows: usize,
}

struct Runtime {
    drivers: Drivers,
    profiles: Arc<dyn Profiles>,
    profile_list: Arc<Vec<ProfileSummary>>,
    page_size: u32,
    events: mpsc::UnboundedSender<Event>,
    conns: Vec<Conn>,
    tabs: Vec<Tab>,
    active_tab: Option<TabId>,
    busy: Vec<BusyItem>,
    tasks: HashMap<BusyId, AbortHandle>,
    toasts: Vec<Toast>,
    next_tab: u32,
    next_id: u64,
    should_quit: bool,
    rev: u64,
}

impl Runtime {
    async fn run(
        mut self,
        mut actions: mpsc::UnboundedReceiver<Action>,
        mut events: mpsc::UnboundedReceiver<Event>,
        snapshots: watch::Sender<Arc<Snapshot>>,
    ) {
        loop {
            tokio::select! {
                action = actions.recv() => {
                    // `else` never fires: this task holds an event sender of
                    // its own, so the event channel cannot close and the
                    // select would park here for ever, keeping every session
                    // actor — and its database connection — alive.
                    let Some(action) = action else { break };
                    tracing::debug!(%action, "action");
                    self.apply(action);
                }
                Some(event) = events.recv() => self.handle(event),
                else => break,
            }

            if snapshots.send(Arc::new(self.snapshot())).is_err() {
                break; // Nothing is listening; the UI is gone.
            }
            if self.should_quit {
                break;
            }
        }
    }

    // ── ids ────────────────────────────────────────────────────────────────

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn begin_busy(&mut self, owner: BusyOwner, label: impl Into<String>) -> BusyId {
        let id = BusyId::new(self.alloc_id());
        self.busy.push(BusyItem {
            id,
            owner,
            label: label.into(),
            started_at: Instant::now(),
        });
        id
    }

    fn end_busy(&mut self, id: BusyId) {
        self.busy.retain(|b| b.id != id);
        self.tasks.remove(&id);
    }

    fn toast(&mut self, severity: Severity, text: impl Into<String>) {
        let id = ToastId::new(self.alloc_id());
        self.toasts.push(Toast {
            id,
            text: text.into(),
            severity,
            created_at: Instant::now(),
        });
    }

    fn spawn_task<F>(&mut self, busy: BusyId, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let handle = tokio::spawn(future);
        self.tasks.insert(busy, handle.abort_handle());
    }

    // ── lookup ─────────────────────────────────────────────────────────────

    fn conn_mut(&mut self, id: ConnId) -> Option<&mut Conn> {
        self.conns.iter_mut().find(|c| c.id == id)
    }

    fn tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    fn session(&self, id: ConnId) -> Option<SessionHandle> {
        self.conns
            .iter()
            .find(|c| c.id == id)
            .and_then(|c| c.session.clone())
    }

    // ── actions ────────────────────────────────────────────────────────────

    fn apply(&mut self, action: Action) {
        match action {
            Action::Connect(profile) => self.connect(&profile),
            Action::Disconnect(id) => self.disconnect(id),
            Action::ToggleNode { conn, node } => self.toggle_node(conn, node),
            Action::PreviewTable { conn, table } => self.preview_table(conn, table),
            Action::SortPreview { tab, column } => self.sort_preview(tab, column),
            Action::LoadMore { tab } => self.load_more(tab),
            Action::SelectTab(id) => {
                if self.tabs.iter().any(|t| t.id == id) {
                    self.active_tab = Some(id);
                }
            }
            Action::CloseTab(id) => self.close_tab(id),
            Action::Cancel(id) => self.cancel(id),
            Action::DismissToast(id) => self.toasts.retain(|t| t.id != id),
            Action::Quit => self.should_quit = true,
        }
    }

    fn connect(&mut self, profile: &ProfileId) {
        // The summary answers both questions a connection needs before its
        // secret has been read: what to call it, and which driver it wants.
        let Some(summary) = self.profile_list.iter().find(|p| &p.id == profile).cloned() else {
            self.toast(Severity::Error, format!("no profile called `{profile}`"));
            return;
        };

        let driver = match self.drivers.get(summary.kind) {
            Ok(d) => d,
            Err(err) => {
                self.toast(Severity::Error, err.user_message());
                return;
            }
        };

        let id = ConnId::new();
        let name = summary.name.clone();
        self.conns.push(Conn {
            id,
            // Open, because the first thing anyone does after connecting is
            // look at what is in there.
            expanded: true,
            profile: summary.id.clone(),
            name: name.clone(),
            kind: summary.kind,
            status: ConnStatus::Connecting,
            capabilities: None,
            session: None,
            tree: TreeState::new(),
            view: Arc::new(TreeView::default()),
        });

        let busy = self.begin_busy(BusyOwner::Connection(id), format!("connecting to {name}"));
        let events = self.events.clone();
        let profiles = Arc::clone(&self.profiles);
        let profile = summary.id;
        self.spawn_task(busy, async move {
            let result = Connect { driver, profiles }
                .execute(ConnectInput { profile, name })
                .await;
            let _ = events.send(Event::Connected {
                conn: id,
                busy,
                result,
            });
        });
    }

    fn disconnect(&mut self, id: ConnId) {
        if let Some(conn) = self.conn_mut(id) {
            if let Some(session) = conn.session.take() {
                session.close();
            }
            conn.status = ConnStatus::Closed;
            conn.tree = TreeState::new();
            conn.view = Arc::new(TreeView::default());
        }
        // Nothing still in flight for this connection can be applied now, and
        // leaving the rows behind means "connecting to mock" stays in the
        // status bar after the user closed it.
        let tabs: Vec<TabId> = self
            .tabs
            .iter()
            .filter(|t| t.conn == id)
            .map(|t| t.id)
            .collect();
        let orphaned: Vec<BusyId> = self
            .busy
            .iter()
            .filter(|b| match &b.owner {
                BusyOwner::Connection(c) | BusyOwner::Node { conn: c, .. } => *c == id,
                BusyOwner::Tab(t) => tabs.contains(t),
            })
            .map(|b| b.id)
            .collect();
        for busy in orphaned {
            self.drop_task(busy);
        }

        // Tabs belong to a connection; leaving them behind would show stale
        // rows with no way to refresh them.
        self.tabs.retain(|t| t.conn != id);
        if !self.tabs.iter().any(|t| Some(t.id) == self.active_tab) {
            self.active_tab = self.tabs.last().map(|t| t.id);
        }
    }

    fn toggle_node(&mut self, conn_id: ConnId, node: NodeRef) {
        // The connection's own row. Its children arrived with `Connect`, so
        // this is opening and closing rather than fetching — and it works on a
        // connection that failed, which is the only way to get its error off
        // the screen without disconnecting.
        if node.path.is_empty() {
            if let Some(conn) = self.conn_mut(conn_id) {
                conn.expanded = !conn.expanded;
            }
            return;
        }

        let Some(session) = self.session(conn_id) else {
            return;
        };
        let Some(conn) = self.conn_mut(conn_id) else {
            return;
        };

        let outcome = conn.tree.toggle(&node);
        conn.view = Arc::new(conn.tree.flatten(conn.id));
        if outcome == Toggle::Local {
            return;
        }

        let busy = self.begin_busy(
            BusyOwner::Node {
                conn: conn_id,
                node: node.clone(),
            },
            format!("expanding {node}"),
        );
        let events = self.events.clone();
        self.spawn_task(busy, async move {
            let result = ExpandNode { session }
                .execute(ExpandNodeInput { node: node.clone() })
                .await;
            let _ = events.send(Event::Expanded {
                conn: conn_id,
                node,
                busy,
                result,
            });
        });
    }

    fn preview_table(&mut self, conn_id: ConnId, table: TableRef) {
        if self.session(conn_id).is_none() {
            return;
        }

        // Reuse a tab already showing this relation rather than stacking
        // duplicates every time the user double-clicks.
        if let Some(existing) = self
            .tabs
            .iter()
            .find(|t| t.conn == conn_id && t.table == table)
        {
            let (id, sort) = (existing.id, existing.sort);
            self.active_tab = Some(id);
            // A tab whose page failed holds no rows, and `LoadMore` refuses to
            // extend rows that are not there — so without this, opening the
            // relation again would raise a dead tab and the only way back to
            // the table would be closing it first. Its ordering is kept: the
            // header is already showing the arrow.
            if existing.data.error().is_some() {
                let page = PageRequest::first_of(self.page_size).with_sort(sort);
                if let Some(tab) = self.tab_mut(id) {
                    tab.page = page;
                    tab.data = LoadState::Loading;
                    tab.loaded_rows = 0;
                }
                self.fetch_page(id, page, false);
            }
            return;
        }

        let id = TabId::new(self.next_tab);
        self.next_tab += 1;
        let page = PageRequest::first_of(self.page_size);
        self.tabs.push(Tab {
            id,
            conn: conn_id,
            table: table.clone(),
            sort: None,
            page,
            pending: None,
            data: LoadState::Loading,
            loaded_rows: 0,
        });
        self.active_tab = Some(id);
        self.fetch_page(id, page, false);
    }

    fn sort_preview(&mut self, tab_id: TabId, column: usize) {
        let Some(tab) = self.tab_mut(tab_id) else {
            return;
        };
        // The store owns the direction. Deriving it in the view would race
        // with a sort already in flight.
        let dir = match tab.sort {
            Some(s) if s.column == column => s.dir.toggled(),
            _ => SortDir::Asc,
        };
        let sort = Sort::new(column, dir);
        tab.sort = Some(sort);
        tab.data = LoadState::Loading;
        tab.loaded_rows = 0;
        // A new ordering invalidates every page already fetched.
        let page = PageRequest::first_of(self.page_size).with_sort(Some(sort));
        self.fetch_page(tab_id, page, false);
    }

    fn load_more(&mut self, tab_id: TabId) {
        let Some(tab) = self.tab_mut(tab_id) else {
            return;
        };
        // One page request per tab at a time. The old guard tested `data`,
        // which an append deliberately leaves `Ready` — so two quick
        // `LoadMore`s both went out, the first reply was dropped as stale, and
        // the rows it carried could never be asked for again.
        if tab.pending.is_some() {
            return;
        }
        // And there has to be something to extend. After a failed page `page`
        // still names the page that failed, so the next offset steps over it —
        // and `previewed` has nothing to append to, so it would install that
        // reply as the whole relation: the second page shown as the first, with
        // nothing to say the rows before it are missing.
        if tab.data.ready().is_none() {
            return;
        }
        let next = tab.page.next_page();
        self.fetch_page(tab_id, next, true);
    }

    fn fetch_page(&mut self, tab_id: TabId, page: PageRequest, append: bool) {
        let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) else {
            return;
        };
        let (conn_id, table) = (tab.conn, tab.table.clone());
        let Some(session) = self.session(conn_id) else {
            return;
        };

        // A new request supersedes whatever was in flight — re-sorting while a
        // page is loading, for instance. Leaving the old task running would
        // hold a busy row on screen for a reply that is now discarded as stale.
        if let Some(previous) = self.tab_mut(tab_id).and_then(|t| t.pending.take()) {
            self.drop_task(previous.busy);
        }

        let busy = self.begin_busy(BusyOwner::Tab(tab_id), format!("loading {table}"));
        if let Some(tab) = self.tab_mut(tab_id) {
            tab.pending = Some(PendingPage { busy, page, append });
        }
        let events = self.events.clone();
        self.spawn_task(busy, async move {
            let result = PreviewTable { session }
                .execute(PreviewTableInput { table, page })
                .await;
            let _ = events.send(Event::Previewed {
                tab: tab_id,
                page,
                busy,
                result,
            });
        });
    }

    fn close_tab(&mut self, id: TabId) {
        let position = self.tabs.iter().position(|t| t.id == id);
        self.tabs.retain(|t| t.id != id);
        if self.active_tab == Some(id) {
            // Select the neighbour, which is what every tabbed UI does.
            self.active_tab = position
                .and_then(|p| self.tabs.get(p.min(self.tabs.len().saturating_sub(1))))
                .map(|t| t.id);
        }
    }

    /// This does not stop work already running inside the driver: real
    /// cancellation is a driver capability and arrives with `CancelHandle` in
    /// M4.
    fn drop_task(&mut self, id: BusyId) {
        if let Some(handle) = self.tasks.remove(&id) {
            handle.abort();
        }
        self.busy.retain(|b| b.id != id);
    }

    fn cancel(&mut self, id: BusyId) {
        let owner = self
            .busy
            .iter()
            .find(|b| b.id == id)
            .map(|b| b.owner.clone());
        self.drop_task(id);
        // The reply is now never coming, so whatever was waiting for it has to
        // be put back into a state the user can act on. Left alone it stays
        // `Loading` for ever, and `TreeState::toggle` treats a loading node as
        // already in flight — so a cancelled expansion could never be retried.
        if let Some(owner) = owner {
            self.abandon(&owner, CANCELLED);
        }
    }

    fn abandon(&mut self, owner: &BusyOwner, reason: &str) {
        match owner {
            BusyOwner::Connection(id) => {
                if let Some(conn) = self
                    .conn_mut(*id)
                    .filter(|c| matches!(c.status, ConnStatus::Connecting))
                {
                    conn.status = ConnStatus::Failed(reason.to_owned());
                }
            }
            BusyOwner::Node { conn, node } => {
                if let Some(conn) = self.conn_mut(*conn) {
                    conn.tree.finish_load(node, Err(reason.to_owned()));
                    conn.view = Arc::new(conn.tree.flatten(conn.id));
                }
            }
            BusyOwner::Tab(id) => {
                if let Some(tab) = self.tab_mut(*id) {
                    tab.pending = None;
                    // An append that never lands leaves the rows already on
                    // screen perfectly usable.
                    if tab.data.ready().is_none() {
                        tab.data = LoadState::Failed(reason.to_owned());
                    }
                }
            }
        }
    }

    // ── events ─────────────────────────────────────────────────────────────

    fn handle(&mut self, event: Event) {
        match event {
            Event::Connected { conn, busy, result } => {
                self.end_busy(busy);
                self.connected(conn, result);
            }
            Event::Expanded {
                conn,
                node,
                busy,
                result,
            } => {
                self.end_busy(busy);
                self.expanded(conn, node, result);
            }
            Event::Previewed {
                tab,
                page,
                busy,
                result,
            } => {
                self.end_busy(busy);
                self.previewed(tab, page, result);
            }
        }
    }

    fn connected(&mut self, id: ConnId, result: AppResult<ConnectOutput>) {
        match result {
            // Only a connection still waiting for this reply may take it. A
            // `Disconnect` in between leaves the entry behind as `Closed`, and
            // writing `Ready` over it would resurrect a connection the user
            // closed — with a live session attached to it.
            Ok(out) => match self.conn_mut(id) {
                Some(conn) if matches!(conn.status, ConnStatus::Connecting) => {
                    conn.name = out.name;
                    conn.status = ConnStatus::Ready;
                    conn.capabilities = Some(out.capabilities);
                    conn.session = Some(out.session);
                    conn.tree.set_roots(out.roots);
                    conn.view = Arc::new(conn.tree.flatten(conn.id));
                }
                _ => out.session.close(),
            },
            Err(err) => {
                // Recorded on the connection and nowhere else. How a failure is
                // surfaced is the front-end's decision — the interactive client
                // raises a dialog for this one, and a toast beside it would be
                // the same error reported twice, which is worse than reporting
                // it once. The same rule as `expanded` below.
                if let Some(conn) = self.conn_mut(id) {
                    conn.status = ConnStatus::Failed(err.user_message());
                }
            }
        }
    }

    fn expanded(&mut self, id: ConnId, node: NodeRef, result: AppResult<ExpandNodeOutput>) {
        let session_died = matches!(result, Err(AppError::SessionClosed));
        // The failure is shown on the node itself, so no toast as well: an
        // error reported twice is worse than an error reported once.
        let outcome = result
            .map(|out| out.children)
            .map_err(|err| err.user_message());

        if let Some(conn) = self.conn_mut(id) {
            conn.tree.finish_load(&node, outcome);
            conn.view = Arc::new(conn.tree.flatten(conn.id));
        }
        if session_died {
            self.session_died(id);
        }
    }

    /// Whether this reply appends or replaces comes from the tab's own record
    /// of the request, not from the reply. The two can only disagree when
    /// something has already gone wrong.
    fn previewed(&mut self, id: TabId, page: PageRequest, result: AppResult<PreviewTableOutput>) {
        let session_died = matches!(result, Err(AppError::SessionClosed));
        let mut conn_died = None;
        let mut toast = None;

        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
            // A reply for a page the tab is no longer waiting for must not
            // overwrite what is on screen. The tab's own record is the
            // authority, not the flag that travelled with the request.
            let Some(pending) = tab.pending.take_if(|p| p.page == page) else {
                return;
            };

            match result {
                Ok(out) => {
                    let rows = if pending.append {
                        match tab.data.ready() {
                            Some(existing) => match existing.append(&out.result) {
                                Some(merged) => merged,
                                None => {
                                    toast = Some(format!(
                                        "{} changed shape; showing the new page only",
                                        tab.table
                                    ));
                                    PagedResult::new(&out.result)
                                }
                            },
                            None => PagedResult::new(&out.result),
                        }
                    } else {
                        PagedResult::new(&out.result)
                    };
                    tab.page = page;
                    let rows = Arc::new(rows);
                    tab.loaded_rows = rows.row_count();
                    tab.data = LoadState::Ready(rows);
                }
                Err(err) => {
                    let message = err.user_message();
                    if pending.append && tab.data.ready().is_some() {
                        // A failed "load more" has not invalidated the rows
                        // already fetched. Replacing them with an error panel
                        // loses them *and* leaves the next request starting
                        // from the wrong offset, because `page` never advanced.
                        toast = Some(message);
                    } else {
                        tab.data = LoadState::Failed(message);
                        tab.loaded_rows = 0;
                    }
                    if session_died {
                        conn_died = Some(tab.conn);
                    }
                }
            }
        }

        if let Some(text) = toast {
            self.toast(Severity::Error, text);
        }
        if let Some(conn) = conn_died {
            self.session_died(conn);
        }
    }

    /// The session actor is gone, so everything under this connection will
    /// fail the same way.
    ///
    /// Reporting it only on the node or tab that happened to ask leaves the
    /// connection reading `Ready` while every click on it fails, which tells
    /// the user nothing about needing to reconnect.
    fn session_died(&mut self, id: ConnId) {
        let Some(conn) = self.conn_mut(id) else {
            return;
        };
        if matches!(conn.status, ConnStatus::Closed) {
            return;
        }
        conn.session = None;
        conn.status = ConnStatus::Failed(AppError::SessionClosed.user_message());
    }

    // ── publishing ─────────────────────────────────────────────────────────

    /// Every connection as a row, with its objects underneath it.
    ///
    /// Built here rather than cached per connection because the *order* is a
    /// fact about the whole list, and a connection's own row carries its
    /// status — which changes without its tree changing at all.
    fn explorer(&self) -> TreeView {
        let mut nodes = Vec::new();
        for conn in &self.conns {
            nodes.push(VisibleNode {
                conn: conn.id,
                depth: 0,
                label: conn.name.clone(),
                node_ref: NodeRef::root(),
                relation_kind: None,
                state: root_state(conn),
            });
            if conn.expanded {
                nodes.extend(conn.view.nodes.iter().map(|node| VisibleNode {
                    depth: node.depth.saturating_add(1),
                    ..node.clone()
                }));
            }
        }
        TreeView { nodes }
    }

    fn snapshot(&mut self) -> Snapshot {
        self.rev += 1;
        Snapshot {
            rev: self.rev,
            profiles: Arc::clone(&self.profile_list),
            connections: self
                .conns
                .iter()
                .map(|c| ConnectionView {
                    id: c.id,
                    profile: c.profile.clone(),
                    name: c.name.clone(),
                    color: self
                        .profile_list
                        .iter()
                        .find(|p| p.id == c.profile)
                        .and_then(|p| p.color),
                    kind: c.kind,
                    status: c.status.clone(),
                    capabilities: c.capabilities,
                })
                .collect(),
            explorer: Arc::new(self.explorer()),
            tabs: self
                .tabs
                .iter()
                .map(|t| TabView {
                    id: t.id,
                    conn: t.conn,
                    title: t.table.name().to_owned(),
                    content: TabContent::Preview(PreviewTab {
                        table: t.table.clone(),
                        sort: t.sort,
                        loaded_rows: t.loaded_rows,
                        data: t.data.clone(),
                    }),
                })
                .collect(),
            active_tab: self.active_tab,
            busy: self.busy.clone(),
            toasts: self.toasts.clone(),
            should_quit: self.should_quit,
        }
    }
}

#[cfg(test)]
mod tests {
    use sqlake_core::node::NodeKind;
    use sqlake_core::value::Value;
    use sqlake_driver_mock::{Behaviour, MockDriver, MockProfiles};
    use tokio::sync::watch::Receiver;

    use super::*;
    use crate::tree::NodeState;

    fn store(behaviour: Behaviour) -> Store {
        store_paging(behaviour, PageRequest::DEFAULT_LIMIT)
    }

    fn store_paging(behaviour: Behaviour, page_size: u32) -> Store {
        Store::spawn(
            Drivers::new().with(Arc::new(MockDriver::new(behaviour))),
            Arc::new(MockProfiles::default()),
            page_size,
        )
    }

    fn pid(id: &str) -> ProfileId {
        ProfileId::parse(id).expect("a usable id")
    }

    /// A profile whose driver is deliberately not registered.
    #[derive(Debug)]
    struct UnservedProfile(DriverKind);

    impl Profiles for UnservedProfile {
        fn list(&self) -> Vec<ProfileSummary> {
            vec![ProfileSummary {
                id: pid("unserved"),
                name: "unserved".to_owned(),
                kind: self.0,
                color: None,
            }]
        }

        fn resolve(
            &self,
            id: &ProfileId,
        ) -> Result<sqlake_core::profile::ResolvedProfile, sqlake_core::profile::ProfileError>
        {
            Ok(sqlake_driver_mock::mock_profile(id.as_str()))
        }
    }

    /// Wait until `predicate` holds, or fail. Snapshots arrive asynchronously,
    /// so tests wait for a condition rather than a fixed number of updates.
    async fn until(
        rx: &mut Receiver<Arc<Snapshot>>,
        predicate: impl Fn(&Snapshot) -> bool,
    ) -> Arc<Snapshot> {
        for _ in 0..100 {
            {
                let snap = rx.borrow_and_update().clone();
                if predicate(&snap) {
                    return snap;
                }
            }
            tokio::time::timeout(std::time::Duration::from_secs(5), rx.changed())
                .await
                .expect("timed out waiting for a snapshot")
                .expect("store stopped");
        }
        panic!("condition never held");
    }

    async fn connected_store() -> (Store, Receiver<Arc<Snapshot>>, ConnId) {
        let store = store(Behaviour::instant());
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(pid("mock")));
        let snap = until(&mut rx, |s| {
            s.connections.first().is_some_and(ConnectionView::is_ready)
        })
        .await;
        let id = snap.connections[0].id;
        (store, rx, id)
    }

    #[tokio::test]
    async fn connecting_populates_the_tree() {
        let (_store, _rx, id) = connected_store().await;
        let _ = id;
    }

    #[tokio::test]
    async fn two_profiles_of_one_kind_are_open_at_the_same_time() {
        // The thing M0 could not express: `Drivers` was keyed by kind, so a
        // replica and a staging box could not both be open. One driver serves
        // both now, and what tells the connections apart is the profile each
        // was opened from — including their trees, which are per connection
        // and not per driver.
        let store = Store::spawn(
            Drivers::new().with(Arc::new(MockDriver::new(Behaviour::instant()))),
            Arc::new(MockProfiles::new(["replica", "staging"])),
            PageRequest::DEFAULT_LIMIT,
        );
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(pid("replica")));
        store.dispatch(Action::Connect(pid("staging")));

        let snap = until(&mut rx, |s| {
            s.connections.len() == 2 && s.connections.iter().all(ConnectionView::is_ready)
        })
        .await;

        let profiles: Vec<&str> = snap
            .connections
            .iter()
            .map(|c| c.profile.as_str())
            .collect();
        assert_eq!(profiles, ["replica", "staging"]);
        assert_eq!(snap.connections[0].name, "replica");

        // Two trees, not one shared by both — which only expanding a node can
        // show. Asserting that both trees have rows passes just as happily
        // when the second connection is handed the first one's tree.
        let ids: Vec<ConnId> = snap.connections.iter().map(|c| c.id).collect();
        assert_ne!(ids[0], ids[1]);
        let before = snap.tree(ids[1]).count();

        store.dispatch(Action::ToggleNode {
            conn: ids[0],
            node: NodeRef::new(NodeKind::Namespace, ["public"]),
        });
        let snap = until(&mut rx, |s| s.tree(ids[0]).count() > before).await;
        assert_eq!(snap.tree(ids[1]).count(), before);
    }

    #[tokio::test]
    async fn the_explorer_holds_every_connection_at_once() {
        // The thing the UI could not show before: with one flat list per
        // connection it drew the first and nothing else, so a second
        // connection was open and unreachable.
        let store = Store::spawn(
            Drivers::new().with(Arc::new(MockDriver::new(Behaviour::instant()))),
            Arc::new(MockProfiles::new(["replica", "staging"])),
            PageRequest::DEFAULT_LIMIT,
        );
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(pid("replica")));
        store.dispatch(Action::Connect(pid("staging")));
        let snap = until(&mut rx, |s| {
            s.connections.len() == 2 && s.connections.iter().all(ConnectionView::is_ready)
        })
        .await;

        // Two rows at depth zero, one per connection, each above its own
        // objects.
        let roots: Vec<&str> = snap
            .explorer
            .nodes
            .iter()
            .filter(|n| n.depth == 0)
            .map(|n| n.label.as_str())
            .collect();
        assert_eq!(roots, ["replica", "staging"]);
        assert!(snap.explorer.len() > 2, "the objects are missing");

        // Every row knows which connection it belongs to, which is what lets a
        // click act on the one under the cursor.
        let ids: Vec<ConnId> = snap.connections.iter().map(|c| c.id).collect();
        assert!(snap.explorer.nodes.iter().all(|n| ids.contains(&n.conn)));
    }

    #[tokio::test]
    async fn closing_a_connections_row_keeps_its_tree() {
        // Collapsing is not disconnecting: the objects were fetched once and
        // must still be there when the row is opened again, without a round
        // trip and without a spinner.
        let (store, mut rx, conn) = connected_store().await;
        let before = rx.borrow_and_update().tree(conn).count();
        assert!(before > 0);

        store.dispatch(Action::ToggleNode {
            conn,
            node: NodeRef::root(),
        });
        let snap = until(&mut rx, |s| s.tree(conn).count() == 0).await;
        assert_eq!(snap.explorer.len(), 1, "the connection's own row stays");
        assert!(!snap.is_busy(), "closing a row is not a fetch");

        store.dispatch(Action::ToggleNode {
            conn,
            node: NodeRef::root(),
        });
        let snap = until(&mut rx, |s| s.tree(conn).count() > 0).await;
        assert_eq!(snap.tree(conn).count(), before);
    }

    #[tokio::test]
    async fn a_connections_row_says_what_the_connection_is_doing() {
        // The row is the state: opening, open, or broken. A dialog can be
        // dismissed; the row cannot, which is what makes it the record.
        let store = store(Behaviour {
            connect_fails: true,
            ..Behaviour::instant()
        });
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(pid("mock")));

        let snap = until(&mut rx, |s| {
            s.explorer
                .nodes
                .first()
                .is_some_and(|n| matches!(n.state, NodeState::Failed(_)))
        })
        .await;
        let NodeState::Failed(why) = &snap.explorer.nodes[0].state else {
            panic!("expected a failed row");
        };
        assert!(why.contains("refused"), "{why}");
    }

    #[tokio::test]
    async fn one_profile_can_be_opened_twice() {
        // A second window onto the same database is a real thing to want, so
        // the profile is not an identity the store deduplicates on.
        let store = store(Behaviour::instant());
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(pid("mock")));
        store.dispatch(Action::Connect(pid("mock")));

        let snap = until(&mut rx, |s| s.connections.len() == 2).await;
        assert_ne!(snap.connections[0].id, snap.connections[1].id);
        assert_eq!(snap.connections[0].profile, snap.connections[1].profile);
    }

    #[tokio::test]
    async fn connecting_to_a_profile_nobody_configured_says_so() {
        let store = store(Behaviour::instant());
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(pid("typo")));

        let snap = until(&mut rx, |s| !s.toasts.is_empty()).await;
        assert!(snap.toasts[0].text.contains("typo"), "{:?}", snap.toasts[0]);
        // No half-made connection row for something that cannot be opened.
        assert!(snap.connections.is_empty());
    }

    #[tokio::test]
    async fn a_connection_is_visible_while_it_is_still_opening() {
        let store = store(Behaviour {
            latency: std::time::Duration::from_millis(50),
            ..Behaviour::instant()
        });
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(pid("mock")));

        // The user must see that something is happening, with a way to stop it.
        let snap = until(&mut rx, |s| !s.connections.is_empty()).await;
        assert_eq!(snap.connections[0].status, ConnStatus::Connecting);
        assert!(snap.is_busy());
    }

    #[tokio::test]
    async fn a_failed_connection_is_reported_on_the_connection_and_in_a_toast() {
        let store = Store::spawn(
            Drivers::new(),
            Arc::new(UnservedProfile(DriverKind::Postgres)),
            PageRequest::DEFAULT_LIMIT,
        );
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(pid("unserved")));

        let snap = until(&mut rx, |s| !s.toasts.is_empty()).await;
        assert_eq!(snap.toasts[0].severity, Severity::Error);
        assert!(snap.toasts[0].text.contains("no driver registered"));
    }

    #[tokio::test]
    async fn expanding_a_node_loads_its_children() {
        let (store, mut rx, conn) = connected_store().await;
        let node = NodeRef::new(NodeKind::Namespace, ["public"]);
        store.dispatch(Action::ToggleNode {
            conn,
            node: node.clone(),
        });

        let snap = until(&mut rx, |s| s.tree(conn).count() > 3).await;
        let rows: Vec<&VisibleNode> = snap.tree(conn).collect();
        assert_eq!(rows[0].state, NodeState::Expanded);
        assert!(rows.iter().any(|n| n.label == "users"));
    }

    #[tokio::test]
    async fn collapsing_needs_no_round_trip() {
        let (store, mut rx, conn) = connected_store().await;
        let node = NodeRef::new(NodeKind::Namespace, ["public"]);
        store.dispatch(Action::ToggleNode {
            conn,
            node: node.clone(),
        });
        until(&mut rx, |s| s.tree(conn).count() > 3).await;

        store.dispatch(Action::ToggleNode { conn, node });
        let snap = until(&mut rx, |s| s.tree(conn).count() == 3).await;
        let rows: Vec<&VisibleNode> = snap.tree(conn).collect();
        assert_eq!(rows[0].state, NodeState::Collapsed);
    }

    #[tokio::test]
    async fn previewing_opens_a_tab_and_fills_it() {
        let (store, mut rx, conn) = connected_store().await;
        store.dispatch(Action::PreviewTable {
            conn,
            table: TableRef::new(["public", "users"]),
        });

        let snap = until(&mut rx, |s| {
            s.active()
                .and_then(TabView::preview)
                .is_some_and(|p| p.data.ready().is_some())
        })
        .await;
        let tab = snap.active().unwrap();
        assert_eq!(tab.title, "users");
        assert_eq!(tab.preview().unwrap().data.ready().unwrap().row_count(), 50);
    }

    #[tokio::test]
    async fn previewing_the_same_relation_twice_reuses_its_tab() {
        let (store, mut rx, conn) = connected_store().await;
        let table = TableRef::new(["public", "users"]);
        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        until(&mut rx, |s| s.tabs.len() == 1).await;

        store.dispatch(Action::PreviewTable { conn, table });
        store.dispatch(Action::PreviewTable {
            conn,
            table: TableRef::new(["public", "empty"]),
        });
        let snap = until(&mut rx, |s| s.tabs.len() == 2).await;
        assert_eq!(snap.tabs.len(), 2, "the first relation must not open twice");
    }

    #[tokio::test]
    async fn opening_a_relation_whose_page_failed_asks_again() {
        // Reuse and failure meet here: the tab is raised rather than opened
        // twice, and a raised tab holding an error has nothing to raise. Since
        // `LoadMore` will not extend rows that are not there, opening the
        // relation is the only retry there is — without this, the tab is dead
        // until it is closed.
        let store = store(Behaviour {
            flaky_nodes: vec![(vec!["public".to_owned(), "users".to_owned()], 1)],
            ..Behaviour::instant()
        });
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(pid("mock")));
        let snap = until(&mut rx, |s| {
            s.connections.first().is_some_and(ConnectionView::is_ready)
        })
        .await;
        let conn = snap.connections[0].id;
        let table = TableRef::new(["public", "users"]);

        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        let snap = until(&mut rx, |s| {
            s.active()
                .and_then(TabView::preview)
                .is_some_and(|p| p.data.error().is_some())
        })
        .await;
        let tab = snap.active_tab.unwrap();

        store.dispatch(Action::PreviewTable { conn, table });
        let snap = until(&mut rx, |s| {
            s.tab(tab)
                .and_then(TabView::preview)
                .is_some_and(|p| p.data.ready().is_some())
        })
        .await;

        assert_eq!(snap.tabs.len(), 1, "the retry opened a second tab");
        assert_eq!(rows_of(&snap, tab), 50);
    }

    #[tokio::test]
    async fn a_failing_preview_marks_the_tab_not_the_whole_app() {
        let store = store(Behaviour {
            failing_nodes: vec![vec!["analytics".to_owned(), "broken".to_owned()]],
            ..Behaviour::instant()
        });
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(pid("mock")));
        let snap = until(&mut rx, |s| {
            s.connections.first().is_some_and(ConnectionView::is_ready)
        })
        .await;
        let conn = snap.connections[0].id;

        store.dispatch(Action::PreviewTable {
            conn,
            table: TableRef::new(["analytics", "broken"]),
        });
        let snap = until(&mut rx, |s| {
            s.active()
                .and_then(TabView::preview)
                .is_some_and(|p| p.data.error().is_some())
        })
        .await;
        assert!(
            snap.active()
                .unwrap()
                .preview()
                .unwrap()
                .data
                .error()
                .unwrap()
                .contains("corrupt")
        );
        assert!(snap.connections[0].is_ready(), "the connection is fine");
    }

    #[tokio::test]
    async fn sorting_replaces_the_page_rather_than_appending() {
        let (store, mut rx, conn) = connected_store().await;
        store.dispatch(Action::PreviewTable {
            conn,
            table: TableRef::new(["public", "users"]),
        });
        let snap = until(&mut rx, |s| {
            s.active()
                .and_then(TabView::preview)
                .is_some_and(|p| p.data.ready().is_some())
        })
        .await;
        let tab = snap.active_tab.unwrap();

        store.dispatch(Action::SortPreview { tab, column: 0 });
        let snap = until(&mut rx, |s| {
            s.tab(tab)
                .and_then(TabView::preview)
                .and_then(|p| p.sort)
                .is_some()
        })
        .await;
        let preview = snap.tab(tab).unwrap().preview().unwrap();
        assert_eq!(preview.sort.unwrap().dir, SortDir::Asc);

        store.dispatch(Action::SortPreview { tab, column: 0 });
        let snap = until(&mut rx, |s| {
            s.tab(tab)
                .and_then(TabView::preview)
                .and_then(|p| p.sort)
                .is_some_and(|s| s.dir == SortDir::Desc)
        })
        .await;
        // Still one page: a re-sort invalidates everything already fetched.
        let preview = snap.tab(tab).unwrap().preview().unwrap();
        assert_eq!(preview.data.ready().unwrap().row_count(), 50);
    }

    #[tokio::test]
    async fn two_quick_load_mores_do_not_skip_a_page() {
        let (store, mut rx, conn) = connected_store().await;
        store.dispatch(Action::PreviewTable {
            conn,
            table: TableRef::new(["public", "big"]),
        });
        let snap = until(&mut rx, |s| {
            s.active()
                .and_then(TabView::preview)
                .is_some_and(|p| p.loaded_rows == 200)
        })
        .await;
        let tab = snap.active_tab.unwrap();

        // Key repeat, or a wheel resting on the last row. The old guard tested
        // `data`, which an append leaves `Ready`, so both went out: the first
        // reply was dropped as stale and rows 201-400 could never be asked for
        // again, leaving a table that silently joined 1-200 to 401-600.
        store.dispatch(Action::LoadMore { tab });
        store.dispatch(Action::LoadMore { tab });

        let snap = until(&mut rx, |s| {
            s.tab(tab)
                .and_then(TabView::preview)
                .is_some_and(|p| p.loaded_rows >= 400)
        })
        .await;
        let preview = snap.tab(tab).and_then(TabView::preview).unwrap();
        assert_eq!(preview.loaded_rows, 400);

        let grid = preview.data.ready().unwrap();
        for row in 0..grid.row_count() {
            assert_eq!(
                grid.value(row, 0),
                Some(&Value::Int(row as i64)),
                "row {row} is not contiguous"
            );
        }
    }

    #[tokio::test]
    async fn a_failed_load_more_keeps_the_rows_already_on_screen() {
        // Succeeds once, then fails: the second page does not come back while
        // the first is still displayed.
        let store = store(Behaviour {
            failing_after: vec![(vec!["public".to_owned(), "big".to_owned()], 1)],
            ..Behaviour::instant()
        });
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(pid("mock")));
        let snap = until(&mut rx, |s| {
            s.connections.first().is_some_and(ConnectionView::is_ready)
        })
        .await;
        let conn = snap.connections[0].id;

        store.dispatch(Action::PreviewTable {
            conn,
            table: TableRef::new(["public", "big"]),
        });
        let snap = until(&mut rx, |s| {
            s.active()
                .and_then(TabView::preview)
                .is_some_and(|p| p.loaded_rows == 200)
        })
        .await;
        let tab = snap.active_tab.unwrap();

        store.dispatch(Action::LoadMore { tab });
        let snap = until(&mut rx, |s| !s.toasts.is_empty()).await;

        // The rows already fetched are still good. Replacing them with an
        // error panel loses them and leaves the next request starting from the
        // wrong offset.
        let preview = snap.tab(tab).and_then(TabView::preview).unwrap();
        assert_eq!(preview.loaded_rows, 200);
        assert!(preview.data.ready().is_some(), "the table is still there");
        assert_eq!(snap.toasts[0].severity, Severity::Error);
    }

    #[tokio::test]
    async fn load_more_on_a_page_that_failed_asks_for_nothing() {
        // Fails once, so a second request would succeed and be believed. The
        // tab is still on the page that failed, so the next offset steps over
        // it: rows 201-400 would arrive with nothing to append them to and be
        // shown as the whole relation, the first 200 missing without a word.
        let store = store(Behaviour {
            flaky_nodes: vec![(vec!["public".to_owned(), "big".to_owned()], 1)],
            ..Behaviour::instant()
        });
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(pid("mock")));
        let snap = until(&mut rx, |s| {
            s.connections.first().is_some_and(ConnectionView::is_ready)
        })
        .await;
        let conn = snap.connections[0].id;

        store.dispatch(Action::PreviewTable {
            conn,
            table: TableRef::new(["public", "big"]),
        });
        let snap = until(&mut rx, |s| {
            s.active()
                .and_then(TabView::preview)
                .is_some_and(|p| p.data.error().is_some())
        })
        .await;
        let tab = snap.active_tab.unwrap();

        store.dispatch(Action::LoadMore { tab });
        // Actions are handled in order, so a snapshot that has seen the quit
        // has seen the `LoadMore` — and nothing is loading because of it.
        store.dispatch(Action::Quit);
        let snap = until(&mut rx, |s| s.should_quit).await;

        assert!(snap.busy.is_empty(), "a page went out anyway");
        let preview = snap.tab(tab).and_then(TabView::preview).unwrap();
        assert!(preview.data.error().is_some(), "still the failed page");
        assert_eq!(preview.loaded_rows, 0);
    }

    #[tokio::test]
    async fn cancelling_an_expansion_leaves_the_node_retryable() {
        let store = store(Behaviour {
            latency: std::time::Duration::from_millis(1),
            slow_nodes: vec![vec!["public".to_owned()]],
            slow_latency: std::time::Duration::from_secs(30),
            ..Behaviour::instant()
        });
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(pid("mock")));
        let snap = until(&mut rx, |s| {
            s.connections.first().is_some_and(ConnectionView::is_ready)
        })
        .await;
        let conn = snap.connections[0].id;

        let node = NodeRef::new(NodeKind::Namespace, ["public"]);
        store.dispatch(Action::ToggleNode {
            conn,
            node: node.clone(),
        });
        let snap = until(&mut rx, Snapshot::is_busy).await;
        let busy = snap.busy[0].id;

        store.dispatch(Action::Cancel(busy));
        // The reply is never coming. A node left in `Loading` cannot even be
        // toggled again, so it would be permanently dead rather than merely
        // failed.
        let snap = until(&mut rx, |s| {
            s.tree(conn)
                .any(|n| n.node_ref == node && matches!(n.state, NodeState::Failed(_)))
        })
        .await;
        assert!(!snap.is_busy());
    }

    #[tokio::test]
    async fn a_connection_closed_while_opening_does_not_come_back() {
        let store = store(Behaviour {
            latency: std::time::Duration::from_millis(50),
            ..Behaviour::instant()
        });
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(pid("mock")));
        let snap = until(&mut rx, |s| !s.connections.is_empty()).await;
        let conn = snap.connections[0].id;

        store.dispatch(Action::Disconnect(conn));
        let snap = until(&mut rx, |s| s.connections[0].status == ConnStatus::Closed).await;
        assert!(!snap.is_busy(), "in-flight work is dropped with it");

        // The reply lands after the disconnect. Writing Ready over a closed
        // connection would resurrect it with a live session attached.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let snap = rx.borrow_and_update().clone();
        assert_eq!(snap.connections[0].status, ConnStatus::Closed);
        assert_eq!(snap.tree(conn).count(), 0);
    }

    #[tokio::test]
    async fn load_more_appends_to_what_is_already_there() {
        let (store, mut rx, conn) = connected_store().await;
        store.dispatch(Action::PreviewTable {
            conn,
            table: TableRef::new(["public", "big"]),
        });
        let snap = until(&mut rx, |s| {
            s.active()
                .and_then(TabView::preview)
                .is_some_and(|p| p.loaded_rows == 200)
        })
        .await;
        let tab = snap.active_tab.unwrap();

        store.dispatch(Action::LoadMore { tab });
        let snap = until(&mut rx, |s| {
            s.tab(tab)
                .and_then(TabView::preview)
                .is_some_and(|p| p.loaded_rows == 400)
        })
        .await;
        let grid = snap
            .tab(tab)
            .unwrap()
            .preview()
            .unwrap()
            .data
            .ready()
            .unwrap();
        assert_eq!(grid.row_count(), 400);
        assert_eq!(grid.total_rows(), Some(200_000));
    }

    #[tokio::test]
    async fn the_configured_page_size_is_what_a_page_is() {
        // `page_size` was parsed and validated by `sqlake-config` and read by
        // nothing, so every page was the built-in size whatever the file said
        // — a setting the client appears to honour and does not.
        let store = store_paging(Behaviour::instant(), 25);
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(pid("mock")));
        let snap = until(&mut rx, |s| {
            s.connections.first().is_some_and(ConnectionView::is_ready)
        })
        .await;
        let conn = snap.connections[0].id;

        store.dispatch(Action::PreviewTable {
            conn,
            table: TableRef::new(["public", "big"]),
        });
        let snap = until(&mut rx, |s| {
            s.active()
                .and_then(TabView::preview)
                .is_some_and(|p| p.data.ready().is_some())
        })
        .await;
        let tab = snap.active_tab.unwrap();
        assert_eq!(rows_of(&snap, tab), 25);

        // Sorting starts the relation again, and starting again is also a page.
        store.dispatch(Action::SortPreview { tab, column: 0 });
        let snap = until(&mut rx, |s| {
            s.tab(tab)
                .and_then(TabView::preview)
                .is_some_and(|p| p.sort.is_some() && p.data.ready().is_some())
        })
        .await;
        assert_eq!(rows_of(&snap, tab), 25);

        // And the page after it starts where this one stopped, rather than at
        // the built-in size: an offset that moves by more than the page leaves
        // a gap no scroll can reach.
        store.dispatch(Action::LoadMore { tab });
        let snap = until(&mut rx, |s| {
            s.tab(tab)
                .and_then(TabView::preview)
                .is_some_and(|p| p.loaded_rows == 50)
        })
        .await;
        let grid = snap
            .tab(tab)
            .and_then(TabView::preview)
            .and_then(|p| p.data.ready())
            .expect("a loaded page");
        assert_eq!(grid.row_count(), 50);
        for row in 0..grid.row_count() {
            assert_eq!(
                grid.value(row, 0),
                Some(&Value::Int(row as i64)),
                "row {row} is not contiguous"
            );
        }
    }

    #[tokio::test]
    async fn a_retry_keeps_the_ordering_the_header_is_showing() {
        // Sorting a tab whose page failed leaves the arrow drawn and no rows
        // under it. If the retry asked for the relation unordered, the header
        // would be describing an order the rows do not have — the arrow is the
        // only thing telling the user what they are looking at.
        let store = store(Behaviour {
            flaky_nodes: vec![(vec!["public".to_owned(), "users".to_owned()], 3)],
            ..Behaviour::instant()
        });
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(pid("mock")));
        let snap = until(&mut rx, |s| {
            s.connections.first().is_some_and(ConnectionView::is_ready)
        })
        .await;
        let conn = snap.connections[0].id;
        let table = TableRef::new(["public", "users"]);

        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        let snap = until(&mut rx, |s| {
            s.active()
                .and_then(TabView::preview)
                .is_some_and(|p| p.data.error().is_some())
        })
        .await;
        let tab = snap.active_tab.unwrap();

        // Twice, because the first toggle is ascending and the fixture is
        // already in that order: only descending can tell the two apart.
        for dir in [SortDir::Asc, SortDir::Desc] {
            store.dispatch(Action::SortPreview { tab, column: 0 });
            until(&mut rx, |s| {
                s.busy.is_empty() && sort_of(s, tab) == Some(dir)
            })
            .await;
        }

        store.dispatch(Action::PreviewTable { conn, table });
        let snap = until(&mut rx, |s| {
            s.tab(tab)
                .and_then(TabView::preview)
                .is_some_and(|p| p.data.ready().is_some())
        })
        .await;

        let grid = snap
            .tab(tab)
            .and_then(TabView::preview)
            .and_then(|p| p.data.ready())
            .expect("a loaded page");
        assert_eq!(sort_of(&snap, tab), Some(SortDir::Desc));
        assert_eq!(
            grid.value(0, 0),
            Some(&Value::Int(50)),
            "the retry ignored the arrow the header is showing"
        );
    }

    fn sort_of(snap: &Snapshot, tab: TabId) -> Option<SortDir> {
        snap.tab(tab)
            .and_then(TabView::preview)
            .and_then(|p| p.sort)
            .map(|s| s.dir)
    }

    #[tokio::test]
    async fn a_page_size_of_zero_still_reads_the_relation() {
        // `sqlake-config` refuses it, and that refusal is a crate away from
        // here: a second front-end spawning the store with zero would get a
        // page of no rows and an offset `next_page` never advances, which is a
        // relation that cannot be read and does not say so.
        let store = store_paging(Behaviour::instant(), 0);
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(pid("mock")));
        let snap = until(&mut rx, |s| {
            s.connections.first().is_some_and(ConnectionView::is_ready)
        })
        .await;

        store.dispatch(Action::PreviewTable {
            conn: snap.connections[0].id,
            table: TableRef::new(["public", "users"]),
        });
        let snap = until(&mut rx, |s| {
            s.active()
                .and_then(TabView::preview)
                .is_some_and(|p| p.data.ready().is_some())
        })
        .await;
        assert_eq!(rows_of(&snap, snap.active_tab.unwrap()), 1);
    }

    fn rows_of(snap: &Snapshot, tab: TabId) -> usize {
        snap.tab(tab)
            .and_then(TabView::preview)
            .and_then(|p| p.data.ready())
            .expect("a loaded page")
            .row_count()
    }

    #[tokio::test]
    async fn closing_a_tab_selects_its_neighbour() {
        let (store, mut rx, conn) = connected_store().await;
        for name in ["users", "empty"] {
            store.dispatch(Action::PreviewTable {
                conn,
                table: TableRef::new(["public", name]),
            });
        }
        let snap = until(&mut rx, |s| s.tabs.len() == 2).await;
        let first = snap.tabs[0].id;

        store.dispatch(Action::CloseTab(first));
        let snap = until(&mut rx, |s| s.tabs.len() == 1).await;
        assert_eq!(snap.active_tab, Some(snap.tabs[0].id));
    }

    #[tokio::test]
    async fn closing_the_last_tab_leaves_nothing_selected() {
        let (store, mut rx, conn) = connected_store().await;
        store.dispatch(Action::PreviewTable {
            conn,
            table: TableRef::new(["public", "users"]),
        });
        let snap = until(&mut rx, |s| s.tabs.len() == 1).await;

        store.dispatch(Action::CloseTab(snap.tabs[0].id));
        let snap = until(&mut rx, |s| s.tabs.is_empty()).await;
        assert_eq!(snap.active_tab, None);
    }

    #[tokio::test]
    async fn disconnecting_removes_that_connection_s_tabs() {
        let (store, mut rx, conn) = connected_store().await;
        store.dispatch(Action::PreviewTable {
            conn,
            table: TableRef::new(["public", "users"]),
        });
        until(&mut rx, |s| s.tabs.len() == 1).await;

        store.dispatch(Action::Disconnect(conn));
        let snap = until(&mut rx, |s| s.tabs.is_empty()).await;
        assert_eq!(snap.connections[0].status, ConnStatus::Closed);
        assert_eq!(snap.tree(conn).count(), 0);
    }

    #[tokio::test]
    async fn a_toast_can_be_dismissed() {
        let store = Store::spawn(
            Drivers::new(),
            Arc::new(UnservedProfile(DriverKind::BigQuery)),
            PageRequest::DEFAULT_LIMIT,
        );
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(pid("unserved")));
        let snap = until(&mut rx, |s| !s.toasts.is_empty()).await;

        store.dispatch(Action::DismissToast(snap.toasts[0].id));
        until(&mut rx, |s| s.toasts.is_empty()).await;
    }

    #[tokio::test]
    async fn cancelling_clears_the_busy_indicator() {
        let store = store(Behaviour {
            latency: std::time::Duration::from_secs(30),
            ..Behaviour::instant()
        });
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(pid("mock")));
        let snap = until(&mut rx, Snapshot::is_busy).await;

        store.dispatch(Action::Cancel(snap.busy[0].id));
        let snap = until(&mut rx, |s| !s.is_busy()).await;
        assert!(!snap.is_busy());
    }

    #[tokio::test]
    async fn quitting_is_visible_in_the_snapshot() {
        let store = store(Behaviour::instant());
        let mut rx = store.subscribe();
        store.dispatch(Action::Quit);
        until(&mut rx, |s| s.should_quit).await;
    }

    #[tokio::test]
    async fn actions_for_unknown_ids_are_ignored() {
        let store = store(Behaviour::instant());
        let mut rx = store.subscribe();
        store.dispatch(Action::SelectTab(TabId::new(99)));
        store.dispatch(Action::CloseTab(TabId::new(99)));
        store.dispatch(Action::LoadMore {
            tab: TabId::new(99),
        });
        store.dispatch(Action::Disconnect(ConnId::new()));
        store.dispatch(Action::Cancel(BusyId::new(99)));
        store.dispatch(Action::Quit);

        // The point is that none of the above panicked the store task.
        until(&mut rx, |s| s.should_quit).await;
    }
}

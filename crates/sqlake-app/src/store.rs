//! The single owner of application state.
//!
//! One task owns everything mutable and is the only writer. It applies
//! [`Action`]s, invokes use cases, and republishes an immutable
//! [`Snapshot`] on a watch channel.
//!
//! The store never awaits a use case inline. Every call is spawned, and its
//! result comes back as an internal `Event`. Awaiting inline would let one slow
//! expansion block every other action on every connection, not only the one it
//! is slow on.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use time::OffsetDateTime;

use sqlake_core::capability::{Capabilities, DriverKind};
use sqlake_core::driver::Driver;
use sqlake_core::id::{ConnId, ProfileId, QueryId};
use sqlake_core::library::{
    Library, LibraryError, LibraryResult, RunId, RunOutcome, RunStart, Template,
};
use sqlake_core::node::{NodeRef, TableRef};
use sqlake_core::profile::{ProfileSummary, Profiles};
use sqlake_core::result::{PageRequest, Sort, SortDir};
use sqlake_core::sql::{Access, Estimate, RawSql};
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;

use crate::action::{Action, BusyId};
use crate::error::{AppError, AppResult};

/// What a session with no library says instead of a list.
///
/// A message rather than an empty list: nothing was saved *and* nothing can
/// be, and the two look identical in a pane that draws no rows.
const NOTHING_KEPT: &str = "this session is not keeping anything";
use crate::pages::PagedResult;
use crate::session::SessionHandle;
use crate::snapshot::{
    BusyItem, BusyOwner, ConnStatus, ConnectionView, DefinitionView, LoadState, PreviewView,
    QueryView, Snapshot, TemplatesView,
};
use crate::tree::{NodeState, Toggle, TreeState, TreeView, VisibleNode};
use crate::usecase::{
    Connect, ConnectInput, ConnectOutput, DescribeTable, DescribeTableInput, EstimateQuery,
    EstimateQueryInput, ExpandNode, ExpandNodeInput, ExpandNodeOutput, PreviewTable,
    PreviewTableInput, PreviewTableOutput, RunApproved, RunQuery, RunQueryInput, RunQueryOutput,
    UseCase,
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

/// Where an action sits in the queue.
///
/// Returned by [`Store::dispatch`] so a caller can tell a snapshot published
/// after its action from one published before. Without it every wait starts by
/// examining state that predates what it is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Dispatched(u64);

impl Dispatched {
    #[must_use]
    pub const fn ordinal(self) -> u64 {
        self.0
    }
}

/// The counter and the sender move together on purpose: an ordinal handed out
/// before a send that another thread wins is an ordinal for somebody else's
/// action. Once the socket server exists, dispatching concurrently is the
/// normal case rather than a corner of one.
#[derive(Debug)]
struct Queue {
    sent: u64,
    actions: mpsc::UnboundedSender<Action>,
}

#[derive(Debug, Clone)]
pub struct Store {
    queue: Arc<std::sync::Mutex<Queue>>,
    snapshots: watch::Receiver<Arc<Snapshot>>,
}

/// Everything the store is given rather than makes.
///
/// A struct because there are five of them now and the two at the end were
/// already unreadable at the call site: `PageRequest::DEFAULT_LIMIT, None`
/// says nothing about which is the page size and which the budget. The
/// defaults are what a test wants, so a test names only what it is about.
#[derive(Debug)]
pub struct Wiring {
    pub drivers: Drivers,
    pub profiles: Arc<dyn Profiles>,
    /// From the configuration rather than a constant here: how many rows are
    /// worth waiting for depends on the database and the link to it, which is
    /// something only the person using it knows.
    ///
    /// Zero is taken as one. `sqlake-config` refuses it, but that validation is
    /// a crate away and not on the path a second front-end takes: a page of no
    /// rows leaves an offset that `next_page` never advances, so the relation
    /// could never be read and nothing on screen would say why.
    pub page_size: u32,
    pub budget: Option<u64>,
    /// Where templates and history are kept, when anything is.
    ///
    /// `None` is a session that keeps nothing — a test, or a client whose
    /// state directory could not be opened. Optional rather than a do-nothing
    /// implementation living here: a second implementation of a trait whose
    /// first one is a schema would be a second set of answers to drift apart,
    /// and "nothing is being saved" is worth saying out loud rather than
    /// imitating.
    pub library: Option<Arc<dyn Library>>,
}

impl Wiring {
    #[must_use]
    pub fn new(drivers: Drivers, profiles: Arc<dyn Profiles>) -> Self {
        Self {
            drivers,
            profiles,
            page_size: PageRequest::DEFAULT_LIMIT,
            budget: None,
            library: None,
        }
    }

    #[must_use]
    pub const fn page_size(mut self, rows: u32) -> Self {
        self.page_size = rows;
        self
    }

    #[must_use]
    pub const fn budget(mut self, bytes: Option<u64>) -> Self {
        self.budget = bytes;
        self
    }

    #[must_use]
    pub fn library(mut self, library: Arc<dyn Library>) -> Self {
        self.library = Some(library);
        self
    }
}

impl Store {
    #[must_use]
    pub fn spawn(wiring: Wiring) -> Self {
        let Wiring {
            drivers,
            profiles,
            page_size,
            budget,
            library,
        } = wiring;
        let (action_tx, action_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let mut runtime = Runtime {
            drivers,
            // Read once, at startup. Editing `connections.toml` while the
            // client is running is a reload, and a reload is a feature with
            // its own questions — not a thing to do silently on every frame.
            profile_list: Arc::new(profiles.list()),
            profiles,
            page_size: page_size.max(1),
            budget,
            library,
            templates: TemplatesView::default(),
            runs: HashMap::new(),
            events: event_tx,
            conns: Vec::new(),
            previews: Vec::new(),
            definitions: Vec::new(),
            queries: Vec::new(),
            busy: Vec::new(),
            tasks: HashMap::new(),
            next_id: 1,
            should_quit: false,
            rev: 0,
            applied: 0,
        };
        // Published before the task starts, not from inside its loop, which
        // only runs when something happens. The profile list is known here and
        // is the answer to "what can I connect to" — a caller that asks before
        // it has done anything is exactly the caller that needs it, and would
        // otherwise be told there is nothing.
        let (snapshot_tx, snapshot_rx) = watch::channel(Arc::new(runtime.snapshot()));
        tokio::spawn(runtime.run(action_rx, event_rx, snapshot_tx));

        Self {
            queue: Arc::new(std::sync::Mutex::new(Queue {
                sent: 0,
                actions: action_tx,
            })),
            snapshots: snapshot_rx,
        }
    }

    /// Non-blocking on purpose: the render loop must never await.
    pub fn dispatch(&self, action: Action) -> Dispatched {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.sent += 1;
        // A closed store means the process is shutting down; dropping the
        // action is the correct response. The ordinal is still handed back, and
        // the wait it belongs to ends as `Stopped`.
        let _ = queue.actions.send(action);
        Dispatched(queue.sent)
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

/// A run's history row, which may not exist yet.
///
/// The row is written on a blocking task and a fast query can finish before
/// that task lands. Without somewhere to hold the outcome, the fast queries —
/// the ordinary ones — would be the ones the history never says the end of.
#[derive(Debug)]
enum Recording {
    Starting(Option<RunOutcome>),
    Row(RunId),
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
    Ran {
        query: QueryId,
        busy: BusyId,
        result: AppResult<RunQueryOutput>,
    },
    Estimated {
        query: QueryId,
        busy: BusyId,
        result: AppResult<Estimate>,
    },
    Described {
        conn: ConnId,
        table: TableRef,
        busy: BusyId,
        result: AppResult<sqlake_core::detail::TableDetail>,
    },
    /// A run's history row exists, and this is its id.
    ///
    /// Its own event because the row is written on a blocking task and the
    /// query may finish first — which is what [`Recording`] is for.
    Recorded {
        query: QueryId,
        result: LibraryResult<RunId>,
    },
    /// The library answered, and the answer is always the whole list: a write
    /// is followed by a read in the same trip to the file, because another
    /// window may have changed it too and the list is what a pane draws.
    Library {
        busy: BusyId,
        /// `Err` here is the *write* failing. A read that failed leaves the
        /// list alone and is reported in its place.
        wrote: Result<(), String>,
        result: AppResult<Vec<Template>>,
    },
    Previewed {
        conn: ConnId,
        table: TableRef,
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
    /// What the profile allows. `ReadOnly` until the connection opens, which
    /// is the safe answer to a question nobody can yet have asked: nothing
    /// runs on a connection that is not open.
    access: Access,
    session: Option<SessionHandle>,
    tree: TreeState,
    /// Cached flattening, refreshed only when the tree actually changes.
    view: Arc<TreeView>,
}

const CANCELLED: &str = "cancelled";
const DISCONNECTED: &str = "the connection was closed";

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

/// Whether opening a node may also close it.
#[derive(Debug, Clone, Copy)]
enum Open {
    Toggle,
    ExpandOnly,
}

/// A page request that has gone out and not yet come back.
///
/// The preview keeps this rather than optimistically advancing `page`, so
/// that a failed or cancelled request leaves it describing what it actually
/// holds. It is also the only correct guard against a second `LoadMore`: the
/// old one tested `data`, which an append deliberately leaves `Ready`.
#[derive(Debug)]
struct PendingPage {
    busy: BusyId,
    page: PageRequest,
    append: bool,
}

/// A relation's data, and the request still out for more of it.
#[derive(Debug)]
struct Preview {
    conn: ConnId,
    table: TableRef,
    sort: Option<Sort>,
    /// The last page successfully loaded, never a page merely asked for.
    page: PageRequest,
    /// Where the next page starts: one past the last row actually received.
    ///
    /// Counted from the rows that came back rather than from the limit that
    /// was asked for. BigQuery answers a 200-row request with fewer when the
    /// response would pass its 10 MB cap, and stepping by the limit would
    /// skip the rows it did not send — a grid drawing a contiguous relation
    /// that has holes in it.
    next_offset: u64,
    /// How wide the relation turned out to be. `None` until a page lands.
    columns: Option<usize>,
    pending: Option<PendingPage>,
    data: LoadState<Arc<PagedResult>>,
    loaded_rows: usize,
    last_error: Option<String>,
    /// The driver answered with fewer rows than were asked for, so there are
    /// no more.
    ///
    /// Recorded here because only the store knows what it asked for. Without
    /// it the end of a relation is "the request changed nothing", which is
    /// also what a cancelled request and a repeated identical failure look
    /// like — and a front-end reading them as the same thing either stops
    /// paging a relation that has more, or asks for a page past the end on
    /// every scroll for ever.
    exhausted: bool,
    /// How many page requests have finished for this preview, however they
    /// finished.
    ///
    /// A caller waiting for "something happened" cannot get that from the
    /// state a request leaves: a cancellation leaves none, and a retry that
    /// fails the same way leaves the same message.
    attempts: u64,
}

struct Runtime {
    drivers: Drivers,
    profiles: Arc<dyn Profiles>,
    profile_list: Arc<Vec<ProfileSummary>>,
    page_size: u32,
    /// Bytes a query may cost before somebody has to say yes, or `None` for no
    /// ceiling.
    ///
    /// Here rather than on the action, so raising it is a config change rather
    /// than something a caller can do per request.
    budget: Option<u64>,
    /// Templates and history, when this session keeps any.
    ///
    /// Held here rather than reached for at each use so that "nothing is being
    /// kept" is one branch in one place. Every call on it blocks, so every
    /// call on it goes through `spawn_blocking`.
    library: Option<Arc<dyn Library>>,
    templates: TemplatesView,
    /// The history row each running query is being kept in.
    ///
    /// Not in the snapshot: a `RunId` is bookkeeping between this store and a
    /// file, and no front-end has anything to do with one.
    runs: HashMap<QueryId, Recording>,
    events: mpsc::UnboundedSender<Event>,
    conns: Vec<Conn>,
    previews: Vec<Preview>,
    /// Fetched once each and kept, which is why `disconnect` drops them: a
    /// definition is exactly the thing somebody changes in another window, and
    /// nothing about it is re-fetched by scrolling the way a preview is.
    definitions: Vec<DefinitionView>,
    /// Every run this session has started, in order.
    ///
    /// Kept rather than replaced: a front-end may have several SQL tabs, and
    /// each of them is looking at a different one. `ForgetQuery` is how one
    /// goes, the way `ForgetPreview` already works for a relation.
    queries: Vec<QueryView>,
    busy: Vec<BusyItem>,
    tasks: HashMap<BusyId, AbortHandle>,
    next_id: u64,
    should_quit: bool,
    rev: u64,
    applied: u64,
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
                    self.applied += 1;
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

    fn preview_mut(&mut self, conn: ConnId, table: &TableRef) -> Option<&mut Preview> {
        self.previews
            .iter_mut()
            .find(|p| p.conn == conn && &p.table == table)
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
            Action::Connect { profile, conn } => self.connect(&profile, conn),
            Action::Disconnect(id) => self.disconnect(id),
            Action::ToggleNode { conn, node } => self.open_node(conn, node, Open::Toggle),
            Action::ExpandNode { conn, node } => self.open_node(conn, node, Open::ExpandOnly),
            Action::PreviewTable { conn, table } => self.preview_table(conn, table),
            Action::DescribeTable {
                conn,
                table,
                refresh,
            } => self.describe_table(conn, table, refresh),
            Action::ForgetDefinition { conn, table } => self.forget_definition(conn, &table),
            Action::SortPreview {
                conn,
                table,
                column,
            } => {
                self.sort_preview(conn, table, column);
            }
            Action::LoadMore { conn, table } => self.load_more(conn, table),
            Action::ForgetPreview { conn, table } => self.forget_preview(conn, &table),
            Action::RunQuery {
                conn,
                query,
                sql,
                max_rows,
                max_bytes,
            } => self.run_query(conn, query, sql, max_rows, max_bytes),
            Action::EstimateQuery { conn, query, sql } => self.estimate_query(conn, query, sql),
            Action::ApproveQuery(query) => self.approve_query(query),
            Action::ForgetQuery(query) => self.forget_query(query),
            Action::LoadTemplates => self.with_library(None, |_| Ok(())),
            Action::SaveTemplate(template) => {
                let name = template.name.clone();
                self.with_library(Some(format!("saving {name}")), move |library| {
                    library.add(template).map(|_| ())
                });
            }
            Action::ReplaceTemplate { id, with } => {
                let name = with.name.clone();
                self.with_library(Some(format!("saving {name}")), move |library| {
                    library.replace(id, with).map(|_| ())
                });
            }
            Action::DeleteTemplate(id) => {
                self.with_library(Some("deleting a template".to_owned()), move |library| {
                    library.remove(id)
                });
            }
            Action::Cancel(id) => self.cancel(id),
            Action::Quit => self.should_quit = true,
        }
    }

    fn connect(&mut self, profile: &ProfileId, id: ConnId) {
        // The caller names its own connection, so a duplicate is a caller that
        // has lost track of one it already has — not a request for a second
        // window onto the same database, which is what a second `Connect` with
        // a fresh id is. A closed or failed connection keeps its row and so
        // keeps its id: reopening is a fresh id, not this one back.
        if self.conns.iter().any(|c| c.id == id) {
            tracing::warn!(%profile, conn = %id.short(), "connect: that id is taken");
            return;
        }

        // The summary answers both questions a connection needs before its
        // secret has been read: what to call it, and which driver it wants.
        //
        // A name nothing listed is only reachable from a caller that can send
        // an arbitrary id — the agent surface, eventually. Logged rather than
        // surfaced: there is no connection yet to attach the reason to, and
        // inventing a row for a profile that does not exist would misreport
        // what was asked for.
        let Some(summary) = self.profile_list.iter().find(|p| &p.id == profile).cloned() else {
            tracing::warn!(%profile, "connect: no such profile");
            return;
        };

        let driver = match self.drivers.get(summary.kind) {
            Ok(d) => d,
            Err(err) => {
                // Unlike the lookup above, a profile can legitimately name a
                // driver this build has not shipped yet — so it gets a row,
                // where every other connection failure already shows up.
                self.conns.push(Conn {
                    id,
                    expanded: true,
                    profile: summary.id,
                    name: summary.name,
                    kind: summary.kind,
                    status: ConnStatus::Failed(err.user_message()),
                    capabilities: None,
                    access: Access::ReadOnly,
                    session: None,
                    tree: TreeState::new(),
                    view: Arc::new(TreeView::default()),
                });
                return;
            }
        };

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
            access: Access::ReadOnly,
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
        let orphaned: Vec<(BusyId, BusyOwner)> = self
            .busy
            .iter()
            .filter(|b| match &b.owner {
                BusyOwner::Connection(c) | BusyOwner::Node { conn: c, .. } => *c == id,
                BusyOwner::Preview { conn, .. } | BusyOwner::Definition { conn, .. } => *conn == id,
                BusyOwner::Query(query) => {
                    self.queries.iter().any(|q| q.id == *query && q.conn == id)
                }
                // Templates belong to the session, not to a connection: they
                // are the same file whichever database is open, and closing
                // one is no reason to abandon a save.
                BusyOwner::Templates => false,
            })
            .map(|b| (b.id, b.owner.clone()))
            .collect();
        for (busy, owner) in orphaned {
            self.drop_task(busy);
            // A query keeps its rows, but a query that had none yet has to be
            // told the reply is not coming: left `Loading` it spins for ever,
            // and a caller with no screen waiting for it to settle never
            // returns.
            if matches!(owner, BusyOwner::Query(_)) {
                self.abandon(&owner, DISCONNECTED);
            }
        }

        // Previews belong to a connection; leaving them behind would show
        // stale rows with no way to refresh them. A definition is worse:
        // nothing about it is ever re-fetched by scrolling, so one left behind
        // shows a dropped column until the process restarts.
        self.previews.retain(|p| p.conn != id);
        self.definitions.retain(|d| d.conn != id);
        // A query's rows do not: they are an answer that was given, and the
        // connection closing does not make it untrue. What it does make
        // impossible is running it again, which is a fact about the connection
        // and is already visible there.
    }

    fn open_node(&mut self, conn_id: ConnId, node: NodeRef, how: Open) {
        // The connection's own row. Its children arrived with `Connect`, so
        // this is opening and closing rather than fetching — and it works on a
        // connection that failed, which is the only way to get its error off
        // the screen without disconnecting.
        if node.path.is_empty() {
            if let Some(conn) = self.conn_mut(conn_id) {
                conn.expanded = match how {
                    Open::Toggle => !conn.expanded,
                    Open::ExpandOnly => true,
                };
            }
            return;
        }

        let Some(session) = self.session(conn_id) else {
            return;
        };
        let Some(conn) = self.conn_mut(conn_id) else {
            return;
        };

        // A front-end takes this from a row it drew, so it is always a node the
        // tree holds. One arriving over a socket need not be, and an unknown
        // node is worse than a wasted round trip: the reply has nowhere to land,
        // so all anyone sees is "expanding …" naming something that is not
        // there.
        if !conn.tree.contains(&node) {
            tracing::warn!(%node, "expand: no such node");
            return;
        }

        let outcome = match how {
            Open::Toggle => conn.tree.toggle(&node),
            Open::ExpandOnly => conn.tree.expand(&node),
        };
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
        // Not checked against the tree, unlike a node: a preview is its own
        // place to report a failure, so a relation that is not there comes back
        // as the database's own answer rather than as this store's guess from a
        // cache of whatever happens to have been expanded. An empty path is
        // refused because it names nothing to answer about.
        if table.path.is_empty() {
            tracing::warn!("preview: a relation with no name");
            return;
        }
        if self.session(conn_id).is_none() {
            return;
        }

        // Reuse what is already cached for this relation rather than fetching
        // it again every time a front-end asks.
        let page_size = self.page_size;
        if let Some(existing) = self.preview_mut(conn_id, &table) {
            // `LoadMore` refuses to extend rows that are not there, so
            // asking again is the only retry a failed preview has. The
            // ordering is kept: a front-end drawing the sort arrow should
            // not have it ignored.
            if existing.data.error().is_some() {
                let page = PageRequest::first_of(page_size).with_sort(existing.sort);
                existing.page = page;
                existing.next_offset = 0;
                existing.data = LoadState::Loading;
                existing.loaded_rows = 0;
                existing.exhausted = false;
                self.fetch_page(conn_id, table, page, false);
            }
            return;
        }

        let page = PageRequest::first_of(self.page_size);
        self.previews.push(Preview {
            conn: conn_id,
            table: table.clone(),
            sort: None,
            page,
            next_offset: 0,
            columns: None,
            pending: None,
            data: LoadState::Loading,
            loaded_rows: 0,
            last_error: None,
            exhausted: false,
            attempts: 0,
        });
        self.fetch_page(conn_id, table, page, false);
    }

    /// Fetch a definition, or leave the one already there alone.
    ///
    /// Cached hard: unlike a preview, nothing about a definition is re-fetched
    /// by using it, so asking again has to be asked for. `refresh` is that,
    /// and a failed one always retries — a definition stuck on a message is
    /// not an answer to keep.
    fn describe_table(&mut self, conn_id: ConnId, table: TableRef, refresh: bool) {
        let Some(session) = self.session(conn_id) else {
            return;
        };
        if table.path.is_empty() {
            tracing::warn!("describe: a relation with no name");
            return;
        }

        let existing = self
            .definitions
            .iter()
            .position(|d| d.conn == conn_id && d.table == table);
        if let Some(at) = existing {
            let held = &self.definitions[at];
            if !refresh && held.data.error().is_none() {
                return;
            }
            self.definitions[at].data = LoadState::Loading;
        } else {
            self.definitions.push(DefinitionView {
                conn: conn_id,
                table: table.clone(),
                data: LoadState::Loading,
            });
        }

        // A refresh supersedes whatever was in flight. Without this the older
        // reply can land last and overwrite the newer one — the definition
        // that was asked for again is the one that is thrown away — and its
        // busy row sits on screen for an answer nothing wants.
        let owner = BusyOwner::Definition {
            conn: conn_id,
            table: table.clone(),
        };
        let superseded: Vec<BusyId> = self
            .busy
            .iter()
            .filter(|b| b.owner == owner)
            .map(|b| b.id)
            .collect();
        for busy in superseded {
            self.drop_task(busy);
        }

        let busy = self.begin_busy(owner, format!("describing {table}"));
        let events = self.events.clone();
        let for_event = table.clone();
        self.spawn_task(busy, async move {
            let result = DescribeTable { session }
                .execute(DescribeTableInput { table })
                .await;
            let _ = events.send(Event::Described {
                conn: conn_id,
                table: for_event,
                busy,
                result,
            });
        });
    }

    fn described(
        &mut self,
        conn_id: ConnId,
        table: &TableRef,
        result: AppResult<sqlake_core::detail::TableDetail>,
    ) {
        let Some(definition) = self
            .definitions
            .iter_mut()
            .find(|d| d.conn == conn_id && &d.table == table)
        else {
            return;
        };
        definition.data = match result {
            Ok(detail) => LoadState::Ready(Arc::new(detail)),
            Err(err) => LoadState::Failed(err.user_message()),
        };
    }

    fn forget_definition(&mut self, conn_id: ConnId, table: &TableRef) {
        let owner = BusyOwner::Definition {
            conn: conn_id,
            table: table.clone(),
        };
        let running: Vec<BusyId> = self
            .busy
            .iter()
            .filter(|b| b.owner == owner)
            .map(|b| b.id)
            .collect();
        for busy in running {
            self.drop_task(busy);
        }
        self.definitions
            .retain(|d| !(d.conn == conn_id && &d.table == table));
    }

    fn sort_preview(&mut self, conn_id: ConnId, table: TableRef, column: usize) {
        // Without this, a preview whose connection has already died goes to
        // `Loading` below and stays there for ever: `fetch_page` returns
        // before sending anything, so no reply ever arrives to un-stick it.
        let Some(session) = self.session(conn_id) else {
            return;
        };
        // The driver would answer `Unsupported`, which reports as a failed
        // preview: the rows on screen replaced by an error, for a gesture the
        // front-end should not have offered. A front-end that reads
        // `Capabilities` never gets here; one that does not — an agent sending
        // actions straight in — is stopped here rather than at the driver.
        if !session.capabilities().sortable_preview {
            tracing::warn!(%table, "sort: this connection cannot order a preview");
            return;
        }
        let Some(preview) = self.preview_mut(conn_id, &table) else {
            return;
        };
        // A column index the view computed came from a grid it drew; one that
        // arrived over a socket did not. Unchecked it sticks to the preview,
        // and `preview_table`'s retry re-issues the same rejected ordering for
        // as long as the preview lives.
        //
        // Until a page has landed the width is unknown, and column 0 is then
        // the only index that cannot be wrong for a relation with any columns
        // at all. It is also the only one a front-end can reach, having no grid
        // to have selected a cell in — so treating the unknown width as 1
        // refuses nothing anybody can ask for, and still lets a preview whose
        // first page failed be sorted, which is where a retry has to keep the
        // ordering the header is showing.
        if column >= preview.columns.unwrap_or(1) {
            tracing::warn!(%table, column, "sort: no such column");
            return;
        }
        // The store owns the direction. Deriving it in the view would race
        // with a sort already in flight.
        let dir = match preview.sort {
            Some(s) if s.column == column => s.dir.toggled(),
            _ => SortDir::Asc,
        };
        let sort = Sort::new(column, dir);
        preview.sort = Some(sort);
        preview.data = LoadState::Loading;
        preview.loaded_rows = 0;
        preview.next_offset = 0;
        preview.exhausted = false;
        // A new ordering invalidates every page already fetched.
        let page = PageRequest::first_of(self.page_size).with_sort(Some(sort));
        self.fetch_page(conn_id, table, page, false);
    }

    fn load_more(&mut self, conn_id: ConnId, table: TableRef) {
        let Some(preview) = self.preview_mut(conn_id, &table) else {
            return;
        };
        // One page request per preview at a time. The old guard tested
        // `data`, which an append deliberately leaves `Ready` — so two quick
        // `LoadMore`s both went out, the first reply was dropped as stale,
        // and the rows it carried could never be asked for again.
        if preview.pending.is_some() {
            return;
        }
        // And there has to be something to extend. After a failed page
        // `page` still names the page that failed, so the next offset steps
        // over it — and `previewed` has nothing to append to, so it would
        // install that reply as the whole relation: the second page shown
        // as the first, with nothing to say the rows before it are missing.
        if preview.data.ready().is_none() {
            return;
        }
        // And nothing past the end. A front-end that reads `exhausted` never
        // asks; one that does not — an agent sending actions straight in — is
        // stopped here rather than charged a round trip for a page that is
        // known to be empty.
        if preview.exhausted {
            return;
        }
        let next = PageRequest {
            offset: preview.next_offset,
            ..preview.page
        };
        self.fetch_page(conn_id, table, next, true);
    }

    /// Start a run, under an id the caller chose.
    ///
    /// The budget comes from this store rather than from the caller: it is the
    /// user's own ceiling, and a front-end that could name its own would be a
    /// front-end that could raise it.
    fn run_query(
        &mut self,
        conn_id: ConnId,
        id: QueryId,
        sql: String,
        max_rows: Option<u32>,
        max_bytes: Option<u64>,
    ) {
        let Some(conn) = self.conns.iter().find(|c| c.id == conn_id) else {
            return;
        };
        let Some(session) = conn.session.clone() else {
            return;
        };
        // The connection's own, not the caller's: a front-end that could name
        // its own would be one that could grant itself write access.
        let access = conn.access;
        // An id already in use is a caller that lost track of one, and reusing
        // it would replace an answer somebody may still be reading.
        if self.queries.iter().any(|q| q.id == id) {
            tracing::warn!(query = %id.short(), "run: that query id is already in use");
            return;
        }

        self.queries.push(QueryView {
            id,
            conn: conn_id,
            sql: sql.clone(),
            estimate: None,
            needs_approval: None,
            data: LoadState::Loading,
            failed_at: None,
            started_at: Instant::now(),
            took: None,
        });
        self.record_start(id, conn_id, &sql);

        let busy = self.begin_busy(BusyOwner::Query(id), "running a query");
        let events = self.events.clone();
        // The tighter of the two. A caller can lower the session's ceiling and
        // never raise it — which is what makes an agent's budget a budget
        // rather than a suggestion.
        let budget = match (self.budget, max_bytes) {
            (Some(session), Some(asked)) => Some(session.min(asked)),
            (session, asked) => session.or(asked),
        };
        self.spawn_task(busy, async move {
            let result = RunQuery { session }
                .execute(RunQueryInput {
                    sql: RawSql::new(sql),
                    access,
                    max_rows,
                    budget,
                })
                .await;
            let _ = events.send(Event::Ran {
                query: id,
                busy,
                result,
            });
        });
    }

    /// Cost a statement and stop there.
    ///
    /// The query is recorded like any other, so the answer arrives where every
    /// other answer does — and `data` stays `Idle`, which is exactly true: no
    /// rows were requested.
    fn estimate_query(&mut self, conn_id: ConnId, id: QueryId, sql: String) {
        let Some(conn) = self.conns.iter().find(|c| c.id == conn_id) else {
            return;
        };
        let Some(session) = conn.session.clone() else {
            return;
        };
        let access = conn.access;
        if self.queries.iter().any(|q| q.id == id) {
            tracing::warn!(query = %id.short(), "estimate: that query id is already in use");
            return;
        }

        self.queries.push(QueryView {
            id,
            conn: conn_id,
            sql: sql.clone(),
            estimate: None,
            needs_approval: None,
            data: LoadState::Loading,
            failed_at: None,
            started_at: Instant::now(),
            took: None,
        });
        self.record_start(id, conn_id, &sql);

        let busy = self.begin_busy(BusyOwner::Query(id), "estimating a query");
        let events = self.events.clone();
        self.spawn_task(busy, async move {
            let result = EstimateQuery { session }
                .execute(EstimateQueryInput {
                    sql: RawSql::new(sql),
                    access,
                })
                .await;
            let _ = events.send(Event::Estimated {
                query: id,
                busy,
                result,
            });
        });
    }

    fn estimated(&mut self, id: QueryId, result: AppResult<Estimate>) {
        let Some(query) = self.queries.iter_mut().find(|q| q.id == id) else {
            return;
        };
        query.failed_at = None;
        match result {
            Ok(estimate) => {
                query.estimate = Some(estimate);
                // Idle rather than Ready: nothing was run, and there are no
                // rows to be had from this query without asking again.
                query.data = LoadState::Idle;
            }
            Err(err) => {
                query.failed_at = err.at();
                query.data = LoadState::Failed(err.user_message());
            }
        }
    }

    /// Run what a person has just said yes to.
    ///
    /// The statement comes out of the `OverBudget` the refusal left behind, so
    /// what runs is what was estimated — not whatever the buffer says now,
    /// which after an `$EDITOR` round trip need not be the same thing.
    fn approve_query(&mut self, id: QueryId) {
        let Some(conn_id) = self.queries.iter().find(|q| q.id == id).map(|q| q.conn) else {
            return;
        };
        // Before the question is taken and the state goes to `Loading`: a
        // connection that died while the dialog was open sends nothing, so no
        // reply ever arrives to un-stick it — and the answer that would have
        // run it would have been consumed on the way.
        let Some(session) = self.session(conn_id) else {
            return;
        };
        let Some(query) = self.queries.iter_mut().find(|q| q.id == id) else {
            return;
        };
        let Some(refused) = query.needs_approval.take() else {
            // Nothing is waiting on an answer. An approval arriving twice — a
            // double click, or an agent retrying — must not run it twice.
            return;
        };
        query.data = LoadState::Loading;
        // A second run, so a second row: the first is the one that was refused
        // and is still true. Its timing starts again here — a duration counted
        // from before the dialog would be however long somebody took to read
        // it.
        query.started_at = Instant::now();
        query.took = None;
        let sql = query.sql.clone();
        self.record_start(id, conn_id, &sql);

        let busy = self.begin_busy(BusyOwner::Query(id), "running an approved query");
        let events = self.events.clone();
        self.spawn_task(busy, async move {
            let refused = Arc::try_unwrap(refused).unwrap_or_else(|shared| (*shared).clone());
            let result = RunApproved { session }.execute(refused).await;
            let _ = events.send(Event::Ran {
                query: id,
                busy,
                result,
            });
        });
    }

    fn ran(&mut self, id: QueryId, result: AppResult<RunQueryOutput>) {
        let Some(query) = self.queries.iter_mut().find(|q| q.id == id) else {
            return;
        };
        query.failed_at = None;
        query.took = Some(query.started_at.elapsed());
        let duration_ms = u64::try_from(query.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        let outcome = match result {
            Ok(RunQueryOutput::Ran { estimate, result }) => {
                query.estimate = Some(estimate);
                let rows = result.rows.len();
                query.data = LoadState::Ready(Arc::new(PagedResult::new(&result)));
                RunOutcome::Ok {
                    duration_ms,
                    row_count: Some(rows as u64),
                    // What the statement was *estimated* to cost is not what it
                    // cost, and writing an estimate into a column called
                    // `bytes_processed` would be a number nobody could tell
                    // apart from a measurement.
                    bytes_processed: None,
                }
            }
            Ok(RunQueryOutput::NeedsApproval(over)) => {
                // The estimate and the ceiling it exceeded, in bytes. Read
                // back out of the history, "5.2 GB against a 1 GB budget" is
                // the whole of why the run did not happen.
                let message = format!(
                    "over the budget: {} against {} bytes",
                    match over.estimate {
                        Estimate::Bytes(bytes) => format!("{bytes} bytes"),
                        Estimate::Cost(cost) => format!("a plan cost of {cost}"),
                        Estimate::Unknown => "an unknown cost".to_owned(),
                    },
                    over.budget
                );
                query.estimate = Some(over.estimate);
                query.needs_approval = Some(Arc::new(*over));
                // Not `Loading`: nothing is on its way, and a spinner over a
                // question nobody has answered is the client waiting for
                // itself. `Idle` is "not requested", which is what this is
                // until somebody says yes.
                query.data = LoadState::Idle;
                // Nothing ran, and nothing went wrong. Saying so is what makes
                // "the expensive one I decided against" findable later —
                // approving it writes a second row, which is the run.
                RunOutcome::Refused {
                    duration_ms,
                    message,
                }
            }
            Err(err) => {
                query.failed_at = err.at();
                let message = err.user_message();
                query.data = LoadState::Failed(message.clone());
                RunOutcome::Failed {
                    duration_ms,
                    message,
                }
            }
        };
        self.record_end(id, outcome);
    }

    fn forget_query(&mut self, id: QueryId) {
        // Otherwise the busy row outlives its reader, the same way a forgotten
        // preview's would: "running a query" with a cancel button, for a result
        // nothing is going to show, until a reply nobody wants lands.
        let running: Vec<BusyId> = self
            .busy
            .iter()
            .filter(|b| matches!(b.owner, BusyOwner::Query(q) if q == id))
            .map(|b| b.id)
            .collect();
        for busy in running {
            self.drop_task(busy);
        }
        self.queries.retain(|q| q.id != id);
    }

    fn forget_preview(&mut self, conn_id: ConnId, table: &TableRef) {
        // Otherwise the busy row outlives its reader: "loading …" for
        // something nobody is looking at, until a reply nothing wants lands.
        if let Some(busy) = self
            .preview_mut(conn_id, table)
            .and_then(|p| p.pending.as_ref())
            .map(|p| p.busy)
        {
            self.drop_task(busy);
        }
        self.previews
            .retain(|p| !(p.conn == conn_id && &p.table == table));
    }

    fn fetch_page(&mut self, conn_id: ConnId, table: TableRef, page: PageRequest, append: bool) {
        let Some(session) = self.session(conn_id) else {
            return;
        };

        // A new request supersedes whatever was in flight — re-sorting while
        // a page is loading, for instance. Leaving the old task running
        // would hold a busy row on screen for a reply that is now discarded
        // as stale.
        if let Some(previous) = self
            .preview_mut(conn_id, &table)
            .and_then(|p| p.pending.take())
        {
            self.drop_task(previous.busy);
        }

        let busy = self.begin_busy(
            BusyOwner::Preview {
                conn: conn_id,
                table: table.clone(),
            },
            format!("loading {table}"),
        );
        if let Some(preview) = self.preview_mut(conn_id, &table) {
            preview.pending = Some(PendingPage { busy, page, append });
        }
        let events = self.events.clone();
        let event_table = table.clone();
        self.spawn_task(busy, async move {
            let result = PreviewTable { session }
                .execute(PreviewTableInput { table, page })
                .await;
            let _ = events.send(Event::Previewed {
                conn: conn_id,
                table: event_table,
                page,
                busy,
                result,
            });
        });
    }

    /// Aborting the task is also what stops the work at the server.
    ///
    /// The task holds the far end of the session actor's cancel channel, so
    /// dropping it closes that channel, the actor's `select` fires, and the
    /// driver's own future is unwound mid-call — which is where a driver that
    /// can reach its server sends the cancel. A driver whose
    /// [`Capabilities::cancel`] is false stops the client waiting and nothing
    /// more, which is what that flag says.
    ///
    /// [`Capabilities::cancel`]: sqlake_core::capability::Capabilities::cancel
    /// Write the row that says this statement was sent.
    ///
    /// Before it has an answer, so that the query somebody is waiting on — the
    /// one they are most likely to go looking for — is in the history while
    /// they wait. Nothing here is on the path of the query itself: the write
    /// is a blocking task of its own, and a library that refuses only costs a
    /// line in the log.
    fn record_start(&mut self, query: QueryId, conn: ConnId, sql: &str) {
        let (Some(library), Some(driver)) = (
            self.library.clone(),
            self.conns.iter().find(|c| c.id == conn).map(|c| c.kind),
        ) else {
            return;
        };
        self.runs.insert(query, Recording::Starting(None));

        let events = self.events.clone();
        let run = RunStart {
            connection: conn,
            driver,
            sql: sql.to_owned(),
            started_at: OffsetDateTime::now_utc(),
        };
        // Not a `spawn_task`: it has no busy row, because it is not something
        // anybody asked for or would cancel. The query it describes has one.
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || library.started(run))
                .await
                .unwrap_or_else(|why| Err(LibraryError::Failed(why.to_string())));
            let _ = events.send(Event::Recorded { query, result });
        });
    }

    fn recorded(&mut self, query: QueryId, result: LibraryResult<RunId>) {
        let row = match result {
            Ok(row) => row,
            Err(why) => {
                // Said once, in the log. A history that could not be written
                // is not a reason to interrupt somebody reading a result.
                tracing::warn!(query = %query.short(), "not recorded: {why}");
                self.runs.remove(&query);
                return;
            }
        };
        match self.runs.remove(&query) {
            // The query finished before the row existed. Now it does, so the
            // outcome that was waiting for it can be written.
            Some(Recording::Starting(Some(outcome))) => self.write_outcome(row, outcome),
            Some(Recording::Starting(None)) => {
                self.runs.insert(query, Recording::Row(row));
            }
            // Forgotten while the row was being written — the tab was closed.
            // The row stays, unsettled, which is what it is: a statement that
            // was sent and whose end nobody kept.
            Some(Recording::Row(_)) | None => {}
        }
    }

    /// Record how a run ended, or hold the answer until its row exists.
    fn record_end(&mut self, query: QueryId, outcome: RunOutcome) {
        match self.runs.remove(&query) {
            Some(Recording::Row(row)) => self.write_outcome(row, outcome),
            Some(Recording::Starting(_)) => {
                self.runs.insert(query, Recording::Starting(Some(outcome)));
            }
            None => {}
        }
    }

    fn write_outcome(&self, row: RunId, outcome: RunOutcome) {
        let Some(library) = self.library.clone() else {
            return;
        };
        tokio::spawn(async move {
            let settled = tokio::task::spawn_blocking(move || library.settled(row, outcome)).await;
            if let Ok(Err(why)) = settled {
                tracing::warn!(run = %row, "not settled: {why}");
            }
        });
    }

    /// How long a run took, from the moment the store dispatched it.
    ///
    /// Monotonic rather than wall clock: a clock that steps back over a
    /// running query would otherwise record a negative duration as a very
    /// large one.
    fn ran_for(&self, query: QueryId) -> u64 {
        self.queries.iter().find(|q| q.id == query).map_or(0, |q| {
            u64::try_from(q.started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
        })
    }

    /// Every library operation is a write, then a read, on one blocking task.
    ///
    /// The read happens even when nothing was written, and even when the write
    /// failed: the file is shared with whatever else is running against it, so
    /// the list a pane draws has to be the file's rather than this session's
    /// idea of what it did to it. Doing both inside one `spawn_blocking` is
    /// what keeps that to one trip.
    ///
    /// No use case for these. The layer above exists to make a skipped step a
    /// compile error — `RawSql` → `ValidatedSql` → `ApprovedQuery` — and there
    /// is no pipeline here to skip a step of.
    fn with_library(
        &mut self,
        label: Option<String>,
        write: impl FnOnce(&dyn Library) -> LibraryResult<()> + Send + 'static,
    ) {
        let Some(library) = self.library.clone() else {
            // Said once, in the place the list would be. A session with no
            // library is one where saving cannot work, and a pane that drew an
            // empty list would be claiming there is nothing saved.
            self.templates.data = LoadState::Failed(NOTHING_KEPT.to_owned());
            // And said again against the attempt, when there was one: somebody
            // who pressed save is owed an answer about the thing they pressed,
            // not only a note in the list they were not looking at.
            if label.is_some() {
                self.templates.failed = Some(NOTHING_KEPT.to_owned());
            }
            return;
        };
        self.templates.failed = None;
        if self.templates.data.ready().is_none() {
            self.templates.data = LoadState::Loading;
        }

        let busy = self.begin_busy(
            BusyOwner::Templates,
            label.unwrap_or_else(|| "reading templates".to_owned()),
        );
        let events = self.events.clone();
        // Cancelling this aborts the wait, not the write: a blocking task runs
        // to completion whatever happens to the future holding it, so a save
        // that was cancelled may still have landed. Which is why every
        // operation re-reads — the next one puts the list back in step with
        // the file, and the file is the one that was right all along.
        self.spawn_task(busy, async move {
            let answer = tokio::task::spawn_blocking(move || {
                let wrote = write(library.as_ref()).map_err(|why| why.to_string());
                (wrote, library.templates())
            })
            .await;
            let (wrote, result) = match answer {
                Ok((wrote, read)) => (wrote, read.map_err(AppError::from)),
                // The blocking pool dropped the task, which here means the
                // runtime is going down. Nothing else knows that, so it is
                // reported rather than left as a spinner.
                Err(why) => (
                    Err(why.to_string()),
                    Err(AppError::Refused(why.to_string())),
                ),
            };
            let _ = events.send(Event::Library {
                busy,
                wrote,
                result,
            });
        });
    }

    fn library_answered(
        &mut self,
        busy: BusyId,
        wrote: Result<(), String>,
        result: AppResult<Vec<Template>>,
    ) {
        self.end_busy(busy);
        self.templates.failed = wrote.err();
        self.templates.data = match result {
            Ok(templates) => LoadState::Ready(Arc::new(templates)),
            Err(why) => LoadState::Failed(why.user_message()),
        };
    }

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
            BusyOwner::Templates => {
                self.templates.failed = Some(reason.to_owned());
                // A list that was on screen stays on screen: a cancelled read
                // did not make it wrong. One that never arrived has to say
                // something, or the pane spins for an answer nobody is
                // bringing.
                if self.templates.data.is_loading() {
                    self.templates.data = LoadState::Failed(reason.to_owned());
                }
            }
            BusyOwner::Query(id) => {
                let duration_ms = self.ran_for(*id);
                if let Some(query) = self.queries.iter_mut().find(|q| q.id == *id) {
                    query.data = LoadState::Failed(reason.to_owned());
                    query.took = Some(query.started_at.elapsed());
                }
                // Stopped on purpose is not the same as gone wrong, and a
                // history that files the first under the second reports
                // somebody's own decisions as failures.
                self.record_end(
                    *id,
                    if reason == CANCELLED {
                        RunOutcome::Cancelled { duration_ms }
                    } else {
                        RunOutcome::Failed {
                            duration_ms,
                            message: reason.to_owned(),
                        }
                    },
                );
            }
            BusyOwner::Definition { conn, table } => {
                if let Some(definition) = self
                    .definitions
                    .iter_mut()
                    .find(|d| d.conn == *conn && &d.table == table)
                {
                    definition.data = LoadState::Failed(reason.to_owned());
                }
            }
            BusyOwner::Preview { conn, table } => {
                if let Some(preview) = self.preview_mut(*conn, table) {
                    preview.pending = None;
                    // A cancellation is a request that finished. Left
                    // uncounted, it is indistinguishable from the end of the
                    // relation and the preview never pages again.
                    preview.attempts = preview.attempts.saturating_add(1);
                    // An append that never lands leaves the rows already on
                    // screen perfectly usable.
                    if preview.data.ready().is_none() {
                        preview.data = LoadState::Failed(reason.to_owned());
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
            Event::Ran {
                query,
                busy,
                result,
            } => {
                self.end_busy(busy);
                self.ran(query, result);
            }
            Event::Estimated {
                query,
                busy,
                result,
            } => {
                self.end_busy(busy);
                self.estimated(query, result);
            }
            Event::Described {
                conn,
                table,
                busy,
                result,
            } => {
                self.end_busy(busy);
                self.described(conn, &table, result);
            }
            Event::Previewed {
                conn,
                table,
                page,
                busy,
                result,
            } => {
                self.end_busy(busy);
                self.previewed(conn, table, page, result);
            }
            Event::Library {
                busy,
                wrote,
                result,
            } => self.library_answered(busy, wrote, result),
            Event::Recorded { query, result } => self.recorded(query, result),
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
                    conn.access = out.access;
                    conn.session = Some(out.session);
                    conn.tree.set_roots(out.roots);
                    conn.view = Arc::new(conn.tree.flatten(conn.id));
                }
                _ => out.session.close(),
            },
            Err(err) => {
                // Recorded on the connection and nowhere else. How a failure
                // is surfaced is the front-end's decision.
                if let Some(conn) = self.conn_mut(id) {
                    conn.status = ConnStatus::Failed(err.user_message());
                }
            }
        }
    }

    fn expanded(&mut self, id: ConnId, node: NodeRef, result: AppResult<ExpandNodeOutput>) {
        let session_died = matches!(result, Err(AppError::SessionClosed));
        // The failure is shown on the node itself.
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

    /// Whether this reply appends or replaces comes from the preview's own
    /// record of the request, not from the reply. The two can only disagree
    /// when something has already gone wrong.
    fn previewed(
        &mut self,
        conn_id: ConnId,
        table: TableRef,
        page: PageRequest,
        result: AppResult<PreviewTableOutput>,
    ) {
        let session_died = matches!(result, Err(AppError::SessionClosed));
        let mut conn_died = None;

        if let Some(preview) = self.preview_mut(conn_id, &table) {
            // A reply for a page the preview is no longer waiting for must
            // not overwrite what is on screen. The preview's own record is
            // the authority, not the flag that travelled with the request.
            let Some(pending) = preview.pending.take_if(|p| p.page == page) else {
                return;
            };

            preview.attempts = preview.attempts.saturating_add(1);
            match result {
                Ok(out) => {
                    let rows = if pending.append {
                        match preview.data.ready() {
                            Some(existing) => match existing.append(&out.result) {
                                Some(merged) => {
                                    // A successful append supersedes whatever
                                    // the previous attempt left behind.
                                    preview.last_error = None;
                                    merged
                                }
                                None => {
                                    preview.last_error = Some(format!(
                                        "{table} changed shape; showing the new page only"
                                    ));
                                    PagedResult::new(&out.result)
                                }
                            },
                            None => {
                                preview.last_error = None;
                                PagedResult::new(&out.result)
                            }
                        }
                    } else {
                        preview.last_error = None;
                        PagedResult::new(&out.result)
                    };
                    // A short page is the end of the relation, unless the
                    // driver is allowed to cut one — and BigQuery is:
                    // `tabledata.list` caps a response at 10 MB and answers
                    // with fewer rows than `maxResults` while more are still
                    // there. On the shortness alone a wide table would be
                    // stranded on its first screenful for the life of the tab.
                    // So where the driver knows the total that is the
                    // authority, and where it does not the limit is all there
                    // is to go on.
                    let reached = page
                        .offset
                        .saturating_add(u64::try_from(out.result.rows.len()).unwrap_or(u64::MAX));
                    preview.exhausted = out.result.rows.len() < page.limit as usize
                        && out.result.total_rows.is_none_or(|total| reached >= total);
                    preview.page = page;
                    preview.next_offset = reached;
                    let rows = Arc::new(rows);
                    preview.columns = Some(rows.columns().len());
                    preview.loaded_rows = rows.row_count();
                    preview.data = LoadState::Ready(rows);
                }
                Err(err) => {
                    let message = err.user_message();
                    if pending.append && preview.data.ready().is_some() {
                        // A failed "load more" has not invalidated the rows
                        // already fetched. Replacing them with an error panel
                        // loses them *and* leaves the next request starting
                        // from the wrong offset, because `page` never advanced.
                        preview.last_error = Some(message);
                    } else {
                        preview.data = LoadState::Failed(message);
                        preview.loaded_rows = 0;
                        preview.last_error = None;
                    }
                    if session_died {
                        conn_died = Some(conn_id);
                    }
                }
            }
        }

        if let Some(conn) = conn_died {
            self.session_died(conn);
        }
    }

    /// The session actor is gone, so everything under this connection will
    /// fail the same way.
    ///
    /// Reporting it only on the node or preview that happened to ask leaves
    /// the connection reading `Ready` while every click on it fails, which
    /// tells the user nothing about needing to reconnect.
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
            applied: self.applied,
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
                    tree: Arc::clone(&c.view),
                })
                .collect(),
            explorer: Arc::new(self.explorer()),
            previews: self
                .previews
                .iter()
                .map(|p| PreviewView {
                    conn: p.conn,
                    table: p.table.clone(),
                    sort: p.sort,
                    loaded_rows: p.loaded_rows,
                    exhausted: p.exhausted,
                    attempts: p.attempts,
                    data: p.data.clone(),
                    last_error: p.last_error.clone(),
                })
                .collect(),
            // Cloned wholesale: a `QueryView` is already the shape a front-end
            // wants, and the rows behind it are an `Arc`.
            definitions: self.definitions.clone(),
            queries: self.queries.clone(),
            busy: self.busy.clone(),
            templates: self.templates.clone(),
            should_quit: self.should_quit,
        }
    }
}

#[cfg(test)]
mod tests {
    use sqlake_core::node::NodeKind;
    use std::time::Duration;

    use sqlake_core::sql::Estimate;
    use sqlake_core::value::Value;
    use sqlake_driver_mock::{Behaviour, MockDriver, MockProfiles, NO_SORT};

    use super::*;
    use crate::tree::NodeState;

    fn store(behaviour: Behaviour) -> Store {
        store_paging(behaviour, PageRequest::DEFAULT_LIMIT)
    }

    fn store_paging(behaviour: Behaviour, page_size: u32) -> Store {
        store_of(MockDriver::new(behaviour), page_size)
    }

    /// A store with a byte ceiling, and a driver that answers a number to
    /// compare against it.
    fn store_with_budget(behaviour: Behaviour, budget: Option<u64>) -> Store {
        Store::spawn(
            Wiring::new(
                Drivers::new().with(Arc::new(
                    MockDriver::new(behaviour).with_capabilities(sqlake_driver_mock::ESTIMATES),
                )),
                Arc::new(MockProfiles::default()),
            )
            .budget(budget),
        )
    }

    fn store_of(driver: MockDriver, page_size: u32) -> Store {
        Store::spawn(
            Wiring::new(
                Drivers::new().with(Arc::new(driver)),
                Arc::new(MockProfiles::default()),
            )
            .page_size(page_size),
        )
    }

    /// The relation every definition test asks about.
    fn users() -> TableRef {
        TableRef::new(["public", "users"])
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

    /// The same wait a headless caller makes, with a timeout long enough that
    /// only a real hang trips it.
    async fn until(store: &Store, done: impl Fn(&Snapshot) -> bool) -> Arc<Snapshot> {
        store
            .settle(std::time::Duration::from_secs(5), done)
            .await
            .expect("the condition never held")
    }

    async fn connected_store() -> (Store, ConnId) {
        connected(store(Behaviour::instant())).await
    }

    async fn connected(store: Store) -> (Store, ConnId) {
        let conn = ConnId::new();
        settled(
            &store,
            Action::Connect {
                profile: pid("mock"),
                conn,
            },
            move |s| s.connection_settled(conn),
        )
        .await;
        (store, conn)
    }

    /// Dispatch and wait, with a timeout long enough that only a real hang
    /// trips it. `settled` rather than `until` wherever the condition is one
    /// the store could also have satisfied before the action.
    async fn settled(
        store: &Store,
        action: Action,
        done: impl Fn(&Snapshot) -> bool,
    ) -> Arc<Snapshot> {
        store
            .dispatch_and_settle(action, std::time::Duration::from_secs(5), done)
            .await
            .expect("the condition never held")
    }

    /// A store with a library in memory — the real one, with no file.
    fn store_keeping() -> Store {
        Store::spawn(
            Wiring::new(
                Drivers::new().with(Arc::new(MockDriver::new(Behaviour::instant()))),
                Arc::new(MockProfiles::default()),
            )
            .library(Arc::new(
                sqlake_library::Sqlite::in_memory().expect("a library opens"),
            )),
        )
    }

    fn new_template(name: &str) -> sqlake_core::library::NewTemplate {
        sqlake_core::library::NewTemplate {
            name: name.to_owned(),
            body: "select * from {{ident:table}}".to_owned(),
            driver: None,
            tags: Vec::new(),
        }
    }

    async fn library_settled(store: &Store, action: Action) -> Arc<Snapshot> {
        settled(store, action, Snapshot::templates_settled).await
    }

    fn saved(snap: &Snapshot) -> Vec<String> {
        snap.templates
            .data
            .ready()
            .expect("a list")
            .iter()
            .map(|t| t.name.clone())
            .collect()
    }

    #[tokio::test]
    async fn a_saved_template_is_in_the_next_snapshot() {
        // Saved and listed in one trip to the file: the write is followed by a
        // read, so a front-end never has to guess what the file now holds.
        let store = store_keeping();
        let snap = library_settled(&store, Action::SaveTemplate(new_template("daily"))).await;
        assert_eq!(saved(&snap), ["daily"]);
        assert_eq!(snap.templates.failed, None);
    }

    #[tokio::test]
    async fn a_name_already_taken_is_reported_without_losing_the_list() {
        let store = store_keeping();
        library_settled(&store, Action::SaveTemplate(new_template("daily"))).await;
        let snap = library_settled(&store, Action::SaveTemplate(new_template("daily"))).await;

        let why = snap.templates.failed.as_deref().expect("a reason");
        assert!(why.contains("daily"), "{why}");
        assert_eq!(
            saved(&snap),
            ["daily"],
            "a refused save should not have disturbed what is there"
        );
    }

    #[tokio::test]
    async fn the_reason_a_write_failed_does_not_outlive_the_next_one() {
        // It is about the attempt somebody just made, not about the session.
        let store = store_keeping();
        library_settled(&store, Action::SaveTemplate(new_template("daily"))).await;
        library_settled(&store, Action::SaveTemplate(new_template("daily"))).await;
        let snap = library_settled(&store, Action::SaveTemplate(new_template("weekly"))).await;
        assert_eq!(snap.templates.failed, None);
        assert_eq!(saved(&snap).len(), 2);
    }

    #[tokio::test]
    async fn editing_and_deleting_go_through_the_file() {
        let store = store_keeping();
        let snap = library_settled(&store, Action::SaveTemplate(new_template("first"))).await;
        let id = snap.templates.data.ready().expect("a list")[0].id;

        let snap = library_settled(
            &store,
            Action::ReplaceTemplate {
                id,
                with: new_template("second"),
            },
        )
        .await;
        assert_eq!(saved(&snap), ["second"]);

        let snap = library_settled(&store, Action::DeleteTemplate(id)).await;
        assert!(saved(&snap).is_empty());
    }

    #[tokio::test]
    async fn a_session_keeping_nothing_says_so_rather_than_showing_an_empty_list() {
        // Nothing was saved *and* nothing can be, which a pane drawing no rows
        // would report as "you have no templates".
        let store = store(Behaviour::instant());
        let snap = library_settled(&store, Action::LoadTemplates).await;
        assert_eq!(snap.templates.data.error(), Some(NOTHING_KEPT));
        assert!(snap.templates.data.ready().is_none());
    }

    #[tokio::test]
    async fn saving_with_nothing_to_save_into_answers_the_attempt() {
        // Not only the list: somebody who pressed save is owed an answer about
        // what they pressed.
        let store = store(Behaviour::instant());
        let snap = library_settled(&store, Action::SaveTemplate(new_template("nowhere"))).await;
        assert_eq!(snap.templates.failed.as_deref(), Some(NOTHING_KEPT));
        assert_eq!(snap.templates.data.error(), Some(NOTHING_KEPT));
    }

    #[tokio::test]
    async fn closing_a_connection_leaves_the_templates_alone() {
        // They belong to the session and not to a database: the same file
        // whichever connection is open.
        let (store, conn) = connected(store_keeping()).await;
        let snap = library_settled(&store, Action::SaveTemplate(new_template("kept"))).await;
        assert_eq!(saved(&snap), ["kept"]);

        let snap = settled(&store, Action::Disconnect(conn), |s| {
            s.connections.iter().all(|c| !c.is_live())
        })
        .await;
        assert_eq!(saved(&snap), ["kept"]);
    }

    /// A store with a library this test can read back, and the handle to it.
    fn store_recording() -> (Store, Arc<sqlake_library::Sqlite>) {
        let library = Arc::new(sqlake_library::Sqlite::in_memory().expect("a library opens"));
        let store = Store::spawn(
            Wiring::new(
                Drivers::new().with(Arc::new(MockDriver::new(Behaviour::instant()))),
                Arc::new(MockProfiles::default()),
            )
            .library(Arc::clone(&library) as Arc<dyn Library>),
        );
        (store, library)
    }

    /// The history, once the writes behind it have landed.
    ///
    /// Recording is deliberately off the path of the query — it is a task with
    /// no busy row, because nobody asked for it and nobody would cancel it —
    /// so a settled snapshot does not mean the row is written yet.
    async fn history(
        library: &sqlake_library::Sqlite,
        done: impl Fn(&[sqlake_core::library::HistoryEntry]) -> bool,
    ) -> Vec<sqlake_core::library::HistoryEntry> {
        for _ in 0..200 {
            let held = library.history(10).expect("it reads");
            if done(&held) {
                return held;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the history never said what was expected");
    }

    #[tokio::test]
    async fn a_run_leaves_a_row_saying_what_it_did() {
        let (store, library) = store_recording();
        let (store, conn) = connected(store).await;
        let id = QueryId::new();
        settled(
            &store,
            Action::RunQuery {
                conn,
                query: id,
                sql: "select * from public.users".to_owned(),
                max_rows: None,
                max_bytes: None,
            },
            |s| s.query(id).is_some_and(QueryView::is_settled),
        )
        .await;

        let held = history(&library, |h| h.first().is_some_and(|e| e.status.is_some())).await;
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].sql, "select * from public.users");
        assert_eq!(held[0].status.as_deref(), Some("ok"));
        assert!(held[0].row_count.is_some_and(|rows| rows > 0));
        assert_eq!(held[0].driver, Some(DriverKind::Mock));
    }

    #[tokio::test]
    async fn a_statement_that_failed_is_recorded_as_one() {
        // The failures are half of what a history is for: in a personal tool
        // they are the part somebody goes back to.
        let (library, store) = {
            let library = Arc::new(sqlake_library::Sqlite::in_memory().expect("a library opens"));
            let store = Store::spawn(
                Wiring::new(
                    Drivers::new().with(Arc::new(MockDriver::new(Behaviour {
                        failing_sql: vec!["boom".to_owned()],
                        ..Behaviour::instant()
                    }))),
                    Arc::new(MockProfiles::default()),
                )
                .library(Arc::clone(&library) as Arc<dyn Library>),
            );
            (library, store)
        };
        let (store, conn) = connected(store).await;
        let id = QueryId::new();
        settled(
            &store,
            Action::RunQuery {
                conn,
                query: id,
                sql: "select boom".to_owned(),
                max_rows: None,
                max_bytes: None,
            },
            |s| s.query(id).is_some_and(QueryView::is_settled),
        )
        .await;

        let held = history(&library, |h| h.first().is_some_and(|e| e.status.is_some())).await;
        assert_eq!(held[0].status.as_deref(), Some("error"));
        assert!(held[0].error.is_some(), "{:?}", held[0]);
    }

    #[tokio::test]
    async fn a_query_over_the_budget_is_recorded_as_refused_and_the_run_after_it_as_a_run() {
        // Two rows for one statement, which is what happened: one attempt that
        // was costed and stopped, and one that ran.
        let library = Arc::new(sqlake_library::Sqlite::in_memory().expect("a library opens"));
        let store = Store::spawn(
            Wiring::new(
                Drivers::new().with(Arc::new(
                    MockDriver::new(Behaviour {
                        estimate_bytes: 5_000,
                        ..Behaviour::instant()
                    })
                    .with_capabilities(sqlake_driver_mock::ESTIMATES),
                )),
                Arc::new(MockProfiles::default()),
            )
            .budget(Some(10))
            .library(Arc::clone(&library) as Arc<dyn Library>),
        );
        let (store, conn) = connected(store).await;
        let id = QueryId::new();
        let snap = settled(
            &store,
            Action::RunQuery {
                conn,
                query: id,
                sql: "select * from public.users".to_owned(),
                max_rows: None,
                max_bytes: None,
            },
            |s| s.query(id).is_some_and(|q| q.needs_approval.is_some()),
        )
        .await;
        assert!(snap.query(id).expect("a query").needs_approval.is_some());

        let refused = history(&library, |h| h.first().is_some_and(|e| e.status.is_some())).await;
        assert_eq!(refused[0].status.as_deref(), Some("refused"));
        assert!(
            refused[0]
                .error
                .as_deref()
                .is_some_and(|why| why.contains("5000")),
            "{:?}",
            refused[0]
        );

        settled(&store, Action::ApproveQuery(id), |s| {
            s.query(id).is_some_and(QueryView::is_settled)
        })
        .await;
        let both = history(&library, |h| h.len() == 2 && h[0].status.is_some()).await;
        assert_eq!(both[0].status.as_deref(), Some("ok"));
        assert_eq!(both[1].status.as_deref(), Some("refused"));
    }

    #[tokio::test]
    async fn a_query_somebody_stopped_is_not_filed_under_failures() {
        // Cancelling is a decision, not a fault. A history that reports one as
        // the other is a history that says the user's own choices went wrong.
        let library = Arc::new(sqlake_library::Sqlite::in_memory().expect("a library opens"));
        let store = Store::spawn(
            Wiring::new(
                Drivers::new().with(Arc::new(MockDriver::new(Behaviour {
                    query_latency: Duration::from_secs(30),
                    ..Behaviour::instant()
                }))),
                Arc::new(MockProfiles::default()),
            )
            .library(Arc::clone(&library) as Arc<dyn Library>),
        );
        let (store, conn) = connected(store).await;

        let id = QueryId::new();
        store.dispatch(Action::RunQuery {
            conn,
            query: id,
            sql: "select * from public.users".to_owned(),
            max_rows: None,
            max_bytes: None,
        });
        let snap = until(&store, |s| {
            s.busy
                .iter()
                .any(|b| matches!(b.owner, BusyOwner::Query(q) if q == id))
        })
        .await;
        let busy = snap
            .busy
            .iter()
            .find(|b| matches!(b.owner, BusyOwner::Query(q) if q == id))
            .expect("the query's own row")
            .id;

        store.dispatch(Action::Cancel(busy));
        until(&store, |s| s.query(id).is_some_and(QueryView::is_settled)).await;

        let held = history(&library, |h| h.first().is_some_and(|e| e.status.is_some())).await;
        assert_eq!(held[0].status.as_deref(), Some("cancelled"));
        assert_eq!(held[0].error, None, "nothing went wrong");
    }

    #[tokio::test]
    async fn a_session_keeping_nothing_still_runs_queries() {
        // The library is where history goes, not something a query needs.
        let (store, conn) = connected_store().await;
        let id = QueryId::new();
        let snap = settled(
            &store,
            Action::RunQuery {
                conn,
                query: id,
                sql: "select * from public.users".to_owned(),
                max_rows: None,
                max_bytes: None,
            },
            |s| s.query(id).is_some_and(QueryView::is_settled),
        )
        .await;
        assert!(snap.query(id).expect("a query").data.ready().is_some());
    }

    fn preview_of<'a>(snap: &'a Snapshot, conn: ConnId, table: &TableRef) -> &'a PreviewView {
        snap.preview(conn, table).expect("a preview")
    }

    #[tokio::test]
    async fn cancelling_a_query_unwinds_the_driver_rather_than_only_the_wait() {
        // The point of the whole mechanism: the session actor runs the query,
        // and a cancel sent down its own channel would queue behind the thing
        // it is meant to stop. Dropping the caller's task closes a channel the
        // actor is selecting on instead.
        let driver = Arc::new(MockDriver::new(Behaviour {
            query_latency: Duration::from_secs(30),
            ..Behaviour::instant()
        }));
        let store = Store::spawn(Wiring::new(
            Drivers::new().with(Arc::clone(&driver) as Arc<dyn Driver>),
            Arc::new(MockProfiles::default()),
        ));
        let (store, conn) = connected(store).await;

        let id = QueryId::new();
        let snap = settled(
            &store,
            Action::RunQuery {
                conn,
                query: id,
                sql: "select * from public.users".to_owned(),
                max_rows: None,
                max_bytes: None,
            },
            |s| !s.busy.is_empty(),
        )
        .await;
        let busy = snap.busy[0].id;

        let snap = settled(&store, Action::Cancel(busy), move |s| {
            s.query(id).is_some_and(QueryView::is_settled)
        })
        .await;
        assert!(snap.busy.is_empty(), "the spinner outlived the cancel");
        assert_eq!(
            snap.query(id).unwrap().data.error(),
            Some(CANCELLED),
            "the query has to be left in a state somebody can act on"
        );

        // The driver was unwound, not merely abandoned. Without this the test
        // would pass against a store that stopped listening while the query
        // ran on.
        for _ in 0..50 {
            if driver.cancelled() == 1 {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("the driver was never told: {} unwound", driver.cancelled());
    }

    #[tokio::test]
    async fn a_connection_answers_again_after_a_query_is_cancelled() {
        // The actor is the one thing every call on a connection goes through,
        // so a cancel that left it awaiting the abandoned query would take the
        // whole connection with it.
        let store = store(Behaviour {
            query_latency: Duration::from_millis(400),
            ..Behaviour::instant()
        });
        let (store, conn) = connected(store).await;
        let id = QueryId::new();
        let snap = settled(
            &store,
            Action::RunQuery {
                conn,
                query: id,
                sql: "select * from public.users".to_owned(),
                max_rows: None,
                max_bytes: None,
            },
            |s| !s.busy.is_empty(),
        )
        .await;
        store.dispatch(Action::Cancel(snap.busy[0].id));

        let second = QueryId::new();
        let snap = settled(
            &store,
            Action::RunQuery {
                conn,
                query: second,
                sql: "select * from public.users".to_owned(),
                max_rows: None,
                max_bytes: None,
            },
            move |s| s.query(second).is_some_and(QueryView::is_settled),
        )
        .await;
        assert!(snap.query(second).unwrap().data.ready().is_some());
    }

    #[tokio::test]
    async fn a_read_only_connection_refuses_a_write() {
        // BigQuery has no `default_transaction_read_only`, so for it this is
        // the only defence there is — which is why it lives here rather than
        // being left to the server.
        let store = Store::spawn(Wiring::new(
            Drivers::new().with(Arc::new(MockDriver::new(Behaviour::instant()))),
            Arc::new(MockProfiles::read_only()),
        ));
        let (store, conn) = connected(store).await;
        let id = QueryId::new();
        let snap = settled(
            &store,
            Action::RunQuery {
                conn,
                query: id,
                sql: "delete from public.users".to_owned(),
                max_rows: None,
                max_bytes: None,
            },
            move |s| s.query(id).is_some_and(QueryView::is_settled),
        )
        .await;

        let query = snap.query(id).unwrap();
        let why = query.data.error().expect("a refusal");
        assert!(why.contains("read-only"), "{why}");
        // Not something to approve: no answer a person could give would make
        // it run.
        assert!(query.needs_approval.is_none());
    }

    #[tokio::test]
    async fn a_read_only_connection_still_reads() {
        let store = Store::spawn(Wiring::new(
            Drivers::new().with(Arc::new(MockDriver::new(Behaviour::instant()))),
            Arc::new(MockProfiles::read_only()),
        ));
        let (store, conn) = connected(store).await;
        let id = QueryId::new();
        let snap = settled(
            &store,
            Action::RunQuery {
                conn,
                query: id,
                sql: "select * from public.users".to_owned(),
                max_rows: None,
                max_bytes: None,
            },
            move |s| s.query(id).is_some_and(QueryView::is_settled),
        )
        .await;
        assert!(snap.query(id).unwrap().data.ready().is_some());
    }

    #[tokio::test]
    async fn a_relation_is_described_once_and_kept() {
        let (store, conn) = connected_store().await;
        let table = users();
        let describe = |refresh| Action::DescribeTable {
            conn,
            table: table.clone(),
            refresh,
        };
        let snap = settled(&store, describe(false), {
            let table = table.clone();
            move |s| {
                s.definition(conn, &table)
                    .is_some_and(|d| d.data.ready().is_some())
            }
        })
        .await;
        let definition = snap.definition(conn, &table).unwrap();
        assert!(definition.data.ready().is_some());
        let applied = snap.applied;

        // Asked again without `refresh`, nothing is fetched: a definition is
        // not re-read by using it, so a second ask is answered from what is
        // already there.
        let snap = settled(&store, describe(false), move |s| s.applied > applied).await;
        assert!(snap.busy.is_empty(), "it asked the server again");
        assert_eq!(snap.definitions.len(), 1);
    }

    #[tokio::test]
    async fn refreshing_asks_again() {
        // The only way out of a stale definition short of closing the
        // connection, which is why it needs a gesture of its own.
        //
        // Slow on purpose: against an instant driver the busy row appears and
        // is gone inside one snapshot, so "it asked" would be a race rather
        // than an assertion.
        let (store, conn) = connected(store(Behaviour {
            latency: Duration::from_millis(300),
            ..Behaviour::instant()
        }))
        .await;
        let table = users();
        let ready = {
            let table = table.clone();
            move |s: &Snapshot| {
                s.definition(conn, &table)
                    .is_some_and(|d| d.data.ready().is_some())
            }
        };
        let _ = settled(
            &store,
            Action::DescribeTable {
                conn,
                table: table.clone(),
                refresh: false,
            },
            ready.clone(),
        )
        .await;
        let snap = settled(
            &store,
            Action::DescribeTable {
                conn,
                table: table.clone(),
                refresh: true,
            },
            |s| !s.busy.is_empty(),
        )
        .await;
        assert!(
            snap.busy
                .iter()
                .any(|b| matches!(b.owner, BusyOwner::Definition { .. })),
            "refreshing should have asked"
        );
    }

    #[tokio::test]
    async fn a_definition_that_failed_retries_without_being_asked_twice() {
        // A definition stuck on a message is not an answer worth keeping, so
        // reopening the tab is enough — unlike a successful one, which is.
        let (store, conn) = connected(store(Behaviour {
            failing_nodes: vec![vec!["public".to_owned(), "users".to_owned()]],
            latency: Duration::from_millis(300),
            ..Behaviour::instant()
        }))
        .await;
        let table = users();
        let describe = Action::DescribeTable {
            conn,
            table: table.clone(),
            refresh: false,
        };
        let settled_once = {
            let table = table.clone();
            move |s: &Snapshot| {
                s.definition(conn, &table)
                    .is_some_and(|d| d.data.error().is_some())
            }
        };
        let _ = settled(&store, describe.clone(), settled_once.clone()).await;
        let snap = settled(&store, describe, |s| !s.busy.is_empty()).await;
        assert!(
            snap.busy
                .iter()
                .any(|b| matches!(b.owner, BusyOwner::Definition { .. })),
            "a failed definition should be asked about again"
        );
    }

    #[tokio::test]
    async fn a_closed_connection_takes_its_definitions_with_it() {
        // Worse than a stale preview: nothing about a definition is re-fetched
        // by using it, so one left behind shows a dropped column until the
        // process restarts.
        let (store, conn) = connected_store().await;
        let table = users();
        let _ = settled(
            &store,
            Action::DescribeTable {
                conn,
                table: table.clone(),
                refresh: false,
            },
            {
                let table = table.clone();
                move |s| {
                    s.definition(conn, &table)
                        .is_some_and(|d| d.data.ready().is_some())
                }
            },
        )
        .await;
        let snap = settled(&store, Action::Disconnect(conn), move |s| {
            s.connection(conn)
                .is_some_and(|c| c.status == ConnStatus::Closed)
        })
        .await;
        assert!(snap.definitions.is_empty());
    }

    #[tokio::test]
    async fn a_query_runs_and_its_rows_reach_the_snapshot() {
        let (store, conn) = connected_store().await;
        let id = QueryId::new();
        let snap = settled(
            &store,
            Action::RunQuery {
                conn,
                query: id,
                sql: "select * from public.users".to_owned(),
                max_rows: None,
                max_bytes: None,
            },
            move |s| s.query(id).is_some_and(QueryView::is_settled),
        )
        .await;

        let query = snap.query(id).expect("the query");
        let rows = query.data.ready().expect("rows");
        assert!(rows.row_count() > 0 && !rows.columns().is_empty());
        assert_eq!(query.sql, "select * from public.users");
        assert!(query.needs_approval.is_none());
    }

    #[tokio::test]
    async fn two_runs_of_the_same_sql_are_two_answers() {
        // Which is why a query is keyed by an id the caller chose: there is no
        // name to look one up by.
        let (store, conn) = connected_store().await;
        let (a, b) = (QueryId::new(), QueryId::new());
        for id in [a, b] {
            store.dispatch(Action::RunQuery {
                conn,
                query: id,
                sql: "select * from public.users".to_owned(),
                max_rows: None,
                max_bytes: None,
            });
        }
        let snap = until(&store, |s| {
            [a, b]
                .iter()
                .all(|id| s.query(*id).is_some_and(QueryView::is_settled))
        })
        .await;
        assert_eq!(snap.queries.len(), 2);
    }

    #[tokio::test]
    async fn an_id_already_in_use_starts_nothing() {
        // Reusing one would replace an answer somebody may still be reading.
        let (store, conn) = connected_store().await;
        let id = QueryId::new();
        let run = |sql: &str| Action::RunQuery {
            conn,
            query: id,
            sql: sql.to_owned(),
            max_rows: None,
            max_bytes: None,
        };
        let _ = settled(&store, run("select * from public.users"), move |s| {
            s.query(id).is_some_and(QueryView::is_settled)
        })
        .await;
        let snap = settled(&store, run("select * from public.orders"), move |s| {
            s.query(id).is_some()
        })
        .await;
        assert_eq!(snap.queries.len(), 1);
        assert_eq!(snap.query(id).unwrap().sql, "select * from public.users");
    }

    #[tokio::test]
    async fn a_query_over_the_budget_waits_for_an_answer_rather_than_failing() {
        let store = store_with_budget(
            Behaviour {
                estimate_bytes: 5_000,
                ..Behaviour::instant()
            },
            Some(1_000),
        );
        let (store, conn) = connected(store).await;
        let id = QueryId::new();
        let snap = settled(
            &store,
            Action::RunQuery {
                conn,
                query: id,
                sql: "select * from public.users".to_owned(),
                max_rows: None,
                max_bytes: None,
            },
            move |s| s.query(id).is_some_and(|q| q.needs_approval.is_some()),
        )
        .await;

        let query = snap.query(id).unwrap();
        // Idle, not Loading: nothing is on its way, and a spinner over a
        // question nobody has answered is the client waiting for itself.
        assert!(matches!(query.data, LoadState::Idle), "{:?}", query.data);
        assert_eq!(query.estimate, Some(Estimate::Bytes(5_000)));

        // And saying yes runs it.
        let snap = settled(&store, Action::ApproveQuery(id), move |s| {
            s.query(id).is_some_and(|q| q.data.ready().is_some())
        })
        .await;
        let query = snap.query(id).unwrap();
        assert!(query.needs_approval.is_none(), "the question stayed open");
        assert!(query.data.ready().is_some_and(|r| r.row_count() > 0));
    }

    #[tokio::test]
    async fn approving_twice_runs_it_once() {
        // A double click, or an agent retrying. The second must not spend the
        // money again.
        let store = store_with_budget(
            Behaviour {
                estimate_bytes: 5_000,
                ..Behaviour::instant()
            },
            Some(1_000),
        );
        let (store, conn) = connected(store).await;
        let id = QueryId::new();
        let _ = settled(
            &store,
            Action::RunQuery {
                conn,
                query: id,
                sql: "select * from public.users".to_owned(),
                max_rows: None,
                max_bytes: None,
            },
            move |s| s.query(id).is_some_and(|q| q.needs_approval.is_some()),
        )
        .await;
        let after_one = settled(&store, Action::ApproveQuery(id), move |s| {
            s.query(id).is_some_and(|q| q.data.ready().is_some())
        })
        .await
        .applied;
        let snap = settled(&store, Action::ApproveQuery(id), move |s| {
            s.applied > after_one
        })
        .await;
        assert_eq!(snap.queries.len(), 1);
        assert!(
            snap.busy.is_empty(),
            "the second approval started something"
        );
    }

    #[tokio::test]
    async fn a_statement_that_is_not_one_statement_never_reaches_the_server() {
        let (store, conn) = connected_store().await;
        let id = QueryId::new();
        let snap = settled(
            &store,
            Action::RunQuery {
                conn,
                query: id,
                sql: "drop table x; select 1".to_owned(),
                max_rows: None,
                max_bytes: None,
            },
            move |s| s.query(id).is_some_and(QueryView::is_settled),
        )
        .await;
        let why = snap.query(id).unwrap().data.error().expect("a failure");
        assert!(why.contains('2'), "{why}");
    }

    #[tokio::test]
    async fn a_forgotten_query_leaves_nothing_behind() {
        let (store, conn) = connected_store().await;
        let id = QueryId::new();
        let _ = settled(
            &store,
            Action::RunQuery {
                conn,
                query: id,
                sql: "select * from public.users".to_owned(),
                max_rows: None,
                max_bytes: None,
            },
            move |s| s.query(id).is_some_and(QueryView::is_settled),
        )
        .await;
        let snap = settled(&store, Action::ForgetQuery(id), move |s| {
            s.query(id).is_none()
        })
        .await;
        assert!(snap.queries.is_empty());
    }

    #[tokio::test]
    async fn a_closed_connection_keeps_the_answer_it_already_gave() {
        // The rows are an answer that was given, and closing the connection
        // does not make it untrue. What it makes impossible is running it
        // again, which is a fact about the connection.
        let (store, conn) = connected_store().await;
        let id = QueryId::new();
        let _ = settled(
            &store,
            Action::RunQuery {
                conn,
                query: id,
                sql: "select * from public.users".to_owned(),
                max_rows: None,
                max_bytes: None,
            },
            move |s| s.query(id).is_some_and(QueryView::is_settled),
        )
        .await;
        let snap = settled(&store, Action::Disconnect(conn), move |s| {
            s.connection(conn)
                .is_some_and(|c| c.status == ConnStatus::Closed)
        })
        .await;
        assert!(snap.query(id).is_some_and(|q| q.data.ready().is_some()));
    }

    #[tokio::test]
    async fn a_query_still_running_when_its_connection_closes_is_told_so() {
        // Its task is abandoned, so no reply is coming. Left `Loading` it
        // spins for ever, and a caller waiting for it to settle never returns.
        let store = store(Behaviour {
            latency: std::time::Duration::from_millis(200),
            ..Behaviour::instant()
        });
        let (store, conn) = connected(store).await;
        let id = QueryId::new();
        store.dispatch(Action::RunQuery {
            conn,
            query: id,
            sql: "select * from public.users".to_owned(),
            max_rows: None,
            max_bytes: None,
        });
        until(&store, move |s| {
            s.query(id).is_some_and(|q| q.data.is_loading())
        })
        .await;

        let snap = settled(&store, Action::Disconnect(conn), move |s| {
            s.query(id).is_some_and(QueryView::is_settled)
        })
        .await;
        assert!(snap.query(id).unwrap().data.error().is_some());
    }

    #[tokio::test]
    async fn approving_after_the_connection_closed_strands_nothing() {
        // The question is only worth taking when there is something to run it
        // on: consumed with no session, the query keeps a spinner nobody can
        // cancel and an answer nobody can give again.
        let store = store_with_budget(
            Behaviour {
                estimate_bytes: 5_000,
                ..Behaviour::instant()
            },
            Some(1_000),
        );
        let (store, conn) = connected(store).await;
        let id = QueryId::new();
        let asked = settled(
            &store,
            Action::RunQuery {
                conn,
                query: id,
                sql: "select * from public.users".to_owned(),
                max_rows: None,
                max_bytes: None,
            },
            move |s| s.query(id).is_some_and(|q| q.needs_approval.is_some()),
        )
        .await
        .applied;
        let _ = settled(&store, Action::Disconnect(conn), move |s| {
            s.connection(conn)
                .is_some_and(|c| c.status == ConnStatus::Closed)
        })
        .await;

        let snap = settled(&store, Action::ApproveQuery(id), move |s| s.applied > asked).await;
        let query = snap.query(id).unwrap();
        assert!(!query.data.is_loading(), "{:?}", query.data);
        assert!(query.needs_approval.is_some(), "the answer was eaten");
        assert!(snap.busy.is_empty());
    }

    #[tokio::test]
    async fn forgetting_a_query_takes_its_spinner_with_it() {
        // Otherwise the busy row outlives its reader: "running a query", with
        // a cancel button, for a result nothing is going to show.
        let store = store(Behaviour {
            latency: std::time::Duration::from_millis(200),
            ..Behaviour::instant()
        });
        let (store, conn) = connected(store).await;
        let id = QueryId::new();
        store.dispatch(Action::RunQuery {
            conn,
            query: id,
            sql: "select * from public.users".to_owned(),
            max_rows: None,
            max_bytes: None,
        });
        until(&store, move |s| !s.busy.is_empty()).await;

        let snap = settled(&store, Action::ForgetQuery(id), move |s| {
            s.query(id).is_none()
        })
        .await;
        assert!(snap.busy.is_empty(), "{:?}", snap.busy);
    }

    #[tokio::test]
    async fn connecting_populates_the_tree() {
        let (_store, id) = connected_store().await;
        let _ = id;
    }

    #[tokio::test]
    async fn two_profiles_of_one_kind_are_open_at_the_same_time() {
        // The thing M0 could not express: `Drivers` was keyed by kind, so a
        // replica and a staging box could not both be open. One driver serves
        // both now, and what tells the connections apart is the profile each
        // was opened from — including their trees, which are per connection
        // and not per driver.
        let store = Store::spawn(Wiring::new(
            Drivers::new().with(Arc::new(MockDriver::new(Behaviour::instant()))),
            Arc::new(MockProfiles::new(["replica", "staging"])),
        ));
        store.dispatch(Action::Connect {
            profile: pid("replica"),
            conn: ConnId::new(),
        });
        store.dispatch(Action::Connect {
            profile: pid("staging"),
            conn: ConnId::new(),
        });

        let snap = until(&store, |s| {
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
        let snap = until(&store, |s| s.tree(ids[0]).count() > before).await;
        assert_eq!(snap.tree(ids[1]).count(), before);
    }

    #[tokio::test]
    async fn the_explorer_holds_every_connection_at_once() {
        // The thing the UI could not show before: with one flat list per
        // connection it drew the first and nothing else, so a second
        // connection was open and unreachable.
        let store = Store::spawn(Wiring::new(
            Drivers::new().with(Arc::new(MockDriver::new(Behaviour::instant()))),
            Arc::new(MockProfiles::new(["replica", "staging"])),
        ));
        store.dispatch(Action::Connect {
            profile: pid("replica"),
            conn: ConnId::new(),
        });
        store.dispatch(Action::Connect {
            profile: pid("staging"),
            conn: ConnId::new(),
        });
        let snap = until(&store, |s| {
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
        let (store, conn) = connected_store().await;
        let before = store.snapshot().tree(conn).count();
        assert!(before > 0);

        store.dispatch(Action::ToggleNode {
            conn,
            node: NodeRef::root(),
        });
        let snap = until(&store, |s| s.tree(conn).count() == 0).await;
        assert_eq!(snap.explorer.len(), 1, "the connection's own row stays");
        assert!(!snap.is_busy(), "closing a row is not a fetch");

        store.dispatch(Action::ToggleNode {
            conn,
            node: NodeRef::root(),
        });
        let snap = until(&store, |s| s.tree(conn).count() > 0).await;
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
        store.dispatch(Action::Connect {
            profile: pid("mock"),
            conn: ConnId::new(),
        });

        let snap = until(&store, |s| {
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
    async fn a_connection_id_the_caller_already_used_opens_nothing() {
        // The same id twice is a caller that has lost track of a connection it
        // holds, not a request for a second window onto the database — that is
        // a second `Connect` with a fresh id, which is the test below.
        let (store, conn) = connected_store().await;
        store.dispatch(Action::Connect {
            profile: pid("mock"),
            conn,
        });
        store.dispatch(Action::Quit);
        let snap = until(&store, |s| s.should_quit).await;
        assert_eq!(snap.connections.len(), 1);
    }

    #[tokio::test]
    async fn a_connection_that_fails_before_it_starts_still_answers_to_its_id() {
        // The caller is waiting on the id it chose. A failure row under some
        // other id would leave that wait to time out, reporting a hang for
        // something that failed immediately.
        let store = Store::spawn(Wiring::new(
            Drivers::new(),
            Arc::new(UnservedProfile(DriverKind::Postgres)),
        ));
        let conn = ConnId::new();
        let snap = settled(
            &store,
            Action::Connect {
                profile: pid("unserved"),
                conn,
            },
            move |s| s.connection_settled(conn),
        )
        .await;
        assert_eq!(
            snap.connection(conn).map(|c| c.status.clone()),
            Some(ConnStatus::Failed(
                "no driver registered for postgres".to_owned()
            ))
        );
    }

    #[tokio::test]
    async fn one_profile_can_be_opened_twice() {
        // A second window onto the same database is a real thing to want, so
        // the profile is not an identity the store deduplicates on.
        let store = store(Behaviour::instant());
        store.dispatch(Action::Connect {
            profile: pid("mock"),
            conn: ConnId::new(),
        });
        store.dispatch(Action::Connect {
            profile: pid("mock"),
            conn: ConnId::new(),
        });

        let snap = until(&store, |s| s.connections.len() == 2).await;
        assert_ne!(snap.connections[0].id, snap.connections[1].id);
        assert_eq!(snap.connections[0].profile, snap.connections[1].profile);
    }

    #[tokio::test]
    async fn a_store_that_has_done_nothing_still_says_what_it_could_connect_to() {
        // The loop publishes when something happens, and until a caller acts
        // nothing has. Left to it, "what can I connect to" answers "nothing" —
        // to the agent surface asking before its first command, and to the
        // interactive client drawing its first frame.
        let store = store(Behaviour::instant());
        assert!(!store.snapshot().profiles.is_empty());
    }

    #[tokio::test]
    async fn connecting_to_a_profile_nobody_configured_is_ignored() {
        // Unreachable from the interactive client, which only ever names a
        // profile it already listed — so there is nothing to show this on:
        // no connection row exists yet, and inventing one for a name that
        // does not exist would misrepresent what was asked for.
        let store = store(Behaviour::instant());
        store.dispatch(Action::Connect {
            profile: pid("typo"),
            conn: ConnId::new(),
        });
        store.dispatch(Action::Quit);
        let snap = until(&store, |s| s.should_quit).await;
        assert!(snap.connections.is_empty());
    }

    #[tokio::test]
    async fn a_profile_naming_an_unbuilt_driver_fails_as_a_connection() {
        let store = Store::spawn(Wiring::new(
            Drivers::new(),
            Arc::new(UnservedProfile(DriverKind::Postgres)),
        ));
        store.dispatch(Action::Connect {
            profile: pid("unserved"),
            conn: ConnId::new(),
        });

        let snap = until(&store, |s| !s.connections.is_empty()).await;
        assert_eq!(
            snap.connections[0].status,
            ConnStatus::Failed("no driver registered for postgres".to_owned())
        );
    }

    #[tokio::test]
    async fn a_connection_is_visible_while_it_is_still_opening() {
        let store = store(Behaviour {
            latency: std::time::Duration::from_millis(50),
            ..Behaviour::instant()
        });
        store.dispatch(Action::Connect {
            profile: pid("mock"),
            conn: ConnId::new(),
        });

        // The user must see that something is happening, with a way to stop it.
        let snap = until(&store, |s| !s.connections.is_empty()).await;
        assert_eq!(snap.connections[0].status, ConnStatus::Connecting);
        assert!(snap.is_busy());
    }

    #[tokio::test]
    async fn expanding_a_node_loads_its_children() {
        let (store, conn) = connected_store().await;
        let node = NodeRef::new(NodeKind::Namespace, ["public"]);
        store.dispatch(Action::ToggleNode {
            conn,
            node: node.clone(),
        });

        let snap = until(&store, |s| s.tree(conn).count() > 3).await;
        let rows: Vec<&VisibleNode> = snap.tree(conn).collect();
        assert_eq!(rows[0].state, NodeState::Expanded);
        assert!(rows.iter().any(|n| n.label == "users"));
    }

    #[tokio::test]
    async fn collapsing_needs_no_round_trip() {
        let (store, conn) = connected_store().await;
        let node = NodeRef::new(NodeKind::Namespace, ["public"]);
        store.dispatch(Action::ToggleNode {
            conn,
            node: node.clone(),
        });
        until(&store, |s| s.tree(conn).count() > 3).await;

        store.dispatch(Action::ToggleNode { conn, node });
        let snap = until(&store, |s| s.tree(conn).count() == 3).await;
        let rows: Vec<&VisibleNode> = snap.tree(conn).collect();
        assert_eq!(rows[0].state, NodeState::Collapsed);
    }

    #[tokio::test]
    async fn expanding_leaves_an_open_node_open() {
        // A toggle is a statement about a state the caller can see. This
        // caller cannot, and in a session somebody else is clicking in, the
        // node can be opened between the read and the dispatch.
        let (store, conn) = connected_store().await;
        let public = NodeRef::new(NodeKind::Namespace, ["public"]);
        store.dispatch(Action::ToggleNode {
            conn,
            node: public.clone(),
        });
        until(&store, |s| s.tree(conn).count() > 3).await;

        store.dispatch(Action::ExpandNode { conn, node: public });
        // A collapse needs no round trip, so it would already have happened by
        // the time this second expansion has anything to show.
        store.dispatch(Action::ExpandNode {
            conn,
            node: NodeRef::new(NodeKind::Namespace, ["analytics"]),
        });
        let snap = until(&store, |s| s.tree(conn).any(|n| n.label == "daily_summary")).await;
        assert!(snap.tree(conn).any(|n| n.label == "users"));
    }

    #[tokio::test]
    async fn expanding_a_connections_own_row_opens_it_and_leaves_it_open() {
        let (store, conn) = connected_store().await;
        let root = NodeRef::root();
        store.dispatch(Action::ToggleNode {
            conn,
            node: root.clone(),
        });
        until(&store, |s| s.tree(conn).count() == 0).await;

        store.dispatch(Action::ExpandNode {
            conn,
            node: root.clone(),
        });
        store.dispatch(Action::ExpandNode { conn, node: root });
        store.dispatch(Action::Quit);
        let snap = until(&store, |s| s.should_quit).await;
        assert_eq!(snap.tree(conn).count(), 3);
    }

    #[tokio::test]
    async fn expanding_a_node_the_tree_never_heard_of_asks_for_nothing() {
        // Unreachable from a front-end holding the row. The cost of letting it
        // through is not a wasted round trip but a reply with nowhere to land:
        // the node is invisible either way, so all the user would see is
        // "expanding ghost" in the status bar.
        let (store, conn) = connected_store().await;
        store.dispatch(Action::ExpandNode {
            conn,
            node: NodeRef::new(NodeKind::Namespace, ["ghost"]),
        });
        store.dispatch(Action::Quit);
        let snap = until(&store, |s| s.should_quit).await;
        assert!(snap.busy.is_empty(), "{:?}", snap.busy);
    }

    #[tokio::test]
    async fn previewing_a_relation_with_no_name_asks_for_nothing() {
        let (store, conn) = connected_store().await;
        store.dispatch(Action::PreviewTable {
            conn,
            table: TableRef::new([] as [&str; 0]),
        });
        store.dispatch(Action::Quit);
        let snap = until(&store, |s| s.should_quit).await;
        assert!(snap.previews.is_empty());
    }

    #[tokio::test]
    async fn previewing_fills_a_preview_for_that_relation() {
        let (store, conn) = connected_store().await;
        let table = TableRef::new(["public", "users"]);
        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });

        let snap = until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.data.ready().is_some())
        })
        .await;
        let preview = preview_of(&snap, conn, &table);
        assert_eq!(preview.data.ready().unwrap().row_count(), 50);
    }

    #[tokio::test]
    async fn previewing_the_same_relation_twice_does_not_duplicate_it() {
        let (store, conn) = connected_store().await;
        let table = TableRef::new(["public", "users"]);
        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        until(&store, |s| s.previews.len() == 1).await;

        store.dispatch(Action::PreviewTable { conn, table });
        store.dispatch(Action::PreviewTable {
            conn,
            table: TableRef::new(["public", "empty"]),
        });
        let snap = until(&store, |s| s.previews.len() == 2).await;
        assert_eq!(
            snap.previews.len(),
            2,
            "the first relation must not open twice"
        );
    }

    #[tokio::test]
    async fn asking_again_for_a_relation_whose_page_failed_retries_it() {
        // Reuse and failure meet here: asking for an already-cached relation
        // does not refetch it, and a cached relation holding an error has
        // nothing to reuse. Since `LoadMore` will not extend rows that are
        // not there, asking again is the only retry there is — without this,
        // the entry is dead until something forgets it.
        let store = store(Behaviour {
            flaky_nodes: vec![(vec!["public".to_owned(), "users".to_owned()], 1)],
            ..Behaviour::instant()
        });
        store.dispatch(Action::Connect {
            profile: pid("mock"),
            conn: ConnId::new(),
        });
        let snap = until(&store, |s| {
            s.connections.first().is_some_and(ConnectionView::is_ready)
        })
        .await;
        let conn = snap.connections[0].id;
        let table = TableRef::new(["public", "users"]);

        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.data.error().is_some())
        })
        .await;

        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        let snap = until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.data.ready().is_some())
        })
        .await;

        assert_eq!(snap.previews.len(), 1, "the retry duplicated the entry");
        assert_eq!(rows_of(&snap, conn, &table), 50);
    }

    #[tokio::test]
    async fn a_failing_preview_marks_itself_not_the_whole_app() {
        let store = store(Behaviour {
            failing_nodes: vec![vec!["analytics".to_owned(), "broken".to_owned()]],
            ..Behaviour::instant()
        });
        store.dispatch(Action::Connect {
            profile: pid("mock"),
            conn: ConnId::new(),
        });
        let snap = until(&store, |s| {
            s.connections.first().is_some_and(ConnectionView::is_ready)
        })
        .await;
        let conn = snap.connections[0].id;
        let table = TableRef::new(["analytics", "broken"]);

        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        let snap = until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.data.error().is_some())
        })
        .await;
        assert!(
            preview_of(&snap, conn, &table)
                .data
                .error()
                .unwrap()
                .contains("corrupt")
        );
        assert!(snap.connections[0].is_ready(), "the connection is fine");
    }

    #[tokio::test]
    async fn sorting_replaces_the_page_rather_than_appending() {
        let (store, conn) = connected_store().await;
        let table = TableRef::new(["public", "users"]);
        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.data.ready().is_some())
        })
        .await;

        store.dispatch(Action::SortPreview {
            conn,
            table: table.clone(),
            column: 0,
        });
        let snap = until(&store, |s| {
            s.preview(conn, &table).and_then(|p| p.sort).is_some()
        })
        .await;
        assert_eq!(
            preview_of(&snap, conn, &table).sort.unwrap().dir,
            SortDir::Asc
        );

        store.dispatch(Action::SortPreview {
            conn,
            table: table.clone(),
            column: 0,
        });
        let snap = until(&store, |s| {
            s.preview(conn, &table)
                .and_then(|p| p.sort)
                .is_some_and(|s| s.dir == SortDir::Desc)
        })
        .await;
        // Still one page: a re-sort invalidates everything already fetched.
        assert_eq!(
            preview_of(&snap, conn, &table)
                .data
                .ready()
                .unwrap()
                .row_count(),
            50
        );
    }

    #[tokio::test]
    async fn sorting_by_a_column_that_is_not_there_is_refused() {
        // The mock answers "out of range", as a real engine does. Passed
        // through, the ordering would stick to the preview, so every later
        // request — including the retry `PreviewTable` is — would carry it and
        // fail the same way for as long as the preview lived.
        let (store, conn) = connected_store().await;
        let table = TableRef::new(["public", "users"]);
        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        let snap = until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.data.ready().is_some())
        })
        .await;
        let width = preview_of(&snap, conn, &table)
            .data
            .ready()
            .unwrap()
            .columns()
            .len();

        store.dispatch(Action::SortPreview {
            conn,
            table: table.clone(),
            column: width,
        });
        store.dispatch(Action::Quit);
        let snap = until(&store, |s| s.should_quit).await;

        let preview = preview_of(&snap, conn, &table);
        assert!(preview.sort.is_none());
        assert!(preview.data.ready().is_some(), "{:?}", preview.data);
    }

    #[tokio::test]
    async fn a_preview_that_has_never_loaded_can_still_only_be_sorted_by_column_zero() {
        // The state the width is unknown in, and the one the loop lived in: a
        // sort accepted here would ride every later request, including the
        // retry that is the only way back.
        let store = store(Behaviour {
            flaky_nodes: vec![(vec!["public".to_owned(), "users".to_owned()], 1)],
            ..Behaviour::instant()
        });
        let (store, conn) = connected(store).await;
        let table = TableRef::new(["public", "users"]);
        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.data.error().is_some())
        })
        .await;

        store.dispatch(Action::SortPreview {
            conn,
            table: table.clone(),
            column: 99,
        });
        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        let snap = until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.data.ready().is_some())
        })
        .await;
        assert!(preview_of(&snap, conn, &table).sort.is_none());
    }

    #[tokio::test]
    async fn a_connection_that_cannot_order_a_preview_is_never_asked_to() {
        // The TUI hides the gesture, so this is about the caller that does not
        // read `Capabilities` — the agent surface. Passed through, the driver
        // answers `Unsupported`, and the preview reports a failure for
        // something nobody could have asked for.
        let store = store_of(
            MockDriver::new(Behaviour::instant()).with_capabilities(NO_SORT),
            PageRequest::DEFAULT_LIMIT,
        );
        let (store, conn) = connected(store).await;
        let table = TableRef::new(["public", "big"]);
        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.loaded_rows == 200)
        })
        .await;

        store.dispatch(Action::SortPreview {
            conn,
            table: table.clone(),
            column: 0,
        });
        // The append is what proves the sort was dropped rather than merely
        // slow: had it gone through, `data` would be `Loading` and `LoadMore`
        // would refuse to extend it, so this would time out instead.
        store.dispatch(Action::LoadMore {
            conn,
            table: table.clone(),
        });
        let snap = until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.loaded_rows >= 400)
        })
        .await;

        let preview = preview_of(&snap, conn, &table);
        assert!(preview.sort.is_none(), "an ordering nothing can serve");
        assert!(preview.data.error().is_none(), "{:?}", preview.data);
    }

    #[tokio::test]
    async fn two_quick_load_mores_do_not_skip_a_page() {
        let (store, conn) = connected_store().await;
        let table = TableRef::new(["public", "big"]);
        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.loaded_rows == 200)
        })
        .await;

        // Key repeat, or a wheel resting on the last row. The old guard tested
        // `data`, which an append leaves `Ready`, so both went out: the first
        // reply was dropped as stale and rows 201-400 could never be asked for
        // again, leaving a table that silently joined 1-200 to 401-600.
        store.dispatch(Action::LoadMore {
            conn,
            table: table.clone(),
        });
        store.dispatch(Action::LoadMore {
            conn,
            table: table.clone(),
        });

        let snap = until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.loaded_rows >= 400)
        })
        .await;
        let preview = preview_of(&snap, conn, &table);
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
        store.dispatch(Action::Connect {
            profile: pid("mock"),
            conn: ConnId::new(),
        });
        let snap = until(&store, |s| {
            s.connections.first().is_some_and(ConnectionView::is_ready)
        })
        .await;
        let conn = snap.connections[0].id;
        let table = TableRef::new(["public", "big"]);

        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.loaded_rows == 200)
        })
        .await;

        store.dispatch(Action::LoadMore {
            conn,
            table: table.clone(),
        });
        let snap = until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.last_error.is_some())
        })
        .await;

        // The rows already fetched are still good. Replacing them with an
        // error panel loses them and leaves the next request starting from the
        // wrong offset.
        let preview = preview_of(&snap, conn, &table);
        assert_eq!(preview.loaded_rows, 200);
        assert!(preview.data.ready().is_some(), "the table is still there");
        assert!(preview.last_error.is_some());
    }

    #[tokio::test]
    async fn load_more_on_a_page_that_failed_asks_for_nothing() {
        // Fails once, so a second request would succeed and be believed. The
        // preview is still on the page that failed, so the next offset steps
        // over it: rows 201-400 would arrive with nothing to append them to
        // and be shown as the whole relation, the first 200 missing without a
        // word.
        let store = store(Behaviour {
            flaky_nodes: vec![(vec!["public".to_owned(), "big".to_owned()], 1)],
            ..Behaviour::instant()
        });
        store.dispatch(Action::Connect {
            profile: pid("mock"),
            conn: ConnId::new(),
        });
        let snap = until(&store, |s| {
            s.connections.first().is_some_and(ConnectionView::is_ready)
        })
        .await;
        let conn = snap.connections[0].id;
        let table = TableRef::new(["public", "big"]);

        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.data.error().is_some())
        })
        .await;

        store.dispatch(Action::LoadMore {
            conn,
            table: table.clone(),
        });
        // Actions are handled in order, so a snapshot that has seen the quit
        // has seen the `LoadMore` — and nothing is loading because of it.
        store.dispatch(Action::Quit);
        let snap = until(&store, |s| s.should_quit).await;

        assert!(snap.busy.is_empty(), "a page went out anyway");
        let preview = preview_of(&snap, conn, &table);
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
        store.dispatch(Action::Connect {
            profile: pid("mock"),
            conn: ConnId::new(),
        });
        let snap = until(&store, |s| {
            s.connections.first().is_some_and(ConnectionView::is_ready)
        })
        .await;
        let conn = snap.connections[0].id;

        let node = NodeRef::new(NodeKind::Namespace, ["public"]);
        store.dispatch(Action::ToggleNode {
            conn,
            node: node.clone(),
        });
        let snap = until(&store, Snapshot::is_busy).await;
        let busy = snap.busy[0].id;

        store.dispatch(Action::Cancel(busy));
        // The reply is never coming. A node left in `Loading` cannot even be
        // toggled again, so it would be permanently dead rather than merely
        // failed.
        let snap = until(&store, |s| {
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
        store.dispatch(Action::Connect {
            profile: pid("mock"),
            conn: ConnId::new(),
        });
        let snap = until(&store, |s| !s.connections.is_empty()).await;
        let conn = snap.connections[0].id;

        store.dispatch(Action::Disconnect(conn));
        let snap = until(&store, |s| s.connections[0].status == ConnStatus::Closed).await;
        assert!(!snap.is_busy(), "in-flight work is dropped with it");

        // The reply lands after the disconnect. Writing Ready over a closed
        // connection would resurrect it with a live session attached.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let snap = store.snapshot();
        assert_eq!(snap.connections[0].status, ConnStatus::Closed);
        assert_eq!(snap.tree(conn).count(), 0);
    }

    #[tokio::test]
    async fn load_more_appends_to_what_is_already_there() {
        let (store, conn) = connected_store().await;
        let table = TableRef::new(["public", "big"]);
        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.loaded_rows == 200)
        })
        .await;

        store.dispatch(Action::LoadMore {
            conn,
            table: table.clone(),
        });
        let snap = until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.loaded_rows == 400)
        })
        .await;
        let grid = preview_of(&snap, conn, &table).data.ready().unwrap();
        assert_eq!(grid.row_count(), 400);
        assert_eq!(grid.total_rows(), Some(200_000));
    }

    #[tokio::test]
    async fn a_relation_shorter_than_a_page_is_finished_on_arrival() {
        // The whole point of `exhausted`: the store knows it asked for 200 and
        // got 50, so nobody has to spend a second request discovering it.
        let (store, conn) = connected_store().await;
        let table = TableRef::new(["public", "users"]);
        let snap = settled(
            &store,
            Action::PreviewTable {
                conn,
                table: table.clone(),
            },
            {
                let table = table.clone();
                move |s| {
                    s.preview(conn, &table)
                        .is_some_and(|p| p.data.ready().is_some())
                }
            },
        )
        .await;
        assert!(preview_of(&snap, conn, &table).exhausted);
    }

    #[tokio::test]
    async fn a_page_the_driver_cut_short_is_not_the_end_of_the_relation() {
        // BigQuery answers a 200-row request with fewer when the response
        // would pass its 10 MB cap. Read as the end, a wide table would sit on
        // its first screenful for the life of the tab; and the next page has
        // to start where the rows actually stopped, or the grid draws a
        // contiguous relation with a hole in it.
        let table = TableRef::new(["public", "big"]);
        let store = store(Behaviour {
            short_pages: vec![table.path.clone()],
            ..Behaviour::instant()
        });
        let (store, conn) = connected(store).await;
        let snap = settled(
            &store,
            Action::PreviewTable {
                conn,
                table: table.clone(),
            },
            {
                let table = table.clone();
                move |s| {
                    s.preview(conn, &table)
                        .is_some_and(|p| p.data.ready().is_some())
                }
            },
        )
        .await;
        let preview = preview_of(&snap, conn, &table);
        assert_eq!(preview.loaded_rows, 100);
        assert!(
            !preview.exhausted,
            "a page the driver cut short was taken for the end of the relation"
        );

        store.dispatch(Action::LoadMore {
            conn,
            table: table.clone(),
        });
        let snap = until(&store, |s| {
            s.preview(conn, &table).is_some_and(|p| p.loaded_rows > 100)
        })
        .await;
        let grid = preview_of(&snap, conn, &table).data.ready().unwrap();
        // Row 100 rather than row 200: the second request picked up where the
        // rows stopped, not where the limit said they would.
        assert_eq!(grid.value(100, 0), Some(&Value::Int(100)));
    }

    #[tokio::test]
    async fn a_finished_relation_is_not_asked_for_another_page() {
        // A front-end reading `exhausted` never asks; one that does not — an
        // agent sending actions straight in — should not be charged a round
        // trip for a page that is known to be empty.
        let (store, conn) = connected_store().await;
        let table = TableRef::new(["public", "users"]);
        let snap = settled(
            &store,
            Action::PreviewTable {
                conn,
                table: table.clone(),
            },
            {
                let table = table.clone();
                move |s| {
                    s.preview(conn, &table)
                        .is_some_and(|p| p.data.ready().is_some())
                }
            },
        )
        .await;
        let before = preview_of(&snap, conn, &table).attempts;

        store.dispatch(Action::LoadMore {
            conn,
            table: table.clone(),
        });
        // Nothing to wait for, so the wait is for the store to have seen it.
        until(&store, |s| !s.is_busy()).await;
        let snap = store.snapshot();
        assert_eq!(preview_of(&snap, conn, &table).attempts, before);
    }

    #[tokio::test]
    async fn the_configured_page_size_is_what_a_page_is() {
        // `page_size` was parsed and validated by `sqlake-config` and read by
        // nothing, so every page was the built-in size whatever the file said
        // — a setting the client appears to honour and does not.
        let store = store_paging(Behaviour::instant(), 25);
        store.dispatch(Action::Connect {
            profile: pid("mock"),
            conn: ConnId::new(),
        });
        let snap = until(&store, |s| {
            s.connections.first().is_some_and(ConnectionView::is_ready)
        })
        .await;
        let conn = snap.connections[0].id;
        let table = TableRef::new(["public", "big"]);

        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        let snap = until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.data.ready().is_some())
        })
        .await;
        assert_eq!(rows_of(&snap, conn, &table), 25);

        // Sorting starts the relation again, and starting again is also a page.
        store.dispatch(Action::SortPreview {
            conn,
            table: table.clone(),
            column: 0,
        });
        let snap = until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.sort.is_some() && p.data.ready().is_some())
        })
        .await;
        assert_eq!(rows_of(&snap, conn, &table), 25);

        // And the page after it starts where this one stopped, rather than at
        // the built-in size: an offset that moves by more than the page leaves
        // a gap no scroll can reach.
        store.dispatch(Action::LoadMore {
            conn,
            table: table.clone(),
        });
        let snap = until(&store, |s| {
            s.preview(conn, &table).is_some_and(|p| p.loaded_rows == 50)
        })
        .await;
        let grid = preview_of(&snap, conn, &table).data.ready().unwrap();
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
        // Sorting a preview whose page failed leaves the arrow drawn and no
        // rows under it. If the retry asked for the relation unordered, the
        // header would be describing an order the rows do not have — the
        // arrow is the only thing telling the user what they are looking at.
        let store = store(Behaviour {
            flaky_nodes: vec![(vec!["public".to_owned(), "users".to_owned()], 3)],
            ..Behaviour::instant()
        });
        store.dispatch(Action::Connect {
            profile: pid("mock"),
            conn: ConnId::new(),
        });
        let snap = until(&store, |s| {
            s.connections.first().is_some_and(ConnectionView::is_ready)
        })
        .await;
        let conn = snap.connections[0].id;
        let table = TableRef::new(["public", "users"]);

        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.data.error().is_some())
        })
        .await;

        // Twice, because the first toggle is ascending and the fixture is
        // already in that order: only descending can tell the two apart.
        for dir in [SortDir::Asc, SortDir::Desc] {
            store.dispatch(Action::SortPreview {
                conn,
                table: table.clone(),
                column: 0,
            });
            until(&store, |s| {
                s.busy.is_empty() && sort_of(s, conn, &table) == Some(dir)
            })
            .await;
        }

        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        let snap = until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.data.ready().is_some())
        })
        .await;

        let grid = preview_of(&snap, conn, &table).data.ready().unwrap();
        assert_eq!(sort_of(&snap, conn, &table), Some(SortDir::Desc));
        assert_eq!(
            grid.value(0, 0),
            Some(&Value::Int(50)),
            "the retry ignored the arrow the header is showing"
        );
    }

    fn sort_of(snap: &Snapshot, conn: ConnId, table: &TableRef) -> Option<SortDir> {
        snap.preview(conn, table)
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
        store.dispatch(Action::Connect {
            profile: pid("mock"),
            conn: ConnId::new(),
        });
        let snap = until(&store, |s| {
            s.connections.first().is_some_and(ConnectionView::is_ready)
        })
        .await;
        let conn = snap.connections[0].id;
        let table = TableRef::new(["public", "users"]);

        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        let snap = until(&store, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.data.ready().is_some())
        })
        .await;
        assert_eq!(rows_of(&snap, conn, &table), 1);
    }

    fn rows_of(snap: &Snapshot, conn: ConnId, table: &TableRef) -> usize {
        snap.preview(conn, table)
            .and_then(|p| p.data.ready())
            .expect("a loaded page")
            .row_count()
    }

    #[tokio::test]
    async fn forgetting_a_preview_drops_its_own_page_in_flight() {
        // Otherwise the busy row for a page nobody is waiting on anymore
        // stays on screen until the slow reply eventually arrives — "loading
        // …" for a preview nothing has open anymore.
        let store = store(Behaviour {
            latency: std::time::Duration::from_millis(1),
            slow_nodes: vec![vec!["public".to_owned(), "users".to_owned()]],
            slow_latency: std::time::Duration::from_secs(30),
            ..Behaviour::instant()
        });
        store.dispatch(Action::Connect {
            profile: pid("mock"),
            conn: ConnId::new(),
        });
        let snap = until(&store, |s| {
            s.connections.first().is_some_and(ConnectionView::is_ready)
        })
        .await;
        let conn = snap.connections[0].id;
        let table = TableRef::new(["public", "users"]);

        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        until(&store, Snapshot::is_busy).await;

        store.dispatch(Action::ForgetPreview {
            conn,
            table: table.clone(),
        });
        let snap = until(&store, |s| s.previews.is_empty()).await;
        assert!(
            snap.busy.is_empty(),
            "the forgotten preview's page is still loading"
        );
    }

    #[tokio::test]
    async fn disconnecting_removes_that_connection_s_previews() {
        let (store, conn) = connected_store().await;
        let table = TableRef::new(["public", "users"]);
        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        until(&store, |s| s.previews.len() == 1).await;

        store.dispatch(Action::Disconnect(conn));
        let snap = until(&store, |s| s.previews.is_empty()).await;
        assert_eq!(snap.connections[0].status, ConnStatus::Closed);
        assert_eq!(snap.tree(conn).count(), 0);
    }

    #[tokio::test]
    async fn cancelling_clears_the_busy_indicator() {
        let store = store(Behaviour {
            latency: std::time::Duration::from_secs(30),
            ..Behaviour::instant()
        });
        store.dispatch(Action::Connect {
            profile: pid("mock"),
            conn: ConnId::new(),
        });
        let snap = until(&store, Snapshot::is_busy).await;

        store.dispatch(Action::Cancel(snap.busy[0].id));
        let snap = until(&store, |s| !s.is_busy()).await;
        assert!(!snap.is_busy());
    }

    #[tokio::test]
    async fn quitting_is_visible_in_the_snapshot() {
        let store = store(Behaviour::instant());
        store.dispatch(Action::Quit);
        until(&store, |s| s.should_quit).await;
    }

    #[tokio::test]
    async fn actions_for_unknown_relations_are_ignored() {
        let store = store(Behaviour::instant());
        let conn = ConnId::new();
        let table = TableRef::new(["public", "ghost"]);
        store.dispatch(Action::LoadMore {
            conn,
            table: table.clone(),
        });
        store.dispatch(Action::ForgetPreview { conn, table });
        store.dispatch(Action::Disconnect(conn));
        store.dispatch(Action::Cancel(BusyId::new(99)));
        store.dispatch(Action::Quit);

        // The point is that none of the above panicked the store task.
        until(&store, |s| s.should_quit).await;
    }
}

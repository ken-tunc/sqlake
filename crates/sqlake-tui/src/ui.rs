//! Everything about appearance, which the store must not own.
//!
//! Scroll offsets, the selected row and cell, column widths and the split
//! position live here and are changed synchronously by [`UiState::apply`].
//! Routing a wheel tick through an async task would put a round trip in front
//! of every notch, and mixing appearance into the snapshot makes the scroll
//! position jump whenever an unrelated update arrives.
//!
//! Clamping needs to know how tall a pane is, which is a fact about the frame
//! the user is looking at. That frame's rectangles are recorded during drawing,
//! the same as [`crate::hit::HitMap`] — an event is always answered against the
//! layout that produced the pixels it was aimed at.
//!
//! [`OpenTab`] and [`Toast`] are here for the same reason: the store holds the
//! data a preview is, and this screen decides what to call a tab of it and
//! which failures are worth a passing note.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use ratatui::layout::Rect;
use sqlake_app::PagedResult;

use crate::detail::RenderedDetail;
use sqlake_app::action::Action;
use sqlake_app::snapshot::{ConnStatus, ConnectionView, LoadState, Snapshot};
use sqlake_core::detail::TableDetail;

use crate::definition::Laid;
#[cfg(test)]
use sqlake_app::tree::TreeView;
use sqlake_app::tree::VisibleNode;
use sqlake_core::id::{ConnId, QueryId, TabId};
use sqlake_core::library::TemplateId;
use sqlake_core::node::TableRef;
use sqlake_core::result::Sort;

use crate::chrome::MIN_GRID_HEIGHT;
use crate::grid::RenderedGrid;
use crate::hit::{PaneId, SplitId, Target, ToastId};
use crate::intent::ViewCmd;

/// What a tab is showing.
///
/// The connection is on [`OpenTab`] rather than in here: both kinds belong to
/// one, and a SQL tab with no connection is a query with nowhere to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabContent {
    Preview(TableRef),
    /// A SQL buffer, numbered so that two of them can be told apart in a bar
    /// where neither has a relation's name to wear.
    ///
    /// The text is this screen's, not the store's: what is being typed is one
    /// person's half-finished sentence, and what crosses into `sqlake-app` is
    /// the text of a query somebody asked to run. Empty until the `$EDITOR`
    /// handoff fills it.
    ///
    /// `query` is the run this tab last started, once it has started one. The
    /// id rather than the result: the result is the store's, and a copy here
    /// would be a second answer to go stale.
    Sql {
        number: u32,
        text: String,
        query: Option<QueryId>,
    },
    /// What a relation is, rather than what is in it.
    ///
    /// Its own kind rather than a mode on `Preview`: a relation can have both
    /// open at once, and they are different questions about it — which is also
    /// why the tab bar has to be able to tell them apart by name.
    Definition {
        table: TableRef,
        /// Which of the definition's sections is drawn, by position in its own
        /// list. Kept per tab, because two definitions open at once are two
        /// people's places in two lists.
        section: usize,
    },
}

/// A tab this screen has open on one connection.
///
/// At most one per `(conn, table)` for a preview: opening a relation already
/// open selects that tab rather than minting a second, and `close_tab` in
/// `input` depends on it to know when the store's copy can go. SQL tabs have
/// no such rule — two of them are two different questions, and nothing in the
/// store is keyed by either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTab {
    pub id: TabId,
    pub conn: ConnId,
    pub content: TabContent,
}

impl OpenTab {
    /// The relation this tab is a preview of, or `None` for a SQL tab.
    ///
    /// Most of what reads a tab wants a relation and has nothing to do without
    /// one — sorting, paging, the store's cache key — so an `Option` here is
    /// what turns each of those into one `?` rather than a `match` repeated at
    /// every call site.
    #[must_use]
    pub const fn table(&self) -> Option<&TableRef> {
        match &self.content {
            TabContent::Preview(table) => Some(table),
            // Deliberately not the definition's. Everything that reads this
            // wants the relation a *preview* is of — sorting it, paging it,
            // forgetting the store's copy of it — and a definition shares none
            // of that: it is fetched once, never paged and never sorted.
            TabContent::Sql { .. } | TabContent::Definition { .. } => None,
        }
    }

    /// The statement in this tab's buffer, or `None` when it is not a SQL tab.
    #[must_use]
    pub fn sql(&self) -> Option<&str> {
        match &self.content {
            TabContent::Sql { text, .. } => Some(text),
            TabContent::Preview(_) | TabContent::Definition { .. } => None,
        }
    }

    /// The relation this tab is a definition of.
    #[must_use]
    pub const fn defines(&self) -> Option<&TableRef> {
        match &self.content {
            TabContent::Definition { table, .. } => Some(table),
            TabContent::Preview(_) | TabContent::Sql { .. } => None,
        }
    }

    /// What the tab bar and the pane border call it.
    #[must_use]
    pub fn title(&self) -> Cow<'_, str> {
        self.content.title()
    }
}

impl TabContent {
    #[must_use]
    pub fn title(&self) -> Cow<'_, str> {
        match self {
            Self::Preview(table) => Cow::Borrowed(table.name()),
            Self::Sql { number, .. } => Cow::Owned(format!("SQL#{number}")),
            // Named apart from the preview of the same relation, because two
            // tabs reading `users` would otherwise be two tabs called `users`.
            Self::Definition { table, .. } => Cow::Owned(format!("{}: def", table.name())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

/// A transient notice. "Show this briefly and let it go" rather than "attach
/// it to the row it is about" is a rendering choice, not a fact about the data.
#[derive(Debug, Clone)]
pub struct Toast {
    pub id: ToastId,
    pub text: String,
    pub severity: Severity,
    pub created_at: Instant,
}

/// A search over the explorer.
///
/// `editing` is separate from the text because the two ends of a search are
/// different things: while it is being typed the box holds the keyboard, and
/// once it is not, the *filtered tree* does — which is the only way a
/// keyboard can reach what was searched for. design.md §1: nothing is
/// reachable by mouse only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    pub text: String,
    pub editing: bool,
}

impl Filter {
    /// The box, open and empty, as `/` leaves it.
    #[must_use]
    pub fn opening() -> Self {
        Self {
            text: String::new(),
            editing: true,
        }
    }
}

/// Neither pane is useful below this, so the splitter stops here rather than
/// letting one side be dragged out of existence.
pub const MIN_PANE_WIDTH: u16 = 12;

/// Where the splitter sits before anyone moves it, as a fraction of the screen.
const DEFAULT_EXPLORER_PERMILLE: u32 = 280;

/// How far above the last loaded row a scroll starts fetching.
///
/// Above the last row rather than at it, so the fetch overlaps the scrolling
/// instead of stopping it. A constant rather than the viewport's height: on a
/// tall terminal that would exceed a page and ask before the previous page had
/// anywhere to go.
const LOAD_MARGIN_ROWS: usize = 20;

/// A byte count somebody is about to be charged for, in units they price in.
///
/// Powers of a thousand, because that is what the budget beside it was written
/// in: `max_bytes_billed = "20GB"` is twenty thousand million bytes, and a
/// dialog that divided it by 1024s would answer "the limit is 18.6 GB" about a
/// number the user typed as 20.
///
/// Truncating rather than rounding: this is shown next to that limit, and a
/// number that rounded up to the limit would read as being at it.
fn bytes(n: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (1_000_000_000_000, "TB"),
        (1_000_000_000, "GB"),
        (1_000_000, "MB"),
        (1_000, "kB"),
    ];
    for (scale, unit) in UNITS {
        if n >= scale {
            return format!("{}.{} {unit}", n / scale, n % scale * 10 / scale);
        }
    }
    format!("{n} B")
}

/// Rows the detail pane takes.
///
/// Enough for a small document without taking the grid over. Fixed, because
/// there is no splitter for it yet: nothing but this constant decides how much
/// of a long value is reachable, which is what a `SplitId` for the pane would
/// change.
const DEFAULT_DETAIL_HEIGHT: u16 = 8;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TreeUi {
    pub offset: usize,
    pub selected: Option<usize>,
}

/// Per tab because two tabs showing different relations have nothing to say to
/// each other about column widths or which cell is selected.
#[derive(Debug, Default)]
pub struct GridUi {
    pub row_offset: usize,
    /// How many page requests had finished for this preview when this tab last
    /// asked for another.
    ///
    /// A count of what came back, rather than a description of what the last
    /// request left behind. That description cannot say whether a request was
    /// made at all, and three cases needing different answers look identical
    /// in it: a cancelled page changes nothing, a retry that fails the same
    /// way leaves the same message, and the end of a relation changes nothing
    /// either. The end is the one the store can actually tell — it knows what
    /// it asked for — so `PreviewView::exhausted` says it, and this only has
    /// to answer "has anything come back since I asked".
    asked_after: Option<u64>,
    /// Horizontal position in whole columns. Per column rather than per cell:
    /// the wheel and the scrollbar both move in column steps, and a half-drawn
    /// leading column is worse than a hard edge.
    pub col_offset: usize,
    pub row: usize,
    pub col: usize,
    widths: HashMap<usize, u16>,
    grid: Option<RenderedGrid>,
    /// The selected cell, laid out to be read, and which cell that was.
    ///
    /// Cached for the same reason the grid is: a snapshot is republished for
    /// reasons that have nothing to do with this tab, and rebuilding here
    /// means sanitising the whole value again — which for the megabyte-long
    /// text this pane exists for is the cost `MAX_CELL_CHARS` was introduced
    /// to avoid, reintroduced one pane over.
    detail: Option<((usize, usize), Arc<RenderedDetail>)>,
    /// The other corner of the selection, and the ordering it was made under.
    ///
    /// `None` is one cell. The ordering rides along because a selection is
    /// indexes into a result the store is free to replace: sorting fetches page
    /// one again, so rows 10..20 of the old order name different rows in the
    /// new one, and a copy taken from a stale anchor comes out wrong with
    /// nothing said. Paging is the case that must *not* clear it — appending
    /// leaves every existing row where it was.
    anchor: Option<((usize, usize), Option<Sort>)>,
}

impl GridUi {
    /// Rebuilt only when the rows change. A snapshot is republished for reasons that have nothing to do with this
    /// tab — a spinner tick will do it — and rebuilding on each one would
    /// re-sample the column widths and make them twitch as pages arrive.
    pub fn grid(&mut self, rows: &Arc<PagedResult>) -> &RenderedGrid {
        if !self.grid.as_ref().is_some_and(|g| g.is_for(rows)) {
            self.grid = Some(RenderedGrid::new(Arc::clone(rows)));
        }
        self.grid
            .as_ref()
            .expect("just built when it was missing or stale")
    }

    /// The rectangle the selection covers, as `(top, left, bottom, right)`
    /// inclusive.
    ///
    /// A rectangle rather than a set: anything else needs an answer to what a
    /// discontiguous selection means as CSV, and there is not a good one. A
    /// rectangle has one — the rows it covers, each cut to the columns.
    #[must_use]
    pub fn selection(&self, sort: Option<Sort>) -> (usize, usize, usize, usize) {
        match self.anchor {
            Some((at, made_under)) if made_under == sort => (
                at.0.min(self.row),
                at.1.min(self.col),
                at.0.max(self.row),
                at.1.max(self.col),
            ),
            _ => (self.row, self.col, self.row, self.col),
        }
    }

    /// How many cells are selected, or `None` when it is just the one.
    #[must_use]
    pub fn selected_cells(&self, sort: Option<Sort>) -> Option<(usize, usize)> {
        let (top, left, bottom, right) = self.selection(sort);
        let size = (bottom - top + 1, right - left + 1);
        (size != (1, 1)).then_some(size)
    }

    /// The document for the selected cell, built once per cell rather than
    /// once per frame.
    pub fn detail(&mut self) -> Option<Arc<RenderedDetail>> {
        let at = (self.row, self.col);
        if self.detail.as_ref().is_none_or(|(was, _)| *was != at) {
            let rendered = self.grid.as_ref()?;
            let value = rendered.raw(at.0, at.1)?;
            let column = rendered.columns().get(at.1)?;
            self.detail = Some((
                at,
                Arc::new(RenderedDetail::of(&column.name, &column.type_name, value)),
            ));
        }
        self.detail.as_ref().map(|(_, d)| Arc::clone(d))
    }

    /// Drawing needs the grid and the widths and offsets beside it at the same
    /// time, and the `&mut` that builds the grid cannot lend out both. Building
    /// through `grid` and then reading through this one keeps the caller from
    /// cloning a `RenderedGrid` — every column name reallocated — per frame.
    #[must_use]
    pub fn rendered(&self) -> Option<&RenderedGrid> {
        self.grid.as_ref()
    }

    /// The width to draw column `col` at.
    #[must_use]
    pub fn width(&self, col: usize, natural: u16) -> u16 {
        self.widths.get(&col).copied().unwrap_or(natural)
    }

    /// A column's width, set outright rather than nudged.
    ///
    /// Test-only, and marked so rather than shipped: nothing in the running
    /// program sets a width except by dragging, and an API that exists for the
    /// tests is one the tests have added to the program. The alternative was to
    /// move a column one cell at a time through `apply` and a whole `Snapshot`.
    #[cfg(test)]
    pub(crate) fn set_width(&mut self, col: usize, width: u16) {
        self.widths.insert(col, width.max(1));
    }

    fn resize(&mut self, col: usize, delta: i16, natural: u16) {
        let current = i32::from(self.width(col, natural));
        let next = (current + i32::from(delta)).clamp(1, i32::from(u16::MAX));
        self.widths.insert(col, next as u16);
    }
}

#[derive(Debug, Default)]
pub struct UiState {
    pub focus: PaneId,
    pub tree: TreeUi,
    pub hover: Option<Target>,
    /// The dialog on screen, if any. Whether one is open is a fact about this
    /// screen rather than about the data, so it lives here.
    pub modal: Option<crate::overlay::Modal>,
    /// The connections whose failure has already been raised as a dialog, so
    /// that dismissing it is final. A set rather than one id: remembering only
    /// the last leaves a second connection's failure unreported, because the
    /// first stays in the list and is what a search keeps finding.
    pub reported_failures: HashSet<ConnId>,
    /// The queries whose cost has already been put as a question, so that
    /// dismissing it is final.
    asked_approval: HashSet<QueryId>,
    /// The explorer's search, or `None` when there is not one.
    ///
    /// Screen state, not application state: it decides which rows *this*
    /// screen draws, the way scrolling does. An agent reading the same
    /// snapshot through `sqlake-api` wants the tree, not one person's search.
    pub filter: Option<Filter>,
    /// Every tab this screen has open, in the order they appear in the tab
    /// bar. At most one per `(conn, table)`.
    pub tabs: Vec<OpenTab>,
    pub active_tab: Option<TabId>,
    next_tab: u32,
    /// What the next SQL tab is called. Separate from `next_tab`: the id is
    /// bookkeeping and the number is read off the screen, so a session that
    /// opened six previews must not name its first query tab `SQL#7`.
    next_sql: u32,
    pub toasts: Vec<Toast>,
    next_toast: u64,
    /// The palette, while it is up. It holds the keyboard, so its presence is
    /// what `Context::Palette` is.
    pub palette: Option<crate::palette::Palette>,
    /// The last error already raised for each preview, so that a redraw does
    /// not raise it again — only a *new* message does.
    reported_preview_errors: HashMap<(ConnId, TableRef), String>,
    /// The runs whose failure has already been raised. By id rather than by
    /// message: a run is one attempt and fails once, and running again is a
    /// new id.
    reported_query_errors: HashSet<QueryId>,
    grids: HashMap<TabId, GridUi>,
    /// Each definition tab's laid-out copy, and the detail it was built from.
    laid: HashMap<TabId, (Arc<TableDetail>, Laid)>,
    /// How tall the detail pane is, or `None` while it is closed.
    ///
    /// View state like the splitter: which cell is being read closely is one
    /// person's question, and a caller reading the same snapshot through
    /// `sqlake-api` has the value already.
    detail_height: Option<u16>,
    /// How far down the detail pane is scrolled, and how many lines it has to
    /// scroll through.
    ///
    /// The pane exists for values longer than a column, and a value longer
    /// than a column is often longer than a pane: without this it shows the
    /// first few rows of something the grid had already shown 512 characters
    /// of, which is worse than not opening it.
    detail_offset: usize,
    detail_rows: usize,
    /// Lines in the active SQL tab's buffer, as of the last frame.
    ///
    /// Recorded the way `detail_rows` is, and for the same reason: the scroll
    /// clamp asks how much content there is, and the only thing that has
    /// counted it is the pass that drew it.
    sql_lines: usize,
    /// An OSC 52 sequence waiting for the render loop to write it.
    ///
    /// Not written from here: this type holds no terminal, and the sequence has
    /// to go out through the writer the TUI already owns rather than a second
    /// thing reaching for stdout while the alternate screen is up.
    pending_copy: Option<String>,
    /// The open context menu, if any. Screen state like the modal: which cell
    /// somebody right-clicked is one person's gesture.
    pub menu: Option<crate::menu::Menu>,
    /// `None` until the splitter is moved, so the default follows the terminal
    /// width instead of being frozen at whatever it was on the first frame.
    explorer_width: Option<u16>,
    /// Rectangles as of the last frame drawn. See the module doc.
    viewport: HashMap<PaneId, Rect>,
    /// The whole frame, as of the last one drawn.
    ///
    /// Recorded separately from the viewports because those are the areas
    /// *inside* the pane borders: adding them back up loses a column per border
    /// and the splitter would then move by a different amount than it was
    /// dragged.
    screen: Rect,
}

/// The panes `Tab` cycles through, in order. The tab bar and status bar are not
/// in it: nothing in them is reached by moving focus.
const FOCUS_ORDER: [PaneId; 2] = [PaneId::Explorer, PaneId::Grid];

impl UiState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_viewport(&mut self, pane: PaneId, rect: Rect) {
        self.viewport.insert(pane, rect);
    }

    /// Record the frame the layout was divided from. Called by
    /// [`crate::chrome::layout`], so it cannot drift from what was drawn.
    pub fn set_screen(&mut self, area: Rect) {
        self.screen = area;
    }

    #[must_use]
    pub fn viewport(&self, pane: PaneId) -> Rect {
        self.viewport.get(&pane).copied().unwrap_or_default()
    }

    /// Rows visible in `pane`, which is what a page key and a clamp both need.
    #[must_use]
    fn page(&self, pane: PaneId) -> usize {
        self.viewport(pane).height as usize
    }

    pub fn grid_mut(&mut self, tab: TabId) -> &mut GridUi {
        self.grids.entry(tab).or_default()
    }

    #[must_use]
    pub fn grid(&self, tab: TabId) -> Option<&GridUi> {
        self.grids.get(&tab)
    }

    /// The explorer's width for a screen `total` cells wide.
    #[must_use]
    pub fn explorer_width(&self, total: u16) -> u16 {
        let wanted = self.explorer_width.unwrap_or_else(|| {
            u16::try_from(u32::from(total) * DEFAULT_EXPLORER_PERMILLE / 1000).unwrap_or(total)
        });
        // The other side needs room too — and the splitter between them takes a
        // column of its own, which is why this is not `total - MIN_PANE_WIDTH`.
        // On a screen too narrow for both, the explorer is the one that yields.
        let ceiling = total.saturating_sub(MIN_PANE_WIDTH + 1);
        wanted.clamp(MIN_PANE_WIDTH.min(ceiling), ceiling)
    }

    /// How many rows the detail pane gets, which is none when it is closed or
    /// when the grid cannot spare them.
    #[must_use]
    pub fn detail_height(&self, total: u16) -> u16 {
        let Some(wanted) = self.detail_height else {
            return 0;
        };
        // The grid keeps its minimum whatever this asks for. Below that the
        // pane is not shown rather than shown small: the cell being read is
        // chosen in the grid, and a grid squeezed to its header cannot be used
        // to choose one.
        let ceiling = total.saturating_sub(MIN_GRID_HEIGHT);
        wanted.min(ceiling)
    }

    /// How many lines the pane has to scroll through, which only the frame
    /// that drew it knows.
    pub fn set_detail_rows(&mut self, rows: usize) {
        self.detail_rows = rows;
        // A shorter value must not leave the pane scrolled past its end,
        // showing an empty pane for a value that is there.
        self.detail_offset = self.detail_offset.min(rows.saturating_sub(1));
    }

    /// Called by the draw pass, which is the only thing that has counted the
    /// lines.
    pub fn set_sql_lines(&mut self, lines: usize) {
        self.sql_lines = lines;
        if let Some(grid) = self.active_grid_mut() {
            grid.row_offset = grid.row_offset.min(lines.saturating_sub(1));
        }
    }

    #[must_use]
    pub fn sql_offset(&self) -> usize {
        self.offset(PaneId::Grid)
    }

    #[must_use]
    pub fn detail_offset(&self) -> usize {
        self.detail_offset
    }

    #[must_use]
    pub fn detail_open(&self) -> bool {
        self.detail_height.is_some()
    }

    /// Open or close the pane.
    pub fn toggle_detail(&mut self) {
        self.detail_height = match self.detail_height {
            Some(_) => None,
            None => Some(DEFAULT_DETAIL_HEIGHT),
        };
    }

    /// Turn a new `last_error` on one of this screen's open previews into a
    /// toast.
    ///
    /// Recorded per preview rather than as one "last message raised": with a
    /// single slot, a second preview's failure hides behind the first one
    /// still sitting in it. Clearing on success is what keeps the *same*
    /// message failing again later — a second timeout, say — from being
    /// swallowed as a duplicate.
    pub fn raise_preview_errors(&mut self, snapshot: &Snapshot) {
        // Collected before mutating: `push_toast` needs `&mut self`.
        let keys: Vec<(ConnId, TableRef)> = self
            .tabs
            .iter()
            .filter_map(|t| Some((t.conn, t.table()?.clone())))
            .collect();

        for (conn, table) in keys {
            let Some(preview) = snapshot.preview(conn, &table) else {
                continue;
            };
            let key = (conn, table);
            match &preview.last_error {
                Some(text) if self.reported_preview_errors.get(&key) != Some(text) => {
                    self.reported_preview_errors.insert(key, text.clone());
                    self.push_toast(Severity::Error, text.clone());
                }
                None => {
                    self.reported_preview_errors.remove(&key);
                }
                Some(_) => {}
            }
        }
    }

    /// Say why a run produced nothing.
    ///
    /// Without this a statement the server — or `ValidatedSql` — refused is
    /// invisible: the pane falls back to the buffer because there are no rows,
    /// the spinner has already gone, and pressing Run reads as having done
    /// nothing at all.
    pub fn raise_query_errors(&mut self, snapshot: &Snapshot) {
        // Collected before mutating: `push_toast` needs `&mut self`.
        let failures: Vec<(QueryId, String)> = self
            .tabs
            .iter()
            .filter_map(|t| self.query_of(t.id))
            .filter(|id| !self.reported_query_errors.contains(id))
            .filter_map(|id| Some((id, snapshot.query(id)?.data.error()?.to_owned())))
            .collect();
        for (id, why) in failures {
            self.reported_query_errors.insert(id);
            self.push_toast(Severity::Error, why);
        }
    }

    /// Ask about a query the budget stopped, once per query.
    ///
    /// Once, because a snapshot is republished for reasons that have nothing
    /// to do with this — a spinner tick will do it — and a dialog re-raised on
    /// each one could never be dismissed.
    pub fn raise_approvals(&mut self, snapshot: &Snapshot) {
        // Only about a query one of this screen's tabs is showing: an agent
        // running something over the socket is not this person's question to
        // answer, and a dialog about it would appear out of nowhere.
        let mine: Vec<QueryId> = self
            .tabs
            .iter()
            .filter_map(|t| self.query_of(t.id))
            .collect();
        let asking = snapshot
            .queries
            .iter()
            .filter(|q| mine.contains(&q.id) && !self.asked_approval.contains(&q.id))
            .find_map(|q| {
                q.needs_approval
                    .as_ref()
                    .map(|over| (q.id, Arc::clone(over)))
            });
        let Some((id, over)) = asking else {
            // Nothing to ask. If a dialog is up about a question the store no
            // longer has open — because it was answered — it goes: the screen
            // follows the snapshot rather than closing itself, which is also
            // what keeps an answer from being given twice.
            if self.modal.as_ref().is_some_and(|m| !m.choices.is_empty()) {
                self.modal = None;
            }
            return;
        };
        self.asked_approval.insert(id);
        // The menu sits above the modal, so one left open would float over the
        // dialog — the same reason `raise_connection_failure` closes it.
        self.menu = None;
        self.modal = Some(crate::overlay::Modal::asking(
            "This query costs more than the limit",
            format!(
                "It would read {}, and the limit is {}.\n\n{}",
                bytes(over.estimate.bytes().unwrap_or(0)),
                bytes(over.budget),
                over.sql.text()
            ),
            vec![crate::overlay::Choice {
                label: "Run it anyway".to_owned(),
                intent: Action::ApproveQuery(id).into(),
            }],
        ));
    }

    /// A closed connection takes its *preview* tabs with it.
    ///
    /// `Disconnect` drops the connection's previews, and a tab left pointing
    /// at one shows a blank pane for ever: nothing to fetch, and no session
    /// left to fetch it with. Reopening it fetches the rows again, so nothing
    /// is lost by closing it.
    ///
    /// A SQL tab is kept. Its buffer is the user's own writing rather than a
    /// copy of something the database still has, and closing the connection is
    /// not a reason to throw away what somebody typed — it stays readable and
    /// copyable, and what it cannot do is run.
    pub fn close_disconnected_tabs(&mut self, snapshot: &Snapshot) {
        let closed: Vec<TabId> = self
            .tabs
            .iter()
            .filter(|t| t.table().is_some())
            .filter(|t| {
                snapshot
                    .connection(t.conn)
                    .is_none_or(|c| c.status == ConnStatus::Closed)
            })
            .map(|t| t.id)
            .collect();
        for id in closed {
            // Bound rather than asserted in place: `debug_assert!` does not
            // evaluate its argument in a release build, so closing the tab
            // would happen only in tests.
            let fetch = self.apply(ViewCmd::CloseTab(id), snapshot);
            debug_assert!(fetch.is_none(), "closing a tab fetches nothing");
        }
    }

    /// Move or set which section of the open definition is drawn.
    ///
    /// Clamped to what the definition actually has, and to nothing at all
    /// while it is still loading: a section index that outran the list would
    /// draw an empty grid under a title that is not there.
    fn select_section(&mut self, pick: crate::intent::SectionPick, snapshot: &Snapshot) {
        let Some(id) = self.active_tab else { return };
        let last = self
            .definition(snapshot)
            .map_or(0, |d| crate::definition::section_count(d).saturating_sub(1));
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) else {
            return;
        };
        let TabContent::Definition { section, .. } = &mut tab.content else {
            return;
        };
        *section = match pick {
            crate::intent::SectionPick::At(at) => at.min(last),
            crate::intent::SectionPick::By(delta) => step(*section, delta).min(last),
        };
        // The new section is a different grid, so the cursor and the scroll
        // that belonged to the old one do not mean anything in it.
        self.grids.remove(&id);
    }

    /// The definition the active tab is showing, once it has arrived.
    #[must_use]
    pub fn definition<'a>(&self, snapshot: &'a Snapshot) -> Option<&'a Arc<TableDetail>> {
        let id = self.active_tab?;
        let tab = self.tabs.iter().find(|t| t.id == id)?;
        snapshot.definition(tab.conn, tab.defines()?)?.data.ready()
    }

    /// The same, laid out for this screen and kept until it changes.
    ///
    /// Rebuilt only when the detail behind it is a different `Arc`, for the
    /// reason `GridUi::grid` is: a snapshot is republished for reasons that
    /// have nothing to do with this tab — a spinner tick will do it — and
    /// laying out every column again on each one is work with nothing to show
    /// for it.
    pub fn lay_out(&mut self, tab: TabId, detail: &Arc<TableDetail>) {
        let stale = self
            .laid
            .get(&tab)
            .is_none_or(|(held, _)| !Arc::ptr_eq(held, detail));
        if stale {
            self.laid
                .insert(tab, (Arc::clone(detail), Laid::out(detail)));
        }
    }

    /// What [`UiState::lay_out`] built for this tab, if it has been asked for.
    ///
    /// Separate from building it so the draw pass can read the layout through
    /// a shared borrow — one `&mut self` that stayed alive across the whole
    /// pane would have to be paid for with a clone of everything in it, the
    /// generated statement included.
    #[must_use]
    pub fn laid(&self, tab: TabId) -> Option<&Laid> {
        self.laid.get(&tab).map(|(_, laid)| laid)
    }

    /// Whether the active definition tab is on its generated statement, which
    /// the pane draws as text rather than as a grid.
    ///
    /// Clamped the way the draw pass clamps it, so a section index left over
    /// from a longer definition scrolls whatever is actually on the screen.
    fn statement_shown(&self, snapshot: &Snapshot) -> bool {
        let Some(definition) = self.definition(snapshot) else {
            return false;
        };
        let Some(section) = self.active_tab.and_then(|id| self.section_of(id)) else {
            return false;
        };
        // Asked of the detail rather than of a `Laid`: this is on the input
        // path, and laying one out would build every section's grid to answer
        // a boolean about one of them.
        let at = section.min(crate::definition::section_count(definition).saturating_sub(1));
        crate::definition::is_statement(definition, at)
    }

    /// Which section the active definition tab is on.
    #[must_use]
    pub fn section_of(&self, tab: TabId) -> Option<usize> {
        match &self.tabs.iter().find(|t| t.id == tab)?.content {
            TabContent::Definition { section, .. } => Some(*section),
            TabContent::Preview(_) | TabContent::Sql { .. } => None,
        }
    }

    /// Mint a tab and focus it.
    fn open(&mut self, conn: ConnId, content: TabContent) {
        self.next_tab += 1;
        let id = TabId::new(self.next_tab);
        self.tabs.push(OpenTab { id, conn, content });
        self.active_tab = Some(id);
    }

    /// One tab's SQL, or `None` when it is a preview or gone.
    #[must_use]
    pub fn buffer_of(&self, tab: TabId) -> Option<&str> {
        match &self.tabs.iter().find(|t| t.id == tab)?.content {
            TabContent::Sql { text, .. } => Some(text),
            TabContent::Preview(_) | TabContent::Definition { .. } => None,
        }
    }

    /// The run a tab is showing, if it has started one.
    #[must_use]
    pub fn query_of(&self, tab: TabId) -> Option<QueryId> {
        match &self.tabs.iter().find(|t| t.id == tab)?.content {
            TabContent::Sql { query, .. } => *query,
            TabContent::Preview(_) | TabContent::Definition { .. } => None,
        }
    }

    /// Record which run a tab started, so its rows can be found again.
    pub fn set_query(&mut self, tab: TabId, id: QueryId) {
        if let Some(TabContent::Sql { query, .. }) = self
            .tabs
            .iter_mut()
            .find(|t| t.id == tab)
            .map(|t| &mut t.content)
        {
            *query = Some(id);
        }
    }

    /// Replace one tab's SQL with what came back from the editor.
    ///
    /// By id rather than "the active one": the editor had the terminal, and a
    /// snapshot arriving while it ran can have closed the tab underneath it.
    pub fn set_buffer(&mut self, tab: TabId, text: String) {
        if let Some(TabContent::Sql { text: buffer, .. }) = self
            .tabs
            .iter_mut()
            .find(|t| t.id == tab)
            .map(|t| &mut t.content)
        {
            *buffer = text;
        }
    }

    /// Put a template in the buffer, or ask for what it needs first.
    ///
    /// Which of the two is a fact about the template rather than about the
    /// gesture, so one command covers both: somebody picking a saved statement
    /// is aiming at the buffer either way.
    fn use_template(&mut self, id: TemplateId, snapshot: &Snapshot) {
        let Some(template) = snapshot
            .templates
            .data
            .ready()
            .and_then(|held| held.iter().find(|t| t.id == id))
            .cloned()
        else {
            return;
        };
        let dialect = self.dialect(snapshot);
        match sqlake_core::template::placeholders(&template.body, dialect) {
            Ok(placeholders) if placeholders.is_empty() => {
                self.palette = None;
                self.insert(template.body, snapshot);
            }
            Ok(placeholders) => {
                // What the explorer has selected answers `{{ident:table}}`
                // before anybody types — shown and editable, because a guess
                // about what somebody meant is only ever a guess.
                let selected = self.selected_table(snapshot);
                if let Some(palette) = &mut self.palette {
                    palette.asking = Some(crate::palette::Form::new(
                        id,
                        template.name.clone(),
                        placeholders,
                        &|placeholder| {
                            (placeholder.kind == sqlake_core::template::Kind::Ident
                                && placeholder.name == "table")
                                .then(|| selected.clone())
                                .flatten()
                        },
                    ));
                }
            }
            // A template whose body cannot be read at all — an unterminated
            // string in it, or a `{{` inside one. Said where the palette is
            // rather than as a toast: it is about the thing under the cursor.
            Err(why) => {
                if let Some(palette) = &mut self.palette {
                    palette.asking = Some(crate::palette::Form {
                        template: id,
                        name: template.name.clone(),
                        fields: Vec::new(),
                        at: 0,
                        failed: Some(why.to_string()),
                    });
                }
            }
        }
    }

    /// Keep what the palette is holding, under the name typed into it.
    ///
    /// The palette closes here rather than when the answer lands: the answer
    /// is a list and a possible failure, and both are read where the palette
    /// was — a list somebody has to dismiss to see what happened to their save
    /// is a list in the way.
    fn commit_template(&mut self, snapshot: &Snapshot) -> Option<Action> {
        let open = self.palette.take()?;
        let name = open.filter.trim().to_owned();
        let body = open.saving_body()?.to_owned();
        if name.is_empty() {
            return None;
        }
        let with = sqlake_core::library::NewTemplate {
            name: name.clone(),
            body,
            // Any driver, and no tags. Both are things to say *about* a
            // template rather than things to ask for while keeping one, and a
            // dialog with three fields in it is a dialog nobody uses to save a
            // query they are in the middle of.
            driver: None,
            tags: Vec::new(),
        };

        // A name already saved is an edit, and an edit is a thing to be asked
        // about: the store refuses a duplicate name, so without this the only
        // way to change a saved statement would be to delete it and write it
        // again — and the refusal would arrive as a failure for something
        // somebody meant to do.
        match snapshot
            .templates
            .data
            .ready()
            .and_then(|held| held.iter().find(|t| t.name == name))
        {
            Some(existing) => {
                self.modal = Some(crate::overlay::Modal::asking(
                    format!("Replace `{name}`?"),
                    "What is saved under that name goes.",
                    vec![crate::overlay::Choice {
                        label: "Replace".to_owned(),
                        intent: Action::ReplaceTemplate {
                            id: existing.id,
                            with,
                        }
                        .into(),
                    }],
                ));
                None
            }
            None => Some(Action::SaveTemplate(with)),
        }
    }

    /// Bind what the form holds and put the result in the buffer.
    fn submit_template(&mut self, snapshot: &Snapshot) {
        let Some(form) = self.palette.as_ref().and_then(|p| p.asking.clone()) else {
            return;
        };
        let Some(body) = snapshot
            .templates
            .data
            .ready()
            .and_then(|held| held.iter().find(|t| t.id == form.template))
            .map(|t| t.body.clone())
        else {
            return;
        };
        let dialect = self.dialect(snapshot);
        match sqlake_core::template::BoundTemplate::bind(&body, &form.values(), dialect) {
            Ok(bound) => {
                self.palette = None;
                self.insert(bound.text().to_owned(), snapshot);
            }
            // Stays open with the reason on it: the answers are still there to
            // be corrected, which is the whole reason binding happens before
            // the text reaches the buffer.
            Err(why) => {
                if let Some(asking) = self.palette.as_mut().and_then(|p| p.asking.as_mut()) {
                    asking.failed = Some(why.to_string());
                }
            }
        }
    }

    /// Put a statement where it can be run.
    ///
    /// Into the active SQL tab when it is empty, and into a new one otherwise.
    /// Never over something already written: there is no editor here and so no
    /// undo, and a template that replaced a half-written query would be a
    /// keystroke that destroys work. Appending instead would make two
    /// statements out of one, which `ValidatedSql` then refuses.
    fn insert(&mut self, text: String, snapshot: &Snapshot) {
        let empty = self.active_tab.filter(|id| {
            self.buffer_of(*id)
                .is_some_and(|held| held.trim().is_empty())
        });
        if let Some(tab) = empty {
            self.set_buffer(tab, text);
            return;
        }
        let Some(conn) = self.connection_for_a_new_tab(snapshot) else {
            self.warn("nothing is connected to run a statement on");
            return;
        };
        let _ = self.apply(ViewCmd::OpenSqlTab { conn }, snapshot);
        if let Some(tab) = self.active_tab {
            self.set_buffer(tab, text);
        }
    }

    /// The connection a new SQL tab would open on: the active tab's, or the
    /// first live one. A template is about a statement, not about a tab, so
    /// picking one is better than refusing when there is an obvious answer.
    fn connection_for_a_new_tab(&self, snapshot: &Snapshot) -> Option<ConnId> {
        self.active_tab
            .and_then(|id| self.tabs.iter().find(|t| t.id == id))
            .map(|t| t.conn)
            .filter(|conn| {
                snapshot
                    .connection(*conn)
                    .is_some_and(ConnectionView::is_live)
            })
            .or_else(|| {
                snapshot
                    .connections
                    .iter()
                    .find(|c| c.is_live())
                    .map(|c| c.id)
            })
    }

    /// How the connection the statement is going to quotes things.
    ///
    /// The active tab's connection, falling back to PostgreSQL's rules. A
    /// fallback is needed because a palette can be opened with nothing
    /// connected, and the alternative — refusing to fill in a template until
    /// something is — would be a rule about quoting standing in the way of
    /// writing a statement.
    fn dialect(&self, snapshot: &Snapshot) -> sqlake_core::template::Dialect {
        self.active_tab
            .and_then(|id| self.tabs.iter().find(|t| t.id == id))
            .and_then(|tab| snapshot.connection(tab.conn))
            .and_then(|c| c.capabilities.as_ref())
            .map_or(
                sqlake_core::template::Dialect {
                    quote_style: sqlake_core::capability::QuoteStyle::DoubleQuote,
                    escaping: sqlake_core::capability::Escaping::None,
                },
                sqlake_core::template::Dialect::from,
            )
    }

    /// The relation the explorer has selected, as a dotted name.
    fn selected_table(&self, snapshot: &Snapshot) -> Option<String> {
        let row = snapshot.explorer.nodes.get(self.tree.selected?)?;
        row.node_ref.as_table().map(|table| table.to_string())
    }

    /// A passing notice, for something that went differently rather than
    /// wrongly.
    pub fn warn(&mut self, text: impl Into<String>) {
        self.push_toast(Severity::Warning, text);
    }

    /// A dialog, for something that has to be read before carrying on.
    pub fn raise_error(&mut self, title: impl Into<String>, body: impl Into<String>) {
        // The menu sits above the modal, so one left open would float over the
        // dialog — the same reason `raise_connection_failure` closes it.
        self.menu = None;
        self.modal = Some(crate::overlay::Modal::error(title, body));
    }

    /// Whether the active tab has something to run, and somewhere to run it.
    ///
    /// The same question the input layer asks before producing the action, so
    /// the button is drawn exactly when pressing it would do something.
    #[must_use]
    pub fn can_run(&self, snapshot: &Snapshot) -> bool {
        let Some(tab) = self
            .active_tab
            .and_then(|id| self.tabs.iter().find(|t| t.id == id))
        else {
            return false;
        };
        let TabContent::Sql { text, .. } = &tab.content else {
            return false;
        };
        !text.trim().is_empty()
            && snapshot
                .connection(tab.conn)
                .is_some_and(ConnectionView::is_live)
    }

    /// The active tab's SQL, or `None` when it is a preview.
    #[must_use]
    pub fn active_sql(&self) -> Option<&str> {
        self.buffer_of(self.active_tab?)
    }

    fn push_toast(&mut self, severity: Severity, text: impl Into<String>) {
        self.next_toast += 1;
        self.toasts.push(Toast {
            id: ToastId::new(self.next_toast),
            text: text.into(),
            severity,
            created_at: Instant::now(),
        });
    }

    /// Apply a view command, synchronously and without touching the store.
    ///
    /// Returns the one action a view command can cause: moving towards the end
    /// of a preview asks for the next page. It stays a return value rather than
    /// a dispatch from in here so that this type still has no way to reach the
    /// store, and so the caller can see which commands fetch.
    #[must_use]
    pub fn apply(&mut self, cmd: ViewCmd, snapshot: &Snapshot) -> Option<Action> {
        match cmd {
            ViewCmd::FocusPane(pane) => self.focus = pane,
            ViewCmd::FocusNextPane => self.cycle_focus(1),
            ViewCmd::FocusPrevPane => self.cycle_focus(-1),

            ViewCmd::ScrollBy { pane, delta } => {
                let offset = self.offset(pane);
                self.set_offset(pane, step(offset, delta), snapshot);
                return self.wants_a_page(pane, snapshot);
            }
            ViewCmd::ScrollToRatio { pane, permille } => {
                let span = self.scrollable(pane, snapshot);
                let target = span * usize::from(permille.min(1000)) / 1000;
                self.set_offset(pane, target, snapshot);
                return self.wants_a_page(pane, snapshot);
            }
            ViewCmd::ScrollToStart(pane) => self.set_offset(pane, 0, snapshot),
            ViewCmd::ScrollToEnd(pane) => {
                self.set_offset(pane, usize::MAX, snapshot);
                return self.wants_a_page(pane, snapshot);
            }
            ViewCmd::ScrollXBy { delta } => {
                let columns = self.column_count(snapshot);
                if let Some(grid) = self.active_grid_mut() {
                    grid.col_offset = step(grid.col_offset, delta).min(columns.saturating_sub(1));
                }
            }

            ViewCmd::SetFilter(filter) => self.set_filter(filter, snapshot),
            ViewCmd::SelectTreeRow(index) => self.select_tree_row(index, snapshot),
            ViewCmd::MoveTreeSelection(delta) => {
                let from = self.tree.selected.unwrap_or(0);
                // A first press with nothing selected lands on row zero rather
                // than on row one.
                let to = if self.tree.selected.is_none() {
                    0
                } else {
                    step(from, delta)
                };
                self.select_tree_row(to, snapshot);
            }

            // The cursor drags the viewport along with it, so `J` reaches the
            // last loaded row exactly as a scroll does. Left out, the whole
            // feature is missing from the keyboard.
            // Moving the cursor ends a selection; extending it keeps the
            // anchor. That is the whole difference between the two pairs.
            ViewCmd::SelectCell { row, col } => {
                self.clear_anchor();
                self.select_cell(row, col, snapshot);
                return self.wants_a_page(PaneId::Grid, snapshot);
            }
            ViewCmd::MoveCellSelection { drow, dcol } => {
                let (row, col) = self.active_grid().map_or((0, 0), |g| (g.row, g.col));
                self.clear_anchor();
                self.select_cell(step(row, drow), step(col, dcol), snapshot);
                return self.wants_a_page(PaneId::Grid, snapshot);
            }
            ViewCmd::ExtendCellSelection { drow, dcol } => {
                let (row, col) = self.active_grid().map_or((0, 0), |g| (g.row, g.col));
                self.anchor_here(snapshot);
                self.select_cell(step(row, drow), step(col, dcol), snapshot);
                return self.wants_a_page(PaneId::Grid, snapshot);
            }
            ViewCmd::ExtendCellSelectionTo { row, col } => {
                self.anchor_here(snapshot);
                self.select_cell(row, col, snapshot);
                return self.wants_a_page(PaneId::Grid, snapshot);
            }

            ViewCmd::ResizeColumn { col, delta } => {
                let natural = self.natural_width(col, snapshot);
                if let Some(grid) = self.active_grid_mut() {
                    grid.resize(col, delta, natural);
                }
            }
            ViewCmd::MoveSplit { split, delta } => {
                let SplitId::Explorer = split;
                let current = self.explorer_width(self.screen.width);
                let wanted = i32::from(current) + i32::from(delta);
                self.explorer_width = Some(wanted.clamp(0, i32::from(u16::MAX)) as u16);
            }
            ViewCmd::EvenSplit(split) => {
                let SplitId::Explorer = split;
                self.explorer_width = None;
            }

            ViewCmd::OpenMenu { at, ranged } => {
                // A gesture with no coordinates gets the top-left of the grid,
                // which is at least inside the pane the menu is about. The
                // screen origin would put it over the tab bar.
                let at = at.unwrap_or_else(|| {
                    let grid = self.viewport(PaneId::Grid);
                    (grid.x, grid.y)
                });
                self.menu = Some(crate::menu::Menu::for_grid(at, ranged));
            }
            ViewCmd::CloseMenu => self.menu = None,
            ViewCmd::ToggleDetail => self.toggle_detail(),
            ViewCmd::Copy { format, all } => self.copy(format, all, snapshot),
            ViewCmd::DismissModal => self.modal = None,

            // A relation already open is raised, not duplicated — the same
            // rule the store used to apply when tabs lived there.
            ViewCmd::OpenTab { conn, table } => {
                if let Some(existing) = self
                    .tabs
                    .iter()
                    .find(|t| t.conn == conn && t.table() == Some(&table))
                {
                    self.active_tab = Some(existing.id);
                } else {
                    self.open(conn, TabContent::Preview(table));
                }
            }
            // Raised rather than duplicated, the same way a preview is: one
            // definition of one relation is all there is to look at.
            ViewCmd::OpenDefinition { conn, table } => {
                if let Some(existing) = self
                    .tabs
                    .iter()
                    .find(|t| t.conn == conn && t.defines() == Some(&table))
                {
                    self.active_tab = Some(existing.id);
                } else {
                    self.open(conn, TabContent::Definition { table, section: 0 });
                }
            }
            ViewCmd::SelectSection(pick) => self.select_section(pick, snapshot),

            // Never raised onto an existing one, unlike a preview: two SQL
            // tabs on one connection are two different questions, and there is
            // nothing to match them on anyway.
            ViewCmd::OpenSqlTab { conn } => {
                self.next_sql += 1;
                self.open(
                    conn,
                    TabContent::Sql {
                        number: self.next_sql,
                        text: String::new(),
                        query: None,
                    },
                );
            }
            ViewCmd::SelectTab(id) => {
                if self.tabs.iter().any(|t| t.id == id) {
                    self.active_tab = Some(id);
                }
            }
            ViewCmd::CloseTab(id) => {
                let position = self.tabs.iter().position(|t| t.id == id);
                self.tabs.retain(|t| t.id != id);
                self.laid.remove(&id);
                // The cached grid and column widths belonged to this tab and
                // nothing else; without dropping them a long session
                // accumulates a `GridUi` — and the `RenderedGrid` it caches —
                // for every tab ever opened.
                self.grids.remove(&id);
                if self.active_tab == Some(id) {
                    // Select the neighbour, which is what every tabbed UI does.
                    self.active_tab = position
                        .and_then(|p| self.tabs.get(p.min(self.tabs.len().saturating_sub(1))))
                        .map(|t| t.id);
                }
            }
            ViewCmd::DismissToast(id) => self.toasts.retain(|t| t.id != id),
            ViewCmd::Palette(open) => {
                let asked_for = open.is_some();
                self.palette = open;
                // Reading them is what opening it means. Asked for here rather
                // than by the key, so that every way of opening the palette —
                // to use one, to save one — reads the list without each of
                // them having to remember to.
                if asked_for {
                    return Some(Action::LoadTemplates);
                }
            }
            ViewCmd::CommitTemplate => return self.commit_template(snapshot),
            ViewCmd::UseTemplate(id) => self.use_template(id, snapshot),
            ViewCmd::SubmitTemplate => self.submit_template(snapshot),
            ViewCmd::ConfirmDeleteTemplate { id, name } => {
                // The palette goes first: the dialog is drawn over everything,
                // and a list still underneath it is a list the answer is about
                // to change.
                self.palette = None;
                self.modal = Some(crate::overlay::Modal::asking(
                    format!("Delete `{name}`?"),
                    "It is not kept anywhere else.",
                    vec![crate::overlay::Choice {
                        label: "Delete".to_owned(),
                        intent: Action::DeleteTemplate(id).into(),
                    }],
                ));
            }
        }
        None
    }

    /// Whether this move should fetch, and the action if so.
    ///
    /// Only reached from a move somebody made. A predicate the render loop
    /// checked each frame would fetch on its own: `page_size` goes down to a
    /// single row, so a viewport taller than a page is still "near the end" the
    /// moment the page lands, and a relation would walk itself to the end with
    /// nobody touching the wheel.
    fn wants_a_page(&mut self, pane: PaneId, snapshot: &Snapshot) -> Option<Action> {
        if pane != PaneId::Grid {
            return None;
        }
        let id = self.active_tab?;
        let at = self.tabs.iter().position(|t| t.id == id)?;
        let conn = self.tabs[at].conn;
        let table = self.tabs[at].table()?.clone();
        let preview = snapshot.preview(conn, &table)?;
        // A first page still in flight is not something to hurry along, and its
        // `loaded_rows` of zero would otherwise read as "at the end".
        if !matches!(preview.data, LoadState::Ready(_)) {
            return None;
        }
        // The relation's end is the store's answer, not an inference from a
        // request that changed nothing — which is also what a cancelled page
        // and a repeat of the same failure leave behind.
        if preview.exhausted {
            return None;
        }
        if self.offset(PaneId::Grid) + self.page(PaneId::Grid) + LOAD_MARGIN_ROWS
            < preview.loaded_rows
        {
            return None;
        }

        let attempts = preview.attempts;
        let grid = self.grids.entry(id).or_default();
        if grid.asked_after == Some(attempts) {
            return None;
        }
        grid.asked_after = Some(attempts);
        Some(Action::LoadMore { conn, table })
    }

    fn cycle_focus(&mut self, delta: i32) {
        let at = FOCUS_ORDER.iter().position(|p| *p == self.focus);
        let len = FOCUS_ORDER.len();
        let next = match at {
            Some(i) => (i + if delta < 0 { len - 1 } else { 1 }) % len,
            // Focus was on a pane outside the cycle, so start at the beginning.
            None => 0,
        };
        self.focus = FOCUS_ORDER[next];
    }

    fn offset(&self, pane: PaneId) -> usize {
        match pane {
            PaneId::Explorer => self.tree.offset,
            PaneId::Grid => self.active_grid().map_or(0, |g| g.row_offset),
            PaneId::Detail => self.detail_offset,
            PaneId::TabBar | PaneId::StatusBar => 0,
        }
    }

    /// How far the pane can be scrolled: content beyond one screenful.
    fn scrollable(&self, pane: PaneId, snapshot: &Snapshot) -> usize {
        self.content_rows(pane, snapshot)
            .saturating_sub(self.page(pane))
    }

    fn content_rows(&self, pane: PaneId, snapshot: &Snapshot) -> usize {
        match pane {
            PaneId::Explorer => self.visible_len(snapshot),
            // A SQL tab reuses the grid pane's offset rather than keeping one
            // of its own, so `j`, `PageDown` and `G` all work in it with
            // nothing added — only the count of what is being scrolled
            // through differs.
            // The definition tab's DDL section is drawn by the same text
            // renderer and so is measured the same way: its section has no
            // rows at all, and counting those would pin it to the first
            // screenful of a statement that is usually longer than one.
            PaneId::Grid if self.active_sql().is_some() || self.statement_shown(snapshot) => {
                self.sql_lines
            }
            PaneId::Grid => self.row_count(snapshot),
            PaneId::Detail => self.detail_rows,
            PaneId::TabBar | PaneId::StatusBar => 0,
        }
    }

    fn set_offset(&mut self, pane: PaneId, to: usize, snapshot: &Snapshot) {
        let clamped = to.min(self.scrollable(pane, snapshot));
        match pane {
            PaneId::Explorer => self.tree.offset = clamped,
            PaneId::Grid => {
                if let Some(grid) = self.active_grid_mut() {
                    grid.row_offset = clamped;
                }
            }
            PaneId::Detail => self.detail_offset = clamped,
            PaneId::TabBar | PaneId::StatusBar => {}
        }
    }

    fn active_grid(&self) -> Option<&GridUi> {
        self.grids.get(&self.active_tab?)
    }

    fn active_grid_mut(&mut self) -> Option<&mut GridUi> {
        let tab = self.active_tab?;
        Some(self.grids.entry(tab).or_default())
    }

    /// The rows the explorer is showing, as indices into the flattened tree.
    ///
    /// Both drawing and input go through this, so a click lands on the row the
    /// user is looking at rather than on whatever is at that position in the
    /// unfiltered tree.
    #[must_use]
    pub fn visible_rows(&self, snapshot: &Snapshot) -> Vec<usize> {
        crate::tree::visible(
            &snapshot.explorer.nodes,
            self.filter.as_ref().map(|f| f.text.as_str()),
        )
    }

    /// The node a visible row points at.
    #[must_use]
    pub fn visible_node<'a>(&self, snapshot: &'a Snapshot, row: usize) -> Option<&'a VisibleNode> {
        snapshot
            .explorer
            .get(*self.visible_rows(snapshot).get(row)?)
    }

    /// Set the filter, keeping the same *node* selected rather than the same
    /// row number.
    ///
    /// Filtering renumbers every row under the first one it removes, so a
    /// selection left where it was would slide onto something the user never
    /// pointed at — and then `Enter` opens it. When the selected node is
    /// filtered out there is nothing to follow, and the first row is where a
    /// search leaves you anyway.
    fn set_filter(&mut self, filter: Option<Filter>, snapshot: &Snapshot) {
        let was = self
            .tree
            .selected
            .and_then(|row| self.visible_node(snapshot, row))
            .map(|node| (node.conn, node.node_ref.clone()));

        self.filter = filter;

        // Once, and reused: this runs on every keystroke, and each call walks
        // the whole tree lower-casing labels.
        let rows = self.visible_rows(snapshot);
        let now = was.and_then(|(conn, node_ref)| {
            rows.iter().position(|&index| {
                snapshot
                    .explorer
                    .get(index)
                    .is_some_and(|node| node.conn == conn && node.node_ref == node_ref)
            })
        });
        match now {
            Some(row) => self.select_tree_row(row, snapshot),
            None if rows.is_empty() => self.tree.selected = None,
            None => self.select_tree_row(0, snapshot),
        }
    }

    fn visible_len(&self, snapshot: &Snapshot) -> usize {
        self.visible_rows(snapshot).len()
    }

    fn select_tree_row(&mut self, index: usize, snapshot: &Snapshot) {
        let len = self.visible_len(snapshot);
        if len == 0 {
            self.tree.selected = None;
            return;
        }
        let index = index.min(len - 1);
        self.tree.selected = Some(index);
        // Keep the selection on screen, which is the whole reason selection and
        // scrolling are not independent.
        self.tree.offset = scroll_into_view(self.tree.offset, index, self.page(PaneId::Explorer));
    }

    /// The sequence the render loop should write, if there is one.
    pub fn take_copy(&mut self) -> Option<String> {
        self.pending_copy.take()
    }

    /// Put the selection — or the whole result — on the clipboard.
    ///
    /// What is reported afterwards is what was *sent*. OSC 52 has no reply, and
    /// a terminal with clipboard writes turned off — tmux without
    /// `set-clipboard on` is the common one — swallows the sequence silently,
    /// so a message saying "copied" would be a claim this cannot check.
    fn copy(&mut self, format: crate::copy::Format, all: bool, snapshot: &Snapshot) {
        let Some(rows) = self.rows_of(snapshot) else {
            return;
        };
        let sort = self.active_sort(snapshot);
        let asked = match (all, self.active_grid()) {
            (true, _) => (
                0,
                0,
                rows.row_count().saturating_sub(1),
                rows.columns().len().saturating_sub(1),
            ),
            (false, Some(grid)) => grid.selection(sort),
            (false, None) => return,
        };
        // Counted from the rectangle that is there rather than the one asked
        // for: a selection outlives a result that shrank under it, and the
        // count is what the message claims was sent.
        let Some(area) = crate::copy::clamped(&rows, asked) else {
            self.push_toast(Severity::Info, "nothing to copy");
            return;
        };

        let text = crate::copy::render(&rows, format, area);
        let cells = (area.2 - area.0 + 1) * (area.3 - area.1 + 1);
        match crate::copy::sequence(&text) {
            Ok(sequence) => {
                self.pending_copy = Some(sequence);
                self.push_toast(
                    Severity::Info,
                    format!("sent {cells} cells to the clipboard"),
                );
            }
            Err(crate::copy::Refused::Empty) => {
                self.push_toast(Severity::Info, "nothing to copy");
            }
            // Refused rather than sent: a sequence past the terminal's limit is
            // dropped whole, so sending it would report a copy that never
            // happened.
            Err(crate::copy::Refused::TooLarge { bytes }) => self.push_toast(
                Severity::Warning,
                format!(
                    "{cells} cells is {}KB, past what a terminal takes in one go — select fewer",
                    bytes / 1024
                ),
            ),
        }
    }

    fn clear_anchor(&mut self) {
        if let Some(grid) = self.active_grid_mut() {
            grid.anchor = None;
        }
    }

    /// Start a selection at the cursor if there is not one already.
    ///
    /// The ordering is recorded with it, so a sort — which fetches page one
    /// again under a different order — leaves an anchor that no longer names
    /// the row it was put on, and `selection` reads it as absent.
    fn anchor_here(&mut self, snapshot: &Snapshot) {
        let sort = self.active_sort(snapshot);
        if let Some(grid) = self.active_grid_mut()
            && grid.anchor.is_none()
        {
            grid.anchor = Some(((grid.row, grid.col), sort));
        }
    }

    #[must_use]
    pub fn active_sort(&self, snapshot: &Snapshot) -> Option<Sort> {
        let id = self.active_tab?;
        let tab = self.tabs.iter().find(|t| t.id == id)?;
        snapshot.preview(tab.conn, tab.table()?)?.sort
    }

    fn select_cell(&mut self, row: usize, col: usize, snapshot: &Snapshot) {
        let rows = self.row_count(snapshot);
        let cols = self.column_count(snapshot);
        if rows == 0 || cols == 0 {
            return;
        }
        let (row, col) = (row.min(rows - 1), col.min(cols - 1));
        let page = self.page(PaneId::Grid);
        let leftmost = self.leftmost_visible(col, snapshot);
        if let Some(grid) = self.active_grid_mut() {
            grid.row = row;
            grid.col = col;
            grid.row_offset = scroll_into_view(grid.row_offset, row, page);
            // Left of the offset the cursor is scrolled back to; right of the
            // last column that fits, forward to. Leaving the second one out
            // walks the cursor off the edge and nothing follows it.
            grid.col_offset = grid.col_offset.clamp(leftmost, col);
        }
    }

    /// The furthest left the grid can be scrolled while `col` is still drawn.
    fn leftmost_visible(&mut self, col: usize, snapshot: &Snapshot) -> usize {
        let available = usize::from(self.viewport(PaneId::Grid).width);
        if available == 0 {
            // No frame has been drawn yet, so nothing is known about what fits.
            // Scrolling on a guess would push the first columns off the screen
            // before the screen exists.
            return 0;
        }
        let mut used = 0;
        let mut first = col;
        for c in (0..=col).rev() {
            let natural = self.natural_width(c, snapshot);
            let drawn = self.active_grid().map_or(natural, |g| g.width(c, natural));
            // One cell for the separator that follows every column.
            used += usize::from(drawn) + 1;
            // The cursor's own column is kept even when it is wider than the
            // pane: there is nowhere better to put it.
            if used > available && c != col {
                break;
            }
            first = c;
        }
        first
    }

    /// The rows behind the active tab.
    ///
    /// `None` covers a real frame: a tab opened this tick has nothing in the
    /// snapshot until the store answers.
    fn rows_of(&self, snapshot: &Snapshot) -> Option<Arc<PagedResult>> {
        let tab = self.active_tab?;
        let open = self.tabs.iter().find(|t| t.id == tab)?;
        match &open.content {
            TabContent::Preview(table) => snapshot.preview(open.conn, table)?.data.ready().cloned(),
            // A run this screen started. `None` while it is still running, or
            // before there has been one, which is what makes the pane show the
            // buffer instead.
            TabContent::Sql { query, .. } => snapshot.query((*query)?)?.data.ready().cloned(),
            // The section this tab is looking at, which is a different grid
            // per tab even for the same relation.
            TabContent::Definition { table, section } => {
                let detail = snapshot.definition(open.conn, table)?.data.ready()?;
                match self.laid.get(&tab) {
                    // What the last draw built, when it was built from this
                    // same detail.
                    Some((held, laid)) if Arc::ptr_eq(held, detail) => laid.rows(*section).cloned(),
                    // And otherwise laid out here, thrown away, and built
                    // again by the next draw — which is the whole cost, and is
                    // paid once per refresh rather than per event. Returning
                    // nothing instead would drop a scroll or a copy already
                    // queued behind a `--refresh`, silently.
                    _ => Laid::out(detail).rows(*section).cloned(),
                }
            }
        }
    }

    fn row_count(&self, snapshot: &Snapshot) -> usize {
        let Some(rows) = self.rows_of(snapshot) else {
            return 0;
        };
        rows.row_count()
    }

    fn column_count(&self, snapshot: &Snapshot) -> usize {
        let Some(rows) = self.rows_of(snapshot) else {
            return 0;
        };
        rows.columns().len()
    }

    fn natural_width(&mut self, col: usize, snapshot: &Snapshot) -> u16 {
        let Some(rows) = self.rows_of(snapshot) else {
            return 0;
        };
        let Some(tab) = self.active_tab else {
            return 0;
        };
        let grid = self.grids.entry(tab).or_default();
        grid.grid(&rows)
            .columns()
            .get(col)
            .map_or(0, |c| c.natural_width)
    }
}

/// Apply a signed delta to an index without wrapping past zero.
fn step(from: usize, delta: i32) -> usize {
    if delta < 0 {
        from.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        from.saturating_add(delta as usize)
    }
}

/// The smallest scroll that keeps `index` visible.
fn scroll_into_view(offset: usize, index: usize, page: usize) -> usize {
    if page == 0 {
        return offset;
    }
    if index < offset {
        index
    } else if index >= offset + page {
        index + 1 - page
    } else {
        offset
    }
}

#[cfg(test)]
mod tests {
    use sqlake_driver_mock::mock_summary;
    use std::sync::Arc;

    use sqlake_app::snapshot::{LoadState, PreviewView, QueryView};
    use sqlake_app::tree::{NodeState, VisibleNode};
    use sqlake_core::id::ConnId;
    use sqlake_core::node::{NodeKind, NodeRef, TableRef};
    use sqlake_core::result::{Column, ResultSet, Row};
    use sqlake_core::value::Value;

    use super::*;

    fn table() -> TableRef {
        TableRef::new(["public", "users"])
    }

    /// A query the budget stopped, as the store would publish it.
    fn over_budget(conn: ConnId, id: QueryId) -> QueryView {
        let sql = sqlake_core::sql::ValidatedSql::parse(
            &sqlake_core::sql::RawSql::new("select * from big"),
            sqlake_core::capability::Escaping::None,
        )
        .expect("one statement");
        QueryView {
            id,
            conn,
            sql: sql.text().to_owned(),
            estimate: Some(sqlake_core::sql::Estimate::Bytes(5_000_000_000)),
            needs_approval: Some(Arc::new(sqlake_core::sql::OverBudget {
                sql,
                max_rows: None,
                estimate: sqlake_core::sql::Estimate::Bytes(5_000_000_000),
                budget: 1_000_000_000,
            })),
            data: LoadState::Idle,
            failed_at: None,
            started_at: std::time::Instant::now(),
            took: None,
        }
    }

    fn rows(count: usize, columns: usize) -> Arc<PagedResult> {
        let cols: Vec<Column> = (0..columns)
            .map(|c| Column::new(format!("c{c}"), "text", false))
            .collect();
        let data: Vec<Row> = (0..count)
            .map(|r| Row((0..columns).map(|_| Value::Int(r as i64)).collect()))
            .collect();
        Arc::new(PagedResult::new(&ResultSet::new(cols, data, None)))
    }

    fn snapshot(conn: ConnId, tree_rows: usize, grid_rows: usize, grid_cols: usize) -> Snapshot {
        let explorer = Arc::new(TreeView {
            nodes: (0..tree_rows)
                .map(|i| VisibleNode {
                    conn,
                    depth: 0,
                    label: format!("n{i}"),
                    node_ref: NodeRef::new(NodeKind::Namespace, [format!("n{i}")]),
                    relation_kind: None,
                    state: NodeState::Collapsed,
                })
                .collect(),
        });

        Snapshot {
            rev: 1,
            applied: 0,
            profiles: Arc::new(vec![mock_summary("mock")]),
            connections: vec![sqlake_app::snapshot::ConnectionView {
                id: conn,
                profile: mock_summary("mock").id,
                name: "mock".into(),
                color: None,
                kind: sqlake_core::capability::DriverKind::Mock,
                status: sqlake_app::snapshot::ConnStatus::Ready,
                capabilities: None,
                tree: std::sync::Arc::default(),
            }],
            explorer,
            definitions: Vec::new(),
            previews: vec![PreviewView {
                exhausted: false,
                attempts: 0,
                conn,
                table: table(),
                sort: None,
                loaded_rows: grid_rows,
                data: LoadState::Ready(rows(grid_rows, grid_cols)),
                last_error: None,
            }],
            queries: Vec::new(),
            busy: Vec::new(),
            templates: sqlake_app::snapshot::TemplatesView::default(),
            should_quit: false,
        }
    }

    /// A snapshot and a `UiState` with its one preview already open as tab
    /// `TabId::new(1)` — what the render loop would have done on the frame
    /// that first drew it.
    /// A search that has been made, which is the state the tree is filtered
    /// in — the box's own editing state changes no rows.
    fn search(text: &str) -> Filter {
        Filter {
            text: text.to_owned(),
            editing: false,
        }
    }

    fn setup(tree_rows: usize, grid_rows: usize, grid_cols: usize) -> (Snapshot, UiState) {
        let conn = ConnId::new();
        let snap = snapshot(conn, tree_rows, grid_rows, grid_cols);
        let mut ui = UiState::new();
        // As a frame would leave it: the pane viewports are the areas inside
        // the borders, the screen is the whole of it.
        ui.set_screen(Rect::new(0, 0, 82, 12));
        ui.set_viewport(PaneId::Explorer, Rect::new(0, 1, 20, 10));
        ui.set_viewport(PaneId::Grid, Rect::new(21, 1, 60, 10));
        let _ = ui.apply(
            ViewCmd::OpenTab {
                conn,
                table: table(),
            },
            &snap,
        );
        (snap, ui)
    }

    #[test]
    fn a_menu_opened_without_coordinates_lands_on_the_grid() {
        // The screen origin is the tab bar, which is not what the menu is
        // about — and a key press has no coordinates of its own to offer.
        let (snap, mut ui) = setup(3, 5, 3);
        let grid = ui.viewport(PaneId::Grid);
        let _ = ui.apply(
            ViewCmd::OpenMenu {
                at: None,
                ranged: false,
            },
            &snap,
        );
        assert_eq!(ui.menu.as_ref().map(|m| m.at), Some((grid.x, grid.y)));
    }

    #[test]
    fn a_selection_is_the_rectangle_between_two_corners() {
        let mut grid = GridUi {
            row: 5,
            col: 3,
            anchor: Some(((2, 1), None)),
            ..GridUi::default()
        };
        assert_eq!(grid.selection(None), (2, 1, 5, 3));
        assert_eq!(grid.selected_cells(None), Some((4, 3)));

        // And the other way round: dragging up and left is the same rectangle.
        grid.row = 2;
        grid.col = 1;
        grid.anchor = Some(((5, 3), None));
        assert_eq!(grid.selection(None), (2, 1, 5, 3));
    }

    #[test]
    fn one_cell_is_not_a_range() {
        let grid = GridUi::default();
        assert_eq!(grid.selection(None), (0, 0, 0, 0));
        assert_eq!(
            grid.selected_cells(None),
            None,
            "the status bar would say `1×1 selected` about the cursor"
        );
    }

    #[test]
    fn a_selection_does_not_survive_a_sort() {
        // It is indexes into a result the store replaces: sorting fetches page
        // one again, so rows 2..5 of the old order are different rows now, and
        // a copy taken from the old anchor comes out wrong with nothing said.
        let grid = GridUi {
            row: 5,
            col: 3,
            anchor: Some(((2, 1), None)),
            ..GridUi::default()
        };

        let after = Some(sqlake_core::result::Sort::new(
            0,
            sqlake_core::result::SortDir::Asc,
        ));
        assert_eq!(
            grid.selection(after),
            (5, 3, 5, 3),
            "a selection made under one ordering was kept under another"
        );
        assert_eq!(grid.selected_cells(after), None);
    }

    #[test]
    fn the_detail_document_is_built_once_per_cell() {
        // A snapshot is republished for reasons that have nothing to do with
        // this tab, and rebuilding sanitises the whole value again — the cost
        // `MAX_CELL_CHARS` exists to avoid, one pane over.
        let mut grid = GridUi::default();
        let rows = std::sync::Arc::new(sqlake_app::PagedResult::new(
            &sqlake_core::result::ResultSet::new(
                vec![sqlake_core::result::Column::new("c", "text", false)],
                vec![sqlake_core::result::Row(vec![
                    sqlake_core::value::Value::Text("x".repeat(1000)),
                ])],
                None,
            ),
        ));
        grid.grid(&rows);

        let first = grid.detail().expect("a document");
        let again = grid.detail().expect("a document");
        assert!(
            std::sync::Arc::ptr_eq(&first, &again),
            "the value was laid out again for the same cell"
        );

        grid.col = 0;
        grid.row = 0;
        assert!(std::sync::Arc::ptr_eq(
            &first,
            &grid.detail().expect("a document")
        ));
    }

    #[test]
    fn filtering_keeps_the_same_node_selected_not_the_same_row() {
        // The answer M2 owed to its second open question. Rows are renumbered
        // by everything the filter removes above them, so a selection left at
        // its old number lands on a node the user never pointed at — and then
        // `Enter` opens it.
        let (snap, mut ui) = setup(30, 0, 0);
        let _ = ui.apply(ViewCmd::SelectTreeRow(17), &snap);
        assert_eq!(
            ui.visible_node(&snap, 17).map(|n| n.label.clone()),
            Some("n17".to_owned())
        );

        // `n17` is the only row left, so it is row zero now.
        let _ = ui.apply(ViewCmd::SetFilter(Some(search("n17"))), &snap);
        assert_eq!(ui.tree.selected, Some(0));
        assert_eq!(
            ui.visible_node(&snap, 0).map(|n| n.label.clone()),
            Some("n17".to_owned())
        );

        // And back again: clearing restores the tree, and the selection goes
        // with the node rather than staying at zero.
        let _ = ui.apply(ViewCmd::SetFilter(None), &snap);
        assert_eq!(ui.tree.selected, Some(17));
    }

    #[test]
    fn a_selection_the_filter_removes_falls_to_the_first_row() {
        // There is nothing to follow, and the first row is where a search
        // leaves you anyway.
        let (snap, mut ui) = setup(30, 0, 0);
        let _ = ui.apply(ViewCmd::SelectTreeRow(17), &snap);
        let _ = ui.apply(ViewCmd::SetFilter(Some(search("n2"))), &snap);
        assert_eq!(ui.tree.selected, Some(0));
        assert_eq!(
            ui.visible_node(&snap, 0).map(|n| n.label.clone()),
            Some("n2".to_owned())
        );
    }

    #[test]
    fn a_filter_that_matches_nothing_selects_nothing() {
        let (snap, mut ui) = setup(30, 0, 0);
        let _ = ui.apply(ViewCmd::SelectTreeRow(3), &snap);
        let _ = ui.apply(ViewCmd::SetFilter(Some(search("zzz"))), &snap);
        assert_eq!(ui.tree.selected, None);
    }

    #[test]
    fn moving_the_selection_stays_inside_the_filtered_rows() {
        // The clamp reads the visible count, not the tree's: otherwise `G`
        // runs off the end of a filtered list into rows that are not drawn.
        let (snap, mut ui) = setup(30, 0, 0);
        let _ = ui.apply(ViewCmd::SetFilter(Some(search("n1"))), &snap);
        // n1, n10..n19 — eleven rows.
        let _ = ui.apply(ViewCmd::MoveTreeSelection(1000), &snap);
        assert_eq!(ui.tree.selected, Some(10));
        assert_eq!(
            ui.visible_node(&snap, 10).map(|n| n.label.clone()),
            Some("n19".to_owned())
        );
    }

    #[test]
    fn scrolling_stops_at_the_last_screenful() {
        let (snap, mut ui) = setup(30, 0, 0);
        let _ = ui.apply(
            ViewCmd::ScrollBy {
                pane: PaneId::Explorer,
                delta: 1000,
            },
            &snap,
        );
        // Thirty rows in a ten-row pane: the last useful offset is twenty.
        assert_eq!(ui.tree.offset, 20);
    }

    #[test]
    fn scrolling_the_grid_continues_from_where_it_was() {
        let (snap, mut ui) = setup(0, 50, 2);
        for _ in 0..3 {
            let _ = ui.apply(
                ViewCmd::ScrollBy {
                    pane: PaneId::Grid,
                    delta: 3,
                },
                &snap,
            );
        }
        // Reading the offset from the wrong place makes every notch start over
        // from the top, so three notches land on three rows rather than nine.
        assert_eq!(ui.grid(TabId::new(1)).unwrap().row_offset, 9);
    }

    #[test]
    fn content_shorter_than_the_pane_never_scrolls() {
        let (snap, mut ui) = setup(3, 0, 0);
        let _ = ui.apply(ViewCmd::ScrollToEnd(PaneId::Explorer), &snap);
        assert_eq!(ui.tree.offset, 0, "there is nothing below to reach");
    }

    #[test]
    fn a_track_click_lands_proportionally() {
        let (snap, mut ui) = setup(30, 0, 0);
        let _ = ui.apply(
            ViewCmd::ScrollToRatio {
                pane: PaneId::Explorer,
                permille: 500,
            },
            &snap,
        );
        assert_eq!(ui.tree.offset, 10);
    }

    #[test]
    fn the_selection_pulls_the_viewport_with_it() {
        let (snap, mut ui) = setup(30, 0, 0);
        let _ = ui.apply(ViewCmd::SelectTreeRow(25), &snap);
        assert_eq!(ui.tree.selected, Some(25));
        // Just far enough that row 25 is the last visible row, not a jump that
        // puts it in the middle and loses the reader's place.
        assert_eq!(ui.tree.offset, 16);

        let _ = ui.apply(ViewCmd::SelectTreeRow(2), &snap);
        assert_eq!(ui.tree.offset, 2);
    }

    #[test]
    fn the_first_move_selects_the_first_row() {
        let (snap, mut ui) = setup(30, 0, 0);
        assert_eq!(ui.tree.selected, None);
        let _ = ui.apply(ViewCmd::MoveTreeSelection(1), &snap);
        assert_eq!(ui.tree.selected, Some(0), "not row one");
    }

    #[test]
    fn selection_cannot_leave_the_content() {
        let (snap, mut ui) = setup(3, 0, 0);
        let _ = ui.apply(ViewCmd::MoveTreeSelection(-5), &snap);
        assert_eq!(ui.tree.selected, Some(0));
        let _ = ui.apply(ViewCmd::SelectTreeRow(99), &snap);
        assert_eq!(ui.tree.selected, Some(2));
    }

    #[test]
    fn an_empty_tree_has_nothing_selected() {
        let (snap, mut ui) = setup(0, 0, 0);
        let _ = ui.apply(ViewCmd::SelectTreeRow(0), &snap);
        assert_eq!(ui.tree.selected, None);
    }

    #[test]
    fn the_cell_cursor_stays_inside_the_result() {
        let (snap, mut ui) = setup(0, 50, 4);
        let _ = ui.apply(ViewCmd::SelectCell { row: 99, col: 99 }, &snap);
        let grid = ui.grid(TabId::new(1)).unwrap();
        assert_eq!((grid.row, grid.col), (49, 3));
    }

    #[test]
    fn moving_the_cell_cursor_scrolls_the_grid() {
        let (snap, mut ui) = setup(0, 50, 4);
        let _ = ui.apply(ViewCmd::SelectCell { row: 0, col: 0 }, &snap);
        let _ = ui.apply(ViewCmd::MoveCellSelection { drow: 20, dcol: 0 }, &snap);
        let grid = ui.grid(TabId::new(1)).unwrap();
        assert_eq!(grid.row, 20);
        assert_eq!(grid.row_offset, 11);
    }

    #[test]
    fn the_cell_cursor_pulls_the_grid_sideways() {
        // Sixty columns of at least the minimum width: the cursor cannot reach
        // column fifty without the grid scrolling after it.
        let (snap, mut ui) = setup(0, 10, 60);
        let _ = ui.apply(ViewCmd::SelectCell { row: 0, col: 50 }, &snap);
        let grid = ui.grid(TabId::new(1)).unwrap();
        assert_eq!(grid.col, 50);
        assert!(
            grid.col_offset > 0 && grid.col_offset <= 50,
            "{}",
            grid.col_offset
        );

        let _ = ui.apply(ViewCmd::SelectCell { row: 0, col: 0 }, &snap);
        assert_eq!(ui.grid(TabId::new(1)).unwrap().col_offset, 0, "and back");
    }

    #[test]
    fn a_grid_with_no_rows_ignores_the_cursor() {
        let (snap, mut ui) = setup(0, 0, 0);
        let _ = ui.apply(ViewCmd::SelectCell { row: 3, col: 3 }, &snap);
        assert!(ui.grid(TabId::new(1)).is_none_or(|g| g.row == 0));
    }

    #[test]
    fn a_resized_column_keeps_its_width() {
        let (snap, mut ui) = setup(0, 10, 3);
        let natural = {
            let rows = ui.rows_of(&snap).unwrap();
            ui.grid_mut(TabId::new(1)).grid(&rows).columns()[1].natural_width
        };
        let _ = ui.apply(ViewCmd::ResizeColumn { col: 1, delta: 5 }, &snap);
        let grid = ui.grid(TabId::new(1)).unwrap();
        assert_eq!(grid.width(1, natural), natural + 5);
        assert_eq!(grid.width(0, natural), natural, "only the one column moved");
    }

    #[test]
    fn a_column_cannot_be_dragged_to_nothing() {
        let (snap, mut ui) = setup(0, 10, 3);
        let _ = ui.apply(
            ViewCmd::ResizeColumn {
                col: 0,
                delta: -500,
            },
            &snap,
        );
        assert_eq!(ui.grid(TabId::new(1)).unwrap().width(0, 10), 1);
    }

    #[test]
    fn the_rendered_grid_is_built_once_per_page() {
        let (snap, mut ui) = setup(0, 10, 2);
        let rows = ui.rows_of(&snap).unwrap();
        let first = ui.grid_mut(TabId::new(1)).grid(&rows) as *const RenderedGrid;
        let again = ui.grid_mut(TabId::new(1)).grid(&rows) as *const RenderedGrid;
        assert_eq!(first, again, "an unchanged snapshot must not rebuild it");
    }

    #[test]
    fn focus_cycles_between_the_two_panes() {
        let (snap, mut ui) = setup(0, 0, 0);
        assert_eq!(ui.focus, PaneId::Explorer);
        let _ = ui.apply(ViewCmd::FocusNextPane, &snap);
        assert_eq!(ui.focus, PaneId::Grid);
        let _ = ui.apply(ViewCmd::FocusNextPane, &snap);
        assert_eq!(ui.focus, PaneId::Explorer);
        let _ = ui.apply(ViewCmd::FocusPrevPane, &snap);
        assert_eq!(ui.focus, PaneId::Grid);
    }

    #[test]
    fn focus_from_outside_the_cycle_enters_it() {
        let (snap, mut ui) = setup(0, 0, 0);
        let _ = ui.apply(ViewCmd::FocusPane(PaneId::StatusBar), &snap);
        let _ = ui.apply(ViewCmd::FocusNextPane, &snap);
        assert_eq!(ui.focus, PaneId::Explorer);
    }

    #[test]
    fn the_splitter_leaves_both_panes_usable() {
        let (snap, mut ui) = setup(0, 0, 0);
        let _ = ui.apply(
            ViewCmd::MoveSplit {
                split: SplitId::Explorer,
                delta: -500,
            },
            &snap,
        );
        assert_eq!(ui.explorer_width(80), MIN_PANE_WIDTH);

        let _ = ui.apply(
            ViewCmd::MoveSplit {
                split: SplitId::Explorer,
                delta: 500,
            },
            &snap,
        );
        // The splitter's own column comes out of the explorer's side, so the
        // grid still gets `MIN_PANE_WIDTH`.
        assert_eq!(ui.explorer_width(80), 80 - MIN_PANE_WIDTH - 1);
    }

    #[test]
    fn the_splitter_ends_up_where_it_was_dragged() {
        // Deriving the screen width from the pane viewports loses a column per
        // border, and a drag one cell to the right then moves the splitter one
        // cell to the left.
        let snap = snapshot(ConnId::new(), 0, 0, 0);
        let mut ui = UiState::new();
        ui.set_screen(Rect::new(0, 0, 100, 30));
        ui.set_viewport(PaneId::Explorer, Rect::new(1, 2, 26, 26));
        ui.set_viewport(PaneId::Grid, Rect::new(30, 2, 68, 26));

        let before = ui.explorer_width(100);
        let _ = ui.apply(
            ViewCmd::MoveSplit {
                split: SplitId::Explorer,
                delta: 1,
            },
            &snap,
        );
        assert_eq!(ui.explorer_width(100), before + 1);
    }

    #[test]
    fn evening_the_split_returns_to_a_fraction_of_the_screen() {
        let (snap, mut ui) = setup(0, 0, 0);
        let default = ui.explorer_width(100);
        let _ = ui.apply(
            ViewCmd::MoveSplit {
                split: SplitId::Explorer,
                delta: 10,
            },
            &snap,
        );
        assert_ne!(ui.explorer_width(100), default);
        let _ = ui.apply(ViewCmd::EvenSplit(SplitId::Explorer), &snap);
        assert_eq!(ui.explorer_width(100), default);
        // And it follows the terminal rather than being frozen.
        assert!(ui.explorer_width(200) > default);
    }

    #[test]
    fn a_screen_too_narrow_for_both_still_yields_a_layout() {
        let (_snap, ui) = setup(0, 0, 0);
        let width = ui.explorer_width(10);
        assert!(width <= 10, "{width}");
    }

    #[test]
    fn closing_a_tab_releases_its_view_state() {
        let (snap, mut ui) = setup(0, 10, 2);
        let _ = ui.apply(ViewCmd::SelectCell { row: 1, col: 1 }, &snap);
        let tab = ui.active_tab.unwrap();
        assert!(ui.grid(tab).is_some());

        let _ = ui.apply(ViewCmd::CloseTab(tab), &snap);
        assert!(ui.grid(tab).is_none(), "the cached grid goes with it");
    }

    #[test]
    fn opening_a_relation_already_open_selects_it_instead_of_duplicating() {
        let (snap, mut ui) = setup(0, 0, 0);
        let first = ui.active_tab.unwrap();
        let conn = ui.tabs[0].conn;

        // A second tab, so the first is no longer active — reopening it must
        // find it rather than assume it is still in front.
        let _ = ui.apply(
            ViewCmd::OpenTab {
                conn: ConnId::new(),
                table: TableRef::new(["public", "orders"]),
            },
            &snap,
        );
        assert_ne!(ui.active_tab, Some(first));

        let _ = ui.apply(
            ViewCmd::OpenTab {
                conn,
                table: table(),
            },
            &snap,
        );
        assert_eq!(ui.tabs.len(), 2, "the first relation must not open twice");
        assert_eq!(ui.active_tab, Some(first));
    }

    #[test]
    fn closing_a_tab_selects_its_neighbour() {
        let (snap, mut ui) = setup(0, 0, 0);
        let first = ui.active_tab.unwrap();
        let _ = ui.apply(
            ViewCmd::OpenTab {
                conn: ConnId::new(),
                table: TableRef::new(["public", "orders"]),
            },
            &snap,
        );

        let _ = ui.apply(ViewCmd::CloseTab(first), &snap);
        assert_eq!(ui.tabs.len(), 1);
        assert_eq!(ui.active_tab, Some(ui.tabs[0].id));
    }

    #[test]
    fn closing_the_last_tab_leaves_nothing_selected() {
        let (snap, mut ui) = setup(0, 0, 0);
        let tab = ui.active_tab.unwrap();
        let _ = ui.apply(ViewCmd::CloseTab(tab), &snap);
        assert!(ui.tabs.is_empty());
        assert_eq!(ui.active_tab, None);
    }

    #[test]
    fn a_new_preview_error_becomes_a_toast_once() {
        let (mut snap, mut ui) = setup(0, 0, 0);

        snap.previews[0].last_error = Some("timed out".to_owned());
        ui.raise_preview_errors(&snap);
        assert_eq!(ui.toasts.len(), 1);
        assert_eq!(ui.toasts[0].text, "timed out");

        // The same snapshot again — a redraw with nothing new — must not
        // raise a second toast for a message already shown.
        ui.raise_preview_errors(&snap);
        assert_eq!(ui.toasts.len(), 1);

        // Success clears the record, so the *same* text failing again later
        // is treated as new rather than silently swallowed.
        snap.previews[0].last_error = None;
        ui.raise_preview_errors(&snap);
        snap.previews[0].last_error = Some("timed out".to_owned());
        ui.raise_preview_errors(&snap);
        assert_eq!(ui.toasts.len(), 2);
    }

    #[test]
    fn sql_tabs_and_a_preview_tab_are_open_at_once() {
        let conn = ConnId::new();
        let snap = snapshot(conn, 3, 10, 3);
        let mut ui = UiState::new();

        let _ = ui.apply(
            ViewCmd::OpenTab {
                conn,
                table: table(),
            },
            &snap,
        );
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);

        assert_eq!(ui.tabs.len(), 3);
        // Two SQL tabs, not one raised twice: they are two questions, and the
        // rule that raises an already-open relation has nothing to match on.
        let titles: Vec<String> = ui.tabs.iter().map(|t| t.title().into_owned()).collect();
        assert_eq!(titles, ["users", "SQL#1", "SQL#2"]);
    }

    #[test]
    fn the_sql_numbering_does_not_count_previews() {
        // The id is bookkeeping and the number is read off the screen: a
        // session that opened a relation first must not name its first query
        // tab `SQL#2`.
        let conn = ConnId::new();
        let snap = snapshot(conn, 3, 10, 3);
        let mut ui = UiState::new();
        let _ = ui.apply(
            ViewCmd::OpenTab {
                conn,
                table: table(),
            },
            &snap,
        );
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);
        assert_eq!(ui.tabs[1].title(), "SQL#1");
    }

    #[test]
    fn closing_one_tab_leaves_the_others_where_they_were() {
        let conn = ConnId::new();
        let snap = snapshot(conn, 3, 10, 3);
        let mut ui = UiState::new();
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);
        let first = ui.active_tab.expect("a tab");
        let _ = ui.apply(
            ViewCmd::OpenTab {
                conn,
                table: table(),
            },
            &snap,
        );
        let preview = ui.active_tab.expect("a tab");
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);

        // Something view-local on the preview, to see whether closing a
        // neighbour disturbs it.
        let _ = ui.apply(ViewCmd::SelectTab(preview), &snap);
        let _ = ui.apply(ViewCmd::SelectCell { row: 4, col: 2 }, &snap);

        let _ = ui.apply(ViewCmd::CloseTab(first), &snap);

        assert_eq!(ui.tabs.len(), 2);
        assert_eq!(
            ui.grid(preview).map(|g| (g.row, g.col)),
            Some((4, 2)),
            "closing a SQL tab moved the preview's cursor"
        );
    }

    #[test]
    fn a_sql_tab_scrolls_by_its_own_lines() {
        // The grid pane's offset is reused, so `j` and `PageDown` need nothing
        // added — but the clamp asks how much content there is, and for a SQL
        // tab that is lines rather than rows.
        let conn = ConnId::new();
        let snap = snapshot(conn, 3, 10, 3);
        let mut ui = UiState::new();
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);
        ui.set_viewport(PaneId::Grid, Rect::new(0, 0, 40, 4));
        ui.set_sql_lines(20);

        let _ = ui.apply(
            ViewCmd::ScrollBy {
                pane: PaneId::Grid,
                delta: 100,
            },
            &snap,
        );
        // Twenty lines in a pane four tall: sixteen is the last screenful.
        assert_eq!(ui.sql_offset(), 16);
    }

    fn with_templates(mut snap: Snapshot, bodies: &[(&str, &str)]) -> Snapshot {
        snap.templates = sqlake_app::snapshot::TemplatesView {
            data: LoadState::Ready(Arc::new(
                bodies
                    .iter()
                    .enumerate()
                    .map(|(at, (name, body))| sqlake_core::library::Template {
                        id: sqlake_core::library::TemplateId::new(at as i64 + 1),
                        name: (*name).to_owned(),
                        body: (*body).to_owned(),
                        driver: None,
                        tags: Vec::new(),
                        created_at: time::OffsetDateTime::UNIX_EPOCH,
                        updated_at: time::OffsetDateTime::UNIX_EPOCH,
                    })
                    .collect(),
            )),
            failed: None,
        };
        snap
    }

    fn template_id(snap: &Snapshot, name: &str) -> sqlake_core::library::TemplateId {
        snap.templates
            .data
            .ready()
            .expect("a list")
            .iter()
            .find(|t| t.name == name)
            .expect("that template")
            .id
    }

    #[test]
    fn opening_the_palette_is_what_reads_the_templates() {
        // Every way of opening it, so that neither has to remember to.
        let conn = ConnId::new();
        let snap = with_templates(snapshot(conn, 3, 10, 3), &[]);
        let mut ui = UiState::new();
        assert!(matches!(
            ui.apply(
                ViewCmd::Palette(Some(crate::palette::Palette::opening())),
                &snap
            ),
            Some(Action::LoadTemplates)
        ));
        assert!(
            ui.apply(ViewCmd::Palette(None), &snap).is_none(),
            "closing it asks for nothing"
        );
        assert!(matches!(
            ui.apply(
                ViewCmd::Palette(Some(crate::palette::Palette::saving("select 1".to_owned()))),
                &snap
            ),
            Some(Action::LoadTemplates)
        ));
    }

    #[test]
    fn saving_names_what_is_in_the_buffer() {
        let conn = ConnId::new();
        let snap = with_templates(snapshot(conn, 3, 10, 3), &[]);
        let mut ui = UiState::new();
        let _ = ui.apply(
            ViewCmd::Palette(Some(crate::palette::Palette::saving(
                "select * from users".to_owned(),
            ))),
            &snap,
        );
        if let Some(palette) = ui.palette.as_mut() {
            palette.filter = "  daily  ".to_owned();
        }

        let asked = ui.apply(ViewCmd::CommitTemplate, &snap);
        let Some(Action::SaveTemplate(template)) = asked else {
            panic!("{asked:?}");
        };
        // Trimmed: a name with a space on the end is one nothing will match
        // when it is typed again.
        assert_eq!(template.name, "daily");
        assert_eq!(template.body, "select * from users");
        assert!(ui.palette.is_none(), "the answer is read where it was");
    }

    #[test]
    fn saving_under_a_name_already_saved_asks_before_replacing_it() {
        // The store refuses a duplicate name, so without this the only way to
        // change a saved statement would be to delete it and write it again —
        // and the refusal would arrive as a failure for something somebody
        // meant to do.
        let conn = ConnId::new();
        let snap = with_templates(snapshot(conn, 3, 10, 3), &[("daily", "select 1")]);
        let mut ui = UiState::new();
        let _ = ui.apply(
            ViewCmd::Palette(Some(crate::palette::Palette::saving("select 2".to_owned()))),
            &snap,
        );
        if let Some(palette) = ui.palette.as_mut() {
            palette.filter = "daily".to_owned();
        }

        assert!(
            ui.apply(ViewCmd::CommitTemplate, &snap).is_none(),
            "nothing is saved until the question is answered"
        );
        let modal = ui.modal.as_ref().expect("a dialog");
        assert!(modal.title.contains("daily"), "{modal:?}");
        let Some(crate::intent::Intent::App(Action::ReplaceTemplate { id, with })) =
            modal.choices.first().map(|c| c.intent.clone())
        else {
            panic!("{modal:?}");
        };
        assert_eq!(id, template_id(&snap, "daily"));
        assert_eq!(with.body, "select 2");
    }

    #[test]
    fn a_save_with_no_name_is_not_a_save() {
        let conn = ConnId::new();
        let snap = with_templates(snapshot(conn, 3, 10, 3), &[]);
        let mut ui = UiState::new();
        let _ = ui.apply(
            ViewCmd::Palette(Some(crate::palette::Palette::saving("select 1".to_owned()))),
            &snap,
        );
        assert!(ui.apply(ViewCmd::CommitTemplate, &snap).is_none());
    }

    #[test]
    fn deleting_asks_first_and_the_answer_is_the_deletion() {
        // A saved statement is somebody's own writing and there is no undo, so
        // the one thing this must not be is a key that quietly loses it.
        let conn = ConnId::new();
        let snap = with_templates(snapshot(conn, 3, 10, 3), &[("daily", "select 1")]);
        let mut ui = UiState::new();
        let _ = ui.apply(
            ViewCmd::Palette(Some(crate::palette::Palette::opening())),
            &snap,
        );
        let id = template_id(&snap, "daily");
        let _ = ui.apply(
            ViewCmd::ConfirmDeleteTemplate {
                id,
                name: "daily".to_owned(),
            },
            &snap,
        );

        assert!(ui.palette.is_none(), "the question is drawn over nothing");
        let modal = ui.modal.as_ref().expect("a dialog");
        assert!(modal.title.contains("daily"), "{modal:?}");
        assert_eq!(
            modal.choices.first().map(|c| c.intent.clone()),
            Some(Action::DeleteTemplate(id).into())
        );
    }

    #[test]
    fn a_template_with_nothing_to_fill_in_goes_straight_to_the_buffer() {
        let conn = ConnId::new();
        let snap = with_templates(snapshot(conn, 3, 10, 3), &[("plain", "select 1")]);
        let mut ui = UiState::new();
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);
        let _ = ui.apply(
            ViewCmd::Palette(Some(crate::palette::Palette::opening())),
            &snap,
        );

        let _ = ui.apply(ViewCmd::UseTemplate(template_id(&snap, "plain")), &snap);
        assert_eq!(ui.active_sql(), Some("select 1"));
        assert!(ui.palette.is_none(), "it is done, so it closes");
    }

    #[test]
    fn a_template_with_placeholders_asks_before_the_buffer_sees_it() {
        // The whole reason `BoundTemplate` exists: an unanswered `{{table}}`
        // reaching the buffer is a syntax error against text nobody wrote.
        let conn = ConnId::new();
        let snap = with_templates(
            snapshot(conn, 3, 10, 3),
            &[("by table", "select * from {{ident:table}} where x = {{x}}")],
        );
        let mut ui = UiState::new();
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);
        let _ = ui.apply(
            ViewCmd::Palette(Some(crate::palette::Palette::opening())),
            &snap,
        );
        let _ = ui.apply(ViewCmd::UseTemplate(template_id(&snap, "by table")), &snap);

        let form = ui
            .palette
            .as_ref()
            .and_then(|p| p.asking.as_ref())
            .expect("a form");
        assert_eq!(
            form.fields
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            ["table", "x"]
        );
        assert_eq!(ui.active_sql(), Some(""), "nothing reaches the buffer yet");
    }

    #[test]
    fn answering_the_form_quotes_by_what_each_answer_is() {
        let conn = ConnId::new();
        let snap = with_templates(
            snapshot(conn, 3, 10, 3),
            &[(
                "by table",
                "select * from {{ident:table}} where name = {{name}}",
            )],
        );
        let mut ui = UiState::new();
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);
        let _ = ui.apply(
            ViewCmd::Palette(Some(crate::palette::Palette::opening())),
            &snap,
        );
        let _ = ui.apply(ViewCmd::UseTemplate(template_id(&snap, "by table")), &snap);

        if let Some(form) = ui.palette.as_mut().and_then(|p| p.asking.as_mut()) {
            form.fields[0].value = "users".to_owned();
            form.fields[1].value = "o'brien".to_owned();
        }
        let _ = ui.apply(ViewCmd::SubmitTemplate, &snap);

        assert_eq!(
            ui.active_sql(),
            Some(r#"select * from "users" where name = 'o''brien'"#)
        );
        assert!(ui.palette.is_none());
    }

    #[test]
    fn a_template_never_writes_over_what_is_already_in_a_buffer() {
        // There is no editor here and so no undo: a keystroke that replaced a
        // half-written query would destroy work with nothing to get it back.
        let conn = ConnId::new();
        let snap = with_templates(snapshot(conn, 3, 10, 3), &[("plain", "select 1")]);
        let mut ui = UiState::new();
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);
        let started = ui.active_tab.expect("a tab");
        ui.set_buffer(started, "select mine".to_owned());

        let _ = ui.apply(
            ViewCmd::Palette(Some(crate::palette::Palette::opening())),
            &snap,
        );
        let _ = ui.apply(ViewCmd::UseTemplate(template_id(&snap, "plain")), &snap);

        assert_eq!(ui.buffer_of(started), Some("select mine"));
        assert_eq!(ui.active_sql(), Some("select 1"));
        assert_ne!(ui.active_tab, Some(started), "it opened its own tab");
    }

    #[test]
    fn a_body_that_cannot_be_read_says_so_in_the_palette() {
        // Not a toast: it is about the row under the cursor, and the palette
        // is where that row is.
        let conn = ConnId::new();
        let snap = with_templates(snapshot(conn, 3, 10, 3), &[("broken", "select '{{x}}'")]);
        let mut ui = UiState::new();
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);
        let _ = ui.apply(
            ViewCmd::Palette(Some(crate::palette::Palette::opening())),
            &snap,
        );
        let _ = ui.apply(ViewCmd::UseTemplate(template_id(&snap, "broken")), &snap);

        let form = ui
            .palette
            .as_ref()
            .and_then(|p| p.asking.as_ref())
            .expect("a form");
        assert!(form.failed.is_some(), "{form:?}");
        assert_eq!(ui.active_sql(), Some(""));
    }

    #[test]
    fn the_relation_the_explorer_is_on_answers_the_table_placeholder() {
        let conn = ConnId::new();
        let mut snap = with_templates(
            snapshot(conn, 1, 10, 3),
            &[("by table", "select * from {{ident:table}}")],
        );
        let rows = Arc::get_mut(&mut snap.explorer).expect("sole owner");
        rows.nodes.push(VisibleNode {
            conn,
            depth: 1,
            label: "users".into(),
            node_ref: NodeRef::new(NodeKind::Relation, ["public", "users"]),
            relation_kind: Some(sqlake_core::node::RelationKind::Table),
            state: NodeState::Leaf,
        });

        let mut ui = UiState::new();
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);
        let _ = ui.apply(ViewCmd::SelectTreeRow(1), &snap);
        let _ = ui.apply(
            ViewCmd::Palette(Some(crate::palette::Palette::opening())),
            &snap,
        );
        let _ = ui.apply(ViewCmd::UseTemplate(template_id(&snap, "by table")), &snap);

        let form = ui
            .palette
            .as_ref()
            .and_then(|p| p.asking.as_ref())
            .expect("a form");
        assert_eq!(form.fields[0].value, "public.users");
    }

    #[test]
    fn a_definition_answers_for_a_scroll_no_draw_has_seen_yet() {
        // The layout is built during the draw, and events queued behind a
        // `--refresh` are handled before the next one. Reading only what the
        // last draw built would answer "no rows" here, which clamps the grid
        // to the top and makes a copy do nothing at all — silently, since
        // neither says why it found nothing.
        let conn = ConnId::new();
        let mut snap = snapshot(conn, 3, 10, 3);
        let detail = |name: &str| {
            Arc::new(sqlake_core::detail::TableDetail::new(
                table(),
                sqlake_core::node::RelationKind::Table,
                vec![sqlake_core::detail::ColumnDef {
                    name: name.to_owned(),
                    type_name: "integer".to_owned(),
                    nullable: false,
                    default: None,
                    comment: None,
                }],
            ))
        };
        snap.definitions.push(sqlake_app::snapshot::DefinitionView {
            conn,
            table: table(),
            data: LoadState::Ready(detail("id")),
        });

        let mut ui = UiState::new();
        let _ = ui.apply(
            ViewCmd::OpenDefinition {
                conn,
                table: table(),
            },
            &snap,
        );
        assert_eq!(ui.row_count(&snap), 1, "before the first draw");

        // What a refresh does: the same table, a different `Arc`.
        snap.definitions[0].data = LoadState::Ready(detail("renamed"));
        assert_eq!(ui.row_count(&snap), 1, "after the detail was replaced");
    }

    #[test]
    fn a_definition_scrolls_its_statement_by_line() {
        // The DDL section has no rows at all, so a clamp that counted them
        // would leave a long `CREATE TABLE` stuck on its first screenful.
        let conn = ConnId::new();
        let mut snap = snapshot(conn, 3, 10, 3);
        let mut detail = sqlake_core::detail::TableDetail::new(
            table(),
            sqlake_core::node::RelationKind::Table,
            Vec::new(),
        );
        detail.ddl = Some(sqlake_core::detail::Ddl::generated("CREATE TABLE t ()"));
        snap.definitions.push(sqlake_app::snapshot::DefinitionView {
            conn,
            table: table(),
            data: LoadState::Ready(Arc::new(detail)),
        });

        let mut ui = UiState::new();
        let _ = ui.apply(
            ViewCmd::OpenDefinition {
                conn,
                table: table(),
            },
            &snap,
        );
        // Columns, then the DDL: this definition has no driver sections.
        let _ = ui.apply(
            ViewCmd::SelectSection(crate::intent::SectionPick::At(1)),
            &snap,
        );
        ui.set_viewport(PaneId::Grid, Rect::new(0, 0, 40, 4));
        ui.set_sql_lines(20);

        let _ = ui.apply(
            ViewCmd::ScrollBy {
                pane: PaneId::Grid,
                delta: 100,
            },
            &snap,
        );
        assert_eq!(ui.sql_offset(), 16);
    }

    #[test]
    fn a_shorter_buffer_does_not_leave_the_pane_scrolled_off_it() {
        let conn = ConnId::new();
        let snap = snapshot(conn, 3, 10, 3);
        let mut ui = UiState::new();
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);
        ui.set_viewport(PaneId::Grid, Rect::new(0, 0, 40, 4));
        ui.set_sql_lines(20);
        let _ = ui.apply(
            ViewCmd::ScrollBy {
                pane: PaneId::Grid,
                delta: 100,
            },
            &snap,
        );
        ui.set_sql_lines(2);
        assert!(ui.sql_offset() <= 1, "{}", ui.sql_offset());
    }

    #[test]
    fn a_sql_tab_asks_for_no_pages() {
        // `wants_a_page` reaches for the tab's relation, and a SQL tab has
        // none. Without the `?` it would page whatever relation happened to
        // be first in the snapshot.
        let conn = ConnId::new();
        let snap = snapshot(conn, 3, 10, 3);
        let mut ui = UiState::new();
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);
        ui.set_viewport(PaneId::Grid, Rect::new(0, 0, 40, 10));
        let fetch = ui.apply(
            ViewCmd::ScrollBy {
                pane: PaneId::Grid,
                delta: 1000,
            },
            &snap,
        );
        assert!(fetch.is_none(), "{fetch:?}");
    }

    #[test]
    fn a_question_about_cost_is_asked_once_and_stays_dismissed() {
        let conn = ConnId::new();
        let mut snap = snapshot(conn, 3, 10, 3);
        let mut ui = UiState::new();
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);
        let tab = ui.active_tab.expect("a tab");
        let id = QueryId::new();
        ui.set_query(tab, id);
        snap.queries.push(over_budget(conn, id));

        ui.raise_approvals(&snap);
        let modal = ui.modal.clone().expect("a dialog");
        assert_eq!(modal.choices.len(), 1);
        assert!(!modal.grave, "a question about money is not a failure");

        // A snapshot is republished for reasons that have nothing to do with
        // this — a spinner tick will do it — and a dialog re-raised on each
        // one could never be dismissed.
        ui.modal = None;
        ui.raise_approvals(&snap);
        assert!(
            ui.modal.is_none(),
            "the question came back after being dismissed"
        );
    }

    #[test]
    fn the_dialog_goes_when_the_store_stops_asking() {
        // It follows the snapshot rather than closing itself, which is what
        // keeps an answer from being given twice.
        let conn = ConnId::new();
        let mut snap = snapshot(conn, 3, 10, 3);
        let mut ui = UiState::new();
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);
        let tab = ui.active_tab.expect("a tab");
        let id = QueryId::new();
        ui.set_query(tab, id);
        snap.queries.push(over_budget(conn, id));
        ui.raise_approvals(&snap);
        assert!(ui.modal.is_some());

        snap.queries[0].needs_approval = None;
        ui.raise_approvals(&snap);
        assert!(ui.modal.is_none());
    }

    #[test]
    fn a_question_about_somebody_elses_query_is_not_raised_here() {
        // An agent running something over the socket is not this person's
        // question to answer, and a dialog about it would appear out of
        // nowhere.
        let conn = ConnId::new();
        let mut snap = snapshot(conn, 3, 10, 3);
        let mut ui = UiState::new();
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);
        snap.queries.push(over_budget(conn, QueryId::new()));
        ui.raise_approvals(&snap);
        assert!(ui.modal.is_none());
    }

    #[test]
    fn a_sql_tab_shows_the_rows_of_the_run_it_started() {
        let conn = ConnId::new();
        let mut snap = snapshot(conn, 3, 10, 3);
        let mut ui = UiState::new();
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);
        let tab = ui.active_tab.expect("a tab");
        assert!(ui.rows_of(&snap).is_none(), "before there is a run");

        let id = QueryId::new();
        ui.set_query(tab, id);
        snap.queries.push(QueryView {
            id,
            conn,
            sql: "select 1".to_owned(),
            estimate: None,
            needs_approval: None,
            data: LoadState::Ready(rows(4, 2)),
            failed_at: None,
            started_at: std::time::Instant::now(),
            took: None,
        });
        assert_eq!(ui.rows_of(&snap).map(|r| r.row_count()), Some(4));
    }

    #[test]
    fn a_run_that_failed_says_so_once() {
        // A failure leaves no rows, so the pane falls back to the buffer and
        // the spinner is already gone: without a toast, pressing Run on a
        // statement the server refused reads as having done nothing at all.
        let conn = ConnId::new();
        let mut snap = snapshot(conn, 3, 10, 3);
        let mut ui = UiState::new();
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);
        let tab = ui.active_tab.expect("a tab");
        let id = QueryId::new();
        ui.set_query(tab, id);
        snap.queries.push(QueryView {
            id,
            conn,
            sql: "select nope".to_owned(),
            estimate: None,
            needs_approval: None,
            data: LoadState::Failed("no such column: nope".to_owned()),
            failed_at: None,
            started_at: std::time::Instant::now(),
            took: None,
        });

        ui.raise_query_errors(&snap);
        assert_eq!(ui.toasts.len(), 1);
        assert_eq!(ui.toasts[0].text, "no such column: nope");
        // The same snapshot again — a redraw with nothing new — must not raise
        // a second toast for a run that has already failed once.
        ui.raise_query_errors(&snap);
        assert_eq!(ui.toasts.len(), 1);
    }

    #[test]
    fn the_run_button_shows_exactly_when_pressing_it_would_do_something() {
        let conn = ConnId::new();
        let snap = snapshot(conn, 3, 10, 3);
        let mut ui = UiState::new();
        assert!(!ui.can_run(&snap), "with no tab at all");

        let _ = ui.apply(
            ViewCmd::OpenTab {
                conn,
                table: table(),
            },
            &snap,
        );
        assert!(!ui.can_run(&snap), "on a preview");

        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);
        assert!(!ui.can_run(&snap), "with an empty buffer");

        let tab = ui.active_tab.expect("a tab");
        ui.set_buffer(tab, "select 1".to_owned());
        assert!(ui.can_run(&snap));
    }

    #[test]
    fn a_byte_count_reads_in_the_units_somebody_prices_in() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1_000_000), "1.0 MB");
        // The units the budget is written in: `max_bytes_billed = "20GB"` is
        // this number, and the dialog has to say 20 back.
        assert_eq!(bytes(20_000_000_000), "20.0 GB");
        // Truncating rather than rounding: shown next to a limit, a number
        // that rounded up to the limit would read as being at it.
        assert_eq!(bytes(1_999_999_999), "1.9 GB");
        assert_eq!(bytes(999_999_999), "999.9 MB");
    }

    #[test]
    fn a_closed_connection_keeps_a_sql_tab_and_its_buffer() {
        // The buffer is the user's own writing rather than a copy of
        // something the database still has. Closing the connection is not a
        // reason to throw away what somebody typed.
        let conn = ConnId::new();
        let mut snap = snapshot(conn, 3, 10, 3);
        let mut ui = UiState::new();
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);
        let sql = ui.active_tab.expect("a tab");
        ui.set_buffer(sql, "select 1".to_owned());
        let _ = ui.apply(
            ViewCmd::OpenTab {
                conn,
                table: table(),
            },
            &snap,
        );

        snap.connections[0].status = ConnStatus::Closed;
        ui.close_disconnected_tabs(&snap);

        assert_eq!(ui.tabs.len(), 1, "the preview should have gone");
        assert_eq!(ui.buffer_of(sql), Some("select 1"));
    }

    #[test]
    fn a_buffer_is_set_by_id_rather_than_by_which_tab_is_active() {
        // The editor had the terminal, and a snapshot arriving while it ran
        // can have moved the focus — or closed the tab underneath it.
        let conn = ConnId::new();
        let snap = snapshot(conn, 3, 10, 3);
        let mut ui = UiState::new();
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);
        let first = ui.active_tab.expect("a tab");
        let _ = ui.apply(ViewCmd::OpenSqlTab { conn }, &snap);
        let second = ui.active_tab.expect("a tab");

        ui.set_buffer(first, "select 1".to_owned());
        assert_eq!(ui.buffer_of(first), Some("select 1"));
        assert_eq!(ui.buffer_of(second), Some(""));

        // A tab that is gone swallows the text rather than putting it
        // somewhere else.
        let _ = ui.apply(ViewCmd::CloseTab(first), &snap);
        ui.set_buffer(first, "select 2".to_owned());
        assert_eq!(ui.buffer_of(second), Some(""));
    }

    #[test]
    fn a_preview_tab_has_no_buffer_to_write_to() {
        let conn = ConnId::new();
        let snap = snapshot(conn, 3, 10, 3);
        let mut ui = UiState::new();
        let _ = ui.apply(
            ViewCmd::OpenTab {
                conn,
                table: table(),
            },
            &snap,
        );
        let tab = ui.active_tab.expect("a tab");
        assert_eq!(ui.buffer_of(tab), None);
        // Silently, because the input layer already refuses to produce the
        // handover for a preview — this is the second half of that, so a
        // caller that got it wrong cannot turn rows into text.
        ui.set_buffer(tab, "select 1".to_owned());
        assert_eq!(ui.buffer_of(tab), None);
    }

    #[test]
    fn a_closed_connection_takes_its_tabs_with_it() {
        // Otherwise the tab outlives the connection it points at: its
        // preview is gone from the store along with the session, so
        // switching to it would show a blank pane for ever.
        let (mut snap, mut ui) = setup(0, 0, 0);
        assert_eq!(ui.tabs.len(), 1);

        snap.connections[0].status = ConnStatus::Closed;
        ui.close_disconnected_tabs(&snap);
        assert!(ui.tabs.is_empty(), "the tab outlived its own connection");
        assert_eq!(ui.active_tab, None);
    }

    #[test]
    fn a_tab_is_untouched_while_its_connection_is_still_open() {
        let (snap, mut ui) = setup(0, 0, 0);
        let before = ui.tabs.len();
        ui.close_disconnected_tabs(&snap);
        assert_eq!(before, ui.tabs.len(), "a live connection's tab was closed");
    }
}

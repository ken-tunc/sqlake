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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use ratatui::layout::Rect;
use sqlake_app::PagedResult;
use sqlake_app::snapshot::{ConnStatus, Snapshot};
#[cfg(test)]
use sqlake_app::tree::TreeView;
use sqlake_app::tree::VisibleNode;
use sqlake_core::id::{ConnId, TabId};
use sqlake_core::node::TableRef;

use crate::grid::RenderedGrid;
use crate::hit::{PaneId, SplitId, Target, ToastId};
use crate::intent::ViewCmd;

/// A tab this screen has open, pointing at one connection's relation.
///
/// At most one per `(conn, table)`: opening a relation already open selects
/// that tab rather than minting a second. `close_tab` in `input` depends on
/// this to know when the store's copy can go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTab {
    pub id: TabId,
    pub conn: ConnId,
    pub table: TableRef,
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

/// Neither pane is useful below this, so the splitter stops here rather than
/// letting one side be dragged out of existence.
pub const MIN_PANE_WIDTH: u16 = 12;

/// Where the splitter sits before anyone moves it, as a fraction of the screen.
const DEFAULT_EXPLORER_PERMILLE: u32 = 280;

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
    /// Horizontal position in whole columns. Per column rather than per cell:
    /// the wheel and the scrollbar both move in column steps, and a half-drawn
    /// leading column is worse than a hard edge.
    pub col_offset: usize,
    pub row: usize,
    pub col: usize,
    widths: HashMap<usize, u16>,
    grid: Option<RenderedGrid>,
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
    /// What the explorer's filter box holds, or `None` when it is closed.
    ///
    /// Screen state, not application state: it decides which rows *this*
    /// screen draws, the way scrolling does. An agent reading the same
    /// snapshot through `sqlake-api` wants the tree, not one person's search.
    pub filter: Option<String>,
    /// Every tab this screen has open, in the order they appear in the tab
    /// bar. At most one per `(conn, table)`.
    pub tabs: Vec<OpenTab>,
    pub active_tab: Option<TabId>,
    next_tab: u32,
    pub toasts: Vec<Toast>,
    next_toast: u64,
    /// The last error already raised for each preview, so that a redraw does
    /// not raise it again — only a *new* message does.
    reported_preview_errors: HashMap<(ConnId, TableRef), String>,
    grids: HashMap<TabId, GridUi>,
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
            .map(|t| (t.conn, t.table.clone()))
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

    /// A closed connection takes its tabs with it.
    ///
    /// `Disconnect` drops the connection's previews, and a tab left pointing
    /// at one shows a blank pane for ever: nothing to fetch, and no session
    /// left to fetch it with.
    pub fn close_disconnected_tabs(&mut self, snapshot: &Snapshot) {
        let closed: Vec<TabId> = self
            .tabs
            .iter()
            .filter(|t| {
                snapshot
                    .connection(t.conn)
                    .is_none_or(|c| c.status == ConnStatus::Closed)
            })
            .map(|t| t.id)
            .collect();
        for id in closed {
            self.apply(ViewCmd::CloseTab(id), snapshot);
        }
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
    pub fn apply(&mut self, cmd: ViewCmd, snapshot: &Snapshot) {
        match cmd {
            ViewCmd::FocusPane(pane) => self.focus = pane,
            ViewCmd::FocusNextPane => self.cycle_focus(1),
            ViewCmd::FocusPrevPane => self.cycle_focus(-1),

            ViewCmd::ScrollBy { pane, delta } => {
                let offset = self.offset(pane);
                self.set_offset(pane, step(offset, delta), snapshot);
            }
            ViewCmd::ScrollToRatio { pane, permille } => {
                let span = self.scrollable(pane, snapshot);
                let target = span * usize::from(permille.min(1000)) / 1000;
                self.set_offset(pane, target, snapshot);
            }
            ViewCmd::ScrollToStart(pane) => self.set_offset(pane, 0, snapshot),
            ViewCmd::ScrollToEnd(pane) => self.set_offset(pane, usize::MAX, snapshot),
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

            ViewCmd::SelectCell { row, col } => self.select_cell(row, col, snapshot),
            ViewCmd::MoveCellSelection { drow, dcol } => {
                let (row, col) = self.active_grid().map_or((0, 0), |g| (g.row, g.col));
                self.select_cell(step(row, drow), step(col, dcol), snapshot);
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

            ViewCmd::DismissModal => self.modal = None,

            // A relation already open is raised, not duplicated — the same
            // rule the store used to apply when tabs lived there.
            ViewCmd::OpenTab { conn, table } => {
                if let Some(existing) = self
                    .tabs
                    .iter()
                    .find(|t| t.conn == conn && t.table == table)
                {
                    self.active_tab = Some(existing.id);
                } else {
                    self.next_tab += 1;
                    let id = TabId::new(self.next_tab);
                    self.tabs.push(OpenTab { id, conn, table });
                    self.active_tab = Some(id);
                }
            }
            ViewCmd::SelectTab(id) => {
                if self.tabs.iter().any(|t| t.id == id) {
                    self.active_tab = Some(id);
                }
            }
            ViewCmd::CloseTab(id) => {
                let position = self.tabs.iter().position(|t| t.id == id);
                self.tabs.retain(|t| t.id != id);
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
        }
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
            PaneId::Grid => self.row_count(snapshot),
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
        crate::tree::visible(&snapshot.explorer.nodes, self.filter.as_deref())
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
    fn set_filter(&mut self, filter: Option<String>, snapshot: &Snapshot) {
        let was = self
            .tree
            .selected
            .and_then(|row| self.visible_node(snapshot, row))
            .map(|node| (node.conn, node.node_ref.clone()));

        self.filter = filter;

        let now = was.and_then(|(conn, node_ref)| {
            self.visible_rows(snapshot).iter().position(|&index| {
                snapshot
                    .explorer
                    .get(index)
                    .is_some_and(|node| node.conn == conn && node.node_ref == node_ref)
            })
        });
        match now {
            Some(row) => self.select_tree_row(row, snapshot),
            None if self.visible_len(snapshot) == 0 => self.tree.selected = None,
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
    fn rows_of<'a>(&self, snapshot: &'a Snapshot) -> Option<&'a Arc<PagedResult>> {
        let tab = self.active_tab?;
        let open = self.tabs.iter().find(|t| t.id == tab)?;
        snapshot.preview(open.conn, &open.table)?.data.ready()
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
        let rows = Arc::clone(rows);
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

    use sqlake_app::snapshot::{LoadState, PreviewView};
    use sqlake_app::tree::{NodeState, VisibleNode};
    use sqlake_core::id::ConnId;
    use sqlake_core::node::{NodeKind, NodeRef, TableRef};
    use sqlake_core::result::{Column, ResultSet, Row};
    use sqlake_core::value::Value;

    use super::*;

    fn table() -> TableRef {
        TableRef::new(["public", "users"])
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
            profiles: Arc::new(vec![mock_summary("mock")]),
            connections: vec![sqlake_app::snapshot::ConnectionView {
                id: conn,
                profile: mock_summary("mock").id,
                name: "mock".into(),
                color: None,
                kind: sqlake_core::capability::DriverKind::Mock,
                status: sqlake_app::snapshot::ConnStatus::Ready,
                capabilities: None,
            }],
            explorer,
            previews: vec![PreviewView {
                conn,
                table: table(),
                sort: None,
                loaded_rows: grid_rows,
                data: LoadState::Ready(rows(grid_rows, grid_cols)),
                last_error: None,
            }],
            busy: Vec::new(),
            should_quit: false,
        }
    }

    /// A snapshot and a `UiState` with its one preview already open as tab
    /// `TabId::new(1)` — what the render loop would have done on the frame
    /// that first drew it.
    fn setup(tree_rows: usize, grid_rows: usize, grid_cols: usize) -> (Snapshot, UiState) {
        let conn = ConnId::new();
        let snap = snapshot(conn, tree_rows, grid_rows, grid_cols);
        let mut ui = UiState::new();
        // As a frame would leave it: the pane viewports are the areas inside
        // the borders, the screen is the whole of it.
        ui.set_screen(Rect::new(0, 0, 82, 12));
        ui.set_viewport(PaneId::Explorer, Rect::new(0, 1, 20, 10));
        ui.set_viewport(PaneId::Grid, Rect::new(21, 1, 60, 10));
        ui.apply(
            ViewCmd::OpenTab {
                conn,
                table: table(),
            },
            &snap,
        );
        (snap, ui)
    }

    #[test]
    fn filtering_keeps_the_same_node_selected_not_the_same_row() {
        // The answer M2 owed to its second open question. Rows are renumbered
        // by everything the filter removes above them, so a selection left at
        // its old number lands on a node the user never pointed at — and then
        // `Enter` opens it.
        let (snap, mut ui) = setup(30, 0, 0);
        ui.apply(ViewCmd::SelectTreeRow(17), &snap);
        assert_eq!(
            ui.visible_node(&snap, 17).map(|n| n.label.clone()),
            Some("n17".to_owned())
        );

        // `n17` is the only row left, so it is row zero now.
        ui.apply(ViewCmd::SetFilter(Some("n17".to_owned())), &snap);
        assert_eq!(ui.tree.selected, Some(0));
        assert_eq!(
            ui.visible_node(&snap, 0).map(|n| n.label.clone()),
            Some("n17".to_owned())
        );

        // And back again: clearing restores the tree, and the selection goes
        // with the node rather than staying at zero.
        ui.apply(ViewCmd::SetFilter(None), &snap);
        assert_eq!(ui.tree.selected, Some(17));
    }

    #[test]
    fn a_selection_the_filter_removes_falls_to_the_first_row() {
        // There is nothing to follow, and the first row is where a search
        // leaves you anyway.
        let (snap, mut ui) = setup(30, 0, 0);
        ui.apply(ViewCmd::SelectTreeRow(17), &snap);
        ui.apply(ViewCmd::SetFilter(Some("n2".to_owned())), &snap);
        assert_eq!(ui.tree.selected, Some(0));
        assert_eq!(
            ui.visible_node(&snap, 0).map(|n| n.label.clone()),
            Some("n2".to_owned())
        );
    }

    #[test]
    fn a_filter_that_matches_nothing_selects_nothing() {
        let (snap, mut ui) = setup(30, 0, 0);
        ui.apply(ViewCmd::SelectTreeRow(3), &snap);
        ui.apply(ViewCmd::SetFilter(Some("zzz".to_owned())), &snap);
        assert_eq!(ui.tree.selected, None);
    }

    #[test]
    fn moving_the_selection_stays_inside_the_filtered_rows() {
        // The clamp reads the visible count, not the tree's: otherwise `G`
        // runs off the end of a filtered list into rows that are not drawn.
        let (snap, mut ui) = setup(30, 0, 0);
        ui.apply(ViewCmd::SetFilter(Some("n1".to_owned())), &snap);
        // n1, n10..n19 — eleven rows.
        ui.apply(ViewCmd::MoveTreeSelection(1000), &snap);
        assert_eq!(ui.tree.selected, Some(10));
        assert_eq!(
            ui.visible_node(&snap, 10).map(|n| n.label.clone()),
            Some("n19".to_owned())
        );
    }

    #[test]
    fn scrolling_stops_at_the_last_screenful() {
        let (snap, mut ui) = setup(30, 0, 0);
        ui.apply(
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
            ui.apply(
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
        ui.apply(ViewCmd::ScrollToEnd(PaneId::Explorer), &snap);
        assert_eq!(ui.tree.offset, 0, "there is nothing below to reach");
    }

    #[test]
    fn a_track_click_lands_proportionally() {
        let (snap, mut ui) = setup(30, 0, 0);
        ui.apply(
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
        ui.apply(ViewCmd::SelectTreeRow(25), &snap);
        assert_eq!(ui.tree.selected, Some(25));
        // Just far enough that row 25 is the last visible row, not a jump that
        // puts it in the middle and loses the reader's place.
        assert_eq!(ui.tree.offset, 16);

        ui.apply(ViewCmd::SelectTreeRow(2), &snap);
        assert_eq!(ui.tree.offset, 2);
    }

    #[test]
    fn the_first_move_selects_the_first_row() {
        let (snap, mut ui) = setup(30, 0, 0);
        assert_eq!(ui.tree.selected, None);
        ui.apply(ViewCmd::MoveTreeSelection(1), &snap);
        assert_eq!(ui.tree.selected, Some(0), "not row one");
    }

    #[test]
    fn selection_cannot_leave_the_content() {
        let (snap, mut ui) = setup(3, 0, 0);
        ui.apply(ViewCmd::MoveTreeSelection(-5), &snap);
        assert_eq!(ui.tree.selected, Some(0));
        ui.apply(ViewCmd::SelectTreeRow(99), &snap);
        assert_eq!(ui.tree.selected, Some(2));
    }

    #[test]
    fn an_empty_tree_has_nothing_selected() {
        let (snap, mut ui) = setup(0, 0, 0);
        ui.apply(ViewCmd::SelectTreeRow(0), &snap);
        assert_eq!(ui.tree.selected, None);
    }

    #[test]
    fn the_cell_cursor_stays_inside_the_result() {
        let (snap, mut ui) = setup(0, 50, 4);
        ui.apply(ViewCmd::SelectCell { row: 99, col: 99 }, &snap);
        let grid = ui.grid(TabId::new(1)).unwrap();
        assert_eq!((grid.row, grid.col), (49, 3));
    }

    #[test]
    fn moving_the_cell_cursor_scrolls_the_grid() {
        let (snap, mut ui) = setup(0, 50, 4);
        ui.apply(ViewCmd::SelectCell { row: 0, col: 0 }, &snap);
        ui.apply(ViewCmd::MoveCellSelection { drow: 20, dcol: 0 }, &snap);
        let grid = ui.grid(TabId::new(1)).unwrap();
        assert_eq!(grid.row, 20);
        assert_eq!(grid.row_offset, 11);
    }

    #[test]
    fn the_cell_cursor_pulls_the_grid_sideways() {
        // Sixty columns of at least the minimum width: the cursor cannot reach
        // column fifty without the grid scrolling after it.
        let (snap, mut ui) = setup(0, 10, 60);
        ui.apply(ViewCmd::SelectCell { row: 0, col: 50 }, &snap);
        let grid = ui.grid(TabId::new(1)).unwrap();
        assert_eq!(grid.col, 50);
        assert!(
            grid.col_offset > 0 && grid.col_offset <= 50,
            "{}",
            grid.col_offset
        );

        ui.apply(ViewCmd::SelectCell { row: 0, col: 0 }, &snap);
        assert_eq!(ui.grid(TabId::new(1)).unwrap().col_offset, 0, "and back");
    }

    #[test]
    fn a_grid_with_no_rows_ignores_the_cursor() {
        let (snap, mut ui) = setup(0, 0, 0);
        ui.apply(ViewCmd::SelectCell { row: 3, col: 3 }, &snap);
        assert!(ui.grid(TabId::new(1)).is_none_or(|g| g.row == 0));
    }

    #[test]
    fn a_resized_column_keeps_its_width() {
        let (snap, mut ui) = setup(0, 10, 3);
        let natural = {
            let rows = ui.rows_of(&snap).unwrap().clone();
            ui.grid_mut(TabId::new(1)).grid(&rows).columns()[1].natural_width
        };
        ui.apply(ViewCmd::ResizeColumn { col: 1, delta: 5 }, &snap);
        let grid = ui.grid(TabId::new(1)).unwrap();
        assert_eq!(grid.width(1, natural), natural + 5);
        assert_eq!(grid.width(0, natural), natural, "only the one column moved");
    }

    #[test]
    fn a_column_cannot_be_dragged_to_nothing() {
        let (snap, mut ui) = setup(0, 10, 3);
        ui.apply(
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
        let rows = ui.rows_of(&snap).unwrap().clone();
        let first = ui.grid_mut(TabId::new(1)).grid(&rows) as *const RenderedGrid;
        let again = ui.grid_mut(TabId::new(1)).grid(&rows) as *const RenderedGrid;
        assert_eq!(first, again, "an unchanged snapshot must not rebuild it");
    }

    #[test]
    fn focus_cycles_between_the_two_panes() {
        let (snap, mut ui) = setup(0, 0, 0);
        assert_eq!(ui.focus, PaneId::Explorer);
        ui.apply(ViewCmd::FocusNextPane, &snap);
        assert_eq!(ui.focus, PaneId::Grid);
        ui.apply(ViewCmd::FocusNextPane, &snap);
        assert_eq!(ui.focus, PaneId::Explorer);
        ui.apply(ViewCmd::FocusPrevPane, &snap);
        assert_eq!(ui.focus, PaneId::Grid);
    }

    #[test]
    fn focus_from_outside_the_cycle_enters_it() {
        let (snap, mut ui) = setup(0, 0, 0);
        ui.apply(ViewCmd::FocusPane(PaneId::StatusBar), &snap);
        ui.apply(ViewCmd::FocusNextPane, &snap);
        assert_eq!(ui.focus, PaneId::Explorer);
    }

    #[test]
    fn the_splitter_leaves_both_panes_usable() {
        let (snap, mut ui) = setup(0, 0, 0);
        ui.apply(
            ViewCmd::MoveSplit {
                split: SplitId::Explorer,
                delta: -500,
            },
            &snap,
        );
        assert_eq!(ui.explorer_width(80), MIN_PANE_WIDTH);

        ui.apply(
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
        ui.apply(
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
        ui.apply(
            ViewCmd::MoveSplit {
                split: SplitId::Explorer,
                delta: 10,
            },
            &snap,
        );
        assert_ne!(ui.explorer_width(100), default);
        ui.apply(ViewCmd::EvenSplit(SplitId::Explorer), &snap);
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
        ui.apply(ViewCmd::SelectCell { row: 1, col: 1 }, &snap);
        let tab = ui.active_tab.unwrap();
        assert!(ui.grid(tab).is_some());

        ui.apply(ViewCmd::CloseTab(tab), &snap);
        assert!(ui.grid(tab).is_none(), "the cached grid goes with it");
    }

    #[test]
    fn opening_a_relation_already_open_selects_it_instead_of_duplicating() {
        let (snap, mut ui) = setup(0, 0, 0);
        let first = ui.active_tab.unwrap();
        let conn = ui.tabs[0].conn;

        // A second tab, so the first is no longer active — reopening it must
        // find it rather than assume it is still in front.
        ui.apply(
            ViewCmd::OpenTab {
                conn: ConnId::new(),
                table: TableRef::new(["public", "orders"]),
            },
            &snap,
        );
        assert_ne!(ui.active_tab, Some(first));

        ui.apply(
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
        ui.apply(
            ViewCmd::OpenTab {
                conn: ConnId::new(),
                table: TableRef::new(["public", "orders"]),
            },
            &snap,
        );

        ui.apply(ViewCmd::CloseTab(first), &snap);
        assert_eq!(ui.tabs.len(), 1);
        assert_eq!(ui.active_tab, Some(ui.tabs[0].id));
    }

    #[test]
    fn closing_the_last_tab_leaves_nothing_selected() {
        let (snap, mut ui) = setup(0, 0, 0);
        let tab = ui.active_tab.unwrap();
        ui.apply(ViewCmd::CloseTab(tab), &snap);
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

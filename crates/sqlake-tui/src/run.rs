//! The render loop: one frame, then wait for something to change.
//!
//! The loop draws and then blocks on two sources — terminal events and new
//! snapshots — so an idle client costs nothing and a slow query never blocks a
//! keystroke. Everything expensive happened in the store task before the
//! snapshot arrived.
//!
//! The [`HitMap`] is rebuilt on every frame and consulted for the events that
//! follow it, so a click is always answered against the layout that drew the
//! pixels it was aimed at.
//!
//! A frame is drawn only when something changed. Mouse capture reports every
//! cell the pointer crosses, so redrawing per event would relayout, rebuild the
//! hit map and reformat every visible cell a hundred times for one sweep across
//! the screen — while [`MouseState`] is careful to report nothing for exactly
//! that reason.

use std::io;
use std::sync::Arc;
use std::time::Instant;

use futures::{FutureExt as _, StreamExt as _};
use ratatui::Frame;
use ratatui::crossterm::event::{Event, EventStream, KeyEventKind};
use ratatui::layout::Rect;
use sqlake_app::action::Action;
use sqlake_app::snapshot::{ConnStatus, ConnectionView, Snapshot};
use sqlake_app::store::Store;
use tokio::sync::watch;

use crate::chrome;
use crate::datagrid;
use crate::editor::{Edited, Editor};
use crate::hit::{HitMap, PaneId, Target};
use crate::input::{self, InputContext};
use crate::intent::Handover;
use crate::intent::Intent;
use crate::mouse::MouseState;
use crate::overlay;
use crate::terminal::{TerminalGuard, Tui};
use crate::tree;
use crate::ui::{TabContent, UiState};

/// Run until the store says to quit or the terminal closes.
///
/// # Errors
///
/// Propagates terminal write failures. The caller still holds the
/// `TerminalGuard`, so the screen is restored either way.
pub async fn run(
    terminal: &mut Tui,
    guard: &mut TerminalGuard,
    store: &Store,
    mouse_enabled: bool,
    editor: &Editor,
) -> io::Result<()> {
    let mut mouse = MouseState::new();
    let mut events = EventStream::new();
    let mut snapshots = store.subscribe();
    let mut ui = initial_ui(&snapshots.borrow_and_update().clone());
    let mut snapshot = snapshots.borrow_and_update().clone();
    // The first snapshot as well as every later one: connecting is dispatched
    // before the loop starts, so a connection that failed while the terminal
    // was being taken over is already in this one — and a failure produces no
    // further snapshot to be caught by the arm below.
    let mut hits = HitMap::new();
    let mut dirty = true;

    loop {
        if dirty {
            // Cleared rather than replaced: one entry per visible cell adds up
            // to hundreds, and growing a fresh `Vec` for them every frame is a
            // cost with nothing to show for it.
            hits.clear();
            terminal.draw(|frame| draw(frame, &mut ui, &snapshot, &mut hits))?;
            dirty = false;
        }

        // After the frame, through the terminal's own writer. The alternate
        // screen is up, so a second thing reaching for stdout would corrupt
        // it — and OSC 52 is the one escape sequence here that is not the
        // renderer's.
        if let Some(sequence) = ui.take_copy() {
            use std::io::Write as _;
            let backend = terminal.backend_mut();
            backend.write_all(sequence.as_bytes())?;
            backend.flush()?;
        }

        if snapshot.should_quit {
            return Ok(());
        }

        let mut intents = Vec::new();
        tokio::select! {
            event = events.next() => match event {
                // The terminal is gone; there is nothing left to draw on.
                None | Some(Err(_)) => return Ok(()),
                Some(Ok(event)) => {
                    dirty |= apply_event(
                        event, &hits, &mut mouse, &ui, &snapshot, mouse_enabled, &mut intents,
                    );
                    ui.hover = mouse.hovered();
                }
            },
            changed = snapshots.changed() => {
                if changed.is_err() {
                    // The store is gone, which is a crash rather than a quit:
                    // `should_quit` above is the way out that means "finished".
                    return Err(io::Error::other("the store stopped unexpectedly"));
                }
                snapshot = snapshots.borrow_and_update().clone();
                raise_connection_failure(&snapshot, &mut ui);
                ui.raise_preview_errors(&snapshot);
                ui.raise_approvals(&snapshot);
                ui.raise_query_errors(&snapshot);
                ui.close_disconnected_tabs(&snapshot);
                dirty = true;
            }
        }

        // Whatever else has arrived while that was being decided. A key repeat
        // or a drag delivers faster than a frame takes, and handling one event
        // per frame turns the backlog into lag that never catches up.
        while let Some(Ok(event)) = events.next().now_or_never().flatten() {
            dirty |= apply_event(
                event,
                &hits,
                &mut mouse,
                &ui,
                &snapshot,
                mouse_enabled,
                &mut intents,
            );
            ui.hover = mouse.hovered();
        }

        for intent in intents {
            match intent {
                // Applied here, on this thread, before the next frame. A wheel
                // notch that went through the store would arrive a round trip
                // later than the hand that turned it.
                Intent::View(cmd) => {
                    // A view command is applied here, on this thread — but
                    // scrolling towards the end of a preview is also a reason
                    // to fetch, and that part does go to the store.
                    if let Some(action) = ui.apply(cmd, &snapshot) {
                        store.dispatch(action);
                    }
                    dirty = true;
                }
                Intent::App(action) => {
                    // The tab remembers which run it started, because the id
                    // is chosen here and the store answers under it. Recorded
                    // before dispatching: the reply can arrive on the next
                    // snapshot, and a tab that had not written the id down yet
                    // would not recognise its own result.
                    if let Action::RunQuery { query, .. } = &action
                        && let Some(tab) = ui.active_tab
                    {
                        // The run this tab is about to stop showing. Nothing
                        // else can be: a query is keyed by the run, so leaving
                        // it behind would keep a whole result set in the store
                        // that nothing could ever reach again.
                        if let Some(previous) = ui.query_of(tab) {
                            store.dispatch(Action::ForgetQuery(previous));
                        }
                        ui.set_query(tab, *query);
                    }
                    store.dispatch(action);
                }
                // With the terminal handed over and the loop stopped. Nothing
                // is drawn until it comes back, which is the point: the editor
                // owns the screen while it runs.
                Intent::Handover(Handover::Edit(tab)) => {
                    // The event stream goes first, and a fresh one comes back
                    // after. Its reader thread sits in a blocking read on
                    // `/dev/tty` from the moment the stream returns `Pending`,
                    // and nothing about handing the screen over stops it: the
                    // first key typed into the editor would be eaten there and
                    // then delivered here as a command once the screen is back,
                    // which is how a `q` meant for vim quits the client.
                    drop(events);
                    let handed = hand_over(terminal, guard, editor, &mut ui, tab);
                    events = EventStream::new();
                    handed?;
                    dirty = true;
                }
            }
        }
    }
}

/// Write the tab's buffer out, run the editor on it, and take back what came
/// back.
///
/// The store's task keeps running throughout — a query still streaming is
/// still consumed, and the display catches up when the screen returns.
///
/// # Errors
///
/// Only a terminal that could not be given back or taken again. Everything
/// about the editor itself — missing, refused, exited without saving — is a
/// message to the user, because none of it is a reason to stop the client.
fn hand_over(
    terminal: &mut Tui,
    guard: &mut TerminalGuard,
    editor: &Editor,
    ui: &mut UiState,
    tab: sqlake_core::id::TabId,
) -> io::Result<()> {
    let Some(text) = ui.buffer_of(tab).map(str::to_owned) else {
        return Ok(());
    };
    let path = editor.path_for(tab);
    let outcome = guard.suspended(terminal, || editor.edit(&path, &text))?;
    apply_edit(ui, editor, tab, outcome);
    Ok(())
}

/// What the screen does with each way an edit can end.
///
/// Split from `hand_over` so it can be tested: the half above it takes the
/// terminal over, and a test that ran it would put the terminal running the
/// tests into raw mode.
fn apply_edit(ui: &mut UiState, editor: &Editor, tab: sqlake_core::id::TabId, outcome: Edited) {
    match outcome {
        Edited::Changed(back) => ui.set_buffer(tab, back),
        // Nothing to say. Closing without saving is how somebody says no, and
        // a message about it is a message about a decision already made.
        Edited::Unchanged => {}
        // A notice rather than a dialog: the buffer is as it was, so there is
        // nothing to answer — only a setting worth knowing about.
        Edited::Returned => ui.warn(editor.hurried()),
        // A dialog, because nothing happened and the reason is a sentence
        // wider than the status bar: `$EDITOR` naming a program that is not
        // installed reads as `e` doing nothing at all.
        Edited::Failed(why) => ui.raise_error("The editor", why),
    }
}

/// The reason a connection failed, in the one place it fits.
///
/// The row in the explorer says which connection is broken and keeps saying
/// it, but the pane is twenty-six columns wide at the sizes this client draws
/// at, so "could not connect: password authentication failed for user…" is cut
/// to about four words. The row is the state; this is the reason.
///
/// Shown once per failure: dismissed, not re-raised by the next unrelated
/// snapshot.
fn raise_connection_failure(snapshot: &Snapshot, ui: &mut UiState) {
    // Skipping the ones already reported before looking at the status: a single
    // remembered id would let the first failure hide every later one, because
    // it stays in the list and is what a plain search keeps finding.
    let failure = snapshot
        .connections
        .iter()
        .filter(|c| !ui.reported_failures.contains(&c.id))
        .find_map(|c| match &c.status {
            ConnStatus::Failed(why) => Some((c.id, c.name.clone(), why.clone())),
            _ => None,
        });
    let Some((id, name, why)) = failure else {
        return;
    };
    ui.reported_failures.insert(id);
    // The menu sits above the modal — `Z_MENU` is higher than `Z_MODAL`, which
    // is what keeps a click on it from reaching the cell underneath — so a
    // menu left open would float over the dialog and stay clickable, which is
    // the fall-through the backdrop exists to prevent.
    ui.menu = None;
    ui.modal = Some(overlay::Modal::error(
        format!("{name} could not be opened"),
        why,
    ));
}

/// A connection dispatched before the terminal was taken over can already have
/// failed by the time the loop starts, and a failed connect publishes nothing
/// afterwards — so waiting for the next snapshot would mean waiting for one
/// that never comes.
fn initial_ui(snapshot: &Snapshot) -> UiState {
    let mut ui = UiState::new();
    raise_connection_failure(snapshot, &mut ui);
    ui.raise_preview_errors(snapshot);
    ui.raise_approvals(snapshot);
    ui.raise_query_errors(snapshot);
    ui.close_disconnected_tabs(snapshot);
    ui
}

/// Translate one event, collecting its intents. Returns whether the screen has
/// to be drawn again because of it.
fn apply_event(
    event: Event,
    hits: &HitMap,
    mouse: &mut MouseState,
    ui: &UiState,
    snapshot: &Snapshot,
    mouse_enabled: bool,
    intents: &mut Vec<Intent>,
) -> bool {
    // A resize invalidates every rectangle the pointer was measured against,
    // and `Target::TreeRow` is an index into a layout that no longer exists —
    // a press held across it would be released onto a different row.
    if matches!(event, Event::Resize(..)) {
        mouse.reset();
        return true;
    }

    let before = mouse.hovered();
    let produced = from_event(event, hits, mouse, ui, snapshot, mouse_enabled);
    let hover_moved = mouse.hovered() != before;
    let any = !produced.is_empty();
    intents.extend(produced);
    // Hover is the one thing that changes the screen without producing an
    // intent, and it is why the pointer moving *within* a target is free.
    any || hover_moved
}

fn from_event(
    event: Event,
    hits: &HitMap,
    mouse: &mut MouseState,
    ui: &UiState,
    snapshot: &Snapshot,
    mouse_enabled: bool,
) -> Vec<Intent> {
    let mut ctx = context(ui, snapshot);
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => input::on_key(key, &ctx),
        Event::Mouse(event) if mouse_enabled => {
            let gestures = mouse.feed(event, hits, Instant::now());
            // After the event, not before it: a right-click *is* how the
            // pointer arrives at a cell on a terminal that reports no motion,
            // and reading a position recorded on the previous event would open
            // the menu wherever the mouse was last heard from.
            ctx.pointer = mouse.position();
            gestures
                .into_iter()
                .flat_map(|(target, gesture)| input::on_mouse(target, gesture, &ctx))
                .collect()
        }
        // A resize redraws by falling out of the loop; the layout is computed
        // from the frame each time and has nothing to invalidate.
        _ => Vec::new(),
    }
}

fn context<'a>(ui: &'a UiState, snapshot: &'a Snapshot) -> InputContext<'a> {
    InputContext {
        snapshot,
        focus: ui.focus,
        modal_open: ui.modal.is_some(),
        modal: ui.modal.as_ref(),
        // Overwritten from the pointer's own position on a mouse event; a key
        // press has no pointer and does not read it.
        pointer: (0, 0),
        selection: ui
            .active_tab
            .and_then(|id| ui.grid(id))
            .map(|g| g.selection(ui.active_sort(snapshot))),
        menu: ui.menu.as_ref(),
        // The selected row's connection, falling back to the first: with
        // several open, `D` has to disconnect the one being looked at.
        connection: ui
            .tree
            .selected
            .and_then(|row| ui.visible_node(snapshot, row))
            .map(|node| node.conn)
            .or_else(|| snapshot.connections.first().map(|c| c.id)),
        tree_selection: ui.tree.selected,
        grid_column: ui.active_tab.and_then(|tab| ui.grid(tab)).map(|g| g.col),
        tabs: &ui.tabs,
        active_tab: ui.active_tab,
        toasts: &ui.toasts,
        filter: ui.filter.as_ref(),
    }
}

fn draw(frame: &mut Frame<'_>, ui: &mut UiState, snapshot: &Snapshot, hits: &mut HitMap) {
    let area = frame.area();
    if chrome::too_small(area) {
        chrome::render_too_small(frame, area);
        return;
    }

    let frames = chrome::layout(area, ui);
    chrome::tab_bar(
        frame,
        hits,
        frames.tab_bar,
        ui,
        snapshot.connections.iter().any(ConnectionView::is_live),
    );

    let explorer = chrome::pane(
        frame,
        hits,
        frames.explorer,
        PaneId::Explorer,
        "Explorer",
        ui.focus == PaneId::Explorer,
    );
    // The rows, not the pane: the search box takes the top line, and a
    // viewport measured with it in leaves the last row unscrollable-to.
    ui.set_viewport(
        PaneId::Explorer,
        tree::rows_area(explorer, ui.filter.as_ref()),
    );
    tree::render(
        frame,
        hits,
        explorer,
        snapshot,
        &ui.tree,
        ui.filter.as_ref(),
    );

    chrome::splitter(
        frame,
        hits,
        frames.splitter,
        matches!(ui.hover, Some(Target::Splitter(_))),
    );

    // Owned, because `ui.grid_mut` below needs `&mut ui`.
    let active = ui
        .active_tab
        .and_then(|id| ui.tabs.iter().find(|t| t.id == id))
        .map(|t| (t.id, t.conn, t.content.clone()));
    let title = active.as_ref().map_or_else(
        || "Preview".to_owned(),
        |(_, _, content)| content.title().into_owned(),
    );
    let grid = chrome::pane(
        frame,
        hits,
        frames.grid,
        PaneId::Grid,
        &title,
        ui.focus == PaneId::Grid,
    );
    // The *rows*, not the pane: the header takes a row and the scrollbar a
    // column, and a viewport measured without them makes `ScrollToEnd` stop a
    // row short of the end.
    ui.set_viewport(PaneId::Grid, datagrid::body_area(grid));
    let mut detail = None;
    // A SQL tab shows its rows once it has some, and its buffer until then.
    // Not both: the buffer is one key press away in `$EDITOR`, and splitting
    // the pane costs the grid rows it needs more. T7 revisits it, because an
    // error has to point at a line somebody can see.
    let sql_rows = active
        .as_ref()
        .and_then(|(_, _, content)| match content {
            TabContent::Sql { query, .. } => snapshot.query((*query)?),
            TabContent::Preview(_) | TabContent::Definition { .. } => None,
        })
        .is_some_and(|q| q.data.ready().is_some());
    if let Some((id, _, TabContent::Sql { text, .. })) = &active
        && !sql_rows
    {
        // The whole pane, not `body_area`: there is no header row and no
        // scrollbar column to leave out, because there is no grid.
        ui.set_viewport(PaneId::Grid, grid);
        ui.set_sql_lines(crate::sql::line_count(text));
        // Where the *server* said, which is why nothing here works it out: the
        // driver converted its own dialect's answer, and this crate names no
        // driver.
        let at = ui
            .query_of(*id)
            .and_then(|query| snapshot.query(query))
            .and_then(|query| query.failed_at);
        crate::sql::render(frame, grid, text, ui.sql_offset(), at);
    }
    if let Some((id, _, TabContent::Sql { .. })) = &active
        && sql_rows
        && let Some(rows) = snapshot
            .query(ui.query_of(*id).expect("shown only when there is a query"))
            .and_then(|q| q.data.ready())
    {
        let rows = Arc::clone(rows);
        let id = *id;
        // Sorting a result is not paging a relation: there is no column to
        // re-fetch under a different order, so the header is not a control
        // here and says so by not being one.
        datagrid::render_rows(frame, hits, grid, &rows, ui.grid_mut(id), false, None);
        if frames.detail.height > 0 {
            detail = ui.grid_mut(id).detail();
        }
    }
    if let Some((id, conn, TabContent::Definition { table, section })) = &active {
        let (id, section) = (*id, *section);
        match snapshot.definition(*conn, table).map(|d| &d.data) {
            Some(sqlake_app::snapshot::LoadState::Ready(definition)) => {
                // The list first, because what is left of the pane is what the
                // grid gets — and working that out twice is how the two come
                // to disagree.
                // Clamped here as well as where it is picked: a refresh can
                // come back with fewer sections than the tab was on, and an
                // index past the end draws an empty pane with nothing in it
                // saying why.
                let section = section.min(definition.titles().len().saturating_sub(1));
                let body = crate::definition::sections(frame, hits, grid, definition, section);
                let summary = crate::definition::summary(definition);
                let rows = definition.rows(section).map(Arc::clone);
                let body = chrome::caption(frame, body, &summary);
                ui.set_viewport(PaneId::Grid, datagrid::body_area(body));
                if let Some(rows) = rows {
                    // Not sortable: a definition is not paged, so there is no
                    // second fetch for a header click to ask for.
                    datagrid::render_rows(frame, hits, body, &rows, ui.grid_mut(id), false, None);
                    if frames.detail.height > 0 {
                        detail = ui.grid_mut(id).detail();
                    }
                }
            }
            Some(sqlake_app::snapshot::LoadState::Failed(why)) => {
                ui.set_viewport(PaneId::Grid, grid);
                datagrid::message(frame, grid, why, ratatui::style::Color::Red);
            }
            // Nothing held for it can mean two things, and only one of them is
            // worth a spinner: the store has not seen the fetch yet, or the
            // connection closed and took the definition with it. Saying
            // "describing…" for the second is a wait that never ends, because
            // nothing is going to answer.
            state => {
                ui.set_viewport(PaneId::Grid, grid);
                let live = state.is_some()
                    || snapshot
                        .connection(*conn)
                        .is_some_and(sqlake_app::snapshot::ConnectionView::is_live);
                if live {
                    datagrid::message(frame, grid, "describing…", ratatui::style::Color::Yellow);
                } else {
                    datagrid::message(
                        frame,
                        grid,
                        "the connection is closed",
                        ratatui::style::Color::DarkGray,
                    );
                }
            }
        }
    }
    if let Some((id, conn, TabContent::Preview(table))) = active
        && let Some(preview) = snapshot.preview(conn, &table)
    {
        // A driver that cannot order a preview — BigQuery — makes the header
        // not a control, and that is the front-end's rendering of a
        // capability rather than a branch on which driver it is.
        let sortable = snapshot
            .connection(conn)
            .is_some_and(ConnectionView::can_sort_preview);
        datagrid::render(frame, hits, grid, preview, ui.grid_mut(id), sortable);

        // Built from the value rather than from the cell the grid drew: the
        // grid clamps at `MAX_CELL_CHARS` and writes `{2 keys}` for a
        // document, which is what somebody opening this pane is trying to see
        // past. Cached per cell, so the value is not sanitised again on a
        // frame drawn for a spinner tick.
        if frames.detail.height > 0 {
            detail = ui.grid_mut(id).detail();
        }
    }

    // Outside the preview: the pane has already taken its rows from the grid,
    // and drawing nothing into them when the tab it was opened over has gone
    // leaves a band of screen that belongs to no pane at all.
    if frames.detail.height > 0 {
        // The pane is a hit target and a scroll position of its own: the
        // values it exists for are longer than any pane, so showing the first
        // few rows of one and no way to reach the rest would be worse than not
        // opening it.
        hits.push(
            frames.detail,
            crate::hit::Z_BASE,
            Target::Pane(PaneId::Detail),
        );
        ui.set_viewport(PaneId::Detail, frames.detail);
        ui.set_detail_rows(detail.as_ref().map_or(0, |d| d.lines.len()));
        crate::detail::render(
            frame,
            frames.detail,
            detail.as_ref().map(AsRef::as_ref),
            ui.detail_offset(),
        );
    }

    let selected = ui
        .active_tab
        .and_then(|id| ui.grid(id))
        .and_then(|g| g.selected_cells(ui.active_sort(snapshot)));
    chrome::status_bar(
        frame,
        hits,
        frames.status_bar,
        snapshot,
        selected,
        ui.can_run(snapshot),
    );

    // Toasts first so a dialog covers them: a message drawn over the thing
    // waiting for an answer hides the answer.
    overlay::toasts(frame, hits, body_of(frames), &ui.toasts);
    if let Some(dialog) = ui.modal.clone() {
        overlay::modal(frame, hits, area, &dialog);
    }
    // Last, and at `Z_MENU`: a menu covers what it was opened over, and a click
    // on it must not fall through to the cell underneath.
    if let Some(menu) = ui.menu.clone() {
        crate::menu::render(frame, hits, area, &menu);
    }
}

/// The area between the bars, which is where a toast belongs: over the content
/// rather than over the status bar it would hide.
fn body_of(frames: chrome::Frames) -> Rect {
    Rect::new(
        frames.explorer.x,
        frames.explorer.y,
        frames
            .explorer
            .width
            .saturating_add(frames.splitter.width)
            .saturating_add(frames.grid.width),
        frames.explorer.height,
    )
}

/// Wait for the store to publish something matching `predicate`.
///
/// Used by the binary to hold the first frame until a connection has been
/// asked for, so the explorer is never drawn empty for one frame and then
/// filled.
pub async fn until(
    snapshots: &mut watch::Receiver<Arc<Snapshot>>,
    predicate: impl Fn(&Snapshot) -> bool,
) {
    loop {
        if predicate(&snapshots.borrow_and_update().clone()) {
            return;
        }
        if snapshots.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, MouseEventKind};
    use ratatui::layout::Position;
    use sqlake_app::action::Action;
    use sqlake_app::snapshot::{ConnStatus, ConnectionView};
    use sqlake_app::store::Drivers;
    use sqlake_core::id::ConnId;
    use sqlake_core::node::{NodeRef, TableRef};
    use sqlake_core::result::PageRequest;
    use sqlake_driver_mock::{Behaviour, MockDriver, MockProfiles, mock_summary};

    use super::*;

    fn store() -> Store {
        store_of(Behaviour::instant())
    }

    /// The same, advertising a capability set of its own.
    fn store_of_with(
        behaviour: Behaviour,
        capabilities: sqlake_core::capability::Capabilities,
    ) -> Store {
        Store::spawn(
            Drivers::new().with(Arc::new(
                MockDriver::new(behaviour).with_capabilities(capabilities),
            )),
            Arc::new(MockProfiles::default()),
            PageRequest::DEFAULT_LIMIT,
            None,
        )
    }

    fn store_of(behaviour: Behaviour) -> Store {
        Store::spawn(
            Drivers::new().with(Arc::new(MockDriver::new(behaviour))),
            Arc::new(MockProfiles::default()),
            PageRequest::DEFAULT_LIMIT,
            None,
        )
    }

    async fn connected() -> (Store, Arc<Snapshot>) {
        connected_to(store()).await
    }

    async fn connected_to(store: Store) -> (Store, Arc<Snapshot>) {
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect {
            profile: mock_summary("mock").id,
            conn: ConnId::new(),
        });
        until(&mut rx, |s| {
            s.connections.first().is_some_and(ConnectionView::is_ready)
        })
        .await;
        let snap = rx.borrow_and_update().clone();
        (store, snap)
    }

    #[test]
    fn an_empty_explorer_says_which_kind_of_empty_it_is() {
        // Two different problems that look identical on screen: nothing to
        // connect to, and nothing connected yet. Only one of them is fixed by
        // pressing a key, and a blank pane says neither.
        let mut ui = UiState::new();
        let fresh = Snapshot::default();
        let (rows, _) = render(&fresh, &mut ui, 120, 30);
        let screen = rows.join("\n");
        assert!(screen.contains("no connections"), "{screen}");

        let configured = Snapshot {
            profiles: Arc::new(vec![mock_summary("mock")]),
            ..Snapshot::default()
        };
        let (rows, _) = render(&configured, &mut ui, 120, 30);
        let screen = rows.join("\n");
        assert!(screen.contains("nothing connected"), "{screen}");
        assert!(!screen.contains("no connections"), "{screen}");

        // And a third: the tree is empty because the handshake has not
        // finished. Telling the user to press `c` here would open a second
        // connection to the profile they are already waiting for.
        let summary = mock_summary("mock");
        let connecting = Snapshot {
            connections: vec![ConnectionView {
                id: ConnId::new(),
                profile: summary.id.clone(),
                name: summary.name.clone(),
                color: None,
                kind: summary.kind,
                status: ConnStatus::Connecting,
                capabilities: None,
                tree: std::sync::Arc::default(),
            }],
            profiles: Arc::new(vec![summary]),
            ..Snapshot::default()
        };
        let (rows, _) = render(&connecting, &mut ui, 120, 30);
        let screen = rows.join("\n");
        assert!(screen.contains("connecting"), "{screen}");
        assert!(!screen.contains("press c"), "{screen}");
    }

    fn render(snapshot: &Snapshot, ui: &mut UiState, w: u16, h: u16) -> (Vec<String>, HitMap) {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut hits = HitMap::new();
        terminal
            .draw(|frame| draw(frame, ui, snapshot, &mut hits))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows = (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect();
        (rows, hits)
    }

    #[tokio::test]
    async fn a_connected_screen_shows_the_tree() {
        let (_store, snap) = connected().await;
        let mut ui = UiState::new();
        let (text, _) = render(&snap, &mut ui, 100, 30);
        assert!(text.join("").contains("public"), "{text:?}");
    }

    #[tokio::test]
    async fn the_explorer_viewport_leaves_out_the_search_box() {
        // The scroll clamp and the page size are measured from it, so a
        // viewport that counts the box's row leaves the last row of a filtered
        // tree impossible to scroll to.
        let (_store, snap) = connected().await;
        let mut ui = UiState::new();
        let _ = render(&snap, &mut ui, 100, 30);
        let whole = ui.viewport(PaneId::Explorer).height;

        let _ = ui.apply(
            crate::intent::ViewCmd::SetFilter(Some(crate::ui::Filter::opening())),
            &snap,
        );
        let _ = render(&snap, &mut ui, 100, 30);
        assert_eq!(ui.viewport(PaneId::Explorer).height, whole - 1);
    }

    #[tokio::test]
    async fn every_pane_is_reachable_with_the_mouse() {
        let (_store, snap) = connected().await;
        let mut ui = UiState::new();
        let (_, hits) = render(&snap, &mut ui, 100, 30);

        let mut seen = std::collections::BTreeSet::new();
        for x in 0..100 {
            for y in 0..30 {
                if let Some(target) = hits.at(Position::new(x, y)) {
                    seen.insert(
                        format!("{target:?}")
                            .split(['(', ' '])
                            .next()
                            .unwrap()
                            .to_owned(),
                    );
                }
            }
        }
        // A pane the mouse cannot land on is a pane the mouse cannot focus.
        assert!(seen.contains("Pane"), "{seen:?}");
        assert!(seen.contains("Splitter"), "{seen:?}");
        assert!(seen.contains("TreeRow"), "{seen:?}");
    }

    /// Feed one event the way the loop does, and report what it produced.
    fn step(
        event: Event,
        hits: &HitMap,
        mouse: &mut MouseState,
        ui: &UiState,
        snapshot: &Snapshot,
    ) -> (Vec<Intent>, bool) {
        let mut intents = Vec::new();
        let dirty = apply_event(event, hits, mouse, ui, snapshot, true, &mut intents);
        (intents, dirty)
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(ratatui::crossterm::event::KeyEvent::new(
            code,
            ratatui::crossterm::event::KeyModifiers::NONE,
        ))
    }

    fn mouse_at(kind: MouseEventKind, x: u16, y: u16) -> Event {
        Event::Mouse(ratatui::crossterm::event::MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
        })
    }

    #[tokio::test]
    async fn a_pointer_crossing_one_target_does_not_ask_for_a_frame() {
        let (_store, snap) = connected().await;
        let mut ui = UiState::new();
        let (_, hits) = render(&snap, &mut ui, 100, 30);
        let mut mouse = MouseState::new();

        // Settle on the explorer, then move within it. Mouse capture reports
        // every cell, and redrawing for each is a full relayout, a rebuilt hit
        // map and every visible cell reformatted.
        let _ = step(
            mouse_at(MouseEventKind::Moved, 10, 5),
            &hits,
            &mut mouse,
            &ui,
            &snap,
        );
        let (intents, dirty) = step(
            mouse_at(MouseEventKind::Moved, 11, 5),
            &hits,
            &mut mouse,
            &ui,
            &snap,
        );
        assert!(intents.is_empty());
        assert!(!dirty, "a move inside one target changed nothing to draw");
    }

    #[tokio::test]
    async fn crossing_into_another_target_does_ask_for_one() {
        let (_store, snap) = connected().await;
        let mut ui = UiState::new();
        let (_, hits) = render(&snap, &mut ui, 100, 30);
        let mut mouse = MouseState::new();

        let _ = step(
            mouse_at(MouseEventKind::Moved, 10, 5),
            &hits,
            &mut mouse,
            &ui,
            &snap,
        );
        // Onto the splitter, which highlights while the pointer is on it.
        let splitter_x = chrome::layout(Rect::new(0, 0, 100, 30), &mut ui).splitter.x;
        let (_, dirty) = step(
            mouse_at(MouseEventKind::Moved, splitter_x, 5),
            &hits,
            &mut mouse,
            &ui,
            &snap,
        );
        assert!(dirty, "hover is the one change that produces no intent");
        assert!(matches!(mouse.hovered(), Some(Target::Splitter(_))));
    }

    #[tokio::test]
    async fn a_press_held_across_a_resize_is_not_released_onto_a_new_layout() {
        let (_store, snap) = connected().await;
        let mut ui = UiState::new();
        let (_, hits) = render(&snap, &mut ui, 100, 30);
        let mut mouse = MouseState::new();

        let down = mouse_at(
            MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left),
            5,
            5,
        );
        let up = mouse_at(
            MouseEventKind::Up(ratatui::crossterm::event::MouseButton::Left),
            5,
            5,
        );

        // Without the resize, press and release is a click on a tree row.
        let mut fresh = MouseState::new();
        let _ = step(down.clone(), &hits, &mut fresh, &ui, &snap);
        let (clicked, _) = step(up.clone(), &hits, &mut fresh, &ui, &snap);
        assert!(!clicked.is_empty(), "the control case is not a click");

        // With one in between, the release lands on nothing. `Target::TreeRow`
        // is an index into a layout that no longer exists, so keeping the press
        // would click whatever row 5 has become.
        let _ = step(down, &hits, &mut mouse, &ui, &snap);
        let (_, dirty) = step(Event::Resize(80, 24), &hits, &mut mouse, &ui, &snap);
        assert!(dirty, "a resize redraws");
        let (after, _) = step(up, &hits, &mut mouse, &ui, &snap);
        assert!(after.is_empty(), "{after:?}");
    }

    #[tokio::test]
    async fn a_view_intent_never_reaches_the_store() {
        let (_store, snap) = connected().await;
        let mut ui = UiState::new();
        let (_, hits) = render(&snap, &mut ui, 100, 30);
        let mut mouse = MouseState::new();

        // `j` scrolls, and scrolling stays out of the store: a wheel notch
        // through an async task arrives a round trip after the hand that
        // turned it.
        let (intents, dirty) = step(key(KeyCode::Char('j')), &hits, &mut mouse, &ui, &snap);
        assert!(dirty);
        assert!(
            intents.iter().all(|i| matches!(i, Intent::View(_))),
            "{intents:?}"
        );
    }

    #[tokio::test]
    async fn an_app_intent_is_the_only_kind_that_leaves() {
        let (_store, snap) = connected().await;
        let mut ui = UiState::new();
        let (_, hits) = render(&snap, &mut ui, 100, 30);
        let mut mouse = MouseState::new();

        let (intents, _) = step(key(KeyCode::Char('q')), &hits, &mut mouse, &ui, &snap);
        assert!(
            intents.iter().any(|i| matches!(i, Intent::App(_))),
            "{intents:?}"
        );
    }

    #[tokio::test]
    async fn an_unbound_key_asks_for_nothing() {
        let (_store, snap) = connected().await;
        let mut ui = UiState::new();
        let (_, hits) = render(&snap, &mut ui, 100, 30);
        let mut mouse = MouseState::new();

        let (intents, dirty) = step(key(KeyCode::Char('%')), &hits, &mut mouse, &ui, &snap);
        assert!(intents.is_empty());
        assert!(!dirty, "an unbound key must not cost a frame");
    }

    #[tokio::test]
    async fn the_grid_viewport_excludes_the_header_and_the_scrollbar() {
        let (_store, snap) = connected().await;
        let mut ui = UiState::new();
        render(&snap, &mut ui, 100, 30);

        // Against the pane's *inside*, which is what the earlier version
        // recorded. Comparing with the pane's outer rectangle passes either
        // way, because the border alone accounts for the difference — the
        // assertion has to be the exact rectangle, not "smaller than".
        let outer = chrome::layout(Rect::new(0, 0, 100, 30), &mut ui).grid;
        let inner = Rect::new(outer.x + 1, outer.y + 1, outer.width - 2, outer.height - 2);
        let viewport = ui.viewport(PaneId::Grid);
        assert_eq!(
            viewport,
            datagrid::body_area(inner),
            "the header row and the scrollbar column are not the viewport's"
        );
        // And that is strictly less than the pane inside, or `ScrollToEnd`
        // stops a row short of the last row of the relation.
        assert!(viewport.height < inner.height, "{viewport:?} vs {inner:?}");
        assert!(viewport.width < inner.width, "{viewport:?} vs {inner:?}");
    }

    #[tokio::test]
    async fn a_dialog_covers_the_toasts_rather_than_the_other_way_round() {
        let (_store, snap) = connected().await;
        let mut ui = UiState::new();
        ui.toasts.push(crate::ui::Toast {
            id: crate::hit::ToastId::new(1),
            text: "something happened".into(),
            severity: crate::ui::Severity::Error,
            created_at: Instant::now(),
        });
        ui.modal = Some(overlay::Modal::error("Failed", "could not connect"));
        let (_, hits) = render(&snap, &mut ui, 100, 30);

        // The dialog is what is waiting for an answer, so it is what has to be
        // on top.
        assert_eq!(hits.at(Position::new(1, 1)), Some(Target::Backdrop));
    }

    #[tokio::test]
    async fn a_terminal_below_the_minimum_says_so_and_draws_nothing_else() {
        let (_store, snap) = connected().await;
        let mut ui = UiState::new();
        let (text, hits) = render(&snap, &mut ui, 40, 10);
        assert!(text.join("").contains("sqlake needs"), "{text:?}");
        assert_eq!(hits.at(Position::new(20, 5)), None, "no half-drawn layout");
    }

    #[tokio::test]
    async fn the_focused_pane_is_the_one_marked() {
        let (_store, snap) = connected().await;
        let mut ui = UiState::new();
        let (explorer_focused, _) = render(&snap, &mut ui, 100, 30);
        ui.focus = PaneId::Grid;
        let (grid_focused, _) = render(&snap, &mut ui, 100, 30);
        assert_ne!(
            explorer_focused, grid_focused,
            "focus that is recorded and not drawn is not focus"
        );
    }

    #[tokio::test]
    async fn the_input_context_follows_the_selected_cell() {
        let (store, snap) = connected().await;
        let mut ui = UiState::new();
        let conn = snap.connections[0].id;
        let table = sqlake_core::node::TableRef::new(["public", "users"]);
        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        let mut rx = store.subscribe();
        until(&mut rx, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.data.ready().is_some())
        })
        .await;
        let snap = rx.borrow_and_update().clone();

        // What the input layer would have applied alongside the action: the
        // tab this test then selects a cell in.
        let _ = ui.apply(
            crate::intent::ViewCmd::OpenTab {
                conn,
                table: table.clone(),
            },
            &snap,
        );
        render(&snap, &mut ui, 100, 30);
        let _ = ui.apply(crate::intent::ViewCmd::SelectCell { row: 2, col: 3 }, &snap);
        // Sorting and resizing act on the selected column, so the context has
        // to carry it or every key press means column zero.
        assert_eq!(context(&ui, &snap).grid_column, Some(3));
    }

    #[tokio::test]
    async fn disconnecting_closes_the_tabs_it_had_open() {
        // The store drops the connection's previews on `Disconnect`, but
        // nothing tells this crate to let go of the tabs pointing at them —
        // left alone, a tab survives its own connection with no preview left
        // to show and no session left to fetch one with.
        let (store, snap) = connected().await;
        let conn = snap.connections[0].id;
        let table = sqlake_core::node::TableRef::new(["public", "users"]);
        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        let mut rx = store.subscribe();
        until(&mut rx, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.data.ready().is_some())
        })
        .await;
        let mut snap = rx.borrow_and_update().clone();

        let mut ui = UiState::new();
        let _ = ui.apply(crate::intent::ViewCmd::OpenTab { conn, table }, &snap);
        assert_eq!(ui.tabs.len(), 1);

        store.dispatch(Action::Disconnect(conn));
        until(&mut rx, |s| s.connections[0].status == ConnStatus::Closed).await;
        snap = rx.borrow_and_update().clone();

        ui.close_disconnected_tabs(&snap);
        assert!(ui.tabs.is_empty(), "a tab outlived its own connection");
    }

    /// Connects, previews `table`, and opens it in a tab with a frame drawn.
    ///
    /// Drawn, because the grid's viewport comes from the frame: without one
    /// the scroll clamp works with a page of zero and puts the offset past the
    /// last row, a position no rendered grid ever reaches — so the margin the
    /// fetch actually turns on would go untested.
    async fn opened(
        store: Store,
        table: sqlake_core::node::TableRef,
    ) -> (Store, Arc<Snapshot>, UiState, ConnId) {
        let (store, snap) = connected_to(store).await;
        let conn = snap.connections[0].id;
        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        let mut rx = store.subscribe();
        until(&mut rx, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.data.ready().is_some())
        })
        .await;
        let snap = rx.borrow_and_update().clone();

        let mut ui = UiState::new();
        let _ = ui.apply(crate::intent::ViewCmd::OpenTab { conn, table }, &snap);
        let _ = render(&snap, &mut ui, 120, 40);
        (store, snap, ui, conn)
    }

    /// Opens `public.big` (200,000 rows, paged) and hands back everything a
    /// paging test needs to look at.
    async fn paging() -> (
        Store,
        Arc<Snapshot>,
        UiState,
        ConnId,
        sqlake_core::node::TableRef,
    ) {
        let table = sqlake_core::node::TableRef::new(["public", "big"]);
        let (store, snap, ui, conn) = opened(store(), table.clone()).await;
        (store, snap, ui, conn, table)
    }

    fn to_the_end(ui: &mut UiState, snap: &Arc<Snapshot>) -> Option<Action> {
        ui.apply(crate::intent::ViewCmd::ScrollToEnd(PaneId::Grid), snap)
    }

    #[tokio::test]
    async fn scrolling_to_the_end_asks_for_another_page() {
        let (_store, snap, mut ui, conn, table) = paging().await;
        assert_eq!(
            to_the_end(&mut ui, &snap),
            Some(Action::LoadMore { conn, table }),
            "reaching the end of a paged relation did not fetch"
        );
    }

    #[tokio::test]
    async fn a_page_in_flight_is_not_asked_for_twice() {
        // The store drops a second `LoadMore` for the same preview, but leaning
        // on that would make this crate's correctness a fact about the store's
        // deduplication.
        let (_store, snap, mut ui, _, _) = paging().await;
        assert!(to_the_end(&mut ui, &snap).is_some());
        assert_eq!(
            to_the_end(&mut ui, &snap),
            None,
            "the same page was asked for twice"
        );
    }

    #[tokio::test]
    async fn a_page_that_landed_leaves_the_view_able_to_ask_again() {
        let (store, snap, mut ui, _, table) = paging().await;
        let conn = snap.connections[0].id;
        let before = snap
            .preview(conn, &table)
            .map(|p| p.loaded_rows)
            .expect("a preview");
        let action = to_the_end(&mut ui, &snap).expect("it asks");
        store.dispatch(action);

        let mut rx = store.subscribe();
        until(&mut rx, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.loaded_rows > before)
        })
        .await;
        let snap = rx.borrow_and_update().clone();

        assert!(
            to_the_end(&mut ui, &snap).is_some(),
            "a page landed and the view still would not ask for the next"
        );
    }

    #[tokio::test]
    async fn a_relation_that_arrived_whole_is_never_asked_for_more() {
        // `public.users` is 50 rows and the page size is 200, so the first page
        // is the relation: the store saw a short page and said so, and no
        // amount of scrolling to the bottom is a reason to go back to the
        // driver.
        let (_store, snap, mut ui, _) = opened(
            store(),
            sqlake_core::node::TableRef::new(["public", "users"]),
        )
        .await;
        assert_eq!(
            to_the_end(&mut ui, &snap),
            None,
            "a relation already read whole was asked for another page"
        );
    }

    #[tokio::test]
    async fn cancelling_a_page_does_not_end_paging_for_the_tab() {
        // A cancellation leaves `loaded_rows`, `last_error` and the ordering
        // exactly as they were — the same state the end of a relation leaves —
        // so a view reading that state could not tell them apart, and the tab
        // never paged again.
        // Latency, so the page is genuinely in flight when it is cancelled.
        // Against an instant driver it lands first and there is nothing to
        // cancel — and `until` would wait for a state that never comes.
        let store = store_of(Behaviour {
            latency: std::time::Duration::from_millis(100),
            ..Behaviour::instant()
        });
        let (store, snap, mut ui, conn) =
            opened(store, sqlake_core::node::TableRef::new(["public", "big"])).await;
        let table = sqlake_core::node::TableRef::new(["public", "big"]);

        let action = to_the_end(&mut ui, &snap).expect("it asks");
        store.dispatch(action);
        let mut rx = store.subscribe();
        // Read inside the predicate, not from a second borrow afterwards: the
        // store can publish the finished page between the wait returning and
        // the borrow, and indexing a list that has emptied panics.
        let seen = std::cell::Cell::new(None);
        until(&mut rx, |s| {
            seen.set(s.busy.first().map(|b| b.id));
            seen.get().is_some()
        })
        .await;
        let id = seen.get().expect("a busy row while the page was in flight");

        store.dispatch(Action::Cancel(id));
        until(&mut rx, |s| s.busy.is_empty()).await;
        let snap = rx.borrow_and_update().clone();
        assert!(
            snap.preview(conn, &table).is_some_and(|p| !p.exhausted),
            "a cancelled page was taken for the end of the relation"
        );

        assert!(
            to_the_end(&mut ui, &snap).is_some(),
            "cancelling one page stopped the tab paging for good"
        );
    }

    #[tokio::test]
    async fn a_second_identical_failure_can_still_be_retried() {
        // The retry leaves the same message behind, so a view keyed on what a
        // request left could not see that anything had happened — and a
        // database that blipped twice ended paging for the life of the tab,
        // even after it recovered.
        let store = store_of(Behaviour {
            failing_after: vec![(vec!["public".to_owned(), "big".to_owned()], 1)],
            ..Behaviour::instant()
        });
        let (store, snap, mut ui, conn) =
            opened(store, sqlake_core::node::TableRef::new(["public", "big"])).await;
        let table = sqlake_core::node::TableRef::new(["public", "big"]);

        let mut rx = store.subscribe();
        let mut snap = snap;
        for attempt in 1..=2 {
            let before = snap.preview(conn, &table).map_or(0, |p| p.attempts);
            let action = to_the_end(&mut ui, &snap)
                .unwrap_or_else(|| panic!("attempt {attempt} would not ask"));
            store.dispatch(action);
            // Waited for by the count rather than by the error: after the
            // first failure the error is already there, so a wait on it would
            // return before the second request had even been sent.
            until(&mut rx, |s| {
                s.preview(conn, &table)
                    .is_some_and(|p| p.attempts > before && p.last_error.is_some())
            })
            .await;
            snap = rx.borrow_and_update().clone();
        }
        assert!(
            to_the_end(&mut ui, &snap).is_some(),
            "two failures with the same message ended paging for good"
        );
    }

    #[tokio::test]
    async fn the_cell_cursor_pages_the_way_scrolling_does() {
        // `J` pulls the viewport along to the last loaded row without ever
        // producing a scroll command. Fetching only on the scroll arms leaves
        // the whole feature out of reach of the keyboard: the cursor stops at
        // the end of page one and nothing asks for page two.
        let (_store, snap, mut ui, conn, table) = paging().await;
        assert_eq!(
            ui.apply(
                crate::intent::ViewCmd::MoveCellSelection {
                    drow: 1000,
                    dcol: 0
                },
                &snap
            ),
            Some(Action::LoadMore { conn, table }),
            "the cursor reached the last loaded row and asked for nothing"
        );
    }

    #[tokio::test]
    async fn sorting_starts_the_asking_over() {
        // `SortPreview` restarts the relation at page one, so whatever the tab
        // remembers about having already asked describes a preview that no
        // longer exists — and paging stops for as long as the tab lives.
        let (store, snap, mut ui, conn, table) = paging().await;
        assert!(to_the_end(&mut ui, &snap).is_some());

        store.dispatch(Action::SortPreview {
            conn,
            table: table.clone(),
            column: 0,
        });
        let mut rx = store.subscribe();
        until(&mut rx, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.sort.is_some() && p.data.ready().is_some())
        })
        .await;
        let snap = rx.borrow_and_update().clone();

        assert!(
            to_the_end(&mut ui, &snap).is_some(),
            "paging stopped for the life of the tab because it had been sorted"
        );
    }

    #[tokio::test]
    async fn only_the_grid_pages() {
        let (_store, snap, mut ui, _, _) = paging().await;
        assert_eq!(
            ui.apply(crate::intent::ViewCmd::ScrollToEnd(PaneId::Explorer), &snap),
            None,
            "scrolling the tree fetched a page"
        );
    }

    #[tokio::test]
    async fn shift_and_an_arrow_extend_the_selection() {
        let (_store, snap, mut ui, _) = opened(
            store(),
            sqlake_core::node::TableRef::new(["public", "users"]),
        )
        .await;
        let id = ui.active_tab.expect("a tab");

        // The binding is `Context::Grid`, which is where focus has to be for a
        // key pressed over the grid to mean anything.
        let _ = ui.apply(crate::intent::ViewCmd::FocusPane(PaneId::Grid), &snap);
        let _ = ui.apply(crate::intent::ViewCmd::SelectCell { row: 2, col: 1 }, &snap);
        for _ in 0..2 {
            for intent in crate::input::on_key(
                ratatui::crossterm::event::KeyEvent::new(
                    KeyCode::Down,
                    ratatui::crossterm::event::KeyModifiers::SHIFT,
                ),
                &context(&ui, &snap),
            ) {
                if let Intent::View(cmd) = intent {
                    let _ = ui.apply(cmd, &snap);
                }
            }
        }

        let grid = ui.grid(id).expect("a grid");
        assert_eq!(grid.selection(None), (2, 1, 4, 1));
    }

    #[tokio::test]
    async fn an_arrow_without_shift_ends_the_selection() {
        let (_store, snap, mut ui, _) = opened(
            store(),
            sqlake_core::node::TableRef::new(["public", "users"]),
        )
        .await;
        let id = ui.active_tab.expect("a tab");

        let _ = ui.apply(crate::intent::ViewCmd::SelectCell { row: 2, col: 1 }, &snap);
        let _ = ui.apply(
            crate::intent::ViewCmd::ExtendCellSelection { drow: 2, dcol: 0 },
            &snap,
        );
        assert!(ui.grid(id).expect("a grid").selected_cells(None).is_some());

        let _ = ui.apply(
            crate::intent::ViewCmd::MoveCellSelection { drow: 1, dcol: 0 },
            &snap,
        );
        assert_eq!(
            ui.grid(id).expect("a grid").selected_cells(None),
            None,
            "moving the cursor left the old anchor behind"
        );
    }

    async fn copyable() -> (Arc<Snapshot>, UiState) {
        let (_store, snap, mut ui, _) = opened(
            store(),
            sqlake_core::node::TableRef::new(["public", "users"]),
        )
        .await;
        let _ = ui.apply(crate::intent::ViewCmd::FocusPane(PaneId::Grid), &snap);
        (snap, ui)
    }

    #[tokio::test]
    async fn copying_the_selection_sends_a_sequence_and_says_what_it_sent() {
        let (snap, mut ui) = copyable().await;
        let _ = ui.apply(crate::intent::ViewCmd::SelectCell { row: 0, col: 0 }, &snap);
        let _ = ui.apply(
            crate::intent::ViewCmd::ExtendCellSelection { drow: 2, dcol: 1 },
            &snap,
        );
        let _ = ui.apply(
            crate::intent::ViewCmd::Copy {
                format: crate::copy::Format::Csv,
                all: false,
            },
            &snap,
        );

        let sequence = ui.take_copy().expect("a sequence to write");
        assert!(sequence.starts_with("\u{1b}]52;c;"), "{sequence:?}");
        assert!(
            ui.toasts.iter().any(|t| t.text.contains("6 cells")),
            "{:?}",
            ui.toasts
        );
        assert!(
            ui.take_copy().is_none(),
            "the same sequence would be written on the next frame too"
        );
    }

    #[tokio::test]
    async fn what_is_reported_is_what_was_sent_and_not_that_it_arrived() {
        // OSC 52 has no reply, and a terminal with clipboard writes off
        // swallows it. "Copied" would be a claim nothing here can check.
        let (snap, mut ui) = copyable().await;
        let _ = ui.apply(
            crate::intent::ViewCmd::Copy {
                format: crate::copy::Format::Csv,
                all: false,
            },
            &snap,
        );
        let said = ui.toasts.last().expect("a toast").text.clone();
        assert!(said.contains("sent"), "{said}");
        assert!(!said.contains("copied"), "{said}");
    }

    #[tokio::test]
    async fn copying_everything_takes_the_whole_result_rather_than_the_selection() {
        let (snap, mut ui) = copyable().await;
        let _ = ui.apply(crate::intent::ViewCmd::SelectCell { row: 0, col: 0 }, &snap);
        let _ = ui.apply(
            crate::intent::ViewCmd::Copy {
                format: crate::copy::Format::Json,
                all: true,
            },
            &snap,
        );
        assert!(ui.take_copy().is_some());
        // `public.users` is 50 rows of 8 columns.
        assert!(
            ui.toasts.iter().any(|t| t.text.contains("400 cells")),
            "{:?}",
            ui.toasts
        );
    }

    #[tokio::test]
    async fn a_table_with_no_rows_copies_nothing_and_says_so() {
        // "Everything" of an empty result is row zero of no rows: the sequence
        // sent was one row of empty fields, under a message saying two cells
        // had gone to the clipboard.
        let (_store, snap, mut ui, _) = opened(
            store(),
            sqlake_core::node::TableRef::new(["public", "empty"]),
        )
        .await;
        let _ = ui.apply(crate::intent::ViewCmd::FocusPane(PaneId::Grid), &snap);
        let _ = ui.apply(
            crate::intent::ViewCmd::Copy {
                format: crate::copy::Format::Csv,
                all: true,
            },
            &snap,
        );

        assert!(ui.take_copy().is_none());
        assert_eq!(ui.toasts.last().expect("a toast").text, "nothing to copy");
    }

    // ── screens ────────────────────────────────────────────────────────────
    //
    // The whole frame, as a string, reviewed by eye once and then held still.
    // Unit tests say a rectangle is where it should be; only this says the
    // screen is one a person would want to look at.

    /// Draw, and return the frame as its characters *and* its styling.
    ///
    /// Text alone would leave all six of these unchanged if every highlight in
    /// the client broke at once: focus, the selected row, the cell cursor and
    /// the severity of a message are all colour and nothing else. The mask
    /// gives each distinct style a character and lists what they were, so a
    /// change to any of them shows up as a diff a person can read.
    fn screen(snapshot: &Snapshot, ui: &mut UiState, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut hits = HitMap::new();
        terminal
            .draw(|frame| draw(frame, ui, snapshot, &mut hits))
            .unwrap();
        let buffer = terminal.backend().buffer();

        let mut legend: Vec<ratatui::style::Style> = Vec::new();
        let mut text = String::new();
        let mut mask = String::new();
        for y in 0..h {
            for x in 0..w {
                let cell = &buffer[(x, y)];
                text.push_str(cell.symbol());
                let style = cell.style();
                let index = legend.iter().position(|s| *s == style).unwrap_or_else(|| {
                    legend.push(style);
                    legend.len() - 1
                });
                // Beyond the thirty-sixth distinct style the mask stops being
                // readable, and a screen with that many is worth noticing.
                mask.push(char::from_digit(u32::try_from(index).unwrap_or(35), 36).unwrap_or('?'));
            }
            text.push('\n');
            mask.push('\n');
        }

        let mut out = text;
        out.push_str("\n── styles ──\n");
        out.push_str(&mask);
        for (i, style) in legend.iter().enumerate() {
            let key = char::from_digit(u32::try_from(i).unwrap_or(35), 36).unwrap_or('?');
            out.push_str(&format!("{key} = {}\n", describe(*style)));
        }
        out
    }

    /// Only the parts of a `Style` this client sets, so the snapshot does not
    /// churn on a field nothing touches.
    fn describe(style: ratatui::style::Style) -> String {
        let mut parts = Vec::new();
        if let Some(fg) = style.fg {
            parts.push(format!("fg={fg:?}"));
        }
        if let Some(bg) = style.bg {
            parts.push(format!("bg={bg:?}"));
        }
        if !style.add_modifier.is_empty() {
            parts.push(format!("{:?}", style.add_modifier));
        }
        if parts.is_empty() {
            "default".to_owned()
        } else {
            parts.join(" ")
        }
    }

    #[tokio::test]
    async fn screen_before_anything_is_open() {
        let (_store, snap) = connected().await;
        let mut ui = UiState::new();
        insta::assert_snapshot!(screen(&snap, &mut ui, 100, 30));
    }

    #[tokio::test]
    async fn screen_with_a_relation_open() {
        let (store, _) = connected().await;
        let mut rx = store.subscribe();
        let conn = rx.borrow_and_update().connections[0].id;
        let table = TableRef::new(["public", "users"]);
        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        until(&mut rx, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.data.ready().is_some())
        })
        .await;
        let snap = rx.borrow_and_update().clone();

        let mut ui = UiState::new();
        ui.focus = PaneId::Grid;
        // What the input layer would have applied alongside the action.
        let _ = ui.apply(
            crate::intent::ViewCmd::OpenTab {
                conn,
                table: table.clone(),
            },
            &snap,
        );
        // Drawn first, the way the loop does it: an intent applied before any
        // frame exists is measured against a viewport of zero.
        let _ = render(&snap, &mut ui, 100, 30);
        let _ = ui.apply(crate::intent::ViewCmd::SelectCell { row: 2, col: 1 }, &snap);
        insta::assert_snapshot!(screen(&snap, &mut ui, 100, 30));
    }

    fn an_editor() -> Editor {
        Editor::new(
            "vi".into(),
            Vec::new(),
            std::path::PathBuf::from("/scratch"),
        )
    }

    #[tokio::test]
    async fn a_saved_buffer_reaches_the_tab_it_was_edited_for() {
        let (_store, snap) = connected().await;
        let conn = snap.connections[0].id;
        let mut ui = UiState::new();
        let _ = ui.apply(crate::intent::ViewCmd::OpenSqlTab { conn }, &snap);
        let tab = ui.active_tab.expect("a tab");

        apply_edit(
            &mut ui,
            &an_editor(),
            tab,
            Edited::Changed("select 1".to_owned()),
        );
        assert_eq!(ui.buffer_of(tab), Some("select 1"));
        assert!(ui.toasts.is_empty() && ui.modal.is_none());
    }

    #[tokio::test]
    async fn closing_the_editor_without_saving_says_nothing() {
        // It is how somebody says no, and a message about it is a message
        // about a decision already made.
        let (_store, snap) = connected().await;
        let conn = snap.connections[0].id;
        let mut ui = UiState::new();
        let _ = ui.apply(crate::intent::ViewCmd::OpenSqlTab { conn }, &snap);
        let tab = ui.active_tab.expect("a tab");

        apply_edit(&mut ui, &an_editor(), tab, Edited::Unchanged);
        assert_eq!(ui.buffer_of(tab), Some(""));
        assert!(ui.toasts.is_empty() && ui.modal.is_none());
    }

    #[tokio::test]
    async fn an_editor_that_forked_is_a_notice_and_a_broken_one_is_a_dialog() {
        // The first left the buffer as it was and only wants a setting
        // changed; the second did nothing at all, and `e` appearing to do
        // nothing is what the dialog is for.
        let (_store, snap) = connected().await;
        let conn = snap.connections[0].id;
        let mut ui = UiState::new();
        let _ = ui.apply(crate::intent::ViewCmd::OpenSqlTab { conn }, &snap);
        let tab = ui.active_tab.expect("a tab");

        apply_edit(&mut ui, &an_editor(), tab, Edited::Returned);
        assert_eq!(ui.toasts.len(), 1);
        assert!(ui.toasts[0].text.contains("editor_args"), "{:?}", ui.toasts);
        assert!(ui.modal.is_none());

        apply_edit(
            &mut ui,
            &an_editor(),
            tab,
            Edited::Failed("no such file".to_owned()),
        );
        assert!(
            ui.modal
                .as_ref()
                .is_some_and(|m| m.body.contains("no such file")),
            "{:?}",
            ui.modal
        );
    }

    #[tokio::test]
    async fn screen_with_a_query_refused_on_its_second_line() {
        // The whole of T7 on screen: the server said where, the driver turned
        // that into a line, and nothing between here and it knows which server
        // it was.
        let (store, _) = connected_to(store_of(Behaviour {
            failing_sql: vec!["sql_that_is_wrong".to_owned()],
            ..Behaviour::instant()
        }))
        .await;
        let mut rx = store.subscribe();
        let conn = rx.borrow_and_update().connections[0].id;

        let mut ui = UiState::new();
        let snap = rx.borrow_and_update().clone();
        let _ = ui.apply(crate::intent::ViewCmd::OpenSqlTab { conn }, &snap);
        let tab = ui.active_tab.expect("a tab");
        let sql = "select\n  sql_that_is_wrong\nfrom public.users";
        ui.set_buffer(tab, sql.to_owned());

        let query = sqlake_core::id::QueryId::new();
        ui.set_query(tab, query);
        store.dispatch(Action::RunQuery {
            conn,
            query,
            sql: sql.to_owned(),
            max_rows: None,
            max_bytes: None,
        });
        until(&mut rx, |s| {
            s.query(query).is_some_and(|q| q.data.error().is_some())
        })
        .await;
        let snap = rx.borrow_and_update().clone();
        assert_eq!(
            snap.query(query)
                .and_then(|q| q.failed_at)
                .map(|at| at.line),
            Some(2)
        );

        insta::assert_snapshot!(screen(&snap, &mut ui, 100, 20));
    }

    #[tokio::test]
    async fn screen_with_a_definition_open() {
        // The whole of T4 on screen: a section list, one section in the grid
        // the preview uses, and a summary line saying what the relation is.
        let (store, _) = connected().await;
        let mut rx = store.subscribe();
        let conn = rx.borrow_and_update().connections[0].id;
        let table = TableRef::new(["public", "users"]);

        let mut ui = UiState::new();
        let snap = rx.borrow_and_update().clone();
        let _ = ui.apply(
            crate::intent::ViewCmd::OpenDefinition {
                conn,
                table: table.clone(),
            },
            &snap,
        );
        store.dispatch(Action::DescribeTable {
            conn,
            table: table.clone(),
            refresh: false,
        });
        until(&mut rx, |s| {
            s.definition(conn, &table)
                .is_some_and(|d| d.data.ready().is_some())
        })
        .await;
        let snap = rx.borrow_and_update().clone();

        insta::assert_snapshot!(screen(&snap, &mut ui, 100, 20));
    }

    #[tokio::test]
    async fn a_definition_whose_connection_closed_stops_saying_it_is_loading() {
        // Disconnecting drops the store's definitions, so the tab is left
        // holding nothing — which is not the same as waiting for something.
        let (store, _) = connected().await;
        let mut rx = store.subscribe();
        let conn = rx.borrow_and_update().connections[0].id;
        let table = TableRef::new(["public", "users"]);

        let mut ui = UiState::new();
        let snap = rx.borrow_and_update().clone();
        let _ = ui.apply(
            crate::intent::ViewCmd::OpenDefinition {
                conn,
                table: table.clone(),
            },
            &snap,
        );
        store.dispatch(Action::DescribeTable {
            conn,
            table: table.clone(),
            refresh: false,
        });
        until(&mut rx, |s| {
            s.definition(conn, &table)
                .is_some_and(|d| d.data.ready().is_some())
        })
        .await;

        store.dispatch(Action::Disconnect(conn));
        until(&mut rx, |s| s.definitions.is_empty()).await;
        let snap = rx.borrow_and_update().clone();

        let screen = screen(&snap, &mut ui, 100, 20);
        assert!(!screen.contains("describing"), "{screen}");
        assert!(screen.contains("the connection is closed"), "{screen}");
    }

    #[tokio::test]
    async fn a_definition_shows_the_section_that_was_picked() {
        // A driver that has indexes, so the list has more than one line: the
        // mock's default capability set claims none, and a section list of one
        // cannot show that picking moves.
        let (store, _) = connected_to(store_of_with(
            Behaviour::instant(),
            sqlake_core::capability::Capabilities {
                indexes: true,
                ..sqlake_driver_mock::CAPABILITIES
            },
        ))
        .await;
        let mut rx = store.subscribe();
        let conn = rx.borrow_and_update().connections[0].id;
        let table = TableRef::new(["public", "users"]);

        let mut ui = UiState::new();
        let snap = rx.borrow_and_update().clone();
        let _ = ui.apply(
            crate::intent::ViewCmd::OpenDefinition {
                conn,
                table: table.clone(),
            },
            &snap,
        );
        store.dispatch(Action::DescribeTable {
            conn,
            table: table.clone(),
            refresh: false,
        });
        until(&mut rx, |s| {
            s.definition(conn, &table)
                .is_some_and(|d| d.data.ready().is_some())
        })
        .await;
        let snap = rx.borrow_and_update().clone();
        let tab = ui.active_tab.expect("a tab");

        // Columns lead, because that is what somebody opened the pane for.
        assert_eq!(ui.section_of(tab), Some(0));
        let rows_now = |ui: &UiState| {
            let definition = ui.definition(&snap).expect("it arrived");
            definition
                .rows(ui.section_of(tab).expect("a definition tab"))
                .map(|r| r.row_count())
        };
        let columns = rows_now(&ui);

        let _ = ui.apply(
            crate::intent::ViewCmd::SelectSection(crate::intent::SectionPick::By(1)),
            &snap,
        );
        assert_eq!(ui.section_of(tab), Some(1));
        assert_ne!(
            rows_now(&ui),
            columns,
            "a different section, a different grid"
        );

        // Clamped rather than wrapped: a list you can see all of does not jump
        // back to the top when you step off the end.
        for _ in 0..10 {
            let _ = ui.apply(
                crate::intent::ViewCmd::SelectSection(crate::intent::SectionPick::By(1)),
                &snap,
            );
        }
        let last = ui.definition(&snap).expect("it arrived").titles().len() - 1;
        assert_eq!(ui.section_of(tab), Some(last));
    }

    #[tokio::test]
    async fn screen_with_a_query_result_in_a_sql_tab() {
        // What T4 is for: rows in the same grid a preview uses, under a tab
        // that is a query rather than a relation.
        let (store, _) = connected().await;
        let mut rx = store.subscribe();
        let conn = rx.borrow_and_update().connections[0].id;

        let mut ui = UiState::new();
        let snap = rx.borrow_and_update().clone();
        let _ = ui.apply(crate::intent::ViewCmd::OpenSqlTab { conn }, &snap);
        let tab = ui.active_tab.expect("a tab");
        ui.set_buffer(tab, "select * from public.users".to_owned());

        let query = sqlake_core::id::QueryId::new();
        ui.set_query(tab, query);
        store.dispatch(Action::RunQuery {
            conn,
            query,
            sql: "select * from public.users".to_owned(),
            max_rows: None,
            max_bytes: None,
        });
        until(&mut rx, |s| {
            s.query(query).is_some_and(|q| q.data.ready().is_some())
        })
        .await;
        let snap = rx.borrow_and_update().clone();

        insta::assert_snapshot!(screen(&snap, &mut ui, 100, 20));
    }

    #[tokio::test]
    async fn screen_with_a_sql_tab_beside_a_preview() {
        // Both kinds of tab on one bar, and the pane showing a buffer instead
        // of a grid — which is the whole of what T1 changes on screen.
        let (store, _) = connected().await;
        let mut rx = store.subscribe();
        let conn = rx.borrow_and_update().connections[0].id;
        let table = TableRef::new(["public", "users"]);
        store.dispatch(Action::PreviewTable {
            conn,
            table: table.clone(),
        });
        until(&mut rx, |s| {
            s.preview(conn, &table)
                .is_some_and(|p| p.data.ready().is_some())
        })
        .await;
        let snap = rx.borrow_and_update().clone();

        let mut ui = UiState::new();
        let _ = ui.apply(crate::intent::ViewCmd::OpenTab { conn, table }, &snap);
        let _ = ui.apply(crate::intent::ViewCmd::OpenSqlTab { conn }, &snap);
        insta::assert_snapshot!(screen(&snap, &mut ui, 100, 20));
    }

    #[tokio::test]
    async fn screen_with_the_tree_expanded() {
        let (store, _) = connected().await;
        let mut rx = store.subscribe();
        let conn = rx.borrow_and_update().connections[0].id;
        store.dispatch(Action::ToggleNode {
            conn,
            node: NodeRef::new(sqlake_core::node::NodeKind::Namespace, ["public"]),
        });
        until(&mut rx, |s| s.tree(conn).count() > 3).await;
        let snap = rx.borrow_and_update().clone();

        let mut ui = UiState::new();
        let _ = render(&snap, &mut ui, 100, 30);
        let _ = ui.apply(crate::intent::ViewCmd::SelectTreeRow(1), &snap);
        insta::assert_snapshot!(screen(&snap, &mut ui, 100, 30));
    }

    #[tokio::test]
    async fn screen_reporting_a_failure() {
        let (_store, snap) = connected().await;
        let mut ui = UiState::new();
        ui.modal = Some(overlay::Modal::error(
            "mock could not be opened",
            "could not connect: refused by the configured behaviour",
        ));
        insta::assert_snapshot!(screen(&snap, &mut ui, 100, 30));
    }

    #[tokio::test]
    async fn screen_at_the_smallest_usable_size() {
        let (_store, snap) = connected().await;
        let mut ui = UiState::new();
        insta::assert_snapshot!(screen(&snap, &mut ui, 60, 20));
    }

    #[tokio::test]
    async fn screen_below_the_smallest_usable_size() {
        let (_store, snap) = connected().await;
        let mut ui = UiState::new();
        insta::assert_snapshot!(screen(&snap, &mut ui, 40, 10));
    }

    #[tokio::test]
    async fn a_failed_connection_shows_on_its_row_and_says_why_in_a_dialog() {
        // The row is the state and stays; the dialog is the reason, which does
        // not fit in a pane twenty-six columns wide.
        let store = Store::spawn(
            Drivers::new().with(Arc::new(MockDriver::new(Behaviour {
                connect_fails: true,
                ..Behaviour::instant()
            }))),
            Arc::new(MockProfiles::default()),
            PageRequest::DEFAULT_LIMIT,
            None,
        );
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect {
            profile: mock_summary("mock").id,
            conn: ConnId::new(),
        });
        until(&mut rx, |s| {
            s.connections
                .first()
                .is_some_and(|c| matches!(c.status, ConnStatus::Failed(_)))
        })
        .await;
        let snap = rx.borrow_and_update().clone();

        // Through `initial_ui`, which is the path the first frame takes: a
        // connect dispatched before the terminal was taken over can already
        // have failed, and a failed connect publishes nothing afterwards.
        let mut ui = initial_ui(&snap);
        assert!(ui.modal.is_some(), "the reason has nowhere else to fit");

        // The row says which connection is broken and is cut long before the
        // reason is readable, which is why the dialog is still here: it is
        // where the message fits.
        let (rows, _) = render(&snap, &mut ui, 100, 30);
        let screen = rows.join("\n");
        assert!(screen.contains("! mock"), "{screen}");
        // A fragment, because the dialog wraps its body: the sentence is on
        // screen but not on one line of it.
        assert!(
            screen.contains("refused"),
            "the dialog is where the reason fits: {screen}"
        );

        // Dismissed once, not raised again by the next unrelated snapshot.
        ui.modal = None;
        raise_connection_failure(&snap, &mut ui);
        assert!(ui.modal.is_none(), "the dialog came back on its own");
    }
}

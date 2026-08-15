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
use sqlake_app::snapshot::{ConnStatus, Snapshot, TabContent};
use sqlake_app::store::Store;
use sqlake_app::tree::TreeView;
use tokio::sync::watch;

use crate::chrome;
use crate::datagrid;
use crate::hit::{HitMap, PaneId, Target};
use crate::input::{self, InputContext};
use crate::intent::Intent;
use crate::mouse::MouseState;
use crate::overlay;
use crate::terminal::Tui;
use crate::tree;
use crate::ui::UiState;

/// Run until the store says to quit or the terminal closes.
///
/// # Errors
///
/// Propagates terminal write failures. The caller still holds the
/// `TerminalGuard`, so the screen is restored either way.
pub async fn run(terminal: &mut Tui, store: &Store, mouse_enabled: bool) -> io::Result<()> {
    let mut mouse = MouseState::new();
    let mut events = EventStream::new();
    let mut snapshots = store.subscribe();
    let mut ui = initial_ui(&snapshots.borrow_and_update().clone());
    let mut snapshot = snapshots.borrow_and_update().clone();
    // The first snapshot as well as every later one: connecting is dispatched
    // before the loop starts, so a connection that failed while the terminal
    // was being taken over is already in this one — and a failure produces no
    // further snapshot to be caught by the arm below.
    raise_connection_failure(&snapshot, &mut ui);
    let mut hits = HitMap::new();
    let mut dirty = true;

    loop {
        if dirty {
            ui.retain_tabs(&snapshot);
            // Cleared rather than replaced: one entry per visible cell adds up
            // to hundreds, and growing a fresh `Vec` for them every frame is a
            // cost with nothing to show for it.
            hits.clear();
            terminal.draw(|frame| draw(frame, &mut ui, &snapshot, &mut hits))?;
            dirty = false;
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
                    ui.apply(cmd, &snapshot);
                    dirty = true;
                }
                Intent::App(action) => store.dispatch(action),
            }
        }
    }
}

/// A connection dispatched before the terminal was taken over can already have
/// failed by the time the loop starts, and a failed connect publishes nothing
/// afterwards — so waiting for the next snapshot would mean waiting for one
/// that never comes.
fn initial_ui(snapshot: &Snapshot) -> UiState {
    let mut ui = UiState::new();
    raise_connection_failure(snapshot, &mut ui);
    ui
}

/// A toast is right for something that went wrong beside work that is still
/// going; a connection that never opened leaves nothing to do at all, and a
/// message that fades on its own leaves the user with an empty explorer and no
/// account of why. It is the same reason a failed node reports on the node.
///
/// Shown once per failure: the dialog is dismissed, not re-raised by the next
/// unrelated snapshot.
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
    ui.modal = Some(overlay::Modal::error(
        format!("{name} could not be opened"),
        why,
    ));
}

/// What the explorer says when it has nothing to show.
///
/// "Nothing connected" is only true when there is something to connect to. A
/// fresh install has no config file at all, and a pane that says nothing
/// leaves the user pressing keys at a client that cannot do anything yet.
///
/// The first connection is the one whose tree this pane draws, so its status is
/// what the message has to be about: a handshake can take the whole of the
/// driver's deadline, and `c` during one opens a *second* connection to the
/// same profile rather than hurrying the first along.
fn waiting_for(snapshot: &Snapshot) -> &'static str {
    match snapshot.connections.first().map(|c| &c.status) {
        Some(ConnStatus::Connecting) => " connecting… ",
        _ if snapshot.profiles.is_empty() => " no connections configured — write connections.toml ",
        _ => " nothing connected — press c ",
    }
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
    let ctx = context(ui, snapshot);
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => input::on_key(key, &ctx),
        Event::Mouse(event) if mouse_enabled => mouse
            .feed(event, hits, Instant::now())
            .into_iter()
            .flat_map(|(target, gesture)| input::on_mouse(target, gesture, &ctx))
            .collect(),
        // A resize redraws by falling out of the loop; the layout is computed
        // from the frame each time and has nothing to invalidate.
        _ => Vec::new(),
    }
}

fn context<'a>(ui: &UiState, snapshot: &'a Snapshot) -> InputContext<'a> {
    InputContext {
        snapshot,
        focus: ui.focus,
        modal_open: ui.modal.is_some(),
        connection: snapshot.connections.first().map(|c| c.id),
        tree_selection: ui.tree.selected,
        grid_column: snapshot
            .active_tab
            .and_then(|tab| ui.grid(tab))
            .map(|g| g.col),
    }
}

fn draw(frame: &mut Frame<'_>, ui: &mut UiState, snapshot: &Snapshot, hits: &mut HitMap) {
    let area = frame.area();
    if chrome::too_small(area) {
        chrome::render_too_small(frame, area);
        return;
    }

    let frames = chrome::layout(area, ui);
    chrome::tab_bar(frame, hits, frames.tab_bar, snapshot);

    let explorer = chrome::pane(
        frame,
        hits,
        frames.explorer,
        PaneId::Explorer,
        "Explorer",
        ui.focus == PaneId::Explorer,
    );
    ui.set_viewport(PaneId::Explorer, explorer);
    // An empty view is drawn rather than skipped: with no connection there is
    // no tree at all, and a blank pane says nothing about why.
    let empty = TreeView::default();
    let view = snapshot
        .connections
        .first()
        .and_then(|c| snapshot.tree(c.id))
        .unwrap_or(&empty);
    tree::render(frame, hits, explorer, view, &ui.tree, waiting_for(snapshot));

    chrome::splitter(
        frame,
        hits,
        frames.splitter,
        matches!(ui.hover, Some(Target::Splitter(_))),
    );

    let title = snapshot
        .active()
        .map_or_else(|| "Preview".to_owned(), |t| t.title.clone());
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
    if let Some(tab) = snapshot.active() {
        let id = tab.id;
        let TabContent::Preview(preview) = &tab.content;
        datagrid::render(frame, hits, grid, preview, ui.grid_mut(id));
    }

    chrome::status_bar(frame, hits, frames.status_bar, snapshot);

    // Toasts first so a dialog covers them: a message drawn over the thing
    // waiting for an answer hides the answer.
    overlay::toasts(frame, hits, body_of(frames), snapshot);
    if let Some(dialog) = ui.modal.clone() {
        overlay::modal(frame, hits, area, &dialog);
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
    use sqlake_app::snapshot::{ConnectionView, TabView};
    use sqlake_app::store::Drivers;
    use sqlake_core::id::ConnId;
    use sqlake_core::node::{NodeRef, TableRef};
    use sqlake_driver_mock::{Behaviour, MockDriver, MockProfiles, mock_summary};

    use super::*;

    fn store() -> Store {
        Store::spawn(
            Drivers::new().with(Arc::new(MockDriver::new(Behaviour::instant()))),
            Arc::new(MockProfiles::default()),
        )
    }

    async fn connected() -> (Store, Arc<Snapshot>) {
        let store = store();
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(mock_summary("mock").id));
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
                kind: summary.kind,
                status: ConnStatus::Connecting,
                capabilities: None,
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
        let (_store, mut snap) = connected().await;
        let mut owned = (*snap).clone();
        owned.toasts.push(sqlake_app::snapshot::Toast {
            id: sqlake_app::action::ToastId::new(1),
            text: "something happened".into(),
            severity: sqlake_app::snapshot::Severity::Error,
            created_at: Instant::now(),
        });
        snap = Arc::new(owned);

        let mut ui = UiState::new();
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
        store.dispatch(Action::PreviewTable {
            conn: snap.connections[0].id,
            table: sqlake_core::node::TableRef::new(["public", "users"]),
        });
        let mut rx = store.subscribe();
        until(&mut rx, |s| s.active().is_some()).await;
        let snap = rx.borrow_and_update().clone();

        render(&snap, &mut ui, 100, 30);
        ui.apply(crate::intent::ViewCmd::SelectCell { row: 2, col: 3 }, &snap);
        // Sorting and resizing act on the selected column, so the context has
        // to carry it or every key press means column zero.
        assert_eq!(context(&ui, &snap).grid_column, Some(3));
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
        store.dispatch(Action::PreviewTable {
            conn,
            table: TableRef::new(["public", "users"]),
        });
        until(&mut rx, |s| {
            s.active()
                .and_then(TabView::preview)
                .is_some_and(|p| p.data.ready().is_some())
        })
        .await;
        let snap = rx.borrow_and_update().clone();

        let mut ui = UiState::new();
        ui.focus = PaneId::Grid;
        // Drawn first, the way the loop does it: an intent applied before any
        // frame exists is measured against a viewport of zero.
        let _ = render(&snap, &mut ui, 100, 30);
        ui.apply(crate::intent::ViewCmd::SelectCell { row: 2, col: 1 }, &snap);
        insta::assert_snapshot!(screen(&snap, &mut ui, 100, 30));
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
        until(&mut rx, |s| s.tree(conn).is_some_and(|t| t.len() > 3)).await;
        let snap = rx.borrow_and_update().clone();

        let mut ui = UiState::new();
        let _ = render(&snap, &mut ui, 100, 30);
        ui.apply(crate::intent::ViewCmd::SelectTreeRow(1), &snap);
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
    async fn a_failed_connection_is_raised_as_a_dialog() {
        let store = Store::spawn(
            Drivers::new().with(Arc::new(MockDriver::new(Behaviour {
                connect_fails: true,
                ..Behaviour::instant()
            }))),
            Arc::new(MockProfiles::default()),
        );
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(mock_summary("mock").id));
        until(&mut rx, |s| {
            s.connections
                .first()
                .is_some_and(|c| matches!(c.status, ConnStatus::Failed(_)))
        })
        .await;
        let snap = rx.borrow_and_update().clone();

        // Through `initial_ui`, which is the path the first frame takes. A
        // connect dispatched before the terminal was taken over can already
        // have failed, and a failed connect publishes nothing afterwards — so
        // raising it only on the next snapshot means never.
        let mut ui = initial_ui(&snap);
        // A toast fades; an explorer that will never fill needs the reason to
        // stay on screen until it is read.
        assert!(ui.modal.is_some());

        // Dismissed once, not raised again by the next unrelated snapshot.
        ui.modal = None;
        raise_connection_failure(&snap, &mut ui);
        assert!(ui.modal.is_none(), "the dialog came back on its own");

        // A second connection failing is a second thing to report. Remembering
        // only one id leaves it silent, because the first failure stays in the
        // list and is what the search keeps finding.
        store.dispatch(Action::Connect(mock_summary("mock").id));
        until(&mut rx, |s| {
            s.connections.len() == 2
                && s.connections
                    .iter()
                    .all(|c| matches!(c.status, ConnStatus::Failed(_)))
        })
        .await;
        let snap = rx.borrow_and_update().clone();
        raise_connection_failure(&snap, &mut ui);
        assert!(ui.modal.is_some(), "the second failure went unreported");
    }
}

//! The render loop: one frame, then wait for something to change.
//!
//! The loop draws and then blocks on two sources — terminal events and new
//! snapshots — so an idle client costs nothing and a slow query never blocks a
//! keystroke. Everything expensive happened in the store task before the
//! snapshot arrived.
//!
//! The [`HitMap`] is rebuilt from scratch on every frame and consulted for the
//! events that follow it, so a click is always answered against the layout that
//! drew the pixels it was aimed at.

use std::io;
use std::sync::Arc;
use std::time::Instant;

use futures::StreamExt as _;
use ratatui::Frame;
use ratatui::crossterm::event::{Event, EventStream, KeyEventKind};
use ratatui::layout::Rect;
use sqlake_app::snapshot::{Snapshot, TabContent};
use sqlake_app::store::Store;
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
    let mut ui = UiState::new();
    let mut mouse = MouseState::new();
    let mut events = EventStream::new();
    let mut snapshots = store.subscribe();
    let mut snapshot = snapshots.borrow_and_update().clone();

    loop {
        ui.retain_tabs(&snapshot);
        let mut hits = HitMap::new();
        terminal.draw(|frame| draw(frame, &mut ui, &snapshot, &mut hits))?;

        if snapshot.should_quit {
            return Ok(());
        }

        let intents = tokio::select! {
            event = events.next() => match event {
                // The terminal is gone; there is nothing left to draw on.
                None | Some(Err(_)) => return Ok(()),
                Some(Ok(event)) => {
                    from_event(event, &hits, &mut mouse, &ui, &snapshot, mouse_enabled)
                }
            },
            changed = snapshots.changed() => {
                if changed.is_err() {
                    return Ok(()); // The store stopped.
                }
                snapshot = snapshots.borrow_and_update().clone();
                Vec::new()
            }
        };

        for intent in intents {
            match intent {
                // Applied here, on this thread, before the next frame. A wheel
                // notch that went through the store would arrive a round trip
                // later than the hand that turned it.
                Intent::View(cmd) => ui.apply(cmd, &snapshot),
                Intent::App(action) => store.dispatch(action),
            }
        }
    }
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
    let view = snapshot
        .connections
        .first()
        .and_then(|c| snapshot.tree(c.id));
    if let Some(view) = view {
        tree::render(frame, hits, explorer, view, &ui.tree);
    }

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
    use ratatui::layout::Position;
    use sqlake_app::action::Action;
    use sqlake_app::snapshot::ConnectionView;
    use sqlake_app::store::Drivers;
    use sqlake_core::capability::DriverKind;
    use sqlake_driver_mock::{Behaviour, MockDriver};

    use super::*;

    fn store() -> Store {
        Store::spawn(Drivers::new().with(Arc::new(MockDriver::new(Behaviour::instant()))))
    }

    async fn connected() -> (Store, Arc<Snapshot>) {
        let store = store();
        let mut rx = store.subscribe();
        store.dispatch(Action::Connect(DriverKind::Mock));
        until(&mut rx, |s| {
            s.connections.first().is_some_and(ConnectionView::is_ready)
        })
        .await;
        let snap = rx.borrow_and_update().clone();
        (store, snap)
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

    #[tokio::test]
    async fn the_grid_viewport_excludes_the_header_and_the_scrollbar() {
        let (_store, snap) = connected().await;
        let mut ui = UiState::new();
        render(&snap, &mut ui, 100, 30);

        let pane_inner = chrome::layout(Rect::new(0, 0, 100, 30), &mut ui).grid;
        let viewport = ui.viewport(PaneId::Grid);
        // Measured against the pane, `ScrollToEnd` stops a row short and the
        // last row of a relation can never be reached.
        assert!(viewport.height < pane_inner.height, "{viewport:?}");
        assert!(viewport.width < pane_inner.width, "{viewport:?}");
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
}

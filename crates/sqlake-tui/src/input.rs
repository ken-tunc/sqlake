//! Turning gestures and keystrokes into intents.
//!
//! Both halves are pure functions of an event and a small context, which is
//! what lets the test at the bottom of this file assert that every capability
//! reachable with the mouse also has a key binding.
//!
//! The key map is data rather than code. That makes it enumerable — for the
//! coverage test now, and for a help modal and user-defined bindings later.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use sqlake_app::action::Action;
use sqlake_app::snapshot::{ConnStatus, Snapshot};
use sqlake_app::tree::VisibleNode;
use sqlake_core::id::{ConnId, ProfileId, TabId};
use sqlake_core::node::TableRef;

use crate::hit::{ButtonId, PaneId, ScrollPart, SplitId, Target};
use crate::intent::{Context, Intent, IntentKind, ViewCmd};
use crate::mouse::Gesture;
use crate::ui::{OpenTab, Toast};

/// Rows moved by one wheel notch. Three is the common terminal convention.
const WHEEL_LINES: i32 = 3;

/// Rows moved by a page key or a click on the scrollbar track.
const PAGE_LINES: i32 = 20;

// ── key map ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyCombo {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyCombo {
    #[must_use]
    pub const fn new(code: KeyCode) -> Self {
        Self {
            code,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[must_use]
    pub const fn ctrl(code: KeyCode) -> Self {
        Self {
            code,
            modifiers: KeyModifiers::CONTROL,
        }
    }

    /// Shift is ignored on purpose: terminals report it inconsistently for
    /// characters that are already shifted, so `G` is matched by its character
    /// rather than by a modifier.
    fn matches(self, event: KeyEvent) -> bool {
        const RELEVANT: KeyModifiers = KeyModifiers::CONTROL.union(KeyModifiers::ALT);
        self.code == event.code && (self.modifiers & RELEVANT) == (event.modifiers & RELEVANT)
    }
}

#[derive(Debug)]
pub struct KeyBinding {
    pub keys: &'static [KeyCombo],
    pub context: Context,
    pub kind: IntentKind,
}

const fn key(c: char) -> KeyCombo {
    KeyCombo::new(KeyCode::Char(c))
}

const fn ctrl(c: char) -> KeyCombo {
    KeyCombo::ctrl(KeyCode::Char(c))
}

pub const KEYMAP: &[KeyBinding] = &[
    KeyBinding {
        keys: &[
            KeyCombo::ctrl(KeyCode::Char('h')),
            KeyCombo::new(KeyCode::BackTab),
        ],
        context: Context::Global,
        kind: IntentKind::Focus,
    },
    KeyBinding {
        keys: &[KeyCombo::new(KeyCode::Tab)],
        context: Context::Global,
        kind: IntentKind::Focus,
    },
    // `/` opens the box, and inside it every key is the box's. `Filter` is the
    // only kind bound in two contexts, and it has to be: a keymap that let the
    // global bindings through would make a search for a table called `q` quit.
    KeyBinding {
        keys: &[key('/')],
        context: Context::Global,
        kind: IntentKind::Filter,
    },
    KeyBinding {
        keys: &[
            KeyCombo::new(KeyCode::Backspace),
            KeyCombo::new(KeyCode::Esc),
            KeyCombo::new(KeyCode::Enter),
        ],
        context: Context::Filter,
        kind: IntentKind::Filter,
    },
    KeyBinding {
        keys: &[
            key('j'),
            key('k'),
            KeyCombo::new(KeyCode::Down),
            KeyCombo::new(KeyCode::Up),
            KeyCombo::new(KeyCode::PageDown),
            KeyCombo::new(KeyCode::PageUp),
        ],
        context: Context::Global,
        kind: IntentKind::Scroll,
    },
    KeyBinding {
        keys: &[
            key('g'),
            key('G'),
            KeyCombo::new(KeyCode::Home),
            KeyCombo::new(KeyCode::End),
        ],
        context: Context::Global,
        kind: IntentKind::ScrollEdge,
    },
    KeyBinding {
        keys: &[KeyCombo::new(KeyCode::Left), KeyCombo::new(KeyCode::Right)],
        context: Context::Grid,
        kind: IntentKind::ScrollHorizontally,
    },
    KeyBinding {
        keys: &[KeyCombo::new(KeyCode::Down), KeyCombo::new(KeyCode::Up)],
        context: Context::Explorer,
        kind: IntentKind::TreeSelection,
    },
    // Upper case moves the selection, lower case and the arrows move the
    // view: `H`/`L` are to `J`/`K` what `Left`/`Right` are to `j`/`k`. Without
    // the horizontal pair the mouse can select any cell and the keyboard
    // cannot, which the coverage sweep does not catch because both are the
    // same capability.
    KeyBinding {
        keys: &[key('J'), key('K')],
        context: Context::Grid,
        kind: IntentKind::GridSelection,
    },
    KeyBinding {
        keys: &[key('H'), key('L')],
        context: Context::Grid,
        kind: IntentKind::GridSelection,
    },
    KeyBinding {
        keys: &[key('<'), key('>')],
        context: Context::Grid,
        kind: IntentKind::ResizeColumn,
    },
    KeyBinding {
        keys: &[
            KeyCombo::ctrl(KeyCode::Left),
            KeyCombo::ctrl(KeyCode::Right),
        ],
        context: Context::Global,
        kind: IntentKind::MoveSplit,
    },
    KeyBinding {
        keys: &[key('=')],
        context: Context::Global,
        kind: IntentKind::EvenSplit,
    },
    KeyBinding {
        keys: &[KeyCombo::new(KeyCode::Esc)],
        context: Context::Modal,
        kind: IntentKind::DismissModal,
    },
    KeyBinding {
        keys: &[key('c')],
        context: Context::Global,
        kind: IntentKind::Connect,
    },
    KeyBinding {
        keys: &[key('D')],
        context: Context::Global,
        kind: IntentKind::Disconnect,
    },
    // `Space` toggles; the arrows are directional, so `Right` opens and `Left`
    // only ever closes. Leaving `Left` unbound made the tree the one place
    // where an arrow key did nothing at all.
    KeyBinding {
        keys: &[
            KeyCombo::new(KeyCode::Char(' ')),
            KeyCombo::new(KeyCode::Right),
            KeyCombo::new(KeyCode::Left),
        ],
        context: Context::Explorer,
        kind: IntentKind::ToggleNode,
    },
    KeyBinding {
        keys: &[KeyCombo::new(KeyCode::Enter)],
        context: Context::Explorer,
        kind: IntentKind::PreviewTable,
    },
    KeyBinding {
        keys: &[key('s')],
        context: Context::Grid,
        kind: IntentKind::SortPreview,
    },
    KeyBinding {
        keys: &[key('m')],
        context: Context::Grid,
        kind: IntentKind::LoadMore,
    },
    KeyBinding {
        keys: &[KeyCombo::ctrl(KeyCode::Tab), key(']'), key('[')],
        context: Context::Global,
        kind: IntentKind::SelectTab,
    },
    KeyBinding {
        keys: &[ctrl('w')],
        context: Context::Global,
        kind: IntentKind::CloseTab,
    },
    KeyBinding {
        keys: &[ctrl('g')],
        context: Context::Global,
        kind: IntentKind::Cancel,
    },
    KeyBinding {
        keys: &[KeyCombo::new(KeyCode::Esc)],
        context: Context::Global,
        kind: IntentKind::DismissToast,
    },
    KeyBinding {
        keys: &[key('q'), ctrl('c')],
        context: Context::Global,
        kind: IntentKind::Quit,
    },
];

// ── context ────────────────────────────────────────────────────────────────

/// What the input layer needs to know to turn an event into an intent.
///
/// Assembled by the render loop from `UiState` and the current snapshot.
#[derive(Debug, Clone, Copy)]
pub struct InputContext<'a> {
    pub snapshot: &'a Snapshot,
    pub focus: PaneId,
    pub modal_open: bool,
    /// The connection the selected row belongs to, for the operations that
    /// are about a connection rather than about a node.
    pub connection: Option<ConnId>,
    pub tree_selection: Option<usize>,
    /// The column of the selected grid cell. The keyboard equivalents of
    /// clicking a header and dragging a column edge act on it, which is the
    /// only thing that makes them equivalent: without it every key press would
    /// sort and resize column zero whatever the user had selected.
    pub grid_column: Option<usize>,
    /// Which relation a tab points at is what turns a click on a header into
    /// a `SortPreview` for the right table.
    pub tabs: &'a [OpenTab],
    pub active_tab: Option<TabId>,
    pub toasts: &'a [Toast],
    /// What the explorer's filter box holds, or `None` when it is closed.
    /// Its presence is what redirects the keyboard into it.
    pub filter: Option<&'a str>,
}

impl InputContext<'_> {
    /// Which profile `c` connects to, until T7 puts a picker in front of it.
    ///
    /// The first one nothing is connected to, so that with several profiles it
    /// works through them rather than reopening the first — and the first
    /// profile again once they all have a connection, because a second window
    /// onto the same database is a real thing to want, and a key that goes
    /// dead once is worse than one that repeats itself.
    ///
    /// A connection the user closed, or one that failed, is not a connection:
    /// counting its row would make `c` skip past the profile the user is
    /// trying to reopen and connect to something else instead.
    fn connectable_profile(&self) -> Option<ProfileId> {
        let profiles = &self.snapshot.profiles;
        let live = |id: &ProfileId| {
            self.snapshot.connections.iter().any(|c| {
                &c.profile == id && matches!(c.status, ConnStatus::Connecting | ConnStatus::Ready)
            })
        };
        profiles
            .iter()
            .find(|p| !live(&p.id))
            .or_else(|| profiles.first())
            .map(|p| p.id.clone())
    }

    /// The node a *visible* row points at.
    ///
    /// Through the filter, not straight into the tree: a row number means a
    /// position on screen, and with rows hidden the two stop agreeing. Acting
    /// on the wrong one is how a click opens a table the user cannot see.
    fn node(&self, index: usize) -> Option<&VisibleNode> {
        let row = *crate::tree::visible(&self.snapshot.explorer.nodes, self.filter).get(index)?;
        self.snapshot.explorer.get(row)
    }

    fn active_tab(&self) -> Option<TabId> {
        self.active_tab
    }

    /// The relation the active tab points at, if any.
    fn active_preview(&self) -> Option<(ConnId, TableRef)> {
        let id = self.active_tab?;
        self.tabs
            .iter()
            .find(|t| t.id == id)
            .map(|t| (t.conn, t.table.clone()))
    }

    /// The same relation, but only where its driver will order a preview.
    ///
    /// The header keeps its hit target and the click is simply not read.
    /// Dropping the target would let the click fall through to the pane
    /// beneath and focus the grid, which reads as a header that was never a
    /// target; greying it out would say the data is unavailable rather than
    /// the ordering.
    fn sortable_preview(&self) -> Option<(ConnId, TableRef)> {
        let (conn, table) = self.active_preview()?;
        self.snapshot
            .connection(conn)?
            .capabilities?
            .sortable_preview
            .then_some((conn, table))
    }

    /// The context a keystroke is read in. A modal takes the keyboard over
    /// entirely, which is why `Esc` can mean two different things without
    /// being ambiguous.
    fn key_context(&self) -> Context {
        if self.modal_open {
            Context::Modal
        } else if self.filter.is_some() {
            Context::Filter
        } else {
            match self.focus {
                PaneId::Explorer => Context::Explorer,
                PaneId::Grid => Context::Grid,
                PaneId::TabBar | PaneId::StatusBar => Context::Global,
            }
        }
    }
}

// ── mouse ──────────────────────────────────────────────────────────────────

/// What a gesture on a target means.
#[must_use]
pub fn on_mouse(target: Target, gesture: Gesture, ctx: &InputContext<'_>) -> Vec<Intent> {
    match (target, gesture) {
        (Target::Pane(pane), Gesture::Click) => vec![ViewCmd::FocusPane(pane).into()],
        // The wheel over the part of a pane its content does not fill. Without
        // this, a five-row result in a forty-row grid ignores the wheel
        // everywhere below the last row.
        (Target::Pane(pane), Gesture::Scroll(delta)) => vec![scroll(pane, delta)],
        (Target::Pane(PaneId::Grid), Gesture::ScrollX(delta)) => vec![
            ViewCmd::ScrollXBy {
                delta: i32::from(delta),
            }
            .into(),
        ],

        (Target::TreeRow { index }, Gesture::Click) => vec![
            ViewCmd::FocusPane(PaneId::Explorer).into(),
            ViewCmd::SelectTreeRow(index).into(),
        ],
        (Target::TreeRow { index }, Gesture::DoubleClick) => activate_node(index, ctx),
        (Target::TreeToggle { index }, Gesture::Click) => toggle_node(index, ctx, false),
        (Target::TreeRow { .. } | Target::TreeToggle { .. }, Gesture::Scroll(delta)) => {
            vec![scroll(PaneId::Explorer, delta)]
        }

        (Target::GridCell { row, col }, Gesture::Click) => vec![
            ViewCmd::FocusPane(PaneId::Grid).into(),
            ViewCmd::SelectCell { row, col }.into(),
        ],
        (Target::GridHeader { col }, Gesture::Click) => ctx
            .sortable_preview()
            .map(|(conn, table)| {
                vec![
                    Action::SortPreview {
                        conn,
                        table,
                        column: col,
                    }
                    .into(),
                ]
            })
            .unwrap_or_default(),
        (Target::GridColEdge { col }, Gesture::DragBy { dx, .. }) => {
            vec![ViewCmd::ResizeColumn { col, delta: dx }.into()]
        }
        (
            Target::GridCell { .. } | Target::GridHeader { .. } | Target::GridColEdge { .. },
            Gesture::Scroll(delta),
        ) => vec![scroll(PaneId::Grid, delta)],
        (Target::GridCell { .. } | Target::GridHeader { .. }, Gesture::ScrollX(delta)) => vec![
            ViewCmd::ScrollXBy {
                delta: i32::from(delta),
            }
            .into(),
        ],

        // Dragging the thumb scrolls by the distance dragged; clicking the
        // track pages towards the click.
        (
            Target::Scrollbar {
                pane,
                part: ScrollPart::Thumb,
            },
            Gesture::DragBy { dy, .. },
        ) => {
            vec![
                ViewCmd::ScrollBy {
                    pane,
                    delta: i32::from(dy),
                }
                .into(),
            ]
        }
        (
            Target::Scrollbar {
                pane,
                part: ScrollPart::TrackBefore,
            },
            Gesture::Click,
        ) => {
            vec![
                ViewCmd::ScrollBy {
                    pane,
                    delta: -PAGE_LINES,
                }
                .into(),
            ]
        }
        (
            Target::Scrollbar {
                pane,
                part: ScrollPart::TrackAfter,
            },
            Gesture::Click,
        ) => {
            vec![
                ViewCmd::ScrollBy {
                    pane,
                    delta: PAGE_LINES,
                }
                .into(),
            ]
        }
        (Target::Scrollbar { pane, .. }, Gesture::Scroll(delta)) => vec![scroll(pane, delta)],

        (Target::Splitter(split), Gesture::DragBy { dx, .. }) => {
            vec![ViewCmd::MoveSplit { split, delta: dx }.into()]
        }
        (Target::Splitter(split), Gesture::DoubleClick) => vec![ViewCmd::EvenSplit(split).into()],

        (Target::Tab(id), Gesture::Click) => vec![ViewCmd::SelectTab(id).into()],
        (Target::TabClose(id), Gesture::Click)
        // Middle-click is the second way to close a tab, and the one that does
        // not require hitting a one-cell `×`.
        | (Target::Tab(id), Gesture::MiddleClick) => close_tab(id, ctx),

        (Target::Button(ButtonId::Cancel(busy)), Gesture::Click) => {
            vec![Action::Cancel(busy).into()]
        }
        // A pointer cannot type, so the box's only gesture is to be dismissed.
        (Target::Button(ButtonId::Filter), Gesture::Click) => {
            vec![ViewCmd::SetFilter(None).into()]
        }
        (Target::Button(ButtonId::DismissModal), Gesture::Click) => {
            vec![ViewCmd::DismissModal.into()]
        }
        (Target::Toast(id), Gesture::Click) => vec![ViewCmd::DismissToast(id).into()],
        (Target::Backdrop, Gesture::Click) => vec![ViewCmd::DismissModal.into()],

        // Presses, releases and hover carry no action of their own; they exist
        // so the view can show feedback.
        _ => Vec::new(),
    }
}

fn scroll(pane: PaneId, delta: i8) -> Intent {
    ViewCmd::ScrollBy {
        pane,
        delta: i32::from(delta) * WHEEL_LINES,
    }
    .into()
}

/// Opening a node: relations open a preview, branches expand.
fn activate_node(index: usize, ctx: &InputContext<'_>) -> Vec<Intent> {
    let Some(node) = ctx.node(index) else {
        return Vec::new();
    };
    // The row's own connection, not the first one. With several open, the
    // difference is between opening the table under the cursor and opening a
    // table of the same name somewhere else.
    let conn = node.conn;
    match node.node_ref.as_table() {
        Some(table) => open_or_focus_tab(conn, table),
        None => vec![
            Action::ToggleNode {
                conn,
                node: node.node_ref.clone(),
            }
            .into(),
        ],
    }
}

/// Both, unconditionally: `PreviewTable` is what makes a tab raised from
/// `Failed` retry, and the store treats it as a no-op when the relation is
/// already loaded — so there is nothing to decide between here.
fn open_or_focus_tab(conn: ConnId, table: TableRef) -> Vec<Intent> {
    vec![
        ViewCmd::OpenTab {
            conn,
            table: table.clone(),
        }
        .into(),
        Action::PreviewTable { conn, table }.into(),
    ]
}

/// Closing a tab. If it was the last one open on this relation, the store's
/// own cache of it — and any page still in flight for it — goes too.
///
/// `still_open` is always false today, because [`OpenTab`] never mints a
/// second tab on one relation. Checked rather than assumed: the day something
/// does — split panes, say — this is what stops one tab's close from pulling
/// the other's data out from under it.
fn close_tab(id: TabId, ctx: &InputContext<'_>) -> Vec<Intent> {
    let Some(closing) = ctx.tabs.iter().find(|t| t.id == id) else {
        return Vec::new();
    };
    let (conn, table) = (closing.conn, closing.table.clone());
    let mut intents = vec![ViewCmd::CloseTab(id).into()];
    let still_open = ctx
        .tabs
        .iter()
        .any(|t| t.id != id && t.conn == conn && t.table == table);
    if !still_open {
        intents.push(Action::ForgetPreview { conn, table }.into());
    }
    intents
}

/// `collapse_only` is set for `Left`, which must never open a subtree: a key
/// that closes one and also opens one is not a direction, it is a toggle with
/// a misleading name.
fn toggle_node(index: usize, ctx: &InputContext<'_>, collapse_only: bool) -> Vec<Intent> {
    let Some(node) = ctx.node(index) else {
        return Vec::new();
    };
    let conn = node.conn;
    if !node.state.is_toggleable() {
        return Vec::new();
    }
    if collapse_only && !node.state.is_expanded() {
        return Vec::new();
    }
    vec![
        Action::ToggleNode {
            conn,
            node: node.node_ref.clone(),
        }
        .into(),
    ]
}

// ── keyboard ───────────────────────────────────────────────────────────────

/// What a keystroke means, given where focus is.
#[must_use]
pub fn on_key(event: KeyEvent, ctx: &InputContext<'_>) -> Vec<Intent> {
    // Windows terminals report releases as well as presses.
    if event.kind != KeyEventKind::Press {
        return Vec::new();
    }

    let context = ctx.key_context();
    let matching = |wanted: Context| {
        KEYMAP
            .iter()
            .find(|b| b.context == wanted && b.keys.iter().any(|k| k.matches(event)))
    };

    // A pane binding beats the global one for the same key, so `Esc` in a modal
    // dismisses the modal rather than a toast behind it. A modal is the one
    // context with no fallback: it has the keyboard, so `q` behind a "discard
    // these changes?" dialog must not quit instead of answering it.
    let bound = matching(context).or_else(|| match context {
        // Neither has a fallback: both hold the keyboard, so `q` behind a
        // "discard these changes?" dialog must not quit instead of answering
        // it, and a `q` typed into the search box must reach the box.
        Context::Modal | Context::Filter => None,
        _ => matching(Context::Global),
    });

    // Every remaining key belongs to the box, which is what a text input is.
    // Enumerating the printable characters in `KEYMAP` instead would be a
    // hundred bindings that the coverage test would then have to skip.
    if bound.is_none() && context == Context::Filter {
        return materialise(IntentKind::Filter, event, ctx);
    }

    let Some(binding) = bound else {
        return Vec::new();
    };
    materialise(binding.kind, event, ctx)
}

/// Build the concrete intent for a bound capability.
///
/// The key map says *what*; this decides the parameters from the current
/// state, which is why the same key can scroll whichever pane has focus.
fn materialise(kind: IntentKind, event: KeyEvent, ctx: &InputContext<'_>) -> Vec<Intent> {
    let pane = ctx.focus;
    let backwards = matches!(
        event.code,
        KeyCode::Up | KeyCode::PageUp | KeyCode::Left | KeyCode::Home | KeyCode::BackTab
    ) || matches!(
        event.code,
        KeyCode::Char('h' | 'H' | 'k' | 'K' | '<' | 'g' | '[')
    );

    match kind {
        IntentKind::Focus => vec![if backwards {
            ViewCmd::FocusPrevPane.into()
        } else {
            ViewCmd::FocusNextPane.into()
        }],
        IntentKind::Scroll => {
            let magnitude = if matches!(event.code, KeyCode::PageUp | KeyCode::PageDown) {
                PAGE_LINES
            } else {
                1
            };
            vec![
                ViewCmd::ScrollBy {
                    pane,
                    delta: if backwards { -magnitude } else { magnitude },
                }
                .into(),
            ]
        }
        IntentKind::ScrollEdge => vec![if backwards {
            ViewCmd::ScrollToStart(pane).into()
        } else {
            ViewCmd::ScrollToEnd(pane).into()
        }],
        IntentKind::ScrollHorizontally => vec![
            ViewCmd::ScrollXBy {
                delta: if backwards { -1 } else { 1 },
            }
            .into(),
        ],
        IntentKind::TreeSelection => {
            vec![ViewCmd::MoveTreeSelection(if backwards { -1 } else { 1 }).into()]
        }
        IntentKind::GridSelection => {
            let step = if backwards { -1 } else { 1 };
            let sideways = matches!(event.code, KeyCode::Char('H' | 'L'));
            vec![
                ViewCmd::MoveCellSelection {
                    drow: if sideways { 0 } else { step },
                    dcol: if sideways { step } else { 0 },
                }
                .into(),
            ]
        }
        IntentKind::ResizeColumn => vec![
            ViewCmd::ResizeColumn {
                col: ctx.grid_column.unwrap_or(0),
                delta: if backwards { -1 } else { 1 },
            }
            .into(),
        ],
        IntentKind::MoveSplit => vec![
            ViewCmd::MoveSplit {
                split: SplitId::Explorer,
                delta: if backwards { -1 } else { 1 },
            }
            .into(),
        ],
        IntentKind::EvenSplit => vec![ViewCmd::EvenSplit(SplitId::Explorer).into()],
        IntentKind::DismissModal => vec![ViewCmd::DismissModal.into()],
        IntentKind::Filter => vec![ViewCmd::SetFilter(next_filter(event, ctx)).into()],

        IntentKind::Connect => ctx
            .connectable_profile()
            .map(|id| vec![Action::Connect(id).into()])
            .unwrap_or_default(),
        IntentKind::Disconnect => ctx
            .connection
            .map(|c| vec![Action::Disconnect(c).into()])
            .unwrap_or_default(),
        IntentKind::ToggleNode => ctx
            .tree_selection
            .map(|i| toggle_node(i, ctx, matches!(event.code, KeyCode::Left)))
            .unwrap_or_default(),
        IntentKind::PreviewTable => ctx
            .tree_selection
            .map(|i| activate_node(i, ctx))
            .unwrap_or_default(),
        IntentKind::SortPreview => ctx
            .sortable_preview()
            .map(|(conn, table)| {
                vec![
                    Action::SortPreview {
                        conn,
                        table,
                        column: ctx.grid_column.unwrap_or(0),
                    }
                    .into(),
                ]
            })
            .unwrap_or_default(),
        IntentKind::LoadMore => ctx
            .active_preview()
            .map(|(conn, table)| vec![Action::LoadMore { conn, table }.into()])
            .unwrap_or_default(),
        IntentKind::SelectTab => neighbouring_tab(ctx, backwards)
            .map(|tab| vec![ViewCmd::SelectTab(tab).into()])
            .unwrap_or_default(),
        IntentKind::CloseTab => ctx
            .active_tab()
            .map(|tab| close_tab(tab, ctx))
            .unwrap_or_default(),
        IntentKind::Cancel => ctx
            .snapshot
            .busy
            .first()
            .map(|b| vec![Action::Cancel(b.id).into()])
            .unwrap_or_default(),
        IntentKind::DismissToast => ctx
            .toasts
            .first()
            .map(|t| vec![ViewCmd::DismissToast(t.id).into()])
            .unwrap_or_default(),
        IntentKind::Quit => vec![Action::Quit.into()],
    }
}

/// What the filter box holds after this key.
///
/// `Esc` closes it and `Enter` leaves it closed with the tree whole again:
/// both are ways of saying "done", and a filter that outlived its box would
/// hide rows with nothing on screen to explain why. `Backspace` on an empty
/// box closes it too, which is where the user's fingers already are.
fn next_filter(event: KeyEvent, ctx: &InputContext<'_>) -> Option<String> {
    let Some(current) = ctx.filter else {
        // Not open yet, so this is the `/` that opens it — and `/` is a
        // character the box would otherwise have taken as its first letter.
        return Some(String::new());
    };
    match event.code {
        KeyCode::Esc | KeyCode::Enter => None,
        KeyCode::Backspace => {
            let mut text = current.to_owned();
            text.pop().map(|_| text)
        }
        KeyCode::Char(c) => Some(format!("{current}{c}")),
        _ => Some(current.to_owned()),
    }
}

fn neighbouring_tab(ctx: &InputContext<'_>, backwards: bool) -> Option<TabId> {
    let tabs = ctx.tabs;
    if tabs.is_empty() {
        return None;
    }
    let Some(current) = ctx
        .active_tab()
        .and_then(|id| tabs.iter().position(|t| t.id == id))
    else {
        // Nothing is active — the last tab was just closed. Stepping from an
        // assumed index 0 would skip the first tab entirely.
        return tabs.first().map(|t| t.id);
    };
    let next = if backwards {
        (current + tabs.len() - 1) % tabs.len()
    } else {
        (current + 1) % tabs.len()
    };
    tabs.get(next).map(|t| t.id)
}

#[cfg(test)]
mod tests {
    use sqlake_driver_mock::{CAPABILITIES, NO_SORT, mock_summary};
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use sqlake_app::action::BusyId;
    use sqlake_app::snapshot::{
        BusyItem, BusyOwner, ConnStatus, ConnectionView, LoadState, PreviewView,
    };
    use sqlake_app::tree::{NodeState, TreeView, VisibleNode};
    use sqlake_core::capability::{Capabilities, DriverKind};
    use sqlake_core::node::{NodeKind, NodeRef, RelationKind, TableRef};

    use super::*;
    use crate::hit::ToastId;
    use crate::ui::Severity;

    // ── fixtures ───────────────────────────────────────────────────────────

    /// A snapshot, and the tabs and toasts a screen showing it might have.
    struct Fixture {
        snapshot: Snapshot,
        conn: ConnId,
        tabs: Vec<OpenTab>,
        toasts: Vec<Toast>,
    }

    impl Fixture {
        /// What every connection in the fixture claims it can do.
        fn advertising(mut self, capabilities: Capabilities) -> Self {
            for conn in &mut self.snapshot.connections {
                conn.capabilities = Some(capabilities);
            }
            self
        }

        fn ctx(&self, focus: PaneId) -> InputContext<'_> {
            InputContext {
                active_tab: Some(self.tabs[0].id),
                ..self.ctx_no_active_tab(focus)
            }
        }

        /// The active tab just closed, so nothing is active — the state
        /// `switching_tabs_with_nothing_active_lands_on_the_first_one` exists
        /// to cover.
        fn ctx_no_active_tab(&self, focus: PaneId) -> InputContext<'_> {
            InputContext {
                snapshot: &self.snapshot,
                focus,
                modal_open: false,
                connection: self.snapshot.connections.first().map(|c| c.id),
                tree_selection: Some(0),
                grid_column: Some(2),
                tabs: &self.tabs,
                active_tab: None,
                toasts: &self.toasts,
                filter: None,
            }
        }
    }

    fn fixture() -> Fixture {
        let conn = ConnId::new();
        let explorer = Arc::new(TreeView {
            nodes: vec![
                VisibleNode {
                    conn,
                    depth: 0,
                    label: "public".into(),
                    node_ref: NodeRef::new(NodeKind::Namespace, ["public"]),
                    relation_kind: None,
                    // Expanded, because the row below it is its child. A
                    // collapsed node listing a child is a state the store
                    // cannot produce.
                    state: NodeState::Expanded,
                },
                VisibleNode {
                    conn,
                    depth: 1,
                    label: "users".into(),
                    node_ref: NodeRef::new(NodeKind::Relation, ["public", "users"]),
                    relation_kind: Some(RelationKind::Table),
                    state: NodeState::Leaf,
                },
            ],
        });

        let tabs = vec![
            OpenTab {
                id: TabId::new(1),
                conn,
                table: TableRef::new(["public", "users"]),
            },
            OpenTab {
                id: TabId::new(2),
                conn,
                table: TableRef::new(["public", "empty"]),
            },
        ];

        let snapshot = Snapshot {
            rev: 1,
            profiles: Arc::new(vec![mock_summary("mock")]),
            connections: vec![ConnectionView {
                id: conn,
                profile: mock_summary("mock").id,
                name: "mock".into(),
                color: None,
                kind: DriverKind::Mock,
                status: ConnStatus::Ready,
                capabilities: Some(CAPABILITIES),
            }],
            explorer,
            previews: tabs
                .iter()
                .map(|t| PreviewView {
                    conn: t.conn,
                    table: t.table.clone(),
                    sort: None,
                    loaded_rows: 0,
                    data: LoadState::Idle,
                    last_error: None,
                })
                .collect(),
            busy: vec![BusyItem {
                id: BusyId::new(1),
                owner: BusyOwner::Preview {
                    conn,
                    table: TableRef::new(["public", "users"]),
                },
                label: "loading".into(),
                started_at: std::time::Instant::now(),
            }],
            should_quit: false,
        };

        let toasts = vec![Toast {
            id: ToastId::new(1),
            text: "oops".into(),
            severity: Severity::Error,
            created_at: std::time::Instant::now(),
        }];

        Fixture {
            snapshot,
            conn,
            tabs,
            toasts,
        }
    }

    #[test]
    fn a_row_acts_on_its_own_connection() {
        // With two connections in the explorer, the row under the cursor is
        // the only thing that says which database is meant — and both have a
        // `public.users`, so picking the first connection instead would open
        // the wrong table and look right doing it.
        let second = ConnId::new();
        let mut f = fixture();
        let rows = Arc::get_mut(&mut f.snapshot.explorer).expect("sole owner");
        rows.nodes.push(VisibleNode {
            conn: second,
            depth: 1,
            label: "users".into(),
            node_ref: NodeRef::new(NodeKind::Relation, ["public", "users"]),
            relation_kind: Some(RelationKind::Table),
            state: NodeState::Leaf,
        });
        let last = rows.nodes.len() - 1;

        let mut context = f.ctx(PaneId::Explorer);
        // The first connection stays selected in the context, which is what
        // the previous version of this code would have used.
        context.tree_selection = Some(last);

        let out = on_key(press(KeyCode::Enter), &context);
        assert_eq!(
            out,
            [
                Intent::View(ViewCmd::OpenTab {
                    conn: second,
                    table: TableRef::new(["public", "users"]),
                }),
                Intent::App(Action::PreviewTable {
                    conn: second,
                    table: TableRef::new(["public", "users"]),
                }),
            ]
        );
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press_ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    // ── mouse ──────────────────────────────────────────────────────────────

    #[test]
    fn clicking_a_tree_row_focuses_the_explorer_and_selects_it() {
        let f = fixture();
        let out = on_mouse(
            Target::TreeRow { index: 1 },
            Gesture::Click,
            &f.ctx(PaneId::Grid),
        );
        assert_eq!(
            out,
            [
                Intent::View(ViewCmd::FocusPane(PaneId::Explorer)),
                Intent::View(ViewCmd::SelectTreeRow(1)),
            ]
        );
    }

    #[test]
    fn double_clicking_a_relation_opens_it_and_a_branch_expands() {
        let f = fixture();
        let c = f.ctx(PaneId::Explorer);

        let out = on_mouse(Target::TreeRow { index: 1 }, Gesture::DoubleClick, &c);
        assert_eq!(
            out,
            [
                Intent::View(ViewCmd::OpenTab {
                    conn: f.conn,
                    table: TableRef::new(["public", "users"]),
                }),
                Intent::App(Action::PreviewTable {
                    conn: f.conn,
                    table: TableRef::new(["public", "users"]),
                }),
            ]
        );

        let out = on_mouse(Target::TreeRow { index: 0 }, Gesture::DoubleClick, &c);
        assert!(matches!(out[0], Intent::App(Action::ToggleNode { .. })));
    }

    #[test]
    fn the_toggle_glyph_never_expands_a_leaf() {
        let f = fixture();
        let c = f.ctx(PaneId::Explorer);
        assert!(on_mouse(Target::TreeToggle { index: 1 }, Gesture::Click, &c).is_empty());
        assert!(!on_mouse(Target::TreeToggle { index: 0 }, Gesture::Click, &c).is_empty());
    }

    #[test]
    fn a_click_on_a_row_that_no_longer_exists_does_nothing() {
        // The hit map is one frame old, so an index can outlive its row.
        let f = fixture();
        let c = f.ctx(PaneId::Explorer);
        assert!(on_mouse(Target::TreeRow { index: 99 }, Gesture::DoubleClick, &c).is_empty());
        assert!(on_mouse(Target::TreeToggle { index: 99 }, Gesture::Click, &c).is_empty());
    }

    #[test]
    fn the_wheel_scrolls_the_pane_it_is_over_not_the_focused_one() {
        let f = fixture();
        let c = f.ctx(PaneId::Grid);
        assert_eq!(
            on_mouse(Target::TreeRow { index: 0 }, Gesture::Scroll(-1), &c),
            [Intent::View(ViewCmd::ScrollBy {
                pane: PaneId::Explorer,
                delta: -WHEEL_LINES
            })]
        );
    }

    #[test]
    fn dragging_a_column_edge_resizes_that_column() {
        let f = fixture();
        let out = on_mouse(
            Target::GridColEdge { col: 4 },
            Gesture::DragBy { dx: -3, dy: 0 },
            &f.ctx(PaneId::Grid),
        );
        assert_eq!(
            out,
            [Intent::View(ViewCmd::ResizeColumn { col: 4, delta: -3 })]
        );
    }

    #[test]
    fn clicking_the_track_pages_towards_the_click() {
        let f = fixture();
        let c = f.ctx(PaneId::Grid);
        let before = on_mouse(
            Target::Scrollbar {
                pane: PaneId::Grid,
                part: ScrollPart::TrackBefore,
            },
            Gesture::Click,
            &c,
        );
        let after = on_mouse(
            Target::Scrollbar {
                pane: PaneId::Grid,
                part: ScrollPart::TrackAfter,
            },
            Gesture::Click,
            &c,
        );
        assert_eq!(
            before,
            [Intent::View(ViewCmd::ScrollBy {
                pane: PaneId::Grid,
                delta: -PAGE_LINES
            })]
        );
        assert_eq!(
            after,
            [Intent::View(ViewCmd::ScrollBy {
                pane: PaneId::Grid,
                delta: PAGE_LINES
            })]
        );
    }

    #[test]
    fn the_backdrop_dismisses_the_modal_rather_than_reaching_behind_it() {
        let f = fixture();
        assert_eq!(
            on_mouse(Target::Backdrop, Gesture::Click, &f.ctx(PaneId::Grid)),
            [Intent::View(ViewCmd::DismissModal)]
        );
    }

    #[test]
    fn presses_and_hover_carry_no_action() {
        let f = fixture();
        let c = f.ctx(PaneId::Explorer);
        for gesture in [
            Gesture::Down,
            Gesture::Up,
            Gesture::HoverEnter,
            Gesture::HoverLeave,
        ] {
            assert!(
                on_mouse(Target::TreeRow { index: 0 }, gesture, &c).is_empty(),
                "{gesture:?}"
            );
        }
    }

    // ── keyboard ───────────────────────────────────────────────────────────

    #[test]
    fn scrolling_applies_to_the_focused_pane() {
        let f = fixture();
        assert_eq!(
            on_key(press(KeyCode::Char('j')), &f.ctx(PaneId::Grid)),
            [Intent::View(ViewCmd::ScrollBy {
                pane: PaneId::Grid,
                delta: 1
            })]
        );
        assert_eq!(
            on_key(press(KeyCode::Char('k')), &f.ctx(PaneId::StatusBar)),
            [Intent::View(ViewCmd::ScrollBy {
                pane: PaneId::StatusBar,
                delta: -1
            })]
        );
    }

    #[test]
    fn a_pane_binding_beats_the_global_one_for_the_same_key() {
        // Down scrolls globally, but in the explorer it moves the selection.
        let f = fixture();
        assert_eq!(
            on_key(press(KeyCode::Down), &f.ctx(PaneId::Explorer)),
            [Intent::View(ViewCmd::MoveTreeSelection(1))]
        );
        assert!(matches!(
            on_key(press(KeyCode::Down), &f.ctx(PaneId::Grid))[0],
            Intent::View(ViewCmd::ScrollBy { .. })
        ));
    }

    #[test]
    fn escape_means_different_things_in_different_contexts() {
        let f = fixture();
        let mut c = f.ctx(PaneId::Grid);
        assert!(matches!(
            on_key(press(KeyCode::Esc), &c)[0],
            Intent::View(ViewCmd::DismissToast(_))
        ));

        c.modal_open = true;
        assert_eq!(
            on_key(press(KeyCode::Esc), &c),
            [Intent::View(ViewCmd::DismissModal)]
        );
    }

    #[test]
    fn a_modal_takes_the_keyboard_over_entirely() {
        let f = fixture();
        let mut c = f.ctx(PaneId::Grid);
        c.modal_open = true;
        // `s` sorts in the grid, but the grid is not what has the keyboard.
        assert!(on_key(press(KeyCode::Char('s')), &c).is_empty());
        // And neither is the global map, which is where the dangerous ones are.
        assert!(on_key(press(KeyCode::Char('q')), &c).is_empty());
        assert!(on_key(press_ctrl(KeyCode::Char('c')), &c).is_empty());
    }

    #[test]
    fn sorting_and_resizing_act_on_the_selected_column() {
        // The mouse can sort any header and resize any edge. Bound to column
        // zero, the keys would only look like the same capability.
        let f = fixture();
        let c = f.ctx(PaneId::Grid);
        assert_eq!(
            on_key(press(KeyCode::Char('s')), &c),
            [Intent::App(Action::SortPreview {
                conn: f.conn,
                table: TableRef::new(["public", "users"]),
                column: 2
            })]
        );
        assert_eq!(
            on_key(press(KeyCode::Char('>')), &c),
            [Intent::View(ViewCmd::ResizeColumn { col: 2, delta: 1 })]
        );
    }

    #[test]
    fn a_driver_that_cannot_sort_is_never_asked_to() {
        // Both halves. Gating only the click would leave `s` sending an action
        // the store drops, and a key that quietly does nothing is the harder of
        // the two to notice.
        let f = fixture().advertising(NO_SORT);
        let c = f.ctx(PaneId::Grid);
        assert!(on_mouse(Target::GridHeader { col: 2 }, Gesture::Click, &c).is_empty());
        assert!(on_key(press(KeyCode::Char('s')), &c).is_empty());

        // The column is still the one to widen, though: what the capability
        // takes away is the ordering, not the header.
        assert_eq!(
            on_mouse(
                Target::GridColEdge { col: 2 },
                Gesture::DragBy { dx: 3, dy: 0 },
                &c
            ),
            [Intent::View(ViewCmd::ResizeColumn { col: 2, delta: 3 })]
        );
    }

    #[test]
    fn the_search_box_takes_the_keyboard_from_everything_else() {
        // The reason it needs a context of its own: `q` quits, `s` sorts, `/`
        // opens the box — and every one of them is a letter in a table's name.
        let f = fixture();
        let mut c = f.ctx(PaneId::Explorer);
        c.filter = Some("us");

        for (key, expected) in [('q', "usq"), ('s', "uss"), ('/', "us/")] {
            assert_eq!(
                on_key(press(KeyCode::Char(key)), &c),
                [Intent::View(ViewCmd::SetFilter(Some(expected.to_owned())))],
                "{key} should have gone into the box"
            );
        }
    }

    #[test]
    fn slash_opens_the_box_and_is_not_typed_into_it() {
        let f = fixture();
        let c = f.ctx(PaneId::Explorer);
        assert_eq!(
            on_key(press(KeyCode::Char('/')), &c),
            [Intent::View(ViewCmd::SetFilter(Some(String::new())))]
        );
    }

    #[test]
    fn every_way_out_of_the_box_leaves_the_tree_whole() {
        // `None` and not `Some("")`: a filter that outlived its box would hide
        // rows with nothing on screen to explain why.
        let f = fixture();
        let mut c = f.ctx(PaneId::Explorer);
        c.filter = Some("users");
        for key in [KeyCode::Esc, KeyCode::Enter] {
            assert_eq!(
                on_key(press(key), &c),
                [Intent::View(ViewCmd::SetFilter(None))],
                "{key:?}"
            );
        }
        // And the pointer's way out, which is the only thing a pointer can do
        // to a box it cannot type into.
        assert_eq!(
            on_mouse(Target::Button(ButtonId::Filter), Gesture::Click, &c),
            [Intent::View(ViewCmd::SetFilter(None))]
        );
    }

    #[test]
    fn backspace_empties_the_box_and_then_closes_it() {
        let f = fixture();
        let mut c = f.ctx(PaneId::Explorer);
        c.filter = Some("us");
        assert_eq!(
            on_key(press(KeyCode::Backspace), &c),
            [Intent::View(ViewCmd::SetFilter(Some("u".to_owned())))]
        );
        // On an empty box there is nothing to delete, and closing is where the
        // user's fingers already are.
        c.filter = Some("");
        assert_eq!(
            on_key(press(KeyCode::Backspace), &c),
            [Intent::View(ViewCmd::SetFilter(None))]
        );
    }

    #[test]
    fn a_row_the_filter_removed_is_not_reached_through_the_tree() {
        // Row numbers are positions on screen. Read straight out of the tree
        // they keep meaning whatever sits at that index, so a click lands on a
        // relation that is not drawn — and `Enter` opens it.
        let f = fixture();
        let mut c = f.ctx(PaneId::Explorer);
        assert!(
            !on_mouse(Target::TreeRow { index: 1 }, Gesture::DoubleClick, &c).is_empty(),
            "row 1 is `users` with the tree whole"
        );

        // `public` matches and `users` does not, so only row 0 is left.
        c.filter = Some("public");
        assert!(
            on_mouse(Target::TreeRow { index: 1 }, Gesture::DoubleClick, &c).is_empty(),
            "row 1 is not on screen and must not act on anything"
        );
    }

    #[test]
    fn shift_tab_moves_focus_the_other_way() {
        let f = fixture();
        let c = f.ctx(PaneId::Grid);
        assert_eq!(
            on_key(press(KeyCode::Tab), &c),
            [Intent::View(ViewCmd::FocusNextPane)]
        );
        assert_eq!(
            on_key(press(KeyCode::BackTab), &c),
            [Intent::View(ViewCmd::FocusPrevPane)]
        );
        assert_eq!(
            on_key(press_ctrl(KeyCode::Char('h')), &c),
            [Intent::View(ViewCmd::FocusPrevPane)]
        );
    }

    #[test]
    fn left_closes_a_node_but_never_opens_one() {
        let f = fixture();
        let mut c = f.ctx(PaneId::Explorer);
        let conn = f.snapshot.connections[0].id;
        let public = NodeRef::new(NodeKind::Namespace, ["public"]);

        // Row 0 is expanded, so Left collapses it.
        c.tree_selection = Some(0);
        assert_eq!(
            on_key(press(KeyCode::Left), &c),
            [Intent::App(Action::ToggleNode {
                conn,
                node: public.clone()
            })]
        );
        // Right and Space still toggle in both directions.
        assert_eq!(
            on_key(press(KeyCode::Right), &c),
            [Intent::App(Action::ToggleNode { conn, node: public })]
        );

        // Row 1 is a leaf: nothing to close, and nothing to open either.
        c.tree_selection = Some(1);
        assert!(on_key(press(KeyCode::Left), &c).is_empty());
    }

    #[test]
    fn the_cell_cursor_moves_on_both_axes() {
        let f = fixture();
        let c = f.ctx(PaneId::Grid);

        // A click selects any cell, so the keyboard has to reach any cell too.
        // Both directions are `GridSelection`, so the coverage sweep cannot
        // see the difference — it was missing until someone looked.
        for (k, drow, dcol) in [('J', 1, 0), ('K', -1, 0), ('L', 0, 1), ('H', 0, -1)] {
            assert_eq!(
                on_key(press(KeyCode::Char(k)), &c),
                [Intent::View(ViewCmd::MoveCellSelection { drow, dcol })],
                "{k}"
            );
        }
    }

    #[test]
    fn the_arrows_still_move_the_view_not_the_selection() {
        // Lower case and the arrows scroll; upper case selects. Breaking that
        // symmetry is how `Left` ends up meaning two things.
        let f = fixture();
        let c = f.ctx(PaneId::Grid);
        assert_eq!(
            on_key(press(KeyCode::Right), &c),
            [Intent::View(ViewCmd::ScrollXBy { delta: 1 })]
        );
    }

    #[test]
    fn a_middle_click_closes_the_tab_it_lands_on() {
        // The second way to close a tab, and the one that does not require
        // hitting a one-cell `×`.
        let f = fixture();
        let c = f.ctx(PaneId::Grid);
        let tab = TabId::new(2);
        // Tab 2 ("empty") is the only tab open on that relation, so closing
        // it also tells the store to forget the cached preview.
        assert_eq!(
            on_mouse(Target::Tab(tab), Gesture::MiddleClick, &c),
            [
                Intent::View(ViewCmd::CloseTab(tab)),
                Intent::App(Action::ForgetPreview {
                    conn: f.conn,
                    table: TableRef::new(["public", "empty"]),
                }),
            ]
        );
        // A left click still selects rather than closes.
        assert_eq!(
            on_mouse(Target::Tab(tab), Gesture::Click, &c),
            [Intent::View(ViewCmd::SelectTab(tab))]
        );
    }

    #[test]
    fn closing_one_of_two_tabs_on_the_same_relation_keeps_the_data() {
        // `ViewCmd::OpenTab` never produces this today — it raises the
        // existing tab instead of minting a second one on the same
        // `(conn, table)` — but `close_tab` checks anyway, so this pins the
        // behaviour down independently of that invariant holding forever.
        let mut f = fixture();
        let twin = TabId::new(99);
        f.tabs.push(OpenTab {
            id: twin,
            conn: f.conn,
            table: TableRef::new(["public", "users"]),
        });

        // `on_mouse` removes nothing itself, so the context is rebuilt
        // between the two closes the way `ui.apply` would leave it.
        assert_eq!(
            on_mouse(
                Target::TabClose(TabId::new(1)),
                Gesture::Click,
                &f.ctx(PaneId::Grid)
            ),
            [Intent::View(ViewCmd::CloseTab(TabId::new(1)))],
            "the twin is still showing the same relation"
        );

        f.tabs.retain(|t| t.id != TabId::new(1));
        assert_eq!(
            on_mouse(Target::TabClose(twin), Gesture::Click, &f.ctx(PaneId::Grid)),
            [
                Intent::View(ViewCmd::CloseTab(twin)),
                Intent::App(Action::ForgetPreview {
                    conn: f.conn,
                    table: TableRef::new(["public", "users"]),
                }),
            ],
            "the twin was the last one left"
        );
    }

    #[test]
    fn switching_tabs_with_nothing_active_lands_on_the_first_one() {
        // The active tab was just closed. Stepping from an assumed index 0
        // would skip the tab the user is looking at.
        let f = fixture();
        let c = f.ctx_no_active_tab(PaneId::Grid);
        assert_eq!(
            on_key(press(KeyCode::Char(']')), &c),
            [Intent::View(ViewCmd::SelectTab(TabId::new(1)))]
        );
        assert_eq!(
            on_key(press(KeyCode::Char('[')), &c),
            [Intent::View(ViewCmd::SelectTab(TabId::new(1)))]
        );
    }

    #[test]
    fn the_wheel_works_over_the_empty_part_of_a_pane() {
        let f = fixture();
        let c = f.ctx(PaneId::Grid);
        assert_eq!(
            on_mouse(Target::Pane(PaneId::Explorer), Gesture::Scroll(1), &c),
            [Intent::View(ViewCmd::ScrollBy {
                pane: PaneId::Explorer,
                delta: WHEEL_LINES
            })]
        );
    }

    #[test]
    fn enter_opens_the_selected_relation() {
        let f = fixture();
        let mut c = f.ctx(PaneId::Explorer);
        c.tree_selection = Some(1);
        let out = on_key(press(KeyCode::Enter), &c);
        assert_eq!(
            out,
            [
                Intent::View(ViewCmd::OpenTab {
                    conn: f.conn,
                    table: TableRef::new(["public", "users"]),
                }),
                Intent::App(Action::PreviewTable {
                    conn: f.conn,
                    table: TableRef::new(["public", "users"]),
                }),
            ]
        );
    }

    #[test]
    fn tab_switching_wraps_in_both_directions() {
        let f = fixture();
        let c = f.ctx(PaneId::Grid);
        assert_eq!(
            on_key(press(KeyCode::Char(']')), &c),
            [Intent::View(ViewCmd::SelectTab(TabId::new(2)))]
        );
        assert_eq!(
            on_key(press(KeyCode::Char('[')), &c),
            [Intent::View(ViewCmd::SelectTab(TabId::new(2)))],
            "from the first tab, backwards wraps to the last"
        );
    }

    #[test]
    fn cancel_targets_what_is_actually_running() {
        let f = fixture();
        assert_eq!(
            on_key(press_ctrl(KeyCode::Char('g')), &f.ctx(PaneId::Grid)),
            [Intent::App(Action::Cancel(BusyId::new(1)))]
        );
    }

    #[test]
    fn quit_is_bound_twice_for_the_two_habits() {
        let f = fixture();
        let c = f.ctx(PaneId::Grid);
        assert_eq!(
            on_key(press(KeyCode::Char('q')), &c),
            [Intent::App(Action::Quit)]
        );
        assert_eq!(
            on_key(press_ctrl(KeyCode::Char('c')), &c),
            [Intent::App(Action::Quit)]
        );
    }

    #[test]
    fn key_releases_are_ignored() {
        // Windows terminals report them, and acting on both would double every
        // keystroke.
        let f = fixture();
        let mut event = press(KeyCode::Char('q'));
        event.kind = KeyEventKind::Release;
        assert!(on_key(event, &f.ctx(PaneId::Grid)).is_empty());
    }

    #[test]
    fn an_unbound_key_does_nothing() {
        let f = fixture();
        assert!(on_key(press(KeyCode::Char('~')), &f.ctx(PaneId::Grid)).is_empty());
    }

    #[test]
    fn an_action_with_nothing_to_act_on_produces_nothing() {
        let empty = Snapshot::default();
        let c = InputContext {
            snapshot: &empty,
            focus: PaneId::Grid,
            modal_open: false,
            connection: None,
            tree_selection: None,
            grid_column: None,
            tabs: &[],
            active_tab: None,
            toasts: &[],
            filter: None,
        };
        for code in [
            KeyCode::Char('s'),
            KeyCode::Char('m'),
            KeyCode::Char(']'),
            KeyCode::Char('D'),
            KeyCode::Enter,
        ] {
            assert!(on_key(press(code), &c).is_empty(), "{code:?}");
        }
    }

    #[test]
    fn connecting_walks_the_profiles_and_can_reopen_a_closed_one() {
        let mut f = fixture();
        f.snapshot.profiles = Arc::new(vec![mock_summary("replica"), mock_summary("staging")]);
        f.snapshot.connections[0].profile = mock_summary("replica").id;

        // `replica` is open, so `c` reaches for the one that is not.
        let out = on_key(press(KeyCode::Char('c')), &f.ctx(PaneId::Explorer));
        assert_eq!(
            out,
            [Intent::App(Action::Connect(mock_summary("staging").id))]
        );

        // Still opening counts as open. Otherwise a second press while the
        // first connection is still on its way opens a duplicate of it rather
        // than moving on to the profile that has nothing.
        f.snapshot.connections[0].status = ConnStatus::Connecting;
        let out = on_key(press(KeyCode::Char('c')), &f.ctx(PaneId::Explorer));
        assert_eq!(
            out,
            [Intent::App(Action::Connect(mock_summary("staging").id))]
        );

        // Closing a connection leaves its row behind, and a row is not a
        // connection: `c` has to be able to open `replica` again rather than
        // skipping past it for ever.
        f.snapshot.connections[0].status = ConnStatus::Closed;
        let out = on_key(press(KeyCode::Char('c')), &f.ctx(PaneId::Explorer));
        assert_eq!(
            out,
            [Intent::App(Action::Connect(mock_summary("replica").id))]
        );

        // The same profile twice is a second window onto one database, so the
        // key never goes dead once everything is open.
        f.snapshot.connections[0].status = ConnStatus::Ready;
        f.snapshot.profiles = Arc::new(vec![mock_summary("replica")]);
        let out = on_key(press(KeyCode::Char('c')), &f.ctx(PaneId::Explorer));
        assert_eq!(
            out,
            [Intent::App(Action::Connect(mock_summary("replica").id))]
        );
    }

    // ── the rule this whole module exists to keep ──────────────────────────

    /// Declares the sample values for an enum next to an exhaustive match over
    /// it, from one list.
    ///
    /// The samples have to be generated from the same arms that make the match
    /// exhaustive. Counting variants against a literal instead would let a new
    /// variant be given a match arm and no sample: the count would still equal
    /// the literal, and the sweep below would silently stop covering it.
    macro_rules! samples {
        ($ty:ty, $all:ident, $exhaustive:ident, $($pattern:pat => [$($sample:expr),+ $(,)?]),+ $(,)?) => {
            fn $all() -> Vec<$ty> {
                vec![$($($sample),+),+]
            }

            /// Never called: it exists so that adding a variant stops this
            /// module compiling until the arm — and therefore the sample —
            /// is written.
            #[allow(dead_code)]
            const fn $exhaustive(value: &$ty) {
                match value {
                    $($pattern => ()),+
                }
            }
        };
    }

    samples! {
        Target, all_targets, every_target_is_sampled,
        Target::Pane(_) => [Target::Pane(PaneId::Grid), Target::Pane(PaneId::Explorer)],
        Target::TreeRow { .. } => [Target::TreeRow { index: 0 }],
        Target::TreeToggle { .. } => [Target::TreeToggle { index: 0 }],
        Target::GridCell { .. } => [Target::GridCell { row: 0, col: 0 }],
        Target::GridHeader { .. } => [Target::GridHeader { col: 0 }],
        Target::GridColEdge { .. } => [Target::GridColEdge { col: 0 }],
        Target::Scrollbar { .. } => [
            Target::Scrollbar { pane: PaneId::Grid, part: ScrollPart::Thumb },
            Target::Scrollbar { pane: PaneId::Grid, part: ScrollPart::TrackBefore },
            Target::Scrollbar { pane: PaneId::Grid, part: ScrollPart::TrackAfter },
        ],
        Target::Splitter(_) => [Target::Splitter(SplitId::Explorer)],
        Target::Tab(_) => [Target::Tab(TabId::new(1))],
        Target::TabClose(_) => [Target::TabClose(TabId::new(1))],
        Target::Button(_) => [
            Target::Button(ButtonId::Cancel(BusyId::new(1))),
            Target::Button(ButtonId::DismissModal),
        ],
        Target::Toast(_) => [Target::Toast(ToastId::new(1))],
        Target::Backdrop => [Target::Backdrop],
        Target::Modal => [Target::Modal],
    }

    samples! {
        Gesture, all_gestures, every_gesture_is_sampled,
        Gesture::Down => [Gesture::Down],
        Gesture::Up => [Gesture::Up],
        Gesture::Click => [Gesture::Click],
        Gesture::DoubleClick => [Gesture::DoubleClick],
        Gesture::RightClick => [Gesture::RightClick],
        Gesture::MiddleClick => [Gesture::MiddleClick],
        Gesture::DragBy { .. } => [Gesture::DragBy { dx: 1, dy: 1 }, Gesture::DragBy { dx: -1, dy: -1 }],
        Gesture::Scroll(_) => [Gesture::Scroll(1), Gesture::Scroll(-1)],
        Gesture::ScrollX(_) => [Gesture::ScrollX(1), Gesture::ScrollX(-1)],
        Gesture::HoverEnter => [Gesture::HoverEnter],
        Gesture::HoverLeave => [Gesture::HoverLeave],
    }

    #[test]
    fn every_capability_reachable_with_the_mouse_has_a_key_binding() {
        // This is the mechanical form of "nothing is mouse-only". The reverse
        // is deliberately not required: keyboard-only capabilities are fine.
        let f = fixture();
        let mut contexts = Vec::new();
        for focus in [PaneId::Explorer, PaneId::Grid] {
            for selection in [Some(0), Some(1)] {
                let mut c = f.ctx(focus);
                c.tree_selection = selection;
                contexts.push(c);
            }
        }

        let mut reachable = BTreeSet::new();
        for target in all_targets() {
            for gesture in all_gestures() {
                for c in &contexts {
                    for intent in on_mouse(target, gesture, c) {
                        reachable.insert(IntentKind::of(&intent));
                    }
                }
            }
        }
        assert!(!reachable.is_empty(), "the sweep found nothing at all");

        let bound: BTreeSet<_> = KEYMAP.iter().map(|b| b.kind).collect();
        let unbound: Vec<_> = reachable.difference(&bound).collect();
        assert!(
            unbound.is_empty(),
            "reachable with the mouse but not with the keyboard: {unbound:?}"
        );
    }

    #[test]
    fn every_key_binding_is_reachable_from_some_key() {
        for binding in KEYMAP {
            assert!(!binding.keys.is_empty(), "{:?} has no keys", binding.kind);
        }
    }

    /// Every focus, selection and modal state a keystroke can be read in.
    fn every_context(f: &Fixture) -> Vec<InputContext<'_>> {
        let mut out = Vec::new();
        for focus in [
            PaneId::TabBar,
            PaneId::Explorer,
            PaneId::Grid,
            PaneId::StatusBar,
        ] {
            for selection in [None, Some(0), Some(1)] {
                for modal_open in [false, true] {
                    // The search box included: a binding in `Context::Filter`
                    // can only fire while it is open, and a sweep that never
                    // opens it would report those bindings as dead.
                    for filter in [None, Some("")] {
                        let mut c = f.ctx(focus);
                        c.tree_selection = selection;
                        c.modal_open = modal_open;
                        c.filter = filter;
                        out.push(c);
                    }
                }
            }
        }
        out
    }

    #[test]
    fn every_binding_produces_the_capability_it_claims() {
        // The sweep above compares the set of kinds the mouse reaches against
        // the set of kinds the map *names*. On its own that is satisfiable by a
        // binding that names a kind and produces nothing — a key that is listed
        // in the help and does not work. This closes that half.
        let f = fixture();
        let contexts = every_context(&f);

        for binding in KEYMAP {
            for combo in binding.keys {
                let event = KeyEvent::new(combo.code, combo.modifiers);
                let works = contexts.iter().any(|c| {
                    let intents = on_key(event, c);
                    !intents.is_empty() && intents.iter().all(|i| IntentKind::of(i) == binding.kind)
                });
                assert!(
                    works,
                    "{:?} in {:?} never produces {:?}",
                    combo.code, binding.context, binding.kind
                );
            }
        }
    }

    #[test]
    fn a_modal_leaves_no_key_bound_to_anything_else() {
        // "A modal takes the keyboard over" is a claim about every key, not
        // just the ones belonging to a pane: `q` behind a dialog must answer
        // the dialog or do nothing, never quit.
        let f = fixture();
        let mut c = f.ctx(PaneId::Grid);
        c.modal_open = true;

        for binding in KEYMAP {
            for combo in binding.keys {
                let event = KeyEvent::new(combo.code, combo.modifiers);
                for intent in on_key(event, &c) {
                    assert_eq!(
                        IntentKind::of(&intent),
                        IntentKind::DismissModal,
                        "{:?} still reaches {intent:?} with a modal open",
                        combo.code
                    );
                }
            }
        }
    }

    #[test]
    fn no_key_is_bound_twice_in_the_same_context() {
        let mut seen = BTreeSet::new();
        for binding in KEYMAP {
            for combo in binding.keys {
                let key = (
                    binding.context,
                    format!("{:?}", combo.code),
                    combo.modifiers.bits(),
                );
                assert!(
                    seen.insert(key.clone()),
                    "{:?} is bound twice in {:?}",
                    key.1,
                    binding.context
                );
            }
        }
    }
}

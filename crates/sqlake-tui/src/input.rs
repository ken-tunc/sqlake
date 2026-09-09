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
use sqlake_app::snapshot::{ConnectionView, Snapshot};
use sqlake_app::tree::VisibleNode;
use sqlake_core::id::{ConnId, ProfileId, QueryId, TabId};
use sqlake_core::library::{HistoryEntry, Template};
use sqlake_core::node::TableRef;

use crate::hit::{ButtonId, PaneId, ScrollPart, SplitId, Target};
use crate::intent::{Context, Handover, Intent, IntentKind, ViewCmd};
use crate::mouse::Gesture;
use crate::ui::{Filter, OpenTab, TabContent, Toast};

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

    #[must_use]
    pub const fn shift(code: KeyCode) -> Self {
        Self {
            code,
            modifiers: KeyModifiers::SHIFT,
        }
    }

    /// Shift counts on a key that has no shifted form of its own, and not on
    /// one that has.
    ///
    /// A terminal reports shift inconsistently for a key that already arrives
    /// shifted — `G` arrives as `Char('G')` with or without the modifier
    /// depending on the terminal, and `BackTab` *is* `Shift-Tab`, which
    /// crossterm hands over carrying `SHIFT` on every platform. Both are
    /// matched by their code alone; reading the modifier on them would leave
    /// `Shift-Tab` bound to nothing. An arrow has no shifted form to arrive as
    /// instead, so shift on one is reported and is the only way to say
    /// `Shift-Down`.
    fn matches(self, event: KeyEvent) -> bool {
        const BASE: KeyModifiers = KeyModifiers::CONTROL.union(KeyModifiers::ALT);
        let relevant = if matches!(self.code, KeyCode::Char(_) | KeyCode::BackTab) {
            BASE
        } else {
            BASE.union(KeyModifiers::SHIFT)
        };
        self.code == event.code && (self.modifiers & relevant) == (event.modifiers & relevant)
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

/// Every binding, in one place, because a key map is data.
///
/// A pane binding beats a global one for the same key, which is how `Down`
/// scrolls everywhere and moves the selection in the tree, and how `Esc`
/// cancels a filter rather than dismissing a toast while one is being typed.
/// It is also the hazard when adding a binding: taking a letter a global
/// binding uses stops that capability working while the pane has focus, and
/// nothing here catches it — `no_key_is_bound_twice_in_the_same_context` sees
/// one context at a time, and whether the shadowed meaning still matters in
/// that pane is a judgement rather than a rule. Both cases above are
/// deliberate; `c` for a JSON copy in the grid was not, and would have stopped
/// `c` opening a connection.
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
    // `/` opens the box, and inside it every character is the box's — the
    // chords stay global, so `Ctrl-C` still quits. `Filter` is the only kind
    // bound in two contexts, and it has to be: a keymap that let the global
    // bindings through would make a search for a table called `q` quit.
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
    // `Ctrl-r` for the history, which is the key a shell searches its own with
    // — and a chord rather than a letter because the pane it opens is a search
    // box, so a letter would be the first thing typed into it as often as it
    // opened it. Not `Ctrl-h`: terminals send that for `Backspace`, which is
    // why it is already the backwards half of `Tab`.
    KeyBinding {
        keys: &[KeyCombo::ctrl(KeyCode::Char('r'))],
        context: Context::Global,
        kind: IntentKind::History,
    },
    // `Ctrl-p`, which design.md §6 has reserved for this since before there
    // was anything to put in it. A chord rather than a letter because it has
    // to work while a SQL tab has the keyboard, which is where somebody wants
    // a saved statement.
    KeyBinding {
        keys: &[KeyCombo::ctrl(KeyCode::Char('p'))],
        context: Context::Global,
        kind: IntentKind::Palette,
    },
    // Inside it every character is the filter's or the field's, the way the
    // search box works — these are the keys that are not text.
    KeyBinding {
        keys: &[
            KeyCombo::new(KeyCode::Backspace),
            KeyCombo::new(KeyCode::Esc),
            KeyCombo::new(KeyCode::Up),
            KeyCombo::new(KeyCode::Down),
            KeyCombo::new(KeyCode::Tab),
            KeyCombo::new(KeyCode::BackTab),
        ],
        context: Context::Palette,
        kind: IntentKind::Palette,
    },
    KeyBinding {
        keys: &[KeyCombo::new(KeyCode::Enter)],
        context: Context::Palette,
        kind: IntentKind::UseTemplate,
    },
    // `Ctrl-s` names the statement in the buffer, and `Ctrl-s` again saves it.
    // The same key for both halves because it is one errand, and not `Enter`
    // for the second: `Enter` is the palette's own key and picks from the
    // list, which is what somebody typing a *name* over that list is not
    // doing.
    KeyBinding {
        keys: &[KeyCombo::ctrl(KeyCode::Char('s'))],
        context: Context::Global,
        kind: IntentKind::SaveTemplate,
    },
    KeyBinding {
        keys: &[KeyCombo::ctrl(KeyCode::Char('s'))],
        context: Context::Palette,
        kind: IntentKind::SaveTemplate,
    },
    KeyBinding {
        keys: &[KeyCombo::ctrl(KeyCode::Char('d'))],
        context: Context::Palette,
        kind: IntentKind::DeleteTemplate,
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
        // Shift with the arrows, which is what every grid does. All four,
        // because the pair above already learned that binding one axis leaves
        // the other reachable by mouse and not by keyboard — and the coverage
        // sweep cannot see that, both axes being one capability.
        keys: &[
            KeyCombo::shift(KeyCode::Up),
            KeyCombo::shift(KeyCode::Down),
            KeyCombo::shift(KeyCode::Left),
            KeyCombo::shift(KeyCode::Right),
        ],
        context: Context::Grid,
        kind: IntentKind::ExtendSelection,
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
        // In the grid, where the cell it shows is chosen. `Enter` because it
        // reads as "look at this one", and it is not otherwise bound there.
        keys: &[KeyCombo::new(KeyCode::Enter)],
        context: Context::Grid,
        kind: IntentKind::ToggleDetail,
    },
    KeyBinding {
        // The menu itself needs a key, not only its entries: a terminal that
        // cannot deliver a right-click would otherwise have no way to see what
        // is on offer. `.` because it is free in every context and reads as
        // "and the rest" — `m` is `LoadMore` here, and taking it would have
        // been the `c` mistake again.
        keys: &[key('.')],
        context: Context::Grid,
        kind: IntentKind::Menu,
    },
    KeyBinding {
        // The letter says how much and the case says which format: `y` yanks
        // what is selected, `a` takes it all, and shift on either asks for
        // JSON instead of CSV.
        //
        // Not `c` for JSON, which is what this first reached for: `c` already
        // opens a connection, and a pane binding beats a global one — so with
        // the grid focused a working gesture would quietly have started doing
        // something else. `Ctrl-c` is not available either; it is the
        // terminal's interrupt, and a grid nobody can get out of is worse than
        // a shortcut nobody has.
        keys: &[key('y'), key('Y'), key('a'), key('A')],
        context: Context::Grid,
        kind: IntentKind::Copy,
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
    // `u` for use, in the grid — where it reaches anything only in the
    // history, the one grid whose rows are statements rather than somebody's
    // data. Not `Enter`, which is already "look at this one" in every grid and
    // is worth more here than anywhere: a statement too long for its column is
    // exactly what the detail pane is for.
    KeyBinding {
        keys: &[key('u')],
        context: Context::Grid,
        kind: IntentKind::ReuseRun,
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
        // `r` for run. `Ctrl-Enter` is bound too because it is what design.md
        // named and what every other client uses — but only as a second combo:
        // a terminal without the kitty keyboard protocol cannot send it at
        // all, so binding it alone would be a gesture half the terminals in
        // the world do not have.
        keys: &[key('r'), KeyCombo::ctrl(KeyCode::Enter)],
        context: Context::Global,
        kind: IntentKind::RunQuery,
    },
    KeyBinding {
        // Only reaches anything while a dialog is asking, which is the whole
        // of when there is something to approve. `Enter` rather than a letter:
        // a dialog's default answer is the one under the finger already.
        keys: &[KeyCombo::new(KeyCode::Enter)],
        context: Context::Modal,
        kind: IntentKind::ApproveQuery,
    },
    KeyBinding {
        // Global rather than in the grid: the pane it acts on is the grid's,
        // but a query being written is what the whole client is doing at that
        // point, and needing focus in the pane first is a step with nothing
        // behind it. `e` is free everywhere, and design.md has reserved it
        // since before there was a SQL tab to spend it on.
        keys: &[key('e')],
        context: Context::Global,
        kind: IntentKind::EditExternally,
    },
    KeyBinding {
        // `d` shows what the selected relation is. In the explorer, where the
        // relation is chosen — `D` is disconnect and global, and the pane
        // binding beating it is the hazard `KEYMAP` warns about, so the case
        // matters here.
        keys: &[key('d')],
        context: Context::Explorer,
        kind: IntentKind::DescribeTable,
    },
    KeyBinding {
        // Through the definition's sections. `Tab` moves focus and `[`/`]`
        // move tabs, so neither is free; `{`/`}` are the shifted forms of the
        // brackets already used for the thing one level up, which is the
        // relationship they have.
        keys: &[key('{'), key('}')],
        context: Context::Grid,
        kind: IntentKind::SelectSection,
    },
    KeyBinding {
        // `n` for a new one. Global rather than in the grid: the gesture it
        // matches is the `+` on the tab bar, which no pane owns, and a tab
        // opened only while the grid has focus would be unreachable from the
        // explorer where the connection is chosen.
        keys: &[key('n')],
        context: Context::Global,
        kind: IntentKind::OpenSqlTab,
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
    /// Where the pointer is, for a gesture that opens something at it.
    pub pointer: (u16, u16),
    /// The selected rectangle, `(top, left, bottom, right)` inclusive.
    ///
    /// The whole rectangle rather than "is it more than one cell", because a
    /// right-click has to know whether it landed *inside* the selection: on it,
    /// the menu is about the selection; anywhere else, it is about the cell
    /// that was clicked, and leaving the selection where it was would act on a
    /// cell nobody pointed at.
    pub selection: Option<(usize, usize, usize, usize)>,
    /// The open context menu, so a click on one of its lines can be resolved to
    /// the intent that line carries.
    pub menu: Option<&'a crate::menu::Menu>,
    /// Which relation a tab points at is what turns a click on a header into
    /// a `SortPreview` for the right table.
    pub tabs: &'a [OpenTab],
    pub active_tab: Option<TabId>,
    /// The open dialog, so a click on one of its buttons resolves to the
    /// intent that button carries — the same way a menu line does.
    pub modal: Option<&'a crate::overlay::Modal>,
    pub toasts: &'a [Toast],
    /// The explorer's search, or `None` when there is not one. It redirects
    /// the keyboard into itself only while it is being edited.
    pub filter: Option<&'a Filter>,
    /// The open palette, which holds the keyboard while it is up — and which
    /// a key has to be able to read in order to change it: the whole state
    /// travels on the command that changes it.
    pub palette: Option<&'a crate::palette::Palette>,
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
            self.snapshot
                .connections
                .iter()
                .any(|c| &c.profile == id && c.is_live())
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
        let row = *crate::tree::visible(
            &self.snapshot.explorer.nodes,
            self.filter.map(|f| f.text.as_str()),
        )
        .get(index)?;
        self.snapshot.explorer.get(row)
    }

    fn active_tab(&self) -> Option<TabId> {
        self.active_tab
    }

    /// A connection a SQL tab can be opened on: the selected row's, and
    /// otherwise the first that is still live.
    ///
    /// Not [`Self::connection`] as it stands, which falls back to the first row
    /// whatever its status. A closed connection keeps its row, so after
    /// disconnecting the last one that fallback still answers — and the tab it
    /// mints is taken away again by `close_disconnected_tabs` on the next
    /// frame, which is a `+` that answers a click with nothing. The fallback to
    /// the first *live* one is what keeps this and the bar's own `+` agreeing
    /// on when there is somewhere to run a query.
    fn sql_connection(&self) -> Option<ConnId> {
        let live = |id: ConnId| {
            self.snapshot
                .connection(id)
                .is_some_and(ConnectionView::is_live)
        };
        self.connection.filter(|c| live(*c)).or_else(|| {
            self.snapshot
                .connections
                .iter()
                .find(|c| c.is_live())
                .map(|c| c.id)
        })
    }

    /// The active tab, when it is one with a buffer to edit.
    fn editable_tab(&self) -> Option<TabId> {
        let id = self.active_tab?;
        self.tabs
            .iter()
            .find(|t| t.id == id)?
            .table()
            .is_none()
            .then_some(id)
    }

    /// The active tab's buffer and where to run it, when there is something
    /// to run.
    ///
    /// An empty buffer is not: running nothing produces a statement the server
    /// refuses, reported as an error, for a key press that should have done
    /// nothing at all.
    fn runnable(&self) -> Option<(ConnId, String)> {
        let id = self.active_tab?;
        let tab = self.tabs.iter().find(|t| t.id == id)?;
        let TabContent::Sql { text: sql, .. } = &tab.content else {
            return None;
        };
        (!sql.trim().is_empty()
            && self
                .snapshot
                .connection(tab.conn)
                .is_some_and(ConnectionView::is_live))
        .then(|| (tab.conn, sql.clone()))
    }

    /// The relation the active tab points at, if any.
    fn active_preview(&self) -> Option<(ConnId, TableRef)> {
        let id = self.active_tab?;
        let tab = self.tabs.iter().find(|t| t.id == id)?;
        Some((tab.conn, tab.table()?.clone()))
    }

    /// The same relation, but only where its driver will order a preview.
    ///
    /// The header keeps its hit target and the click is simply not read.
    /// Dropping the target would let the click fall through to the pane
    /// beneath and focus the grid, which reads as a header that was never a
    /// target. What the click cannot do is said in how `datagrid` draws the
    /// header, off the same capability.
    fn sortable_preview(&self) -> Option<(ConnId, TableRef)> {
        let (conn, table) = self.active_preview()?;
        self.snapshot
            .connection(conn)?
            .can_sort_preview()
            .then_some((conn, table))
    }

    /// Whether the selection covers more than one cell.
    #[must_use]
    fn ranged(&self) -> bool {
        self.selection
            .is_some_and(|(top, left, bottom, right)| top != bottom || left != right)
    }

    /// The history's search box, when its tab is the one in front.
    fn searching(&self) -> Option<&Filter> {
        let tab = self.active_tab?;
        self.tabs.iter().find(|t| t.id == tab)?.searching()
    }

    /// The context a keystroke is read in. A modal takes the keyboard over
    /// entirely, which is why `Esc` can mean two different things without
    /// being ambiguous.
    fn key_context(&self) -> Context {
        if self.modal_open {
            Context::Modal
        } else if self.palette.is_some() {
            // Above the search box and below a dialog: the palette is opened
            // deliberately and typed into, and a dialog on top of it is
            // something that has to be answered first.
            Context::Palette
        // Whichever box is in front owns the keyboard. The explorer's search
        // can be left open behind the history tab, and a keystroke that went
        // to it would be typed into something nobody is looking at — while the
        // box in front sat there apparently ignoring it.
        } else if match self.searching() {
            Some(history) => history.editing,
            None => self.filter.is_some_and(|f| f.editing),
        } {
            Context::Filter
        } else {
            match self.focus {
                PaneId::Explorer => Context::Explorer,
                PaneId::Grid => Context::Grid,
                // The pane shows the grid's selected cell, so the grid's
                // bindings are the ones that make sense in it — and scrolling,
                // which is `Global`, is what it is mostly for.
                PaneId::Detail => Context::Grid,
                PaneId::TabBar | PaneId::StatusBar => Context::Global,
            }
        }
    }
}

// ── mouse ──────────────────────────────────────────────────────────────────

/// What a gesture on a target means.
#[must_use]
pub fn on_mouse(target: Target, gesture: Gesture, ctx: &InputContext<'_>) -> Vec<Intent> {
    // A gesture anywhere but the menu closes it, and still does what it was a
    // gesture on. A menu that stayed open would have to be dismissed before
    // anything else worked, which is a mode nobody asked for.
    //
    // The press as well as the click, because a drag on the grid starts on the
    // press: without it a rectangle is swept out under a menu that is still
    // sitting on top of it. The wheel too — the menu is placed in screen
    // coordinates, so content scrolling beneath leaves it pointing at a cell
    // that has moved.
    if ctx.menu.is_some()
        && matches!(
            gesture,
            Gesture::Click
                | Gesture::RightClick
                | Gesture::Down
                | Gesture::Scroll(_)
                | Gesture::ScrollX(_)
        )
        && !matches!(target, Target::MenuItem { .. } | Target::Menu)
    {
        let mut intents = vec![ViewCmd::CloseMenu.into()];
        intents.extend(mouse_intents(target, gesture, ctx));
        return intents;
    }
    mouse_intents(target, gesture, ctx)
}

fn mouse_intents(target: Target, gesture: Gesture, ctx: &InputContext<'_>) -> Vec<Intent> {
    match (target, gesture) {
        (Target::Pane(pane), Gesture::Click) => vec![ViewCmd::FocusPane(pane).into()],
        // A SQL tab draws no cells, so the pane itself is what a pointer lands
        // on — which makes double-clicking the buffer the obvious way to open
        // it, and the reason design.md called it "a click on the editor area".
        // On a preview the pane is covered by cells, so this is never reached
        // there; the `table().is_none()` check is what makes that a fact rather
        // than an assumption about layout.
        (Target::Pane(PaneId::Grid), Gesture::DoubleClick) => ctx
            .editable_tab()
            .map(|tab| vec![Handover::Edit(tab).into()])
            .unwrap_or_default(),
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

        (Target::GridCell { row, col }, Gesture::RightClick) => {
            let inside = ctx.selection.is_some_and(|(top, left, bottom, right)| {
                (top..=bottom).contains(&row) && (left..=right).contains(&col)
            });
            let mut intents = Vec::new();
            if !inside {
                // Outside it, the click moves the selection first: a menu whose
                // "Copy cell" copies a cell somewhere else on screen is worse
                // than no menu.
                intents.push(ViewCmd::FocusPane(PaneId::Grid).into());
                intents.push(ViewCmd::SelectCell { row, col }.into());
            }
            intents.push(
                ViewCmd::OpenMenu {
                    at: Some(ctx.pointer),
                    ranged: inside && ctx.ranged(),
                }
                .into(),
            );
            intents
        }
        // Choosing an entry is the entry's own intent. That is what makes
        // `every_menu_entry_has_a_key_binding` a check rather than a
        // convention: the menu cannot offer anything the keyboard cannot.
        (Target::MenuItem { index }, Gesture::Click) => ctx
            .menu
            .and_then(|menu| menu.entries.get(index))
            .filter(|entry| entry.enabled)
            .map(|entry| vec![ViewCmd::CloseMenu.into(), entry.intent.clone()])
            .unwrap_or_default(),
        // Everything else on the menu is swallowed rather than passed down: it
        // covers the grid, and a gesture on it is not a gesture on the cell it
        // is covering.
        (Target::MenuItem { .. } | Target::Menu, _) => Vec::new(),

        // The press, not the release: the first motion extends from wherever
        // the cursor is, so a drag that began on a cell has to have moved it
        // there already. Waiting for the click would anchor the rectangle on
        // whatever was selected before and sweep it across everything between.
        (Target::GridCell { row, col }, Gesture::Down) => vec![
            ViewCmd::FocusPane(PaneId::Grid).into(),
            ViewCmd::SelectCell { row, col }.into(),
        ],
        // The cell under the pointer, not the one the drag started on — that
        // one is the anchor, and it is already where the selection began.
        // Only for a drag that began on a cell: a splitter or a column edge
        // dragged past the grid puts the pointer over cells it is not about.
        (
            Target::GridCell { row, col },
            Gesture::DragOver {
                from: Target::GridCell { .. },
            },
        ) => {
            vec![ViewCmd::ExtendCellSelectionTo { row, col }.into()]
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
        // What clicking a text box means everywhere else: put the keyboard in
        // it. Discarding the search instead is the surprise, and there is no
        // `×` on the box to say that is what the click would do.
        (Target::Button(ButtonId::Filter), Gesture::Click) => ctx
            .filter
            .map(|filter| {
                vec![
                    ViewCmd::SetFilter(Some(Filter {
                        editing: true,
                        ..filter.clone()
                    }))
                    .into(),
                ]
            })
            .unwrap_or_default(),
        // One click both selects and uses: the palette is open in order to
        // pick something, and a click that only highlighted a row would leave
        // the pointer needing the keyboard to finish.
        (Target::PaletteRow { index }, Gesture::Click) => {
            let Some(open) = ctx.palette else {
                return Vec::new();
            };
            let mut next = open.clone();
            next.selected = index;
            let Some(held) = templates(ctx) else {
                return Vec::new();
            };
            next.picked(held)
                .map(|template| template.id)
                .map(|id| {
                    vec![
                        ViewCmd::Palette(Some(next.clone())).into(),
                        ViewCmd::UseTemplate(id).into(),
                    ]
                })
                .unwrap_or_default()
        }
        (Target::Section { index }, Gesture::Click) => {
            vec![ViewCmd::SelectSection(crate::intent::SectionPick::At(index)).into()]
        }
        // The list sits inside the grid pane, so the wheel over it has to mean
        // what the wheel one column to the right means. Without this the
        // leftmost sixteen columns of the pane are a strip the wheel does
        // nothing in, which reads as the mouse being broken.
        (Target::Section { .. }, Gesture::Scroll(delta)) => vec![scroll(PaneId::Grid, delta)],
        (Target::Button(ButtonId::RunQuery), Gesture::Click) => {
            ctx.runnable().map_or_else(Vec::new, |(conn, sql)| {
                vec![
                    Action::RunQuery {
                        conn,
                        query: QueryId::new(),
                        sql,
                        max_rows: None,
                        // The session's own ceiling is the only one here: a person
                        // running a query in front of them has already agreed to
                        // whatever `max_bytes_billed` says.
                        max_bytes: None,
                    }
                    .into(),
                ]
            })
        }
        (Target::Button(ButtonId::NewSqlTab), Gesture::Click) => ctx
            .sql_connection()
            .map(|conn| vec![ViewCmd::OpenSqlTab { conn }.into()])
            .unwrap_or_default(),
        (Target::Button(ButtonId::ModalChoice { index }), Gesture::Click) => ctx
            .modal
            .and_then(|m| m.choices.get(index))
            // Just the answer. The dialog closes because the snapshot stops
            // saying the question is open, the same way every other piece of
            // this screen follows the store — and until it does, answering
            // twice is refused there rather than prevented here.
            .map(|choice| vec![choice.intent.clone()])
            .unwrap_or_default(),
        (Target::Button(ButtonId::DismissModal), Gesture::Click) => {
            vec![ViewCmd::DismissModal.into()]
        }
        (Target::Toast(id), Gesture::Click) => vec![ViewCmd::DismissToast(id).into()],
        // Whichever overlay the backdrop is behind. A dialog wins when both
        // are up, because it is the one on top and the one that has to be
        // answered — and because closing the palette underneath it would
        // leave the click having done something invisible.
        (Target::Backdrop, Gesture::Click) => {
            if ctx.palette.is_some() && !ctx.modal_open {
                vec![ViewCmd::Palette(None).into()]
            } else {
                vec![ViewCmd::DismissModal.into()]
            }
        }

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

/// Opening a definition: both halves, unconditionally.
///
/// `DescribeTable` is what makes a tab raised from a failure retry, and the
/// store treats it as a no-op when the definition is already cached — so
/// there is nothing to decide between here, the same as `open_or_focus_tab`.
fn describe_node(index: usize, ctx: &InputContext<'_>) -> Vec<Intent> {
    let Some(node) = ctx.node(index) else {
        return Vec::new();
    };
    let conn = node.conn;
    let Some(table) = node.node_ref.as_table() else {
        // A namespace has no definition. Silent rather than a message: the
        // gesture is one key, and a row that is not a relation is most of the
        // tree.
        return Vec::new();
    };
    vec![
        ViewCmd::OpenDefinition {
            conn,
            table: table.clone(),
        }
        .into(),
        Action::DescribeTable {
            conn,
            table,
            refresh: false,
        }
        .into(),
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
    let mut intents = vec![ViewCmd::CloseTab(id).into()];
    // A definition is cached until something says otherwise, so the tab going
    // is the only thing that ever will.
    if let Some(table) = closing.defines() {
        intents.push(
            Action::ForgetDefinition {
                conn: closing.conn,
                table: table.clone(),
            }
            .into(),
        );
        return intents;
    }
    // A SQL tab's buffer is this screen's, so there is nothing there to
    // forget — but the run it started is the store's, and nothing else is
    // looking at it: a query is keyed by the run, so no other tab can be.
    let (conn, Some(table)) = (closing.conn, closing.table().cloned()) else {
        if let TabContent::Sql {
            query: Some(query), ..
        } = &closing.content
        {
            intents.push(Action::ForgetQuery(*query).into());
        }
        return intents;
    };
    let still_open = ctx
        .tabs
        .iter()
        .any(|t| t.id != id && t.conn == conn && t.table() == Some(&table));
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

    // A chord is not a character, so the search box does not hold it: `Ctrl-C`
    // is not a letter of any table's name, and swallowing it would put the one
    // key that always quits behind a text box — typing a `c` instead, which is
    // worse than doing nothing.
    let chord = event
        .modifiers
        .intersects(KeyModifiers::CONTROL.union(KeyModifiers::ALT));

    // A pane binding beats the global one for the same key, so `Esc` in a modal
    // dismisses the modal rather than a toast behind it. A modal is the one
    // context with no fallback: it has the keyboard, so `q` behind a "discard
    // these changes?" dialog must not quit instead of answering it.
    let bound = matching(context).or_else(|| match context {
        // Neither has a fallback: both hold the keyboard, so `q` behind a
        // "discard these changes?" dialog must not quit instead of answering
        // it, and a `q` typed into the search box must reach the box.
        Context::Modal => None,
        Context::Filter | Context::Palette if !chord => None,
        _ => matching(Context::Global),
    });

    // Every remaining character belongs to the box, which is what a text input
    // is. Enumerating the printable characters in `KEYMAP` instead would be a
    // hundred bindings that the coverage test would then have to skip. Only a
    // character: an unbound `F5` is not text, and re-sending the filter it did
    // not change is a redraw and a re-search for nothing.
    if context == Context::Filter
        && bound.is_none()
        && !chord
        && matches!(event.code, KeyCode::Char(_))
    {
        return materialise(IntentKind::Filter, event, ctx);
    }
    // The same for the palette: its filter and its fields are text boxes, and
    // enumerating the printable characters in `KEYMAP` would be a hundred
    // bindings the coverage test then has to skip.
    if context == Context::Palette
        && bound.is_none()
        && !chord
        && matches!(event.code, KeyCode::Char(_))
    {
        return materialise(IntentKind::Palette, event, ctx);
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
        KeyCode::Char('h' | 'H' | 'k' | 'K' | '<' | 'g' | '[' | '{')
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
        IntentKind::ExtendSelection => {
            let step = if backwards { -1 } else { 1 };
            let sideways = matches!(event.code, KeyCode::Left | KeyCode::Right);
            vec![
                ViewCmd::ExtendCellSelection {
                    drow: if sideways { 0 } else { step },
                    dcol: if sideways { step } else { 0 },
                }
                .into(),
            ]
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
        IntentKind::Menu => {
            // The same key closes it. Nothing else on the keyboard can: `Esc`
            // in the grid is `DismissToast`, and shadowing that to reach a
            // menu would be the hazard `KEYMAP` warns about. Without the
            // toggle the one gesture that opens a menu on a terminal with no
            // right-click leaves it on screen with no way out.
            if ctx.menu.is_some() {
                return vec![ViewCmd::CloseMenu.into()];
            }
            // No coordinates to give: `None` asks the view to place it, which
            // is the only thing that knows where the grid was drawn.
            ctx.active_tab
                .map(|_| {
                    vec![
                        ViewCmd::OpenMenu {
                            at: None,
                            ranged: ctx.ranged(),
                        }
                        .into(),
                    ]
                })
                .unwrap_or_default()
        }
        IntentKind::Copy => {
            // Two axes on one gesture: the letter says how much, the case
            // says which format.
            let all = matches!(event.code, KeyCode::Char('a' | 'A'));
            let format = if matches!(event.code, KeyCode::Char('Y' | 'A')) {
                crate::copy::Format::Json
            } else {
                crate::copy::Format::Csv
            };
            ctx.active_tab
                .map(|_| vec![ViewCmd::Copy { format, all }.into()])
                .unwrap_or_default()
        }
        // Nothing to show in full when there is no grid to have chosen a cell
        // in, and a pane opening onto "no cell selected" is a gesture that
        // appeared to do something.
        IntentKind::ToggleDetail => ctx
            .active_tab
            .map(|_| vec![ViewCmd::ToggleDetail.into()])
            .unwrap_or_default(),
        IntentKind::DismissModal => vec![ViewCmd::DismissModal.into()],
        // Whichever box is in front. The history's when its tab is open,
        // because that pane is a search with rows under it; the explorer's
        // otherwise. One capability either way — a key that searched the
        // explorer from inside the history would be searching what somebody
        // is not looking at.
        IntentKind::Filter => match ctx.searching() {
            Some(box_) => vec![ViewCmd::SetHistoryTerms(next_filter(event, Some(box_))).into()],
            None => vec![ViewCmd::SetFilter(next_filter(event, ctx.filter)).into()],
        },

        IntentKind::Connect => ctx
            .connectable_profile()
            .map(|profile| {
                vec![
                    Action::Connect {
                        profile,
                        conn: ConnId::new(),
                    }
                    .into(),
                ]
            })
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
        // With nothing live there is nowhere to run a query, and a tab that
        // could never run one is a tab that lies.
        IntentKind::OpenSqlTab => ctx
            .sql_connection()
            .map(|conn| vec![ViewCmd::OpenSqlTab { conn }.into()])
            .unwrap_or_default(),
        // Only a SQL tab has a buffer. On a preview this does nothing rather
        // than opening an editor on the rows, which is not a thing to edit.
        IntentKind::EditExternally => ctx
            .editable_tab()
            .map(|tab| vec![Handover::Edit(tab).into()])
            .unwrap_or_default(),
        IntentKind::RunQuery => ctx.runnable().map_or_else(Vec::new, |(conn, sql)| {
            vec![
                Action::RunQuery {
                    conn,
                    query: QueryId::new(),
                    sql,
                    max_rows: None,
                    // The session's own ceiling is the only one here: a person
                    // running a query in front of them has already agreed to
                    // whatever `max_bytes_billed` says.
                    max_bytes: None,
                }
                .into(),
            ]
        }),
        // The dialog's own first answer, so the key and the button cannot
        // disagree about what "yes" means.
        IntentKind::ApproveQuery => ctx
            .modal
            .and_then(|m| m.choices.first())
            .map(|choice| vec![choice.intent.clone()])
            .unwrap_or_default(),
        // The relation under the cursor in the explorer, which is where a
        // definition is opened from — the same row `Enter` previews.
        IntentKind::DescribeTable => ctx
            .tree_selection
            .map(|i| describe_node(i, ctx))
            .unwrap_or_default(),
        IntentKind::SelectSection => vec![
            ViewCmd::SelectSection(crate::intent::SectionPick::By(if backwards {
                -1
            } else {
                1
            }))
            .into(),
        ],
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
        IntentKind::Palette => palette(event, ctx),
        IntentKind::UseTemplate => use_template(ctx),
        IntentKind::ReuseRun => selected_run(ctx)
            .map(|run| vec![ViewCmd::ReuseRun(run.id).into()])
            .unwrap_or_default(),
        IntentKind::History => ctx
            .connection
            .map(|conn| vec![ViewCmd::OpenHistoryTab { conn }.into()])
            .unwrap_or_default(),
        IntentKind::SaveTemplate => save_template(ctx),
        IntentKind::DeleteTemplate => delete_template(ctx),
    }
}

/// Open the palette, or change the one that is open.
///
/// Every one of these produces the whole new state, which is what makes the
/// view's job an assignment rather than an edit: the key that was pressed is
/// the only thing that knows what it did.
fn palette(event: KeyEvent, ctx: &InputContext<'_>) -> Vec<Intent> {
    let Some(open) = ctx.palette else {
        // Just the view command: opening the palette is what reads the
        // templates, and the view says so on its way out — the same way a
        // scroll asks for the next page.
        return vec![ViewCmd::Palette(Some(crate::palette::Palette::opening())).into()];
    };
    let mut next = open.clone();

    if let Some(form) = &mut next.asking {
        match event.code {
            // Back to the list rather than out altogether: filling in the
            // wrong template is a thing that happens, and one `Esc` should
            // undo one step.
            KeyCode::Esc => next.asking = None,
            KeyCode::Down | KeyCode::Tab => form.move_to(1),
            KeyCode::Up | KeyCode::BackTab => form.move_to(-1),
            KeyCode::Backspace => {
                let mut value = form
                    .fields
                    .get(form.at)
                    .map_or_else(String::new, |f| f.value.clone());
                value.pop();
                form.edit(value);
            }
            KeyCode::Char(c) => {
                let mut value = form
                    .fields
                    .get(form.at)
                    .map_or_else(String::new, |f| f.value.clone());
                value.push(c);
                form.edit(value);
            }
            _ => return Vec::new(),
        }
        return vec![ViewCmd::Palette(Some(next)).into()];
    }

    let matches = templates(ctx).map_or(0, |held| next.matching(held).len());
    match event.code {
        KeyCode::Esc => return vec![ViewCmd::Palette(None).into()],
        KeyCode::Down | KeyCode::Tab => next.move_selection(1, matches),
        KeyCode::Up | KeyCode::BackTab => next.move_selection(-1, matches),
        KeyCode::Backspace => {
            next.filter.pop();
            // The list under it just changed, and a selection counted over the
            // old one points at a different template — or at nothing.
            next.selected = 0;
        }
        KeyCode::Char(c) => {
            next.filter.push(c);
            next.selected = 0;
        }
        _ => return Vec::new(),
    }
    vec![ViewCmd::Palette(Some(next)).into()]
}

/// Pick what the palette has selected, or answer the form it turned into.
fn use_template(ctx: &InputContext<'_>) -> Vec<Intent> {
    let Some(open) = ctx.palette else {
        return Vec::new();
    };
    if open.asking.is_some() {
        return vec![ViewCmd::SubmitTemplate.into()];
    }
    let Some(held) = templates(ctx) else {
        return Vec::new();
    };
    open.picked(held)
        .map(|template| vec![ViewCmd::UseTemplate(template.id).into()])
        .unwrap_or_default()
}

/// Name the statement in the buffer, or save it under the name that is typed.
///
/// Nothing at all when there is nothing to save: a key that opened an empty
/// "save as" over an empty buffer would be one that always appears to work.
/// The run the history's grid has selected, when that is what is in front.
fn selected_run<'a>(ctx: &InputContext<'a>) -> Option<&'a HistoryEntry> {
    // Only from the history tab: the same selection on a preview is a cell of
    // somebody's data, and a key that read it as a statement would be reading
    // the wrong list.
    ctx.searching()?;
    let (row, ..) = ctx.selection?;
    ctx.snapshot.history.data.ready()?.get(row)
}

fn save_template(ctx: &InputContext<'_>) -> Vec<Intent> {
    match ctx.palette.and_then(|open| open.saving_body()) {
        Some(body) => {
            let name = ctx.palette.map(|open| open.filter.trim()).unwrap_or("");
            if name.is_empty() {
                return Vec::new();
            }
            // The view holds the name and the body already, so it does the
            // building — and closes itself, which is a view's own business.
            let _ = body;
            vec![ViewCmd::CommitTemplate.into()]
        }
        // Not while the palette is already up for something else: `Ctrl-s`
        // there would replace what is on screen with a different errand.
        None if ctx.palette.is_some() => Vec::new(),
        // What is in front: a SQL tab's buffer, or the run the history has
        // selected. One key and one dialog either way — a second way to name a
        // statement would be a second dialog to keep in step, and the one that
        // never learns from the other's mistakes.
        None => ctx
            .active_tab
            .and_then(|id| ctx.tabs.iter().find(|t| t.id == id))
            .and_then(|tab| tab.sql())
            .map(str::to_owned)
            .or_else(|| selected_run(ctx).map(|run| run.sql.clone()))
            .filter(|sql| !sql.trim().is_empty())
            .map(|sql| vec![ViewCmd::Palette(Some(crate::palette::Palette::saving(sql))).into()])
            .unwrap_or_default(),
    }
}

/// Ask before deleting the selected template.
///
/// Through a dialog rather than straight away: a saved statement is somebody's
/// own writing and there is no undo here, so the one thing this must not be is
/// a key that quietly loses it.
fn delete_template(ctx: &InputContext<'_>) -> Vec<Intent> {
    let (Some(open), Some(held)) = (ctx.palette, templates(ctx)) else {
        return Vec::new();
    };
    if open.asking.is_some() || open.saving_body().is_some() {
        return Vec::new();
    }
    open.picked(held)
        .map(|template| {
            vec![
                ViewCmd::ConfirmDeleteTemplate {
                    id: template.id,
                    name: template.name.clone(),
                }
                .into(),
            ]
        })
        .unwrap_or_default()
}

fn templates<'a>(ctx: &InputContext<'a>) -> Option<&'a [Template]> {
    ctx.snapshot
        .templates
        .data
        .ready()
        .map(|held| held.as_slice())
}

/// What the filter box holds after this key.
///
/// `Esc` closes it and `Enter` leaves it closed with the tree whole again:
/// both are ways of saying "done", and a filter that outlived its box would
/// hide rows with nothing on screen to explain why. `Backspace` on an empty
/// box closes it too, which is where the user's fingers already are.
fn next_filter(event: KeyEvent, current: Option<&Filter>) -> Option<Filter> {
    let Some(current) = current else {
        // No search yet, so this is the `/` that starts one — and `/` is a
        // character the box would otherwise have taken as its first letter.
        return Some(Filter::opening());
    };
    if !current.editing {
        // `/` again on a search already made: re-open the box on what is in
        // it, rather than throwing the query away to retype it.
        return Some(Filter {
            editing: true,
            ..current.clone()
        });
    }

    let typed = |text: String| {
        Some(Filter {
            text,
            editing: true,
        })
    };
    match event.code {
        // Done typing, but not done searching. The box stays on screen with
        // the query in it and the *tree* takes the keyboard, which is the only
        // way a keyboard reaches what was searched for: `Esc` is the way to
        // abandon a search, and if `Enter` did that too there would be none.
        KeyCode::Enter if !current.text.is_empty() => Some(Filter {
            editing: false,
            ..current.clone()
        }),
        // An empty box has nothing to hand over.
        KeyCode::Esc | KeyCode::Enter => None,
        KeyCode::Backspace => {
            let mut text = current.text.clone();
            // On an empty box there is nothing to delete, and abandoning the
            // search is where the user's fingers already are.
            text.pop()?;
            typed(text)
        }
        KeyCode::Char(c) => typed(format!("{}{c}", current.text)),
        _ => Some(current.clone()),
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
        /// A dialog with an answer on it, for the sweep.
        asking: crate::overlay::Modal,
        snapshot: Snapshot,
        conn: ConnId,
        tabs: Vec<OpenTab>,
        toasts: Vec<Toast>,
        /// The two states a search can be in, held here so that a context
        /// borrowing one lives as long as the fixture does.
        searches: [Filter; 2],
        /// And the two the palette can be in — a list, and a form over it.
        palettes: [crate::palette::Palette; 2],
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
                palette: None,
                snapshot: &self.snapshot,
                focus,
                modal_open: false,
                modal: None,
                connection: self.snapshot.connections.first().map(|c| c.id),
                tree_selection: Some(0),
                grid_column: Some(2),
                pointer: (0, 0),
                selection: None,
                menu: None,
                tabs: &self.tabs,
                active_tab: None,
                toasts: &self.toasts,
                filter: None,
            }
        }
    }

    /// A search that is being typed.
    fn editing(text: &str) -> Filter {
        Filter {
            text: text.to_owned(),
            editing: true,
        }
    }

    /// A search that has been made, with the keyboard back on the tree.
    fn made(text: &str) -> Filter {
        Filter {
            text: text.to_owned(),
            editing: false,
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
                content: TabContent::Preview(TableRef::new(["public", "users"])),
            },
            OpenTab {
                id: TabId::new(2),
                conn,
                content: TabContent::Preview(TableRef::new(["public", "empty"])),
            },
            // The history, so the sweep reaches the bindings that only fire
            // there — and, just as much, so it sees the ones that must not.
            OpenTab {
                id: TabId::new(4),
                conn,
                content: TabContent::History {
                    filter: Filter {
                        text: String::new(),
                        // Not editing: while it is, every letter is the box's
                        // and none of the grid's bindings can be reached.
                        editing: false,
                    },
                },
            },
            // A SQL tab, so the sweep reaches the bindings that only fire on
            // one — and, just as much, so it sees the ones that must *not*
            // fire on one.
            OpenTab {
                id: TabId::new(3),
                conn,
                content: TabContent::Sql {
                    number: 1,
                    text: "select 1".to_owned(),
                    query: None,
                },
            },
        ];

        let snapshot = Snapshot {
            rev: 1,
            applied: 0,
            profiles: Arc::new(vec![mock_summary("mock")]),
            connections: vec![ConnectionView {
                id: conn,
                profile: mock_summary("mock").id,
                name: "mock".into(),
                color: None,
                kind: DriverKind::Mock,
                status: ConnStatus::Ready,
                capabilities: Some(CAPABILITIES),
                tree: std::sync::Arc::default(),
            }],
            explorer,
            definitions: Vec::new(),
            previews: tabs
                .iter()
                // A SQL tab has no relation, so the store holds nothing for
                // it — which is the shape being pinned down here.
                .filter_map(|t| Some((t.conn, t.table()?.clone())))
                .map(|(conn, table)| PreviewView {
                    exhausted: false,
                    attempts: 0,
                    conn,
                    table,
                    sort: None,
                    loaded_rows: 0,
                    data: LoadState::Idle,
                    last_error: None,
                })
                .collect(),
            queries: Vec::new(),
            busy: vec![BusyItem {
                id: BusyId::new(1),
                owner: BusyOwner::Preview {
                    conn,
                    table: TableRef::new(["public", "users"]),
                },
                label: "loading".into(),
                started_at: std::time::Instant::now(),
            }],
            // Something saved, so the palette's own bindings reach a
            // template rather than an empty list — a sweep over an empty
            // palette would report `Enter` dead.
            // A run, so a binding that acts on the selected one reaches
            // something.
            history: sqlake_app::snapshot::HistoryView {
                terms: String::new(),
                data: sqlake_app::snapshot::LoadState::Ready(Arc::new(vec![
                    sqlake_core::library::HistoryEntry {
                        id: sqlake_core::library::RunId::new(1),
                        connection: conn.to_string(),
                        driver: Some(DriverKind::Mock),
                        sql: "select * from public.users".to_owned(),
                        started_at: time::OffsetDateTime::UNIX_EPOCH,
                        status: Some("ok".to_owned()),
                        duration_ms: Some(12),
                        row_count: Some(3),
                        bytes_processed: None,
                        error: None,
                    },
                ])),
            },
            templates: sqlake_app::snapshot::TemplatesView {
                data: sqlake_app::snapshot::LoadState::Ready(Arc::new(vec![
                    sqlake_core::library::Template {
                        id: sqlake_core::library::TemplateId::new(1),
                        name: "daily".to_owned(),
                        body: "select * from {{ident:table}}".to_owned(),
                        driver: None,
                        tags: Vec::new(),
                        created_at: time::OffsetDateTime::UNIX_EPOCH,
                        updated_at: time::OffsetDateTime::UNIX_EPOCH,
                    },
                ])),
                failed: None,
            },
            should_quit: false,
        };

        let toasts = vec![Toast {
            id: ToastId::new(1),
            text: "oops".into(),
            severity: Severity::Error,
            created_at: std::time::Instant::now(),
        }];

        Fixture {
            asking: crate::overlay::Modal::asking(
                "This query costs more than the limit",
                "It would read a lot.",
                vec![crate::overlay::Choice {
                    label: "Run it anyway".to_owned(),
                    intent: Action::ApproveQuery(QueryId::new()).into(),
                }],
            ),
            snapshot,
            conn,
            tabs,
            toasts,
            searches: [editing(""), made("public")],
            palettes: [
                crate::palette::Palette::opening(),
                crate::palette::Palette {
                    asking: Some(crate::palette::Form::new(
                        sqlake_core::library::TemplateId::new(1),
                        "daily".to_owned(),
                        vec![sqlake_core::template::Placeholder {
                            name: "table".to_owned(),
                            kind: sqlake_core::template::Kind::Ident,
                        }],
                        &|_| None,
                    )),
                    ..crate::palette::Palette::opening()
                },
            ],
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
    fn a_drag_that_began_elsewhere_does_not_select_the_cells_it_crosses() {
        // The splitter's grab area is three cells wide and the whole height of
        // the screen, so moving it sweeps the pointer across the grid beside
        // it. Read as a selection, resizing the panes drags the highlight over
        // everything on the way.
        let f = fixture();
        let c = f.ctx(PaneId::Grid);
        for from in [
            Target::Splitter(SplitId::Explorer),
            Target::GridColEdge { col: 0 },
            Target::Scrollbar {
                pane: PaneId::Grid,
                part: ScrollPart::Thumb,
            },
        ] {
            assert!(
                on_mouse(
                    Target::GridCell { row: 7, col: 2 },
                    Gesture::DragOver { from },
                    &c,
                )
                .is_empty(),
                "{from:?}"
            );
        }
        assert_eq!(
            on_mouse(
                Target::GridCell { row: 7, col: 2 },
                Gesture::DragOver {
                    from: Target::GridCell { row: 3, col: 1 }
                },
                &c,
            ),
            [Intent::View(ViewCmd::ExtendCellSelectionTo {
                row: 7,
                col: 2
            })]
        );
    }

    #[test]
    fn a_press_on_a_cell_is_where_a_drag_selection_starts() {
        // The anchor is the cursor, and the first motion event is already too
        // late to put it under the press: it would stretch the rectangle from
        // whatever was selected before.
        let f = fixture();
        assert_eq!(
            on_mouse(
                Target::GridCell { row: 9, col: 3 },
                Gesture::Down,
                &f.ctx(PaneId::Explorer),
            ),
            [
                Intent::View(ViewCmd::FocusPane(PaneId::Grid)),
                Intent::View(ViewCmd::SelectCell { row: 9, col: 3 }),
            ]
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
    fn typing_in_the_history_searches_the_history_and_not_the_explorer() {
        // One capability, two boxes. A key that searched the explorer from
        // inside the history would be searching what nobody is looking at.
        let mut f = fixture();
        let history = TabId::new(60);
        f.tabs.push(OpenTab {
            id: history,
            conn: f.conn,
            content: TabContent::History {
                filter: Filter::opening(),
            },
        });
        let mut c = f.ctx(PaneId::Grid);
        c.active_tab = Some(history);

        let out = on_key(press(KeyCode::Char('o')), &c);
        assert!(
            matches!(
                out.as_slice(),
                [Intent::View(ViewCmd::SetHistoryTerms(Some(box_)))] if box_.text == "o"
            ),
            "{out:?}"
        );

        // And with a tab that is not the history in front, the same key is the
        // explorer's.
        let mut elsewhere = f.ctx(PaneId::Grid);
        elsewhere.active_tab = Some(TabId::new(1));
        elsewhere.filter = Some(&f.searches[0]);
        assert!(matches!(
            on_key(press(KeyCode::Char('o')), &elsewhere).as_slice(),
            [Intent::View(ViewCmd::SetFilter(_))]
        ));
    }

    #[test]
    fn a_run_is_put_back_from_the_history_and_from_nowhere_else() {
        let f = fixture();
        let mut history = f.ctx(PaneId::Grid);
        history.active_tab = Some(TabId::new(4));
        history.selection = Some((0, 0, 0, 0));
        assert_eq!(
            on_key(press(KeyCode::Char('u')), &history),
            [Intent::View(ViewCmd::ReuseRun(
                sqlake_core::library::RunId::new(1)
            ))]
        );

        // The same selection on a preview is a cell of somebody's data, and a
        // key that read it as a statement would be reading the wrong list.
        let mut preview = f.ctx(PaneId::Grid);
        preview.active_tab = Some(TabId::new(1));
        preview.selection = Some((0, 0, 0, 0));
        assert!(on_key(press(KeyCode::Char('u')), &preview).is_empty());
    }

    #[test]
    fn keeping_a_run_is_the_same_dialog_that_keeps_a_buffer() {
        // One key and one dialog for both — a second way to name a statement
        // would be a second dialog to keep in step.
        let f = fixture();
        let mut history = f.ctx(PaneId::Grid);
        history.active_tab = Some(TabId::new(4));
        history.selection = Some((0, 0, 0, 0));

        let out = on_key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            &history,
        );
        assert!(
            matches!(
                out.as_slice(),
                [Intent::View(ViewCmd::Palette(Some(palette)))]
                    if palette.saving_body() == Some("select * from public.users")
            ),
            "{out:?}"
        );
    }

    #[test]
    fn the_box_in_front_owns_the_keyboard() {
        // The explorer's search can be left open behind the history tab. A
        // keystroke that reached it would be typed into something nobody is
        // looking at, while the box in front sat there apparently ignoring it.
        let mut f = fixture();
        let history = TabId::new(61);
        f.tabs.push(OpenTab {
            id: history,
            conn: f.conn,
            content: TabContent::History {
                filter: Filter {
                    text: String::new(),
                    editing: false,
                },
            },
        });
        let mut c = f.ctx(PaneId::Grid);
        c.active_tab = Some(history);
        // Editing, and behind the history tab.
        c.filter = Some(&f.searches[0]);

        // `q` is the explorer box's letter only while that box has the
        // keyboard. In front of a history tab whose own box is closed, it is
        // the global binding again.
        assert_eq!(
            on_key(press(KeyCode::Char('q')), &c),
            [Intent::App(Action::Quit)]
        );
    }

    #[test]
    fn saving_needs_something_to_save() {
        // A key that opened an empty "save as" over an empty buffer would be
        // one that always looks like it worked.
        let f = fixture();
        let save = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);

        let mut on_a_preview = f.ctx(PaneId::Grid);
        on_a_preview.active_tab = Some(TabId::new(1));
        assert!(on_key(save, &on_a_preview).is_empty());

        let mut on_sql = f.ctx(PaneId::Grid);
        on_sql.active_tab = Some(TabId::new(3));
        let opened = on_key(save, &on_sql);
        assert!(
            matches!(
                opened.as_slice(),
                [Intent::View(ViewCmd::Palette(Some(palette)))] if palette.saving_body() == Some("select 1")
            ),
            "{opened:?}"
        );
    }

    #[test]
    fn deleting_is_only_offered_where_there_is_something_to_delete() {
        let f = fixture();
        let delete = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);

        let mut listing = f.ctx(PaneId::Grid);
        listing.palette = Some(&f.palettes[0]);
        assert!(matches!(
            on_key(delete, &listing).as_slice(),
            [Intent::View(ViewCmd::ConfirmDeleteTemplate { .. })]
        ));

        // Not while a form is up: the row under the cursor is not what is on
        // screen any more.
        let mut asking = f.ctx(PaneId::Grid);
        asking.palette = Some(&f.palettes[1]);
        assert!(on_key(delete, &asking).is_empty());
    }

    #[test]
    fn clicking_outside_the_palette_closes_it() {
        // It draws a backdrop, so a click outside has to mean something. It
        // meant `DismissModal`, which closes a dialog that is not there and
        // leaves the palette exactly where it was.
        let f = fixture();
        let mut c = f.ctx(PaneId::Grid);
        c.palette = Some(&f.palettes[0]);
        assert_eq!(
            on_mouse(Target::Backdrop, Gesture::Click, &c),
            [Intent::View(ViewCmd::Palette(None))]
        );

        // And a dialog over it still wins: it is on top, and it is the one
        // that has to be answered.
        c.modal_open = true;
        assert_eq!(
            on_mouse(Target::Backdrop, Gesture::Click, &c),
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
        let held = editing("us");
        c.filter = Some(&held);

        for (key, expected) in [('q', "usq"), ('s', "uss"), ('/', "us/")] {
            assert_eq!(
                on_key(press(KeyCode::Char(key)), &c),
                [Intent::View(ViewCmd::SetFilter(Some(editing(expected))))],
                "{key} should have gone into the box"
            );
        }
    }

    #[test]
    fn a_chord_is_not_typed_into_the_box() {
        // In raw mode there is no SIGINT, so `Ctrl-C` is the one key that has
        // to work everywhere — and it arrives as `Char('c')` with a modifier,
        // which a box that took every character would swallow as a letter.
        let f = fixture();
        let mut c = f.ctx(PaneId::Explorer);
        let held = editing("us");
        c.filter = Some(&held);
        assert_eq!(
            on_key(press_ctrl(KeyCode::Char('c')), &c),
            [Intent::App(Action::Quit)]
        );
    }

    #[test]
    fn slash_opens_the_box_and_is_not_typed_into_it() {
        let f = fixture();
        let c = f.ctx(PaneId::Explorer);
        assert_eq!(
            on_key(press(KeyCode::Char('/')), &c),
            [Intent::View(ViewCmd::SetFilter(Some(Filter::opening())))]
        );
    }

    #[test]
    fn enter_hands_the_keyboard_to_the_tree_and_keeps_the_search() {
        // design.md §1: nothing is reachable by mouse only. If `Enter` cleared
        // the search like `Esc` does, the rows it found would be gone before
        // anything could be selected, and a table could only be opened by
        // double-clicking it.
        let f = fixture();
        let mut c = f.ctx(PaneId::Explorer);
        let held = editing("users");
        c.filter = Some(&held);
        assert_eq!(
            on_key(press(KeyCode::Enter), &c),
            [Intent::View(ViewCmd::SetFilter(Some(made("users"))))]
        );

        // And with the keyboard back on the tree, the ordinary bindings work
        // again — which is what makes the found row reachable.
        let made = made("users");
        c.filter = Some(&made);
        assert_eq!(
            on_key(press(KeyCode::Down), &c),
            [Intent::View(ViewCmd::MoveTreeSelection(1))]
        );
    }

    #[test]
    fn escape_abandons_the_search_and_slash_resumes_it() {
        let f = fixture();
        let mut c = f.ctx(PaneId::Explorer);
        let held = editing("users");
        c.filter = Some(&held);
        assert_eq!(
            on_key(press(KeyCode::Esc), &c),
            [Intent::View(ViewCmd::SetFilter(None))]
        );

        // `/` on a search already made re-opens the box on what is in it,
        // rather than throwing the query away to be retyped.
        let made = made("users");
        c.filter = Some(&made);
        assert_eq!(
            on_key(press(KeyCode::Char('/')), &c),
            [Intent::View(ViewCmd::SetFilter(Some(editing("users"))))]
        );
    }

    #[test]
    fn backspace_empties_the_box_and_then_abandons_the_search() {
        let f = fixture();
        let mut c = f.ctx(PaneId::Explorer);
        let held = editing("us");
        c.filter = Some(&held);
        assert_eq!(
            on_key(press(KeyCode::Backspace), &c),
            [Intent::View(ViewCmd::SetFilter(Some(editing("u"))))]
        );
        // On an empty box there is nothing to delete, and abandoning is where
        // the user's fingers already are.
        let empty = editing("");
        c.filter = Some(&empty);
        assert_eq!(
            on_key(press(KeyCode::Backspace), &c),
            [Intent::View(ViewCmd::SetFilter(None))]
        );
    }

    #[test]
    fn clicking_the_box_puts_the_keyboard_in_it() {
        // What clicking a text box means everywhere else. Discarding the
        // search instead is the surprise, and there is no `×` on the box to
        // say that is what the click would do.
        let f = fixture();
        let mut c = f.ctx(PaneId::Explorer);
        let made = made("users");
        c.filter = Some(&made);
        assert_eq!(
            on_mouse(Target::Button(ButtonId::Filter), Gesture::Click, &c),
            [Intent::View(ViewCmd::SetFilter(Some(editing("users"))))]
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
        let made = made("public");
        c.filter = Some(&made);
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
        // How a terminal actually sends it: `BackTab` *is* `Shift-Tab`, and
        // crossterm hands it over carrying the modifier. Read as a modifier
        // that has to be asked for, the binding matches nothing.
        assert_eq!(
            on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), &c),
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
    fn d_opens_a_definition_of_the_selected_relation() {
        // Both halves, the way `Enter` opens a preview: the tab and the fetch.
        let f = fixture();
        let mut c = f.ctx(PaneId::Explorer);
        // Row one: the fixture's tree is `public` and `users` under it.
        c.tree_selection = Some(1);
        let intents = on_key(press(KeyCode::Char('d')), &c);
        assert!(
            intents
                .iter()
                .any(|i| matches!(i, Intent::View(ViewCmd::OpenDefinition { .. }))),
            "{intents:?}"
        );
        assert!(
            intents
                .iter()
                .any(|i| matches!(i, Intent::App(Action::DescribeTable { refresh: false, .. }))),
            "{intents:?}"
        );
    }

    #[test]
    fn a_namespace_has_no_definition_to_open() {
        // Most of the tree is not a relation, and a message about that on
        // every stray keypress would be noise.
        let f = fixture();
        let mut c = f.ctx(PaneId::Explorer);
        c.tree_selection = Some(0);
        assert!(on_key(press(KeyCode::Char('d')), &c).is_empty());
    }

    #[test]
    fn closing_a_definition_forgets_it() {
        // Nothing else ever will: a definition is cached until something says
        // otherwise, unlike a preview, which every page request refreshes.
        let mut f = fixture();
        let table = TableRef::new(["public", "users"]);
        f.tabs.push(OpenTab {
            id: TabId::new(70),
            conn: f.conn,
            content: TabContent::Definition {
                table: table.clone(),
                section: 0,
            },
        });
        let intents = on_mouse(
            Target::TabClose(TabId::new(70)),
            Gesture::Click,
            &f.ctx(PaneId::Grid),
        );
        assert!(
            intents.contains(&Intent::App(Action::ForgetDefinition {
                conn: f.conn,
                table,
            })),
            "{intents:?}"
        );
        // And not a `ForgetPreview`: a definition tab never had one.
        assert!(
            !intents
                .iter()
                .any(|i| matches!(i, Intent::App(Action::ForgetPreview { .. }))),
            "{intents:?}"
        );
    }

    #[test]
    fn a_section_is_pickable_by_click_and_by_key() {
        let f = fixture();
        let c = f.ctx(PaneId::Grid);
        assert_eq!(
            on_mouse(Target::Section { index: 2 }, Gesture::Click, &c),
            [Intent::View(ViewCmd::SelectSection(
                crate::intent::SectionPick::At(2)
            ))]
        );
        assert_eq!(
            on_key(press(KeyCode::Char('}')), &c),
            [Intent::View(ViewCmd::SelectSection(
                crate::intent::SectionPick::By(1)
            ))]
        );
        assert_eq!(
            on_key(press(KeyCode::Char('{')), &c),
            [Intent::View(ViewCmd::SelectSection(
                crate::intent::SectionPick::By(-1)
            ))]
        );
    }

    #[test]
    fn r_runs_a_sql_tab_and_leaves_a_preview_alone() {
        let f = fixture();
        let mut c = f.ctx(PaneId::Grid);
        // A preview has no buffer, and running its rows is not a thing to do.
        assert!(on_key(press(KeyCode::Char('r')), &c).is_empty());

        c.active_tab = Some(TabId::new(3));
        let intents = on_key(press(KeyCode::Char('r')), &c);
        assert!(
            matches!(
                intents.as_slice(),
                [Intent::App(Action::RunQuery { conn, sql, .. })]
                    if *conn == f.conn && sql == "select 1"
            ),
            "{intents:?}"
        );
    }

    #[test]
    fn an_empty_buffer_runs_nothing() {
        // Running nothing produces a statement the server refuses, reported as
        // an error, for a gesture that should have done nothing at all.
        let mut f = fixture();
        f.tabs.push(OpenTab {
            id: TabId::new(60),
            conn: f.conn,
            content: TabContent::Sql {
                number: 2,
                text: "   \n ".to_owned(),
                query: None,
            },
        });
        let mut c = f.ctx(PaneId::Grid);
        c.active_tab = Some(TabId::new(60));
        assert!(on_key(press(KeyCode::Char('r')), &c).is_empty());
        assert!(on_mouse(Target::Button(ButtonId::RunQuery), Gesture::Click, &c).is_empty());
    }

    #[test]
    fn the_run_button_and_its_key_do_the_same_thing() {
        let f = fixture();
        let mut c = f.ctx(PaneId::Grid);
        c.active_tab = Some(TabId::new(3));
        let by_key = on_key(press(KeyCode::Char('r')), &c);
        let by_click = on_mouse(Target::Button(ButtonId::RunQuery), Gesture::Click, &c);
        // Not equal: each mints its own `QueryId`, which is the point of the
        // id being the caller's. The statement and the connection are what
        // have to agree.
        let sql_of = |intents: &[Intent]| match intents {
            [Intent::App(Action::RunQuery { conn, sql, .. })] => Some((*conn, sql.clone())),
            _ => None,
        };
        assert_eq!(sql_of(&by_key), sql_of(&by_click));
        assert!(sql_of(&by_key).is_some(), "{by_key:?}");
    }

    #[test]
    fn a_dialog_is_answered_by_its_own_first_choice() {
        // The key and the button cannot disagree about what "yes" means,
        // because both read it off the dialog.
        let f = fixture();
        let mut c = f.ctx(PaneId::Grid);
        c.modal_open = true;
        c.modal = Some(&f.asking);

        let expected = vec![f.asking.choices[0].intent.clone()];
        assert_eq!(on_key(press(KeyCode::Enter), &c), expected);
        assert_eq!(
            on_mouse(
                Target::Button(ButtonId::ModalChoice { index: 0 }),
                Gesture::Click,
                &c
            ),
            expected
        );
    }

    #[test]
    fn closing_a_sql_tab_forgets_the_run_it_started() {
        let mut f = fixture();
        let query = QueryId::new();
        f.tabs.push(OpenTab {
            id: TabId::new(61),
            conn: f.conn,
            content: TabContent::Sql {
                number: 2,
                text: "select 1".to_owned(),
                query: Some(query),
            },
        });
        let intents = on_mouse(
            Target::TabClose(TabId::new(61)),
            Gesture::Click,
            &f.ctx(PaneId::Grid),
        );
        assert!(
            intents.contains(&Intent::App(Action::ForgetQuery(query))),
            "{intents:?}"
        );
    }

    #[test]
    fn e_edits_a_sql_tab_and_leaves_a_preview_alone() {
        let f = fixture();
        let mut c = f.ctx(PaneId::Grid);
        // The fixture's first tab is a preview: nothing to edit there, and
        // opening an editor on the rows is not a thing to do to them.
        assert!(on_key(press(KeyCode::Char('e')), &c).is_empty());

        c.active_tab = Some(TabId::new(3));
        assert_eq!(
            on_key(press(KeyCode::Char('e')), &c),
            [Intent::Handover(Handover::Edit(TabId::new(3)))]
        );
    }

    #[test]
    fn double_clicking_the_buffer_does_what_e_does() {
        // A SQL tab draws no cells, so the pane itself is what a pointer
        // lands on.
        let f = fixture();
        let mut c = f.ctx(PaneId::Grid);
        c.active_tab = Some(TabId::new(3));
        assert_eq!(
            on_mouse(Target::Pane(PaneId::Grid), Gesture::DoubleClick, &c),
            [Intent::Handover(Handover::Edit(TabId::new(3)))]
        );
        c.active_tab = Some(TabId::new(1));
        assert!(on_mouse(Target::Pane(PaneId::Grid), Gesture::DoubleClick, &c).is_empty());
    }

    #[test]
    fn closing_a_sql_tab_forgets_nothing() {
        // A `ForgetPreview` here would drop the cache of whatever relation
        // happened to be reachable, on a tab that never had one.
        let mut f = fixture();
        let sql = TabId::new(50);
        f.tabs.push(OpenTab {
            id: sql,
            conn: f.conn,
            content: TabContent::Sql {
                number: 1,
                text: String::new(),
                query: None,
            },
        });
        assert_eq!(
            on_mouse(Target::TabClose(sql), Gesture::Click, &f.ctx(PaneId::Grid)),
            [Intent::View(ViewCmd::CloseTab(sql))]
        );
    }

    #[test]
    fn a_sql_tab_sorts_and_pages_nothing() {
        // Both reach for the active tab's relation. Without the `?` they act
        // on whichever relation another tab left in the context.
        let mut f = fixture();
        let sql = TabId::new(50);
        f.tabs.push(OpenTab {
            id: sql,
            conn: f.conn,
            content: TabContent::Sql {
                number: 1,
                text: String::new(),
                query: None,
            },
        });
        let mut c = f.ctx(PaneId::Grid);
        c.active_tab = Some(sql);
        assert!(on_key(press(KeyCode::Char('s')), &c).is_empty());
        assert!(on_key(press(KeyCode::Char('m')), &c).is_empty());
    }

    #[test]
    fn the_new_tab_button_and_its_key_do_the_same_thing() {
        let f = fixture();
        let c = f.ctx(PaneId::Grid);
        let expected: Vec<Intent> = vec![ViewCmd::OpenSqlTab { conn: f.conn }.into()];
        assert_eq!(on_key(press(KeyCode::Char('n')), &c), expected);
        assert_eq!(
            on_mouse(Target::Button(ButtonId::NewSqlTab), Gesture::Click, &c),
            expected
        );
    }

    #[test]
    fn a_sql_tab_needs_somewhere_to_run() {
        // With no connection there is nowhere to send a query, and a tab that
        // could never run one is a tab that lies.
        let mut f = fixture();
        f.snapshot.connections.clear();
        let mut c = f.ctx(PaneId::Grid);
        c.connection = None;
        assert!(on_key(press(KeyCode::Char('n')), &c).is_empty());
        assert!(on_mouse(Target::Button(ButtonId::NewSqlTab), Gesture::Click, &c).is_empty());
    }

    #[test]
    fn a_closed_connection_is_not_somewhere_to_run_either() {
        // A closed connection keeps its row so the user can see what happened
        // to it. Opening a SQL tab on one mints a tab that
        // `close_disconnected_tabs` takes away on the next frame, which is a
        // `+` that answers a click with nothing.
        let mut f = fixture();
        f.snapshot.connections[0].status = ConnStatus::Closed;
        let c = f.ctx(PaneId::Grid);
        assert!(on_key(press(KeyCode::Char('n')), &c).is_empty());
        assert!(on_mouse(Target::Button(ButtonId::NewSqlTab), Gesture::Click, &c).is_empty());
    }

    #[test]
    fn a_new_sql_tab_skips_a_dead_connection_for_a_live_one() {
        // The selected row's connection is the one to use, but only while it
        // is one: with a closed connection selected and a live one open, `n`
        // opens on the live one rather than on nothing.
        let mut f = fixture();
        let live = ConnId::new();
        f.snapshot.connections[0].status = ConnStatus::Closed;
        let mut second = f.snapshot.connections[0].clone();
        second.id = live;
        second.status = ConnStatus::Ready;
        f.snapshot.connections.push(second);
        let c = f.ctx(PaneId::Grid);
        assert_eq!(
            on_key(press(KeyCode::Char('n')), &c),
            [Intent::View(ViewCmd::OpenSqlTab { conn: live })]
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
            content: TabContent::Preview(TableRef::new(["public", "users"])),
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
        // With only two tabs open both directions gave the same answer, so
        // this could not tell them apart. The third makes it a test.
        assert_eq!(
            on_key(press(KeyCode::Char('[')), &c),
            [Intent::View(ViewCmd::SelectTab(TabId::new(3)))],
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
            palette: None,
            snapshot: &empty,
            focus: PaneId::Grid,
            modal_open: false,
            modal: None,
            connection: None,
            tree_selection: None,
            grid_column: None,
            pointer: (0, 0),
            selection: None,
            menu: None,
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
        assert!(
            matches!(
                &out[..],
                [Intent::App(Action::Connect { profile, .. })]
                    if *profile == mock_summary("staging").id
            ),
            "{out:?}"
        );

        // Still opening counts as open. Otherwise a second press while the
        // first connection is still on its way opens a duplicate of it rather
        // than moving on to the profile that has nothing.
        f.snapshot.connections[0].status = ConnStatus::Connecting;
        let out = on_key(press(KeyCode::Char('c')), &f.ctx(PaneId::Explorer));
        assert!(
            matches!(
                &out[..],
                [Intent::App(Action::Connect { profile, .. })]
                    if *profile == mock_summary("staging").id
            ),
            "{out:?}"
        );

        // Closing a connection leaves its row behind, and a row is not a
        // connection: `c` has to be able to open `replica` again rather than
        // skipping past it for ever.
        f.snapshot.connections[0].status = ConnStatus::Closed;
        let out = on_key(press(KeyCode::Char('c')), &f.ctx(PaneId::Explorer));
        assert!(
            matches!(
                &out[..],
                [Intent::App(Action::Connect { profile, .. })]
                    if *profile == mock_summary("replica").id
            ),
            "{out:?}"
        );

        // The same profile twice is a second window onto one database, so the
        // key never goes dead once everything is open.
        f.snapshot.connections[0].status = ConnStatus::Ready;
        f.snapshot.profiles = Arc::new(vec![mock_summary("replica")]);
        let out = on_key(press(KeyCode::Char('c')), &f.ctx(PaneId::Explorer));
        assert!(
            matches!(
                &out[..],
                [Intent::App(Action::Connect { profile, .. })]
                    if *profile == mock_summary("replica").id
            ),
            "{out:?}"
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
            Target::Button(ButtonId::NewSqlTab),
            Target::Button(ButtonId::RunQuery),
            Target::Button(ButtonId::ModalChoice { index: 0 }),
        ],
        Target::Section { .. } => [Target::Section { index: 0 }],
        Target::PaletteRow { .. } => [Target::PaletteRow { index: 0 }],
        Target::Toast(_) => [Target::Toast(ToastId::new(1))],
        Target::MenuItem { .. } => [Target::MenuItem { index: 0 }],
        Target::Menu => [Target::Menu],
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
        Gesture::DragOver { .. } => [
            Gesture::DragOver { from: Target::GridCell { row: 0, col: 0 } },
            Gesture::DragOver { from: Target::Splitter(SplitId::Explorer) },
        ],
        Gesture::Scroll(_) => [Gesture::Scroll(1), Gesture::Scroll(-1)],
        Gesture::ScrollX(_) => [Gesture::ScrollX(1), Gesture::ScrollX(-1)],
        Gesture::HoverEnter => [Gesture::HoverEnter],
        Gesture::HoverLeave => [Gesture::HoverLeave],
    }

    /// The menu's own half of "nothing is mouse-only".
    ///
    /// Asserted here as well as through the sweep, because the sweep proves a
    /// *reachable* kind is bound and this proves every entry is one of them —
    /// an entry the sweep's contexts happened not to resolve would otherwise
    /// slip through.
    #[test]
    fn choosing_an_entry_does_what_the_entry_says() {
        let f = fixture();
        let menu = crate::menu::Menu::for_grid((0, 0), true);
        let mut c = f.ctx(PaneId::Grid);
        c.menu = Some(&menu);

        for (index, entry) in menu.entries.iter().enumerate() {
            let intents = on_mouse(Target::MenuItem { index }, Gesture::Click, &c);
            assert!(
                intents.contains(&entry.intent),
                "line {index} (`{}`) produced {intents:?}",
                entry.label
            );
            assert!(
                intents.contains(&ViewCmd::CloseMenu.into()),
                "the menu stayed open after a choice"
            );
        }
    }

    #[test]
    fn a_click_outside_the_menu_closes_it_and_still_lands() {
        // A menu that had to be dismissed before anything else worked would be
        // a mode, and this one is not.
        let f = fixture();
        let menu = crate::menu::Menu::for_grid((0, 0), true);
        let mut c = f.ctx(PaneId::Grid);
        c.menu = Some(&menu);

        let intents = on_mouse(Target::GridCell { row: 2, col: 1 }, Gesture::Click, &c);
        assert!(intents.contains(&ViewCmd::CloseMenu.into()), "{intents:?}");
        assert!(
            intents.contains(&ViewCmd::SelectCell { row: 2, col: 1 }.into()),
            "the click that closed the menu was swallowed: {intents:?}"
        );
    }

    #[test]
    fn a_right_click_on_a_cell_opens_the_menu() {
        let f = fixture();
        let intents = on_mouse(
            Target::GridCell { row: 0, col: 0 },
            Gesture::RightClick,
            &f.ctx(PaneId::Grid),
        );
        assert!(
            intents
                .iter()
                .any(|i| matches!(i, Intent::View(ViewCmd::OpenMenu { .. }))),
            "{intents:?}"
        );
    }

    #[test]
    fn the_key_that_opens_the_menu_closes_it_again() {
        // The only way out on the keyboard. `Esc` in the grid is
        // `DismissToast`, so without this the one gesture that reaches the menu
        // on a terminal with no right-click leaves it on screen for good.
        let f = fixture();
        let menu = crate::menu::Menu::for_grid((0, 0), false);
        let mut c = f.ctx(PaneId::Grid);
        assert!(matches!(
            on_key(press(KeyCode::Char('.')), &c)[..],
            [Intent::View(ViewCmd::OpenMenu { .. })]
        ));
        c.menu = Some(&menu);
        assert_eq!(
            on_key(press(KeyCode::Char('.')), &c),
            [ViewCmd::CloseMenu.into()]
        );
    }

    #[test]
    fn a_right_click_outside_the_selection_moves_it_there_first() {
        // Otherwise the menu's "Copy cell" copies a cell somewhere else on
        // screen — the one that happened to be selected before.
        let f = fixture();
        let mut c = f.ctx(PaneId::Grid);
        c.selection = Some((0, 0, 2, 2));

        let intents = on_mouse(Target::GridCell { row: 9, col: 4 }, Gesture::RightClick, &c);
        assert!(
            intents.contains(&ViewCmd::SelectCell { row: 9, col: 4 }.into()),
            "{intents:?}"
        );
        assert!(
            intents
                .iter()
                .any(|i| matches!(i, Intent::View(ViewCmd::OpenMenu { ranged: false, .. }))),
            "a click outside the selection still called the menu after it: {intents:?}"
        );
    }

    #[test]
    fn a_right_click_inside_the_selection_keeps_it() {
        let f = fixture();
        let mut c = f.ctx(PaneId::Grid);
        c.selection = Some((0, 0, 2, 2));

        let intents = on_mouse(Target::GridCell { row: 1, col: 1 }, Gesture::RightClick, &c);
        assert!(
            !intents
                .iter()
                .any(|i| matches!(i, Intent::View(ViewCmd::SelectCell { .. }))),
            "right-clicking inside the selection collapsed it: {intents:?}"
        );
        assert!(
            intents
                .iter()
                .any(|i| matches!(i, Intent::View(ViewCmd::OpenMenu { ranged: true, .. }))),
            "{intents:?}"
        );
    }

    #[test]
    fn the_keyboard_menu_is_named_after_the_selection_too() {
        // Not hard-coded to one cell: `.` with a rectangle selected would
        // otherwise offer to "copy cell" and copy the whole rectangle.
        let f = fixture();
        let mut c = f.ctx(PaneId::Grid);
        c.selection = Some((0, 0, 2, 2));
        assert!(matches!(
            on_key(press(KeyCode::Char('.')), &c)[..],
            [Intent::View(ViewCmd::OpenMenu { ranged: true, .. })]
        ));
    }

    #[test]
    fn a_press_on_the_grid_closes_the_menu_before_it_drags() {
        // The press is where a drag-selection starts, so a menu that waited for
        // the click would have a rectangle swept out underneath it.
        let f = fixture();
        let menu = crate::menu::Menu::for_grid((0, 0), true);
        let mut c = f.ctx(PaneId::Grid);
        c.menu = Some(&menu);

        let intents = on_mouse(Target::GridCell { row: 2, col: 1 }, Gesture::Down, &c);
        assert!(intents.contains(&ViewCmd::CloseMenu.into()), "{intents:?}");
    }

    #[test]
    fn the_menu_swallows_what_lands_on_it_rather_than_its_lines() {
        // The border, and any line too far down to be drawn. Without a target
        // of its own the gesture reaches the cell underneath and moves the
        // selection the menu was opened about.
        let f = fixture();
        let menu = crate::menu::Menu::for_grid((0, 0), true);
        let mut c = f.ctx(PaneId::Grid);
        c.menu = Some(&menu);

        for gesture in [Gesture::Down, Gesture::Click] {
            assert_eq!(on_mouse(Target::Menu, gesture, &c), Vec::new());
        }
    }

    #[test]
    fn every_menu_entry_has_a_key_binding() {
        // Bindings that actually carry a key: an entry in `KEYMAP` with none is
        // a kind nothing can press, and counting it would make this pass on a
        // menu whose entries are unreachable.
        let bound: BTreeSet<_> = KEYMAP
            .iter()
            .filter(|b| !b.keys.is_empty())
            .map(|b| b.kind)
            .collect();
        for ranged in [false, true] {
            for entry in crate::menu::Menu::for_grid((0, 0), ranged).entries {
                assert!(
                    bound.contains(&entry.kind()),
                    "the menu offers `{}` and the keyboard cannot reach {:?}",
                    entry.label,
                    entry.kind()
                );
            }
        }
    }

    #[test]
    fn every_capability_reachable_with_the_mouse_has_a_key_binding() {
        // This is the mechanical form of "nothing is mouse-only". The reverse
        // is deliberately not required: keyboard-only capabilities are fine.
        let f = fixture();
        // An open menu, because a `MenuItem` sample resolves to nothing without
        // one: the sweep would walk over every entry and find no intents, and
        // pass while proving nothing about the menu at all.
        let menu = crate::menu::Menu::for_grid((0, 0), true);
        let mut contexts = Vec::new();
        for focus in [PaneId::Explorer, PaneId::Grid] {
            for selection in [Some(0), Some(1)] {
                for open in [None, Some(&menu)] {
                    // A dialog with an answer on it, for the same reason the
                    // menu is opened: a `ModalChoice` resolves to nothing
                    // without one, and the sweep would pass while proving
                    // nothing about the dialog's buttons at all.
                    for asking in [None, Some(&f.asking)] {
                        let mut c = f.ctx(focus);
                        c.tree_selection = selection;
                        c.menu = open;
                        c.modal = asking;
                        c.modal_open = asking.is_some();
                        contexts.push(c);
                    }
                }
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
        // And every entry, not only the first: one sample per `Target` means
        // the sweep only ever clicks line zero.
        for entry in &menu.entries {
            reachable.insert(entry.kind());
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
                    for filter in [None, Some(&f.searches[0]), Some(&f.searches[1])] {
                        // Both kinds of tab: `e` fires only on a SQL one, and
                        // a sweep that never focused one would report it dead.
                        for tab in f.tabs.iter().map(|t| t.id) {
                            let mut c = f.ctx(focus);
                            c.tree_selection = selection;
                            c.modal_open = modal_open;
                            c.filter = filter;
                            c.active_tab = Some(tab);
                            // A selected row, because the history's bindings
                            // act on the run under the cursor and a sweep with
                            // nothing selected would report them dead.
                            c.selection = Some((0, 0, 0, 0));
                            out.push(c);
                            // Both stages of the palette: its bindings only
                            // fire while it is up, and its form answers keys
                            // the list does not.
                            for palette in &f.palettes {
                                let mut open = c;
                                open.palette = Some(palette);
                                out.push(open);
                            }
                            // And with a dialog that has something to answer.
                            // A sweep that only ever opened one which tells
                            // would report every binding on a question dead.
                            if modal_open {
                                let mut asking = c;
                                asking.modal = Some(&f.asking);
                                out.push(asking);
                            }
                        }
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

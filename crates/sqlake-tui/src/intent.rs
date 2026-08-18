//! What an input event means.
//!
//! Mouse and keyboard both produce [`Intent`]s, which is the mechanism behind
//! "nothing is mouse-only". An intent is either handled inside the render loop
//! or dispatched to the store — the split matters because scrolling must not
//! round-trip through an async task.
//!
//! [`IntentKind`] names the *capability* rather than the exact variant, so
//! clicking a specific tree row and moving the selection with the arrow keys
//! are the same kind. That is the level at which "reachable by keyboard" is a
//! meaningful claim.

use sqlake_app::action::Action;
use sqlake_core::id::{ConnId, TabId};
use sqlake_core::node::TableRef;

use crate::hit::{PaneId, SplitId, ToastId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Handled inside the render loop. Never leaves this crate.
    View(ViewCmd),
    /// Dispatched to the store. May perform I/O.
    App(Action),
}

impl From<ViewCmd> for Intent {
    fn from(cmd: ViewCmd) -> Self {
        Self::View(cmd)
    }
}

impl From<Action> for Intent {
    fn from(action: Action) -> Self {
        Self::App(action)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewCmd {
    FocusPane(PaneId),
    FocusNextPane,
    FocusPrevPane,

    /// Positive scrolls towards the end of the content.
    ScrollBy {
        pane: PaneId,
        delta: i32,
    },
    /// Jump to a fraction of the way down, from a click on a scrollbar track.
    ///
    /// Per mille rather than a float so the type stays `Eq` and tests compare
    /// exactly.
    ScrollToRatio {
        pane: PaneId,
        permille: u16,
    },
    ScrollToStart(PaneId),
    ScrollToEnd(PaneId),
    ScrollXBy {
        delta: i32,
    },

    SelectTreeRow(usize),
    MoveTreeSelection(i32),

    /// The explorer's filter, or `None` to close the box and show the tree
    /// whole again.
    ///
    /// The whole string rather than one edit at a time: the key that changed
    /// it is the only thing that knows what it did, and a `Backspace` that
    /// removed nothing is not a state the view should have to reason about.
    SetFilter(Option<crate::ui::Filter>),

    SelectCell {
        row: usize,
        col: usize,
    },
    MoveCellSelection {
        drow: i32,
        dcol: i32,
    },

    ResizeColumn {
        col: usize,
        delta: i16,
    },
    MoveSplit {
        split: SplitId,
        delta: i16,
    },
    EvenSplit(SplitId),

    DismissModal,

    /// A relation now has a tab open for it, focused — raising the existing
    /// one if there is already a tab on that relation.
    OpenTab {
        conn: ConnId,
        table: TableRef,
    },
    SelectTab(TabId),
    CloseTab(TabId),

    DismissToast(ToastId),
}

/// Generates the kind enum and its complete list from one place, so the two
/// cannot drift apart.
macro_rules! intent_kinds {
    ($($name:ident => $description:literal),* $(,)?) => {
        /// A capability, not a variant. Several intents can share a kind when
        /// they are two ways of doing the same thing.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum IntentKind {
            $($name),*
        }

        impl IntentKind {
            pub const ALL: &'static [Self] = &[$(Self::$name),*];

            /// Shown in the help modal, and in the failure message when the
            /// coverage test finds an unbound capability.
            #[must_use]
            pub const fn description(self) -> &'static str {
                match self { $(Self::$name => $description),* }
            }
        }
    };
}

intent_kinds! {
    Focus              => "move focus between panes",
    Scroll             => "scroll the focused pane",
    ScrollEdge         => "jump to the start or end",
    ScrollHorizontally => "scroll the grid sideways",
    TreeSelection      => "select a node in the explorer",
    GridSelection      => "select a cell",
    ResizeColumn       => "change a column's width",
    MoveSplit          => "move the split between panes",
    EvenSplit          => "reset the split",
    DismissModal       => "close the dialog",
    Filter             => "search the explorer",

    Connect            => "open a connection",
    Disconnect         => "close a connection",
    ToggleNode         => "expand or collapse a node",
    PreviewTable       => "open a relation",
    SortPreview        => "sort by a column",
    LoadMore           => "fetch the next page",
    SelectTab          => "switch tabs",
    CloseTab           => "close the tab",
    Cancel             => "stop what is running",
    DismissToast       => "dismiss a message",
    Quit               => "quit",
}

impl IntentKind {
    /// Exhaustive on purpose.
    ///
    /// Adding an [`Intent`] variant stops this compiling, which forces the new
    /// intent to be given a kind — and a kind with no key binding fails the
    /// coverage test in `input`. That chain is the whole mechanism.
    #[must_use]
    pub const fn of(intent: &Intent) -> Self {
        match intent {
            Intent::View(cmd) => match cmd {
                ViewCmd::FocusPane(_) | ViewCmd::FocusNextPane | ViewCmd::FocusPrevPane => {
                    Self::Focus
                }
                ViewCmd::ScrollBy { .. } | ViewCmd::ScrollToRatio { .. } => Self::Scroll,
                ViewCmd::ScrollToStart(_) | ViewCmd::ScrollToEnd(_) => Self::ScrollEdge,
                ViewCmd::ScrollXBy { .. } => Self::ScrollHorizontally,
                ViewCmd::SelectTreeRow(_) | ViewCmd::MoveTreeSelection(_) => Self::TreeSelection,
                ViewCmd::SetFilter(_) => Self::Filter,
                ViewCmd::SelectCell { .. } | ViewCmd::MoveCellSelection { .. } => {
                    Self::GridSelection
                }
                ViewCmd::ResizeColumn { .. } => Self::ResizeColumn,
                ViewCmd::MoveSplit { .. } => Self::MoveSplit,
                ViewCmd::EvenSplit(_) => Self::EvenSplit,
                ViewCmd::DismissModal => Self::DismissModal,
                // Paired with `Action::PreviewTable`: one capability,
                // "open a relation", not two.
                ViewCmd::OpenTab { .. } => Self::PreviewTable,
                ViewCmd::SelectTab(_) => Self::SelectTab,
                ViewCmd::CloseTab(_) => Self::CloseTab,
                ViewCmd::DismissToast(_) => Self::DismissToast,
            },
            Intent::App(action) => match action {
                Action::Connect(_) => Self::Connect,
                Action::Disconnect(_) => Self::Disconnect,
                Action::ToggleNode { .. } => Self::ToggleNode,
                Action::PreviewTable { .. } => Self::PreviewTable,
                Action::SortPreview { .. } => Self::SortPreview,
                Action::LoadMore { .. } => Self::LoadMore,
                // Paired with `ViewCmd::CloseTab`: one capability.
                Action::ForgetPreview { .. } => Self::CloseTab,
                Action::Cancel(_) => Self::Cancel,
                Action::Quit => Self::Quit,
            },
        }
    }
}

/// Where an input arrived, which decides how it is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Context {
    /// Applies wherever focus is.
    Global,
    Explorer,
    Grid,
    /// A dialog is open, which takes over the keyboard.
    Modal,
    /// The explorer's filter box has the keyboard. Like `Modal`, it takes it
    /// over entirely: a search for a table called `q` must not quit.
    Filter,
}

impl Context {
    /// The pane a context corresponds to, for resolving a keystroke against
    /// the current focus.
    #[must_use]
    pub const fn pane(self) -> Option<PaneId> {
        match self {
            Self::Explorer => Some(PaneId::Explorer),
            Self::Grid => Some(PaneId::Grid),
            Self::Global | Self::Modal | Self::Filter => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use sqlake_core::id::ConnId;

    use super::*;

    #[test]
    fn every_kind_has_a_description() {
        for kind in IntentKind::ALL {
            assert!(!kind.description().is_empty(), "{kind:?}");
        }
    }

    #[test]
    fn kinds_are_listed_exactly_once() {
        let unique: BTreeSet<_> = IntentKind::ALL.iter().collect();
        assert_eq!(unique.len(), IntentKind::ALL.len());
    }

    #[test]
    fn two_ways_of_doing_one_thing_share_a_kind() {
        // Clicking a row and arrowing onto it are the same capability. If they
        // were different kinds, "reachable by keyboard" would be a claim about
        // variants rather than about what the user can do.
        assert_eq!(
            IntentKind::of(&ViewCmd::SelectTreeRow(3).into()),
            IntentKind::of(&ViewCmd::MoveTreeSelection(1).into())
        );
        assert_eq!(
            IntentKind::of(&ViewCmd::SelectCell { row: 0, col: 0 }.into()),
            IntentKind::of(&ViewCmd::MoveCellSelection { drow: 1, dcol: 0 }.into())
        );
        assert_eq!(
            IntentKind::of(
                &ViewCmd::ScrollBy {
                    pane: PaneId::Grid,
                    delta: 1
                }
                .into()
            ),
            IntentKind::of(
                &ViewCmd::ScrollToRatio {
                    pane: PaneId::Grid,
                    permille: 500
                }
                .into()
            )
        );
    }

    #[test]
    fn app_intents_map_to_their_own_kinds() {
        assert_eq!(IntentKind::of(&Action::Quit.into()), IntentKind::Quit);
        assert_eq!(
            IntentKind::of(&Action::Disconnect(ConnId::new()).into()),
            IntentKind::Disconnect
        );
    }

    #[test]
    fn only_pane_contexts_name_a_pane() {
        assert_eq!(Context::Explorer.pane(), Some(PaneId::Explorer));
        assert_eq!(Context::Grid.pane(), Some(PaneId::Grid));
        assert_eq!(Context::Global.pane(), None);
        assert_eq!(Context::Modal.pane(), None);
    }
}

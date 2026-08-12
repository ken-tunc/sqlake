//! Application state, intents and use cases. UI-agnostic.
//!
//! This crate owns everything the terminal does not: what is connected, what
//! has been loaded, and what the user asked for. It has no dependency on
//! ratatui, and nothing here knows how wide the terminal is.
//!
//! `Intent` and `ViewCmd` deliberately live in `sqlake-tui` rather than here.
//! Only [`action::Action`] crosses the boundary, so no UI vocabulary — panes,
//! splits, scroll — reaches this crate.

pub mod action;
pub mod grid;
pub mod snapshot;
pub mod tree;

pub use action::{Action, BusyId, ToastId};
pub use grid::{Align, Cell, CellKind, RenderedColumn, RenderedGrid};
pub use snapshot::{
    BusyItem, ConnStatus, ConnectionView, LoadState, PreviewTab, Severity, Snapshot, TabContent,
    TabView, Toast,
};
pub use tree::{NodeState, Toggle, TreeState, TreeView, VisibleNode};

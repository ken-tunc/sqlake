//! Application state, intents and use cases. UI-agnostic.
//!
//! This crate owns everything the terminal does not: what is connected, what
//! has been loaded, and what the user asked for. It has no dependency on
//! ratatui, and **nothing here assumes a character-cell display** — no widths,
//! no glyphs, no truncation. `sqlake-tui` and the agent surface are peers over
//! this layer and want opposite renderings of the same rows.
//!
//! `Intent` and `ViewCmd` deliberately live in `sqlake-tui` rather than here.
//! Only [`action::Action`] crosses the boundary, so no UI vocabulary — panes,
//! splits, scroll, tabs, toasts — reaches this crate. Which previews a screen
//! has open, in what order, and which one has focus is exactly that kind of
//! state: [`snapshot::PreviewView`] is addressed by connection and table, not
//! by a number this crate hands out.

pub mod action;
pub mod error;
pub mod pages;
pub mod session;
pub mod snapshot;
pub mod store;
pub mod tree;
pub mod usecase;

pub use action::{Action, BusyId};
pub use error::{AppError, AppResult};
pub use pages::PagedResult;
pub use session::SessionHandle;
pub use snapshot::{
    BusyItem, ConnStatus, ConnectionView, LoadState, PreviewView, Severity, Snapshot,
};
pub use store::{Drivers, Store};
pub use tree::{NodeState, Toggle, TreeState, TreeView, VisibleNode};
pub use usecase::UseCase;

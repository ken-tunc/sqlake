//! Terminal rendering and input, built on ratatui.
//!
//! The rules this crate exists to keep:
//!
//! - Drawing code never sees a `Value`. Grid content arrives as a
//!   [`grid::RenderedGrid`], which is where every character-cell decision —
//!   widths, glyphs, elision — is made and where they are confined.
//! - It never branches on which driver is connected. Differences are fields on
//!   `Capabilities`.
//! - Mouse and keyboard produce the same [`Intent`], so nothing can become
//!   mouse-only by accident.
//! - Terminal modes are changed in exactly one place, [`terminal`].

pub mod chrome;
pub mod grid;
pub mod hit;
pub mod input;
pub mod intent;
pub mod mouse;
pub mod terminal;
pub mod tree;
pub mod ui;

pub use chrome::{Frames, layout};
pub use grid::{Align, Cell, CellKind, RenderedColumn, RenderedGrid};
pub use hit::{HitMap, PaneId, ScrollPart, SplitId, Target};
pub use input::{InputContext, KEYMAP, on_key, on_mouse};
pub use intent::{Context, Intent, IntentKind, ViewCmd};
pub use mouse::{Gesture, MouseState};
pub use terminal::{TerminalGuard, Tui, install_panic_hook, restore};
pub use ui::{GridUi, TreeUi, UiState};

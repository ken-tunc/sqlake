//! Application state, intents and use cases. UI-agnostic.
//!
//! This crate owns everything the terminal does not: what is connected, what
//! has been loaded, and what the user asked for. It has no dependency on
//! ratatui, and nothing here knows how wide the terminal is.

pub mod grid;

pub use grid::{Align, Cell, CellKind, RenderedColumn, RenderedGrid};

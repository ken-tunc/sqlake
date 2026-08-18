//! Mouse hit testing.
//!
//! ratatui has no notion of what a rectangle *is*, but it hands every
//! rectangle to the code that draws it. So each widget records "this rectangle
//! is this thing" while drawing, and a click is resolved against that record.
//!
//! The `z` level is what makes overlays correct. Without it, a click outside a
//! modal falls through to whatever is underneath, and a confirmation dialog
//! becomes a way to trigger the very thing it was confirming.

use ratatui::layout::{Position, Rect};
use sqlake_app::action::BusyId;
use sqlake_core::id::TabId;

/// A transient message, minted and owned entirely by this crate: nothing
/// about which notices are on screen is a fact the application layer knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ToastId(u64);

impl ToastId {
    #[must_use]
    pub const fn new(n: u64) -> Self {
        Self(n)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Background of a pane, behind its content.
pub const Z_BASE: u8 = 0;
/// Rows and cells inside a pane.
pub const Z_CONTENT: u8 = 10;
/// Scrollbars, splitters, column edges: things drawn over content.
pub const Z_CHROME: u8 = 20;
/// The full-screen catcher behind a modal.
pub const Z_BACKDROP: u8 = 90;
pub const Z_MODAL: u8 = 100;
pub const Z_MENU: u8 = 110;

/// The panes M0 has. A fixed set, not a generated id: there are four of them
/// and they never change at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PaneId {
    TabBar,
    /// Where focus starts: it is the only pane with anything in it before a
    /// table is opened.
    #[default]
    Explorer,
    Grid,
    StatusBar,
}

/// M0 has exactly one adjustable split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SplitId {
    Explorer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScrollPart {
    Thumb,
    /// The track above the thumb: page up.
    TrackBefore,
    /// The track below the thumb: page down.
    TrackAfter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ButtonId {
    Cancel(BusyId),
    /// The modal's own way out, for a pointer that never goes near `Esc`.
    DismissModal,
    /// The explorer's search box. Clicking it closes it, which is the only
    /// thing a pointer can usefully do to a box it cannot type into.
    Filter,
}

/// What a rectangle on screen belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    Pane(PaneId),

    /// A row of the explorer, by position in the flattened tree. The connection
    /// is whichever one the explorer is showing, so it is not carried here.
    TreeRow {
        index: usize,
    },
    /// The expand glyph on a tree row. Overlaps `TreeRow` and must win.
    TreeToggle {
        index: usize,
    },

    GridCell {
        row: usize,
        col: usize,
    },
    /// A column header: click to sort.
    GridHeader {
        col: usize,
    },
    /// The boundary between two columns: drag to resize.
    GridColEdge {
        col: usize,
    },

    Scrollbar {
        pane: PaneId,
        part: ScrollPart,
    },
    Splitter(SplitId),

    Tab(TabId),
    TabClose(TabId),

    Button(ButtonId),

    /// A transient message. Clicking it dismisses it.
    Toast(ToastId),

    /// Everything behind a modal. Clicking it dismisses the modal instead of
    /// reaching what is underneath.
    Backdrop,
    /// The dialog's own body.
    ///
    /// It swallows the click rather than doing anything with it. Without a
    /// target of its own the click lands on the backdrop underneath and closes
    /// the dialog, so a confirmation would be dismissed by pressing on its own
    /// question.
    Modal,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    rect: Rect,
    z: u8,
    target: Target,
}

/// Everything clickable on the current frame.
#[derive(Debug, Default)]
pub struct HitMap {
    entries: Vec<Entry>,
}

impl HitMap {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Empty the map but keep its allocation. Called once per frame, so
    /// reallocating would be a per-frame cost for no reason.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Record a rectangle. Empty rectangles are dropped: a zero-width column
    /// cannot be clicked, and keeping it would let `contains` match nothing
    /// while still costing a comparison.
    pub fn push(&mut self, rect: Rect, z: u8, target: Target) {
        if rect.is_empty() {
            return;
        }
        self.entries.push(Entry { rect, z, target });
    }

    /// What is at this position.
    ///
    /// The highest `z` wins. Between equal `z`, the most recently pushed wins,
    /// which is the thing drawn last and therefore the thing on top.
    #[must_use]
    pub fn at(&self, position: Position) -> Option<Target> {
        self.entries
            .iter()
            .filter(|e| e.rect.contains(position))
            .max_by_key(|e| e.z)
            .map(|e| e.target)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Widen a one-cell boundary into something a pointer can actually hit.
///
/// A column separator is drawn one cell wide, but asking the user to land on a
/// single cell to resize a column is asking too much. The hit area is three
/// cells; the line stays one.
#[must_use]
pub fn grab_area(boundary: Rect) -> Rect {
    Rect {
        x: boundary.x.saturating_sub(1),
        y: boundary.y,
        width: boundary.width.saturating_add(2),
        height: boundary.height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(map: &HitMap, x: u16, y: u16) -> Option<Target> {
        map.at(Position::new(x, y))
    }

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::new(x, y, w, h)
    }

    #[test]
    fn a_click_inside_a_rectangle_finds_it() {
        let mut map = HitMap::new();
        map.push(rect(2, 3, 4, 2), Z_CONTENT, Target::TreeRow { index: 7 });
        assert_eq!(at(&map, 3, 4), Some(Target::TreeRow { index: 7 }));
    }

    #[test]
    fn rectangle_edges_are_inclusive_at_the_start_and_exclusive_at_the_end() {
        // Off-by-one here means the last row of every pane is dead.
        let mut map = HitMap::new();
        map.push(rect(2, 3, 4, 2), Z_CONTENT, Target::TreeRow { index: 0 });

        assert!(at(&map, 2, 3).is_some(), "top-left corner");
        assert!(at(&map, 5, 4).is_some(), "bottom-right corner");
        assert!(at(&map, 1, 3).is_none(), "one cell left");
        assert!(at(&map, 6, 3).is_none(), "one cell right");
        assert!(at(&map, 2, 2).is_none(), "one cell above");
        assert!(at(&map, 2, 5).is_none(), "one cell below");
    }

    #[test]
    fn nothing_is_found_outside_every_rectangle() {
        let mut map = HitMap::new();
        map.push(rect(0, 0, 2, 2), Z_CONTENT, Target::Backdrop);
        assert_eq!(at(&map, 50, 50), None);
    }

    #[test]
    fn an_empty_map_finds_nothing() {
        assert_eq!(at(&HitMap::new(), 0, 0), None);
    }

    #[test]
    fn a_higher_z_wins_regardless_of_push_order() {
        let mut map = HitMap::new();
        map.push(rect(0, 0, 10, 10), Z_MODAL, Target::Pane(PaneId::Grid));
        map.push(rect(0, 0, 10, 10), Z_BASE, Target::Pane(PaneId::Explorer));
        assert_eq!(at(&map, 5, 5), Some(Target::Pane(PaneId::Grid)));
    }

    #[test]
    fn at_equal_z_the_last_pushed_wins() {
        // Draw order is z-order within a level: whatever was drawn last is on
        // top, so it is what the user clicked.
        let mut map = HitMap::new();
        map.push(rect(0, 0, 10, 1), Z_CONTENT, Target::TreeRow { index: 0 });
        map.push(rect(0, 0, 2, 1), Z_CONTENT, Target::TreeToggle { index: 0 });
        assert_eq!(at(&map, 1, 0), Some(Target::TreeToggle { index: 0 }));
        assert_eq!(at(&map, 5, 0), Some(Target::TreeRow { index: 0 }));
    }

    #[test]
    fn chrome_wins_over_the_content_it_is_drawn_on() {
        let mut map = HitMap::new();
        map.push(rect(0, 0, 20, 1), Z_CONTENT, Target::GridHeader { col: 3 });
        map.push(rect(9, 0, 3, 1), Z_CHROME, Target::GridColEdge { col: 3 });
        assert_eq!(at(&map, 10, 0), Some(Target::GridColEdge { col: 3 }));
        assert_eq!(at(&map, 4, 0), Some(Target::GridHeader { col: 3 }));
    }

    #[test]
    fn a_modal_backdrop_swallows_clicks_meant_for_what_is_behind_it() {
        // Without this, "are you sure?" becomes a way to trigger the thing it
        // was asking about.
        let mut map = HitMap::new();
        map.push(
            rect(0, 0, 40, 20),
            Z_CONTENT,
            Target::GridCell { row: 1, col: 1 },
        );
        map.push(rect(0, 0, 40, 20), Z_BACKDROP, Target::Backdrop);
        map.push(rect(10, 5, 20, 8), Z_MODAL, Target::Pane(PaneId::Grid));

        assert_eq!(at(&map, 2, 2), Some(Target::Backdrop), "outside the modal");
        assert_eq!(
            at(&map, 15, 8),
            Some(Target::Pane(PaneId::Grid)),
            "inside the modal"
        );
    }

    #[test]
    fn a_menu_sits_above_a_modal() {
        let mut map = HitMap::new();
        map.push(rect(0, 0, 40, 20), Z_MODAL, Target::Pane(PaneId::Grid));
        map.push(rect(5, 5, 10, 3), Z_MENU, Target::Tab(TabId::new(1)));
        assert_eq!(at(&map, 6, 6), Some(Target::Tab(TabId::new(1))));
    }

    #[test]
    fn empty_rectangles_are_not_recorded() {
        let mut map = HitMap::new();
        map.push(rect(5, 5, 0, 3), Z_CONTENT, Target::GridHeader { col: 0 });
        map.push(rect(5, 5, 3, 0), Z_CONTENT, Target::GridHeader { col: 1 });
        assert!(map.is_empty());
    }

    #[test]
    fn clearing_keeps_the_map_usable() {
        let mut map = HitMap::new();
        map.push(rect(0, 0, 4, 4), Z_CONTENT, Target::Backdrop);
        map.clear();
        assert!(map.is_empty());
        assert_eq!(at(&map, 1, 1), None);

        map.push(
            rect(0, 0, 4, 4),
            Z_CONTENT,
            Target::Splitter(SplitId::Explorer),
        );
        assert_eq!(at(&map, 1, 1), Some(Target::Splitter(SplitId::Explorer)));
    }

    #[test]
    fn a_grab_area_widens_a_boundary_without_moving_it() {
        let area = grab_area(rect(10, 2, 1, 5));
        assert_eq!(area, rect(9, 2, 3, 5));

        // At the left edge of the screen there is nothing to widen into.
        let clamped = grab_area(rect(0, 0, 1, 1));
        assert_eq!(clamped.x, 0);
        assert_eq!(clamped.width, 3);
    }

    #[test]
    fn a_widened_boundary_is_easier_to_hit_than_the_line() {
        let mut map = HitMap::new();
        map.push(rect(0, 0, 30, 1), Z_CONTENT, Target::GridHeader { col: 0 });
        map.push(
            grab_area(rect(10, 0, 1, 1)),
            Z_CHROME,
            Target::GridColEdge { col: 0 },
        );

        for x in 9..=11 {
            assert_eq!(
                at(&map, x, 0),
                Some(Target::GridColEdge { col: 0 }),
                "x={x} should hit the edge"
            );
        }
        assert_eq!(at(&map, 12, 0), Some(Target::GridHeader { col: 0 }));
    }
}

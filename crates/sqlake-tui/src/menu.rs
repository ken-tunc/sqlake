//! The context menu, which is data.
//!
//! An entry is a label and the intent it produces. Built that way the menu
//! cannot offer an action the keyboard has no route to — that is not a
//! convention to remember but the thing `every_menu_entry_has_a_key_binding`
//! checks, and the reason `IntentKind` is derived from the intent rather than
//! written beside it.
//!
//! Some terminals and tmux configurations cannot deliver a right-click at all,
//! which is why the rule matters here more than anywhere else: for those the
//! menu does not exist, and everything in it has to be reachable without it.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::hit::{HitMap, Target, Z_MENU};
use crate::intent::{Intent, IntentKind, ViewCmd};

/// One line of the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub label: &'static str,
    /// What choosing it does. The concrete intent rather than a kind, because a
    /// menu entry has no direction to derive one from the way a key press does
    /// — and the kind, which is what the coverage check wants, comes back out
    /// of it.
    pub intent: Intent,
    /// Whether it can be chosen now. A greyed entry is still drawn: a menu
    /// whose contents move depending on what is selected is one nobody can
    /// learn.
    pub enabled: bool,
}

impl Entry {
    #[must_use]
    pub const fn kind(&self) -> IntentKind {
        IntentKind::of(&self.intent)
    }
}

/// An open menu, and where it was opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    pub at: (u16, u16),
    pub entries: Vec<Entry>,
}

impl Menu {
    /// The menu for a cell in the grid.
    ///
    /// `ranged` says whether more than one cell is selected, which decides only
    /// what the copy entries are *called*: they act on the selection either
    /// way, and one cell is a rectangle of one.
    #[must_use]
    pub fn for_grid(at: (u16, u16), ranged: bool) -> Self {
        use crate::copy::Format;
        Self {
            at,
            entries: vec![
                Entry {
                    label: "Show in full",
                    intent: ViewCmd::ToggleDetail.into(),
                    enabled: true,
                },
                Entry {
                    label: if ranged {
                        "Copy selection as CSV"
                    } else {
                        "Copy cell as CSV"
                    },
                    intent: ViewCmd::Copy {
                        format: Format::Csv,
                        all: false,
                    }
                    .into(),
                    enabled: true,
                },
                Entry {
                    label: if ranged {
                        "Copy selection as JSON"
                    } else {
                        "Copy cell as JSON"
                    },
                    intent: ViewCmd::Copy {
                        format: Format::Json,
                        all: false,
                    }
                    .into(),
                    enabled: true,
                },
                Entry {
                    label: "Copy everything as CSV",
                    intent: ViewCmd::Copy {
                        format: Format::Csv,
                        all: true,
                    }
                    .into(),
                    enabled: true,
                },
                Entry {
                    label: "Copy everything as JSON",
                    intent: ViewCmd::Copy {
                        format: Format::Json,
                        all: true,
                    }
                    .into(),
                    enabled: true,
                },
            ],
        }
    }

    #[must_use]
    pub fn width(&self) -> u16 {
        let widest = self
            .entries
            .iter()
            .map(|e| crate::grid::display_width(e.label))
            .max()
            .unwrap_or(0);
        widest.saturating_add(2)
    }

    #[must_use]
    pub fn height(&self) -> u16 {
        u16::try_from(self.entries.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2)
    }
}

/// Draw the menu, pushed above everything so a click on it is not a click on
/// what it covers.
pub fn render(frame: &mut Frame<'_>, hits: &mut HitMap, screen: Rect, menu: &Menu) {
    let area = placed(screen, menu);
    frame.render_widget(Clear, area);
    // The whole area first, so the lines pushed below win by being later at the
    // same `z`. Without it the border — and any entry that cannot be chosen —
    // is a hole through to the grid, and pressing on the frame selects the cell
    // the menu is covering.
    hits.push(area, Z_MENU, Target::Menu);
    let block = Block::bordered().border_style(Style::new().fg(Color::Cyan));
    let inside = block.inner(area);
    frame.render_widget(block, area);

    for (index, entry) in menu.entries.iter().enumerate() {
        let Ok(offset) = u16::try_from(index) else {
            break;
        };
        if offset >= inside.height {
            break;
        }
        let row = Rect::new(inside.x, inside.y + offset, inside.width, 1);
        if entry.enabled {
            hits.push(row, Z_MENU, Target::MenuItem { index });
        }
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                entry.label,
                if entry.enabled {
                    Style::new().fg(Color::White)
                } else {
                    Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM)
                },
            ))),
            row,
        );
    }
}

/// Where the menu fits, which is not always where it was asked for.
///
/// A menu opened near the right edge or the bottom would otherwise be drawn
/// half off the screen, and the half that is missing is the half nobody can
/// click.
fn placed(screen: Rect, menu: &Menu) -> Rect {
    let (width, height) = (
        menu.width().min(screen.width),
        menu.height().min(screen.height),
    );
    let x = menu.at.0.min(screen.right().saturating_sub(width));
    let y = menu.at.1.min(screen.bottom().saturating_sub(height));
    Rect::new(x.max(screen.x), y.max(screen.y), width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_labels_say_what_the_selection_is() {
        let one = Menu::for_grid((0, 0), false);
        let many = Menu::for_grid((0, 0), true);
        assert!(one.entries.iter().any(|e| e.label.contains("cell")));
        assert!(many.entries.iter().any(|e| e.label.contains("selection")));
        assert_eq!(
            one.entries.len(),
            many.entries.len(),
            "a menu whose entries come and go is one nobody can learn"
        );
    }

    #[test]
    fn a_menu_near_an_edge_is_moved_rather_than_cut() {
        let screen = Rect::new(0, 0, 80, 24);
        let menu = Menu::for_grid((78, 23), true);
        let area = placed(screen, &menu);
        assert!(area.right() <= screen.right(), "{area:?}");
        assert!(area.bottom() <= screen.bottom(), "{area:?}");
    }

    #[test]
    fn a_menu_bigger_than_the_screen_still_fits_inside_it() {
        let screen = Rect::new(0, 0, 10, 3);
        let area = placed(screen, &Menu::for_grid((0, 0), true));
        assert!(area.width <= screen.width && area.height <= screen.height);
    }
}

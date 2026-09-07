//! The SQL tab's pane: the buffer, shown but not edited.
//!
//! Read-only on purpose. Editing happens in `$EDITOR` — the user's own
//! configuration, completion and key bindings — and this pane exists so that
//! what is about to run is visible without opening the editor to look at it.
//!
//! Nothing here is a text widget. Wrapping, a cursor and a selection are the
//! start of the editor this crate deliberately does not have, and a pane that
//! grew them would be a second, worse one.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::grid::sanitise;

/// Shown in place of the buffer while it is empty.
const EMPTY: &str = "Press e to write a query.";

/// `offset` is the first line drawn, so a buffer longer than the pane can be
/// scrolled through the same way everything else is.
pub fn render(frame: &mut Frame<'_>, area: Rect, text: &str, offset: usize) {
    if area.height == 0 {
        return;
    }
    if text.trim().is_empty() {
        frame.render_widget(
            Paragraph::new(EMPTY)
                .style(Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM)),
            area,
        );
        return;
    }

    // Cut to the pane rather than wrapped: a wrapped line makes "line 3" mean
    // two different things, and line 3 is what an error is about to point at.
    let lines: Vec<Line<'_>> = text
        .lines()
        .skip(offset)
        .take(area.height as usize)
        .map(|line| Line::from(sanitise(line)))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// How many lines the buffer has, for the scrollbar and the clamp.
#[must_use]
pub fn line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.lines().count()
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn drawn(text: &str, offset: usize, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(30, height)).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), text, offset))
            .unwrap();
        terminal.backend().to_string()
    }

    #[test]
    fn an_empty_buffer_says_how_to_fill_it() {
        assert!(drawn("", 0, 3).contains("Press e"));
        // Whitespace is not a query either, and a pane showing three blank
        // lines is one nobody can tell from a broken one.
        assert!(drawn("\n  \n", 0, 3).contains("Press e"));
    }

    #[test]
    fn the_offset_is_the_first_line_drawn() {
        let text = "one\ntwo\nthree";
        let screen = drawn(text, 1, 2);
        assert!(screen.contains("two"), "{screen}");
        assert!(!screen.contains("one"), "{screen}");
    }

    #[test]
    fn a_control_character_in_the_buffer_does_not_reach_the_terminal() {
        // The buffer is a file the user's editor wrote, so it is data like a
        // cell is: an escape sequence in it must not be executed by the
        // terminal drawing it.
        let screen = drawn("select \u{1b}[31m1", 0, 1);
        assert!(!screen.contains('\u{1b}'), "{screen}");
    }

    #[test]
    fn an_empty_buffer_has_no_lines_to_scroll() {
        // `"".lines()` yields nothing, but `"\n".lines()` yields one — the
        // clamp needs the first answer, not `1`.
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("one"), 1);
        assert_eq!(line_count("one\ntwo\n"), 2);
    }
}

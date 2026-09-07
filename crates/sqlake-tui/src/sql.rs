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
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use sqlake_core::sql::Position;

use crate::grid::sanitise;

/// Shown in place of the buffer while it is empty.
///
/// Names the key, now that `e` is bound to something. The double-click that
/// does the same thing is deliberately not mentioned: a terminal that cannot
/// deliver one is exactly the terminal this line has to be useful on.
const EMPTY: &str = "Press e to write a query.";

/// `offset` is the first line drawn, so a buffer longer than the pane can be
/// scrolled through the same way everything else is. `at` is where the server
/// said the statement was wrong, which is why lines are cut here and never
/// wrapped: a wrapped line makes "line 3" mean two different things.
pub fn render(frame: &mut Frame<'_>, area: Rect, text: &str, offset: usize, at: Option<Position>) {
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
    // two different things, and line 3 is what an error points at.
    let bad = at.map(|at| at.line as usize);
    let lines: Vec<Line<'_>> = text
        .lines()
        .enumerate()
        .skip(offset)
        .take(area.height as usize)
        .map(|(index, line)| {
            let line = sanitise(line);
            // The whole line, not the column. A column is a guess about where
            // the mistake starts — the server reports where it *stopped
            // understanding*, which is usually just after it — and a caret
            // under the wrong character reads as a claim the line does not
            // make.
            if bad == Some(index + 1) {
                Line::from(Span::styled(
                    line,
                    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(line)
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// How many lines the buffer has, which is what the scroll clamp measures
/// against.
#[must_use]
pub fn line_count(text: &str) -> usize {
    text.lines().count()
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn drawn(text: &str, offset: usize, height: u16) -> String {
        marked(text, offset, height, None).0
    }

    /// The screen, and which of its lines were drawn in the error style.
    fn marked(
        text: &str,
        offset: usize,
        height: u16,
        at: Option<Position>,
    ) -> (String, Vec<usize>) {
        let mut terminal = Terminal::new(TestBackend::new(30, height)).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), text, offset, at))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let flagged = (0..height)
            .filter(|y| buffer[(0, *y)].style().fg == Some(Color::Red))
            .map(|y| y as usize)
            .collect();
        (terminal.backend().to_string(), flagged)
    }

    #[test]
    fn the_line_the_server_named_is_the_one_marked() {
        let text = "select\n  nope\nfrom t";
        let (_, flagged) = marked(text, 0, 3, Some(Position::new(2, 3)));
        assert_eq!(flagged, [1], "row 1 on screen is line 2 of the buffer");

        // And scrolled, the marker moves with the text rather than staying on
        // the row it was drawn at.
        let (_, flagged) = marked(text, 1, 3, Some(Position::new(2, 3)));
        assert_eq!(flagged, [0]);
    }

    #[test]
    fn a_position_off_the_screen_marks_nothing_rather_than_the_wrong_line() {
        let text = "select\n  nope\nfrom t";
        let (_, flagged) = marked(text, 0, 3, Some(Position::new(9, 1)));
        assert!(flagged.is_empty(), "{flagged:?}");
    }

    #[test]
    fn a_failure_with_nowhere_to_point_marks_nothing() {
        let text = "select\n  nope\nfrom t";
        let (_, flagged) = marked(text, 0, 3, None);
        assert!(flagged.is_empty(), "{flagged:?}");
    }

    #[test]
    fn an_empty_buffer_says_how_to_fill_it() {
        assert!(drawn("", 0, 3).contains(EMPTY));
        // Whitespace is not a query either, and a pane showing three blank
        // lines is one nobody can tell from a broken one.
        assert!(drawn("\n  \n", 0, 3).contains(EMPTY));
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
        // The clamp subtracts one from this, so an empty buffer answering `1`
        // would leave the pane scrollable past a line that is not there.
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("one"), 1);
        assert_eq!(line_count("one\ntwo\n"), 2);
    }
}

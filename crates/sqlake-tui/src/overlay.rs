//! What is drawn over everything else: dialogs and transient messages.
//!
//! A dialog goes down as two rectangles, not one. The backdrop covers the whole
//! screen at [`Z_BACKDROP`] so a click outside the dialog cannot reach what is
//! behind it, and the dialog itself covers its own area at [`Z_MODAL`] so a
//! click inside it does not reach the backdrop. Miss either and a confirmation
//! dialog becomes a way to trigger the thing it was confirming, or a way to
//! dismiss itself by pressing on its own question.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use sqlake_app::snapshot::{Severity, Snapshot};

use crate::chrome;
use crate::grid::{display_width, sanitise};
use crate::hit::{ButtonId, HitMap, Target, Z_BACKDROP, Z_CHROME, Z_MODAL};

/// A dialog's contents. Held by `UiState`, because whether a dialog is open is
/// a fact about this screen and not about the data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Modal {
    pub title: String,
    pub body: String,
}

impl Modal {
    #[must_use]
    pub fn error(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
        }
    }
}

/// Widest a dialog gets, so a long message wraps rather than spanning a
/// forty-inch terminal in one line.
const MODAL_WIDTH: u16 = 60;
const DISMISS: &str = " Dismiss ";

pub fn modal(frame: &mut Frame<'_>, hits: &mut HitMap, area: Rect, dialog: &Modal) {
    // Everything, so nothing behind can be clicked.
    hits.push(area, Z_BACKDROP, Target::Backdrop);

    let width = MODAL_WIDTH.min(area.width);
    let body = sanitise(&dialog.body);
    // Two rows of border, one for the button, one to breathe.
    let text_rows = wrapped_rows(&body, width.saturating_sub(2));
    let height = text_rows.saturating_add(4).min(area.height);
    let rect = centre(area, width, height);

    hits.push(rect, Z_MODAL, Target::Modal);
    frame.render_widget(Clear, rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Red))
        .title(Span::styled(
            format!(" {} ", sanitise(&dialog.title)),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    if inner.height == 0 {
        return;
    }
    let text_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(2),
    );
    frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), text_area);

    // A dialog reachable only by `Esc` is one a pointer cannot leave.
    let button_width = display_width(DISMISS);
    let button = Rect::new(
        inner.x + inner.width.saturating_sub(button_width),
        inner.bottom().saturating_sub(1),
        button_width.min(inner.width),
        1,
    );
    hits.push(button, Z_MODAL, Target::Button(ButtonId::DismissModal));
    frame.render_widget(
        Paragraph::new(DISMISS).style(
            Style::new()
                .fg(Color::Black)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        ),
        button,
    );
}

/// Transient messages, newest at the bottom, stacked up from above the status
/// bar.
///
/// Above the content but below a dialog: a toast that covered a dialog would
/// hide the thing waiting for an answer.
pub fn toasts(frame: &mut Frame<'_>, hits: &mut HitMap, area: Rect, snapshot: &Snapshot) {
    if snapshot.toasts.is_empty() || area.height == 0 {
        return;
    }

    let width = MODAL_WIDTH.min(area.width);
    let x = area.right().saturating_sub(width);
    let mut y = area.bottom();

    // Newest last in the snapshot, and newest nearest the bottom edge, so a new
    // message never shifts the ones already being read.
    for toast in snapshot.toasts.iter().rev() {
        if y <= area.y {
            break;
        }
        y -= 1;
        let rect = Rect::new(x, y, width, 1);
        hits.push(rect, Z_CHROME, Target::Toast(toast.id));
        frame.render_widget(Clear, rect);

        let (colour, badge) = match toast.severity {
            Severity::Info => (Color::Cyan, "i"),
            Severity::Warning => (Color::Yellow, "!"),
            Severity::Error => (Color::Red, "✕"),
        };
        let text = chrome::fit(
            &sanitise(&toast.text),
            width.saturating_sub(display_width(badge) + 3),
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(" {badge} "),
                    Style::new()
                        .fg(Color::Black)
                        .bg(colour)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {text}"), Style::new().fg(colour)),
            ])),
            rect,
        );
    }
}

fn centre(area: Rect, width: u16, height: u16) -> Rect {
    Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width.min(area.width),
        height.min(area.height),
    )
}

/// Rows `text` needs once wrapped to `width`.
///
/// `Paragraph` breaks between words, so dividing the total width by `width`
/// under-counts: a message made of long words leaves the tail of each line
/// empty and takes more rows than its columns divided by the width. The dialog
/// is sized from this and does not scroll, so under-counting silently cuts the
/// end off the message. The text has already been through `sanitise`, so there
/// are no newlines left to break on.
fn wrapped_rows(text: &str, width: u16) -> u16 {
    if width == 0 {
        return 1;
    }
    let mut rows: u16 = 1;
    let mut used: u16 = 0;
    for (i, word) in text.split(' ').enumerate() {
        let word_width = display_width(word);
        // Every word but the first carries the space before it.
        let joined = if i == 0 {
            word_width
        } else {
            used.saturating_add(1).saturating_add(word_width)
        };
        if joined <= width {
            used = joined;
            continue;
        }
        // It starts a line of its own, and is broken across lines of its own
        // if it is wider than one.
        rows = rows.saturating_add(1);
        used = word_width;
        while used > width {
            rows = rows.saturating_add(1);
            used -= width;
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Instant;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;
    use sqlake_app::action::ToastId;
    use sqlake_app::snapshot::Toast;

    use super::*;

    fn snapshot(items: Vec<(Severity, &str)>) -> Snapshot {
        Snapshot {
            rev: 1,
            connections: Vec::new(),
            trees: HashMap::new(),
            tabs: Vec::new(),
            active_tab: None,
            busy: Vec::new(),
            toasts: items
                .into_iter()
                .enumerate()
                .map(|(i, (severity, text))| Toast {
                    id: ToastId::new(i as u64),
                    text: text.into(),
                    severity,
                    created_at: Instant::now(),
                })
                .collect(),
            should_quit: false,
        }
    }

    fn draw(f: impl FnOnce(&mut Frame<'_>, &mut HitMap), w: u16, h: u16) -> (Vec<String>, HitMap) {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut hits = HitMap::new();
        terminal.draw(|frame| f(frame, &mut hits)).unwrap();
        let buffer = terminal.backend().buffer();
        let rows = (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect();
        (rows, hits)
    }

    #[test]
    fn a_click_beside_a_dialog_cannot_reach_what_is_behind_it() {
        let dialog = Modal::error("Failed", "could not connect");
        let (_, hits) = draw(
            |frame, hits| {
                // Something clickable underneath, as the real screen has.
                hits.push(
                    Rect::new(0, 0, 80, 24),
                    crate::hit::Z_CONTENT,
                    Target::TreeRow { index: 0 },
                );
                modal(frame, hits, Rect::new(0, 0, 80, 24), &dialog);
            },
            80,
            24,
        );

        // Without the backdrop this is the tree row, and a confirmation dialog
        // becomes a way to trigger the thing it was confirming.
        assert_eq!(hits.at(Position::new(1, 1)), Some(Target::Backdrop));
    }

    #[test]
    fn a_click_on_the_dialog_does_not_dismiss_it() {
        let dialog = Modal::error("Failed", "could not connect");
        let (_, hits) = draw(
            |frame, hits| modal(frame, hits, Rect::new(0, 0, 80, 24), &dialog),
            80,
            24,
        );

        // Pressing on the question is not an answer to it.
        assert_eq!(hits.at(Position::new(40, 12)), Some(Target::Modal));
    }

    #[test]
    fn a_dialog_can_be_left_with_the_pointer() {
        let dialog = Modal::error("Failed", "could not connect");
        let (text, hits) = draw(
            |frame, hits| modal(frame, hits, Rect::new(0, 0, 80, 24), &dialog),
            80,
            24,
        );

        assert!(text.iter().any(|l| l.contains("Dismiss")), "{text:?}");
        let found = (0..80)
            .flat_map(|x| (0..24).map(move |y| (x, y)))
            .any(|(x, y)| {
                hits.at(Position::new(x, y)) == Some(Target::Button(ButtonId::DismissModal))
            });
        assert!(found, "a dialog only Esc can leave is one a pointer cannot");
    }

    #[test]
    fn the_dialog_says_what_went_wrong() {
        let dialog = Modal::error("Connection failed", "could not connect: refused");
        let (text, _) = draw(
            |frame, hits| modal(frame, hits, Rect::new(0, 0, 80, 24), &dialog),
            80,
            24,
        );
        assert!(
            text.iter().any(|l| l.contains("Connection failed")),
            "{text:?}"
        );
        assert!(text.iter().any(|l| l.contains("refused")), "{text:?}");
    }

    #[test]
    fn a_long_message_wraps_instead_of_being_cut() {
        let body = "a ".repeat(120);
        let dialog = Modal::error("Failed", body);
        let (text, _) = draw(
            |frame, hits| modal(frame, hits, Rect::new(0, 0, 80, 24), &dialog),
            80,
            24,
        );
        let lines = text.iter().filter(|l| l.contains("a a")).count();
        assert!(lines > 1, "wrapped onto one line only: {text:?}");
    }

    #[test]
    fn a_message_of_long_words_is_not_cut_off_at_the_bottom() {
        // Words wider than half the box take a row each, so measuring the
        // message as if it wrapped mid-word makes the box too short and the
        // last thing the driver said is the part that goes missing.
        let words: Vec<String> = (0..8).map(|i| format!("{}{i}", "x".repeat(39))).collect();
        let dialog = Modal::error("Failed", words.join(" "));
        let (text, _) = draw(
            |frame, hits| modal(frame, hits, Rect::new(0, 0, 80, 24), &dialog),
            80,
            24,
        );
        let last = words.last().expect("no words");
        assert!(
            text.iter().any(|l| l.contains(last.as_str())),
            "the end of the message was cut: {text:?}"
        );
    }

    #[test]
    fn a_dialog_taller_than_the_screen_still_fits_on_it() {
        let dialog = Modal::error("Failed", "x ".repeat(400));
        let (_, hits) = draw(
            |frame, hits| modal(frame, hits, Rect::new(0, 0, 40, 8), &dialog),
            40,
            8,
        );
        // It fills the screen rather than being drawn off it — and the way
        // out has to survive that, or a small terminal traps the user in a
        // dialog with no visible answer.
        let button = (0..40)
            .flat_map(|x| (0..8).map(move |y| (x, y)))
            .any(|(x, y)| {
                hits.at(Position::new(x, y)) == Some(Target::Button(ButtonId::DismissModal))
            });
        assert!(button, "no way out on a small screen");
        assert_eq!(hits.at(Position::new(0, 0)), Some(Target::Modal));
    }

    #[test]
    fn a_newline_in_the_body_does_not_become_a_line_of_its_own() {
        // A raw newline reaches Paragraph as a line break, so the message
        // silently gains rows and the box grows to hold them — driver text
        // deciding how big the dialog is.
        let one = Modal::error("Failed", "alpha beta");
        let two = Modal::error("Failed", "alpha\nbeta");

        let rows_of = |dialog: &Modal| {
            let (text, _) = draw(
                |frame, hits| modal(frame, hits, Rect::new(0, 0, 60, 12), dialog),
                60,
                12,
            );
            text.iter().filter(|l| l.contains("alpha")).count()
                + text.iter().filter(|l| l.contains("beta")).count()
        };
        assert_eq!(rows_of(&one), rows_of(&two), "the newline added a row");

        let (text, _) = draw(
            |frame, hits| modal(frame, hits, Rect::new(0, 0, 60, 12), &two),
            60,
            12,
        );
        assert!(text.join("").contains('␊'), "{text:?}");
    }

    #[test]
    fn a_control_character_in_the_title_cannot_break_the_box() {
        let dialog = Modal::error("we\nird", "body");
        let (text, _) = draw(
            |frame, hits| modal(frame, hits, Rect::new(0, 0, 60, 12), &dialog),
            60,
            12,
        );
        assert!(text.join("").contains('␊'), "{text:?}");
    }

    #[test]
    fn every_toast_can_be_dismissed_by_clicking_it() {
        let snap = snapshot(vec![
            (Severity::Info, "connected"),
            (Severity::Error, "boom"),
        ]);
        let (_, hits) = draw(
            |frame, hits| toasts(frame, hits, Rect::new(0, 0, 80, 24), &snap),
            80,
            24,
        );

        let found: std::collections::BTreeSet<_> = (0..80)
            .flat_map(|x| (0..24).map(move |y| (x, y)))
            .filter_map(|(x, y)| match hits.at(Position::new(x, y)) {
                Some(Target::Toast(id)) => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(found.len(), 2, "{found:?}");
    }

    #[test]
    fn a_new_toast_does_not_shift_the_one_being_read() {
        let first = snapshot(vec![(Severity::Info, "first")]);
        let (before, _) = draw(
            |frame, hits| toasts(frame, hits, Rect::new(0, 0, 80, 24), &first),
            80,
            24,
        );
        let row_of = |text: &[String], needle: &str| text.iter().position(|l| l.contains(needle));
        let was = row_of(&before, "first").expect("not drawn");

        let both = snapshot(vec![(Severity::Info, "first"), (Severity::Error, "second")]);
        let (after, _) = draw(
            |frame, hits| toasts(frame, hits, Rect::new(0, 0, 80, 24), &both),
            80,
            24,
        );
        // The newest takes the bottom row and the older one moves up, so the
        // message being read does not jump out from under the pointer.
        assert_eq!(row_of(&after, "second"), Some(was));
        assert_eq!(row_of(&after, "first"), Some(was - 1));
    }

    #[test]
    fn severity_is_visible_and_not_only_recorded() {
        for (severity, badge) in [
            (Severity::Info, "i"),
            (Severity::Warning, "!"),
            (Severity::Error, "✕"),
        ] {
            let snap = snapshot(vec![(severity, "message")]);
            let (text, _) = draw(
                |frame, hits| toasts(frame, hits, Rect::new(0, 0, 40, 6), &snap),
                40,
                6,
            );
            assert!(text.join("").contains(badge), "{severity:?}: {text:?}");
        }
    }

    #[test]
    fn more_toasts_than_rows_do_not_draw_off_the_top() {
        let snap = snapshot((0..30).map(|_| (Severity::Info, "message")).collect());
        let (_, hits) = draw(
            |frame, hits| toasts(frame, hits, Rect::new(0, 2, 40, 3), &snap),
            40,
            6,
        );
        for y in 0..2 {
            assert_eq!(hits.at(Position::new(39, y)), None, "row {y} is not ours");
        }
    }

    #[test]
    fn a_toast_wider_than_the_screen_is_cut_to_it() {
        let long = "x".repeat(200);
        let snap = snapshot(vec![(Severity::Error, &long)]);
        let (text, _) = draw(
            |frame, hits| toasts(frame, hits, Rect::new(0, 0, 30, 4), &snap),
            30,
            4,
        );
        assert!(text.iter().any(|l| l.contains('…')), "{text:?}");
    }
}

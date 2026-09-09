//! What this client has run, as a grid with a search box over it.
//!
//! The rows are shaped here rather than in `sqlake-app`, for the reason a
//! definition's are: an agent asking for its history wants `started_at` as a
//! timestamp and `duration_ms` as a number, and a screen wants "4m ago" and
//! "12ms". Both readings of one row is what the layering rule keeps apart.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use time::OffsetDateTime;

use sqlake_app::PagedResult;
use sqlake_core::library::HistoryEntry;
use sqlake_core::result::{Column, ResultSet, Row};
use sqlake_core::value::Value;

use crate::chrome::fit;
use crate::grid::sanitise;
use crate::ui::Filter;

/// The runs, in the shape the grid takes.
///
/// One statement per row and nothing wrapped: a history is scanned for the
/// query somebody half-remembers, and the pane that shows one in full is the
/// cell detail, which every grid already has.
#[must_use]
pub fn rows(entries: &[HistoryEntry], now: OffsetDateTime) -> PagedResult {
    let result = ResultSet::new(
        vec![
            Column::new("when", "text", false),
            Column::new("by", "text", true),
            Column::new("status", "text", true),
            Column::new("took", "text", true),
            Column::new("rows", "text", true),
            Column::new("statement", "text", false),
            Column::new("error", "text", true),
        ],
        entries
            .iter()
            .map(|entry| {
                Row(vec![
                    Value::Text(ago(entry.started_at, now)),
                    // Who asked for it, and empty for a row written before
                    // this client kept the answer — which is not "a person",
                    // and saying so would be a guess written as a fact.
                    entry
                        .issuer
                        .map_or(Value::Null, |who| Value::Text(who.as_str().to_owned())),
                    // Empty rather than "running": the `when` column already
                    // says it started, and a word that appears only while
                    // somebody is looking is a row that changes under them.
                    entry.status.clone().map_or(Value::Null, Value::Text),
                    entry
                        .duration_ms
                        .map_or(Value::Null, |ms| Value::Text(took(ms))),
                    entry
                        .row_count
                        .map_or(Value::Null, |rows| Value::Text(rows.to_string())),
                    Value::Text(one_line(&entry.sql)),
                    entry.error.clone().map_or(Value::Null, Value::Text),
                ])
            })
            .collect(),
        Some(entries.len() as u64),
    );
    PagedResult::new(&result)
}

/// A statement on one line.
///
/// A grid row is one line high, and a `\n` in a cell is a glyph the terminal
/// draws as a box — so the newlines go before the cell is built rather than
/// being elided into one by the renderer, which cannot know that the runs of
/// whitespace around them were an indented `FROM`.
fn one_line(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// How long ago, in the roughest unit that still says something.
///
/// Relative rather than a local time, and not because it reads better: a local
/// time needs the machine's UTC offset, and `time` refuses to read one in a
/// process with threads — which this is. UTC in a column called `when` would be
/// a number nobody can compare with their own clock.
fn ago(at: OffsetDateTime, now: OffsetDateTime) -> String {
    let seconds = (now - at).whole_seconds();
    if seconds < 0 {
        // A row from the future: the clock stepped back between the run and
        // the reading of it. "just now" is wrong by less than the step.
        return "just now".to_owned();
    }
    match seconds {
        0..=59 => format!("{seconds}s ago"),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86_399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// A duration in the unit it is worth reading in.
fn took(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// Draw the search box, and answer with what is left for the grid.
pub fn search_box(frame: &mut Frame<'_>, area: Rect, filter: &Filter, count: usize) -> Rect {
    if area.height == 0 {
        return area;
    }
    let line = Rect { height: 1, ..area };
    let colour = if filter.editing {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let mut spans = vec![
        Span::styled("search ", Style::default().fg(colour)),
        Span::raw(sanitise(&filter.text)),
    ];
    if filter.editing {
        // A block rather than the terminal's own cursor, which belongs to the
        // grid's selected cell and would be taken off it.
        spans.push(Span::styled("▏", Style::default().fg(Color::Cyan)));
    }
    // Said on the same line as the box because it is the answer to what is in
    // it: a count somewhere else is one somebody has to look for.
    let tally = format!("  {count} runs");
    if fit(&tally, line.width).len() < line.width as usize {
        spans.push(Span::styled(
            tally,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), line);

    Rect {
        y: area.y + 1,
        height: area.height - 1,
        ..area
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use sqlake_core::capability::DriverKind;
    use sqlake_core::library::RunId;
    use time::Duration;

    use super::*;

    fn entry(sql: &str, at: OffsetDateTime) -> HistoryEntry {
        HistoryEntry {
            id: RunId::new(1),
            connection: "c".to_owned(),
            issuer: Some(sqlake_core::library::Issuer::Human),
            driver: Some(DriverKind::Mock),
            sql: sql.to_owned(),
            started_at: at,
            status: Some("ok".to_owned()),
            duration_ms: Some(12),
            row_count: Some(3),
            bytes_processed: None,
            error: None,
        }
    }

    #[test]
    fn a_statement_is_one_line_however_it_was_written() {
        // A `\n` in a cell is a glyph the terminal draws as a box, and the
        // renderer cannot know that the run of whitespace around it was an
        // indented `FROM`.
        let now = OffsetDateTime::UNIX_EPOCH;
        let rows = rows(&[entry("select *\n  from users", now)], now);
        assert_eq!(
            rows.value(0, 5),
            Some(&Value::Text("select * from users".to_owned()))
        );
    }

    #[test]
    fn how_long_ago_is_said_in_one_unit() {
        let now = OffsetDateTime::UNIX_EPOCH + Duration::days(400);
        let at = |d: Duration| ago(now - d, now);
        assert_eq!(at(Duration::seconds(3)), "3s ago");
        assert_eq!(at(Duration::seconds(90)), "1m ago");
        assert_eq!(at(Duration::hours(5)), "5h ago");
        assert_eq!(at(Duration::days(2)), "2d ago");
    }

    #[test]
    fn a_clock_that_stepped_back_does_not_make_a_run_negative() {
        let now = OffsetDateTime::UNIX_EPOCH;
        assert_eq!(ago(now + Duration::hours(1), now), "just now");
    }

    #[test]
    fn a_duration_is_read_in_the_unit_that_suits_it() {
        assert_eq!(took(12), "12ms");
        assert_eq!(took(999), "999ms");
        assert_eq!(took(1500), "1.5s");
    }

    #[test]
    fn a_run_that_has_not_ended_leaves_its_columns_empty() {
        // Rather than saying "running": the row would change under somebody
        // reading it, and `when` already says it started.
        let now = OffsetDateTime::UNIX_EPOCH;
        let mut running = entry("select 1", now);
        running.status = None;
        running.duration_ms = None;
        running.row_count = None;
        let rows = rows(&[running], now);
        assert_eq!(rows.value(0, 2), Some(&Value::Null));
        assert_eq!(rows.value(0, 3), Some(&Value::Null));
    }

    #[test]
    fn the_box_says_what_is_in_it_and_how_much_it_found() {
        let mut terminal = Terminal::new(TestBackend::new(60, 6)).unwrap();
        let mut left = Rect::default();
        terminal
            .draw(|frame| {
                left = search_box(
                    frame,
                    frame.area(),
                    &Filter {
                        text: "orders".to_owned(),
                        editing: true,
                    },
                    7,
                );
            })
            .unwrap();
        let screen = terminal.backend().to_string();
        assert!(screen.contains("orders"), "{screen}");
        assert!(screen.contains("7 runs"), "{screen}");
        assert_eq!(left.height, 5, "the grid gets the rest");
    }
}

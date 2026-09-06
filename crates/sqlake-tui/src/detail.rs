//! One value, laid out to be read rather than to fit a column.
//!
//! Built beside [`crate::grid::RenderedGrid`] rather than from it. The grid
//! clamps a cell at [`crate::grid::MAX_CELL_CHARS`] and collapses a document to
//! `{2 keys}`, which is right in a column a few cells wide and is exactly what
//! somebody opening the detail pane is trying to get past. Reading the grid's
//! output here would make the pane show the same abbreviation in more space.
//!
//! Drawing code still never sees a `Value`: the wrapping, the escaping and how
//! far to indent a nested field are terminal decisions, so they are made here
//! and the widget receives lines.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use sqlake_core::value::Value;

use crate::grid::{CellKind, sanitise_unbounded};

/// One line of the document, and how deep it sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailLine {
    pub depth: u8,
    /// Present on a struct field and on nothing else — an array's elements are
    /// positional, and numbering them here would invent names the value does
    /// not have.
    pub label: Option<String>,
    pub text: String,
    pub kind: CellKind,
}

/// A value, ready to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedDetail {
    pub column: String,
    pub type_name: String,
    pub lines: Vec<DetailLine>,
}

/// How deep a document is followed before it is left as one line.
///
/// A guard against a value that nests further than anybody will read, not a
/// display choice: the grid's own recursion has no depth limit either, but it
/// stops at one line. Twelve is past every shape a `RECORD` or a composite
/// takes in practice, and a value deeper than that is still shown — as JSON on
/// one line, which is what it would have looked like anyway.
const MAX_DEPTH: u8 = 12;

impl RenderedDetail {
    #[must_use]
    pub fn of(column: &str, type_name: &str, value: &Value) -> Self {
        let mut lines = Vec::new();
        push(&mut lines, 0, None, value);
        Self {
            column: column.to_owned(),
            type_name: type_name.to_owned(),
            lines,
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

fn push(lines: &mut Vec<DetailLine>, depth: u8, label: Option<String>, value: &Value) {
    match value {
        Value::Struct(fields) if depth < MAX_DEPTH && !fields.is_empty() => {
            lines.push(DetailLine {
                depth,
                label,
                text: String::new(),
                kind: CellKind::Complex,
            });
            for (name, nested) in fields {
                push(lines, depth.saturating_add(1), Some(name.clone()), nested);
            }
        }
        Value::Array(items) if depth < MAX_DEPTH && !items.is_empty() => {
            lines.push(DetailLine {
                depth,
                label,
                text: String::new(),
                kind: CellKind::Complex,
            });
            for item in items {
                push(lines, depth.saturating_add(1), None, item);
            }
        }
        // A JSON document is followed the same way, because an agent asked for
        // the document and so did whoever opened this pane.
        Value::Json(json) => push_json(lines, depth, label, json),
        _ => lines.push(DetailLine {
            depth,
            label,
            text: scalar(value),
            kind: kind_of(value),
        }),
    }
}

fn push_json(
    lines: &mut Vec<DetailLine>,
    depth: u8,
    label: Option<String>,
    json: &serde_json::Value,
) {
    match json {
        serde_json::Value::Object(map) if depth < MAX_DEPTH && !map.is_empty() => {
            lines.push(DetailLine {
                depth,
                label,
                text: String::new(),
                kind: CellKind::Complex,
            });
            for (name, nested) in map {
                push_json(lines, depth.saturating_add(1), Some(name.clone()), nested);
            }
        }
        serde_json::Value::Array(items) if depth < MAX_DEPTH && !items.is_empty() => {
            lines.push(DetailLine {
                depth,
                label,
                text: String::new(),
                kind: CellKind::Complex,
            });
            for item in items {
                push_json(lines, depth.saturating_add(1), None, item);
            }
        }
        serde_json::Value::Null => lines.push(DetailLine {
            depth,
            label,
            text: "null".to_owned(),
            kind: CellKind::Null,
        }),
        serde_json::Value::String(text) => lines.push(DetailLine {
            depth,
            label,
            text: sanitise_unbounded(text),
            kind: CellKind::Text,
        }),
        other => lines.push(DetailLine {
            depth,
            label,
            text: other.to_string(),
            kind: CellKind::Number,
        }),
    }
}

/// The whole value as text, with no length limit.
///
/// The limit is the difference between this pane and the grid: a cell is
/// clamped because a column is a few characters wide, and a value that was
/// clamped is the reason somebody is looking here.
fn scalar(value: &Value) -> String {
    match value {
        Value::Null => crate::grid::NULL_GLYPH.to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Decimal(text) => text.clone(),
        Value::Text(text) => sanitise_unbounded(text),
        Value::Bytes(bytes) => format!("{} bytes", bytes.len()),
        Value::Date(d) => d.to_string(),
        Value::Time(t) => t.to_string(),
        Value::Timestamp(ts) => ts.to_string(),
        Value::TimestampTz(ts) => ts.to_string(),
        // Reached only for the empty ones; the rest are followed above.
        Value::Struct(_) => "{}".to_owned(),
        Value::Array(_) => "[]".to_owned(),
        Value::Json(json) => sanitise_unbounded(&json.to_string()),
        Value::Opaque { type_name, text } => format!("{type_name}: {}", sanitise_unbounded(text)),
    }
}

fn kind_of(value: &Value) -> CellKind {
    match value {
        Value::Null => CellKind::Null,
        Value::Int(_) | Value::Float(_) | Value::Decimal(_) => CellKind::Number,
        Value::Struct(_) | Value::Array(_) | Value::Json(_) => CellKind::Complex,
        Value::Opaque { .. } => CellKind::Opaque,
        _ => CellKind::Text,
    }
}

/// Draw the document into `area`.
///
/// The pane takes what a value needs and no more: a line longer than the pane
/// wraps rather than being cut, because a value cut here has not been read —
/// which is the one thing this pane exists to prevent.
pub fn render(frame: &mut Frame<'_>, area: Rect, detail: Option<&RenderedDetail>, offset: usize) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let block = Block::bordered()
        .border_style(Style::new().fg(Color::DarkGray))
        .title(detail.map_or_else(
            || " cell ".to_owned(),
            |d| format!(" {} · {} ", d.column, d.type_name),
        ));
    let inside = block.inner(area);
    frame.render_widget(block, area);

    let Some(detail) = detail else {
        frame.render_widget(
            Paragraph::new("no cell selected").style(Style::new().fg(Color::DarkGray)),
            inside,
        );
        return;
    };

    let lines: Vec<Line<'_>> = detail
        .lines
        .iter()
        .map(|line| {
            let mut spans = vec![Span::raw("  ".repeat(usize::from(line.depth)))];
            if let Some(label) = &line.label {
                spans.push(Span::styled(
                    format!("{label}: "),
                    Style::new().fg(Color::Gray),
                ));
            }
            spans.push(Span::styled(line.text.clone(), style_for(line.kind)));
            Line::from(spans)
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((u16::try_from(offset).unwrap_or(u16::MAX), 0)),
        inside,
    );
}

/// The grid's palette, so one value does not change colour on the way here.
const fn style_for(kind: CellKind) -> Style {
    match kind {
        CellKind::Null => Style::new().fg(Color::DarkGray),
        CellKind::Number => Style::new().fg(Color::Cyan),
        CellKind::Text => Style::new().fg(Color::White),
        CellKind::Complex => Style::new().fg(Color::Magenta),
        CellKind::Opaque => Style::new().fg(Color::Yellow),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(value: &Value) -> RenderedDetail {
        RenderedDetail::of("c", "t", value)
    }

    fn drawn(detail: Option<&RenderedDetail>, w: u16, h: u16) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).expect("a terminal");
        terminal
            .draw(|frame| render(frame, Rect::new(0, 0, w, h), detail, 0))
            .expect("it draws");
        let buffer = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_pane_says_which_column_and_type_it_is_showing() {
        let detail = RenderedDetail::of("email", "text", &Value::Text("a@example.com".into()));
        let screen = drawn(Some(&detail), 40, 5);
        assert!(screen.contains("email"), "{screen}");
        assert!(screen.contains("text"), "{screen}");
        assert!(screen.contains("a@example.com"), "{screen}");
    }

    #[test]
    fn a_long_value_wraps_rather_than_being_cut() {
        // Cutting here would leave the pane showing the same abbreviation the
        // grid already showed, in more space.
        let long = "ab".repeat(60);
        let detail = RenderedDetail::of("c", "text", &Value::Text(long));
        let screen = drawn(Some(&detail), 20, 12);
        let body: String = screen.lines().skip(1).collect();
        assert!(
            body.matches("ab").count() > 20,
            "the value was cut at the pane's width: {screen}"
        );
    }

    #[test]
    fn a_pane_with_nothing_chosen_says_so() {
        let screen = drawn(None, 30, 4);
        assert!(screen.contains("no cell selected"), "{screen}");
    }

    #[test]
    fn a_value_the_grid_had_to_cut_is_here_in_full() {
        // The whole reason for this pane. `MAX_CELL_CHARS` is 512, so a test
        // written against a shorter value would pass on a pane built out of
        // `RenderedGrid` — which shows the same abbreviation in more room.
        let long = "x".repeat(crate::grid::MAX_CELL_CHARS * 4);
        let detail = of(&Value::Text(long.clone()));
        assert_eq!(detail.lines.len(), 1);
        assert_eq!(detail.lines[0].text, long);
    }

    #[test]
    fn a_document_is_a_document_rather_than_two_keys() {
        let value = Value::Struct(vec![
            ("name".into(), Value::Text("ada".into())),
            (
                "address".into(),
                Value::Struct(vec![("city".into(), Value::Text("London".into()))]),
            ),
        ]);
        let detail = of(&value);
        let shape: Vec<(u8, Option<&str>, &str)> = detail
            .lines
            .iter()
            .map(|l| (l.depth, l.label.as_deref(), l.text.as_str()))
            .collect();
        assert_eq!(
            shape,
            [
                (0, None, ""),
                (1, Some("name"), "ada"),
                (1, Some("address"), ""),
                (2, Some("city"), "London"),
            ]
        );
    }

    #[test]
    fn an_arrays_elements_are_not_given_invented_names() {
        let detail = of(&Value::Array(vec![Value::Int(1), Value::Int(2)]));
        assert!(
            detail.lines[1..].iter().all(|l| l.label.is_none()),
            "an element was labelled with something the value does not carry"
        );
    }

    #[test]
    fn a_newline_survives_as_a_newline() {
        // The grid writes `␊` because a real newline would break the row it is
        // drawn in. Here there are lines to spare, and the escape would be the
        // thing standing between somebody and the value.
        let detail = of(&Value::Text("a\nb".into()));
        assert!(detail.lines[0].text.contains('\n'), "{:?}", detail.lines[0]);
    }

    #[test]
    fn an_escape_sequence_does_not_reach_the_terminal() {
        // A pane with room for the value is not a reason to write a value that
        // rewrites the screen.
        let detail = of(&Value::Text("\u{1b}[2Jgone".into()));
        assert!(
            !detail.lines[0].text.contains('\u{1b}'),
            "{:?}",
            detail.lines[0]
        );
    }

    #[test]
    fn an_empty_document_is_still_a_value() {
        assert_eq!(of(&Value::Struct(Vec::new())).lines[0].text, "{}");
        assert_eq!(of(&Value::Array(Vec::new())).lines[0].text, "[]");
    }

    #[test]
    fn nesting_past_the_limit_is_shown_rather_than_dropped() {
        let mut value = Value::Int(1);
        for _ in 0..(MAX_DEPTH + 4) {
            value = Value::Array(vec![value]);
        }
        let detail = of(&value);
        assert!(detail.lines.iter().all(|l| l.depth <= MAX_DEPTH));
        assert!(
            !detail.lines.last().expect("a line").text.is_empty(),
            "the value past the depth limit was dropped instead of shown"
        );
    }
}

//! The definition pane: a list of sections, and one of them in the grid.
//!
//! One at a time rather than five stacked, because five grids in one pane give
//! each of them four rows. A list on the left and a grid beside it is the
//! arrangement the explorer and the preview already have, and it means the
//! grid, its scrolling, its selection and its copy are the ones that exist.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use sqlake_app::snapshot::Definition;

use crate::grid::{display_width, sanitise};
use crate::hit::{HitMap, Target, Z_CHROME};

/// How wide the section list gets.
///
/// Fixed, and wide enough for the longest title any driver produces —
/// "Partitioning" — plus its marker. A splitter for it would be a third
/// draggable edge on a screen that has two, for a list nobody resizes.
const LIST_WIDTH: u16 = 16;

/// The list, and the area left for the grid.
///
/// Returns the grid's rectangle so the caller draws into what is left rather
/// than working the arithmetic out a second time and disagreeing about it.
pub fn sections(
    frame: &mut Frame<'_>,
    hits: &mut HitMap,
    area: Rect,
    definition: &Definition,
    selected: usize,
) -> Rect {
    let titles = definition.titles();
    // Below the width where a list and a grid both fit, the grid wins: the
    // list names what you could look at and the grid is what you are looking
    // at.
    if area.width <= LIST_WIDTH * 2 {
        return area;
    }

    for (index, title) in titles.iter().enumerate() {
        let Ok(offset) = u16::try_from(index) else {
            break;
        };
        if offset >= area.height {
            break;
        }
        let row = Rect::new(area.x, area.y + offset, LIST_WIDTH, 1);
        hits.push(row, Z_CHROME, Target::Section { index });
        let style = if index == selected {
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::Gray)
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {} ", fit(title, LIST_WIDTH.saturating_sub(2))),
                style,
            ))),
            row,
        );
    }

    Rect::new(
        area.x + LIST_WIDTH,
        area.y,
        area.width - LIST_WIDTH,
        area.height,
    )
}

/// What the pane says above the grid: what this relation is, and its numbers.
///
/// One line, because the sections below it are the content and a header that
/// grows pushes them off the screen.
#[must_use]
pub fn summary(definition: &Definition) -> String {
    let mut parts = vec![definition.kind.as_str().to_owned()];
    parts.extend(
        definition
            .stats
            .iter()
            .map(|(name, value)| format!("{}: {value}", name.to_lowercase())),
    );
    if let Some(comment) = &definition.comment {
        parts.push(sanitise(comment));
    }
    parts.join(" · ")
}

fn fit(text: &str, max: u16) -> String {
    if display_width(text) <= max {
        text.to_owned()
    } else {
        crate::chrome::fit(text, max)
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use sqlake_core::detail::{ColumnDef, DetailSection, TableDetail};
    use sqlake_core::node::{RelationKind, TableRef};
    use sqlake_core::result::{Column, ResultSet, Row};
    use sqlake_core::value::Value;

    use super::*;

    fn definition(section_titles: &[&str]) -> Definition {
        let mut detail = TableDetail::new(
            TableRef::new(["public", "users"]),
            RelationKind::Table,
            vec![ColumnDef {
                name: "id".to_owned(),
                type_name: "integer".to_owned(),
                nullable: false,
                default: None,
                comment: None,
            }],
        );
        detail.stats = vec![("Rows".to_owned(), "12".to_owned())];
        for title in section_titles {
            detail.sections.push(DetailSection {
                title: (*title).to_owned(),
                table: ResultSet::new(
                    vec![Column::new("name", "text", false)],
                    vec![Row(vec![Value::Text("a".to_owned())])],
                    None,
                ),
            });
        }
        Definition::of(&detail)
    }

    fn drawn(definition: &Definition, selected: usize, w: u16) -> (String, Rect) {
        let mut terminal = Terminal::new(TestBackend::new(w, 6)).unwrap();
        let mut hits = HitMap::new();
        let mut grid = Rect::default();
        terminal
            .draw(|frame| {
                grid = sections(frame, &mut hits, frame.area(), definition, selected);
            })
            .unwrap();
        (terminal.backend().to_string(), grid)
    }

    #[test]
    fn columns_lead_and_the_driver_names_the_rest() {
        let definition = definition(&["Indexes", "Partitioning"]);
        assert_eq!(definition.titles(), ["Columns", "Indexes", "Partitioning"]);
        let (screen, _) = drawn(&definition, 0, 60);
        assert!(screen.contains("Columns"), "{screen}");
        assert!(screen.contains("Indexes"), "{screen}");
    }

    #[test]
    fn the_grid_gets_what_the_list_did_not_take() {
        let definition = definition(&["Indexes"]);
        let (_, grid) = drawn(&definition, 0, 60);
        assert_eq!(grid.x, LIST_WIDTH);
        assert_eq!(grid.width, 60 - LIST_WIDTH);
    }

    #[test]
    fn a_pane_too_narrow_for_both_gives_it_all_to_the_grid() {
        // The list names what you could look at; the grid is what you are
        // looking at, and the second is the one worth the columns.
        let definition = definition(&["Indexes"]);
        let (_, grid) = drawn(&definition, 0, 20);
        assert_eq!(grid.width, 20);
        assert_eq!(grid.x, 0);
    }

    #[test]
    fn every_section_is_a_target_a_pointer_can_reach() {
        let definition = definition(&["Indexes", "Partitioning"]);
        let mut terminal = Terminal::new(TestBackend::new(60, 6)).unwrap();
        let mut hits = HitMap::new();
        terminal
            .draw(|frame| {
                sections(frame, &mut hits, frame.area(), &definition, 0);
            })
            .unwrap();
        for index in 0..definition.titles().len() {
            let at = hits.at(ratatui::layout::Position::new(
                1,
                u16::try_from(index).unwrap(),
            ));
            assert_eq!(at, Some(Target::Section { index }), "section {index}");
        }
    }

    #[test]
    fn the_summary_says_what_it_is_before_what_is_in_it() {
        let mut definition = definition(&[]);
        definition.comment = Some("everyone who signed up".to_owned());
        let summary = summary(&definition);
        assert!(summary.starts_with("table"), "{summary}");
        assert!(summary.contains("rows: 12"), "{summary}");
        assert!(summary.contains("everyone who signed up"), "{summary}");
    }
}

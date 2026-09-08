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
use std::sync::Arc;

use sqlake_app::PagedResult;
use sqlake_core::detail::{Ddl, TableDetail};
use sqlake_core::result::{Column as ResultColumn, ResultSet, Row};
use sqlake_core::value::Value;

use crate::chrome::fit;
use crate::grid::sanitise;
use crate::hit::{HitMap, Target, Z_CHROME};

/// A definition laid out for this screen.
///
/// Built here rather than in `sqlake-app`, which is forbidden a display
/// decision — and turning `nullable: false` into the words `not null` is one.
/// `sqlake-api` renders the same [`TableDetail`] as JSON with the boolean
/// still a boolean, which is the opposite rendering the layering rule exists
/// to keep separate.
#[derive(Debug, Clone)]
pub struct Laid {
    kind: &'static str,
    comment: Option<String>,
    columns: Arc<PagedResult>,
    sections: Vec<(String, Arc<PagedResult>)>,
    ddl: Option<Ddl>,
    stats: Vec<(String, String)>,
}

impl Laid {
    /// The columns as a grid, and every other section beside them.
    ///
    /// Columns become a section like the rest here, which is what lets the
    /// pane draw one list and one grid rather than a special case in front of
    /// a loop.
    #[must_use]
    pub fn out(detail: &TableDetail) -> Self {
        let columns = ResultSet::new(
            vec![
                ResultColumn::new("column", "text", false),
                ResultColumn::new("type", "text", false),
                ResultColumn::new("null", "text", false),
                ResultColumn::new("default", "text", true),
                ResultColumn::new("comment", "text", true),
            ],
            detail
                .columns
                .iter()
                .map(|column| {
                    Row(vec![
                        Value::Text(column.name.clone()),
                        Value::Text(column.type_name.clone()),
                        // A word rather than a boolean: `false` under a column
                        // headed `null` is two negatives to hold at once.
                        Value::Text(if column.nullable { "" } else { "not null" }.to_owned()),
                        column.default.clone().map_or(Value::Null, Value::Text),
                        column.comment.clone().map_or(Value::Null, Value::Text),
                    ])
                })
                .collect(),
            Some(detail.columns.len() as u64),
        );
        Self {
            kind: detail.kind.as_str(),
            comment: detail.comment.clone(),
            columns: Arc::new(PagedResult::new(&columns)),
            sections: detail
                .sections
                .iter()
                .map(|section| {
                    (
                        section.title.clone(),
                        Arc::new(PagedResult::new(&section.table)),
                    )
                })
                .collect(),
            ddl: detail.ddl.clone(),
            stats: detail.stats.clone(),
        }
    }

    /// Every section this pane can show: columns, the driver's own, and the
    /// DDL last if there is one.
    ///
    /// Columns lead because they are what somebody opened the pane for. The
    /// DDL is last because it is the longest and the one most often skipped.
    #[must_use]
    pub fn titles(&self) -> Vec<&str> {
        std::iter::once("Columns")
            .chain(self.sections.iter().map(|(title, _)| title.as_str()))
            .chain(self.ddl.is_some().then_some("DDL"))
            .collect()
    }

    /// The rows under the section at `index`, or `None` for the DDL — which is
    /// text. A caller drawing whichever of the two is there cannot show the
    /// wrong one.
    #[must_use]
    pub fn rows(&self, index: usize) -> Option<&Arc<PagedResult>> {
        match index.checked_sub(1) {
            None => Some(&self.columns),
            Some(at) => self.sections.get(at).map(|(_, rows)| rows),
        }
    }

    /// The statement, when `index` is the DDL section.
    #[must_use]
    pub fn statement(&self, index: usize) -> Option<&Ddl> {
        let ddl = self.ddl.as_ref()?;
        (index == self.sections.len() + 1).then_some(ddl)
    }
}

/// Whether the section at `index` is the DDL, without laying anything out.
///
/// The index is a fact about where [`Laid::titles`] puts things, so it is
/// worked out here rather than in `sqlake-app` — and asked of the detail
/// rather than of a `Laid`, because the questions on the input path (is this
/// section scrolled by line or by row?) would otherwise build every section's
/// grid to answer one boolean.
#[must_use]
pub fn is_statement(detail: &TableDetail, index: usize) -> bool {
    detail.ddl.is_some() && index == detail.sections.len() + 1
}

/// How many sections the list has, which is the clamp every section index
/// wants and the one thing [`Laid::titles`] is otherwise built for.
#[must_use]
pub fn section_count(detail: &TableDetail) -> usize {
    1 + detail.sections.len() + usize::from(detail.ddl.is_some())
}

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
    definition: &Laid,
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
pub fn summary(definition: &Laid) -> String {
    let mut parts = vec![definition.kind.to_owned()];
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

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use sqlake_core::detail::{ColumnDef, DetailSection};
    use sqlake_core::node::{RelationKind, TableRef};

    use super::*;

    fn detail_of(section_titles: &[&str], ddl: bool) -> TableDetail {
        let mut detail = TableDetail::new(
            TableRef::new(["public", "users"]),
            RelationKind::Table,
            Vec::new(),
        );
        for title in section_titles {
            detail.sections.push(DetailSection {
                title: (*title).to_owned(),
                table: ResultSet::new(
                    vec![ResultColumn::new("name", "text", false)],
                    Vec::new(),
                    None,
                ),
            });
        }
        if ddl {
            detail.ddl = Some(Ddl::generated("CREATE TABLE users ()"));
        }
        detail
    }

    #[test]
    fn the_cheap_answers_are_the_same_answers() {
        // `is_statement` and `section_count` exist so the input path need not
        // lay a definition out to ask about it. Two answers to one question is
        // two answers that can disagree, so this is what says they do not.
        for titles in [&[][..], &["Indexes"][..], &["Indexes", "Triggers"][..]] {
            for ddl in [false, true] {
                let detail = detail_of(titles, ddl);
                let laid = Laid::out(&detail);
                assert_eq!(laid.titles().len(), section_count(&detail), "{titles:?}");
                for index in 0..=section_count(&detail) {
                    assert_eq!(
                        laid.statement(index).is_some(),
                        is_statement(&detail, index),
                        "section {index} of {titles:?} with ddl {ddl}"
                    );
                }
            }
        }
    }

    fn definition(section_titles: &[&str]) -> Laid {
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
                    vec![ResultColumn::new("name", "text", false)],
                    vec![Row(vec![Value::Text("a".to_owned())])],
                    None,
                ),
            });
        }
        Laid::out(&detail)
    }

    fn drawn(definition: &Laid, selected: usize, w: u16) -> (String, Rect) {
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

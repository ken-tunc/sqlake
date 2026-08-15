//! The result grid.
//!
//! Only the cells on screen are ever formatted. [`RenderedGrid`] formats on
//! access for exactly this reason: a 200k-row relation is drawn by asking for
//! the thirty rows that fit, and the other 199,970 cost nothing.
//!
//! Three kinds of hit target share the header row — the header itself sorts,
//! the seam between two headers resizes, and neither is the cell below. Getting
//! that layering wrong is how a drag to widen a column becomes a sort.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use sqlake_app::snapshot::{LoadState, PreviewTab};
use sqlake_core::result::{Sort, SortDir};

use crate::chrome;
use crate::grid::{Align, CellKind, RenderedGrid};
use crate::hit::{HitMap, PaneId, Target, Z_CHROME, Z_CONTENT, grab_area};
use crate::ui::GridUi;

/// One blank column between neighbours, so values do not touch.
const GAP: u16 = 1;

/// The rows of the grid: the pane's inside without the header row or the
/// column the scrollbar keeps.
///
/// The caller records *this* as the pane's viewport. Measured against the pane
/// instead, a page is one row too tall and `ScrollToEnd` stops a row short of
/// the end, so the last row of a relation can never be reached.
#[must_use]
pub fn body_area(area: Rect) -> Rect {
    Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width.saturating_sub(1),
        area.height.saturating_sub(1),
    )
}

/// Draw the preview into `area`, the inside of the grid pane.
///
/// `ui` is taken mutably because the rendered grid is built here, from the rows
/// in the snapshot, and cached for the frames that follow.
pub fn render(
    frame: &mut Frame<'_>,
    hits: &mut HitMap,
    area: Rect,
    tab: &PreviewTab,
    ui: &mut GridUi,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    match &tab.data {
        LoadState::Idle => message(frame, area, "select a table", Color::DarkGray),
        LoadState::Loading => message(frame, area, "loading…", Color::Yellow),
        LoadState::Failed(why) => message(frame, area, why, Color::Red),
        LoadState::Ready(rows) => {
            // Built through the `&mut`, then read through a shared reborrow.
            // Cloning it instead would reallocate every column name on every
            // frame, which is the cost this module exists to avoid.
            ui.grid(rows);
            let ui = &*ui;
            let Some(grid) = ui.rendered() else {
                return;
            };
            if grid.is_empty() {
                // The columns are still worth drawing: an empty relation with
                // its headers reads as "nothing here", and one without reads
                // as "nothing happened". The header goes down first so the
                // message lands under it rather than being painted over.
                header(frame, hits, area, grid, ui, tab.sort);
                if area.height > 1 {
                    let below = Rect::new(
                        area.x,
                        area.y.saturating_add(1),
                        area.width,
                        area.height - 1,
                    );
                    message(frame, below, "no rows", Color::DarkGray);
                }
                return;
            }
            body(frame, hits, area, grid, ui, tab.sort);
        }
    }
}

fn message(frame: &mut Frame<'_>, area: Rect, text: &str, colour: Color) {
    frame.render_widget(
        Paragraph::new(format!(" {} ", crate::grid::sanitise(text))).style(Style::new().fg(colour)),
        area,
    );
}

/// Columns that fit, as `(index, x, width)`, starting from the horizontal
/// offset.
fn visible_columns(area: Rect, grid: &RenderedGrid, ui: &GridUi) -> Vec<(usize, u16, u16)> {
    let mut out = Vec::new();
    let mut x = area.x;
    for (index, column) in grid.columns().iter().enumerate().skip(ui.col_offset) {
        if x >= area.right() {
            break;
        }
        let wanted = ui.width(index, column.natural_width);
        // The last column is cut to the edge rather than dropped: a column
        // that is half-visible can still be scrolled to, and dropping it
        // leaves a band of empty pane beside the data.
        let width = wanted.min(area.right() - x);
        out.push((index, x, width));
        x = x.saturating_add(width).saturating_add(GAP);
    }
    out
}

/// The glyph drawn in a sorted column's header.
const fn arrow(dir: SortDir) -> &'static str {
    match dir {
        SortDir::Asc => "▲",
        SortDir::Desc => "▼",
    }
}

fn header(
    frame: &mut Frame<'_>,
    hits: &mut HitMap,
    area: Rect,
    grid: &RenderedGrid,
    ui: &GridUi,
    sort: Option<Sort>,
) {
    let row = Rect::new(area.x, area.y, area.width, 1);
    frame.render_widget(Paragraph::new("").style(Style::new().bg(Color::Black)), row);

    for (index, x, width) in visible_columns(area, grid, ui) {
        let Some(column) = grid.columns().get(index) else {
            continue;
        };
        let sorted = sort.filter(|s| s.column == index);
        let marker = sorted.map_or("", |s| arrow(s.dir));

        let rect = Rect::new(x, area.y, width, 1);
        hits.push(rect, Z_CONTENT, Target::GridHeader { col: index });

        // Terminal columns, not bytes: the arrow is three bytes and one cell,
        // and measuring it in bytes eats two cells of the name — enough to cut
        // a five-cell header down to `a…`.
        let text = chrome::fit(
            &column.name,
            width.saturating_sub(crate::grid::display_width(marker)),
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(text),
                Span::styled(marker, Style::new().fg(Color::Cyan)),
            ]))
            .style(
                Style::new()
                    .fg(if sorted.is_some() {
                        Color::Cyan
                    } else {
                        Color::Gray
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            rect,
        );

        // The seam belongs to the drag, and it is pushed after the header so
        // it wins where they overlap: a drag that started on the edge must not
        // be read as a click on the header and reorder the whole relation.
        let seam = Rect::new(x.saturating_add(width), area.y, GAP, 1);
        if seam.right() <= area.right() {
            hits.push(
                grab_area(seam),
                Z_CHROME,
                Target::GridColEdge { col: index },
            );
        }
    }
}

fn body(
    frame: &mut Frame<'_>,
    hits: &mut HitMap,
    area: Rect,
    grid: &RenderedGrid,
    ui: &GridUi,
    sort: Option<Sort>,
) {
    // One column is left for the scrollbar so a wide value is cut rather than
    // hidden behind the thumb.
    let bar = area.width > 1;
    let content = if bar {
        Rect::new(area.x, area.y, area.width - 1, area.height)
    } else {
        area
    };

    header(frame, hits, content, grid, ui, sort);

    let rows_area = Rect::new(
        content.x,
        content.y.saturating_add(1),
        content.width,
        content.height.saturating_sub(1),
    );
    let columns = visible_columns(rows_area, grid, ui);
    let page = rows_area.height as usize;
    let last = grid.row_count().min(ui.row_offset.saturating_add(page));

    for (line, index) in (ui.row_offset..last).enumerate() {
        let y = rows_area.y + u16::try_from(line).unwrap_or(u16::MAX);
        for &(col, x, width) in &columns {
            let rect = Rect::new(x, y, width, 1);
            hits.push(rect, Z_CONTENT, Target::GridCell { row: index, col });

            // Formatted here and nowhere else: this is the call the whole lazy
            // arrangement exists to keep down to what is on screen.
            let cell = grid.cell(index, col);
            let selected = ui.row == index && ui.col == col;
            let align = grid.columns().get(col).map_or(Align::Left, |c| c.align);

            frame.render_widget(
                Paragraph::new(pad(&chrome::fit(&cell.text, width), width, align))
                    .style(style_for(cell.kind, selected)),
                rect,
            );
        }
    }

    if bar {
        // The full width of the pane, not the content's: the column held back
        // above is the one at the right edge, and handing the scrollbar the
        // narrowed rectangle draws the thumb one cell to its left — over the
        // value the reserved column existed to keep clear, leaving a dead
        // stripe down the edge.
        chrome::scrollbar(
            frame,
            hits,
            Rect::new(area.x, rows_area.y, area.width, rows_area.height),
            PaneId::Grid,
            ui.row_offset,
            grid.row_count(),
        );
    }
}

/// Right-aligned columns line their digits up, which is the only way a column
/// of numbers can be compared by eye.
fn pad(text: &str, width: u16, align: Align) -> String {
    let used = crate::grid::display_width(text);
    let room = usize::from(width.saturating_sub(used));
    match align {
        Align::Left => format!("{text}{}", " ".repeat(room)),
        Align::Right => format!("{}{text}", " ".repeat(room)),
    }
}

fn style_for(kind: CellKind, selected: bool) -> Style {
    let base = match kind {
        // A null is dimmed against the pane, but the selection's background is
        // that same dark grey: left alone, moving the cursor onto a null makes
        // the ∅ vanish and the cell reads as an empty string instead.
        CellKind::Null if selected => Style::new().fg(Color::Gray),
        CellKind::Null => Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        CellKind::Number => Style::new().fg(Color::LightBlue),
        CellKind::Text => Style::new(),
        CellKind::Complex => Style::new().fg(Color::Magenta),
        CellKind::Opaque => Style::new().fg(Color::Yellow),
    };
    if selected {
        base.bg(Color::DarkGray).add_modifier(Modifier::BOLD)
    } else {
        base
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;
    use sqlake_app::PagedResult;
    use sqlake_core::node::TableRef;
    use sqlake_core::result::{Column, ResultSet, Row};
    use sqlake_core::value::Value;

    use super::*;
    use crate::hit::ScrollPart;

    fn paged(columns: Vec<Column>, rows: Vec<Row>) -> Arc<PagedResult> {
        Arc::new(PagedResult::new(&ResultSet::new(columns, rows, None)))
    }

    fn numbers(rows: usize, cols: usize) -> Arc<PagedResult> {
        paged(
            (0..cols)
                .map(|c| Column::new(format!("c{c}"), "int8", false))
                .collect(),
            (0..rows)
                .map(|r| Row((0..cols).map(|c| Value::Int((r * 10 + c) as i64)).collect()))
                .collect(),
        )
    }

    fn tab(data: LoadState<Arc<PagedResult>>, sort: Option<Sort>) -> PreviewTab {
        PreviewTab {
            table: TableRef::new(["public", "users"]),
            sort,
            loaded_rows: 0,
            data,
        }
    }

    fn draw(
        tab: &PreviewTab,
        ui: &mut GridUi,
        w: u16,
        h: u16,
    ) -> (Vec<String>, HitMap, ratatui::buffer::Buffer) {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut hits = HitMap::new();
        terminal
            .draw(|frame| render(frame, &mut hits, Rect::new(0, 0, w, h), tab, ui))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows = (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect();
        (rows, hits, buffer)
    }

    #[test]
    fn only_the_rows_on_screen_are_formatted() {
        // The point of the lazy arrangement. A relation far larger than the
        // pane must cost the pane, not the relation.
        let rows = numbers(200_000, 2);
        let mut ui = GridUi::default();
        let (text, hits, _) = draw(&tab(LoadState::Ready(rows), None), &mut ui, 40, 6);

        assert!(text[1].contains('0'), "{:?}", text[1]);
        let cells = (0..40)
            .flat_map(|x| (0..6).map(move |y| (x, y)))
            .filter(|&(x, y)| matches!(hits.at(Position::new(x, y)), Some(Target::GridCell { .. })))
            .count();
        assert!(cells > 0);
        // Five body rows in a six-row pane, never two hundred thousand.
        let drawn: std::collections::BTreeSet<usize> = (0..40)
            .flat_map(|x| (0..6).map(move |y| (x, y)))
            .filter_map(|(x, y)| match hits.at(Position::new(x, y)) {
                Some(Target::GridCell { row, .. }) => Some(row),
                _ => None,
            })
            .collect();
        assert_eq!(drawn.len(), 5, "{drawn:?}");
    }

    #[test]
    fn a_header_click_and_an_edge_drag_are_different_requests() {
        let mut ui = GridUi::default();
        let (_, hits, _) = draw(&tab(LoadState::Ready(numbers(3, 3)), None), &mut ui, 40, 5);

        let header: Vec<_> = (0..40)
            .map(|x| hits.at(Position::new(x, 0)))
            .filter_map(|t| match t {
                Some(Target::GridHeader { col }) => Some(("h", col)),
                Some(Target::GridColEdge { col }) => Some(("e", col)),
                _ => None,
            })
            .collect();
        assert!(header.iter().any(|(k, _)| *k == "h"), "{header:?}");
        // Without the seam, a drag to widen a column reads as a click and
        // reorders the whole relation instead.
        assert!(header.iter().any(|(k, _)| *k == "e"), "{header:?}");
    }

    #[test]
    fn the_seam_wins_where_it_overlaps_the_header() {
        let mut ui = GridUi::default();
        let (_, hits, _) = draw(&tab(LoadState::Ready(numbers(3, 3)), None), &mut ui, 40, 5);

        let first_edge = (0..40)
            .find(|&x| {
                matches!(
                    hits.at(Position::new(x, 0)),
                    Some(Target::GridColEdge { .. })
                )
            })
            .expect("no edge");
        assert!(matches!(
            hits.at(Position::new(first_edge, 0)),
            Some(Target::GridColEdge { .. })
        ));
    }

    #[test]
    fn the_sorted_column_says_which_way() {
        let mut ui = GridUi::default();
        let sorted = Some(Sort::new(1, SortDir::Desc));
        let (text, _, _) = draw(
            &tab(LoadState::Ready(numbers(3, 3)), sorted),
            &mut ui,
            40,
            5,
        );
        assert!(text[0].contains('▼'), "{:?}", text[0]);
        assert!(!text[0].contains('▲'), "{:?}", text[0]);
        // The arrow takes one cell from the name, not the three its bytes
        // would suggest — otherwise the column being sorted is the one whose
        // name the user can no longer read.
        assert!(text[0].contains("c1"), "{:?}", text[0]);
    }

    #[test]
    fn the_scrollbar_sits_in_the_column_reserved_for_it() {
        // The pane holds a column back so a wide value is cut rather than
        // hidden behind the thumb. Drawing the bar one cell to the left of it
        // spends that column twice: the thumb still covers a value, and the
        // edge of the pane is a stripe nothing can ever be drawn in.
        let mut ui = GridUi::default();
        let (_, hits, buffer) = draw(
            &tab(LoadState::Ready(numbers(500, 1)), None),
            &mut ui,
            30,
            6,
        );

        assert_eq!(buffer[(29, 1)].symbol(), "█");
        assert_eq!(
            hits.at(Position::new(29, 1)),
            Some(Target::Scrollbar {
                pane: PaneId::Grid,
                part: ScrollPart::Thumb,
            })
        );
        // And the header row above it is not part of the track: dragging the
        // thumb to the top must reach row zero, not one row short of it.
        assert!(!matches!(
            hits.at(Position::new(29, 0)),
            Some(Target::Scrollbar { .. })
        ));
    }

    #[test]
    fn numbers_are_right_aligned_so_the_digits_line_up() {
        let rows = paged(
            vec![Column::new("n", "int8", false)],
            vec![Row(vec![Value::Int(7)]), Row(vec![Value::Int(1_000_000)])],
        );
        let mut ui = GridUi::default();
        let (text, _, _) = draw(&tab(LoadState::Ready(rows), None), &mut ui, 20, 4);

        let seven = text[1].find('7').unwrap();
        let million_end = text[2].rfind('0').unwrap();
        assert_eq!(seven, million_end, "{:?} vs {:?}", text[1], text[2]);
    }

    #[test]
    fn the_selected_cell_is_marked() {
        // Field-by-field rather than a functional update: `GridUi` keeps its
        // cache private, so `..Default::default()` cannot see past it.
        let mut ui = GridUi::default();
        ui.row = 1;
        ui.col = 1;
        let (_, hits, buffer) = draw(&tab(LoadState::Ready(numbers(3, 3)), None), &mut ui, 40, 5);

        // Find the cell by asking where it was drawn rather than assuming.
        let find = |want_row: usize, want_col: usize| {
            (0..40)
                .flat_map(|x| (0..5).map(move |y| (x, y)))
                .find(|&(x, y)| {
                    matches!(
                        hits.at(Position::new(x, y)),
                        Some(Target::GridCell { row, col }) if row == want_row && col == want_col
                    )
                })
                .expect("cell not drawn")
        };
        let (sx, sy) = find(1, 1);
        let (nx, ny) = find(0, 1);
        assert_ne!(
            buffer[(sx, sy)].style().bg,
            buffer[(nx, ny)].style().bg,
            "a selection recorded and not drawn is not a selection"
        );
    }

    #[test]
    fn scrolling_moves_the_window_and_keeps_the_row_indices() {
        let mut ui = GridUi::default();
        ui.row_offset = 100;
        let (_, hits, _) = draw(
            &tab(LoadState::Ready(numbers(500, 2)), None),
            &mut ui,
            40,
            5,
        );

        let first = (0..40)
            .find_map(|x| match hits.at(Position::new(x, 1)) {
                Some(Target::GridCell { row, .. }) => Some(row),
                _ => None,
            })
            .expect("no cell");
        // The index is the row in the relation, not the line on screen: a
        // click after scrolling must select what it looks like it selected.
        assert_eq!(first, 100);
    }

    #[test]
    fn scrolling_sideways_starts_from_a_later_column() {
        let mut ui = GridUi::default();
        ui.col_offset = 5;
        let (_, hits, _) = draw(&tab(LoadState::Ready(numbers(3, 20)), None), &mut ui, 40, 5);

        let first = (0..40)
            .find_map(|x| match hits.at(Position::new(x, 0)) {
                Some(Target::GridHeader { col }) => Some(col),
                _ => None,
            })
            .expect("no header");
        assert_eq!(first, 5);
    }

    #[test]
    fn a_resized_column_is_laid_out_at_the_width_it_was_given() {
        let rows = numbers(3, 3);
        let mut ui = GridUi::default();
        let grid = ui.grid(&rows).clone();
        let area = Rect::new(0, 0, 60, 5);

        // Asked of the function that decides the widths. Reading them back out
        // of the hit map instead measures the resize seam as well, because it
        // is deliberately drawn over the end of the header it belongs to.
        let was = visible_columns(area, &grid, &ui)[0].2;
        ui.set_width(0, was + 6);

        let after = visible_columns(area, &grid, &ui);
        assert_eq!(after[0].2, was + 6);
        assert_eq!(
            after[1].1,
            after[0].1 + was + 6 + GAP,
            "the column beside it moves over by the same amount"
        );
    }

    #[test]
    fn a_column_is_cut_at_the_pane_edge_rather_than_dropped() {
        let rows = numbers(3, 3);
        let mut ui = GridUi::default();
        let grid = ui.grid(&rows).clone();
        ui.set_width(0, 100);

        let area = Rect::new(0, 0, 20, 5);
        let columns = visible_columns(area, &grid, &ui);
        assert_eq!(columns.len(), 1, "{columns:?}");
        // Half a column can still be scrolled to; dropping it leaves a band of
        // empty pane beside the data.
        assert_eq!(columns[0].2, 20);
    }

    #[test]
    fn a_selected_null_is_still_legible() {
        let rows = paged(
            vec![Column::new("v", "text", true)],
            vec![Row(vec![Value::Null]), Row(vec![Value::Null])],
        );
        let mut ui = GridUi::default();
        ui.row = 0;
        ui.col = 0;
        let (_, hits, buffer) = draw(&tab(LoadState::Ready(rows), None), &mut ui, 20, 4);

        let at = |want: usize| {
            (0..20)
                .flat_map(|x| (0..4).map(move |y| (x, y)))
                .find(|&(x, y)| {
                    matches!(
                        hits.at(Position::new(x, y)),
                        Some(Target::GridCell { row, .. }) if row == want
                    )
                })
                .expect("cell not drawn")
        };
        let (sx, sy) = at(0);
        let (nx, ny) = at(1);

        assert_eq!(buffer[(sx, sy)].symbol(), crate::grid::NULL_GLYPH);
        // Dimmed dark grey on the selection's dark grey background is the glyph
        // disappearing, which is the confusion NULL_GLYPH exists to prevent.
        assert_ne!(
            buffer[(sx, sy)].style().fg,
            buffer[(sx, sy)].style().bg,
            "the selected null is invisible"
        );
        assert_eq!(
            buffer[(nx, ny)].symbol(),
            crate::grid::NULL_GLYPH,
            "and the unselected one is unchanged"
        );
    }

    #[test]
    fn every_load_state_says_something() {
        let mut ui = GridUi::default();
        for (state, expected) in [
            (LoadState::Idle, "select a table"),
            (LoadState::Loading, "loading"),
            (LoadState::Failed("boom".into()), "boom"),
        ] {
            let (text, _, _) = draw(&tab(state, None), &mut ui, 40, 5);
            assert!(text[0].contains(expected), "{:?}", text[0]);
        }
    }

    #[test]
    fn an_empty_relation_still_shows_its_columns() {
        let rows = paged(
            vec![
                Column::new("id", "int8", false),
                Column::new("v", "text", true),
            ],
            Vec::new(),
        );
        let mut ui = GridUi::default();
        let (text, _, _) = draw(&tab(LoadState::Ready(rows), None), &mut ui, 40, 5);
        // Headers without rows read as "nothing here"; nothing at all reads as
        // "nothing happened".
        assert!(text[0].contains("id"), "{:?}", text[0]);
        assert!(text.iter().any(|l| l.contains("no rows")), "{text:?}");
    }

    #[test]
    fn a_cut_value_says_that_it_was_cut() {
        let rows = paged(
            vec![
                Column::new("a", "text", false),
                Column::new("b", "text", false),
            ],
            vec![Row(vec![
                Value::Text("x".repeat(100)),
                Value::Text("edge".into()),
            ])],
        );
        let mut ui = GridUi::default();
        ui.set_width(0, 6);
        let (text, _, _) = draw(&tab(LoadState::Ready(rows), None), &mut ui, 30, 4);

        // Not that it stays inside its column — ratatui clips it there whether
        // anything asks or not, so asserting that tests ratatui. The ellipsis
        // is the part only this code can add, and without it a value that was
        // cut reads as a value that was complete.
        assert!(text[1].starts_with("xxxxx…"), "{:?}", text[1]);
        assert!(text[1].contains("edge"), "{:?}", text[1]);
    }

    #[test]
    fn a_double_width_value_is_cut_on_a_character_boundary() {
        let rows = paged(
            vec![
                Column::new("a", "text", false),
                Column::new("b", "text", false),
            ],
            vec![Row(vec![
                Value::Text("日本語日本語".into()),
                Value::Text("end".into()),
            ])],
        );
        let mut ui = GridUi::default();
        ui.set_width(0, 6);
        let (text, _, buffer) = draw(&tab(LoadState::Ready(rows), None), &mut ui, 30, 4);

        // Read cell by cell: each double-width character occupies two, so the
        // six-cell column holds exactly two of them and the ellipsis. Measured
        // in characters it would have fitted five and left the sixth broken
        // across the boundary.
        assert_eq!(buffer[(0, 1)].symbol(), "日");
        assert_eq!(buffer[(2, 1)].symbol(), "本");
        assert_eq!(buffer[(4, 1)].symbol(), "…");
        assert!(text[1].contains("end"), "{:?}", text[1]);
    }

    #[test]
    fn a_pane_too_short_for_a_body_still_draws_the_header() {
        let mut ui = GridUi::default();
        let (text, _, _) = draw(&tab(LoadState::Ready(numbers(10, 2)), None), &mut ui, 30, 1);
        assert!(text[0].contains("c0"), "{:?}", text[0]);
    }
}

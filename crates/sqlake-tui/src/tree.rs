//! The object explorer.
//!
//! The store publishes the tree already flattened, so drawing it is a slice and
//! an index rather than a recursive walk — and a row's position in that slice
//! is the only identity the mouse needs.
//!
//! The toggle is a separate hit target from the row. Clicking a schema's name
//! and clicking its `▸` are different requests, and merging them makes one of
//! the two impossible to express.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use sqlake_app::tree::{NodeState, TreeView, VisibleNode};
use sqlake_core::node::RelationKind;

use crate::chrome;
use crate::grid::{display_width, sanitise};
use crate::hit::{HitMap, PaneId, Target, Z_CONTENT};
use crate::ui::TreeUi;

/// Cells of indent per level.
const INDENT: u16 = 2;

/// The toggle column, wide enough to hit and to hold the widest glyph.
const TOGGLE_WIDTH: u16 = 2;

/// Draw the explorer's contents into `area`, which is the inside of its pane.
pub fn render(frame: &mut Frame<'_>, hits: &mut HitMap, area: Rect, view: &TreeView, ui: &TreeUi) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    if view.is_empty() {
        frame.render_widget(
            Paragraph::new(" nothing connected ").style(Style::new().fg(Color::DarkGray)),
            area,
        );
        return;
    }

    // The scrollbar takes a column from the rows rather than covering them, so
    // a long label is truncated instead of being hidden behind the thumb.
    let bar = area.width > 1;
    let rows_area = if bar {
        Rect::new(area.x, area.y, area.width - 1, area.height)
    } else {
        area
    };

    let height = rows_area.height as usize;
    for (line, index) in (ui.offset..view.len().min(ui.offset.saturating_add(height))).enumerate() {
        let Some(node) = view.get(index) else {
            break;
        };
        let y = rows_area.y + u16::try_from(line).unwrap_or(u16::MAX);
        let row = Rect::new(rows_area.x, y, rows_area.width, 1);
        render_row(frame, hits, row, node, index, ui.selected == Some(index));
    }

    if bar {
        chrome::scrollbar(frame, hits, area, PaneId::Explorer, ui.offset, view.len());
    }
}

fn render_row(
    frame: &mut Frame<'_>,
    hits: &mut HitMap,
    row: Rect,
    node: &VisibleNode,
    index: usize,
    selected: bool,
) {
    hits.push(row, Z_CONTENT, Target::TreeRow { index });

    let indent = node.depth.saturating_mul(INDENT).min(row.width);
    let toggle_x = row.x + indent;

    // Only a node that can be expanded gets a toggle target. A relation has no
    // children, and offering a control that does nothing is worse than offering
    // none: the user learns the tree is unreliable rather than that the node is
    // a leaf.
    if node.state.is_toggleable() && toggle_x + TOGGLE_WIDTH <= row.right() {
        hits.push(
            Rect::new(toggle_x, row.y, TOGGLE_WIDTH, 1),
            Z_CONTENT,
            Target::TreeToggle { index },
        );
    }

    let (glyph, glyph_style) = toggle(&node.state);
    let base = if selected {
        Style::new()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };

    // The icon is dimmer than the label, but dark grey on the selection's dark
    // grey background is the same colour: the kind of a row would disappear the
    // moment it was picked.
    let dim = if selected {
        Color::Gray
    } else {
        Color::DarkGray
    };
    let icon = icon(node.relation_kind);
    let label = sanitise(&node.label);

    let mut spans = vec![
        Span::styled(" ".repeat(indent as usize), base),
        Span::styled(glyph, glyph_style.patch(base)),
        Span::styled(icon, base.fg(dim)),
    ];

    let used = indent + TOGGLE_WIDTH + display_width(icon);
    let room = row.width.saturating_sub(used);
    spans.push(Span::styled(
        chrome::fit(&label, room),
        base.fg(label_colour(&node.state)),
    ));

    if let NodeState::Failed(message) = &node.state {
        // The reason belongs on the row that failed. A toast would be gone by
        // the time the user looks, and the node would just sit there.
        let so_far = used + display_width(&label);
        let left = row.width.saturating_sub(so_far);
        if left > 3 {
            spans.push(Span::styled(
                chrome::fit(&format!("  {}", sanitise(message)), left),
                Style::new().fg(Color::Red),
            ));
        }
    }

    frame.render_widget(Paragraph::new(Line::from(spans)).style(base), row);
}

fn toggle(state: &NodeState) -> (&'static str, Style) {
    match state {
        NodeState::Leaf => ("  ", Style::new()),
        NodeState::Collapsed => ("▸ ", Style::new().fg(Color::Gray)),
        NodeState::Expanded => ("▾ ", Style::new().fg(Color::Gray)),
        // A node that is fetching says so where the toggle was, so the answer
        // to "did my click do anything" is in the place that was clicked.
        NodeState::Loading => ("⋯ ", Style::new().fg(Color::Yellow)),
        NodeState::Failed(_) => ("! ", Style::new().fg(Color::Red)),
    }
}

const fn icon(kind: Option<RelationKind>) -> &'static str {
    match kind {
        None => "",
        Some(RelationKind::Table) => "▦ ",
        Some(RelationKind::View) => "◫ ",
        Some(RelationKind::MaterializedView) => "▩ ",
        Some(RelationKind::Routine) => "ƒ ",
        Some(RelationKind::External) => "↗ ",
    }
}

const fn label_colour(state: &NodeState) -> Color {
    match state {
        NodeState::Failed(_) => Color::Red,
        NodeState::Loading => Color::Gray,
        _ => Color::Reset,
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Position;
    use sqlake_core::node::{NodeKind, NodeRef};

    use super::*;

    fn node(depth: u16, label: &str, state: NodeState) -> VisibleNode {
        VisibleNode {
            depth,
            label: label.into(),
            node_ref: NodeRef::new(NodeKind::Namespace, [label]),
            relation_kind: None,
            state,
        }
    }

    fn relation(depth: u16, label: &str) -> VisibleNode {
        VisibleNode {
            relation_kind: Some(RelationKind::Table),
            ..node(depth, label, NodeState::Leaf)
        }
    }

    /// The screen as one string per row. Whether a row was cut to the pane is
    /// only answerable per row: run together, an overflowing row is
    /// indistinguishable from the one below it.
    ///
    /// One char per cell, so a char index is a column: the trailing cell of a
    /// double-width glyph holds a space of its own.
    fn draw_rows(view: &TreeView, ui: &TreeUi, w: u16, h: u16) -> (Vec<String>, HitMap) {
        let (buffer, hits) = draw_buffer(view, ui, w, h);
        let rows = (0..h)
            .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        (rows, hits)
    }

    fn draw_buffer(view: &TreeView, ui: &TreeUi, w: u16, h: u16) -> (Buffer, HitMap) {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut hits = HitMap::new();
        terminal
            .draw(|frame| render(frame, &mut hits, Rect::new(0, 0, w, h), view, ui))
            .unwrap();
        (terminal.backend().buffer().clone(), hits)
    }

    fn draw(view: &TreeView, ui: &TreeUi, w: u16, h: u16) -> (String, HitMap) {
        let (rows, hits) = draw_rows(view, ui, w, h);
        (rows.concat(), hits)
    }

    fn tree(nodes: Vec<VisibleNode>) -> TreeView {
        TreeView { nodes }
    }

    #[test]
    fn every_visible_row_can_be_clicked() {
        let view = tree(vec![
            node(0, "public", NodeState::Expanded),
            relation(1, "users"),
            relation(1, "orders"),
        ]);
        let (_, hits) = draw(&view, &TreeUi::default(), 30, 5);

        for (y, expected) in (0..3).map(|y| (y, y as usize)) {
            assert_eq!(
                hits.at(Position::new(20, y)),
                Some(Target::TreeRow { index: expected }),
                "row {y}"
            );
        }
    }

    #[test]
    fn the_toggle_is_a_target_of_its_own() {
        let view = tree(vec![node(0, "public", NodeState::Collapsed)]);
        let (_, hits) = draw(&view, &TreeUi::default(), 30, 3);

        // The glyph itself expands; the name beside it selects. Merging the two
        // would make one of them impossible to ask for.
        assert_eq!(
            hits.at(Position::new(0, 0)),
            Some(Target::TreeToggle { index: 0 })
        );
        assert_eq!(
            hits.at(Position::new(10, 0)),
            Some(Target::TreeRow { index: 0 })
        );
    }

    #[test]
    fn a_leaf_offers_no_toggle_to_press() {
        let view = tree(vec![relation(0, "users")]);
        let (_, hits) = draw(&view, &TreeUi::default(), 30, 3);
        assert_eq!(
            hits.at(Position::new(0, 0)),
            Some(Target::TreeRow { index: 0 }),
            "a control that does nothing teaches the tree is unreliable"
        );
    }

    #[test]
    fn scrolling_moves_the_window_and_the_indices_with_it() {
        let view = tree(
            (0..20)
                .map(|i| node(0, &format!("n{i}"), NodeState::Collapsed))
                .collect(),
        );
        let ui = TreeUi {
            offset: 15,
            selected: None,
        };
        let (text, hits) = draw(&view, &ui, 30, 3);

        assert!(text.contains("n15"), "{text:?}");
        assert!(!text.contains("n14"), "{text:?}");
        // The index carried by the hit is the index in the whole tree, not the
        // row on screen, or a click after scrolling opens the wrong node.
        assert_eq!(
            hits.at(Position::new(20, 0)),
            Some(Target::TreeRow { index: 15 })
        );
    }

    #[test]
    fn depth_is_visible_as_indentation() {
        let view = tree(vec![
            node(0, "db", NodeState::Expanded),
            node(1, "public", NodeState::Expanded),
            relation(2, "users"),
        ]);
        let (text, hits) = draw(&view, &TreeUi::default(), 30, 3);
        assert!(text.contains("db"), "{text:?}");

        // The toggle of a deeper node sits further right, which is what makes
        // the nesting readable at a glance.
        assert_eq!(
            hits.at(Position::new(2, 1)),
            Some(Target::TreeToggle { index: 1 })
        );
        assert_eq!(
            hits.at(Position::new(0, 1)),
            Some(Target::TreeRow { index: 1 })
        );
    }

    #[test]
    fn a_loading_node_says_so_where_it_was_clicked() {
        let view = tree(vec![node(0, "slow", NodeState::Loading)]);
        let (text, _) = draw(&view, &TreeUi::default(), 30, 3);
        assert!(text.contains('⋯'), "{text:?}");
    }

    #[test]
    fn a_failure_is_reported_on_the_row_that_failed() {
        let view = tree(vec![node(
            0,
            "restricted",
            NodeState::Failed("permission denied".into()),
        )]);
        let (text, _) = draw(&view, &TreeUi::default(), 40, 3);
        // Not a toast: by the time the user looks at the tree the toast is
        // gone and the node is just sitting there.
        assert!(text.contains("permission denied"), "{text:?}");
    }

    #[test]
    fn an_empty_tree_says_why_it_is_empty() {
        let (text, _) = draw(&TreeView::default(), &TreeUi::default(), 30, 3);
        assert!(text.contains("nothing connected"), "{text:?}");
    }

    #[test]
    fn a_long_label_is_cut_rather_than_spilling_into_the_scrollbar() {
        let view = tree(vec![node(
            0,
            "a_very_long_relation_name_indeed",
            NodeState::Collapsed,
        )]);
        let (rows, _) = draw_rows(&view, &TreeUi::default(), 16, 3);
        let first: Vec<char> = rows[0].chars().collect();
        // Cut with an ellipsis rather than run off the edge and clipped: the
        // ellipsis is what says the name goes on.
        assert_eq!(first[14], '…', "{:?}", rows[0]);
        assert_eq!(
            first[15], ' ',
            "the scrollbar's column is not the label's to use: {:?}",
            rows[0]
        );
    }

    #[test]
    fn a_double_width_label_still_fits_the_pane() {
        let view = tree(vec![node(0, "ユーザー情報テーブル", NodeState::Collapsed)]);
        let (rows, hits) = draw_rows(&view, &TreeUi::default(), 20, 3);
        // Ten characters, twenty columns. Measuring the label in characters
        // would keep four more of them than fit and run it past the edge, so
        // the ellipsis has to land in the last column the rows own.
        let first: Vec<char> = rows[0].chars().collect();
        assert_eq!(first[18], '…', "{:?}", rows[0]);
        assert_eq!(
            first[19], ' ',
            "the scrollbar's column is not the label's to use: {:?}",
            rows[0]
        );
        assert_eq!(
            hits.at(Position::new(19, 0)),
            None,
            "the scrollbar column stays clear when there is nothing to scroll"
        );
    }

    #[test]
    fn a_control_character_in_a_label_cannot_break_the_row() {
        let view = tree(vec![node(0, "we\nird\ttab", NodeState::Collapsed)]);
        let (text, _) = draw(&view, &TreeUi::default(), 30, 3);
        assert!(!text.contains('\n'), "a raw newline splits the pane open");
        assert!(text.contains('␊'), "{text:?}");
    }

    #[test]
    fn the_selected_row_is_the_one_marked() {
        let view = tree(vec![
            node(0, "a", NodeState::Collapsed),
            node(0, "b", NodeState::Collapsed),
        ]);
        let ui = TreeUi {
            offset: 0,
            selected: Some(1),
        };
        let (buffer, _) = draw_buffer(&view, &ui, 20, 3);
        assert_ne!(
            buffer[(0, 0)].style().bg,
            buffer[(0, 1)].style().bg,
            "the selection has to be visible, not just recorded"
        );
    }

    #[test]
    fn a_selected_row_does_not_paint_over_its_own_icon() {
        let view = tree(vec![relation(0, "users")]);
        let ui = TreeUi {
            offset: 0,
            selected: Some(0),
        };
        let (buffer, _) = draw_buffer(&view, &ui, 20, 3);
        // The icon follows the two-cell toggle, so it starts at column two.
        let icon = buffer[(2, 0)].style();
        assert_ne!(
            icon.fg, icon.bg,
            "an icon the colour of the row it sits on is not drawn at all"
        );
    }

    #[test]
    fn a_pane_one_column_wide_draws_nothing_it_cannot_fit() {
        let view = tree(vec![node(0, "public", NodeState::Collapsed)]);
        // Degenerate, but reachable while dragging the splitter.
        let (_, hits) = draw(&view, &TreeUi::default(), 1, 3);
        assert!(hits.at(Position::new(0, 0)).is_some());
    }
}

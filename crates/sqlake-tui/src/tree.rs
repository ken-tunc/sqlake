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
use ratatui::widgets::{Paragraph, Wrap};
use sqlake_app::snapshot::{ConnStatus, Snapshot};
#[cfg(test)]
use sqlake_app::tree::TreeView;
use sqlake_app::tree::{NodeState, VisibleNode};
use sqlake_core::node::RelationKind;
use sqlake_core::profile::ProfileColor;

use crate::chrome;
use crate::grid::{display_width, sanitise};
use crate::hit::{ButtonId, HitMap, PaneId, Target, Z_CONTENT};
use crate::ui::TreeUi;

/// Cells of indent per level.
const INDENT: u16 = 2;

/// The toggle column, wide enough to hit and to hold the widest glyph.
const TOGGLE_WIDTH: u16 = 2;

/// Which rows the filter leaves on screen, as indices into the flattened tree.
///
/// A node stays if it matches, and also if anything under it does: a table
/// called `users` with its dataset hidden would be a row with no path to it,
/// and the level names are half of what a path means. Nothing is fetched to
/// find out — the tree is lazy, and reaching into unloaded nodes would put a
/// network round trip, and on BigQuery a bill, behind every keystroke.
///
/// The match is case-insensitive and on the label alone. Matching the whole
/// path instead would make one dataset's name select every table under it,
/// which is what expanding the dataset already does.
#[must_use]
pub fn visible(nodes: &[VisibleNode], filter: Option<&str>) -> Vec<usize> {
    let Some(needle) = filter else {
        return (0..nodes.len()).collect();
    };
    // The box right after `/`, and a shortcut rather than a rule: `contains`
    // on an empty needle is true of everything, so the walk below would reach
    // the same answer. What it would also do is lower-case every label in the
    // tree, on every frame, to decide nothing.
    if needle.is_empty() {
        return (0..nodes.len()).collect();
    }
    let needle = needle.to_lowercase();

    // Backwards, because whether a node stays depends on its descendants, and
    // in a flattened tree those are the rows after it that are deeper. The
    // shallowest depth kept so far is all that has to be carried: a row is an
    // ancestor of something kept exactly when it is shallower than that, and
    // it is never *un*-kept by a row further right.
    let mut kept = Vec::new();
    let mut shallowest_kept: Option<u16> = None;
    for (index, node) in nodes.iter().enumerate().rev() {
        // `None` and not `u16::MAX`: with nothing kept yet there is no
        // descendant to be an ancestor of, and a sentinel deeper than every
        // depth answers that question the other way round — which keeps the
        // last row of the tree whatever it says.
        let holds_a_match = shallowest_kept.is_some_and(|depth| depth > node.depth);
        if holds_a_match || node.label.to_lowercase().contains(&needle) {
            kept.push(index);
            shallowest_kept = Some(shallowest_kept.map_or(node.depth, |d| d.min(node.depth)));
        }
    }
    kept.reverse();
    kept
}

/// Draw the explorer's contents into `area`, which is the inside of its pane.
///
/// `empty` is what to say when there is nothing to draw. The reason differs —
/// no config file, or a config file nobody has connected from — and the pane
/// is the only place either one can be read.
pub fn render(
    frame: &mut Frame<'_>,
    hits: &mut HitMap,
    area: Rect,
    snapshot: &Snapshot,
    ui: &TreeUi,
    filter: Option<&str>,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    // The box takes the top line whether or not anything matches: a search
    // that hid its own text as soon as it stopped matching would look like the
    // client had lost the keystroke.
    let area = match filter {
        Some(text) => {
            render_filter(frame, hits, Rect::new(area.x, area.y, area.width, 1), text);
            if area.height == 1 {
                return;
            }
            Rect::new(area.x, area.y + 1, area.width, area.height - 1)
        }
        None => area,
    };
    let view = &snapshot.explorer;
    if view.is_empty() {
        // Wrapped, because the explorer is twelve columns wide at the smallest
        // size this client draws at, and a sentence that says why the pane is
        // empty is longer than that. Truncated, it would be one more thing
        // that says nothing.
        frame.render_widget(
            Paragraph::new(waiting_for(snapshot))
                .wrap(Wrap { trim: true })
                .style(Style::new().fg(Color::DarkGray)),
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

    let rows = visible(&view.nodes, filter);
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(" no match ").style(Style::new().fg(Color::DarkGray)),
            rows_area,
        );
        return;
    }

    let height = rows_area.height as usize;
    for (line, index) in (ui.offset..rows.len().min(ui.offset.saturating_add(height))).enumerate() {
        // `index` is the row on screen and `rows[index]` is the node it stands
        // for. Every hit target is recorded with the first, because that is
        // what a later click will arrive as.
        let Some(node) = rows.get(index).and_then(|&node| view.get(node)) else {
            break;
        };
        let y = rows_area.y + u16::try_from(line).unwrap_or(u16::MAX);
        let row = Rect::new(rows_area.x, y, rows_area.width, 1);
        // A connection's own row is the one the profile colours, and the only
        // one whose label is not an object's name.
        let colour = node
            .node_ref
            .path
            .is_empty()
            .then(|| {
                snapshot
                    .connection(node.conn)
                    .and_then(|c| c.color)
                    .map(colour_of)
            })
            .flatten();
        render_row(
            frame,
            hits,
            row,
            node,
            index,
            ui.selected == Some(index),
            colour,
        );
    }

    if bar {
        chrome::scrollbar(frame, hits, area, PaneId::Explorer, ui.offset, rows.len());
    }
}

/// The search box, drawn whether or not it has anything in it yet.
fn render_filter(frame: &mut Frame<'_>, hits: &mut HitMap, row: Rect, text: &str) {
    hits.push(row, Z_CONTENT, Target::Button(ButtonId::Filter));
    let shown = chrome::fit(&sanitise(text), row.width.saturating_sub(2));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("/", Style::new().fg(Color::DarkGray)),
            Span::styled(shown, Style::new().fg(Color::White)),
            // A block where the next character goes. The terminal's own cursor
            // is parked off-screen while the TUI is up, so without this there
            // is nothing to say the box is taking input.
            Span::styled("▏", Style::new().fg(Color::Cyan)),
        ]))
        .style(Style::new().bg(Color::Black)),
        row,
    );
}

fn render_row(
    frame: &mut Frame<'_>,
    hits: &mut HitMap,
    row: Rect,
    node: &VisibleNode,
    index: usize,
    selected: bool,
    colour: Option<Color>,
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
        base.fg(colour.unwrap_or_else(|| label_colour(&node.state))),
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

/// What the explorer says when it has nothing to draw.
///
/// "Nothing connected" is only true when there is something to connect to, and
/// a fresh install has no config file at all: one of those is fixed by pressing
/// a key and the other is not.
fn waiting_for(snapshot: &Snapshot) -> &'static str {
    match snapshot.connections.first().map(|c| &c.status) {
        // A handshake can take the whole of a driver's deadline, and `c`
        // during one opens a *second* connection rather than hurrying the
        // first along.
        Some(ConnStatus::Connecting) => " connecting… ",
        _ if snapshot.profiles.is_empty() => " no connections configured — write connections.toml ",
        _ => " nothing connected — press c ",
    }
}

const fn colour_of(colour: ProfileColor) -> Color {
    match colour {
        ProfileColor::Red => Color::Red,
        ProfileColor::Yellow => Color::Yellow,
        ProfileColor::Green => Color::Green,
        ProfileColor::Blue => Color::Blue,
        ProfileColor::Magenta => Color::Magenta,
        ProfileColor::Cyan => Color::Cyan,
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
    use sqlake_driver_mock::mock_summary;

    fn node(depth: u16, label: &str, state: NodeState) -> VisibleNode {
        VisibleNode {
            conn: sqlake_core::id::ConnId::new(),
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
        let snapshot = Snapshot {
            explorer: std::sync::Arc::new(view.clone()),
            ..Snapshot::default()
        };
        draw_buffer_of(&snapshot, ui, w, h)
    }

    fn draw_snapshot(snapshot: &Snapshot, ui: &TreeUi, w: u16, h: u16) -> (String, HitMap) {
        let (buffer, hits) = draw_buffer_of(snapshot, ui, w, h);
        let text = (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .map(|(x, y)| buffer[(x, y)].symbol().to_owned())
            .collect();
        (text, hits)
    }

    fn draw_buffer_of(snapshot: &Snapshot, ui: &TreeUi, w: u16, h: u16) -> (Buffer, HitMap) {
        draw_filtered(snapshot, ui, w, h, None)
    }

    fn draw_filtered(
        snapshot: &Snapshot,
        ui: &TreeUi,
        w: u16,
        h: u16,
        filter: Option<&str>,
    ) -> (Buffer, HitMap) {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut hits = HitMap::new();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    &mut hits,
                    Rect::new(0, 0, w, h),
                    snapshot,
                    ui,
                    filter,
                );
            })
            .unwrap();
        (terminal.backend().buffer().clone(), hits)
    }

    fn draw(view: &TreeView, ui: &TreeUi, w: u16, h: u16) -> (String, HitMap) {
        let (rows, hits) = draw_rows(view, ui, w, h);
        (rows.concat(), hits)
    }

    fn flat(rows: &[(u16, &str)]) -> Vec<VisibleNode> {
        rows.iter()
            .map(|(depth, label)| VisibleNode {
                conn: sqlake_core::id::ConnId::new(),
                depth: *depth,
                label: (*label).to_owned(),
                node_ref: NodeRef::new(NodeKind::Namespace, [*label]),
                relation_kind: None,
                state: NodeState::Collapsed,
            })
            .collect()
    }

    fn kept<'a>(nodes: &'a [VisibleNode], filter: Option<&str>) -> Vec<&'a str> {
        visible(nodes, filter)
            .into_iter()
            .map(|index| nodes[index].label.as_str())
            .collect()
    }

    #[test]
    fn no_filter_and_an_empty_one_show_the_whole_tree() {
        // An empty box is the state right after `/`, and hiding everything
        // then would make the feature look broken before a letter is typed.
        let nodes = flat(&[(0, "prod"), (1, "public"), (2, "users")]);
        assert_eq!(kept(&nodes, None), ["prod", "public", "users"]);
        assert_eq!(kept(&nodes, Some("")), ["prod", "public", "users"]);
    }

    #[test]
    fn a_match_brings_its_ancestors_with_it() {
        // A table with its dataset hidden is a row with no path to it, and the
        // level names are half of what a path means.
        let nodes = flat(&[
            (0, "prod"),
            (1, "public"),
            (2, "users"),
            (2, "orders"),
            (1, "analytics"),
            (2, "events"),
        ]);
        assert_eq!(kept(&nodes, Some("orders")), ["prod", "public", "orders"]);
    }

    #[test]
    fn a_branch_with_nothing_under_it_goes() {
        // The case a naive "keep every ancestor" gets wrong: `public` is an
        // ancestor of a kept row, `analytics` is only an ancestor of the row
        // that was dropped.
        let nodes = flat(&[
            (0, "prod"),
            (1, "public"),
            (2, "users"),
            (1, "analytics"),
            (2, "events"),
        ]);
        assert_eq!(kept(&nodes, Some("users")), ["prod", "public", "users"]);
    }

    #[test]
    fn a_match_under_a_later_branch_keeps_the_root() {
        // Walking backwards, the run that kept `analytics` is interrupted by
        // rows that were dropped; the root is still an ancestor of it.
        let nodes = flat(&[
            (0, "prod"),
            (1, "public"),
            (2, "users"),
            (1, "analytics"),
            (2, "hits"),
        ]);
        assert_eq!(kept(&nodes, Some("hits")), ["prod", "analytics", "hits"]);
    }

    #[test]
    fn a_branch_that_matches_keeps_what_is_under_it() {
        // Matching a dataset shows the dataset, not its tables: opening it is
        // what shows those, and a filter that expanded on a keystroke is the
        // one thing the tree is lazy to avoid.
        let nodes = flat(&[(0, "prod"), (1, "public"), (2, "users")]);
        assert_eq!(kept(&nodes, Some("public")), ["prod", "public"]);
    }

    #[test]
    fn every_connection_is_searched_and_the_ones_that_miss_go() {
        let nodes = flat(&[(0, "prod"), (1, "public"), (0, "staging"), (1, "reporting")]);
        assert_eq!(kept(&nodes, Some("report")), ["staging", "reporting"]);
    }

    #[test]
    fn the_match_is_case_insensitive() {
        let nodes = flat(&[(0, "prod"), (1, "Users")]);
        assert_eq!(kept(&nodes, Some("users")), ["prod", "Users"]);
        assert_eq!(kept(&nodes, Some("USERS")), ["prod", "Users"]);
    }

    #[test]
    fn a_filter_that_matches_nothing_keeps_nothing() {
        let nodes = flat(&[(0, "prod"), (1, "public")]);
        assert!(kept(&nodes, Some("zzz")).is_empty());
    }

    #[test]
    fn the_box_is_drawn_even_when_nothing_matches() {
        // Otherwise the pane goes blank as soon as the search stops matching,
        // and the text that caused it goes with it.
        let view = tree(flat(&[(0, "prod"), (1, "public")]));
        let snapshot = Snapshot {
            explorer: std::sync::Arc::new(view),
            ..Snapshot::default()
        };
        let (buffer, _) = draw_filtered(&snapshot, &TreeUi::default(), 20, 4, Some("zzz"));
        let text: String = (0..4)
            .flat_map(|y| (0..20).map(move |x| (x, y)))
            .map(|(x, y)| buffer[(x, y)].symbol().to_owned())
            .collect();
        assert!(text.contains("zzz"), "{text}");
        assert!(text.contains("no match"), "{text}");
    }

    #[test]
    fn a_row_is_hit_by_where_it_is_on_screen() {
        // The filter renumbers the rows, and a hit target carrying the node's
        // position in the unfiltered tree would open whatever happened to sit
        // at that index.
        let view = tree(flat(&[
            (0, "prod"),
            (1, "public"),
            (2, "users"),
            (1, "analytics"),
            (2, "hits"),
        ]));
        let snapshot = Snapshot {
            explorer: std::sync::Arc::new(view),
            ..Snapshot::default()
        };
        let (_, hits) = draw_filtered(&snapshot, &TreeUi::default(), 20, 5, Some("hits"));
        // Row 0 is the box; `prod`, `analytics`, `hits` follow.
        assert_eq!(
            hits.at(Position::new(1, 3)),
            Some(Target::TreeRow { index: 2 })
        );
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
    fn an_empty_tree_says_which_kind_of_empty_it_is() {
        // Two different problems that look identical on screen: nothing to
        // connect to, and nothing connected yet. Only one is fixed by pressing
        // a key.
        let (text, _) = draw(&TreeView::default(), &TreeUi::default(), 30, 3);
        assert!(text.contains("no connections configured"), "{text:?}");

        let configured = Snapshot {
            profiles: std::sync::Arc::new(vec![mock_summary("mock")]),
            ..Snapshot::default()
        };
        let (text, _) = draw_snapshot(&configured, &TreeUi::default(), 30, 3);
        assert!(text.contains("nothing connected"), "{text:?}");
    }

    #[test]
    fn a_profiles_colour_marks_its_connection_and_nothing_below_it() {
        // The point of the colour is that production does not look like a
        // scratch database. Colouring the objects too would make every row in
        // the pane red, which marks nothing at all.
        let conn = sqlake_core::id::ConnId::new();
        let root = VisibleNode {
            conn,
            depth: 0,
            label: "prod".into(),
            node_ref: NodeRef::root(),
            relation_kind: None,
            state: NodeState::Expanded,
        };
        let child = VisibleNode {
            conn,
            depth: 1,
            ..node(1, "public", NodeState::Collapsed)
        };
        let snapshot = Snapshot {
            explorer: std::sync::Arc::new(TreeView {
                nodes: vec![root, child],
            }),
            connections: vec![sqlake_app::snapshot::ConnectionView {
                id: conn,
                profile: mock_summary("prod").id,
                name: "prod".into(),
                color: Some(ProfileColor::Red),
                kind: sqlake_core::capability::DriverKind::Mock,
                status: ConnStatus::Ready,
                capabilities: None,
            }],
            ..Snapshot::default()
        };

        let (buffer, _) = draw_buffer_of(&snapshot, &TreeUi::default(), 20, 3);
        let is_red = |y: u16| (0..20).any(|x| buffer[(x, y)].style().fg == Some(Color::Red));
        assert!(is_red(0), "the connection's own row");
        assert!(!is_red(1), "an object below it");
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

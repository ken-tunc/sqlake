//! The frame around the content: panes, the splitter, scrollbars, the tab bar
//! and the status bar.
//!
//! Each of these draws itself *and* records where it landed, in the same call.
//! That is the whole reason for an immediate-mode library here: the rectangle
//! is in hand at draw time, so hit testing is `rect.contains(position)` and
//! draw order is z order. Nothing has to be kept in step with anything.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use sqlake_app::snapshot::Snapshot;

use crate::hit::{ButtonId, HitMap, PaneId, ScrollPart, SplitId, Target, Z_BASE, Z_CHROME};
use crate::ui::{MIN_PANE_WIDTH, UiState};

/// Below this the layout stops being a layout, so one message is drawn instead
/// of five broken widgets.
pub const MIN_WIDTH: u16 = 60;
pub const MIN_HEIGHT: u16 = 20;

/// Where each part of the screen ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frames {
    pub tab_bar: Rect,
    pub explorer: Rect,
    pub splitter: Rect,
    pub grid: Rect,
    pub status_bar: Rect,
}

/// Divide the screen. The explorer's width comes from [`UiState`], so dragging
/// the splitter survives a redraw.
#[must_use]
pub fn layout(area: Rect, ui: &UiState) -> Frames {
    let [tab_bar, body, status_bar] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);

    let explorer_width = ui.explorer_width(area.width);
    let [explorer, splitter, grid] = Layout::horizontal([
        Constraint::Length(explorer_width),
        Constraint::Length(1),
        Constraint::Min(MIN_PANE_WIDTH),
    ])
    .areas(body);

    Frames {
        tab_bar,
        explorer,
        splitter,
        grid,
        status_bar,
    }
}

/// True when the terminal is too small to lay out at all.
#[must_use]
pub fn too_small(area: Rect) -> bool {
    area.width < MIN_WIDTH || area.height < MIN_HEIGHT
}

pub fn render_too_small(frame: &mut Frame<'_>, area: Rect) {
    let text = format!(
        "terminal is {}×{}; sqlake needs {MIN_WIDTH}×{MIN_HEIGHT}",
        area.width, area.height
    );
    frame.render_widget(
        Paragraph::new(text).style(Style::new().fg(Color::Yellow)),
        area,
    );
}

/// A titled box. Returns the area inside the border, which is where the caller
/// draws and what it should record as the pane's viewport.
pub fn pane(
    frame: &mut Frame<'_>,
    hits: &mut HitMap,
    area: Rect,
    id: PaneId,
    title: &str,
    focused: bool,
) -> Rect {
    // The whole pane, border included: a click on the border focuses it, and
    // the background is what the wheel finds over empty space below the last
    // row. Z_BASE so anything drawn inside wins.
    hits.push(area, Z_BASE, Target::Pane(id));

    let style = if focused {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(if focused {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .border_style(style)
        .title(Span::styled(
            format!(" {title} "),
            if focused {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::Gray)
            },
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// The draggable divider between the two panes.
pub fn splitter(frame: &mut Frame<'_>, hits: &mut HitMap, area: Rect, hovered: bool) {
    // One cell is too thin to hit with a mouse, so the grab area is widened
    // while the drawn line stays one cell.
    hits.push(
        crate::hit::grab_area(area),
        Z_CHROME,
        Target::Splitter(SplitId::Explorer),
    );

    let style = if hovered {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    for y in area.top()..area.bottom() {
        frame.render_widget(Paragraph::new("│").style(style), Rect::new(area.x, y, 1, 1));
    }
}

/// A vertical scrollbar down the right edge of `area`.
///
/// Drawn by hand rather than with ratatui's `Scrollbar` because the thumb's
/// rectangle has to be recorded for hit testing, and reproducing the widget's
/// arithmetic to guess where it drew is worse than doing the arithmetic once.
pub fn scrollbar(
    frame: &mut Frame<'_>,
    hits: &mut HitMap,
    area: Rect,
    pane: PaneId,
    offset: usize,
    content: usize,
) {
    if area.height == 0 || content <= area.height as usize {
        return; // Nothing to scroll: a full-height thumb is just noise.
    }

    let track = area.height as usize;
    let thumb_len = (track * track / content).max(1).min(track);
    let span = content - track;
    let travel = track - thumb_len;
    let thumb_at = (offset.min(span) * travel).checked_div(span).unwrap_or(0);

    let x = area.right().saturating_sub(1);
    let cut = |from: usize, len: usize| {
        Rect::new(
            x,
            area.y + u16::try_from(from).unwrap_or(u16::MAX),
            1,
            u16::try_from(len).unwrap_or(u16::MAX),
        )
    };

    let before = cut(0, thumb_at);
    let thumb = cut(thumb_at, thumb_len);
    let after = cut(thumb_at + thumb_len, track - thumb_at - thumb_len);

    // Pushed before the thumb so the thumb, pushed later at the same z, wins
    // where they touch.
    for (rect, part) in [
        (before, ScrollPart::TrackBefore),
        (after, ScrollPart::TrackAfter),
        (thumb, ScrollPart::Thumb),
    ] {
        hits.push(rect, Z_CHROME, Target::Scrollbar { pane, part });
    }

    for y in 0..track {
        let (glyph, style) = if y >= thumb_at && y < thumb_at + thumb_len {
            ("█", Style::new().fg(Color::Gray))
        } else {
            ("│", Style::new().fg(Color::DarkGray))
        };
        frame.render_widget(Paragraph::new(glyph).style(style), cut(y, 1));
    }
}

/// One entry per open tab, each with a close box.
pub fn tab_bar(frame: &mut Frame<'_>, hits: &mut HitMap, area: Rect, snapshot: &Snapshot) {
    hits.push(area, Z_BASE, Target::Pane(PaneId::TabBar));
    frame.render_widget(
        Paragraph::new("").style(Style::new().bg(Color::Black)),
        area,
    );

    let mut x = area.x;
    for tab in &snapshot.tabs {
        let active = snapshot.active_tab == Some(tab.id);
        let label = format!(" {} ", tab.title);
        let width = u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
        // Stop at the edge rather than drawing a tab half off the screen.
        if x + width + 2 > area.right() {
            break;
        }

        let style = if active {
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::Gray)
        };
        let title_rect = Rect::new(x, area.y, width, 1);
        frame.render_widget(Paragraph::new(label).style(style), title_rect);
        hits.push(title_rect, Z_CHROME, Target::Tab(tab.id));

        let close_rect = Rect::new(x + width, area.y, 2, 1);
        frame.render_widget(Paragraph::new("× ").style(style), close_rect);
        hits.push(close_rect, Z_CHROME, Target::TabClose(tab.id));

        x += width + 2;
    }
}

/// Running work, with a way to stop it, and the hints that have to be visible
/// rather than discovered.
pub fn status_bar(frame: &mut Frame<'_>, hits: &mut HitMap, area: Rect, snapshot: &Snapshot) {
    hits.push(area, Z_BASE, Target::Pane(PaneId::StatusBar));

    let mut spans = Vec::new();
    let mut x = area.x;

    for item in &snapshot.busy {
        let label = format!(" ⟳ {} ", item.label);
        let width = u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
        if x + width + 3 > area.right() {
            break;
        }
        spans.push(Span::styled(label, Style::new().fg(Color::Yellow)));
        x += width;

        let cancel = Rect::new(x, area.y, 3, 1);
        spans.push(Span::styled(
            "[×]",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
        hits.push(cancel, Z_CHROME, Target::Button(ButtonId::Cancel(item.id)));
        x += 3;
    }

    if spans.is_empty() {
        // Mouse capture takes the terminal's own text selection away, so the
        // way to get it back cannot be something the user has to know already.
        spans.push(Span::styled(
            " Shift (or Option) + drag to select text ",
            Style::new().fg(Color::DarkGray),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Instant;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;
    use sqlake_app::action::BusyId;
    use sqlake_app::snapshot::{
        BusyItem, BusyOwner, ConnStatus, ConnectionView, LoadState, PreviewTab, TabContent, TabView,
    };
    use sqlake_core::capability::DriverKind;
    use sqlake_core::id::{ConnId, TabId};
    use sqlake_core::node::TableRef;

    use super::*;

    fn snapshot(tabs: usize, busy: usize) -> Snapshot {
        let conn = ConnId::new();
        Snapshot {
            rev: 1,
            connections: vec![ConnectionView {
                id: conn,
                name: "mock".into(),
                kind: DriverKind::Mock,
                status: ConnStatus::Ready,
                capabilities: None,
            }],
            trees: HashMap::new(),
            tabs: (0..tabs)
                .map(|i| TabView {
                    id: TabId::new(i as u32),
                    conn,
                    title: format!("t{i}"),
                    content: TabContent::Preview(PreviewTab {
                        table: TableRef::new(["public", "users"]),
                        sort: None,
                        loaded_rows: 0,
                        data: LoadState::Idle,
                    }),
                })
                .collect(),
            active_tab: (tabs > 0).then(|| TabId::new(0)),
            busy: (0..busy)
                .map(|i| BusyItem {
                    id: BusyId::new(i as u64),
                    owner: BusyOwner::Connection(conn),
                    label: format!("job {i}"),
                    started_at: Instant::now(),
                })
                .collect(),
            toasts: Vec::new(),
            should_quit: false,
        }
    }

    fn draw(f: impl FnOnce(&mut Frame<'_>, &mut HitMap), w: u16, h: u16) -> HitMap {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut hits = HitMap::new();
        terminal
            .draw(|frame| f(frame, &mut hits))
            .expect("draw failed");
        hits
    }

    #[test]
    fn the_layout_accounts_for_every_row_and_column() {
        let ui = UiState::new();
        let area = Rect::new(0, 0, 100, 30);
        let f = layout(area, &ui);

        assert_eq!(f.tab_bar.height, 1);
        assert_eq!(f.status_bar.height, 1);
        assert_eq!(f.tab_bar.y + 1, f.explorer.y);
        assert_eq!(f.explorer.bottom(), f.status_bar.y);
        assert_eq!(f.explorer.width + f.splitter.width + f.grid.width, 100);
        assert_eq!(f.splitter.x, f.explorer.right());
        assert_eq!(f.grid.x, f.splitter.right());
    }

    #[test]
    fn a_narrow_terminal_still_leaves_both_panes_usable() {
        let ui = UiState::new();
        let f = layout(Rect::new(0, 0, MIN_WIDTH, MIN_HEIGHT), &ui);
        assert!(f.explorer.width >= MIN_PANE_WIDTH, "{}", f.explorer.width);
        assert!(f.grid.width >= MIN_PANE_WIDTH, "{}", f.grid.width);
    }

    #[test]
    fn below_the_minimum_one_message_replaces_the_layout() {
        assert!(too_small(Rect::new(0, 0, 59, 30)));
        assert!(too_small(Rect::new(0, 0, 100, 19)));
        assert!(!too_small(Rect::new(0, 0, MIN_WIDTH, MIN_HEIGHT)));
    }

    #[test]
    fn a_pane_catches_clicks_on_its_border_and_its_empty_space() {
        let area = Rect::new(0, 0, 20, 10);
        let hits = draw(
            |frame, hits| {
                pane(frame, hits, area, PaneId::Explorer, "Explorer", true);
            },
            20,
            10,
        );
        // The border, which is outside the area content is drawn in.
        assert_eq!(
            hits.at(Position::new(0, 0)),
            Some(Target::Pane(PaneId::Explorer))
        );
        // And the middle, which is where the wheel lands below the last row.
        assert_eq!(
            hits.at(Position::new(10, 5)),
            Some(Target::Pane(PaneId::Explorer))
        );
    }

    #[test]
    fn the_splitter_is_wider_to_the_mouse_than_to_the_eye() {
        let area = Rect::new(10, 0, 1, 8);
        let hits = draw(|frame, hits| splitter(frame, hits, area, false), 30, 8);
        for x in 9..=11 {
            assert_eq!(
                hits.at(Position::new(x, 3)),
                Some(Target::Splitter(SplitId::Explorer)),
                "x={x}"
            );
        }
    }

    #[test]
    fn a_scrollbar_appears_only_when_there_is_something_below() {
        let area = Rect::new(0, 0, 20, 10);
        let hits = draw(
            |frame, hits| scrollbar(frame, hits, area, PaneId::Grid, 0, 5),
            20,
            10,
        );
        assert_eq!(hits.at(Position::new(19, 5)), None, "content fits");
    }

    #[test]
    fn the_thumb_sits_between_the_two_halves_of_the_track() {
        let area = Rect::new(0, 0, 20, 10);
        let hits = draw(
            |frame, hits| scrollbar(frame, hits, area, PaneId::Grid, 45, 100),
            20,
            10,
        );

        let parts: Vec<_> = (0..10)
            .map(|y| match hits.at(Position::new(19, y)) {
                Some(Target::Scrollbar { part, .. }) => Some(part),
                _ => None,
            })
            .collect();

        let thumb_at = parts.iter().position(|p| *p == Some(ScrollPart::Thumb));
        assert!(thumb_at.is_some(), "{parts:?}");
        let thumb_at = thumb_at.unwrap();
        assert!(
            parts[..thumb_at]
                .iter()
                .all(|p| *p == Some(ScrollPart::TrackBefore)),
            "{parts:?}"
        );
        assert!(
            parts
                .iter()
                .rev()
                .take(1)
                .all(|p| *p == Some(ScrollPart::TrackAfter) || *p == Some(ScrollPart::Thumb)),
            "{parts:?}"
        );
    }

    #[test]
    fn scrolled_to_the_end_puts_the_thumb_at_the_bottom() {
        let area = Rect::new(0, 0, 20, 10);
        let hits = draw(
            |frame, hits| scrollbar(frame, hits, area, PaneId::Grid, 90, 100),
            20,
            10,
        );
        assert!(matches!(
            hits.at(Position::new(19, 9)),
            Some(Target::Scrollbar {
                part: ScrollPart::Thumb,
                ..
            })
        ));
    }

    #[test]
    fn every_tab_offers_its_own_close_box() {
        let snap = snapshot(3, 0);
        let area = Rect::new(0, 0, 60, 1);
        let hits = draw(|frame, hits| tab_bar(frame, hits, area, &snap), 60, 1);

        let mut seen_titles = 0;
        let mut seen_closes = 0;
        for x in 0..60 {
            match hits.at(Position::new(x, 0)) {
                Some(Target::Tab(_)) => seen_titles += 1,
                Some(Target::TabClose(_)) => seen_closes += 1,
                _ => {}
            }
        }
        assert!(seen_titles >= 3, "{seen_titles}");
        assert_eq!(seen_closes, 6, "two cells each");
    }

    #[test]
    fn a_tab_that_does_not_fit_is_left_out_rather_than_cut() {
        let snap = snapshot(20, 0);
        // Each tab is " tN " plus a two-cell close box: six cells. Thirty-one
        // holds five of them with one cell spare, and the sixth is dropped
        // rather than drawn with its close box off the edge.
        let area = Rect::new(0, 0, 31, 1);
        let hits = draw(|frame, hits| tab_bar(frame, hits, area, &snap), 31, 1);

        let mut tabs = std::collections::BTreeSet::new();
        for x in 0..31 {
            if let Some(Target::Tab(id) | Target::TabClose(id)) = hits.at(Position::new(x, 0)) {
                tabs.insert(id);
            }
        }
        assert_eq!(tabs.len(), 5, "{tabs:?}");
        // The spare cell belongs to the bar itself, not to a tab drawn across
        // the edge.
        assert_eq!(
            hits.at(Position::new(30, 0)),
            Some(Target::Pane(PaneId::TabBar))
        );
    }

    #[test]
    fn a_running_job_gets_a_cancel_button() {
        let snap = snapshot(0, 1);
        let area = Rect::new(0, 0, 60, 1);
        let hits = draw(|frame, hits| status_bar(frame, hits, area, &snap), 60, 1);

        let found = (0..60)
            .filter_map(|x| hits.at(Position::new(x, 0)))
            .any(|t| matches!(t, Target::Button(ButtonId::Cancel(_))));
        assert!(found, "no way to stop it");
    }

    #[test]
    fn an_idle_status_bar_says_how_to_select_text() {
        let snap = snapshot(0, 0);
        let mut terminal = Terminal::new(TestBackend::new(60, 1)).unwrap();
        let mut hits = HitMap::new();
        terminal
            .draw(|frame| status_bar(frame, &mut hits, Rect::new(0, 0, 60, 1), &snap))
            .unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(rendered.contains("drag to select"), "{rendered:?}");
    }
}

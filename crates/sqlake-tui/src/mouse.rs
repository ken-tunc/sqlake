//! Turning raw mouse events into gestures.
//!
//! The terminal reports presses, releases and motion. What the UI wants is
//! clicks, double clicks and drags, resolved against whatever was under the
//! pointer. That translation happens here, once, so no widget has to reason
//! about button state.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Position;

use crate::hit::{HitMap, Target};

/// Two clicks further apart than this are two clicks.
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(300);

/// How far the pointer may drift between the two clicks of a double click.
const DOUBLE_CLICK_SLOP: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gesture {
    /// The button went down. Useful for immediate feedback; most handlers want
    /// `Click` instead.
    Down,
    Up,
    Click,
    DoubleClick,
    RightClick,
    /// Movement since the last report, while a button is held.
    DragBy {
        dx: i16,
        dy: i16,
    },
    /// Vertical wheel. Positive scrolls towards the end of the content.
    Scroll(i8),
    /// Horizontal wheel. Positive scrolls right.
    ScrollX(i8),
    HoverEnter,
    HoverLeave,
}

#[derive(Debug, Clone, Copy)]
struct Press {
    /// Captured when the button went down, and kept until it comes up even if
    /// the pointer leaves the rectangle. Without this, resizing a column stops
    /// the moment the pointer outruns the cursor.
    target: Target,
    /// Where the button went down. A click happens where it started, not where
    /// the pointer happened to be on release.
    origin: Position,
    last: Position,
    moved: bool,
}

#[derive(Debug, Default)]
pub struct MouseState {
    pressed: Option<Press>,
    last_click: Option<(Target, Position, Instant)>,
    hover: Option<Target>,
}

impl MouseState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// What the pointer is currently over, if anything.
    #[must_use]
    pub fn hovered(&self) -> Option<Target> {
        self.hover
    }

    /// Whether a drag is in progress.
    #[must_use]
    pub fn is_dragging(&self) -> bool {
        self.pressed.is_some_and(|p| p.moved)
    }

    /// Forget any in-flight press. Called when the layout changes underneath a
    /// drag, since the captured target may no longer mean anything.
    pub fn reset(&mut self) {
        self.pressed = None;
        self.hover = None;
    }

    /// Translate one terminal event.
    ///
    /// `now` is passed in rather than read here so that double-click timing is
    /// testable.
    pub fn feed(
        &mut self,
        event: MouseEvent,
        map: &HitMap,
        now: Instant,
    ) -> Vec<(Target, Gesture)> {
        let position = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => self.down(position, map),
            MouseEventKind::Up(MouseButton::Left) => self.up(position, now),
            MouseEventKind::Drag(MouseButton::Left) => self.drag(position),
            MouseEventKind::Down(MouseButton::Right) => map
                .at(position)
                .map(|t| vec![(t, Gesture::RightClick)])
                .unwrap_or_default(),
            MouseEventKind::Moved => self.moved(position, map),
            MouseEventKind::ScrollDown => wheel(map, position, Gesture::Scroll(1)),
            MouseEventKind::ScrollUp => wheel(map, position, Gesture::Scroll(-1)),
            MouseEventKind::ScrollRight => wheel(map, position, Gesture::ScrollX(1)),
            MouseEventKind::ScrollLeft => wheel(map, position, Gesture::ScrollX(-1)),
            // Middle and right releases, and drags with other buttons, carry
            // nothing this application acts on.
            _ => Vec::new(),
        }
    }

    fn down(&mut self, position: Position, map: &HitMap) -> Vec<(Target, Gesture)> {
        let Some(target) = map.at(position) else {
            self.pressed = None;
            return Vec::new();
        };
        self.pressed = Some(Press {
            target,
            origin: position,
            last: position,
            moved: false,
        });
        vec![(target, Gesture::Down)]
    }

    fn drag(&mut self, position: Position) -> Vec<(Target, Gesture)> {
        let Some(press) = self.pressed.as_mut() else {
            return Vec::new();
        };
        let dx = i32::from(position.x) - i32::from(press.last.x);
        let dy = i32::from(position.y) - i32::from(press.last.y);
        press.last = position;
        if dx == 0 && dy == 0 {
            return Vec::new();
        }
        press.moved = true;
        vec![(
            press.target,
            Gesture::DragBy {
                dx: narrow(dx),
                dy: narrow(dy),
            },
        )]
    }

    fn up(&mut self, _position: Position, now: Instant) -> Vec<(Target, Gesture)> {
        let Some(press) = self.pressed.take() else {
            // A release with no press: the button was already down when the
            // application started, or the press was reset under it.
            return Vec::new();
        };

        let mut out = vec![(press.target, Gesture::Up)];
        if press.moved {
            // A drag is not a click. Emitting both would fire the row's action
            // every time the user finished resizing something on it.
            self.last_click = None;
            return out;
        }

        let is_double = self.last_click.is_some_and(|(target, at, when)| {
            target == press.target
                && now.duration_since(when) <= DOUBLE_CLICK_WINDOW
                && near(at, press.origin)
        });

        if is_double {
            out.push((press.target, Gesture::DoubleClick));
            // Consumed, so a third click starts a new pair rather than firing
            // a second double click.
            self.last_click = None;
        } else {
            out.push((press.target, Gesture::Click));
            self.last_click = Some((press.target, press.origin, now));
        }
        out
    }

    fn moved(&mut self, position: Position, map: &HitMap) -> Vec<(Target, Gesture)> {
        // While a button is held the pointer is dragging, not hovering.
        if self.pressed.is_some() {
            return Vec::new();
        }
        let now_over = map.at(position);
        if now_over == self.hover {
            // The common case by far: motion within the same row. Reporting
            // nothing is what keeps the screen from redrawing on every cell.
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some(previous) = self.hover {
            out.push((previous, Gesture::HoverLeave));
        }
        if let Some(current) = now_over {
            out.push((current, Gesture::HoverEnter));
        }
        self.hover = now_over;
        out
    }
}

fn wheel(map: &HitMap, position: Position, gesture: Gesture) -> Vec<(Target, Gesture)> {
    map.at(position)
        .map(|t| vec![(t, gesture)])
        .unwrap_or_default()
}

/// A terminal is at most u16 cells wide, so a delta always fits; clamping
/// keeps that assumption from becoming a silent wrap if it ever stops holding.
fn narrow(v: i32) -> i16 {
    i16::try_from(v.clamp(i32::from(i16::MIN), i32::from(i16::MAX))).unwrap_or(0)
}

fn near(a: Position, b: Position) -> bool {
    a.x.abs_diff(b.x) <= DOUBLE_CLICK_SLOP && a.y.abs_diff(b.y) <= DOUBLE_CLICK_SLOP
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::KeyModifiers;
    use ratatui::layout::Rect;

    use super::*;
    use crate::hit::{Z_CONTENT, grab_area};

    fn map() -> HitMap {
        let mut map = HitMap::new();
        map.push(
            Rect::new(0, 0, 20, 1),
            Z_CONTENT,
            Target::TreeRow { index: 0 },
        );
        map.push(
            Rect::new(0, 1, 20, 1),
            Z_CONTENT,
            Target::TreeRow { index: 1 },
        );
        map.push(
            grab_area(Rect::new(30, 0, 1, 5)),
            Z_CONTENT,
            Target::GridColEdge { col: 2 },
        );
        map
    }

    fn event(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn down(x: u16, y: u16) -> MouseEvent {
        event(MouseEventKind::Down(MouseButton::Left), x, y)
    }

    fn up(x: u16, y: u16) -> MouseEvent {
        event(MouseEventKind::Up(MouseButton::Left), x, y)
    }

    fn drag(x: u16, y: u16) -> MouseEvent {
        event(MouseEventKind::Drag(MouseButton::Left), x, y)
    }

    fn moved(x: u16, y: u16) -> MouseEvent {
        event(MouseEventKind::Moved, x, y)
    }

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_press_and_release_is_a_click() {
        let (map, mut s, t) = (map(), MouseState::new(), t0());
        assert_eq!(
            s.feed(down(3, 0), &map, t),
            [(Target::TreeRow { index: 0 }, Gesture::Down)]
        );
        assert_eq!(
            s.feed(up(3, 0), &map, t),
            [
                (Target::TreeRow { index: 0 }, Gesture::Up),
                (Target::TreeRow { index: 0 }, Gesture::Click),
            ]
        );
    }

    #[test]
    fn two_quick_clicks_are_a_double_click() {
        let (map, mut s, t) = (map(), MouseState::new(), t0());
        s.feed(down(3, 0), &map, t);
        s.feed(up(3, 0), &map, t);

        let t2 = t + Duration::from_millis(150);
        s.feed(down(3, 0), &map, t2);
        let out = s.feed(up(3, 0), &map, t2);
        assert!(out.contains(&(Target::TreeRow { index: 0 }, Gesture::DoubleClick)));
        assert!(!out.contains(&(Target::TreeRow { index: 0 }, Gesture::Click)));
    }

    #[test]
    fn a_third_click_does_not_fire_a_second_double_click() {
        let (map, mut s, t) = (map(), MouseState::new(), t0());
        for step in [0, 100, 200] {
            let at = t + Duration::from_millis(step);
            s.feed(down(3, 0), &map, at);
            let out = s.feed(up(3, 0), &map, at);
            if step == 200 {
                assert!(
                    out.contains(&(Target::TreeRow { index: 0 }, Gesture::Click)),
                    "the pair was consumed, so this starts a new one"
                );
            }
        }
    }

    #[test]
    fn a_slow_second_click_is_two_clicks() {
        let (map, mut s, t) = (map(), MouseState::new(), t0());
        s.feed(down(3, 0), &map, t);
        s.feed(up(3, 0), &map, t);

        let t2 = t + DOUBLE_CLICK_WINDOW + Duration::from_millis(1);
        s.feed(down(3, 0), &map, t2);
        let out = s.feed(up(3, 0), &map, t2);
        assert!(out.contains(&(Target::TreeRow { index: 0 }, Gesture::Click)));
    }

    #[test]
    fn clicks_on_different_targets_are_not_a_double_click() {
        let (map, mut s, t) = (map(), MouseState::new(), t0());
        s.feed(down(3, 0), &map, t);
        s.feed(up(3, 0), &map, t);
        s.feed(down(3, 1), &map, t);
        let out = s.feed(up(3, 1), &map, t);
        assert!(out.contains(&(Target::TreeRow { index: 1 }, Gesture::Click)));
    }

    #[test]
    fn a_second_click_far_away_is_not_a_double_click() {
        // Same target, but the pointer travelled. Two deliberate clicks on a
        // wide row are not one double click.
        let (map, mut s, t) = (map(), MouseState::new(), t0());
        s.feed(down(1, 0), &map, t);
        s.feed(up(1, 0), &map, t);
        s.feed(down(15, 0), &map, t);
        let out = s.feed(up(15, 0), &map, t);
        assert!(out.contains(&(Target::TreeRow { index: 0 }, Gesture::Click)));
    }

    #[test]
    fn dragging_reports_movement_relative_to_the_last_report() {
        let (map, mut s, t) = (map(), MouseState::new(), t0());
        s.feed(down(30, 0), &map, t);
        assert_eq!(
            s.feed(drag(33, 0), &map, t),
            [(
                Target::GridColEdge { col: 2 },
                Gesture::DragBy { dx: 3, dy: 0 }
            )]
        );
        assert_eq!(
            s.feed(drag(31, 2), &map, t),
            [(
                Target::GridColEdge { col: 2 },
                Gesture::DragBy { dx: -2, dy: 2 }
            )]
        );
    }

    #[test]
    fn a_drag_keeps_its_original_target_after_leaving_the_rectangle() {
        // Otherwise a column stops resizing the moment the pointer outruns it.
        let (map, mut s, t) = (map(), MouseState::new(), t0());
        s.feed(down(30, 0), &map, t);
        let out = s.feed(drag(3, 0), &map, t);
        assert_eq!(out[0].0, Target::GridColEdge { col: 2 });
    }

    #[test]
    fn a_drag_that_does_not_move_reports_nothing() {
        let (map, mut s, t) = (map(), MouseState::new(), t0());
        s.feed(down(30, 0), &map, t);
        assert!(s.feed(drag(30, 0), &map, t).is_empty());
    }

    #[test]
    fn releasing_after_a_drag_is_not_a_click() {
        let (map, mut s, t) = (map(), MouseState::new(), t0());
        s.feed(down(3, 0), &map, t);
        s.feed(drag(9, 0), &map, t);
        let out = s.feed(up(9, 0), &map, t);
        assert_eq!(out, [(Target::TreeRow { index: 0 }, Gesture::Up)]);
        assert!(!s.is_dragging(), "the press is finished");
    }

    #[test]
    fn a_release_without_a_press_is_ignored() {
        let (map, mut s, t) = (map(), MouseState::new(), t0());
        assert!(s.feed(up(3, 0), &map, t).is_empty());
    }

    #[test]
    fn a_press_on_nothing_does_not_capture() {
        let (map, mut s, t) = (map(), MouseState::new(), t0());
        assert!(s.feed(down(50, 50), &map, t).is_empty());
        assert!(s.feed(drag(3, 0), &map, t).is_empty());
        assert!(s.feed(up(3, 0), &map, t).is_empty());
    }

    #[test]
    fn hover_reports_only_when_the_target_changes() {
        let (map, mut s, t) = (map(), MouseState::new(), t0());
        assert_eq!(
            s.feed(moved(3, 0), &map, t),
            [(Target::TreeRow { index: 0 }, Gesture::HoverEnter)]
        );
        // Moving within the same row is the common case; reporting it would
        // redraw the screen on every cell of movement.
        assert!(s.feed(moved(9, 0), &map, t).is_empty());

        assert_eq!(
            s.feed(moved(3, 1), &map, t),
            [
                (Target::TreeRow { index: 0 }, Gesture::HoverLeave),
                (Target::TreeRow { index: 1 }, Gesture::HoverEnter),
            ]
        );
    }

    #[test]
    fn leaving_everything_reports_only_the_departure() {
        let (map, mut s, t) = (map(), MouseState::new(), t0());
        s.feed(moved(3, 0), &map, t);
        assert_eq!(
            s.feed(moved(50, 50), &map, t),
            [(Target::TreeRow { index: 0 }, Gesture::HoverLeave)]
        );
        assert_eq!(s.hovered(), None);
    }

    #[test]
    fn hover_is_suppressed_while_dragging() {
        let (map, mut s, t) = (map(), MouseState::new(), t0());
        s.feed(down(3, 0), &map, t);
        assert!(s.feed(moved(3, 1), &map, t).is_empty());
    }

    #[test]
    fn the_wheel_targets_what_is_under_the_pointer() {
        let (map, mut s, t) = (map(), MouseState::new(), t0());
        assert_eq!(
            s.feed(event(MouseEventKind::ScrollDown, 3, 1), &map, t),
            [(Target::TreeRow { index: 1 }, Gesture::Scroll(1))]
        );
        assert_eq!(
            s.feed(event(MouseEventKind::ScrollUp, 3, 0), &map, t),
            [(Target::TreeRow { index: 0 }, Gesture::Scroll(-1))]
        );
        assert_eq!(
            s.feed(event(MouseEventKind::ScrollRight, 3, 0), &map, t),
            [(Target::TreeRow { index: 0 }, Gesture::ScrollX(1))]
        );
    }

    #[test]
    fn a_right_click_reports_without_capturing() {
        let (map, mut s, t) = (map(), MouseState::new(), t0());
        assert_eq!(
            s.feed(
                event(MouseEventKind::Down(MouseButton::Right), 3, 0),
                &map,
                t
            ),
            [(Target::TreeRow { index: 0 }, Gesture::RightClick)]
        );
        assert!(s.feed(drag(9, 0), &map, t).is_empty());
    }

    #[test]
    fn resetting_abandons_an_in_flight_press() {
        // The layout moved underneath the drag, so the captured target no
        // longer means what it did.
        let (map, mut s, t) = (map(), MouseState::new(), t0());
        s.feed(down(3, 0), &map, t);
        s.reset();
        assert!(s.feed(drag(9, 0), &map, t).is_empty());
        assert!(s.feed(up(9, 0), &map, t).is_empty());
    }
}

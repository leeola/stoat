use gpui::{px, Axis, Pixels, Point};
use std::time::{Duration, Instant};
use stoat_text::Anchor;

/// Minimum gap between scroll events for a new axis-lock decision.
/// Events arriving within this window keep the prior lock so a
/// trackpad gesture that briefly slips perpendicular does not flip
/// the locked axis.
pub const SCROLL_EVENT_SEPARATION: Duration = Duration::from_millis(28);

const UNLOCK_PERCENT: f32 = 1.9;
const UNLOCK_LOWER_BOUND: Pixels = px(6.);

/// Stable scroll origin for the editor, expressed as an [`Anchor`] in
/// the underlying buffer plus a sub-line pixel offset. The anchor
/// survives edits between frames; the offset carries sub-pixel
/// fractional position so smooth scrolling can paint between integer
/// rows.
///
/// Offsets are tracked in `f64` (never `f32`) across the whole scroll
/// pipeline so an accumulated trackpad gesture never loses precision
/// to single-precision rounding.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollAnchor {
    pub anchor: Anchor,
    pub offset: Point<f64>,
}

impl ScrollAnchor {
    pub fn new() -> Self {
        Self {
            anchor: Anchor::min(),
            offset: Point::default(),
        }
    }
}

impl Default for ScrollAnchor {
    fn default() -> Self {
        Self::new()
    }
}

/// In-progress trackpad gesture state. Trackpad pixel deltas are run
/// through [`OngoingScroll::filter`] so a gesture that started
/// vertically stays vertical even when the user briefly drifts
/// horizontally; the perpendicular axis is zeroed in the delta and
/// the locked axis returned to the caller.
#[derive(Clone, Copy, Debug)]
pub struct OngoingScroll {
    last_event: Instant,
    axis: Option<Axis>,
}

impl OngoingScroll {
    /// Construct an unlocked scroll. `now` is the wall-clock
    /// timestamp the caller wants to bind to the first event; passing
    /// it in keeps the constructor pure so tests can drive time
    /// without reaching for `Instant::now()`.
    pub fn new(now: Instant) -> Self {
        Self {
            last_event: now - SCROLL_EVENT_SEPARATION,
            axis: None,
        }
    }

    pub fn axis(&self) -> Option<Axis> {
        self.axis
    }

    /// Apply axis-lock to `delta`. When an axis is locked, the
    /// perpendicular component of `delta` is zeroed and the locked
    /// axis is returned. When `now` is more than
    /// [`SCROLL_EVENT_SEPARATION`] past `last_event`, the lock is
    /// recomputed from `delta` (vertical when y >= x, else
    /// horizontal). When the perpendicular component exceeds the
    /// locked component by at least [`UNLOCK_PERCENT`] (and at least
    /// [`UNLOCK_LOWER_BOUND`] pixels in magnitude), the lock
    /// releases and `delta` passes through unchanged.
    pub fn filter(&self, delta: &mut Point<Pixels>, now: Instant) -> Option<Axis> {
        let mut axis = self.axis;
        let x = delta.x.abs();
        let y = delta.y.abs();
        let duration = now.duration_since(self.last_event);

        if duration > SCROLL_EVENT_SEPARATION {
            axis = if x <= y {
                Some(Axis::Vertical)
            } else {
                Some(Axis::Horizontal)
            };
        } else if x.max(y) >= UNLOCK_LOWER_BOUND {
            match axis {
                Some(Axis::Vertical) if x > y && x >= y * UNLOCK_PERCENT => {
                    axis = None;
                },
                Some(Axis::Horizontal) if y > x && y >= x * UNLOCK_PERCENT => {
                    axis = None;
                },
                _ => {},
            }
        }

        match axis {
            Some(Axis::Vertical) => *delta = Point::new(px(0.), delta.y),
            Some(Axis::Horizontal) => *delta = Point::new(delta.x, px(0.)),
            None => {},
        }
        axis
    }

    /// Stamp the gesture with `axis` and timestamp `now`. Callers
    /// invoke this after consuming a wheel event so the next
    /// [`filter`] sees the up-to-date lock and event time.
    pub fn update(&mut self, axis: Option<Axis>, now: Instant) {
        self.last_event = now;
        self.axis = axis;
    }
}

/// Visual state of a scrollbar thumb. Drives the rendered thumb's
/// fill / hover ring and the drag follow-through; the minimap reuses
/// the same vocabulary via [`ScrollManager::minimap_thumb_state`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum ScrollbarThumbState {
    #[default]
    Idle,
    Hovered,
    Dragging,
}

/// Per-editor scroll state. Owns the active [`ScrollAnchor`], the
/// in-progress [`OngoingScroll`] gesture, a cached visible-line count
/// the render path fills in during paint, and the minimap thumb
/// state. The wheel listener mutates this struct on each frame; the
/// render path reads from it; tests construct it directly.
pub struct ScrollManager {
    anchor: ScrollAnchor,
    ongoing: OngoingScroll,
    visible_line_count: Option<f64>,
    minimap_thumb_state: Option<ScrollbarThumbState>,
}

impl ScrollManager {
    pub fn new(now: Instant) -> Self {
        Self {
            anchor: ScrollAnchor::new(),
            ongoing: OngoingScroll::new(now),
            visible_line_count: None,
            minimap_thumb_state: None,
        }
    }

    pub fn anchor(&self) -> &ScrollAnchor {
        &self.anchor
    }

    pub fn set_anchor(&mut self, anchor: ScrollAnchor) {
        self.anchor = anchor;
    }

    pub fn ongoing(&self) -> &OngoingScroll {
        &self.ongoing
    }

    pub fn ongoing_mut(&mut self) -> &mut OngoingScroll {
        &mut self.ongoing
    }

    pub fn visible_line_count(&self) -> Option<f64> {
        self.visible_line_count
    }

    pub fn set_visible_line_count(&mut self, count: Option<f64>) {
        self.visible_line_count = count;
    }

    pub fn minimap_thumb_state(&self) -> Option<ScrollbarThumbState> {
        self.minimap_thumb_state
    }

    pub fn set_minimap_thumb_state(&mut self, state: Option<ScrollbarThumbState>) {
        self.minimap_thumb_state = state;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch() -> Instant {
        Instant::now()
    }

    #[test]
    fn scroll_anchor_new_defaults() {
        let a = ScrollAnchor::new();
        assert_eq!(a.anchor, Anchor::min());
        assert_eq!(a.offset, Point::default());
    }

    #[test]
    fn ongoing_new_starts_unlocked() {
        let now = epoch();
        let ongoing = OngoingScroll::new(now);
        assert_eq!(ongoing.axis(), None);
    }

    #[test]
    fn ongoing_filter_locks_vertical_on_dominant_y() {
        let now = epoch();
        let ongoing = OngoingScroll::new(now);
        let mut delta = Point::new(px(2.), px(20.));
        let later = now + SCROLL_EVENT_SEPARATION + Duration::from_millis(1);
        let axis = ongoing.filter(&mut delta, later);
        assert_eq!(axis, Some(Axis::Vertical));
        assert_eq!(delta, Point::new(px(0.), px(20.)));
    }

    #[test]
    fn ongoing_filter_locks_horizontal_on_dominant_x() {
        let now = epoch();
        let ongoing = OngoingScroll::new(now);
        let mut delta = Point::new(px(20.), px(2.));
        let later = now + SCROLL_EVENT_SEPARATION + Duration::from_millis(1);
        let axis = ongoing.filter(&mut delta, later);
        assert_eq!(axis, Some(Axis::Horizontal));
        assert_eq!(delta, Point::new(px(20.), px(0.)));
    }

    #[test]
    fn ongoing_filter_holds_lock_within_separation_window() {
        let now = epoch();
        let mut ongoing = OngoingScroll::new(now);
        ongoing.update(Some(Axis::Vertical), now);
        let within = now + Duration::from_millis(5);
        let mut delta = Point::new(px(3.), px(1.));
        let axis = ongoing.filter(&mut delta, within);
        assert_eq!(axis, Some(Axis::Vertical));
        assert_eq!(delta, Point::new(px(0.), px(1.)));
    }

    #[test]
    fn ongoing_filter_unlocks_when_perpendicular_exceeds_threshold() {
        let now = epoch();
        let mut ongoing = OngoingScroll::new(now);
        ongoing.update(Some(Axis::Vertical), now);
        let within = now + Duration::from_millis(5);
        let mut delta = Point::new(px(20.), px(2.));
        let axis = ongoing.filter(&mut delta, within);
        assert_eq!(axis, None);
        assert_eq!(delta, Point::new(px(20.), px(2.)));
    }

    #[test]
    fn ongoing_filter_starts_new_lock_after_separation_window() {
        let now = epoch();
        let mut ongoing = OngoingScroll::new(now);
        ongoing.update(Some(Axis::Vertical), now);
        let after = now + SCROLL_EVENT_SEPARATION + Duration::from_millis(1);
        let mut delta = Point::new(px(20.), px(2.));
        let axis = ongoing.filter(&mut delta, after);
        assert_eq!(axis, Some(Axis::Horizontal));
        assert_eq!(delta, Point::new(px(20.), px(0.)));
    }

    #[test]
    fn ongoing_update_sets_axis_and_advances_time() {
        let now = epoch();
        let mut ongoing = OngoingScroll::new(now);
        let later = now + Duration::from_millis(10);
        ongoing.update(Some(Axis::Horizontal), later);
        assert_eq!(ongoing.axis(), Some(Axis::Horizontal));
        let mut delta = Point::new(px(2.), px(2.));
        let axis = ongoing.filter(
            &mut delta,
            later + SCROLL_EVENT_SEPARATION + Duration::from_millis(1),
        );
        assert_eq!(axis, Some(Axis::Vertical));
    }

    #[test]
    fn scroll_manager_new_defaults() {
        let mgr = ScrollManager::new(epoch());
        assert_eq!(mgr.anchor(), &ScrollAnchor::new());
        assert_eq!(mgr.ongoing().axis(), None);
        assert_eq!(mgr.visible_line_count(), None);
        assert_eq!(mgr.minimap_thumb_state(), None);
    }

    #[test]
    fn scroll_manager_setters_store_values() {
        let mut mgr = ScrollManager::new(epoch());
        mgr.set_visible_line_count(Some(42.5));
        mgr.set_minimap_thumb_state(Some(ScrollbarThumbState::Hovered));

        let new_anchor = ScrollAnchor {
            anchor: Anchor::max(),
            offset: Point::new(1.5, 7.25),
        };
        mgr.set_anchor(new_anchor);

        assert_eq!(mgr.visible_line_count(), Some(42.5));
        assert_eq!(
            mgr.minimap_thumb_state(),
            Some(ScrollbarThumbState::Hovered)
        );
        assert_eq!(mgr.anchor(), &new_anchor);
    }
}

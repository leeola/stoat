//! Scroll handling action implementation and tests.
//!
//! Provides smooth scrolling functionality for viewport navigation using wheel/trackpad
//! events. The scroll handler applies sensitivity multipliers, converts deltas to line
//! units, and enforces buffer bounds.

use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Handle scroll wheel/trackpad events.
    ///
    /// Processes scroll input from mouse wheels and trackpads, applying sensitivity
    /// multipliers and converting pixel/line deltas into scroll position updates.
    /// Enforces buffer bounds to prevent scrolling past the document edges.
    ///
    /// # Arguments
    ///
    /// * `delta` - The scroll delta from the input event (pixels or lines)
    /// * `fast_scroll` - Whether to apply fast scroll multiplier (e.g., Shift+scroll)
    /// * `cx` - GPUI context for buffer access
    ///
    /// # Workflow
    ///
    /// 1. Applies base sensitivity (1.0) and optional fast multiplier (3.0)
    /// 2. Converts scroll delta to line units using line height
    /// 3. Calculates new scroll position via [`crate::scroll::ScrollState::apply_scroll_delta`]
    /// 4. Enforces bounds checking against buffer max row
    /// 5. Updates scroll position via [`crate::scroll::ScrollState::scroll_to`]
    ///
    /// # Behavior
    ///
    /// - Base sensitivity: 1.0 (standard scroll speed)
    /// - Fast multiplier: 3.0 (when fast_scroll is true)
    /// - Line height: 20.0 pixels (for delta conversion)
    /// - Bounds: (0, buffer.max_row) - cannot scroll past document edges
    /// - Smooth scrolling: Updates scroll state for smooth animation
    ///
    /// # Integration
    ///
    /// This is not directly bound to an action but called by the GUI layer's mouse
    /// event handler when scroll events occur. The GUI passes scroll deltas and
    /// modifier state (for fast_scroll) to this method.
    ///
    /// # Related
    ///
    /// - [`crate::scroll::ScrollState`] - manages scroll position and animation
    /// - [`crate::scroll::ScrollDelta`] - represents scroll input deltas
    /// - GUI mouse event handler - dispatches scroll events to this method
    pub fn handle_scroll(
        &mut self,
        delta: &crate::scroll::ScrollDelta,
        fast_scroll: bool,
        cx: &mut Context<Self>,
    ) {
        // Scroll sensitivity values
        let base_sensitivity = 1.0;
        let fast_multiplier = 3.0;

        // Line height for delta conversion
        let line_height = 20.0; // Default line height in pixels

        // Calculate new scroll position using existing infrastructure
        let new_position = self.scroll.apply_scroll_delta(
            delta,
            line_height,
            base_sensitivity,
            fast_multiplier,
            fast_scroll,
        );

        // Apply bounds checking
        let buffer_item_entity = self.active_buffer(cx);
        let max_scroll_y = if let Some(merge_rows) = self.merge_display_row_count {
            (merge_rows as f32 - 1.0).max(0.0)
        } else if self.is_in_diff_review(cx) {
            let mode = Some(self.review_comparison_mode());
            let display_buffer = buffer_item_entity.read(cx).display_buffer(cx, true, mode);
            (display_buffer.row_count() as f32 - 1.0).max(0.0)
        } else {
            let buffer_item = buffer_item_entity.read(cx);
            let max_point = buffer_item.buffer().read(cx).max_point();
            max_point.row as f32
        };

        let bounded_position = gpui::point(
            new_position.x.max(0.0),
            new_position.y.max(0.0).min(max_scroll_y),
        );

        // Update scroll position
        self.scroll.scroll_to(bounded_position);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scroll::ScrollDelta;
    use gpui::TestAppContext;

    #[gpui::test]
    fn scrolls_down_with_positive_delta(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        // Create multi-line buffer
        stoat.update(|s, cx| {
            let buffer_item = s.active_buffer(cx);
            buffer_item.update(cx, |item, cx| {
                let buffer = item.buffer();
                buffer.update(cx, |buf, _cx| {
                    buf.edit(vec![(0..0, "Line 1\nLine 2\nLine 3\nLine 4\nLine 5")]);
                });
            });
        });

        stoat.update(|s, cx| {
            let initial_y = s.scroll.position.y;

            // Scroll down by 2 lines
            let delta = ScrollDelta::Lines(gpui::point(0.0, 2.0));
            s.handle_scroll(&delta, false, cx);

            // Should have scrolled down
            assert!(s.scroll.position.y > initial_y);
        });
    }

    #[gpui::test]
    fn scrolls_up_with_negative_delta(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        // Create multi-line buffer and scroll down first
        stoat.update(|s, cx| {
            let buffer_item = s.active_buffer(cx);
            buffer_item.update(cx, |item, cx| {
                let buffer = item.buffer();
                buffer.update(cx, |buf, _cx| {
                    buf.edit(vec![(0..0, "Line 1\nLine 2\nLine 3\nLine 4\nLine 5")]);
                });
            });

            // Scroll down to line 3
            s.scroll.scroll_to(gpui::point(0.0, 3.0));
        });

        stoat.update(|s, cx| {
            let initial_y = s.scroll.position.y;

            // Scroll up by 1 line
            let delta = ScrollDelta::Lines(gpui::point(0.0, -1.0));
            s.handle_scroll(&delta, false, cx);

            // Should have scrolled up
            assert!(s.scroll.position.y < initial_y);
        });
    }

    #[gpui::test]
    fn fast_scroll_multiplier_increases_speed(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        // Create multi-line buffer
        stoat.update(|s, cx| {
            let buffer_item = s.active_buffer(cx);
            buffer_item.update(cx, |item, cx| {
                let buffer = item.buffer();
                buffer.update(cx, |buf, _cx| {
                    buf.edit(vec![(0..0, "Line 1\nLine 2\nLine 3\nLine 4\nLine 5\nLine 6\nLine 7\nLine 8\nLine 9\nLine 10")]);
                });
            });
        });

        let (normal_delta, fast_delta) = stoat.update(|s, cx| {
            let initial_y = s.scroll.position.y;

            // Normal scroll
            let delta = ScrollDelta::Lines(gpui::point(0.0, 1.0));
            s.handle_scroll(&delta, false, cx);
            let normal_delta = s.scroll.position.y - initial_y;

            // Reset and try fast scroll
            s.scroll.scroll_to(gpui::point(0.0, initial_y));
            s.handle_scroll(&delta, true, cx);
            let fast_delta = s.scroll.position.y - initial_y;

            (normal_delta, fast_delta)
        });

        // Fast scroll should move more than normal scroll
        assert!(fast_delta > normal_delta);
    }

    #[gpui::test]
    fn enforces_upper_bound(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        // Create small buffer (3 lines)
        stoat.update(|s, cx| {
            let buffer_item = s.active_buffer(cx);
            buffer_item.update(cx, |item, cx| {
                let buffer = item.buffer();
                buffer.update(cx, |buf, _cx| {
                    buf.edit(vec![(0..0, "Line 1\nLine 2\nLine 3")]);
                });
            });
        });

        stoat.update(|s, cx| {
            // Try to scroll way past the end
            let delta = ScrollDelta::Lines(gpui::point(0.0, 100.0));
            s.handle_scroll(&delta, false, cx);

            // Should be clamped to max row (2, since 0-indexed)
            assert!(s.scroll.position.y <= 2.0);
        });
    }

    #[gpui::test]
    fn enforces_lower_bound(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        stoat.update(|s, cx| {
            // Try to scroll way up (negative)
            let delta = ScrollDelta::Lines(gpui::point(0.0, -100.0));
            s.handle_scroll(&delta, false, cx);

            // Should be clamped to 0
            assert_eq!(s.scroll.position.y, 0.0);
        });
    }
}

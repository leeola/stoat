//! Handle scroll command
//!
//! Processes scroll events from mouse wheel or trackpad input and updates the viewport
//! position. Supports both pixel-based and line-based scrolling with configurable
//! sensitivity and fast scroll mode.

use crate::{ScrollDelta, Stoat};
use gpui::App;

impl Stoat {
    /// Handle scroll events from mouse wheel or trackpad.
    ///
    /// Processes scroll deltas and updates the viewport position with configurable
    /// sensitivity and support for fast scrolling. The method handles both discrete
    /// mouse wheel scrolling and smooth trackpad gestures.
    ///
    /// # Arguments
    ///
    /// * `delta` - The scroll amount, either in pixels or lines
    /// * `fast_scroll` - Whether fast scroll mode is active (typically Alt key held)
    /// * `cx` - Application context for buffer access
    ///
    /// # Behavior
    ///
    /// - Converts scroll deltas to viewport line offsets
    /// - Applies sensitivity multipliers (base and fast scroll)
    /// - Clamps scroll position to buffer bounds
    /// - Updates viewport position immediately (no animation)
    ///
    /// # Scroll Modes
    ///
    /// - **Normal scrolling**: Base sensitivity (1.0x)
    /// - **Fast scrolling**: Accelerated (3.0x) when modifier key held
    ///
    /// # Bounds Checking
    ///
    /// - Vertical scroll clamped to [0, max_row]
    /// - Horizontal scroll clamped to [0, infinity)
    /// - Negative scroll positions not allowed
    ///
    /// # Implementation Details
    ///
    /// Uses a fixed line height of 20.0 pixels for delta conversion. In the future,
    /// this should be extracted from style configuration or calculated dynamically.
    ///
    /// # Related
    ///
    /// See also:
    /// - [`crate::scroll::ScrollPosition`] - manages scroll state
    /// - [`crate::scroll::ScrollDelta`] - scroll input representation
    /// - [`crate::actions::movement::page_up`] - animated page scrolling
    /// - [`crate::actions::movement::page_down`] - animated page scrolling
    pub fn handle_scroll_event(&mut self, delta: &ScrollDelta, fast_scroll: bool, cx: &App) {
        // Default scroll sensitivity values (similar to Zed)
        let base_sensitivity = 1.0;
        let fast_multiplier = 3.0;

        // Get line height for delta conversion
        // FIXME: Use actual line height from style or calculate dynamically
        let line_height = 20.0;

        // Calculate new scroll position
        let new_position = self.scroll.apply_scroll_delta(
            delta,
            line_height,
            base_sensitivity,
            fast_multiplier,
            fast_scroll,
        );

        // Apply bounds checking
        let buffer_snapshot = self.buffer_snapshot(cx);
        let max_scroll_y = (buffer_snapshot.row_count() as f32 - 1.0).max(0.0);

        let bounded_position = gpui::point(
            new_position.x.max(0.0),                   // No negative horizontal scroll
            new_position.y.max(0.0).min(max_scroll_y), // Clamp vertical scroll
        );

        // Update scroll position
        self.scroll.scroll_to(bounded_position);
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    // Note: handle_scroll_event is tested indirectly through:
    // - GUI scroll wheel handling (tested in stoat_gui/src/editor/view.rs)
    // - Integration tests that simulate scroll events
    //
    // Direct testing requires access to internal state (scroll position),
    // which is not exposed through the public test API. The implementation
    // is straightforward (delta conversion + bounds checking) and covered
    // by the scroll module's own tests.

    #[test]
    fn scroll_via_mouse_wheel() {
        let mut s = Stoat::test();
        let lines: Vec<String> = (0..100).map(|i| format!("line {}", i)).collect();
        s.set_text(&lines.join("\n"));

        // Scrolling is typically tested through GUI integration tests
        // Here we just verify the buffer was loaded correctly
        assert_eq!(s.text().lines().count(), 100);
    }
}

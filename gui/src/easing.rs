//! Easing functions for smooth animations.
//!
//! This module provides various easing curves for animation interpolation,
//! enabling smooth acceleration and deceleration of animated values.

use std::time::Duration;

/// Calculates the animation progress as a value between 0.0 and 1.0.
pub fn progress(elapsed: Duration, total: Duration) -> f32 {
    let elapsed_ms = elapsed.as_millis() as f32;
    let total_ms = total.as_millis() as f32;
    (elapsed_ms / total_ms).min(1.0).max(0.0)
}

/// Cubic ease-out function for natural deceleration.
/// Starts fast and gradually slows down.
pub fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Cubic ease-in-out function for smooth acceleration and deceleration.
/// Starts slow, speeds up in the middle, then slows down.
pub fn ease_in_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// Quadratic ease-out function for gentler deceleration.
pub fn ease_out_quad(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t) * (1.0 - t)
}

/// Linear interpolation (no easing).
pub fn linear(t: f32) -> f32 {
    t.clamp(0.0, 1.0)
}

/// Interpolates between two values using an easing function.
pub fn interpolate(from: f32, to: f32, progress: f32, easing_fn: fn(f32) -> f32) -> f32 {
    let eased_progress = easing_fn(progress);
    from + (to - from) * eased_progress
}

/// Calculates appropriate animation duration based on scroll distance.
pub fn duration_for_distance(distance: f32) -> Duration {
    let abs_distance = distance.abs();

    if abs_distance <= 3.0 {
        // Short scrolls: quick animation
        Duration::from_millis(150)
    } else if abs_distance <= 20.0 {
        // Medium scrolls (e.g., half-page)
        Duration::from_millis(250)
    } else {
        // Large scrolls: longer but still snappy
        Duration::from_millis(350)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_calculation() {
        assert_eq!(
            progress(Duration::from_millis(0), Duration::from_millis(100)),
            0.0
        );
        assert_eq!(
            progress(Duration::from_millis(50), Duration::from_millis(100)),
            0.5
        );
        assert_eq!(
            progress(Duration::from_millis(100), Duration::from_millis(100)),
            1.0
        );
        assert_eq!(
            progress(Duration::from_millis(150), Duration::from_millis(100)),
            1.0
        );
    }

    #[test]
    fn easing_functions_bounds() {
        // Test that all easing functions respect 0-1 bounds
        for t in [0.0, 0.25, 0.5, 0.75, 1.0, -0.5, 1.5] {
            let result = ease_out_cubic(t);
            assert!(result >= 0.0 && result <= 1.0);

            let result = ease_in_out_cubic(t);
            assert!(result >= 0.0 && result <= 1.0);

            let result = ease_out_quad(t);
            assert!(result >= 0.0 && result <= 1.0);

            let result = linear(t);
            assert!(result >= 0.0 && result <= 1.0);
        }
    }

    #[test]
    fn interpolation() {
        assert_eq!(interpolate(0.0, 100.0, 0.0, linear), 0.0);
        assert_eq!(interpolate(0.0, 100.0, 0.5, linear), 50.0);
        assert_eq!(interpolate(0.0, 100.0, 1.0, linear), 100.0);

        assert_eq!(interpolate(100.0, 0.0, 0.5, linear), 50.0);
        assert_eq!(interpolate(-50.0, 50.0, 0.5, linear), 0.0);
    }

    #[test]
    fn duration_scaling() {
        assert_eq!(duration_for_distance(1.0), Duration::from_millis(150));
        assert_eq!(duration_for_distance(10.0), Duration::from_millis(250));
        assert_eq!(duration_for_distance(50.0), Duration::from_millis(350));
        assert_eq!(duration_for_distance(-50.0), Duration::from_millis(350));
    }
}

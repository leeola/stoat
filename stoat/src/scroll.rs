use gpui::{point, Pixels, Point};
use std::time::{Duration, Instant};

/// Scroll delta for mouse wheel and trackpad events
///
/// Different input devices provide different types of scroll data:
/// - Mouse wheels typically provide discrete line-based scrolling
/// - Trackpads provide precise pixel-based scrolling with touch phases
#[derive(Debug, Clone, PartialEq)]
pub enum ScrollDelta {
    /// Precise pixel-based scrolling (trackpads)
    Pixels(Point<Pixels>),
    /// Line-based scrolling (mouse wheels)
    Lines(Point<f32>),
}

/// Duration for scroll animations in milliseconds
const SCROLL_ANIMATION_DURATION_MS: u64 = 150; // Fast and responsive scrolling

/// Manages scroll position for the editor with animation support
#[derive(Clone, Debug)]
pub struct ScrollPosition {
    /// The current scroll position as a fractional point
    /// x: horizontal scroll offset (in columns)
    /// y: vertical scroll offset (in rows)
    pub position: gpui::Point<f32>,

    /// Target position for animated scrolling
    pub target_position: Option<gpui::Point<f32>>,

    /// Start position when animation began
    pub animation_start_position: Option<gpui::Point<f32>>,

    /// Time when the current animation started
    pub animation_start_time: Option<Instant>,

    /// Duration for the animation
    pub animation_duration: Duration,
}

impl ScrollPosition {
    pub fn new() -> Self {
        Self {
            position: point(0.0, 0.0),
            target_position: None,
            animation_start_position: None,
            animation_start_time: None,
            animation_duration: Duration::from_millis(SCROLL_ANIMATION_DURATION_MS),
        }
    }

    pub fn reset(&mut self) {
        self.position = point(0.0, 0.0);
        self.target_position = None;
        self.animation_start_position = None;
        self.animation_start_time = None;
    }

    pub fn scroll_to(&mut self, position: gpui::Point<f32>) {
        self.position = position;
        self.target_position = None;
        self.animation_start_position = None;
        self.animation_start_time = None;
    }

    /// Start an animated scroll to the target position
    pub fn start_animation_to(&mut self, target: gpui::Point<f32>) {
        self.target_position = Some(target);
        self.animation_start_position = Some(self.position);
        self.animation_start_time = Some(Instant::now());
    }

    /// Update the scroll position based on animation progress
    /// Returns true if animation is complete
    pub fn update_animation(&mut self) -> bool {
        if let (Some(target), Some(start_pos), Some(start_time)) = (
            self.target_position,
            self.animation_start_position,
            self.animation_start_time,
        ) {
            let elapsed = start_time.elapsed();

            if elapsed >= self.animation_duration {
                // Animation complete
                self.position = target;
                self.target_position = None;
                self.animation_start_position = None;
                self.animation_start_time = None;
                return true;
            }

            // Calculate progress (0.0 to 1.0)
            let progress = elapsed.as_secs_f32() / self.animation_duration.as_secs_f32();

            // Apply easing function (cubic ease-in-out)
            let eased_progress = Self::ease_in_out_cubic(progress);

            // Interpolate position
            self.position.x = start_pos.x + (target.x - start_pos.x) * eased_progress;
            self.position.y = start_pos.y + (target.y - start_pos.y) * eased_progress;

            return false;
        }

        true // No animation in progress
    }

    /// Cubic ease-in-out function - the industry standard for smooth animations
    /// Provides natural acceleration and deceleration
    fn ease_in_out_cubic(t: f32) -> f32 {
        // Clamp t to [0, 1] for safety
        let t = t.clamp(0.0, 1.0);

        // Cubic ease-in-out: smooth acceleration and deceleration
        // Used by CSS transitions, iOS, Android, and most modern apps
        if t < 0.5 {
            4.0 * t * t * t
        } else {
            1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
        }
    }

    /// Check if an animation is currently in progress
    pub fn is_animating(&self) -> bool {
        self.target_position.is_some()
    }

    /// Apply a scroll delta to the current position
    ///
    /// This method converts different delta types to screen coordinates and applies
    /// sensitivity multipliers for smooth scrolling behavior.
    pub fn apply_scroll_delta(
        &mut self,
        delta: &ScrollDelta,
        line_height: f32,
        sensitivity: f32,
        fast_multiplier: f32,
        is_fast: bool,
    ) -> gpui::Point<f32> {
        let multiplier = if is_fast { fast_multiplier } else { 1.0 };

        let scroll_offset = match delta {
            ScrollDelta::Pixels(pixel_delta) => {
                // Convert pixels to lines for consistent behavior
                gpui::point(
                    pixel_delta.x.0 / line_height * sensitivity * multiplier,
                    pixel_delta.y.0 / line_height * sensitivity * multiplier,
                )
            },
            ScrollDelta::Lines(line_delta) => {
                // Line-based scrolling (mouse wheel)
                gpui::point(
                    line_delta.x * sensitivity * multiplier,
                    line_delta.y * sensitivity * multiplier,
                )
            },
        };

        // Apply the scroll offset to current position
        let new_position = gpui::point(
            self.position.x + scroll_offset.x,
            (self.position.y + scroll_offset.y).max(0.0), // Prevent negative Y scroll
        );

        new_position
    }
}

impl Default for ScrollPosition {
    fn default() -> Self {
        Self::new()
    }
}

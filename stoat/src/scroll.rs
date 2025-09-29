use gpui::point;
use std::time::{Duration, Instant};

/// Duration for scroll animations in milliseconds
const SCROLL_ANIMATION_DURATION_MS: u64 = 100;

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

    /// Smooth easing function with quick start and gentle stop
    /// Uses smootherstep (Ken Perlin's improved version) for ultra-smooth animation
    fn ease_in_out_cubic(t: f32) -> f32 {
        // Clamp t to [0, 1] for safety
        let t = t.clamp(0.0, 1.0);

        // Smootherstep function - zero 1st and 2nd derivatives at endpoints
        // This creates the smoothest possible animation with no jerky movements
        t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
    }

    /// Check if an animation is currently in progress
    pub fn is_animating(&self) -> bool {
        self.target_position.is_some()
    }
}

impl Default for ScrollPosition {
    fn default() -> Self {
        Self::new()
    }
}

use gpui::point;

/// Manages scroll position for the editor
#[derive(Clone, Debug)]
pub struct ScrollPosition {
    /// The scroll position as a fractional point
    /// x: horizontal scroll offset (in columns)
    /// y: vertical scroll offset (in rows)
    pub position: gpui::Point<f32>,
}

impl ScrollPosition {
    pub fn new() -> Self {
        Self {
            position: point(0.0, 0.0),
        }
    }

    pub fn reset(&mut self) {
        self.position = point(0.0, 0.0);
    }

    pub fn scroll_to(&mut self, position: gpui::Point<f32>) {
        self.position = position;
    }
}

impl Default for ScrollPosition {
    fn default() -> Self {
        Self::new()
    }
}

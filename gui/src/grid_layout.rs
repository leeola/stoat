use stoat_core::view::GridPosition;

/// Manages conversion between grid coordinates and screen pixels
#[derive(Debug, Clone)]
pub struct GridLayout {
    /// Width of each grid cell in pixels
    pub cell_width: f32,
    /// Height of each grid cell in pixels
    pub cell_height: f32,
    /// Horizontal spacing between cells
    pub h_spacing: f32,
    /// Vertical spacing between cells
    pub v_spacing: f32,
    /// Origin offset for handling negative coordinates
    pub origin_offset: (f32, f32),
}

impl GridLayout {
    /// Create a new grid layout with default settings
    pub fn new() -> Self {
        Self {
            cell_width: 200.0,
            cell_height: 150.0,
            h_spacing: 20.0,
            v_spacing: 20.0,
            origin_offset: (0.0, 0.0), // Center at origin
        }
    }

    /// Convert grid position to screen coordinates
    pub fn grid_to_screen(&self, pos: GridPosition) -> (f32, f32) {
        let x = self.origin_offset.0 + (pos.col as f32) * (self.cell_width + self.h_spacing);
        let y = self.origin_offset.1 + (pos.row as f32) * (self.cell_height + self.v_spacing);
        (x, y)
    }

    /// Convert screen coordinates to nearest grid position
    pub fn screen_to_grid(&self, x: f32, y: f32) -> GridPosition {
        let col = ((x - self.origin_offset.0) / (self.cell_width + self.h_spacing)).round() as i32;
        let row = ((y - self.origin_offset.1) / (self.cell_height + self.v_spacing)).round() as i32;
        GridPosition::new(row, col)
    }

    /// Get the size of a grid cell (for node rendering)
    pub fn cell_size(&self) -> (f32, f32) {
        (self.cell_width, self.cell_height)
    }

    /// Check if a screen point is within a grid cell
    pub fn hit_test(&self, screen_x: f32, screen_y: f32, grid_pos: GridPosition) -> bool {
        let (cell_x, cell_y) = self.grid_to_screen(grid_pos);
        screen_x >= cell_x
            && screen_x <= cell_x + self.cell_width
            && screen_y >= cell_y
            && screen_y <= cell_y + self.cell_height
    }
}

impl Default for GridLayout {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;

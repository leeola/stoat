use crate::node::NodeId;
use std::collections::HashMap;

/// Runtime view state - computed per session, not persisted
/// All coordinates are in abstract integer units
#[derive(Debug, Clone)]
pub struct ViewState {
    /// Position of each node in the view
    pub positions: HashMap<NodeId, Position>,
    /// Size of each node
    pub sizes: HashMap<NodeId, Size>,
    /// Current viewport into the canvas
    pub viewport: Viewport,
    /// Currently selected node
    pub selected: Option<NodeId>,
    /// Zoom level (1.0 = 100%)
    pub zoom: f32,
}

/// Position in integer units (can be negative for off-screen)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Position {
    pub x: i32,
    pub y: i32,
}

/// Size in integer units
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

/// Viewport defining the visible area
#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    /// Top-left corner of the viewport in canvas coordinates
    pub offset: Position,
    /// Size of the viewport
    pub size: Size,
}

impl Position {
    pub fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

impl Size {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            positions: HashMap::new(),
            sizes: HashMap::new(),
            viewport: Viewport {
                offset: Position::new(0, 0),
                size: Size::new(1280, 720),
            },
            selected: None,
            zoom: 1.0,
        }
    }
}

impl ViewState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the position of a node
    pub fn set_position(&mut self, id: NodeId, pos: Position) {
        self.positions.insert(id, pos);
    }

    /// Set the size of a node
    pub fn set_size(&mut self, id: NodeId, size: Size) {
        self.sizes.insert(id, size);
    }

    /// Select a node
    pub fn select(&mut self, id: NodeId) {
        self.selected = Some(id);
    }

    /// Clear selection
    pub fn clear_selection(&mut self) {
        self.selected = None;
    }

    /// Update the viewport size (e.g., when window resizes)
    pub fn update_viewport_size(&mut self, width: u32, height: u32) {
        self.viewport.size = Size::new(width, height);
    }

    /// Center viewport on the selected node
    pub fn center_on_selected(&mut self) {
        if let Some(id) = self.selected {
            if let Some(&pos) = self.positions.get(&id) {
                if let Some(&size) = self.sizes.get(&id) {
                    // Calculate center of the node
                    let node_center_x = pos.x + (size.width / 2) as i32;
                    let node_center_y = pos.y + (size.height / 2) as i32;

                    // Calculate viewport offset to center the node
                    let viewport_center_x = (self.viewport.size.width / 2) as i32;
                    let viewport_center_y = (self.viewport.size.height / 2) as i32;

                    self.viewport.offset = Position {
                        x: node_center_x - viewport_center_x,
                        y: node_center_y - viewport_center_y,
                    };
                }
            }
        }
    }

    /// Apply a simple grid layout to position nodes
    pub fn apply_grid_layout(&mut self, node_ids: &[NodeId], columns: usize, spacing: i32) {
        for (index, &id) in node_ids.iter().enumerate() {
            let col = (index % columns) as i32;
            let row = (index / columns) as i32;

            let position = Position {
                x: col * spacing + 50,
                y: row * spacing + 50,
            };

            self.positions.insert(id, position);

            // Set default size if not already set
            self.sizes.entry(id).or_insert(Size::new(400, 600));
        }
    }

    /// Initialize layout with default positions
    pub fn initialize_default_layout(&mut self, node_ids: &[NodeId]) {
        self.apply_grid_layout(node_ids, 3, 500);
    }

    /// Convert a canvas position to screen coordinates
    pub fn canvas_to_screen(&self, pos: Position) -> (f32, f32) {
        let screen_x = (pos.x - self.viewport.offset.x) as f32 * self.zoom;
        let screen_y = (pos.y - self.viewport.offset.y) as f32 * self.zoom;
        (screen_x, screen_y)
    }

    /// Convert screen coordinates to canvas position
    pub fn screen_to_canvas(&self, screen_x: f32, screen_y: f32) -> Position {
        Position {
            x: (screen_x / self.zoom) as i32 + self.viewport.offset.x,
            y: (screen_y / self.zoom) as i32 + self.viewport.offset.y,
        }
    }
}

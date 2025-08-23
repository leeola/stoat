use crate::buffer_manager::BufferId;
use serde::{Deserialize, Serialize};

/// Position in a grid coordinate system
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct GridPosition {
    pub row: i32,
    pub col: i32,
}

impl GridPosition {
    pub fn new(row: i32, col: i32) -> Self {
        Self { row, col }
    }

    /// Offset this position by the given delta
    pub fn offset(&self, row_delta: i32, col_delta: i32) -> Self {
        Self {
            row: self.row + row_delta,
            col: self.col + col_delta,
        }
    }

    /// Manhattan distance between two positions
    pub fn distance(&self, other: &Self) -> i32 {
        (self.row - other.row).abs() + (self.col - other.col).abs()
    }
}

#[derive(Debug, Default, Clone)]
pub struct View {
    pub buffers: Vec<BufferView>,
}

impl View {
    pub fn add_buffer_view(&mut self, id: BufferId, pos: GridPosition) {
        self.buffers.push(BufferView { id, pos });
    }
}

#[derive(Debug, Clone)]
pub struct BufferView {
    pub id: BufferId,
    pub pos: GridPosition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewEvent {
    Close,
}

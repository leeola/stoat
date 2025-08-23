use crate::buffer_manager::BufferId;

/// Runtime view state - computed per session, not persisted
/// Manages the currently selected buffer without spatial positioning
#[derive(Debug, Clone, Default)]
pub struct ViewState {
    /// Currently selected/active buffer
    pub selected: Option<BufferId>,
}

impl ViewState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Select a buffer
    pub fn select(&mut self, id: BufferId) {
        self.selected = Some(id);
    }

    /// Clear selection
    pub fn clear_selection(&mut self) {
        self.selected = None;
    }
}

use crate::{
    buffer_manager::{BufferId, BufferManager, SerializableBuffer},
    view::View,
    view_state::ViewState,
    Result,
};
use serde::{Deserialize, Serialize};

#[derive(Default)]
pub struct Workspace {
    /// Buffer manager for all text content
    buffers: BufferManager,
    /// View into the workspace (canvas/editor layout)
    view: View,
    /// View state for cursor positions and viewport
    view_state: ViewState,
}

/// Serializable representation of workspace state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableWorkspace {
    /// All buffers in the workspace
    pub buffers: Vec<SerializableBuffer>,
    /// View state data (cursor positions, viewport)
    pub view_data: Option<String>, // Simplified view serialization
}

impl From<&Workspace> for SerializableWorkspace {
    fn from(workspace: &Workspace) -> Self {
        let buffers = workspace.buffers.get_serializable_buffers();

        Self {
            buffers,
            view_data: None, // TODO: implement view serialization
        }
    }
}

impl SerializableWorkspace {}

impl Workspace {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new buffer in the workspace
    pub fn create_buffer(&mut self, name: String) -> BufferId {
        self.buffers.create_buffer(name)
    }

    /// Create a buffer from a file
    pub fn create_buffer_from_file(&mut self, path: std::path::PathBuf) -> Result<BufferId> {
        self.buffers.create_buffer_from_file(path)
    }

    /// Create a buffer with content
    pub fn create_buffer_with_content(&mut self, name: String, content: String) -> BufferId {
        self.buffers.create_buffer_with_content(name, content)
    }

    /// Get a buffer by ID
    pub fn get_buffer(&self, id: BufferId) -> Option<&stoat_text::buffer::Buffer> {
        self.buffers.get(id)
    }

    /// Get a mutable buffer by ID
    pub fn get_buffer_mut(&mut self, id: BufferId) -> Option<&mut stoat_text::buffer::Buffer> {
        self.buffers.get_mut(id)
    }

    /// List all buffers in the workspace
    pub fn list_buffers(&self) -> Vec<(BufferId, &crate::buffer_manager::BufferInfo)> {
        self.buffers.list_buffers()
    }

    /// Create a workspace from a serializable representation
    pub fn from_serializable(serializable: SerializableWorkspace) -> Self {
        let mut workspace = Self::default();

        // Restore buffers
        workspace
            .buffers
            .restore_from_serializable(serializable.buffers);

        // TODO: deserialize view from view_data

        workspace
    }

    /// Get the buffer manager
    pub fn buffers(&self) -> &BufferManager {
        &self.buffers
    }

    /// Get mutable access to the buffer manager
    pub fn buffers_mut(&mut self) -> &mut BufferManager {
        &mut self.buffers
    }

    pub fn view(&self) -> &View {
        &self.view
    }

    pub fn view_mut(&mut self) -> &mut View {
        &mut self.view
    }

    /// Get the view state for rendering
    pub fn view_state(&self) -> &ViewState {
        &self.view_state
    }

    /// Get mutable access to view state
    pub fn view_state_mut(&mut self) -> &mut ViewState {
        &mut self.view_state
    }

    /// Initialize view layout for all buffers
    pub fn initialize_layout(&mut self) {
        // For now, we don't need to initialize layout for buffers
        // as they will be displayed fullscreen in the editor
        // This is kept for compatibility but can be removed later
    }
}

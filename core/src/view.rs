use crate::buffer_manager::BufferId;

#[derive(Debug, Default, Clone)]
pub struct View {
    pub buffers: Vec<BufferView>,
}

impl View {
    pub fn add_buffer_view(&mut self, id: BufferId) {
        self.buffers.push(BufferView { id });
    }
}

#[derive(Debug, Clone)]
pub struct BufferView {
    pub id: BufferId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewEvent {
    Close,
}

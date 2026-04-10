use crate::{
    buffer::{BufferId, SharedBuffer},
    display_map::DisplayMap,
    multi_buffer::MultiBuffer,
    review::ReviewRow,
};
use slotmap::new_key_type;
use stoat_scheduler::Executor;

new_key_type! { pub struct EditorId; }

pub(crate) struct EditorState {
    pub(crate) buffer_id: BufferId,
    pub(crate) display_map: DisplayMap,
    pub(crate) scroll_row: u32,
    pub(crate) review_rows: Option<Vec<ReviewRow>>,
}

impl EditorState {
    pub(crate) fn new(buffer_id: BufferId, buffer: SharedBuffer, executor: Executor) -> Self {
        let multi_buffer = MultiBuffer::singleton(buffer_id, buffer);
        Self {
            buffer_id,
            display_map: DisplayMap::new(multi_buffer, executor),
            scroll_row: 0,
            review_rows: None,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn from_multi_buffer(
        buffer_id: BufferId,
        multi_buffer: MultiBuffer,
        executor: Executor,
    ) -> Self {
        Self {
            buffer_id,
            display_map: DisplayMap::new(multi_buffer, executor),
            scroll_row: 0,
            review_rows: None,
        }
    }
}

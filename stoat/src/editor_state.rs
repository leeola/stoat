use crate::{
    buffer::{BufferId, SharedBuffer},
    display_map::DisplayMap,
    multi_buffer::MultiBuffer,
    review_session::ReviewViewState,
    selection::SelectionsCollection,
};
use slotmap::new_key_type;
use stoat_scheduler::Executor;

new_key_type! { pub struct EditorId; }

pub(crate) struct EditorState {
    pub(crate) buffer_id: BufferId,
    pub(crate) display_map: DisplayMap,
    pub(crate) scroll_row: u32,
    /// When `Some`, this editor is a review view; `render_editor` dispatches
    /// to the side-by-side renderer and flattened rows are read from the
    /// cache here. The cache is rebuilt by action handlers whenever the
    /// backing session's `version` advances past `review_view.session_version`.
    pub(crate) review_view: Option<ReviewViewState>,
    pub(crate) selections: SelectionsCollection,
    /// Per-editor cursor for cycling through ambiguous move sources.
    /// `(hunk_line, source_index)` identifies which source the user is
    /// currently focused on; `None` means no active move navigation.
    /// Reset whenever the editor's cursor moves off the owning hunk.
    pub(crate) move_source_cursor: Option<(u32, usize)>,
}

impl EditorState {
    pub(crate) fn new(buffer_id: BufferId, buffer: SharedBuffer, executor: Executor) -> Self {
        let multi_buffer = MultiBuffer::singleton(buffer_id, buffer);
        Self {
            buffer_id,
            display_map: DisplayMap::new(multi_buffer, executor),
            scroll_row: 0,
            review_view: None,
            selections: SelectionsCollection::new(),
            move_source_cursor: None,
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
            review_view: None,
            selections: SelectionsCollection::new(),
            move_source_cursor: None,
        }
    }
}

use crate::{
    buffer::{BufferId, SharedBuffer},
    display_map::DisplayMap,
    multi_buffer::MultiBuffer,
    review_session::ReviewViewState,
    selection::SelectionsCollection,
};
use serde::{Deserialize, Serialize};
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

/// Snapshot of an [`EditorState`] suitable for workspace save/load.
///
/// Anchors in `selections` survive restore because [`crate::buffer::TextBuffer`]
/// replays its op log on load, reassigning the same sequential timestamps.
/// `display_map` and `review_view` are omitted: the display map rebuilds from
/// the restored buffer, and review views depend on a review session (whose
/// persistence is tracked separately).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct EditorStateSnapshot {
    pub(crate) buffer_id: BufferId,
    pub(crate) scroll_row: u32,
    pub(crate) selections: SelectionsCollection,
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

    pub(crate) fn snapshot(&self) -> EditorStateSnapshot {
        EditorStateSnapshot {
            buffer_id: self.buffer_id,
            scroll_row: self.scroll_row,
            selections: self.selections.clone(),
            move_source_cursor: self.move_source_cursor,
        }
    }

    pub(crate) fn restore(
        snap: EditorStateSnapshot,
        buffer: SharedBuffer,
        executor: Executor,
    ) -> Self {
        let mut state = Self::new(snap.buffer_id, buffer, executor);
        state.scroll_row = snap.scroll_row;
        state.selections = snap.selections;
        state.move_source_cursor = snap.move_source_cursor;
        state
    }
}

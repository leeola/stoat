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
    // FIXME: Selections not persisted across workspace save/load. `text::Anchor`
    // is plain data and serializable, but anchors carry an edit-timestamp that
    // indexes into the buffer's fragment tree. A freshly-loaded buffer rebuilds
    // its fragment tree from disk (no edit history), so saved anchors won't
    // resolve. Resolution paths: (a) persist the buffer's undo/edit history
    // alongside content and replay on load so timestamps line up, or (b) fall
    // back to byte offsets at save time and resolve to anchors after load
    // (lossy across external edits). Blocked on buffer-history serialization.
    pub(crate) selections: SelectionsCollection,
    /// Per-editor cursor for cycling through ambiguous move sources.
    /// `(hunk_line, source_index)` identifies which source the user is
    /// currently focused on; `None` means no active move navigation.
    /// Reset whenever the editor's cursor moves off the owning hunk.
    pub(crate) move_source_cursor: Option<(u32, usize)>,
}

/// Snapshot of an [`EditorState`] suitable for workspace save/load.
///
/// Only the fields that can round-trip without an [`Executor`] and without
/// fragment-tree stability are captured; see the FIXME on
/// [`EditorState::selections`] for the anchor-persistence gap.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct EditorStateSnapshot {
    pub(crate) buffer_id: BufferId,
    pub(crate) scroll_row: u32,
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
        }
    }

    pub(crate) fn restore(
        snap: EditorStateSnapshot,
        buffer: SharedBuffer,
        executor: Executor,
    ) -> Self {
        let mut state = Self::new(snap.buffer_id, buffer, executor);
        state.scroll_row = snap.scroll_row;
        state
    }
}

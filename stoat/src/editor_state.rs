use crate::{
    buffer::{BufferId, SharedBuffer},
    display_map::{CachedHighlightEndpoints, DisplayMap},
    multi_buffer::MultiBuffer,
    review_session::ReviewViewState,
    selection::SelectionsCollection,
};
use serde::{Deserialize, Serialize};
use slotmap::new_key_type;
use stoat_scheduler::Executor;

new_key_type! { pub struct EditorId; }

/// Cached in-buffer search matches for one `(version, query, visible span)`.
///
/// Lets `render_editor` reuse match byte-ranges across frames instead of
/// re-materializing the visible rope slice and re-scanning with the regex every
/// frame a search is active. Recomputed when the buffer version, the query, or
/// the visible byte span changes. The visible span covers scrolling and
/// folding, which move the window. The per-match display mapping stays
/// per-frame.
pub(crate) struct SearchMatchCache {
    pub(crate) version: u64,
    pub(crate) query: String,
    /// Visible buffer byte span the matches were scanned over. Part of the key
    /// because a scroll moves it without bumping `version`.
    pub(crate) visible: std::ops::Range<usize>,
    /// Byte ranges `[start, end)` of each non-empty match within `visible`,
    /// stored as absolute buffer offsets.
    pub(crate) matches: Vec<(usize, usize)>,
    /// Scratch holding the scanned window text, retained across rebuilds to
    /// reuse the allocation. Not part of the cache key.
    pub(crate) window: String,
}

pub(crate) struct EditorState {
    pub(crate) buffer_id: BufferId,
    /// This editor's input mode (`normal`, `insert`, `select`, ...). Held
    /// per-editor so the mode survives focus moving to another pane and back,
    /// rather than living in one global slot shared by every pane.
    pub(crate) mode: String,
    pub(crate) display_map: DisplayMap,
    pub(crate) scroll_row: u32,
    /// Fractional top row for inertial scroll. [`Self::scroll_row`] stays
    /// equal to `scroll_offset.floor()` and drives the integer-row render and
    /// pool paths. The fraction carries sub-row glide between animation frames.
    pub(crate) scroll_offset: f32,
    /// Inertial scroll velocity in rows per second. Nonzero only while a wheel
    /// flick is coasting. The momentum step decays it to zero at rest.
    pub(crate) scroll_velocity: f32,
    /// Set when a mouse-wheel scroll moves the view off the cursor, consumed by
    /// the next key. The post-key view-follow re-couples the view to the cursor
    /// while this is set, so a wheel scroll that stranded the cursor snaps back
    /// on the next key even if that key is a clamped no-op. A keyboard scroll
    /// (`z j`/`z k`) leaves this `false`, so it moves the view without the next
    /// key snapping it back. Transient, not persisted.
    pub(crate) scroll_decoupled: bool,
    /// Set while a keyboard page motion eases `scroll_offset` toward the
    /// `scroll_row` target it jumped to. The animation tick eases the offset up
    /// and clears this on settle, and the pool emit trusts the fractional offset
    /// while it is set so the glide reaches the terminal. Transient, not
    /// persisted.
    pub(crate) scroll_glide: bool,
    /// Last-rendered viewport height in rows. Page-motion handlers read
    /// this to compute scroll distance without taking a dependency on
    /// the render pipeline's layout `Rect`. `None` until the editor has
    /// been rendered at least once; handlers fall back to a default.
    pub(crate) viewport_rows: Option<u32>,
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
    /// Stack of selection byte ranges that `expand_selection` walked
    /// up from. `shrink_selection` pops the top to descend back.
    /// Cleared when an expand finds the selection drifted off
    /// [`Self::expansion_tip`], indicating the user wandered off the
    /// chain.
    pub(crate) expansion_history: Vec<std::ops::Range<usize>>,
    /// Range the most recent expand or shrink set the selection to.
    /// `expand` compares the current selection against this to detect
    /// chain breakage and clear the history. Transient; not persisted.
    pub(crate) expansion_tip: Option<std::ops::Range<usize>>,
    /// Cached search-match byte ranges for the current `(version, query)`.
    /// See [`SearchMatchCache`]. Transient render state, not persisted.
    pub(crate) search_match_cache: Option<SearchMatchCache>,
    /// Cached visible syntax-highlight endpoints, keyed by buffer version,
    /// highlight identity, and visible byte range. Lets `render_editor` reuse
    /// resolved endpoints across repaints that change none of those. Transient
    /// render state, not persisted.
    pub(crate) highlight_endpoint_cache: Option<CachedHighlightEndpoints>,
    /// Ids of the LSP inlay-hint inlays currently spliced into this editor's
    /// display map. Kept so a refresh can remove the prior hints before adding
    /// the new set. Transient render state, not persisted.
    pub(crate) hint_inlay_ids: Vec<crate::display_map::InlayId>,
    /// Cached diagnostic gutter severity map, keyed by the diagnostic-set
    /// version. Transient render state, not persisted.
    pub(crate) gutter_severity_cache: Option<crate::render::editor::GutterSeverityCache>,
    /// Cells the last render reserved on the left for the diagnostic gutter (0
    /// or 1), so click-to-offset can subtract the same inset the text rect was
    /// shifted by. Transient render state written by `render_editor`, not
    /// persisted.
    pub(crate) gutter_width: u16,
    /// Absolute terminal cell `(col, row)` where the last render painted this
    /// editor's primary cursor, set only while running inside stoatty so the
    /// terminal cursor can be positioned there instead of a styled grid cell.
    /// `None` when not focused, off-screen, or outside stoatty. Transient
    /// render state, not persisted.
    pub(crate) cursor_screen_cell: Option<(u16, u16)>,
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
            mode: "normal".into(),
            display_map: DisplayMap::new(multi_buffer, executor),
            scroll_row: 0,
            scroll_offset: 0.0,
            scroll_velocity: 0.0,
            scroll_decoupled: false,
            scroll_glide: false,
            viewport_rows: None,
            review_view: None,
            selections: SelectionsCollection::new(),
            move_source_cursor: None,
            expansion_history: Vec::new(),
            expansion_tip: None,
            search_match_cache: None,
            highlight_endpoint_cache: None,
            hint_inlay_ids: Vec::new(),
            gutter_severity_cache: None,
            gutter_width: 0,
            cursor_screen_cell: None,
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
            mode: "normal".into(),
            display_map: DisplayMap::new(multi_buffer, executor),
            scroll_row: 0,
            scroll_offset: 0.0,
            scroll_velocity: 0.0,
            scroll_decoupled: false,
            scroll_glide: false,
            viewport_rows: None,
            review_view: None,
            selections: SelectionsCollection::new(),
            move_source_cursor: None,
            expansion_history: Vec::new(),
            expansion_tip: None,
            search_match_cache: None,
            highlight_endpoint_cache: None,
            hint_inlay_ids: Vec::new(),
            gutter_severity_cache: None,
            gutter_width: 0,
            cursor_screen_cell: None,
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

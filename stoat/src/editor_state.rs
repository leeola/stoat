use crate::{
    buffer::{BufferId, SharedBuffer},
    display_map::{CachedHighlightEndpoints, DisplayMap},
    multi_buffer::MultiBuffer,
    review_session::ReviewViewState,
    selection::SelectionsCollection,
};
use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};
use slotmap::new_key_type;
use stoat_config::WrapMode;
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

/// Which scroll glide, if any, is easing an editor's `scroll_offset` toward its
/// `scroll_row` target.
///
/// The two glides ease at different rates. A keyboard page motion jumps to a
/// distant target and closes it quickly. A wheel report nudges the target a few
/// rows and eases slower, so a stream of reports at wheel rates overlaps into
/// continuous motion instead of pulsing.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum ScrollGlide {
    None,
    Page,
    Wheel,
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
    /// Which glide, if any, is easing `scroll_offset` toward the `scroll_row`
    /// target. The animation tick eases the offset up and clears this to
    /// [`ScrollGlide::None`] on settle, and the pool emit trusts the fractional
    /// offset while a glide is active so it reaches the terminal. Transient, not
    /// persisted.
    pub(crate) scroll_glide: ScrollGlide,
    /// Cursor buffer line last baked into pooled relative-number gutters.
    ///
    /// Under relative numbering the pooled pages' content version folds in the
    /// cursor's line, but a wheel glide's cursor-follow drags that line every
    /// tick. Holding this value steady while [`Self::scroll_glide`] is active
    /// keeps the content version stable so the buffered window does not refill
    /// per dragged row. The settle emit refreshes it. Transient, not persisted.
    pub(crate) pool_current_line: Option<u32>,
    /// Last-rendered viewport height in rows. Page-motion handlers read
    /// this to compute scroll distance without taking a dependency on
    /// the render pipeline's layout `Rect`. `None` until the editor has
    /// been rendered at least once; handlers fall back to a default.
    pub(crate) viewport_rows: Option<u32>,
    /// Per-editor soft-wrap override, toggled by `ToggleWrap`. `Some` forces this
    /// editor's wrap mode regardless of the global `editor.wrap` setting; `None`
    /// follows the setting. Transient, not persisted.
    pub(crate) wrap_override: Option<WrapMode>,
    /// When `Some`, this editor is a review view; `render_editor` dispatches
    /// to the side-by-side renderer and flattened rows are read from the
    /// cache here. The cache is rebuilt by action handlers whenever the
    /// backing session's `version` advances past `review_view.session_version`.
    pub(crate) review_view: Option<ReviewViewState>,
    /// When set, `render_editor` paints this editor as a side-by-side diff: the
    /// right column is the normal syntax-highlighted buffer and the left column
    /// shows the base (HEAD) text via the buffer's diff map. Unlike
    /// [`Self::review_view`] there is no backing session -- the buffer stays the
    /// real editable buffer, so input handling is unchanged. Set through
    /// [`Self::set_diff_view`], which also flips the display map's
    /// deleted-block splicing.
    pub(crate) diff_view: bool,
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
    /// Cached gutter geometry (folded lines, digit width, diff marks, and rich
    /// component lines), keyed by the inputs that change the drawn gutter. Lets
    /// an unchanged repaint reuse the collections instead of rebuilding them
    /// every frame. Transient render state, not persisted.
    pub(crate) gutter_geometry_cache: Option<crate::render::editor::GutterGeometryCache>,
    /// Diagnostic spans resolved to byte offsets, keyed by the diagnostic-set
    /// and buffer versions, so the per-frame render paths reuse one resolution.
    /// Transient render state, not persisted.
    pub(crate) diagnostic_span_cache: Option<crate::render::editor::DiagnosticSpanCache>,
    /// Cells the last render reserved on the left for the diagnostic gutter (0
    /// or 1), so click-to-offset can subtract the same inset the text rect was
    /// shifted by. Transient render state written by `render_editor`, not
    /// persisted.
    pub(crate) gutter_width: u16,
    /// Screen rect of the right-edge minimap strip the last render reserved,
    /// so pointer handlers can map a click inside it back to a buffer line.
    ///
    /// `None` whenever the strip is absent (not under stoatty, minimap
    /// disabled, or the pane too narrow to inset). Transient render state
    /// written by `render_editor`, not persisted.
    pub(crate) minimap_rect: Option<Rect>,
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
        let mut selections = SelectionsCollection::new();
        selections.seed_cursor(&multi_buffer.snapshot());
        Self {
            buffer_id,
            mode: "normal".into(),
            display_map: DisplayMap::new(multi_buffer, executor),
            scroll_row: 0,
            scroll_offset: 0.0,
            scroll_glide: ScrollGlide::None,
            pool_current_line: None,
            viewport_rows: None,
            wrap_override: None,
            review_view: None,
            diff_view: false,
            selections,
            move_source_cursor: None,
            expansion_history: Vec::new(),
            expansion_tip: None,
            search_match_cache: None,
            highlight_endpoint_cache: None,
            hint_inlay_ids: Vec::new(),
            gutter_severity_cache: None,
            gutter_geometry_cache: None,
            diagnostic_span_cache: None,
            gutter_width: 0,
            minimap_rect: None,
            cursor_screen_cell: None,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn from_multi_buffer(
        buffer_id: BufferId,
        multi_buffer: MultiBuffer,
        executor: Executor,
    ) -> Self {
        let mut selections = SelectionsCollection::new();
        selections.seed_cursor(&multi_buffer.snapshot());
        Self {
            buffer_id,
            mode: "normal".into(),
            display_map: DisplayMap::new(multi_buffer, executor),
            scroll_row: 0,
            scroll_offset: 0.0,
            scroll_glide: ScrollGlide::None,
            pool_current_line: None,
            viewport_rows: None,
            wrap_override: None,
            review_view: None,
            diff_view: false,
            selections,
            move_source_cursor: None,
            expansion_history: Vec::new(),
            expansion_tip: None,
            search_match_cache: None,
            highlight_endpoint_cache: None,
            hint_inlay_ids: Vec::new(),
            gutter_severity_cache: None,
            gutter_geometry_cache: None,
            diagnostic_span_cache: None,
            gutter_width: 0,
            minimap_rect: None,
            cursor_screen_cell: None,
        }
    }

    /// Toggle the side-by-side diff view. Enabling it also turns on the display
    /// map's deleted-block splicing, so the base text of removed and modified
    /// hunks aligns as block rows in the left column. Disabling it turns the
    /// splicing back off.
    pub(crate) fn set_diff_view(&mut self, on: bool) {
        self.diff_view = on;
        self.display_map.set_show_deleted_blocks(on);
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

pub mod actions;
pub mod mouse;
pub mod regex_input_modal;
pub mod render;
pub mod scroll;
pub mod search;

use crate::{
    buffer::Buffer,
    diff_map::{DiffMap, DiffMapEvent},
    display_map::{DisplayMap, DisplayMapEvent},
    editor::scroll::autoscroll::{
        compute_autoscroll_y, AutoscrollStrategy, DEFAULT_VERTICAL_SCROLL_MARGIN,
    },
    globals::ExecutorGlobal,
    item::{DeserializeSnafu, ItemError, ItemView},
    multi_buffer::{MultiBuffer, MultiBufferEvent},
    settings::Settings,
    theme::{self, ActiveTheme, DEFAULT_EDITOR_FONT_FAMILY, DEFAULT_EDITOR_FONT_SIZE},
};
use gpui::{
    canvas, div, fill, font, outline, px, relative, size as gpui_size, uniform_list, App,
    AppContext, BorderStyle, Bounds, Context, DispatchPhase, Div, ElementInputHandler, Entity,
    EventEmitter, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Point, Render, ScrollWheelEvent, SharedString, Size,
    Styled, Subscription, Task, UniformListScrollHandle, WeakEntity, Window,
};
use lru::LruCache;
use serde_json::Value;
use std::{cell::RefCell, num::NonZeroUsize, ops::Range};
use stoat::{
    buffer::BufferId, jumplist::JumpList, multi_buffer::MultiBufferSnapshot,
    review_session::ChunkStatus, selection::SelectionsCollection, DisplayPoint,
};
use stoat_text::{
    next_word_start, prev_word_start, Anchor, Bias, OffsetUtf16, Selection, SelectionGoal,
};

/// Sizing / behavior classification carried on each [`Editor`].
/// The future render, scroll, and mouse paths consult these to
/// decide soft-wrap, gutter visibility, scroll axes, and reported
/// size without re-deriving the policy at every paint site.
///
/// - [`EditorMode::Full`] is the standard pane editor: soft-wrap on, gutter shown, both-axis
///   scroll, fills the container.
/// - [`EditorMode::SingleLine`] is a one-line text input used by picker queries, the file-finder
///   query, the command palette query, the in-buffer search input, the Claude chat input, the
///   rename input, and prompt inputs. No soft-wrap, no gutter, no vertical scroll, fixed line
///   height.
/// - [`EditorMode::AutoHeight`] grows with content from `min_lines` up to `max_lines` (or unbounded
///   when `None`). No gutter; no vertical scroll until the cap is hit.
/// - [`EditorMode::Minimap`] mirrors a parent editor at a reduced scale. No gutter, no independent
///   scroll (driven by the parent).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorMode {
    Full {},
    SingleLine,
    AutoHeight {
        min_lines: usize,
        max_lines: Option<usize>,
    },
    Minimap {
        parent: WeakEntity<Editor>,
    },
}

impl EditorMode {
    /// Convenience constructor for the standard pane editor mode.
    pub fn full() -> Self {
        Self::Full {}
    }

    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full { .. })
    }

    pub fn is_single_line(&self) -> bool {
        matches!(self, Self::SingleLine)
    }

    pub fn is_auto_height(&self) -> bool {
        matches!(self, Self::AutoHeight { .. })
    }

    pub fn is_minimap(&self) -> bool {
        matches!(self, Self::Minimap { .. })
    }

    /// Whether the render path soft-wraps long lines onto multiple
    /// visual rows. `Full` and `Minimap` wrap; the single-line input
    /// modes (`SingleLine`, `AutoHeight`) never wrap so cursor
    /// motion stays one-row-per-line.
    pub fn soft_wrap(&self) -> bool {
        matches!(self, Self::Full { .. } | Self::Minimap { .. })
    }

    /// Whether the render path paints the gutter (line numbers, diff
    /// strip, diagnostic markers). Only the standard pane editor
    /// shows the gutter.
    pub fn show_gutter(&self) -> bool {
        matches!(self, Self::Full { .. })
    }
}

/// Anchor and fixed metrics for an in-progress minimap thumb drag,
/// captured at mouse-down and held for the gesture. The parent's
/// [`ScrollManager::minimap_thumb_state`] separately records that a drag
/// is active; this carries the numbers the move handler maps the pointer
/// delta through into a parent scroll position.
#[derive(Clone, Copy)]
struct MinimapDrag {
    start_mouse_y: Pixels,
    start_scroll_y: f64,
    total_lines: f64,
    visible_lines: f64,
    minimap_height: f64,
}

/// Entity holding the state a single editor view needs:
/// [`Entity<MultiBuffer>`] for the source text, [`Entity<DisplayMap>`]
/// for the visible-line projection, [`Entity<DiffMap>`] for the
/// gutter-strip diff data, the user's selections and jumplist, the
/// current scroll row, and the [`EditorMode`] that classifies the
/// view (pane editor, single-line input, auto-height input,
/// minimap).
///
/// Render, mouse handling, action handlers, and `ItemView` registration
/// land in sibling items; this struct exposes only the state fields,
/// a subscription that re-emits child changes as
/// [`EditorEvent::Changed`], and the minimum mutation surface needed to
/// validate the event pipeline.
pub struct Editor {
    multi_buffer: Entity<MultiBuffer>,
    display_map: Entity<DisplayMap>,
    diff_map: Entity<DiffMap>,
    mode: EditorMode,
    selections: SelectionsCollection,
    scroll_row: u32,
    scroll_manager: scroll::ScrollManager,
    /// Accumulator for wheel events arriving within a single executor
    /// tick. The first event spawns a one-shot task that yields and
    /// then drains the accumulator; subsequent events in the same
    /// tick coalesce into the stored delta via
    /// [`gpui::ScrollDelta::coalesce`] so only the merged delta hits
    /// `apply_wheel`. Carries the originating modifiers alongside
    /// the delta because the alt key flips axis behavior.
    pending_scroll_delta: Option<(gpui::ScrollDelta, gpui::Modifiers)>,
    scroll_handle: UniformListScrollHandle,
    jumplist: JumpList,
    cell_size: Option<Size<Pixels>>,
    /// LRU cache of formatted line-number cells keyed by
    /// `(buffer_row, width)`. Lookups happen in
    /// [`render::build_gutter_prefix`] so a small scroll reuses the
    /// strings the prior frame allocated instead of reformatting.
    gutter_line_number_cache: RefCell<LruCache<(u32, usize), SharedString>>,
    /// LRU cache of formatted blame-strip cells keyed by
    /// `(buffer_row, now_hour_bucket)`. The `now_hour_bucket` is
    /// `now_seconds / 3600` so the relative-age string stays stable
    /// within an hour. Value is the pair of bytes and the color runs
    /// the strip paints inside the gutter prefix.
    #[allow(clippy::type_complexity)]
    gutter_blame_cache:
        RefCell<LruCache<(u32, i64), (SharedString, Vec<(Range<usize>, gpui::HighlightStyle)>)>>,
    file_path: Option<std::path::PathBuf>,
    diagnostic_set: Option<Entity<crate::diagnostics::DiagnosticSet>>,
    review_session: Option<Entity<crate::review_session::ReviewSession>>,
    review_file_index: Option<usize>,
    search_state: Option<search::SearchState>,
    /// Cached compiled regex for the active search query. Keyed by
    /// the query string so scrolling against a stable query reuses
    /// the prior compilation instead of recompiling per frame.
    /// Populated lazily by [`Self::compiled_search_regex`].
    cached_search_regex: Option<(String, regex::Regex)>,
    workspace: Option<WeakEntity<crate::workspace::Workspace>>,
    text_region_bounds: Option<Bounds<Pixels>>,
    hover_position: Option<(u32, u32)>,
    hover_debounce_task: Option<Task<()>>,
    /// Monotonic id for the most recent hover-debounce spawn. The
    /// spawn captures the id before the 50ms timer; the timer body
    /// re-checks it so a debounce queued for an older cursor cell
    /// cannot dispatch `HoverAt` after the cursor has moved on.
    hover_debounce_seq: u64,
    hover_popup: Option<Entity<crate::lsp::HoverPopup>>,
    completion_popup: Option<Entity<crate::lsp::CompletionPopup>>,
    inlay_hints_manager: Option<Entity<crate::lsp::InlayHintsManager>>,
    semantic_tokens_manager: Option<Entity<crate::lsp::SemanticTokensManager>>,
    syntax_map_updater: Option<Entity<crate::syntax_updater::SyntaxMapUpdater>>,
    pending_goto_word_labels: Option<std::collections::BTreeMap<String, usize>>,
    pending_goto_word_input: String,
    expansion_history: Vec<Range<usize>>,
    expansion_tip: Option<Range<usize>>,
    blame_state: Option<Entity<crate::git::blame::BlameState>>,
    blame_visible: bool,
    minimap_visible: bool,
    minimap: Option<Entity<Editor>>,
    minimap_drag: Option<MinimapDrag>,
    scroll_animation_task: Option<Task<()>>,
    _subscriptions: [Subscription; 3],
    _diagnostic_subscription: Option<Subscription>,
    _review_session_subscription: Option<Subscription>,
    _blame_subscription: Option<Subscription>,
}

/// Single coalesced "editor changed" signal. Subscribers re-render on
/// any event; finer-grained variants are added when a consumer needs
/// to discriminate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorEvent {
    Changed,
}

impl EventEmitter<EditorEvent> for Editor {}

/// Side of each selection that
/// [`Editor::paste_at_selections`] inserts at.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PastePosition {
    /// Insert at every selection's start offset.
    Before,
    /// Insert at every selection's end offset.
    After,
}

/// Direction the cursor widens by one UTF-8 character before
/// [`Editor::delete_around_cursors`] deletes the covered range.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DeleteDirection {
    /// Cover the character at or immediately after the cursor.
    Forward,
    /// Cover the character immediately before the cursor.
    Backward,
}

/// Direction [`Editor::add_selection_step`] walks the display
/// map when fanning a new cursor off the primary selection.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AddDirection {
    /// Walk toward row 0; primary cursor sits one row below.
    Above,
    /// Walk toward the last display row; primary cursor sits
    /// one row above.
    Below,
}

/// Direction [`Editor::open_line`] inserts a blank line relative
/// to each selection's head row.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OpenLineDir {
    /// Insert at the row's start, so the new blank line sits
    /// above the original row.
    Above,
    /// Insert at the row's end, so the new blank line sits
    /// below the original row.
    Below,
}

/// Step one UTF-8 character forward in `rope` starting at byte
/// `offset`. Returns `offset` unchanged when at or past end-of-rope.
fn step_char_forward(rope: &stoat_text::Rope, offset: usize) -> usize {
    let len = rope.len();
    if offset >= len {
        return offset;
    }
    let mut probe = offset.saturating_add(1).min(len);
    while probe < len && !rope.is_char_boundary(probe) {
        probe += 1;
    }
    probe
}

/// Per-selection alignment-input row/column metadata, populated
/// in selection order. `align_selections` ranks these by their
/// row-occurrence index before computing alignment padding.
struct AlignEntry {
    insert_offset: usize,
    head_col: u32,
    head_row: u32,
}

/// `AlignEntry` plus its rank (nth selection on the row) and
/// stable row index. Decoupling the two passes keeps the
/// max-column-per-rank scan straightforward.
struct RankedEntry {
    insert_offset: usize,
    head_col: u32,
    row_idx: usize,
    rank: usize,
}

/// Trim leading and trailing whitespace from `[start, end)`,
/// returning the narrowed range or `None` when the range is
/// empty or all whitespace. Used by [`Editor::trim_selections`].
fn trim_whitespace_in_rope(
    rope: &stoat_text::Rope,
    start: usize,
    end: usize,
) -> Option<(usize, usize)> {
    if start >= end {
        return None;
    }
    let mut new_start: Option<usize> = None;
    let mut last_non_ws_end: Option<usize> = None;
    let mut cursor = start;
    for ch in rope.chars_at(start) {
        if cursor >= end {
            break;
        }
        let next_cursor = cursor + ch.len_utf8();
        if !ch.is_whitespace() {
            new_start.get_or_insert(cursor);
            last_non_ws_end = Some(next_cursor);
        }
        cursor = next_cursor;
    }
    Some((new_start?, last_non_ws_end?))
}

/// Step one UTF-8 character backward in `rope` starting at byte
/// `offset`. Returns `offset` unchanged when at start-of-rope.
fn step_char_backward(rope: &stoat_text::Rope, offset: usize) -> usize {
    if offset == 0 {
        return 0;
    }
    let mut probe = offset - 1;
    while probe > 0 && !rope.is_char_boundary(probe) {
        probe -= 1;
    }
    probe
}

/// Sorted, deduplicated row indices touched by any selection in
/// `selections`. A selection ending exactly at column 0 of a row
/// (with non-zero extent) excludes that row, matching the
/// convention shared by indent / unindent / line-comment
/// operations.
fn touched_rows(snapshot: &MultiBufferSnapshot, selections: &SelectionsCollection) -> Vec<u32> {
    let rope = snapshot.rope();
    let mut rows: Vec<u32> = Vec::new();
    for sel in selections.all_anchors() {
        let start_offset = snapshot.resolve_anchor(&sel.start);
        let end_offset = snapshot.resolve_anchor(&sel.end);
        let (lo, hi) = if start_offset <= end_offset {
            (start_offset, end_offset)
        } else {
            (end_offset, start_offset)
        };
        let start_row = rope.offset_to_point(lo).row;
        let end_point = rope.offset_to_point(hi);
        let end_row = if hi > lo && end_point.column == 0 {
            end_point.row.saturating_sub(1)
        } else {
            end_point.row
        };
        for row in start_row..=end_row {
            rows.push(row);
        }
    }
    rows.sort_unstable();
    rows.dedup();
    rows
}

#[derive(Default)]
struct ReviewRenderData {
    chunk_markers: Vec<(u32, ChunkStatus)>,
    provenances: Vec<(u32, stoat::review::MoveProvenance)>,
    moved_spans: Vec<(u32, Range<usize>)>,
}

impl Editor {
    pub fn new(
        multi_buffer: Entity<MultiBuffer>,
        display_map: Entity<DisplayMap>,
        diff_map: Entity<DiffMap>,
        mode: EditorMode,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let mb_sub = cx.subscribe(&multi_buffer, |_, _, _event: &MultiBufferEvent, cx| {
            cx.emit(EditorEvent::Changed);
            cx.notify();
        });
        let dm_sub = cx.subscribe(&display_map, |_, _, _event: &DisplayMapEvent, cx| {
            cx.emit(EditorEvent::Changed);
            cx.notify();
        });
        let diff_sub = cx.subscribe(&diff_map, |_, _, _event: &DiffMapEvent, cx| {
            cx.emit(EditorEvent::Changed);
            cx.notify();
        });
        Self {
            multi_buffer,
            display_map,
            diff_map,
            mode,
            selections: SelectionsCollection::new(),
            scroll_row: 0,
            scroll_manager: scroll::ScrollManager::new(std::time::Instant::now()),
            pending_scroll_delta: None,
            scroll_handle: UniformListScrollHandle::new(),
            jumplist: JumpList::new(),
            cell_size: None,
            gutter_line_number_cache: RefCell::new(LruCache::new(
                NonZeroUsize::new(1024).expect("nonzero"),
            )),
            gutter_blame_cache: RefCell::new(LruCache::new(
                NonZeroUsize::new(1024).expect("nonzero"),
            )),
            file_path: None,
            diagnostic_set: None,
            review_session: None,
            review_file_index: None,
            search_state: None,
            cached_search_regex: None,
            workspace: None,
            text_region_bounds: None,
            hover_position: None,
            hover_debounce_task: None,
            hover_debounce_seq: 0,
            hover_popup: None,
            completion_popup: None,
            inlay_hints_manager: None,
            semantic_tokens_manager: None,
            syntax_map_updater: None,
            pending_goto_word_labels: None,
            pending_goto_word_input: String::new(),
            expansion_history: Vec::new(),
            expansion_tip: None,
            blame_state: None,
            blame_visible: false,
            minimap_visible: false,
            minimap: None,
            minimap_drag: None,
            scroll_animation_task: None,
            _subscriptions: [mb_sub, dm_sub, diff_sub],
            _diagnostic_subscription: None,
            _review_session_subscription: None,
            _blame_subscription: None,
        }
    }

    /// Convenience constructor for an empty-buffer editor in
    /// [`EditorMode::SingleLine`].
    pub fn single_line(window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        Self::new_inline(EditorMode::SingleLine, window, cx)
    }

    /// Convenience constructor for an empty-buffer editor in
    /// [`EditorMode::AutoHeight`] capped at `max_lines`.
    pub fn auto_height(
        min_lines: usize,
        max_lines: usize,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        Self::new_inline(
            EditorMode::AutoHeight {
                min_lines,
                max_lines: Some(max_lines),
            },
            window,
            cx,
        )
    }

    /// Convenience constructor for an empty-buffer editor in
    /// [`EditorMode::AutoHeight`] with no upper bound.
    pub fn auto_height_unbounded(
        min_lines: usize,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        Self::new_inline(
            EditorMode::AutoHeight {
                min_lines,
                max_lines: None,
            },
            window,
            cx,
        )
    }

    fn new_inline(mode: EditorMode, _window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let buffer = cx.new(|_| Buffer::with_text(BufferId::new(0), ""));
        let multi_buffer = cx.new({
            let buffer = buffer.clone();
            |cx| MultiBuffer::singleton(buffer, cx)
        });
        let executor = cx.global::<ExecutorGlobal>().0.clone();
        let display_map = cx.new({
            let buffer = buffer.clone();
            |cx| DisplayMap::new(buffer, executor, cx)
        });
        let diff_map = cx.new(|cx| DiffMap::new(buffer, cx));
        Self::new(multi_buffer, display_map, diff_map, mode, cx)
    }

    pub fn mode(&self) -> &EditorMode {
        &self.mode
    }

    pub fn multi_buffer(&self) -> &Entity<MultiBuffer> {
        &self.multi_buffer
    }

    pub fn display_map(&self) -> &Entity<DisplayMap> {
        &self.display_map
    }

    pub fn diff_map(&self) -> &Entity<DiffMap> {
        &self.diff_map
    }

    pub fn selections(&self) -> &SelectionsCollection {
        &self.selections
    }

    pub fn selections_mut(&mut self) -> &mut SelectionsCollection {
        &mut self.selections
    }

    pub fn scroll_row(&self) -> u32 {
        self.scroll_row
    }

    pub fn scroll_manager(&self) -> &scroll::ScrollManager {
        &self.scroll_manager
    }

    pub fn scroll_manager_mut(&mut self) -> &mut scroll::ScrollManager {
        &mut self.scroll_manager
    }

    pub fn scroll_handle(&self) -> &UniformListScrollHandle {
        &self.scroll_handle
    }

    pub fn jumplist(&self) -> &JumpList {
        &self.jumplist
    }

    pub fn set_scroll_row(&mut self, row: u32, cx: &mut Context<'_, Self>) {
        if self.scroll_row == row {
            return;
        }
        self.scroll_row = row;
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Request that the next layout pass scroll the viewport according
    /// to `strategy`. The request is stored on the scroll manager and
    /// consumed once by [`Editor::apply_pending_autoscroll`]; subsequent
    /// frames do not re-apply it.
    pub fn request_autoscroll(&mut self, strategy: AutoscrollStrategy, cx: &mut Context<'_, Self>) {
        self.scroll_manager.set_autoscroll_request(Some(strategy));
        cx.notify();
    }

    /// Insert `text` at every selection in this editor. Range
    /// selections are replaced by `text`; empty selections (cursors)
    /// have `text` inserted at their position. After all edits each
    /// selection collapses to a single cursor immediately after the
    /// inserted text in the post-edit buffer.
    ///
    /// Edits are applied in reverse-offset order so an earlier
    /// edit's range is still valid after later edits have committed.
    /// Each cursor's post-edit offset accounts for cumulative shifts
    /// from edits at earlier offsets: for cursor `i` (ascending
    /// offset order) the new offset is
    /// `pre_start_i + text.len() + sum_{j<i}(text.len() - (pre_end_j - pre_start_j))`.
    /// Multi-excerpt buffers are skipped with a `tracing::warn` --
    /// the multi-buffer edit surface is not yet built.
    pub fn apply_text_to_all_cursors(&mut self, text: &str, cx: &mut Context<'_, Self>) {
        let buffer = match self.multi_buffer.read(cx).as_singleton() {
            Some(b) => b.clone(),
            None => {
                tracing::warn!(
                    target: "stoat::editor",
                    "apply_text_to_all_cursors on multi-excerpt buffer is not yet supported",
                );
                return;
            },
        };

        let mut ascending: Vec<(usize, Range<usize>)> = {
            let snapshot = self.multi_buffer.read(cx).snapshot();
            self.selections
                .all_anchors()
                .iter()
                .map(|sel| {
                    let start = snapshot.resolve_anchor(&sel.start);
                    let end = snapshot.resolve_anchor(&sel.end);
                    let (lo, hi) = if start <= end {
                        (start, end)
                    } else {
                        (end, start)
                    };
                    (sel.id, lo..hi)
                })
                .collect()
        };
        ascending.sort_by_key(|(_, range)| range.start);

        let text_len = text.len();
        let mut cumulative_shift: isize = 0;
        let mut post_offsets: Vec<(usize, usize)> = Vec::with_capacity(ascending.len());
        for (id, range) in &ascending {
            let post = (range.start as isize + cumulative_shift) as usize + text_len;
            post_offsets.push((*id, post));
            cumulative_shift += text_len as isize - (range.end - range.start) as isize;
        }

        for (_id, range) in ascending.iter().rev() {
            buffer.update(cx, |b, cx| b.edit(range.clone(), text, cx));
        }

        let new_snapshot = self.multi_buffer.read(cx).snapshot();
        let mut new_disjoint: Vec<Selection<Anchor>> = post_offsets
            .into_iter()
            .map(|(id, post)| {
                let anchor = new_snapshot.anchor_at(post, Bias::Left);
                Selection {
                    id,
                    start: anchor,
                    end: anchor,
                    reversed: false,
                    goal: SelectionGoal::None,
                }
            })
            .collect();
        new_disjoint.sort_by_key(|s| s.id);

        self.selections.replace_with(new_disjoint, &new_snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Apply `transform` to the text of every non-empty selection
    /// and replace each selection's range with the transformed
    /// text. Collapsed cursors and selections whose transform is
    /// a no-op are skipped. After the edit, each affected
    /// selection spans the new range. Multi-excerpt buffers are
    /// logged and skipped, matching
    /// [`Self::apply_text_to_all_cursors`].
    pub fn transform_selections_text<F>(&mut self, transform: F, cx: &mut Context<'_, Self>)
    where
        F: Fn(&str) -> String,
    {
        let buffer = match self.multi_buffer.read(cx).as_singleton() {
            Some(b) => b.clone(),
            None => {
                tracing::warn!(
                    target: "stoat::editor",
                    "transform_selections_text on multi-excerpt buffer is not yet supported",
                );
                return;
            },
        };

        let mut edits: Vec<(usize, usize, usize, String)> = {
            let snapshot = self.multi_buffer.read(cx).snapshot();
            let rope = snapshot.rope();
            self.selections
                .all_anchors()
                .iter()
                .filter_map(|sel| {
                    let s = snapshot.resolve_anchor(&sel.start);
                    let e = snapshot.resolve_anchor(&sel.end);
                    let (lo, hi) = if s <= e { (s, e) } else { (e, s) };
                    if lo == hi {
                        return None;
                    }
                    let text = rope.slice(lo..hi).to_string();
                    let new_text = transform(&text);
                    if new_text == text {
                        return None;
                    }
                    Some((sel.id, lo, hi, new_text))
                })
                .collect()
        };

        if edits.is_empty() {
            return;
        }

        edits.sort_by_key(|(_, s, _, _)| *s);

        for (_, s, e, text) in edits.iter().rev() {
            buffer.update(cx, |b, cx| b.edit(*s..*e, text.as_str(), cx));
        }

        let mut id_to_post: std::collections::HashMap<usize, (usize, usize)> =
            std::collections::HashMap::with_capacity(edits.len());
        let mut shift: isize = 0;
        for (id, s, e, text) in edits.iter() {
            let post_start = (*s as isize + shift) as usize;
            let post_end = post_start + text.len();
            id_to_post.insert(*id, (post_start, post_end));
            shift += text.len() as isize - (*e as isize - *s as isize);
        }

        let new_snapshot = self.multi_buffer.read(cx).snapshot();
        let mut new_selections: Vec<Selection<Anchor>> = self
            .selections
            .all_anchors()
            .iter()
            .map(|sel| match id_to_post.get(&sel.id) {
                Some(&(post_start, post_end)) => Selection {
                    id: sel.id,
                    start: new_snapshot.anchor_at(post_start, Bias::Left),
                    end: new_snapshot.anchor_at(post_end, Bias::Right),
                    reversed: sel.reversed,
                    goal: SelectionGoal::None,
                },
                None => sel.clone(),
            })
            .collect();
        new_selections.sort_by_key(|s| s.id);

        self.selections.replace_with(new_selections, &new_snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Replace every character of each non-empty selection with
    /// `ch`. Selections that are collapsed cursors (`start ==
    /// end`) are skipped. The replacement preserves char count
    /// per selection -- a 3-char selection becomes 3 copies of
    /// `ch` regardless of UTF-8 byte width -- so the
    /// post-replacement byte length may grow or shrink. After
    /// the edit, each affected selection spans the newly written
    /// range. Multi-excerpt buffers are logged and skipped,
    /// matching [`Self::apply_text_to_all_cursors`].
    pub fn replace_char_in_selections(&mut self, ch: char, cx: &mut Context<'_, Self>) {
        let buffer = match self.multi_buffer.read(cx).as_singleton() {
            Some(b) => b.clone(),
            None => {
                tracing::warn!(
                    target: "stoat::editor",
                    "replace_char_in_selections on multi-excerpt buffer is not yet supported",
                );
                return;
            },
        };

        let mut entries: Vec<(usize, usize, usize, String)> = {
            let snapshot = self.multi_buffer.read(cx).snapshot();
            let rope = snapshot.rope();
            self.selections
                .all_anchors()
                .iter()
                .filter_map(|sel| {
                    let s = snapshot.resolve_anchor(&sel.start);
                    let e = snapshot.resolve_anchor(&sel.end);
                    let (lo, hi) = if s <= e { (s, e) } else { (e, s) };
                    if lo == hi {
                        return None;
                    }
                    let mut char_count = 0usize;
                    let mut byte_pos = lo;
                    for c in rope.chars_at(lo) {
                        if byte_pos >= hi {
                            break;
                        }
                        byte_pos += c.len_utf8();
                        char_count += 1;
                    }
                    let mut replacement = String::with_capacity(char_count * ch.len_utf8());
                    for _ in 0..char_count {
                        replacement.push(ch);
                    }
                    Some((sel.id, lo, hi, replacement))
                })
                .collect()
        };

        if entries.is_empty() {
            return;
        }

        entries.sort_by_key(|(_, s, _, _)| *s);

        for (_, s, e, text) in entries.iter().rev() {
            buffer.update(cx, |b, cx| b.edit(*s..*e, text.as_str(), cx));
        }

        let mut id_to_post: std::collections::HashMap<usize, (usize, usize)> =
            std::collections::HashMap::with_capacity(entries.len());
        let mut shift: isize = 0;
        for (id, s, e, text) in entries.iter() {
            let post_start = (*s as isize + shift) as usize;
            let post_end = post_start + text.len();
            id_to_post.insert(*id, (post_start, post_end));
            shift += text.len() as isize - (*e as isize - *s as isize);
        }

        let new_snapshot = self.multi_buffer.read(cx).snapshot();
        let mut new_selections: Vec<Selection<Anchor>> = self
            .selections
            .all_anchors()
            .iter()
            .map(|sel| match id_to_post.get(&sel.id) {
                Some(&(post_start, post_end)) => Selection {
                    id: sel.id,
                    start: new_snapshot.anchor_at(post_start, Bias::Left),
                    end: new_snapshot.anchor_at(post_end, Bias::Right),
                    reversed: sel.reversed,
                    goal: SelectionGoal::None,
                },
                None => sel.clone(),
            })
            .collect();
        new_selections.sort_by_key(|s| s.id);

        self.selections.replace_with(new_selections, &new_snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Insert a blank line above or below each selection's head
    /// row and collapse every selection to a cursor on that new
    /// blank line. Cursors that share a row collapse to a single
    /// insert: only one blank line is opened per row, and all of
    /// the row's cursors land at the same offset on it.
    /// Multi-excerpt buffers are skipped with a `tracing::warn`,
    /// matching [`Self::apply_text_to_all_cursors`].
    pub fn open_line(&mut self, dir: OpenLineDir, cx: &mut Context<'_, Self>) {
        let buffer = match self.multi_buffer.read(cx).as_singleton() {
            Some(b) => b.clone(),
            None => {
                tracing::warn!(
                    target: "stoat::editor",
                    "open_line on multi-excerpt buffer is not yet supported",
                );
                return;
            },
        };

        let (selection_rows, row_to_pre_offset) = {
            let snapshot = self.multi_buffer.read(cx).snapshot();
            let rope = snapshot.rope();
            let mut selection_rows: Vec<(usize, u32)> = Vec::new();
            let mut row_to_pre_offset: std::collections::HashMap<u32, usize> =
                std::collections::HashMap::new();
            for sel in self.selections.all_anchors().iter() {
                let row = snapshot.point_for_anchor(&sel.head()).row;
                selection_rows.push((sel.id, row));
                row_to_pre_offset.entry(row).or_insert_with(|| match dir {
                    OpenLineDir::Above => rope.point_to_offset(stoat_text::Point::new(row, 0)),
                    OpenLineDir::Below => {
                        rope.point_to_offset(stoat_text::Point::new(row, rope.line_len(row)))
                    },
                });
            }
            (selection_rows, row_to_pre_offset)
        };

        if row_to_pre_offset.is_empty() {
            return;
        }

        let mut sorted_offsets: Vec<usize> = row_to_pre_offset.values().copied().collect();
        sorted_offsets.sort_unstable();

        for offset in sorted_offsets.iter().rev() {
            buffer.update(cx, |b, cx| b.edit(*offset..*offset, "\n", cx));
        }

        let bias = match dir {
            OpenLineDir::Above => Bias::Left,
            OpenLineDir::Below => Bias::Right,
        };
        let cursor_delta: usize = match dir {
            OpenLineDir::Above => 0,
            OpenLineDir::Below => 1,
        };

        let new_snapshot = self.multi_buffer.read(cx).snapshot();
        let mut new_selections: Vec<Selection<Anchor>> = selection_rows
            .into_iter()
            .map(|(id, row)| {
                let pre_offset = row_to_pre_offset[&row];
                let earlier_inserts = sorted_offsets.iter().filter(|o| **o < pre_offset).count();
                let cursor_offset = pre_offset + earlier_inserts + cursor_delta;
                let anchor = new_snapshot.anchor_at(cursor_offset, bias);
                Selection {
                    id,
                    start: anchor,
                    end: anchor,
                    reversed: false,
                    goal: SelectionGoal::None,
                }
            })
            .collect();
        new_selections.sort_by_key(|s| s.id);

        self.selections.replace_with(new_selections, &new_snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Delete the contents of every non-empty selection and collapse
    /// each affected selection to a cursor at the deletion's start.
    /// Empty selections are left untouched. Multi-excerpt buffers
    /// are logged and skipped, matching
    /// [`Self::apply_text_to_all_cursors`].
    pub fn delete_selections(&mut self, cx: &mut Context<'_, Self>) {
        let buffer = match self.multi_buffer.read(cx).as_singleton() {
            Some(b) => b.clone(),
            None => {
                tracing::warn!(
                    target: "stoat::editor",
                    "delete_selections on multi-excerpt buffer is not yet supported",
                );
                return;
            },
        };
        let mut ascending: Vec<(usize, Range<usize>)> = {
            let snapshot = self.multi_buffer.read(cx).snapshot();
            self.selections
                .all_anchors()
                .iter()
                .filter_map(|sel| {
                    let start = snapshot.resolve_anchor(&sel.start);
                    let end = snapshot.resolve_anchor(&sel.end);
                    let (lo, hi) = if start <= end {
                        (start, end)
                    } else {
                        (end, start)
                    };
                    (lo != hi).then_some((sel.id, lo..hi))
                })
                .collect()
        };
        if ascending.is_empty() {
            return;
        }
        ascending.sort_by_key(|(_, range)| range.start);

        let mut cumulative_shift: isize = 0;
        let mut post_offsets: Vec<(usize, usize)> = Vec::with_capacity(ascending.len());
        for (id, range) in &ascending {
            let post = (range.start as isize + cumulative_shift) as usize;
            post_offsets.push((*id, post));
            cumulative_shift -= (range.end - range.start) as isize;
        }

        for (_id, range) in ascending.iter().rev() {
            buffer.update(cx, |b, cx| b.edit(range.clone(), "", cx));
        }

        let new_snapshot = self.multi_buffer.read(cx).snapshot();
        let deleted: std::collections::HashSet<usize> =
            post_offsets.iter().map(|(id, _)| *id).collect();
        let post_map: std::collections::HashMap<usize, usize> = post_offsets.into_iter().collect();
        self.selections.transform(&new_snapshot, |sel| {
            if !deleted.contains(&sel.id) {
                return sel.clone();
            }
            let offset = post_map[&sel.id];
            let anchor = new_snapshot.anchor_at(offset, Bias::Left);
            Selection {
                id: sel.id,
                start: anchor,
                end: anchor,
                reversed: false,
                goal: SelectionGoal::None,
            }
        });
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Widen each empty (cursor-only) selection by one UTF-8
    /// character in `direction`, then delete the covered range and
    /// collapse to a cursor at the resulting position. Selections
    /// that already span text delegate to [`Self::delete_selections`].
    /// No-op when the cursor sits at the boundary (start of buffer
    /// for `Backward`, end of buffer for `Forward`).
    pub fn delete_around_cursors(
        &mut self,
        direction: DeleteDirection,
        cx: &mut Context<'_, Self>,
    ) {
        self.delete_widened_around_cursors(cx, |rope, head| match direction {
            DeleteDirection::Forward => (head, step_char_forward(rope, head)),
            DeleteDirection::Backward => (step_char_backward(rope, head), head),
        });
    }

    /// Widen each empty (cursor-only) selection from the cursor to the
    /// next/previous word boundary in `direction`, delete the covered
    /// range, and collapse to a cursor. Selections that already span
    /// text delegate to [`Self::delete_selections`]. No-op when the
    /// cursor already sits at the boundary.
    pub fn delete_word_around_cursors(
        &mut self,
        direction: DeleteDirection,
        cx: &mut Context<'_, Self>,
    ) {
        self.delete_widened_around_cursors(cx, |rope, head| match direction {
            DeleteDirection::Forward => (head, next_word_start(rope, head)),
            DeleteDirection::Backward => (prev_word_start(rope, head), head),
        });
    }

    /// Shared core for the cursor-relative deletes. For each empty
    /// selection, `widen` maps the cursor offset to the `(lo, hi)` byte
    /// range to remove; the ranges are deleted and each selection
    /// collapses to a cursor at the resulting position. Any non-empty
    /// selection short-circuits to [`Self::delete_selections`].
    /// Restricted to singleton buffers.
    fn delete_widened_around_cursors(
        &mut self,
        cx: &mut Context<'_, Self>,
        widen: impl Fn(&stoat_text::Rope, usize) -> (usize, usize),
    ) {
        let has_nonempty = {
            let snapshot = self.multi_buffer.read(cx).snapshot();
            self.selections
                .all_anchors()
                .iter()
                .any(|sel| snapshot.resolve_anchor(&sel.start) != snapshot.resolve_anchor(&sel.end))
        };
        if has_nonempty {
            self.delete_selections(cx);
            return;
        }
        if self.multi_buffer.read(cx).as_singleton().is_none() {
            tracing::warn!(
                target: "stoat::editor",
                "delete_around_cursors on multi-excerpt buffer is not yet supported",
            );
            return;
        }

        let widened: Vec<Selection<Anchor>> = {
            let snapshot = self.multi_buffer.read(cx).snapshot();
            let rope = snapshot.rope();
            self.selections
                .all_anchors()
                .iter()
                .map(|sel| {
                    let head = snapshot.resolve_anchor(&sel.start);
                    let (lo, hi) = widen(rope, head);
                    let start = snapshot.anchor_at(lo, Bias::Right);
                    let end = snapshot.anchor_at(hi, Bias::Left);
                    Selection {
                        id: sel.id,
                        start,
                        end,
                        reversed: false,
                        goal: SelectionGoal::None,
                    }
                })
                .collect()
        };
        let snapshot = self.multi_buffer.read(cx).snapshot();
        self.selections.replace_with(widened, &snapshot);
        self.delete_selections(cx);
    }

    /// Build the join-by-newline yank payload from each selection's
    /// covered text. Returns `None` when every selection is empty,
    /// matching the TUI's yank no-op-on-empty contract; otherwise
    /// the returned string contains the per-selection slices joined
    /// by `"\n"` in selection order (`SelectionsCollection::all_anchors`).
    pub fn yank_payload(&self, cx: &App) -> Option<String> {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let rope = snapshot.rope();
        let mut pieces: Vec<String> = Vec::new();
        let mut any_nonempty = false;
        for sel in self.selections.all_anchors() {
            let start = snapshot.resolve_anchor(&sel.start);
            let end = snapshot.resolve_anchor(&sel.end);
            let (lo, hi) = if start <= end {
                (start, end)
            } else {
                (end, start)
            };
            if lo != hi {
                any_nonempty = true;
            }
            pieces.push(rope.slice(lo..hi).to_string());
        }
        if !any_nonempty {
            return None;
        }
        Some(pieces.join("\n"))
    }

    /// Insert `text` at a per-selection anchor side and collapse
    /// each selection to a cursor at the end of the inserted text.
    /// `position` selects whether the insert lands at every
    /// selection's start (before) or end (after) offset.
    pub fn paste_at_selections(
        &mut self,
        text: &str,
        position: PastePosition,
        cx: &mut Context<'_, Self>,
    ) {
        if text.is_empty() {
            return;
        }
        let buffer = match self.multi_buffer.read(cx).as_singleton() {
            Some(b) => b.clone(),
            None => {
                tracing::warn!(
                    target: "stoat::editor",
                    "paste_at_selections on multi-excerpt buffer is not yet supported",
                );
                return;
            },
        };
        let mut ascending: Vec<(usize, usize)> = {
            let snapshot = self.multi_buffer.read(cx).snapshot();
            self.selections
                .all_anchors()
                .iter()
                .map(|sel| {
                    let start = snapshot.resolve_anchor(&sel.start);
                    let end = snapshot.resolve_anchor(&sel.end);
                    let (lo, hi) = if start <= end {
                        (start, end)
                    } else {
                        (end, start)
                    };
                    let insertion = match position {
                        PastePosition::Before => lo,
                        PastePosition::After => hi,
                    };
                    (sel.id, insertion)
                })
                .collect()
        };
        ascending.sort_by_key(|(_, offset)| *offset);

        let text_len = text.len();
        let mut cumulative_shift: isize = 0;
        let mut post_offsets: Vec<(usize, usize)> = Vec::with_capacity(ascending.len());
        for (id, offset) in &ascending {
            let post = (*offset as isize + cumulative_shift) as usize + text_len;
            post_offsets.push((*id, post));
            cumulative_shift += text_len as isize;
        }

        for (_id, offset) in ascending.iter().rev() {
            buffer.update(cx, |b, cx| b.edit(*offset..*offset, text, cx));
        }

        let new_snapshot = self.multi_buffer.read(cx).snapshot();
        let mut new_disjoint: Vec<Selection<Anchor>> = post_offsets
            .into_iter()
            .map(|(id, post)| {
                let anchor = new_snapshot.anchor_at(post, Bias::Left);
                Selection {
                    id,
                    start: anchor,
                    end: anchor,
                    reversed: false,
                    goal: SelectionGoal::None,
                }
            })
            .collect();
        new_disjoint.sort_by_key(|s| s.id);
        self.selections.replace_with(new_disjoint, &new_snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Collapse each selection to a cursor at its start anchor.
    /// Used by the `Insert` action to enter insert mode at the
    /// left side of every selection.
    pub fn collapse_selections_to_start(&mut self, cx: &mut Context<'_, Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        self.selections.transform(&snapshot, |sel| {
            let anchor = sel.start;
            Selection {
                id: sel.id,
                start: anchor,
                end: anchor,
                reversed: false,
                goal: SelectionGoal::None,
            }
        });
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Collapse each selection to a cursor at its end anchor. Used
    /// by the `Append` action to enter insert mode at the right
    /// side of every selection.
    pub fn collapse_selections_to_end(&mut self, cx: &mut Context<'_, Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        self.selections.transform(&snapshot, |sel| {
            let anchor = sel.end;
            Selection {
                id: sel.id,
                start: anchor,
                end: anchor,
                reversed: false,
                goal: SelectionGoal::None,
            }
        });
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Collapse every selection to a cursor at its head anchor
    /// (start if reversed, end otherwise). Mirrors the helix-style
    /// `CollapseSelection` action.
    /// Whether the primary selection's head sits on its line
    /// such that everything from the start of the line to the
    /// head is whitespace (including the empty case when the
    /// head is at column 0). Used by SmartTab's indent branch
    /// to decide between inserting `\t` and falling through.
    pub fn cursor_after_only_whitespace(&self, cx: &App) -> bool {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let rope = snapshot.rope();
        let primary = self.selections.newest_anchor();
        let head_offset = snapshot.resolve_anchor(&primary.head());
        let head_point = rope.offset_to_point(head_offset);
        let line_start = rope.point_to_offset(stoat_text::Point::new(head_point.row, 0));
        if line_start >= head_offset {
            return true;
        }
        rope.slice(line_start..head_offset)
            .chars()
            .all(char::is_whitespace)
    }

    pub fn collapse_selection(&mut self, cx: &mut Context<'_, Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        self.selections.transform(&snapshot, |sel| {
            let mut new = sel.clone();
            new.collapse_to(sel.head(), sel.goal);
            new
        });
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Toggle the `reversed` flag on every non-empty selection,
    /// swapping which end is the head. Empty selections are left
    /// untouched.
    pub fn flip_selections(&mut self, cx: &mut Context<'_, Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        self.selections.transform(&snapshot, |sel| {
            let mut new = sel.clone();
            if !new.is_empty() {
                new.reversed = !new.reversed;
            }
            new
        });
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Replace every selection with a single selection spanning
    /// the entire buffer.
    pub fn select_all(&mut self, cx: &mut Context<'_, Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let end_offset = snapshot.rope().len();
        let start_anchor = snapshot.anchor_at(0, Bias::Left);
        let end_anchor = snapshot.anchor_at(end_offset, Bias::Right);
        self.selections
            .set_single_range(start_anchor, end_anchor, SelectionGoal::None);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Extend every selection to cover full lines, then add `count`
    /// additional lines below each. Selections that already cover
    /// whole lines extend by `count` rows; partial selections
    /// snap to full-line shape and extend by `count - 1` so a
    /// single invocation always covers exactly `count` more
    /// lines of text.
    pub fn select_line_below(&mut self, count: u32, cx: &mut Context<'_, Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let rope = snapshot.rope().clone();
        let max_row = rope.max_point().row;
        let rope_len = rope.len();
        let count = count.max(1);
        self.selections.transform(&snapshot, |sel| {
            let line_start = |row: u32| -> usize {
                if row > max_row {
                    rope_len
                } else {
                    rope.point_to_offset(stoat_text::Point::new(row, 0))
                }
            };
            let start_offset = snapshot.resolve_anchor(&sel.start);
            let end_offset = snapshot.resolve_anchor(&sel.end);
            let start_row = rope.offset_to_point(start_offset).row;
            let end_point = rope.offset_to_point(end_offset);
            let end_row = if end_offset > start_offset && end_point.column == 0 {
                end_point.row.saturating_sub(1)
            } else {
                end_point.row
            };
            let current_line_start = line_start(start_row);
            let current_line_end = line_start(end_row + 1);
            let already_line_shaped =
                start_offset == current_line_start && end_offset == current_line_end;
            let extension_rows = if already_line_shaped {
                count
            } else {
                count.saturating_sub(1)
            };
            let target_end_row = end_row.saturating_add(extension_rows);
            let new_end_offset = line_start(target_end_row.saturating_add(1));
            let start_anchor = snapshot.anchor_at(current_line_start, Bias::Left);
            let end_anchor = snapshot.anchor_at(new_end_offset, Bias::Right);
            Selection {
                id: sel.id,
                start: start_anchor,
                end: end_anchor,
                reversed: false,
                goal: SelectionGoal::None,
            }
        });
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Keep only the primary (newest) selection, dropping all
    /// others.
    pub fn keep_primary_selection(&mut self, cx: &mut Context<'_, Self>) {
        self.selections.keep_primary();
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Remove the primary selection, keeping the rest. No-op when
    /// fewer than two selections are present.
    pub fn remove_primary_selection(&mut self, cx: &mut Context<'_, Self>) {
        self.selections.remove_primary();
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Rotate which selection is primary by `count` positions
    /// (`forward = true` advances; `false` retreats). No-op when
    /// fewer than two selections are present.
    pub fn rotate_selections(&mut self, forward: bool, count: u32, cx: &mut Context<'_, Self>) {
        let count = count.max(1);
        self.selections.rotate_primary_by(forward, count);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Trim leading and trailing whitespace from every selection.
    /// Selections containing only whitespace collapse to a cursor
    /// at their head; if every selection collapses, the result
    /// keeps only the primary.
    pub fn trim_selections(&mut self, cx: &mut Context<'_, Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let rope = snapshot.rope().clone();
        let trimmed: Vec<Selection<Anchor>> = self
            .selections
            .all_anchors()
            .iter()
            .filter_map(|sel| {
                let start = snapshot.resolve_anchor(&sel.start);
                let end = snapshot.resolve_anchor(&sel.end);
                let (new_start, new_end) = trim_whitespace_in_rope(&rope, start, end)?;
                let mut new = sel.clone();
                new.start = snapshot.anchor_at(new_start, Bias::Left);
                new.end = snapshot.anchor_at(new_end, Bias::Right);
                Some(new)
            })
            .collect();
        if trimmed.is_empty() {
            self.selections.transform(&snapshot, |sel| {
                let mut new = sel.clone();
                new.collapse_to(sel.head(), sel.goal);
                new
            });
            self.selections.keep_primary();
        } else {
            self.selections.replace_with(trimmed, &snapshot);
        }
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Split every non-empty selection at each line-feed,
    /// producing one piece per line. Selections without newlines
    /// are left unchanged.
    pub fn split_selection_on_newline(&mut self, cx: &mut Context<'_, Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let rope = snapshot.rope().clone();
        self.selections.split_each(&snapshot, |sel| {
            let start_offset = snapshot.resolve_anchor(&sel.start);
            let end_offset = snapshot.resolve_anchor(&sel.end);
            if start_offset == end_offset {
                return Vec::new();
            }
            let mut newline_positions: Vec<usize> = Vec::new();
            let mut byte_pos = start_offset;
            for ch in rope.chars_at(start_offset) {
                if byte_pos >= end_offset {
                    break;
                }
                if ch == '\n' {
                    newline_positions.push(byte_pos);
                }
                byte_pos += ch.len_utf8();
            }
            if newline_positions.is_empty() {
                return Vec::new();
            }
            let mut pieces: Vec<Selection<Anchor>> =
                Vec::with_capacity(newline_positions.len() + 1);
            let mut prev = start_offset;
            for nl in &newline_positions {
                if *nl > prev {
                    pieces.push(Selection {
                        id: 0,
                        start: snapshot.anchor_at(prev, Bias::Right),
                        end: snapshot.anchor_at(*nl, Bias::Right),
                        reversed: false,
                        goal: SelectionGoal::None,
                    });
                }
                prev = nl + 1;
            }
            if prev < end_offset {
                pieces.push(Selection {
                    id: 0,
                    start: snapshot.anchor_at(prev, Bias::Right),
                    end: snapshot.anchor_at(end_offset, Bias::Right),
                    reversed: false,
                    goal: SelectionGoal::None,
                });
            }
            pieces
        });
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Insert spaces ahead of selections that share a buffer row
    /// so each row's nth selection lands in the same display
    /// column. No-op if any selection spans more than one row.
    pub fn align_selections(&mut self, cx: &mut Context<'_, Self>) {
        let buffer = match self.multi_buffer.read(cx).as_singleton() {
            Some(b) => b.clone(),
            None => {
                tracing::warn!(
                    target: "stoat::editor",
                    "align_selections on multi-excerpt buffer is not yet supported",
                );
                return;
            },
        };
        let display_snapshot = self.display_map.update(cx, |dm, _| dm.snapshot());
        let buffer_snapshot = self.multi_buffer.read(cx).snapshot();
        let rope = buffer_snapshot.rope().clone();

        let mut entries: Vec<AlignEntry> = Vec::with_capacity(self.selections.all_anchors().len());
        for sel in self.selections.all_anchors() {
            let start_offset = buffer_snapshot.resolve_anchor(&sel.start);
            let end_offset = buffer_snapshot.resolve_anchor(&sel.end);
            let start_pt = rope.offset_to_point(start_offset);
            let end_pt = rope.offset_to_point(end_offset);
            if start_pt.row != end_pt.row {
                return;
            }
            let head_pt = if sel.reversed { start_pt } else { end_pt };
            let head_display = display_snapshot.buffer_to_display(head_pt);
            entries.push(AlignEntry {
                insert_offset: start_offset,
                head_col: head_display.column,
                head_row: head_display.row,
            });
        }
        if entries.is_empty() {
            return;
        }

        let mut row_indices: Vec<u32> = Vec::new();
        let row_index_for = |row_indices: &mut Vec<u32>, row: u32| -> usize {
            match row_indices.iter().position(|r| *r == row) {
                Some(i) => i,
                None => {
                    row_indices.push(row);
                    row_indices.len() - 1
                },
            }
        };

        let mut ranked: Vec<RankedEntry> = Vec::with_capacity(entries.len());
        let mut last_row: Option<u32> = None;
        let mut rank: usize = 0;
        for entry in entries {
            if Some(entry.head_row) == last_row {
                rank += 1;
            } else {
                rank = 0;
                last_row = Some(entry.head_row);
            }
            let row_idx = row_index_for(&mut row_indices, entry.head_row);
            ranked.push(RankedEntry {
                insert_offset: entry.insert_offset,
                head_col: entry.head_col,
                row_idx,
                rank,
            });
        }

        let max_rank = ranked
            .iter()
            .map(|e| e.rank)
            .max()
            .expect("entries non-empty");
        let mut offs = vec![0u32; row_indices.len()];
        let mut edits: Vec<(usize, String)> = Vec::new();
        for current_rank in 0..=max_rank {
            let max_col = ranked
                .iter()
                .filter(|e| e.rank == current_rank)
                .map(|e| e.head_col + offs[e.row_idx])
                .max();
            let Some(max_col) = max_col else { continue };
            for entry in ranked.iter().filter(|e| e.rank == current_rank) {
                let actual = entry.head_col + offs[entry.row_idx];
                if max_col > actual {
                    let pad = (max_col - actual) as usize;
                    edits.push((entry.insert_offset, " ".repeat(pad)));
                    offs[entry.row_idx] += pad as u32;
                }
            }
        }
        if edits.is_empty() {
            return;
        }
        edits.sort_by_key(|(offset, _)| *offset);
        for (offset, text) in edits.iter().rev() {
            buffer.update(cx, |b, cx| b.edit(*offset..*offset, text, cx));
        }
        let new_snapshot = self.multi_buffer.read(cx).snapshot();
        self.selections.transform(&new_snapshot, |sel| sel.clone());
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Split every non-empty selection at each match of
    /// `pattern`. An invalid regex is a no-op and a warning is
    /// logged. Selections without matches are left unchanged.
    pub fn split_selection_by_pattern(&mut self, pattern: &str, cx: &mut Context<'_, Self>) {
        let regex = match stoat::action_handlers::search::compile_search_regex(pattern) {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(
                    target: "stoat::editor",
                    ?err,
                    %pattern,
                    "split_selection_by_pattern: invalid regex",
                );
                return;
            },
        };
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let rope = snapshot.rope().clone();
        self.selections.split_each(&snapshot, |sel| {
            let start = snapshot.resolve_anchor(&sel.start);
            let end = snapshot.resolve_anchor(&sel.end);
            if start == end {
                return Vec::new();
            }
            let text: String = rope.chunks_in_range(start..end).collect();
            let mut pieces: Vec<Selection<Anchor>> = Vec::new();
            let mut piece_start = start;
            for m in regex.find_iter(&text) {
                let match_start = start + m.start();
                let match_end = start + m.end();
                if match_start > piece_start {
                    pieces.push(Selection {
                        id: 0,
                        start: snapshot.anchor_at(piece_start, Bias::Right),
                        end: snapshot.anchor_at(match_start, Bias::Right),
                        reversed: false,
                        goal: SelectionGoal::None,
                    });
                }
                piece_start = match_end;
            }
            if piece_start < end {
                pieces.push(Selection {
                    id: 0,
                    start: snapshot.anchor_at(piece_start, Bias::Right),
                    end: snapshot.anchor_at(end, Bias::Right),
                    reversed: false,
                    goal: SelectionGoal::None,
                });
            }
            pieces
        });
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Keep (or remove, with `remove = true`) selections whose
    /// covered text matches `pattern`. An invalid regex is a
    /// no-op. When the filter produces an empty result the
    /// selection set is left unchanged so the editor never lands
    /// without any cursor.
    pub fn filter_selections_by_pattern(
        &mut self,
        pattern: &str,
        remove: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let regex = match stoat::action_handlers::search::compile_search_regex(pattern) {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(
                    target: "stoat::editor",
                    ?err,
                    %pattern,
                    "filter_selections_by_pattern: invalid regex",
                );
                return;
            },
        };
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let rope = snapshot.rope().clone();
        let kept: Vec<Selection<Anchor>> = self
            .selections
            .all_anchors()
            .iter()
            .filter(|sel| {
                let start = snapshot.resolve_anchor(&sel.start);
                let end = snapshot.resolve_anchor(&sel.end);
                let text: String = rope.chunks_in_range(start..end).collect();
                regex.is_match(&text) ^ remove
            })
            .cloned()
            .collect();
        if kept.is_empty() {
            return;
        }
        self.selections.replace_with(kept, &snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Insert a new cursor one display row above or below the
    /// primary selection's head, clamping to the source's goal
    /// column. Returns `true` when a cursor was added; returns
    /// `false` when the primary is already at the top / bottom of
    /// the display map and no further row is available. Callers
    /// invoke this in a loop driven by the dispatch pending-count.
    pub fn add_selection_step(
        &mut self,
        direction: AddDirection,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let display_snapshot = self.display_map.update(cx, |dm, _| dm.snapshot());
        let buffer_snapshot = self.multi_buffer.read(cx).snapshot();

        let source = self.selections.newest_anchor().clone();
        let source_head = source.head();
        let source_point = buffer_snapshot.point_for_anchor(&source_head);
        let source_display = display_snapshot.buffer_to_display(source_point);
        let goal_col = match source.goal {
            SelectionGoal::Column(c) => c,
            SelectionGoal::None => source_display.column,
        };

        let max_row = display_snapshot.max_point().row;
        let mut row = source_display.row;
        let target_anchor = loop {
            match direction {
                AddDirection::Below => {
                    if row >= max_row {
                        return false;
                    }
                    row += 1;
                },
                AddDirection::Above => {
                    if row == 0 {
                        return false;
                    }
                    row -= 1;
                },
            }
            let clamped_col = goal_col.min(display_snapshot.line_len(row));
            let raw = DisplayPoint::new(row, clamped_col);
            let clipped = display_snapshot.clip_point(raw, Bias::Left);
            let Some(buffer_pt) = display_snapshot.display_to_buffer(clipped) else {
                continue;
            };
            let offset = buffer_snapshot.rope().point_to_offset(buffer_pt);
            break buffer_snapshot.anchor_at(offset, Bias::Right);
        };
        self.selections.insert_cursor(
            target_anchor,
            SelectionGoal::Column(goal_col),
            &buffer_snapshot,
        );
        cx.emit(EditorEvent::Changed);
        cx.notify();
        true
    }

    /// Wrap every non-empty selection with the pair returned by
    /// [`stoat::action_handlers::surround::surround_pair_for`].
    /// Selections collapse direction is preserved. Empty
    /// (cursor-only) selections are skipped.
    pub fn handle_surround_add(&mut self, ch: char, cx: &mut Context<'_, Self>) {
        let (open, close) = stoat::action_handlers::surround::surround_pair_for(ch);
        let buffer = match self.multi_buffer.read(cx).as_singleton() {
            Some(b) => b.clone(),
            None => {
                tracing::warn!(
                    target: "stoat::editor",
                    "handle_surround_add on multi-excerpt buffer is not yet supported",
                );
                return;
            },
        };
        let mut entries: Vec<(usize, usize, usize, bool)> = {
            let snapshot = self.multi_buffer.read(cx).snapshot();
            self.selections
                .all_anchors()
                .iter()
                .filter_map(|sel| {
                    let s = snapshot.resolve_anchor(&sel.start);
                    let e = snapshot.resolve_anchor(&sel.end);
                    if s == e {
                        return None;
                    }
                    Some((sel.id, s, e, sel.reversed))
                })
                .collect()
        };
        if entries.is_empty() {
            return;
        }
        entries.sort_by_key(|(_, s, _, _)| *s);

        let open_str = open.to_string();
        let close_str = close.to_string();
        for (_, s, e, _) in entries.iter().rev() {
            buffer.update(cx, |b, cx| b.edit(*e..*e, &close_str, cx));
            buffer.update(cx, |b, cx| b.edit(*s..*s, &open_str, cx));
        }

        let open_len = open.len_utf8();
        let close_len = close.len_utf8();
        let mut id_to_range: std::collections::HashMap<usize, (usize, usize, bool)> =
            std::collections::HashMap::with_capacity(entries.len());
        let mut shift: i64 = 0;
        for (id, s, e, reversed) in entries.iter() {
            let new_start = (*s as i64 + shift) as usize + open_len;
            let new_end = (*e as i64 + shift) as usize + open_len;
            id_to_range.insert(*id, (new_start, new_end, *reversed));
            shift += (open_len + close_len) as i64;
        }

        let new_snapshot = self.multi_buffer.read(cx).snapshot();
        self.selections.transform(&new_snapshot, |sel| {
            let mut new = sel.clone();
            if let Some(&(start_off, end_off, reversed)) = id_to_range.get(&sel.id) {
                new.start = new_snapshot.anchor_at(start_off, Bias::Left);
                new.end = new_snapshot.anchor_at(end_off, Bias::Right);
                new.reversed = reversed;
                new.goal = SelectionGoal::None;
            }
            new
        });
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Find the enclosing pair for `ch` around every selection
    /// head via
    /// [`stoat::action_handlers::surround::find_surround_pair`],
    /// dedupe, and remove the pair. Tree-sitter-aware when the
    /// active buffer carries a [`stoat_language::SyntaxMap`].
    pub fn handle_surround_delete(&mut self, ch: char, cx: &mut Context<'_, Self>) {
        let (open, close) = stoat::action_handlers::surround::surround_pair_for(ch);
        let pairs = match self.collect_surround_pairs(open, close, cx) {
            Some(p) if !p.is_empty() => p,
            _ => return,
        };
        let buffer = match self.multi_buffer.read(cx).as_singleton() {
            Some(b) => b.clone(),
            None => return,
        };
        let open_len = open.len_utf8();
        let close_len = close.len_utf8();
        for (open_off, close_off) in pairs.iter().rev() {
            buffer.update(cx, |b, cx| {
                b.edit(*close_off..*close_off + close_len, "", cx)
            });
            buffer.update(cx, |b, cx| b.edit(*open_off..*open_off + open_len, "", cx));
        }
        let new_snapshot = self.multi_buffer.read(cx).snapshot();
        self.selections.transform(&new_snapshot, |sel| sel.clone());
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Replace the enclosing pair for `from` around every
    /// selection head with the canonical pair for `to`.
    /// Tree-sitter-aware when the active buffer carries a
    /// [`stoat_language::SyntaxMap`].
    pub fn handle_surround_replace(&mut self, from: char, to: char, cx: &mut Context<'_, Self>) {
        let (old_open, old_close) = stoat::action_handlers::surround::surround_pair_for(from);
        let (new_open, new_close) = stoat::action_handlers::surround::surround_pair_for(to);
        let pairs = match self.collect_surround_pairs(old_open, old_close, cx) {
            Some(p) if !p.is_empty() => p,
            _ => return,
        };
        let buffer = match self.multi_buffer.read(cx).as_singleton() {
            Some(b) => b.clone(),
            None => return,
        };
        let old_open_len = old_open.len_utf8();
        let old_close_len = old_close.len_utf8();
        let new_open_str = new_open.to_string();
        let new_close_str = new_close.to_string();
        for (open_off, close_off) in pairs.iter().rev() {
            buffer.update(cx, |b, cx| {
                b.edit(*close_off..*close_off + old_close_len, &new_close_str, cx)
            });
            buffer.update(cx, |b, cx| {
                b.edit(*open_off..*open_off + old_open_len, &new_open_str, cx)
            });
        }
        let new_snapshot = self.multi_buffer.read(cx).snapshot();
        self.selections.transform(&new_snapshot, |sel| sel.clone());
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Walk every selection's primary cursor head and gather the
    /// enclosing surround pair for `(open, close)`. Returns
    /// deduped + sorted offsets; falls back to plain
    /// non-tree-sitter search when the buffer has no
    /// [`stoat_language::SyntaxMap`].
    fn collect_surround_pairs(
        &self,
        open: char,
        close: char,
        cx: &mut Context<'_, Self>,
    ) -> Option<Vec<(usize, usize)>> {
        let singleton = self.multi_buffer.read(cx).as_singleton().cloned()?;
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let rope = snapshot.rope().clone();
        let cursors: Vec<usize> = self
            .selections
            .all_anchors()
            .iter()
            .map(|sel| snapshot.resolve_anchor(&sel.head()))
            .collect();
        let map_snapshot = singleton.read(cx).syntax_map().map(|m| m.snapshot());
        let mut pairs: Vec<(usize, usize)> = cursors
            .into_iter()
            .filter_map(|head| {
                let tree = map_snapshot.as_ref().and_then(|s| {
                    s.iter_layers()
                        .fold(None::<&stoat_language::SyntaxLayer>, |acc, layer| {
                            let lstart = layer.start_offset as usize;
                            let lend = layer.end_offset as usize;
                            if lstart <= head && lend >= head {
                                match acc {
                                    Some(prev) if prev.depth >= layer.depth => acc,
                                    _ => Some(layer),
                                }
                            } else {
                                acc
                            }
                        })
                        .map(|layer| &layer.tree)
                });
                stoat::action_handlers::surround::find_surround_pair(&rope, head, open, close, tree)
            })
            .collect();
        pairs.sort_unstable();
        pairs.dedup();
        Some(pairs)
    }

    /// Prepend `count` tab characters at the start of every line
    /// touched by any selection. The previous indent stays in
    /// place; the new tabs are inserted at column 0. Multi-excerpt
    /// buffers are logged and skipped, matching
    /// [`Self::apply_text_to_all_cursors`].
    pub fn indent_lines(&mut self, count: u32, cx: &mut Context<'_, Self>) {
        let buffer = match self.multi_buffer.read(cx).as_singleton() {
            Some(b) => b.clone(),
            None => {
                tracing::warn!(
                    target: "stoat::editor",
                    "indent_lines on multi-excerpt buffer is not yet supported",
                );
                return;
            },
        };
        let count = count.max(1) as usize;

        let edits: Vec<(usize, usize, String)> = {
            let snapshot = self.multi_buffer.read(cx).snapshot();
            let rope = snapshot.rope().clone();
            let rows = touched_rows(&snapshot, &self.selections);
            rows.into_iter()
                .map(|row| {
                    let start = rope.point_to_offset(stoat_text::Point::new(row, 0));
                    (start, start, "\t".repeat(count))
                })
                .collect()
        };
        if edits.is_empty() {
            return;
        }

        for (start, end, text) in edits.iter().rev() {
            buffer.update(cx, |b, cx| b.edit(*start..*end, text, cx));
        }
        let new_snapshot = self.multi_buffer.read(cx).snapshot();
        self.selections.transform(&new_snapshot, |sel| sel.clone());
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Remove up to `count` indent groups from the start of every
    /// line touched by any selection. A tab counts as one group;
    /// a run of up to four spaces counts as one group. Lines whose
    /// first character is neither tab nor space are skipped.
    /// Multi-excerpt buffers are logged and skipped, matching
    /// [`Self::apply_text_to_all_cursors`].
    pub fn unindent_lines(&mut self, count: u32, cx: &mut Context<'_, Self>) {
        const INDENT_WIDTH: usize = 4;

        let buffer = match self.multi_buffer.read(cx).as_singleton() {
            Some(b) => b.clone(),
            None => {
                tracing::warn!(
                    target: "stoat::editor",
                    "unindent_lines on multi-excerpt buffer is not yet supported",
                );
                return;
            },
        };
        let count = count.max(1) as usize;

        let edits: Vec<(usize, usize, String)> = {
            let snapshot = self.multi_buffer.read(cx).snapshot();
            let rope = snapshot.rope().clone();
            let rows = touched_rows(&snapshot, &self.selections);
            rows.into_iter()
                .filter_map(|row| {
                    let line_start = rope.point_to_offset(stoat_text::Point::new(row, 0));
                    let head: Vec<char> = rope
                        .chars_at(line_start)
                        .take(count.saturating_mul(INDENT_WIDTH))
                        .collect();
                    let mut consumed = 0usize;
                    let mut idx = 0usize;
                    for _ in 0..count {
                        if idx >= head.len() {
                            break;
                        }
                        match head[idx] {
                            '\t' => {
                                idx += 1;
                                consumed += 1;
                            },
                            ' ' => {
                                let group_start = idx;
                                while idx < head.len()
                                    && head[idx] == ' '
                                    && idx - group_start < INDENT_WIDTH
                                {
                                    idx += 1;
                                }
                                consumed += idx - group_start;
                            },
                            _ => break,
                        }
                    }
                    (consumed > 0).then(|| (line_start, line_start + consumed, String::new()))
                })
                .collect()
        };
        if edits.is_empty() {
            return;
        }

        for (start, end, text) in edits.iter().rev() {
            buffer.update(cx, |b, cx| b.edit(*start..*end, text, cx));
        }
        let new_snapshot = self.multi_buffer.read(cx).snapshot();
        self.selections.transform(&new_snapshot, |sel| sel.clone());
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Toggle `prefix` at the first non-whitespace position of every
    /// line touched by any selection. Lines where the prefix is
    /// already present have it removed (along with one trailing
    /// space when present); other lines get `"{prefix} "` inserted
    /// at the first non-whitespace position. Blank / whitespace-only
    /// lines are skipped. Multi-excerpt buffers are logged and
    /// skipped, matching [`Self::apply_text_to_all_cursors`].
    pub fn toggle_line_comments(&mut self, prefix: &str, cx: &mut Context<'_, Self>) {
        let buffer = match self.multi_buffer.read(cx).as_singleton() {
            Some(b) => b.clone(),
            None => {
                tracing::warn!(
                    target: "stoat::editor",
                    "toggle_line_comments on multi-excerpt buffer is not yet supported",
                );
                return;
            },
        };

        let edits: Vec<(usize, usize, String)> = {
            let snapshot = self.multi_buffer.read(cx).snapshot();
            let rope = snapshot.rope().clone();
            let rows = touched_rows(&snapshot, &self.selections);
            let prefix_chars = prefix.chars().count();
            rows.into_iter()
                .filter_map(|row| {
                    let line_start = rope.point_to_offset(stoat_text::Point::new(row, 0));
                    let line_end = line_start + rope.line_len(row) as usize;
                    let mut content_start = line_start;
                    for ch in rope.chars_at(line_start) {
                        if content_start >= line_end || !ch.is_whitespace() {
                            break;
                        }
                        content_start += ch.len_utf8();
                    }
                    if content_start >= line_end {
                        return None;
                    }

                    let after_prefix = content_start + prefix.len();
                    let prefix_matches = after_prefix <= line_end
                        && rope
                            .chars_at(content_start)
                            .take(prefix_chars)
                            .collect::<String>()
                            == prefix;

                    if prefix_matches {
                        let drop_trailing_space =
                            matches!(rope.chars_at(after_prefix).next(), Some(' '));
                        let remove_end = if drop_trailing_space {
                            after_prefix + 1
                        } else {
                            after_prefix
                        };
                        Some((content_start, remove_end, String::new()))
                    } else {
                        Some((content_start, content_start, format!("{prefix} ")))
                    }
                })
                .collect()
        };
        if edits.is_empty() {
            return;
        }

        for (start, end, text) in edits.iter().rev() {
            buffer.update(cx, |b, cx| b.edit(*start..*end, text, cx));
        }
        let new_snapshot = self.multi_buffer.read(cx).snapshot();
        self.selections.transform(&new_snapshot, |sel| sel.clone());
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Find the number near each selection head via
    /// [`stoat_text::find_number_seeking`], compute the new
    /// value via
    /// [`stoat::action_handlers::movement::compute_number_delta`],
    /// and apply the edits. Cursors that land on the same
    /// number range share one edit. Re-anchors affected
    /// selections to span the new text. No-op when no cursor
    /// has a number nearby on its line.
    pub fn handle_number_delta(&mut self, delta: i64, cx: &mut Context<'_, Self>) {
        let buffer = match self.multi_buffer.read(cx).as_singleton() {
            Some(b) => b.clone(),
            None => {
                tracing::warn!(
                    target: "stoat::editor",
                    "handle_number_delta on multi-excerpt buffer is not yet supported",
                );
                return;
            },
        };
        let mut edits: Vec<(usize, usize, usize, String)> = {
            let snapshot = self.multi_buffer.read(cx).snapshot();
            let rope = snapshot.rope().clone();
            let mut seen = std::collections::HashSet::<(usize, usize)>::new();
            self.selections
                .all_anchors()
                .iter()
                .filter_map(|sel| {
                    let head_offset = snapshot.resolve_anchor(&sel.head());
                    let num_match = stoat_text::find_number_seeking(&rope, head_offset)?;
                    let key = (num_match.range.start, num_match.range.end);
                    if !seen.insert(key) {
                        return None;
                    }
                    let text = rope
                        .slice(num_match.range.start..num_match.range.end)
                        .to_string();
                    let new_text = stoat::action_handlers::movement::compute_number_delta(
                        &text,
                        num_match.kind,
                        delta,
                    )?;
                    if new_text == text {
                        return None;
                    }
                    Some((sel.id, num_match.range.start, num_match.range.end, new_text))
                })
                .collect()
        };
        if edits.is_empty() {
            return;
        }
        edits.sort_by_key(|(_, s, _, _)| *s);
        for (_, s, e, new_text) in edits.iter().rev() {
            buffer.update(cx, |b, cx| b.edit(*s..*e, new_text, cx));
        }
        let edited_ranges: std::collections::HashMap<usize, (usize, usize)> = edits
            .iter()
            .map(|(id, s, _, new_text)| (*id, (*s, *s + new_text.len())))
            .collect();
        let new_snapshot = self.multi_buffer.read(cx).snapshot();
        self.selections.transform(&new_snapshot, |sel| {
            let mut new = sel.clone();
            if let Some((s, e)) = edited_ranges.get(&sel.id) {
                new.start = new_snapshot.anchor_at(*s, Bias::Left);
                new.end = new_snapshot.anchor_at(*e, Bias::Right);
            }
            new
        });
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Pop up to `count` entries off the active buffer's undo
    /// stack, applying each in reverse and re-anchoring selections
    /// against the resulting snapshot. Stops early when the stack
    /// is exhausted. Returns the number of operations actually
    /// applied so callers can decide whether to redraw.
    pub fn handle_undo(&mut self, count: u32, cx: &mut Context<'_, Self>) -> u32 {
        self.apply_buffer_history(count, |buffer, cx| buffer.undo(cx), cx)
    }

    /// Pop up to `count` entries off the redo stack and re-apply
    /// each. Symmetric to [`Self::handle_undo`].
    pub fn handle_redo(&mut self, count: u32, cx: &mut Context<'_, Self>) -> u32 {
        self.apply_buffer_history(count, |buffer, cx| buffer.redo(cx), cx)
    }

    /// Record an unlabeled checkpoint in the active buffer's op
    /// log. Callers that need labeled checkpoints pass `Some(label)`.
    pub fn commit_checkpoint(
        &mut self,
        label: Option<String>,
        cx: &mut Context<'_, Self>,
    ) -> Option<stoat::buffer::CheckpointId> {
        let buffer = self.multi_buffer.read(cx).as_singleton().cloned()?;
        Some(buffer.update(cx, |b, _| b.checkpoint(label)))
    }

    fn apply_buffer_history<F>(&mut self, count: u32, mut op: F, cx: &mut Context<'_, Self>) -> u32
    where
        F: FnMut(&Buffer, &mut Context<'_, Buffer>) -> bool,
    {
        let buffer = match self.multi_buffer.read(cx).as_singleton() {
            Some(b) => b.clone(),
            None => {
                tracing::warn!(
                    target: "stoat::editor",
                    "buffer-history on multi-excerpt buffer is not yet supported",
                );
                return 0;
            },
        };
        let target = count.max(1);
        let mut applied = 0u32;
        for _ in 0..target {
            if buffer.update(cx, |b, cx| op(b, cx)) {
                applied += 1;
            } else {
                break;
            }
        }
        if applied == 0 {
            return 0;
        }
        let snapshot = self.multi_buffer.read(cx).snapshot();
        self.selections.transform(&snapshot, |sel| sel.clone());
        cx.emit(EditorEvent::Changed);
        cx.notify();
        applied
    }

    /// Place a single cursor at display-grid `(row, col)`, replacing
    /// every existing selection. Off-buffer rows or columns clamp to
    /// the nearest valid display position via [`DisplaySnapshot::clip_point`].
    /// Display rows that correspond to a custom block (no matching
    /// buffer point) are silently ignored.
    pub fn set_cursor_at_grid(&mut self, row: u32, col: u32, cx: &mut Context<'_, Self>) {
        let display_snapshot = self.display_map.update(cx, |dm, _| dm.snapshot());
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let clipped = display_snapshot.clip_point(DisplayPoint::new(row, col), Bias::Left);
        let Some(buffer_point) = display_snapshot.display_to_buffer(clipped) else {
            return;
        };
        let offset = snapshot.rope().point_to_offset(buffer_point);
        let anchor = snapshot.anchor_at(offset, Bias::Left);
        let new_id = self
            .selections
            .all_anchors()
            .iter()
            .map(|s| s.id)
            .max()
            .map(|m| m + 1)
            .unwrap_or(1);
        let selection = Selection {
            id: new_id,
            start: anchor,
            end: anchor,
            reversed: false,
            goal: SelectionGoal::None,
        };
        self.selections.replace_with(vec![selection], &snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Place a single cursor at the start of buffer `row`, replacing
    /// every existing selection. Rows past the buffer's last line
    /// clamp to the rope's last valid point via [`stoat_text::Rope::clip_point`].
    /// Used by review-chunk navigation; future review handlers that
    /// jump to a buffer row reuse this entry point.
    pub fn set_cursor_at_buffer_row(&mut self, row: u32, cx: &mut Context<'_, Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let clipped = snapshot
            .rope()
            .clip_point(stoat_text::Point::new(row, 0), Bias::Left);
        let offset = snapshot.rope().point_to_offset(clipped);
        let anchor = snapshot.anchor_at(offset, Bias::Left);
        let new_id = self
            .selections
            .all_anchors()
            .iter()
            .map(|s| s.id)
            .max()
            .map(|m| m + 1)
            .unwrap_or(1);
        let selection = Selection {
            id: new_id,
            start: anchor,
            end: anchor,
            reversed: false,
            goal: SelectionGoal::None,
        };
        self.selections.replace_with(vec![selection], &snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Buffer row (0-based) of the primary (newest) cursor's head, in the
    /// same multi-buffer coordinate [`Self::set_cursor_at_buffer_row`]
    /// accepts. For a review editor this is the file row, comparable to a
    /// chunk row's `right.line_num - 1`.
    pub fn primary_cursor_buffer_row(&self, cx: &App) -> u32 {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let head = self.selections.newest_anchor().head();
        snapshot.point_for_anchor(&head).row
    }

    pub fn cell_size(&self) -> Option<Size<Pixels>> {
        self.cell_size
    }

    /// Record the cell metrics the render path is laying out with.
    /// Off-screen consumers (the IME bounds query in particular) need
    /// these dimensions to convert display positions into pixel
    /// coordinates. Emits [`EditorEvent::Changed`] when the value
    /// actually changes.
    pub fn set_cell_size(&mut self, size: Size<Pixels>, cx: &mut Context<'_, Self>) {
        if self.cell_size == Some(size) {
            return;
        }
        self.cell_size = Some(size);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    pub fn file_path(&self) -> Option<&std::path::Path> {
        self.file_path.as_deref()
    }

    pub fn set_file_path(&mut self, path: Option<std::path::PathBuf>, cx: &mut Context<'_, Self>) {
        if self.file_path == path {
            return;
        }
        self.file_path = path;
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    pub fn diagnostic_set(&self) -> Option<&Entity<crate::diagnostics::DiagnosticSet>> {
        self.diagnostic_set.as_ref()
    }

    /// Attach an [`Entity<DiagnosticSet>`] whose diagnostics light up the
    /// gutter glyph for `file_path`. The editor subscribes to the set
    /// and re-emits [`EditorEvent::Changed`] when a diagnostic publish
    /// touches any path; the gutter render filters by `file_path`.
    pub fn set_diagnostic_set(
        &mut self,
        set: Option<Entity<crate::diagnostics::DiagnosticSet>>,
        cx: &mut Context<'_, Self>,
    ) {
        self._diagnostic_subscription = set.as_ref().map(|entity| {
            cx.subscribe(
                entity,
                |_, _, _event: &crate::diagnostics::DiagnosticSetEvent, cx| {
                    cx.emit(EditorEvent::Changed);
                    cx.notify();
                },
            )
        });
        self.diagnostic_set = set;
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    pub fn blame_state(&self) -> Option<&Entity<crate::git::blame::BlameState>> {
        self.blame_state.as_ref()
    }

    pub fn blame_visible(&self) -> bool {
        self.blame_visible
    }

    /// Attach (or clear) the per-buffer [`BlameState`] whose cached
    /// [`stoat::host::BlameLine`] entries feed the optional left-side
    /// gutter strip. The editor subscribes to the state so any
    /// cache update or edit-driven invalidation re-renders the
    /// gutter without polling. Toggling visibility on or off is a
    /// separate switch ([`set_blame_visible`]); attaching a state
    /// alone does not paint the strip.
    pub fn set_blame_state(
        &mut self,
        state: Option<Entity<crate::git::blame::BlameState>>,
        cx: &mut Context<'_, Self>,
    ) {
        self._blame_subscription = state.as_ref().map(|entity| {
            cx.subscribe(
                entity,
                |_, _, _event: &crate::git::blame::BlameStateEvent, cx| {
                    cx.emit(EditorEvent::Changed);
                    cx.notify();
                },
            )
        });
        self.blame_state = state;
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Flip the per-editor blame-strip visibility flag. When `true`
    /// and a [`BlameState`] is attached, the gutter prepends one
    /// fixed-width column per row carrying short sha, first author
    /// name, and short relative age.
    pub fn set_blame_visible(&mut self, visible: bool, cx: &mut Context<'_, Self>) {
        if self.blame_visible == visible {
            return;
        }
        self.blame_visible = visible;
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    pub fn minimap_visible(&self) -> bool {
        self.minimap_visible
    }

    /// The minimap child editor, present only while the minimap is
    /// visible. It mirrors this editor in [`EditorMode::Minimap`],
    /// sharing the display and diff maps so it reflects the same
    /// content without an independent layout pass.
    pub fn minimap(&self) -> Option<&Entity<Editor>> {
        self.minimap.as_ref()
    }

    /// Flip the per-editor minimap visibility. Toggling on constructs
    /// the [`EditorMode::Minimap`] child via [`Self::make_minimap`] and
    /// retains it; toggling off drops it. Toggling does not affect this
    /// editor's own viewport or selections.
    pub fn set_minimap_visible(&mut self, visible: bool, cx: &mut Context<'_, Self>) {
        if self.minimap_visible == visible {
            return;
        }
        self.minimap_visible = visible;
        if visible {
            if self.minimap.is_none() {
                let minimap = self.make_minimap(cx);
                self.minimap = Some(minimap);
            }
        } else {
            self.minimap = None;
        }
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Construct an [`EditorMode::Minimap`] child of this editor. The
    /// child shares this editor's [`MultiBuffer`], [`DisplayMap`], and
    /// [`DiffMap`] entities so it reflects the same content and diff
    /// state, and holds a [`WeakEntity`] back-reference to this editor
    /// as its scroll and paint anchor. The reduced font and viewport
    /// thumb are applied by the minimap render path.
    fn make_minimap(&self, cx: &mut Context<'_, Self>) -> Entity<Editor> {
        let multi_buffer = self.multi_buffer.clone();
        let display_map = self.display_map.clone();
        let diff_map = self.diff_map.clone();
        let parent = cx.entity().downgrade();
        cx.new(|cx| {
            Editor::new(
                multi_buffer,
                display_map,
                diff_map,
                EditorMode::Minimap { parent },
                cx,
            )
        })
    }

    /// Begin a minimap thumb drag when `position` (window coordinates) lands
    /// on the painted thumb. Captures the scroll/viewport metrics for the
    /// gesture and marks the parent's
    /// [`ScrollManager::minimap_thumb_state`] as dragging. No-op on a
    /// non-minimap editor, before the metrics are known, or when the click
    /// misses the thumb.
    fn minimap_thumb_drag_start(&mut self, position: Point<Pixels>, cx: &mut Context<'_, Self>) {
        let Some(region) = self.text_region_bounds() else {
            return;
        };
        let EditorMode::Minimap { parent } = &self.mode else {
            return;
        };
        let Some(parent) = parent.upgrade() else {
            return;
        };
        let total_lines = self
            .display_map
            .update(cx, |dm, _| dm.snapshot())
            .max_point()
            .row as f64
            + 1.0;
        let (visible_lines, start_scroll_y) = {
            let parent = parent.read(cx);
            let (Some(p_region), Some(cell)) = (parent.text_region_bounds(), parent.cell_size())
            else {
                return;
            };
            let line_height = f32::from(cell.height) as f64;
            if line_height <= 0.0 {
                return;
            }
            (
                f32::from(p_region.size.height) as f64 / line_height,
                parent.scroll_manager().anchor().offset.y,
            )
        };

        let Some(thumb) = minimap_thumb_bounds(region, total_lines, visible_lines, start_scroll_y)
        else {
            return;
        };
        if !thumb.contains(&position) {
            return;
        }

        self.minimap_drag = Some(MinimapDrag {
            start_mouse_y: position.y,
            start_scroll_y,
            total_lines,
            visible_lines,
            minimap_height: f32::from(region.size.height) as f64,
        });
        parent.update(cx, |parent, _| {
            parent
                .scroll_manager_mut()
                .set_minimap_thumb_state(Some(scroll::ScrollbarThumbState::Dragging));
        });
        cx.notify();
    }

    /// Continue an active minimap thumb drag: map the pointer's Y delta
    /// since drag start into a parent scroll position and apply it. No-op
    /// when no drag is active.
    fn minimap_thumb_drag_to(&mut self, position: Point<Pixels>, cx: &mut Context<'_, Self>) {
        let Some(drag) = self.minimap_drag else {
            return;
        };
        if drag.minimap_height <= 0.0 {
            return;
        }
        let EditorMode::Minimap { parent } = &self.mode else {
            return;
        };
        let Some(parent) = parent.upgrade() else {
            return;
        };

        let delta_y = f32::from(position.y - drag.start_mouse_y) as f64;
        let delta_rows = delta_y * drag.total_lines / drag.minimap_height;
        let max_scroll = (drag.total_lines - drag.visible_lines).max(0.0);
        let new_y = (drag.start_scroll_y + delta_rows).clamp(0.0, max_scroll);

        parent.update(cx, |parent, cx| parent.set_scroll_position_y(new_y, cx));
    }

    /// End a minimap thumb drag, clearing the captured metrics and the
    /// parent's dragging marker. No-op when no drag is active.
    fn minimap_thumb_drag_end(&mut self, cx: &mut Context<'_, Self>) {
        if self.minimap_drag.take().is_none() {
            return;
        }
        if let EditorMode::Minimap { parent } = &self.mode {
            if let Some(parent) = parent.upgrade() {
                parent.update(cx, |parent, _| {
                    parent.scroll_manager_mut().set_minimap_thumb_state(None);
                });
            }
        }
        cx.notify();
    }

    /// Scroll the viewport to fractional row `new_y`, updating the scroll
    /// anchor, the tracked [`UniformListScrollHandle`] offset, and the
    /// integer [`Editor::scroll_row`]. Always requests a repaint so sub-row
    /// changes (e.g. a minimap drag in progress) still refresh. No-op until
    /// cell metrics are known.
    fn set_scroll_position_y(&mut self, new_y: f64, cx: &mut Context<'_, Self>) {
        let Some(cell) = self.cell_size else {
            return;
        };
        let mut anchor = *self.scroll_manager.anchor();
        anchor.offset.y = new_y;
        self.scroll_manager.set_anchor(anchor);

        let pixel_offset_y = render::scroll_position_to_pixel_offset_y(new_y, cell.height);
        self.scroll_handle
            .0
            .borrow()
            .base_handle
            .set_offset(Point::new(px(0.0), pixel_offset_y));

        self.set_scroll_row(new_y.floor().max(0.0) as u32, cx);
        cx.notify();
    }

    /// Begin an eased scroll to fractional row `target_y`, spawning a task
    /// that steps the animation each frame until it completes. The task is
    /// retained on the editor; starting a new animation replaces (and so
    /// cancels) any prior one.
    fn animate_scroll_to(&mut self, target_y: f64, cx: &mut Context<'_, Self>) {
        let executor = cx.global::<ExecutorGlobal>().0.clone();
        let start = self.scroll_manager.anchor().offset;
        self.scroll_manager.start_scroll_animation(
            start,
            Point::new(start.x, target_y),
            executor.now(),
            SCROLL_ANIMATION_DURATION,
        );
        self.scroll_animation_task = Some(cx.spawn(async move |editor, cx| loop {
            executor.timer(SCROLL_ANIMATION_FRAME).await;
            let still_animating = editor
                .update(cx, |editor, cx| editor.step_scroll_animation(cx))
                .unwrap_or(false);
            if !still_animating {
                break;
            }
        }));
    }

    /// Apply one frame of the active scroll animation, sampling the
    /// executor clock. Returns whether the animation is still running;
    /// clears it and returns `false` once complete or absent.
    fn step_scroll_animation(&mut self, cx: &mut Context<'_, Self>) -> bool {
        let Some(animation) = self.scroll_manager.animation().copied() else {
            return false;
        };
        let now = cx.global::<ExecutorGlobal>().0.now();
        self.set_scroll_position_y(animation.position_at(now).y, cx);
        if animation.is_complete(now) {
            self.scroll_manager.clear_animation();
            false
        } else {
            true
        }
    }

    pub fn review_session(&self) -> Option<&Entity<crate::review_session::ReviewSession>> {
        self.review_session.as_ref()
    }

    pub fn search_state(&self) -> Option<&search::SearchState> {
        self.search_state.as_ref()
    }

    /// Set the editor's in-buffer search state. The status-bar
    /// indicator and (in sibling work) the highlight pass observe
    /// the editor's [`EditorEvent::Changed`] and refresh from the
    /// new state. Pass `None` to clear an active search.
    pub fn set_search_state(
        &mut self,
        state: Option<search::SearchState>,
        cx: &mut Context<'_, Self>,
    ) {
        if self.search_state == state {
            return;
        }
        self.search_state = state;
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Return a compiled regex for `query`, reusing the cached
    /// compilation when `query` matches the cached key. Recompiles
    /// and caches when the query changes; returns `None` if the
    /// query is empty or fails to compile.
    pub(crate) fn compiled_search_regex(&mut self, query: &str) -> Option<&regex::Regex> {
        if query.is_empty() {
            self.cached_search_regex = None;
            return None;
        }
        let cache_hit = self
            .cached_search_regex
            .as_ref()
            .is_some_and(|(cached, _)| cached == query);
        if !cache_hit {
            match stoat::action_handlers::search::compile_search_regex(query) {
                Ok(regex) => self.cached_search_regex = Some((query.to_string(), regex)),
                Err(_) => {
                    self.cached_search_regex = None;
                    return None;
                },
            }
        }
        self.cached_search_regex.as_ref().map(|(_, r)| r)
    }

    /// Attach an [`Entity<ReviewSession>`] so review-aware UI -- the
    /// status-bar progress badge and (in sibling items) the review
    /// ItemView -- can read the session's progress and chunk state.
    /// The editor subscribes to the session and re-emits
    /// [`EditorEvent::Changed`] on every mutation so observers refresh
    /// without polling.
    pub fn set_review_session(
        &mut self,
        session: Option<Entity<crate::review_session::ReviewSession>>,
        cx: &mut Context<'_, Self>,
    ) {
        self._review_session_subscription = session.as_ref().map(|entity| {
            cx.subscribe(
                entity,
                |_, _, _event: &crate::review_session::ReviewSessionEvent, cx| {
                    cx.emit(EditorEvent::Changed);
                    cx.notify();
                },
            )
        });
        self.review_session = session;
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Index of this editor's file within the attached
    /// [`crate::review_session::ReviewSession`]'s `files` vec. The
    /// render path filters chunks to those whose `file_index` matches
    /// this value when painting per-chunk gutter glyphs; without a
    /// file index, no glyphs are painted even when a review session
    /// is attached.
    pub fn review_file_index(&self) -> Option<usize> {
        self.review_file_index
    }

    pub fn set_review_file_index(&mut self, index: Option<usize>, cx: &mut Context<'_, Self>) {
        if self.review_file_index == index {
            return;
        }
        self.review_file_index = index;
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    pub fn workspace(&self) -> Option<&WeakEntity<crate::workspace::Workspace>> {
        self.workspace.as_ref()
    }

    /// Wire the editor to its enclosing [`Workspace`] so mouse handlers
    /// in the render path can dispatch positional actions
    /// ([`crate::actions::ClickAt`], [`crate::actions::DragSelectTo`],
    /// [`crate::actions::HoverAt`]) through the workspace's action
    /// dispatch surface. Production callers set this after constructing
    /// the editor; tests that exercise the render-side mouse handlers
    /// must set it before simulating mouse events.
    pub fn set_workspace(&mut self, workspace: Option<WeakEntity<crate::workspace::Workspace>>) {
        self.workspace = workspace;
    }

    pub fn text_region_bounds(&self) -> Option<Bounds<Pixels>> {
        self.text_region_bounds
    }

    /// Record the bounds of the editor's text region as observed during
    /// paint. Mouse handlers subtract `bounds.origin` from
    /// window-relative event positions to obtain element-relative
    /// pixels before passing them through
    /// [`crate::editor::mouse::point_to_grid`]. Emits
    /// [`EditorEvent::Changed`] only when the value actually changes.
    pub fn set_text_region_bounds(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<'_, Self>) {
        if self.text_region_bounds == Some(bounds) {
            return;
        }
        self.text_region_bounds = Some(bounds);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    pub fn hover_position(&self) -> Option<(u32, u32)> {
        self.hover_position
    }

    /// Store the most recent debounced hover grid position. The LSP
    /// hover popup observes this to compute the hover request; the
    /// mouse-move debounce in the editor's render path drives updates
    /// through the [`crate::actions::HoverAt`] dispatch arm. Emits
    /// [`EditorEvent::Changed`] only when the value actually changes.
    pub fn set_hover_position(&mut self, position: Option<(u32, u32)>, cx: &mut Context<'_, Self>) {
        if self.hover_position == position {
            return;
        }
        self.hover_position = position;
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Construct the [`crate::lsp::HoverPopup`] entity that observes
    /// this editor's `hover_position` transitions and renders the
    /// floating LSP hover panel. Workspace wiring calls this once
    /// after [`Self::set_workspace`] so production editors paint
    /// hover content above the text region; tests that exercise the
    /// popup directly skip this and construct the entity themselves.
    pub fn install_hover_popup(&mut self, cx: &mut Context<'_, Self>) {
        if self.hover_popup.is_some() {
            return;
        }
        let editor = cx.entity();
        let popup = cx.new(|popup_cx| crate::lsp::HoverPopup::new(editor, popup_cx));
        self.hover_popup = Some(popup);
    }

    pub fn hover_popup(&self) -> Option<&Entity<crate::lsp::HoverPopup>> {
        self.hover_popup.as_ref()
    }

    /// Construct the [`crate::lsp::CompletionPopup`] entity that
    /// observes this editor's buffer edits and surfaces LSP
    /// completion results while the workspace is in insert mode.
    /// Production wiring calls this once after [`Self::set_workspace`].
    pub fn install_completion_popup(&mut self, cx: &mut Context<'_, Self>) {
        if self.completion_popup.is_some() {
            return;
        }
        let editor = cx.entity();
        let popup = cx.new(|popup_cx| crate::lsp::CompletionPopup::new(editor, popup_cx));
        self.completion_popup = Some(popup);
    }

    pub fn completion_popup(&self) -> Option<&Entity<crate::lsp::CompletionPopup>> {
        self.completion_popup.as_ref()
    }

    /// Construct the [`crate::lsp::InlayHintsManager`] entity that
    /// observes this editor's buffer edits and scroll changes and
    /// drives `textDocument/inlayHint` requests into the editor's
    /// [`crate::display_map::DisplayMap`]. Production wiring calls
    /// this once after [`Self::set_workspace`].
    pub fn install_inlay_hints(&mut self, cx: &mut Context<'_, Self>) {
        if self.inlay_hints_manager.is_some() {
            return;
        }
        let editor = cx.entity();
        let manager = cx.new(|mgr_cx| crate::lsp::InlayHintsManager::new(editor, mgr_cx));
        self.inlay_hints_manager = Some(manager);
    }

    pub fn inlay_hints_manager(&self) -> Option<&Entity<crate::lsp::InlayHintsManager>> {
        self.inlay_hints_manager.as_ref()
    }

    /// Construct the [`crate::lsp::SemanticTokensManager`] entity
    /// that drives `textDocument/semanticTokens/full` requests for
    /// the active buffer into the editor's
    /// [`crate::display_map::DisplayMap`]. Production wiring calls
    /// this once after [`Self::set_workspace`].
    pub fn install_semantic_tokens(&mut self, cx: &mut Context<'_, Self>) {
        if self.semantic_tokens_manager.is_some() {
            return;
        }
        let editor = cx.entity();
        let manager = cx.new(|mgr_cx| crate::lsp::SemanticTokensManager::new(editor, mgr_cx));
        self.semantic_tokens_manager = Some(manager);
    }

    pub fn semantic_tokens_manager(&self) -> Option<&Entity<crate::lsp::SemanticTokensManager>> {
        self.semantic_tokens_manager.as_ref()
    }

    /// Construct the [`crate::syntax_updater::SyntaxMapUpdater`] entity
    /// that observes this editor's buffer edits and rebuilds the
    /// multi-layer parse tree on each change. Resolves the buffer's
    /// language from its `file_path` via the global
    /// [`crate::globals::LanguageRegistry`]; no-op when the buffer has
    /// no file path, no language matches the extension, or the
    /// underlying multi-buffer is not a singleton (the buffer the
    /// updater would observe is ambiguous in the multi-excerpt case).
    pub fn install_syntax_map_updater(&mut self, cx: &mut Context<'_, Self>) {
        if self.syntax_map_updater.is_some() {
            return;
        }
        let Some(path) = self.file_path.clone() else {
            return;
        };
        let Some(language) = cx
            .try_global::<crate::globals::LanguageRegistry>()
            .and_then(|reg| reg.0.for_path(&path))
        else {
            return;
        };
        let Some(buffer) = self.multi_buffer.read(cx).as_singleton().cloned() else {
            return;
        };
        let updater =
            cx.new(|upd_cx| crate::syntax_updater::SyntaxMapUpdater::new(buffer, language, upd_cx));
        self.syntax_map_updater = Some(updater);
    }

    pub fn syntax_map_updater(&self) -> Option<&Entity<crate::syntax_updater::SyntaxMapUpdater>> {
        self.syntax_map_updater.as_ref()
    }

    /// Retarget a stand-alone preview [`Editor`] at `path`. Updates the
    /// editor's and singleton buffer's `file_path`, scrolls back to
    /// the top, drops the current
    /// [`crate::syntax_updater::SyntaxMapUpdater`], and re-installs it
    /// so the new path's language drives the syntax pipeline. Used by
    /// the file finder's preview pane on every selection change.
    pub fn set_preview_target(&mut self, path: std::path::PathBuf, cx: &mut Context<'_, Self>) {
        if let Some(buffer) = self.multi_buffer.read(cx).as_singleton().cloned() {
            buffer.update(cx, |b, cx| b.set_file_path(Some(path.clone()), cx));
        }
        self.set_file_path(Some(path), cx);
        self.set_scroll_row(0, cx);
        self.syntax_map_updater = None;
        self.install_syntax_map_updater(cx);
    }

    /// Arm a `goto_word` jump: store the label set and clear any
    /// in-progress typed prefix so the next character keystrokes
    /// step through [`stoat::goto_word::step_jump`]. The render
    /// overlay observes [`Self::pending_goto_word_labels`] on the
    /// next frame.
    pub fn arm_pending_goto_word(
        &mut self,
        labels: std::collections::BTreeMap<String, usize>,
        cx: &mut Context<'_, Self>,
    ) {
        self.pending_goto_word_labels = Some(labels);
        self.pending_goto_word_input.clear();
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    pub fn pending_goto_word_labels(&self) -> Option<&std::collections::BTreeMap<String, usize>> {
        self.pending_goto_word_labels.as_ref()
    }

    pub fn pending_goto_word_input(&self) -> &str {
        &self.pending_goto_word_input
    }

    /// Append `ch` to the typed prefix for an in-progress
    /// `goto_word` jump. Called by
    /// [`crate::input_state_machine::InputStateMachine::feed`] on
    /// every [`stoat::goto_word::JumpStep::Continue`] step so the
    /// overlay can dim the matched prefix on the next frame.
    pub fn push_pending_goto_word_input(&mut self, ch: char, cx: &mut Context<'_, Self>) {
        self.pending_goto_word_input.push(ch);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Clear the pending `goto_word` chord. No-op when no chord is
    /// armed.
    pub fn clear_pending_goto_word(&mut self, cx: &mut Context<'_, Self>) {
        if self.pending_goto_word_labels.is_none() && self.pending_goto_word_input.is_empty() {
            return;
        }
        self.pending_goto_word_labels = None;
        self.pending_goto_word_input.clear();
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Collapse every selection's primary cursor to a left-biased
    /// anchor at the buffer byte `offset`. Offsets past the buffer
    /// end clamp to the rope's length. Mirrors the TUI's
    /// `stoat::action_handlers::movement::jump_to_offset`.
    pub fn jump_to_offset(&mut self, offset: usize, cx: &mut Context<'_, Self>) {
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let clamped = offset.min(snapshot.rope().len());
        let anchor = snapshot.anchor_at(clamped, Bias::Left);
        self.selections.transform(&snapshot, |sel| {
            let mut new = sel.clone();
            new.collapse_to(anchor, SelectionGoal::None);
            new
        });
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Extend the primary selection's head to display-grid `(row, col)`,
    /// preserving its anchor (`start`). Mouse-drag uses this to grow
    /// the selection under the cursor while the user holds the left
    /// button. Off-buffer rows or columns clamp to the nearest valid
    /// display position via [`DisplaySnapshot::clip_point`]; display
    /// rows that correspond to a custom block (no matching buffer
    /// point) are silently ignored. No-op when the editor has no
    /// selections.
    pub fn extend_primary_selection_to_grid(
        &mut self,
        row: u32,
        col: u32,
        cx: &mut Context<'_, Self>,
    ) {
        let display_snapshot = self.display_map.update(cx, |dm, _| dm.snapshot());
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let clipped = display_snapshot.clip_point(DisplayPoint::new(row, col), Bias::Left);
        let Some(buffer_point) = display_snapshot.display_to_buffer(clipped) else {
            return;
        };
        let offset = snapshot.rope().point_to_offset(buffer_point);
        let head = snapshot.anchor_at(offset, Bias::Left);

        let mut all = self.selections.all_anchors().to_vec();
        let Some(primary) = all.first_mut() else {
            return;
        };
        let anchor_offset = snapshot.resolve_anchor(&primary.start);
        let head_offset = offset;
        if head_offset >= anchor_offset {
            primary.end = head;
            primary.reversed = false;
        } else {
            primary.end = head;
            primary.reversed = true;
        }
        primary.goal = SelectionGoal::None;
        self.selections.replace_with(all, &snapshot);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Schedule a 50ms hover debounce that dispatches
    /// [`crate::actions::HoverAt`] through the wired [`Workspace`].
    /// Each call cancels the prior pending timer by replacing the
    /// stored task; the new task fires only if the editor still has a
    /// live workspace handle. No-op when the editor has no workspace
    /// wired ([`set_workspace`] not called).
    /// Resolve `position` (window-relative pixels) to a `(row, col)`
    /// grid coordinate using the editor's stored text-region bounds
    /// and cell metrics. Returns `None` when either is unset (no
    /// paint has run yet or cell metrics have not been reported).
    fn position_to_grid(&self, position: Point<Pixels>) -> Option<(u32, u32)> {
        let bounds = self.text_region_bounds?;
        let cell = self.cell_size?;
        let elem = Point::new(position.x - bounds.origin.x, position.y - bounds.origin.y);
        Some(mouse::point_to_grid(elem, cell))
    }

    pub(crate) fn dispatch_click_at(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((row, col)) = self.position_to_grid(position) else {
            return;
        };
        let Some(workspace) = self.workspace.as_ref().and_then(WeakEntity::upgrade) else {
            return;
        };
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |w, cx| {
                w.dispatch_action(Box::new(crate::actions::ClickAt { row, col }), window, cx);
            });
        });
    }

    pub(crate) fn dispatch_drag_select_to(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((row, col)) = self.position_to_grid(position) else {
            return;
        };
        let Some(workspace) = self.workspace.as_ref().and_then(WeakEntity::upgrade) else {
            return;
        };
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |w, cx| {
                w.dispatch_action(
                    Box::new(crate::actions::DragSelectTo { row, col }),
                    window,
                    cx,
                );
            });
        });
    }

    fn schedule_hover_at(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some((row, col)) = self.position_to_grid(position) else {
            return;
        };
        self.schedule_hover_debounce(row, col, window, cx);
    }

    pub fn schedule_hover_debounce(
        &mut self,
        row: u32,
        col: u32,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(weak_workspace) = self.workspace.clone() else {
            self.hover_debounce_task = None;
            return;
        };
        let executor = cx.global::<ExecutorGlobal>().0.clone();
        let request_id = self.bump_hover_debounce_id();
        let weak_self = cx.weak_entity();
        let task = cx.spawn_in(window, async move |_, cx| {
            executor.timer(std::time::Duration::from_millis(50)).await;
            let _ = cx.update(|window, cx| {
                let still_current = weak_self
                    .read_with(cx, |ed, _| ed.hover_debounce_id() == request_id)
                    .unwrap_or(false);
                if !still_current {
                    // A newer hover position superseded this debounce
                    // before the 50ms window elapsed.
                    return;
                }
                let Some(workspace) = weak_workspace.upgrade() else {
                    return;
                };
                workspace.update(cx, |w, cx| {
                    w.dispatch_action(Box::new(crate::actions::HoverAt { row, col }), window, cx);
                });
            });
        });
        self.hover_debounce_task = Some(task);
    }

    pub(crate) fn bump_hover_debounce_id(&mut self) -> u64 {
        self.hover_debounce_seq += 1;
        self.hover_debounce_seq
    }

    pub(crate) fn hover_debounce_id(&self) -> u64 {
        self.hover_debounce_seq
    }

    /// Apply a [`ScrollWheelEvent`] to the editor's scroll state.
    /// Resolves the line height from [`Editor::cell_size`] (no-op
    /// when unset), clamps the resulting fractional offset against
    /// the buffer's last display row, mirrors the floored row into
    /// [`Editor::scroll_row`], and pushes the fractional pixel offset
    /// into the tracked [`UniformListScrollHandle`] so the
    /// `uniform_list` paints each visible row at
    /// `padded_bounds.origin.y + visible_ix * line_height - (scroll_pixel_y % line_height)`.
    pub fn handle_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some((existing_delta, _)) = self.pending_scroll_delta.take() {
            // Same-tick burst: fold the new delta into the pending one
            // and keep waiting -- the spawned closure already in flight
            // will pick up the merged value once the executor yields.
            // Modifiers from the most recent event win because alt
            // can flip the axis mid-burst.
            self.pending_scroll_delta =
                Some((existing_delta.coalesce(event.delta), event.modifiers));
            return;
        }
        self.pending_scroll_delta = Some((event.delta, event.modifiers));
        cx.spawn(async move |this, cx| {
            let _ = this.update(cx, |editor, cx| {
                let Some((delta, modifiers)) = editor.pending_scroll_delta.take() else {
                    return;
                };
                editor.apply_scroll_delta(delta, modifiers, cx);
            });
        })
        .detach();
    }

    /// Apply a wheel-equivalent scroll to the editor's anchor and
    /// scroll handles, mirroring the floored row into the editor's
    /// internal scroll_row. Resolves the line height from
    /// [`Self::cell_size`] (no-op when unset) and clamps the
    /// resulting fractional offset against the buffer's last display
    /// row.
    fn apply_scroll_delta(
        &mut self,
        delta: gpui::ScrollDelta,
        modifiers: gpui::Modifiers,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(cell) = self.cell_size else {
            return;
        };
        let max_row = self
            .display_map
            .update(cx, |dm, _| dm.snapshot())
            .max_point()
            .row as f64;
        let changed = self.scroll_manager.apply_wheel(
            delta,
            cell.height,
            modifiers.alt,
            std::time::Instant::now(),
            max_row,
        );
        if !changed {
            return;
        }
        let scroll_position_y = self.scroll_manager.anchor().offset.y;
        let pixel_offset_y =
            render::scroll_position_to_pixel_offset_y(scroll_position_y, cell.height);
        self.scroll_handle
            .0
            .borrow()
            .base_handle
            .set_offset(Point::new(px(0.0), pixel_offset_y));
        let new_row = scroll_position_y.floor().max(0.0) as u32;
        self.set_scroll_row(new_row, cx);
    }

    /// Consume any pending [`AutoscrollStrategy`] and snap the scroll
    /// position toward the resolved target row. No-op when cell
    /// metrics or text-region bounds have not been reported yet (e.g.
    /// the first paint); a future `request_autoscroll` after those
    /// fields populate will scroll on its own next pass.
    pub fn apply_pending_autoscroll(&mut self, cx: &mut Context<'_, Self>) {
        let Some(strategy) = self.scroll_manager.take_autoscroll_request() else {
            return;
        };
        let Some(cell) = self.cell_size else {
            return;
        };
        let Some(text_region) = self.text_region_bounds else {
            return;
        };
        let line_height_f64 = f32::from(cell.height) as f64;
        let viewport_height_f64 = f32::from(text_region.size.height) as f64;
        if line_height_f64 <= 0.0 || viewport_height_f64 <= 0.0 {
            return;
        }
        let visible_rows = viewport_height_f64 / line_height_f64;

        let display_snapshot = self.display_map.update(cx, |dm, _| dm.snapshot());
        let multi_buffer_snapshot = self.multi_buffer.read(cx).snapshot();

        let selections = self.selections.all_anchors();
        if selections.is_empty() {
            return;
        }

        let mut min_row = u32::MAX;
        let mut max_row = 0u32;
        let mut newest: &Selection<Anchor> = &selections[0];
        for selection in selections {
            if selection.id > newest.id {
                newest = selection;
            }
            let head = selection.head();
            let head_offset = multi_buffer_snapshot.resolve_anchor(&head);
            let head_point = multi_buffer_snapshot.rope().offset_to_point(head_offset);
            let head_display = display_snapshot.buffer_to_display(head_point);
            min_row = min_row.min(head_display.row);
            max_row = max_row.max(head_display.row);
        }
        let mut target_top = min_row as f64;
        let mut target_bottom = max_row as f64 + 1.0;

        let selections_fit = target_bottom - target_top <= visible_rows;
        if matches!(strategy, AutoscrollStrategy::Newest)
            || (matches!(strategy, AutoscrollStrategy::Fit) && !selections_fit)
        {
            let head = newest.head();
            let head_offset = multi_buffer_snapshot.resolve_anchor(&head);
            let head_point = multi_buffer_snapshot.rope().offset_to_point(head_offset);
            let head_display = display_snapshot.buffer_to_display(head_point);
            target_top = head_display.row as f64;
            target_bottom = target_top + 1.0;
        }

        let total_rows = (display_snapshot.max_point().row + 1) as f64;
        let max_scroll_top = (total_rows - visible_rows).max(0.0);
        let current_y = self.scroll_manager.anchor().offset.y;

        let new_y = compute_autoscroll_y(
            strategy,
            current_y,
            target_top,
            target_bottom,
            visible_rows,
            max_scroll_top,
            DEFAULT_VERTICAL_SCROLL_MARGIN,
        );

        if new_y == current_y {
            return;
        }
        let animate = matches!(
            strategy,
            AutoscrollStrategy::Center | AutoscrollStrategy::Top | AutoscrollStrategy::Bottom
        ) && (new_y - current_y).abs() >= MIN_ANIMATED_SCROLL_ROWS;
        if animate {
            self.animate_scroll_to(new_y, cx);
        } else {
            self.set_scroll_position_y(new_y, cx);
        }
    }

    /// Translate a buffer UTF-16 offset to the pixel rectangle of the
    /// corresponding character cell, anchored relative to
    /// `element_origin`. Returns `None` when [`Editor::cell_size`] is
    /// unset (the render path has not yet reported cell metrics).
    pub fn pixel_bounds_for_utf16_offset(
        &mut self,
        offset_utf16: usize,
        element_origin: Point<Pixels>,
        cx: &mut Context<'_, Self>,
    ) -> Option<Bounds<Pixels>> {
        let cell = self.cell_size?;
        let display_snapshot = self.display_map.update(cx, |dm, _| dm.snapshot());
        let snapshot = self.multi_buffer.read(cx).snapshot();
        let byte_offset = snapshot
            .rope()
            .offset_utf16_to_offset(OffsetUtf16(offset_utf16));
        let buffer_point = snapshot.rope().offset_to_point(byte_offset);
        let display_point = display_snapshot.buffer_to_display(buffer_point);
        let origin = Point::new(
            element_origin.x + cell.width * display_point.column as usize,
            element_origin.y + cell.height * display_point.row as usize,
        );
        Some(Bounds { origin, size: cell })
    }

    fn collect_review_render_data(&self, cx: &App) -> ReviewRenderData {
        let (Some(session), Some(file_index)) = (&self.review_session, self.review_file_index)
        else {
            return ReviewRenderData::default();
        };
        let session_ref = session.read(cx);
        let inner = session_ref.inner();
        let Some(file) = inner.files.get(file_index) else {
            return ReviewRenderData::default();
        };
        let chunk_markers = file
            .chunks
            .iter()
            .filter_map(|id| inner.chunks.get(id))
            .map(|chunk| (chunk.buffer_line_range.start, chunk.status))
            .collect::<Vec<_>>();
        let mut provenances = Vec::new();
        let mut moved_spans = Vec::new();
        for id in &file.chunks {
            let Some(chunk) = inner.chunks.get(id) else {
                continue;
            };
            for row in &chunk.hunk.rows {
                let stoat::review::ReviewRow::Changed { right, .. } = row else {
                    continue;
                };
                let Some(right) = right else { continue };
                let buffer_row = right.line_num.saturating_sub(1);
                if let Some(prov) = right.move_provenance.clone() {
                    provenances.push((buffer_row, prov));
                }
                for span in &right.moved_spans {
                    moved_spans.push((buffer_row, span.clone()));
                }
            }
        }
        ReviewRenderData {
            chunk_markers,
            provenances,
            moved_spans,
        }
    }

    fn render_visible_rows(&mut self, range: Range<usize>, cx: &mut Context<'_, Self>) -> Vec<Div> {
        let _span = tracing::trace_span!("editor.render_visible_rows").entered();
        let is_minimap = self.mode.is_minimap();
        let display_snapshot = self.display_map.update(cx, |dm, _| dm.snapshot());
        let total_rows = (display_snapshot.max_point().row + 1) as usize;
        let end = range.end.min(total_rows);
        let start = range.start.min(end);
        let mut rows = render::build_rendered_rows(&display_snapshot, start as u32..end as u32);
        let byte_maps =
            render::build_row_byte_maps(&rows, &display_snapshot, start as u32..end as u32);

        // Skip overlays whose pixels are invisible at the minimap's
        // 2px font size: per-character chips, cyan move underlines,
        // search highlight backgrounds, and goto-word labels all
        // collapse below perceptibility. Syntax-color bands and the
        // active-line band still read as horizontal stripes, so those
        // remain wired below.
        let review_data = self.collect_review_render_data(cx);
        if !is_minimap {
            render::apply_move_chip_overlay(&mut rows, &display_snapshot, start as u32..end as u32);

            render::apply_review_moved_overlay(
                &mut rows,
                &display_snapshot,
                start as u32..end as u32,
                &review_data.moved_spans,
            );
        }

        if let Some(buffer) = self.multi_buffer.read(cx).as_singleton().cloned() {
            let syntax_snapshot = buffer.read(cx).syntax_map().map(|m| m.snapshot().clone());
            if let Some(syntax_snapshot) = syntax_snapshot {
                let theme = cx
                    .try_global::<theme::Theme>()
                    .map(|t| t.0.clone())
                    .unwrap_or_else(stoat::theme::Theme::empty);
                let styles = stoat::display_map::syntax_theme::SyntaxStyles::from_theme(&theme);
                render::apply_syntax_overlay(
                    &mut rows,
                    &byte_maps,
                    &display_snapshot,
                    start as u32..end as u32,
                    &syntax_snapshot,
                    &styles,
                );
            }
        }

        let search_query: Option<String> = self
            .search_state
            .as_ref()
            .map(|s| s.query().to_string())
            .filter(|q| !q.is_empty());
        if !is_minimap {
            if let Some(query) = search_query {
                let color = cx.theme().search_match;
                if let Some(regex) = self.compiled_search_regex(&query) {
                    render::apply_search_overlay(
                        &mut rows,
                        &byte_maps,
                        &display_snapshot,
                        start as u32..end as u32,
                        regex,
                        color,
                    );
                }
            }
        }

        if !is_minimap {
            if let Some(labels) = self.pending_goto_word_labels.as_ref() {
                let input = self.pending_goto_word_input.clone();
                let label_color = cx.theme().goto_word_label;
                let prefix_color = cx.theme().goto_word_prefix;
                render::apply_goto_word_overlay(
                    &mut rows,
                    &display_snapshot,
                    start as u32..end as u32,
                    labels,
                    &input,
                    label_color,
                    prefix_color,
                );
            }
        }

        let selection_paint = render::compute_selection_paint(
            &display_snapshot,
            self.selections.all_anchors(),
            &rows,
            start as u32,
        );
        let selection_color = cx.theme().selection_editor;
        let cursor_color = cx.theme().cursor;
        let cursor_text_color = cx.theme().cursor_text;
        let active_line_color = cx.theme().line_highlight;

        let rows: Vec<render::RenderedRow> = rows
            .into_iter()
            .enumerate()
            .map(|(idx, row)| {
                let display_row = (start + idx) as u32;
                render::apply_selection_paint(
                    row,
                    display_row,
                    &selection_paint,
                    selection_color,
                    cursor_color,
                    cursor_text_color,
                    active_line_color,
                    is_minimap,
                )
            })
            .collect();

        if !self.mode.show_gutter() {
            return rows.into_iter().map(render::render_row_element).collect();
        }

        let blame_lines = match (self.blame_visible, self.blame_state.as_ref()) {
            (true, Some(state)) => Some(state.read(cx).blame().to_vec()),
            _ => None,
        };
        let blame_visible_with_data = blame_lines.as_ref().is_some_and(|v| !v.is_empty());
        let metrics = render::gutter_metrics(&display_snapshot, blame_visible_with_data);
        let diff_map_inner = self.diff_map.read(cx).diff().clone();
        let diagnostic_row_map = match (self.file_path.as_deref(), self.diagnostic_set.as_ref()) {
            (Some(path), Some(set)) => {
                Some(render::compute_row_severity_for_path(set.read(cx), path))
            },
            _ => None,
        };
        let blame_paint =
            blame_lines
                .as_ref()
                .filter(|v| !v.is_empty())
                .map(|lines| render::BlamePaint {
                    lines,
                    now_seconds: now_unix_seconds(),
                });
        let paint = render::GutterPaint {
            display_snapshot: &display_snapshot,
            diff_map: &diff_map_inner,
            diagnostics: diagnostic_row_map.as_ref(),
            review_chunk_markers: &review_data.chunk_markers,
            review_move_provenances: &review_data.provenances,
            blame: blame_paint,
            metrics,
            line_number_color: cx.theme().muted_text,
            line_number_cache: Some(&self.gutter_line_number_cache),
            blame_cache: Some(&self.gutter_blame_cache),
        };
        rows.into_iter()
            .enumerate()
            .map(|(idx, row)| {
                let display_row = (start + idx) as u32;
                render::render_row_with_gutter(row, display_row, &paint)
            })
            .collect()
    }
}

impl Render for Editor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let _span = tracing::trace_span!("editor.render").entered();
        self.apply_pending_autoscroll(cx);
        let is_minimap = self.mode.is_minimap();
        let document_rows = self
            .display_map
            .update(cx, |dm, _| dm.snapshot())
            .max_point()
            .row as usize
            + 1;
        let total_rows = if is_minimap {
            document_rows.min(MAX_MINIMAP_LINES)
        } else {
            document_rows
        };
        let handle = cx.entity().downgrade();
        let bounds_handle = handle.clone();
        let list = uniform_list("editor-rows", total_rows, move |range, _window, cx| {
            handle
                .upgrade()
                .map(|editor| editor.update(cx, |ed, cx| ed.render_visible_rows(range, cx)))
                .unwrap_or_default()
        })
        .track_scroll(self.scroll_handle.clone())
        .size_full();

        let (family, base_size) = editor_font(cx);
        let size = if is_minimap {
            MINIMAP_FONT_SIZE
        } else {
            base_size
        };
        let font_size = px(size);
        let line_height = if is_minimap {
            px(MINIMAP_LINE_HEIGHT)
        } else {
            px((size * GPUI_DEFAULT_LINE_HEIGHT_RATIO).round())
        };
        let cell_family = family.clone();
        let editor_input = self
            .workspace
            .as_ref()
            .and_then(|ws| ws.upgrade())
            .map(|ws| ws.read(cx).editor_input().clone());
        let bounds_capture = canvas(
            move |bounds, window, cx| {
                let font_id = window
                    .text_system()
                    .resolve_font(&font(cell_family.clone()));
                let measured_cell = window
                    .text_system()
                    .em_advance(font_id, font_size)
                    .ok()
                    .map(|width| gpui_size(width, line_height));
                let _ = bounds_handle.update(cx, |ed, cx| {
                    ed.set_text_region_bounds(bounds, cx);
                    if let Some(cell) = measured_cell {
                        ed.set_cell_size(cell, cx);
                    }
                });
                bounds
            },
            move |_bounds, prepaint_bounds, window, cx| {
                if let Some(editor_input) = editor_input {
                    let focus_handle = editor_input.read(cx).focus_handle().clone();
                    window.handle_input(
                        &focus_handle,
                        ElementInputHandler::new(prepaint_bounds, editor_input),
                        cx,
                    );
                }
            },
        )
        .size_full();

        let hover_popup = self.hover_popup.clone();
        let completion_popup = self.completion_popup.clone();
        let minimap = self.minimap.clone();
        let mut root = div()
            .relative()
            .w_full()
            .font_family(family)
            .text_size(font_size);
        root = match &self.mode {
            EditorMode::SingleLine => root.h(line_height),
            EditorMode::AutoHeight {
                min_lines,
                max_lines,
            } => {
                let capped = match max_lines {
                    Some(max) => document_rows.min(*max),
                    None => document_rows,
                };
                root.h(line_height * capped.max(*min_lines).max(1) as f32)
            },
            EditorMode::Full {} | EditorMode::Minimap { .. } => root.h_full(),
        };
        root = root.child(list).child(bounds_capture);
        if is_minimap {
            root = root.line_height(line_height);
        }
        if let Some(popup) = hover_popup {
            root = root.child(popup);
        }
        if let Some(popup) = completion_popup {
            root = root.child(popup);
        }
        if let Some(minimap) = minimap {
            root = root.child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .h_full()
                    .w(relative(MINIMAP_WIDTH_FRACTION))
                    .min_w(px(MINIMAP_MIN_WIDTH))
                    .child(minimap),
            );
        }
        if let EditorMode::Minimap { parent } = &self.mode {
            let parent = parent.clone();
            let total_lines = document_rows as f64;
            let drag_handle = cx.entity().downgrade();
            let dragging = self.minimap_drag.is_some();
            root = root.child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, cx| {
                        if dragging {
                            window.on_mouse_event::<MouseMoveEvent>({
                                let drag_handle = drag_handle.clone();
                                move |event, phase, _window, cx| {
                                    if phase == DispatchPhase::Bubble {
                                        if let Some(minimap) = drag_handle.upgrade() {
                                            minimap.update(cx, |minimap, cx| {
                                                minimap.minimap_thumb_drag_to(event.position, cx);
                                            });
                                        }
                                    }
                                }
                            });
                            window.on_mouse_event::<MouseUpEvent>({
                                let drag_handle = drag_handle.clone();
                                move |_event: &MouseUpEvent, phase, _window, cx| {
                                    if phase == DispatchPhase::Bubble {
                                        if let Some(minimap) = drag_handle.upgrade() {
                                            minimap.update(cx, |minimap, cx| {
                                                minimap.minimap_thumb_drag_end(cx);
                                            });
                                        }
                                    }
                                }
                            });
                        }

                        let Some(parent) = parent.upgrade() else {
                            return;
                        };
                        let parent = parent.read(cx);
                        let (Some(region), Some(cell)) =
                            (parent.text_region_bounds(), parent.cell_size())
                        else {
                            return;
                        };
                        let line_height = f32::from(cell.height) as f64;
                        if line_height <= 0.0 {
                            return;
                        }
                        let visible_lines = f32::from(region.size.height) as f64 / line_height;
                        let scroll_y = parent.scroll_manager().anchor().offset.y;
                        if let Some(thumb) =
                            minimap_thumb_bounds(bounds, total_lines, visible_lines, scroll_y)
                        {
                            let theme = cx.theme();
                            window.paint_quad(fill(thumb, theme.minimap_thumb));
                            window.paint_quad(outline(
                                thumb,
                                theme.minimap_thumb_border,
                                BorderStyle::Solid,
                            ));
                        }
                    },
                )
                .absolute()
                .size_full(),
            );
        }
        let root = if is_minimap {
            root.on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                    this.minimap_thumb_drag_start(event.position, cx);
                    cx.stop_propagation();
                }),
            )
        } else {
            root.on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.dispatch_click_at(event.position, window, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                if event.dragging() {
                    this.dispatch_drag_select_to(event.position, window, cx);
                } else {
                    this.schedule_hover_at(event.position, window, cx);
                }
            }))
        };
        root.on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
            this.handle_scroll_wheel(event, window, cx);
        }))
    }
}

/// Matches the GPUI `TextStyle::default` `line_height` of `phi()`
/// (golden ratio) applied when no `with_text_style` refinement on the
/// element tree overrides it. The editor's render path relies on that
/// default, so the cell-height measurement reproduces the constant
/// rather than threading a `TextStyle` through the paint callback.
const GPUI_DEFAULT_LINE_HEIGHT_RATIO: f32 = 1.618_034;

/// Font size and line height (in pixels) used when an editor renders in
/// [`EditorMode::Minimap`]. The reduced scale packs many source lines
/// into the narrow overview column; the line height is deliberately
/// tighter than the golden-ratio default so rows stack densely.
const MINIMAP_FONT_SIZE: f32 = 2.0;
const MINIMAP_LINE_HEIGHT: f32 = 2.5;

/// Upper bound on the rows a minimap paints. Beyond this the overview
/// stops growing so a very long buffer does not shape thousands of
/// tiny rows every frame.
const MAX_MINIMAP_LINES: usize = 200;

/// The minimap column occupies this fraction of the parent editor's
/// width, floored at [`MINIMAP_MIN_WIDTH`] pixels (roughly 20 columns
/// at [`MINIMAP_FONT_SIZE`]) so it stays visible in a narrow pane.
const MINIMAP_WIDTH_FRACTION: f32 = 0.15;
const MINIMAP_MIN_WIDTH: f32 = 24.0;

/// Duration of an eased programmatic-jump scroll animation.
const SCROLL_ANIMATION_DURATION: std::time::Duration = std::time::Duration::from_millis(150);
/// Wake interval of the scroll-animation task (~60 fps).
const SCROLL_ANIMATION_FRAME: std::time::Duration = std::time::Duration::from_millis(16);
/// Jumps shorter than this many display rows snap instantly; only larger
/// programmatic jumps are worth animating.
const MIN_ANIMATED_SCROLL_ROWS: f64 = 3.0;

/// Bounds of the minimap viewport thumb within `minimap_bounds`: the
/// overlay rectangle marking which slice of the document the parent
/// editor currently shows. Returns [`None`] when the whole document fits
/// the viewport (`total_lines <= visible_editor_lines`), where no thumb
/// is drawn.
///
/// The thumb height is `minimap_height * visible_editor_lines /
/// total_lines`; its top slides down the leftover track in proportion to
/// how far the editor has scrolled, so a fully scrolled editor pins the
/// thumb to the minimap's bottom edge.
fn minimap_thumb_bounds(
    minimap_bounds: Bounds<Pixels>,
    total_lines: f64,
    visible_editor_lines: f64,
    editor_scroll_y: f64,
) -> Option<Bounds<Pixels>> {
    if total_lines <= visible_editor_lines {
        return None;
    }

    let minimap_height = minimap_bounds.size.height;
    let thumb_height_ratio = (visible_editor_lines / total_lines).clamp(0.0, 1.0);
    let thumb_height = minimap_height * thumb_height_ratio as f32;

    let max_scroll = (total_lines - visible_editor_lines).max(0.0);
    let scroll_ratio = if max_scroll > 0.0 {
        (editor_scroll_y / max_scroll).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let thumb_y = minimap_bounds.origin.y + (minimap_height - thumb_height) * scroll_ratio as f32;

    Some(Bounds {
        origin: Point::new(minimap_bounds.origin.x, thumb_y),
        size: gpui_size(minimap_bounds.size.width, thumb_height),
    })
}

/// Wall-clock reference seeded into the blame strip's `now_seconds`
/// field so relative ages render against the user's current time.
/// Pre-1970 clocks fall back to 0 so render still produces a defined
/// (large negative-age folds to "now") output.
fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn editor_font(cx: &App) -> (SharedString, f32) {
    let (family, size) = match cx.try_global::<Settings>() {
        Some(settings) => (
            settings.resolved.editor_font_family.clone(),
            settings.resolved.editor_font_size,
        ),
        None => (None, None),
    };
    (
        family
            .map(SharedString::from)
            .unwrap_or_else(|| SharedString::from(DEFAULT_EDITOR_FONT_FAMILY)),
        size.unwrap_or(DEFAULT_EDITOR_FONT_SIZE),
    )
}

impl ItemView for Editor {
    fn tab_label(&self, _cx: &App) -> SharedString {
        self.file_path
            .as_deref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|s| SharedString::from(s.to_string()))
            .unwrap_or_else(|| SharedString::from("(scratch)"))
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.multi_buffer
            .read(cx)
            .as_singleton()
            .map(|b| b.read(cx).is_dirty())
            .unwrap_or(false)
    }

    fn save(&mut self, cx: &mut Context<'_, Self>) -> Task<Result<(), ItemError>> {
        if let Some(buffer) = self.multi_buffer.read(cx).as_singleton().cloned() {
            buffer.update(cx, |b, cx| b.save(cx));
        }
        Task::ready(Ok(()))
    }

    fn deserialize(_value: Value, _cx: &mut Context<'_, Self>) -> Result<Self, ItemError> {
        DeserializeSnafu {
            reason: "Editor deserialize requires workspace-persistence wiring \
                     that has not yet landed",
        }
        .fail()
    }

    fn item_kind(&self) -> crate::item::ItemKind {
        crate::item::ItemKind::Editor
    }

    fn serialize(&self, _cx: &App) -> Value {
        let file_path = self
            .file_path
            .as_deref()
            .and_then(|p| p.to_str())
            .map(String::from);
        serde_json::json!({ "file_path": file_path })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{buffer::Buffer, globals::ExecutorGlobal};
    use gpui::{AppContext, TestAppContext, VisualTestContext};
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
    };
    use stoat::buffer::BufferId;
    use stoat_scheduler::{Executor, TestScheduler};

    struct Recorder {
        _subscription: Subscription,
    }

    impl Recorder {
        fn install(
            cx: &mut TestAppContext,
            editor: &Entity<Editor>,
        ) -> (Entity<Recorder>, Arc<Mutex<Vec<EditorEvent>>>) {
            let events: Arc<Mutex<Vec<EditorEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let editor = editor.clone();
            let recorder = cx.update(|cx| {
                let sink = events.clone();
                cx.new(|cx| {
                    let subscription =
                        cx.subscribe(&editor, move |_, _, event: &EditorEvent, _| {
                            sink.lock().expect("recorder mutex").push(event.clone());
                        });
                    Recorder {
                        _subscription: subscription,
                    }
                })
            });
            (recorder, events)
        }
    }

    fn drain(events: &Arc<Mutex<Vec<EditorEvent>>>) -> Vec<EditorEvent> {
        std::mem::take(&mut *events.lock().expect("recorder mutex"))
    }

    fn test_executor() -> Executor {
        Executor::new(Arc::new(TestScheduler::new()))
    }

    fn new_editor(cx: &mut TestAppContext, text: &str) -> (Entity<Buffer>, Entity<Editor>) {
        let buffer = cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)));
        let executor = test_executor();
        let multi_buffer = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| MultiBuffer::singleton(buffer, cx)))
        };
        let display_map = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| DisplayMap::new(buffer, executor, cx)))
        };
        let diff_map = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| DiffMap::new(buffer, cx)))
        };
        let editor = cx.update(|cx| {
            cx.new(|cx| Editor::new(multi_buffer, display_map, diff_map, EditorMode::full(), cx))
        });
        (buffer, editor)
    }

    #[test]
    fn bump_hover_debounce_id_increments_and_records_latest() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello");

        let first = editor.update(&mut cx, |ed, _| ed.bump_hover_debounce_id());
        let second = editor.update(&mut cx, |ed, _| ed.bump_hover_debounce_id());

        assert_eq!(first, 1);
        assert_eq!(second, 2);
        assert_eq!(
            editor.read_with(&cx, |ed, _| ed.hover_debounce_id()),
            2,
            "hover_debounce_id must track the most recent bump",
        );
    }

    #[test]
    fn new_initializes_default_state() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello");

        editor.read_with(&cx, |ed, _| {
            assert_eq!(ed.scroll_row(), 0);
            assert_eq!(ed.selections().all_anchors().len(), 1);
            assert_eq!(ed.jumplist().entries(), &[] as &[usize]);
            assert_eq!(ed.jumplist().cursor(), 0);
        });
    }

    #[test]
    fn buffer_edit_re_emits_changed() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hello");
        let (_recorder, events) = Recorder::install(&mut cx, &editor);

        buffer.update(&mut cx, |b, cx| b.edit(5..5, " world", cx));
        cx.run_until_parked();

        let observed = drain(&events);
        assert!(
            observed.iter().all(|e| *e == EditorEvent::Changed),
            "unexpected event in {observed:?}",
        );
        assert!(
            !observed.is_empty(),
            "expected at least one Changed event from buffer edit",
        );
    }

    #[test]
    fn set_scroll_row_updates_and_emits_changed() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "x");
        let (_recorder, events) = Recorder::install(&mut cx, &editor);

        editor.update(&mut cx, |ed, cx| ed.set_scroll_row(7, cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), vec![EditorEvent::Changed]);
        editor.read_with(&cx, |ed, _| assert_eq!(ed.scroll_row(), 7));
    }

    #[test]
    fn set_scroll_row_same_value_is_noop() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "x");
        let (_recorder, events) = Recorder::install(&mut cx, &editor);

        editor.update(&mut cx, |ed, cx| ed.set_scroll_row(0, cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), Vec::<EditorEvent>::new());
    }

    #[test]
    fn scroll_manager_defaults_on_construction() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "x");

        editor.read_with(&cx, |ed, _| {
            let mgr = ed.scroll_manager();
            assert_eq!(mgr.anchor(), &scroll::ScrollAnchor::new());
            assert_eq!(mgr.ongoing().axis(), None);
            assert_eq!(mgr.visible_line_count(), None);
            assert_eq!(mgr.minimap_thumb_state(), None);
        });
    }

    #[test]
    fn scroll_manager_mut_lets_callers_update_visible_line_count() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "x");

        editor.update(&mut cx, |ed, _| {
            ed.scroll_manager_mut().set_visible_line_count(Some(24.5));
        });
        cx.run_until_parked();

        editor.read_with(&cx, |ed, _| {
            assert_eq!(ed.scroll_manager().visible_line_count(), Some(24.5));
        });
    }

    fn wheel_event(delta: gpui::ScrollDelta, alt: bool) -> ScrollWheelEvent {
        let modifiers = gpui::Modifiers {
            alt,
            ..gpui::Modifiers::default()
        };
        ScrollWheelEvent {
            position: Point::new(px(0.), px(0.)),
            delta,
            modifiers,
            touch_phase: gpui::TouchPhase::Moved,
        }
    }

    #[test]
    fn handle_scroll_wheel_no_op_without_cell_size() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let vcx = cx.add_empty_window();
        let (_buffer, editor) = new_editor_in_window(vcx, "a\nb\nc\nd\ne\nf");

        let before = editor.read_with(vcx, |ed, _| ed.scroll_row());
        editor.update_in(vcx, |ed, window, cx| {
            ed.handle_scroll_wheel(
                &wheel_event(gpui::ScrollDelta::Lines(Point::new(0., -3.)), false),
                window,
                cx,
            );
        });
        vcx.run_until_parked();

        assert_eq!(editor.read_with(vcx, |ed, _| ed.scroll_row()), before);
    }

    #[test]
    fn handle_scroll_wheel_advances_scroll_row() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let vcx = cx.add_empty_window();
        let (_buffer, editor) = new_editor_in_window(vcx, "a\nb\nc\nd\ne\nf");
        editor.update_in(vcx, |ed, _, cx| ed.set_cell_size(cell(8.0, 16.0), cx));
        vcx.run_until_parked();

        editor.update_in(vcx, |ed, window, cx| {
            ed.handle_scroll_wheel(
                &wheel_event(gpui::ScrollDelta::Lines(Point::new(0., -3.)), false),
                window,
                cx,
            );
        });
        vcx.run_until_parked();

        assert_eq!(editor.read_with(vcx, |ed, _| ed.scroll_row()), 3);
        assert_eq!(
            editor.read_with(vcx, |ed, _| ed.scroll_manager().anchor().offset.y),
            3.0,
        );
    }

    #[test]
    fn handle_scroll_wheel_coalesces_same_tick_events() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let vcx = cx.add_empty_window();
        let (_buffer, editor) = new_editor_in_window(
            vcx,
            &(0..20)
                .map(|i| format!("l{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        editor.update_in(vcx, |ed, _, cx| ed.set_cell_size(cell(8.0, 16.0), cx));
        vcx.run_until_parked();

        editor.update_in(vcx, |ed, window, cx| {
            ed.handle_scroll_wheel(
                &wheel_event(gpui::ScrollDelta::Lines(Point::new(0., -1.)), false),
                window,
                cx,
            );
            ed.handle_scroll_wheel(
                &wheel_event(gpui::ScrollDelta::Lines(Point::new(0., -2.)), false),
                window,
                cx,
            );
            ed.handle_scroll_wheel(
                &wheel_event(gpui::ScrollDelta::Lines(Point::new(0., -3.)), false),
                window,
                cx,
            );
        });
        vcx.run_until_parked();

        // Three same-sign deltas summing to -6; coalescing applies one
        // merged scroll instead of three intermediate steps.
        assert_eq!(editor.read_with(vcx, |ed, _| ed.scroll_row()), 6);
        assert_eq!(
            editor.read_with(vcx, |ed, _| ed.scroll_manager().anchor().offset.y),
            6.0,
        );
        assert!(
            editor.read_with(vcx, |ed, _| ed.pending_scroll_delta.is_none()),
            "pending delta must drain by the time we observe the scroll state",
        );
    }

    #[test]
    fn handle_scroll_wheel_pushes_pixel_offset_to_scroll_handle() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let vcx = cx.add_empty_window();
        let (_buffer, editor) = new_editor_in_window(vcx, "a\nb\nc\nd\ne\nf\ng\nh");
        editor.update_in(vcx, |ed, _, cx| ed.set_cell_size(cell(8.0, 16.0), cx));
        vcx.run_until_parked();

        editor.update_in(vcx, |ed, window, cx| {
            ed.handle_scroll_wheel(
                &wheel_event(
                    gpui::ScrollDelta::Pixels(Point::new(px(0.), px(-24.5))),
                    false,
                ),
                window,
                cx,
            );
        });
        vcx.run_until_parked();

        let offset = editor.read_with(vcx, |ed, _| {
            ed.scroll_handle().0.borrow().base_handle.offset()
        });
        assert_eq!(offset.y, px(-24.5));
        assert_eq!(offset.x, px(0.0));
    }

    fn editor_with_viewport(
        vcx: &mut VisualTestContext,
        text: &str,
    ) -> (Entity<Buffer>, Entity<Editor>) {
        let (buffer, editor) = new_editor_in_window(vcx, text);
        editor.update_in(vcx, |ed, _, cx| {
            ed.set_cell_size(cell(8.0, 16.0), cx);
            ed.set_text_region_bounds(
                Bounds {
                    origin: Point::new(px(0.0), px(0.0)),
                    size: gpui::size(px(160.0), px(320.0)),
                },
                cx,
            );
        });
        vcx.run_until_parked();
        (buffer, editor)
    }

    fn multiline_text(rows: usize) -> String {
        (0..rows)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn request_autoscroll_stores_pending_strategy() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let vcx = cx.add_empty_window();
        let (_buffer, editor) = new_editor_in_window(vcx, "a");

        editor.update_in(vcx, |ed, _, cx| {
            ed.request_autoscroll(AutoscrollStrategy::Center, cx);
        });

        let pending = editor.read_with(vcx, |ed, _| ed.scroll_manager().autoscroll_request());
        assert_eq!(pending, Some(AutoscrollStrategy::Center));
    }

    #[test]
    fn apply_pending_autoscroll_center_moves_scroll_position() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let vcx = cx.add_empty_window();
        let text = multiline_text(30);
        let (_buffer, editor) = editor_with_viewport(vcx, &text);
        editor.update_in(vcx, |ed, _, cx| {
            ed.set_cursor_at_grid(10, 0, cx);
            ed.request_autoscroll(AutoscrollStrategy::Center, cx);
            ed.apply_pending_autoscroll(cx);
        });
        vcx.run_until_parked();

        assert_eq!(
            editor.read_with(vcx, |ed, _| ed.scroll_manager().anchor().offset.y),
            1.0,
        );
        assert_eq!(editor.read_with(vcx, |ed, _| ed.scroll_row()), 1);
    }

    #[test]
    fn apply_pending_autoscroll_top_places_cursor_at_top() {
        let mut cx = TestAppContext::single();
        let scheduler = install_executor_global_returning_scheduler(&mut cx);
        let vcx = cx.add_empty_window();
        let text = multiline_text(30);
        let (_buffer, editor) = editor_with_viewport(vcx, &text);
        editor.update_in(vcx, |ed, _, cx| {
            ed.set_cursor_at_grid(15, 0, cx);
            ed.request_autoscroll(AutoscrollStrategy::Top, cx);
            ed.apply_pending_autoscroll(cx);
        });
        // A 10-row jump animates; drive the animation to completion.
        vcx.run_until_parked();
        advance(&scheduler, vcx, 300);

        assert_eq!(
            editor.read_with(vcx, |ed, _| ed.scroll_manager().anchor().offset.y),
            10.0,
        );
    }

    #[test]
    fn apply_pending_autoscroll_consumes_request() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let vcx = cx.add_empty_window();
        let text = multiline_text(30);
        let (_buffer, editor) = editor_with_viewport(vcx, &text);
        editor.update_in(vcx, |ed, _, cx| {
            ed.request_autoscroll(AutoscrollStrategy::Center, cx);
            ed.apply_pending_autoscroll(cx);
        });

        let pending = editor.read_with(vcx, |ed, _| ed.scroll_manager().autoscroll_request());
        assert_eq!(pending, None);
    }

    #[test]
    fn apply_pending_autoscroll_pushes_pixel_offset_to_scroll_handle() {
        let mut cx = TestAppContext::single();
        let scheduler = install_executor_global_returning_scheduler(&mut cx);
        let vcx = cx.add_empty_window();
        let text = multiline_text(30);
        let (_buffer, editor) = editor_with_viewport(vcx, &text);
        editor.update_in(vcx, |ed, _, cx| {
            ed.set_cursor_at_grid(15, 0, cx);
            ed.request_autoscroll(AutoscrollStrategy::Top, cx);
            ed.apply_pending_autoscroll(cx);
        });
        // A 10-row jump animates; drive it to completion before reading the
        // settled pixel offset.
        vcx.run_until_parked();
        advance(&scheduler, vcx, 300);

        let offset = editor.read_with(vcx, |ed, _| {
            ed.scroll_handle().0.borrow().base_handle.offset()
        });
        assert_eq!(offset.y, px(-160.0));
    }

    #[test]
    fn apply_pending_autoscroll_animates_large_jump() {
        let mut cx = TestAppContext::single();
        let scheduler = install_executor_global_returning_scheduler(&mut cx);
        let vcx = cx.add_empty_window();
        let text = multiline_text(1000);
        let (_buffer, editor) = editor_with_viewport(vcx, &text);
        editor.update_in(vcx, |ed, _, cx| {
            ed.set_cursor_at_grid(500, 0, cx);
            ed.request_autoscroll(AutoscrollStrategy::Top, cx);
            ed.apply_pending_autoscroll(cx);
        });

        // The jump animates rather than snapping: an animation is stored and
        // the position has not yet moved.
        editor.read_with(vcx, |ed, _| {
            assert!(ed.scroll_manager().animation().is_some());
            assert_eq!(ed.scroll_manager().anchor().offset.y, 0.0);
        });

        // Partway through, the position interpolates strictly between the
        // start and the target row 500.
        vcx.run_until_parked();
        advance(&scheduler, vcx, 75);
        let mid = editor.read_with(vcx, |ed, _| ed.scroll_manager().anchor().offset.y);
        assert!(mid > 0.0 && mid < 500.0, "midpoint y was {mid}");

        // After the duration elapses it settles on the target and clears.
        advance(&scheduler, vcx, 300);
        editor.read_with(vcx, |ed, _| {
            assert_eq!(ed.scroll_manager().anchor().offset.y, 500.0);
            assert!(ed.scroll_manager().animation().is_none());
        });
    }

    #[test]
    fn apply_pending_autoscroll_small_jump_does_not_animate() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let vcx = cx.add_empty_window();
        let text = multiline_text(30);
        let (_buffer, editor) = editor_with_viewport(vcx, &text);
        editor.update_in(vcx, |ed, _, cx| {
            ed.set_cursor_at_grid(2, 0, cx);
            ed.request_autoscroll(AutoscrollStrategy::Top, cx);
            ed.apply_pending_autoscroll(cx);
        });

        // A 2-row jump (< 3) snaps instantly with no animation stored.
        editor.read_with(vcx, |ed, _| {
            assert!(ed.scroll_manager().animation().is_none());
            assert_eq!(ed.scroll_manager().anchor().offset.y, 2.0);
        });
    }

    #[test]
    fn apply_pending_autoscroll_noop_when_cell_size_unset() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let vcx = cx.add_empty_window();
        let (_buffer, editor) = new_editor_in_window(vcx, "a\nb\nc");
        editor.update_in(vcx, |ed, _, cx| {
            ed.request_autoscroll(AutoscrollStrategy::Top, cx);
            ed.apply_pending_autoscroll(cx);
        });

        let y = editor.read_with(vcx, |ed, _| ed.scroll_manager().anchor().offset.y);
        assert_eq!(y, 0.0);
    }

    #[test]
    fn handle_scroll_wheel_pixel_offset_uses_fractional_row_position() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let vcx = cx.add_empty_window();
        let (_buffer, editor) = new_editor_in_window(vcx, "a\nb\nc\nd\ne\nf");
        editor.update_in(vcx, |ed, _, cx| ed.set_cell_size(cell(8.0, 16.0), cx));
        vcx.run_until_parked();

        editor.update_in(vcx, |ed, window, cx| {
            ed.handle_scroll_wheel(
                &wheel_event(gpui::ScrollDelta::Lines(Point::new(0., -1.)), false),
                window,
                cx,
            );
        });
        vcx.run_until_parked();

        let offset = editor.read_with(vcx, |ed, _| {
            ed.scroll_handle().0.borrow().base_handle.offset()
        });
        assert_eq!(offset.y, px(-16.0));
    }

    fn new_editor_in_window(
        vcx: &mut VisualTestContext,
        text: &str,
    ) -> (Entity<Buffer>, Entity<Editor>) {
        let buffer = vcx.update(|_, cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)));
        let executor = test_executor();
        let multi_buffer = {
            let buffer = buffer.clone();
            vcx.update(|_, cx| cx.new(|cx| MultiBuffer::singleton(buffer, cx)))
        };
        let display_map = {
            let buffer = buffer.clone();
            vcx.update(|_, cx| cx.new(|cx| DisplayMap::new(buffer, executor, cx)))
        };
        let diff_map = {
            let buffer = buffer.clone();
            vcx.update(|_, cx| cx.new(|cx| DiffMap::new(buffer, cx)))
        };
        let editor = vcx.update(|_, cx| {
            cx.new(|cx| Editor::new(multi_buffer, display_map, diff_map, EditorMode::full(), cx))
        });
        (buffer, editor)
    }

    #[test]
    fn accessors_return_stored_entities() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abc");

        let (mb_id, dm_id, diff_id) = editor.read_with(&cx, |ed, _| {
            (
                ed.multi_buffer().entity_id(),
                ed.display_map().entity_id(),
                ed.diff_map().entity_id(),
            )
        });
        assert_ne!(mb_id, dm_id);
        assert_ne!(mb_id, diff_id);
        assert_ne!(dm_id, diff_id);
    }

    fn cursor_offsets(editor: &Entity<Editor>, cx: &mut TestAppContext) -> Vec<usize> {
        editor.update(cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            ed.selections()
                .all_anchors()
                .iter()
                .map(|s| snapshot.resolve_anchor(&s.start))
                .collect()
        })
    }

    fn seed_cursors(editor: &Entity<Editor>, cx: &mut TestAppContext, offsets: &[usize]) {
        let offsets = offsets.to_vec();
        editor.update(cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let cursors: Vec<Selection<Anchor>> = offsets
                .iter()
                .enumerate()
                .map(|(idx, off)| {
                    let anchor = snapshot.anchor_at(*off, Bias::Left);
                    Selection {
                        id: 100 + idx,
                        start: anchor,
                        end: anchor,
                        reversed: false,
                        goal: SelectionGoal::None,
                    }
                })
                .collect();
            ed.selections_mut().replace_with(cursors, &snapshot);
        });
    }

    #[test]
    fn apply_text_to_all_cursors_inserts_at_default_cursor() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hello");

        editor.update(&mut cx, |ed, cx| ed.apply_text_to_all_cursors("X", cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "Xhello");
        assert_eq!(cursor_offsets(&editor, &mut cx), vec![1]);
    }

    #[test]
    fn apply_text_to_all_cursors_replaces_range_selection() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hello");
        editor.update(&mut cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let start = snapshot.anchor_at(0, Bias::Left);
            let end = snapshot.anchor_at(3, Bias::Left);
            let sel = Selection {
                id: 200,
                start,
                end,
                reversed: false,
                goal: SelectionGoal::None,
            };
            ed.selections_mut().replace_with(vec![sel], &snapshot);
        });

        editor.update(&mut cx, |ed, cx| ed.apply_text_to_all_cursors("Y", cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "Ylo");
        assert_eq!(cursor_offsets(&editor, &mut cx), vec![1]);
    }

    #[test]
    fn apply_text_to_all_cursors_inserts_at_each_of_multiple_cursors() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hello");
        seed_cursors(&editor, &mut cx, &[1, 3]);

        editor.update(&mut cx, |ed, cx| ed.apply_text_to_all_cursors("X", cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "hXelXlo");
        assert_eq!(cursor_offsets(&editor, &mut cx), vec![2, 5]);
    }

    #[test]
    fn apply_text_to_all_cursors_emits_changed() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello");
        let (_recorder, events) = Recorder::install(&mut cx, &editor);

        editor.update(&mut cx, |ed, cx| ed.apply_text_to_all_cursors("Z", cx));
        cx.run_until_parked();

        let observed = drain(&events);
        assert!(
            observed.iter().all(|e| *e == EditorEvent::Changed),
            "unexpected event in {observed:?}",
        );
        assert!(!observed.is_empty(), "expected at least one Changed event");
    }

    #[test]
    fn transform_selections_text_uppercases_selection() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "foo bar");
        editor.update(&mut cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let sel = Selection {
                id: 500,
                start: snapshot.anchor_at(0, Bias::Left),
                end: snapshot.anchor_at(7, Bias::Right),
                reversed: false,
                goal: SelectionGoal::None,
            };
            ed.selections_mut().replace_with(vec![sel], &snapshot);
        });

        editor.update(&mut cx, |ed, cx| {
            ed.transform_selections_text(|s: &str| s.to_uppercase(), cx)
        });
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "FOO BAR");
        let ranges = editor.read_with(&cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            ed.selections()
                .all_anchors()
                .iter()
                .map(|s| {
                    (
                        snapshot.resolve_anchor(&s.start),
                        snapshot.resolve_anchor(&s.end),
                    )
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(ranges, vec![(0, 7)]);
    }

    #[test]
    fn transform_selections_text_toggles_case_per_char() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "Foo Bar");
        editor.update(&mut cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let sel = Selection {
                id: 501,
                start: snapshot.anchor_at(0, Bias::Left),
                end: snapshot.anchor_at(7, Bias::Right),
                reversed: false,
                goal: SelectionGoal::None,
            };
            ed.selections_mut().replace_with(vec![sel], &snapshot);
        });

        let toggle = |s: &str| -> String {
            s.chars()
                .flat_map(|c| {
                    if c.is_lowercase() {
                        c.to_uppercase().collect::<Vec<_>>()
                    } else if c.is_uppercase() {
                        c.to_lowercase().collect::<Vec<_>>()
                    } else {
                        vec![c]
                    }
                })
                .collect()
        };
        editor.update(&mut cx, |ed, cx| ed.transform_selections_text(toggle, cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "fOO bAR");
    }

    #[test]
    fn transform_selections_text_collapsed_cursor_is_noop() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "abc");

        editor.update(&mut cx, |ed, cx| {
            ed.transform_selections_text(|s: &str| s.to_uppercase(), cx)
        });
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "abc");
    }

    #[test]
    fn replace_char_in_selections_replaces_each_char() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "abcdef");
        editor.update(&mut cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let sel = Selection {
                id: 300,
                start: snapshot.anchor_at(0, Bias::Left),
                end: snapshot.anchor_at(3, Bias::Right),
                reversed: false,
                goal: SelectionGoal::None,
            };
            ed.selections_mut().replace_with(vec![sel], &snapshot);
        });

        editor.update(&mut cx, |ed, cx| ed.replace_char_in_selections('X', cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "XXXdef");
        let ranges = editor.read_with(&cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            ed.selections()
                .all_anchors()
                .iter()
                .map(|s| {
                    (
                        snapshot.resolve_anchor(&s.start),
                        snapshot.resolve_anchor(&s.end),
                    )
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(ranges, vec![(0, 3)]);
    }

    #[test]
    fn replace_char_in_selections_collapsed_cursor_is_noop() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "abc");

        editor.update(&mut cx, |ed, cx| ed.replace_char_in_selections('X', cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "abc");
        assert_eq!(cursor_offsets(&editor, &mut cx), vec![0]);
    }

    #[test]
    fn replace_char_in_selections_multibyte_grows_buffer() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "abc");
        editor.update(&mut cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let sel = Selection {
                id: 301,
                start: snapshot.anchor_at(0, Bias::Left),
                end: snapshot.anchor_at(2, Bias::Right),
                reversed: false,
                goal: SelectionGoal::None,
            };
            ed.selections_mut().replace_with(vec![sel], &snapshot);
        });

        editor.update(&mut cx, |ed, cx| ed.replace_char_in_selections('é', cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "ééc");
    }

    #[test]
    fn open_line_below_inserts_blank_line_after_cursor_row() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "line0\nline1\n");
        seed_cursors(&editor, &mut cx, &[2]);

        editor.update(&mut cx, |ed, cx| ed.open_line(OpenLineDir::Below, cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "line0\n\nline1\n");
        assert_eq!(cursor_offsets(&editor, &mut cx), vec![6]);
    }

    #[test]
    fn open_line_above_inserts_blank_line_before_cursor_row() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "line0\nline1\n");
        seed_cursors(&editor, &mut cx, &[8]);

        editor.update(&mut cx, |ed, cx| ed.open_line(OpenLineDir::Above, cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "line0\n\nline1\n");
        assert_eq!(cursor_offsets(&editor, &mut cx), vec![6]);
    }

    #[test]
    fn open_line_below_dedupes_cursors_on_same_row() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hello\n");
        seed_cursors(&editor, &mut cx, &[1, 3]);

        editor.update(&mut cx, |ed, cx| ed.open_line(OpenLineDir::Below, cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "hello\n\n");
        assert_eq!(cursor_offsets(&editor, &mut cx), vec![6]);
    }

    fn install_executor_global(cx: &mut TestAppContext) {
        let executor = test_executor();
        cx.update(|cx| cx.set_global(ExecutorGlobal(executor)));
    }

    /// Install the executor global and return the [`TestScheduler`] so the
    /// test can drive its clock (`advance_clock`) to step scroll animations.
    fn install_executor_global_returning_scheduler(cx: &mut TestAppContext) -> Arc<TestScheduler> {
        let scheduler = Arc::new(TestScheduler::new());
        let executor = Executor::new(scheduler.clone());
        cx.update(|cx| cx.set_global(ExecutorGlobal(executor)));
        scheduler
    }

    /// Advance both the gpui and stoat clocks by `ms` and pump the queues,
    /// stepping any in-flight scroll-animation task. The task must already
    /// have been polled once (via a prior `run_until_parked`) so its first
    /// timer is registered.
    fn advance(scheduler: &Arc<TestScheduler>, vcx: &mut VisualTestContext, ms: u64) {
        let step = std::time::Duration::from_millis(ms);
        vcx.executor().advance_clock(step);
        scheduler.advance_clock(step);
        vcx.run_until_parked();
    }

    fn assert_inline_editor_state(
        editor: &Entity<Editor>,
        vcx: &mut VisualTestContext,
        expected_mode: &EditorMode,
    ) {
        editor.read_with(vcx, |ed, cx| {
            assert_eq!(ed.mode(), expected_mode);
            let mb = ed.multi_buffer().read(cx);
            assert!(mb.is_singleton(), "expected singleton multi-buffer");
            let buffer = mb.as_singleton().expect("singleton buffer");
            assert_eq!(buffer.read(cx).text(), "");
        });
    }

    #[test]
    fn single_line_constructs_empty_singleton_in_single_line_mode() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let vcx = cx.add_empty_window();
        let editor = vcx.update(|window, cx| cx.new(|cx| Editor::single_line(window, cx)));

        assert_inline_editor_state(&editor, vcx, &EditorMode::SingleLine);
    }

    #[test]
    fn auto_height_constructs_with_min_and_max_bounds() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let vcx = cx.add_empty_window();
        let editor = vcx.update(|window, cx| cx.new(|cx| Editor::auto_height(2, 8, window, cx)));

        assert_inline_editor_state(
            &editor,
            vcx,
            &EditorMode::AutoHeight {
                min_lines: 2,
                max_lines: Some(8),
            },
        );
    }

    #[test]
    fn auto_height_unbounded_constructs_with_no_max() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let vcx = cx.add_empty_window();
        let editor =
            vcx.update(|window, cx| cx.new(|cx| Editor::auto_height_unbounded(3, window, cx)));

        assert_inline_editor_state(
            &editor,
            vcx,
            &EditorMode::AutoHeight {
                min_lines: 3,
                max_lines: None,
            },
        );
    }

    #[test]
    fn set_cursor_at_grid_places_cursor_at_position() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello world");

        editor.update(&mut cx, |ed, cx| ed.set_cursor_at_grid(0, 6, cx));
        cx.run_until_parked();

        assert_eq!(cursor_offsets(&editor, &mut cx), vec![6]);
    }

    #[test]
    fn set_cursor_at_grid_on_multiline_uses_row_col() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "line0\nline1\nline2");

        editor.update(&mut cx, |ed, cx| ed.set_cursor_at_grid(1, 2, cx));
        cx.run_until_parked();

        assert_eq!(cursor_offsets(&editor, &mut cx), vec![8]);
    }

    #[test]
    fn set_cursor_at_grid_clamps_out_of_bounds_row() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "ab\ncd");

        editor.update(&mut cx, |ed, cx| ed.set_cursor_at_grid(99, 0, cx));
        cx.run_until_parked();

        assert_eq!(cursor_offsets(&editor, &mut cx), vec![3]);
    }

    #[test]
    fn set_cursor_at_grid_clamps_out_of_bounds_col() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "ab\ncdef");

        editor.update(&mut cx, |ed, cx| ed.set_cursor_at_grid(0, 99, cx));
        cx.run_until_parked();

        assert_eq!(cursor_offsets(&editor, &mut cx), vec![2]);
    }

    #[test]
    fn set_cursor_at_grid_replaces_existing_selections() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello world");
        seed_cursors(&editor, &mut cx, &[1, 3, 5]);

        editor.update(&mut cx, |ed, cx| ed.set_cursor_at_grid(0, 7, cx));
        cx.run_until_parked();

        assert_eq!(cursor_offsets(&editor, &mut cx), vec![7]);
    }

    #[test]
    fn set_cursor_at_grid_emits_changed() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello");
        let (_recorder, events) = Recorder::install(&mut cx, &editor);

        editor.update(&mut cx, |ed, cx| ed.set_cursor_at_grid(0, 2, cx));
        cx.run_until_parked();

        let observed = drain(&events);
        assert!(
            observed.iter().all(|e| *e == EditorEvent::Changed),
            "unexpected event in {observed:?}",
        );
        assert!(!observed.is_empty(), "expected at least one Changed event");
    }

    #[test]
    fn set_cursor_at_buffer_row_places_cursor_at_row_start() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "line0\nline1\nline2");

        editor.update(&mut cx, |ed, cx| ed.set_cursor_at_buffer_row(1, cx));
        cx.run_until_parked();

        assert_eq!(cursor_offsets(&editor, &mut cx), vec![6]);
    }

    #[test]
    fn set_cursor_at_buffer_row_clamps_past_last_row() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "ab\ncd");

        editor.update(&mut cx, |ed, cx| ed.set_cursor_at_buffer_row(99, cx));
        cx.run_until_parked();

        assert_eq!(cursor_offsets(&editor, &mut cx), vec![5]);
    }

    #[test]
    fn set_cursor_at_buffer_row_replaces_existing_selections() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "line0\nline1\nline2");
        seed_cursors(&editor, &mut cx, &[1, 3, 5]);

        editor.update(&mut cx, |ed, cx| ed.set_cursor_at_buffer_row(2, cx));
        cx.run_until_parked();

        assert_eq!(cursor_offsets(&editor, &mut cx), vec![12]);
    }

    fn selection_offsets(editor: &Entity<Editor>, cx: &mut TestAppContext) -> Vec<(usize, usize)> {
        editor.update(cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            ed.selections()
                .all_anchors()
                .iter()
                .map(|s| {
                    (
                        snapshot.resolve_anchor(&s.start),
                        snapshot.resolve_anchor(&s.end),
                    )
                })
                .collect()
        })
    }

    #[test]
    fn extend_primary_selection_to_grid_extends_head() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello world");
        seed_cursors(&editor, &mut cx, &[0]);

        editor.update(&mut cx, |ed, cx| {
            ed.extend_primary_selection_to_grid(0, 5, cx)
        });
        cx.run_until_parked();

        assert_eq!(selection_offsets(&editor, &mut cx), vec![(0, 5)]);
    }

    #[test]
    fn extend_primary_selection_to_grid_preserves_anchor() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello world");
        editor.update(&mut cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let sel = Selection {
                id: 1,
                start: snapshot.anchor_at(2, Bias::Left),
                end: snapshot.anchor_at(2, Bias::Left),
                reversed: false,
                goal: SelectionGoal::None,
            };
            ed.selections_mut().replace_with(vec![sel], &snapshot);
        });

        editor.update(&mut cx, |ed, cx| {
            ed.extend_primary_selection_to_grid(0, 8, cx)
        });
        cx.run_until_parked();

        assert_eq!(selection_offsets(&editor, &mut cx), vec![(2, 8)]);
    }

    #[test]
    fn extend_primary_selection_to_grid_marks_reversed_when_head_precedes_anchor() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello world");
        seed_cursors(&editor, &mut cx, &[8]);

        editor.update(&mut cx, |ed, cx| {
            ed.extend_primary_selection_to_grid(0, 2, cx)
        });
        cx.run_until_parked();

        let reversed = editor.update(&mut cx, |ed, _| {
            ed.selections()
                .all_anchors()
                .first()
                .map(|s| s.reversed)
                .unwrap_or(false)
        });
        assert!(reversed, "head before anchor should mark reversed");
        assert_eq!(selection_offsets(&editor, &mut cx), vec![(8, 2)]);
    }

    #[test]
    fn extend_primary_selection_to_grid_clamps_out_of_bounds_row() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "ab\ncd");
        seed_cursors(&editor, &mut cx, &[0]);

        editor.update(&mut cx, |ed, cx| {
            ed.extend_primary_selection_to_grid(99, 0, cx)
        });
        cx.run_until_parked();

        assert_eq!(selection_offsets(&editor, &mut cx), vec![(0, 3)]);
    }

    #[test]
    fn extend_primary_selection_to_grid_emits_changed() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello");
        seed_cursors(&editor, &mut cx, &[0]);
        let (_recorder, events) = Recorder::install(&mut cx, &editor);

        editor.update(&mut cx, |ed, cx| {
            ed.extend_primary_selection_to_grid(0, 3, cx)
        });
        cx.run_until_parked();

        let observed = drain(&events);
        assert!(
            observed.iter().all(|e| *e == EditorEvent::Changed),
            "unexpected event in {observed:?}",
        );
        assert!(!observed.is_empty(), "expected at least one Changed event");
    }

    fn cell(width: f32, height: f32) -> Size<Pixels> {
        gpui::size(px(width), px(height))
    }

    #[test]
    fn cell_size_defaults_to_none() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello");

        assert_eq!(editor.read_with(&cx, |ed, _| ed.cell_size()), None);
    }

    #[test]
    fn set_cell_size_stores_and_emits_changed() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "x");
        let (_recorder, events) = Recorder::install(&mut cx, &editor);

        editor.update(&mut cx, |ed, cx| ed.set_cell_size(cell(7.0, 14.0), cx));
        cx.run_until_parked();

        assert_eq!(
            editor.read_with(&cx, |ed, _| ed.cell_size()),
            Some(cell(7.0, 14.0)),
        );
        assert_eq!(drain(&events), vec![EditorEvent::Changed]);
    }

    #[test]
    fn set_cell_size_idempotent_no_event() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "x");
        editor.update(&mut cx, |ed, cx| ed.set_cell_size(cell(7.0, 14.0), cx));
        cx.run_until_parked();
        let (_recorder, events) = Recorder::install(&mut cx, &editor);

        editor.update(&mut cx, |ed, cx| ed.set_cell_size(cell(7.0, 14.0), cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), Vec::<EditorEvent>::new());
    }

    fn bounds_at(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds {
            origin: Point::new(px(x), px(y)),
            size: gpui::size(px(w), px(h)),
        }
    }

    #[test]
    fn workspace_defaults_to_none_and_setter_stores() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "x");

        assert!(editor
            .read_with(&cx, |ed, _| ed.workspace().cloned())
            .is_none());

        let workspace = cx.update(|cx| {
            cx.new(|cx| crate::workspace::Workspace::new("test", PathBuf::from("/tmp/repo"), cx))
        });
        editor.update(&mut cx, |ed, _| {
            ed.set_workspace(Some(workspace.downgrade()))
        });

        let stored_id = editor
            .read_with(&cx, |ed, _| ed.workspace().and_then(|w| w.upgrade()))
            .map(|w| w.entity_id());
        assert_eq!(stored_id, Some(workspace.entity_id()));
    }

    #[test]
    fn text_region_bounds_defaults_to_none() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "x");

        assert_eq!(editor.read_with(&cx, |ed, _| ed.text_region_bounds()), None);
    }

    #[test]
    fn set_text_region_bounds_stores_and_emits_changed() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "x");
        let (_recorder, events) = Recorder::install(&mut cx, &editor);
        let b = bounds_at(10.0, 20.0, 200.0, 100.0);

        editor.update(&mut cx, |ed, cx| ed.set_text_region_bounds(b, cx));
        cx.run_until_parked();

        assert_eq!(
            editor.read_with(&cx, |ed, _| ed.text_region_bounds()),
            Some(b)
        );
        assert_eq!(drain(&events), vec![EditorEvent::Changed]);
    }

    #[test]
    fn set_text_region_bounds_idempotent_no_event() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "x");
        let b = bounds_at(10.0, 20.0, 200.0, 100.0);
        editor.update(&mut cx, |ed, cx| ed.set_text_region_bounds(b, cx));
        cx.run_until_parked();
        let (_recorder, events) = Recorder::install(&mut cx, &editor);

        editor.update(&mut cx, |ed, cx| ed.set_text_region_bounds(b, cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), Vec::<EditorEvent>::new());
    }

    #[test]
    fn hover_position_defaults_to_none() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "x");

        assert_eq!(editor.read_with(&cx, |ed, _| ed.hover_position()), None);
    }

    #[test]
    fn set_hover_position_stores_and_emits_changed() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "x");
        let (_recorder, events) = Recorder::install(&mut cx, &editor);

        editor.update(&mut cx, |ed, cx| ed.set_hover_position(Some((4, 9)), cx));
        cx.run_until_parked();

        assert_eq!(
            editor.read_with(&cx, |ed, _| ed.hover_position()),
            Some((4, 9))
        );
        assert_eq!(drain(&events), vec![EditorEvent::Changed]);
    }

    #[test]
    fn set_hover_position_idempotent_no_event() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "x");
        editor.update(&mut cx, |ed, cx| ed.set_hover_position(Some((1, 2)), cx));
        cx.run_until_parked();
        let (_recorder, events) = Recorder::install(&mut cx, &editor);

        editor.update(&mut cx, |ed, cx| ed.set_hover_position(Some((1, 2)), cx));
        cx.run_until_parked();

        assert_eq!(drain(&events), Vec::<EditorEvent>::new());
    }

    #[test]
    fn pixel_bounds_for_utf16_offset_returns_none_without_cell_size() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello");

        let result = editor.update(&mut cx, |ed, cx| {
            ed.pixel_bounds_for_utf16_offset(2, Point::new(px(0.0), px(0.0)), cx)
        });
        assert_eq!(result, None);
    }

    #[test]
    fn pixel_bounds_for_utf16_offset_positions_at_offset_zero() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello");
        editor.update(&mut cx, |ed, cx| ed.set_cell_size(cell(8.0, 16.0), cx));

        let result = editor.update(&mut cx, |ed, cx| {
            ed.pixel_bounds_for_utf16_offset(0, Point::new(px(10.0), px(20.0)), cx)
        });
        assert_eq!(
            result,
            Some(Bounds {
                origin: Point::new(px(10.0), px(20.0)),
                size: cell(8.0, 16.0),
            }),
        );
    }

    #[test]
    fn pixel_bounds_for_utf16_offset_uses_display_row_col() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abc\ndef");
        editor.update(&mut cx, |ed, cx| ed.set_cell_size(cell(8.0, 16.0), cx));

        let result = editor.update(&mut cx, |ed, cx| {
            ed.pixel_bounds_for_utf16_offset(5, Point::new(px(0.0), px(0.0)), cx)
        });
        assert_eq!(
            result,
            Some(Bounds {
                origin: Point::new(px(8.0), px(16.0)),
                size: cell(8.0, 16.0),
            }),
        );
    }

    #[test]
    fn pixel_bounds_for_utf16_offset_handles_utf16_surrogate_pair() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "\u{1F600}x");
        editor.update(&mut cx, |ed, cx| ed.set_cell_size(cell(8.0, 16.0), cx));

        let result = editor.update(&mut cx, |ed, cx| {
            ed.pixel_bounds_for_utf16_offset(2, Point::new(px(0.0), px(0.0)), cx)
        });
        assert_eq!(
            result,
            Some(Bounds {
                origin: Point::new(px(16.0), px(0.0)),
                size: cell(8.0, 16.0),
            }),
        );
    }

    #[test]
    fn render_visible_rows_clamps_beyond_buffer() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "a\nb\nc");

        let count = editor.update(&mut cx, |ed, cx| ed.render_visible_rows(0..10, cx).len());
        assert_eq!(count, 3);
    }

    #[test]
    fn render_visible_rows_returns_zero_for_empty_range() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "a\nb");

        let count = editor.update(&mut cx, |ed, cx| ed.render_visible_rows(1..1, cx).len());
        assert_eq!(count, 0);
    }

    #[test]
    fn render_does_not_panic_on_empty_buffer() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let vcx = cx.add_empty_window();
        let editor = vcx.update(|window, cx| cx.new(|cx| Editor::single_line(window, cx)));

        let built = editor.update_in(vcx, |ed, window, cx| {
            let _element = ed.render(window, cx).into_any_element();
            true
        });
        assert!(built);
    }

    #[test]
    fn render_does_not_panic_with_multiline_buffer() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "alpha\nbeta\ngamma");
        let vcx = cx.add_empty_window();

        let built = editor.update_in(vcx, |ed, window, cx| {
            let _element = ed.render(window, cx).into_any_element();
            true
        });
        assert!(built);
    }

    #[test]
    fn minimap_render_skips_search_highlight_overlay() {
        use crate::editor::search::{SearchDirection, SearchState};
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "alpha\nbeta\nalpha");
        let vcx = cx.add_empty_window();
        editor.update(vcx, |ed, cx| {
            ed.set_search_state(
                Some(SearchState::new(
                    "alpha".to_string(),
                    SearchDirection::Forward,
                )),
                cx,
            );
            ed.set_minimap_visible(true, cx);
        });
        let minimap = editor
            .read_with(vcx, |ed, _| ed.minimap().cloned())
            .expect("minimap child constructed");

        let search_color = vcx.read(|cx| cx.theme().search_match);
        let bg_colors: Vec<Option<gpui::Hsla>> = minimap.update(vcx, |mm, cx| {
            let rows = mm.render_visible_rows(0..3, cx);
            let _ = rows;
            // Re-render to also inspect the produced rows directly via
            // the underlying overlay pipeline -- this assertion focuses
            // on the per-row runs the minimap *would* paint, ignoring
            // the wrapping Div elements that render_visible_rows
            // returns.
            let display_snapshot = mm.display_map.update(cx, |dm, _| dm.snapshot());
            let mut rows = render::build_rendered_rows(&display_snapshot, 0..3);
            let byte_maps = render::build_row_byte_maps(&rows, &display_snapshot, 0..3);
            // Apply only the syntax overlay (the minimap keeps this)
            // to mirror what render_visible_rows does for the minimap;
            // search overlay must NOT run.
            if let Some(buffer) = mm.multi_buffer.read(cx).as_singleton().cloned() {
                if let Some(syntax) = buffer.read(cx).syntax_map().map(|m| m.snapshot().clone()) {
                    let theme = cx
                        .try_global::<theme::Theme>()
                        .map(|t| t.0.clone())
                        .unwrap_or_else(stoat::theme::Theme::empty);
                    let styles = stoat::display_map::syntax_theme::SyntaxStyles::from_theme(&theme);
                    render::apply_syntax_overlay(
                        &mut rows,
                        &byte_maps,
                        &display_snapshot,
                        0..3,
                        &syntax,
                        &styles,
                    );
                }
            }
            rows.into_iter()
                .flat_map(|r| r.runs.into_iter().map(|(_, s)| s.background_color))
                .collect()
        });
        assert!(
            !bg_colors.contains(&Some(search_color)),
            "minimap must not paint search_match backgrounds (found {bg_colors:?})",
        );
    }

    #[test]
    fn render_with_minimap_visible_does_not_panic() {
        let mut cx = TestAppContext::single();
        let text = (0..300)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (_buffer, editor) = new_editor(&mut cx, &text);
        let vcx = cx.add_empty_window();
        editor.update(vcx, |ed, cx| ed.set_minimap_visible(true, cx));
        let minimap = editor
            .read_with(vcx, |ed, _| ed.minimap().cloned())
            .expect("minimap child constructed");

        let parent_built = editor.update_in(vcx, |ed, window, cx| {
            ed.render(window, cx).into_any_element();
            true
        });
        let minimap_built = minimap.update_in(vcx, |mm, window, cx| {
            mm.render(window, cx).into_any_element();
            true
        });
        assert!(parent_built && minimap_built);
    }

    #[test]
    fn minimap_thumb_bounds_none_when_document_fits() {
        let minimap = Bounds {
            origin: Point::new(px(0.0), px(0.0)),
            size: gpui_size(px(20.0), px(200.0)),
        };
        assert_eq!(minimap_thumb_bounds(minimap, 40.0, 50.0, 0.0), None);
    }

    #[test]
    fn minimap_thumb_bounds_half_scroll_centers_thumb() {
        let minimap = Bounds {
            origin: Point::new(px(0.0), px(0.0)),
            size: gpui_size(px(20.0), px(200.0)),
        };
        let thumb = minimap_thumb_bounds(minimap, 500.0, 50.0, 225.0)
            .expect("thumb present when document exceeds viewport");

        assert_eq!(thumb.size, gpui_size(px(20.0), px(20.0)));
        assert_eq!(thumb.origin, Point::new(px(0.0), px(90.0)));
    }

    /// Parent editor (`editor_with_viewport`: 320px / 16px = 20 visible
    /// lines) over a 500-line buffer, with a visible minimap whose column
    /// is 200px tall. The thumb therefore spans `200 * 20/500 = 8px` at
    /// scroll 0, and the minimap-to-document ratio is `200/500 = 0.4`
    /// px/line.
    fn minimap_drag_fixture(vcx: &mut VisualTestContext) -> (Entity<Editor>, Entity<Editor>) {
        let text = multiline_text(500);
        let (_buffer, editor) = editor_with_viewport(vcx, &text);
        editor.update(vcx, |ed, cx| ed.set_minimap_visible(true, cx));
        let minimap = editor
            .read_with(vcx, |ed, _| ed.minimap().cloned())
            .expect("minimap child constructed");
        minimap.update(vcx, |mm, cx| {
            mm.set_text_region_bounds(
                Bounds {
                    origin: Point::new(px(0.0), px(0.0)),
                    size: gpui_size(px(20.0), px(200.0)),
                },
                cx,
            );
        });
        vcx.run_until_parked();
        (editor, minimap)
    }

    #[test]
    fn minimap_thumb_drag_scrolls_parent_by_delta_over_ratio() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let vcx = cx.add_empty_window();
        let (editor, minimap) = minimap_drag_fixture(vcx);

        minimap.update(vcx, |mm, cx| {
            mm.minimap_thumb_drag_start(Point::new(px(10.0), px(4.0)), cx);
            mm.minimap_thumb_drag_to(Point::new(px(10.0), px(104.0)), cx);
        });

        assert_eq!(editor.read_with(vcx, |ed, _| ed.scroll_row()), 250);
        assert_eq!(
            editor.read_with(vcx, |ed, _| ed.scroll_manager().minimap_thumb_state()),
            Some(scroll::ScrollbarThumbState::Dragging),
        );
    }

    #[test]
    fn minimap_thumb_drag_start_ignores_click_off_thumb() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let vcx = cx.add_empty_window();
        let (editor, minimap) = minimap_drag_fixture(vcx);

        minimap.update(vcx, |mm, cx| {
            mm.minimap_thumb_drag_start(Point::new(px(10.0), px(150.0)), cx);
            mm.minimap_thumb_drag_to(Point::new(px(10.0), px(250.0)), cx);
        });

        assert_eq!(editor.read_with(vcx, |ed, _| ed.scroll_row()), 0);
        assert_eq!(
            editor.read_with(vcx, |ed, _| ed.scroll_manager().minimap_thumb_state()),
            None,
        );
    }

    #[test]
    fn minimap_thumb_drag_end_clears_dragging_state() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let vcx = cx.add_empty_window();
        let (editor, minimap) = minimap_drag_fixture(vcx);

        minimap.update(vcx, |mm, cx| {
            mm.minimap_thumb_drag_start(Point::new(px(10.0), px(4.0)), cx);
        });
        assert_eq!(
            editor.read_with(vcx, |ed, _| ed.scroll_manager().minimap_thumb_state()),
            Some(scroll::ScrollbarThumbState::Dragging),
        );

        minimap.update(vcx, |mm, cx| mm.minimap_thumb_drag_end(cx));
        assert_eq!(
            editor.read_with(vcx, |ed, _| ed.scroll_manager().minimap_thumb_state()),
            None,
        );
    }

    #[test]
    fn file_path_defaults_to_none_and_setter_stores() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "x");

        assert_eq!(
            editor.read_with(&cx, |ed, _| ed.file_path().map(|p| p.to_path_buf())),
            None,
        );

        editor.update(&mut cx, |ed, cx| {
            ed.set_file_path(Some(PathBuf::from("/ws/a.rs")), cx)
        });
        cx.run_until_parked();

        assert_eq!(
            editor.read_with(&cx, |ed, _| ed.file_path().map(|p| p.to_path_buf())),
            Some(PathBuf::from("/ws/a.rs")),
        );
    }

    #[test]
    fn diagnostic_set_change_emits_changed_on_editor() {
        use crate::diagnostics::DiagnosticSet;
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "x");
        let diag_set = cx.update(|cx| cx.new(|_| DiagnosticSet::new()));

        editor.update(&mut cx, |ed, cx| {
            ed.set_diagnostic_set(Some(diag_set.clone()), cx)
        });
        cx.run_until_parked();
        let (_recorder, events) = Recorder::install(&mut cx, &editor);

        let path = PathBuf::from("/ws/a.rs");
        diag_set.update(&mut cx, |s, cx| {
            s.replace_for_path(
                path,
                vec![lsp_types::Diagnostic {
                    range: lsp_types::Range::new(
                        lsp_types::Position::new(0, 0),
                        lsp_types::Position::new(0, 1),
                    ),
                    severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                    code: None,
                    code_description: None,
                    source: None,
                    message: String::new(),
                    related_information: None,
                    tags: None,
                    data: None,
                }],
                cx,
            )
        });
        cx.run_until_parked();

        let observed = drain(&events);
        assert!(
            observed.contains(&EditorEvent::Changed),
            "expected Changed event from diagnostic publish; got {observed:?}",
        );
    }

    #[test]
    fn render_visible_rows_includes_gutter_in_full_mode() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "alpha\nbeta\ngamma");

        let rows = editor.update(&mut cx, |ed, cx| ed.render_visible_rows(0..3, cx).len());
        assert_eq!(rows, 3);
    }

    #[test]
    fn render_visible_rows_omits_gutter_in_single_line_mode() {
        let mut cx = TestAppContext::single();
        install_executor_global(&mut cx);
        let vcx = cx.add_empty_window();
        let editor = vcx.update(|window, cx| cx.new(|cx| Editor::single_line(window, cx)));

        let count = editor.update(vcx, |ed, cx| ed.render_visible_rows(0..1, cx).len());
        assert_eq!(count, 1);
    }

    #[test]
    fn set_blame_state_emits_changed() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hi");
        let state =
            cx.update(|cx| cx.new(|cx| crate::git::blame::BlameState::new(buffer.clone(), cx)));
        let (_recorder, events) = Recorder::install(&mut cx, &editor);

        editor.update(&mut cx, |ed, cx| ed.set_blame_state(Some(state), cx));
        cx.run_until_parked();

        let observed = drain(&events);
        assert!(
            observed.contains(&EditorEvent::Changed),
            "expected Changed event when blame state attaches, got {observed:?}",
        );
    }

    #[test]
    fn set_blame_visible_toggles_flag_and_emits_changed() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hi");

        editor.read_with(&cx, |ed, _| assert!(!ed.blame_visible()));
        let (_recorder, events) = Recorder::install(&mut cx, &editor);

        editor.update(&mut cx, |ed, cx| ed.set_blame_visible(true, cx));
        cx.run_until_parked();
        editor.read_with(&cx, |ed, _| assert!(ed.blame_visible()));
        assert_eq!(drain(&events), vec![EditorEvent::Changed]);

        editor.update(&mut cx, |ed, cx| ed.set_blame_visible(true, cx));
        cx.run_until_parked();
        assert_eq!(drain(&events), Vec::<EditorEvent>::new());
    }

    #[test]
    fn blame_state_update_emits_editor_changed() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hi");
        let state =
            cx.update(|cx| cx.new(|cx| crate::git::blame::BlameState::new(buffer.clone(), cx)));
        editor.update(&mut cx, |ed, cx| {
            ed.set_blame_state(Some(state.clone()), cx)
        });
        let (_recorder, events) = Recorder::install(&mut cx, &editor);

        state.update(&mut cx, |s, cx| {
            s.set_blame(
                vec![stoat::host::BlameLine {
                    line: 0,
                    commit_sha: "abc".to_string(),
                    short_sha: "abc".to_string(),
                    author_name: "Ada".to_string(),
                    time: 0,
                }],
                cx,
            )
        });
        cx.run_until_parked();

        let observed = drain(&events);
        assert!(
            observed.contains(&EditorEvent::Changed),
            "expected Changed from blame state mutation, got {observed:?}",
        );
    }

    #[test]
    fn render_visible_rows_with_cursor_and_selection_does_not_panic() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "alpha\nbeta\ngamma");
        editor.update(&mut cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let sel = Selection {
                id: 1,
                start: snapshot.anchor_at(2, Bias::Left),
                end: snapshot.anchor_at(8, Bias::Left),
                reversed: false,
                goal: SelectionGoal::None,
            };
            ed.selections_mut().replace_with(vec![sel], &snapshot);
        });

        let rows = editor.update(&mut cx, |ed, cx| ed.render_visible_rows(0..3, cx).len());
        assert_eq!(rows, 3);
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn collect_review_render_data_extracts_moved_spans_from_review_chunks() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "alpha modified\nbeta\n");

        let session = cx.update(|cx| {
            cx.new(|_| {
                let mut inner = stoat::review_session::ReviewSession::new(
                    stoat::review_session::ReviewSource::InMemory {
                        files: Arc::new(Vec::new()),
                    },
                );
                inner.add_files(vec![stoat::review::ReviewFileInput {
                    path: PathBuf::from("a.txt"),
                    rel_path: "a.txt".to_string(),
                    language: None,
                    base_text: Arc::new("alpha\nbeta\n".to_string()),
                    buffer_text: Arc::new("alpha modified\nbeta\n".to_string()),
                }]);
                let chunk_id = inner.order[0];
                let chunk = inner.chunks.get_mut(&chunk_id).expect("seeded chunk");
                chunk.hunk = stoat::review::ReviewHunk {
                    rows: vec![stoat::review::ReviewRow::Changed {
                        left: None,
                        right: Some(stoat::review::ReviewSide {
                            text: "alpha modified".to_string(),
                            line_num: 1,
                            change_spans: vec![],
                            moved_spans: vec![6..14],
                            move_provenance: None,
                        }),
                    }],
                };
                crate::review_session::ReviewSession::new(inner)
            })
        });

        editor.update(&mut cx, |ed, cx| {
            ed.set_review_session(Some(session.clone()), cx);
            ed.set_review_file_index(Some(0), cx);
        });

        editor.read_with(&cx, |ed, app| {
            let data = ed.collect_review_render_data(app);
            assert_eq!(data.moved_spans, [(0u32, 6..14)]);
        });
    }

    #[test]
    fn collect_review_render_data_returns_empty_when_no_review_session() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hi");

        editor.read_with(&cx, |ed, app| {
            let data = ed.collect_review_render_data(app);
            assert!(data.chunk_markers.is_empty());
            assert!(data.provenances.is_empty());
            assert!(data.moved_spans.is_empty());
        });
    }

    #[test]
    fn tab_label_returns_scratch_when_file_path_is_unset() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello");

        editor.read_with(&cx, |ed, app| {
            assert_eq!(ed.tab_label(app), SharedString::from("(scratch)"));
        });
    }

    #[test]
    fn tab_label_returns_basename_when_file_path_is_set() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello");
        editor.update(&mut cx, |ed, cx| {
            ed.set_file_path(Some(PathBuf::from("/tmp/repo/src/main.rs")), cx);
        });

        editor.read_with(&cx, |ed, app| {
            assert_eq!(ed.tab_label(app), SharedString::from("main.rs"));
        });
    }

    #[test]
    fn is_dirty_reflects_underlying_singleton_buffer() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hello");

        editor.read_with(&cx, |ed, app| assert!(!ed.is_dirty(app)));

        buffer.update(&mut cx, |b, cx| b.edit(5..5, "!", cx));
        cx.run_until_parked();

        editor.read_with(&cx, |ed, app| assert!(ed.is_dirty(app)));
    }

    #[test]
    fn save_clears_dirty_on_singleton_buffer() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hello");
        buffer.update(&mut cx, |b, cx| b.edit(5..5, "!", cx));
        cx.run_until_parked();
        assert!(buffer.read_with(&cx, |b, _| b.is_dirty()));

        let _task = editor.update(&mut cx, |ed, cx| ed.save(cx));
        cx.run_until_parked();

        assert!(!buffer.read_with(&cx, |b, _| b.is_dirty()));
    }

    #[test]
    fn deserialize_returns_error_until_persistence_wires_through() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "");

        let outcome = editor.update(&mut cx, |_, cx| Editor::deserialize(Value::Null, cx).err());
        let err = outcome.expect("Editor::deserialize is unimplemented");
        assert!(matches!(err, ItemError::Deserialize { .. }));
    }

    fn set_single_selection(
        editor: &Entity<Editor>,
        cx: &mut TestAppContext,
        start: usize,
        end: usize,
    ) {
        editor.update(cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let start_anchor = snapshot.anchor_at(start, Bias::Right);
            let end_anchor = snapshot.anchor_at(end, Bias::Left);
            let id = ed.selections.all_anchors().first().map_or(1, |s| s.id);
            ed.selections.replace_with(
                vec![Selection {
                    id,
                    start: start_anchor,
                    end: end_anchor,
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &snapshot,
            );
        });
    }

    #[test]
    fn delete_selections_drops_non_empty_and_collapses_cursor() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hello world");
        set_single_selection(&editor, &mut cx, 0, 5);

        editor.update(&mut cx, |ed, cx| ed.delete_selections(cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), " world");
        editor.read_with(&cx, |ed, cx| {
            let snap = ed.multi_buffer.read(cx).snapshot();
            let sel = ed.selections().all_anchors().first().unwrap();
            assert_eq!(snap.resolve_anchor(&sel.start), 0);
            assert_eq!(snap.resolve_anchor(&sel.end), 0);
        });
    }

    #[test]
    fn delete_selections_skips_empty_selections() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hello");
        set_single_selection(&editor, &mut cx, 2, 2);

        editor.update(&mut cx, |ed, cx| ed.delete_selections(cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "hello");
    }

    #[test]
    fn delete_around_cursors_backward_removes_char_before_cursor() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "abc");
        set_single_selection(&editor, &mut cx, 2, 2);

        editor.update(&mut cx, |ed, cx| {
            ed.delete_around_cursors(DeleteDirection::Backward, cx)
        });
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "ac");
    }

    #[test]
    fn delete_around_cursors_forward_removes_char_after_cursor() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "abc");
        set_single_selection(&editor, &mut cx, 1, 1);

        editor.update(&mut cx, |ed, cx| {
            ed.delete_around_cursors(DeleteDirection::Forward, cx)
        });
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "ac");
    }

    #[test]
    fn delete_around_cursors_backward_respects_utf8_boundaries() {
        let mut cx = TestAppContext::single();
        // Each emoji is 4 bytes in UTF-8.
        let (buffer, editor) = new_editor(&mut cx, "a\u{1F600}b");
        let snap = editor.read_with(&cx, |ed, cx| ed.multi_buffer.read(cx).snapshot());
        let after_emoji = snap.rope().point_to_offset(stoat_text::Point::new(0, 5));
        set_single_selection(&editor, &mut cx, after_emoji, after_emoji);

        editor.update(&mut cx, |ed, cx| {
            ed.delete_around_cursors(DeleteDirection::Backward, cx)
        });
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "ab");
    }

    #[test]
    fn delete_word_around_cursors_backward_removes_word_before_cursor() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "foo bar");
        set_single_selection(&editor, &mut cx, 7, 7);

        editor.update(&mut cx, |ed, cx| {
            ed.delete_word_around_cursors(DeleteDirection::Backward, cx)
        });
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "foo ");
    }

    #[test]
    fn delete_word_around_cursors_forward_removes_word_after_cursor() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "foo bar");
        set_single_selection(&editor, &mut cx, 0, 0);

        editor.update(&mut cx, |ed, cx| {
            ed.delete_word_around_cursors(DeleteDirection::Forward, cx)
        });
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "bar");
    }

    #[test]
    fn yank_payload_joins_selections_by_newline() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello world");
        set_single_selection(&editor, &mut cx, 0, 5);

        let payload = editor.read_with(&cx, |ed, cx| ed.yank_payload(cx));
        assert_eq!(payload, Some("hello".to_string()));
    }

    #[test]
    fn yank_payload_returns_none_for_only_empty_selections() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello");
        set_single_selection(&editor, &mut cx, 2, 2);

        let payload = editor.read_with(&cx, |ed, cx| ed.yank_payload(cx));
        assert_eq!(payload, None);
    }

    #[test]
    fn paste_at_selections_after_inserts_at_selection_end() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hello world");
        set_single_selection(&editor, &mut cx, 0, 5);

        editor.update(&mut cx, |ed, cx| {
            ed.paste_at_selections("XYZ", PastePosition::After, cx)
        });
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "helloXYZ world");
    }

    #[test]
    fn paste_at_selections_before_inserts_at_selection_start() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hello world");
        set_single_selection(&editor, &mut cx, 6, 11);

        editor.update(&mut cx, |ed, cx| {
            ed.paste_at_selections("XYZ", PastePosition::Before, cx)
        });
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "hello XYZworld");
    }

    #[test]
    fn collapse_selections_to_start_pins_cursor_at_left() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello");
        set_single_selection(&editor, &mut cx, 0, 5);

        editor.update(&mut cx, |ed, cx| ed.collapse_selections_to_start(cx));
        cx.run_until_parked();

        editor.read_with(&cx, |ed, cx| {
            let snap = ed.multi_buffer.read(cx).snapshot();
            let sel = ed.selections().all_anchors().first().unwrap();
            assert_eq!(snap.resolve_anchor(&sel.start), 0);
            assert_eq!(snap.resolve_anchor(&sel.end), 0);
        });
    }

    #[test]
    fn collapse_selections_to_end_pins_cursor_at_right() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello");
        set_single_selection(&editor, &mut cx, 0, 5);

        editor.update(&mut cx, |ed, cx| ed.collapse_selections_to_end(cx));
        cx.run_until_parked();

        editor.read_with(&cx, |ed, cx| {
            let snap = ed.multi_buffer.read(cx).snapshot();
            let sel = ed.selections().all_anchors().first().unwrap();
            assert_eq!(snap.resolve_anchor(&sel.start), 5);
            assert_eq!(snap.resolve_anchor(&sel.end), 5);
        });
    }

    #[test]
    fn handle_undo_reverts_a_buffer_edit() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hello");
        buffer.update(&mut cx, |b, cx| b.edit(5..5, " world", cx));
        cx.run_until_parked();
        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "hello world");

        let applied = editor.update(&mut cx, |ed, cx| ed.handle_undo(1, cx));
        cx.run_until_parked();

        assert_eq!(applied, 1);
        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "hello");
    }

    #[test]
    fn handle_undo_returns_zero_on_empty_history() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "");

        let applied = editor.update(&mut cx, |ed, cx| ed.handle_undo(1, cx));
        cx.run_until_parked();

        assert_eq!(applied, 0);
    }

    #[test]
    fn handle_redo_restores_undone_edit() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hello");
        buffer.update(&mut cx, |b, cx| b.edit(5..5, " world", cx));
        cx.run_until_parked();
        editor.update(&mut cx, |ed, cx| ed.handle_undo(1, cx));
        cx.run_until_parked();
        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "hello");

        let applied = editor.update(&mut cx, |ed, cx| ed.handle_redo(1, cx));
        cx.run_until_parked();

        assert_eq!(applied, 1);
        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "hello world");
    }

    #[test]
    fn handle_undo_applies_up_to_count_then_stops() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "");
        buffer.update(&mut cx, |b, cx| b.edit(0..0, "a", cx));
        buffer.update(&mut cx, |b, cx| b.edit(1..1, "b", cx));
        buffer.update(&mut cx, |b, cx| b.edit(2..2, "c", cx));
        cx.run_until_parked();
        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "abc");

        // Ask for 10 undos with only 3 entries in history.
        let applied = editor.update(&mut cx, |ed, cx| ed.handle_undo(10, cx));
        cx.run_until_parked();

        assert_eq!(applied, 3);
        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "");
    }

    #[test]
    fn commit_checkpoint_returns_a_checkpoint_id() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello");

        let first = editor.update(&mut cx, |ed, cx| ed.commit_checkpoint(None, cx));
        let second = editor.update(&mut cx, |ed, cx| {
            ed.commit_checkpoint(Some("after".into()), cx)
        });
        cx.run_until_parked();

        assert!(first.is_some());
        assert!(second.is_some());
        assert_ne!(first, second);
    }

    #[test]
    fn collapse_selection_collapses_to_head() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello world");
        set_single_selection(&editor, &mut cx, 0, 5);

        editor.update(&mut cx, |ed, cx| ed.collapse_selection(cx));
        cx.run_until_parked();

        editor.read_with(&cx, |ed, cx| {
            let snap = ed.multi_buffer.read(cx).snapshot();
            let sel = ed.selections().all_anchors().first().unwrap();
            assert_eq!(snap.resolve_anchor(&sel.start), 5);
            assert_eq!(snap.resolve_anchor(&sel.end), 5);
        });
    }

    #[test]
    fn flip_selections_toggles_reversed() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello");
        set_single_selection(&editor, &mut cx, 0, 5);

        editor.update(&mut cx, |ed, cx| ed.flip_selections(cx));
        cx.run_until_parked();

        editor.read_with(&cx, |ed, _| {
            assert!(ed.selections().all_anchors().first().unwrap().reversed);
        });
    }

    #[test]
    fn select_all_replaces_with_full_range() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello world");

        editor.update(&mut cx, |ed, cx| ed.select_all(cx));
        cx.run_until_parked();

        editor.read_with(&cx, |ed, cx| {
            let snap = ed.multi_buffer.read(cx).snapshot();
            let sel = ed.selections().all_anchors().first().unwrap();
            assert_eq!(snap.resolve_anchor(&sel.start), 0);
            assert_eq!(snap.resolve_anchor(&sel.end), 11);
        });
    }

    #[test]
    fn select_line_below_extends_through_newline() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abc\ndef\nghi");
        set_single_selection(&editor, &mut cx, 1, 1);

        editor.update(&mut cx, |ed, cx| ed.select_line_below(1, cx));
        cx.run_until_parked();

        editor.read_with(&cx, |ed, cx| {
            let snap = ed.multi_buffer.read(cx).snapshot();
            let sel = ed.selections().all_anchors().first().unwrap();
            assert_eq!(snap.resolve_anchor(&sel.start), 0);
            assert_eq!(snap.resolve_anchor(&sel.end), 4);
        });
    }

    #[test]
    fn keep_primary_drops_others() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "ab\ncd\nef");
        set_single_selection(&editor, &mut cx, 0, 8);
        editor.update(&mut cx, |ed, cx| ed.split_selection_on_newline(cx));
        cx.run_until_parked();
        editor.read_with(&cx, |ed, _| {
            assert_eq!(ed.selections().all_anchors().len(), 3);
        });

        editor.update(&mut cx, |ed, cx| ed.keep_primary_selection(cx));
        cx.run_until_parked();

        editor.read_with(&cx, |ed, _| {
            assert_eq!(ed.selections().all_anchors().len(), 1);
        });
    }

    #[test]
    fn remove_primary_keeps_others() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "ab\ncd\nef");
        set_single_selection(&editor, &mut cx, 0, 8);
        editor.update(&mut cx, |ed, cx| ed.split_selection_on_newline(cx));
        cx.run_until_parked();
        editor.read_with(&cx, |ed, _| {
            assert_eq!(ed.selections().all_anchors().len(), 3);
        });

        editor.update(&mut cx, |ed, cx| ed.remove_primary_selection(cx));
        cx.run_until_parked();

        editor.read_with(&cx, |ed, _| {
            assert_eq!(ed.selections().all_anchors().len(), 2);
        });
    }

    #[test]
    fn rotate_selections_forward_advances_primary() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "ab\ncd\nef");
        set_single_selection(&editor, &mut cx, 0, 8);
        editor.update(&mut cx, |ed, cx| ed.split_selection_on_newline(cx));
        cx.run_until_parked();
        let primary_before = editor.read_with(&cx, |ed, _| ed.selections().newest_anchor().id);

        editor.update(&mut cx, |ed, cx| ed.rotate_selections(true, 1, cx));
        cx.run_until_parked();

        let primary_after = editor.read_with(&cx, |ed, _| ed.selections().newest_anchor().id);
        assert_ne!(primary_before, primary_after);
    }

    #[test]
    fn trim_selections_strips_surrounding_whitespace() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "  hello  ");
        set_single_selection(&editor, &mut cx, 0, 9);

        editor.update(&mut cx, |ed, cx| ed.trim_selections(cx));
        cx.run_until_parked();

        editor.read_with(&cx, |ed, cx| {
            let snap = ed.multi_buffer.read(cx).snapshot();
            let sel = ed.selections().all_anchors().first().unwrap();
            assert_eq!(snap.resolve_anchor(&sel.start), 2);
            assert_eq!(snap.resolve_anchor(&sel.end), 7);
        });
    }

    #[test]
    fn split_selection_on_newline_splits_per_line() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "ab\ncd\nef");
        set_single_selection(&editor, &mut cx, 0, 8);

        editor.update(&mut cx, |ed, cx| ed.split_selection_on_newline(cx));
        cx.run_until_parked();

        editor.read_with(&cx, |ed, _| {
            assert_eq!(ed.selections().all_anchors().len(), 3);
        });
    }

    #[test]
    fn split_selection_by_pattern_splits_on_each_match() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "foo, bar, baz");
        set_single_selection(&editor, &mut cx, 0, 13);

        editor.update(&mut cx, |ed, cx| ed.split_selection_by_pattern(", ", cx));
        cx.run_until_parked();

        editor.read_with(&cx, |ed, cx| {
            let snap = ed.multi_buffer.read(cx).snapshot();
            let anchors = ed.selections().all_anchors();
            assert_eq!(anchors.len(), 3);
            let texts: Vec<String> = anchors
                .iter()
                .map(|s| {
                    let lo = snap.resolve_anchor(&s.start);
                    let hi = snap.resolve_anchor(&s.end);
                    snap.rope().slice(lo..hi).to_string()
                })
                .collect();
            assert_eq!(texts, vec!["foo", "bar", "baz"]);
        });
    }

    #[test]
    fn filter_selections_by_pattern_keeps_matches() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "alpha beta gamma");
        // Split into three space-delimited selections so we can filter.
        set_single_selection(&editor, &mut cx, 0, 16);
        editor.update(&mut cx, |ed, cx| ed.split_selection_by_pattern(" ", cx));
        cx.run_until_parked();
        editor.read_with(&cx, |ed, _| {
            assert_eq!(ed.selections().all_anchors().len(), 3);
        });

        editor.update(&mut cx, |ed, cx| {
            ed.filter_selections_by_pattern("^a", false, cx)
        });
        cx.run_until_parked();

        editor.read_with(&cx, |ed, cx| {
            let snap = ed.multi_buffer.read(cx).snapshot();
            let anchors = ed.selections().all_anchors();
            assert_eq!(anchors.len(), 1);
            let sel = anchors.first().unwrap();
            let lo = snap.resolve_anchor(&sel.start);
            let hi = snap.resolve_anchor(&sel.end);
            assert_eq!(snap.rope().slice(lo..hi).to_string(), "alpha");
        });
    }

    #[test]
    fn align_selections_pads_to_max_column() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "a\nbbb\nc");
        // Cursors at each row's end:
        // row 0 col 1 (offset 1)
        // row 1 col 3 (offset 5)
        // row 2 col 1 (offset 7)
        editor.update(&mut cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let mk = |off: usize| Selection {
                id: 0,
                start: snapshot.anchor_at(off, Bias::Right),
                end: snapshot.anchor_at(off, Bias::Left),
                reversed: false,
                goal: SelectionGoal::None,
            };
            ed.selections
                .replace_with(vec![mk(1), mk(5), mk(7)], &snapshot);
        });

        editor.update(&mut cx, |ed, cx| ed.align_selections(cx));
        cx.run_until_parked();

        // After alignment, row 0 column 1 should be padded to column 3 (extra 2 spaces),
        // row 2 column 1 should be padded to column 3 (extra 2 spaces), row 1 unchanged.
        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "a  \nbbb\nc  ");
    }

    #[test]
    fn handle_surround_add_wraps_non_empty_selection_with_brackets() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hello world");
        set_single_selection(&editor, &mut cx, 0, 5);

        editor.update(&mut cx, |ed, cx| ed.handle_surround_add('(', cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "(hello) world");
        editor.read_with(&cx, |ed, cx| {
            let snap = ed.multi_buffer.read(cx).snapshot();
            let sel = ed.selections().all_anchors().first().unwrap();
            assert_eq!(snap.resolve_anchor(&sel.start), 1);
            assert_eq!(snap.resolve_anchor(&sel.end), 6);
        });
    }

    #[test]
    fn handle_surround_add_doubles_for_quote_char() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hi");
        set_single_selection(&editor, &mut cx, 0, 2);

        editor.update(&mut cx, |ed, cx| ed.handle_surround_add('"', cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "\"hi\"");
    }

    #[test]
    fn handle_surround_add_skips_empty_selection() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hi");
        set_single_selection(&editor, &mut cx, 1, 1);

        editor.update(&mut cx, |ed, cx| ed.handle_surround_add('(', cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "hi");
    }

    #[test]
    fn handle_surround_delete_removes_enclosing_brackets() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "a(hello)b");
        set_single_selection(&editor, &mut cx, 4, 4);

        editor.update(&mut cx, |ed, cx| ed.handle_surround_delete('(', cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "ahellob");
    }

    #[test]
    fn handle_surround_delete_no_enclosing_pair_is_noop() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "hello");
        set_single_selection(&editor, &mut cx, 2, 2);

        editor.update(&mut cx, |ed, cx| ed.handle_surround_delete('(', cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "hello");
    }

    #[test]
    fn handle_surround_replace_swaps_pair() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "a(hello)b");
        set_single_selection(&editor, &mut cx, 4, 4);

        editor.update(&mut cx, |ed, cx| ed.handle_surround_replace('(', '[', cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "a[hello]b");
    }

    #[test]
    fn handle_surround_replace_swaps_to_quote() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "a(hello)b");
        set_single_selection(&editor, &mut cx, 4, 4);

        editor.update(&mut cx, |ed, cx| ed.handle_surround_replace('(', '"', cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "a\"hello\"b");
    }

    #[test]
    fn cursor_after_only_whitespace_true_at_column_zero() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello");
        set_single_selection(&editor, &mut cx, 0, 0);

        let result = editor.read_with(&cx, |ed, cx| ed.cursor_after_only_whitespace(cx));
        assert!(result);
    }

    #[test]
    fn cursor_after_only_whitespace_true_after_indent() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "\t\thello");
        set_single_selection(&editor, &mut cx, 2, 2);

        let result = editor.read_with(&cx, |ed, cx| ed.cursor_after_only_whitespace(cx));
        assert!(result);
    }

    #[test]
    fn cursor_after_only_whitespace_false_with_text_before() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello world");
        set_single_selection(&editor, &mut cx, 3, 3);

        let result = editor.read_with(&cx, |ed, cx| ed.cursor_after_only_whitespace(cx));
        assert!(!result);
    }

    #[test]
    fn cursor_after_only_whitespace_per_line() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abc\n  def");
        // Cursor after the two spaces on line 1 (offset = 4 + 2 = 6).
        set_single_selection(&editor, &mut cx, 6, 6);

        let result = editor.read_with(&cx, |ed, cx| ed.cursor_after_only_whitespace(cx));
        assert!(result);
    }

    #[test]
    fn handle_number_delta_increments_decimal_at_cursor() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "x = 41");
        set_single_selection(&editor, &mut cx, 4, 4);

        editor.update(&mut cx, |ed, cx| ed.handle_number_delta(1, cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "x = 42");
    }

    #[test]
    fn handle_number_delta_decrements_decimal() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "5");
        set_single_selection(&editor, &mut cx, 0, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_number_delta(-1, cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "4");
    }

    #[test]
    fn handle_number_delta_walks_forward_to_first_digit() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "x = 9");
        set_single_selection(&editor, &mut cx, 0, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_number_delta(1, cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "x = 10");
    }

    #[test]
    fn handle_number_delta_no_number_is_noop() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "no digits here");
        set_single_selection(&editor, &mut cx, 3, 3);

        editor.update(&mut cx, |ed, cx| ed.handle_number_delta(1, cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "no digits here");
    }

    #[test]
    fn handle_number_delta_increments_hex_and_preserves_case() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "0xFF");
        set_single_selection(&editor, &mut cx, 0, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_number_delta(1, cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "0x100");
    }

    #[test]
    fn handle_number_delta_increments_binary() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "0b0011");
        set_single_selection(&editor, &mut cx, 0, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_number_delta(1, cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "0b0100");
    }

    #[test]
    fn handle_number_delta_increments_negative_decimal_toward_zero() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "-3");
        set_single_selection(&editor, &mut cx, 0, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_number_delta(1, cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "-2");
    }

    #[test]
    fn handle_number_delta_count_three_adds_three() {
        let mut cx = TestAppContext::single();
        let (buffer, editor) = new_editor(&mut cx, "10");
        set_single_selection(&editor, &mut cx, 0, 0);

        editor.update(&mut cx, |ed, cx| ed.handle_number_delta(3, cx));
        cx.run_until_parked();

        assert_eq!(buffer.read_with(&cx, |b, _| b.text()), "13");
    }

    fn set_search(
        editor: &Entity<Editor>,
        cx: &mut TestAppContext,
        query: &str,
        dir: search::SearchDirection,
    ) {
        editor.update(cx, |ed, cx| {
            ed.set_search_state(Some(search::SearchState::new(query, dir)), cx);
        });
    }

    #[test]
    fn search_next_jumps_to_first_match_after_cursor() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abc def abc");
        seed_cursors(&editor, &mut cx, &[0]);
        set_search(&editor, &mut cx, "abc", search::SearchDirection::Forward);

        editor.update(&mut cx, |ed, cx| ed.search_next(cx));

        assert_eq!(cursor_offsets(&editor, &mut cx), vec![8]);
    }

    #[test]
    fn search_next_wraps_when_no_match_after_cursor() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abc def");
        seed_cursors(&editor, &mut cx, &[5]);
        set_search(&editor, &mut cx, "abc", search::SearchDirection::Forward);

        editor.update(&mut cx, |ed, cx| ed.search_next(cx));

        assert_eq!(cursor_offsets(&editor, &mut cx), vec![0]);
    }

    #[test]
    fn search_next_in_reverse_direction_goes_backward() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abc def abc");
        seed_cursors(&editor, &mut cx, &[10]);
        set_search(&editor, &mut cx, "abc", search::SearchDirection::Reverse);

        editor.update(&mut cx, |ed, cx| ed.search_next(cx));

        assert_eq!(cursor_offsets(&editor, &mut cx), vec![8]);
    }

    #[test]
    fn search_prev_flips_direction() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abc def abc xyz");
        seed_cursors(&editor, &mut cx, &[8]);
        set_search(&editor, &mut cx, "abc", search::SearchDirection::Forward);

        editor.update(&mut cx, |ed, cx| ed.search_prev(cx));

        assert_eq!(cursor_offsets(&editor, &mut cx), vec![0]);
    }

    #[test]
    fn search_next_without_state_is_noop() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abc");
        seed_cursors(&editor, &mut cx, &[1]);

        editor.update(&mut cx, |ed, cx| ed.search_next(cx));

        assert_eq!(cursor_offsets(&editor, &mut cx), vec![1]);
    }

    #[test]
    fn search_next_with_empty_query_is_noop() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abc");
        seed_cursors(&editor, &mut cx, &[1]);
        set_search(&editor, &mut cx, "", search::SearchDirection::Forward);

        editor.update(&mut cx, |ed, cx| ed.search_next(cx));

        assert_eq!(cursor_offsets(&editor, &mut cx), vec![1]);
    }

    #[test]
    fn search_next_with_no_match_leaves_cursor_unchanged() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abc");
        seed_cursors(&editor, &mut cx, &[1]);
        set_search(&editor, &mut cx, "zzz", search::SearchDirection::Forward);

        editor.update(&mut cx, |ed, cx| ed.search_next(cx));

        assert_eq!(cursor_offsets(&editor, &mut cx), vec![1]);
    }

    #[test]
    fn search_next_with_invalid_regex_is_noop() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abc");
        seed_cursors(&editor, &mut cx, &[1]);
        set_search(
            &editor,
            &mut cx,
            "[unclosed",
            search::SearchDirection::Forward,
        );

        editor.update(&mut cx, |ed, cx| ed.search_next(cx));

        assert_eq!(cursor_offsets(&editor, &mut cx), vec![1]);
    }

    #[test]
    fn search_next_collapses_every_selection_onto_match() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abc def abc");
        seed_cursors(&editor, &mut cx, &[0, 5]);
        set_search(&editor, &mut cx, "abc", search::SearchDirection::Forward);

        editor.update(&mut cx, |ed, cx| ed.search_next(cx));

        assert_eq!(cursor_offsets(&editor, &mut cx), vec![8]);
    }

    #[test]
    fn search_next_emits_changed_event() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abc def abc");
        seed_cursors(&editor, &mut cx, &[0]);
        set_search(&editor, &mut cx, "abc", search::SearchDirection::Forward);
        let (_recorder, events) = Recorder::install(&mut cx, &editor);

        editor.update(&mut cx, |ed, cx| ed.search_next(cx));

        assert!(drain(&events).contains(&EditorEvent::Changed));
    }

    fn arm_labels(editor: &Entity<Editor>, cx: &mut TestAppContext, entries: &[(&str, usize)]) {
        let labels: std::collections::BTreeMap<String, usize> =
            entries.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        editor.update(cx, |ed, cx| ed.arm_pending_goto_word(labels, cx));
    }

    #[test]
    fn arm_pending_goto_word_stores_labels_and_clears_input() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "alpha beta");
        editor.update(&mut cx, |ed, cx| ed.push_pending_goto_word_input('z', cx));
        arm_labels(&editor, &mut cx, &[("a", 0)]);

        editor.read_with(&cx, |ed, _| {
            assert_eq!(ed.pending_goto_word_labels().map(|m| m.len()), Some(1));
            assert_eq!(ed.pending_goto_word_input(), "");
        });
    }

    #[test]
    fn clear_pending_goto_word_drops_labels_and_input() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "alpha");
        arm_labels(&editor, &mut cx, &[("a", 0)]);
        editor.update(&mut cx, |ed, cx| ed.push_pending_goto_word_input('a', cx));

        editor.update(&mut cx, |ed, cx| ed.clear_pending_goto_word(cx));

        editor.read_with(&cx, |ed, _| {
            assert!(ed.pending_goto_word_labels().is_none());
            assert_eq!(ed.pending_goto_word_input(), "");
        });
    }

    #[test]
    fn jump_to_offset_collapses_primary_cursor() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "alpha beta gamma");
        seed_cursors(&editor, &mut cx, &[0]);

        editor.update(&mut cx, |ed, cx| ed.jump_to_offset(11, cx));

        assert_eq!(cursor_offsets(&editor, &mut cx), vec![11]);
    }

    #[test]
    fn jump_to_offset_clamps_past_buffer_end() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "abc");
        seed_cursors(&editor, &mut cx, &[0]);

        editor.update(&mut cx, |ed, cx| ed.jump_to_offset(9999, cx));

        assert_eq!(cursor_offsets(&editor, &mut cx), vec![3]);
    }
}

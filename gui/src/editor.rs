pub mod actions;
pub mod mouse;
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
    theme::{self, DEFAULT_EDITOR_FONT_FAMILY, DEFAULT_EDITOR_FONT_SIZE},
};
use gpui::{
    canvas, div, font, px, size as gpui_size, uniform_list, App, AppContext, Bounds, Context, Div,
    Entity, EventEmitter, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, ParentElement, Pixels, Point, Render, ScrollWheelEvent, SharedString, Size,
    Styled, Subscription, Task, UniformListScrollHandle, WeakEntity, Window,
};
use serde_json::Value;
use stoat::{
    buffer::BufferId, jumplist::JumpList, review_session::ChunkStatus,
    selection::SelectionsCollection, DisplayPoint,
};
use stoat_text::{Anchor, Bias, OffsetUtf16, Selection, SelectionGoal};

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
    scroll_handle: UniformListScrollHandle,
    jumplist: JumpList,
    cell_size: Option<Size<Pixels>>,
    file_path: Option<std::path::PathBuf>,
    diagnostic_set: Option<Entity<crate::diagnostics::DiagnosticSet>>,
    review_session: Option<Entity<crate::review_session::ReviewSession>>,
    review_file_index: Option<usize>,
    search_state: Option<search::SearchState>,
    workspace: Option<WeakEntity<crate::workspace::Workspace>>,
    text_region_bounds: Option<Bounds<Pixels>>,
    hover_position: Option<(u32, u32)>,
    hover_debounce_task: Option<Task<()>>,
    hover_popup: Option<Entity<crate::lsp::HoverPopup>>,
    completion_popup: Option<Entity<crate::lsp::CompletionPopup>>,
    inlay_hints_manager: Option<Entity<crate::lsp::InlayHintsManager>>,
    semantic_tokens_manager: Option<Entity<crate::lsp::SemanticTokensManager>>,
    expansion_history: Vec<std::ops::Range<usize>>,
    expansion_tip: Option<std::ops::Range<usize>>,
    blame_state: Option<Entity<crate::git::blame::BlameState>>,
    blame_visible: bool,
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

#[derive(Default)]
struct ReviewRenderData {
    chunk_markers: Vec<(u32, ChunkStatus)>,
    provenances: Vec<(u32, stoat::review::MoveProvenance)>,
    moved_spans: Vec<(u32, std::ops::Range<usize>)>,
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
            scroll_handle: UniformListScrollHandle::new(),
            jumplist: JumpList::new(),
            cell_size: None,
            file_path: None,
            diagnostic_set: None,
            review_session: None,
            review_file_index: None,
            search_state: None,
            workspace: None,
            text_region_bounds: None,
            hover_position: None,
            hover_debounce_task: None,
            hover_popup: None,
            completion_popup: None,
            inlay_hints_manager: None,
            semantic_tokens_manager: None,
            expansion_history: Vec::new(),
            expansion_tip: None,
            blame_state: None,
            blame_visible: false,
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

        let mut ascending: Vec<(usize, std::ops::Range<usize>)> = {
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

    fn dispatch_click_at(
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
        workspace.update(cx, |w, cx| {
            w.dispatch_action(Box::new(crate::actions::ClickAt { row, col }), window, cx);
        });
    }

    fn dispatch_drag_select_to(
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
        workspace.update(cx, |w, cx| {
            w.dispatch_action(
                Box::new(crate::actions::DragSelectTo { row, col }),
                window,
                cx,
            );
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
        let task = cx.spawn_in(window, async move |this, cx| {
            executor.timer(std::time::Duration::from_millis(50)).await;
            let _ = this.update_in(cx, |_, window, cx| {
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
        let Some(cell) = self.cell_size else {
            return;
        };
        let max_row = self
            .display_map
            .update(cx, |dm, _| dm.snapshot())
            .max_point()
            .row as f64;
        let changed = self.scroll_manager.apply_wheel(
            event.delta,
            cell.height,
            event.modifiers.alt,
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
        let mut anchor = *self.scroll_manager.anchor();
        anchor.offset.y = new_y;
        self.scroll_manager.set_anchor(anchor);
        let pixel_offset_y = render::scroll_position_to_pixel_offset_y(new_y, cell.height);
        self.scroll_handle
            .0
            .borrow()
            .base_handle
            .set_offset(Point::new(px(0.0), pixel_offset_y));
        let new_row = new_y.floor().max(0.0) as u32;
        self.set_scroll_row(new_row, cx);
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

    fn render_visible_rows(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<'_, Self>,
    ) -> Vec<Div> {
        let display_snapshot = self.display_map.update(cx, |dm, _| dm.snapshot());
        let total_rows = (display_snapshot.max_point().row + 1) as usize;
        let end = range.end.min(total_rows);
        let start = range.start.min(end);
        let mut rows = render::build_rendered_rows(&display_snapshot, start as u32..end as u32);

        let review_data = self.collect_review_render_data(cx);
        render::apply_review_moved_overlay(
            &mut rows,
            &display_snapshot,
            start as u32..end as u32,
            &review_data.moved_spans,
        );

        let selection_paint = render::compute_selection_paint(
            &display_snapshot,
            self.selections.all_anchors(),
            &rows,
            start as u32,
        );
        let selection_color = theme::selection_color(cx);
        let cursor_color = theme::cursor_color(cx);
        let active_line_color = theme::active_line_color(cx);

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
                    active_line_color,
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
            line_number_color: theme::muted_text_color(cx),
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
        self.apply_pending_autoscroll(cx);
        let total_rows = self
            .display_map
            .update(cx, |dm, _| dm.snapshot())
            .max_point()
            .row as usize
            + 1;
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

        let (family, size) = editor_font(cx);
        let cell_family = family.clone();
        let bounds_capture = canvas(
            move |bounds, window, cx| {
                let font_id = window
                    .text_system()
                    .resolve_font(&font(cell_family.clone()));
                let font_size = px(size);
                let line_height = px((size * GPUI_DEFAULT_LINE_HEIGHT_RATIO).round());
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
            },
            |_, _, _, _| {},
        )
        .size_full();

        let hover_popup = self.hover_popup.clone();
        let completion_popup = self.completion_popup.clone();
        let mut root = div()
            .relative()
            .size_full()
            .font_family(family)
            .text_size(px(size))
            .child(list)
            .child(bounds_capture);
        if let Some(popup) = hover_popup {
            root = root.child(popup);
        }
        if let Some(popup) = completion_popup {
            root = root.child(popup);
        }
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
        .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
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
        install_executor_global(&mut cx);
        let vcx = cx.add_empty_window();
        let text = multiline_text(30);
        let (_buffer, editor) = editor_with_viewport(vcx, &text);
        editor.update_in(vcx, |ed, _, cx| {
            ed.set_cursor_at_grid(15, 0, cx);
            ed.request_autoscroll(AutoscrollStrategy::Top, cx);
            ed.apply_pending_autoscroll(cx);
        });
        vcx.run_until_parked();

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
        install_executor_global(&mut cx);
        let vcx = cx.add_empty_window();
        let text = multiline_text(30);
        let (_buffer, editor) = editor_with_viewport(vcx, &text);
        editor.update_in(vcx, |ed, _, cx| {
            ed.set_cursor_at_grid(15, 0, cx);
            ed.request_autoscroll(AutoscrollStrategy::Top, cx);
            ed.apply_pending_autoscroll(cx);
        });
        vcx.run_until_parked();

        let offset = editor.read_with(vcx, |ed, _| {
            ed.scroll_handle().0.borrow().base_handle.offset()
        });
        assert_eq!(offset.y, px(-160.0));
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

    fn install_executor_global(cx: &mut TestAppContext) {
        let executor = test_executor();
        cx.update(|cx| cx.set_global(ExecutorGlobal(executor)));
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
}

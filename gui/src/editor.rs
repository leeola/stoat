pub mod mouse;
pub mod render;

use crate::{
    buffer::Buffer,
    diff_map::{DiffMap, DiffMapEvent},
    display_map::{DisplayMap, DisplayMapEvent},
    globals::ExecutorGlobal,
    multi_buffer::{MultiBuffer, MultiBufferEvent},
};
use gpui::{
    uniform_list, AppContext, Bounds, Context, Div, Entity, EventEmitter, IntoElement, Pixels,
    Point, Render, Size, Styled, Subscription, WeakEntity, Window,
};
use stoat::{buffer::BufferId, jumplist::JumpList, selection::SelectionsCollection, DisplayPoint};
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
    jumplist: JumpList,
    cell_size: Option<Size<Pixels>>,
    file_path: Option<std::path::PathBuf>,
    diagnostic_set: Option<Entity<crate::diagnostics::DiagnosticSet>>,
    _subscriptions: [Subscription; 3],
    _diagnostic_subscription: Option<Subscription>,
}

/// Single coalesced "editor changed" signal. Subscribers re-render on
/// any event; finer-grained variants are added when a consumer needs
/// to discriminate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorEvent {
    Changed,
}

impl EventEmitter<EditorEvent> for Editor {}

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
            jumplist: JumpList::new(),
            cell_size: None,
            file_path: None,
            diagnostic_set: None,
            _subscriptions: [mb_sub, dm_sub, diff_sub],
            _diagnostic_subscription: None,
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

    fn render_visible_rows(
        &mut self,
        range: std::ops::Range<usize>,
        cx: &mut Context<'_, Self>,
    ) -> Vec<Div> {
        let display_snapshot = self.display_map.update(cx, |dm, _| dm.snapshot());
        let total_rows = (display_snapshot.max_point().row + 1) as usize;
        let end = range.end.min(total_rows);
        let start = range.start.min(end);
        let rows = render::build_rendered_rows(&display_snapshot, start as u32..end as u32);

        if !self.mode.show_gutter() {
            return rows.into_iter().map(render::render_row_element).collect();
        }

        let metrics = render::gutter_metrics(&display_snapshot);
        let diff_map_inner = self.diff_map.read(cx).diff().clone();
        let diagnostic_row_map = match (self.file_path.as_deref(), self.diagnostic_set.as_ref()) {
            (Some(path), Some(set)) => {
                Some(render::compute_row_severity_for_path(set.read(cx), path))
            },
            _ => None,
        };
        let paint = render::GutterPaint {
            display_snapshot: &display_snapshot,
            diff_map: &diff_map_inner,
            diagnostics: diagnostic_row_map.as_ref(),
            metrics,
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
        let total_rows = self
            .display_map
            .update(cx, |dm, _| dm.snapshot())
            .max_point()
            .row as usize
            + 1;
        let handle = cx.entity().downgrade();
        uniform_list("editor-rows", total_rows, move |range, _window, cx| {
            handle
                .upgrade()
                .map(|editor| editor.update(cx, |ed, cx| ed.render_visible_rows(range, cx)))
                .unwrap_or_default()
        })
        .size_full()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{buffer::Buffer, globals::ExecutorGlobal};
    use gpui::{AppContext, TestAppContext, VisualTestContext};
    use std::sync::{Arc, Mutex};
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

    fn cell(width: f32, height: f32) -> Size<Pixels> {
        gpui::size(gpui::px(width), gpui::px(height))
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

    #[test]
    fn pixel_bounds_for_utf16_offset_returns_none_without_cell_size() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello");

        let result = editor.update(&mut cx, |ed, cx| {
            ed.pixel_bounds_for_utf16_offset(2, Point::new(gpui::px(0.0), gpui::px(0.0)), cx)
        });
        assert_eq!(result, None);
    }

    #[test]
    fn pixel_bounds_for_utf16_offset_positions_at_offset_zero() {
        let mut cx = TestAppContext::single();
        let (_buffer, editor) = new_editor(&mut cx, "hello");
        editor.update(&mut cx, |ed, cx| ed.set_cell_size(cell(8.0, 16.0), cx));

        let result = editor.update(&mut cx, |ed, cx| {
            ed.pixel_bounds_for_utf16_offset(0, Point::new(gpui::px(10.0), gpui::px(20.0)), cx)
        });
        assert_eq!(
            result,
            Some(Bounds {
                origin: Point::new(gpui::px(10.0), gpui::px(20.0)),
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
            ed.pixel_bounds_for_utf16_offset(5, Point::new(gpui::px(0.0), gpui::px(0.0)), cx)
        });
        assert_eq!(
            result,
            Some(Bounds {
                origin: Point::new(gpui::px(8.0), gpui::px(16.0)),
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
            ed.pixel_bounds_for_utf16_offset(2, Point::new(gpui::px(0.0), gpui::px(0.0)), cx)
        });
        assert_eq!(
            result,
            Some(Bounds {
                origin: Point::new(gpui::px(16.0), gpui::px(0.0)),
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
            ed.set_file_path(Some(std::path::PathBuf::from("/ws/a.rs")), cx)
        });
        cx.run_until_parked();

        assert_eq!(
            editor.read_with(&cx, |ed, _| ed.file_path().map(|p| p.to_path_buf())),
            Some(std::path::PathBuf::from("/ws/a.rs")),
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

        let path = std::path::PathBuf::from("/ws/a.rs");
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
}

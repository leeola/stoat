use crate::input_state_machine::InputStateMachine;
use gpui::{
    App, Bounds, Context, EntityInputHandler, FocusHandle, Focusable, Pixels, Point,
    UTF16Selection, WeakEntity, Window,
};
use std::ops::Range;
use stoat::DisplayPoint;
use stoat_text::{Bias, OffsetUtf16};

/// Entity that bridges the platform's IME pipeline to the
/// workspace-hosted [`InputStateMachine`]. The render path wraps
/// it in `gpui::ElementInputHandler` and registers it via
/// `Window::handle_input` against [`EditorInput::focus_handle`].
///
/// Only the three IME write paths
/// (`replace_text_in_range`, `replace_and_mark_text_in_range`,
/// `unmark_text`) carry behavior today: each forwards into the
/// state machine, which mode-gates the call. Read-side queries
/// (`text_for_range`, `selected_text_range`,
/// `character_index_for_point`) and `bounds_for_range` return
/// `None` until the editor exposes a selection / text-by-utf16
/// surface and the active-editor lookup lands.
pub struct EditorInput {
    state_machine: WeakEntity<InputStateMachine>,
    focus_handle: FocusHandle,
}

impl EditorInput {
    pub fn new(state_machine: WeakEntity<InputStateMachine>, cx: &mut Context<'_, Self>) -> Self {
        Self {
            state_machine,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    pub fn state_machine(&self) -> &WeakEntity<InputStateMachine> {
        &self.state_machine
    }
}

impl Focusable for EditorInput {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for EditorInput {
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(sm) = self.state_machine.upgrade() else {
            return;
        };
        let text = text.to_string();
        let actions = sm.update(cx, |sm, cx| sm.text_input(&text, range, window, cx));
        if actions.is_empty() {
            return;
        }
        let Some(workspace) = sm.read(cx).workspace().upgrade() else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            for action in actions {
                workspace.dispatch_action(action, window, cx);
            }
        });
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(sm) = self.state_machine.upgrade() {
            let text = new_text.to_string();
            sm.update(cx, |sm, cx| {
                sm.composition_update(&text, range, new_selected_range, cx)
            });
        }
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(sm) = self.state_machine.upgrade() {
            sm.update(cx, |sm, cx| sm.composition_commit(cx));
        }
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Option<Range<usize>> {
        self.state_machine.upgrade()?.read(cx).marked_range()
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Option<UTF16Selection> {
        let sm = self.state_machine.upgrade()?;
        let editor = sm.read(cx).active_editor()?.upgrade()?;
        editor.update(cx, |ed, cx| {
            let selection = ed.selections().all_anchors().first()?.clone();
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let start = snapshot.resolve_anchor(&selection.start);
            let end = snapshot.resolve_anchor(&selection.end);
            let rope = snapshot.rope();
            let start_utf16 = rope.offset_to_offset_utf16(start).0;
            let end_utf16 = rope.offset_to_offset_utf16(end).0;
            Some(UTF16Selection {
                range: start_utf16..end_utf16,
                reversed: selection.reversed,
            })
        })
    }

    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Option<String> {
        let sm = self.state_machine.upgrade()?;
        let editor = sm.read(cx).active_editor()?.upgrade()?;
        editor.update(cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let rope = snapshot.rope();
            let start_byte = rope.offset_utf16_to_offset(OffsetUtf16(range_utf16.start));
            let end_byte = rope.offset_utf16_to_offset(OffsetUtf16(range_utf16.end));
            let actual_start = rope.offset_to_offset_utf16(start_byte).0;
            let actual_end = rope.offset_to_offset_utf16(end_byte).0;
            if actual_start != range_utf16.start || actual_end != range_utf16.end {
                *adjusted_range = Some(actual_start..actual_end);
            }
            Some(rope.chunks_in_range(start_byte..end_byte).collect())
        })
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Option<Bounds<Pixels>> {
        let sm = self.state_machine.upgrade()?;
        let editor = sm.read(cx).active_editor()?.upgrade()?;
        editor.update(cx, |ed, cx| {
            ed.pixel_bounds_for_utf16_offset(range_utf16.start, element_bounds.origin, cx)
        })
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Option<usize> {
        let sm = self.state_machine.upgrade()?;
        let editor = sm.read(cx).active_editor()?.upgrade()?;
        editor.update(cx, |ed, cx| {
            let cell = ed.cell_size()?;
            let bounds = ed.text_region_bounds()?;
            if !bounds.contains(&point) {
                return None;
            }
            let local = Point::new(point.x - bounds.origin.x, point.y - bounds.origin.y);
            let (row, col) = crate::editor::mouse::point_to_grid(local, cell);
            let display_snapshot = ed.display_map().update(cx, |dm, _| dm.snapshot());
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let clipped = display_snapshot.clip_point(DisplayPoint::new(row, col), Bias::Left);
            let buffer_point = display_snapshot.display_to_buffer(clipped)?;
            let byte_offset = snapshot.rope().point_to_offset(buffer_point);
            Some(snapshot.rope().offset_to_offset_utf16(byte_offset).0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;
    use gpui::{AppContext, Entity, TestAppContext, VisualTestContext};
    use std::path::PathBuf;
    use stoat::keymap::{Keymap, StateValue};
    use stoat_config::Config;

    fn empty_keymap() -> Keymap {
        Keymap::compile(&Config {
            blocks: Vec::new(),
            themes: Vec::new(),
        })
    }

    fn new_state_machine_in_window(
        cx: &mut TestAppContext,
    ) -> (
        Entity<InputStateMachine>,
        Entity<Workspace>,
        &mut VisualTestContext,
    ) {
        let (workspace, vcx) =
            cx.add_window_view(|_, cx| Workspace::new("main", PathBuf::from("/tmp/repo"), cx));
        let sm = workspace.read_with(vcx, |w, _| w.input_state_machine().clone());
        sm.update(vcx, |sm, _| sm.set_keymap(empty_keymap()));
        (sm, workspace, vcx)
    }

    fn new_editor_input(
        sm: &Entity<InputStateMachine>,
        vcx: &mut VisualTestContext,
    ) -> Entity<EditorInput> {
        let weak = sm.downgrade();
        vcx.update(|_, cx| cx.new(|cx| EditorInput::new(weak, cx)))
    }

    fn set_mode(vcx: &mut VisualTestContext, sm: &Entity<InputStateMachine>, mode: &str) {
        let mode = mode.to_string();
        sm.update(vcx, |sm, _| {
            sm.set_mode_for_test(StateValue::String(mode.into()));
        });
    }

    #[test]
    fn replace_text_in_range_forwards_to_state_machine() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        set_mode(vcx, &sm, "insert");
        let editor_input = new_editor_input(&sm, vcx);

        editor_input.update_in(vcx, |ei, window, cx| {
            ei.replace_text_in_range(None, "hi", window, cx);
        });

        sm.read_with(vcx, |sm, _| assert_eq!(sm.last_text_input(), Some("hi")));
    }

    #[test]
    fn replace_and_mark_text_in_range_forwards() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        set_mode(vcx, &sm, "insert");
        let editor_input = new_editor_input(&sm, vcx);

        editor_input.update_in(vcx, |ei, window, cx| {
            ei.replace_and_mark_text_in_range(Some(3..5), "ka", Some(0..2), window, cx);
        });

        sm.read_with(vcx, |sm, _| {
            assert_eq!(sm.marked_text(), Some("ka"));
            assert_eq!(sm.marked_range(), Some(3..5));
        });
    }

    #[test]
    fn unmark_text_clears_composition() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        set_mode(vcx, &sm, "insert");
        let editor_input = new_editor_input(&sm, vcx);

        editor_input.update_in(vcx, |ei, window, cx| {
            ei.replace_and_mark_text_in_range(Some(0..2), "ka", Some(0..2), window, cx);
            ei.unmark_text(window, cx);
        });

        sm.read_with(vcx, |sm, _| {
            assert_eq!(sm.marked_text(), None);
            assert_eq!(sm.marked_range(), None);
        });
    }

    #[test]
    fn marked_text_range_reads_state_machine() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        set_mode(vcx, &sm, "insert");
        sm.update(vcx, |sm, cx| {
            sm.composition_update("ka", Some(3..5), None, cx);
        });
        let editor_input = new_editor_input(&sm, vcx);

        let range = editor_input.update_in(vcx, |ei, window, cx| ei.marked_text_range(window, cx));

        assert_eq!(range, Some(3..5));
    }

    #[test]
    fn replace_text_in_range_not_recorded_in_normal_mode() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        let editor_input = new_editor_input(&sm, vcx);

        editor_input.update_in(vcx, |ei, window, cx| {
            ei.replace_text_in_range(None, "hi", window, cx);
        });

        sm.read_with(vcx, |sm, _| assert_eq!(sm.last_text_input(), None));
    }

    #[test]
    fn editor_input_exposes_focus_handle() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        let editor_input = new_editor_input(&sm, vcx);

        let _handle = editor_input.read_with(vcx, |ei, _| ei.focus_handle().clone());
    }

    fn new_singleton_editor(
        vcx: &mut VisualTestContext,
        text: &str,
    ) -> Entity<crate::editor::Editor> {
        use crate::{
            buffer::Buffer,
            diff_map::DiffMap,
            display_map::DisplayMap,
            editor::{Editor, EditorMode},
            multi_buffer::MultiBuffer,
        };
        use std::sync::Arc;
        use stoat::buffer::BufferId;
        use stoat_scheduler::{Executor, TestScheduler};

        let buffer = vcx.update(|_, cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)));
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let multi_buffer = {
            let buffer = buffer.clone();
            vcx.update(|_, cx| cx.new(|cx| MultiBuffer::singleton(buffer, cx)))
        };
        let display_map = {
            let buffer = buffer.clone();
            vcx.update(|_, cx| cx.new(|cx| DisplayMap::new(buffer, executor, cx)))
        };
        let diff_map = vcx.update(|_, cx| cx.new(|cx| DiffMap::new(buffer, cx)));
        vcx.update(|_, cx| {
            cx.new(|cx| Editor::new(multi_buffer, display_map, diff_map, EditorMode::full(), cx))
        })
    }

    fn unit_bounds() -> Bounds<Pixels> {
        Bounds {
            origin: Point::new(gpui::px(0.0), gpui::px(0.0)),
            size: gpui::size(gpui::px(100.0), gpui::px(100.0)),
        }
    }

    #[test]
    fn bounds_for_range_returns_none_without_active_editor() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        let editor_input = new_editor_input(&sm, vcx);

        let result = editor_input.update_in(vcx, |ei, window, cx| {
            ei.bounds_for_range(0..0, unit_bounds(), window, cx)
        });
        assert_eq!(result, None);
    }

    #[test]
    fn bounds_for_range_returns_none_when_editor_lacks_cell_size() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        let editor = new_singleton_editor(vcx, "hello");
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        let editor_input = new_editor_input(&sm, vcx);

        let result = editor_input.update_in(vcx, |ei, window, cx| {
            ei.bounds_for_range(0..0, unit_bounds(), window, cx)
        });
        assert_eq!(result, None);
    }

    fn seed_primary_range(
        vcx: &mut VisualTestContext,
        editor: &Entity<crate::editor::Editor>,
        range: Range<usize>,
        reversed: bool,
    ) {
        use stoat_text::{Bias, Selection, SelectionGoal};
        editor.update(vcx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let start = snapshot.anchor_at(range.start, Bias::Left);
            let end = snapshot.anchor_at(range.end, Bias::Left);
            ed.selections_mut().replace_with(
                vec![Selection {
                    id: 1,
                    start,
                    end,
                    reversed,
                    goal: SelectionGoal::None,
                }],
                &snapshot,
            );
        });
    }

    #[test]
    fn selected_text_range_returns_none_without_active_editor() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        let editor_input = new_editor_input(&sm, vcx);

        let result = editor_input.update_in(vcx, |ei, window, cx| {
            ei.selected_text_range(false, window, cx)
        });
        assert!(result.is_none());
    }

    #[test]
    fn selected_text_range_reports_primary_selection_offsets() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        let editor = new_singleton_editor(vcx, "hello");
        seed_primary_range(vcx, &editor, 1..4, false);
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        let editor_input = new_editor_input(&sm, vcx);

        let result = editor_input
            .update_in(vcx, |ei, window, cx| {
                ei.selected_text_range(false, window, cx)
            })
            .expect("selection range");
        assert_eq!(result.range, 1..4);
        assert!(!result.reversed);
    }

    #[test]
    fn selected_text_range_translates_to_utf16_across_surrogate() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        let editor = new_singleton_editor(vcx, "a\u{1f600}b");
        seed_primary_range(vcx, &editor, 1..5, false);
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        let editor_input = new_editor_input(&sm, vcx);

        let result = editor_input
            .update_in(vcx, |ei, window, cx| {
                ei.selected_text_range(false, window, cx)
            })
            .expect("selection range");
        assert_eq!(result.range, 1..3);
        assert!(!result.reversed);
    }

    #[test]
    fn selected_text_range_reports_reversed_flag() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        let editor = new_singleton_editor(vcx, "hello");
        seed_primary_range(vcx, &editor, 1..4, true);
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        let editor_input = new_editor_input(&sm, vcx);

        let result = editor_input
            .update_in(vcx, |ei, window, cx| {
                ei.selected_text_range(false, window, cx)
            })
            .expect("selection range");
        assert_eq!(result.range, 1..4);
        assert!(result.reversed);
    }

    #[test]
    fn text_for_range_returns_none_without_active_editor() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        let editor_input = new_editor_input(&sm, vcx);

        let mut adjusted: Option<Range<usize>> = None;
        let result = editor_input.update_in(vcx, |ei, window, cx| {
            ei.text_for_range(0..1, &mut adjusted, window, cx)
        });
        assert_eq!(result, None);
        assert_eq!(adjusted, None);
    }

    #[test]
    fn text_for_range_returns_buffer_substring() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        let editor = new_singleton_editor(vcx, "hello world");
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        let editor_input = new_editor_input(&sm, vcx);

        let mut adjusted: Option<Range<usize>> = None;
        let result = editor_input.update_in(vcx, |ei, window, cx| {
            ei.text_for_range(0..5, &mut adjusted, window, cx)
        });
        assert_eq!(result.as_deref(), Some("hello"));
        assert_eq!(adjusted, None);
    }

    #[test]
    fn text_for_range_handles_utf16_offsets() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        let editor = new_singleton_editor(vcx, "a\u{1f600}b");
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        let editor_input = new_editor_input(&sm, vcx);

        let mut adjusted: Option<Range<usize>> = None;
        let result = editor_input.update_in(vcx, |ei, window, cx| {
            ei.text_for_range(1..3, &mut adjusted, window, cx)
        });
        assert_eq!(result.as_deref(), Some("\u{1f600}"));
        assert_eq!(adjusted, None);
    }

    #[test]
    fn text_for_range_adjusts_when_boundary_straddles_surrogate() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        let editor = new_singleton_editor(vcx, "a\u{1f600}b");
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        let editor_input = new_editor_input(&sm, vcx);

        let mut adjusted: Option<Range<usize>> = None;
        let result = editor_input.update_in(vcx, |ei, window, cx| {
            ei.text_for_range(0..2, &mut adjusted, window, cx)
        });
        assert_eq!(result.as_deref(), Some("a\u{1f600}"));
        assert_eq!(adjusted, Some(0..3));
    }

    #[test]
    fn text_for_range_clamps_to_buffer_length() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        let editor = new_singleton_editor(vcx, "abc");
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        let editor_input = new_editor_input(&sm, vcx);

        let mut adjusted: Option<Range<usize>> = None;
        let result = editor_input.update_in(vcx, |ei, window, cx| {
            ei.text_for_range(1..10, &mut adjusted, window, cx)
        });
        assert_eq!(result.as_deref(), Some("bc"));
        assert_eq!(adjusted, Some(1..3));
    }

    fn region_bounds() -> Bounds<Pixels> {
        Bounds {
            origin: Point::new(gpui::px(10.0), gpui::px(20.0)),
            size: gpui::size(gpui::px(100.0), gpui::px(100.0)),
        }
    }

    #[test]
    fn character_index_for_point_returns_none_without_active_editor() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        let editor_input = new_editor_input(&sm, vcx);

        let result = editor_input.update_in(vcx, |ei, window, cx| {
            ei.character_index_for_point(Point::new(gpui::px(10.0), gpui::px(20.0)), window, cx)
        });
        assert_eq!(result, None);
    }

    #[test]
    fn character_index_for_point_returns_none_without_cell_size() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        let editor = new_singleton_editor(vcx, "hello");
        editor.update(vcx, |ed, cx| ed.set_text_region_bounds(region_bounds(), cx));
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        let editor_input = new_editor_input(&sm, vcx);

        let result = editor_input.update_in(vcx, |ei, window, cx| {
            ei.character_index_for_point(Point::new(gpui::px(10.0), gpui::px(20.0)), window, cx)
        });
        assert_eq!(result, None);
    }

    #[test]
    fn character_index_for_point_returns_none_without_text_region_bounds() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        let editor = new_singleton_editor(vcx, "hello");
        editor.update(vcx, |ed, cx| {
            ed.set_cell_size(gpui::size(gpui::px(8.0), gpui::px(16.0)), cx)
        });
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        let editor_input = new_editor_input(&sm, vcx);

        let result = editor_input.update_in(vcx, |ei, window, cx| {
            ei.character_index_for_point(Point::new(gpui::px(10.0), gpui::px(20.0)), window, cx)
        });
        assert_eq!(result, None);
    }

    #[test]
    fn character_index_for_point_returns_none_when_point_outside_bounds() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        let editor = new_singleton_editor(vcx, "hello");
        editor.update(vcx, |ed, cx| {
            ed.set_cell_size(gpui::size(gpui::px(8.0), gpui::px(16.0)), cx);
            ed.set_text_region_bounds(region_bounds(), cx);
        });
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        let editor_input = new_editor_input(&sm, vcx);

        let result = editor_input.update_in(vcx, |ei, window, cx| {
            ei.character_index_for_point(Point::new(gpui::px(0.0), gpui::px(0.0)), window, cx)
        });
        assert_eq!(result, None);
    }

    #[test]
    fn character_index_for_point_maps_point_to_utf16_offset() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        let editor = new_singleton_editor(vcx, "hello");
        editor.update(vcx, |ed, cx| {
            ed.set_cell_size(gpui::size(gpui::px(8.0), gpui::px(16.0)), cx);
            ed.set_text_region_bounds(region_bounds(), cx);
        });
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        let editor_input = new_editor_input(&sm, vcx);

        let result = editor_input
            .update_in(vcx, |ei, window, cx| {
                ei.character_index_for_point(
                    Point::new(gpui::px(10.0 + 16.0), gpui::px(20.0)),
                    window,
                    cx,
                )
            })
            .expect("offset");
        assert_eq!(result, 2);
    }

    #[test]
    fn character_index_for_point_origin_maps_to_offset_zero() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        let editor = new_singleton_editor(vcx, "a\u{1f600}b");
        editor.update(vcx, |ed, cx| {
            ed.set_cell_size(gpui::size(gpui::px(8.0), gpui::px(16.0)), cx);
            ed.set_text_region_bounds(region_bounds(), cx);
        });
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        let editor_input = new_editor_input(&sm, vcx);

        let result = editor_input
            .update_in(vcx, |ei, window, cx| {
                ei.character_index_for_point(Point::new(gpui::px(10.0), gpui::px(20.0)), window, cx)
            })
            .expect("offset");
        assert_eq!(result, 0);
    }

    #[test]
    fn bounds_for_range_returns_bounds_when_editor_has_cell_size() {
        let mut cx = TestAppContext::single();
        let (sm, _ws, vcx) = new_state_machine_in_window(&mut cx);
        let editor = new_singleton_editor(vcx, "hello");
        editor.update(vcx, |ed, cx| {
            ed.set_cell_size(gpui::size(gpui::px(8.0), gpui::px(16.0)), cx)
        });
        sm.update(vcx, |sm, _| sm.set_active_editor(Some(editor.downgrade())));
        let editor_input = new_editor_input(&sm, vcx);

        let bounds = Bounds {
            origin: Point::new(gpui::px(10.0), gpui::px(20.0)),
            size: gpui::size(gpui::px(100.0), gpui::px(100.0)),
        };
        let result = editor_input.update_in(vcx, |ei, window, cx| {
            ei.bounds_for_range(2..2, bounds, window, cx)
        });
        assert_eq!(
            result,
            Some(Bounds {
                origin: Point::new(gpui::px(10.0 + 16.0), gpui::px(20.0)),
                size: gpui::size(gpui::px(8.0), gpui::px(16.0)),
            }),
        );
    }
}

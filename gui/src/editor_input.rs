use crate::input_state_machine::InputStateMachine;
use gpui::{
    App, Bounds, Context, EntityInputHandler, FocusHandle, Focusable, Pixels, Point,
    UTF16Selection, WeakEntity, Window,
};
use std::ops::Range;

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
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(sm) = self.state_machine.upgrade() {
            let text = text.to_string();
            sm.update(cx, |sm, cx| sm.text_input(&text, range, cx));
        }
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
        _cx: &mut Context<'_, Self>,
    ) -> Option<UTF16Selection> {
        None
    }

    fn text_for_range(
        &mut self,
        _range: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Option<String> {
        None
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Option<Bounds<Pixels>> {
        // FIXME: needs the active-editor lookup that lands with the
        // editor-as-ItemView item; without it the IME has no buffer
        // to translate UTF-16 offsets against.
        None
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<'_, Self>,
    ) -> Option<usize> {
        None
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
    fn replace_text_in_range_dropped_in_normal_mode() {
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
}

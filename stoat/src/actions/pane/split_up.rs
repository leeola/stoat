use crate::{
    editor::view::EditorView, pane::SplitDirection, pane_group::view::PaneGroupView, stoat::Stoat,
};
use gpui::{AppContext, Context, Focusable, Window};
use tracing::debug;

impl PaneGroupView {
    pub(crate) fn handle_split_up(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        debug!(
            active_pane = self.active_pane,
            direction = "Up",
            "Splitting pane"
        );

        let new_stoat = if let Some(active_editor) = self
            .pane_contents
            .get(&self.active_pane)
            .and_then(|content| content.as_editor())
        {
            cx.new(|cx| active_editor.read(cx).stoat.read(cx).clone_for_split())
        } else {
            cx.new(|cx| {
                Stoat::new(
                    crate::Config::default(),
                    self.app_state.worktree.clone(),
                    self.app_state.buffer_store.clone(),
                    Some(self.app_state.lsp_manager.clone()),
                    self.compiled_keymap.clone(),
                    cx,
                )
            })
        };
        let new_editor = cx.new(|cx| EditorView::new(new_stoat, cx));
        {
            let stoat = new_editor.read(cx).stoat.clone();
            self.subscribe_to_stoat(&stoat, cx);
        }

        new_editor.update(cx, |view, _| {
            view.set_entity(new_editor.clone());
        });

        self.split(SplitDirection::Up, new_editor.clone(), cx);

        debug!(
            new_pane = self.active_pane,
            "Split complete, focusing new pane"
        );

        window.focus(&new_editor.read(cx).focus_handle(cx));

        self.update_minimap_to_active_pane(cx);

        self.exit_pane_mode(cx);

        cx.notify();
    }
}

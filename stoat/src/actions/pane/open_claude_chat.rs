use crate::{
    claude::{
        state::ClaudeState,
        view::{ClaudeView, ClaudeViewEvent},
    },
    content_view::PaneContent,
    pane::SplitDirection,
    pane_group::view::PaneGroupView,
    stoat::KeyContext,
};
use gpui::{AppContext, Context, Focusable, Window};
use tracing::debug;

impl PaneGroupView {
    pub(crate) fn handle_open_claude_chat(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let visible_claude = self
            .pane_contents
            .iter()
            .find(|(_, content)| matches!(content, PaneContent::Claude(_)))
            .map(|(&id, _)| id);

        if let Some(pane_id) = visible_claude {
            if self.active_pane == pane_id {
                self.hide_claude_pane(window, cx);
                return;
            }
            if let Some(PaneContent::Claude(claude_view)) = self.pane_contents.get(&pane_id) {
                self.active_pane = pane_id;
                let stoat = claude_view.read(cx).stoat().clone();
                stoat.update(cx, |s, _| s.set_key_context(KeyContext::Claude));
                stoat.update(cx, |s, _| s.set_mode("normal"));
                window.focus(&claude_view.read(cx).focus_handle(cx));
                cx.notify();
            }
            return;
        }

        if let Some((state, view)) = self.hidden_claude.take() {
            debug!("Restoring hidden Claude chat panel");
            self.restore_claude_pane(state, view, window, cx);
            return;
        }

        debug!("Opening Claude chat panel");
        self.create_claude_pane(window, cx);
    }

    fn create_claude_pane(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let workdir = self
            .app_state
            .worktree
            .lock()
            .root()
            .to_string_lossy()
            .to_string();

        let Some(stoat) = self.active_stoat(cx) else {
            debug!("No active Stoat entity for Claude pane");
            return;
        };

        let state = cx.new(ClaudeState::new);
        let claude_view = {
            let stoat = stoat.clone();
            cx.new(|cx| ClaudeView::new(state.clone(), stoat, cx))
        };

        let claude_view_for_sub = claude_view.clone();
        cx.subscribe(&claude_view_for_sub, |this, _view, event, cx| match event {
            ClaudeViewEvent::CloseRequested => {
                this.hide_claude_pane_no_window(cx);
            },
        })
        .detach();

        let new_pane_id = self
            .pane_group
            .split(self.active_pane, SplitDirection::Right);
        self.pane_contents
            .insert(new_pane_id, PaneContent::Claude(claude_view.clone()));
        self.active_pane = new_pane_id;

        stoat.update(cx, |s, _| s.set_key_context(KeyContext::Claude));
        stoat.update(cx, |s, _| s.set_mode("normal"));
        window.focus(&claude_view.read(cx).focus_handle(cx));

        state.update(cx, |s, cx| s.start(workdir, cx));

        self.exit_pane_mode(cx);
        cx.notify();
    }

    fn restore_claude_pane(
        &mut self,
        _state: gpui::Entity<ClaudeState>,
        view: gpui::Entity<ClaudeView>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let view_for_sub = view.clone();
        cx.subscribe(&view_for_sub, |this, _view, event, cx| match event {
            ClaudeViewEvent::CloseRequested => {
                this.hide_claude_pane_no_window(cx);
            },
        })
        .detach();

        let new_pane_id = self
            .pane_group
            .split(self.active_pane, SplitDirection::Right);
        self.pane_contents
            .insert(new_pane_id, PaneContent::Claude(view.clone()));
        self.active_pane = new_pane_id;

        let stoat = view.read(cx).stoat().clone();
        stoat.update(cx, |s, _| s.set_key_context(KeyContext::Claude));
        stoat.update(cx, |s, _| s.set_mode("normal"));
        window.focus(&view.read(cx).focus_handle(cx));

        self.exit_pane_mode(cx);
        cx.notify();
    }

    pub(crate) fn handle_cycle_claude_permission(&mut self, cx: &mut Context<'_, Self>) {
        let claude_pane = self
            .pane_contents
            .iter()
            .find(|(_, content)| matches!(content, PaneContent::Claude(_)));

        if let Some((_, PaneContent::Claude(view))) = claude_pane {
            let state = view.read(cx).state_entity().clone();
            state.update(cx, |s, cx| s.cycle_permission_mode(cx));
        }
    }

    pub(crate) fn hide_claude_pane(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let claude_pane = self
            .pane_contents
            .iter()
            .find(|(_, content)| matches!(content, PaneContent::Claude(_)))
            .map(|(&id, _)| id);

        if let Some(pane_id) = claude_pane {
            if let Some(PaneContent::Claude(view)) = self.pane_contents.remove(&pane_id) {
                let stoat = view.read(cx).stoat().clone();
                stoat.update(cx, |s, _| s.set_key_context(KeyContext::TextEditor));
                stoat.update(cx, |s, _| s.set_mode("normal"));
                let state = view.read(cx).state_entity().clone();
                self.hidden_claude = Some((state, view));
            }

            let _ = self.pane_group.remove(pane_id);

            if self.active_pane == pane_id {
                if let Some(&new_active) = self.pane_group.panes().first() {
                    self.active_pane = new_active;
                    if let Some(content) = self.pane_contents.get(&new_active) {
                        match content {
                            PaneContent::Editor(editor) => {
                                window.focus(&editor.read(cx).focus_handle(cx));
                            },
                            PaneContent::Claude(claude) => {
                                window.focus(&claude.read(cx).focus_handle(cx));
                            },
                            PaneContent::Static(static_view) => {
                                window.focus(&static_view.read(cx).focus_handle(cx));
                            },
                        }
                    }
                }
            }

            cx.notify();
        }
    }

    fn hide_claude_pane_no_window(&mut self, cx: &mut Context<'_, Self>) {
        let claude_pane = self
            .pane_contents
            .iter()
            .find(|(_, content)| matches!(content, PaneContent::Claude(_)))
            .map(|(&id, _)| id);

        if let Some(pane_id) = claude_pane {
            if let Some(PaneContent::Claude(view)) = self.pane_contents.remove(&pane_id) {
                let stoat = view.read(cx).stoat().clone();
                stoat.update(cx, |s, _| s.set_key_context(KeyContext::TextEditor));
                stoat.update(cx, |s, _| s.set_mode("normal"));
                let state = view.read(cx).state_entity().clone();
                self.hidden_claude = Some((state, view));
            }

            let _ = self.pane_group.remove(pane_id);

            if self.active_pane == pane_id {
                if let Some(&new_active) = self.pane_group.panes().first() {
                    self.active_pane = new_active;
                }
            }

            cx.notify();
        }
    }
}

use crate::{
    claude::{
        state::ClaudeState,
        view::{ClaudeView, ClaudeViewEvent},
    },
    content_view::PaneContent,
    pane::SplitDirection,
    pane_group::view::PaneGroupView,
};
use gpui::{AppContext, Context, Focusable, Window};
use tracing::debug;

impl PaneGroupView {
    pub(crate) fn handle_open_claude_chat(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // If a Claude pane already exists, focus it
        for (&pane_id, content) in &self.pane_contents {
            if let PaneContent::Claude(claude_view) = content {
                self.active_pane = pane_id;
                window.focus(&claude_view.read(cx).focus_handle(cx));
                cx.notify();
                return;
            }
        }

        debug!("Opening Claude chat panel");

        let workdir = self
            .app_state
            .worktree
            .lock()
            .root()
            .to_string_lossy()
            .to_string();

        let state = cx.new(ClaudeState::new);

        let claude_view = cx.new(|cx| ClaudeView::new(state.clone(), cx));

        // Subscribe to close events from ClaudeView
        let claude_view_for_sub = claude_view.clone();
        cx.subscribe(&claude_view_for_sub, |this, _view, event, cx| match event {
            ClaudeViewEvent::CloseRequested => {
                this.handle_close_claude_pane(cx);
            },
        })
        .detach();

        let new_pane_id = self
            .pane_group
            .split(self.active_pane, SplitDirection::Right);
        self.pane_contents
            .insert(new_pane_id, PaneContent::Claude(claude_view.clone()));
        self.active_pane = new_pane_id;

        window.focus(&claude_view.read(cx).focus_handle(cx));

        // Start the Claude process
        state.update(cx, |s, cx| s.start(workdir, cx));

        self.exit_pane_mode(cx);
        cx.notify();
    }

    fn handle_close_claude_pane(&mut self, cx: &mut Context<'_, Self>) {
        let claude_pane = self
            .pane_contents
            .iter()
            .find(|(_, content)| matches!(content, PaneContent::Claude(_)))
            .map(|(&id, _)| id);

        if let Some(pane_id) = claude_pane {
            if self.pane_group.remove(pane_id).is_ok() {
                self.pane_contents.remove(&pane_id);

                if self.active_pane == pane_id {
                    if let Some(&new_active) = self.pane_group.panes().first() {
                        self.active_pane = new_active;
                    }
                }

                cx.notify();
            }
        }
    }
}

use crate::{pane_group::view::PaneGroupView, quick_input::QuickInputEvent};
use gpui::{AppContext, Context, Focusable, Window};

impl PaneGroupView {
    pub(crate) fn handle_git_log_search_open(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let search_input = cx.new(crate::quick_input::QuickInput::new);

        cx.subscribe_in(&search_input, window, {
            move |this, _input, event: &QuickInputEvent, window, cx| match event {
                QuickInputEvent::Changed(text) => {
                    this.app_state.git_log.search_query = text.clone();
                    this.recompute_git_log_search_matches();
                    cx.notify();
                },
                QuickInputEvent::Confirm(text) => {
                    this.app_state.git_log.search_query = text.clone();
                    this.handle_git_log_search_confirm(window, cx);
                    this.app_state.git_log.search_input = None;
                },
                QuickInputEvent::Dismiss => {
                    this.app_state.git_log.search_query.clear();
                    this.app_state.git_log.search_matches.clear();
                    this.app_state.git_log.search_input = None;

                    if let Some(editor) = this.active_editor().cloned() {
                        editor.update(cx, |editor, cx| {
                            editor.stoat.update(cx, |stoat, cx| {
                                stoat.set_mode_by_name("git_log", cx);
                            });
                        });
                        window.focus(&editor.focus_handle(cx), cx);
                    }
                    cx.notify();
                },
            }
        })
        .detach();

        let focus_handle = search_input.focus_handle(cx);
        self.app_state.git_log.search_input = Some(search_input);

        if let Some(editor) = self.active_editor().cloned() {
            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, cx| {
                    stoat.set_mode_by_name("git_log_search", cx);
                });
            });
        }

        window.focus(&focus_handle, cx);
        cx.notify();
    }

    pub(crate) fn handle_git_log_search_confirm(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.recompute_git_log_search_matches();

        if let Some(&first) = self.app_state.git_log.search_matches.first() {
            self.app_state.git_log.selected = first;
        }

        if let Some(editor) = self.active_editor().cloned() {
            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, cx| {
                    stoat.set_mode_by_name("git_log", cx);
                });
            });
        }

        self.load_git_log_detail_for_selected(cx);
    }

    pub(crate) fn recompute_git_log_search_matches(&mut self) {
        let query = self.app_state.git_log.search_query.to_lowercase();
        if query.is_empty() {
            self.app_state.git_log.search_matches.clear();
        } else {
            self.app_state.git_log.search_matches = self
                .app_state
                .git_log
                .commits
                .iter()
                .enumerate()
                .filter(|(_, c)| c.message.to_lowercase().contains(&query))
                .map(|(i, _)| i)
                .collect();
        }
    }
}

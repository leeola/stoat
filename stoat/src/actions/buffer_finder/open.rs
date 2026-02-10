use crate::{pane_group::view::PaneGroupView, stoat::KeyContext};
use gpui::Context;
use nucleo_matcher::{
    pattern::{CaseMatching, Normalization, Pattern},
    Matcher,
};

impl PaneGroupView {
    pub(crate) fn handle_open_buffer_finder(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut Context<'_, Self>,
    ) {
        let editor_opt = self.active_editor().cloned();
        if let Some(editor) = editor_opt {
            let (current_mode, current_key_context) = {
                let stoat = editor.read(cx).stoat.read(cx);
                (stoat.mode().to_string(), stoat.key_context())
            };

            self.app_state
                .open_buffer_finder(current_mode, current_key_context, cx);

            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, _cx| {
                    stoat.set_key_context(KeyContext::BufferFinder);
                    stoat.set_mode("buffer_finder");
                    stoat.buffer_finder_input_ref = self.app_state.buffer_finder.input.clone();
                });
            });

            self.update_buffer_finder_list(cx);

            cx.notify();
        }
    }

    pub(crate) fn update_buffer_finder_list(&mut self, cx: &mut Context<'_, Self>) {
        let active_id = self
            .active_editor()
            .and_then(|editor| editor.read(cx).stoat.read(cx).active_buffer_id(cx));

        let visible_ids: Vec<text::BufferId> = self
            .pane_contents
            .values()
            .filter_map(|content| content.as_editor())
            .filter_map(|editor| editor.read(cx).stoat.read(cx).active_buffer_id(cx))
            .collect();

        let buffers = self
            .app_state
            .buffer_store
            .read(cx)
            .buffer_list(active_id, &visible_ids, cx);
        self.app_state.buffer_finder.buffers = buffers.clone();

        let query = self
            .app_state
            .buffer_finder
            .input
            .as_ref()
            .map(|buffer| buffer.read(cx).text())
            .unwrap_or_default();

        self.filter_buffer_finder_buffers(&query);
    }

    pub(crate) fn filter_buffer_finder_buffers(&mut self, query: &str) {
        let all_buffers = &self.app_state.buffer_finder.buffers;

        if query.is_empty() {
            self.app_state.buffer_finder.filtered = all_buffers.clone();
        } else {
            let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
            let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);

            let candidates: Vec<String> = all_buffers
                .iter()
                .map(|buf| buf.display_name.clone())
                .collect();

            let candidate_refs: Vec<&str> = candidates.iter().map(|s| s.as_str()).collect();
            let mut matches = pattern.match_list(candidate_refs, &mut matcher);
            matches.sort_by(|a, b| b.1.cmp(&a.1));

            self.app_state.buffer_finder.filtered = matches
                .into_iter()
                .filter_map(|(matched_str, _score)| {
                    candidates
                        .iter()
                        .position(|s| s.as_str() == matched_str)
                        .and_then(|idx| all_buffers.get(idx).cloned())
                })
                .collect();
        }
    }
}

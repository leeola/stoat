//! Enter line selection mode for partial hunk staging.

use crate::{
    git::{diff::extract_hunk_lines, line_selection::LineSelection},
    stoat::Stoat,
};
use gpui::Context;

impl Stoat {
    /// Enter line selection mode on the current hunk.
    ///
    /// Extracts individual lines from the current hunk and creates a [`LineSelection`]
    /// with all changeable lines selected. Sets the mode to `line_select`.
    pub fn diff_review_enter_line_select(&mut self, cx: &mut Context<Self>) {
        if self.mode != "diff_review" {
            return;
        }

        let hunk_index = self.review_state.hunk_idx;

        let (base_text, buffer_text) = {
            let buffer_item = self.active_buffer(cx);
            let item = buffer_item.read(cx);
            let diff = match item.diff() {
                Some(d) => d,
                None => return,
            };
            let buffer_snapshot = item.buffer().read(cx).snapshot();
            let buffer_text = buffer_snapshot.text().to_string();
            (diff.base_text.clone(), buffer_text)
        };

        match extract_hunk_lines(&base_text, &buffer_text, hunk_index) {
            Ok(hunk_lines) => {
                if hunk_lines.lines.is_empty() {
                    return;
                }
                self.line_selection = Some(LineSelection::new(hunk_lines));
                self.set_mode_by_name("line_select", cx);
                cx.emit(crate::stoat::StoatEvent::Changed);
                cx.notify();
            },
            Err(e) => {
                tracing::error!("Failed to extract hunk lines: {e}");
            },
        }
    }
}

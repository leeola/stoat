//! Cancel line selection mode.

use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Cancel line selection and return to diff review mode.
    pub fn diff_review_line_select_cancel(&mut self, cx: &mut Context<Self>) {
        self.line_selection = None;
        self.set_mode_by_name("diff_review", cx);
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }
}

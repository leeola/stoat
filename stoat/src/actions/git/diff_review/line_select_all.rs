//! Select all / deselect all in line selection mode.

use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    pub fn diff_review_line_select_all(&mut self, cx: &mut Context<Self>) {
        if let Some(sel) = &mut self.line_selection {
            sel.select_all();
            cx.emit(crate::stoat::StoatEvent::Changed);
            cx.notify();
        }
    }

    pub fn diff_review_line_select_none(&mut self, cx: &mut Context<Self>) {
        if let Some(sel) = &mut self.line_selection {
            sel.deselect_all();
            cx.emit(crate::stoat::StoatEvent::Changed);
            cx.notify();
        }
    }
}

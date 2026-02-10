//! Toggle line selection in line selection mode.

use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    pub fn diff_review_line_select_toggle(&mut self, cx: &mut Context<Self>) {
        if let Some(sel) = &mut self.line_selection {
            sel.toggle_line();
            cx.emit(crate::stoat::StoatEvent::Changed);
            cx.notify();
        }
    }

    pub fn diff_review_line_select_down(&mut self, cx: &mut Context<Self>) {
        if let Some(sel) = &mut self.line_selection {
            sel.move_cursor_down();
            cx.emit(crate::stoat::StoatEvent::Changed);
            cx.notify();
        }
    }

    pub fn diff_review_line_select_up(&mut self, cx: &mut Context<Self>) {
        if let Some(sel) = &mut self.line_selection {
            sel.move_cursor_up();
            cx.emit(crate::stoat::StoatEvent::Changed);
            cx.notify();
        }
    }
}

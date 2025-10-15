//! Enter insert mode action implementation and tests.

use crate::Stoat;
use gpui::Context;

impl Stoat {
    // TODO: Mode transitions probably shouldn't be actions - consider refactoring to internal methods
    /// Enter insert mode.
    ///
    /// Transitions to insert mode where text input is directly inserted into the buffer.
    /// This is the primary mode for editing text content.
    pub fn enter_insert_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = "insert".to_string();
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn enters_insert_mode(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.mode = "normal".to_string();
            s.enter_insert_mode(cx);
            assert_eq!(s.mode(), "insert");
        });
    }
}

//! Enter normal mode action implementation and tests.

use crate::Stoat;
use gpui::Context;

impl Stoat {
    // TODO: Mode transitions probably shouldn't be actions - consider refactoring to internal methods
    /// Enter normal mode.
    ///
    /// Transitions to normal mode where keys trigger movement and command actions instead
    /// of inserting text. This is the default mode for navigation and manipulation.
    pub fn enter_normal_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = "normal".to_string();
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn enters_normal_mode(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.mode = "insert".to_string();
            s.enter_normal_mode(cx);
            assert_eq!(s.mode(), "normal");
        });
    }
}

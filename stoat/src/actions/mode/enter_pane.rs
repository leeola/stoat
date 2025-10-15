//! Enter pane mode action implementation and tests.

use crate::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    // TODO: Mode transitions probably shouldn't be actions - consider refactoring to internal
    // methods
    /// Enter pane mode (window management).
    ///
    /// Transitions to pane mode which enables window management commands like splitting,
    /// focusing, and closing panes.
    pub fn enter_pane_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = "pane".to_string();
        debug!("Entering pane mode");
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn enters_pane_mode(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.mode = "normal".to_string();
            s.enter_pane_mode(cx);
            assert_eq!(s.mode(), "pane");
        });
    }
}

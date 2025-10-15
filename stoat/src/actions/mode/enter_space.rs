//! Enter space mode action implementation and tests.

use crate::Stoat;
use gpui::Context;

impl Stoat {
    // TODO: Mode transitions probably shouldn't be actions - consider refactoring to internal methods
    /// Enter space mode (leader key).
    ///
    /// Transitions to space mode which acts as a leader key for additional command sequences.
    /// This mode is typically accessed by pressing space in normal mode.
    pub fn enter_space_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = "space".to_string();
        tracing::info!("Entering space mode");
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn enters_space_mode(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.mode = "normal".to_string();
            s.enter_space_mode(cx);
            assert_eq!(s.mode(), "space");
        });
    }
}

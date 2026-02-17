//! Enter visual mode action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    // TODO: Mode transitions probably shouldn't be actions - consider refactoring to internal
    // methods
    /// Enter visual mode.
    ///
    /// Transitions to Visual mode for text selection. Movement commands extend the
    /// selection rather than moving the cursor.
    ///
    /// # Behavior
    ///
    /// - Sets editor mode to Visual
    /// - Selection is anchored at current cursor position
    /// - Movement commands now extend selection
    /// - Can transition from Normal or Insert mode
    /// - Typically bound to 'v' key
    ///
    /// # Related
    ///
    /// See also [`Self::enter_normal_mode`] for returning to command mode.
    pub fn enter_visual_mode(&mut self, cx: &mut Context<Self>) {
        self.record_app_state();
        self.set_mode_by_name("visual", cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn enters_visual_mode(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.mode = "normal".to_string();
            s.enter_visual_mode(cx);
            assert_eq!(s.mode(), "visual");
        });
    }
}

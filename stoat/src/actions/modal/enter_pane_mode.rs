//! Enter pane mode command
//!
//! Transitions the editor to Pane mode for pane management operations. This mode
//! enables one-shot commands for splitting, closing, and navigating between panes.

use crate::Stoat;
use tracing::debug;

impl Stoat {
    /// Enter Pane mode for pane management.
    ///
    /// Transitions to Pane mode, where single keypresses trigger pane operations.
    /// After executing a pane command, the editor automatically returns to Normal mode.
    ///
    /// # Behavior
    ///
    /// - Sets editor mode to Pane
    /// - Simple keys execute pane commands (v/s/h/j/k/l/q)
    /// - Auto-exits to Normal mode after command execution
    /// - Escape returns to Normal mode without executing a command
    ///
    /// # Common Bindings
    ///
    /// - `ctrl-w` - enter pane mode (from Normal mode)
    ///
    /// # Pane Mode Commands
    ///
    /// - `v` - split right
    /// - `s` - split down
    /// - `h/j/k/l` - focus left/down/up/right
    /// - `q` - close pane
    /// - `escape` - exit to Normal mode
    ///
    /// # Related
    ///
    /// See also:
    /// - [`crate::actions::shell::SplitRight`] - split pane operations
    /// - [`crate::actions::shell::FocusPaneLeft`] - pane navigation
    /// - [`crate::actions::modal::enter_normal_mode`] - return to command mode
    pub fn enter_pane_mode(&mut self) {
        let old_mode = self.mode();
        debug!(from = old_mode, to = "pane", "Entering pane mode");
        self.set_mode("pane");
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;

    #[test]
    fn enter_pane_from_normal() {
        let mut s = Stoat::test();
        s.set_text("hello");
        assert_eq!(s.mode(), "normal");

        s.command("EnterPaneMode");
        assert_eq!(s.mode(), "pane");
    }

    #[test]
    fn escape_exits_pane_mode() {
        let mut s = Stoat::test();
        s.set_text("hello");

        s.command("EnterPaneMode");
        assert_eq!(s.mode(), "pane");

        s.command("EnterNormalMode");
        assert_eq!(s.mode(), "normal");
    }
}

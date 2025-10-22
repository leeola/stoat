//! Enter git filter mode action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    // TODO: Mode transitions probably shouldn't be actions - consider refactoring to internal
    // methods
    /// Enter git filter mode.
    ///
    /// Transitions to git_filter mode which allows selecting a filter type for the git status view.
    /// This mode is typically accessed from the git status modal.
    pub fn enter_git_filter_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = "git_filter".to_string();
        debug!("Entering git_filter mode");
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn enters_git_filter_mode(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.mode = "git_status".to_string();
            s.enter_git_filter_mode(cx);
            assert_eq!(s.mode(), "git_filter");
        });
    }
}

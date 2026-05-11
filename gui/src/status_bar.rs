use gpui::Context;

/// Placeholder status bar entity. The real surface -- left/right
/// item registries, `StatusItemView` trait, mode badge, workspace
/// label, file label, cursor position, count prefix, diagnostics
/// badge, LSP progress, and search indicator -- lives under the
/// "Foundation: status bar" parent in `.todo-plans/TODO.md`
/// (lines 115-125). `Workspace` owns one of these so its field
/// set is stable; the entity carries no state for now.
pub struct StatusBar;

impl StatusBar {
    pub fn new(_cx: &mut Context<'_, Self>) -> Self {
        Self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};

    #[test]
    fn constructs() {
        let cx = TestAppContext::single();
        let _bar = cx.update(|cx| cx.new(StatusBar::new));
    }
}

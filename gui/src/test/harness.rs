use crate::workspace::Workspace;
use gpui::{
    AnyWindowHandle, App, AppContext, Bounds, Empty, Entity, Global, Keystroke, TestAppContext,
    WindowBounds, WindowHandle, WindowOptions,
};
use std::{sync::Arc, time::Duration};
use stoat::host::fake::{
    terminal::FakeTerminal, FakeClaudeCodeHost, FakeClipboard, FakeGit, FakeLsp,
};
use stoat_host::{FakeEnv, FakeFs, FakeFsWatcher, FakeShell};
use stoat_scheduler::TestScheduler;

pub struct TestHarness {
    cx: TestAppContext,
    window: AnyWindowHandle,
    active_workspace: Option<Entity<Workspace>>,
    fs: Arc<FakeFs>,
    fs_watcher: Arc<FakeFsWatcher>,
    env: Arc<FakeEnv>,
    shell: Arc<FakeShell>,
    lsp: Arc<FakeLsp>,
    git: Arc<FakeGit>,
    claude: Arc<FakeClaudeCodeHost>,
    clipboard: Arc<FakeClipboard>,
    terminal: Arc<FakeTerminal>,
    scheduler: Arc<TestScheduler>,
}

impl TestHarness {
    pub fn new() -> Self {
        let cx = TestAppContext::single();
        let window: WindowHandle<Empty> = cx.update(|cx| {
            let bounds = Bounds::maximized(None, cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_window, cx| cx.new(|_cx| Empty),
            )
            .expect("open test window")
        });
        Self {
            cx,
            window: window.into(),
            active_workspace: None,
            fs: Arc::new(FakeFs::new()),
            fs_watcher: Arc::new(FakeFsWatcher::new()),
            env: Arc::new(FakeEnv::new()),
            shell: Arc::new(FakeShell::new()),
            lsp: Arc::new(FakeLsp::new()),
            git: Arc::new(FakeGit::new()),
            claude: Arc::new(FakeClaudeCodeHost::new()),
            clipboard: Arc::new(FakeClipboard::new()),
            terminal: Arc::new(FakeTerminal::new()),
            scheduler: Arc::new(TestScheduler::new()),
        }
    }

    /// Register the workspace that [`keystroke`] / [`keystrokes`]
    /// feed into. Tests construct a workspace and call this once
    /// before driving keystrokes; the harness panics on
    /// `keystroke` invocations made before the active workspace is
    /// registered.
    pub fn set_active_workspace(&mut self, workspace: Entity<Workspace>) {
        self.active_workspace = Some(workspace);
    }

    /// Parse `source` as a single [`Keystroke`] and feed it into
    /// the active workspace's [`InputStateMachine`], dispatching
    /// every resolved action via
    /// [`Workspace::dispatch_action`]. Bypasses gpui's OS
    /// keystroke pipeline so tests do not depend on the
    /// platform's input event loop.
    ///
    /// Panics on a parse failure with the offending source, and
    /// when called before
    /// [`set_active_workspace`].
    pub fn keystroke(&mut self, source: &str) {
        let keystroke =
            Keystroke::parse(source).unwrap_or_else(|_| panic!("invalid keystroke `{source}`"));
        let workspace = self
            .active_workspace
            .clone()
            .expect("set_active_workspace was not called");
        let window = self.window;
        self.cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |w, cx| {
                    let actions = w
                        .input_state_machine()
                        .clone()
                        .update(cx, |sm, cx| sm.feed(&keystroke, cx));
                    for action in actions {
                        w.dispatch_action(action, window, cx);
                    }
                });
            })
            .expect("test window is open");
    }

    /// Feed each character in `source` as a single-key
    /// [`Keystroke`] (`"ggjj"` -> `g`, `g`, `j`, `j`). For
    /// keystrokes with modifiers (`"ctrl-a"`) call [`keystroke`]
    /// directly.
    pub fn keystrokes(&mut self, source: &str) {
        for ch in source.chars() {
            self.keystroke(&ch.to_string());
        }
    }

    pub fn run_until_parked(&mut self) {
        self.cx.run_until_parked();
    }

    pub fn advance_clock(&self, duration: Duration) {
        self.cx.executor().advance_clock(duration);
        self.scheduler.advance_clock(duration);
    }

    pub fn read_entity<T: 'static, R>(
        &self,
        entity: &Entity<T>,
        f: impl FnOnce(&T, &App) -> R,
    ) -> R {
        self.cx.read_entity(entity, f)
    }

    pub fn set_global<T: Global>(&mut self, global: T) {
        self.cx.update(|cx| cx.set_global(global));
    }

    pub fn fs(&self) -> &Arc<FakeFs> {
        &self.fs
    }

    pub fn fs_watcher(&self) -> &Arc<FakeFsWatcher> {
        &self.fs_watcher
    }

    pub fn env(&self) -> &Arc<FakeEnv> {
        &self.env
    }

    pub fn shell(&self) -> &Arc<FakeShell> {
        &self.shell
    }

    pub fn lsp(&self) -> &Arc<FakeLsp> {
        &self.lsp
    }

    pub fn git(&self) -> &Arc<FakeGit> {
        &self.git
    }

    pub fn claude(&self) -> &Arc<FakeClaudeCodeHost> {
        &self.claude
    }

    pub fn clipboard(&self) -> &Arc<FakeClipboard> {
        &self.clipboard
    }

    pub fn terminal(&self) -> &Arc<FakeTerminal> {
        &self.terminal
    }

    pub fn scheduler(&self) -> &Arc<TestScheduler> {
        &self.scheduler
    }
}

impl Default for TestHarness {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap_loader;
    use std::path::PathBuf;

    fn workspace_with_keymap_source(harness: &mut TestHarness, source: &str) -> Entity<Workspace> {
        let workspace = harness
            .cx
            .update(|cx| cx.new(|cx| Workspace::new("test", PathBuf::from("/tmp/repo"), cx)));
        let keymap = keymap_loader::compile_from_source(source);
        let sm = workspace.read_with(&harness.cx, |w, _| w.input_state_machine().clone());
        sm.update(&mut harness.cx, |sm, _| sm.set_keymap(keymap));
        workspace
    }

    #[test]
    fn keystroke_into_active_workspace_seeds_pending_count() {
        let mut harness = TestHarness::new();
        let workspace = workspace_with_keymap_source(&mut harness, "");
        harness.set_active_workspace(workspace.clone());

        harness.keystroke("5");
        harness.run_until_parked();

        let sm = workspace.read_with(&harness.cx, |w, _| w.input_state_machine().clone());
        sm.read_with(&harness.cx, |sm, _| {
            assert_eq!(sm.pending_count(), Some(5));
        });
    }

    #[test]
    fn keystrokes_splits_per_char() {
        let mut harness = TestHarness::new();
        let workspace = workspace_with_keymap_source(&mut harness, "on key { s -> SplitRight(); }");
        let pane_tree = workspace.read_with(&harness.cx, |w, _| w.pane_tree().clone());
        harness.set_active_workspace(workspace);

        harness.keystrokes("ss");
        harness.run_until_parked();

        assert_eq!(pane_tree.read_with(&harness.cx, |t, _| t.pane_count()), 3);
    }

    #[test]
    fn keystroke_with_modifier_dispatches() {
        let mut harness = TestHarness::new();
        let workspace = workspace_with_keymap_source(&mut harness, "on key { C-q -> Quit(); }");
        let pane_tree = workspace.read_with(&harness.cx, |w, _| w.pane_tree().clone());
        // Split first so Quit closes one pane rather than the app.
        pane_tree.update(&mut harness.cx, |t, cx| {
            t.split(stoat::pane::Axis::Vertical, cx);
        });
        assert_eq!(pane_tree.read_with(&harness.cx, |t, _| t.pane_count()), 2);
        harness.set_active_workspace(workspace);

        harness.keystroke("ctrl-q");
        harness.run_until_parked();

        assert_eq!(pane_tree.read_with(&harness.cx, |t, _| t.pane_count()), 1);
    }

    #[test]
    #[should_panic(expected = "invalid keystroke")]
    fn keystroke_panics_on_invalid_source() {
        let mut harness = TestHarness::new();
        let workspace = workspace_with_keymap_source(&mut harness, "");
        harness.set_active_workspace(workspace);
        harness.keystroke("not--a--keystroke");
    }

    #[test]
    #[should_panic(expected = "set_active_workspace was not called")]
    fn keystroke_panics_without_active_workspace() {
        let mut harness = TestHarness::new();
        harness.keystroke("a");
    }
}

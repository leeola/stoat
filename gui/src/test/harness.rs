use crate::{
    globals::{
        AgentConnectionGlobal, ClipboardHostGlobal, EnvHostGlobal, ExecutorGlobal, FsHostGlobal,
        FsWatchHostGlobal, GitHostGlobal, LspHostGlobal, ShellHostGlobal, TerminalHostGlobal,
    },
    workspace::Workspace,
};
use gpui::{
    AnyWindowHandle, App, AppContext, Bounds, Empty, Entity, Global, Keystroke, TestAppContext,
    WindowBounds, WindowHandle, WindowOptions,
};
use std::{sync::Arc, time::Duration};
use stoat::host::{
    fake::{
        terminal::FakeTerminalSession, FakeAgentConnection, FakeClipboard, FakeGit, FakeLsp,
        FakeLspHost, FakeTerminalHost,
    },
    AgentConnection, ClipboardHost, EnvHost, FsHost, FsWatchHost, GitHost, LspHost, ShellHost,
    TerminalHost,
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
    agent: Arc<FakeAgentConnection>,
    clipboard: Arc<FakeClipboard>,
    terminal: Arc<FakeTerminalSession>,
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

        let fs = Arc::new(FakeFs::new());
        let fs_watcher = Arc::new(FakeFsWatcher::new());
        let env = Arc::new(FakeEnv::new());
        let shell = Arc::new(FakeShell::new());
        let lsp = Arc::new(FakeLsp::new());
        let git = Arc::new(FakeGit::new());
        let agent = Arc::new(FakeAgentConnection::new());
        let clipboard = Arc::new(FakeClipboard::new());
        let terminal = Arc::new(FakeTerminalSession::new());
        let scheduler = Arc::new(TestScheduler::new());

        let harness = Self {
            cx,
            window: window.into(),
            active_workspace: None,
            fs,
            fs_watcher,
            env,
            shell,
            lsp,
            git,
            agent,
            clipboard,
            terminal,
            scheduler,
        };
        harness.install_host_globals();
        harness
    }

    fn install_host_globals(&self) {
        let fs = self.fs.clone();
        let fs_watcher = self.fs_watcher.clone();
        let env = self.env.clone();
        let shell = self.shell.clone();
        let lsp = self.lsp.clone();
        let git = self.git.clone();
        let agent = self.agent.clone();
        let clipboard = self.clipboard.clone();
        let terminal = self.terminal.clone();
        let executor = self.scheduler.executor();
        self.cx.update(move |cx| {
            cx.set_global(FsHostGlobal(fs as Arc<dyn FsHost>));
            cx.set_global(FsWatchHostGlobal(fs_watcher as Arc<dyn FsWatchHost>));
            cx.set_global(EnvHostGlobal(env as Arc<dyn EnvHost>));
            cx.set_global(ShellHostGlobal(shell as Arc<dyn ShellHost>));
            cx.set_global(LspHostGlobal(
                Arc::new(FakeLspHost::new(lsp)) as Arc<dyn LspHost>
            ));
            cx.set_global(GitHostGlobal(git as Arc<dyn GitHost>));
            cx.set_global(AgentConnectionGlobal(agent as Arc<dyn AgentConnection>));
            cx.set_global(ClipboardHostGlobal(clipboard as Arc<dyn ClipboardHost>));
            cx.set_global(TerminalHostGlobal(
                Arc::new(FakeTerminalHost::new(terminal)) as Arc<dyn TerminalHost>,
            ));
            cx.set_global(ExecutorGlobal(executor));
        });
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
                        .update(cx, |sm, cx| sm.feed(&keystroke, window, cx));
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

    /// Swap the registered [`FsHostGlobal`] to wrap `fake` and
    /// update the harness's stored handle so [`Self::fs`] returns
    /// the same `Arc`.
    pub fn set_fs_host(&mut self, fake: Arc<FakeFs>) {
        self.fs = fake.clone();
        let arc = fake as Arc<dyn FsHost>;
        self.cx.update(|cx| cx.set_global(FsHostGlobal(arc)));
    }

    pub fn set_fs_watch_host(&mut self, fake: Arc<FakeFsWatcher>) {
        self.fs_watcher = fake.clone();
        let arc = fake as Arc<dyn FsWatchHost>;
        self.cx.update(|cx| cx.set_global(FsWatchHostGlobal(arc)));
    }

    pub fn set_env_host(&mut self, fake: Arc<FakeEnv>) {
        self.env = fake.clone();
        let arc = fake as Arc<dyn EnvHost>;
        self.cx.update(|cx| cx.set_global(EnvHostGlobal(arc)));
    }

    pub fn set_shell_host(&mut self, fake: Arc<FakeShell>) {
        self.shell = fake.clone();
        let arc = fake as Arc<dyn ShellHost>;
        self.cx.update(|cx| cx.set_global(ShellHostGlobal(arc)));
    }

    /// Swap the registered [`LspHostGlobal`] factory to wrap a
    /// fresh [`FakeLspHost`] over `fake` and update the harness's
    /// stored handle so [`Self::lsp`] returns the same `Arc`.
    pub fn set_lsp_host(&mut self, fake: Arc<FakeLsp>) {
        self.lsp = fake.clone();
        let arc = Arc::new(FakeLspHost::new(fake)) as Arc<dyn LspHost>;
        self.cx.update(|cx| cx.set_global(LspHostGlobal(arc)));
    }

    pub fn set_git_host(&mut self, fake: Arc<FakeGit>) {
        self.git = fake.clone();
        let arc = fake as Arc<dyn GitHost>;
        self.cx.update(|cx| cx.set_global(GitHostGlobal(arc)));
    }

    pub fn set_agent_connection(&mut self, fake: Arc<FakeAgentConnection>) {
        self.agent = fake.clone();
        let arc = fake as Arc<dyn AgentConnection>;
        self.cx
            .update(|cx| cx.set_global(AgentConnectionGlobal(arc)));
    }

    pub fn set_clipboard_host(&mut self, fake: Arc<FakeClipboard>) {
        self.clipboard = fake.clone();
        let arc = fake as Arc<dyn ClipboardHost>;
        self.cx.update(|cx| cx.set_global(ClipboardHostGlobal(arc)));
    }

    /// Swap the registered [`TerminalHostGlobal`] factory to wrap a
    /// fresh [`FakeTerminalHost`] over `fake` and update the
    /// harness's stored handle so [`Self::terminal`] returns the
    /// same `Arc`.
    pub fn set_terminal_host(&mut self, fake: Arc<FakeTerminalSession>) {
        self.terminal = fake.clone();
        let arc = Arc::new(FakeTerminalHost::new(fake)) as Arc<dyn TerminalHost>;
        self.cx.update(|cx| cx.set_global(TerminalHostGlobal(arc)));
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

    pub fn agent(&self) -> &Arc<FakeAgentConnection> {
        &self.agent
    }

    pub fn clipboard(&self) -> &Arc<FakeClipboard> {
        &self.clipboard
    }

    pub fn terminal(&self) -> &Arc<FakeTerminalSession> {
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
        let workspace = workspace_with_keymap_source(&mut harness, "on key { C-q -> quit(); }");
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

    #[test]
    fn host_globals_registered_at_construction() {
        let harness = TestHarness::new();
        harness.cx.read(|cx| {
            assert!(cx.has_global::<FsHostGlobal>(), "FsHostGlobal missing");
            assert!(
                cx.has_global::<FsWatchHostGlobal>(),
                "FsWatchHostGlobal missing"
            );
            assert!(cx.has_global::<EnvHostGlobal>(), "EnvHostGlobal missing");
            assert!(
                cx.has_global::<ShellHostGlobal>(),
                "ShellHostGlobal missing"
            );
            assert!(cx.has_global::<LspHostGlobal>(), "LspHostGlobal missing");
            assert!(cx.has_global::<GitHostGlobal>(), "GitHostGlobal missing");
            assert!(
                cx.has_global::<AgentConnectionGlobal>(),
                "AgentConnectionGlobal missing"
            );
            assert!(
                cx.has_global::<ClipboardHostGlobal>(),
                "ClipboardHostGlobal missing"
            );
            assert!(
                cx.has_global::<TerminalHostGlobal>(),
                "TerminalHostGlobal missing"
            );
            assert!(cx.has_global::<ExecutorGlobal>(), "ExecutorGlobal missing");
        });
    }

    #[test]
    fn set_fs_host_replaces_global() {
        let mut harness = TestHarness::new();
        let next = Arc::new(FakeFs::new());
        let next_ptr = Arc::as_ptr(&next);
        harness.set_fs_host(next);

        assert_eq!(Arc::as_ptr(harness.fs()), next_ptr);
        let registered_ptr = harness
            .cx
            .read(|cx| Arc::as_ptr(&cx.global::<FsHostGlobal>().0) as *const ());
        assert_eq!(registered_ptr, next_ptr as *const ());
    }

    #[test]
    fn set_agent_connection_replaces_global() {
        let mut harness = TestHarness::new();
        let next = Arc::new(FakeAgentConnection::new());
        let next_ptr = Arc::as_ptr(&next) as *const ();
        harness.set_agent_connection(next);

        assert_eq!(Arc::as_ptr(harness.agent()) as *const (), next_ptr);
        let registered_ptr = harness
            .cx
            .read(|cx| Arc::as_ptr(&cx.global::<AgentConnectionGlobal>().0) as *const ());
        assert_eq!(registered_ptr, next_ptr);
    }
}

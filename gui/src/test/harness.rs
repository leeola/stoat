use gpui::{
    AnyWindowHandle, App, AppContext, Bounds, Empty, Entity, Global, TestAppContext, WindowBounds,
    WindowHandle, WindowOptions,
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

    pub fn simulate_keystrokes(&mut self, keystrokes: &str) {
        self.cx.simulate_keystrokes(self.window, keystrokes);
    }

    pub fn run_until_parked(&mut self) {
        self.cx.run_until_parked();
    }

    pub fn advance_clock(&self, duration: Duration) {
        self.cx.executor().advance_clock(duration);
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

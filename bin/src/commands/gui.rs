use snafu::{ResultExt, Whatever};
use std::sync::Arc;
use stoat::host::{
    LocalClipboard, LocalEnv, LocalFs, LocalFsWatcher, LocalGit, LocalLspHost, LocalShell,
    LocalTerminalHost,
};
use stoat_agent_claude_code::ClaudeCodeLauncher;
use stoat_gui::{
    install_panic_hook, ClaudeCodeHostGlobal, ClipboardHostGlobal, EnvHostGlobal, ExecutorGlobal,
    FsHostGlobal, FsWatchHostGlobal, GitHostGlobal, Globals, LanguageRegistry, LspHostGlobal,
    Settings, ShellHostGlobal, TerminalHostGlobal, Theme,
};
use stoat_scheduler::TokioScheduler;

pub fn run() -> Result<(), Whatever> {
    install_panic_hook();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .whatever_context("build tokio runtime")?;
    let _guard = runtime.enter();

    let scheduler = Arc::new(TokioScheduler::new(runtime.handle().clone()));
    let executor = scheduler.executor();

    let fs_host: Arc<dyn stoat::host::FsHost> = Arc::new(LocalFs);
    let fs_watcher = LocalFsWatcher::new().whatever_context("init LocalFsWatcher")?;
    let claude_launcher = ClaudeCodeLauncher::new(fs_host.clone(), executor.clone());

    let globals = Globals {
        settings: Settings::default(),
        theme: Theme::load_from_source("", "default"),
        language_registry: LanguageRegistry::standard(),
        fs_host: FsHostGlobal(fs_host),
        fs_watch_host: FsWatchHostGlobal(Arc::new(fs_watcher)),
        env_host: EnvHostGlobal(Arc::new(LocalEnv)),
        shell_host: ShellHostGlobal(Arc::new(LocalShell)),
        lsp_host: LspHostGlobal(Arc::new(LocalLspHost)),
        git_host: GitHostGlobal(Arc::new(LocalGit::new())),
        claude_code_host: ClaudeCodeHostGlobal(Arc::new(claude_launcher)),
        clipboard_host: ClipboardHostGlobal(Arc::new(LocalClipboard)),
        terminal_host: TerminalHostGlobal(Arc::new(LocalTerminalHost)),
        executor: ExecutorGlobal(executor),
    };

    stoat_gui::run(globals);
    Ok(())
}

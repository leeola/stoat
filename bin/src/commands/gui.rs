use snafu::{ResultExt, Whatever};
use std::{path::PathBuf, sync::Arc};
use stoat::host::{
    LocalClipboard, LocalEnv, LocalFs, LocalFsWatcher, LocalGit, LocalLspHost, LocalShell,
    LocalTerminalHost, PermissionCallback, RuleBasedPolicy,
};
use stoat_agent_claude_code::ClaudeCodeLauncher;
use stoat_gui::{
    install_panic_hook, ClaudeCodeHostGlobal, ClipboardHostGlobal, EnvHostGlobal, ExecutorGlobal,
    FsHostGlobal, FsWatchHostGlobal, GitHostGlobal, Globals, LanguageRegistry, LspHostGlobal,
    MpscPermissionPromptHost, PermissionPromptHost, PermissionPromptHostGlobal, RestoreMode,
    Settings, ShellHostGlobal, TerminalHostGlobal, Theme,
};
use stoat_scheduler::TokioScheduler;
use tokio::sync::mpsc;

/// Bounded queue between the Claude permission policy callback (on
/// the Tokio runtime) and the GUI workspace's modal poll (on the
/// main thread). 8 matches the rule-policy tests; queued prompts
/// remain decision-bound, so even a sustained burst clears once the
/// user works through the modal.
const PERMISSION_PROMPT_CAPACITY: usize = 8;

const DEFAULT_CONFIG: &str = include_str!("../../../config.stcfg");

pub fn run(
    files: Vec<PathBuf>,
    restore: RestoreMode,
    inputs: Option<String>,
) -> Result<(), Whatever> {
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

    let (settings, theme) = load_default_settings_and_theme();
    let language_servers = settings.resolved.language_servers.clone();

    let (prompt_tx, prompt_rx) = mpsc::channel(PERMISSION_PROMPT_CAPACITY);
    let permission_policy =
        RuleBasedPolicy::with_prompt_channel(&settings.resolved.claude_permissions, prompt_tx);
    let claude_launcher = ClaudeCodeLauncher::new(fs_host.clone(), executor.clone())
        .with_permission_callback(Arc::new(permission_policy) as Arc<dyn PermissionCallback>);
    let permission_prompt_host: Arc<dyn PermissionPromptHost> =
        Arc::new(MpscPermissionPromptHost::new(prompt_rx));

    let globals = Globals {
        settings,
        theme,
        language_registry: LanguageRegistry::standard(),
        fs_host: FsHostGlobal(fs_host),
        fs_watch_host: FsWatchHostGlobal(Arc::new(fs_watcher)),
        env_host: EnvHostGlobal(Arc::new(LocalEnv)),
        shell_host: ShellHostGlobal(Arc::new(LocalShell)),
        lsp_host: LspHostGlobal(Arc::new(LocalLspHost::new(
            language_servers,
            executor.clone(),
        ))),
        git_host: GitHostGlobal(Arc::new(LocalGit::new())),
        claude_code_host: ClaudeCodeHostGlobal(Arc::new(claude_launcher)),
        permission_prompt_host: PermissionPromptHostGlobal(permission_prompt_host),
        clipboard_host: ClipboardHostGlobal(Arc::new(LocalClipboard)),
        terminal_host: TerminalHostGlobal(Arc::new(LocalTerminalHost)),
        executor: ExecutorGlobal(executor),
    };

    stoat_gui::run(globals, files, restore, inputs);
    Ok(())
}

fn load_default_settings_and_theme() -> (Settings, Theme) {
    let (config, errors) = stoat_config::parse(DEFAULT_CONFIG);
    if !errors.is_empty() {
        tracing::error!(
            "default config parse errors: {}",
            stoat_config::format_errors(DEFAULT_CONFIG, &errors)
        );
    }
    let Some(config) = config else {
        return (Settings::default(), Theme::empty());
    };

    let settings = Settings::from_config(config);
    let theme = {
        let name = settings.resolved.theme.as_deref().unwrap_or("default_dark");
        Theme::from_config(&settings.config, name)
    };
    (settings, theme)
}

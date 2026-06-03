use etcetera::{base_strategy::Xdg, BaseStrategy};
use snafu::{whatever, ResultExt, Whatever};
use std::{fs, io::ErrorKind, path::PathBuf, sync::Arc};
use stoat::host::{
    LocalClipboard, LocalEnv, LocalFs, LocalFsWatcher, LocalGit, LocalLspHost, LocalShell,
    LocalTerminalHost, PermissionCallback, RuleBasedPolicy,
};
use stoat_agent_claude_code::ClaudeCodeLauncher;
use stoat_gui::{
    install_panic_hook, parse_input_sequence, ClaudeCodeHostGlobal, ClipboardHostGlobal,
    EnvHostGlobal, ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal, GitHostGlobal, Globals,
    LanguageRegistry, LspHostGlobal, MpscPermissionPromptHost, PermissionPromptHost,
    PermissionPromptHostGlobal, RestoreMode, Settings, ShellHostGlobal, TerminalHostGlobal, Theme,
    UserSnippetsGlobal,
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
    timeout: Option<f64>,
) -> Result<(), Whatever> {
    install_panic_hook();

    let inputs = inputs
        .map(|raw| parse_input_sequence(&raw))
        .transpose()
        .with_whatever_context(|e| format!("parse --inputs sequence: {e}"))?;
    let timeout = validate_timeout(timeout)?;

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
        user_snippets: UserSnippetsGlobal(stoat::snippet::load_user_snippets()),
    };

    stoat_gui::run(globals, files, restore, inputs, timeout);
    Ok(())
}

/// Reject `--timeout` values that would panic `Duration::from_secs_f64`.
/// NaN, +/- infinity, and negative seconds are all caught here so the
/// binary exits with a usage error before the gpui main loop is
/// entered. `None` (the flag absent) passes through unchanged.
fn validate_timeout(timeout: Option<f64>) -> Result<Option<f64>, Whatever> {
    if let Some(seconds) = timeout {
        if !seconds.is_finite() || seconds < 0.0 {
            whatever!("invalid --timeout value: {seconds}");
        }
    }
    Ok(timeout)
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

    let mut settings = Settings::from_config(config);
    if let Some(user_source) = read_user_config() {
        settings = settings.layer_user_source(&user_source);
    }

    let theme = {
        let name = settings.resolved.theme.as_deref().unwrap_or("default_dark");
        Theme::from_config(&settings.config, name)
    };
    (settings, theme)
}

/// Read the user's stcfg override at `$XDG_CONFIG_HOME/stoat/config.stcfg`.
/// Returns `None` when the path cannot be resolved or the file is absent,
/// so the caller keeps the bundled default; a genuine read error (e.g. a
/// permissions problem) is logged and also yields `None`.
fn read_user_config() -> Option<String> {
    let path = Xdg::new()
        .ok()?
        .config_dir()
        .join("stoat")
        .join("config.stcfg");
    match fs::read_to_string(&path) {
        Ok(source) => Some(source),
        Err(err) if err.kind() == ErrorKind::NotFound => None,
        Err(err) => {
            tracing::warn!(path = %path.display(), ?err, "failed to read user config");
            None
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err_message(timeout: f64) -> String {
        validate_timeout(Some(timeout))
            .expect_err("validation should reject")
            .to_string()
    }

    #[test]
    fn validate_timeout_passes_through_well_formed_values() {
        assert_eq!(validate_timeout(None).unwrap(), None);
        assert_eq!(validate_timeout(Some(0.0)).unwrap(), Some(0.0));
        assert_eq!(validate_timeout(Some(1.5)).unwrap(), Some(1.5));
        assert_eq!(validate_timeout(Some(3600.0)).unwrap(), Some(3600.0));
    }

    #[test]
    fn validate_timeout_rejects_negative() {
        let message = err_message(-1.0);
        assert!(
            message.contains("invalid --timeout") && message.contains("-1"),
            "message should name the offending value: {message}",
        );
    }

    #[test]
    fn validate_timeout_rejects_nan() {
        let message = err_message(f64::NAN);
        assert!(
            message.contains("invalid --timeout") && message.contains("NaN"),
            "message should name the offending value: {message}",
        );
    }

    #[test]
    fn validate_timeout_rejects_positive_infinity() {
        let message = err_message(f64::INFINITY);
        assert!(
            message.contains("invalid --timeout") && message.contains("inf"),
            "message should name the offending value: {message}",
        );
    }

    #[test]
    fn validate_timeout_rejects_negative_infinity() {
        let message = err_message(f64::NEG_INFINITY);
        assert!(
            message.contains("invalid --timeout") && message.contains("inf"),
            "message should name the offending value: {message}",
        );
    }
}

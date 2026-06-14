use snafu::{whatever, ResultExt, Whatever};
use std::{fs, io::ErrorKind, path::PathBuf, sync::Arc};
use stoat::host::{
    LocalClipboard, LocalEnv, LocalFs, LocalFsWatcher, LocalGit, LocalLspHost, LocalShell,
    LocalTerminalHost,
};
use stoat_gui::{
    install_panic_hook, parse_input_sequence, ClipboardHostGlobal, EnvHostGlobal, ExecutorGlobal,
    FsHostGlobal, FsWatchHostGlobal, GitHostGlobal, Globals, LanguageRegistry, LspHostGlobal,
    RestoreMode, Settings, ShellHostGlobal, TerminalHostGlobal, Theme, UserSnippetsGlobal,
};
use stoat_scheduler::TokioScheduler;

const DEFAULT_CONFIG: &str = include_str!("../../../config.stcfg");

pub fn run(
    files: Vec<PathBuf>,
    restore: RestoreMode,
    stdin: Option<String>,
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
        clipboard_host: ClipboardHostGlobal(Arc::new(LocalClipboard)),
        terminal_host: TerminalHostGlobal(Arc::new(LocalTerminalHost)),
        executor: ExecutorGlobal(executor),
        user_snippets: UserSnippetsGlobal(stoat::snippet::load_user_snippets()),
    };

    stoat_gui::run(globals, files, restore, stdin, inputs, timeout);
    Ok(())
}

/// Reject `--timeout` values that would panic `Duration::from_secs_f64`.
/// NaN, +/- infinity, and negative seconds are all caught here so the
/// binary exits with a usage error before the gpui main loop is
/// entered. `None` (the flag absent) passes through unchanged.
fn validate_timeout(timeout: Option<f64>) -> Result<Option<f64>, Whatever> {
    if let Some(seconds) = timeout
        && (!seconds.is_finite() || seconds < 0.0)
    {
        whatever!("invalid --timeout value: {seconds}");
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
    let path = stoat_config::user_config_path()?;
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

    #[test]
    fn default_config_parses_and_resolves_item_modes() {
        let (config, errors) = stoat_config::parse(DEFAULT_CONFIG);
        assert!(
            errors.is_empty(),
            "bundled default config must parse cleanly: {}",
            stoat_config::format_errors(DEFAULT_CONFIG, &errors),
        );
        let settings = Settings::from_config(config.expect("default config parses"));
        assert_eq!(
            settings
                .resolved
                .item_modes
                .get("conflict")
                .map(String::as_str),
            Some("conflict"),
            "ui.item_mode.conflict from the default config resolves",
        );
    }
}

//! Maps a buffer's language to the language server that serves it.
//!
//! [`resolve_server_command`] is the entry point. It consults a user's
//! [`Settings`] overrides first, then falls back to the built-in
//! [`server_command`] table. Both feed the lazy-spawn path in
//! [`crate::action_handlers::lsp::notify_buffer_opened`].

use stoat_config::Settings;

/// The command and arguments for `language`'s language server, or `None` when
/// no server is known for it.
///
/// `language` is a [`language::Language::name`], e.g. `"rust"`. The returned
/// command is looked up on `PATH` when spawned.
pub(crate) fn server_command(language: &str) -> Option<(&'static str, &'static [&'static str])> {
    match language {
        "rust" => Some(("rust-analyzer", &[])),
        _ => None,
    }
}

/// Resolve the language server for `language`, letting a [`Settings`]
/// entry override the built-in [`server_command`] table.
///
/// A configured entry wins. Its first element is the executable and the
/// rest are arguments. An empty configured argv disables the server for
/// that language, returning `None`. A language with no configured entry
/// falls back to the built-in table.
pub(crate) fn resolve_server_command(
    settings: &Settings,
    language: &str,
) -> Option<(String, Vec<String>)> {
    if let Some(argv) = settings.lsp_servers.get(language) {
        let (command, args) = argv.split_first()?;
        return Some((command.clone(), args.to_vec()));
    }
    let (command, args) = server_command(language)?;
    Some((
        command.to_string(),
        args.iter().map(|arg| arg.to_string()).collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::{resolve_server_command, server_command};
    use stoat_config::Settings;

    #[test]
    fn rust_maps_to_rust_analyzer() {
        let (command, args) = server_command("rust").expect("rust has a server");
        assert_eq!(command, "rust-analyzer");
        assert!(args.is_empty());
    }

    #[test]
    fn unknown_language_has_no_server() {
        assert_eq!(server_command("cobol"), None);
    }

    #[test]
    fn settings_override_wins_over_builtin() {
        let mut settings = Settings::default();
        settings.lsp_servers.insert(
            "rust".to_string(),
            vec!["my-ra".to_string(), "--x".to_string()],
        );
        assert_eq!(
            resolve_server_command(&settings, "rust"),
            Some(("my-ra".to_string(), vec!["--x".to_string()])),
        );
    }

    #[test]
    fn empty_settings_argv_disables_the_language() {
        let mut settings = Settings::default();
        settings.lsp_servers.insert("rust".to_string(), vec![]);
        assert_eq!(resolve_server_command(&settings, "rust"), None);
    }

    #[test]
    fn unconfigured_language_falls_back_to_builtin() {
        let settings = Settings::default();
        assert_eq!(
            resolve_server_command(&settings, "rust"),
            Some(("rust-analyzer".to_string(), vec![])),
        );
    }
}

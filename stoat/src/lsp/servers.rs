//! Maps a buffer's language to the language servers that serve it.
//!
//! [`resolve_servers`] is the entry point. It consults the builtin per-language
//! table, applies the user's `lsp.server.<language>` [`Settings`] override to
//! the primary server, and returns the ordered servers to spawn and route
//! through, feeding the lazy-spawn path in
//! [`crate::action_handlers::lsp::notify_buffer_opened`].

use crate::lsp::registry::ServerSelector;
use stoat_config::Settings;

/// A language server resolved for a language, carrying its registry name and
/// spawn argv.
pub(crate) struct ResolvedServer {
    pub(crate) name: String,
    pub(crate) argv: Vec<String>,
}

impl ResolvedServer {
    /// The routing selector for this server, dropping the spawn argv.
    pub(crate) fn to_selector(&self) -> ServerSelector {
        ServerSelector {
            name: self.name.clone(),
        }
    }
}

/// One entry in the builtin per-language server table.
struct BuiltinServer {
    name: &'static str,
    argv: &'static [&'static str],
}

/// The builtin servers for `language`, primary first.
fn builtin_servers(language: &str) -> &'static [BuiltinServer] {
    const RUST: &[BuiltinServer] = &[BuiltinServer {
        name: "rust-analyzer",
        argv: &["rust-analyzer"],
    }];
    match language {
        "rust" => RUST,
        _ => &[],
    }
}

/// Resolve the ordered language servers for `language`.
///
/// The `lsp.server.<language>` [`Settings`] override, when present, replaces the
/// primary server. Its first element is the executable and the rest arguments,
/// and the server is renamed after that executable. An empty configured argv
/// disables the primary, dropping it. Builtin secondary servers are kept
/// unchanged. A language with no builtin and no override yields an empty list.
pub(crate) fn resolve_servers(settings: &Settings, language: &str) -> Vec<ResolvedServer> {
    let builtin = builtin_servers(language);
    let Some(override_argv) = settings.lsp_servers.get(language) else {
        return builtin.iter().map(resolve_builtin).collect();
    };

    let mut resolved = Vec::new();
    if let Some((command, _)) = override_argv.split_first() {
        resolved.push(ResolvedServer {
            name: command.clone(),
            argv: override_argv.clone(),
        });
    }
    resolved.extend(builtin.iter().skip(1).map(resolve_builtin));
    resolved
}

fn resolve_builtin(server: &BuiltinServer) -> ResolvedServer {
    ResolvedServer {
        name: server.name.to_string(),
        argv: server.argv.iter().map(|arg| arg.to_string()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_servers;
    use stoat_config::Settings;

    fn names_and_argv(settings: &Settings, language: &str) -> Vec<(String, Vec<String>)> {
        resolve_servers(settings, language)
            .into_iter()
            .map(|server| (server.name, server.argv))
            .collect()
    }

    #[test]
    fn rust_maps_to_rust_analyzer() {
        assert_eq!(
            names_and_argv(&Settings::default(), "rust"),
            vec![(
                "rust-analyzer".to_string(),
                vec!["rust-analyzer".to_string()]
            )],
        );
    }

    #[test]
    fn unknown_language_has_no_servers() {
        assert!(resolve_servers(&Settings::default(), "cobol").is_empty());
    }

    #[test]
    fn settings_override_replaces_the_primary() {
        let mut settings = Settings::default();
        settings.lsp_servers.insert(
            "rust".to_string(),
            vec!["my-ra".to_string(), "--x".to_string()],
        );
        assert_eq!(
            names_and_argv(&settings, "rust"),
            vec![(
                "my-ra".to_string(),
                vec!["my-ra".to_string(), "--x".to_string()]
            )],
        );
    }

    #[test]
    fn empty_settings_argv_disables_the_language() {
        let mut settings = Settings::default();
        settings.lsp_servers.insert("rust".to_string(), vec![]);
        assert!(resolve_servers(&settings, "rust").is_empty());
    }

    #[test]
    fn override_defines_a_server_for_a_language_without_a_builtin() {
        let mut settings = Settings::default();
        settings
            .lsp_servers
            .insert("python".to_string(), vec!["pyright".to_string()]);
        assert_eq!(
            names_and_argv(&settings, "python"),
            vec![("pyright".to_string(), vec!["pyright".to_string()])],
        );
    }
}

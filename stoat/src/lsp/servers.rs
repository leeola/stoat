//! Maps a buffer's language to the language servers that serve it.
//!
//! [`resolve_servers`] is the entry point. It consults the builtin per-language
//! table, applies the user's `lsp.server.<language>` [`Settings`] override to
//! the primary server, and returns the ordered servers to spawn and route
//! through, feeding the lazy-spawn path in
//! [`crate::action_handlers::lsp::notify_buffer_opened`].

use crate::{
    host::{LanguageServerFeature, LspHost},
    lsp::registry::ServerSelector,
};
use std::{collections::HashSet, sync::Arc};
use stoat_config::Settings;

/// How a resolved server's host is obtained.
///
/// Most servers are external programs spawned as a subprocess. An in-process
/// server is built directly in the editor by calling a constructor, skipping
/// process spawn and the project environment entirely.
pub(crate) enum ServerSource {
    Command(Vec<String>),
    InProcess(fn() -> Arc<dyn LspHost>),
}

/// A language server resolved for a language, carrying its registry name, how to
/// obtain its host, and the feature filters that route requests to it.
pub(crate) struct ResolvedServer {
    pub(crate) name: String,
    pub(crate) source: ServerSource,
    pub(crate) only: HashSet<LanguageServerFeature>,
    pub(crate) except: HashSet<LanguageServerFeature>,
}

impl ResolvedServer {
    /// The routing selector for this server, dropping the spawn argv.
    pub(crate) fn to_selector(&self) -> ServerSelector {
        ServerSelector {
            name: self.name.clone(),
            only: self.only.clone(),
            except: self.except.clone(),
        }
    }
}

/// One entry in the builtin per-language server table.
struct BuiltinServer {
    name: &'static str,
    source: BuiltinSource,
    only: &'static [LanguageServerFeature],
    except: &'static [LanguageServerFeature],
}

/// The static form of [`ServerSource`] used in the builtin table.
enum BuiltinSource {
    Command(&'static [&'static str]),
    InProcess(fn() -> Arc<dyn LspHost>),
}

/// The builtin servers for `language`, primary first.
fn builtin_servers(language: &str) -> &'static [BuiltinServer] {
    const RUST: &[BuiltinServer] = &[BuiltinServer {
        name: "rust-analyzer",
        source: BuiltinSource::Command(&["rust-analyzer"]),
        only: &[],
        except: &[],
    }];
    const STCFG: &[BuiltinServer] = &[BuiltinServer {
        name: "stcfg-ls",
        source: BuiltinSource::InProcess(stcfg_host),
        only: &[],
        except: &[],
    }];
    match language {
        "rust" => RUST,
        "stcfg" => STCFG,
        _ => &[],
    }
}

fn stcfg_host() -> Arc<dyn LspHost> {
    Arc::new(crate::lsp::stcfg::StcfgLsp::new())
}

/// The LSP language name for a file `extension` that has no tree-sitter grammar.
///
/// A grammar-backed buffer resolves its language through the language registry.
/// A `.stcfg` file has no grammar, so it has no registry language, yet still
/// needs an LSP identity to route to the in-process stcfg server.
pub(crate) fn lsp_language_for_extension(extension: &str) -> Option<&'static str> {
    match extension {
        "stcfg" => Some("stcfg"),
        _ => None,
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
        let (only, except) = builtin
            .first()
            .map(|server| (feature_set(server.only), feature_set(server.except)))
            .unwrap_or_default();
        resolved.push(ResolvedServer {
            name: command.clone(),
            source: ServerSource::Command(override_argv.clone()),
            only,
            except,
        });
    }
    resolved.extend(builtin.iter().skip(1).map(resolve_builtin));
    resolved
}

fn resolve_builtin(server: &BuiltinServer) -> ResolvedServer {
    let source = match server.source {
        BuiltinSource::Command(argv) => {
            ServerSource::Command(argv.iter().map(|arg| arg.to_string()).collect())
        },
        BuiltinSource::InProcess(construct) => ServerSource::InProcess(construct),
    };
    ResolvedServer {
        name: server.name.to_string(),
        source,
        only: feature_set(server.only),
        except: feature_set(server.except),
    }
}

fn feature_set(features: &[LanguageServerFeature]) -> HashSet<LanguageServerFeature> {
    features.iter().copied().collect()
}

#[cfg(test)]
mod tests {
    use super::{lsp_language_for_extension, resolve_servers, ServerSource};
    use stoat_config::Settings;

    fn names_and_argv(settings: &Settings, language: &str) -> Vec<(String, Vec<String>)> {
        resolve_servers(settings, language)
            .into_iter()
            .map(|server| {
                let argv = match server.source {
                    ServerSource::Command(argv) => argv,
                    ServerSource::InProcess(_) => Vec::new(),
                };
                (server.name, argv)
            })
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

    #[test]
    fn stcfg_maps_to_the_in_process_server() {
        let servers = resolve_servers(&Settings::default(), "stcfg");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "stcfg-ls");
        assert!(matches!(servers[0].source, ServerSource::InProcess(_)));
    }

    #[test]
    fn stcfg_extension_resolves_to_the_stcfg_language() {
        assert_eq!(lsp_language_for_extension("stcfg"), Some("stcfg"));
        assert_eq!(lsp_language_for_extension("rs"), None);
    }
}

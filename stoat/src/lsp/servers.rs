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
/// A `lsp.servers.<language>` list, when present, names the servers to run in
/// routing priority order. Otherwise the builtin list for the language is used.
/// Either way, the `lsp.server.<language>` override still replaces the primary
/// server's argv (or drops it when the argv is empty), and per-server
/// `lsp.only`/`lsp.except` filters override the builtins'. A language with no
/// list, no builtin, and no override yields an empty list.
pub(crate) fn resolve_servers(settings: &Settings, language: &str) -> Vec<ResolvedServer> {
    let mut resolved: Vec<ResolvedServer> = match settings.lsp_server_lists.get(language) {
        Some(names) => names
            .iter()
            .map(|name| resolve_named(settings, name))
            .collect(),
        None => builtin_servers(language)
            .iter()
            .map(|server| resolve_named(settings, server.name))
            .collect(),
    };
    apply_primary_override(settings, language, &mut resolved);
    resolved
}

/// Resolve a single server by name, drawing its argv and feature filters from
/// config where set and the builtin of that name otherwise.
///
/// Argv comes from `lsp.command.<name>`, else the builtin (which may be
/// in-process), else the name itself as the command. Filters come from
/// `lsp.only`/`lsp.except`, else the builtin's, else empty.
fn resolve_named(settings: &Settings, name: &str) -> ResolvedServer {
    let mut resolved = match builtin_by_name(name) {
        Some(server) => resolve_builtin(server),
        None => ResolvedServer {
            name: name.to_string(),
            source: ServerSource::Command(vec![name.to_string()]),
            only: HashSet::new(),
            except: HashSet::new(),
        },
    };
    if let Some(argv) = settings.lsp_commands.get(name) {
        resolved.source = ServerSource::Command(argv.clone());
    }
    if let Some(only) = configured_features(settings.lsp_only.get(name)) {
        resolved.only = only;
    }
    if let Some(except) = configured_features(settings.lsp_except.get(name)) {
        resolved.except = except;
    }
    resolved
}

/// Apply the `lsp.server.<language>` primary override to an already-resolved
/// list. Replaces the first server's name and argv, or drops it when the argv is
/// empty. A language with no resolved servers gains the override as its sole
/// server, keeping the pre-list behavior for a language without a builtin.
fn apply_primary_override(settings: &Settings, language: &str, resolved: &mut Vec<ResolvedServer>) {
    let Some(argv) = settings.lsp_servers.get(language) else {
        return;
    };
    let Some((command, _)) = argv.split_first() else {
        if !resolved.is_empty() {
            resolved.remove(0);
        }
        return;
    };
    match resolved.first_mut() {
        Some(primary) => {
            primary.name = command.clone();
            primary.source = ServerSource::Command(argv.clone());
        },
        None => resolved.push(ResolvedServer {
            name: command.clone(),
            source: ServerSource::Command(argv.clone()),
            only: HashSet::new(),
            except: HashSet::new(),
        }),
    }
}

/// Languages the builtin table serves. Must list every arm of
/// [`builtin_servers`] so [`builtin_by_name`] can search them all.
const BUILTIN_LANGUAGES: &[&str] = &["rust", "stcfg"];

/// The builtin server of a given name across all languages, if any. Server names
/// are unique across the builtin table.
fn builtin_by_name(name: &str) -> Option<&'static BuiltinServer> {
    BUILTIN_LANGUAGES
        .iter()
        .flat_map(|&language| builtin_servers(language))
        .find(|server| server.name == name)
}

/// Convert configured kebab-case feature names to a feature set, dropping and
/// warning on any name that does not resolve. `None` when no filter is set.
fn configured_features(names: Option<&Vec<String>>) -> Option<HashSet<LanguageServerFeature>> {
    let names = names?;
    Some(
        names
            .iter()
            .filter_map(|name| {
                let feature = LanguageServerFeature::from_config_name(name);
                if feature.is_none() {
                    tracing::warn!(
                        target: "stoat::lsp",
                        feature = %name,
                        "unknown lsp feature name in config",
                    );
                }
                feature
            })
            .collect(),
    )
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

    #[test]
    fn server_list_resolves_names_in_declared_order() {
        let mut settings = Settings::default();
        settings.lsp_server_lists.insert(
            "rust".to_string(),
            vec!["ra-full".to_string(), "ra-lite".to_string()],
        );
        settings
            .lsp_commands
            .insert("ra-full".to_string(), vec!["rust-analyzer".to_string()]);
        settings.lsp_commands.insert(
            "ra-lite".to_string(),
            vec!["ra".to_string(), "--lite".to_string()],
        );
        assert_eq!(
            names_and_argv(&settings, "rust"),
            vec![
                ("ra-full".to_string(), vec!["rust-analyzer".to_string()]),
                (
                    "ra-lite".to_string(),
                    vec!["ra".to_string(), "--lite".to_string()]
                ),
            ],
        );
    }

    #[test]
    fn bare_name_without_command_resolves_name_as_argv() {
        let mut settings = Settings::default();
        settings
            .lsp_server_lists
            .insert("rust".to_string(), vec!["pyright".to_string()]);
        assert_eq!(
            names_and_argv(&settings, "rust"),
            vec![("pyright".to_string(), vec!["pyright".to_string()])],
        );
    }

    #[test]
    fn only_and_except_filters_land_on_the_server() {
        use crate::host::LanguageServerFeature;
        use std::collections::HashSet;

        let mut settings = Settings::default();
        settings
            .lsp_server_lists
            .insert("rust".to_string(), vec!["ra".to_string()]);
        settings.lsp_only.insert(
            "ra".to_string(),
            vec!["hover".to_string(), "goto-definition".to_string()],
        );
        settings
            .lsp_except
            .insert("ra".to_string(), vec!["format".to_string()]);

        let servers = resolve_servers(&settings, "rust");
        assert_eq!(servers.len(), 1);
        assert_eq!(
            servers[0].only,
            HashSet::from([
                LanguageServerFeature::Hover,
                LanguageServerFeature::GotoDefinition
            ]),
        );
        assert_eq!(
            servers[0].except,
            HashSet::from([LanguageServerFeature::Format]),
        );
    }

    #[test]
    fn primary_override_applies_on_top_of_a_list() {
        let mut settings = Settings::default();
        settings
            .lsp_server_lists
            .insert("rust".to_string(), vec!["a".to_string(), "b".to_string()]);
        settings.lsp_servers.insert(
            "rust".to_string(),
            vec!["custom".to_string(), "--x".to_string()],
        );
        assert_eq!(
            names_and_argv(&settings, "rust"),
            vec![
                (
                    "custom".to_string(),
                    vec!["custom".to_string(), "--x".to_string()]
                ),
                ("b".to_string(), vec!["b".to_string()]),
            ],
        );
    }
}

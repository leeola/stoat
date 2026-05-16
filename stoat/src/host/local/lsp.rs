use crate::host::lsp::{LspHost, LspServer};
use async_trait::async_trait;
use std::{collections::BTreeMap, io, path::Path};
use stoat_config::LanguageServerCommand;
use stoat_language::Language;
use stoat_scheduler::Executor;

mod server;

pub use server::LocalLsp;

/// Production [`LspHost`] factory. Looks up the per-language server
/// command from a snapshot of [`stoat_config::Settings::language_servers`]
/// captured at construction time and spawns a stdio-backed
/// [`LocalLsp`] for each `launch` call.
///
/// A missing entry for the requested language returns an
/// [`io::ErrorKind::NotFound`] error -- callers see "this language has
/// no LSP configured" rather than a silent noop server.
pub struct LocalLspHost {
    language_servers: BTreeMap<String, LanguageServerCommand>,
    executor: Executor,
}

impl LocalLspHost {
    /// Build a factory keyed on `language_servers` (the
    /// `Settings::language_servers` snapshot at startup) and the
    /// canonical [`Executor`]. The map is not re-read after
    /// construction; settings reload at runtime does not retroactively
    /// reconfigure this host.
    pub fn new(
        language_servers: BTreeMap<String, LanguageServerCommand>,
        executor: Executor,
    ) -> Self {
        Self {
            language_servers,
            executor,
        }
    }
}

#[async_trait]
impl LspHost for LocalLspHost {
    async fn launch(&self, language: &Language, _root: &Path) -> io::Result<Box<dyn LspServer>> {
        let cmd = self.language_servers.get(language.name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no LSP server configured for language '{}'", language.name),
            )
        })?;
        let server = LocalLsp::spawn(self.executor.clone(), cmd).await?;
        Ok(Box::new(server))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use stoat_language::LanguageRegistry;
    use stoat_scheduler::TokioScheduler;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn executor() -> Executor {
        Arc::new(TokioScheduler::new(tokio::runtime::Handle::current())).executor()
    }

    #[test]
    fn launch_for_unconfigured_language_returns_not_found() {
        rt().block_on(async {
            let host = LocalLspHost::new(BTreeMap::new(), executor());
            let registry = LanguageRegistry::standard();
            let lang = registry
                .for_path(Path::new("main.rs"))
                .expect("rust language registered");
            let err = match host.launch(&lang, Path::new("/tmp")).await {
                Ok(_) => panic!("expected NotFound for unconfigured language"),
                Err(e) => e,
            };
            assert_eq!(err.kind(), io::ErrorKind::NotFound);
            assert!(
                err.to_string().contains("rust"),
                "error should name the language, got {err}"
            );
        });
    }

    #[test]
    fn launch_for_configured_language_spawns_via_command() {
        rt().block_on(async {
            let mut servers = BTreeMap::new();
            servers.insert(
                "rust".into(),
                LanguageServerCommand {
                    command: "/nonexistent/rust-analyzer-for-test".into(),
                    args: vec![],
                    env: BTreeMap::new(),
                },
            );
            let host = LocalLspHost::new(servers, executor());
            let registry = LanguageRegistry::standard();
            let lang = registry
                .for_path(Path::new("main.rs"))
                .expect("rust language registered");
            let err = match host.launch(&lang, Path::new("/tmp")).await {
                Ok(_) => panic!("missing binary should fail to spawn"),
                Err(e) => e,
            };
            assert_eq!(
                err.kind(),
                io::ErrorKind::NotFound,
                "expected spawn-side NotFound from the OS, got {err}"
            );
        });
    }
}

use crate::host::lsp::{LspHost, LspServer, NoopLspServer};
use async_trait::async_trait;
use std::{io, path::Path};
use stoat_language::Language;

mod server;

pub use server::LocalLsp;

/// Production [`LspHost`] factory. Resolves the per-language server
/// command from settings and spawns a stdio-backed [`LspServer`].
///
/// FIXME: until the settings-driven launch path lands,
/// [`Self::launch`] returns [`NoopLspServer`] unconditionally. The
/// `Settings::language_servers` lookup and [`LocalLsp::spawn`] wiring
/// land in the follow-up sibling item.
pub struct LocalLspHost;

#[async_trait]
impl LspHost for LocalLspHost {
    async fn launch(&self, _language: &Language, _root: &Path) -> io::Result<Box<dyn LspServer>> {
        Ok(Box::new(NoopLspServer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::lsp::LanguageServerFeature;
    use stoat_language::LanguageRegistry;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn local_host_launches_noop_server_until_transport_lands() {
        rt().block_on(async {
            let host = LocalLspHost;
            let registry = LanguageRegistry::standard();
            let lang = registry
                .for_path(Path::new("main.rs"))
                .expect("rust language registered");
            let server = host.launch(&lang, Path::new("/tmp")).await.expect("launch");
            assert!(server.capabilities().position_encoding.is_none());
            assert!(!server.supports_feature(LanguageServerFeature::Completion));
        });
    }
}

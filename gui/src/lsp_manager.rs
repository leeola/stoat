//! Persistent per-`(workspace, language)` language-server cache.
//!
//! Replaces the "launch a fresh server per request" pattern: each
//! `(workspace_root, language)` pair gets one [`LspServer`] that is launched and
//! `initialize`d once, then reused for every later request and document-sync
//! notification. The cache is a workspace-owned gpui entity; cached servers live
//! until the manager is dropped.
//!
//! This is the foundation the request sites and buffer-lifecycle document sync
//! build on; wiring those callers to the cache is layered on top.

use gpui::{Context, Task};
use lsp_types::{DidOpenTextDocumentParams, TextDocumentItem, Uri};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};
use stoat::host::{LspHost, LspServer};
use stoat_language::Language;

/// `file://` URI for an absolute path, or `None` when the path is not valid
/// UTF-8.
pub(crate) fn path_to_uri(path: &Path) -> Option<Uri> {
    Uri::from_str(&format!("file://{}", path.to_str()?)).ok()
}

/// Caches one initialized [`LspServer`] per `(workspace_root, language)`.
///
/// [`Self::server`] returns the cached handle, or launches and `initialize`s a
/// fresh server once and caches it before resolving. Keeping a single
/// persistent server per language lets it retain the synced document state that
/// completion, hover, and diagnostics are answered against, rather than starting
/// a process that has never seen the buffer for every request.
pub struct LspManager {
    servers: HashMap<(PathBuf, &'static str), Arc<dyn LspServer>>,
}

impl LspManager {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
        }
    }

    /// The already-launched server for `(root, language)`, if one is cached.
    pub fn cached(&self, root: &Path, language: &Language) -> Option<Arc<dyn LspServer>> {
        self.servers
            .get(&(root.to_path_buf(), language.name))
            .cloned()
    }

    /// Resolve the persistent server for `(root, language)`: the cached handle,
    /// or a freshly launched one, `initialize`d once with `root_uri` and cached
    /// before it resolves. Resolves to `None` when launch or initialization
    /// fails.
    pub fn server(
        &mut self,
        host: Arc<dyn LspHost>,
        language: Arc<Language>,
        root: PathBuf,
        root_uri: Option<Uri>,
        cx: &mut Context<'_, Self>,
    ) -> Task<Option<Arc<dyn LspServer>>> {
        let name = language.name;
        if let Some(server) = self.servers.get(&(root.clone(), name)) {
            let server = server.clone();
            return Task::ready(Some(server));
        }
        cx.spawn(async move |this, cx| {
            let boxed = host.launch(&language, &root).await.ok()?;
            let server: Arc<dyn LspServer> = Arc::from(boxed);
            server.initialize(root_uri).await.ok()?;
            // FIXME: two concurrent first requests for the same key both launch
            // a server. `or_insert` keeps the cache to a single entry (the
            // loser's server drops), so this is wasteful, not incorrect; dedup
            // the in-flight launch with a shared future.
            let cached = this
                .update(cx, |manager, _| {
                    manager
                        .servers
                        .entry((root, name))
                        .or_insert(server)
                        .clone()
                })
                .ok()?;
            Some(cached)
        })
    }

    /// Send `textDocument/didOpen` for `document` to the `(root, language)`
    /// server, launching and initializing it first if needed. Fire-and-forget:
    /// the notification is dropped if the server cannot be reached.
    pub fn did_open(
        &mut self,
        host: Arc<dyn LspHost>,
        language: Arc<Language>,
        root: PathBuf,
        document: TextDocumentItem,
        cx: &mut Context<'_, Self>,
    ) {
        let root_uri = path_to_uri(&root);
        let server = self.server(host, language, root, root_uri, cx);
        cx.spawn(async move |_this, _cx| {
            if let Some(server) = server.await {
                let params = DidOpenTextDocumentParams {
                    text_document: document,
                };
                let _ = server.did_open(params).await;
            }
        })
        .detach();
    }
}

impl Default for LspManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::LanguageRegistry;
    use gpui::{AppContext, TestAppContext};
    use stoat::host::fake::{FakeLsp, FakeLspHost};

    fn language_for(file: &str) -> Arc<Language> {
        LanguageRegistry::standard()
            .0
            .for_path(Path::new(file))
            .expect("standard registry resolves the language")
    }

    #[test]
    fn caches_one_server_per_workspace_language() {
        let mut cx = TestAppContext::single();
        let host: Arc<dyn LspHost> = Arc::new(FakeLspHost::new(Arc::new(FakeLsp::new())));
        let manager = cx.update(|cx| cx.new(|_| LspManager::new()));
        let rust = language_for("main.rs");
        let json = language_for("data.json");
        let root = PathBuf::from("/repo");

        assert!(manager.read_with(&cx, |m, _| m.cached(&root, &rust).is_none()));

        let _launch = manager.update(&mut cx, |m, cx| {
            m.server(host.clone(), rust.clone(), root.clone(), None, cx)
        });
        cx.run_until_parked();
        let rust_server = manager
            .read_with(&cx, |m, _| m.cached(&root, &rust))
            .expect("rust server cached after first request");

        let _reuse = manager.update(&mut cx, |m, cx| {
            m.server(host.clone(), rust.clone(), root.clone(), None, cx)
        });
        cx.run_until_parked();
        let rust_again = manager
            .read_with(&cx, |m, _| m.cached(&root, &rust))
            .expect("rust server still cached");
        assert!(
            Arc::ptr_eq(&rust_server, &rust_again),
            "same workspace and language reuse one server",
        );

        let _other = manager.update(&mut cx, |m, cx| {
            m.server(host.clone(), json.clone(), root.clone(), None, cx)
        });
        cx.run_until_parked();
        let json_server = manager
            .read_with(&cx, |m, _| m.cached(&root, &json))
            .expect("json server cached");
        assert!(
            !Arc::ptr_eq(&rust_server, &json_server),
            "a different language launches its own server",
        );
    }

    #[test]
    fn did_open_sends_the_document_to_the_server() {
        let mut cx = TestAppContext::single();
        let fake = Arc::new(FakeLsp::new());
        let host: Arc<dyn LspHost> = Arc::new(FakeLspHost::new(fake.clone()));
        let manager = cx.update(|cx| cx.new(|_| LspManager::new()));
        let rust = language_for("main.rs");
        let root = PathBuf::from("/repo");
        let uri = path_to_uri(Path::new("/repo/main.rs")).expect("uri");
        let document = TextDocumentItem {
            uri: uri.clone(),
            language_id: "rust".to_string(),
            version: 0,
            text: "fn main() {}".to_string(),
        };

        manager.update(&mut cx, |m, cx| {
            m.did_open(host, rust, root, document, cx);
        });
        cx.run_until_parked();

        let opens = fake.observed_opens();
        assert_eq!(opens.len(), 1, "did_open reaches the server once");
        assert_eq!(opens[0].text_document.uri, uri);
        assert_eq!(opens[0].text_document.text, "fn main() {}");
    }
}

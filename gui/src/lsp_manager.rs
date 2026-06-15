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

use futures::{future::Shared, FutureExt};
use gpui::{Context, Task};
use lsp_types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    TextDocumentIdentifier, TextDocumentItem, Uri,
};
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

/// An in-progress server launch, shared so every concurrent caller awaiting one
/// key resolves from a single launch. See [`LspManager::in_flight`].
type SharedLaunch = Shared<Task<Option<Arc<dyn LspServer>>>>;

/// Caches one initialized [`LspServer`] per `(workspace_root, language)`.
///
/// [`Self::server`] returns the cached handle, or launches and `initialize`s a
/// fresh server once and caches it before resolving. Keeping a single
/// persistent server per language lets it retain the synced document state that
/// completion, hover, and diagnostics are answered against, rather than starting
/// a process that has never seen the buffer for every request.
pub struct LspManager {
    servers: HashMap<(PathBuf, &'static str), Arc<dyn LspServer>>,

    /// Launches in progress, keyed like [`Self::servers`].
    ///
    /// While a server is launching, its key maps to a shared future that
    /// every concurrent caller awaits, so the host launches one server per
    /// key rather than one per request. The launch task removes its entry and
    /// promotes the server into [`Self::servers`] once it resolves.
    in_flight: HashMap<(PathBuf, &'static str), SharedLaunch>,
}

impl LspManager {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
            in_flight: HashMap::new(),
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
    ///
    /// Concurrent first requests for one key share a single launch rather than
    /// each starting their own server.
    pub fn server(
        &mut self,
        host: Arc<dyn LspHost>,
        language: Arc<Language>,
        root: PathBuf,
        root_uri: Option<Uri>,
        cx: &mut Context<'_, Self>,
    ) -> Task<Option<Arc<dyn LspServer>>> {
        let key = (root, language.name);

        if let Some(server) = self.servers.get(&key) {
            return Task::ready(Some(server.clone()));
        }

        if let Some(in_flight) = self.in_flight.get(&key) {
            let in_flight = in_flight.clone();
            return cx.spawn(async move |_, _| in_flight.await);
        }

        let launch = cx
            .spawn({
                let key = key.clone();
                async move |this, cx| {
                    let server = launch_and_initialize(host, language, &key.0, root_uri).await;
                    this.update(cx, |manager, _| {
                        manager.in_flight.remove(&key);
                        server.map(|server| manager.servers.entry(key).or_insert(server).clone())
                    })
                    .ok()
                    .flatten()
                }
            })
            .shared();

        self.in_flight.insert(key, launch.clone());
        cx.spawn(async move |_, _| launch.await)
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

    /// Send `textDocument/didChange` for the `(root, language)` server.
    /// Fire-and-forget; assumes the document was already opened via
    /// [`Self::did_open`].
    pub fn did_change(
        &mut self,
        host: Arc<dyn LspHost>,
        language: Arc<Language>,
        root: PathBuf,
        change: DidChangeTextDocumentParams,
        cx: &mut Context<'_, Self>,
    ) {
        let root_uri = path_to_uri(&root);
        let server = self.server(host, language, root, root_uri, cx);
        cx.spawn(async move |_this, _cx| {
            if let Some(server) = server.await {
                let _ = server.did_change(change).await;
            }
        })
        .detach();
    }

    /// Send `textDocument/didClose` for `uri` to the `(root, language)` server.
    /// Fire-and-forget.
    pub fn did_close(
        &mut self,
        host: Arc<dyn LspHost>,
        language: Arc<Language>,
        root: PathBuf,
        uri: Uri,
        cx: &mut Context<'_, Self>,
    ) {
        let root_uri = path_to_uri(&root);
        let server = self.server(host, language, root, root_uri, cx);
        cx.spawn(async move |_this, _cx| {
            if let Some(server) = server.await {
                let params = DidCloseTextDocumentParams {
                    text_document: TextDocumentIdentifier { uri },
                };
                let _ = server.did_close(params).await;
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

/// Launch `language`'s server under `root` and run the one-time `initialize`
/// handshake with `root_uri`. `None` if either step fails.
async fn launch_and_initialize(
    host: Arc<dyn LspHost>,
    language: Arc<Language>,
    root: &Path,
    root_uri: Option<Uri>,
) -> Option<Arc<dyn LspServer>> {
    let boxed = host.launch(&language, root).await.ok()?;
    let server: Arc<dyn LspServer> = Arc::from(boxed);
    server.initialize(root_uri).await.ok()?;
    Some(server)
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
    fn concurrent_first_requests_launch_one_server() {
        let mut cx = TestAppContext::single();
        let fake = Arc::new(FakeLsp::new());
        let host: Arc<dyn LspHost> = Arc::new(FakeLspHost::new(fake.clone()));
        let manager = cx.update(|cx| cx.new(|_| LspManager::new()));
        let rust = language_for("main.rs");
        let root = PathBuf::from("/repo");

        let (_first, _second) = manager.update(&mut cx, |m, cx| {
            let first = m.server(host.clone(), rust.clone(), root.clone(), None, cx);
            let second = m.server(host.clone(), rust.clone(), root.clone(), None, cx);
            (first, second)
        });
        cx.run_until_parked();

        assert_eq!(
            fake.launch_count(),
            1,
            "concurrent first requests for one key share a single launch",
        );
        assert!(
            manager.read_with(&cx, |m, _| m.cached(&root, &rust).is_some()),
            "the deduped launch still populates the cache",
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

    #[test]
    fn did_change_sends_the_update_to_the_server() {
        let mut cx = TestAppContext::single();
        let fake = Arc::new(FakeLsp::new());
        let host: Arc<dyn LspHost> = Arc::new(FakeLspHost::new(fake.clone()));
        let manager = cx.update(|cx| cx.new(|_| LspManager::new()));
        let rust = language_for("main.rs");
        let root = PathBuf::from("/repo");
        let uri = path_to_uri(Path::new("/repo/main.rs")).expect("uri");
        let change = DidChangeTextDocumentParams {
            text_document: lsp_types::VersionedTextDocumentIdentifier { uri, version: 2 },
            content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "edited".to_string(),
            }],
        };

        manager.update(&mut cx, |m, cx| {
            m.did_change(host, rust, root, change, cx);
        });
        cx.run_until_parked();

        let changes = fake.observed_changes();
        assert_eq!(changes.len(), 1, "did_change reaches the server once");
        assert_eq!(changes[0].content_changes[0].text, "edited");
    }

    #[test]
    fn did_close_sends_the_close_to_the_server() {
        let mut cx = TestAppContext::single();
        let fake = Arc::new(FakeLsp::new());
        let host: Arc<dyn LspHost> = Arc::new(FakeLspHost::new(fake.clone()));
        let manager = cx.update(|cx| cx.new(|_| LspManager::new()));
        let rust = language_for("main.rs");
        let root = PathBuf::from("/repo");
        let uri = path_to_uri(Path::new("/repo/main.rs")).expect("uri");

        manager.update(&mut cx, |m, cx| {
            m.did_close(host, rust, root, uri.clone(), cx);
        });
        cx.run_until_parked();

        let closes = fake.observed_closes();
        assert_eq!(closes.len(), 1, "did_close reaches the server once");
        assert_eq!(closes[0].text_document.uri, uri);
    }
}

use crate::host::{LspHost, NoopLsp};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

/// Language servers keyed by server name, with a language-to-server table and
/// a per-server spawn-attempt guard.
///
/// Each language runs its own server. [`Self::route`] resolves a buffer's
/// language to its server, and [`Self::serves_language`] plus
/// [`Self::spawn_attempted`] gate spawning so each server starts at most once.
///
/// [`Self::set_sole_client`] injects a single host that serves every language,
/// used by tests and the legacy single-host path. It is kept in a slot
/// separate from the per-language [`Self::insert`] clients so the spawn gate
/// can tell "one real host covers everything" from "one language happens to
/// have a server up".
pub(crate) struct LspRegistry {
    clients: HashMap<String, Arc<dyn LspHost>>,
    languages: HashMap<String, String>,
    spawn_attempted: HashSet<String>,
    sole: Option<Arc<dyn LspHost>>,
    noop: Arc<dyn LspHost>,
}

impl LspRegistry {
    pub(crate) fn new() -> Self {
        Self {
            clients: HashMap::new(),
            languages: HashMap::new(),
            spawn_attempted: HashSet::new(),
            sole: None,
            noop: Arc::new(NoopLsp),
        }
    }

    /// Register `host` under server `name`, replacing any prior host for it.
    pub(crate) fn insert(&mut self, name: String, host: Arc<dyn LspHost>) {
        self.clients.insert(name, host);
    }

    /// Record that `language` is served by the server named `name`.
    pub(crate) fn set_language(&mut self, language: String, name: String) {
        self.languages.insert(language, name);
    }

    /// Inject a single host that serves every language, replacing all
    /// per-language state.
    ///
    /// Used when a host is supplied without a language (tests, the legacy
    /// single-host path). The host lands in a slot separate from the
    /// per-language clients so [`Self::serves_language`] reads it as "every
    /// language is served" rather than as one language's server.
    pub(crate) fn set_sole_client(&mut self, host: Arc<dyn LspHost>) {
        self.clients.clear();
        self.languages.clear();
        self.sole = Some(host);
    }

    /// The host serving `language` through the per-language table, if one is
    /// registered. Ignores the injected sole client.
    pub(crate) fn host_for_language(&self, language: &str) -> Option<Arc<dyn LspHost>> {
        let name = self.languages.get(language)?;
        self.clients.get(name).cloned()
    }

    /// Resolves a buffer of `language` to the host it should use, preferring
    /// its own server, then the injected sole client, then a noop.
    ///
    /// Unlike [`Self::sole_or_noop`], an unmapped language never borrows an
    /// unrelated single client, so a second language does not route to the
    /// first language's server.
    pub(crate) fn route(&self, language: &str) -> Arc<dyn LspHost> {
        if let Some(host) = self.host_for_language(language) {
            return host;
        }
        self.sole.clone().unwrap_or_else(|| self.noop.clone())
    }

    /// Returns a single active client for editor-wide and cross-language
    /// traffic, preferring the injected sole client, then the only
    /// per-language client when exactly one is up, then a noop.
    pub(crate) fn sole_or_noop(&self) -> Arc<dyn LspHost> {
        if let Some(sole) = &self.sole {
            return sole.clone();
        }
        match self.clients.values().next() {
            Some(host) if self.clients.len() == 1 => host.clone(),
            _ => self.noop.clone(),
        }
    }

    /// Whether a real (non-noop) host already serves `language`, so no spawn
    /// is needed. True when the language has its own server or a real injected
    /// sole client covers it.
    pub(crate) fn serves_language(&self, language: &str) -> bool {
        if let Some(host) = self.host_for_language(language) {
            return !host.is_noop();
        }
        self.sole.as_ref().is_some_and(|host| !host.is_noop())
    }

    /// Returns every host that can emit server-initiated traffic, the
    /// per-language clients plus any injected sole client.
    pub(crate) fn hosts(&self) -> Vec<Arc<dyn LspHost>> {
        let mut hosts: Vec<Arc<dyn LspHost>> = self.clients.values().cloned().collect();
        if let Some(sole) = &self.sole {
            hosts.push(sole.clone());
        }
        hosts
    }

    /// Record that a spawn was attempted for server `name`.
    pub(crate) fn mark_spawn_attempted(&mut self, name: String) {
        self.spawn_attempted.insert(name);
    }

    /// Whether a spawn was attempted for server `name`, so it is not retried
    /// even after a failure.
    pub(crate) fn spawn_attempted(&self, name: &str) -> bool {
        self.spawn_attempted.contains(name)
    }

    /// Whether any server spawn has been attempted.
    pub(crate) fn spawn_attempted_any(&self) -> bool {
        !self.spawn_attempted.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeLsp;

    fn fake() -> Arc<dyn LspHost> {
        Arc::new(FakeLsp::new())
    }

    #[test]
    fn sole_or_noop_returns_noop_when_empty() {
        assert!(LspRegistry::new().sole_or_noop().is_noop());
    }

    #[test]
    fn sole_or_noop_returns_the_single_client() {
        let mut registry = LspRegistry::new();
        let host = fake();
        registry.insert("rust-analyzer".into(), host.clone());
        assert!(Arc::ptr_eq(&registry.sole_or_noop(), &host));
    }

    #[test]
    fn host_for_language_routes_to_the_registered_server() {
        let mut registry = LspRegistry::new();
        let host = fake();
        registry.insert("rust-analyzer".into(), host.clone());
        registry.set_language("rust".into(), "rust-analyzer".into());
        assert!(Arc::ptr_eq(
            &registry.host_for_language("rust").expect("rust routes"),
            &host
        ));
        assert!(registry.host_for_language("python").is_none());
    }

    #[test]
    fn set_sole_client_replaces_all_clients() {
        let mut registry = LspRegistry::new();
        registry.insert("a".into(), fake());
        let host = fake();
        registry.set_sole_client(host.clone());
        assert!(Arc::ptr_eq(&registry.sole_or_noop(), &host));
    }

    #[test]
    fn route_prefers_language_then_sole_then_noop() {
        let mut registry = LspRegistry::new();
        assert!(registry.route("rust").is_noop());

        let rust = fake();
        registry.insert("rust-analyzer".into(), rust.clone());
        registry.set_language("rust".into(), "rust-analyzer".into());
        assert!(Arc::ptr_eq(&registry.route("rust"), &rust));
        assert!(registry.route("python").is_noop());
    }

    #[test]
    fn route_uses_the_injected_sole_client_for_unmapped_languages() {
        let mut registry = LspRegistry::new();
        let host = fake();
        registry.set_sole_client(host.clone());
        assert!(Arc::ptr_eq(&registry.route("anything"), &host));
    }

    #[test]
    fn serves_language_reflects_only_real_hosts() {
        let mut registry = LspRegistry::new();
        assert!(!registry.serves_language("rust"));

        registry.set_sole_client(Arc::new(NoopLsp));
        assert!(!registry.serves_language("rust"));

        registry.set_sole_client(fake());
        assert!(registry.serves_language("rust"));
    }

    #[test]
    fn serves_language_is_per_language_for_registered_servers() {
        let mut registry = LspRegistry::new();
        registry.insert("rust-analyzer".into(), fake());
        registry.set_language("rust".into(), "rust-analyzer".into());
        assert!(registry.serves_language("rust"));
        assert!(!registry.serves_language("python"));
    }

    #[test]
    fn spawn_attempted_is_per_name() {
        let mut registry = LspRegistry::new();
        assert!(!registry.spawn_attempted_any());
        registry.mark_spawn_attempted("rust-analyzer".into());
        assert!(registry.spawn_attempted("rust-analyzer"));
        assert!(!registry.spawn_attempted("pyright"));
        assert!(registry.spawn_attempted_any());
    }
}

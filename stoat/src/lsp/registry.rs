use crate::host::{LspHost, NoopLsp};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

/// Language servers keyed by server name, with a language-to-server table and a
/// per-server spawn-attempt guard.
///
/// This phase runs one server at a time. [`Self::sole_or_noop`] returns the
/// single client (or a shared noop), and [`Self::host_for_language`] lets a
/// buffer route to it by language. The per-server maps are populated so a later
/// phase can run several servers without reshaping this type.
pub(crate) struct LspRegistry {
    clients: HashMap<String, Arc<dyn LspHost>>,
    languages: HashMap<String, String>,
    spawn_attempted: HashSet<String>,
    noop: Arc<dyn LspHost>,
}

impl LspRegistry {
    pub(crate) fn new() -> Self {
        Self {
            clients: HashMap::new(),
            languages: HashMap::new(),
            spawn_attempted: HashSet::new(),
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

    /// Replace every client with a single one under a synthetic name.
    ///
    /// Used when a host is injected without a language (tests, or a pending
    /// install that recorded none), so [`Self::sole_or_noop`] returns it.
    pub(crate) fn set_sole_client(&mut self, host: Arc<dyn LspHost>) {
        self.clients.clear();
        self.clients.insert(String::from("default"), host);
    }

    /// The host serving `language`, if one is registered for it.
    pub(crate) fn host_for_language(&self, language: &str) -> Option<Arc<dyn LspHost>> {
        let name = self.languages.get(language)?;
        self.clients.get(name).cloned()
    }

    /// The single registered client, or the shared noop when zero (or, this
    /// phase, ambiguously many) are registered.
    pub(crate) fn sole_or_noop(&self) -> Arc<dyn LspHost> {
        match self.clients.values().next() {
            Some(host) if self.clients.len() == 1 => host.clone(),
            _ => self.noop.clone(),
        }
    }

    /// Every registered client, for server-initiated traffic pumps.
    pub(crate) fn hosts(&self) -> Vec<Arc<dyn LspHost>> {
        self.clients.values().cloned().collect()
    }

    /// Record that a spawn was attempted for server `name`.
    pub(crate) fn mark_spawn_attempted(&mut self, name: String) {
        self.spawn_attempted.insert(name);
    }

    /// Whether any server spawn has been attempted, the single-server gate.
    /// Once one server is on its way up, no other is spawned.
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
    fn spawn_attempted_tracks_server_names() {
        let mut registry = LspRegistry::new();
        assert!(!registry.spawn_attempted_any());
        registry.mark_spawn_attempted("rust-analyzer".into());
        assert!(registry.spawn_attempted_any());
    }
}

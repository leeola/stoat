use crate::host::{LanguageServerFeature, LspHost, NoopLsp, OffsetEncoding};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

/// One server serving a language, with the feature filters that decide which
/// requests route to it.
///
/// The filter mirrors helix. An empty `only` set means "every feature except
/// those in `except`", while a non-empty `only` restricts to exactly those,
/// still minus `except`. A language pairs a primary server with specialized
/// ones (e.g. a completion-only server) without their features overlapping.
#[derive(Debug, Clone)]
pub(crate) struct ServerSelector {
    pub(crate) name: String,
    pub(crate) only: HashSet<LanguageServerFeature>,
    pub(crate) except: HashSet<LanguageServerFeature>,
}

impl ServerSelector {
    /// Whether requests for `feature` route to this server.
    pub(crate) fn has_feature(&self, feature: LanguageServerFeature) -> bool {
        (self.only.is_empty() || self.only.contains(&feature)) && !self.except.contains(&feature)
    }

    /// A selector for `name` with no feature filter, serving every feature.
    #[cfg(test)]
    pub(crate) fn all(name: String) -> Self {
        Self {
            name,
            only: HashSet::new(),
            except: HashSet::new(),
        }
    }
}

/// Language servers keyed by server name, with an ordered per-language selector
/// table and a per-server spawn-attempt guard.
///
/// A language may run several servers. [`Self::hosts_for_language`] returns all
/// of them for document sync, and [`Self::route`] picks the primary.
///
/// [`Self::set_sole_client`] injects a single host that serves every language,
/// used by tests and the legacy single-host path. It is kept in a slot separate
/// from the per-language clients so the spawn gate can tell "one real host
/// covers everything" from "one language happens to have a server up".
pub(crate) struct LspRegistry {
    clients: HashMap<String, Arc<dyn LspHost>>,
    languages: HashMap<String, Vec<ServerSelector>>,
    /// Servers that route every buffer whatever its language, consulted after
    /// the buffer's own language servers.
    ///
    /// A shortcode expander or a spell checker has nothing to do with the
    /// language it sits in, and a buffer with no language at all still wants
    /// them. Kept beside the per-language map rather than copied into every
    /// entry, since a buffer whose language has no entry needs them too.
    ///
    /// Fan-out traffic only. [`Self::route`] and [`Self::sole_or_noop`] answer
    /// goto and rename, which a narrow global server must never take from a
    /// language's own server.
    globals: Vec<ServerSelector>,
    spawn_attempted: HashSet<String>,
    sole: Option<Arc<dyn LspHost>>,
    noop: Arc<dyn LspHost>,
    /// Bumped by every change to the set of servers, so a consumer caching a
    /// derived view of it can tell that view is stale without re-deriving it.
    ///
    /// Covers [`Self::clients`], [`Self::languages`], and [`Self::globals`],
    /// the three the server set is read out of. A method touching none of them
    /// leaves it alone, since a bump costs every cache keyed on it a rebuild.
    generation: u64,
}

impl LspRegistry {
    pub(crate) fn new() -> Self {
        Self {
            clients: HashMap::new(),
            languages: HashMap::new(),
            globals: Vec::new(),
            spawn_attempted: HashSet::new(),
            sole: None,
            noop: Arc::new(NoopLsp),
            generation: 0,
        }
    }

    /// How many times the server set has changed, for invalidating a cache of
    /// anything derived from it.
    ///
    /// Read-only on purpose. A writer outside the registry could stale a cache
    /// keyed on this without the registry having changed at all.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Register `host` under server `name`, replacing any prior host for it.
    pub(crate) fn insert(&mut self, name: String, host: Arc<dyn LspHost>) {
        self.clients.insert(name, host);
        self.generation = self.generation.wrapping_add(1);
    }

    /// Map `language` to a single default-feature server named `name`.
    ///
    /// A test convenience for the one-server-per-language case. Production sets
    /// the full selector list via [`Self::set_selectors`].
    #[cfg(test)]
    pub(crate) fn set_language(&mut self, language: String, name: String) {
        self.languages
            .insert(language, vec![ServerSelector::all(name)]);
        self.generation = self.generation.wrapping_add(1);
    }

    /// Set `language`'s ordered server selectors, replacing any prior list.
    pub(crate) fn set_selectors(&mut self, language: String, selectors: Vec<ServerSelector>) {
        self.languages.insert(language, selectors);
        self.generation = self.generation.wrapping_add(1);
    }

    /// Set the ordered selectors for servers that route every buffer,
    /// replacing any prior list.
    ///
    /// These rank after a buffer's own language servers, so a language's
    /// server still leads its routing order.
    pub(crate) fn set_global_selectors(&mut self, selectors: Vec<ServerSelector>) {
        self.globals = selectors;
        self.generation = self.generation.wrapping_add(1);
    }

    /// Append `selector` to `language`'s ordered server list, creating the list
    /// when the language has none yet.
    ///
    /// Unlike [`Self::set_selectors`], which replaces the whole list, this adds a
    /// single server so callers build a language's set incrementally. Install
    /// order is routing priority, so a later push ranks after earlier ones.
    #[cfg(test)]
    pub(crate) fn push_selector(&mut self, language: &str, selector: ServerSelector) {
        self.languages
            .entry(language.to_string())
            .or_default()
            .push(selector);
        self.generation = self.generation.wrapping_add(1);
    }

    /// Inject a single host that serves every language, replacing all
    /// per-language state.
    ///
    /// Used when a host is supplied without a language (tests, the legacy
    /// single-host path). The host lands in a slot separate from the
    /// per-language clients so [`Self::has_real_sole_client`] reads it as "every
    /// language is served" rather than as one language's server.
    pub(crate) fn set_sole_client(&mut self, host: Arc<dyn LspHost>) {
        self.clients.clear();
        self.languages.clear();
        self.sole = Some(host);
        self.generation = self.generation.wrapping_add(1);
    }

    /// The up client for `language`'s primary (first selector's) server, if any.
    /// Ignores the injected sole client.
    pub(crate) fn host_for_language(&self, language: &str) -> Option<Arc<dyn LspHost>> {
        self.languages
            .get(language)?
            .iter()
            .find_map(|selector| self.clients.get(&selector.name).cloned())
    }

    /// Whether a server named `name` is up.
    pub(crate) fn contains_client(&self, name: &str) -> bool {
        self.clients.contains_key(name)
    }

    /// The client registered under server `name`, if up.
    pub(crate) fn client(&self, name: &str) -> Option<Arc<dyn LspHost>> {
        self.clients.get(name).cloned()
    }

    /// Resolves a buffer of `language` to its primary host, preferring its own
    /// server, then the injected sole client, then a noop.
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
    /// traffic, preferring the injected sole client, then the only registered
    /// client when exactly one is up, then a noop.
    ///
    /// The one registered client is whichever is up, global or per-language.
    /// Nothing else being up is what makes it the editor's server.
    pub(crate) fn sole_or_noop(&self) -> Arc<dyn LspHost> {
        if let Some(sole) = &self.sole {
            return sole.clone();
        }
        match self.clients.values().next() {
            Some(host) if self.clients.len() == 1 => host.clone(),
            _ => self.noop.clone(),
        }
    }

    /// Whether a real (non-noop) host is injected as the sole client.
    ///
    /// The spawn gate uses this to suppress auto-spawn when a test or the legacy
    /// path has provided a host that already serves every language.
    pub(crate) fn has_real_sole_client(&self) -> bool {
        self.sole.as_ref().is_some_and(|host| !host.is_noop())
    }

    /// Whether any non-noop language server is running, across named clients and
    /// the injected sole client.
    ///
    /// Status surfaces gate on this rather than the sole-host probe, which
    /// reports a noop the moment two servers serve one language.
    pub(crate) fn has_active_host(&self) -> bool {
        self.named_hosts().iter().any(|(_, host)| !host.is_noop())
    }

    /// `language`'s own selectors followed by the global ones, in routing
    /// order.
    ///
    /// The order carries the rule. A language's servers lead, and the globals
    /// answer beside them rather than instead of them.
    fn selectors_for(&self, language: &str) -> impl Iterator<Item = &ServerSelector> {
        self.languages
            .get(language)
            .into_iter()
            .flatten()
            .chain(&self.globals)
    }

    /// Every up host serving `language`, for document-sync notifications that
    /// each running server must mirror.
    ///
    /// Falls back to the injected sole client when neither the language nor the
    /// globals have a server up.
    pub(crate) fn hosts_for_language(&self, language: &str) -> Vec<Arc<dyn LspHost>> {
        let hosts: Vec<Arc<dyn LspHost>> = self
            .selectors_for(language)
            .filter_map(|selector| self.clients.get(&selector.name).cloned())
            .collect();
        if !hosts.is_empty() {
            return hosts;
        }
        self.sole.iter().cloned().collect()
    }

    /// Whether any up host would serve `language`, the allocation-free predicate
    /// counterpart of [`Self::hosts_for_language`] for callers that only need to
    /// know a server exists rather than the hosts themselves.
    pub(crate) fn has_host_for_language(&self, language: &str) -> bool {
        self.selectors_for(language)
            .any(|selector| self.clients.contains_key(&selector.name))
            || self.sole.is_some()
    }

    /// The names of the running servers that route `language`, its own first
    /// and the globals after, in selector (routing) order.
    ///
    /// Only named servers appear. The nameless sole client is omitted, so the
    /// status bar can label each server it lists. Empty when no named server
    /// routing the language is up.
    pub(crate) fn names_for_language(&self, language: &str) -> Vec<String> {
        self.selectors_for(language)
            .filter(|selector| self.clients.contains_key(&selector.name))
            .map(|selector| selector.name.clone())
            .collect()
    }

    /// Every up host routing `language` whose selector routes `feature` and
    /// whose capabilities support it, each paired with its server name, the
    /// language's own servers first and the globals after.
    ///
    /// Requests fan out over the result (completion) or take the first (hover,
    /// goto, ...). Falls back to the injected sole client when nothing matches,
    /// so tests driving a single fake still route.
    pub(crate) fn hosts_with_feature(
        &self,
        language: &str,
        feature: LanguageServerFeature,
    ) -> Vec<(String, Arc<dyn LspHost>)> {
        let hosts: Vec<(String, Arc<dyn LspHost>)> = self
            .selectors_for(language)
            .filter(|selector| selector.has_feature(feature))
            .filter_map(|selector| {
                let host = self.clients.get(&selector.name)?;
                host.supports_feature(feature)
                    .then(|| (selector.name.clone(), host.clone()))
            })
            .collect();
        if !hosts.is_empty() {
            return hosts;
        }
        match &self.sole {
            Some(sole) if sole.supports_feature(feature) => {
                vec![(String::from("default"), sole.clone())]
            },
            _ => Vec::new(),
        }
    }

    /// Returns every host that can emit server-initiated traffic, the
    /// per-language clients plus any injected sole client.
    ///
    /// Owned, for a caller that outlives the borrow. A caller that does not
    /// wants [`Self::hosts_iter`], which allocates nothing.
    pub(crate) fn hosts(&self) -> Vec<Arc<dyn LspHost>> {
        self.hosts_iter().cloned().collect()
    }

    /// [`Self::hosts`] without the vector.
    pub(crate) fn hosts_iter(&self) -> impl Iterator<Item = &Arc<dyn LspHost>> {
        self.clients.values().chain(self.sole.as_ref())
    }

    /// Returns every host paired with its server name, for traffic that must
    /// attribute progress or diagnostics to the reporting server. The injected
    /// sole client is named `default`.
    ///
    /// Owned, for a caller that outlives the borrow. A caller that does not
    /// wants [`Self::named_hosts_iter`], which skips a string clone per server
    /// as well as the vector.
    pub(crate) fn named_hosts(&self) -> Vec<(String, Arc<dyn LspHost>)> {
        self.named_hosts_iter()
            .map(|(name, host)| (name.to_string(), host.clone()))
            .collect()
    }

    /// [`Self::named_hosts`] without the vector or the name clones.
    pub(crate) fn named_hosts_iter(&self) -> impl Iterator<Item = (&str, &Arc<dyn LspHost>)> {
        self.clients
            .iter()
            .map(|(name, host)| (name.as_str(), host))
            .chain(self.sole.as_ref().map(|sole| ("default", sole)))
    }

    /// Map each server name to the offset encoding it negotiated.
    ///
    /// Consumers that convert a published position (a diagnostic's range) to a
    /// byte offset look the reporting server up here. A server absent from the
    /// map is treated as UTF-16 by the caller, matching the LSP default.
    pub(crate) fn offset_encodings(&self) -> HashMap<String, OffsetEncoding> {
        self.named_hosts()
            .into_iter()
            .map(|(name, host)| (name, host.offset_encoding()))
            .collect()
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
    use lsp_types::{CompletionOptions, HoverProviderCapability, ServerCapabilities};

    fn fake() -> Arc<dyn LspHost> {
        Arc::new(FakeLsp::new())
    }

    fn fake_with(caps: ServerCapabilities) -> Arc<dyn LspHost> {
        let lsp = FakeLsp::new();
        lsp.set_capabilities(caps);
        Arc::new(lsp)
    }

    /// A consumer caching what the server set resolves to only rebuilds when the
    /// generation moves, so every method that can change that set has to move it.
    /// One that does not would leave the cache painting a stale answer.
    #[test]
    fn every_change_to_the_server_set_moves_the_generation() {
        let mut registry = LspRegistry::new();
        let mut seen = registry.generation();
        let mut moved = |registry: &LspRegistry, what: &str| {
            let now = registry.generation();
            assert_ne!(now, seen, "{what} left the generation where it was");
            seen = now;
        };

        registry.insert("ra".to_string(), fake());
        moved(&registry, "insert");

        registry.set_language("rust".to_string(), "ra".to_string());
        moved(&registry, "set_language");

        registry.set_selectors(
            "rust".to_string(),
            vec![ServerSelector::all("ra".to_string())],
        );
        moved(&registry, "set_selectors");

        registry.push_selector("rust", ServerSelector::all("second".to_string()));
        moved(&registry, "push_selector");

        registry.set_sole_client(fake());
        moved(&registry, "set_sole_client");
    }

    fn hover_caps() -> ServerCapabilities {
        ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            ..ServerCapabilities::default()
        }
    }

    fn completion_caps() -> ServerCapabilities {
        ServerCapabilities {
            completion_provider: Some(CompletionOptions::default()),
            ..ServerCapabilities::default()
        }
    }

    fn selector(name: &str, only: &[LanguageServerFeature]) -> ServerSelector {
        ServerSelector {
            name: name.to_string(),
            only: only.iter().copied().collect(),
            except: HashSet::new(),
        }
    }

    #[test]
    fn has_feature_honours_only_and_except() {
        let all = ServerSelector::all("s".into());
        assert!(all.has_feature(LanguageServerFeature::Hover));

        let only = selector("s", &[LanguageServerFeature::Completion]);
        assert!(only.has_feature(LanguageServerFeature::Completion));
        assert!(!only.has_feature(LanguageServerFeature::Hover));

        let mut except = ServerSelector::all("s".into());
        except.except.insert(LanguageServerFeature::Hover);
        assert!(!except.has_feature(LanguageServerFeature::Hover));
        assert!(except.has_feature(LanguageServerFeature::Completion));
    }

    #[test]
    fn hosts_with_feature_filters_by_selector_and_capabilities() {
        let mut registry = LspRegistry::new();
        // The primary advertises hover and completion. The secondary advertises
        // only completion, and its selector restricts it to completion too.
        registry.insert("primary".into(), fake_with(hover_caps()));
        registry.insert("secondary".into(), fake_with(completion_caps()));
        registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("primary".into()),
                selector("secondary", &[LanguageServerFeature::Completion]),
            ],
        );

        // The primary advertises hover. The secondary's selector excludes it.
        let hover = registry.hosts_with_feature("rust", LanguageServerFeature::Hover);
        assert_eq!(hover.len(), 1);
        assert_eq!(hover[0].0, "primary");

        // The primary lacks the completion capability. The secondary has it.
        let completion = registry.hosts_with_feature("rust", LanguageServerFeature::Completion);
        assert_eq!(completion.len(), 1);
        assert_eq!(completion[0].0, "secondary");
    }

    #[test]
    fn a_global_server_routes_every_language_and_none() {
        let mut registry = LspRegistry::new();
        registry.insert("emoji-ls".into(), fake_with(completion_caps()));
        registry.set_global_selectors(vec![ServerSelector::all("emoji-ls".into())]);

        for language in ["rust", "python", ""] {
            let hosts = registry.hosts_with_feature(language, LanguageServerFeature::Completion);
            assert_eq!(
                hosts
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
                vec!["emoji-ls"],
                "a global server answers for {language:?}, mapped or not",
            );
            assert_eq!(registry.hosts_for_language(language).len(), 1);
            assert!(registry.has_host_for_language(language));
            assert_eq!(registry.names_for_language(language), vec!["emoji-ls"]);
        }
    }

    #[test]
    fn a_languages_own_servers_rank_before_the_globals() {
        let mut registry = LspRegistry::new();
        registry.insert("rust-analyzer".into(), fake_with(completion_caps()));
        registry.insert("emoji-ls".into(), fake_with(completion_caps()));
        registry.set_selectors(
            "rust".into(),
            vec![ServerSelector::all("rust-analyzer".into())],
        );
        registry.set_global_selectors(vec![ServerSelector::all("emoji-ls".into())]);

        assert_eq!(
            registry.names_for_language("rust"),
            vec!["rust-analyzer", "emoji-ls"],
            "the language's own server leads its routing order",
        );
        assert_eq!(
            registry
                .hosts_with_feature("rust", LanguageServerFeature::Completion)
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["rust-analyzer", "emoji-ls"],
        );
    }

    #[test]
    fn a_global_server_never_answers_a_single_target_request() {
        let mut registry = LspRegistry::new();
        let rust_analyzer = fake_with(hover_caps());
        registry.insert("rust-analyzer".into(), rust_analyzer.clone());
        registry.insert("emoji-ls".into(), fake_with(hover_caps()));
        registry.set_selectors(
            "rust".into(),
            vec![ServerSelector::all("rust-analyzer".into())],
        );
        registry.set_global_selectors(vec![ServerSelector::all("emoji-ls".into())]);

        assert!(
            Arc::ptr_eq(&registry.route("rust"), &rust_analyzer),
            "goto and rename go to the language's own server",
        );
        assert!(
            registry.route("python").is_noop(),
            "and an unmapped language gets nothing rather than a global",
        );
    }

    #[test]
    fn hosts_with_feature_falls_back_to_sole_when_supported() {
        let mut registry = LspRegistry::new();
        registry.set_sole_client(fake_with(hover_caps()));
        assert_eq!(
            registry
                .hosts_with_feature("rust", LanguageServerFeature::Hover)
                .len(),
            1
        );
        assert!(registry
            .hosts_with_feature("rust", LanguageServerFeature::Completion)
            .is_empty());
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
    fn host_for_language_returns_the_primary_up_server() {
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
    fn hosts_for_language_returns_every_up_server() {
        let mut registry = LspRegistry::new();
        let primary = fake();
        let secondary = fake();
        registry.insert("rust-analyzer".into(), primary.clone());
        registry.insert("extra".into(), secondary.clone());
        registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("rust-analyzer".into()),
                ServerSelector::all("extra".into()),
            ],
        );
        let hosts = registry.hosts_for_language("rust");
        assert_eq!(hosts.len(), 2);
        assert!(hosts.iter().any(|h| Arc::ptr_eq(h, &primary)));
        assert!(hosts.iter().any(|h| Arc::ptr_eq(h, &secondary)));
    }

    #[test]
    fn has_real_sole_client_ignores_the_noop() {
        let mut registry = LspRegistry::new();
        assert!(!registry.has_real_sole_client());
        registry.set_sole_client(Arc::new(NoopLsp));
        assert!(!registry.has_real_sole_client());
        registry.set_sole_client(fake());
        assert!(registry.has_real_sole_client());
    }

    #[test]
    fn contains_client_checks_registration() {
        let mut registry = LspRegistry::new();
        registry.insert("rust-analyzer".into(), fake());
        assert!(registry.contains_client("rust-analyzer"));
        assert!(!registry.contains_client("pyright"));
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

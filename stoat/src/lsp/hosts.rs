//! Which language server answers for a given buffer, and for a given feature on
//! it.
//!
//! One buffer can have several servers running against it, each declaring which
//! features it serves, so every LSP request starts by asking which of them to
//! send it to. A single-target request takes the first that routes the feature
//! and advertises it; a fan-out request takes all of them. A buffer with no
//! language, or none of whose servers are up, falls back to a noop host, so a
//! caller never has to hold a `None`.

use crate::{
    action_handlers,
    app::Stoat,
    buffer::BufferId,
    host::{LanguageServerFeature, LspHost},
};
use futures::future;
use std::{sync::Arc, time::Duration};

/// Backstop on reaping every language server at quit, applied across all of
/// them together. Exceeds what one host's own shutdown and reap bounds add up
/// to, so this fires only for a host that hangs somewhere those do not cover.
const SHUTDOWN_LSP_TIMEOUT: Duration = Duration::from_millis(750);

/// The single active language server, or a noop when none is up.
///
/// Editor-wide LSP traffic (shutdown, notification pumps) routes through
/// this. Buffer-specific requests use [`lsp_for`].
pub(crate) fn lsp_host(stoat: &Stoat) -> Arc<dyn LspHost> {
    stoat.lsp_registry.sole_or_noop()
}

/// The language server that should serve `buffer_id`.
///
/// A buffer with a language routes to that language's own server, an
/// injected sole client, or a noop. A buffer with no language falls back
/// to the sole client, or a noop.
pub(crate) fn lsp_for(stoat: &Stoat, buffer_id: BufferId) -> Arc<dyn LspHost> {
    match action_handlers::lsp::lsp_language_name(&stoat.active_workspace().buffers, buffer_id) {
        Some(name) => stoat.lsp_registry.route(&name),
        None => stoat.lsp_registry.sole_or_noop(),
    }
}

/// Every language server that mirrors `buffer_id`'s document, for
/// fan-out of `did_open` / `did_change` / `did_save` / `did_close`.
///
/// Every running server for the buffer's language needs the document, so
/// this returns all of them (or the injected sole client when none are up).
pub(crate) fn hosts_for_buffer(stoat: &Stoat, buffer_id: BufferId) -> Vec<Arc<dyn LspHost>> {
    let name =
        action_handlers::lsp::lsp_language_name(&stoat.active_workspace().buffers, buffer_id)
            .unwrap_or_default();
    stoat.lsp_registry.hosts_for_language(&name)
}

/// The language server that should answer a single-target `feature` request
/// for `buffer_id`, the first of its language's servers whose selector
/// routes the feature and whose capabilities support it.
///
/// Falls back to [`lsp_host`] (a noop when nothing supports it), so a
/// caller's `supports_feature` guard still rejects unavailable features.
pub(crate) fn lsp_for_feature(
    stoat: &Stoat,
    buffer_id: BufferId,
    feature: LanguageServerFeature,
) -> Arc<dyn LspHost> {
    feature_hosts(stoat, buffer_id, feature)
        .into_iter()
        .next()
        .map(|(_, host)| host)
        .unwrap_or_else(|| lsp_host(stoat))
}

/// Every server, with its registry name, that routes `feature` for
/// `buffer_id`'s language and advertises it.
///
/// Fan-out requests (completion) dispatch to all of them; single-target
/// requests take the first via [`lsp_for_feature`].
pub(crate) fn feature_hosts(
    stoat: &Stoat,
    buffer_id: BufferId,
    feature: LanguageServerFeature,
) -> Vec<(String, Arc<dyn LspHost>)> {
    let name =
        action_handlers::lsp::lsp_language_name(&stoat.active_workspace().buffers, buffer_id)
            .unwrap_or_default();
    stoat.lsp_registry.hosts_with_feature(&name, feature)
}

/// Reap the language servers on quit.
///
/// Every host is shut down at once rather than in turn, so one server that
/// drags its exit out cannot spend the budget the others need. [`NoopLsp`]
/// and the test fake return immediately, so the call is unconditional.
/// Errors are ignored, the process being on its way out regardless.
///
/// [`SHUTDOWN_LSP_TIMEOUT`] is only a backstop against a host that hangs
/// somewhere its own bounds do not cover. It has to exceed those bounds, or
/// it cuts short the kill they exist to reach.
pub async fn shutdown_lsp(stoat: &Stoat) {
    let hosts = stoat.lsp_registry.hosts();
    let reaps = future::join_all(hosts.iter().map(|host| host.shutdown()));
    let _ = tokio::time::timeout(SHUTDOWN_LSP_TIMEOUT, reaps).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_lsp_reaps_the_server_on_quit() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let h = Stoat::test();
            assert!(!h.fake_lsp().was_shut_down());
            shutdown_lsp(&h.stoat).await;
            assert!(
                h.fake_lsp().was_shut_down(),
                "the quit teardown shuts the language server down",
            );
        });
    }
}

//! What a language server pushes at the editor, and what the editor does with
//! it.
//!
//! A server talks back on its own schedule. Diagnostics, progress, and messages
//! arrive as notifications it never expects an answer to, and configuration and
//! edit requests arrive as questions it blocks on until answered. Both are
//! drained here at the top of every event, before anything acts on editor
//! state.
//!
//! A server that has just finished starting up also lands here. Installing it
//! reopens every buffer of its language, since those were announced to whatever
//! placeholder was standing in.
//!
//! Both drains take a per-pass cap. A server that floods would otherwise starve
//! the event loop, and whatever is left drains on the next pass.

use crate::{
    app::{SpawnScope, Stoat},
    buffer::BufferId,
    buffer_registry::ReadOnly,
    host::LspHost,
    lsp::{util, watched_files},
    render::sanitize,
};
use lsp_types::{Registration, Unregistration};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat_text::Rope;

/// Drains every notification currently buffered on
/// [`crate::host::LspHost::try_recv_notification`] and dispatches
/// each by variant. `Progress` updates the [`crate::lsp::progress::LspProgressMap`];
/// other variants log via tracing for now and become future
/// per-feature consumer hooks. Cap is per-tick to avoid starving
/// the event loop on a pathological notification burst; the
/// remainder drains on the next update.
pub(crate) fn drain_lsp_notifications(stoat: &mut Stoat) {
    // Borrowed rather than collected, since this runs on every event and
    // the owned form clones a name string per server each time. The fields
    // the drain writes are disjoint from the registry it walks.
    let Stoat {
        lsp_registry,
        lsp_progress,
        diagnostics,
        lsp_message,
        lsp_doc_versions,
        workspaces,
        lsp_workspace,
        ..
    } = stoat;
    let buffers = &workspaces[*lsp_workspace].buffers;
    for (server, host) in lsp_registry.named_hosts_iter() {
        drain_notifications_from(
            server,
            host,
            lsp_progress,
            diagnostics,
            lsp_message,
            lsp_doc_versions,
            buffers,
        );
    }
}

#[cfg(test)]
pub(crate) fn drain_host_notifications(stoat: &mut Stoat, server: &str, host: &Arc<dyn LspHost>) {
    let Stoat {
        lsp_progress,
        diagnostics,
        lsp_message,
        lsp_doc_versions,
        workspaces,
        lsp_workspace,
        ..
    } = stoat;
    drain_notifications_from(
        server,
        host,
        lsp_progress,
        diagnostics,
        lsp_message,
        lsp_doc_versions,
        &workspaces[*lsp_workspace].buffers,
    );
}

/// Drain and answer server-to-client requests the LSP host has
/// queued, so a server that pulls configuration or requests an edit
/// does not block waiting on the editor.
///
/// Mirrors [`drain_lsp_notifications`] with a bounded
/// `now_or_never` loop over
/// [`crate::host::LspHost::try_recv_incoming_request`]. Each request
/// carries an id the server blocks on, so every one is answered on a
/// detached [`crate::host::LspHost::reply`] task. A `workspace/applyEdit`
/// mutates buffers synchronously here because it needs the whole app. Only
/// the reply is deferred.
pub(crate) fn drain_lsp_incoming_requests(stoat: &mut Stoat) {
    // Copied into a reused buffer rather than walked borrowed, because
    // answering a workspace/applyEdit needs the whole app and may
    // reach the registry. Refilling keeps the allocation off this per-event
    // path, and the handles themselves are refcount bumps.
    let mut hosts = std::mem::take(&mut stoat.lsp_drain_hosts);
    hosts.clear();
    hosts.extend(stoat.lsp_registry.hosts_iter().cloned());
    for host in &hosts {
        drain_incoming_requests_from(stoat, host);
    }
    // Emptied before it is parked, so a server that shuts down is not held
    // alive by a handle sitting in the scratch until the next event.
    hosts.clear();
    stoat.lsp_drain_hosts = hosts;
}

fn drain_incoming_requests_from(stoat: &mut Stoat, host: &Arc<dyn LspHost>) {
    use crate::host::lsp::{IncomingRequest, LspResponseError};
    use futures::FutureExt;
    use lsp_types::ApplyWorkspaceEditResponse;
    use serde_json::Value;

    for _ in 0..256 {
        let Some(slot) = host.try_recv_incoming_request().now_or_never() else {
            break;
        };
        let Some(request) = slot else {
            break;
        };

        let id = request.id().clone();
        let result: Result<Value, LspResponseError> = match request {
            IncomingRequest::WorkDoneProgressCreate { params, .. } => {
                tracing::trace!(target: "stoat::lsp", ?params, "workDoneProgress/create");
                Ok(Value::Null)
            },
            IncomingRequest::RegisterCapability { params, .. } => {
                tracing::debug!(target: "stoat::lsp", ?params, "client/registerCapability");
                register_file_watchers(stoat, host, &params.registrations);
                Ok(Value::Null)
            },
            IncomingRequest::UnregisterCapability { params, .. } => {
                tracing::debug!(target: "stoat::lsp", ?params, "client/unregisterCapability");
                unregister_file_watchers(stoat, host, &params.unregisterations);
                Ok(Value::Null)
            },
            IncomingRequest::WorkspaceConfiguration { params, .. } => {
                Ok(Value::Array(vec![Value::Null; params.items.len()]))
            },
            IncomingRequest::ShowMessageRequest { .. } => Ok(Value::Null),
            IncomingRequest::WorkspaceApplyEdit { params, .. } => {
                // The server that asked is the one whose units the edit is in.
                let response = match crate::lsp::edit_apply::apply_workspace_edit(
                    stoat,
                    params.edit,
                    host.offset_encoding(),
                ) {
                    Ok(_) => ApplyWorkspaceEditResponse {
                        applied: true,
                        failure_reason: None,
                        failed_change: None,
                    },
                    Err(err) => {
                        tracing::warn!(target: "stoat::lsp", %err, "workspace/applyEdit failed");
                        ApplyWorkspaceEditResponse {
                            applied: false,
                            failure_reason: Some(err.to_string()),
                            failed_change: None,
                        }
                    },
                };
                serde_json::to_value(response).map_err(|err| LspResponseError {
                    code: -32603,
                    message: err.to_string(),
                    data: None,
                })
            },
            IncomingRequest::Unknown { method, .. } => {
                tracing::debug!(target: "stoat::lsp", %method, "unhandled server->client request");
                Err(LspResponseError {
                    code: -32601,
                    message: "method not found".to_string(),
                    data: None,
                })
            },
        };

        let reply_host = host.clone();
        stoat
            .executor
            .spawn(async move {
                if let Err(err) = reply_host.reply(id, result).await {
                    tracing::warn!(target: "stoat::lsp", ?err, "lsp reply failed");
                }
            })
            .detach();
    }
}

/// Hold the file watchers among `registrations` for the server of `host`.
///
/// A registration replaces any that the server holds under the same id. A
/// server that reloads its project registers again under the old id, and the
/// replacement keeps one copy. A request that registers no file watchers
/// leaves [`Stoat::lsp_file_watchers`] as it was.
///
/// Each request that holds watchers starts a walk of the language-server root,
/// which refills the server's record of the files its watchers name.
fn register_file_watchers(
    stoat: &mut Stoat,
    host: &Arc<dyn LspHost>,
    registrations: &[Registration],
) {
    let compiled: Vec<_> = registrations
        .iter()
        .filter_map(watched_files::compile)
        .collect();
    if compiled.is_empty() {
        return;
    }
    let Some(server) = server_name(stoat, host) else {
        return;
    };

    let root = stoat.workspaces[stoat.lsp_workspace].git_root.clone();
    let held = stoat.lsp_file_watchers.entry(server).or_default();
    for registration in compiled {
        held.registrations
            .retain(|watching| watching.id != registration.id);
        held.registrations.push(registration);
    }
    held.walk(
        &stoat.executor,
        stoat.fs_host.clone(),
        root,
        stoat.drain_notify.clone(),
    );
}

/// Drop each file watcher of the server of `host` that `unregistrations` names
/// by id.
fn unregister_file_watchers(
    stoat: &mut Stoat,
    host: &Arc<dyn LspHost>,
    unregistrations: &[Unregistration],
) {
    let Some(server) = server_name(stoat, host) else {
        return;
    };
    let Some(held) = stoat.lsp_file_watchers.get_mut(&server) else {
        return;
    };

    held.registrations
        .retain(|watching| unregistrations.iter().all(|gone| gone.id != watching.id));
    if held.registrations.is_empty() {
        stoat.lsp_file_watchers.remove(&server);
    }
}

/// The name the registry holds `host` under, which keys the server's file
/// watchers.
///
/// The drain runs on every event, and a name carried for each server costs a
/// string clone on each one. The lookup happens here instead, where only the
/// rare registration requests pay for it.
fn server_name(stoat: &Stoat, host: &Arc<dyn LspHost>) -> Option<String> {
    stoat
        .lsp_registry
        .named_hosts_iter()
        .find(|(_, held)| Arc::ptr_eq(held, host))
        .map(|(name, _)| name.to_string())
}

/// Install every language server that finished spawning since the last
/// tick.
///
/// The lazy-spawn tasks armed by
/// [`crate::lsp::session::notify_buffer_opened`] park ready
/// [`crate::host::LocalLsp`] hosts in [`pending_lsp_host`]. This
/// drains the queue. Each ready host is registered via
/// [`install_ready_server`], and a failed spawn surfaces in the
/// message row while its language keeps the [`NoopLsp`] placeholder.
pub(crate) fn install_pending_lsp_host(stoat: &mut Stoat) {
    let pending = std::mem::take(
        &mut *stoat
            .pending_lsp_host
            .lock()
            .expect("pending lsp host mutex"),
    );
    for spawn in pending {
        match spawn.result {
            Ok(host) => install_ready_server(stoat, spawn.server, spawn.scope, host),
            Err(msg) => {
                // The server never came up, so its language keeps the noop
                // placeholder and the failure surfaces in the status bar
                // rather than only the log. Retained so a later LSP action
                // can restate why no server is up.
                stoat.lsp_spawn_failed = Some(msg.clone());
                stoat.set_status(format!("lsp: {msg}"));
            },
        }
    }
}

/// Register a ready `host` under its `server` name, install it into the
/// routing list its `scope` names, then deliver the documents it now serves.
///
/// A language server takes only its own language's buffers, leaving other
/// languages' servers untouched. A global server serves every open buffer, so
/// it takes every one of them.
fn install_ready_server(
    stoat: &mut Stoat,
    server: String,
    scope: SpawnScope,
    host: Arc<dyn LspHost>,
) {
    // Watchers belong to the process that registered them. A server that
    // comes up again registers its own.
    stoat.lsp_file_watchers.remove(&server);
    stoat.lsp_registry.insert(server.clone(), Arc::clone(&host));
    let language = match &scope {
        SpawnScope::Language(language) => {
            let selectors = crate::lsp::servers::resolve_servers(&stoat.settings, language)
                .iter()
                .map(|resolved| resolved.to_selector())
                .collect();
            stoat
                .lsp_registry
                .set_selectors(language.clone(), selectors);
            Some(language.clone())
        },
        SpawnScope::Global => {
            let selectors = crate::lsp::servers::resolve_global_servers(&stoat.settings)
                .iter()
                .map(|resolved| resolved.to_selector())
                .collect();
            stoat.lsp_registry.set_global_selectors(selectors);
            None
        },
    };

    open_on_new_server(stoat, &server, &host, language.as_deref());
}

/// Deliver the documents a server that just came up now serves, the buffers of
/// `language` or every buffer for a global server.
///
/// A buffer that another running server already holds opens on `host` alone,
/// at the text and version the others hold. Reopening it on every host sends
/// the others a second didOpen with no didClose between, which restarts their
/// document version. Every other buffer reopens as [`reopen_buffers`] does.
///
/// The change scan runs last. A change already waiting in its debounce goes
/// only to the hosts it was armed for, so the scan replaces it with one over
/// every host, measured from the text the new server took.
fn open_on_new_server(
    stoat: &mut Stoat,
    server: &str,
    host: &Arc<dyn LspHost>,
    language: Option<&str>,
) {
    let mut reopen = Vec::new();
    for (id, path, text) in covered_buffers(stoat, language) {
        if stoat.lsp_opened.contains(&id)
            && held_by_another_server(stoat, server, id)
            && crate::lsp::session::open_on_host(stoat, Arc::clone(host), id, &path)
        {
            stoat.lsp_buffer_versions.remove(&id);
        } else {
            reopen.push((id, path, text));
        }
    }
    reopen_each(stoat, reopen);
    crate::lsp::sync::notify_buffer_changes_pending(stoat);
}

/// Whether a running server other than `server` routes the language of `id`.
fn held_by_another_server(stoat: &Stoat, server: &str, id: BufferId) -> bool {
    let language =
        crate::lsp::session::lsp_language_name(&stoat.workspaces[stoat.lsp_workspace].buffers, id)
            .unwrap_or_default();
    stoat
        .lsp_registry
        .names_for_language(&language)
        .iter()
        .any(|name| name != server)
}

/// Re-send `did_open` for the path-bound buffers of [`Stoat::lsp_workspace`],
/// every one of them or only `language`'s.
///
/// Each buffer leaves [`Stoat::lsp_opened`] first. A host that never saw a
/// buffer then receives it, and a buffer that calls for a server not yet
/// started starts it. A server that just came up and a restored session both
/// need this, because no `did_open` for their buffers reached that server.
pub(crate) fn reopen_buffers(stoat: &mut Stoat, language: Option<&str>) {
    let buffers = covered_buffers(stoat, language);
    reopen_each(stoat, buffers);
}

/// The path-bound buffers of [`Stoat::lsp_workspace`] in `language`, or every
/// one of them when `language` is `None`, each with its path and rope.
///
/// A compressed buffer is left out. Its text is a gzip file's decompressed
/// content, which disagrees with the file the server indexes under that path.
fn covered_buffers(stoat: &Stoat, language: Option<&str>) -> Vec<(BufferId, PathBuf, Rope)> {
    // Ropes rather than strings, because this runs over every buffer it
    // covers in one turn and a rope clone is a refcount bump where
    // materializing each buffer is not.
    let buffers = &stoat.workspaces[stoat.lsp_workspace].buffers;
    buffers
        .open_paths()
        .into_iter()
        .filter_map(|path| {
            let id = buffers.id_for_path(&path)?;
            if buffers.read_only(id) == Some(ReadOnly::Compressed) {
                return None;
            }
            if let Some(language) = language
                && crate::lsp::session::lsp_language_name(buffers, id).as_deref() != Some(language)
            {
                return None;
            }
            let text = buffers
                .get(id)?
                .read()
                .expect("buffer poisoned")
                .rope()
                .clone();
            Some((id, path, text))
        })
        .collect()
}

/// Send `did_open` for each of `buffers` to every host, from version 0.
fn reopen_each(stoat: &mut Stoat, buffers: Vec<(BufferId, PathBuf, Rope)>) {
    for (id, path, text) in buffers {
        stoat.lsp_opened.remove(&id);
        stoat.lsp_doc_versions.remove(&id);
        stoat.lsp_buffer_versions.remove(&id);
        let workspace = stoat.lsp_workspace;
        crate::lsp::session::notify_buffer_opened(stoat, workspace, id, &path, text);
    }
}

fn drain_notifications_from(
    server: &str,
    host: &Arc<dyn LspHost>,
    progress: &mut crate::lsp::progress::LspProgressMap,
    diagnostics: &mut crate::diagnostics::DiagnosticSet,
    message: &mut Option<(lsp_types::MessageType, String)>,
    doc_versions: &std::collections::HashMap<BufferId, i32>,
    buffers: &crate::buffer_registry::BufferRegistry,
) {
    use crate::host::LspNotification;
    use futures::FutureExt;
    for _ in 0..256 {
        // try_recv_notification is implemented on top of a non-blocking channel
        // poll, so its future resolves synchronously and now_or_never returns
        // Some immediately. Any host that breaks that contract returns None
        // here and the drain ends safely.
        let Some(slot) = host.try_recv_notification().now_or_never() else {
            break;
        };
        let Some(notification) = slot else {
            break;
        };
        if progress.update(server, &notification) {
            continue;
        }
        match notification {
            LspNotification::Diagnostics {
                uri,
                diagnostics: published,
                version,
            } => {
                if let Some(path) = util::lsp_uri_to_path(&uri) {
                    if let Some(stale) =
                        stale_publish_version(&path, version, doc_versions, buffers)
                    {
                        tracing::debug!(
                            target: "stoat::lsp",
                            path = %path.display(),
                            published = stale,
                            "diagnostics for an older document version; dropped",
                        );
                        continue;
                    }
                    let count = published.len();
                    let spans =
                        util::publish_spans(&path, &published, host.offset_encoding(), buffers);
                    diagnostics.replace_from_server(
                        path.clone(),
                        server.to_string(),
                        published,
                        spans,
                    );
                    tracing::debug!(
                        target: "stoat::lsp",
                        server,
                        path = %path.display(),
                        count,
                        "diagnostics applied",
                    );
                } else {
                    tracing::debug!(
                        target: "stoat::lsp",
                        uri = uri.as_str(),
                        "diagnostics arrived for non-file URI; dropped",
                    );
                }
            },
            LspNotification::ShowMessage { typ, message: text } => {
                let text = sanitize::sanitize_status_text(&format!("{server}: {text}"));
                *message = Some((typ, text));
            },
            other => {
                tracing::debug!(
                    target: "stoat::lsp",
                    ?other,
                    "unhandled LSP notification"
                );
            },
        }
    }
}

/// The version a publish names when it is behind the document stoat has since
/// sent, or `None` when the publish should be applied.
///
/// A server measures its diagnostics against a document version it was given,
/// and says which. One arriving after stoat has sent a newer document describes
/// text that has moved, so applying it would overwrite fresher marks with ones
/// computed for text nobody is looking at.
///
/// A publish with no version is applied. A server not tracking versions has
/// nothing to be behind, and so does a buffer stoat has never announced.
fn stale_publish_version(
    path: &Path,
    version: Option<i32>,
    doc_versions: &std::collections::HashMap<BufferId, i32>,
    buffers: &crate::buffer_registry::BufferRegistry,
) -> Option<i32> {
    let version = version?;
    let current = buffers
        .id_for_path(path)
        .and_then(|id| doc_versions.get(&id))?;
    (version < *current).then_some(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::PendingSpawn,
        host::{lsp::IncomingRequest, FakeLsp, LspNotification},
        test_fixture,
        test_harness::TestHarness,
    };
    use lsp_types::{
        notification::{DidChangeWatchedFiles, Notification},
        GlobPattern, MessageType, NumberOrString, UnregistrationParams,
    };
    use stoat_config::Settings;

    /// The text of `a.rs` as [`edited_with_only_a_up`] opens it.
    const OPENED: &str = "fn a() {}\n";
    /// The text of `a.rs` after the edit [`edited_with_only_a_up`] makes.
    const EDITED: &str = "// x\nfn a() {}\n";

    /// A server start or a restore reopens every covered buffer, and a
    /// compressed one stays out, as its open left it out.
    #[test]
    fn reopening_leaves_a_compressed_buffer_closed() {
        let mut h = Stoat::test();
        let buffers = &mut h.stoat.active_workspace_mut().buffers;
        let (plain, _) = buffers.open(Path::new("/w/a.rs"), "fn a() {}\n");
        let (packed, _) = buffers.open(Path::new("/w/b.rs.gz"), "fn b() {}\n");
        buffers.set_read_only(packed, Some(ReadOnly::Compressed));

        reopen_buffers(&mut h.stoat, None);

        assert_eq!(
            (
                h.stoat.lsp_opened.contains(&plain),
                h.stoat.lsp_opened.contains(&packed)
            ),
            (true, false)
        );
    }

    #[test]
    fn show_message_is_attributed_to_its_server() {
        use crate::host::{FakeLsp, LspHost, LspNotification};
        use lsp_types::MessageType;
        let mut h = Stoat::test();
        let fake = Arc::new(FakeLsp::new());
        fake.push_notification(LspNotification::ShowMessage {
            typ: MessageType::ERROR,
            message: "workspace load failed".to_string(),
        });
        let host: Arc<dyn LspHost> = fake;

        drain_host_notifications(&mut h.stoat, "rust-analyzer", &host);

        assert_eq!(
            h.stoat.lsp_message.as_ref().map(|(_, m)| m.as_str()),
            Some("rust-analyzer: workspace load failed"),
        );
    }

    /// A server message of several paragraphs is stored as one line, since
    /// every surface that shows it paints a single row of cells.
    #[test]
    fn a_multi_line_show_message_is_stored_as_one_line() {
        let mut h = Stoat::test();
        let fake = Arc::new(FakeLsp::new());
        fake.push_notification(LspNotification::ShowMessage {
            typ: MessageType::ERROR,
            message: "failed to fetch workspace\n\nno Cargo.toml above the file".to_string(),
        });
        let host: Arc<dyn LspHost> = fake;

        drain_host_notifications(&mut h.stoat, "rust-analyzer", &host);

        assert_eq!(
            h.stoat.lsp_message.as_ref().map(|(_, m)| m.as_str()),
            Some("rust-analyzer: failed to fetch workspace no Cargo.toml above the file"),
        );
    }

    #[test]
    fn lsp_spawn_failure_surfaces_in_message_row() {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(scheduler.executor(), Settings::default(), PathBuf::new());
        stoat
            .pending_lsp_host
            .lock()
            .expect("pending lsp host mutex")
            .push(PendingSpawn {
                server: "rust-analyzer".to_string(),
                scope: SpawnScope::Language("rust".to_string()),
                result: Err("rust-analyzer: NotFound".to_string()),
            });

        install_pending_lsp_host(&mut stoat);

        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("lsp: rust-analyzer: NotFound")
        );
        assert!(
            crate::lsp::hosts::lsp_host(&stoat).is_noop(),
            "the placeholder stays after a spawn failure"
        );
    }

    #[test]
    fn lsp_ready_host_installs_without_a_message() {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(scheduler.executor(), Settings::default(), PathBuf::new());
        let host: Arc<dyn LspHost> = Arc::new(FakeLsp::new());
        stoat
            .pending_lsp_host
            .lock()
            .expect("pending lsp host mutex")
            .push(PendingSpawn {
                server: "rust-analyzer".to_string(),
                scope: SpawnScope::Language("rust".to_string()),
                result: Ok(host),
            });

        install_pending_lsp_host(&mut stoat);

        assert!(
            !crate::lsp::hosts::lsp_host(&stoat).is_noop(),
            "a ready host replaces the placeholder"
        );
        assert_eq!(
            stoat.pending_message, None,
            "a successful install shows no message"
        );
    }

    /// A server that comes up beside one already holding a buffer opens that
    /// buffer on itself alone, at the text and version the first one holds. A
    /// second didOpen to the first server, with no didClose between, restarts
    /// its document version.
    #[test]
    fn a_second_server_opens_a_held_buffer_on_itself_alone() {
        use std::time::Duration;
        let mut h = Stoat::test();
        let (a, b) = edited_with_only_a_up(&mut h);
        h.advance_clock(Duration::from_millis(60));

        install_b(&mut h, &b);

        assert_eq!(
            (opens(&a), opens(&b)),
            (vec![(OPENED.to_string(), 0)], vec![(EDITED.to_string(), 1)]),
        );
    }

    /// An edit still in its debounce at the install was armed for `a` alone.
    /// The install replaces it with one change for both servers, measured from
    /// the text `b` opened with.
    #[test]
    fn a_second_server_gets_the_change_pending_at_its_install() {
        use std::time::Duration;
        let mut h = Stoat::test();
        let (a, b) = edited_with_only_a_up(&mut h);

        install_b(&mut h, &b);
        h.advance_clock(Duration::from_millis(60));

        assert_eq!(
            (opens(&b), changes(&a), changes(&b)),
            (
                vec![(OPENED.to_string(), 1)],
                vec![(2, EDITED.to_string())],
                vec![(2, EDITED.to_string())],
            ),
        );
    }

    /// Two full-sync rust servers `a` and `b`, with only `a` up and `a.rs` open
    /// and edited. The edit's change waits in its debounce.
    fn edited_with_only_a_up(h: &mut TestHarness) -> (Arc<FakeLsp>, Arc<FakeLsp>) {
        use crate::lsp::registry::ServerSelector;
        use lsp_types::{ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind};
        use stoat_action::OpenFile;

        let full_sync = || {
            let fake = Arc::new(FakeLsp::new());
            fake.set_capabilities(ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                ..Default::default()
            });
            fake
        };
        let (a, b) = (full_sync(), full_sync());
        h.stoat
            .settings
            .lsp_server_lists
            .insert("rust".into(), vec!["a".into(), "b".into()]);
        h.stoat.lsp_registry.insert("a".into(), a.clone());
        h.stoat
            .lsp_registry
            .set_selectors("rust".into(), vec![ServerSelector::all("a".into())]);

        let path = PathBuf::from("/ws/a.rs");
        h.fake_fs().insert_file(&path, OPENED.as_bytes());
        crate::action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        h.edit_focused(0..0, "// x\n");
        crate::lsp::sync::notify_buffer_changes_pending(&mut h.stoat);
        (a, b)
    }

    /// Install `b` as a ready rust server, and let what it sends go out.
    fn install_b(h: &mut TestHarness, b: &Arc<FakeLsp>) {
        h.stoat
            .pending_lsp_host
            .lock()
            .expect("pending lsp host mutex")
            .push(PendingSpawn {
                server: "b".to_string(),
                scope: SpawnScope::Language("rust".to_string()),
                result: Ok(b.clone()),
            });
        install_pending_lsp_host(&mut h.stoat);
        h.settle();
    }

    /// The text and version of every didOpen `fake` saw.
    fn opens(fake: &FakeLsp) -> Vec<(String, i32)> {
        fake.observed_opens()
            .into_iter()
            .map(|open| (open.text_document.text, open.text_document.version))
            .collect()
    }

    /// The version and full text of every didChange `fake` saw.
    fn changes(fake: &FakeLsp) -> Vec<(i32, String)> {
        fake.observed_changes()
            .into_iter()
            .flat_map(|change| {
                let version = change.text_document.version;
                change
                    .content_changes
                    .into_iter()
                    .map(move |event| (version, event.text))
            })
            .collect()
    }

    #[test]
    fn diagnostics_notification_with_non_file_uri_dropped() {
        use crate::host::LspNotification;
        use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range, Uri};
        use std::str::FromStr;
        let mut h = Stoat::test();
        let uri = Uri::from_str("https://example.com/a.rs").unwrap();
        let diag = Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 5)),
            severity: Some(DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: None,
            message: "ignored".into(),
            related_information: None,
            tags: None,
            data: None,
        };
        h.fake_lsp()
            .push_notification(LspNotification::Diagnostics {
                uri,
                diagnostics: vec![diag],
                version: None,
            });
        h.drain_lsp();
        let summary = h.stoat.diagnostics.summarize(Path::new("/ws/a.rs"));
        assert!(summary.is_empty());
    }

    #[test]
    fn incoming_apply_edit_mutates_buffer_and_replies_applied() {
        use crate::host::lsp::IncomingRequest;
        use lsp_types::{
            ApplyWorkspaceEditParams, ApplyWorkspaceEditResponse, DocumentChanges, NumberOrString,
            OneOf, OptionalVersionedTextDocumentIdentifier, Position, Range, TextDocumentEdit,
            TextEdit, WorkspaceEdit,
        };
        use std::path::PathBuf;

        let mut h = Stoat::test();
        let path = PathBuf::from("/ws/a.rs");
        h.fake_fs().insert_file(&path, b"abcde\n");
        h.stoat
            .active_workspace_mut()
            .buffers
            .open(&path, "abcde\n");

        let uri = crate::action_handlers::lsp::path_to_uri(&path).expect("uri");
        let edit = WorkspaceEdit {
            changes: None,
            document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier { uri, version: None },
                edits: vec![OneOf::Left(TextEdit {
                    range: Range::new(Position::new(0, 1), Position::new(0, 4)),
                    new_text: "X".to_string(),
                })],
            }])),
            change_annotations: None,
        };
        let id = NumberOrString::Number(7);
        h.fake_lsp()
            .push_incoming_request(IncomingRequest::WorkspaceApplyEdit {
                id: id.clone(),
                params: ApplyWorkspaceEditParams { label: None, edit },
            });
        drain_lsp_incoming_requests(&mut h.stoat);
        h.settle();

        let buffer_id = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(&path)
            .expect("buffer");
        let text = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .unwrap()
            .read()
            .unwrap()
            .rope()
            .to_string();
        assert_eq!(text, "aXe\n");

        let applied = serde_json::to_value(ApplyWorkspaceEditResponse {
            applied: true,
            failure_reason: None,
            failed_change: None,
        })
        .unwrap();
        assert_eq!(h.fake_lsp().observed_replies(), vec![(id, Ok(applied))]);
    }

    /// A server's file watchers are held from its registration until its
    /// unregistration. A second registration under one id replaces the first.
    #[test]
    fn file_watchers_are_held_until_unregistered() {
        let mut h = Stoat::test();
        let rust_files = || {
            test_fixture::watched_files_registration(
                "w1",
                GlobPattern::String("**/*.rs".into()),
                None,
            )
        };
        test_fixture::register_capability(&mut h, rust_files());
        test_fixture::register_capability(&mut h, rust_files());

        let held: Vec<(&str, Vec<&str>)> = h
            .stoat
            .lsp_file_watchers
            .iter()
            .map(|(server, watchers)| {
                let ids = watchers
                    .registrations
                    .iter()
                    .map(|registration| registration.id.as_str())
                    .collect();
                (server.as_str(), ids)
            })
            .collect();
        assert_eq!(held, [("default", vec!["w1"])]);

        h.fake_lsp()
            .push_incoming_request(IncomingRequest::UnregisterCapability {
                id: NumberOrString::Number(2),
                params: UnregistrationParams {
                    unregisterations: vec![Unregistration {
                        id: "w1".into(),
                        method: DidChangeWatchedFiles::METHOD.into(),
                    }],
                },
            });
        drain_lsp_incoming_requests(&mut h.stoat);

        assert!(
            h.stoat.lsp_file_watchers.is_empty(),
            "the last unregistration drops the server's entry"
        );
    }

    #[test]
    fn incoming_configuration_replies_null_per_item() {
        use crate::host::lsp::IncomingRequest;
        use lsp_types::{ConfigurationItem, ConfigurationParams, NumberOrString};

        let mut h = Stoat::test();
        let id = NumberOrString::Number(8);
        let item = |section: &str| ConfigurationItem {
            scope_uri: None,
            section: Some(section.to_string()),
        };
        h.fake_lsp()
            .push_incoming_request(IncomingRequest::WorkspaceConfiguration {
                id: id.clone(),
                params: ConfigurationParams {
                    items: vec![item("a"), item("b")],
                },
            });
        drain_lsp_incoming_requests(&mut h.stoat);
        h.settle();

        let nulls = serde_json::Value::Array(vec![serde_json::Value::Null; 2]);
        assert_eq!(h.fake_lsp().observed_replies(), vec![(id, Ok(nulls))]);
    }

    #[test]
    fn incoming_unknown_request_replies_method_not_found() {
        use crate::host::lsp::{IncomingRequest, LspResponseError};
        use lsp_types::NumberOrString;

        let mut h = Stoat::test();
        let id = NumberOrString::Number(9);
        h.fake_lsp()
            .push_incoming_request(IncomingRequest::Unknown {
                id: id.clone(),
                method: "experimental/foo".to_string(),
                params: serde_json::Value::Null,
            });
        drain_lsp_incoming_requests(&mut h.stoat);
        h.settle();

        assert_eq!(
            h.fake_lsp().observed_replies(),
            vec![(
                id,
                Err(LspResponseError {
                    code: -32601,
                    message: "method not found".to_string(),
                    data: None,
                }),
            )],
        );
    }
}

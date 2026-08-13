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

use crate::{action_handlers, app::Stoat, buffer::BufferId, host::LspHost, lsp::util};
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
        active_workspace,
        ..
    } = stoat;
    let buffers = &workspaces[*active_workspace].buffers;
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
        active_workspace,
        ..
    } = stoat;
    drain_notifications_from(
        server,
        host,
        lsp_progress,
        diagnostics,
        lsp_message,
        lsp_doc_versions,
        &workspaces[*active_workspace].buffers,
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
                tracing::debug!(target: "stoat::lsp", ?params, "workDoneProgress/create");
                Ok(Value::Null)
            },
            IncomingRequest::RegisterCapability { params, .. } => {
                tracing::debug!(target: "stoat::lsp", ?params, "client/registerCapability");
                Ok(Value::Null)
            },
            IncomingRequest::UnregisterCapability { params, .. } => {
                tracing::debug!(target: "stoat::lsp", ?params, "client/unregisterCapability");
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

/// Install every language server that finished spawning since the last
/// tick.
///
/// The lazy-spawn tasks armed by
/// [`action_handlers::lsp::notify_buffer_opened`] park ready
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
            Ok(host) => install_ready_server(stoat, spawn.server, spawn.language, host),
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

/// Register a ready `host` under its `server` name and `language`, then
/// re-fire `did_open` for the open buffers of that language.
///
/// Those buffers already sent `did_open` to the noop while the server was
/// starting, so they are dropped from [`lsp_opened`] and reopened to
/// deliver the documents to the real server. Buffers of other languages
/// keep their own servers untouched.
fn install_ready_server(
    stoat: &mut Stoat,
    server: String,
    language: String,
    host: Arc<dyn LspHost>,
) {
    stoat.lsp_registry.insert(server, host);
    let selectors = crate::lsp::servers::resolve_servers(&stoat.settings, &language)
        .iter()
        .map(|resolved| resolved.to_selector())
        .collect();
    stoat
        .lsp_registry
        .set_selectors(language.clone(), selectors);

    // Ropes rather than strings, because this runs over every open buffer
    // of the language in one turn and a rope clone is a refcount bump where
    // materializing each buffer is not.
    let reopen: Vec<(BufferId, PathBuf, Rope)> = {
        let buffers = &stoat.active_workspace().buffers;
        buffers
            .open_paths()
            .into_iter()
            .filter_map(|path| {
                let id = buffers.id_for_path(&path)?;
                if action_handlers::lsp::lsp_language_name(buffers, id).as_deref()
                    != Some(language.as_str())
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
    };

    for (id, path, text) in reopen {
        stoat.lsp_opened.remove(&id);
        stoat.lsp_doc_versions.remove(&id);
        stoat.lsp_buffer_versions.remove(&id);
        let workspace = stoat.active_workspace;
        action_handlers::lsp::notify_buffer_opened(stoat, workspace, id, &path, text);
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
                    tracing::info!(
                        target: "stoat::lsp",
                        path = %path.display(),
                        count,
                        "diagnostics applied",
                    );
                } else {
                    tracing::debug!(
                        target: "stoat::app",
                        uri = uri.as_str(),
                        "diagnostics arrived for non-file URI; dropped",
                    );
                }
            },
            LspNotification::ShowMessage { typ, message: text } => {
                *message = Some((typ, format!("{server}: {text}")));
            },
            other => {
                tracing::debug!(
                    target: "stoat::app",
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
    use crate::app::PendingSpawn;
    use stoat_config::Settings;

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
                language: "rust".to_string(),
                result: Err("rust-analyzer: NotFound".to_string()),
            });

        install_pending_lsp_host(&mut stoat);

        assert_eq!(
            stoat.pending_message.as_deref(),
            Some("lsp: rust-analyzer: NotFound")
        );
        assert!(
            stoat.lsp_host().is_noop(),
            "the placeholder stays after a spawn failure"
        );
    }

    #[test]
    fn lsp_ready_host_installs_without_a_message() {
        let scheduler = Arc::new(stoat_scheduler::TestScheduler::new());
        let mut stoat = Stoat::new(scheduler.executor(), Settings::default(), PathBuf::new());
        let host: Arc<dyn LspHost> = Arc::new(crate::host::FakeLsp::new());
        stoat
            .pending_lsp_host
            .lock()
            .expect("pending lsp host mutex")
            .push(PendingSpawn {
                server: "rust-analyzer".to_string(),
                language: "rust".to_string(),
                result: Ok(host),
            });

        install_pending_lsp_host(&mut stoat);

        assert!(
            !stoat.lsp_host().is_noop(),
            "a ready host replaces the placeholder"
        );
        assert_eq!(
            stoat.pending_message, None,
            "a successful install shows no message"
        );
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

        let uri = action_handlers::lsp::path_to_uri(&path).expect("uri");
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

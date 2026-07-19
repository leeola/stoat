//! LSP buffer-lifecycle plumbing. This module routes
//! [`crate::buffer::BufferId`] open / close / save / change events to
//! the workspace's [`crate::host::LspHost`] so a real language server
//! can keep its document mirror in sync with the editor.
//!
//! `did_open` fires synchronously per [`notify_buffer_opened`] and
//! `did_change` fires after a 50ms quiet window per
//! [`notify_buffer_changes_pending`]. `did_save` / `did_close` are
//! still pending; both wait on user-facing buffer-save / buffer-close
//! actions that do not yet exist.

use crate::{
    agent_ipc::AgentQuery,
    app::{PendingSpawn, Stoat, UpdateEffect},
    buffer::BufferId,
    buffer_registry::{BufferRegistry, LspSymbolKindIndex},
    display_map::{
        syntax_theme, DisplayPoint, DisplaySnapshot, HighlightKey, HighlightLayer, HighlightStyle,
        HighlightStyleInterner, InlayKind, SemanticTokenHighlight,
    },
    editor_state::EditorId,
    host::{LanguageServerFeature, LocalLsp, LspHost, LspTranscript, OffsetEncoding},
    location_picker::{LocationEntry, LocationPicker},
    lsp::{servers::ServerSource, LspSymbolKind},
    symbol_finder::{SymbolFinder, SymbolFinderEntry, SymbolTarget},
    theme::scope,
    workspace::WorkspaceUid,
};
use codegraph::SymbolKey;
pub(crate) use lsp_types::Uri;
use lsp_types::{
    CodeActionContext, CodeActionOrCommand, CodeActionParams, Diagnostic,
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, DocumentDiagnosticParams,
    DocumentDiagnosticReport, DocumentDiagnosticReportResult, DocumentFormattingParams,
    DocumentHighlight, DocumentHighlightKind, DocumentHighlightParams,
    DocumentRangeFormattingParams, DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse,
    Documentation, FoldingRange, FoldingRangeParams, FormattingOptions, GotoDefinitionParams,
    GotoDefinitionResponse, HoverContents, HoverParams, InlayHint, InlayHintLabel, InlayHintParams,
    MarkedString, MarkupKind, OneOf, ParameterLabel, Position, PrepareRenameResponse, Range,
    ReferenceContext, ReferenceParams, RenameParams, SemanticToken, SemanticTokenType,
    SemanticTokensParams, SemanticTokensResult, SemanticTokensServerCapabilities, ServerInfo,
    SignatureHelp, SignatureHelpParams, SignatureInformation, SymbolInformation, SymbolKind,
    TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit,
    VersionedTextDocumentIdentifier, WorkDoneProgressParams, WorkspaceEdit, WorkspaceSymbol,
    WorkspaceSymbolParams, WorkspaceSymbolResponse,
};
use ratatui::{layout::Rect, style::Style};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use stoat_text::{patch::Patch, Anchor, Bias, Point, Rope};
use tokio::sync::oneshot;

/// Quiet window after the last edit before a buffer's `did_change`
/// fires. Matches Helix's default and prevents per-keystroke storms
/// of LSP traffic.
pub(crate) const LSP_DID_CHANGE_DEBOUNCE: Duration = Duration::from_millis(50);

/// Direction for [`goto_diagnostic`]. `Next` searches forward from
/// the cursor's byte offset; `Prev` searches backward. Neither
/// wraps when the search exhausts.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DiagnosticDirection {
    Next,
    Prev,
}

/// Move the focused editor's primary cursor to the next or previous
/// LSP diagnostic for that buffer. No-op when the focused pane is
/// not an editor, the buffer has no path, or no diagnostic lies in
/// the requested direction.
pub(crate) fn goto_diagnostic(stoat: &mut Stoat, direction: DiagnosticDirection) -> UpdateEffect {
    let (cursor_offset, buffer_id, rope) = {
        let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let tail_off = buffer_snapshot.resolve_anchor(&sel.tail());
        let head_off = buffer_snapshot.resolve_anchor(&sel.head());
        let offset = stoat_text::cursor_offset(buffer_snapshot.rope(), tail_off, head_off);
        (offset, editor.buffer_id, buffer_snapshot.rope().clone())
    };

    let path = match stoat.active_workspace().buffers.path_for(buffer_id) {
        Some(p) => p.to_path_buf(),
        None => return UpdateEffect::None,
    };

    let encodings = stoat.lsp_registry.offset_encodings();
    let mut offsets: Vec<usize> = stoat
        .diagnostics
        .attributed(&path)
        .map(|(server, d)| {
            let encoding = encodings
                .get(server)
                .copied()
                .unwrap_or(OffsetEncoding::Utf16);
            crate::lsp::util::lsp_pos_to_byte_offset(&rope, d.range.start, encoding)
        })
        .collect();
    offsets.sort_unstable();

    let target = match direction {
        DiagnosticDirection::Next => offsets.into_iter().find(|&o| o > cursor_offset),
        DiagnosticDirection::Prev => offsets.into_iter().rev().find(|&o| o < cursor_offset),
    };

    let Some(target) = target else {
        return UpdateEffect::None;
    };

    crate::action_handlers::movement::jump_to_offset(stoat, target)
}

/// The language name used to route LSP traffic for `buffer_id`.
///
/// A grammar-backed buffer uses its tree-sitter language's name. A buffer with
/// no grammar (e.g. `.stcfg`) falls back to an extension-keyed LSP identity via
/// [`crate::lsp::servers::lsp_language_for_extension`], so an in-process server
/// can still serve it. `None` when neither resolves, leaving the buffer without
/// a language server.
pub(crate) fn lsp_language_name(buffers: &BufferRegistry, buffer_id: BufferId) -> Option<String> {
    if let Some(language) = buffers.language_for(buffer_id) {
        return Some(language.name.to_string());
    }
    let extension = buffers.path_for(buffer_id)?.extension()?.to_str()?;
    crate::lsp::servers::lsp_language_for_extension(extension).map(str::to_string)
}

/// Notify the workspace's LSP host that `buffer_id` was just opened.
/// No-op when `buffer_id` is already in [`Stoat::lsp_opened`]; that
/// dedupes the second `OpenFile` of an already-loaded buffer (which
/// is idempotent in [`crate::buffer_registry::BufferRegistry::open`]
/// but must fire `did_open` exactly once over the buffer's lifetime).
///
/// The dispatch is detached on the workspace's `Executor` because
/// `did_open` is a fire-and-forget notification; production
/// [`crate::host::LspHost`] implementations may write to a JSON-RPC
/// channel asynchronously, so blocking the open path on it would be
/// wrong. Errors are swallowed -- a notification failure is not
/// fatal to the open.
pub(crate) fn notify_buffer_opened(
    stoat: &mut Stoat,
    buffer_id: BufferId,
    path: &Path,
    text: &str,
) {
    maybe_spawn_language_server(stoat, buffer_id);
    if !stoat.lsp_opened.insert(buffer_id) {
        return;
    }
    let Some(uri) = path_to_uri(path) else {
        return;
    };
    let language_id = lsp_language_name(&stoat.active_workspace().buffers, buffer_id)
        .unwrap_or_else(|| "plaintext".to_string());
    let buffer_version = stoat
        .active_workspace()
        .buffers
        .get(buffer_id)
        .map(|b| b.read().expect("buffer lock").version())
        .unwrap_or(0);
    stoat.lsp_buffer_versions.insert(buffer_id, buffer_version);
    stoat.lsp_doc_versions.insert(buffer_id, 0);
    let text_arc = Arc::new(text.to_string());
    stoat
        .lsp_last_delivered_text
        .lock()
        .expect("lsp text mutex")
        .insert(buffer_id, Arc::clone(&text_arc));
    stoat
        .lsp_last_delivered_buffer_version
        .lock()
        .expect("lsp version mutex")
        .insert(buffer_id, buffer_version);
    for lsp in stoat.hosts_for_buffer(buffer_id) {
        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.clone(),
                language_id: language_id.clone(),
                version: 0,
                text: text_arc.as_ref().clone(),
            },
        };
        stoat
            .executor
            .spawn(async move {
                if let Err(err) = lsp.did_open(params).await {
                    tracing::warn!(target: "stoat::lsp", ?err, "did_open notification failed");
                }
            })
            .detach();
    }
}

/// Launch the language servers for `buffer_id`'s language the first time a
/// buffer of that language opens, registering each in [`Stoat::lsp_registry`]
/// once it is ready.
///
/// No-op unless auto-spawn is enabled and the language has known servers
/// ([`crate::lsp::servers::resolve_servers`]). Each of the language's servers
/// spawns at most once. A server already up or already spawn-attempted is
/// skipped, and a real injected sole host (tests, legacy) suppresses spawning
/// entirely. The binary opts into auto-spawn via [`Stoat::set_lsp_auto_spawn`].
/// Tests leave it off, so no server IO happens.
///
/// Each spawn plus `initialize` handshake runs detached on the workspace
/// [`Stoat::executor`] via [`spawn_server`]. The ready host, or the failure, is
/// parked in [`Stoat::pending_lsp_host`] for [`Stoat::update`] to install.
pub(crate) fn maybe_spawn_language_server(stoat: &mut Stoat, buffer_id: BufferId) {
    if !stoat.lsp_auto_spawn {
        return;
    }
    // A real injected sole host (tests, legacy) already serves every language.
    if stoat.lsp_registry.has_real_sole_client() {
        return;
    }
    let Some(language_name) = lsp_language_name(&stoat.active_workspace().buffers, buffer_id)
    else {
        return;
    };

    // Only the language's servers not already up and not already tried this
    // session. A failed spawn is never retried.
    let to_spawn: Vec<crate::lsp::servers::ResolvedServer> =
        crate::lsp::servers::resolve_servers(&stoat.settings, &language_name)
            .into_iter()
            .filter(|server| {
                !stoat.lsp_registry.contains_client(&server.name)
                    && !stoat.lsp_registry.spawn_attempted(&server.name)
            })
            .collect();
    if to_spawn.is_empty() {
        return;
    }

    // A subprocess server started before the direnv environment lands would run
    // under the wrong PATH, so defer those until install_pending re-fires them.
    // An in-process server has no process and no environment, so it starts now
    // regardless.
    let env_loading =
        stoat.active_workspace().env.state == crate::project_env::EnvLoadState::Loading;
    let mut deferred = false;
    for server in to_spawn {
        if env_loading && matches!(server.source, ServerSource::Command(_)) {
            deferred = true;
            continue;
        }
        stoat.lsp_registry.mark_spawn_attempted(server.name.clone());
        spawn_server(stoat, server, language_name.clone());
    }
    if deferred {
        stoat.lsp_spawn_deferred = Some(buffer_id);
    }
}

/// Spawn one resolved `server` for `language` detached on the workspace
/// executor, parking the ready host or the failure in
/// [`Stoat::pending_lsp_host`].
fn spawn_server(stoat: &mut Stoat, server: crate::lsp::servers::ResolvedServer, language: String) {
    let crate::lsp::servers::ResolvedServer { name, source, .. } = server;
    match source {
        ServerSource::Command(argv) => spawn_command_server(stoat, name, argv, language),
        ServerSource::InProcess(construct) => {
            spawn_in_process_server(stoat, name, construct, language)
        },
    }
}

/// Spawn a subprocess language server `command` with `argv`, initialize it under
/// the workspace environment, and park the result.
fn spawn_command_server(stoat: &mut Stoat, command: String, argv: Vec<String>, language: String) {
    let git_root = stoat.active_workspace().git_root.clone();
    let env = stoat.active_workspace().env.diff.clone();
    let root_uri = path_to_uri(&git_root);
    let slot = stoat.pending_lsp_host.clone();
    let wake = stoat.redraw_notify.clone();
    let transcript = if stoat.settings.text_proto_log == Some(true) {
        match LspTranscript::create(&command) {
            Ok(transcript) => Some(transcript),
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "text_proto_log transcript disabled");
                None
            },
        }
    } else {
        None
    };

    let args: Vec<String> = argv.into_iter().skip(1).collect();

    stoat
        .executor
        .spawn(async move {
            let host: Arc<dyn LspHost> =
                match LocalLsp::spawn(&command, &args, &env, &git_root, transcript, wake) {
                    Ok(host) => Arc::new(host),
                    Err(err) => {
                        tracing::warn!(target: "stoat::lsp", ?err, %command, "language server spawn failed");
                        slot.lock().expect("pending lsp host mutex").push(PendingSpawn {
                            server: command.clone(),
                            language: language.clone(),
                            result: Err(format!("{command}: {err}")),
                        });
                        return;
                    },
                };
            match host.initialize(root_uri).await {
                Ok(result) => {
                    tracing::info!(
                        target: "stoat::lsp",
                        %command,
                        server = %server_label(result.server_info.as_ref()),
                        "language server initialized",
                    );
                },
                Err(err) => {
                    tracing::warn!(target: "stoat::lsp", ?err, %command, "language server initialize failed");
                    slot.lock().expect("pending lsp host mutex").push(PendingSpawn {
                        server: command.clone(),
                        language: language.clone(),
                        result: Err(format!("{command}: {err}")),
                    });
                    return;
                },
            }
            slot.lock().expect("pending lsp host mutex").push(PendingSpawn {
                server: command,
                language,
                result: Ok(host),
            });
        })
        .detach();
}

/// Build an in-process language server `name` for `language` on the workspace
/// executor and park the result.
///
/// There is no subprocess and no environment overlay. An in-process host emits
/// no server-initiated traffic, so once parked it wakes the redraw loop itself;
/// otherwise nothing would drive [`Stoat::install_pending_lsp_host`].
fn spawn_in_process_server(
    stoat: &mut Stoat,
    name: String,
    construct: fn() -> Arc<dyn LspHost>,
    language: String,
) {
    let slot = stoat.pending_lsp_host.clone();
    let wake = stoat.redraw_notify.clone();

    stoat
        .executor
        .spawn(async move {
            let host = construct();
            let result = match host.initialize(None).await {
                Ok(_) => {
                    tracing::info!(target: "stoat::lsp", %name, "in-process language server initialized");
                    Ok(host)
                },
                Err(err) => {
                    tracing::warn!(target: "stoat::lsp", ?err, %name, "in-process language server initialize failed");
                    Err(format!("{name}: {err}"))
                },
            };
            slot.lock().expect("pending lsp host mutex").push(PendingSpawn {
                server: name,
                language,
                result,
            });
            wake.notify_one();
        })
        .detach();
}

/// A language server's `name@version` identity from its
/// `InitializeResult`, for logging. The version is omitted when the
/// server reported a name but no version, and the whole label is
/// "unknown" when the server reported no `serverInfo` at all.
fn server_label(info: Option<&ServerInfo>) -> String {
    let Some(info) = info else {
        return "unknown".to_string();
    };
    match &info.version {
        Some(version) => format!("{}@{}", info.name, version),
        None => info.name.clone(),
    }
}

/// Deliver `msg` as the transient status message.
fn set_lsp_status(stoat: &mut Stoat, msg: String) {
    stoat.set_status(msg);
}

/// Report why a user-launched LSP action for `what` cannot be served, then
/// return [`UpdateEffect::Redraw`] so the frame repaints with the message.
///
/// Walks the language-server state in priority order and reports the first
/// reason that applies. An installed host that simply lacks the capability
/// comes first, then each reason the [`NoopLsp`] placeholder is still in place,
/// namely that the spawn failed, is deferred until the project environment
/// loads, is still starting, or was never attempted.
fn report_lsp_unavailable(stoat: &mut Stoat, what: &str) -> UpdateEffect {
    let msg = if stoat.lsp_registry.has_active_host() {
        format!("lsp: server does not support {what}")
    } else if let Some(err) = &stoat.lsp_spawn_failed {
        format!("lsp: {err}")
    } else if stoat.lsp_spawn_deferred.is_some() {
        "lsp: server start waiting on the project environment".to_string()
    } else if stoat.lsp_registry.spawn_attempted_any() {
        "lsp: server still starting".to_string()
    } else {
        "lsp: no language server running".to_string()
    };

    set_lsp_status(stoat, msg);
    UpdateEffect::Redraw
}

/// Scan every buffer in [`Stoat::lsp_opened`] for an updated
/// [`crate::buffer::Buffer::version`] and arm a 50ms debounce per
/// buffer that has changed. Replacing the entry in
/// [`Stoat::lsp_pending_changes`] drops the prior pending task,
/// which cancels its spawned future before its timer fires; only
/// the most recent edit's snapshot ever reaches the server.
///
/// Capability honouring: dispatches when the server advertises
/// [`TextDocumentSyncKind::FULL`] (full document text) or
/// [`TextDocumentSyncKind::INCREMENTAL`] (per-edit ranges via
/// [`patch_to_content_changes`]). `NONE` skips silently.
pub(crate) fn notify_buffer_changes_pending(stoat: &mut Stoat) {
    for id in stoat.lsp_opened.iter().copied().collect::<Vec<_>>() {
        // Group the buffer's hosts by the sync kind and encoding each
        // negotiated, so a FULL host and an INCREMENTAL one -- or two hosts on
        // different encodings -- each get content changes shaped their own way.
        // A host on sync NONE takes no change.
        let mut groups: HostSyncGroups = Vec::new();
        for host in stoat.hosts_for_buffer(id) {
            let sync_kind = resolve_sync_kind(&host.capabilities().text_document_sync);
            if !matches!(
                sync_kind,
                TextDocumentSyncKind::FULL | TextDocumentSyncKind::INCREMENTAL
            ) {
                continue;
            }
            let key = (sync_kind, host.offset_encoding());
            match groups.iter_mut().find(|(existing, _)| *existing == key) {
                Some((_, group)) => group.push(host),
                None => groups.push((key, vec![host])),
            }
        }

        // Each group reads the same last-seen and last-delivered baseline, which
        // advance only after the plans are built.
        let plans: Vec<(DispatchPlan, Vec<Arc<dyn LspHost>>)> = groups
            .into_iter()
            .filter_map(|((sync_kind, encoding), hosts)| {
                build_dispatch_plan(stoat, id, sync_kind, encoding).map(|plan| (plan, hosts))
            })
            .collect();

        // The change is consumed for this buffer once seen, whether or not any
        // group took it, mirroring the sync-NONE path.
        if let Some(buffer) = stoat.active_workspace().buffers.get(id) {
            let v = buffer.read().expect("buffer lock").version();
            stoat.lsp_buffer_versions.insert(id, v);
        }

        if plans.is_empty() {
            continue;
        }

        // One monotonic LSP document version per buffer, shared across groups --
        // a per-server counter buys nothing since each server still sees it rise.
        let lsp_version = {
            let version = stoat.lsp_doc_versions.entry(id).or_insert(0);
            *version += 1;
            *version
        };

        // The delivered baseline is per-buffer, so every group targets the same
        // text and version.
        let target_text = plans[0].0.target_text.clone();
        let target_version = plans[0].0.target_buffer_version;

        let dispatches: Vec<(DidChangeTextDocumentParams, Vec<Arc<dyn LspHost>>)> = plans
            .into_iter()
            .map(|(plan, hosts)| {
                (
                    DidChangeTextDocumentParams {
                        text_document: VersionedTextDocumentIdentifier {
                            uri: plan.uri,
                            version: lsp_version,
                        },
                        content_changes: plan.content_changes,
                    },
                    hosts,
                )
            })
            .collect();

        let executor = stoat.executor.clone();
        let last_text = stoat.lsp_last_delivered_text.clone();
        let last_version = stoat.lsp_last_delivered_buffer_version.clone();

        let task = stoat.executor.spawn(async move {
            executor.timer(LSP_DID_CHANGE_DEBOUNCE).await;
            let mut delivered = true;
            for (params, hosts) in dispatches {
                for lsp in hosts {
                    if let Err(err) = lsp.did_change(params.clone()).await {
                        tracing::warn!(target: "stoat::lsp", ?err, "did_change notification failed");
                        delivered = false;
                    }
                }
            }
            // Only advance the delivered baseline when every server in every
            // group received the change, so a failed server's next delta still
            // replays it.
            if delivered {
                last_text
                    .lock()
                    .expect("lsp text mutex")
                    .insert(id, target_text);
                last_version
                    .lock()
                    .expect("lsp version mutex")
                    .insert(id, target_version);
            }
        });
        stoat.lsp_pending_changes.insert(id, task);
    }
}

/// A buffer's fanned-out hosts grouped by the sync kind and encoding each
/// negotiated, so one shaped [`DispatchPlan`] serves every host in a group.
type HostSyncGroups = Vec<(
    (TextDocumentSyncKind, OffsetEncoding),
    Vec<Arc<dyn LspHost>>,
)>;

struct DispatchPlan {
    uri: Uri,
    content_changes: Vec<TextDocumentContentChangeEvent>,
    target_text: Arc<String>,
    target_buffer_version: u64,
}

fn build_dispatch_plan(
    stoat: &Stoat,
    id: BufferId,
    sync_kind: TextDocumentSyncKind,
    encoding: OffsetEncoding,
) -> Option<DispatchPlan> {
    let workspace = stoat.active_workspace();
    let buffer = workspace.buffers.get(id)?;
    let buffer_b = buffer.read().expect("buffer lock");
    let current_version = buffer_b.version();
    let last_seen = stoat.lsp_buffer_versions.get(&id).copied().unwrap_or(0);
    if current_version == last_seen {
        return None;
    }
    let path = workspace.buffers.path_for(id)?.to_path_buf();
    let uri = path_to_uri(&path)?;
    let new_text = buffer_b.rope().to_string();

    let content_changes = match sync_kind {
        TextDocumentSyncKind::FULL => {
            vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: new_text.clone(),
            }]
        },
        TextDocumentSyncKind::INCREMENTAL => {
            let last_delivered_version = stoat
                .lsp_last_delivered_buffer_version
                .lock()
                .expect("lsp version mutex")
                .get(&id)
                .copied()
                .unwrap_or(0);
            let last_delivered_text = stoat
                .lsp_last_delivered_text
                .lock()
                .expect("lsp text mutex")
                .get(&id)
                .cloned()
                .unwrap_or_else(|| Arc::new(String::new()));
            let patch = buffer_b.snapshot.edits_since(last_delivered_version);
            patch_to_content_changes(&last_delivered_text, buffer_b.rope(), &patch, encoding)
        },
        _ => return None,
    };

    if content_changes.is_empty() {
        return None;
    }

    Some(DispatchPlan {
        uri,
        content_changes,
        target_text: Arc::new(new_text),
        target_buffer_version: current_version,
    })
}

/// Translate a [`Patch`] of byte-range edits between `old_text` and
/// `new_rope` into a sequence of [`TextDocumentContentChangeEvent`]s.
/// LSP requires positions in the *sequential* state at the moment
/// each change is applied -- after prior changes in the same call
/// have been applied -- not in the original or final document. The
/// walk below tracks `current_lsp` as the LSP position in the seq
/// state: a retain advances both old and seq; an insertion advances
/// seq by the inserted text's length; a deletion leaves seq alone
/// because the deleted bytes are removed from seq before the next
/// edit applies.
fn patch_to_content_changes(
    old_text: &str,
    new_rope: &Rope,
    patch: &Patch<usize>,
    encoding: OffsetEncoding,
) -> Vec<TextDocumentContentChangeEvent> {
    let mut changes = Vec::new();
    let mut old_pos: usize = 0;
    let mut current_lsp = Position::new(0, 0);

    for edit in patch {
        if edit.old.start > old_pos {
            let retain = &old_text[old_pos..edit.old.start];
            current_lsp = advance_lsp_position(current_lsp, retain, encoding);
            old_pos = edit.old.start;
        }

        let start = current_lsp;
        let old_len = edit.old.end - edit.old.start;
        let new_len = edit.new.end - edit.new.start;

        if old_len > 0 {
            let deleted = &old_text[edit.old.start..edit.old.end];
            let end = advance_lsp_position(start, deleted, encoding);
            changes.push(TextDocumentContentChangeEvent {
                range: Some(Range::new(start, end)),
                range_length: None,
                text: String::new(),
            });
            old_pos = edit.old.end;
        } else if new_len > 0 {
            let inserted = new_rope.slice(edit.new.start..edit.new.end).to_string();
            current_lsp = advance_lsp_position(current_lsp, &inserted, encoding);
            changes.push(TextDocumentContentChangeEvent {
                range: Some(Range::new(start, start)),
                range_length: None,
                text: inserted,
            });
        }
    }

    changes
}

/// Walk `text` from `start` and return the LSP position that lands
/// at the end. Counts `\n`, `\r`, and `\r\n` as line breaks per LSP
/// spec. Per-character column advance follows the negotiated
/// encoding so positions match what the server expects.
fn advance_lsp_position(start: Position, text: &str, encoding: OffsetEncoding) -> Position {
    let mut line = start.line;
    let mut character = start.character;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\n' || ch == '\r' {
            if ch == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            line += 1;
            character = 0;
        } else {
            character += match encoding {
                OffsetEncoding::Utf8 => ch.len_utf8() as u32,
                OffsetEncoding::Utf16 => ch.len_utf16() as u32,
                OffsetEncoding::Utf32 => 1,
            };
        }
    }
    Position::new(line, character)
}

fn resolve_sync_kind(cap: &Option<TextDocumentSyncCapability>) -> TextDocumentSyncKind {
    match cap {
        Some(TextDocumentSyncCapability::Kind(k)) => *k,
        Some(TextDocumentSyncCapability::Options(o)) => {
            o.change.unwrap_or(TextDocumentSyncKind::NONE)
        },
        None => TextDocumentSyncKind::NONE,
    }
}

/// Discriminator for the goto-style LSP requests that all return
/// `Option<GotoDefinitionResponse>` (a single Location or list of
/// candidates) and feed the same `Stoat::pending_lsp_jump` slot.
#[derive(Debug, Clone, Copy)]
pub(crate) enum LspJumpKind {
    Definition,
    Declaration,
    TypeDefinition,
    Implementation,
}

impl LspJumpKind {
    fn feature(self) -> LanguageServerFeature {
        match self {
            Self::Definition => LanguageServerFeature::GotoDefinition,
            Self::Declaration => LanguageServerFeature::GotoDeclaration,
            Self::TypeDefinition => LanguageServerFeature::GotoTypeDefinition,
            Self::Implementation => LanguageServerFeature::GotoImplementation,
        }
    }

    fn warn_label(self) -> &'static str {
        match self {
            Self::Definition => "goto_definition",
            Self::Declaration => "goto_declaration",
            Self::TypeDefinition => "goto_type_definition",
            Self::Implementation => "goto_implementation",
        }
    }

    fn status_label(self) -> &'static str {
        match self {
            Self::Definition => "definition",
            Self::Declaration => "declaration",
            Self::TypeDefinition => "type definition",
            Self::Implementation => "implementation",
        }
    }
}

/// Issue a `textDocument/definition` request for the symbol under the
/// focused editor's primary cursor. Thin wrapper over [`lsp_jump`].
pub(crate) fn goto_definition(stoat: &mut Stoat) -> UpdateEffect {
    lsp_jump(stoat, LspJumpKind::Definition)
}

/// Issue a `textDocument/declaration` request for the symbol under the
/// focused editor's primary cursor. Thin wrapper over [`lsp_jump`].
pub(crate) fn goto_declaration(stoat: &mut Stoat) -> UpdateEffect {
    lsp_jump(stoat, LspJumpKind::Declaration)
}

/// Issue a `textDocument/typeDefinition` request for the symbol under
/// the focused editor's primary cursor. Thin wrapper over [`lsp_jump`].
pub(crate) fn goto_type_definition(stoat: &mut Stoat) -> UpdateEffect {
    lsp_jump(stoat, LspJumpKind::TypeDefinition)
}

/// Issue a `textDocument/implementation` request for the symbol under
/// the focused editor's primary cursor. Thin wrapper over [`lsp_jump`].
pub(crate) fn goto_implementation(stoat: &mut Stoat) -> UpdateEffect {
    lsp_jump(stoat, LspJumpKind::Implementation)
}

/// Issue a `textDocument/references` request for the symbol under the
/// focused editor's primary cursor and feed the results to the
/// multi-location picker via [`Stoat::pending_lsp_jump`]. A single
/// reference jumps directly. Several open the picker. The declaration is
/// included, matching the common editor default.
///
/// Falls back to code-graph reference navigation
/// ([`crate::code_index::nav::goto_references`]) when the server does not
/// advertise `references`, so references keep working with no language
/// server. No-op when the focused pane is not an editor, its buffer has
/// no path, or a review cursor does not map to a file line.
pub(crate) fn goto_references(stoat: &mut Stoat) -> UpdateEffect {
    let Some(site) = lsp_request_site(stoat) else {
        return UpdateEffect::None;
    };
    let hosts = stoat.feature_hosts(site.buffer_id, LanguageServerFeature::GotoReference);
    if hosts.is_empty() {
        return crate::code_index::nav::goto_references(stoat);
    }
    let Some(source_uri) = path_to_uri(&site.path) else {
        return UpdateEffect::None;
    };

    let fs = stoat.fs_host.clone();
    let LspRequestSite {
        path: source_path,
        rope: source_rope,
        offset,
        ..
    } = site;
    let task = stoat.spawn_woken(async move {
        let requests = hosts.iter().map(|(_, host)| {
            let encoding = host.offset_encoding();
            let position = crate::lsp::util::byte_offset_to_lsp_pos(&source_rope, offset, encoding);
            let params = ReferenceParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier {
                        uri: source_uri.clone(),
                    },
                    position,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: ReferenceContext {
                    include_declaration: true,
                },
            };
            async move { (encoding, host.references(params).await) }
        });
        let responses = futures::future::join_all(requests).await;

        let mut entries = Vec::new();
        for (encoding, result) in responses {
            match result {
                Ok(Some(locations)) => entries.extend(resolve_goto_targets(
                    GotoDefinitionResponse::Array(locations),
                    &source_path,
                    &source_rope,
                    encoding,
                    &*fs,
                )),
                Ok(None) => {},
                Err(err) => tracing::warn!(
                    target: "stoat::lsp",
                    ?err,
                    "references request failed",
                ),
            }
        }
        dedup_locations(entries)
    });
    stoat.pending_lsp_jump = Some(("references", task));
    UpdateEffect::None
}

/// Resolve a focused review editor's cursor to the real working-tree file it
/// mirrors, readying that file for an LSP request.
///
/// Ensures the file's buffer is open and did-opened (no pane swap), then
/// returns its path, rope, and the cursor's byte offset in it. This is what
/// lets hover and goto work from the side-by-side diff, whose own buffer is a
/// pathless placeholder the language server knows nothing about. `None` when
/// the cursor is not on a new-side line or the source is not a working tree
/// (see [`review::review_cursor_file_position`]).
fn review_lsp_source(stoat: &mut Stoat) -> Option<(PathBuf, Rope, usize)> {
    let (path, line, col) = super::review::review_cursor_file_position(stoat)?;
    let content = super::read_string_via_host(&*stoat.fs_host, &path).ok()?;
    let lang = stoat.language_registry.for_path(&path);

    let (buffer_id, buffer) = {
        let ws = stoat.active_workspace_mut();
        let (buffer_id, buffer) = ws.buffers.open(&path, &content);
        if let Some(lang) = lang
            && ws.buffers.language_for(buffer_id).is_none()
        {
            ws.buffers.set_language(buffer_id, lang);
        }
        (buffer_id, buffer)
    };
    notify_buffer_opened(stoat, buffer_id, &path, &content);

    let rope = buffer.read().expect("buffer lock").rope().clone();
    let offset = rope.point_to_offset(Point::new(line, col));
    Some((path, rope, offset))
}

/// The focused editor's cursor resolved to an LSP request site: the
/// source file, its rope, and the cursor's byte offset into it.
struct LspRequestSite {
    buffer_id: BufferId,
    path: PathBuf,
    rope: Rope,
    offset: usize,
}

/// Resolve the focused editor's cursor to an [`LspRequestSite`] for a
/// position-based request.
///
/// A working-tree review cursor resolves to the real file it mirrors via
/// [`review_lsp_source`], so requests target disk content rather than the
/// diff placeholder. Returns `None` when the focused pane is not an editor,
/// its buffer has no path, or a review cursor does not map to a file line.
fn lsp_request_site(stoat: &mut Stoat) -> Option<LspRequestSite> {
    let (focused_offset, buffer_id, focused_rope, is_review) = {
        let editor = crate::action_handlers::focused_editor_mut(stoat)?;
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let tail_off = buf_snap.resolve_anchor(&sel.tail());
        let head_off = buf_snap.resolve_anchor(&sel.head());
        let offset = stoat_text::cursor_offset(buf_snap.rope(), tail_off, head_off);
        (
            offset,
            editor.buffer_id,
            buf_snap.rope().clone(),
            editor.review_view.is_some(),
        )
    };

    if is_review {
        let (path, rope, offset) = review_lsp_source(stoat)?;
        Some(LspRequestSite {
            buffer_id,
            path,
            rope,
            offset,
        })
    } else {
        let path = stoat
            .active_workspace()
            .buffers
            .path_for(buffer_id)
            .map(Path::to_path_buf)?;
        Some(LspRequestSite {
            buffer_id,
            path,
            rope: focused_rope,
            offset: focused_offset,
        })
    }
}

/// Issue an LSP jump-style request (definition / type definition /
/// implementation / declaration) for the symbol under the focused
/// editor's primary cursor. The async response is stored on
/// [`Stoat::pending_lsp_jump`] and applied by [`pump_lsp_jumps`] on
/// the next render tick.
///
/// From a working-tree review the cursor resolves to the real file via
/// [`review_lsp_source`], so the request targets disk content, not the diff
/// placeholder.
///
/// No-op when the focused pane is not an editor or the buffer has no
/// path. When the server does not advertise the matching
/// [`LanguageServerFeature`], reports the language-server state to the
/// status bar via [`report_lsp_unavailable`] instead of doing nothing.
///
/// Replacing the prior pending task drops it and cancels its spawned
/// future, so only one in-flight jump is tracked at a time.
fn lsp_jump(stoat: &mut Stoat, kind: LspJumpKind) -> UpdateEffect {
    let Some(site) = lsp_request_site(stoat) else {
        return UpdateEffect::None;
    };
    let hosts = stoat.feature_hosts(site.buffer_id, kind.feature());
    if hosts.is_empty() {
        return report_lsp_unavailable(stoat, &format!("goto {}", kind.status_label()));
    }
    let Some(source_uri) = path_to_uri(&site.path) else {
        return UpdateEffect::None;
    };

    let fs = stoat.fs_host.clone();
    let LspRequestSite {
        path: source_path,
        rope: source_rope,
        offset,
        ..
    } = site;
    let task = stoat.spawn_woken(async move {
        let requests = hosts.iter().map(|(_, host)| {
            let encoding = host.offset_encoding();
            let position = crate::lsp::util::byte_offset_to_lsp_pos(&source_rope, offset, encoding);
            let params = GotoDefinitionParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier {
                        uri: source_uri.clone(),
                    },
                    position,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            async move {
                let result = match kind {
                    LspJumpKind::Definition => host.goto_definition(params).await,
                    LspJumpKind::Declaration => host.goto_declaration(params).await,
                    LspJumpKind::TypeDefinition => host.goto_type_definition(params).await,
                    LspJumpKind::Implementation => host.goto_implementation(params).await,
                };
                (encoding, result)
            }
        });
        let responses = futures::future::join_all(requests).await;

        let mut entries = Vec::new();
        for (encoding, result) in responses {
            match result {
                Ok(Some(response)) => entries.extend(resolve_goto_targets(
                    response,
                    &source_path,
                    &source_rope,
                    encoding,
                    &*fs,
                )),
                Ok(None) => {},
                Err(err) => tracing::warn!(
                    target: "stoat::lsp",
                    request = kind.warn_label(),
                    ?err,
                    "lsp jump request failed",
                ),
            }
        }
        dedup_locations(entries)
    });
    stoat.pending_lsp_jump = Some((kind.status_label(), task));
    UpdateEffect::None
}

/// Resolve every candidate in a `GotoDefinitionResponse` into a
/// [`LocationEntry`]. A single-target response yields one entry (the
/// caller jumps directly); a multi-target response yields several (the
/// caller opens a picker). Candidates whose URI is not a `file:` path,
/// or whose target file cannot be read, are dropped rather than
/// aborting the whole batch, so one bad location does not sink the rest.
///
/// Same-file targets reuse the supplied source rope. Cross-file targets
/// read the destination through the supplied [`crate::host::FsHost`] so
/// a closed buffer still resolves without round-tripping through
/// `Stoat`. Each entry carries the byte offset after applying the
/// host's negotiated [`OffsetEncoding`], the 1-based line and column,
/// and the trimmed text of the target line for display.
fn resolve_goto_targets(
    response: GotoDefinitionResponse,
    source_path: &Path,
    source_rope: &Rope,
    encoding: OffsetEncoding,
    fs: &dyn crate::host::FsHost,
) -> Vec<LocationEntry> {
    let candidates: Vec<(Uri, Position)> = match response {
        GotoDefinitionResponse::Scalar(loc) => vec![(loc.uri, loc.range.start)],
        GotoDefinitionResponse::Array(locs) => locs
            .into_iter()
            .map(|loc| (loc.uri, loc.range.start))
            .collect(),
        GotoDefinitionResponse::Link(links) => links
            .into_iter()
            .map(|link| (link.target_uri, link.target_range.start))
            .collect(),
    };

    candidates
        .into_iter()
        .filter_map(|(uri, position)| {
            resolve_one_target(uri, position, source_path, source_rope, encoding, fs)
        })
        .collect()
}

fn resolve_one_target(
    uri: Uri,
    position: Position,
    source_path: &Path,
    source_rope: &Rope,
    encoding: OffsetEncoding,
    fs: &dyn crate::host::FsHost,
) -> Option<LocationEntry> {
    let target_path = crate::app::lsp_uri_to_path(&uri)?;

    let (offset, text) = if target_path == source_path {
        (
            crate::lsp::util::lsp_pos_to_byte_offset(source_rope, position, encoding),
            line_text(source_rope, position.line),
        )
    } else {
        let file_text = match super::read_string_via_host(fs, &target_path) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(
                    target: "stoat::lsp",
                    path = %target_path.display(),
                    ?err,
                    "goto target file unreadable",
                );
                return None;
            },
        };
        let target_rope = Rope::from(file_text.as_str());
        let offset = crate::lsp::util::lsp_pos_to_byte_offset(&target_rope, position, encoding);
        (offset, line_text(&target_rope, position.line))
    };

    Some(LocationEntry {
        path: target_path,
        offset,
        line: position.line + 1,
        column: position.character + 1,
        text,
    })
}

/// Drop duplicate goto targets, keeping the first occurrence of each
/// `(path, offset)` in order.
///
/// Fanning a goto request out to every capable server routinely surfaces the
/// same definition twice (two servers indexing one crate answer identically).
/// Deduplicating keeps a redundant answer from opening a multi-location picker
/// over what is really one target. Order is preserved, so the highest-priority
/// server's copy of a shared target is the one kept.
fn dedup_locations(entries: Vec<LocationEntry>) -> Vec<LocationEntry> {
    let mut seen = HashSet::new();
    entries
        .into_iter()
        .filter(|entry| seen.insert((entry.path.clone(), entry.offset)))
        .collect()
}

/// The trimmed text of `line` (0-based) in `rope`, for display in the
/// location picker. Returns an empty string when the line is out of
/// range so a stale position never panics.
fn line_text(rope: &Rope, line: u32) -> String {
    let start = rope.point_to_offset(Point::new(line, 0));
    let end = rope
        .point_to_offset(Point::new(line + 1, 0))
        .min(rope.len());
    rope.slice(start..end).to_string().trim().to_string()
}

/// Hover response carried from the spawned task to [`pump_lsp_hover`].
///
/// `text` is the flattened markdown, awaiting parsing on the main-loop side
/// where the theme lives. `plain` marks PlainText content that must render
/// verbatim rather than as markdown. `anchor_offset` is the cursor byte offset
/// captured when the request fired so the popup anchors at the symbol even if
/// the cursor moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HoverResponse {
    pub(crate) text: String,
    pub(crate) plain: bool,
    pub(crate) anchor_offset: usize,
    /// The editor focused when the request fired. A response is dropped if
    /// focus has since moved, so a popup never anchors against a pane that did
    /// not request it.
    pub(crate) editor_id: EditorId,
}

/// The outcome of a spawned hover request, carried to [`pump_lsp_hover`].
///
/// Distinguishing an empty answer from a failed request lets the status bar
/// report honest state. A server still indexing says so and a broken request
/// says it failed, rather than collapsing both to a flat "no hover info".
pub(crate) enum HoverOutcome {
    /// The server returned hover content to render.
    Content(HoverResponse),
    /// The server answered with no hover for the cursor position.
    Empty,
    /// The request errored.
    Failed,
}

/// A live text selection over the hover popup body.
///
/// Endpoints are `(content line, char column)` into [`HoverPopup::lines`], so
/// tuple ordering sorts them into a range. `dragging` is true between the mouse
/// down and its release, so a drag past the popup rect keeps extending the head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HoverSelection {
    pub(crate) anchor: (usize, usize),
    pub(crate) head: (usize, usize),
    pub(crate) dragging: bool,
}

/// Hover popup state ready to paint. Mirrors [`HoverResponse`] but
/// lives on [`Stoat::pending_hover`] (separate from the in-flight
/// task slot) so the renderer can borrow it without polling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HoverPopup {
    /// Rendered content, one line per entry, each a list of styled spans.
    pub(crate) lines: Vec<Vec<(String, Style)>>,
    pub(crate) anchor_offset: usize,
    /// The editor that requested this hover. Set from the response's
    /// [`HoverResponse::editor_id`] once focus is confirmed unchanged.
    pub(crate) editor_id: EditorId,
    /// Half-page scroll offset applied by [`crate::render::hover::render_hover`],
    /// advanced by Ctrl-d/Ctrl-u while the popup is open. Clamped to the content
    /// height at render, so an over-scroll past the bottom does not accumulate.
    pub(crate) scroll_half_pages: usize,
    /// Screen rect the popup last painted over, stamped by
    /// [`crate::render::hover::render_hover`]. The mouse handler hit-tests it so
    /// a wheel over the popup scrolls it rather than the pane beneath. Empty
    /// ([`Rect::default`]) until the first render.
    pub(crate) area: Rect,
    /// Interior rect (inside the border) the body last painted over, stamped by
    /// [`crate::render::hover::render_hover`]. The selection hit-test maps a
    /// pointer through this. Empty until the first render.
    pub(crate) inner: Rect,
    /// The live mouse selection over the body, if any.
    pub(crate) selection: Option<HoverSelection>,
    /// Content-version stamp for the hover pool, from the shared generation
    /// counter, set once at construction. A popup's body is immutable once
    /// built, so a new hover gets a new stamp and the per-frame version is O(1)
    /// instead of a walk of every line's text.
    pub(crate) generation: u64,
}

/// Issue a `textDocument/hover` request for the symbol under the
/// focused editor's primary cursor. The async response is stored on
/// [`Stoat::pending_hover_request`] and applied by [`pump_lsp_hover`]
/// on the next render tick.
///
/// No-op when the focused pane is not an editor or the buffer has no
/// path. When the server does not advertise
/// [`LanguageServerFeature::Hover`], reports the language-server state
/// to the status bar instead. Replacing the prior pending task
/// drops it, cancelling its spawned future -- only one in-flight hover
/// is tracked at a time.
pub(crate) fn hover(stoat: &mut Stoat) -> UpdateEffect {
    let Some((editor_id, _)) = stoat.focused_editor_ids() else {
        return UpdateEffect::None;
    };

    let (anchor_offset, buffer_id, focused_rope, is_review) = {
        let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let tail_off = buf_snap.resolve_anchor(&sel.tail());
        let head_off = buf_snap.resolve_anchor(&sel.head());
        let offset = stoat_text::cursor_offset(buf_snap.rope(), tail_off, head_off);
        (
            offset,
            editor.buffer_id,
            buf_snap.rope().clone(),
            editor.review_view.is_some(),
        )
    };

    let hosts = stoat.feature_hosts(buffer_id, LanguageServerFeature::Hover);
    if hosts.is_empty() {
        return report_lsp_unavailable(stoat, "hover");
    }

    // A review cursor requests against the real working-tree file, but the
    // popup still anchors at the placeholder cursor cell, so `anchor_offset`
    // stays the review-editor offset while the request uses the real file.
    let (source_path, source_rope, cursor_offset) = if is_review {
        match review_lsp_source(stoat) {
            Some(resolved) => resolved,
            None => return UpdateEffect::None,
        }
    } else {
        let Some(path) = stoat
            .active_workspace()
            .buffers
            .path_for(buffer_id)
            .map(Path::to_path_buf)
        else {
            return UpdateEffect::None;
        };
        (path, focused_rope, anchor_offset)
    };
    let Some(source_uri) = path_to_uri(&source_path) else {
        return UpdateEffect::None;
    };

    let task = stoat.spawn_woken(async move {
        let requests = hosts.iter().map(|(name, host)| {
            let name = name.clone();
            let encoding = host.offset_encoding();
            let position =
                crate::lsp::util::byte_offset_to_lsp_pos(&source_rope, cursor_offset, encoding);
            let params = HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier {
                        uri: source_uri.clone(),
                    },
                    position,
                },
                work_done_progress_params: Default::default(),
            };
            async move { (name, host.hover(params).await) }
        });
        let responses = futures::future::join_all(requests).await;

        let mut sections = Vec::new();
        let mut any_empty = false;
        for (name, result) in responses {
            match result {
                Ok(Some(hover)) => {
                    let (text, plain) = flatten_hover_contents(hover.contents);
                    sections.push((name, text, plain));
                },
                Ok(None) => any_empty = true,
                Err(err) => tracing::warn!(target: "stoat::lsp", ?err, "hover request failed"),
            }
        }

        if sections.is_empty() {
            if any_empty {
                HoverOutcome::Empty
            } else {
                HoverOutcome::Failed
            }
        } else {
            let (text, plain) = merge_hovers(sections);
            HoverOutcome::Content(HoverResponse {
                text,
                plain,
                anchor_offset,
                editor_id,
            })
        }
    });
    stoat.pending_hover_request = Some(task);
    UpdateEffect::None
}

/// Answer a runtime [`AgentQuery`] from live session state, firing `reply` with
/// the JSON result.
///
/// `lsp-status` and `diagnostics` reply synchronously. `hover` requires the path
/// to be open in the `uid` session (otherwise `{"error":"not open"}`) and runs
/// the request on a detached task so the event loop never blocks on the server.
pub(crate) fn answer_agent_query(
    stoat: &mut Stoat,
    uid: WorkspaceUid,
    request: AgentQuery,
    reply: oneshot::Sender<Value>,
) {
    match request {
        AgentQuery::LspStatus => {
            let servers: Vec<Value> = stoat
                .lsp_registry
                .named_hosts()
                .into_iter()
                .filter(|(_, host)| !host.is_noop())
                .map(|(name, host)| {
                    let capabilities =
                        serde_json::to_value(&*host.capabilities()).unwrap_or(Value::Null);
                    json!({ "name": name, "capabilities": capabilities })
                })
                .collect();
            let _ = reply.send(json!({
                "active": !servers.is_empty(),
                "spawn_attempted": stoat.lsp_registry.spawn_attempted_any(),
                "servers": servers,
            }));
        },
        AgentQuery::Diagnostics { path } => {
            let value = match path {
                Some(path) => {
                    serde_json::to_value(stoat.diagnostics.get(&path)).unwrap_or(Value::Null)
                },
                None => Value::Array(
                    stoat
                        .diagnostics
                        .iter()
                        .map(|(path, diagnostics)| json!({ "path": path, "diagnostics": diagnostics }))
                        .collect(),
                ),
            };
            let _ = reply.send(value);
        },
        AgentQuery::Hover { path, line, col } => {
            let buffer_id = stoat
                .workspaces
                .iter()
                .find(|(_, ws)| ws.uid == uid)
                .and_then(|(_, ws)| ws.buffers.id_for_path(&path));
            let Some(buffer_id) = buffer_id.filter(|id| stoat.lsp_opened.contains(id)) else {
                let _ = reply.send(json!({ "error": "not open" }));
                return;
            };
            let Some(uri) = path_to_uri(&path) else {
                let _ = reply.send(json!({ "error": "invalid path" }));
                return;
            };

            let params = HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier { uri },
                    position: Position {
                        line,
                        character: col,
                    },
                },
                work_done_progress_params: Default::default(),
            };
            let lsp = stoat.lsp_for_feature(buffer_id, LanguageServerFeature::Hover);
            stoat
                .executor
                .spawn(async move {
                    let value = match lsp.hover(params).await {
                        Ok(Some(hover)) => serde_json::to_value(&hover).unwrap_or(Value::Null),
                        Ok(None) => Value::Null,
                        Err(err) => json!({ "error": err.to_string() }),
                    };
                    let _ = reply.send(value);
                })
                .detach();
        },
    }
}

/// Flatten an LSP [`HoverContents`] payload into a markdown string and a flag
/// marking whether it is PlainText.
///
/// A [`MarkedString::LanguageString`] becomes a fenced code block so the
/// language is highlighted, except a `markdown` language passes through as-is.
/// PlainText markup is returned verbatim with the flag set, so the caller
/// renders it without interpreting markdown syntax.
fn flatten_hover_contents(contents: HoverContents) -> (String, bool) {
    fn marked_to_markdown(m: MarkedString) -> String {
        match m {
            MarkedString::String(s) => s,
            MarkedString::LanguageString(ls) if ls.language == "markdown" => ls.value,
            MarkedString::LanguageString(ls) => {
                format!("```{}\n{}\n```", ls.language, ls.value)
            },
        }
    }

    match contents {
        HoverContents::Scalar(m) => (marked_to_markdown(m), false),
        HoverContents::Array(items) => (
            items
                .into_iter()
                .map(marked_to_markdown)
                .collect::<Vec<_>>()
                .join("\n"),
            false,
        ),
        HoverContents::Markup(markup) => (markup.value, markup.kind == MarkupKind::PlainText),
    }
}

/// Combine every responding server's hover markdown into one popup body.
///
/// A lone responder renders exactly as a single-server hover always has, its
/// own text with no header. Two or more responders each get a `**{server}**`
/// section header and are joined by a `---` rule so a reader can tell which
/// server said what.
///
/// The merged body is plain only when every section is plain text. One markdown
/// section makes the whole popup markdown.
///
/// `sections` is `(server_name, text, plain)` in server routing order, which the
/// output preserves.
fn merge_hovers(sections: Vec<(String, String, bool)>) -> (String, bool) {
    if sections.len() == 1 {
        let (_, text, plain) = sections.into_iter().next().expect("one section");
        return (text, plain);
    }

    let plain = sections.iter().all(|(_, _, plain)| *plain);
    let body = sections
        .iter()
        .map(|(server, text, _)| format!("**{server}**\n\n{text}"))
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    (body, plain)
}

/// Poll any in-flight hover request ([`Stoat::pending_hover_request`])
/// and apply the [`HoverOutcome`].
///
/// `Content` writes the popup to [`Stoat::pending_hover`]. `Empty` and `Failed`
/// clear it and set an honest status message. `Pending` puts the task back.
/// Returns true when state changed so the caller can request a redraw.
pub(crate) fn pump_lsp_hover(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_hover_request.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(HoverOutcome::Content(response)) => {
            // Drop a response whose editor lost focus while the request was in
            // flight, so the popup never anchors against a pane that did not
            // request it.
            if stoat.focused_editor_ids().map(|(id, _)| id) != Some(response.editor_id) {
                stoat.pending_hover = None;
                return true;
            }
            let lines = if response.plain {
                response
                    .text
                    .lines()
                    .map(|line| vec![(line.to_string(), Style::default())])
                    .collect()
            } else {
                crate::markdown::render_markdown(
                    &response.text,
                    &stoat.theme,
                    &stoat.language_registry,
                )
            };
            stoat.pending_hover = Some(HoverPopup {
                lines,
                anchor_offset: response.anchor_offset,
                editor_id: response.editor_id,
                scroll_half_pages: 0,
                area: Rect::default(),
                inner: Rect::default(),
                selection: None,
                generation: crate::picker::next_generation(),
            });
            true
        },
        Poll::Ready(HoverOutcome::Empty) => {
            // A busy server is still worth naming, so an empty result during
            // work-done progress reports which operation is running. The
            // progress segment already shows the percentage, so none is added.
            let status = match stoat.lsp_progress.current() {
                Some(entry) => {
                    let body = if !entry.title.is_empty() {
                        entry.title.as_str()
                    } else {
                        entry.message.as_deref().unwrap_or("working")
                    };
                    format!("lsp: no hover info yet ({} {})", entry.server, body)
                },
                None => "lsp: no hover info".to_string(),
            };
            set_lsp_status(stoat, status);
            stoat.pending_hover = None;
            true
        },
        Poll::Ready(HoverOutcome::Failed) => {
            set_lsp_status(stoat, "lsp: hover request failed".to_string());
            stoat.pending_hover = None;
            true
        },
        Poll::Pending => {
            stoat.pending_hover_request = Some(task);
            false
        },
    }
}

/// Signature-help popup state ready to paint.
///
/// `label` is the active signature's text. `active_param` is the char range
/// within `label` the renderer emphasizes, present when the server reports an
/// active parameter. `doc` is the signature's first documentation line, if any.
/// `anchor_offset` is the cursor byte offset when the request fired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SignatureHelpPopup {
    pub(crate) label: String,
    pub(crate) active_param: Option<std::ops::Range<usize>>,
    pub(crate) doc: Option<String>,
    pub(crate) anchor_offset: usize,
}

/// Re-fire the signature-help request when a trigger character was just typed,
/// or a retrigger character while the popup is showing. Clears the popup when
/// the editor leaves insert mode or the completion popup takes over, so the two
/// never overlap.
///
/// Version-gated on the focused buffer so a cursor-only tick does not re-request.
pub(crate) fn signature_help_trigger(stoat: &mut Stoat) {
    let in_insert_editor = stoat.focused_mode() == "insert" && {
        let ws = stoat.active_workspace();
        matches!(
            ws.panes.pane(ws.panes.focus()).view,
            crate::pane::View::Editor(_)
        )
    };
    if !in_insert_editor || stoat.pending_completion.is_some() {
        stoat.pending_signature_help = None;
        stoat.pending_signature_help_request = None;
        stoat.last_signature_help_key = None;
        return;
    }

    let Some((buffer_id, version, rope, cursor_offset)) = focused_edit_snapshot(stoat) else {
        return;
    };
    if stoat.last_signature_help_key == Some((buffer_id, version)) {
        return;
    }
    stoat.last_signature_help_key = Some((buffer_id, version));

    let context = crate::completion::request::compute_context(&rope, cursor_offset);
    let Some(ch) = context.text_before_cursor.chars().last() else {
        return;
    };
    let ch = ch.to_string();

    let Some((_, host)) = stoat
        .feature_hosts(buffer_id, LanguageServerFeature::SignatureHelp)
        .into_iter()
        .next()
    else {
        return;
    };
    let caps = host.capabilities();
    let Some(opts) = caps.signature_help_provider.as_ref() else {
        return;
    };
    let is_trigger = opts
        .trigger_characters
        .as_ref()
        .is_some_and(|chars| chars.contains(&ch));
    let is_retrigger = stoat.pending_signature_help.is_some()
        && opts
            .retrigger_characters
            .as_ref()
            .is_some_and(|chars| chars.contains(&ch));

    if is_trigger || is_retrigger {
        request_signature_help(stoat);
    }
}

/// The focused editor's `(buffer_id, version, rope, cursor_offset)`, or `None`
/// when the focused pane is not an editor.
fn focused_edit_snapshot(stoat: &mut Stoat) -> Option<(BufferId, u64, Rope, usize)> {
    let editor = crate::action_handlers::focused_editor_mut(stoat)?;
    let snapshot = editor.display_map.snapshot();
    let buf_snap = snapshot.buffer_snapshot();
    let sel = editor.selections.newest_anchor();
    let tail_off = buf_snap.resolve_anchor(&sel.tail());
    let head_off = buf_snap.resolve_anchor(&sel.head());
    let offset = stoat_text::cursor_offset(buf_snap.rope(), tail_off, head_off);
    Some((
        editor.buffer_id,
        buf_snap.version(),
        buf_snap.rope().clone(),
        offset,
    ))
}

/// Issue a `textDocument/signatureHelp` request for the focused editor's primary
/// cursor. The async response is stored on
/// [`Stoat::pending_signature_help_request`] and applied by
/// [`pump_lsp_signature_help`]. No-op when the pane is not an editor, the buffer
/// has no path, or the server does not advertise the capability.
pub(crate) fn request_signature_help(stoat: &mut Stoat) {
    let (anchor_offset, buffer_id, focused_rope, is_review) = {
        let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
            return;
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let tail_off = buf_snap.resolve_anchor(&sel.tail());
        let head_off = buf_snap.resolve_anchor(&sel.head());
        let offset = stoat_text::cursor_offset(buf_snap.rope(), tail_off, head_off);
        (
            offset,
            editor.buffer_id,
            buf_snap.rope().clone(),
            editor.review_view.is_some(),
        )
    };

    let Some((_, host)) = stoat
        .feature_hosts(buffer_id, LanguageServerFeature::SignatureHelp)
        .into_iter()
        .next()
    else {
        return;
    };
    let encoding = host.offset_encoding();

    let (source_path, source_rope, cursor_offset) = if is_review {
        match review_lsp_source(stoat) {
            Some(resolved) => resolved,
            None => return,
        }
    } else {
        let Some(path) = stoat
            .active_workspace()
            .buffers
            .path_for(buffer_id)
            .map(Path::to_path_buf)
        else {
            return;
        };
        (path, focused_rope, anchor_offset)
    };
    let Some(source_uri) = path_to_uri(&source_path) else {
        return;
    };

    let position = crate::lsp::util::byte_offset_to_lsp_pos(&source_rope, cursor_offset, encoding);
    let params = SignatureHelpParams {
        context: None,
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: source_uri },
            position,
        },
        work_done_progress_params: Default::default(),
    };

    let task = stoat.spawn_woken(async move {
        match host.signature_help(params).await {
            Ok(Some(help)) => signature_help_to_popup(help, anchor_offset),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "signature_help request failed");
                None
            },
        }
    });
    stoat.pending_signature_help_request = Some(task);
}

/// Reduce an LSP [`SignatureHelp`] to the active signature's paintable state:
/// its label, the char range of the active parameter within that label, and the
/// first documentation line. Returns `None` when there is no active signature.
fn signature_help_to_popup(
    help: SignatureHelp,
    anchor_offset: usize,
) -> Option<SignatureHelpPopup> {
    let active_sig = help.active_signature.unwrap_or(0) as usize;
    let SignatureInformation {
        label,
        documentation,
        parameters,
        active_parameter,
    } = help.signatures.into_iter().nth(active_sig)?;

    let active_param = active_parameter
        .or(help.active_parameter)
        .and_then(|idx| parameters.as_ref()?.get(idx as usize).cloned())
        .and_then(|param| param_label_range(&param.label, &label));

    let doc = documentation.and_then(documentation_first_line);

    Some(SignatureHelpPopup {
        label,
        active_param,
        doc,
        anchor_offset,
    })
}

/// Resolve a parameter's label into a char range within the signature label.
/// Offset labels are taken as-is. A substring label is located in `sig_label`
/// and its byte position converted to a char range for the renderer.
fn param_label_range(label: &ParameterLabel, sig_label: &str) -> Option<std::ops::Range<usize>> {
    match label {
        ParameterLabel::LabelOffsets([start, end]) => Some(*start as usize..*end as usize),
        ParameterLabel::Simple(text) => {
            let byte_start = sig_label.find(text.as_str())?;
            let char_start = sig_label[..byte_start].chars().count();
            Some(char_start..char_start + text.chars().count())
        },
    }
}

/// First non-empty documentation line, plain text (markdown passes through).
fn documentation_first_line(doc: Documentation) -> Option<String> {
    let text = match doc {
        Documentation::String(s) => s,
        Documentation::MarkupContent(markup) => markup.value,
    };
    text.lines()
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// Poll any in-flight signature-help request
/// ([`Stoat::pending_signature_help_request`]) and apply the result to
/// [`Stoat::pending_signature_help`]. Returns true when state changed.
pub(crate) fn pump_lsp_signature_help(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_signature_help_request.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(popup) => {
            stoat.pending_signature_help = popup;
            true
        },
        Poll::Pending => {
            stoat.pending_signature_help_request = Some(task);
            false
        },
    }
}

/// Debounce before requesting inlay hints, so a burst of edits or scrolls
/// collapses into a single viewport request.
const INLAY_HINT_DEBOUNCE: Duration = Duration::from_millis(100);

/// One resolved inlay hint ready to splice into the display map. It bundles a
/// byte offset in the request-time buffer with the rendered text and the kind.
pub(crate) type InlayHintItem = (usize, String, InlayKind);

/// A completed inlay-hint request's payload. It carries the buffer the request
/// targeted and the hints resolved for its viewport.
pub(crate) type InlayHintResponse = (BufferId, Vec<InlayHintItem>);

/// Everything a viewport inlay-hint request carries. It names the target buffer
/// and version, the visible display-row window, the rope for offset conversion,
/// and the built request params.
struct InlayHintRequest {
    buffer_id: BufferId,
    version: u64,
    scroll_row: u32,
    end_row: u32,
    rope: Rope,
    params: InlayHintParams,
}

/// Request inlay hints for the focused editor's viewport when enabled, the
/// server supports them, and the (buffer, version, visible rows) key changed
/// since the last request. Buffer edits and scrolls change the key and
/// re-request. The response is applied by [`pump_lsp_inlay_hints`].
pub(crate) fn inlay_hints_trigger(stoat: &mut Stoat) {
    if !stoat.inlay_hints_enabled {
        return;
    }
    request_inlay_hints(stoat, INLAY_HINT_DEBOUNCE);
}

/// Issue a viewport inlay-hint request for the focused editor, waiting
/// `debounce` before the server call (pass [`Duration::ZERO`] to skip it).
///
/// Returns whether a server capable of inlay hints was found. A capable server
/// with no buildable request yet (no viewport, review view) or an unchanged
/// (buffer, version, visible rows) key still returns `true` without spawning:
/// the caller treats inlay hints as available and the per-frame trigger will
/// request once viable. The response is applied by [`pump_lsp_inlay_hints`].
fn request_inlay_hints(stoat: &mut Stoat, debounce: Duration) -> bool {
    let Some((_, buffer_id)) = stoat.focused_editor_ids() else {
        return false;
    };
    let Some((_, host)) = stoat
        .feature_hosts(buffer_id, LanguageServerFeature::InlayHints)
        .into_iter()
        .next()
    else {
        return false;
    };
    let encoding = host.offset_encoding();
    let Some(request) = build_inlay_hint_request(stoat, encoding) else {
        return true;
    };

    let key = (
        request.buffer_id,
        request.version,
        request.scroll_row,
        request.end_row,
    );
    if stoat.last_inlay_hint_key == Some(key) {
        return true;
    }
    stoat.last_inlay_hint_key = Some(key);

    let InlayHintRequest {
        buffer_id,
        rope,
        params,
        ..
    } = request;
    let executor = stoat.executor.clone();
    let task = stoat.spawn_woken(async move {
        if !debounce.is_zero() {
            executor.timer(debounce).await;
        }
        match host.range_inlay_hint(params).await {
            Ok(Some(hints)) => Some((buffer_id, convert_inlay_hints(hints, &rope, encoding))),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "inlay_hint request failed");
                None
            },
        }
    });
    stoat.pending_inlay_hint_request = Some(task);
    true
}

/// Enable inlay hints from the ToggleInlayHints action, requesting the focused
/// viewport immediately and acknowledging in the status bar.
///
/// Skips the scroll debounce so hints appear on the keystroke rather than after
/// the settle delay. Reports "inlay hints on" when a capable server was found,
/// even if the request cannot be built yet, since the per-frame trigger issues
/// it once viable. Reports the [`report_lsp_unavailable`] reason otherwise.
pub(crate) fn enable_inlay_hints_now(stoat: &mut Stoat) {
    if request_inlay_hints(stoat, Duration::ZERO) {
        set_lsp_status(stoat, "inlay hints on".to_string());
    } else {
        report_lsp_unavailable(stoat, "inlay hints");
    }
}

fn build_inlay_hint_request(
    stoat: &mut Stoat,
    encoding: OffsetEncoding,
) -> Option<InlayHintRequest> {
    let (buffer_id, version, scroll_row, end_row, rope, start_offset, end_offset) = {
        let editor = crate::action_handlers::focused_editor_mut(stoat)?;
        if editor.review_view.is_some() {
            return None;
        }
        let viewport = editor.viewport_rows?;
        let scroll_row = editor.scroll_row;
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let rope = buf_snap.rope().clone();
        let end_row = (scroll_row + viewport).min(snapshot.line_count());
        (
            editor.buffer_id,
            buf_snap.version(),
            scroll_row,
            end_row,
            rope.clone(),
            display_row_offset(&snapshot, &rope, scroll_row),
            display_row_offset(&snapshot, &rope, end_row),
        )
    };

    let path = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)?;
    let uri = path_to_uri(&path)?;
    let range = Range::new(
        crate::lsp::util::byte_offset_to_lsp_pos(&rope, start_offset, encoding),
        crate::lsp::util::byte_offset_to_lsp_pos(&rope, end_offset, encoding),
    );
    let params = InlayHintParams {
        work_done_progress_params: Default::default(),
        text_document: TextDocumentIdentifier { uri },
        range,
    };

    Some(InlayHintRequest {
        buffer_id,
        version,
        scroll_row,
        end_row,
        rope,
        params,
    })
}

/// Byte offset of the start of display `row`, clamped to the rope length.
fn display_row_offset(snapshot: &DisplaySnapshot, rope: &Rope, row: u32) -> usize {
    let rope_len = rope.len();
    snapshot
        .display_to_buffer(DisplayPoint::new(row, 0))
        .map(|point| rope.point_to_offset(point))
        .unwrap_or(rope_len)
        .min(rope_len)
}

/// Convert LSP inlay hints into [`InlayHintItem`]s using the request-time rope.
/// Both LSP hint kinds render as [`InlayKind::Hint`].
fn convert_inlay_hints(
    hints: Vec<InlayHint>,
    rope: &Rope,
    encoding: OffsetEncoding,
) -> Vec<InlayHintItem> {
    let positions: Vec<Position> = hints.iter().map(|hint| hint.position).collect();
    let offsets = crate::lsp::util::lsp_positions_to_byte_offsets_batch(rope, &positions, encoding);
    hints
        .into_iter()
        .zip(offsets)
        .map(|(hint, offset)| (offset, inlay_hint_text(&hint), InlayKind::Hint))
        .collect()
}

/// The rendered text of a hint. The label is joined when the server sends parts,
/// then wrapped in any requested left or right padding spaces.
fn inlay_hint_text(hint: &InlayHint) -> String {
    let core: String = match &hint.label {
        InlayHintLabel::String(s) => s.clone(),
        InlayHintLabel::LabelParts(parts) => parts.iter().map(|part| part.value.as_str()).collect(),
    };
    let mut text = String::new();
    if hint.padding_left == Some(true) {
        text.push(' ');
    }
    text.push_str(&core);
    if hint.padding_right == Some(true) {
        text.push(' ');
    }
    text
}

/// Poll any in-flight inlay-hint request and splice the results into the focused
/// editor's display map, replacing the buffer's previous hint inlays. Returns
/// true when state changed.
pub(crate) fn pump_lsp_inlay_hints(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_inlay_hint_request.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(Some((buffer_id, items))) => {
            apply_inlay_hints(stoat, buffer_id, items);
            true
        },
        Poll::Ready(None) => true,
        Poll::Pending => {
            stoat.pending_inlay_hint_request = Some(task);
            false
        },
    }
}

fn apply_inlay_hints(stoat: &mut Stoat, buffer_id: BufferId, items: Vec<InlayHintItem>) {
    let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
        return;
    };
    if editor.buffer_id != buffer_id {
        return;
    }

    let inserts: Vec<(Anchor, String, InlayKind)> = {
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        items
            .into_iter()
            .map(|(offset, text, kind)| (buf_snap.anchor_at(offset, Bias::Left), text, kind))
            .collect()
    };

    let prev = std::mem::take(&mut editor.hint_inlay_ids);
    editor.hint_inlay_ids = editor.display_map.splice_inlays(prev, inserts);
}

/// Remove every inlay hint from every editor's display map, across all
/// workspaces.
///
/// A hint is spliced into whichever editor was focused when its response
/// applied, so with splits or after switching buffers, hints outlive the moment
/// they were requested and sit in editors that are no longer focused. Once the
/// toggle is off the trigger returns early and never runs again, so a
/// focused-only clear would strand those hints forever. The sweep must reach
/// every editor.
pub(crate) fn clear_inlay_hints(stoat: &mut Stoat) {
    for ws in stoat.workspaces.values_mut() {
        for editor in ws.editors.values_mut() {
            let prev = std::mem::take(&mut editor.hint_inlay_ids);
            if !prev.is_empty() {
                editor.display_map.splice_inlays(prev, Vec::new());
            }
        }
    }
}

/// Debounce before requesting document highlights, so the symbol under the
/// cursor lights up only once cursor motion settles.
const DOCUMENT_HIGHLIGHT_DEBOUNCE: Duration = Duration::from_millis(200);

/// A completed document-highlight request's payload. It carries the buffer the
/// request targeted and each occurrence as a byte-offset range paired with
/// whether the server marked it a write.
pub(crate) type DocumentHighlightResponse = (BufferId, Vec<(std::ops::Range<usize>, bool)>);

/// Highlight the occurrences of the symbol under the focused editor's cursor when
/// the server supports it and the cursor rests in normal mode.
///
/// Leaving normal mode, or a change to the `(buffer, version, cursor offset)`
/// key, clears the current highlights immediately and re-arms a debounced
/// request. Occurrences therefore vanish while navigating and reappear once the
/// cursor settles. [`pump_lsp_document_highlight`] applies the response.
pub(crate) fn document_highlight_trigger(stoat: &mut Stoat) {
    if stoat.focused_mode() != "normal" {
        if stoat.last_document_highlight_key.is_some() {
            clear_document_highlights(stoat);
            stoat.last_document_highlight_key = None;
            stoat.pending_document_highlight_request = None;
        }
        return;
    }

    let Some((_, buffer_id)) = stoat.focused_editor_ids() else {
        return;
    };
    let Some((_, host)) = stoat
        .feature_hosts(buffer_id, LanguageServerFeature::DocumentHighlight)
        .into_iter()
        .next()
    else {
        return;
    };
    let encoding = host.offset_encoding();
    let Some((buffer_id, version, offset, rope, params)) =
        build_document_highlight_request(stoat, encoding)
    else {
        return;
    };

    let key = (buffer_id, version, offset);
    if stoat.last_document_highlight_key == Some(key) {
        return;
    }
    stoat.last_document_highlight_key = Some(key);
    clear_document_highlights(stoat);

    let executor = stoat.executor.clone();
    let task = stoat.spawn_woken(async move {
        executor.timer(DOCUMENT_HIGHLIGHT_DEBOUNCE).await;
        match host.document_highlight(params).await {
            Ok(Some(highlights)) => Some((
                buffer_id,
                convert_document_highlights(highlights, &rope, encoding),
            )),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "document_highlight request failed");
                None
            },
        }
    });
    stoat.pending_document_highlight_request = Some(task);
}

fn build_document_highlight_request(
    stoat: &mut Stoat,
    encoding: OffsetEncoding,
) -> Option<(BufferId, u64, usize, Rope, DocumentHighlightParams)> {
    let (buffer_id, version, offset, rope) = {
        let editor = crate::action_handlers::focused_editor_mut(stoat)?;
        if editor.review_view.is_some() {
            return None;
        }
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let tail_off = buf_snap.resolve_anchor(&sel.tail());
        let head_off = buf_snap.resolve_anchor(&sel.head());
        let offset = stoat_text::cursor_offset(buf_snap.rope(), tail_off, head_off);
        (
            editor.buffer_id,
            buf_snap.version(),
            offset,
            buf_snap.rope().clone(),
        )
    };

    let path = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)?;
    let uri = path_to_uri(&path)?;
    let position = crate::lsp::util::byte_offset_to_lsp_pos(&rope, offset, encoding);
    let params = DocumentHighlightParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    Some((buffer_id, version, offset, rope, params))
}

/// Convert LSP document highlights into `(byte range, is_write)` pairs using the
/// request-time rope. WRITE occurrences carry the write flag; READ, TEXT, and
/// unspecified occurrences carry the read flag.
fn convert_document_highlights(
    highlights: Vec<DocumentHighlight>,
    rope: &Rope,
    encoding: OffsetEncoding,
) -> Vec<(std::ops::Range<usize>, bool)> {
    highlights
        .into_iter()
        .map(|hl| {
            let start = crate::lsp::util::lsp_pos_to_byte_offset(rope, hl.range.start, encoding);
            let end = crate::lsp::util::lsp_pos_to_byte_offset(rope, hl.range.end, encoding);
            let is_write = hl.kind == Some(DocumentHighlightKind::WRITE);
            (start..end, is_write)
        })
        .collect()
}

/// Poll any in-flight document-highlight request and paint the results as read
/// and write text highlights on the focused editor. Returns true when state
/// changed.
pub(crate) fn pump_lsp_document_highlight(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_document_highlight_request.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(Some((buffer_id, items))) => {
            apply_document_highlights(stoat, buffer_id, items);
            true
        },
        Poll::Ready(None) => true,
        Poll::Pending => {
            stoat.pending_document_highlight_request = Some(task);
            false
        },
    }
}

fn apply_document_highlights(
    stoat: &mut Stoat,
    buffer_id: BufferId,
    items: Vec<(std::ops::Range<usize>, bool)>,
) {
    let read_style = document_highlight_style(stoat, scope::UI_HIGHLIGHT_READ);
    let write_style = document_highlight_style(stoat, scope::UI_HIGHLIGHT_WRITE);

    let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
        return;
    };
    if editor.buffer_id != buffer_id {
        return;
    }

    let (read, write) = {
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let mut read: Vec<std::ops::Range<Anchor>> = Vec::new();
        let mut write: Vec<std::ops::Range<Anchor>> = Vec::new();
        for (range, is_write) in items {
            let anchors = buf_snap.anchor_at(range.start, Bias::Right)
                ..buf_snap.anchor_at(range.end, Bias::Left);
            if is_write {
                write.push(anchors);
            } else {
                read.push(anchors);
            }
        }
        (read, write)
    };

    let read_key = HighlightKey::layer(HighlightLayer::DocumentHighlightRead);
    if read.is_empty() {
        editor.display_map.clear_highlights(read_key);
    } else {
        editor
            .display_map
            .highlight_text(read_key, read, read_style);
    }

    let write_key = HighlightKey::layer(HighlightLayer::DocumentHighlightWrite);
    if write.is_empty() {
        editor.display_map.clear_highlights(write_key);
    } else {
        editor
            .display_map
            .highlight_text(write_key, write, write_style);
    }
}

/// Remove the read and write document-highlight ranges from the focused editor.
pub(crate) fn clear_document_highlights(stoat: &mut Stoat) {
    let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
        return;
    };
    editor
        .display_map
        .clear_highlights(HighlightKey::layer(HighlightLayer::DocumentHighlightRead));
    editor
        .display_map
        .clear_highlights(HighlightKey::layer(HighlightLayer::DocumentHighlightWrite));
}

fn document_highlight_style(stoat: &Stoat, scope_key: &str) -> HighlightStyle {
    syntax_theme::style_to_highlight_style(&stoat.theme.get(scope_key))
}

/// Debounce before pulling diagnostics, so a burst of edits collapses into a
/// single request once typing settles.
const PULL_DIAGNOSTICS_DEBOUNCE: Duration = Duration::from_millis(300);

/// The outcome of a `textDocument/diagnostic` pull, ready to apply.
///
/// `Full` replaces the buffer's diagnostics with a fresh set. `Unchanged` is the
/// server's bandwidth optimisation, meaning the previous set still holds, so only
/// the result id is refreshed.
pub(crate) enum PullDiagnosticsOutcome {
    Full {
        path: PathBuf,
        diagnostics: Vec<Diagnostic>,
        result_id: Option<String>,
    },
    Unchanged {
        result_id: String,
    },
}

/// Pull diagnostics for every open buffer whose version changed since its last
/// pull, when the server advertises the capability.
///
/// A newly-opened buffer has no key yet, so its first tick pulls. A later edit
/// bumps the version and re-pulls. Each request carries the buffer's previous
/// result id so the server may answer Unchanged. [`pump_lsp_pull_diagnostics`]
/// applies the responses.
pub(crate) fn pull_diagnostics_trigger(stoat: &mut Stoat) {
    let plans: Vec<PullPlan> = stoat
        .lsp_opened
        .iter()
        .copied()
        .filter(|&id| {
            stoat
                .feature_hosts(id, LanguageServerFeature::PullDiagnostics)
                .into_iter()
                .next()
                .is_some()
        })
        .filter_map(|id| build_pull_plan(stoat, id))
        .collect();

    for plan in plans {
        stoat.last_pull_diagnostic_key.insert(plan.id, plan.version);

        let params = DocumentDiagnosticParams {
            text_document: TextDocumentIdentifier { uri: plan.uri },
            identifier: None,
            previous_result_id: stoat.pull_diagnostic_result_ids.get(&plan.id).cloned(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let lsp = stoat.lsp_for_feature(plan.id, LanguageServerFeature::PullDiagnostics);
        let executor = stoat.executor.clone();
        let path = plan.path;
        let task = stoat.spawn_woken(async move {
            executor.timer(PULL_DIAGNOSTICS_DEBOUNCE).await;
            match lsp.document_diagnostic(params).await {
                Ok(Some(report)) => parse_pull_report(report, path),
                Ok(None) => None,
                Err(err) => {
                    tracing::warn!(target: "stoat::lsp", ?err, "document_diagnostic request failed");
                    None
                },
            }
        });
        stoat.pending_pull_diagnostics.insert(plan.id, task);
    }
}

struct PullPlan {
    id: BufferId,
    version: u64,
    uri: Uri,
    path: PathBuf,
}

fn build_pull_plan(stoat: &Stoat, id: BufferId) -> Option<PullPlan> {
    let workspace = stoat.active_workspace();
    let buffer = workspace.buffers.get(id)?;
    let version = buffer.read().expect("buffer lock").version();
    if stoat.last_pull_diagnostic_key.get(&id) == Some(&version) {
        return None;
    }
    let path = workspace.buffers.path_for(id)?.to_path_buf();
    let uri = path_to_uri(&path)?;
    Some(PullPlan {
        id,
        version,
        uri,
        path,
    })
}

/// Convert a pull report into an applicable outcome, capturing the request-time
/// `path` for the Full case. Streaming `Partial` results carry no primary set and
/// are ignored.
fn parse_pull_report(
    report: DocumentDiagnosticReportResult,
    path: PathBuf,
) -> Option<PullDiagnosticsOutcome> {
    match report {
        DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(full)) => {
            let report = full.full_document_diagnostic_report;
            Some(PullDiagnosticsOutcome::Full {
                path,
                diagnostics: report.items,
                result_id: report.result_id,
            })
        },
        DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Unchanged(unchanged)) => {
            Some(PullDiagnosticsOutcome::Unchanged {
                result_id: unchanged.unchanged_document_diagnostic_report.result_id,
            })
        },
        DocumentDiagnosticReportResult::Partial(_) => None,
    }
}

/// Poll in-flight pull-diagnostic requests and apply any that completed. Returns
/// true when a request resolved.
pub(crate) fn pump_lsp_pull_diagnostics(stoat: &mut Stoat) -> bool {
    if stoat.pending_pull_diagnostics.is_empty() {
        return false;
    }

    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    let ready: Vec<(BufferId, Option<PullDiagnosticsOutcome>)> = {
        let mut ready = Vec::new();
        stoat
            .pending_pull_diagnostics
            .retain(|&id, task| match Pin::new(task).poll(&mut cx) {
                Poll::Ready(outcome) => {
                    ready.push((id, outcome));
                    false
                },
                Poll::Pending => true,
            });
        ready
    };

    if ready.is_empty() {
        return false;
    }
    for (id, outcome) in ready {
        apply_pull_diagnostics(stoat, id, outcome);
    }
    true
}

fn apply_pull_diagnostics(
    stoat: &mut Stoat,
    id: BufferId,
    outcome: Option<PullDiagnosticsOutcome>,
) {
    match outcome {
        Some(PullDiagnosticsOutcome::Full {
            path,
            diagnostics,
            result_id,
        }) => {
            let server = stoat
                .feature_hosts(id, LanguageServerFeature::PullDiagnostics)
                .into_iter()
                .next()
                .map(|(name, _)| name)
                .unwrap_or_else(|| String::from("lsp"));
            stoat
                .diagnostics
                .replace_from_server(path, server, diagnostics);
            match result_id {
                Some(rid) => {
                    stoat.pull_diagnostic_result_ids.insert(id, rid);
                },
                None => {
                    stoat.pull_diagnostic_result_ids.remove(&id);
                },
            }
        },
        Some(PullDiagnosticsOutcome::Unchanged { result_id }) => {
            stoat.pull_diagnostic_result_ids.insert(id, result_id);
        },
        None => {},
    }
}

/// Debounce before requesting semantic tokens, so a burst of edits collapses into
/// a single request once typing settles.
const SEMANTIC_TOKENS_DEBOUNCE: Duration = Duration::from_millis(500);

/// A decoded LSP semantic token. It pairs an absolute buffer span with the
/// tree-sitter highlight scope stem its type maps to and, separately, the
/// coarser [`LspSymbolKind`] the type names.
///
/// The scope and kind are independent. A token may carry a scope but no kind (a
/// keyword), a kind but no scope (a namespace, which has no highlight
/// equivalent), or both. A token with neither is dropped during decode.
#[derive(Debug, PartialEq)]
struct DecodedToken {
    line: u32,
    start: u32,
    length: u32,
    scope: Option<&'static str>,
    kind: Option<LspSymbolKind>,
}

/// A completed semantic-tokens request's payload. It carries the buffer, the
/// buffer version the request was built against, and the resolved `(byte range,
/// scope stem, symbol kind)` spans in request-time coordinates. The scope drives
/// the highlight channel and the kind the symbol-kind index. Each is optional.
pub(crate) type SemanticTokensOutcome = (
    BufferId,
    u64,
    Vec<(
        std::ops::Range<usize>,
        Option<&'static str>,
        Option<LspSymbolKind>,
    )>,
);

/// Request semantic tokens for the focused editor when the server advertises a
/// full-document legend and the `(buffer, version)` key changed.
///
/// A newly-focused buffer and each edit re-request behind a 500ms debounce. A key
/// change also clears the stale LSP highlights first. Tokens layer over the
/// tree-sitter baseline, so they never replace it -- only recolor on top.
/// [`pump_lsp_semantic_tokens`] applies the response.
pub(crate) fn semantic_tokens_trigger(stoat: &mut Stoat) {
    let Some((_, buffer_id)) = stoat.focused_editor_ids() else {
        return;
    };
    let lsp = stoat.lsp_for(buffer_id);
    let capabilities = lsp.capabilities();
    let Some(legend) = semantic_tokens_legend(&capabilities) else {
        return;
    };
    let legend = legend.to_vec();
    let encoding = lsp.offset_encoding();

    let Some((buffer_id, version, rope, params)) = build_semantic_tokens_request(stoat) else {
        return;
    };

    let key = (buffer_id, version);
    if stoat.last_semantic_tokens_key == Some(key) {
        return;
    }
    stoat.last_semantic_tokens_key = Some(key);

    // When the buffer is unchanged since tokens were last computed, reinstall
    // the retained set instead of re-requesting behind the debounce. The
    // invalidate below then fires only on the true re-request path.
    if let Some((cached_version, tokens, interner)) =
        stoat.active_workspace().buffers.lsp_tokens_for(buffer_id)
        && cached_version == version
    {
        if let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) {
            editor
                .display_map
                .set_lsp_token_highlights(buffer_id, tokens, interner);
        }
        return;
    }

    if let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) {
        editor.display_map.invalidate_lsp_highlights(buffer_id);
    }

    let executor = stoat.executor.clone();
    let task = stoat.spawn_woken(async move {
        executor.timer(SEMANTIC_TOKENS_DEBOUNCE).await;
        match lsp.semantic_tokens_full(params).await {
            Ok(Some(result)) => Some((
                buffer_id,
                version,
                convert_semantic_tokens(result, &legend, &rope, encoding),
            )),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "semantic_tokens_full request failed");
                None
            },
        }
    });
    stoat.pending_semantic_tokens = Some(task);
}

fn build_semantic_tokens_request(
    stoat: &mut Stoat,
) -> Option<(BufferId, u64, Rope, SemanticTokensParams)> {
    let (buffer_id, version, rope) = {
        let editor = crate::action_handlers::focused_editor_mut(stoat)?;
        if editor.review_view.is_some() {
            return None;
        }
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        (
            editor.buffer_id,
            buf_snap.version(),
            buf_snap.rope().clone(),
        )
    };

    let path = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)?;
    let uri = path_to_uri(&path)?;
    let params = SemanticTokensParams {
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        text_document: TextDocumentIdentifier { uri },
    };
    Some((buffer_id, version, rope, params))
}

/// The token-type legend from the server's semantic-tokens capability, or `None`
/// when it advertises no full-document semantic tokens.
fn semantic_tokens_legend(caps: &lsp_types::ServerCapabilities) -> Option<&[SemanticTokenType]> {
    let opts = match caps.semantic_tokens_provider.as_ref()? {
        SemanticTokensServerCapabilities::SemanticTokensOptions(o) => o,
        SemanticTokensServerCapabilities::SemanticTokensRegistrationOptions(o) => {
            &o.semantic_tokens_options
        },
    };
    opts.full.as_ref()?;
    Some(&opts.legend.token_types)
}

/// Decode a semantic-tokens response into `(byte range, scope stem)` spans using
/// the request-time rope. Partial (streaming) results carry no full token set and
/// yield nothing.
fn convert_semantic_tokens(
    result: SemanticTokensResult,
    legend: &[SemanticTokenType],
    rope: &Rope,
    encoding: OffsetEncoding,
) -> Vec<(
    std::ops::Range<usize>,
    Option<&'static str>,
    Option<LspSymbolKind>,
)> {
    let SemanticTokensResult::Tokens(tokens) = result else {
        return Vec::new();
    };
    let decoded = decode_semantic_tokens(&tokens.data, legend);
    let positions: Vec<Position> = decoded
        .iter()
        .flat_map(|t| {
            [
                Position::new(t.line, t.start),
                Position::new(t.line, t.start + t.length),
            ]
        })
        .collect();
    let offsets = crate::lsp::util::lsp_positions_to_byte_offsets_batch(rope, &positions, encoding);
    decoded
        .iter()
        .enumerate()
        .map(|(i, t)| (offsets[2 * i]..offsets[2 * i + 1], t.scope, t.kind))
        .collect()
}

/// Map an LSP `SemanticTokenType` name onto a stoat tree-sitter scope stem. Types
/// with no stoat equivalent return `None` and are skipped.
fn lsp_token_scope(token_type: &str) -> Option<&'static str> {
    Some(match token_type {
        "function" | "method" => "function",
        "macro" => "function.special",
        "type" | "class" | "enum" | "interface" | "struct" | "typeParameter" => "type",
        "variable" => "variable",
        "parameter" => "variable.parameter",
        "property" | "enumMember" => "property",
        "keyword" | "modifier" => "keyword",
        "comment" => "comment",
        "string" => "string",
        "number" => "number",
        "operator" => "operator",
        _ => return None,
    })
}

/// Map an LSP `SemanticTokenType` name onto the coarse [`LspSymbolKind`] it
/// names, so the distinction highlight decoding collapses (trait vs struct vs
/// enum, all "type") survives for cursor-aware features. Types that name no
/// symbol -- keywords, punctuation, literals -- return `None`.
fn lsp_symbol_kind(token_type: &str) -> Option<LspSymbolKind> {
    Some(match token_type {
        "interface" => LspSymbolKind::Trait,
        "type" | "class" | "struct" | "enum" | "union" | "typeAlias" | "builtinType"
        | "typeParameter" | "selfTypeKeyword" => LspSymbolKind::Type,
        "function" | "method" => LspSymbolKind::Function,
        "variable" | "parameter" | "property" | "enumMember" | "constParameter" | "selfKeyword" => {
            LspSymbolKind::Value
        },
        "namespace"
        | "macro"
        | "decorator"
        | "event"
        | "derive"
        | "attribute"
        | "label"
        | "lifetime"
        | "unresolvedReference" => LspSymbolKind::Symbol,
        _ => return None,
    })
}

/// Decode the LSP relative token stream into absolute-positioned spans.
///
/// Each token's line and start accumulate from the previous per the LSP encoding.
/// `delta_start` is relative within a line and absolute after a line break. Tokens
/// whose type index falls outside the legend, or whose type maps to neither a
/// highlight scope nor a symbol kind, are skipped.
fn decode_semantic_tokens(
    data: &[SemanticToken],
    legend: &[SemanticTokenType],
) -> Vec<DecodedToken> {
    let mut out = Vec::new();
    let mut line = 0u32;
    let mut col = 0u32;
    for token in data {
        line += token.delta_line;
        if token.delta_line == 0 {
            col += token.delta_start;
        } else {
            col = token.delta_start;
        }
        let Some(ty) = legend.get(token.token_type as usize) else {
            continue;
        };
        let scope = lsp_token_scope(ty.as_str());
        let kind = lsp_symbol_kind(ty.as_str());
        if scope.is_none() && kind.is_none() {
            continue;
        }
        out.push(DecodedToken {
            line,
            start: col,
            length: token.length,
            scope,
            kind,
        });
    }
    out
}

/// Poll any in-flight semantic-tokens request and paint the results onto the
/// focused editor's LSP highlight channel. Returns true when state changed.
pub(crate) fn pump_lsp_semantic_tokens(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_semantic_tokens.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(Some((buffer_id, version, items))) => {
            apply_semantic_tokens(stoat, buffer_id, version, items);
            true
        },
        Poll::Ready(None) => true,
        Poll::Pending => {
            stoat.pending_semantic_tokens = Some(task);
            false
        },
    }
}

fn apply_semantic_tokens(
    stoat: &mut Stoat,
    buffer_id: BufferId,
    version: u64,
    items: Vec<(
        std::ops::Range<usize>,
        Option<&'static str>,
        Option<LspSymbolKind>,
    )>,
) {
    // The highlight channel takes the scope-bearing spans, the symbol-kind index
    // the kind-bearing ones. A token may feed one, both, or (dropped in decode)
    // neither.
    let mut interner = HighlightStyleInterner::default();
    let styled: Vec<(std::ops::Range<usize>, _)> = items
        .iter()
        .filter_map(|(range, scope, _)| {
            let scope = (*scope)?;
            let scope_path = syntax_theme::theme_scope_for_key(scope);
            let style = syntax_theme::style_to_highlight_style(&stoat.theme.get(&scope_path));
            Some((range.clone(), interner.intern(style)))
        })
        .collect();
    let interner = Arc::new(interner);

    let kind_spans: Vec<(std::ops::Range<usize>, LspSymbolKind)> = items
        .iter()
        .filter_map(|(range, _, kind)| kind.map(|kind| (range.clone(), kind)))
        .collect();

    let ws = stoat.active_workspace_mut();
    let Some(shared) = ws.buffers.get(buffer_id) else {
        return;
    };

    // Anchor against the buffer's own snapshot from the registry, not the
    // focused editor's, so the response lands and is retained even when focus
    // has since moved to another buffer.
    let (tokens, kinds): (Arc<[SemanticTokenHighlight]>, LspSymbolKindIndex) = {
        let buf_snap = shared.read().expect("buffer poisoned").snapshot.clone();

        let t_starts: Vec<usize> = styled.iter().map(|(range, _)| range.start).collect();
        let t_ends: Vec<usize> = styled.iter().map(|(range, _)| range.end).collect();
        let tokens: Arc<[SemanticTokenHighlight]> = styled
            .into_iter()
            .zip(buf_snap.anchors_at_batch(&t_starts, Bias::Right))
            .zip(buf_snap.anchors_at_batch(&t_ends, Bias::Left))
            .map(|(((_, style), start), end)| SemanticTokenHighlight {
                range: start..end,
                style,
            })
            .collect();

        let k_starts: Vec<usize> = kind_spans.iter().map(|(range, _)| range.start).collect();
        let k_ends: Vec<usize> = kind_spans.iter().map(|(range, _)| range.end).collect();
        let kinds: LspSymbolKindIndex = kind_spans
            .into_iter()
            .zip(buf_snap.anchors_at_batch(&k_starts, Bias::Right))
            .zip(buf_snap.anchors_at_batch(&k_ends, Bias::Left))
            .map(|(((_, kind), start), end)| (start..end, kind))
            .collect();

        (tokens, kinds)
    };

    ws.buffers
        .store_lsp_tokens(buffer_id, version, tokens.clone(), interner.clone());
    ws.buffers.store_lsp_symbol_kinds(buffer_id, kinds);
    for editor in ws.editors.values_mut() {
        if editor.buffer_id == buffer_id {
            editor.display_map.set_lsp_token_highlights(
                buffer_id,
                tokens.clone(),
                interner.clone(),
            );
        }
    }
}

/// Debounce before requesting folding ranges, so a burst of edits collapses into
/// a single request once typing settles.
const FOLDING_RANGE_DEBOUNCE: Duration = Duration::from_millis(500);

/// A completed folding-range request's payload. It carries the buffer and each
/// foldable region as a `(byte range, collapsed text)` pair in request-time
/// coordinates.
pub(crate) type FoldingRangesOutcome = (BufferId, Vec<(std::ops::Range<usize>, Option<String>)>);

/// Request folding ranges for the focused editor when the server advertises the
/// capability and the `(buffer, version)` key changed.
///
/// A newly-focused buffer and each edit re-request behind a 500ms debounce.
/// [`pump_lsp_folding_ranges`] feeds the response into the display map's
/// `set_lsp_folding_ranges` hook, which replaces the buffer's foldable creases.
pub(crate) fn folding_ranges_trigger(stoat: &mut Stoat) {
    let Some((_, buffer_id)) = stoat.focused_editor_ids() else {
        return;
    };
    let lsp = stoat.lsp_for(buffer_id);
    if lsp.capabilities().folding_range_provider.is_none() {
        return;
    }

    let Some((buffer_id, version, rope, params)) = build_folding_range_request(stoat) else {
        return;
    };

    let key = (buffer_id, version);
    if stoat.last_folding_range_key == Some(key) {
        return;
    }
    stoat.last_folding_range_key = Some(key);

    let executor = stoat.executor.clone();
    let task = stoat.spawn_woken(async move {
        executor.timer(FOLDING_RANGE_DEBOUNCE).await;
        match lsp.folding_range(params).await {
            Ok(Some(ranges)) => Some((buffer_id, convert_folding_ranges(ranges, &rope))),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "folding_range request failed");
                None
            },
        }
    });
    stoat.pending_folding_ranges = Some(task);
}

fn build_folding_range_request(
    stoat: &mut Stoat,
) -> Option<(BufferId, u64, Rope, FoldingRangeParams)> {
    let (buffer_id, version, rope) = {
        let editor = crate::action_handlers::focused_editor_mut(stoat)?;
        if editor.review_view.is_some() {
            return None;
        }
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        (
            editor.buffer_id,
            buf_snap.version(),
            buf_snap.rope().clone(),
        )
    };

    let path = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)?;
    let uri = path_to_uri(&path)?;
    let params = FoldingRangeParams {
        text_document: TextDocumentIdentifier { uri },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    Some((buffer_id, version, rope, params))
}

/// Convert LSP folding ranges into `(byte range, collapsed text)` foldable spans
/// using the request-time rope.
///
/// Each span runs from the end of the start line to the end of the end line, so a
/// fold keeps the header line visible and collapses the body. Degenerate spans
/// (start at or after end) are dropped.
fn convert_folding_ranges(
    ranges: Vec<FoldingRange>,
    rope: &Rope,
) -> Vec<(std::ops::Range<usize>, Option<String>)> {
    let line_end_offset = |line: u32| rope.point_to_offset(Point::new(line, rope.line_len(line)));
    ranges
        .into_iter()
        .filter_map(|fr| {
            let start = line_end_offset(fr.start_line);
            let end = line_end_offset(fr.end_line);
            (start < end).then_some((start..end, fr.collapsed_text))
        })
        .collect()
}

/// Poll any in-flight folding-range request and install the results as foldable
/// creases on the focused editor. Returns true when state changed.
pub(crate) fn pump_lsp_folding_ranges(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_folding_ranges.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(Some((buffer_id, items))) => {
            apply_folding_ranges(stoat, buffer_id, items);
            true
        },
        Poll::Ready(None) => true,
        Poll::Pending => {
            stoat.pending_folding_ranges = Some(task);
            false
        },
    }
}

fn apply_folding_ranges(
    stoat: &mut Stoat,
    buffer_id: BufferId,
    items: Vec<(std::ops::Range<usize>, Option<String>)>,
) {
    let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
        return;
    };
    if editor.buffer_id != buffer_id {
        return;
    }

    let anchored: Vec<(std::ops::Range<Anchor>, Option<String>)> = {
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        items
            .into_iter()
            .map(|(range, text)| {
                (
                    buf_snap.anchor_at(range.start, Bias::Right)
                        ..buf_snap.anchor_at(range.end, Bias::Left),
                    text,
                )
            })
            .collect()
    };
    editor
        .display_map
        .set_lsp_folding_ranges(buffer_id, anchored);
}

/// One actionable entry in [`CodeActionPicker`]. Variants reflect
/// how the entry's effect is obtained: applied from a directly
/// supplied [`WorkspaceEdit`] (with an optional chained command),
/// resolved via a follow-up `codeAction/resolve` call, or dispatched
/// as a `workspace/executeCommand`.
#[derive(Debug, Clone)]
pub(crate) enum CodeActionEntry {
    Direct {
        title: String,
        edit: Box<WorkspaceEdit>,
        command: Option<lsp_types::Command>,
        server: String,
    },
    NeedsResolve {
        title: String,
        action: Box<lsp_types::CodeAction>,
        server: String,
    },
    Command {
        title: String,
        command: lsp_types::Command,
        server: String,
    },
}

impl CodeActionEntry {
    pub(crate) fn title(&self) -> &str {
        match self {
            Self::Direct { title, .. }
            | Self::NeedsResolve { title, .. }
            | Self::Command { title, .. } => title,
        }
    }
}

/// Cursor-anchored code action picker. Painted as a numbered popup
/// over a 9-row viewport that follows [`Self::selected_idx`]; the
/// user navigates with `j`/`k`, picks the selected entry with Enter,
/// picks visible entries 1..=9 with the corresponding digit keys,
/// and dismisses with Escape or any other action.
#[derive(Debug, Clone)]
pub(crate) struct CodeActionPicker {
    pub(crate) entries: Vec<CodeActionEntry>,
    pub(crate) anchor_offset: usize,
    pub(crate) selected_idx: usize,
    /// The routed server that produced these actions, so resolve and execute
    /// route back to it rather than the sole host.
    pub(crate) server: String,
}

/// Issue a `textDocument/codeAction` request for the focused editor's
/// primary selection range. The async response is stored on
/// [`Stoat::pending_code_action_request`] and applied by
/// [`pump_lsp_code_actions`] on the next render tick.
///
/// No-op when the focused pane is not an editor or the buffer has no
/// path. When the server does not advertise
/// [`LanguageServerFeature::CodeAction`], reports the language-server
/// state to the status bar instead. Replacing the prior pending
/// task drops it, cancelling its spawned future -- only one in-flight
/// code-action request is tracked at a time.
pub(crate) fn code_action(stoat: &mut Stoat) -> UpdateEffect {
    let (range_byte, anchor_offset, buffer_id, source_rope) = {
        let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let start = buf_snap.resolve_anchor(&sel.start);
        let end = buf_snap.resolve_anchor(&sel.end);
        let tail_off = buf_snap.resolve_anchor(&sel.tail());
        let head_off = buf_snap.resolve_anchor(&sel.head());
        let head = stoat_text::cursor_offset(buf_snap.rope(), tail_off, head_off);
        let (lo, hi) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        ((lo, hi), head, editor.buffer_id, buf_snap.rope().clone())
    };

    let Some((server, host)) = stoat
        .feature_hosts(buffer_id, LanguageServerFeature::CodeAction)
        .into_iter()
        .next()
    else {
        return report_lsp_unavailable(stoat, "code actions");
    };
    let encoding = host.offset_encoding();

    let Some(source_path) = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)
    else {
        return UpdateEffect::None;
    };
    let Some(source_uri) = path_to_uri(&source_path) else {
        return UpdateEffect::None;
    };

    let lsp_range = crate::lsp::util::byte_range_to_lsp_range(
        &source_rope,
        range_byte.0..range_byte.1,
        encoding,
    );

    let params = CodeActionParams {
        text_document: TextDocumentIdentifier { uri: source_uri },
        range: lsp_range,
        context: CodeActionContext {
            diagnostics: Vec::new(),
            only: None,
            trigger_kind: None,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };

    let task = stoat.spawn_woken(async move {
        match host.code_action(params).await {
            Ok(Some(actions)) => Some(actions),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "code_action request failed");
                None
            },
        }
    });
    stoat.pending_code_action_request = Some(task);
    stoat.pending_code_action_picker = Some(CodeActionPicker {
        entries: Vec::new(),
        anchor_offset,
        selected_idx: 0,
        server,
    });
    // The picker is reset to an empty list above so a stale popup
    // from a prior request does not persist while the new one is
    // in flight; pump_lsp_code_actions overwrites it on response.
    UpdateEffect::None
}

/// Poll any in-flight code-action request
/// ([`Stoat::pending_code_action_request`]) and translate the result
/// into a [`CodeActionPicker`]. Filters out `Command`-only entries
/// and `CodeAction` items that have neither a `WorkspaceEdit` nor a
/// resolve trigger. Clears the picker when no actionable entries
/// remain.
pub(crate) fn pump_lsp_code_actions(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_code_action_request.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(Some(actions)) => {
            let server = stoat
                .pending_code_action_picker
                .as_ref()
                .map(|picker| picker.server.clone())
                .unwrap_or_default();
            let entries: Vec<CodeActionEntry> = actions
                .into_iter()
                .filter_map(|item| match item {
                    CodeActionOrCommand::CodeAction(ca) => {
                        match (ca.edit.clone(), ca.data.clone(), ca.command.clone()) {
                            (Some(edit), _, command) => Some(CodeActionEntry::Direct {
                                title: ca.title.clone(),
                                edit: Box::new(edit),
                                command,
                                server: server.clone(),
                            }),
                            (None, Some(_), _) => Some(CodeActionEntry::NeedsResolve {
                                title: ca.title.clone(),
                                action: Box::new(ca),
                                server: server.clone(),
                            }),
                            (None, None, Some(command)) => Some(CodeActionEntry::Command {
                                title: ca.title.clone(),
                                command,
                                server: server.clone(),
                            }),
                            (None, None, None) => None,
                        }
                    },
                    CodeActionOrCommand::Command(command) => Some(CodeActionEntry::Command {
                        title: command.title.clone(),
                        command,
                        server: server.clone(),
                    }),
                })
                .collect();
            if entries.is_empty() {
                set_lsp_status(stoat, "lsp: no code actions available".to_string());
                stoat.pending_code_action_picker = None;
            } else if let Some(picker) = stoat.pending_code_action_picker.as_mut() {
                picker.entries = entries;
            }
            true
        },
        Poll::Ready(None) => {
            set_lsp_status(stoat, "lsp: no code actions available".to_string());
            stoat.pending_code_action_picker = None;
            true
        },
        Poll::Pending => {
            stoat.pending_code_action_request = Some(task);
            false
        },
    }
}

/// Poll any in-flight `codeAction/resolve` task
/// ([`Stoat::pending_code_action_resolve`]). On `Ready(Some(edit))`
/// applies the edit via [`crate::lsp::edit_apply::apply_workspace_edit`];
/// errors are logged and swallowed so a malformed edit does not crash
/// the app. On `Ready(None)` the resolve produced no edit, which is a
/// silent no-op.
pub(crate) fn pump_lsp_code_action_resolve(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_code_action_resolve.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(Some(edit)) => {
            apply_code_action_edit(stoat, edit);
            true
        },
        Poll::Ready(None) => true,
        Poll::Pending => {
            stoat.pending_code_action_resolve = Some(task);
            false
        },
    }
}

/// Apply a code-action [`WorkspaceEdit`] and log+swallow any error.
/// Code actions arrive from the server and may fail to apply for
/// reasons orthogonal to user action (URI scheme, missing buffer);
/// crashing the app on a server-driven failure is the wrong shape.
fn apply_code_action_edit(stoat: &mut Stoat, edit: WorkspaceEdit) {
    if let Err(err) = crate::lsp::edit_apply::apply_workspace_edit(stoat, edit) {
        tracing::warn!(
            target: "stoat::lsp",
            ?err,
            "code_action workspace edit failed to apply",
        );
    }
}

/// User has picked entry `index` from the open code-action picker.
/// `Direct` entries apply immediately; `NeedsResolve` entries spawn
/// a `codeAction/resolve` task whose result is applied by
/// [`pump_lsp_code_action_resolve`]. Clears the picker either way.
/// No-op when no picker is open or `index` is out of range.
pub(crate) fn pick_code_action(stoat: &mut Stoat, index: usize) -> bool {
    let Some(picker) = stoat.pending_code_action_picker.take() else {
        return false;
    };
    let Some(entry) = picker.entries.into_iter().nth(index) else {
        return false;
    };
    let buffer_id = stoat.focused_editor_ids().map(|(_, id)| id);
    match entry {
        CodeActionEntry::Direct {
            edit,
            command,
            server,
            ..
        } => {
            apply_code_action_edit(stoat, *edit);
            if let Some(command) = command {
                dispatch_execute_command(stoat, &server, buffer_id, command);
            }
        },
        CodeActionEntry::NeedsResolve { action, server, .. } => {
            let lsp = resolve_code_action_host(stoat, &server, buffer_id);
            let task = stoat.spawn_woken(async move {
                match lsp.code_action_resolve(*action).await {
                    Ok(resolved) => resolved.edit,
                    Err(err) => {
                        tracing::warn!(
                            target: "stoat::lsp",
                            ?err,
                            "codeAction/resolve request failed",
                        );
                        None
                    },
                }
            });
            stoat.pending_code_action_resolve = Some(task);
        },
        CodeActionEntry::Command {
            command, server, ..
        } => {
            dispatch_execute_command(stoat, &server, buffer_id, command);
        },
    }
    true
}

/// Resolve the host a code action's resolve or command should target: the named
/// producing server, falling back to the buffer's code-action host and then the
/// sole host.
fn resolve_code_action_host(
    stoat: &Stoat,
    server: &str,
    buffer_id: Option<BufferId>,
) -> Arc<dyn LspHost> {
    if let Some(host) = stoat.lsp_registry.client(server) {
        return host;
    }
    match buffer_id {
        Some(id) => stoat.lsp_for_feature(id, LanguageServerFeature::CodeAction),
        None => stoat.lsp_host(),
    }
}

/// Spawn a `workspace/executeCommand` request through
/// [`Stoat::executor`] and detach the task. The result `Option<Value>`
/// is generally a server-side side-effect (servers that produce edits
/// reply via the `workspace/applyEdit` request path); errors are
/// logged and swallowed so a failing command does not crash the app.
fn dispatch_execute_command(
    stoat: &Stoat,
    server: &str,
    buffer_id: Option<BufferId>,
    command: lsp_types::Command,
) {
    let lsp = resolve_code_action_host(stoat, server, buffer_id);
    let label = command.command.clone();
    let params = lsp_types::ExecuteCommandParams {
        command: command.command,
        arguments: command.arguments.unwrap_or_default(),
        work_done_progress_params: Default::default(),
    };
    stoat
        .executor
        .spawn(async move {
            if let Err(err) = lsp.execute_command(params).await {
                tracing::warn!(
                    target: "stoat::lsp",
                    ?err,
                    command = %label,
                    "workspace/executeCommand request failed",
                );
            }
        })
        .detach();
}

/// Resolved prepare-rename payload carried from the spawned task to
/// [`pump_lsp_prepare_rename`]. Captures both the symbol byte range
/// (so submit can build a `RenameParams` with the right position) and
/// the placeholder text seeded into the input modal.
#[derive(Debug, Clone)]
pub(crate) struct RenamePrep {
    pub(crate) source_uri: Uri,
    pub(crate) symbol_position: Position,
    pub(crate) placeholder: String,
    /// The routed server that answered prepare, carried so submit targets the
    /// same one. `buffer_id` is its fallback route when the name no longer
    /// resolves at submit time.
    pub(crate) server: Option<String>,
    pub(crate) buffer_id: BufferId,
}

/// Open input-modal state for the rename flow. Carries the
/// [`crate::input_view::InputView`] so render can paint the
/// embedded editor and submit can read the typed name; carries
/// the symbol's URI and request position so submit can build the
/// `RenameParams` without touching the editor again.
#[derive(Debug)]
pub(crate) struct RenameInputState {
    pub(crate) input: crate::input_view::InputView,
    pub(crate) source_uri: Uri,
    pub(crate) symbol_position: Position,
    pub(crate) anchor_offset: usize,
    /// The server that answered prepare, resolved again at submit so both halves
    /// of the rename hit the same one. `buffer_id` is the fallback route.
    pub(crate) server: Option<String>,
    pub(crate) buffer_id: BufferId,
}

/// Issue a `textDocument/prepareRename` request for the symbol under
/// the focused editor's primary cursor. The async response is stored
/// on [`Stoat::pending_prepare_rename`] and applied by
/// [`pump_lsp_prepare_rename`] on the next render tick.
///
/// No-op when the focused pane is not an editor or the buffer has no
/// path. When the server does not advertise
/// [`LanguageServerFeature::RenameSymbol`], reports the language-server
/// state to the status bar instead.
pub(crate) fn rename_symbol(stoat: &mut Stoat) -> UpdateEffect {
    let (cursor_offset, buffer_id, source_rope) = {
        let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let tail_off = buf_snap.resolve_anchor(&sel.tail());
        let head_off = buf_snap.resolve_anchor(&sel.head());
        let offset = stoat_text::cursor_offset(buf_snap.rope(), tail_off, head_off);
        (offset, editor.buffer_id, buf_snap.rope().clone())
    };

    let Some((server, host)) = stoat
        .feature_hosts(buffer_id, LanguageServerFeature::RenameSymbol)
        .into_iter()
        .next()
    else {
        return report_lsp_unavailable(stoat, "rename");
    };
    let server = Some(server);
    let encoding = host.offset_encoding();

    let Some(source_path) = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)
    else {
        return UpdateEffect::None;
    };
    let Some(source_uri) = path_to_uri(&source_path) else {
        return UpdateEffect::None;
    };

    let position = crate::lsp::util::byte_offset_to_lsp_pos(&source_rope, cursor_offset, encoding);

    let params = TextDocumentPositionParams {
        text_document: TextDocumentIdentifier {
            uri: source_uri.clone(),
        },
        position,
    };

    let task = stoat.spawn_woken(async move {
        let response = match host.prepare_rename(params).await {
            Ok(Some(resp)) => resp,
            Ok(None) => return None,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "prepare_rename request failed");
                return None;
            },
        };
        let placeholder = match response {
            PrepareRenameResponse::Range(range) => {
                let start_off =
                    crate::lsp::util::lsp_pos_to_byte_offset(&source_rope, range.start, encoding);
                let end_off =
                    crate::lsp::util::lsp_pos_to_byte_offset(&source_rope, range.end, encoding);
                source_rope.slice(start_off..end_off).to_string()
            },
            PrepareRenameResponse::RangeWithPlaceholder { placeholder, .. } => placeholder,
            PrepareRenameResponse::DefaultBehavior { .. } => String::new(),
        };
        Some(RenamePrep {
            source_uri,
            symbol_position: position,
            placeholder,
            server,
            buffer_id,
        })
    });
    stoat.pending_prepare_rename = Some(task);
    UpdateEffect::None
}

/// Poll any in-flight prepare-rename task and, on `Ready(Some)`, open
/// the input modal seeded with the placeholder text. The input is born
/// in insert mode so typing routes through `handle_insert_key` into the
/// modal's [`crate::input_view::InputView`].
pub(crate) fn pump_lsp_prepare_rename(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_prepare_rename.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(Some(prep)) => {
            let anchor_offset = {
                let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
                    return true;
                };
                let snapshot = editor.display_map.snapshot();
                let buf_snap = snapshot.buffer_snapshot();
                let sel = editor.selections.newest_anchor();
                let tail_off = buf_snap.resolve_anchor(&sel.tail());
                let head_off = buf_snap.resolve_anchor(&sel.head());
                stoat_text::cursor_offset(buf_snap.rope(), tail_off, head_off)
            };
            let executor = stoat.executor.clone();
            let ws = stoat.active_workspace_mut();
            let input = crate::input_view::InputView::create(
                ws,
                executor,
                crate::input_view::SubmitTarget::RenameSymbol,
                &prep.placeholder,
                "insert",
                1,
            );
            stoat.rename_input = Some(RenameInputState {
                input,
                source_uri: prep.source_uri,
                symbol_position: prep.symbol_position,
                anchor_offset,
                server: prep.server,
                buffer_id: prep.buffer_id,
            });
            true
        },
        Poll::Ready(None) => true,
        Poll::Pending => {
            stoat.pending_prepare_rename = Some(task);
            false
        },
    }
}

/// Submit the rename input: read the typed text, fire
/// `textDocument/rename`, and tear down the modal. Returns true when
/// the modal was open (so the caller can short-circuit other submit
/// branches).
pub(crate) fn rename_input_submit(stoat: &mut Stoat) -> bool {
    let Some(rename_state) = stoat.rename_input.take() else {
        return false;
    };
    let new_name = rename_state.input.text(stoat.active_workspace());
    let ws = stoat.active_workspace_mut();
    rename_state.input.dispose(ws);

    if new_name.is_empty() {
        return true;
    }

    let params = RenameParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: rename_state.source_uri,
            },
            position: rename_state.symbol_position,
        },
        new_name,
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    let lsp = rename_state
        .server
        .as_deref()
        .and_then(|name| stoat.lsp_registry.client(name))
        .unwrap_or_else(|| {
            stoat.lsp_for_feature(rename_state.buffer_id, LanguageServerFeature::RenameSymbol)
        });
    let task = stoat.spawn_woken(async move {
        match lsp.rename(params).await {
            Ok(edit) => edit,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "rename request failed");
                None
            },
        }
    });
    stoat.pending_rename = Some(task);
    true
}

/// Cancel the rename input modal without firing rename. Disposes the
/// embedded input.
pub(crate) fn rename_input_cancel(stoat: &mut Stoat) -> bool {
    let Some(rename_state) = stoat.rename_input.take() else {
        return false;
    };
    let ws = stoat.active_workspace_mut();
    rename_state.input.dispose(ws);
    true
}

/// Poll any in-flight rename task and apply its [`WorkspaceEdit`].
pub(crate) fn pump_lsp_rename(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_rename.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(Some(edit)) => {
            if let Err(err) = crate::lsp::edit_apply::apply_workspace_edit(stoat, edit) {
                tracing::warn!(
                    target: "stoat::lsp",
                    ?err,
                    "rename workspace edit failed to apply",
                );
            }
            true
        },
        Poll::Ready(None) => true,
        Poll::Pending => {
            stoat.pending_rename = Some(task);
            false
        },
    }
}

/// One entry in the graph-navigation [`SymbolPicker`]. `title` is the symbol
/// name as painted in the popup and `symbol` the graph node the entry jumps to
/// on selection.
#[derive(Debug, Clone)]
pub(crate) struct SymbolEntry {
    pub(crate) title: String,
    pub(crate) symbol: SymbolKey,
}

/// Cursor-anchored graph-navigation picker. Painted as a numbered
/// popup over a viewport of up to 9 visible entries that follows
/// [`Self::selected_idx`]. The user navigates with `j`/`k`, picks
/// the selected entry with Enter, picks visible entries 1..=9 with
/// the corresponding digit keys, and dismisses with Escape or any
/// other action.
///
/// Document symbols use the [`crate::symbol_finder::SymbolFinder`] modal
/// instead. Only code-graph navigation still populates this popup.
#[derive(Debug, Clone)]
pub(crate) struct SymbolPicker {
    pub(crate) entries: Vec<SymbolEntry>,
    pub(crate) anchor_offset: usize,
    pub(crate) selected_idx: usize,
}

/// Issue a `textDocument/documentSymbol` request for the focused
/// buffer. The async response is stored on
/// [`Stoat::pending_symbol_picker_request`] and applied by
/// [`pump_lsp_symbol_picker`] on the next render tick.
///
/// No-op when the focused pane is not an editor or the buffer has no
/// path. When the server does not advertise
/// [`LanguageServerFeature::DocumentSymbols`], reports the
/// language-server state to the status bar instead.
pub(crate) fn open_symbol_picker(stoat: &mut Stoat) -> UpdateEffect {
    let (buffer_id, rope) = {
        let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        (editor.buffer_id, buf_snap.rope().clone())
    };

    let hosts = stoat.feature_hosts(buffer_id, LanguageServerFeature::DocumentSymbols);
    if hosts.is_empty() {
        return report_lsp_unavailable(stoat, "document symbols");
    }

    let Some(source_path) = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)
    else {
        return UpdateEffect::None;
    };
    let Some(source_uri) = path_to_uri(&source_path) else {
        return UpdateEffect::None;
    };

    let params = DocumentSymbolParams {
        text_document: TextDocumentIdentifier { uri: source_uri },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };

    let task = stoat.spawn_woken(async move {
        let requests = hosts.iter().map(|(_, host)| {
            let encoding = host.offset_encoding();
            let params = params.clone();
            async move { (encoding, host.document_symbol(params).await) }
        });
        let responses = futures::future::join_all(requests).await;

        let mut entries = Vec::new();
        for (encoding, result) in responses {
            match result {
                Ok(Some(response)) => {
                    entries.extend(symbol_picker_entries(&rope, encoding, response))
                },
                Ok(None) => {},
                Err(err) => {
                    tracing::warn!(target: "stoat::lsp", ?err, "document_symbol request failed")
                },
            }
        }
        entries
    });
    stoat.pending_symbol_picker_request = Some(task);
    stoat.set_focused_mode("normal".into());
    let executor = stoat.executor.clone();
    let finder = {
        let ws = stoat.active_workspace_mut();
        SymbolFinder::new(ws, executor, buffer_id)
    };
    stoat.symbol_finder = Some(finder);
    UpdateEffect::None
}

/// Poll any in-flight document-symbol request and fill the open
/// [`SymbolFinder`] with the entries every capable server merged, refiltering
/// against the current query.
///
/// The request task converts and concatenates each server's response, so this
/// only installs the result. An empty result keeps the modal open over an empty
/// list, matching finder behavior.
pub(crate) fn pump_lsp_symbol_picker(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_symbol_picker_request.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(entries) => {
            let query = symbol_finder_query(stoat);
            if let Some(finder) = stoat.symbol_finder.as_mut() {
                finder.set_entries(entries, &query);
            }
            true
        },
        Poll::Pending => {
            stoat.pending_symbol_picker_request = Some(task);
            false
        },
    }
}

/// The text currently typed into the symbol finder's input, or empty when no
/// finder is open.
fn symbol_finder_query(stoat: &Stoat) -> String {
    stoat
        .symbol_finder
        .as_ref()
        .map(|finder| finder.input.text(stoat.active_workspace()))
        .unwrap_or_default()
}

/// Refilter the open symbol finder against its current input. Called on the
/// render/idle path so typing narrows the list without a dedicated key handler.
pub(crate) fn sync_symbol_finder(stoat: &mut Stoat) {
    let query = symbol_finder_query(stoat);
    if let Some(finder) = stoat.symbol_finder.as_mut() {
        finder.refilter(&query);
    }
}

/// Move the symbol finder selection by `delta`, saturating at list bounds.
pub(crate) fn symbol_finder_move_selection(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    match stoat.symbol_finder.as_mut() {
        Some(finder) => {
            finder.move_selection(delta);
            UpdateEffect::Redraw
        },
        None => UpdateEffect::None,
    }
}

/// Page the symbol finder selection by half the list height in `dir`.
pub(crate) fn symbol_finder_page(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    match stoat.symbol_finder.as_mut() {
        Some(finder) => {
            finder.page(dir);
            UpdateEffect::Redraw
        },
        None => UpdateEffect::None,
    }
}

/// Jump to the selected symbol and close the finder.
///
/// Returns `None` when no finder is open so [`super::submit_prompt_input`] falls
/// through to the next probe. An empty list closes without jumping.
pub(crate) fn symbol_finder_submit(stoat: &mut Stoat) -> Option<UpdateEffect> {
    stoat.symbol_finder.as_ref()?;
    let target = stoat
        .symbol_finder
        .as_ref()
        .and_then(|finder| finder.selected_entry())
        .map(|entry| entry.target.clone());
    close_symbol_finder(stoat);
    if let Some(SymbolTarget::Offset(offset)) = target {
        crate::action_handlers::movement::jump_to_offset(stoat, offset);
    }
    Some(UpdateEffect::Redraw)
}

/// Close the symbol finder on Escape.
///
/// Returns `None` when no finder is open so [`super::cancel_prompt_input`] falls
/// through to the next probe.
pub(crate) fn symbol_finder_cancel(stoat: &mut Stoat) -> Option<UpdateEffect> {
    if stoat.symbol_finder.is_some() {
        close_symbol_finder(stoat);
        return Some(UpdateEffect::Redraw);
    }
    None
}

/// Close the symbol finder, disposing its input editor.
pub(crate) fn close_symbol_finder(stoat: &mut Stoat) {
    if let Some(finder) = stoat.symbol_finder.take() {
        finder.dispose(stoat.active_workspace_mut());
    }
}

/// Convert a [`DocumentSymbolResponse`] into a flat list of picker
/// entries, resolving each symbol's LSP position to a byte offset
/// in the supplied rope. Nested responses are flattened DFS with a
/// dotted ancestor-path prefix on the title (e.g. `outer.inner`) so
/// the picker conveys hierarchy. The full list is returned; the
/// renderer paints a 9-row viewport over `entries`.
fn symbol_picker_entries(
    rope: &Rope,
    encoding: OffsetEncoding,
    response: DocumentSymbolResponse,
) -> Vec<SymbolFinderEntry> {
    let mut entries: Vec<SymbolFinderEntry> = Vec::new();
    match response {
        DocumentSymbolResponse::Flat(items) => {
            for SymbolInformation {
                name,
                location,
                kind,
                ..
            } in items
            {
                let offset =
                    crate::lsp::util::lsp_pos_to_byte_offset(rope, location.range.start, encoding);
                entries.push(finder_entry(rope, name, kind, offset));
            }
        },
        DocumentSymbolResponse::Nested(items) => {
            fn walk(
                rope: &Rope,
                encoding: OffsetEncoding,
                items: Vec<DocumentSymbol>,
                ancestors: &mut Vec<String>,
                out: &mut Vec<SymbolFinderEntry>,
            ) {
                for symbol in items {
                    let offset = crate::lsp::util::lsp_pos_to_byte_offset(
                        rope,
                        symbol.selection_range.start,
                        encoding,
                    );
                    let title = if ancestors.is_empty() {
                        symbol.name.clone()
                    } else {
                        format!("{}.{}", ancestors.join("."), symbol.name)
                    };
                    out.push(finder_entry(rope, title, symbol.kind, offset));
                    if let Some(children) = symbol.children {
                        ancestors.push(symbol.name);
                        walk(rope, encoding, children, ancestors, out);
                        ancestors.pop();
                    }
                }
            }
            let mut ancestors: Vec<String> = Vec::new();
            walk(rope, encoding, items, &mut ancestors, &mut entries);
        },
    }
    entries
}

/// Build a document-symbol finder entry, deriving the display line from the
/// resolved byte `offset`.
fn finder_entry(rope: &Rope, title: String, kind: SymbolKind, offset: usize) -> SymbolFinderEntry {
    SymbolFinderEntry {
        title,
        kind: Some(kind),
        line: rope.offset_to_point(offset).row,
        target: SymbolTarget::Offset(offset),
    }
}

/// Apply the user's pick from the open graph-navigation picker, jumping to the
/// entry's symbol and opening another file if needed, and clear the picker.
///
/// No-op when no picker is open or `index` is out of range.
pub(crate) fn pick_symbol(stoat: &mut Stoat, index: usize) -> bool {
    let Some(picker) = stoat.pending_symbol_picker.take() else {
        return false;
    };
    let Some(entry) = picker.entries.into_iter().nth(index) else {
        return false;
    };
    crate::code_index::nav::jump_to_symbol(stoat, entry.symbol);
    true
}

/// Open input modal for the workspace-symbol query. Carries the
/// [`crate::input_view::InputView`] so render can paint the
/// embedded editor and submit can read the typed query;
/// `anchor_offset` anchors the modal popup to the cursor.
#[derive(Debug)]
pub(crate) struct WorkspaceSymbolInputState {
    pub(crate) input: crate::input_view::InputView,
    pub(crate) anchor_offset: usize,
    /// Every workspace-symbol server routed at open, resolved by name again at
    /// submit so a focus change mid-query does not re-route. `buffer_id` is the
    /// fallback route when none of the names still resolve.
    pub(crate) servers: Vec<String>,
    pub(crate) buffer_id: BufferId,
}

/// One entry in [`WorkspaceSymbolPicker`].
///
/// `title` is the symbol name, `path` the absolute filesystem path to open, and
/// `position` the LSP position in the target file. `encoding` is the offset
/// encoding of the server that produced this entry, so a fan-out across servers
/// that negotiated different encodings still resolves each position on accept.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceSymbolEntry {
    pub(crate) title: String,
    pub(crate) path: PathBuf,
    pub(crate) position: Position,
    pub(crate) encoding: OffsetEncoding,
}

/// Cursor-anchored workspace-symbol picker. Painted as a numbered
/// popup over a 9-row viewport that follows [`Self::selected_idx`];
/// the user navigates with `j`/`k`, picks the selected entry with
/// Enter, picks visible entries 1..=9 with the corresponding digit
/// keys, and dismisses with Escape or any other action.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceSymbolPicker {
    pub(crate) entries: Vec<WorkspaceSymbolEntry>,
    pub(crate) anchor_offset: usize,
    pub(crate) selected_idx: usize,
}

/// Open the workspace-symbol query input modal. When the server does not
/// advertise [`LanguageServerFeature::WorkspaceSymbols`], reports the
/// language-server state to the status bar instead of opening. The input is
/// born in insert mode so typing routes through `handle_insert_key` into the
/// modal's [`crate::input_view::InputView`]. The modal seed is empty;
/// submit fires the request, cancel disposes the input.
pub(crate) fn open_workspace_symbol_picker(stoat: &mut Stoat) -> UpdateEffect {
    let Some((_, buffer_id)) = stoat.focused_editor_ids() else {
        return UpdateEffect::None;
    };
    let servers: Vec<String> = stoat
        .feature_hosts(buffer_id, LanguageServerFeature::WorkspaceSymbols)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    if servers.is_empty() {
        return report_lsp_unavailable(stoat, "workspace symbols");
    }

    let anchor_offset = {
        let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let tail_off = buf_snap.resolve_anchor(&sel.tail());
        let head_off = buf_snap.resolve_anchor(&sel.head());
        stoat_text::cursor_offset(buf_snap.rope(), tail_off, head_off)
    };

    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let input = crate::input_view::InputView::create(
        ws,
        executor,
        crate::input_view::SubmitTarget::WorkspaceSymbolPicker,
        "",
        "insert",
        1,
    );
    stoat.workspace_symbol_input = Some(WorkspaceSymbolInputState {
        input,
        anchor_offset,
        servers,
        buffer_id,
    });
    UpdateEffect::Redraw
}

/// Submit the workspace-symbol input: read the query text, fire
/// `workspace/symbol` and tear down the modal. Returns true when the
/// modal was open.
pub(crate) fn workspace_symbol_submit(stoat: &mut Stoat) -> bool {
    let Some(state) = stoat.workspace_symbol_input.take() else {
        return false;
    };
    let query = state.input.text(stoat.active_workspace());
    let anchor_offset = state.anchor_offset;
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);

    let params = WorkspaceSymbolParams {
        query,
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };

    let mut hosts: Vec<Arc<dyn LspHost>> = state
        .servers
        .iter()
        .filter_map(|name| stoat.lsp_registry.client(name))
        .collect();
    if hosts.is_empty() {
        hosts = stoat
            .feature_hosts(state.buffer_id, LanguageServerFeature::WorkspaceSymbols)
            .into_iter()
            .map(|(_, host)| host)
            .collect();
    }

    let task = stoat.spawn_woken(async move {
        let requests = hosts.iter().map(|host| {
            let encoding = host.offset_encoding();
            let params = params.clone();
            async move { (encoding, host.workspace_symbol(params).await) }
        });
        let responses = futures::future::join_all(requests).await;

        let mut entries = Vec::new();
        for (encoding, result) in responses {
            match result {
                Ok(Some(response)) => entries.extend(workspace_symbol_entries(response, encoding)),
                Ok(None) => {},
                Err(err) => {
                    tracing::warn!(target: "stoat::lsp", ?err, "workspace_symbol request failed")
                },
            }
        }
        entries
    });
    stoat.pending_workspace_symbol_request = Some(task);
    stoat.pending_workspace_symbol_picker = Some(WorkspaceSymbolPicker {
        entries: Vec::new(),
        anchor_offset,
        selected_idx: 0,
    });
    true
}

/// Cancel the workspace-symbol input modal. Disposes the embedded
/// input.
pub(crate) fn workspace_symbol_cancel(stoat: &mut Stoat) -> bool {
    let Some(state) = stoat.workspace_symbol_input.take() else {
        return false;
    };
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);
    true
}

/// Poll any in-flight workspace-symbol request and fill the
/// [`WorkspaceSymbolPicker`] with the entries every capable server merged.
///
/// The request task converts and concatenates each server's response, so this
/// only installs the result. Drops the picker when no server returned an entry.
pub(crate) fn pump_lsp_workspace_symbol(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_workspace_symbol_request.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(entries) => {
            if entries.is_empty() {
                stoat.pending_workspace_symbol_picker = None;
            } else if let Some(picker) = stoat.pending_workspace_symbol_picker.as_mut() {
                picker.entries = entries;
            }
            true
        },
        Poll::Pending => {
            stoat.pending_workspace_symbol_request = Some(task);
            false
        },
    }
}

fn workspace_symbol_entries(
    response: WorkspaceSymbolResponse,
    encoding: OffsetEncoding,
) -> Vec<WorkspaceSymbolEntry> {
    let mut entries: Vec<WorkspaceSymbolEntry> = Vec::new();
    match response {
        WorkspaceSymbolResponse::Flat(items) => {
            for SymbolInformation { name, location, .. } in items {
                let Some(path) = crate::app::lsp_uri_to_path(&location.uri) else {
                    continue;
                };
                entries.push(WorkspaceSymbolEntry {
                    title: name,
                    path,
                    position: location.range.start,
                    encoding,
                });
            }
        },
        WorkspaceSymbolResponse::Nested(items) => {
            for WorkspaceSymbol { name, location, .. } in items {
                let (uri, position) = match location {
                    OneOf::Left(loc) => (loc.uri, loc.range.start),
                    OneOf::Right(workspace_loc) => {
                        // `WorkspaceLocation` carries no range, so fall back to
                        // the start of file. A future `workspaceSymbol/resolve`
                        // round-trip would refine this.
                        (workspace_loc.uri, Position::new(0, 0))
                    },
                };
                let Some(path) = crate::app::lsp_uri_to_path(&uri) else {
                    continue;
                };
                entries.push(WorkspaceSymbolEntry {
                    title: name,
                    path,
                    position,
                    encoding,
                });
            }
        },
    }
    entries
}

/// Apply the user's pick from the open workspace-symbol picker:
/// open the symbol's file in the focused pane and jump the primary
/// cursor to the symbol's position. Clears the picker.
pub(crate) fn pick_workspace_symbol(stoat: &mut Stoat, index: usize) -> bool {
    let Some(picker) = stoat.pending_workspace_symbol_picker.take() else {
        return false;
    };
    let Some(entry) = picker.entries.into_iter().nth(index) else {
        return false;
    };
    let focused = stoat.active_workspace().panes.focus();
    crate::action_handlers::file::open_file_in_pane(stoat, focused, &entry.path);

    let encoding = entry.encoding;
    let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
        return true;
    };
    let snapshot = editor.display_map.snapshot();
    let buf_snap = snapshot.buffer_snapshot();
    let rope = buf_snap.rope().clone();
    let offset = crate::lsp::util::lsp_pos_to_byte_offset(&rope, entry.position, encoding);
    crate::action_handlers::movement::jump_to_offset(stoat, offset);
    true
}

/// Format response carried from the spawned task to
/// [`pump_lsp_format`]. Pairs the target document URI with the
/// returned text edits so the pump can build a single-document
/// [`WorkspaceEdit`].
#[derive(Debug, Clone)]
pub(crate) struct FormatResponse {
    pub(crate) uri: Uri,
    pub(crate) edits: Vec<TextEdit>,
}

/// Issue a `textDocument/rangeFormatting` request for the focused
/// editor's primary selection. The async response is stored on
/// [`Stoat::pending_format_request`] and applied by
/// [`pump_lsp_format`] on the next render tick.
///
/// No-op when the focused pane is not an editor or the buffer has no
/// path. When the server does not advertise
/// [`LanguageServerFeature::Format`], reports the language-server state
/// to the status bar instead.
pub(crate) fn format_selections(stoat: &mut Stoat) -> UpdateEffect {
    let (range_byte, buffer_id, source_rope) = {
        let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let start = buf_snap.resolve_anchor(&sel.start);
        let end = buf_snap.resolve_anchor(&sel.end);
        let (lo, hi) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        ((lo, hi), editor.buffer_id, buf_snap.rope().clone())
    };

    let Some((_, host)) = stoat
        .feature_hosts(buffer_id, LanguageServerFeature::Format)
        .into_iter()
        .next()
    else {
        return report_lsp_unavailable(stoat, "format");
    };
    let encoding = host.offset_encoding();

    let Some(source_path) = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)
    else {
        return UpdateEffect::None;
    };
    let Some(source_uri) = path_to_uri(&source_path) else {
        return UpdateEffect::None;
    };

    let lsp_range = crate::lsp::util::byte_range_to_lsp_range(
        &source_rope,
        range_byte.0..range_byte.1,
        encoding,
    );

    let params = DocumentRangeFormattingParams {
        text_document: TextDocumentIdentifier {
            uri: source_uri.clone(),
        },
        range: lsp_range,
        options: FormattingOptions::default(),
        work_done_progress_params: WorkDoneProgressParams::default(),
    };

    let task = stoat.spawn_woken(async move {
        match host.range_formatting(params).await {
            Ok(Some(edits)) if !edits.is_empty() => Some(FormatResponse {
                uri: source_uri,
                edits,
            }),
            Ok(_) => None,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "range_formatting request failed");
                None
            },
        }
    });
    stoat.pending_format_request = Some(task);
    UpdateEffect::None
}

/// Issue a `textDocument/formatting` request for the whole focused
/// document. The async response is stored on
/// [`Stoat::pending_format_request`] and applied by [`pump_lsp_format`]
/// on the next render tick, sharing the single-document apply path with
/// [`format_selections`].
///
/// No-op when the focused pane is not an editor or the buffer has no
/// path. When the server does not advertise
/// [`LanguageServerFeature::Format`], reports the language-server state
/// to the status bar instead.
pub(crate) fn format_document(stoat: &mut Stoat) -> UpdateEffect {
    let Some(buffer_id) = crate::action_handlers::focused_editor_mut(stoat).map(|e| e.buffer_id)
    else {
        return UpdateEffect::None;
    };
    let Some((_, host)) = stoat
        .feature_hosts(buffer_id, LanguageServerFeature::Format)
        .into_iter()
        .next()
    else {
        return report_lsp_unavailable(stoat, "format");
    };

    let Some(source_path) = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)
    else {
        return UpdateEffect::None;
    };
    let Some(source_uri) = path_to_uri(&source_path) else {
        return UpdateEffect::None;
    };

    let params = DocumentFormattingParams {
        text_document: TextDocumentIdentifier {
            uri: source_uri.clone(),
        },
        options: FormattingOptions {
            tab_size: 4,
            insert_spaces: true,
            ..FormattingOptions::default()
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };

    let task = stoat.spawn_woken(async move {
        match host.formatting(params).await {
            Ok(Some(edits)) if !edits.is_empty() => Some(FormatResponse {
                uri: source_uri,
                edits,
            }),
            Ok(_) => None,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "formatting request failed");
                None
            },
        }
    });
    stoat.pending_format_request = Some(task);
    UpdateEffect::None
}

/// Poll any in-flight format request and apply the returned text
/// edits as a single-document [`WorkspaceEdit`]. Errors from
/// [`crate::lsp::edit_apply::apply_workspace_edit`] are logged and
/// swallowed so a malformed edit does not crash the app.
pub(crate) fn pump_lsp_format(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_format_request.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(Some(FormatResponse { uri, edits })) => {
            #[allow(clippy::mutable_key_type)]
            let mut changes: std::collections::HashMap<Uri, Vec<TextEdit>> =
                std::collections::HashMap::new();
            changes.insert(uri, edits);
            let edit = WorkspaceEdit {
                changes: Some(changes),
                document_changes: None,
                change_annotations: None,
            };
            if let Err(err) = crate::lsp::edit_apply::apply_workspace_edit(stoat, edit) {
                tracing::warn!(
                    target: "stoat::lsp",
                    ?err,
                    "format text edit failed to apply",
                );
            }
            true
        },
        Poll::Ready(None) => true,
        Poll::Pending => {
            stoat.pending_format_request = Some(task);
            false
        },
    }
}

/// Poll any in-flight LSP jump request ([`Stoat::pending_lsp_jump`])
/// and dispatch on how many locations resolved. Zero locations reports
/// "lsp: no {label} found" in the status bar, naming the jump kind. One
/// jumps to it directly via [`apply_jump`]. Two or more
/// open a [`LocationPicker`] in [`Stoat::location_picker`] so the user
/// chooses. On `Pending` puts the task back. Returns true when state
/// changed so the caller can request a redraw.
pub(crate) fn pump_lsp_jumps(stoat: &mut Stoat) -> bool {
    let Some((label, mut task)) = stoat.pending_lsp_jump.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(mut entries) => {
            match entries.len() {
                0 => set_lsp_status(stoat, format!("lsp: no {label} found")),
                1 => {
                    let entry = entries.remove(0);
                    apply_jump(stoat, &entry.path, entry.offset);
                },
                _ => {
                    stoat.location_picker = Some(LocationPicker::new(entries));
                },
            }
            true
        },
        Poll::Pending => {
            stoat.pending_lsp_jump = Some((label, task));
            false
        },
    }
}

/// Open `path` in the focused pane and collapse every selection onto
/// `offset`. Opening is a no-op when the file is already the pane's
/// buffer.
///
/// A jump issued from a diff review parks the review session first so
/// the review editor survives the pane swap (the gc guard keeps parked
/// editors) and R re-enters the diff.
pub(crate) fn apply_jump(stoat: &mut Stoat, path: &Path, offset: usize) {
    let from_review =
        crate::action_handlers::focused_editor_mut(stoat).is_some_and(|e| e.review_view.is_some());
    if from_review {
        super::review::park_review_session(stoat);
        stoat.set_focused_mode("normal".to_string());
    } else {
        super::jump::push_jump(stoat);
    }

    let focused = stoat.active_workspace().panes.focus();
    super::file::open_file_in_pane(stoat, focused, path);
    super::movement::jump_to_offset(stoat, offset);
}

/// Convert an absolute filesystem path to an `lsp_types::Uri`. Returns
/// `None` for paths that cannot be encoded as a `file://` URI (e.g.
/// non-UTF-8 paths). Mirrors the production behaviour Helix uses
/// internally; LSP servers expect `file:` URIs for local files.
pub(crate) fn path_to_uri(path: &Path) -> Option<Uri> {
    let path_str = path.to_str()?;
    Uri::from_str(&format!("file://{path_str}")).ok()
}

#[cfg(test)]
mod tests {
    use crate::{
        agent_ipc::{AgentControl, AgentQuery},
        lsp::LspSymbolKind,
        test_harness::TestHarness,
    };
    use crossterm::event::{Event, KeyModifiers, MouseEvent, MouseEventKind};
    use futures::FutureExt;
    use lsp_types::TextDocumentSyncKind;
    use ratatui::style::Style;
    use std::{
        path::{Path, PathBuf},
        time::Duration,
    };
    use stoat_action::OpenFile;
    use tokio::sync::oneshot;

    fn seed(h: &mut TestHarness, files: &[(&str, &str)]) -> PathBuf {
        let root = PathBuf::from("/lsp-did-open-test");
        h.fake_fs().insert_files(
            files
                .iter()
                .map(|(rel, content)| (root.join(rel), content.as_bytes())),
        );
        h.stoat.active_workspace_mut().git_root = root.clone();
        root
    }

    #[test]
    fn did_open_dispatched_on_first_open() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("a.rs"),
            },
        );
        h.settle();
        let opens = h.fake_lsp().observed_opens();
        assert_eq!(opens.len(), 1, "expected exactly one did_open");
        assert!(opens[0].text_document.uri.as_str().ends_with("/a.rs"));
        assert_eq!(opens[0].text_document.text, "fn a() {}\n");
        assert_eq!(opens[0].text_document.language_id, "rust");
    }

    #[test]
    fn did_open_not_redispatched_on_reopen() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        for _ in 0..3 {
            crate::action_handlers::dispatch(
                &mut h.stoat,
                &OpenFile {
                    path: root.join("a.rs"),
                },
            );
            h.settle();
        }
        let opens = h.fake_lsp().observed_opens();
        assert_eq!(
            opens.len(),
            1,
            "did_open should fire exactly once per buffer lifetime"
        );
    }

    #[test]
    fn auto_spawn_skipped_when_a_real_host_is_installed() {
        let mut h = TestHarness::with_size(80, 24);
        h.stoat.set_lsp_auto_spawn(true);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        assert!(
            !h.stoat.lsp_registry.spawn_attempted_any(),
            "FakeLsp is a non-noop host, so opening a rust buffer attempts no spawn",
        );
    }

    #[test]
    fn lsp_spawn_defers_while_env_loading() {
        let mut h = TestHarness::with_size(80, 24);
        h.allow_host_swap();
        h.stoat
            .set_lsp_host(std::sync::Arc::new(crate::host::NoopLsp));
        h.stoat.set_lsp_auto_spawn(true);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        h.stoat.active_workspace_mut().env.state = crate::project_env::EnvLoadState::Loading;

        open_buffer(&mut h, root.join("a.rs"));

        assert!(
            !h.stoat.lsp_registry.spawn_attempted_any(),
            "the spawn is deferred, not attempted, while the env loads",
        );
        let buffer_id = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(&root.join("a.rs"));
        assert!(buffer_id.is_some());
        assert_eq!(h.stoat.lsp_spawn_deferred, buffer_id);
    }

    #[test]
    fn env_install_consumes_lsp_deferral() {
        let mut h = TestHarness::with_size(80, 24);
        h.allow_host_swap();
        h.stoat
            .set_lsp_host(std::sync::Arc::new(crate::host::NoopLsp));
        h.stoat.set_lsp_auto_spawn(true);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        let ws_id = h.stoat.active_workspace;
        h.stoat.active_workspace_mut().env.state = crate::project_env::EnvLoadState::Loading;
        open_buffer(&mut h, root.join("a.rs"));
        assert!(h.stoat.lsp_spawn_deferred.is_some());

        // Install a real host so the re-fired spawn is gated at the noop check,
        // keeping the test free of a real language-server process, then land
        // the env.
        h.stoat
            .set_lsp_host(std::sync::Arc::new(crate::host::FakeLsp::new()));
        *h.stoat.pending_env.lock().expect("pending env mutex") =
            Some(crate::project_env::PendingEnvLoad {
                workspace: ws_id,
                manual: false,
                outcome: Ok(Vec::new()),
            });
        crate::project_env::install_pending(&mut h.stoat);

        assert_eq!(
            h.stoat.lsp_spawn_deferred, None,
            "install consumes the deferral"
        );
        assert_eq!(
            h.stoat.active_workspace().env.state,
            crate::project_env::EnvLoadState::Loaded,
        );
    }

    /// Bring up the in-process stcfg server against an empty `config.stcfg`.
    ///
    /// Resets the registry so no injected sole host suppresses auto-spawn, opens
    /// the file (queuing the in-process spawn), then drives one `update` so the
    /// parked host installs. Returns the file path.
    fn open_stcfg_with_server(h: &mut TestHarness) -> PathBuf {
        h.stoat.lsp_registry = crate::lsp::registry::LspRegistry::new();
        h.stoat.set_lsp_auto_spawn(true);

        let root = PathBuf::from("/cfg");
        let path = root.join("config.stcfg");
        h.fake_fs()
            .insert_files(std::iter::once((path.clone(), b"".as_slice())));
        h.stoat.active_workspace_mut().git_root = root;

        open_buffer(h, path.clone());
        h.type_keys("i");
        path
    }

    #[test]
    fn stcfg_buffer_completes_settings_via_in_process_server() {
        use crate::completion::{request::COMPLETION_DEBOUNCE, CompletionSource};

        let mut h = TestHarness::with_size(80, 24);
        h.allow_host_swap();
        open_stcfg_with_server(&mut h);

        h.type_text("on init { form");

        // did_change (50ms) syncs the buffer to the server before the completion
        // request (150ms) reads it.
        h.advance_clock(super::LSP_DID_CHANGE_DEBOUNCE);
        h.advance_clock(COMPLETION_DEBOUNCE);

        let popup = h
            .stoat
            .pending_completion
            .clone()
            .expect("completion popup armed");
        let format_item = popup
            .items
            .iter()
            .find(|item| item.label == "format_on_save")
            .expect("in-process stcfg server offers format_on_save");
        assert_eq!(format_item.source, CompletionSource::Lsp);
    }

    #[test]
    fn stcfg_buffer_reports_syntax_error_diagnostics() {
        use lsp_types::DiagnosticSeverity;

        let mut h = TestHarness::with_size(80, 24);
        h.allow_host_swap();
        let path = open_stcfg_with_server(&mut h);

        h.type_text("on init { format_on_save = ");

        // did_change (50ms) syncs before the pull-diagnostics request (300ms).
        h.advance_clock(super::LSP_DID_CHANGE_DEBOUNCE);
        h.advance_clock(Duration::from_millis(300));

        let diagnostics: Vec<_> = h
            .stoat
            .diagnostics
            .iter()
            .find(|(diag_path, _)| *diag_path == path)
            .map(|(_, diags)| diags.to_vec())
            .expect("diagnostics recorded for config.stcfg");
        assert!(
            diagnostics
                .iter()
                .any(|diag| diag.severity == Some(DiagnosticSeverity::ERROR)),
            "expected a syntax-error diagnostic, got {diagnostics:?}",
        );
    }

    #[test]
    fn each_language_routes_did_open_to_its_own_server() {
        let mut h = TestHarness::with_size(80, 24);
        let rust_server = std::sync::Arc::new(crate::host::FakeLsp::new());
        let json_server = std::sync::Arc::new(crate::host::FakeLsp::new());
        h.stoat
            .lsp_registry
            .insert("rust-analyzer".into(), rust_server.clone());
        h.stoat
            .lsp_registry
            .set_language("rust".into(), "rust-analyzer".into());
        h.stoat
            .lsp_registry
            .insert("json-ls".into(), json_server.clone());
        h.stoat
            .lsp_registry
            .set_language("json".into(), "json-ls".into());

        let root = seed(&mut h, &[("a.rs", "fn a() {}\n"), ("b.json", "{}\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        open_buffer(&mut h, root.join("b.json"));

        let rust_opens = rust_server.observed_opens();
        assert_eq!(rust_opens.len(), 1, "rust server sees only the rust file");
        assert!(rust_opens[0].text_document.uri.as_str().ends_with("/a.rs"));

        let json_opens = json_server.observed_opens();
        assert_eq!(json_opens.len(), 1, "json server sees only the json file");
        assert!(json_opens[0]
            .text_document
            .uri
            .as_str()
            .ends_with("/b.json"));
    }

    #[test]
    fn lsp_for_feature_routes_to_the_capable_server() {
        use crate::{
            host::{LanguageServerFeature, LspHost},
            lsp::registry::ServerSelector,
        };
        use lsp_types::{CompletionOptions, HoverProviderCapability, ServerCapabilities};

        let mut h = TestHarness::with_size(80, 24);
        let hover_server = std::sync::Arc::new(crate::host::FakeLsp::new());
        hover_server.set_capabilities(ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            ..ServerCapabilities::default()
        });
        let completion_server = std::sync::Arc::new(crate::host::FakeLsp::new());
        completion_server.set_capabilities(ServerCapabilities {
            completion_provider: Some(CompletionOptions::default()),
            ..ServerCapabilities::default()
        });
        h.stoat
            .lsp_registry
            .insert("primary".into(), hover_server.clone());
        h.stoat
            .lsp_registry
            .insert("tailwind".into(), completion_server.clone());
        h.stoat.lsp_registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("primary".into()),
                ServerSelector::all("tailwind".into()),
            ],
        );

        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        let id = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(&root.join("a.rs"))
            .expect("buffer open");

        let hover: std::sync::Arc<dyn LspHost> = hover_server.clone();
        let completion: std::sync::Arc<dyn LspHost> = completion_server.clone();
        assert!(
            std::sync::Arc::ptr_eq(
                &h.stoat.lsp_for_feature(id, LanguageServerFeature::Hover),
                &hover,
            ),
            "hover routes to the hover-capable server"
        );
        assert!(
            std::sync::Arc::ptr_eq(
                &h.stoat
                    .lsp_for_feature(id, LanguageServerFeature::Completion),
                &completion,
            ),
            "completion routes to the completion-capable server"
        );
    }

    #[test]
    fn hover_routes_to_a_secondary_when_the_primary_lacks_it() {
        use crate::lsp::registry::ServerSelector;
        use lsp_types::{HoverProviderCapability, ServerCapabilities};

        let mut h = TestHarness::with_size(80, 24);
        let primary = std::sync::Arc::new(crate::host::FakeLsp::new());
        primary.set_capabilities(ServerCapabilities::default());
        let secondary = std::sync::Arc::new(crate::host::FakeLsp::new());
        secondary.set_capabilities(ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            ..ServerCapabilities::default()
        });
        h.stoat
            .lsp_registry
            .insert("primary".into(), primary.clone());
        h.stoat
            .lsp_registry
            .insert("secondary".into(), secondary.clone());
        h.stoat.lsp_registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("primary".into()),
                ServerSelector::all("secondary".into()),
            ],
        );

        let root = seed(&mut h, &[("a.rs", "abc\ndef\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        secondary.set_hover(path.to_str().unwrap(), 0, 0, "from secondary");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        let popup = h
            .stoat
            .pending_hover
            .as_ref()
            .expect("the hover-capable secondary answered");
        assert_eq!(
            popup.lines,
            vec![vec![("from secondary".to_string(), Style::default())]]
        );
        assert_ne!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: server does not support hover"),
            "a capable secondary means the noop sole host never gates hover out"
        );
    }

    #[test]
    fn hover_position_encodes_with_the_routed_host() {
        use crate::{host::OffsetEncoding, lsp::registry::ServerSelector};
        use lsp_types::{HoverProviderCapability, ServerCapabilities};

        let mut h = TestHarness::with_size(80, 24);
        let primary = std::sync::Arc::new(crate::host::FakeLsp::new());
        primary.set_capabilities(ServerCapabilities::default());
        let secondary = std::sync::Arc::new(crate::host::FakeLsp::new());
        secondary.set_capabilities(ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            ..ServerCapabilities::default()
        });
        secondary.set_offset_encoding(OffsetEncoding::Utf8);
        h.stoat
            .lsp_registry
            .insert("primary".into(), primary.clone());
        h.stoat
            .lsp_registry
            .insert("secondary".into(), secondary.clone());
        h.stoat.lsp_registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("primary".into()),
                ServerSelector::all("secondary".into()),
            ],
        );

        // The cursor sits after a 2-byte char, so the LSP column is 2 under UTF-8
        // and 1 under UTF-16. Placing the hover at column 2 means the popup only
        // appears when the position encodes with the routed host's UTF-8, not the
        // noop sole host's default UTF-16.
        let root = seed(&mut h, &[("a.rs", "\u{e9}x\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, 2);
        secondary.set_hover(path.to_str().unwrap(), 0, 2, "utf8 routed");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        let popup = h
            .stoat
            .pending_hover
            .as_ref()
            .expect("hover position encoded with the routed host's UTF-8");
        assert_eq!(
            popup.lines,
            vec![vec![("utf8 routed".to_string(), Style::default())]]
        );
    }

    /// Install two hover-capable fakes routed primary-then-secondary for `rust`,
    /// open a buffer, and return the fakes and its path.
    fn two_hover_servers(
        h: &mut TestHarness,
    ) -> (
        std::sync::Arc<crate::host::FakeLsp>,
        std::sync::Arc<crate::host::FakeLsp>,
        PathBuf,
    ) {
        use crate::lsp::registry::ServerSelector;
        use lsp_types::{HoverProviderCapability, ServerCapabilities};

        let caps = ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            ..ServerCapabilities::default()
        };
        let primary = std::sync::Arc::new(crate::host::FakeLsp::new());
        primary.set_capabilities(caps.clone());
        let secondary = std::sync::Arc::new(crate::host::FakeLsp::new());
        secondary.set_capabilities(caps);
        h.stoat
            .lsp_registry
            .insert("primary".into(), primary.clone());
        h.stoat
            .lsp_registry
            .insert("secondary".into(), secondary.clone());
        h.stoat.lsp_registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("primary".into()),
                ServerSelector::all("secondary".into()),
            ],
        );

        let root = seed(h, &[("a.rs", "abc\ndef\n")]);
        let path = root.join("a.rs");
        open_buffer(h, path.clone());
        (primary, secondary, path)
    }

    fn hover_body(h: &TestHarness) -> String {
        h.stoat
            .pending_hover
            .as_ref()
            .expect("hover popup")
            .lines
            .iter()
            .map(|line| line.iter().map(|(t, _)| t.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn hover_merges_sections_from_two_servers() {
        let mut h = TestHarness::with_size(80, 24);
        let (primary, secondary, path) = two_hover_servers(&mut h);
        let p = path.to_str().unwrap();
        primary.set_hover(p, 0, 0, "alpha docs");
        secondary.set_hover(p, 0, 0, "beta docs");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        let body = hover_body(&h);
        for needle in ["primary", "secondary", "alpha docs", "beta docs"] {
            assert!(
                body.contains(needle),
                "merged hover missing {needle:?}: {body:?}"
            );
        }
    }

    #[test]
    fn hover_omits_the_header_when_only_one_server_answers() {
        let mut h = TestHarness::with_size(80, 24);
        let (_primary, secondary, path) = two_hover_servers(&mut h);
        secondary.set_hover(path.to_str().unwrap(), 0, 0, "from secondary");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        let popup = h
            .stoat
            .pending_hover
            .as_ref()
            .expect("the content section is shown");
        assert_eq!(
            popup.lines,
            vec![vec![("from secondary".to_string(), Style::default())]],
            "a lone responder renders unheaded"
        );
    }

    #[test]
    fn merge_hovers_single_section_passes_through_unheaded() {
        assert_eq!(
            super::merge_hovers(vec![("ra".into(), "hello".into(), false)]),
            ("hello".to_string(), false)
        );
    }

    #[test]
    fn merge_hovers_joins_sections_in_routing_order_with_headers() {
        assert_eq!(
            super::merge_hovers(vec![
                ("ra".into(), "A".into(), false),
                ("ty".into(), "B".into(), false),
            ]),
            ("**ra**\n\nA\n\n---\n\n**ty**\n\nB".to_string(), false)
        );
    }

    #[test]
    fn merge_hovers_is_plain_only_when_every_section_is_plain() {
        assert!(super::merge_hovers(vec![("a".into(), "x".into(), true)]).1);
        assert!(
            !super::merge_hovers(vec![
                ("a".into(), "x".into(), true),
                ("b".into(), "y".into(), false),
            ])
            .1
        );
    }

    #[test]
    fn goto_definition_routes_to_a_secondary_when_the_primary_lacks_it() {
        use crate::lsp::registry::ServerSelector;
        use lsp_types::{OneOf, ServerCapabilities};

        let mut h = TestHarness::with_size(80, 24);
        let primary = std::sync::Arc::new(crate::host::FakeLsp::new());
        primary.set_capabilities(ServerCapabilities::default());
        let secondary = std::sync::Arc::new(crate::host::FakeLsp::new());
        secondary.set_capabilities(ServerCapabilities {
            definition_provider: Some(OneOf::Left(true)),
            ..ServerCapabilities::default()
        });
        h.stoat
            .lsp_registry
            .insert("primary".into(), primary.clone());
        h.stoat
            .lsp_registry
            .insert("secondary".into(), secondary.clone());
        h.stoat.lsp_registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("primary".into()),
                ServerSelector::all("secondary".into()),
            ],
        );

        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        secondary.set_definition(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
        h.settle();

        assert_eq!(
            cursor_offset(&mut h),
            8,
            "the capable secondary served goto-definition"
        );
    }

    /// Install two definition-capable fakes routed primary-then-secondary for
    /// `rust`, open a three-line buffer, and return the fakes and its path.
    fn two_definition_servers(
        h: &mut TestHarness,
    ) -> (
        std::sync::Arc<crate::host::FakeLsp>,
        std::sync::Arc<crate::host::FakeLsp>,
        PathBuf,
    ) {
        use crate::lsp::registry::ServerSelector;
        use lsp_types::{OneOf, ServerCapabilities};

        let caps = ServerCapabilities {
            definition_provider: Some(OneOf::Left(true)),
            ..ServerCapabilities::default()
        };
        let primary = std::sync::Arc::new(crate::host::FakeLsp::new());
        primary.set_capabilities(caps.clone());
        let secondary = std::sync::Arc::new(crate::host::FakeLsp::new());
        secondary.set_capabilities(caps);
        h.stoat
            .lsp_registry
            .insert("primary".into(), primary.clone());
        h.stoat
            .lsp_registry
            .insert("secondary".into(), secondary.clone());
        h.stoat.lsp_registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("primary".into()),
                ServerSelector::all("secondary".into()),
            ],
        );

        let root = seed(h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(h, path.clone());
        (primary, secondary, path)
    }

    #[test]
    fn goto_definition_merges_distinct_locations_from_two_servers() {
        let mut h = TestHarness::with_size(80, 24);
        let (primary, secondary, path) = two_definition_servers(&mut h);
        let p = path.to_str().unwrap();
        primary.set_definition(p, 0, 0, p, 1, 0);
        secondary.set_definition(p, 0, 0, p, 2, 0);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
        h.settle();

        let picker = h.stoat.location_picker.as_ref().expect("picker open");
        let offsets: Vec<usize> = picker.entries().iter().map(|e| e.offset).collect();
        assert_eq!(offsets, vec![4, 8], "both servers' targets, primary first");
    }

    #[test]
    fn goto_definition_dedups_a_shared_location() {
        let mut h = TestHarness::with_size(80, 24);
        let (primary, secondary, path) = two_definition_servers(&mut h);
        let p = path.to_str().unwrap();
        primary.set_definition(p, 0, 0, p, 2, 0);
        secondary.set_definition(p, 0, 0, p, 2, 0);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
        h.settle();

        assert!(
            h.stoat.location_picker.is_none(),
            "identical answers dedup to a single direct jump"
        );
        assert_eq!(cursor_offset(&mut h), 8);
    }

    #[test]
    fn goto_definition_survives_a_failing_server() {
        let mut h = TestHarness::with_size(80, 24);
        let (primary, secondary, path) = two_definition_servers(&mut h);
        let p = path.to_str().unwrap();
        primary.set_method_error("textDocument/definition", std::io::ErrorKind::Other);
        secondary.set_definition(p, 0, 0, p, 2, 0);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
        h.settle();

        assert!(h.stoat.location_picker.is_none());
        assert_eq!(
            cursor_offset(&mut h),
            8,
            "the healthy server's answer lands despite the peer erroring"
        );
    }

    #[test]
    fn workspace_symbol_submit_uses_the_host_stashed_at_open() {
        use crate::lsp::registry::ServerSelector;
        use lsp_types::{OneOf, ServerCapabilities, SymbolKind};

        let mut h = TestHarness::with_size(80, 24);
        let capable = std::sync::Arc::new(crate::host::FakeLsp::new());
        capable.set_capabilities(ServerCapabilities {
            workspace_symbol_provider: Some(OneOf::Left(true)),
            ..ServerCapabilities::default()
        });
        let other = std::sync::Arc::new(crate::host::FakeLsp::new());
        other.set_capabilities(ServerCapabilities::default());
        h.stoat
            .lsp_registry
            .insert("capable".into(), capable.clone());
        h.stoat.lsp_registry.insert("other".into(), other.clone());
        h.stoat.lsp_registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("capable".into()),
                ServerSelector::all("other".into()),
            ],
        );

        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let main = root.join("main.rs");
        open_buffer(&mut h, main.clone());
        capable.add_workspace_symbol(
            "",
            "foo",
            SymbolKind::FUNCTION,
            main.to_str().unwrap(),
            0,
            3,
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();

        // The capable server drops workspace symbols mid-query, so a
        // re-resolution at submit would find no capable server. Submit must
        // still target the server stashed at open, resolved by name.
        capable.set_capabilities(ServerCapabilities::default());
        crate::action_handlers::lsp::workspace_symbol_submit(&mut h.stoat);
        h.settle();

        let picker = h
            .stoat
            .pending_workspace_symbol_picker
            .as_ref()
            .expect("picker filled from the stashed server");
        let titles: Vec<&str> = picker.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["foo"]);
    }

    #[test]
    fn did_open_falls_back_to_plaintext_when_no_language() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("note.txt", "hello\n")]);
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("note.txt"),
            },
        );
        h.settle();
        let opens = h.fake_lsp().observed_opens();
        assert_eq!(opens.len(), 1);
        assert_eq!(opens[0].text_document.language_id, "plaintext");
    }

    #[test]
    fn did_open_separate_files_each_dispatch() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "x\n"), ("b.rs", "y\n")]);
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("a.rs"),
            },
        );
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("b.rs"),
            },
        );
        h.settle();
        let opens = h.fake_lsp().observed_opens();
        assert_eq!(opens.len(), 2);
    }

    fn open_buffer(h: &mut TestHarness, path: PathBuf) {
        crate::action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
    }

    fn edit_buffer(h: &mut TestHarness, range: std::ops::Range<usize>, text: &str) {
        h.edit_focused(range, text);
    }

    fn arm_change(h: &mut TestHarness) {
        super::notify_buffer_changes_pending(&mut h.stoat);
    }

    #[test]
    fn did_change_fires_after_debounce_window() {
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp()
            .set_text_document_sync(TextDocumentSyncKind::FULL);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        edit_buffer(&mut h, 0..0, "// hi\n");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        let changes = h.fake_lsp().observed_changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].text_document.version, 1);
        assert_eq!(changes[0].content_changes.len(), 1);
        assert_eq!(changes[0].content_changes[0].range, None);
        assert_eq!(changes[0].content_changes[0].text, "// hi\nfn a() {}\n");
    }

    #[test]
    fn did_change_coalesces_rapid_edits() {
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp()
            .set_text_document_sync(TextDocumentSyncKind::FULL);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        edit_buffer(&mut h, 0..0, "//1\n");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(20));
        edit_buffer(&mut h, 0..0, "//2\n");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        let changes = h.fake_lsp().observed_changes();
        assert_eq!(changes.len(), 1, "second edit must cancel the first timer");
        assert_eq!(changes[0].content_changes[0].text, "//2\n//1\nfn a() {}\n");
    }

    #[test]
    fn did_change_skipped_when_sync_kind_is_none() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        edit_buffer(&mut h, 0..0, "// hi\n");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        assert!(h.fake_lsp().observed_changes().is_empty());
    }

    #[test]
    fn did_change_shapes_params_per_host_sync_kind() {
        use crate::lsp::registry::ServerSelector;

        let mut h = TestHarness::with_size(80, 24);
        let full = std::sync::Arc::new(crate::host::FakeLsp::new());
        full.set_text_document_sync(TextDocumentSyncKind::FULL);
        let incremental = std::sync::Arc::new(crate::host::FakeLsp::new());
        incremental.set_text_document_sync(TextDocumentSyncKind::INCREMENTAL);
        h.stoat.lsp_registry.insert("full".into(), full.clone());
        h.stoat
            .lsp_registry
            .insert("incremental".into(), incremental.clone());
        h.stoat.lsp_registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("full".into()),
                ServerSelector::all("incremental".into()),
            ],
        );

        let root = seed(&mut h, &[("a.rs", "abc\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        edit_buffer(&mut h, 0..0, "X");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));

        let full_changes = full.observed_changes();
        assert_eq!(full_changes.len(), 1);
        assert_eq!(
            full_changes[0].content_changes[0].range, None,
            "the FULL host receives whole-document text"
        );
        assert_eq!(full_changes[0].content_changes[0].text, "Xabc\n");

        let inc_changes = incremental.observed_changes();
        assert_eq!(inc_changes.len(), 1);
        assert_eq!(
            inc_changes[0].content_changes[0].range,
            Some(lsp_types::Range::new(
                lsp_types::Position::new(0, 0),
                lsp_types::Position::new(0, 0),
            )),
            "the INCREMENTAL host receives a ranged change for the same edit"
        );
        assert_eq!(inc_changes[0].content_changes[0].text, "X");
    }

    #[test]
    fn did_change_reaches_the_full_host_past_a_sync_none_peer() {
        use crate::lsp::registry::ServerSelector;

        let mut h = TestHarness::with_size(80, 24);
        // No sync set defaults to NONE.
        let none = std::sync::Arc::new(crate::host::FakeLsp::new());
        let full = std::sync::Arc::new(crate::host::FakeLsp::new());
        full.set_text_document_sync(TextDocumentSyncKind::FULL);
        h.stoat.lsp_registry.insert("none".into(), none.clone());
        h.stoat.lsp_registry.insert("full".into(), full.clone());
        h.stoat.lsp_registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("none".into()),
                ServerSelector::all("full".into()),
            ],
        );

        let root = seed(&mut h, &[("a.rs", "abc\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        edit_buffer(&mut h, 0..0, "X");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));

        let full_changes = full.observed_changes();
        assert_eq!(
            full_changes.len(),
            1,
            "the FULL host gets the change even though a sync-NONE peer sorts first"
        );
        assert_eq!(full_changes[0].content_changes[0].text, "Xabc\n");
        assert!(
            none.observed_changes().is_empty(),
            "the sync-NONE host takes no change"
        );
    }

    #[test]
    fn did_change_independent_per_buffer() {
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp()
            .set_text_document_sync(TextDocumentSyncKind::FULL);
        let root = seed(&mut h, &[("a.rs", "x\n"), ("b.rs", "y\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        edit_buffer(&mut h, 0..0, "A");
        open_buffer(&mut h, root.join("b.rs"));
        edit_buffer(&mut h, 0..0, "B");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        let mut changes = h.fake_lsp().observed_changes();
        changes.sort_by(|a, b| {
            a.text_document
                .uri
                .as_str()
                .cmp(b.text_document.uri.as_str())
        });
        assert_eq!(changes.len(), 2);
        assert!(changes[0].text_document.uri.as_str().ends_with("/a.rs"));
        assert_eq!(changes[0].content_changes[0].text, "Ax\n");
        assert!(changes[1].text_document.uri.as_str().ends_with("/b.rs"));
        assert_eq!(changes[1].content_changes[0].text, "By\n");
    }

    #[test]
    fn did_change_incremental_single_insertion() {
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp()
            .set_text_document_sync(TextDocumentSyncKind::INCREMENTAL);
        let root = seed(&mut h, &[("a.rs", "abc\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        edit_buffer(&mut h, 0..0, "X");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        let changes = h.fake_lsp().observed_changes();
        assert_eq!(changes.len(), 1);
        let cc = &changes[0].content_changes;
        assert_eq!(cc.len(), 1, "single insertion -> single content_change");
        assert_eq!(cc[0].text, "X");
        assert_eq!(
            cc[0].range,
            Some(lsp_types::Range::new(
                lsp_types::Position::new(0, 0),
                lsp_types::Position::new(0, 0),
            )),
        );
    }

    #[test]
    fn did_change_incremental_single_deletion() {
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp()
            .set_text_document_sync(TextDocumentSyncKind::INCREMENTAL);
        let root = seed(&mut h, &[("a.rs", "abc\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        edit_buffer(&mut h, 1..2, "");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        let changes = h.fake_lsp().observed_changes();
        assert_eq!(changes.len(), 1);
        let cc = &changes[0].content_changes;
        assert_eq!(cc.len(), 1);
        assert_eq!(cc[0].text, "");
        assert_eq!(
            cc[0].range,
            Some(lsp_types::Range::new(
                lsp_types::Position::new(0, 1),
                lsp_types::Position::new(0, 2),
            )),
        );
    }

    #[test]
    fn did_change_incremental_reverts_typed_text_after_undo() {
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp()
            .set_text_document_sync(TextDocumentSyncKind::INCREMENTAL);
        let root = seed(&mut h, &[("a.rs", "abc\n")]);
        open_buffer(&mut h, root.join("a.rs"));

        edit_buffer(&mut h, 0..0, "X");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Undo);
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));

        let changes = h.fake_lsp().observed_changes();
        assert_eq!(
            changes.len(),
            2,
            "the undo sends its own incremental change"
        );
        let cc = &changes[1].content_changes;
        assert_eq!(cc.len(), 1, "reverting the insertion is one deletion");
        assert_eq!(cc[0].text, "");
        assert_eq!(
            cc[0].range,
            Some(lsp_types::Range::new(
                lsp_types::Position::new(0, 0),
                lsp_types::Position::new(0, 1),
            )),
            "the deletion covers the typed X",
        );
    }

    #[test]
    fn did_change_incremental_subsequent_dispatch_starts_from_last_delivered() {
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp()
            .set_text_document_sync(TextDocumentSyncKind::INCREMENTAL);
        let root = seed(&mut h, &[("a.rs", "abc\n")]);
        open_buffer(&mut h, root.join("a.rs"));

        edit_buffer(&mut h, 0..0, "X");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        let after_first = h.fake_lsp().observed_changes();
        assert_eq!(after_first.len(), 1);
        assert!(after_first[0].content_changes.iter().any(|c| c.text == "X"));

        edit_buffer(&mut h, 4..4, "Z");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        let all = h.fake_lsp().observed_changes();
        assert_eq!(all.len(), 2);
        let second = &all[1];
        for change in &second.content_changes {
            assert_ne!(
                change.text, "X",
                "second dispatch must not redeliver the prior insertion",
            );
        }
        assert_eq!(second.content_changes.len(), 1);
        assert_eq!(second.content_changes[0].text, "Z");
        assert_eq!(
            second.content_changes[0].range,
            Some(lsp_types::Range::new(
                lsp_types::Position::new(0, 4),
                lsp_types::Position::new(0, 4),
            )),
        );
    }

    #[test]
    fn did_change_incremental_skips_when_buffer_already_at_delivered_state() {
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp()
            .set_text_document_sync(TextDocumentSyncKind::INCREMENTAL);
        let root = seed(&mut h, &[("a.rs", "abc\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        edit_buffer(&mut h, 0..0, "X");
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        let baseline = h.fake_lsp().observed_changes().len();
        arm_change(&mut h);
        h.advance_clock(Duration::from_millis(60));
        assert_eq!(
            h.fake_lsp().observed_changes().len(),
            baseline,
            "no edit since last delivery -> no new dispatch",
        );
    }

    fn diag(line: u32, col: u32, message: &str) -> lsp_types::Diagnostic {
        use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
        Diagnostic {
            range: Range::new(Position::new(line, col), Position::new(line, col + 1)),
            severity: Some(DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: None,
            message: message.to_string(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    #[test]
    fn drive_background_applies_pushed_diagnostics() {
        use crate::host::lsp::LspNotification;
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\n")]);
        let path = root.join("main.rs");
        let uri = super::path_to_uri(&path).expect("file uri");
        h.fake_lsp()
            .push_notification(LspNotification::Diagnostics {
                uri,
                diagnostics: vec![diag(0, 0, "boom")],
                version: None,
            });

        // No input event and no settle(): the background pass alone (the
        // redraw-wake path) must drain the pushed notification and apply it.
        h.stoat.drive_background();

        assert_eq!(h.stoat.diagnostics.get(&path), &[diag(0, 0, "boom")]);
    }

    fn cursor_offset(h: &mut TestHarness) -> usize {
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        stoat_text::cursor_offset(
            buffer_snapshot.rope(),
            buffer_snapshot.resolve_anchor(&sel.tail()),
            buffer_snapshot.resolve_anchor(&sel.head()),
        )
    }

    #[test]
    fn goto_next_diagnostic_jumps_forward() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.stoat
            .diagnostics
            .replace_for_path(path, vec![diag(1, 0, "first"), diag(2, 0, "second")]);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
        assert_eq!(cursor_offset(&mut h), 4);
    }

    #[test]
    fn goto_next_diagnostic_steps_through_each() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.stoat
            .diagnostics
            .replace_for_path(path, vec![diag(1, 0, "first"), diag(2, 0, "second")]);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
        assert_eq!(cursor_offset(&mut h), 4);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
        assert_eq!(cursor_offset(&mut h), 8);
    }

    #[test]
    fn goto_diagnostic_converts_each_servers_position_with_its_encoding() {
        use crate::host::OffsetEncoding;
        let mut h = TestHarness::with_size(80, 24);
        let ra = std::sync::Arc::new(crate::host::FakeLsp::new());
        ra.set_offset_encoding(OffsetEncoding::Utf8);
        let clippy = std::sync::Arc::new(crate::host::FakeLsp::new());
        clippy.set_offset_encoding(OffsetEncoding::Utf16);
        h.stoat.lsp_registry.insert("ra".into(), ra);
        h.stoat.lsp_registry.insert("clippy".into(), clippy);

        // Line 0 "éx" and line 1 "éy": é is two UTF-8 bytes but one UTF-16 unit,
        // so x sits at byte 2 and y at byte 6.
        let root = seed(&mut h, &[("a.rs", "\u{e9}x\n\u{e9}y\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());

        // ra (utf-8) names x at char 2; clippy (utf-16) names y at char 1.
        h.stoat
            .diagnostics
            .replace_from_server(path.clone(), "ra".into(), vec![diag(0, 2, "ra")]);
        h.stoat
            .diagnostics
            .replace_from_server(path, "clippy".into(), vec![diag(1, 1, "clippy")]);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
        assert_eq!(cursor_offset(&mut h), 2, "ra's utf-8 column lands on x");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
        assert_eq!(
            cursor_offset(&mut h),
            6,
            "clippy's utf-16 column lands on y"
        );
    }

    #[test]
    fn goto_next_diagnostic_no_op_after_last() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.stoat
            .diagnostics
            .replace_for_path(path, vec![diag(0, 0, "only")]);
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, 11);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
        assert_eq!(cursor_offset(&mut h), 11);
    }

    #[test]
    fn goto_prev_diagnostic_jumps_backward() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.stoat
            .diagnostics
            .replace_for_path(path, vec![diag(0, 0, "first"), diag(2, 0, "third")]);
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, 11);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoPrevDiagnostic);
        assert_eq!(cursor_offset(&mut h), 8);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoPrevDiagnostic);
        assert_eq!(cursor_offset(&mut h), 0);
    }

    #[test]
    fn goto_prev_diagnostic_no_op_before_first() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.stoat
            .diagnostics
            .replace_for_path(path, vec![diag(2, 0, "only")]);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoPrevDiagnostic);
        assert_eq!(cursor_offset(&mut h), 0);
    }

    #[test]
    fn diagnostics_picker_enter_jumps_focused_cursor() {
        use crate::test_harness::keys;
        use crossterm::event::{Event, KeyCode};
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.stoat
            .diagnostics
            .replace_for_path(path, vec![diag(1, 0, "first"), diag(2, 0, "second")]);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenDiagnosticsPicker);
        assert!(h.stoat.diagnostics_picker.is_some());

        h.stoat.update(Event::Key(keys::key(KeyCode::Down)));
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        assert!(h.stoat.diagnostics_picker.is_none());
        assert_eq!(cursor_offset(&mut h), 8);
    }

    #[test]
    fn diagnostics_picker_esc_closes_without_jumping() {
        use crate::test_harness::keys;
        use crossterm::event::{Event, KeyCode};
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.stoat
            .diagnostics
            .replace_for_path(path, vec![diag(1, 0, "first")]);
        let before = cursor_offset(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenDiagnosticsPicker);
        h.stoat.update(Event::Key(keys::key(KeyCode::Esc)));
        assert!(h.stoat.diagnostics_picker.is_none());
        assert_eq!(cursor_offset(&mut h), before);
    }

    #[test]
    fn goto_diagnostic_no_op_with_empty_diagnostics() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\n")]);
        open_buffer(&mut h, root.join("a.rs"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
        assert_eq!(cursor_offset(&mut h), 0);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoPrevDiagnostic);
        assert_eq!(cursor_offset(&mut h), 0);
    }

    #[test]
    fn space_l_w_jumps_to_next_diagnostic() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.stoat
            .diagnostics
            .replace_for_path(path, vec![diag(1, 0, "first"), diag(2, 0, "second")]);
        h.type_keys("space l w");
        assert_eq!(cursor_offset(&mut h), 4);
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn space_l_shift_w_jumps_to_prev_diagnostic() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        h.stoat
            .diagnostics
            .replace_for_path(path, vec![diag(0, 0, "first"), diag(2, 0, "third")]);
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, 11);
        h.type_keys("space l shift-w");
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    fn enable_goto_definition(h: &TestHarness) {
        use lsp_types::{OneOf, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            definition_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
    }

    fn enable_goto_references(h: &TestHarness) {
        use lsp_types::{OneOf, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            references_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
    }

    fn focused_buffer_path(h: &TestHarness) -> PathBuf {
        let ws = h.stoat.active_workspace();
        let pane = ws.panes.pane(ws.panes.focus());
        let crate::pane::View::Editor(eid) = pane.view else {
            panic!("focused pane is not an editor");
        };
        let buffer_id = ws.editors.get(eid).expect("editor").buffer_id;
        ws.buffers
            .path_for(buffer_id)
            .expect("focused buffer has path")
            .to_path_buf()
    }

    #[test]
    fn goto_definition_jumps_within_same_file() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_definition(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_definition(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
        h.settle();
        assert!(
            h.stoat.location_picker.is_none(),
            "single target skips picker"
        );
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(focused_buffer_path(&h), path);
    }

    fn enable_goto_declaration(h: &TestHarness) {
        use lsp_types::{DeclarationCapability, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            declaration_provider: Some(DeclarationCapability::Simple(true)),
            ..Default::default()
        });
    }

    #[test]
    fn goto_declaration_jumps_within_same_file() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_declaration(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_declaration(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDeclaration);
        h.settle();
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(focused_buffer_path(&h), path);
    }

    #[test]
    fn goto_declaration_unsupported_capability_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_declaration(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 0, 2);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDeclaration);
        h.settle();
        assert_eq!(cursor_offset(&mut h), 0);
        assert!(h.stoat.pending_lsp_jump.is_none());
    }

    #[test]
    fn space_l_shift_j_jumps_to_declaration() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_declaration(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_declaration(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
        h.type_keys("space l J");
        h.settle();
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn goto_definition_multiple_targets_opens_picker() {
        use crate::test_harness::keys;
        use crossterm::event::{Event, KeyCode};
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_definition(&h);
        let root = seed(
            &mut h,
            &[
                ("main.rs", "abc\n"),
                ("lib.rs", "fn one() {}\nfn two() {}\nfn three() {}\n"),
            ],
        );
        let main_path = root.join("main.rs");
        let lib_path = root.join("lib.rs");
        open_buffer(&mut h, main_path.clone());
        let lib = lib_path.to_str().unwrap();
        h.fake_lsp().set_definitions(
            main_path.to_str().unwrap(),
            0,
            0,
            &[(lib, 0, 3), (lib, 1, 3), (lib, 2, 3)],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
        h.settle();

        let picker = h.stoat.location_picker.as_ref().expect("picker open");
        assert_eq!(picker.entries().len(), 3);
        assert_eq!(
            focused_buffer_path(&h),
            main_path,
            "picker does not jump yet"
        );

        h.stoat.update(Event::Key(keys::key(KeyCode::Down)));
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        h.settle();

        assert!(h.stoat.location_picker.is_none());
        assert_eq!(focused_buffer_path(&h), lib_path);
        assert_eq!(cursor_offset(&mut h), 15);
    }

    #[test]
    fn goto_definition_opens_target_file() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_definition(&h);
        let root = seed(
            &mut h,
            &[
                ("main.rs", "abc\n"),
                ("lib.rs", "fn one() {}\nfn two() {}\n"),
            ],
        );
        let main_path = root.join("main.rs");
        let lib_path = root.join("lib.rs");
        open_buffer(&mut h, main_path.clone());
        h.fake_lsp().set_definition(
            main_path.to_str().unwrap(),
            0,
            0,
            lib_path.to_str().unwrap(),
            1,
            3,
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
        h.settle();
        assert_eq!(focused_buffer_path(&h), lib_path);
        assert_eq!(cursor_offset(&mut h), 15);
    }

    #[test]
    fn goto_definition_no_result_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_definition(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
        h.settle();
        assert_eq!(cursor_offset(&mut h), 0);
        assert_eq!(focused_buffer_path(&h), path);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: no definition found"),
        );
    }

    #[test]
    fn hover_no_result_reports_no_hover_info() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: no hover info"),
        );
    }

    #[test]
    fn hover_no_result_during_progress_names_the_operation() {
        use crate::host::LspNotification;
        use lsp_types::{NumberOrString, WorkDoneProgress, WorkDoneProgressBegin};

        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));

        h.stoat.lsp_progress.update(
            "primary",
            &LspNotification::Progress {
                token: NumberOrString::Number(1),
                value: WorkDoneProgress::Begin(WorkDoneProgressBegin {
                    title: "indexing".into(),
                    cancellable: None,
                    message: None,
                    percentage: None,
                }),
            },
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: no hover info yet (primary indexing)"),
        );
    }

    #[test]
    fn hover_request_failure_reports_it() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));

        h.fake_lsp()
            .fail_next_request("textDocument/hover", std::io::ErrorKind::Other);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: hover request failed"),
        );
    }

    #[test]
    fn in_flight_hover_shows_a_status_segment() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));

        // Hold the response open so the request stays in flight through render.
        h.fake_lsp()
            .set_request_delay("textDocument/hover", Duration::from_secs(60));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();
        assert!(
            h.stoat.pending_hover_request.is_some(),
            "the delayed hover request stays in flight",
        );

        let buf = h.stoat.render();
        let shown = (0..buf.area.height).any(|y| {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            row.contains("lsp: hover...")
        });
        assert!(shown, "the status bar shows the in-flight hover segment");
    }

    #[test]
    fn in_flight_code_action_shows_a_status_segment() {
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));

        // Hold the response open so the request stays in flight through render.
        h.fake_lsp()
            .set_request_delay("textDocument/codeAction", Duration::from_secs(60));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
        h.settle();
        assert!(
            h.stoat.pending_code_action_request.is_some(),
            "the delayed code-action request stays in flight",
        );

        let buf = h.stoat.render();
        let shown = (0..buf.area.height).any(|y| {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            row.contains("lsp: code actions...")
        });
        assert!(
            shown,
            "the status bar shows the in-flight code-action segment"
        );
    }

    #[test]
    fn code_action_no_result_reports_none_available() {
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
        h.settle();

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: no code actions available"),
        );
    }

    #[test]
    fn goto_definition_unsupported_capability_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_definition(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 0, 2);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
        h.settle();
        assert_eq!(cursor_offset(&mut h), 0);
        assert!(h.stoat.pending_lsp_jump.is_none());
    }

    #[test]
    fn goto_references_multiple_opens_picker() {
        use crate::test_harness::keys;
        use crossterm::event::{Event, KeyCode};
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_references(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let p = path.to_str().unwrap();
        h.fake_lsp()
            .set_references(p, 0, 0, &[(p, 0, 0), (p, 1, 0), (p, 2, 0)]);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::defs::editor::GotoReferences);
        h.settle();

        let picker = h.stoat.location_picker.as_ref().expect("picker open");
        assert_eq!(picker.entries().len(), 3);

        h.stoat.update(Event::Key(keys::key(KeyCode::Down)));
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        h.settle();

        assert!(h.stoat.location_picker.is_none());
        assert_eq!(cursor_offset(&mut h), 4);
    }

    #[test]
    fn goto_references_unsupported_uses_code_graph() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let p = path.to_str().unwrap();
        h.fake_lsp()
            .set_references(p, 0, 0, &[(p, 0, 0), (p, 1, 0), (p, 2, 0)]);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::defs::editor::GotoReferences);
        h.settle();

        assert!(h.stoat.location_picker.is_none(), "LSP path is gated off");
        assert!(h.stoat.pending_lsp_jump.is_none());
        assert_eq!(
            cursor_offset(&mut h),
            0,
            "code-graph fallback no-ops on empty graph"
        );
    }

    #[test]
    fn space_l_j_jumps_to_definition() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_definition(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_definition(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
        h.type_keys("space l j");
        h.settle();
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    fn enable_goto_type_definition(h: &TestHarness) {
        use lsp_types::{ServerCapabilities, TypeDefinitionProviderCapability};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
            ..Default::default()
        });
    }

    #[test]
    fn goto_type_definition_jumps_within_same_file() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_type_definition(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_type_definition(
            path.to_str().unwrap(),
            0,
            0,
            path.to_str().unwrap(),
            2,
            0,
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoTypeDefinition);
        h.settle();
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(focused_buffer_path(&h), path);
    }

    #[test]
    fn goto_type_definition_opens_target_file() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_type_definition(&h);
        let root = seed(
            &mut h,
            &[
                ("main.rs", "abc\n"),
                ("types.rs", "struct One;\nstruct Two;\n"),
            ],
        );
        let main_path = root.join("main.rs");
        let types_path = root.join("types.rs");
        open_buffer(&mut h, main_path.clone());
        h.fake_lsp().set_type_definition(
            main_path.to_str().unwrap(),
            0,
            0,
            types_path.to_str().unwrap(),
            1,
            7,
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoTypeDefinition);
        h.settle();
        assert_eq!(focused_buffer_path(&h), types_path);
        assert_eq!(cursor_offset(&mut h), 19);
    }

    #[test]
    fn goto_type_definition_no_result_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_type_definition(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoTypeDefinition);
        h.settle();
        assert_eq!(cursor_offset(&mut h), 0);
        assert_eq!(focused_buffer_path(&h), path);
    }

    #[test]
    fn goto_type_definition_unsupported_capability_is_noop() {
        use lsp_types::{OneOf, ServerCapabilities};
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp().set_capabilities(ServerCapabilities {
            definition_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_type_definition(
            path.to_str().unwrap(),
            0,
            0,
            path.to_str().unwrap(),
            0,
            2,
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoTypeDefinition);
        h.settle();
        assert_eq!(cursor_offset(&mut h), 0);
        assert!(h.stoat.pending_lsp_jump.is_none());
    }

    #[test]
    fn space_l_k_jumps_to_type_definition() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_type_definition(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_type_definition(
            path.to_str().unwrap(),
            0,
            0,
            path.to_str().unwrap(),
            2,
            0,
        );
        h.type_keys("space l k");
        h.settle();
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    fn enable_goto_implementation(h: &TestHarness) {
        use lsp_types::{ImplementationProviderCapability, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
            ..Default::default()
        });
    }

    #[test]
    fn goto_implementation_jumps_within_same_file() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_implementation(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_implementation(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoImplementation);
        h.settle();
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(focused_buffer_path(&h), path);
    }

    #[test]
    fn goto_implementation_opens_target_file() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_implementation(&h);
        let root = seed(
            &mut h,
            &[
                ("trait.rs", "trait X {}\n"),
                ("impl.rs", "impl X for One {}\nimpl X for Two {}\n"),
            ],
        );
        let trait_path = root.join("trait.rs");
        let impl_path = root.join("impl.rs");
        open_buffer(&mut h, trait_path.clone());
        h.fake_lsp().set_implementation(
            trait_path.to_str().unwrap(),
            0,
            0,
            impl_path.to_str().unwrap(),
            1,
            5,
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoImplementation);
        h.settle();
        assert_eq!(focused_buffer_path(&h), impl_path);
        assert_eq!(cursor_offset(&mut h), 23);
    }

    #[test]
    fn goto_implementation_no_result_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_implementation(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoImplementation);
        h.settle();
        assert_eq!(cursor_offset(&mut h), 0);
        assert_eq!(focused_buffer_path(&h), path);
    }

    #[test]
    fn goto_implementation_unsupported_capability_is_noop() {
        use lsp_types::{OneOf, ServerCapabilities};
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp().set_capabilities(ServerCapabilities {
            definition_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_implementation(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 0, 2);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoImplementation);
        h.settle();
        assert_eq!(cursor_offset(&mut h), 0);
        assert!(h.stoat.pending_lsp_jump.is_none());
    }

    #[test]
    fn space_l_t_jumps_to_implementation() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_implementation(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_implementation(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
        h.type_keys("space l t");
        h.settle();
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn g_s_jumps_to_implementation() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_implementation(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_implementation(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
        h.type_keys("g s");
        h.settle();
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    fn enable_hover(h: &TestHarness) {
        use lsp_types::{HoverProviderCapability, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            ..Default::default()
        });
    }

    #[test]
    fn hover_popup_appears_on_response() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 0, "fn foo() -> u32");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();
        let popup = h.stoat.pending_hover.as_ref().expect("popup");
        assert_eq!(
            popup.lines,
            vec![vec![("fn foo() -> u32".to_string(), Style::default())]]
        );
        assert_eq!(popup.anchor_offset, 0);
    }

    #[test]
    fn hover_response_dropped_when_focus_moved_to_another_editor() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 0, "fn foo() -> u32");

        // Hover from the focused pane, then split so focus moves to the new
        // pane's editor before the response settles.
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        h.settle();

        assert!(
            h.stoat.pending_hover.is_none(),
            "a response for an editor that lost focus is dropped"
        );
    }

    #[test]
    fn hover_response_signals_redraw_notify() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 0, "fn foo() -> u32");

        // open_buffer's parse/reindex also wakes redraw_notify. Consume that
        // permit (against an Arc clone, so the observer never borrows `h`
        // across settle) before triggering hover, leaving the hover
        // response's wake as the only one to observe. Notify holds at most
        // one permit, so a single drain clears it.
        let redraw = h.stoat.redraw_notify.clone();
        let _ = redraw.notified().now_or_never();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        let notified = redraw.notified();
        tokio::pin!(notified);
        assert!(
            notified.enable(),
            "hover response should wake redraw_notify so the popup paints \
             without waiting for the next keystroke",
        );
    }

    #[test]
    fn hover_no_response_clears_request() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();
        assert!(h.stoat.pending_hover.is_none());
        assert!(h.stoat.pending_hover_request.is_none());
    }

    #[test]
    fn hover_unsupported_capability_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 0, "ignored");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();
        assert!(h.stoat.pending_hover.is_none());
        assert!(h.stoat.pending_hover_request.is_none());
    }

    #[test]
    fn hover_without_capability_reports_it_in_the_status() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: server does not support hover"),
        );
    }

    #[test]
    fn goto_definition_without_a_server_reports_no_server() {
        let mut h = TestHarness::with_size(80, 24);
        h.allow_host_swap();
        h.stoat
            .set_lsp_host(std::sync::Arc::new(crate::host::NoopLsp));
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: no language server running"),
        );
    }

    #[test]
    fn unsupported_feature_with_two_servers_reports_does_not_support() {
        use lsp_types::ServerCapabilities;
        let mut h = TestHarness::with_size(80, 24);
        // Two servers run but neither advertises hover, so the sole-host probe is
        // a noop. The report must still name the missing capability rather than
        // claim the server is still starting.
        let _ = install_two_servers(&mut h, ServerCapabilities::default());
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: server does not support hover"),
        );
    }

    #[test]
    fn lsp_status_lists_each_running_server() {
        use lsp_types::{HoverProviderCapability, ServerCapabilities};
        let mut h = TestHarness::with_size(80, 24);
        let _ = install_two_servers(
            &mut h,
            ServerCapabilities {
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                ..ServerCapabilities::default()
            },
        );

        let uid = h.stoat.active_workspace().uid();
        let (tx, mut rx) = oneshot::channel();
        super::answer_agent_query(&mut h.stoat, uid, AgentQuery::LspStatus, tx);
        let value = rx.try_recv().expect("lsp-status reply");

        assert_eq!(value["active"], serde_json::json!(true));
        let names: Vec<&str> = value["servers"]
            .as_array()
            .expect("servers array")
            .iter()
            .map(|s| s["name"].as_str().expect("server name"))
            .collect();
        assert!(
            names.contains(&"primary") && names.contains(&"secondary"),
            "servers listed: {names:?}",
        );
    }

    /// Populate a hover popup over `main.rs`, leaving the editor in normal mode.
    fn open_hover(h: &mut TestHarness) {
        enable_hover(h);
        let root = seed(h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(h, path.clone());
        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 0, "details");
        h.type_keys("space l i");
        h.settle();
        assert!(h.stoat.pending_hover.is_some(), "popup should be open");
    }

    #[test]
    fn hover_dismissed_by_escape() {
        use crate::test_harness::keys;
        use crossterm::event::{Event, KeyCode};
        let mut h = TestHarness::with_size(80, 24);
        open_hover(&mut h);

        h.stoat.update(Event::Key(keys::key(KeyCode::Esc)));
        assert!(h.stoat.pending_hover.is_none());
        assert!(h.stoat.pending_hover_request.is_none());
    }

    #[test]
    fn hover_dismissed_by_ctrl_c_without_quitting() {
        use crate::{test_harness::keys, UpdateEffect};
        use crossterm::event::Event;
        let mut h = TestHarness::with_size(80, 24);
        open_hover(&mut h);

        let effect = h.stoat.update(Event::Key(keys::ctrl('c')));
        assert!(
            matches!(effect, UpdateEffect::Redraw),
            "Ctrl-c closes the hover rather than quitting the app"
        );
        assert!(h.stoat.pending_hover.is_none());
    }

    #[test]
    fn hover_dismissed_entering_insert_mode() {
        use crate::test_harness::keys;
        use crossterm::event::{Event, KeyCode};
        let mut h = TestHarness::with_size(80, 24);
        open_hover(&mut h);

        // `i` is SetMode(insert)-only, so it skips the post-dispatch clear; the
        // auto-close intercept closes the popup and the key still enters insert.
        h.stoat.update(Event::Key(keys::key(KeyCode::Char('i'))));
        assert!(h.stoat.pending_hover.is_none(), "the popup closes");
        assert_eq!(
            h.stoat.focused_mode(),
            "insert",
            "and the key still dispatches"
        );
    }

    fn hover_scroll(h: &TestHarness) -> usize {
        h.stoat
            .pending_hover
            .as_ref()
            .expect("popup")
            .scroll_half_pages
    }

    fn scroll_event(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn hover_scrolls_by_half_pages() {
        use crate::test_harness::keys;
        use crossterm::event::Event;
        let mut h = TestHarness::with_size(80, 24);
        open_hover(&mut h);

        h.stoat.update(Event::Key(keys::ctrl('d')));
        h.stoat.update(Event::Key(keys::ctrl('d')));
        assert_eq!(hover_scroll(&h), 2);
        h.stoat.update(Event::Key(keys::ctrl('u')));
        assert_eq!(hover_scroll(&h), 1);
        assert!(
            h.stoat.pending_hover.is_some(),
            "scrolling consumes the key without closing the popup"
        );
    }

    #[test]
    fn hover_scroll_up_saturates_at_the_top() {
        use crate::test_harness::keys;
        use crossterm::event::Event;
        let mut h = TestHarness::with_size(80, 24);
        open_hover(&mut h);

        h.stoat.update(Event::Key(keys::ctrl('u')));
        assert_eq!(hover_scroll(&h), 0);
        assert!(h.stoat.pending_hover.is_some());
    }

    #[test]
    fn wheel_over_the_popup_scrolls_it() {
        let mut h = TestHarness::with_size(80, 24);
        open_hover(&mut h);
        // Render once so render_hover stamps the popup's screen rect.
        h.stoat.render();

        let area = h.stoat.pending_hover.as_ref().expect("popup").area;
        h.stoat.update(scroll_event(
            MouseEventKind::ScrollDown,
            area.x + area.width / 2,
            area.y + area.height / 2,
        ));

        assert_eq!(hover_scroll(&h), 1, "the wheel scrolls the popup");
        assert!(h.stoat.pending_hover.is_some(), "and leaves it open");
    }

    #[test]
    fn wheel_outside_the_popup_leaves_it_unscrolled() {
        let mut h = TestHarness::with_size(80, 24);
        open_hover(&mut h);
        h.stoat.render();

        let area = h.stoat.pending_hover.as_ref().expect("popup").area;
        // Just past the popup's bottom edge, still over the editor pane.
        h.stoat.update(scroll_event(
            MouseEventKind::ScrollDown,
            area.x,
            area.y + area.height,
        ));

        assert_eq!(
            hover_scroll(&h),
            0,
            "a wheel off the popup does not scroll it"
        );
        assert!(h.stoat.pending_hover.is_some(), "and leaves it open");
    }

    #[test]
    fn snapshot_hover_scrolled_down() {
        use crate::test_harness::keys;
        use crossterm::event::Event;
        let mut h = TestHarness::with_size(40, 12);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let body = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        h.fake_lsp().set_hover(path.to_str().unwrap(), 0, 0, &body);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();
        h.stoat.update(Event::Key(keys::ctrl('d')));
        h.stoat.update(Event::Key(keys::ctrl('d')));
        h.assert_snapshot("snapshot_hover_scrolled");
    }

    #[test]
    fn snapshot_hover_below_when_tall() {
        let mut h = TestHarness::with_size(40, 20);
        enable_hover(&h);
        let source = (0..12)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let root = seed(&mut h, &[("main.rs", &source)]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());

        // Cursor on buffer line 5, leaving room below for the tall body.
        if let Some(editor) = crate::action_handlers::focused_editor_mut(&mut h.stoat) {
            crate::action_handlers::movement::set_cursor_row(editor, 5);
        }

        let body = (0..10)
            .map(|i| format!("hover {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        h.fake_lsp().set_hover(path.to_str().unwrap(), 5, 0, &body);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();
        h.assert_snapshot("snapshot_hover_below_when_tall");
    }

    #[test]
    fn query_diagnostics_returns_seeded_set() {
        use lsp_types::Diagnostic;

        let mut h = TestHarness::with_size(40, 10);
        let path = PathBuf::from("/proj/a.rs");
        let diagnostic = Diagnostic {
            message: "boom".into(),
            ..Default::default()
        };
        h.stoat
            .diagnostics
            .replace_for_path(path.clone(), vec![diagnostic.clone()]);

        let uid = h.stoat.active_workspace().uid();
        let (reply_tx, mut reply_rx) = oneshot::channel();
        h.stoat.handle_agent_control(AgentControl::Query {
            uid,
            request: AgentQuery::Diagnostics { path: Some(path) },
            reply: reply_tx,
        });

        let value = reply_rx.try_recv().expect("synchronous diagnostics reply");
        let got: Vec<Diagnostic> = serde_json::from_value(value).unwrap();
        assert_eq!(got, vec![diagnostic]);
    }

    #[test]
    fn query_hover_returns_fake_hover() {
        use lsp_types::{Hover, HoverContents};

        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 1, "hover text");

        let uid = h.stoat.active_workspace().uid();
        let (reply_tx, mut reply_rx) = oneshot::channel();
        h.stoat.handle_agent_control(AgentControl::Query {
            uid,
            request: AgentQuery::Hover {
                path: path.clone(),
                line: 0,
                col: 1,
            },
            reply: reply_tx,
        });
        h.settle();

        let value = reply_rx.try_recv().expect("hover reply");
        let hover: Hover = serde_json::from_value(value).unwrap();
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("expected markup hover contents");
        };
        assert_eq!(markup.value, "hover text");
    }

    #[test]
    fn query_hover_on_unopened_path_replies_error() {
        let mut h = TestHarness::with_size(40, 10);
        let uid = h.stoat.active_workspace().uid();
        let (reply_tx, mut reply_rx) = oneshot::channel();
        h.stoat.handle_agent_control(AgentControl::Query {
            uid,
            request: AgentQuery::Hover {
                path: PathBuf::from("/nope.rs"),
                line: 0,
                col: 0,
            },
            reply: reply_tx,
        });

        let value = reply_rx.try_recv().expect("synchronous error reply");
        assert_eq!(value, serde_json::json!({ "error": "not open" }));
    }

    #[test]
    fn hover_cleared_on_motion() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 0, "details");
        h.type_keys("space l i");
        h.settle();
        assert!(h.stoat.pending_hover.is_some());
        h.type_keys("j");
        assert!(h.stoat.pending_hover.is_none());
    }

    #[test]
    fn space_l_i_triggers_hover() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 0, "documentation");
        h.type_keys("space l i");
        h.settle();
        let popup = h.stoat.pending_hover.as_ref().expect("popup");
        assert_eq!(
            popup.lines,
            vec![vec![("documentation".to_string(), Style::default())]]
        );
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    fn enable_signature_help(h: &TestHarness) {
        use lsp_types::{ServerCapabilities, SignatureHelpOptions};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            signature_help_provider: Some(SignatureHelpOptions {
                trigger_characters: Some(vec!["(".into(), ",".into()]),
                retrigger_characters: Some(vec![",".into()]),
                ..Default::default()
            }),
            ..Default::default()
        });
    }

    fn sig_help(active_param: u32) -> lsp_types::SignatureHelp {
        use lsp_types::{
            ParameterInformation, ParameterLabel, SignatureHelp, SignatureInformation,
        };
        SignatureHelp {
            signatures: vec![SignatureInformation {
                label: "fn add(x: i32, y: i32) -> i32".to_string(),
                documentation: None,
                parameters: Some(vec![
                    ParameterInformation {
                        label: ParameterLabel::Simple("x: i32".into()),
                        documentation: None,
                    },
                    ParameterInformation {
                        label: ParameterLabel::Simple("y: i32".into()),
                        documentation: None,
                    },
                ]),
                active_parameter: Some(active_param),
            }],
            active_signature: Some(0),
            active_parameter: Some(active_param),
        }
    }

    #[test]
    fn signature_help_opens_on_trigger_char() {
        let mut h = TestHarness::with_size(80, 24);
        enable_signature_help(&h);
        let root = seed(&mut h, &[("main.rs", "")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        // After typing `(` the cursor sits at line 0, column 1.
        h.fake_lsp()
            .set_signature_help(path.to_str().unwrap(), 0, 1, sig_help(1));
        h.type_keys("i");
        h.type_text("(");
        h.settle();

        let popup = h.stoat.pending_signature_help.as_ref().expect("popup");
        assert_eq!(popup.label, "fn add(x: i32, y: i32) -> i32");
        assert_eq!(popup.active_param, Some(15..21));
    }

    #[test]
    fn signature_help_retrigger_updates_active_parameter() {
        let mut h = TestHarness::with_size(80, 24);
        enable_signature_help(&h);
        let root = seed(&mut h, &[("main.rs", "")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let p = path.to_str().unwrap();
        h.fake_lsp().set_signature_help(p, 0, 1, sig_help(0));
        h.fake_lsp().set_signature_help(p, 0, 3, sig_help(1));

        h.type_keys("i");
        h.type_text("(");
        h.settle();
        assert_eq!(
            h.stoat
                .pending_signature_help
                .as_ref()
                .expect("popup")
                .active_param,
            Some(7..13),
        );

        h.type_text("x,");
        h.settle();
        assert_eq!(
            h.stoat
                .pending_signature_help
                .as_ref()
                .expect("popup")
                .active_param,
            Some(15..21),
        );
    }

    #[test]
    fn signature_help_cleared_on_leaving_insert() {
        let mut h = TestHarness::with_size(80, 24);
        enable_signature_help(&h);
        let root = seed(&mut h, &[("main.rs", "")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_signature_help(path.to_str().unwrap(), 0, 1, sig_help(1));

        h.type_keys("i");
        h.type_text("(");
        h.settle();
        assert!(h.stoat.pending_signature_help.is_some());

        h.type_keys("escape");
        h.settle();
        assert!(h.stoat.pending_signature_help.is_none());
    }

    #[test]
    fn snapshot_signature_help_active_parameter_bold() {
        let mut h = TestHarness::with_size(60, 12);
        let root = seed(&mut h, &[("main.rs", "add()\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.stoat.pending_signature_help = Some(super::SignatureHelpPopup {
            label: "fn add(x: i32, y: i32) -> i32".to_string(),
            active_param: Some(15..21),
            doc: Some("adds two integers".to_string()),
            anchor_offset: 0,
        });
        h.assert_snapshot("signature_help_active_param_bold");
    }

    #[test]
    fn hover_renders_highlighted_code_and_prose() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_hover(
            path.to_str().unwrap(),
            0,
            0,
            "```rust\nfn foo()\n```\nDocs here",
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();
        let popup = h.stoat.pending_hover.as_ref().expect("popup");

        let texts: Vec<String> = popup
            .lines
            .iter()
            .map(|line| line.iter().map(|(text, _)| text.as_str()).collect())
            .collect();
        assert_eq!(texts, vec!["fn foo()", "", "Docs here"]);
        assert!(
            popup.lines[0].len() > 1,
            "the rust code line is syntax-highlighted into multiple spans"
        );
    }

    #[test]
    fn snapshot_hover_popup_above_cursor() {
        let mut h = TestHarness::with_size(40, 12);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 0, "fn foo() -> u32");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();
        h.assert_snapshot("snapshot_hover_popup");
    }

    /// Move the review editor's text cursor to `buffer_row`. Panics without an
    /// open review session.
    fn place_review_cursor(h: &mut TestHarness, buffer_row: u32) {
        let review_editor_id = h.with_review(|s| s.view_editor).expect("review editor");
        let ws = h.stoat.active_workspace_mut();
        let editor = ws.editors.get_mut(review_editor_id).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, buffer_row);
    }

    #[test]
    fn hover_from_a_non_working_tree_review_issues_nothing() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        // An in-memory (non-working-tree) review: the new side is not disk
        // state, so LSP stays off and no request is issued.
        h.open_review_from_texts(&[("a.rs", "a\nb\nc\nd\n", "a\nb\nX\nd\n")]);

        place_review_cursor(&mut h, 2);
        h.fake_lsp().set_hover("a.rs", 2, 0, "unreachable");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        assert!(
            h.stoat.pending_hover.is_none(),
            "no popup for a non-working-tree review",
        );
        assert!(
            h.stoat.pending_hover_request.is_none(),
            "no request was issued",
        );
    }

    fn enable_code_action(h: &TestHarness) {
        use lsp_types::{CodeActionProviderCapability, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
            ..Default::default()
        });
    }

    #[allow(clippy::mutable_key_type)]
    fn direct_action(
        title: &str,
        file: &str,
        line: u32,
        col: u32,
        text: &str,
    ) -> lsp_types::CodeActionOrCommand {
        use lsp_types::{
            CodeAction, CodeActionOrCommand, Position, Range, TextEdit, Uri, WorkspaceEdit,
        };
        use std::{collections::HashMap, str::FromStr};
        let uri = Uri::from_str(&format!("file://{file}")).expect("uri");
        let edit = TextEdit {
            range: Range::new(Position::new(line, col), Position::new(line, col)),
            new_text: text.to_string(),
        };
        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
        changes.insert(uri, vec![edit]);
        let workspace_edit = WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        };
        CodeActionOrCommand::CodeAction(CodeAction {
            title: title.to_string(),
            kind: None,
            diagnostics: None,
            edit: Some(workspace_edit),
            command: None,
            is_preferred: None,
            disabled: None,
            data: None,
        })
    }

    fn unresolved_action(title: &str) -> lsp_types::CodeActionOrCommand {
        use lsp_types::{CodeAction, CodeActionOrCommand};
        CodeActionOrCommand::CodeAction(CodeAction {
            title: title.to_string(),
            kind: None,
            diagnostics: None,
            edit: None,
            command: None,
            is_preferred: None,
            disabled: None,
            data: Some(serde_json::Value::Null),
        })
    }

    fn command_only_action(title: &str) -> lsp_types::CodeActionOrCommand {
        use lsp_types::{CodeActionOrCommand, Command};
        CodeActionOrCommand::Command(Command {
            title: title.to_string(),
            command: "noop".to_string(),
            arguments: None,
        })
    }

    fn buffer_text(h: &TestHarness, path: &Path) -> String {
        let buffer_id = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(path)
            .expect("buffer for path");
        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer");
        let guard = buffer.read().expect("buffer lock");
        guard.rope().to_string()
    }

    #[test]
    fn code_action_unsupported_capability_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_code_actions(
            path.to_str().unwrap(),
            vec![direct_action("X", path.to_str().unwrap(), 0, 0, "X")],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
        h.settle();
        assert!(h.stoat.pending_code_action_picker.is_none());
        assert!(h.stoat.pending_code_action_request.is_none());
    }

    #[test]
    fn code_action_no_response_clears_picker() {
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
        h.settle();
        assert!(h.stoat.pending_code_action_picker.is_none());
        assert!(h.stoat.pending_code_action_request.is_none());
    }

    #[test]
    fn code_action_populates_picker_with_titles() {
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_code_actions(
            path.to_str().unwrap(),
            vec![
                direct_action("Add import", path.to_str().unwrap(), 0, 0, "use a;\n"),
                direct_action("Inline variable", path.to_str().unwrap(), 0, 0, ""),
            ],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
        h.settle();
        let picker = h
            .stoat
            .pending_code_action_picker
            .as_ref()
            .expect("picker open");
        let titles: Vec<&str> = picker.entries.iter().map(|e| e.title()).collect();
        assert_eq!(titles, vec!["Add import", "Inline variable"]);
    }

    #[test]
    fn code_action_retains_command_only_entries() {
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_code_actions(
            path.to_str().unwrap(),
            vec![
                command_only_action("Run command"),
                direct_action("Real edit", path.to_str().unwrap(), 0, 0, "X"),
            ],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
        h.settle();
        let picker = h
            .stoat
            .pending_code_action_picker
            .as_ref()
            .expect("picker open");
        let titles: Vec<&str> = picker.entries.iter().map(|e| e.title()).collect();
        assert_eq!(titles, vec!["Run command", "Real edit"]);
    }

    #[test]
    fn code_action_pick_command_dispatches_execute_command() {
        use lsp_types::{CodeActionOrCommand, Command};
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_code_actions(
            path.to_str().unwrap(),
            vec![CodeActionOrCommand::Command(Command {
                title: "Apply import".to_string(),
                command: "rust-analyzer.applyImport".to_string(),
                arguments: Some(vec![serde_json::json!({"target": "std::io"})]),
            })],
        );
        h.type_keys("space l a");
        h.settle();
        h.type_keys("1");
        h.settle();
        assert!(h.stoat.pending_code_action_picker.is_none());
        let observed = h.fake_lsp().observed_executed_commands();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].command, "rust-analyzer.applyImport");
        assert_eq!(
            observed[0].arguments,
            vec![serde_json::json!({"target": "std::io"})]
        );
    }

    #[test]
    fn code_action_navigates_with_jk_and_picks_with_enter() {
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let actions: Vec<lsp_types::CodeActionOrCommand> = (0..12)
            .map(|i| {
                direct_action(
                    &format!("Action {i}"),
                    path.to_str().unwrap(),
                    0,
                    0,
                    &format!("// {i}\n"),
                )
            })
            .collect();
        h.fake_lsp()
            .set_code_actions(path.to_str().unwrap(), actions);
        h.type_keys("space l a");
        h.settle();
        for _ in 0..11 {
            h.type_keys("j");
        }
        let picker = h.stoat.pending_code_action_picker.as_ref().expect("picker");
        assert_eq!(picker.selected_idx, 11);

        h.type_keys("enter");
        assert!(h.stoat.pending_code_action_picker.is_none());
        assert_eq!(buffer_text(&h, &path), "// 11\nabc\n");
    }

    #[test]
    fn code_action_pick_one_applies_edit() {
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_code_actions(
            path.to_str().unwrap(),
            vec![direct_action(
                "Insert prefix",
                path.to_str().unwrap(),
                0,
                0,
                "// hi\n",
            )],
        );
        h.type_keys("space l a");
        h.settle();
        h.type_keys("1");
        h.settle();
        assert!(h.stoat.pending_code_action_picker.is_none());
        assert_eq!(buffer_text(&h, &path), "// hi\nabc\n");
    }

    #[test]
    fn code_action_resolve_path_applies_resolved_edit() {
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_code_actions(path.to_str().unwrap(), vec![unresolved_action("Refactor")]);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
        h.settle();
        assert!(h.stoat.pending_code_action_picker.is_some());
        crate::action_handlers::lsp::pick_code_action(&mut h.stoat, 0);
        h.settle();
        assert!(h.stoat.pending_code_action_picker.is_none());
        assert!(h.stoat.pending_code_action_resolve.is_none());
    }

    #[test]
    fn code_action_resolve_routes_back_to_the_producing_server() {
        use crate::lsp::registry::ServerSelector;
        use lsp_types::{
            request::CodeActionResolveRequest, CodeActionProviderCapability, ServerCapabilities,
        };

        let mut h = TestHarness::with_size(80, 24);
        let primary = std::sync::Arc::new(crate::host::FakeLsp::new());
        primary.set_capabilities(ServerCapabilities::default());
        let producer = std::sync::Arc::new(crate::host::FakeLsp::new());
        producer.set_capabilities(ServerCapabilities {
            code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
            ..ServerCapabilities::default()
        });
        h.stoat
            .lsp_registry
            .insert("primary".into(), primary.clone());
        h.stoat
            .lsp_registry
            .insert("producer".into(), producer.clone());
        h.stoat.lsp_registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("primary".into()),
                ServerSelector::all("producer".into()),
            ],
        );

        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        producer.set_code_actions(path.to_str().unwrap(), vec![unresolved_action("Refactor")]);
        producer.set_pending_mode::<CodeActionResolveRequest>(true);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
        h.settle();
        assert!(
            h.stoat.pending_code_action_picker.is_some(),
            "the producing server served the code action"
        );

        crate::action_handlers::lsp::pick_code_action(&mut h.stoat, 0);
        h.settle();

        assert_eq!(
            producer.pending_count("codeAction/resolve"),
            1,
            "resolve routes back to the producing server"
        );
        assert_eq!(
            primary.pending_count("codeAction/resolve"),
            0,
            "resolve does not go to the primary that never saw the action"
        );
    }

    #[test]
    fn code_action_escape_dismisses_picker() {
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_code_actions(
            path.to_str().unwrap(),
            vec![direct_action("X", path.to_str().unwrap(), 0, 0, "X")],
        );
        h.type_keys("space l a");
        h.settle();
        assert!(h.stoat.pending_code_action_picker.is_some());
        h.type_keys("escape");
        assert!(h.stoat.pending_code_action_picker.is_none());
    }

    #[test]
    fn space_l_a_triggers_code_action() {
        let mut h = TestHarness::with_size(80, 24);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_code_actions(
            path.to_str().unwrap(),
            vec![direct_action("X", path.to_str().unwrap(), 0, 0, "X")],
        );
        h.type_keys("space l a");
        h.settle();
        assert!(h.stoat.pending_code_action_picker.is_some());
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn snapshot_code_action_picker() {
        let mut h = TestHarness::with_size(40, 12);
        enable_code_action(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_code_actions(
            path.to_str().unwrap(),
            vec![
                direct_action("Add import", path.to_str().unwrap(), 0, 0, "X"),
                direct_action("Inline", path.to_str().unwrap(), 0, 0, "X"),
            ],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
        h.settle();
        h.assert_snapshot("snapshot_code_action_picker");
    }

    fn enable_rename(h: &TestHarness) {
        use lsp_types::{OneOf, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            rename_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
    }

    #[allow(clippy::mutable_key_type)]
    fn rename_workspace_edit(
        file: &str,
        line: u32,
        col: u32,
        len: u32,
        new: &str,
    ) -> lsp_types::WorkspaceEdit {
        use lsp_types::{Position as LspPosition, Range as LspRange, TextEdit, Uri, WorkspaceEdit};
        use std::{collections::HashMap, str::FromStr};
        let uri = Uri::from_str(&format!("file://{file}")).expect("uri");
        let edit = TextEdit {
            range: LspRange::new(
                LspPosition::new(line, col),
                LspPosition::new(line, col + len),
            ),
            new_text: new.to_string(),
        };
        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
        changes.insert(uri, vec![edit]);
        WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }
    }

    #[test]
    fn rename_unsupported_capability_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RenameSymbol);
        h.settle();
        assert!(h.stoat.rename_input.is_none());
        assert!(h.stoat.pending_prepare_rename.is_none());
    }

    #[test]
    fn rename_no_response_does_not_open_modal() {
        let mut h = TestHarness::with_size(80, 24);
        enable_rename(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RenameSymbol);
        h.settle();
        assert!(h.stoat.rename_input.is_none());
    }

    #[test]
    fn rename_range_response_seeds_placeholder_from_rope() {
        use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
        let mut h = TestHarness::with_size(80, 24);
        enable_rename(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_prepare_rename(
            path.to_str().unwrap(),
            0,
            0,
            PrepareRenameResponse::Range(LspRange::new(
                LspPosition::new(0, 3),
                LspPosition::new(0, 6),
            )),
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RenameSymbol);
        h.settle();
        let modal = h.stoat.rename_input.as_ref().expect("modal open");
        assert_eq!(modal.input.text(h.stoat.active_workspace()), "foo");
        assert_eq!(h.stoat.focused_mode(), "insert");
    }

    #[test]
    fn rename_with_placeholder_form() {
        use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
        let mut h = TestHarness::with_size(80, 24);
        enable_rename(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_prepare_rename(
            path.to_str().unwrap(),
            0,
            0,
            PrepareRenameResponse::RangeWithPlaceholder {
                range: LspRange::new(LspPosition::new(0, 3), LspPosition::new(0, 6)),
                placeholder: "Renamed".to_string(),
            },
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RenameSymbol);
        h.settle();
        let modal = h.stoat.rename_input.as_ref().expect("modal open");
        assert_eq!(modal.input.text(h.stoat.active_workspace()), "Renamed");
    }

    #[test]
    fn rename_submit_applies_workspace_edit() {
        use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
        let mut h = TestHarness::with_size(80, 24);
        enable_rename(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_prepare_rename(
            path.to_str().unwrap(),
            0,
            0,
            PrepareRenameResponse::Range(LspRange::new(
                LspPosition::new(0, 3),
                LspPosition::new(0, 6),
            )),
        );
        h.fake_lsp().set_rename(
            path.to_str().unwrap(),
            0,
            0,
            rename_workspace_edit(path.to_str().unwrap(), 0, 3, 3, "bar"),
        );
        h.type_keys("space l r");
        h.settle();
        assert!(h.stoat.rename_input.is_some());
        crate::action_handlers::lsp::rename_input_submit(&mut h.stoat);
        h.settle();
        assert!(h.stoat.rename_input.is_none());
        assert_eq!(buffer_text(&h, &path), "fn bar() {}\n");
    }

    #[test]
    fn rename_cancel_discards_modal() {
        use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
        let mut h = TestHarness::with_size(80, 24);
        enable_rename(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_prepare_rename(
            path.to_str().unwrap(),
            0,
            0,
            PrepareRenameResponse::Range(LspRange::new(
                LspPosition::new(0, 3),
                LspPosition::new(0, 6),
            )),
        );
        h.type_keys("space l r");
        h.settle();
        assert!(h.stoat.rename_input.is_some());
        let cancelled = crate::action_handlers::lsp::rename_input_cancel(&mut h.stoat);
        assert!(cancelled);
        assert!(h.stoat.rename_input.is_none());
        assert_eq!(buffer_text(&h, &path), "fn foo() {}\n");
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn space_l_r_triggers_rename() {
        use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
        let mut h = TestHarness::with_size(80, 24);
        enable_rename(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_prepare_rename(
            path.to_str().unwrap(),
            0,
            0,
            PrepareRenameResponse::Range(LspRange::new(
                LspPosition::new(0, 3),
                LspPosition::new(0, 6),
            )),
        );
        h.type_keys("space l r");
        h.settle();
        let modal = h.stoat.rename_input.as_ref().expect("modal open");
        assert_eq!(modal.input.text(h.stoat.active_workspace()), "foo");
        assert_eq!(h.stoat.focused_mode(), "insert");
    }

    #[test]
    fn snapshot_rename_input_modal() {
        use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
        let mut h = TestHarness::with_size(40, 12);
        enable_rename(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_prepare_rename(
            path.to_str().unwrap(),
            0,
            0,
            PrepareRenameResponse::Range(LspRange::new(
                LspPosition::new(0, 3),
                LspPosition::new(0, 6),
            )),
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RenameSymbol);
        h.settle();
        h.assert_snapshot("snapshot_rename_input");
    }

    use lsp_types::{DocumentSymbol, DocumentSymbolResponse};

    fn enable_document_symbols(h: &TestHarness) {
        use lsp_types::{OneOf, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            document_symbol_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
    }

    fn flat_symbol(name: &str, file: &str, line: u32, col: u32) -> lsp_types::SymbolInformation {
        use lsp_types::{
            Location, Position as LspPosition, Range as LspRange, SymbolInformation, SymbolKind,
            Uri,
        };
        use std::str::FromStr;
        #[allow(deprecated)]
        SymbolInformation {
            name: name.to_string(),
            kind: SymbolKind::FUNCTION,
            tags: None,
            deprecated: None,
            location: Location {
                uri: Uri::from_str(&format!("file://{file}")).expect("uri"),
                range: LspRange::new(LspPosition::new(line, col), LspPosition::new(line, col + 1)),
            },
            container_name: None,
        }
    }

    #[test]
    fn symbol_picker_unsupported_capability_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_symbols(
            path.to_str().unwrap(),
            DocumentSymbolResponse::Flat(vec![flat_symbol("foo", path.to_str().unwrap(), 0, 3)]),
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();
        assert!(h.stoat.symbol_finder.is_none());
        assert!(h.stoat.pending_symbol_picker_request.is_none());
    }

    #[test]
    fn symbol_picker_no_response_keeps_modal_open() {
        let mut h = TestHarness::with_size(80, 24);
        enable_document_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        open_buffer(&mut h, root.join("main.rs"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();
        let finder = h.stoat.symbol_finder.as_ref().expect("modal stays open");
        assert!(finder.entries.is_empty(), "no symbols yields an empty list");
    }

    #[test]
    fn symbol_picker_populates_with_flat_symbols() {
        let mut h = TestHarness::with_size(80, 24);
        enable_document_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\nfn bar() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_symbols(
            path.to_str().unwrap(),
            DocumentSymbolResponse::Flat(vec![
                flat_symbol("foo", path.to_str().unwrap(), 0, 3),
                flat_symbol("bar", path.to_str().unwrap(), 1, 3),
            ]),
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();
        let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
        let titles: Vec<&str> = finder.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["foo", "bar"]);
    }

    #[test]
    fn symbol_picker_flattens_nested_symbols() {
        use lsp_types::{Position as LspPosition, Range as LspRange, SymbolKind};
        let mut h = TestHarness::with_size(80, 24);
        enable_document_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn outer() {\n  fn inner() {}\n}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let range = LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 1));
        let inner = {
            #[allow(deprecated)]
            DocumentSymbol {
                name: "inner".to_string(),
                detail: None,
                kind: SymbolKind::FUNCTION,
                tags: None,
                deprecated: None,
                range,
                selection_range: range,
                children: None,
            }
        };
        let outer = {
            #[allow(deprecated)]
            DocumentSymbol {
                name: "outer".to_string(),
                detail: None,
                kind: SymbolKind::FUNCTION,
                tags: None,
                deprecated: None,
                range,
                selection_range: range,
                children: Some(vec![inner]),
            }
        };
        h.fake_lsp().set_document_symbols(
            path.to_str().unwrap(),
            DocumentSymbolResponse::Nested(vec![outer]),
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();
        let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
        let titles: Vec<&str> = finder.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["outer", "outer.inner"]);
    }

    #[test]
    fn symbol_picker_pick_jumps_to_offset() {
        let mut h = TestHarness::with_size(80, 24);
        enable_document_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\nfn bar() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_symbols(
            path.to_str().unwrap(),
            DocumentSymbolResponse::Flat(vec![
                flat_symbol("foo", path.to_str().unwrap(), 0, 3),
                flat_symbol("bar", path.to_str().unwrap(), 1, 3),
            ]),
        );
        h.type_keys("space l s");
        h.settle();
        h.type_keys("down");
        h.type_keys("enter");
        assert!(h.stoat.symbol_finder.is_none());
        assert_eq!(cursor_offset(&mut h), 15);
    }

    #[test]
    fn symbol_picker_keeps_all_entries() {
        let mut h = TestHarness::with_size(80, 24);
        enable_document_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "x\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let many: Vec<lsp_types::SymbolInformation> = (0..15)
            .map(|i| flat_symbol(&format!("sym{i}"), path.to_str().unwrap(), 0, 0))
            .collect();
        h.fake_lsp()
            .set_document_symbols(path.to_str().unwrap(), DocumentSymbolResponse::Flat(many));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();
        let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
        assert_eq!(finder.entries.len(), 15);
        assert_eq!(finder.selected, 0);
    }

    #[test]
    fn symbol_picker_navigates_with_arrows_and_picks_with_enter() {
        let mut h = TestHarness::with_size(80, 24);
        enable_document_symbols(&h);
        let mut text = String::new();
        for _ in 0..15 {
            text.push_str("fn x() {}\n");
        }
        let root = seed(&mut h, &[("main.rs", text.as_str())]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let many: Vec<lsp_types::SymbolInformation> = (0..15)
            .map(|i| flat_symbol(&format!("sym{i}"), path.to_str().unwrap(), i as u32, 3))
            .collect();
        h.fake_lsp()
            .set_document_symbols(path.to_str().unwrap(), DocumentSymbolResponse::Flat(many));

        h.type_keys("space l s");
        h.settle();
        for _ in 0..11 {
            h.type_keys("down");
        }
        let finder = h.stoat.symbol_finder.as_ref().expect("finder");
        assert_eq!(finder.selected, 11);

        h.type_keys("enter");
        assert!(h.stoat.symbol_finder.is_none());
        assert_eq!(cursor_offset(&mut h), 11 * 10 + 3);
    }

    #[test]
    fn symbol_picker_escape_dismisses() {
        let mut h = TestHarness::with_size(80, 24);
        enable_document_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_symbols(
            path.to_str().unwrap(),
            DocumentSymbolResponse::Flat(vec![flat_symbol("foo", path.to_str().unwrap(), 0, 3)]),
        );
        h.type_keys("space l s");
        h.settle();
        assert!(h.stoat.symbol_finder.is_some());
        h.type_keys("escape");
        assert!(h.stoat.symbol_finder.is_none());
    }

    #[test]
    fn space_l_s_triggers_symbol_picker() {
        let mut h = TestHarness::with_size(80, 24);
        enable_document_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_symbols(
            path.to_str().unwrap(),
            DocumentSymbolResponse::Flat(vec![flat_symbol("foo", path.to_str().unwrap(), 0, 3)]),
        );
        h.type_keys("space l s");
        h.settle();
        assert!(h.stoat.symbol_finder.is_some());
        assert_eq!(h.stoat.focused_mode(), "insert");
    }

    #[test]
    fn snapshot_symbol_picker() {
        let mut h = TestHarness::with_size(60, 16);
        enable_document_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_symbols(
            path.to_str().unwrap(),
            DocumentSymbolResponse::Flat(vec![
                flat_symbol("foo", path.to_str().unwrap(), 0, 3),
                flat_symbol("bar", path.to_str().unwrap(), 1, 3),
            ]),
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();
        h.assert_snapshot("snapshot_symbol_picker");
    }

    /// Install two identically-capable fakes routed primary-then-secondary for
    /// `rust`. The caller seeds and opens its own buffer.
    fn install_two_servers(
        h: &mut TestHarness,
        caps: lsp_types::ServerCapabilities,
    ) -> (
        std::sync::Arc<crate::host::FakeLsp>,
        std::sync::Arc<crate::host::FakeLsp>,
    ) {
        use crate::lsp::registry::ServerSelector;
        let primary = std::sync::Arc::new(crate::host::FakeLsp::new());
        primary.set_capabilities(caps.clone());
        let secondary = std::sync::Arc::new(crate::host::FakeLsp::new());
        secondary.set_capabilities(caps);
        h.stoat
            .lsp_registry
            .insert("primary".into(), primary.clone());
        h.stoat
            .lsp_registry
            .insert("secondary".into(), secondary.clone());
        h.stoat.lsp_registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("primary".into()),
                ServerSelector::all("secondary".into()),
            ],
        );
        (primary, secondary)
    }

    fn document_symbol_caps() -> lsp_types::ServerCapabilities {
        use lsp_types::{OneOf, ServerCapabilities};
        ServerCapabilities {
            document_symbol_provider: Some(OneOf::Left(true)),
            ..ServerCapabilities::default()
        }
    }

    #[test]
    fn document_symbols_merge_from_two_servers() {
        let mut h = TestHarness::with_size(80, 24);
        let (primary, secondary) = install_two_servers(&mut h, document_symbol_caps());
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\nfn bar() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let p = path.to_str().unwrap();
        primary.set_document_symbols(
            p,
            DocumentSymbolResponse::Flat(vec![flat_symbol("foo", p, 0, 3)]),
        );
        secondary.set_document_symbols(
            p,
            DocumentSymbolResponse::Flat(vec![flat_symbol("bar", p, 1, 3)]),
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();

        let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
        let titles: Vec<&str> = finder.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["foo", "bar"],
            "both servers' symbols, primary first"
        );
    }

    #[test]
    fn document_symbols_convert_each_with_its_servers_encoding() {
        use crate::{host::OffsetEncoding, symbol_finder::SymbolTarget};
        let mut h = TestHarness::with_size(80, 24);
        let (primary, secondary) = install_two_servers(&mut h, document_symbol_caps());
        primary.set_offset_encoding(OffsetEncoding::Utf8);
        secondary.set_offset_encoding(OffsetEncoding::Utf16);
        let root = seed(&mut h, &[("main.rs", "\u{e9}x\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let p = path.to_str().unwrap();

        // `x` sits at byte offset 2. `é` is two UTF-8 bytes but one UTF-16 unit,
        // so each server names `x`'s column in its own encoding.
        primary.set_document_symbols(
            p,
            DocumentSymbolResponse::Flat(vec![flat_symbol("utf8", p, 0, 2)]),
        );
        secondary.set_document_symbols(
            p,
            DocumentSymbolResponse::Flat(vec![flat_symbol("utf16", p, 0, 1)]),
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();

        let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
        let resolved: Vec<(&str, usize)> = finder
            .entries
            .iter()
            .map(|e| {
                let SymbolTarget::Offset(offset) = &e.target;
                (e.title.as_str(), *offset)
            })
            .collect();
        assert_eq!(
            resolved,
            vec![("utf8", 2), ("utf16", 2)],
            "each server's column resolves with its own encoding"
        );
    }

    #[test]
    fn workspace_symbols_merge_from_two_servers() {
        use lsp_types::{OneOf, ServerCapabilities, SymbolKind};
        let mut h = TestHarness::with_size(80, 24);
        let (primary, secondary) = install_two_servers(
            &mut h,
            ServerCapabilities {
                workspace_symbol_provider: Some(OneOf::Left(true)),
                ..ServerCapabilities::default()
            },
        );
        let root = seed(
            &mut h,
            &[("main.rs", "fn foo() {}\n"), ("lib.rs", "fn bar() {}\n")],
        );
        let main = root.join("main.rs");
        let lib = root.join("lib.rs");
        open_buffer(&mut h, main.clone());
        primary.add_workspace_symbol(
            "f",
            "foo",
            SymbolKind::FUNCTION,
            main.to_str().unwrap(),
            0,
            3,
        );
        secondary.add_workspace_symbol(
            "f",
            "bar",
            SymbolKind::FUNCTION,
            lib.to_str().unwrap(),
            0,
            3,
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();
        h.type_keys("f");
        crate::action_handlers::lsp::workspace_symbol_submit(&mut h.stoat);
        h.settle();

        let picker = h
            .stoat
            .pending_workspace_symbol_picker
            .as_ref()
            .expect("picker open");
        let titles: Vec<&str> = picker.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["foo", "bar"],
            "both servers' symbols, primary first"
        );
    }

    fn enable_workspace_symbols(h: &TestHarness) {
        use lsp_types::{OneOf, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            workspace_symbol_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
    }

    #[test]
    fn workspace_symbol_unsupported_capability_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        open_buffer(&mut h, root.join("main.rs"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();
        assert!(h.stoat.workspace_symbol_input.is_none());
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn workspace_symbol_opens_input_modal() {
        let mut h = TestHarness::with_size(80, 24);
        enable_workspace_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        open_buffer(&mut h, root.join("main.rs"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();
        assert!(h.stoat.workspace_symbol_input.is_some());
        assert_eq!(h.stoat.focused_mode(), "insert");
    }

    #[test]
    fn workspace_symbol_submit_populates_picker() {
        use lsp_types::SymbolKind;
        let mut h = TestHarness::with_size(80, 24);
        enable_workspace_symbols(&h);
        let root = seed(
            &mut h,
            &[("main.rs", "fn foo() {}\n"), ("lib.rs", "fn bar() {}\n")],
        );
        let main = root.join("main.rs");
        let lib = root.join("lib.rs");
        open_buffer(&mut h, main.clone());
        h.fake_lsp().add_workspace_symbol(
            "f",
            "foo",
            SymbolKind::FUNCTION,
            main.to_str().unwrap(),
            0,
            3,
        );
        h.fake_lsp().add_workspace_symbol(
            "f",
            "bar",
            SymbolKind::FUNCTION,
            lib.to_str().unwrap(),
            0,
            3,
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();
        h.type_keys("f");
        crate::action_handlers::lsp::workspace_symbol_submit(&mut h.stoat);
        h.settle();
        let picker = h
            .stoat
            .pending_workspace_symbol_picker
            .as_ref()
            .expect("picker open");
        let titles: Vec<&str> = picker.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["foo", "bar"]);
    }

    #[test]
    fn workspace_symbol_submit_handles_nested_response() {
        use lsp_types::{
            Location, OneOf, Position as LspPosition, Range as LspRange, SymbolKind, Uri,
            WorkspaceLocation, WorkspaceSymbol, WorkspaceSymbolResponse,
        };
        use std::str::FromStr;
        let mut h = TestHarness::with_size(80, 24);
        enable_workspace_symbols(&h);
        let root = seed(
            &mut h,
            &[("main.rs", "fn foo() {}\n"), ("lib.rs", "fn bar() {}\n")],
        );
        let main = root.join("main.rs");
        let lib = root.join("lib.rs");
        open_buffer(&mut h, main.clone());
        let main_uri = Uri::from_str(&format!("file://{}", main.to_str().unwrap())).unwrap();
        let lib_uri = Uri::from_str(&format!("file://{}", lib.to_str().unwrap())).unwrap();
        let nested = WorkspaceSymbolResponse::Nested(vec![
            WorkspaceSymbol {
                name: "foo".to_string(),
                kind: SymbolKind::FUNCTION,
                tags: None,
                container_name: None,
                location: OneOf::Left(Location::new(
                    main_uri,
                    LspRange::new(LspPosition::new(0, 3), LspPosition::new(0, 6)),
                )),
                data: None,
            },
            WorkspaceSymbol {
                name: "bar".to_string(),
                kind: SymbolKind::FUNCTION,
                tags: None,
                container_name: None,
                location: OneOf::Right(WorkspaceLocation { uri: lib_uri }),
                data: None,
            },
        ]);
        h.fake_lsp().set_workspace_symbol_response("f", nested);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();
        h.type_keys("f");
        crate::action_handlers::lsp::workspace_symbol_submit(&mut h.stoat);
        h.settle();
        let picker = h
            .stoat
            .pending_workspace_symbol_picker
            .as_ref()
            .expect("picker open");
        let entries: Vec<(&str, &Path, LspPosition)> = picker
            .entries
            .iter()
            .map(|e| (e.title.as_str(), e.path.as_path(), e.position))
            .collect();
        assert_eq!(
            entries,
            vec![
                ("foo", main.as_path(), LspPosition::new(0, 3)),
                ("bar", lib.as_path(), LspPosition::new(0, 0)),
            ]
        );
    }

    #[test]
    fn workspace_symbol_pick_opens_target_file() {
        use lsp_types::SymbolKind;
        let mut h = TestHarness::with_size(80, 24);
        enable_workspace_symbols(&h);
        let root = seed(
            &mut h,
            &[("main.rs", "fn foo() {}\n"), ("lib.rs", "fn bar() {}\n")],
        );
        let main = root.join("main.rs");
        let lib = root.join("lib.rs");
        open_buffer(&mut h, main.clone());
        h.fake_lsp().add_workspace_symbol(
            "bar",
            "bar",
            SymbolKind::FUNCTION,
            lib.to_str().unwrap(),
            0,
            3,
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();
        h.type_keys("b a r");
        crate::action_handlers::lsp::workspace_symbol_submit(&mut h.stoat);
        h.settle();
        crate::action_handlers::lsp::pick_workspace_symbol(&mut h.stoat, 0);
        let ws = h.stoat.active_workspace();
        let pane = ws.panes.pane(ws.panes.focus());
        let crate::pane::View::Editor(editor_id) = pane.view else {
            panic!("not an editor");
        };
        let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;
        let path = ws
            .buffers
            .path_for(buffer_id)
            .expect("buffer path")
            .to_path_buf();
        assert_eq!(path, lib);
        assert_eq!(cursor_offset(&mut h), 3);
    }

    #[test]
    fn workspace_symbol_navigates_with_jk_and_picks_with_enter() {
        use lsp_types::SymbolKind;
        let mut h = TestHarness::with_size(80, 24);
        enable_workspace_symbols(&h);
        let mut files: Vec<(&str, &str)> = (0..12)
            .map(|i| {
                let path = Box::leak(format!("f{i}.rs").into_boxed_str()) as &str;
                (path, "fn target() {}\n")
            })
            .collect();
        files.push(("anchor.rs", "fn anchor() {}\n"));
        let root = seed(&mut h, &files);
        let anchor_path = root.join("anchor.rs");
        open_buffer(&mut h, anchor_path);
        for i in 0..12 {
            let p = root.join(format!("f{i}.rs"));
            h.fake_lsp().add_workspace_symbol(
                "t",
                "target",
                SymbolKind::FUNCTION,
                p.to_str().unwrap(),
                0,
                3,
            );
        }
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();
        h.type_keys("t");
        crate::action_handlers::lsp::workspace_symbol_submit(&mut h.stoat);
        h.settle();

        for _ in 0..11 {
            h.type_keys("j");
        }
        let picker = h
            .stoat
            .pending_workspace_symbol_picker
            .as_ref()
            .expect("picker");
        assert_eq!(picker.selected_idx, 11);

        h.type_keys("enter");
        let ws = h.stoat.active_workspace();
        let pane = ws.panes.pane(ws.panes.focus());
        let crate::pane::View::Editor(eid) = pane.view else {
            panic!("not an editor");
        };
        let buffer_id = ws.editors.get(eid).expect("editor").buffer_id;
        let path = ws.buffers.path_for(buffer_id).expect("path").to_path_buf();
        assert_eq!(path, root.join("f11.rs"));
        assert_eq!(cursor_offset(&mut h), 3);
    }

    #[test]
    fn workspace_symbol_cancel_clears_modal() {
        let mut h = TestHarness::with_size(80, 24);
        enable_workspace_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        open_buffer(&mut h, root.join("main.rs"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();
        assert!(h.stoat.workspace_symbol_input.is_some());
        let cancelled = crate::action_handlers::lsp::workspace_symbol_cancel(&mut h.stoat);
        assert!(cancelled);
        assert!(h.stoat.workspace_symbol_input.is_none());
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn space_l_shift_s_triggers_workspace_symbol() {
        let mut h = TestHarness::with_size(80, 24);
        enable_workspace_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        open_buffer(&mut h, root.join("main.rs"));
        h.type_keys("space l shift-s");
        h.settle();
        assert!(h.stoat.workspace_symbol_input.is_some());
        assert_eq!(h.stoat.focused_mode(), "insert");
    }

    #[test]
    fn snapshot_workspace_symbol_input() {
        let mut h = TestHarness::with_size(40, 12);
        enable_workspace_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        open_buffer(&mut h, root.join("main.rs"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
        h.settle();
        h.assert_snapshot("snapshot_workspace_symbol_input");
    }

    fn enable_format(h: &TestHarness) {
        use lsp_types::{OneOf, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            document_formatting_provider: Some(OneOf::Left(true)),
            document_range_formatting_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
    }

    fn format_text_edit(
        line: u32,
        col: u32,
        end_line: u32,
        end_col: u32,
        new: &str,
    ) -> lsp_types::TextEdit {
        use lsp_types::{Position as LspPosition, Range as LspRange, TextEdit};
        TextEdit {
            range: LspRange::new(
                LspPosition::new(line, col),
                LspPosition::new(end_line, end_col),
            ),
            new_text: new.to_string(),
        }
    }

    #[test]
    fn format_unsupported_capability_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_range_formatting(
            path.to_str().unwrap(),
            vec![format_text_edit(0, 0, 1, 0, "fn foo() {}\n")],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::FormatSelections);
        h.settle();
        assert_eq!(buffer_text(&h, &path), "fn  foo (){}\n");
    }

    #[test]
    fn format_no_response_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        enable_format(&h);
        let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::FormatSelections);
        h.settle();
        assert_eq!(buffer_text(&h, &path), "fn  foo (){}\n");
    }

    #[test]
    fn format_applies_returned_edits() {
        let mut h = TestHarness::with_size(80, 24);
        enable_format(&h);
        let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_range_formatting(
            path.to_str().unwrap(),
            vec![format_text_edit(0, 0, 1, 0, "fn foo() {}\n")],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::FormatSelections);
        h.settle();
        assert_eq!(buffer_text(&h, &path), "fn foo() {}\n");
    }

    #[test]
    fn format_equals_keystroke_triggers() {
        let mut h = TestHarness::with_size(80, 24);
        enable_format(&h);
        let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_range_formatting(
            path.to_str().unwrap(),
            vec![format_text_edit(0, 0, 1, 0, "fn foo() {}\n")],
        );
        h.type_keys("=");
        h.settle();
        assert_eq!(buffer_text(&h, &path), "fn foo() {}\n");
    }

    #[test]
    fn format_document_applies_returned_edits() {
        let mut h = TestHarness::with_size(80, 24);
        enable_format(&h);
        let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_formatting(
            path.to_str().unwrap(),
            vec![format_text_edit(0, 0, 1, 0, "fn foo() {}\n")],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Format);
        h.settle();
        assert_eq!(buffer_text(&h, &path), "fn foo() {}\n");
    }

    #[test]
    fn format_document_unsupported_capability_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_formatting(
            path.to_str().unwrap(),
            vec![format_text_edit(0, 0, 1, 0, "fn foo() {}\n")],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Format);
        h.settle();
        assert_eq!(buffer_text(&h, &path), "fn  foo (){}\n");
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: server does not support format"),
        );
    }

    #[test]
    fn space_l_f_formats_document() {
        let mut h = TestHarness::with_size(80, 24);
        enable_format(&h);
        let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_formatting(
            path.to_str().unwrap(),
            vec![format_text_edit(0, 0, 1, 0, "fn foo() {}\n")],
        );
        h.type_keys("space l f");
        h.settle();
        assert_eq!(buffer_text(&h, &path), "fn foo() {}\n");
    }

    fn enable_inlay_hints(h: &TestHarness) {
        use lsp_types::{OneOf, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            inlay_hint_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
    }

    fn type_hint(line: u32, col: u32, label: &str) -> lsp_types::InlayHint {
        use lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, Position};
        InlayHint {
            position: Position::new(line, col),
            label: InlayHintLabel::String(label.to_string()),
            kind: Some(InlayHintKind::TYPE),
            text_edits: None,
            tooltip: None,
            padding_left: None,
            padding_right: None,
            data: None,
        }
    }

    fn hint_ids_len(h: &mut TestHarness) -> usize {
        crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .hint_inlay_ids
            .len()
    }

    fn focused_editor_id(h: &TestHarness) -> crate::editor_state::EditorId {
        let ws = h.stoat.active_workspace();
        match ws.panes.pane(ws.panes.focus()).view {
            crate::pane::View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        }
    }

    fn editor_hint_ids_len(h: &TestHarness, id: crate::editor_state::EditorId) -> usize {
        h.stoat
            .active_workspace()
            .editors
            .get(id)
            .expect("editor")
            .hint_inlay_ids
            .len()
    }

    #[test]
    fn snapshot_inlay_hints_render_when_enabled() {
        let mut h = TestHarness::with_size(40, 8);
        enable_inlay_hints(&h);
        let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_range_inlay_hints(path.to_str().unwrap(), vec![type_hint(0, 5, ": u32")]);
        h.capture("prime");
        h.type_keys("space l h");
        h.advance_clock(Duration::from_millis(150));
        h.assert_snapshot("inlay_hints_enabled");
    }

    #[test]
    fn inlay_hints_toggle_off_clears_inlays() {
        let mut h = TestHarness::with_size(40, 8);
        enable_inlay_hints(&h);
        let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_range_inlay_hints(path.to_str().unwrap(), vec![type_hint(0, 5, ": u32")]);
        h.capture("prime");
        h.type_keys("space l h");
        h.advance_clock(Duration::from_millis(150));
        assert_eq!(hint_ids_len(&mut h), 1);

        h.type_keys("space l h");
        assert_eq!(hint_ids_len(&mut h), 0);
    }

    #[test]
    fn inlay_hints_toggle_off_clears_unfocused_editors() {
        let mut h = TestHarness::with_size(40, 8);
        enable_inlay_hints(&h);
        let root = seed(&mut h, &[("a.rs", "let x = 1\n"), ("b.rs", "let y = 2\n")]);
        let a = root.join("a.rs");
        open_buffer(&mut h, a.clone());
        h.fake_lsp()
            .set_range_inlay_hints(a.to_str().unwrap(), vec![type_hint(0, 5, ": u32")]);
        h.type_keys("space l h");
        h.advance_clock(Duration::from_millis(150));
        let a_editor = focused_editor_id(&h);
        assert_eq!(editor_hint_ids_len(&h, a_editor), 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        open_buffer(&mut h, root.join("b.rs"));
        assert_ne!(
            focused_editor_id(&h),
            a_editor,
            "opening b.rs in the split moves focus off a.rs's editor"
        );

        h.type_keys("space l h");
        assert_eq!(
            editor_hint_ids_len(&h, a_editor),
            0,
            "toggle-off clears hints from the unfocused a.rs editor"
        );
    }

    #[test]
    fn inlay_hints_toggle_on_requests_without_the_debounce() {
        let mut h = TestHarness::with_size(40, 8);
        enable_inlay_hints(&h);
        let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_range_inlay_hints(path.to_str().unwrap(), vec![type_hint(0, 5, ": u32")]);
        h.capture("prime");
        h.type_keys("space l h");
        h.settle();
        assert_eq!(
            hint_ids_len(&mut h),
            1,
            "toggle-on applies hints without advancing the debounce clock"
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("inlay hints on"),
            "toggle-on acknowledges in the status bar"
        );
    }

    #[test]
    fn inlay_hints_toggle_off_acknowledges() {
        let mut h = TestHarness::with_size(40, 8);
        enable_inlay_hints(&h);
        let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_range_inlay_hints(path.to_str().unwrap(), vec![type_hint(0, 5, ": u32")]);
        h.capture("prime");
        h.type_keys("space l h");
        h.settle();
        h.type_keys("space l h");
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("inlay hints off"),
            "toggle-off acknowledges in the status bar"
        );
    }

    #[test]
    fn inlay_hints_toggle_on_without_a_capable_server_reports_why() {
        let mut h = TestHarness::with_size(40, 8);
        let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.type_keys("space l h");
        h.settle();
        assert_eq!(
            hint_ids_len(&mut h),
            0,
            "no capable server means no hints are applied"
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: server does not support inlay hints"),
            "toggle-on with no inlay capability reports why"
        );
    }

    #[test]
    fn inlay_hints_refresh_after_edit() {
        let mut h = TestHarness::with_size(40, 8);
        enable_inlay_hints(&h);
        let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let p = path.to_str().unwrap();
        h.fake_lsp()
            .set_range_inlay_hints(p, vec![type_hint(0, 5, ": u32")]);
        h.capture("prime");
        h.type_keys("space l h");
        h.advance_clock(Duration::from_millis(150));
        assert_eq!(hint_ids_len(&mut h), 1);

        h.fake_lsp()
            .set_range_inlay_hints(p, vec![type_hint(0, 5, ": u32"), type_hint(0, 8, ": b")]);
        h.type_keys("i");
        h.type_text("z");
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(150));
        assert_eq!(hint_ids_len(&mut h), 2);
    }

    fn enable_document_highlight(h: &TestHarness) {
        use lsp_types::{OneOf, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            document_highlight_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
    }

    fn doc_highlight_count(
        h: &mut TestHarness,
        layer: crate::display_map::HighlightLayer,
    ) -> usize {
        let editor =
            crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        snapshot
            .text_highlights()
            .get(&crate::display_map::HighlightKey::layer(layer))
            .map(|hl| hl.1.len())
            .unwrap_or(0)
    }

    #[test]
    fn snapshot_document_highlight_read_write() {
        use lsp_types::DocumentHighlightKind;
        let mut h = TestHarness::with_size(24, 4);
        enable_document_highlight(&h);
        let root = seed(&mut h, &[("main.rs", "foo bar foo\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_highlights(
            path.to_str().unwrap(),
            0,
            0,
            &[
                (0, 0, 3, DocumentHighlightKind::WRITE),
                (0, 8, 11, DocumentHighlightKind::READ),
            ],
        );
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(250));
        h.assert_snapshot("document_highlight_read_write");
    }

    #[test]
    fn document_highlight_re_requests_on_cursor_move() {
        use crate::display_map::HighlightLayer;
        use lsp_types::DocumentHighlightKind;
        let mut h = TestHarness::with_size(24, 4);
        enable_document_highlight(&h);
        let root = seed(&mut h, &[("main.rs", "foo bar foo\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let p = path.to_str().unwrap();
        h.fake_lsp().set_highlights(
            p,
            0,
            0,
            &[
                (0, 0, 3, DocumentHighlightKind::READ),
                (0, 8, 11, DocumentHighlightKind::READ),
            ],
        );
        h.fake_lsp()
            .set_highlights(p, 0, 1, &[(0, 0, 3, DocumentHighlightKind::READ)]);

        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(250));
        assert_eq!(
            doc_highlight_count(&mut h, HighlightLayer::DocumentHighlightRead),
            2
        );

        h.type_keys("l");
        h.advance_clock(Duration::from_millis(250));
        assert_eq!(
            doc_highlight_count(&mut h, HighlightLayer::DocumentHighlightRead),
            1
        );
    }

    #[test]
    fn document_highlight_cleared_in_insert_mode() {
        use crate::display_map::HighlightLayer;
        use lsp_types::DocumentHighlightKind;
        let mut h = TestHarness::with_size(24, 4);
        enable_document_highlight(&h);
        let root = seed(&mut h, &[("main.rs", "foo bar foo\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_highlights(
            path.to_str().unwrap(),
            0,
            0,
            &[
                (0, 0, 3, DocumentHighlightKind::READ),
                (0, 8, 11, DocumentHighlightKind::READ),
            ],
        );
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(250));
        assert_eq!(
            doc_highlight_count(&mut h, HighlightLayer::DocumentHighlightRead),
            2
        );

        h.type_keys("i");
        assert_eq!(
            doc_highlight_count(&mut h, HighlightLayer::DocumentHighlightRead),
            0
        );
    }

    fn enable_pull_diagnostics(h: &TestHarness) {
        use lsp_types::{DiagnosticOptions, DiagnosticServerCapabilities, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            diagnostic_provider: Some(DiagnosticServerCapabilities::Options(
                DiagnosticOptions::default(),
            )),
            ..Default::default()
        });
    }

    fn full_report(
        diagnostics: Vec<lsp_types::Diagnostic>,
        result_id: &str,
    ) -> lsp_types::DocumentDiagnosticReportResult {
        use lsp_types::{
            DocumentDiagnosticReport, DocumentDiagnosticReportResult, FullDocumentDiagnosticReport,
            RelatedFullDocumentDiagnosticReport,
        };
        DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(
            RelatedFullDocumentDiagnosticReport {
                related_documents: None,
                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                    result_id: Some(result_id.to_string()),
                    items: diagnostics,
                },
            },
        ))
    }

    fn unchanged_report(result_id: &str) -> lsp_types::DocumentDiagnosticReportResult {
        use lsp_types::{
            DocumentDiagnosticReport, DocumentDiagnosticReportResult,
            RelatedUnchangedDocumentDiagnosticReport, UnchangedDocumentDiagnosticReport,
        };
        DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Unchanged(
            RelatedUnchangedDocumentDiagnosticReport {
                related_documents: None,
                unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                    result_id: result_id.to_string(),
                },
            },
        ))
    }

    #[test]
    fn pull_diagnostics_on_open_renders() {
        let mut h = TestHarness::with_size(80, 24);
        enable_pull_diagnostics(&h);
        let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_diagnostic(
            path.to_str().unwrap(),
            full_report(vec![diag(0, 4, "unused")], "rev-1"),
        );
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(350));
        assert_eq!(h.stoat.diagnostics.get(&path).len(), 1);
        assert_eq!(h.stoat.diagnostics.get(&path)[0].message, "unused");
    }

    #[test]
    fn pull_diagnostics_unchanged_keeps_set() {
        let mut h = TestHarness::with_size(80, 24);
        enable_pull_diagnostics(&h);
        let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let p = path.to_str().unwrap();
        h.fake_lsp()
            .set_document_diagnostic(p, full_report(vec![diag(0, 4, "unused")], "rev-1"));
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(350));
        assert_eq!(h.stoat.diagnostics.get(&path).len(), 1);

        h.fake_lsp()
            .set_document_diagnostic(p, unchanged_report("rev-1"));
        edit_buffer(&mut h, 0..0, "// c\n");
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(350));
        assert_eq!(h.stoat.diagnostics.get(&path).len(), 1);
        assert_eq!(h.stoat.diagnostics.get(&path)[0].message, "unused");
    }

    #[test]
    fn pull_diagnostics_push_only_server_never_pulls() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_diagnostic(
            path.to_str().unwrap(),
            full_report(vec![diag(0, 4, "unused")], "rev-1"),
        );
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(350));
        assert!(h.stoat.diagnostics.get(&path).is_empty());
    }

    #[test]
    fn decode_semantic_tokens_accumulates_deltas() {
        use lsp_types::{SemanticToken, SemanticTokenType};
        let legend = vec![
            SemanticTokenType::new("keyword"),
            SemanticTokenType::new("function"),
            SemanticTokenType::new("boolean"),
            SemanticTokenType::new("namespace"),
        ];
        let tok = |delta_line, delta_start, length, token_type| SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type,
            token_modifiers_bitset: 0,
        };
        let data = vec![
            tok(0, 0, 3, 0),
            tok(0, 4, 2, 1),
            tok(1, 2, 5, 0),
            tok(0, 6, 1, 2),
            tok(0, 8, 4, 3),
        ];
        let decoded = super::decode_semantic_tokens(&data, &legend);
        let want = |line, start, length, scope, kind| super::DecodedToken {
            line,
            start,
            length,
            scope,
            kind,
        };
        // The boolean token maps to neither a scope nor a kind and is dropped.
        // The namespace token has no highlight scope but keeps its Symbol kind.
        assert_eq!(
            decoded,
            vec![
                want(0, 0, 3, Some("keyword"), None),
                want(0, 4, 2, Some("function"), Some(LspSymbolKind::Function)),
                want(1, 2, 5, Some("keyword"), None),
                want(1, 16, 4, None, Some(LspSymbolKind::Symbol)),
            ]
        );
    }

    #[test]
    fn lsp_token_scope_maps_standard_types() {
        assert_eq!(super::lsp_token_scope("function"), Some("function"));
        assert_eq!(super::lsp_token_scope("method"), Some("function"));
        assert_eq!(
            super::lsp_token_scope("parameter"),
            Some("variable.parameter")
        );
        assert_eq!(super::lsp_token_scope("struct"), Some("type"));
        assert_eq!(super::lsp_token_scope("regexp"), None);
    }

    #[test]
    fn lsp_symbol_kind_classifies_token_types() {
        use super::lsp_symbol_kind;
        assert_eq!(lsp_symbol_kind("interface"), Some(LspSymbolKind::Trait));
        assert_eq!(lsp_symbol_kind("struct"), Some(LspSymbolKind::Type));
        assert_eq!(lsp_symbol_kind("enum"), Some(LspSymbolKind::Type));
        assert_eq!(lsp_symbol_kind("method"), Some(LspSymbolKind::Function));
        assert_eq!(lsp_symbol_kind("parameter"), Some(LspSymbolKind::Value));
        assert_eq!(lsp_symbol_kind("namespace"), Some(LspSymbolKind::Symbol));
        assert_eq!(lsp_symbol_kind("keyword"), None);
        assert_eq!(lsp_symbol_kind("string"), None);
    }

    fn enable_semantic_tokens(h: &TestHarness) {
        use lsp_types::{
            SemanticTokenType, SemanticTokensFullOptions, SemanticTokensLegend,
            SemanticTokensOptions, SemanticTokensServerCapabilities, ServerCapabilities,
        };
        h.fake_lsp().set_capabilities(ServerCapabilities {
            semantic_tokens_provider: Some(
                SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
                    legend: SemanticTokensLegend {
                        token_types: vec![SemanticTokenType::new("function")],
                        token_modifiers: vec![],
                    },
                    full: Some(SemanticTokensFullOptions::Bool(true)),
                    range: None,
                    work_done_progress_options: Default::default(),
                }),
            ),
            ..Default::default()
        });
    }

    fn lsp_token_count(h: &mut TestHarness) -> usize {
        let editor =
            crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        snapshot
            .lsp_token_highlights()
            .values()
            .map(|channel| channel.tokens.len())
            .sum()
    }

    #[test]
    fn snapshot_semantic_tokens_recolor_over_tree_sitter() {
        use lsp_types::{SemanticToken, SemanticTokens, SemanticTokensResult};
        let mut h = TestHarness::with_size(24, 4);
        enable_semantic_tokens(&h);
        let root = seed(&mut h, &[("main.rs", "let x = y\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_semantic_tokens_full(
            path.to_str().unwrap(),
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![SemanticToken {
                    delta_line: 0,
                    delta_start: 8,
                    length: 1,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                }],
            }),
        );
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(550));
        assert_eq!(lsp_token_count(&mut h), 1);
        h.assert_snapshot("semantic_tokens_recolor");
    }

    fn tree_sitter_token_count(h: &mut TestHarness) -> usize {
        let editor =
            crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        snapshot
            .semantic_token_highlights()
            .values()
            .map(|channel| channel.tokens.len())
            .sum()
    }

    #[test]
    fn switching_back_keeps_tree_sitter_highlights_on_first_frame() {
        let mut h = TestHarness::with_size(24, 4);
        let root = seed(&mut h, &[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);

        // A render cycle drives the parse so A's tokens land in the registry and
        // on its editor.
        open_buffer(&mut h, root.join("a.rs"));
        h.snapshot();
        assert!(tree_sitter_token_count(&mut h) > 0, "file A parses on open");

        open_buffer(&mut h, root.join("b.rs"));
        h.snapshot();

        // Switch back to A with no render or parse cycle in between. The parse
        // pipeline skips a version-current buffer, so the fresh editor is styled
        // only if it was seeded from the registry's retained tokens.
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("a.rs"),
            },
        );
        assert!(
            tree_sitter_token_count(&mut h) > 0,
            "re-shown buffer is styled on the first frame after switch-back"
        );
    }

    fn one_full_token(path: &Path) -> impl Fn(&TestHarness) + '_ {
        use lsp_types::{SemanticToken, SemanticTokens, SemanticTokensResult};
        move |h: &TestHarness| {
            h.fake_lsp().set_semantic_tokens_full(
                path.to_str().unwrap(),
                SemanticTokensResult::Tokens(SemanticTokens {
                    result_id: None,
                    data: vec![SemanticToken {
                        delta_line: 0,
                        delta_start: 8,
                        length: 1,
                        token_type: 0,
                        token_modifiers_bitset: 0,
                    }],
                }),
            );
        }
    }

    #[test]
    fn switching_back_keeps_lsp_tokens_on_first_frame() {
        let mut h = TestHarness::with_size(24, 4);
        enable_semantic_tokens(&h);
        let root = seed(&mut h, &[("a.rs", "let x = y\n"), ("b.rs", "let z = w\n")]);
        let path_a = root.join("a.rs");

        open_buffer(&mut h, path_a.clone());
        one_full_token(&path_a)(&h);
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(550));
        assert_eq!(lsp_token_count(&mut h), 1, "A receives LSP tokens");

        open_buffer(&mut h, root.join("b.rs"));

        // Switch back to A with no debounce cycle. The fresh editor keeps the
        // LSP highlighting only if seeded from the registry's cached tokens.
        crate::action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path_a });
        assert!(
            lsp_token_count(&mut h) > 0,
            "re-shown buffer keeps LSP tokens on the first frame"
        );
    }

    #[test]
    fn cached_lsp_tokens_skip_the_re_request() {
        let mut h = TestHarness::with_size(24, 4);
        enable_semantic_tokens(&h);
        let root = seed(&mut h, &[("a.rs", "let x = y\n")]);
        let path_a = root.join("a.rs");

        open_buffer(&mut h, path_a.clone());
        one_full_token(&path_a)(&h);
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(550));
        assert_eq!(lsp_token_count(&mut h), 1);

        // Re-triggering for the same unchanged version reinstalls the cached
        // tokens without spawning a second request.
        h.stoat.last_semantic_tokens_key = None;
        super::semantic_tokens_trigger(&mut h.stoat);
        assert!(
            h.stoat.pending_semantic_tokens.is_none(),
            "a version-current cache hit spawns no request"
        );
        assert_eq!(lsp_token_count(&mut h), 1, "cached tokens are reinstalled");
    }

    fn enable_folding_range(h: &TestHarness) {
        use lsp_types::{FoldingRangeProviderCapability, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
            ..Default::default()
        });
    }

    fn crease_point_ranges(h: &mut TestHarness) -> Vec<std::ops::Range<stoat_text::Point>> {
        let editor =
            crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let resolve =
            |a: &stoat_text::Anchor| buf_snap.rope().offset_to_point(buf_snap.resolve_anchor(a));
        snapshot
            .crease_snapshot()
            .crease_items_with_offsets(&resolve)
            .into_iter()
            .map(|(_, range)| range)
            .collect()
    }

    #[test]
    fn folding_ranges_land_as_creases() {
        use lsp_types::FoldingRange;
        let mut h = TestHarness::with_size(40, 10);
        enable_folding_range(&h);
        let root = seed(&mut h, &[("main.rs", "fn a() {\n    x;\n}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_folding_ranges(
            path.to_str().unwrap(),
            vec![FoldingRange {
                start_line: 0,
                start_character: None,
                end_line: 2,
                end_character: None,
                kind: None,
                collapsed_text: None,
            }],
        );
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(550));
        assert_eq!(
            crease_point_ranges(&mut h),
            vec![stoat_text::Point::new(0, 8)..stoat_text::Point::new(2, 1)]
        );
    }
}

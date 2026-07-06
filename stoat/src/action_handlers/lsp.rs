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
    app::{Stoat, UpdateEffect},
    buffer::BufferId,
    display_map::{
        syntax_theme, DisplayPoint, DisplaySnapshot, HighlightKey, HighlightLayer, HighlightStyle,
        HighlightStyleInterner, InlayKind, SemanticTokenHighlight,
    },
    host::{LanguageServerFeature, LocalLsp, LspHost, LspTranscript, OffsetEncoding},
    location_picker::{LocationEntry, LocationPicker},
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
    MarkedString, OneOf, ParameterLabel, Position, PrepareRenameResponse, Range, ReferenceContext,
    ReferenceParams, RenameParams, SemanticToken, SemanticTokenType, SemanticTokensParams,
    SemanticTokensResult, SemanticTokensServerCapabilities, ServerInfo, SignatureHelp,
    SignatureHelpParams, SignatureInformation, SymbolInformation, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit, VersionedTextDocumentIdentifier,
    WorkDoneProgressParams, WorkspaceEdit, WorkspaceSymbol, WorkspaceSymbolParams,
    WorkspaceSymbolResponse,
};
use serde_json::{json, Value};
use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use stoat_log::TextProtoLog;
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
    let encoding = stoat.lsp_host.offset_encoding();
    let (cursor_offset, buffer_id, rope) = {
        let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let head = editor.selections.newest_anchor().head();
        let offset = buffer_snapshot.resolve_anchor(&head);
        (offset, editor.buffer_id, buffer_snapshot.rope().clone())
    };

    let path = match stoat.active_workspace().buffers.path_for(buffer_id) {
        Some(p) => p.to_path_buf(),
        None => return UpdateEffect::None,
    };

    let mut offsets: Vec<usize> = stoat
        .diagnostics
        .get(&path)
        .iter()
        .map(|d| crate::lsp::util::lsp_pos_to_byte_offset(&rope, d.range.start, encoding))
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
    let language_id = stoat
        .active_workspace()
        .buffers
        .language_for(buffer_id)
        .map(|lang| lang.name.to_string())
        .unwrap_or_else(|| "plaintext".to_string());
    let buffer_version = stoat
        .active_workspace()
        .buffers
        .get(buffer_id)
        .map(|b| b.read().expect("buffer lock").version())
        .unwrap_or(0);
    stoat.lsp_buffer_versions.insert(buffer_id, buffer_version);
    stoat.lsp_doc_versions.insert(buffer_id, 0);
    stoat
        .lsp_last_delivered_text
        .lock()
        .expect("lsp text mutex")
        .insert(buffer_id, Arc::new(text.to_string()));
    stoat
        .lsp_last_delivered_buffer_version
        .lock()
        .expect("lsp version mutex")
        .insert(buffer_id, buffer_version);
    let params = DidOpenTextDocumentParams {
        text_document: TextDocumentItem {
            uri,
            language_id,
            version: 0,
            text: text.to_string(),
        },
    };
    let lsp = stoat.lsp_host.clone();
    stoat
        .executor
        .spawn(async move {
            if let Err(err) = lsp.did_open(params).await {
                tracing::warn!(target: "stoat::lsp", ?err, "did_open notification failed");
            }
        })
        .detach();
}

/// Launch a language server for `buffer_id`'s language the first time a
/// buffer that has one opens, replacing the [`crate::host::NoopLsp`]
/// placeholder for the rest of the session.
///
/// No-op unless auto-spawn is enabled, the current host is still the
/// [`crate::host::NoopLsp`] placeholder, the buffer's language has a
/// known [`crate::lsp::servers::server_command`], and no spawn has been
/// attempted yet this session. The binary opts into auto-spawn via
/// [`Stoat::set_lsp_auto_spawn`]. Tests leave it off, so the placeholder
/// never performs IO.
///
/// The spawn plus `initialize` handshake runs detached on the workspace
/// [`Stoat::executor`]. The ready host is parked in
/// [`Stoat::pending_lsp_host`] for [`Stoat::update`] to install. A spawn
/// or handshake failure is logged and leaves the placeholder in place
/// with no retry.
fn maybe_spawn_language_server(stoat: &mut Stoat, buffer_id: BufferId) {
    if !stoat.lsp_auto_spawn || stoat.lsp_spawn_attempted || !stoat.lsp_host.is_noop() {
        return;
    }
    let Some(language) = stoat.active_workspace().buffers.language_for(buffer_id) else {
        return;
    };
    let Some((command, args)) =
        crate::lsp::servers::resolve_server_command(&stoat.settings, language.name)
    else {
        return;
    };
    stoat.lsp_spawn_attempted = true;

    let root_uri = path_to_uri(&stoat.active_workspace().git_root);
    let slot = stoat.pending_lsp_host.clone();
    let wake = stoat.redraw_notify.clone();
    let transcript = if stoat.settings.text_proto_log == Some(true) {
        match create_lsp_transcript() {
            Ok(transcript) => Some(transcript),
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "text_proto_log transcript disabled");
                None
            },
        }
    } else {
        None
    };

    stoat
        .executor
        .spawn(async move {
            let host: Arc<dyn LspHost> = match LocalLsp::spawn(&command, &args, transcript, wake) {
                Ok(host) => Arc::new(host),
                Err(err) => {
                    tracing::warn!(target: "stoat::lsp", ?err, %command, "language server spawn failed");
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
                    return;
                },
            }
            *slot.lock().expect("pending lsp host mutex") = Some(host);
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

/// Create the paired protocol transcripts for `text_proto_log`, keyed by
/// stoat's pid so they correlate with `stoat-<pid>.log`.
///
/// Writes `lsp-<pid>.tx.jsonl` (frames sent to the server) and
/// `lsp-<pid>.rx.jsonl` (frames received) under the shared log directory,
/// creating that directory if it does not exist.
fn create_lsp_transcript() -> io::Result<LspTranscript> {
    let dir = stoat_log::log_dir()?;
    std::fs::create_dir_all(&dir)?;
    let pid = std::process::id();
    let tx = TextProtoLog::create_at(&dir.join(format!("lsp-{pid}.tx.jsonl")))?;
    let rx = TextProtoLog::create_at(&dir.join(format!("lsp-{pid}.rx.jsonl")))?;
    Ok(LspTranscript { tx, rx })
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
    let sync_kind = resolve_sync_kind(&stoat.lsp_host.capabilities().text_document_sync);
    if !matches!(
        sync_kind,
        TextDocumentSyncKind::FULL | TextDocumentSyncKind::INCREMENTAL
    ) {
        for id in stoat.lsp_opened.iter().copied().collect::<Vec<_>>() {
            if let Some(buffer) = stoat.active_workspace().buffers.get(id) {
                let v = buffer.read().expect("buffer lock").version();
                stoat.lsp_buffer_versions.insert(id, v);
            }
        }
        return;
    }

    let encoding = stoat.lsp_host.offset_encoding();

    let dispatches: Vec<DispatchPlan> = stoat
        .lsp_opened
        .iter()
        .copied()
        .filter_map(|id| build_dispatch_plan(stoat, id, sync_kind, encoding))
        .collect();

    for plan in dispatches {
        stoat
            .lsp_buffer_versions
            .insert(plan.id, plan.target_buffer_version);
        let lsp_version = stoat.lsp_doc_versions.entry(plan.id).or_insert(0);
        *lsp_version += 1;
        let lsp_version_value = *lsp_version;

        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: plan.uri,
                version: lsp_version_value,
            },
            content_changes: plan.content_changes,
        };

        let lsp = stoat.lsp_host.clone();
        let executor = stoat.executor.clone();
        let last_text = stoat.lsp_last_delivered_text.clone();
        let last_version = stoat.lsp_last_delivered_buffer_version.clone();
        let buffer_id = plan.id;
        let target_text = plan.target_text;
        let target_version = plan.target_buffer_version;

        let task = stoat.executor.spawn(async move {
            executor.timer(LSP_DID_CHANGE_DEBOUNCE).await;
            if let Err(err) = lsp.did_change(params).await {
                tracing::warn!(target: "stoat::lsp", ?err, "did_change notification failed");
                return;
            }
            last_text
                .lock()
                .expect("lsp text mutex")
                .insert(buffer_id, target_text);
            last_version
                .lock()
                .expect("lsp version mutex")
                .insert(buffer_id, target_version);
        });
        stoat.lsp_pending_changes.insert(plan.id, task);
    }
}

struct DispatchPlan {
    id: BufferId,
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
        id,
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
    if !stoat
        .lsp_host
        .supports_feature(LanguageServerFeature::GotoReference)
    {
        return crate::code_index::nav::goto_references(stoat);
    }

    let encoding = stoat.lsp_host.offset_encoding();
    let Some(site) = lsp_request_site(stoat) else {
        return UpdateEffect::None;
    };
    let Some(source_uri) = path_to_uri(&site.path) else {
        return UpdateEffect::None;
    };

    let position = crate::lsp::util::byte_offset_to_lsp_pos(&site.rope, site.offset, encoding);
    let params = ReferenceParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: source_uri },
            position,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: ReferenceContext {
            include_declaration: true,
        },
    };

    let lsp = stoat.lsp_host.clone();
    let fs = stoat.fs_host.clone();
    let source_path = site.path;
    let source_rope = site.rope;
    let task = stoat.executor.spawn(async move {
        let locations = match lsp.references(params).await {
            Ok(Some(locations)) => locations,
            Ok(None) => return Vec::new(),
            Err(err) => {
                tracing::warn!(
                    target: "stoat::lsp",
                    ?err,
                    "references request failed",
                );
                return Vec::new();
            },
        };
        resolve_goto_targets(
            GotoDefinitionResponse::Array(locations),
            &source_path,
            &source_rope,
            encoding,
            &*fs,
        )
    });
    stoat.pending_lsp_jump = Some(task);
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
        let head = editor.selections.newest_anchor().head();
        let offset = buf_snap.resolve_anchor(&head);
        (
            offset,
            editor.buffer_id,
            buf_snap.rope().clone(),
            editor.review_view.is_some(),
        )
    };

    if is_review {
        let (path, rope, offset) = review_lsp_source(stoat)?;
        Some(LspRequestSite { path, rope, offset })
    } else {
        let path = stoat
            .active_workspace()
            .buffers
            .path_for(buffer_id)
            .map(Path::to_path_buf)?;
        Some(LspRequestSite {
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
/// No-op when: the focused pane is not an editor; the buffer has no
/// path; or the server does not advertise the matching
/// [`LanguageServerFeature`]. Replacing the prior pending task drops
/// it, cancelling its spawned future -- only one in-flight jump is
/// tracked at a time.
fn lsp_jump(stoat: &mut Stoat, kind: LspJumpKind) -> UpdateEffect {
    if !stoat.lsp_host.supports_feature(kind.feature()) {
        return UpdateEffect::None;
    }

    let encoding = stoat.lsp_host.offset_encoding();
    let Some(site) = lsp_request_site(stoat) else {
        return UpdateEffect::None;
    };
    let Some(source_uri) = path_to_uri(&site.path) else {
        return UpdateEffect::None;
    };

    let position = crate::lsp::util::byte_offset_to_lsp_pos(&site.rope, site.offset, encoding);
    let params = GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: source_uri },
            position,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };

    let lsp = stoat.lsp_host.clone();
    let fs = stoat.fs_host.clone();
    let source_path = site.path;
    let source_rope = site.rope;
    let task = stoat.executor.spawn(async move {
        let result = match kind {
            LspJumpKind::Definition => lsp.goto_definition(params).await,
            LspJumpKind::Declaration => lsp.goto_declaration(params).await,
            LspJumpKind::TypeDefinition => lsp.goto_type_definition(params).await,
            LspJumpKind::Implementation => lsp.goto_implementation(params).await,
        };
        let response = match result {
            Ok(Some(resp)) => resp,
            Ok(None) => return Vec::new(),
            Err(err) => {
                tracing::warn!(
                    target: "stoat::lsp",
                    request = kind.warn_label(),
                    ?err,
                    "lsp jump request failed",
                );
                return Vec::new();
            },
        };
        resolve_goto_targets(response, &source_path, &source_rope, encoding, &*fs)
    });
    stoat.pending_lsp_jump = Some(task);
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

/// Hover response carried from the spawned task to
/// [`pump_lsp_hover`]. `lines` is the flattened text content; the
/// renderer treats markdown as plain text in v1. `anchor_offset` is
/// the cursor byte offset captured when the request fired so the
/// popup can be anchored at the symbol even if the cursor moves
/// (though [`Stoat::dispatch_key`] clears the popup on motion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HoverResponse {
    pub(crate) lines: Vec<String>,
    pub(crate) anchor_offset: usize,
}

/// Hover popup state ready to paint. Mirrors [`HoverResponse`] but
/// lives on [`Stoat::pending_hover`] (separate from the in-flight
/// task slot) so the renderer can borrow it without polling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HoverPopup {
    pub(crate) lines: Vec<String>,
    pub(crate) anchor_offset: usize,
}

/// Issue a `textDocument/hover` request for the symbol under the
/// focused editor's primary cursor. The async response is stored on
/// [`Stoat::pending_hover_request`] and applied by [`pump_lsp_hover`]
/// on the next render tick.
///
/// No-op when: the focused pane is not an editor; the buffer has no
/// path; or the server does not advertise
/// [`LanguageServerFeature::Hover`]. Replacing the prior pending task
/// drops it, cancelling its spawned future -- only one in-flight hover
/// is tracked at a time.
pub(crate) fn hover(stoat: &mut Stoat) -> UpdateEffect {
    if !stoat
        .lsp_host
        .supports_feature(LanguageServerFeature::Hover)
    {
        return UpdateEffect::None;
    }

    let encoding = stoat.lsp_host.offset_encoding();
    let (anchor_offset, buffer_id, focused_rope, is_review) = {
        let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let head = editor.selections.newest_anchor().head();
        let offset = buf_snap.resolve_anchor(&head);
        (
            offset,
            editor.buffer_id,
            buf_snap.rope().clone(),
            editor.review_view.is_some(),
        )
    };

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

    let position = crate::lsp::util::byte_offset_to_lsp_pos(&source_rope, cursor_offset, encoding);
    let params = HoverParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: source_uri },
            position,
        },
        work_done_progress_params: Default::default(),
    };

    let lsp = stoat.lsp_host.clone();
    let task = stoat.spawn_woken(async move {
        match lsp.hover(params).await {
            Ok(Some(hover)) => Some(HoverResponse {
                lines: flatten_hover_contents(hover.contents),
                anchor_offset,
            }),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "hover request failed");
                None
            },
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
            let capabilities =
                serde_json::to_value(&*stoat.lsp_host.capabilities()).unwrap_or(Value::Null);
            let _ = reply.send(json!({
                "active": !stoat.lsp_host.is_noop(),
                "spawn_attempted": stoat.lsp_spawn_attempted,
                "capabilities": capabilities,
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
            if !buffer_id.is_some_and(|id| stoat.lsp_opened.contains(&id)) {
                let _ = reply.send(json!({ "error": "not open" }));
                return;
            }
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
            let lsp = stoat.lsp_host.clone();
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

/// Flatten an LSP [`HoverContents`] payload into a list of plain-text
/// lines. Markdown is intentionally not parsed in v1 -- the markup
/// text passes through verbatim so callers can read code fences and
/// signatures as-is.
fn flatten_hover_contents(contents: HoverContents) -> Vec<String> {
    fn marked_to_string(m: MarkedString) -> String {
        match m {
            MarkedString::String(s) => s,
            MarkedString::LanguageString(ls) => ls.value,
        }
    }

    let raw = match contents {
        HoverContents::Scalar(m) => marked_to_string(m),
        HoverContents::Array(items) => items
            .into_iter()
            .map(marked_to_string)
            .collect::<Vec<_>>()
            .join("\n"),
        HoverContents::Markup(markup) => markup.value,
    };
    raw.lines().map(str::to_string).collect()
}

/// Poll any in-flight hover request ([`Stoat::pending_hover_request`])
/// and apply the result. On `Ready(Some)` writes the response to
/// [`Stoat::pending_hover`]; on `Ready(None)` clears
/// [`Stoat::pending_hover`]; on `Pending` puts the task back.
/// Returns true when state changed so the caller can request a redraw.
pub(crate) fn pump_lsp_hover(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_hover_request.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(Some(response)) => {
            stoat.pending_hover = Some(HoverPopup {
                lines: response.lines,
                anchor_offset: response.anchor_offset,
            });
            true
        },
        Poll::Ready(None) => {
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

    if !stoat
        .lsp_host
        .supports_feature(LanguageServerFeature::SignatureHelp)
    {
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

    let caps = stoat.lsp_host.capabilities();
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
    let head = editor.selections.newest_anchor().head();
    let offset = buf_snap.resolve_anchor(&head);
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
    if !stoat
        .lsp_host
        .supports_feature(LanguageServerFeature::SignatureHelp)
    {
        return;
    }

    let encoding = stoat.lsp_host.offset_encoding();
    let (anchor_offset, buffer_id, focused_rope, is_review) = {
        let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
            return;
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let head = editor.selections.newest_anchor().head();
        let offset = buf_snap.resolve_anchor(&head);
        (
            offset,
            editor.buffer_id,
            buf_snap.rope().clone(),
            editor.review_view.is_some(),
        )
    };

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

    let lsp = stoat.lsp_host.clone();
    let task = stoat.executor.spawn(async move {
        match lsp.signature_help(params).await {
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
    if !stoat.inlay_hints_enabled
        || !stoat
            .lsp_host
            .supports_feature(LanguageServerFeature::InlayHints)
    {
        return;
    }

    let encoding = stoat.lsp_host.offset_encoding();
    let Some(request) = build_inlay_hint_request(stoat, encoding) else {
        return;
    };

    let key = (
        request.buffer_id,
        request.version,
        request.scroll_row,
        request.end_row,
    );
    if stoat.last_inlay_hint_key == Some(key) {
        return;
    }
    stoat.last_inlay_hint_key = Some(key);

    let InlayHintRequest {
        buffer_id,
        rope,
        params,
        ..
    } = request;
    let lsp = stoat.lsp_host.clone();
    let executor = stoat.executor.clone();
    let task = stoat.executor.spawn(async move {
        executor.timer(INLAY_HINT_DEBOUNCE).await;
        match lsp.range_inlay_hint(params).await {
            Ok(Some(hints)) => Some((buffer_id, convert_inlay_hints(hints, &rope, encoding))),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "inlay_hint request failed");
                None
            },
        }
    });
    stoat.pending_inlay_hint_request = Some(task);
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
    hints
        .into_iter()
        .map(|hint| {
            let offset = crate::lsp::util::lsp_pos_to_byte_offset(rope, hint.position, encoding);
            (offset, inlay_hint_text(&hint), InlayKind::Hint)
        })
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

/// Remove all inlay hints from the focused editor's display map.
pub(crate) fn clear_inlay_hints(stoat: &mut Stoat) {
    let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
        return;
    };
    let prev = std::mem::take(&mut editor.hint_inlay_ids);
    if !prev.is_empty() {
        editor.display_map.splice_inlays(prev, Vec::new());
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
    if !stoat
        .lsp_host
        .supports_feature(LanguageServerFeature::DocumentHighlight)
    {
        return;
    }

    if stoat.focused_mode() != "normal" {
        if stoat.last_document_highlight_key.is_some() {
            clear_document_highlights(stoat);
            stoat.last_document_highlight_key = None;
            stoat.pending_document_highlight_request = None;
        }
        return;
    }

    let encoding = stoat.lsp_host.offset_encoding();
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

    let lsp = stoat.lsp_host.clone();
    let executor = stoat.executor.clone();
    let task = stoat.executor.spawn(async move {
        executor.timer(DOCUMENT_HIGHLIGHT_DEBOUNCE).await;
        match lsp.document_highlight(params).await {
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
        let head = editor.selections.newest_anchor().head();
        let offset = buf_snap.resolve_anchor(&head);
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
    if !stoat
        .lsp_host
        .supports_feature(LanguageServerFeature::PullDiagnostics)
    {
        return;
    }

    let plans: Vec<PullPlan> = stoat
        .lsp_opened
        .iter()
        .copied()
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

        let lsp = stoat.lsp_host.clone();
        let executor = stoat.executor.clone();
        let path = plan.path;
        let task = stoat.executor.spawn(async move {
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
            stoat.diagnostics.replace_for_path(path, diagnostics);
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
/// tree-sitter highlight scope stem its type maps to.
#[derive(Debug, PartialEq)]
struct DecodedToken {
    line: u32,
    start: u32,
    length: u32,
    scope: &'static str,
}

/// A completed semantic-tokens request's payload. It carries the buffer and the
/// resolved `(byte range, scope stem)` spans in request-time coordinates.
pub(crate) type SemanticTokensOutcome = (BufferId, Vec<(std::ops::Range<usize>, &'static str)>);

/// Request semantic tokens for the focused editor when the server advertises a
/// full-document legend and the `(buffer, version)` key changed.
///
/// A newly-focused buffer and each edit re-request behind a 500ms debounce. A key
/// change also clears the stale LSP highlights first. Tokens layer over the
/// tree-sitter baseline, so they never replace it -- only recolor on top.
/// [`pump_lsp_semantic_tokens`] applies the response.
pub(crate) fn semantic_tokens_trigger(stoat: &mut Stoat) {
    let capabilities = stoat.lsp_host.capabilities();
    let Some(legend) = semantic_tokens_legend(&capabilities) else {
        return;
    };
    let legend = legend.to_vec();
    let encoding = stoat.lsp_host.offset_encoding();

    let Some((buffer_id, version, rope, params)) = build_semantic_tokens_request(stoat) else {
        return;
    };

    let key = (buffer_id, version);
    if stoat.last_semantic_tokens_key == Some(key) {
        return;
    }
    stoat.last_semantic_tokens_key = Some(key);
    if let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) {
        editor.display_map.invalidate_lsp_highlights(buffer_id);
    }

    let lsp = stoat.lsp_host.clone();
    let executor = stoat.executor.clone();
    let task = stoat.executor.spawn(async move {
        executor.timer(SEMANTIC_TOKENS_DEBOUNCE).await;
        match lsp.semantic_tokens_full(params).await {
            Ok(Some(result)) => Some((
                buffer_id,
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
) -> Vec<(std::ops::Range<usize>, &'static str)> {
    let SemanticTokensResult::Tokens(tokens) = result else {
        return Vec::new();
    };
    decode_semantic_tokens(&tokens.data, legend)
        .into_iter()
        .map(|t| {
            let start = crate::lsp::util::lsp_pos_to_byte_offset(
                rope,
                Position::new(t.line, t.start),
                encoding,
            );
            let end = crate::lsp::util::lsp_pos_to_byte_offset(
                rope,
                Position::new(t.line, t.start + t.length),
                encoding,
            );
            (start..end, t.scope)
        })
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

/// Decode the LSP relative token stream into absolute-positioned spans.
///
/// Each token's line and start accumulate from the previous per the LSP encoding.
/// `delta_start` is relative within a line and absolute after a line break. Tokens
/// whose type index falls outside the legend, or whose type has no stoat scope,
/// are skipped.
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
        let Some(scope) = lsp_token_scope(ty.as_str()) else {
            continue;
        };
        out.push(DecodedToken {
            line,
            start: col,
            length: token.length,
            scope,
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
        Poll::Ready(Some((buffer_id, items))) => {
            apply_semantic_tokens(stoat, buffer_id, items);
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
    items: Vec<(std::ops::Range<usize>, &'static str)>,
) {
    let mut interner = HighlightStyleInterner::default();
    let styled: Vec<(std::ops::Range<usize>, _)> = items
        .into_iter()
        .map(|(range, scope)| {
            let scope_path = syntax_theme::theme_scope_for_key(scope);
            let style = syntax_theme::style_to_highlight_style(&stoat.theme.get(&scope_path));
            (range, interner.intern(style))
        })
        .collect();
    let interner = Arc::new(interner);

    let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
        return;
    };
    if editor.buffer_id != buffer_id {
        return;
    }

    let tokens: Arc<[SemanticTokenHighlight]> = {
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        styled
            .into_iter()
            .map(|(range, style)| SemanticTokenHighlight {
                range: buf_snap.anchor_at(range.start, Bias::Right)
                    ..buf_snap.anchor_at(range.end, Bias::Left),
                style,
            })
            .collect()
    };
    editor
        .display_map
        .set_lsp_token_highlights(buffer_id, tokens, interner);
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
    if stoat
        .lsp_host
        .capabilities()
        .folding_range_provider
        .is_none()
    {
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

    let lsp = stoat.lsp_host.clone();
    let executor = stoat.executor.clone();
    let task = stoat.executor.spawn(async move {
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
    },
    NeedsResolve {
        title: String,
        action: Box<lsp_types::CodeAction>,
    },
    Command {
        title: String,
        command: lsp_types::Command,
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
}

/// Issue a `textDocument/codeAction` request for the focused editor's
/// primary selection range. The async response is stored on
/// [`Stoat::pending_code_action_request`] and applied by
/// [`pump_lsp_code_actions`] on the next render tick.
///
/// No-op when the focused pane is not an editor, the buffer has no
/// path, or the server does not advertise
/// [`LanguageServerFeature::CodeAction`]. Replacing the prior pending
/// task drops it, cancelling its spawned future -- only one in-flight
/// code-action request is tracked at a time.
pub(crate) fn code_action(stoat: &mut Stoat) -> UpdateEffect {
    if !stoat
        .lsp_host
        .supports_feature(LanguageServerFeature::CodeAction)
    {
        return UpdateEffect::None;
    }

    let encoding = stoat.lsp_host.offset_encoding();
    let (range_byte, anchor_offset, buffer_id, source_rope) = {
        let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let start = buf_snap.resolve_anchor(&sel.start);
        let end = buf_snap.resolve_anchor(&sel.end);
        let head = buf_snap.resolve_anchor(&sel.head());
        let (lo, hi) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        ((lo, hi), head, editor.buffer_id, buf_snap.rope().clone())
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

    let lsp = stoat.lsp_host.clone();
    let task = stoat.executor.spawn(async move {
        match lsp.code_action(params).await {
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
            let entries: Vec<CodeActionEntry> = actions
                .into_iter()
                .filter_map(|item| match item {
                    CodeActionOrCommand::CodeAction(ca) => {
                        match (ca.edit.clone(), ca.data.clone(), ca.command.clone()) {
                            (Some(edit), _, command) => Some(CodeActionEntry::Direct {
                                title: ca.title.clone(),
                                edit: Box::new(edit),
                                command,
                            }),
                            (None, Some(_), _) => Some(CodeActionEntry::NeedsResolve {
                                title: ca.title.clone(),
                                action: Box::new(ca),
                            }),
                            (None, None, Some(command)) => Some(CodeActionEntry::Command {
                                title: ca.title.clone(),
                                command,
                            }),
                            (None, None, None) => None,
                        }
                    },
                    CodeActionOrCommand::Command(command) => Some(CodeActionEntry::Command {
                        title: command.title.clone(),
                        command,
                    }),
                })
                .collect();
            if entries.is_empty() {
                stoat.pending_code_action_picker = None;
            } else if let Some(picker) = stoat.pending_code_action_picker.as_mut() {
                picker.entries = entries;
            }
            true
        },
        Poll::Ready(None) => {
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
    match entry {
        CodeActionEntry::Direct { edit, command, .. } => {
            apply_code_action_edit(stoat, *edit);
            if let Some(command) = command {
                dispatch_execute_command(stoat, command);
            }
        },
        CodeActionEntry::NeedsResolve { action, .. } => {
            let lsp = stoat.lsp_host.clone();
            let task = stoat.executor.spawn(async move {
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
        CodeActionEntry::Command { command, .. } => {
            dispatch_execute_command(stoat, command);
        },
    }
    true
}

/// Spawn a `workspace/executeCommand` request through
/// [`Stoat::executor`] and detach the task. The result `Option<Value>`
/// is generally a server-side side-effect (servers that produce edits
/// reply via the `workspace/applyEdit` request path); errors are
/// logged and swallowed so a failing command does not crash the app.
fn dispatch_execute_command(stoat: &Stoat, command: lsp_types::Command) {
    let lsp = stoat.lsp_host.clone();
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
}

/// Issue a `textDocument/prepareRename` request for the symbol under
/// the focused editor's primary cursor. The async response is stored
/// on [`Stoat::pending_prepare_rename`] and applied by
/// [`pump_lsp_prepare_rename`] on the next render tick.
///
/// No-op when the focused pane is not an editor, the buffer has no
/// path, or the server does not advertise
/// [`LanguageServerFeature::RenameSymbol`].
pub(crate) fn rename_symbol(stoat: &mut Stoat) -> UpdateEffect {
    if !stoat
        .lsp_host
        .supports_feature(LanguageServerFeature::RenameSymbol)
    {
        return UpdateEffect::None;
    }

    let encoding = stoat.lsp_host.offset_encoding();
    let (cursor_offset, buffer_id, source_rope) = {
        let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let head = editor.selections.newest_anchor().head();
        let offset = buf_snap.resolve_anchor(&head);
        (offset, editor.buffer_id, buf_snap.rope().clone())
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

    let position = crate::lsp::util::byte_offset_to_lsp_pos(&source_rope, cursor_offset, encoding);

    let params = TextDocumentPositionParams {
        text_document: TextDocumentIdentifier {
            uri: source_uri.clone(),
        },
        position,
    };

    let lsp = stoat.lsp_host.clone();
    let task = stoat.executor.spawn(async move {
        let response = match lsp.prepare_rename(params).await {
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
                let head = editor.selections.newest_anchor().head();
                buf_snap.resolve_anchor(&head)
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
    let lsp = stoat.lsp_host.clone();
    let task = stoat.executor.spawn(async move {
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

/// One entry in [`SymbolPicker`]. `title` is the symbol name as
/// painted in the popup; `anchor_offset` is the byte offset in the
/// focused buffer that the cursor jumps to on selection (resolved
/// from the symbol's `selection_range.start` for nested responses or
/// `location.range.start` for flat responses).
#[derive(Debug, Clone)]
pub(crate) struct SymbolEntry {
    pub(crate) title: String,
    pub(crate) anchor_offset: usize,
    /// Graph symbol to jump to when picked, for graph-navigation pickers.
    /// `None` for LSP document-symbol entries, which jump to
    /// [`Self::anchor_offset`] in the current buffer instead.
    pub(crate) symbol: Option<SymbolKey>,
}

/// Cursor-anchored document-symbol picker. Painted as a numbered
/// popup over a viewport of up to 9 visible entries that follows
/// [`Self::selected_idx`]; the user navigates with `j`/`k`, picks
/// the selected entry with Enter, picks visible entries 1..=9 with
/// the corresponding digit keys, and dismisses with Escape or any
/// other action.
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
/// No-op when the focused pane is not an editor, the buffer has no
/// path, or the server does not advertise
/// [`LanguageServerFeature::DocumentSymbols`].
pub(crate) fn open_symbol_picker(stoat: &mut Stoat) -> UpdateEffect {
    if !stoat
        .lsp_host
        .supports_feature(LanguageServerFeature::DocumentSymbols)
    {
        return UpdateEffect::None;
    }

    let (anchor_offset, buffer_id) = {
        let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let head = editor.selections.newest_anchor().head();
        (buf_snap.resolve_anchor(&head), editor.buffer_id)
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

    let params = DocumentSymbolParams {
        text_document: TextDocumentIdentifier { uri: source_uri },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };

    let lsp = stoat.lsp_host.clone();
    let task = stoat.executor.spawn(async move {
        match lsp.document_symbol(params).await {
            Ok(resp) => resp,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "document_symbol request failed");
                None
            },
        }
    });
    stoat.pending_symbol_picker_request = Some(task);
    stoat.pending_symbol_picker = Some(SymbolPicker {
        entries: Vec::new(),
        anchor_offset,
        selected_idx: 0,
    });
    UpdateEffect::None
}

/// Poll any in-flight document-symbol request and translate the
/// response into a [`SymbolPicker`]. Flattens the nested
/// `DocumentSymbol` tree via DFS so a single keystroke (1-9)
/// selects from the leading 9 entries in document order. Drops the
/// picker when the response is empty or `None`.
pub(crate) fn pump_lsp_symbol_picker(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_symbol_picker_request.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(Some(response)) => {
            let encoding = stoat.lsp_host.offset_encoding();
            let rope = {
                let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
                    stoat.pending_symbol_picker = None;
                    return true;
                };
                let snapshot = editor.display_map.snapshot();
                let buf_snap = snapshot.buffer_snapshot();
                buf_snap.rope().clone()
            };
            let entries = symbol_picker_entries(&rope, encoding, response);
            if entries.is_empty() {
                stoat.pending_symbol_picker = None;
            } else if let Some(picker) = stoat.pending_symbol_picker.as_mut() {
                picker.entries = entries;
            }
            true
        },
        Poll::Ready(None) => {
            stoat.pending_symbol_picker = None;
            true
        },
        Poll::Pending => {
            stoat.pending_symbol_picker_request = Some(task);
            false
        },
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
) -> Vec<SymbolEntry> {
    let mut entries: Vec<SymbolEntry> = Vec::new();
    match response {
        DocumentSymbolResponse::Flat(items) => {
            for SymbolInformation { name, location, .. } in items {
                let offset =
                    crate::lsp::util::lsp_pos_to_byte_offset(rope, location.range.start, encoding);
                entries.push(SymbolEntry {
                    title: name,
                    anchor_offset: offset,
                    symbol: None,
                });
            }
        },
        DocumentSymbolResponse::Nested(items) => {
            fn walk(
                rope: &Rope,
                encoding: OffsetEncoding,
                items: Vec<DocumentSymbol>,
                ancestors: &mut Vec<String>,
                out: &mut Vec<SymbolEntry>,
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
                    out.push(SymbolEntry {
                        title,
                        anchor_offset: offset,
                        symbol: None,
                    });
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

/// Apply the user's pick from the open symbol picker and clear the picker.
///
/// A graph-navigation entry jumps to its symbol (opening another file if
/// needed); an LSP document-symbol entry jumps the primary cursor to the
/// entry's anchor offset in the current buffer. No-op when no picker is
/// open or `index` is out of range.
pub(crate) fn pick_symbol(stoat: &mut Stoat, index: usize) -> bool {
    let Some(picker) = stoat.pending_symbol_picker.take() else {
        return false;
    };
    let Some(entry) = picker.entries.into_iter().nth(index) else {
        return false;
    };
    match entry.symbol {
        Some(key) => {
            crate::code_index::nav::jump_to_symbol(stoat, key);
        },
        None => {
            crate::action_handlers::movement::jump_to_offset(stoat, entry.anchor_offset);
        },
    }
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
}

/// One entry in [`WorkspaceSymbolPicker`]. `title` is the symbol
/// name; `path` is the absolute filesystem path to open; `position`
/// is the LSP position in the target file.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceSymbolEntry {
    pub(crate) title: String,
    pub(crate) path: PathBuf,
    pub(crate) position: Position,
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

/// Open the workspace-symbol query input modal. Capability-gates on
/// [`LanguageServerFeature::WorkspaceSymbols`]. The input is born in
/// insert mode so typing routes through `handle_insert_key` into the
/// modal's [`crate::input_view::InputView`]. The modal seed is empty;
/// submit fires the request, cancel disposes the input.
pub(crate) fn open_workspace_symbol_picker(stoat: &mut Stoat) -> UpdateEffect {
    if !stoat
        .lsp_host
        .supports_feature(LanguageServerFeature::WorkspaceSymbols)
    {
        return UpdateEffect::None;
    }

    let anchor_offset = {
        let Some(editor) = crate::action_handlers::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let head = editor.selections.newest_anchor().head();
        buf_snap.resolve_anchor(&head)
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

    let lsp = stoat.lsp_host.clone();
    let task = stoat.executor.spawn(async move {
        match lsp.workspace_symbol(params).await {
            Ok(resp) => resp,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "workspace_symbol request failed");
                None
            },
        }
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

/// Poll any in-flight workspace-symbol request and translate the
/// response into a [`WorkspaceSymbolPicker`]. Drops the picker when
/// the response is empty or `None`. v1 caps at the first 9
/// entries (number-key cap). Handles only the
/// [`WorkspaceSymbolResponse::Flat`] variant in v1; nested
/// `WorkspaceSymbol` entries are dropped (rare in practice).
pub(crate) fn pump_lsp_workspace_symbol(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_workspace_symbol_request.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(Some(response)) => {
            let entries = workspace_symbol_entries(response);
            if entries.is_empty() {
                stoat.pending_workspace_symbol_picker = None;
            } else if let Some(picker) = stoat.pending_workspace_symbol_picker.as_mut() {
                picker.entries = entries;
            }
            true
        },
        Poll::Ready(None) => {
            stoat.pending_workspace_symbol_picker = None;
            true
        },
        Poll::Pending => {
            stoat.pending_workspace_symbol_request = Some(task);
            false
        },
    }
}

fn workspace_symbol_entries(response: WorkspaceSymbolResponse) -> Vec<WorkspaceSymbolEntry> {
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
                });
            }
        },
        WorkspaceSymbolResponse::Nested(items) => {
            for WorkspaceSymbol { name, location, .. } in items {
                let (uri, position) = match location {
                    OneOf::Left(loc) => (loc.uri, loc.range.start),
                    OneOf::Right(workspace_loc) => {
                        // `WorkspaceLocation` carries no range; fall back to
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

    let encoding = stoat.lsp_host.offset_encoding();
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
/// No-op when the focused pane is not an editor, the buffer has no
/// path, or the server does not advertise
/// [`LanguageServerFeature::Format`].
pub(crate) fn format_selections(stoat: &mut Stoat) -> UpdateEffect {
    if !stoat
        .lsp_host
        .supports_feature(LanguageServerFeature::Format)
    {
        return UpdateEffect::None;
    }

    let encoding = stoat.lsp_host.offset_encoding();
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

    let lsp = stoat.lsp_host.clone();
    let task = stoat.executor.spawn(async move {
        match lsp.range_formatting(params).await {
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
/// No-op when the focused pane is not an editor, the buffer has no
/// path, or the server does not advertise
/// [`LanguageServerFeature::Format`].
pub(crate) fn format_document(stoat: &mut Stoat) -> UpdateEffect {
    if !stoat
        .lsp_host
        .supports_feature(LanguageServerFeature::Format)
    {
        return UpdateEffect::None;
    }

    let Some(buffer_id) = crate::action_handlers::focused_editor_mut(stoat).map(|e| e.buffer_id)
    else {
        return UpdateEffect::None;
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

    let lsp = stoat.lsp_host.clone();
    let task = stoat.executor.spawn(async move {
        match lsp.formatting(params).await {
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
/// and dispatch on how many locations resolved. Zero locations changes
/// nothing. One jumps to it directly via [`apply_jump`]. Two or more
/// open a [`LocationPicker`] in [`Stoat::location_picker`] so the user
/// chooses. On `Pending` puts the task back. Returns true when state
/// changed so the caller can request a redraw.
pub(crate) fn pump_lsp_jumps(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_lsp_jump.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(mut entries) => {
            match entries.len() {
                0 => {},
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
            stoat.pending_lsp_jump = Some(task);
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
        test_harness::TestHarness,
    };
    use futures::FutureExt;
    use lsp_types::TextDocumentSyncKind;
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
            !h.stoat.lsp_spawn_attempted,
            "FakeLsp is a non-noop host, so opening a rust buffer attempts no spawn",
        );
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
        let head = editor.selections.newest_anchor().head();
        buffer_snapshot.resolve_anchor(&head)
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
        assert_eq!(popup.lines, vec!["fn foo() -> u32".to_string()]);
        assert_eq!(popup.anchor_offset, 0);
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
        assert_eq!(popup.lines, vec!["documentation".to_string()]);
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
    fn hover_multiline_markup_split_by_newline() {
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
        assert_eq!(
            popup.lines,
            vec![
                "```rust".to_string(),
                "fn foo()".to_string(),
                "```".to_string(),
                "Docs here".to_string(),
            ]
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
    fn hover_from_the_diff_cursor_targets_the_real_file() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        h.stage_review_scenario("/work", &[("a.rs", "a\nb\nc\nd\n", "a\nb\nX\nd\n")]);
        h.stoat.open_review();
        h.settle();

        // Cursor on the changed row (new-side line 3, i.e. LSP line 2).
        place_review_cursor(&mut h, 2);
        // Seeded at the real file path and the translated new-side position,
        // so a matching response proves the request left the placeholder
        // buffer for the working-tree file.
        h.fake_lsp()
            .set_hover("/work/a.rs", 2, 0, "the changed line");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        let popup = h
            .stoat
            .pending_hover
            .as_ref()
            .expect("hover popup over the diff");
        assert_eq!(popup.lines, vec!["the changed line".to_string()]);
    }

    #[test]
    fn snapshot_hover_over_the_diff() {
        let mut h = TestHarness::with_size(80, 14);
        enable_hover(&h);
        h.stage_review_scenario("/work", &[("a.rs", "a\nb\nc\nd\n", "a\nb\nX\nd\n")]);
        h.stoat.open_review();
        h.settle();

        place_review_cursor(&mut h, 2);
        h.fake_lsp().set_hover("/work/a.rs", 2, 0, "changed here");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();
        h.assert_snapshot("snapshot_hover_over_diff");
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

    #[test]
    fn goto_definition_from_the_diff_parks_the_review() {
        let mut h = TestHarness::with_size(80, 24);
        enable_goto_definition(&h);
        h.stage_review_scenario("/work", &[("a.rs", "a\nb\nc\nd\n", "a\nb\nX\nd\n")]);
        h.fake_fs().insert_file(
            &PathBuf::from("/work/lib.rs"),
            b"fn one() {}\nfn two() {}\n",
        );
        h.stoat.open_review();
        h.settle();

        place_review_cursor(&mut h, 2);
        h.fake_lsp()
            .set_definition("/work/a.rs", 2, 0, "/work/lib.rs", 1, 3);

        let review_editor_id = h.with_review(|s| s.view_editor).expect("review editor");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
        h.settle();

        assert_eq!(
            focused_buffer_path(&h),
            PathBuf::from("/work/lib.rs"),
            "the jump lands in the target file",
        );
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "the pane left review mode"
        );
        assert!(
            h.with_review(|s| s.toggled_off),
            "the review is parked so R re-enters the diff",
        );
        assert_eq!(
            h.with_review(|s| s.view_editor),
            Some(review_editor_id),
            "the parked review editor survived the pane swap",
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
        assert!(h.stoat.pending_symbol_picker.is_none());
        assert!(h.stoat.pending_symbol_picker_request.is_none());
    }

    #[test]
    fn symbol_picker_no_response_clears_picker() {
        let mut h = TestHarness::with_size(80, 24);
        enable_document_symbols(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        open_buffer(&mut h, root.join("main.rs"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
        h.settle();
        assert!(h.stoat.pending_symbol_picker.is_none());
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
        let picker = h.stoat.pending_symbol_picker.as_ref().expect("picker open");
        let titles: Vec<&str> = picker.entries.iter().map(|e| e.title.as_str()).collect();
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
        let picker = h.stoat.pending_symbol_picker.as_ref().expect("picker open");
        let titles: Vec<&str> = picker.entries.iter().map(|e| e.title.as_str()).collect();
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
        h.type_keys("2");
        assert!(h.stoat.pending_symbol_picker.is_none());
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
        let picker = h.stoat.pending_symbol_picker.as_ref().expect("picker open");
        assert_eq!(picker.entries.len(), 15);
        assert_eq!(picker.selected_idx, 0);
    }

    #[test]
    fn symbol_picker_navigates_with_jk_and_picks_with_enter() {
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
            h.type_keys("j");
        }
        let picker = h.stoat.pending_symbol_picker.as_ref().expect("picker");
        assert_eq!(picker.selected_idx, 11);

        h.type_keys("enter");
        assert!(h.stoat.pending_symbol_picker.is_none());
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
        assert!(h.stoat.pending_symbol_picker.is_some());
        h.type_keys("escape");
        assert!(h.stoat.pending_symbol_picker.is_none());
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
        assert!(h.stoat.pending_symbol_picker.is_some());
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn snapshot_symbol_picker() {
        let mut h = TestHarness::with_size(40, 12);
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
            tok(0, 8, 4, 9),
        ];
        let decoded = super::decode_semantic_tokens(&data, &legend);
        let want = |line, start, length, scope| super::DecodedToken {
            line,
            start,
            length,
            scope,
        };
        assert_eq!(
            decoded,
            vec![
                want(0, 0, 3, "keyword"),
                want(0, 4, 2, "function"),
                want(1, 2, 5, "keyword"),
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
            .map(|(tokens, _)| tokens.len())
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

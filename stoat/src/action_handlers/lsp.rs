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
    app::{Stoat, UpdateEffect},
    buffer::BufferId,
    host::{LanguageServerFeature, OffsetEncoding},
};
pub(crate) use lsp_types::Uri;
use lsp_types::{
    CodeActionContext, CodeActionOrCommand, CodeActionParams, DidChangeTextDocumentParams,
    DidOpenTextDocumentParams, DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse,
    GotoDefinitionParams, GotoDefinitionResponse, HoverContents, HoverParams, MarkedString,
    Position, PrepareRenameResponse, Range, RenameParams, SymbolInformation,
    TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, TextDocumentSyncCapability, TextDocumentSyncKind,
    VersionedTextDocumentIdentifier, WorkDoneProgressParams, WorkspaceEdit,
};
use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use stoat_text::{patch::Patch, Rope};

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

/// Resolved target of an in-flight `textDocument/definition` request.
/// `path` is an absolute filesystem path (file-scheme URIs only;
/// non-`file:` responses are dropped because stoat has no remote-buffer
/// concept). `offset` is a byte offset into that file's rope after
/// applying the host's negotiated [`OffsetEncoding`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JumpTarget {
    pub(crate) path: PathBuf,
    pub(crate) offset: usize,
}

/// Discriminator for the goto-style LSP requests that all return
/// `Option<GotoDefinitionResponse>` (a single Location or list of
/// candidates) and feed the same `Stoat::pending_lsp_jump` slot.
#[derive(Debug, Clone, Copy)]
pub(crate) enum LspJumpKind {
    Definition,
    TypeDefinition,
    Implementation,
}

impl LspJumpKind {
    fn feature(self) -> LanguageServerFeature {
        match self {
            Self::Definition => LanguageServerFeature::GotoDefinition,
            Self::TypeDefinition => LanguageServerFeature::GotoTypeDefinition,
            Self::Implementation => LanguageServerFeature::GotoImplementation,
        }
    }

    fn warn_label(self) -> &'static str {
        match self {
            Self::Definition => "goto_definition",
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

/// Issue an LSP jump-style request (definition / type definition /
/// implementation / declaration) for the symbol under the focused
/// editor's primary cursor. The async response is stored on
/// [`Stoat::pending_lsp_jump`] and applied by [`pump_lsp_jumps`] on
/// the next render tick.
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
    let task = stoat.executor.spawn(async move {
        let result = match kind {
            LspJumpKind::Definition => lsp.goto_definition(params).await,
            LspJumpKind::TypeDefinition => lsp.goto_type_definition(params).await,
            LspJumpKind::Implementation => lsp.goto_implementation(params).await,
        };
        let response = match result {
            Ok(Some(resp)) => resp,
            Ok(None) => return None,
            Err(err) => {
                tracing::warn!(
                    target: "stoat::lsp",
                    request = kind.warn_label(),
                    ?err,
                    "lsp jump request failed",
                );
                return None;
            },
        };
        resolve_goto_target(response, &source_path, &source_rope, encoding, &*fs)
    });
    stoat.pending_lsp_jump = Some(task);
    UpdateEffect::None
}

/// Translate a `GotoDefinitionResponse` into a [`JumpTarget`]. The
/// first candidate is taken across all variants; multi-candidate
/// disambiguation (Helix's picker) is a separate concern. Same-file
/// targets reuse the supplied source rope; cross-file targets read the
/// destination through the supplied [`crate::host::FsHost`] so a
/// closed buffer can still be resolved without round-tripping through
/// `Stoat`.
fn resolve_goto_target(
    response: GotoDefinitionResponse,
    source_path: &Path,
    source_rope: &Rope,
    encoding: OffsetEncoding,
    fs: &dyn crate::host::FsHost,
) -> Option<JumpTarget> {
    let (uri, position) = match response {
        GotoDefinitionResponse::Scalar(loc) => (loc.uri, loc.range.start),
        GotoDefinitionResponse::Array(locs) => {
            let loc = locs.into_iter().next()?;
            (loc.uri, loc.range.start)
        },
        GotoDefinitionResponse::Link(links) => {
            let link = links.into_iter().next()?;
            (link.target_uri, link.target_range.start)
        },
    };

    let target_path = crate::app::lsp_uri_to_path(&uri)?;

    let offset = if target_path == source_path {
        crate::lsp::util::lsp_pos_to_byte_offset(source_rope, position, encoding)
    } else {
        let text = match super::read_string_via_host(fs, &target_path) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(
                    target: "stoat::lsp",
                    path = %target_path.display(),
                    ?err,
                    "goto_definition target file unreadable",
                );
                return None;
            },
        };
        let target_rope = Rope::from(text.as_str());
        crate::lsp::util::lsp_pos_to_byte_offset(&target_rope, position, encoding)
    };

    Some(JumpTarget {
        path: target_path,
        offset,
    })
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
    let params = HoverParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: source_uri },
            position,
        },
        work_done_progress_params: Default::default(),
    };

    let lsp = stoat.lsp_host.clone();
    let task = stoat.executor.spawn(async move {
        match lsp.hover(params).await {
            Ok(Some(hover)) => Some(HoverResponse {
                lines: flatten_hover_contents(hover.contents),
                anchor_offset: cursor_offset,
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

/// One actionable entry in [`CodeActionPicker`]. Variants reflect
/// how the entry's [`WorkspaceEdit`] is obtained: directly from the
/// initial response, or via a follow-up `codeAction/resolve` call.
/// `Command`-only `CodeActionOrCommand` items are filtered out at
/// pump time and do not appear here.
#[derive(Debug, Clone)]
pub(crate) enum CodeActionEntry {
    Direct {
        title: String,
        edit: Box<WorkspaceEdit>,
    },
    NeedsResolve {
        title: String,
        action: Box<lsp_types::CodeAction>,
    },
}

impl CodeActionEntry {
    pub(crate) fn title(&self) -> &str {
        match self {
            Self::Direct { title, .. } | Self::NeedsResolve { title, .. } => title,
        }
    }
}

/// Cursor-anchored code action picker. Painted as a numbered popup;
/// the user picks with keys `1`..=`9`, dismisses with Escape or any
/// other action.
#[derive(Debug, Clone)]
pub(crate) struct CodeActionPicker {
    pub(crate) entries: Vec<CodeActionEntry>,
    pub(crate) anchor_offset: usize,
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
                    CodeActionOrCommand::CodeAction(ca) => match (ca.edit.clone(), ca.data.clone())
                    {
                        (Some(edit), _) => Some(CodeActionEntry::Direct {
                            title: ca.title.clone(),
                            edit: Box::new(edit),
                        }),
                        (None, Some(_)) => Some(CodeActionEntry::NeedsResolve {
                            title: ca.title.clone(),
                            action: Box::new(ca),
                        }),
                        (None, None) => None,
                    },
                    CodeActionOrCommand::Command(_) => None,
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
        CodeActionEntry::Direct { edit, .. } => {
            apply_code_action_edit(stoat, *edit);
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
    }
    true
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
/// `RenameParams` without touching the editor again. `previous_mode`
/// restores the pre-rename mode on cancel.
#[derive(Debug)]
pub(crate) struct RenameInputState {
    pub(crate) input: crate::input_view::InputView,
    pub(crate) source_uri: Uri,
    pub(crate) symbol_position: Position,
    pub(crate) anchor_offset: usize,
    pub(crate) previous_mode: String,
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
/// the input modal seeded with the placeholder text. Transitions
/// [`Stoat::mode`] to "prompt" so typing routes through
/// `handle_insert_key` into the modal's [`crate::input_view::InputView`].
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
            let previous_mode = stoat.mode.clone();
            let executor = stoat.executor.clone();
            let ws = stoat.active_workspace_mut();
            let input = crate::input_view::InputView::create(
                ws,
                executor,
                crate::input_view::SubmitTarget::RenameSymbol,
                &prep.placeholder,
                "prompt",
                1,
            );
            stoat.rename_input = Some(RenameInputState {
                input,
                source_uri: prep.source_uri,
                symbol_position: prep.symbol_position,
                anchor_offset,
                previous_mode,
            });
            stoat.mode = "prompt".into();
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
    let previous_mode = rename_state.previous_mode.clone();
    let ws = stoat.active_workspace_mut();
    rename_state.input.dispose(ws);
    stoat.mode = previous_mode;

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

/// Cancel the rename input modal without firing rename. Restores the
/// previous mode and disposes the embedded input.
pub(crate) fn rename_input_cancel(stoat: &mut Stoat) -> bool {
    let Some(rename_state) = stoat.rename_input.take() else {
        return false;
    };
    let previous_mode = rename_state.previous_mode.clone();
    let ws = stoat.active_workspace_mut();
    rename_state.input.dispose(ws);
    stoat.mode = previous_mode;
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
}

/// Cursor-anchored document-symbol picker. Painted as a numbered
/// popup; the user picks with keys `1`..=`9`, dismisses with
/// Escape or any other action.
#[derive(Debug, Clone)]
pub(crate) struct SymbolPicker {
    pub(crate) entries: Vec<SymbolEntry>,
    pub(crate) anchor_offset: usize,
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
/// in the supplied rope. Entries are limited to the first 9 in
/// document order (number-key cap; v1 limitation).
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
                });
            }
        },
        DocumentSymbolResponse::Nested(items) => {
            fn walk(
                rope: &Rope,
                encoding: OffsetEncoding,
                items: Vec<DocumentSymbol>,
                out: &mut Vec<SymbolEntry>,
            ) {
                for symbol in items {
                    let offset = crate::lsp::util::lsp_pos_to_byte_offset(
                        rope,
                        symbol.selection_range.start,
                        encoding,
                    );
                    out.push(SymbolEntry {
                        title: symbol.name,
                        anchor_offset: offset,
                    });
                    if let Some(children) = symbol.children {
                        walk(rope, encoding, children, out);
                    }
                }
            }
            walk(rope, encoding, items, &mut entries);
        },
    }
    entries.truncate(9);
    entries
}

/// Apply the user's pick from the open symbol picker: jump the
/// primary cursor to the selected entry's anchor offset and clear
/// the picker. No-op when no picker is open or `index` is out of
/// range.
pub(crate) fn pick_symbol(stoat: &mut Stoat, index: usize) -> bool {
    let Some(picker) = stoat.pending_symbol_picker.take() else {
        return false;
    };
    let Some(entry) = picker.entries.into_iter().nth(index) else {
        return false;
    };
    crate::action_handlers::movement::jump_to_offset(stoat, entry.anchor_offset);
    true
}

/// Poll any in-flight LSP jump request ([`Stoat::pending_lsp_jump`])
/// and apply the result. On `Ready(Some)` opens the target file in
/// the focused pane (no-op when already open) and collapses every
/// selection onto the resolved offset; on `Ready(None)` silently
/// drops; on `Pending` puts the task back. Returns true when state
/// changed so the caller can request a redraw.
pub(crate) fn pump_lsp_jumps(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_lsp_jump.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(Some(target)) => {
            let focused = stoat.active_workspace().panes.focus();
            super::file::open_file_in_pane(stoat, focused, &target.path);
            super::movement::jump_to_offset(stoat, target.offset);
            true
        },
        Poll::Ready(None) => true,
        Poll::Pending => {
            stoat.pending_lsp_jump = Some(task);
            false
        },
    }
}

/// Convert an absolute filesystem path to an `lsp_types::Uri`. Returns
/// `None` for paths that cannot be encoded as a `file://` URI (e.g.
/// non-UTF-8 paths). Mirrors the production behaviour Helix uses
/// internally; LSP servers expect `file:` URIs for local files.
fn path_to_uri(path: &Path) -> Option<Uri> {
    let path_str = path.to_str()?;
    Uri::from_str(&format!("file://{path_str}")).ok()
}

#[cfg(test)]
mod tests {
    use crate::test_harness::TestHarness;
    use lsp_types::TextDocumentSyncKind;
    use std::{
        path::{Path, PathBuf},
        time::Duration,
    };
    use stoat_action::OpenFile;

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
        assert_eq!(h.stoat.mode, "normal");
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
        assert_eq!(h.stoat.mode, "normal");
    }

    fn enable_goto_definition(h: &TestHarness) {
        use lsp_types::{OneOf, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            definition_provider: Some(OneOf::Left(true)),
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
        assert_eq!(cursor_offset(&mut h), 8);
        assert_eq!(focused_buffer_path(&h), path);
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
        assert_eq!(h.stoat.mode, "normal");
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
        assert_eq!(h.stoat.mode, "normal");
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
        assert_eq!(h.stoat.mode, "normal");
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
        assert_eq!(h.stoat.mode, "normal");
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
    fn code_action_drops_command_only_entries() {
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
        assert_eq!(titles, vec!["Real edit"]);
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
        assert_eq!(h.stoat.mode, "normal");
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
        assert_eq!(h.stoat.mode, "prompt");
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
        assert_eq!(h.stoat.mode, "normal");
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
        assert_eq!(h.stoat.mode, "prompt");
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
        assert_eq!(titles, vec!["outer", "inner"]);
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
    fn symbol_picker_caps_at_nine_entries() {
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
        assert_eq!(picker.entries.len(), 9);
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
        assert_eq!(h.stoat.mode, "normal");
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
}

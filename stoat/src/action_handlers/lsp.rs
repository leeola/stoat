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
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, GotoDefinitionParams,
    GotoDefinitionResponse, Position, Range, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams,
    TextDocumentSyncCapability, TextDocumentSyncKind, VersionedTextDocumentIdentifier,
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

/// Issue a `textDocument/definition` request for the symbol under the
/// focused editor's primary cursor. The async response is stored on
/// [`Stoat::pending_goto_definition`] and applied by
/// [`pump_lsp_jumps`] on the next render tick.
///
/// No-op when: the focused pane is not an editor; the buffer has no
/// path; or the server does not advertise
/// [`LanguageServerFeature::GotoDefinition`]. Replacing the prior
/// pending task drops it, cancelling its spawned future.
pub(crate) fn goto_definition(stoat: &mut Stoat) -> UpdateEffect {
    if !stoat
        .lsp_host
        .supports_feature(LanguageServerFeature::GotoDefinition)
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
        let response = match lsp.goto_definition(params).await {
            Ok(Some(resp)) => resp,
            Ok(None) => return None,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "goto_definition request failed");
                return None;
            },
        };
        resolve_goto_target(response, &source_path, &source_rope, encoding, &*fs)
    });
    stoat.pending_goto_definition = Some(task);
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

/// Poll any in-flight LSP jump request (`pending_goto_definition`) and
/// apply the result. On `Ready(Some)` opens the target file in the
/// focused pane (no-op when already open) and collapses every selection
/// onto the resolved offset; on `Ready(None)` silently drops; on
/// `Pending` puts the task back. Returns true when state changed so
/// the caller can request a redraw.
pub(crate) fn pump_lsp_jumps(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_goto_definition.take() else {
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
            stoat.pending_goto_definition = Some(task);
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
    use std::{path::PathBuf, time::Duration};
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
        assert!(h.stoat.pending_goto_definition.is_none());
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
}

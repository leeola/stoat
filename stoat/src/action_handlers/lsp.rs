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

use crate::{app::Stoat, buffer::BufferId};
pub(crate) use lsp_types::Uri;
use lsp_types::{
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, TextDocumentContentChangeEvent,
    TextDocumentItem, TextDocumentSyncCapability, TextDocumentSyncKind,
    VersionedTextDocumentIdentifier,
};
use std::{path::Path, str::FromStr, time::Duration};

/// Quiet window after the last edit before a buffer's `did_change`
/// fires. Matches Helix's default and prevents per-keystroke storms
/// of LSP traffic.
pub(crate) const LSP_DID_CHANGE_DEBOUNCE: Duration = Duration::from_millis(50);

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
/// Capability honouring: dispatch only when the server advertises
/// [`TextDocumentSyncKind::FULL`]. `INCREMENTAL` logs once and
/// skips because per-edit range encoding is not yet wired (separate
/// follow-up). `NONE` skips silently.
pub(crate) fn notify_buffer_changes_pending(stoat: &mut Stoat) {
    let sync_kind = resolve_sync_kind(&stoat.lsp_host.capabilities().text_document_sync);
    let snapshots: Vec<(BufferId, Uri, String, u64)> = stoat
        .lsp_opened
        .iter()
        .copied()
        .filter_map(|id| {
            let workspace = stoat.active_workspace();
            let buffer = workspace.buffers.get(id)?;
            let buffer_b = buffer.read().expect("buffer lock");
            let current_version = buffer_b.version();
            let last_version = stoat.lsp_buffer_versions.get(&id).copied().unwrap_or(0);
            if current_version == last_version {
                return None;
            }
            let path = workspace.buffers.path_for(id)?.to_path_buf();
            let uri = path_to_uri(&path)?;
            let text = buffer_b.rope().to_string();
            Some((id, uri, text, current_version))
        })
        .collect();

    if snapshots.is_empty() {
        return;
    }

    match sync_kind {
        TextDocumentSyncKind::FULL => {},
        TextDocumentSyncKind::INCREMENTAL => {
            tracing::warn!(
                target: "stoat::lsp",
                "did_change skipped: server requested Incremental sync, not yet implemented",
            );
            for (id, _, _, version) in &snapshots {
                stoat.lsp_buffer_versions.insert(*id, *version);
            }
            return;
        },
        _ => {
            for (id, _, _, version) in &snapshots {
                stoat.lsp_buffer_versions.insert(*id, *version);
            }
            return;
        },
    }

    for (id, uri, text, buffer_version) in snapshots {
        stoat.lsp_buffer_versions.insert(id, buffer_version);
        let lsp_version = stoat.lsp_doc_versions.entry(id).or_insert(0);
        *lsp_version += 1;
        let lsp_version_value = *lsp_version;

        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri,
                version: lsp_version_value,
            },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text,
            }],
        };

        let lsp = stoat.lsp_host.clone();
        let executor = stoat.executor.clone();
        let task = stoat.executor.spawn(async move {
            executor.timer(LSP_DID_CHANGE_DEBOUNCE).await;
            if let Err(err) = lsp.did_change(params).await {
                tracing::warn!(target: "stoat::lsp", ?err, "did_change notification failed");
            }
        });
        stoat.lsp_pending_changes.insert(id, task);
    }
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
}

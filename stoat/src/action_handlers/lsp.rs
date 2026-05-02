//! LSP buffer-lifecycle plumbing. This module routes
//! [`crate::buffer::BufferId`] open / close / save / change events to
//! the workspace's [`crate::host::LspHost`] so a real language server
//! can keep its document mirror in sync with the editor.
//!
//! Only `did_open` is wired today. The other lifecycle halves
//! (`did_change` debouncing, `did_save`, `did_close`) are tracked as
//! sub-bullets under the parent TODO entry; they need infrastructure
//! that does not exist yet (a debounce timer for changes, save / close
//! actions to hook).

use crate::{app::Stoat, buffer::BufferId};
use lsp_types::{DidOpenTextDocumentParams, TextDocumentItem, Uri};
use std::{path::Path, str::FromStr};

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
    use std::path::PathBuf;
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
}

//! Acceptance handler for the completion popup.
//!
//! Replaces the highlighted item's `replace_range` with its
//! `insert_text` in the focused buffer, places the primary cursor at
//! the inserted end, and clears popup state. Bound to `Tab` in
//! insert mode via the arbitration arm in
//! [`crate::app::Stoat::handle_insert_key`].
//!
//! The popup's `replace_range` is in buffer byte offsets captured at
//! trigger time. LSP items widen this beyond the typed prefix when
//! the server returns a `text_edit.range`; non-LSP items scope it to
//! the prefix range. Acceptance reads the range from the chosen
//! item, so both shapes work uniformly.

use crate::{
    app::{Stoat, UpdateEffect},
    buffer::BufferId,
    completion::CompletionItem,
    pane::{FocusTarget, View},
};
use std::{
    future::Future,
    path::Path,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use stoat_text::Bias;

/// The `additionalTextEdits` a `completionItem/resolve` returned for an
/// accepted completion, plus the buffer they apply to. Carried from the
/// accept resolve task to [`pump_completion_accept`].
pub(crate) struct AcceptedImports {
    buffer_id: BufferId,
    edits: Vec<lsp_types::TextEdit>,
}

/// Accept the highlighted item in [`Stoat::pending_completion`]. No-op
/// when the popup is not showing, the focused pane is not an editor,
/// or the popup's items list is empty.
///
/// Snippet items (`is_snippet: true`) parse the insert text via
/// [`crate::completion::snippet::parse`] and install multi-cursor
/// selections at the first tabstop group; remaining groups stash on
/// [`Stoat::active_snippet`] for [`crate::completion::snippet::advance`]
/// to consume on subsequent Tab presses. Plain items insert the text
/// verbatim and collapse the cursor at the inserted end.
pub(crate) fn execute(stoat: &mut Stoat) -> UpdateEffect {
    let Some(popup) = stoat.pending_completion.take() else {
        return UpdateEffect::None;
    };
    let Some(item) = popup.items.into_iter().nth(popup.selected_idx) else {
        return UpdateEffect::None;
    };

    let ws = stoat.active_workspace_mut();
    let FocusTarget::SplitPane(pane_id) = ws.focus else {
        return UpdateEffect::None;
    };
    let View::Editor(editor_id) = ws.panes.pane(pane_id).view else {
        return UpdateEffect::None;
    };
    let editor = match ws.editors.get_mut(editor_id) {
        Some(e) => e,
        None => return UpdateEffect::None,
    };
    let buffer_id = editor.buffer_id;
    let buffer = match ws.buffers.get(buffer_id) {
        Some(b) => b,
        None => return UpdateEffect::None,
    };

    let rope_len = buffer.read().expect("poisoned").rope().len();
    let start = item.replace_range.start.min(rope_len);
    let end = item.replace_range.end.min(rope_len);
    let edit_range = start..end;

    let snippet_rendered = if item.is_snippet {
        Some(crate::completion::snippet::parse(&item.insert_text).render())
    } else {
        None
    };
    let inserted_text: &str = snippet_rendered
        .as_ref()
        .map(|r| r.text.as_str())
        .unwrap_or(&item.insert_text);

    {
        let mut guard = buffer.write().expect("poisoned");
        guard.edit(edit_range.clone(), inserted_text);
    }

    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();

    let active_snippet = if let Some(rendered) = &snippet_rendered {
        let (selections, active) =
            crate::completion::snippet::install(rendered, edit_range.start, new_buf);
        editor.selections.replace_with(selections, new_buf);
        active
    } else {
        let new_offset = edit_range.start + inserted_text.len();
        let anchor = new_buf.anchor_at(new_offset, Bias::Right);
        editor.selections.transform(new_buf, |s| {
            let mut new = s.clone();
            new.collapse_to(anchor, stoat_text::SelectionGoal::None);
            new
        });
        None
    };

    stoat.pending_completion_request = None;
    crate::completion::request::record_dismiss(stoat);
    stoat.active_snippet = active_snippet;

    apply_or_resolve_additional_edits(stoat, buffer_id, &item);

    UpdateEffect::Redraw
}

/// Apply the accepted item's `additionalTextEdits` -- typically the
/// imports rust-analyzer adds when a completion needs one.
///
/// Applies them synchronously when the item already carries them.
/// Otherwise, for an LSP item whose server advertises `resolveProvider`,
/// resolves the item with a 300ms timeout and applies the edits from
/// [`pump_completion_accept`] once it lands. The main edit has already
/// been applied by [`execute`], so a resolve failure or timeout simply
/// leaves it as the only edit. Non-LSP items do nothing.
fn apply_or_resolve_additional_edits(
    stoat: &mut Stoat,
    buffer_id: BufferId,
    item: &CompletionItem,
) {
    let Some(lsp_item) = &item.lsp_item else {
        return;
    };
    if let Some(edits) = lsp_item.additional_text_edits.clone()
        && !edits.is_empty()
    {
        apply_additional_edits(stoat, buffer_id, edits);
        return;
    }
    if !resolve_advertised(stoat) {
        return;
    }

    let raw = (**lsp_item).clone();
    let lsp = stoat.lsp_host.clone();
    let executor = stoat.executor.clone();
    let task = stoat.spawn_woken(async move {
        let resolve = std::pin::pin!(lsp.completion_resolve(raw));
        let timer = std::pin::pin!(executor.timer(Duration::from_millis(300)));
        let resolved = match futures::future::select(resolve, timer).await {
            futures::future::Either::Left((Ok(item), _)) => item,
            _ => return None,
        };
        let edits = resolved.additional_text_edits?;
        (!edits.is_empty()).then_some(AcceptedImports { buffer_id, edits })
    });
    stoat.pending_completion_accept = Some(task);
}

fn resolve_advertised(stoat: &Stoat) -> bool {
    stoat
        .lsp_host
        .capabilities()
        .completion_provider
        .as_ref()
        .and_then(|opts| opts.resolve_provider)
        .unwrap_or(false)
}

fn apply_additional_edits(stoat: &mut Stoat, buffer_id: BufferId, edits: Vec<lsp_types::TextEdit>) {
    let Some(path) = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)
    else {
        return;
    };
    if let Err(err) = crate::lsp::edit_apply::apply_text_edits_to_buffer(stoat, &path, edits) {
        tracing::warn!(target: "stoat::lsp", ?err, "additionalTextEdits apply failed");
    }
}

/// Poll the in-flight accept-resolve task. On completion, apply the
/// resolved `additionalTextEdits` to the captured buffer. The placed
/// cursor rides edit-tracking anchors, so imports inserted above it keep
/// it correct. Returns `true` when the buffer changed.
pub(crate) fn pump_completion_accept(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_completion_accept.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(Some(imports)) => {
            apply_additional_edits(stoat, imports.buffer_id, imports.edits);
            true
        },
        Poll::Ready(None) => false,
        Poll::Pending => {
            stoat.pending_completion_accept = Some(task);
            false
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        action_handlers::dispatch,
        completion::{CompletionItem, CompletionPopup, CompletionSource},
        test_harness::TestHarness,
    };
    use std::path::PathBuf;
    use stoat_action::{AcceptCompletion, OpenFile};

    fn open_scratch(h: &mut TestHarness, contents: &str) -> PathBuf {
        let path = PathBuf::from("/ws/buf.rs");
        h.fake_fs()
            .insert_files(std::iter::once((path.clone(), contents.as_bytes())));
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/ws");
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        path
    }

    fn buffer_text(h: &TestHarness, path: &Path) -> String {
        let ws = h.stoat.active_workspace();
        let id = ws.buffers.id_for_path(path).expect("buffer registered");
        let buf = ws.buffers.get(id).expect("buffer present");
        let guard = buf.read().expect("buffer lock");
        guard.rope().to_string()
    }

    fn cursor_offset(h: &mut TestHarness) -> usize {
        let ws = h.stoat.active_workspace_mut();
        let FocusTarget::SplitPane(pane_id) = ws.focus else {
            panic!("not a split pane");
        };
        let View::Editor(editor_id) = ws.panes.pane(pane_id).view else {
            panic!("not an editor pane");
        };
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snapshot = snapshot.buffer_snapshot();
        let head = editor.selections.newest_anchor().head();
        buf_snapshot.resolve_anchor(&head)
    }

    fn install_popup(
        h: &mut TestHarness,
        items: Vec<CompletionItem>,
        prefix_range: std::ops::Range<usize>,
    ) {
        h.stoat.pending_completion = Some(CompletionPopup {
            items,
            selected_idx: 0,
            anchor_offset: prefix_range.start,
            prefix_range,
        });
    }

    #[test]
    fn accept_replaces_prefix_with_insert_text() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        h.type_keys("i");
        h.type_text("foo");
        install_popup(
            &mut h,
            vec![CompletionItem {
                label: "foobar".into(),
                source: CompletionSource::Word,
                kind: None,
                detail: None,
                replace_range: 0..3,
                insert_text: "foobar".into(),
                is_snippet: false,
                documentation: None,
                lsp_item: None,
            }],
            0..3,
        );

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(buffer_text(&h, &path), "foobar");
        assert_eq!(cursor_offset(&mut h), 6);
        assert!(h.stoat.pending_completion.is_none());
        assert!(h.stoat.pending_completion_request.is_none());
    }

    #[test]
    fn accept_honors_widened_replace_range_from_lsp_text_edit() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        h.type_keys("i");
        h.type_text("print");
        install_popup(
            &mut h,
            vec![CompletionItem {
                label: "println!".into(),
                source: CompletionSource::Lsp,
                kind: None,
                detail: None,
                replace_range: 0..5,
                insert_text: "println!(\"\")".into(),
                is_snippet: false,
                documentation: None,
                lsp_item: None,
            }],
            0..5,
        );

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(buffer_text(&h, &path), "println!(\"\")");
        assert_eq!(cursor_offset(&mut h), "println!(\"\")".len());
    }

    #[test]
    fn accept_uses_selected_idx_not_first_item() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        h.type_keys("i");
        h.type_text("fo");
        h.stoat.pending_completion = Some(CompletionPopup {
            items: vec![
                CompletionItem {
                    label: "foo".into(),
                    source: CompletionSource::Word,
                    kind: None,
                    detail: None,
                    replace_range: 0..2,
                    insert_text: "foo".into(),
                    is_snippet: false,
                    documentation: None,
                    lsp_item: None,
                },
                CompletionItem {
                    label: "foobar".into(),
                    source: CompletionSource::Word,
                    kind: None,
                    detail: None,
                    replace_range: 0..2,
                    insert_text: "foobar".into(),
                    is_snippet: false,
                    documentation: None,
                    lsp_item: None,
                },
            ],
            selected_idx: 1,
            anchor_offset: 0,
            prefix_range: 0..2,
        });

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(buffer_text(&h, &path), "foobar");
    }

    #[test]
    fn accept_with_no_popup_is_noop() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "abc");
        h.type_keys("a");
        assert!(h.stoat.pending_completion.is_none());

        let effect = dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(effect, UpdateEffect::None);
        assert_eq!(buffer_text(&h, &path), "abc");
    }

    #[test]
    fn accept_snippet_expands_placeholder_and_arms_cursor() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        h.type_keys("i");
        h.type_text("pri");
        install_popup(
            &mut h,
            vec![CompletionItem {
                label: "println!".into(),
                source: CompletionSource::Lsp,
                kind: None,
                detail: None,
                replace_range: 0..3,
                insert_text: "println!(${1:msg})$0".into(),
                is_snippet: true,
                documentation: None,
                lsp_item: None,
            }],
            0..3,
        );

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(buffer_text(&h, &path), "println!(msg)");
        let ws = h.stoat.active_workspace_mut();
        let FocusTarget::SplitPane(pane_id) = ws.focus else {
            panic!("not split");
        };
        let View::Editor(eid) = ws.panes.pane(pane_id).view else {
            panic!("not editor");
        };
        let editor = ws.editors.get_mut(eid).expect("editor");
        let snap = editor.display_map.snapshot();
        let buf_snap = snap.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let start = buf_snap.resolve_anchor(&sel.start);
        let end = buf_snap.resolve_anchor(&sel.end);
        assert_eq!((start, end), (9, 12), "selection on `msg` placeholder");
        assert!(
            h.stoat.active_snippet.is_some(),
            "active snippet should remain so Tab can advance to $0",
        );
    }

    #[test]
    fn accept_snippet_with_only_exit_does_not_arm_active() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        h.type_keys("i");
        h.type_text("pri");
        install_popup(
            &mut h,
            vec![CompletionItem {
                label: "println!".into(),
                source: CompletionSource::Lsp,
                kind: None,
                detail: None,
                replace_range: 0..3,
                insert_text: "println!()$0".into(),
                is_snippet: true,
                documentation: None,
                lsp_item: None,
            }],
            0..3,
        );

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(buffer_text(&h, &path), "println!()");
        assert!(h.stoat.active_snippet.is_none());
    }

    #[test]
    fn accept_snippet_with_linked_tabstops_places_multi_cursor() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        h.type_keys("i");
        h.type_text("foo");
        install_popup(
            &mut h,
            vec![CompletionItem {
                label: "linked".into(),
                source: CompletionSource::Lsp,
                kind: None,
                detail: None,
                replace_range: 0..3,
                insert_text: "${1:x} = ${1}".into(),
                is_snippet: true,
                documentation: None,
                lsp_item: None,
            }],
            0..3,
        );

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(buffer_text(&h, &path), "x = ");
        let ws = h.stoat.active_workspace_mut();
        let FocusTarget::SplitPane(pane_id) = ws.focus else {
            panic!("not split");
        };
        let View::Editor(eid) = ws.panes.pane(pane_id).view else {
            panic!("not editor");
        };
        let editor = ws.editors.get(eid).expect("editor");
        assert_eq!(
            editor.selections.all_anchors().len(),
            2,
            "two cursors at the linked tabstop sites",
        );
    }

    #[test]
    fn non_snippet_item_keeps_existing_behavior() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        h.type_keys("i");
        h.type_text("foo");
        install_popup(
            &mut h,
            vec![CompletionItem {
                label: "foobar".into(),
                source: CompletionSource::Word,
                kind: None,
                detail: None,
                replace_range: 0..3,
                insert_text: "foobar".into(),
                is_snippet: false,
                documentation: None,
                lsp_item: None,
            }],
            0..3,
        );

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(buffer_text(&h, &path), "foobar");
        assert!(h.stoat.active_snippet.is_none());
    }

    fn enable_resolve(h: &TestHarness) {
        use lsp_types::{CompletionOptions, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            completion_provider: Some(CompletionOptions {
                resolve_provider: Some(true),
                ..CompletionOptions::default()
            }),
            ..ServerCapabilities::default()
        });
    }

    fn lsp_row(label: &str, range: std::ops::Range<usize>) -> CompletionItem {
        CompletionItem {
            label: label.into(),
            source: CompletionSource::Lsp,
            kind: None,
            detail: None,
            replace_range: range,
            insert_text: label.into(),
            is_snippet: false,
            documentation: None,
            lsp_item: Some(Box::new(lsp_types::CompletionItem {
                label: label.into(),
                ..Default::default()
            })),
        }
    }

    fn resolved_with_import(label: &str, text: &str) -> lsp_types::CompletionItem {
        lsp_types::CompletionItem {
            label: label.into(),
            additional_text_edits: Some(vec![lsp_types::TextEdit {
                range: lsp_types::Range::new(
                    lsp_types::Position::new(0, 0),
                    lsp_types::Position::new(0, 0),
                ),
                new_text: text.into(),
            }]),
            ..Default::default()
        }
    }

    #[test]
    fn accept_resolves_and_applies_additional_text_edits() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        enable_resolve(&h);
        h.fake_lsp()
            .set_completion_resolve("barbaz", resolved_with_import("barbaz", "use foo;\n"));
        h.type_keys("i");
        h.type_text("bar");
        install_popup(&mut h, vec![lsp_row("barbaz", 0..3)], 0..3);

        dispatch(&mut h.stoat, &AcceptCompletion);
        assert_eq!(
            buffer_text(&h, &path),
            "barbaz",
            "the main edit lands at once"
        );
        h.settle();

        assert_eq!(
            buffer_text(&h, &path),
            "use foo;\nbarbaz",
            "the resolved import is applied above the completion",
        );
    }

    #[test]
    fn accept_resolve_timeout_leaves_the_main_edit_alone() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        enable_resolve(&h);
        h.fake_lsp()
            .set_request_delay("completionItem/resolve", Duration::from_millis(400));
        h.fake_lsp()
            .set_completion_resolve("barbaz", resolved_with_import("barbaz", "use foo;\n"));
        h.type_keys("i");
        h.type_text("bar");
        install_popup(&mut h, vec![lsp_row("barbaz", 0..3)], 0..3);

        dispatch(&mut h.stoat, &AcceptCompletion);
        // The resolve is delayed past the 300ms timeout, which fires first.
        h.advance_clock(Duration::from_millis(300));

        assert_eq!(buffer_text(&h, &path), "barbaz");
    }

    #[test]
    fn word_accept_arms_no_resolve() {
        let mut h = TestHarness::default();
        let path = open_scratch(&mut h, "");
        enable_resolve(&h);
        h.type_keys("i");
        h.type_text("foo");
        install_popup(
            &mut h,
            vec![CompletionItem {
                label: "foobar".into(),
                source: CompletionSource::Word,
                kind: None,
                detail: None,
                replace_range: 0..3,
                insert_text: "foobar".into(),
                is_snippet: false,
                documentation: None,
                lsp_item: None,
            }],
            0..3,
        );

        dispatch(&mut h.stoat, &AcceptCompletion);

        assert_eq!(buffer_text(&h, &path), "foobar");
        assert!(
            h.stoat.pending_completion_accept.is_none(),
            "a word accept issues no resolve",
        );
    }
}

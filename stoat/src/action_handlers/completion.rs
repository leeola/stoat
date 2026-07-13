use crate::app::{Stoat, UpdateEffect};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

/// Quiet window before a resolve request fires, so rapid Up/Down
/// navigation coalesces to one request for the row it settles on.
const RESOLVE_DEBOUNCE: Duration = Duration::from_millis(150);

/// The detail and documentation `completionItem/resolve` returned for a
/// popup row, tagged with the row's label so a resolve landing after the
/// selection moved can be dropped.
pub(crate) struct ResolvedCompletion {
    label: String,
    detail: Option<String>,
    documentation: Option<String>,
}

/// Arbitrate the Tab key in insert mode.
///
/// An in-flight snippet advances its placeholder. An open completion popup
/// accepts its highlighted item. Otherwise, a cursor following only whitespace
/// on its line inserts the buffer's indent unit. Returns [`UpdateEffect::None`]
/// when none of those branches apply, so other dispatch layers (or the user)
/// can decide what to do with the keystroke.
pub(super) fn smart_tab(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.active_snippet.is_some() {
        crate::completion::snippet::advance(stoat);
        return UpdateEffect::Redraw;
    }
    if stoat.pending_completion.is_some() {
        return crate::completion::accept::execute(stoat);
    }
    let Some((editor_id, buffer_id)) = stoat.focused_editor_ids() else {
        return UpdateEffect::None;
    };
    if stoat.cursor_after_only_whitespace(editor_id, buffer_id) {
        let unit = stoat.buffer_indent_style(buffer_id).as_str();
        stoat.editor_insert(editor_id, buffer_id, unit);
        return UpdateEffect::Redraw;
    }
    UpdateEffect::None
}

/// Insert the buffer's indent unit at every cursor, unconditionally.
///
/// The plain counterpart to [`smart_tab`], skipping the snippet, completion,
/// and leading-whitespace arbitration, so a keystroke bound to it always
/// indents. Typically bound to Shift-Tab.
pub(super) fn insert_tab(stoat: &mut Stoat) -> UpdateEffect {
    let Some((editor_id, buffer_id)) = stoat.focused_editor_ids() else {
        return UpdateEffect::None;
    };
    let unit = stoat.buffer_indent_style(buffer_id).as_str();
    stoat.editor_insert(editor_id, buffer_id, unit);
    UpdateEffect::Redraw
}

/// Force a completion request even when the buffer signature is
/// unchanged. Clears the dedup signature consulted by
/// [`crate::completion::request::trigger`] so the trigger does not
/// short-circuit, then arms a fresh request.
pub(super) fn trigger_completion(stoat: &mut Stoat) -> UpdateEffect {
    stoat.last_completion_signature = None;
    crate::completion::request::trigger(stoat);
    UpdateEffect::Redraw
}

/// Arm a debounced `completionItem/resolve` for the popup's selected
/// row. Clears any prior task unless the selected item is an LSP item
/// still missing detail or documentation and the server advertises
/// `resolveProvider`. Storing the task replaces (cancels) any prior one,
/// so navigating past a row drops its pending resolve.
pub(crate) fn arm_completion_resolve(stoat: &mut Stoat) {
    let Some((label, raw)) = resolve_plan(stoat) else {
        stoat.pending_completion_resolve = None;
        return;
    };
    let lsp = stoat.lsp_host();
    let executor = stoat.executor.clone();
    let task = stoat.spawn_woken(async move {
        executor.timer(RESOLVE_DEBOUNCE).await;
        let resolved = lsp.completion_resolve(raw).await.ok()?;
        Some(ResolvedCompletion {
            label,
            detail: resolved.detail,
            documentation: crate::completion::lsp::documentation_string(
                resolved.documentation.as_ref(),
            ),
        })
    });
    stoat.pending_completion_resolve = Some(task);
}

/// The `(label, raw LSP item)` to resolve for the selected row, or `None`
/// when no resolve should fire. No resolve fires when the server does not
/// advertise `resolveProvider`, the selected row is not an LSP item, or
/// it already carries both detail and documentation.
fn resolve_plan(stoat: &Stoat) -> Option<(String, lsp_types::CompletionItem)> {
    if !resolve_advertised(stoat) {
        return None;
    }
    let popup = stoat.pending_completion.as_ref()?;
    let item = popup.items.get(popup.selected_idx)?;
    let lsp_item = item.lsp_item.as_ref()?;
    if item.detail.is_some() && item.documentation.is_some() {
        return None;
    }
    Some((item.label.clone(), (**lsp_item).clone()))
}

fn resolve_advertised(stoat: &Stoat) -> bool {
    stoat
        .lsp_host()
        .capabilities()
        .completion_provider
        .as_ref()
        .and_then(|opts| opts.resolve_provider)
        .unwrap_or(false)
}

/// Poll the in-flight resolve task. On completion, patch the resolved
/// detail and documentation into the popup's currently-selected row when
/// its label still matches. A resolve that lands after the selection
/// moved is discarded. Returns `true` when the popup changed.
pub(crate) fn pump_completion_resolve(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_completion_resolve.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(Some(resolved)) => apply_resolved(stoat, resolved),
        Poll::Ready(None) => false,
        Poll::Pending => {
            stoat.pending_completion_resolve = Some(task);
            false
        },
    }
}

fn apply_resolved(stoat: &mut Stoat, resolved: ResolvedCompletion) -> bool {
    let Some(popup) = stoat.pending_completion.as_mut() else {
        return false;
    };
    let Some(item) = popup.items.get_mut(popup.selected_idx) else {
        return false;
    };
    if item.label != resolved.label {
        return false;
    }
    let mut changed = false;
    if let Some(detail) = resolved.detail
        && item.detail.as_deref() != Some(detail.as_str())
    {
        item.detail = Some(detail);
        changed = true;
    }
    if let Some(doc) = resolved.documentation
        && item.documentation.as_deref() != Some(doc.as_str())
    {
        item.documentation = Some(doc);
        changed = true;
    }
    changed
}

#[cfg(test)]
mod tests {
    use crate::{
        action_handlers::dispatch,
        completion::{CompletionItem, CompletionPopup, CompletionSource},
        Stoat,
    };
    use stoat_action::{SmartTab, TriggerCompletion};

    fn buffer_text(h: &Stoat, path: &std::path::Path) -> String {
        let ws = h.active_workspace();
        let buf_id = ws.buffers.id_for_path(path).expect("buffer");
        let buffer = ws.buffers.get(buf_id).expect("buffer entry");
        let guard = buffer.read().expect("poisoned");
        guard.rope().to_string()
    }

    #[test]
    fn smart_tab_inserts_indent_unit_at_indent_position() {
        let mut h = Stoat::test();
        // The 2-space indent makes the buffer space-styled.
        let path = h.write_file("a.rs", "  abc\n");
        h.open_file(&path);
        h.type_keys("l l i");
        dispatch(&mut h.stoat, &SmartTab);
        assert_eq!(buffer_text(&h.stoat, &path), "    abc\n");
    }

    #[test]
    fn smart_tab_no_op_after_nonwhitespace_without_popup() {
        let mut h = Stoat::test();
        let path = h.write_file("a.rs", "abc\n");
        h.open_file(&path);
        h.type_keys("l l l i");
        dispatch(&mut h.stoat, &SmartTab);
        assert_eq!(buffer_text(&h.stoat, &path), "abc\n");
    }

    #[test]
    fn smart_tab_accepts_pending_completion_when_popup_visible() {
        let mut h = Stoat::test();
        let path = h.write_file("a.rs", "fo\n");
        h.open_file(&path);
        h.type_keys("l l i");
        h.stoat.pending_completion = Some(CompletionPopup {
            items: vec![CompletionItem {
                label: "foo".into(),
                source: CompletionSource::Word,
                kind: None,
                detail: None,
                replace_range: 0..2,
                insert_text: "foo".into(),
                is_snippet: false,
                documentation: None,
                lsp_item: None,
            }],
            selected_idx: 0,
            anchor_offset: 0,
            prefix_range: 0..2,
        });
        dispatch(&mut h.stoat, &SmartTab);
        assert!(h.stoat.pending_completion.is_none());
        assert_eq!(buffer_text(&h.stoat, &path), "foo\n");
    }

    #[test]
    fn trigger_completion_clears_dedup_signature() {
        let mut h = Stoat::test();
        let path = h.write_file("a.rs", "abc\n");
        h.open_file(&path);
        h.type_keys("i");
        let buf_id = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(&path)
            .expect("buf");
        h.stoat.last_completion_signature = Some((buf_id, 999));
        dispatch(&mut h.stoat, &TriggerCompletion);
        assert_ne!(h.stoat.last_completion_signature, Some((buf_id, 999)));
    }

    fn enable_resolve(h: &crate::test_harness::TestHarness) {
        use lsp_types::{CompletionOptions, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            completion_provider: Some(CompletionOptions {
                resolve_provider: Some(true),
                ..CompletionOptions::default()
            }),
            ..ServerCapabilities::default()
        });
    }

    fn lsp_row(label: &str) -> CompletionItem {
        CompletionItem {
            label: label.to_string(),
            source: CompletionSource::Lsp,
            kind: None,
            detail: None,
            documentation: None,
            replace_range: 0..0,
            insert_text: label.to_string(),
            is_snippet: false,
            lsp_item: Some(Box::new(lsp_types::CompletionItem {
                label: label.to_string(),
                ..Default::default()
            })),
        }
    }

    fn resolved_with_doc(label: &str, doc: &str) -> lsp_types::CompletionItem {
        lsp_types::CompletionItem {
            label: label.to_string(),
            documentation: Some(lsp_types::Documentation::String(doc.to_string())),
            ..Default::default()
        }
    }

    fn popup(items: Vec<CompletionItem>) -> CompletionPopup {
        CompletionPopup {
            items,
            selected_idx: 0,
            anchor_offset: 0,
            prefix_range: 0..0,
        }
    }

    #[test]
    fn selecting_lsp_item_resolves_documentation() {
        let mut h = Stoat::test();
        enable_resolve(&h);
        h.fake_lsp()
            .set_completion_resolve("foo", resolved_with_doc("foo", "foo docs"));
        h.stoat.pending_completion = Some(popup(vec![lsp_row("foo")]));

        super::arm_completion_resolve(&mut h.stoat);
        h.advance_clock(std::time::Duration::from_millis(150));

        let popup = h.stoat.pending_completion.as_ref().expect("popup");
        assert_eq!(popup.items[0].documentation.as_deref(), Some("foo docs"));
    }

    #[test]
    fn navigating_away_discards_the_pending_resolve() {
        let mut h = Stoat::test();
        enable_resolve(&h);
        h.fake_lsp()
            .set_completion_resolve("foo", resolved_with_doc("foo", "foo docs"));
        h.fake_lsp()
            .set_completion_resolve("bar", resolved_with_doc("bar", "bar docs"));
        h.stoat.pending_completion = Some(popup(vec![lsp_row("foo"), lsp_row("bar")]));

        super::arm_completion_resolve(&mut h.stoat);
        // Move to "bar" before the debounce elapses. Re-arming cancels
        // foo's in-flight resolve.
        h.stoat.pending_completion.as_mut().unwrap().selected_idx = 1;
        super::arm_completion_resolve(&mut h.stoat);
        h.advance_clock(std::time::Duration::from_millis(150));

        let popup = h.stoat.pending_completion.as_ref().expect("popup");
        assert_eq!(popup.items[0].documentation, None, "foo's resolve dropped");
        assert_eq!(popup.items[1].documentation.as_deref(), Some("bar docs"));
    }
}

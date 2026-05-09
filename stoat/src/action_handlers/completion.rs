use crate::app::{Stoat, UpdateEffect};

/// Arbitrate the Tab key in insert mode. Advance the active snippet
/// placeholder if one is in flight; accept the highlighted completion
/// item if the popup is open; otherwise insert a tab when the cursor
/// follows only whitespace on the current line. Returns
/// [`UpdateEffect::None`] when none of those branches apply so other
/// dispatch layers (or the user) can decide what to do with the
/// keystroke.
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
        stoat.editor_insert(editor_id, buffer_id, "\t");
        return UpdateEffect::Redraw;
    }
    UpdateEffect::None
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
    fn smart_tab_inserts_tab_at_indent_position() {
        let mut h = Stoat::test();
        let path = h.write_file("a.rs", "  abc\n");
        h.open_file(&path);
        h.type_keys("l l i");
        dispatch(&mut h.stoat, &SmartTab);
        assert_eq!(buffer_text(&h.stoat, &path), "  \tabc\n");
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
}

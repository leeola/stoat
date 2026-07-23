//! Handlers for the pane-owned jumplist. Records the focused editor's position
//! and walks backward and forward, restoring the full selection set across
//! buffers.
//!
//! The list lives on the pane, so recording and walking resolve the pane the
//! focused editor sits in and reach its jumplist there. Applying an entry
//! re-shows the entry's buffer when the pane drifted to another, then restores
//! the recorded selections against the current snapshot (anchors ride the
//! fragment tree, so no edit-time remap is needed).

use crate::{
    app::{Stoat, UpdateEffect},
    buffer::BufferId,
    jumplist::JumpEntry,
    pane::{FocusTarget, PaneId, View},
};

/// The focused editor's current position as a [`JumpEntry`], or `None` when no
/// editor is focused.
pub(crate) fn live_entry(stoat: &mut Stoat) -> Option<JumpEntry> {
    let editor = super::focused_editor_mut(stoat)?;
    Some(JumpEntry {
        buffer_id: editor.buffer_id,
        selections: editor.selections.all_anchors().to_vec(),
    })
}

/// Record the focused editor's position on the focused pane's jumplist.
///
/// A no-op when focus is on a dock. Every jump-shaped motion routes its origin
/// through here, and the jumplist's resolved-shape dedup absorbs doubled pushes.
pub(crate) fn push_jump(stoat: &mut Stoat) {
    if let Some(entry) = live_entry(stoat) {
        push_entry(stoat, entry);
    }
}

/// Record a pre-captured `entry` on the focused pane's jumplist. A no-op when
/// focus is on a dock.
///
/// For sites that must capture the origin before a motion moves the cursor
/// (a search submit, a conditional goto) and record it only once the motion
/// lands.
pub(crate) fn push_entry(stoat: &mut Stoat, entry: JumpEntry) {
    let ws = stoat.active_workspace_mut();
    let pane_id = match ws.focus {
        FocusTarget::SplitPane => ws.panes.focus(),
        FocusTarget::Dock(_) => return,
    };
    let buffers = &ws.buffers;
    ws.panes.pane_mut(pane_id).jumplist.push(entry, buffers);
}

/// Record `target` pane's outgoing editor position on its own jumplist before
/// a file open swaps its editor, so the open reverses with a backward jump.
///
/// A no-op when the pane already shows `incoming` (a same-buffer reopen) or
/// shows no editor. Kept out of [`show_buffer_in_pane`](super::file::show_buffer_in_pane)
/// so a jumplist walk re-showing a buffer records nothing.
pub(crate) fn record_pane_switch(stoat: &mut Stoat, target: PaneId, incoming: BufferId) {
    let ws = stoat.active_workspace_mut();
    let entry = {
        let eid = match ws.panes.pane(target).view {
            View::Editor(eid) => eid,
            _ => return,
        };
        let Some(editor) = ws.editors.get(eid) else {
            return;
        };
        if editor.buffer_id == incoming {
            return;
        }
        JumpEntry {
            buffer_id: editor.buffer_id,
            selections: editor.selections.all_anchors().to_vec(),
        }
    };
    let buffers = &ws.buffers;
    ws.panes.pane_mut(target).jumplist.push(entry, buffers);
}

/// Jump the focused pane to `entry`, re-showing its buffer when the pane is not
/// already on it, then restoring the recorded selection set.
///
/// A no-op when the entry's buffer has been closed. Cross-buffer jumps reuse
/// [`show_buffer_in_pane`](super::file::show_buffer_in_pane), so an open buffer
/// is re-shown without a disk read.
pub(crate) fn apply_jump_entry(stoat: &mut Stoat, entry: JumpEntry) {
    let resolved = {
        let ws = stoat.active_workspace();
        let pane_id = match ws.focus {
            FocusTarget::SplitPane => ws.panes.focus(),
            FocusTarget::Dock(_) => return,
        };
        let current_buffer = match ws.panes.pane(pane_id).view {
            View::Editor(eid) => ws.editors.get(eid).map(|e| e.buffer_id),
            _ => None,
        };
        if current_buffer == Some(entry.buffer_id) {
            None
        } else {
            match ws.buffers.get(entry.buffer_id) {
                Some(buffer) => Some((pane_id, buffer)),
                None => return,
            }
        }
    };

    let swapped_buffer = resolved.is_some();
    if let Some((pane_id, buffer)) = resolved {
        let executor = stoat.executor.clone();
        super::file::show_buffer_in_pane(stoat, pane_id, entry.buffer_id, buffer, executor);
    }

    let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
    let Some(editor) = super::focused_editor_mut(stoat) else {
        return;
    };

    {
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        editor
            .selections
            .replace_with(entry.selections.clone(), buf_snap);
    }

    // A cross-buffer jump lands in a freshly shown editor with no prior view to
    // glide from, so it snaps.
    if swapped_buffer {
        super::movement::ensure_cursor_in_view(editor, scrolloff);
    } else {
        super::movement::follow_jump(editor, scrolloff);
    }
}

pub(super) fn save_selection(stoat: &mut Stoat) -> UpdateEffect {
    push_jump(stoat);
    UpdateEffect::None
}

pub(crate) fn jump_backward(stoat: &mut Stoat) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1) as usize;
    let Some(live) = live_entry(stoat) else {
        return UpdateEffect::None;
    };
    let target = {
        let ws = stoat.active_workspace_mut();
        let pane_id = match ws.focus {
            FocusTarget::SplitPane => ws.panes.focus(),
            FocusTarget::Dock(_) => return UpdateEffect::None,
        };
        let buffers = &ws.buffers;
        ws.panes
            .pane_mut(pane_id)
            .jumplist
            .backward(live, buffers, count)
            .cloned()
    };
    let Some(target) = target else {
        return UpdateEffect::None;
    };
    apply_jump_entry(stoat, target);
    UpdateEffect::Redraw
}

pub(crate) fn jump_forward(stoat: &mut Stoat) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1) as usize;
    let target = {
        let ws = stoat.active_workspace_mut();
        let pane_id = match ws.focus {
            FocusTarget::SplitPane => ws.panes.focus(),
            FocusTarget::Dock(_) => return UpdateEffect::None,
        };
        ws.panes.pane_mut(pane_id).jumplist.forward(count).cloned()
    };
    let Some(target) = target else {
        return UpdateEffect::None;
    };
    apply_jump_entry(stoat, target);
    UpdateEffect::Redraw
}

#[cfg(test)]
mod tests {
    use crate::{action_handlers, test_harness::TestHarness};
    use stoat_text::BufferId;

    fn focused_buffer(h: &mut TestHarness) -> BufferId {
        action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .buffer_id
    }

    fn selection_count(h: &mut TestHarness) -> usize {
        action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .selections
            .all_anchors()
            .len()
    }

    #[test]
    fn jump_backward_and_forward_cross_buffers() {
        let mut h = TestHarness::with_size(40, 6);
        let a = h.write_file("a.rs", "aaaa\nbbbb\n");
        let b = h.write_file("b.rs", "xxxx\nyyyy\n");

        h.open_file(&a);
        h.type_keys("l");
        let a_buffer = focused_buffer(&mut h);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);

        h.open_file(&b);
        let b_buffer = focused_buffer(&mut h);
        h.type_keys("l");

        action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
        assert_eq!(focused_buffer(&mut h), a_buffer, "backward re-shows a.rs");
        assert_eq!(
            h.primary_head_offset(),
            1,
            "restores the saved offset in a.rs"
        );

        action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpForward);
        assert_eq!(focused_buffer(&mut h), b_buffer, "forward returns to b.rs");
        assert_eq!(
            h.primary_head_offset(),
            1,
            "returns to the recorded b.rs tip"
        );
    }

    #[test]
    fn jump_restores_the_full_selection_set() {
        let mut h = TestHarness::with_size(40, 6);
        let path = h.write_file("s.rs", "aaaa\nbbbb\ncccc\n");
        h.open_file(&path);

        action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::AddSelectionBelow);
        assert_eq!(selection_count(&mut h), 2, "precondition: two selections");

        action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
        assert_eq!(
            selection_count(&mut h),
            1,
            "backward restored the one-selection entry"
        );

        action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpForward);
        assert_eq!(
            selection_count(&mut h),
            2,
            "forward restored the two-selection set"
        );
    }

    fn jumplist_len(h: &mut TestHarness) -> usize {
        action_handlers::focused_pane_jumplist(&mut h.stoat)
            .map(|jumplist| jumplist.entries().len())
            .unwrap_or(0)
    }

    #[test]
    fn opening_a_file_records_the_origin() {
        let mut h = TestHarness::with_size(40, 6);
        let a = h.write_file("a.rs", "aaaa\nbbbb\n");
        let b = h.write_file("b.rs", "xxxx\nyyyy\n");
        h.open_file(&a);
        h.type_keys("l");
        let a_buffer = focused_buffer(&mut h);

        // Opening b.rs records the a.rs origin with no explicit save.
        h.open_file(&b);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
        assert_eq!(focused_buffer(&mut h), a_buffer, "backward returns to a.rs");
        assert_eq!(h.primary_head_offset(), 1, "at the recorded a.rs offset");
    }

    #[test]
    fn goto_last_line_records_the_origin() {
        let mut h = TestHarness::with_size(40, 8);
        let path = h.write_file("s.rs", "line0\nline1\nline2\nline3\n");
        h.open_file(&path);
        assert_eq!(h.primary_head_offset(), 0);

        action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoLastLine);
        assert_ne!(h.primary_head_offset(), 0, "moved off the origin line");

        action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
        assert_eq!(h.primary_head_offset(), 0, "backward restores the origin");
    }

    #[test]
    fn search_submit_records_once_and_repeats_stay_quiet() {
        let mut h = TestHarness::with_size(40, 10);
        let path = h.write_file("s.rs", "abc def abc xyz abc\n");
        h.open_file(&path);
        let before = jumplist_len(&mut h);

        h.type_keys("/");
        h.type_text("abc");
        h.type_keys("enter");
        assert_eq!(
            jumplist_len(&mut h),
            before + 1,
            "search submit records the origin once"
        );

        action_handlers::dispatch(&mut h.stoat, &stoat_action::SearchNext);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::SearchNext);
        assert_eq!(
            jumplist_len(&mut h),
            before + 1,
            "n and N repeats record nothing"
        );
    }
}

//! The diff view's back and forward hops, which walk changes until a jump
//! leaves the change and then walk the jumplist until the reader is on it
//! again.
//!
//! A reader of a diff jumps from a change to the definition of a type it names,
//! and from there to another. The way back is jump history, and the way on,
//! once back at the change, is the change walk. One pair of buttons serves
//! both by reading where the last walk landed and whether a jump has recorded
//! a position since.
//!
//! A changes view never retraces, so there the buttons walk its hunks on every
//! press, as [`in_changes_view`] explains.

use super::{
    focused_editor_mut, jump,
    movement::{self, ChangeDir, ChangeUnit},
};
use crate::{
    app::{Stoat, UpdateEffect},
    jumplist::ChangeLanding,
    pane::FocusTarget,
};
use stoat_text::cursor_offset;

/// The handler behind `DiffBack`.
///
/// Walks to the next change, the way `GotoNextChange` does. While a jump has
/// carried the cursor off the change the last walk landed on, it retraces the
/// jumplist instead, one position per press, until the cursor is on that
/// change again. In a changes view it always walks.
pub fn back(stoat: &mut Stoat) -> UpdateEffect {
    let left = left_since_landing(stoat);
    let on = on_landing(stoat);
    let retrace = jumped_away(left, on) && !in_changes_view(stoat);
    tracing::debug!(
        target: "stoat::change_walk",
        left_since_landing = left,
        on_landing = on,
        landing_generation = ?landing(stoat).map(|(landing, _)| landing.generation),
        list_generation = ?landing(stoat).map(|(_, list)| list),
        route = if retrace { "retrace jumplist" } else { "walk changes" },
        "diff back"
    );
    if retrace && matches!(jump::jump_backward(stoat), UpdateEffect::Redraw) {
        return UpdateEffect::Redraw;
    }
    movement::goto_change_impl(stoat, ChangeDir::Next, 1, ChangeUnit::Tree)
}

/// The handler behind `DiffForward`.
///
/// Re-advances the jumplist while a jump has left the change the last walk
/// landed on and forward history remains. Otherwise walks to the previous
/// change, the way `GotoPrevChange` does, so a back to the change followed by a
/// forward returns to where the jump went. In a changes view it always walks.
pub fn forward(stoat: &mut Stoat) -> UpdateEffect {
    let left = left_since_landing(stoat);
    let retrace = left && !in_changes_view(stoat);
    tracing::debug!(
        target: "stoat::change_walk",
        left_since_landing = left,
        on_landing = on_landing(stoat),
        landing_generation = ?landing(stoat).map(|(landing, _)| landing.generation),
        list_generation = ?landing(stoat).map(|(_, list)| list),
        route = if retrace { "retrace jumplist" } else { "walk changes" },
        "diff forward"
    );
    if retrace && matches!(jump::jump_forward(stoat), UpdateEffect::Redraw) {
        return UpdateEffect::Redraw;
    }
    movement::goto_change_impl(stoat, ChangeDir::Prev, 1, ChangeUnit::Tree)
}

/// The focused split pane's last change landing, with the pane's jumplist push
/// count now. `None` on a dock, or before any walk landed.
fn landing(stoat: &Stoat) -> Option<(ChangeLanding, u64)> {
    let ws = stoat.active_workspace();
    if let FocusTarget::Dock(_) = ws.focus {
        return None;
    }
    let pane = ws.panes.pane(ws.panes.focus());
    let landing = pane.change_landing.clone()?;
    Some((landing, pane.jumplist.generation()))
}

/// Whether the jumplist recorded a position after the last walk landed.
fn left_since_landing(stoat: &Stoat) -> bool {
    landing(stoat).is_some_and(|(landing, generation)| generation > landing.generation)
}

/// Whether a jump has carried the cursor off the change the last walk landed
/// on.
///
/// A cursor moved by hand records no jump, so it still walks changes, and a
/// history walk that arrives back on the change stops retracing.
fn jumped_away(left_since_landing: bool, on_landing: bool) -> bool {
    left_since_landing && !on_landing
}

/// Whether the focused split pane shows a changes view, where the buttons walk
/// changes on every press.
///
/// A jumplist entry applied in a changes view swaps the pane to a plain editor
/// on the entry's file, since [`jump::apply_jump_entry`] reads the pane's
/// buffer from an editor view alone. That drops the review surface. The view's
/// excerpt hops record a landing on every crossing, so the walk is the answer
/// there.
fn in_changes_view(stoat: &Stoat) -> bool {
    stoat.active_workspace().focused_diff_changes_id().is_some()
}

/// Whether the cursor sits on a row of the change the last walk landed on.
///
/// A jump whose target lies inside the landing's own rows reads as a return,
/// so a back from there walks to the next change instead of retracing. A
/// definition inside the hunk under review is rare, and the reader is on the
/// change either way, so the wart stays.
fn on_landing(stoat: &mut Stoat) -> bool {
    let Some((landing, _)) = landing(stoat) else {
        return false;
    };
    let Some(editor) = focused_editor_mut(stoat) else {
        return false;
    };
    if editor.buffer_id != landing.entry.buffer_id {
        return false;
    }

    let snapshot = editor.display_map.snapshot();
    let buffer = snapshot.buffer_snapshot();
    let rope = buffer.rope();
    let row = |offset| rope.offset_to_point(offset).row;
    let cursor_row = {
        let sel = editor.selections.newest_anchor();
        row(cursor_offset(
            rope,
            buffer.resolve_anchor(&sel.tail()),
            buffer.resolve_anchor(&sel.head()),
        ))
    };

    landing.entry.selections.iter().any(|sel| {
        let start = buffer.resolve_anchor(&sel.start);
        let end = buffer.resolve_anchor(&sel.end);
        // A stop ends at the first offset of the row after it, so its last row
        // is the one that holds the offset before its end.
        let last = if end > start { end - 1 } else { start };
        (row(start)..=row(last)).contains(&cursor_row)
    })
}

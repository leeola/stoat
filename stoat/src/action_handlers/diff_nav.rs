//! The diff view's back and forward hops, which walk changes until a jump
//! leaves the change and then walk the jumplist until the reader is on it
//! again.
//!
//! A reader of a diff jumps from a change to the definition of a type it names,
//! and from there to another. The way back is jump history, and the way on,
//! once back at the change, is the change walk. One pair of buttons serves
//! both by reading where the last walk landed and whether a jump has recorded
//! a position since.

use super::{
    focused_editor_mut, jump,
    movement::{self, ChangeDir},
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
/// change again.
pub(crate) fn back(stoat: &mut Stoat) -> UpdateEffect {
    if jumped_away(stoat) && matches!(jump::jump_backward(stoat), UpdateEffect::Redraw) {
        return UpdateEffect::Redraw;
    }
    movement::goto_change_impl(stoat, ChangeDir::Next, 1)
}

/// The handler behind `DiffForward`.
///
/// Re-advances the jumplist while a jump has left the change the last walk
/// landed on and forward history remains. Otherwise walks to the previous
/// change, the way `GotoPrevChange` does, so a back to the change followed by a
/// forward returns to where the jump went.
pub(crate) fn forward(stoat: &mut Stoat) -> UpdateEffect {
    if left_since_landing(stoat) && matches!(jump::jump_forward(stoat), UpdateEffect::Redraw) {
        return UpdateEffect::Redraw;
    }
    movement::goto_change_impl(stoat, ChangeDir::Prev, 1)
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
fn jumped_away(stoat: &mut Stoat) -> bool {
    left_since_landing(stoat) && !on_landing(stoat)
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

#[cfg(test)]
mod tests {
    use super::{back, forward};
    use crate::{
        action_handlers::{
            focused_editor_mut, jump,
            movement::{self, ChangeDir},
        },
        app::{Stoat, UpdateEffect},
        test_fixture::open_scratch_file,
        test_harness::TestHarness,
    };
    use stoat_text::Point;

    /// Sixty lines with rows 10, 30, and 50 added over the base, in a
    /// twenty-row pane with the diff view on.
    ///
    /// With `walk_first` the cursor starts on the change at row 10 with a
    /// landing recorded, and otherwise on row 0 with none. Pure additions
    /// splice no deleted block, so display rows equal buffer rows.
    fn harness(walk_first: bool) -> TestHarness {
        let lines = |added: bool| {
            (0..60)
                .filter(|i| added || ![10, 30, 50].contains(i))
                .map(|i| format!("l{i}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let mut h = Stoat::test();
        open_scratch_file(&mut h, &lines(true));
        h.seed_current_diff_map(&lines(false));

        let editor_id = h.stoat.focused_editor_ids().expect("editor").0;
        let editor = &mut h.stoat.active_workspace_mut().editors[editor_id];
        editor.set_diff_view(true);
        editor.viewport_rows = Some(20);
        if walk_first {
            movement::goto_change_impl(&mut h.stoat, ChangeDir::Next, 1);
        }
        h
    }

    /// The offset of the start of `row` in the focused buffer.
    fn row_offset(h: &mut TestHarness, row: u32) -> usize {
        let editor = focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        snapshot
            .buffer_snapshot()
            .rope()
            .point_to_offset(Point::new(row, 0))
    }

    /// Jump to the start of `row` the way a definition lookup does, recording
    /// the position it leaves.
    fn jump_to_row(h: &mut TestHarness, row: u32) {
        jump::push_jump(&mut h.stoat);
        let offset = row_offset(h, row);
        movement::jump_to_offset(&mut h.stoat, offset);
    }

    /// The cursor row after each of `presses`, in order.
    fn rows_after(h: &mut TestHarness, presses: &[fn(&mut Stoat) -> UpdateEffect]) -> Vec<u32> {
        presses
            .iter()
            .map(|press| {
                press(&mut h.stoat);
                h.cursor_display_positions()[0].0
            })
            .collect()
    }

    #[test]
    fn back_at_the_landing_walks_the_changes() {
        let mut h = harness(true);
        assert_eq!(rows_after(&mut h, &[back, back]), [30, 50]);
    }

    #[test]
    fn back_with_no_landing_walks_the_changes() {
        let mut h = harness(false);
        assert_eq!(rows_after(&mut h, &[back]), [10]);
    }

    /// Two jumps out of the change at row 10 retrace one position per press,
    /// and the press that finds the cursor on the change again walks on.
    #[test]
    fn back_retraces_a_jump_to_the_change_then_walks_on() {
        let mut h = harness(true);
        jump_to_row(&mut h, 20);
        jump_to_row(&mut h, 40);
        assert_eq!(rows_after(&mut h, &[back, back, back]), [20, 10, 30]);
    }

    /// The change's stop ends where the row below it starts, and that row is
    /// still off the change.
    #[test]
    fn back_retraces_from_the_row_below_the_change() {
        let mut h = harness(true);
        jump_to_row(&mut h, 11);
        assert_eq!(rows_after(&mut h, &[back]), [10]);
    }

    /// Forward returns to where the jump went, and with no forward history
    /// left it walks to the previous change.
    #[test]
    fn forward_re_advances_the_jump_then_walks_back_a_change() {
        let mut h = harness(true);
        jump_to_row(&mut h, 20);
        assert_eq!(rows_after(&mut h, &[back, forward, forward]), [10, 20, 10]);
    }

    #[test]
    fn forward_at_the_landing_with_no_jump_walks_to_the_previous_change() {
        let mut h = harness(true);
        assert_eq!(rows_after(&mut h, &[back, forward]), [30, 10]);
    }

    /// A cursor moved by hand records no jump, so it walks on from where it
    /// stands rather than retracing.
    #[test]
    fn a_hand_moved_cursor_off_the_landing_still_walks_the_changes() {
        let mut h = harness(true);
        let offset = row_offset(&mut h, 20);
        movement::jump_to_offset(&mut h.stoat, offset);
        assert_eq!(rows_after(&mut h, &[back]), [30]);
    }
}

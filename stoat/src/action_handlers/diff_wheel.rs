//! The mouse wheel in the diff view, which scrolls until the change under the
//! cursor passes a jump line and then walks to the next change.
//!
//! A reader of a large diff scrolls through a long change and skips between
//! short ones, and one gesture does both. A notch scrolls while the change
//! still reaches below the line. Once the change has risen past the line, the
//! next notch walks. The walk is the change hop the keys run, so both reach the
//! same stops in the same order.

use super::{
    movement::{self, ChangeDir},
    view::{self, DEFAULT_VIEWPORT_ROWS},
};
use crate::{
    app::{Stoat, UpdateEffect, DIFF_WHEEL_COOLDOWN, DIFF_WHEEL_JUMP_TRAVEL},
    editor_state::{EditorId, EditorState},
};
use stoat_config::Settings;
use stoat_text::cursor_offset;

/// Jump line for a config that leaves `editor.diff_wheel_jump` unset.
const DEFAULT_JUMP_FRACTION: f64 = 0.25;

/// Scroll `editor_id` by `lines` of wheel travel, or walk to the next change
/// once the change under the cursor has passed the jump line.
///
/// The editor must be the focused one, because a walk moves the focused
/// selection. Travel at an open jump line holds the view and accrues until one
/// notch's worth arrives. A walk inside [`DIFF_WHEEL_COOLDOWN`] of the last one
/// is dropped.
pub(crate) fn scroll_or_jump(stoat: &mut Stoat, editor_id: EditorId, lines: f32) -> UpdateEffect {
    if lines == 0.0 {
        return UpdateEffect::None;
    }
    let dir = match lines > 0.0 {
        true => ChangeDir::Next,
        false => ChangeDir::Prev,
    };
    let fraction = jump_fraction(&stoat.settings);

    let Some(editor) = stoat.active_workspace_mut().editors.get_mut(editor_id) else {
        return UpdateEffect::None;
    };
    if !gate_open(editor, dir, fraction) {
        view::wheel_scroll_by(editor, lines);
        stoat.diff_wheel_travel = 0.0;
        return UpdateEffect::None;
    }

    if stoat.diff_wheel_travel * lines < 0.0 {
        stoat.diff_wheel_travel = 0.0;
    }
    stoat.diff_wheel_travel += lines;
    if stoat.diff_wheel_travel.abs() < DIFF_WHEEL_JUMP_TRAVEL {
        return UpdateEffect::None;
    }
    stoat.diff_wheel_travel = 0.0;

    let now = stoat.executor.now();
    if stoat
        .diff_wheel_last
        .is_some_and(|last| now.duration_since(last) < DIFF_WHEEL_COOLDOWN)
    {
        return UpdateEffect::None;
    }
    stoat.diff_wheel_last = Some(now);
    movement::goto_change_impl(stoat, dir, 1)
}

/// Whether the change under the cursor has passed the jump line for `dir`, so
/// that accrued travel walks rather than scrolls.
///
/// A view already at its bound in `dir` reads as open, since no scroll brings
/// the change to the line. A cursor with no change to set out from reads as
/// open too, so the first notch walks to the first change.
///
/// Both sides measure against the wheel's scroll target, not the eased offset,
/// so the gate reads the view the reader is about to see.
fn gate_open(editor: &mut EditorState, dir: ChangeDir, fraction: f32) -> bool {
    let max_scroll = view::max_scroll_offset(editor);
    let top = editor.scroll_row as f32 + editor.scroll_frac;
    let viewport = editor.viewport_rows.unwrap_or(DEFAULT_VIEWPORT_ROWS).max(1) as f32;

    let span = {
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let rope = buffer_snapshot.rope();
        let sel = editor.selections.newest_anchor().clone();
        let tail_off = buffer_snapshot.resolve_anchor(&sel.tail());
        let head_off = buffer_snapshot.resolve_anchor(&sel.head());
        let cursor_row = rope
            .offset_to_point(cursor_offset(rope, tail_off, head_off))
            .row;

        let stops = movement::live_hunk_rows(&snapshot, buffer_snapshot);
        let Some(rows) = movement::departure_stop(&stops, cursor_row, dir) else {
            return true;
        };
        movement::stop_display_span(&snapshot, rows)
    };

    match dir {
        ChangeDir::Next => top >= max_scroll || span.end as f32 <= top + viewport * fraction,
        ChangeDir::Prev => top <= 0.0 || span.start as f32 >= top + viewport * (1.0 - fraction),
    }
}

/// The jump line's distance from the top of the pane, as a fraction of the pane
/// height.
///
/// The clamp keeps the line a notch down reads at or above the mirrored line a
/// notch up reads. Past the middle the two lines cross. A short change that a
/// walk centers then sits past both lines, and every notch walks without a
/// scroll between.
fn jump_fraction(settings: &Settings) -> f32 {
    settings
        .diff_wheel_jump
        .unwrap_or(DEFAULT_JUMP_FRACTION)
        .clamp(0.0, 0.5) as f32
}

#[cfg(test)]
mod tests {
    use crate::{
        action_handlers::{
            dispatch,
            movement::{self, ChangeDir},
        },
        app::DIFF_WHEEL_COOLDOWN,
        editor_state::EditorId,
        mouse,
        pane::View,
        test_fixture::open_scratch_file,
        test_harness::TestHarness,
        Stoat,
    };
    use ratatui::layout::Rect;
    use stoat_action::SplitRight;

    /// Sixty lines with rows 10, 30, and 50 added over the base, landed on the
    /// change at row 10 in a twenty-row pane.
    ///
    /// Pure additions splice no deleted block, so display rows equal buffer
    /// rows and each scroll target reads straight off the line numbers.
    fn diff_harness(diff_view: bool) -> (TestHarness, EditorId) {
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
        editor.set_diff_view(diff_view);
        editor.viewport_rows = Some(20);
        movement::goto_change_impl(&mut h.stoat, ChangeDir::Next, 1);
        (h, editor_id)
    }

    /// Spend each of `reports` as one wheel report over the editor's pane.
    fn wheel(h: &mut TestHarness, editor_id: EditorId, reports: &[f32]) {
        for &lines in reports {
            mouse::scroll_view_at(
                &mut h.stoat,
                View::Editor(editor_id),
                Rect::default(),
                lines,
            );
        }
    }

    /// The focused cursors, and the editor's scroll target, which is the view
    /// the reader is about to see.
    fn state(h: &mut TestHarness, editor_id: EditorId) -> (Vec<(u32, u32)>, f32) {
        let editor = &h.stoat.active_workspace().editors[editor_id];
        let top = editor.scroll_row as f32 + editor.scroll_frac;
        (h.cursor_display_positions(), top)
    }

    /// Backdate the last walk past the cooldown, rather than sleep, so the next
    /// walk a test drives lands.
    fn clear_cooldown(h: &mut TestHarness) {
        h.stoat.diff_wheel_last = h.stoat.diff_wheel_last.map(|t| t - DIFF_WHEEL_COOLDOWN);
    }

    #[test]
    fn a_notch_scrolls_while_the_change_hangs_below_the_jump_line() {
        let (mut h, id) = diff_harness(true);
        wheel(&mut h, id, &[1.0, 1.0]);
        assert_eq!(
            state(&mut h, id),
            (vec![(10, 0)], 6.0),
            "row 11 sits below the line"
        );
    }

    #[test]
    fn a_notch_walks_once_the_change_has_risen_past_the_jump_line() {
        let (mut h, id) = diff_harness(true);
        wheel(&mut h, id, &[1.0, 1.0, 1.0]);
        assert_eq!(
            state(&mut h, id),
            (vec![(30, 0)], 20.0),
            "the walk centers row 30"
        );
    }

    #[test]
    fn sub_notch_travel_holds_the_view_until_a_notch_accrues() {
        let (mut h, id) = diff_harness(true);
        wheel(&mut h, id, &[1.0, 1.0, 0.3, 0.3, 0.3]);
        assert_eq!(
            state(&mut h, id),
            (vec![(10, 0)], 6.0),
            "0.9 of a notch holds"
        );

        wheel(&mut h, id, &[0.3]);
        assert_eq!(
            state(&mut h, id),
            (vec![(30, 0)], 20.0),
            "a whole notch walks"
        );
    }

    #[test]
    fn a_closed_gate_clears_the_accrued_travel() {
        let (mut h, id) = diff_harness(true);
        wheel(&mut h, id, &[1.0, 1.0, 0.6]);
        {
            let editor = &mut h.stoat.active_workspace_mut().editors[id];
            editor.scroll_row = 0;
            editor.scroll_frac = 0.0;
        }
        wheel(&mut h, id, &[1.0, 1.0, 0.6]);
        assert_eq!(
            state(&mut h, id),
            (vec![(10, 0)], 6.0),
            "the two 0.6 reports never sum"
        );
    }

    #[test]
    fn a_notch_up_walks_back_once_the_change_has_sunk_past_the_mirror_line() {
        let (mut h, id) = diff_harness(true);
        wheel(&mut h, id, &[1.0, 1.0, 1.0]);
        clear_cooldown(&mut h);
        wheel(&mut h, id, &[-1.0, -1.0]);
        assert_eq!(
            state(&mut h, id),
            (vec![(30, 0)], 14.0),
            "row 30 sits above the line"
        );

        wheel(&mut h, id, &[-1.0]);
        assert_eq!(
            state(&mut h, id),
            (vec![(10, 0)], 0.0),
            "the walk back lands row 10"
        );
    }

    #[test]
    fn a_notch_inside_the_cooldown_is_dropped_at_the_file_end() {
        let (mut h, id) = diff_harness(true);
        wheel(&mut h, id, &[1.0, 1.0, 1.0]);
        clear_cooldown(&mut h);
        wheel(&mut h, id, &[1.0, 1.0, 1.0, 1.0]);
        assert_eq!(
            (
                state(&mut h, id),
                h.stoat.pending_changed_file_jump.is_some()
            ),
            ((vec![(50, 0)], 40.0), false),
            "the notch after the walk to row 50 is dropped",
        );

        clear_cooldown(&mut h);
        wheel(&mut h, id, &[1.0]);
        assert!(
            h.stoat.pending_changed_file_jump.is_some(),
            "past the cooldown the bound opens the gate onto the next file",
        );
    }

    #[test]
    fn a_notch_up_at_the_file_top_hops_to_the_prior_file() {
        let (mut h, id) = diff_harness(true);
        wheel(&mut h, id, &[-1.0]);
        assert!(
            h.stoat.pending_changed_file_jump.is_some(),
            "no scroll brings row 10 down to the line",
        );
    }

    #[test]
    fn a_reversal_restarts_the_accrued_travel() {
        let (mut h, id) = diff_harness(true);
        movement::set_cursor_row(&mut h.stoat.active_workspace_mut().editors[id], 0);
        wheel(&mut h, id, &[0.9, -0.3, 0.9]);
        assert_eq!(
            state(&mut h, id),
            (vec![(0, 0)], 0.0),
            "above the first change both gates are open, and only the last 0.9 counts",
        );
    }

    #[test]
    fn outside_the_diff_view_every_notch_scrolls() {
        let (mut h, id) = diff_harness(false);
        wheel(&mut h, id, &[1.0, 1.0, 1.0]);
        assert_eq!(state(&mut h, id), (vec![(10, 0)], 9.0));
    }

    #[test]
    fn a_notch_over_an_unfocused_diff_pane_scrolls() {
        let (mut h, id) = diff_harness(true);
        dispatch(&mut h.stoat, &SplitRight);
        wheel(&mut h, id, &[1.0, 1.0, 1.0]);
        let editor = &h.stoat.active_workspace().editors[id];
        assert_eq!(
            editor.scroll_row as f32 + editor.scroll_frac,
            9.0,
            "a walk would move the focused pane, not the one under the pointer",
        );
    }
}

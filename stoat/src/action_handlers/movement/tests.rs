use super::*;
use crate::{
    pane::View,
    test_harness::{
        editor::{focused_buffer_path, focused_cursor_point, focused_head_row, place_cursor},
        TestHarness,
    },
};

/// Seed a repo with two changed files, each carrying one hunk at line 1.
/// `changed_files` sorts by path, so a.rs is index 0 and b.rs is index 1.
fn stage_two_changed_files(h: &mut TestHarness) -> PathBuf {
    let workdir = PathBuf::from("/repo");
    h.stage_review_scenario(
        &workdir,
        &[
            ("a.rs", "a\nb\nc\n", "a\nX\nc\n"),
            ("b.rs", "d\ne\nf\n", "d\nY\nf\n"),
        ],
    );
    h.stoat.set_diff_warm_auto(true);
    workdir
}

#[test]
fn next_change_crosses_to_next_file_first_hunk() {
    let mut h = TestHarness::with_size(40, 20);
    let workdir = stage_two_changed_files(&mut h);
    h.open_file(&workdir.join("a.rs"));
    h.settle_diff_jobs();
    {
        let editor = focused_editor_mut(&mut h.stoat).expect("editor");
        editor.set_diff_view(true);
        set_cursor_row(editor, 1);
    }

    goto_change(&mut h.stoat, ChangeDir::Next);
    h.settle();

    assert_eq!(
        focused_buffer_path(&h.stoat),
        workdir.join("b.rs"),
        "crossed to b.rs"
    );
    assert_eq!(
        focused_head_row(&mut h.stoat),
        1,
        "landed on b.rs's first hunk"
    );
    assert!(
        focused_editor_mut(&mut h.stoat).expect("editor").diff_view,
        "diff_view carried across the file boundary"
    );
}

/// The hop leaves the keypress with nothing but a scan armed, and lands
/// when the pump picks the answer up.
///
/// What this pins is the deferral rather than the thread. The test
/// scheduler runs a blocking closure inline on purpose, so the scan's own
/// work happens on this stack whatever the handler does, and no count of
/// what it touched could tell the two apart. Where the open and the jump
/// happen can be told apart, and that is what moved.
#[test]
fn crossing_files_waits_for_the_scan_before_it_opens_anything() {
    let mut h = TestHarness::with_size(40, 20);
    let workdir = stage_two_changed_files(&mut h);
    h.open_file(&workdir.join("a.rs"));
    h.settle_diff_jobs();
    {
        let editor = focused_editor_mut(&mut h.stoat).expect("editor");
        editor.set_diff_view(true);
        set_cursor_row(editor, 1);
    }

    goto_change(&mut h.stoat, ChangeDir::Next);
    assert_eq!(
        focused_buffer_path(&h.stoat),
        workdir.join("a.rs"),
        "the keypress opens nothing itself",
    );
    assert!(
        h.stoat.pending_changed_file_jump.is_some(),
        "it leaves a scan for the pump to apply",
    );

    h.settle();
    assert_eq!(
        focused_buffer_path(&h.stoat),
        workdir.join("b.rs"),
        "which is where the hop lands",
    );
    assert!(
        h.stoat.pending_changed_file_jump.is_none(),
        "and the scan is spent once applied",
    );
}

#[test]
fn next_change_crosses_into_a_lone_changed_file_from_an_unchanged_buffer() {
    let mut h = TestHarness::with_size(40, 20);
    let workdir = PathBuf::from("/repo");
    h.stage_review_scenario(&workdir, &[("changed.rs", "a\nb\nc\n", "a\nX\nc\n")]);
    h.stoat.set_diff_warm_auto(true);

    // The focused editor is the pathless scratch, absent from the changed
    // list, mirroring the `stoat review` startup on a one-file working tree.
    focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(true);

    goto_change(&mut h.stoat, ChangeDir::Next);
    h.settle();

    assert_eq!(
        focused_buffer_path(&h.stoat),
        workdir.join("changed.rs"),
        "crossed into the sole changed file from the unchanged scratch",
    );
    assert_eq!(
        focused_head_row(&mut h.stoat),
        1,
        "landed on the changed file's first hunk",
    );
}

/// Landing the cursor on a row leaves that cursor and nothing else.
///
/// A fresh collection is seeded with a zero-width selection at offset 0, so
/// a landing that appends to one rather than replacing it leaves a second
/// cursor at the top of the file. It paints there, and every later insert
/// applies at it too, since an edit applies at every cursor.
#[test]
fn landing_on_a_row_leaves_no_cursor_behind_at_the_top() {
    let mut h = TestHarness::with_size(20, 8);
    let path = h.write_file("s.txt", "alpha\nbeta\ngamma\n");
    h.open_file(&path);

    set_cursor_row(focused_editor_mut(&mut h.stoat).expect("editor"), 2);

    let spans = h.selection_spans();
    assert_eq!(spans.len(), 1, "the landing is the only selection");
    assert_eq!(
        spans[0].0, 11,
        "and it sits on the row asked for, not at the file top",
    );
}

#[test]
fn prev_change_crosses_to_previous_file_last_hunk() {
    let mut h = TestHarness::with_size(40, 20);
    let workdir = stage_two_changed_files(&mut h);
    h.open_file(&workdir.join("b.rs"));
    h.settle_diff_jobs();
    set_cursor_row(focused_editor_mut(&mut h.stoat).expect("editor"), 1);

    goto_change(&mut h.stoat, ChangeDir::Prev);
    h.settle();

    assert_eq!(
        focused_buffer_path(&h.stoat),
        workdir.join("a.rs"),
        "crossed back to a.rs"
    );
    assert_eq!(
        focused_head_row(&mut h.stoat),
        1,
        "landed on a.rs's last hunk"
    );
}

#[test]
fn next_change_wraps_from_the_last_file_with_a_message() {
    let mut h = TestHarness::with_size(40, 20);
    let workdir = stage_two_changed_files(&mut h);
    h.open_file(&workdir.join("b.rs"));
    h.settle_diff_jobs();
    set_cursor_row(focused_editor_mut(&mut h.stoat).expect("editor"), 1);

    goto_change(&mut h.stoat, ChangeDir::Next);
    h.settle();

    assert_eq!(
        focused_buffer_path(&h.stoat),
        workdir.join("a.rs"),
        "wrapped to a.rs"
    );
    assert_eq!(h.stoat.pending_message.as_deref(), Some("wrapped"));
}

#[test]
fn next_change_with_one_changed_file_reports_no_more_changes() {
    let mut h = TestHarness::with_size(40, 20);
    let workdir = PathBuf::from("/repo");
    h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\n", "a\nX\nc\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(&workdir.join("a.rs"));
    h.settle_diff_jobs();
    set_cursor_row(focused_editor_mut(&mut h.stoat).expect("editor"), 1);

    goto_change(&mut h.stoat, ChangeDir::Next);
    h.settle();

    assert_eq!(
        focused_buffer_path(&h.stoat),
        workdir.join("a.rs"),
        "stayed on a.rs"
    );
    assert_eq!(h.stoat.pending_message.as_deref(), Some("no more changes"));
}

#[test]
fn next_change_skips_untracked_files() {
    let mut h = TestHarness::with_size(40, 20);
    let workdir = PathBuf::from("/repo");
    h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\n", "a\nX\nc\n")]);
    h.fake_git().add_repo(&workdir).untracked("u.rs");
    h.stoat.set_diff_warm_auto(true);
    h.open_file(&workdir.join("a.rs"));
    h.settle_diff_jobs();
    set_cursor_row(focused_editor_mut(&mut h.stoat).expect("editor"), 1);

    goto_change(&mut h.stoat, ChangeDir::Next);
    h.settle();

    assert_eq!(
        focused_buffer_path(&h.stoat),
        workdir.join("a.rs"),
        "stayed on a.rs; untracked u.rs is not a nav target",
    );
    assert_eq!(h.stoat.pending_message.as_deref(), Some("no more changes"));
}

#[test]
fn snapshot_count_jump_keeps_cursor_visible() {
    let mut h = TestHarness::with_size(40, 12);
    let body: String = (0..80).map(|i| format!("line {i:02}\n")).collect();
    let path = h.write_file("long.rs", &body);
    h.open_file(&path);

    h.stoat.pending_count = Some(50);
    h.type_keys("j");
    h.assert_snapshot("count_jump_keeps_cursor_visible");
}

#[test]
fn vertical_motion_keeps_a_visual_column_across_wide_characters() {
    // Each ideograph is three bytes and two cells wide, so the third one
    // starts at byte 6 and at visual column 4. The ASCII line below has its
    // visual column 4 at byte 4.
    let mut h = TestHarness::with_size(40, 12);
    let path = h.write_file("cjk.rs", "\u{4e00}\u{4e01}\u{4e02}\u{4e03}\nabcdefgh\n");
    h.open_file(&path);
    {
        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        place_cursor(editor, 0, 6);
    }

    move_vertical(&mut h.stoat, 1, false);

    assert_eq!(
        focused_cursor_point(&mut h.stoat),
        Point::new(1, 4),
        "j holds the visual column, which is a different byte column",
    );
}

#[test]
fn vertical_motion_keeps_a_visual_column_across_a_tab() {
    // The tab occupies cells 0 through 3, so the `a` after it is byte 1 and
    // visual column 4.
    let mut h = TestHarness::with_size(40, 12);
    let path = h.write_file("tabbed.rs", "\tabc\nxyzwuvst\n");
    h.open_file(&path);
    {
        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        place_cursor(editor, 0, 1);
    }

    move_vertical(&mut h.stoat, 1, false);

    assert_eq!(
        focused_cursor_point(&mut h.stoat),
        Point::new(1, 4),
        "the cells the tab occupies count toward the column",
    );
}

#[test]
fn vertical_motion_lands_on_the_visual_column_of_a_wide_line() {
    // The other direction. Leaving an ASCII line at visual column 4 has to
    // land on the ideograph that occupies cells 4 and 5, which starts at
    // byte 6, not on byte 4 inside the one before it.
    let mut h = TestHarness::with_size(40, 12);
    let path = h.write_file("cjk.rs", "abcdefgh\n\u{4e00}\u{4e01}\u{4e02}\u{4e03}\n");
    h.open_file(&path);
    {
        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        place_cursor(editor, 0, 4);
    }

    move_vertical(&mut h.stoat, 1, false);

    assert_eq!(focused_cursor_point(&mut h.stoat), Point::new(1, 6));
}

#[test]
fn vertical_motion_lands_past_a_tab_on_the_target_line() {
    // Visual column 4 on the line below is the character after the tab, at
    // byte 1, since the tab occupies the four cells before it.
    let mut h = TestHarness::with_size(40, 12);
    let path = h.write_file("tabbed.rs", "xyzwuvst\n\tabc\n");
    h.open_file(&path);
    {
        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        place_cursor(editor, 0, 4);
    }

    move_vertical(&mut h.stoat, 1, false);

    assert_eq!(focused_cursor_point(&mut h.stoat), Point::new(1, 1));
}

#[test]
fn vertical_motion_returns_to_the_visual_column_it_left() {
    // Down onto a line that cannot reach the column, then back up. The goal
    // is what survives the short line, not where the cursor sat on it.
    let mut h = TestHarness::with_size(40, 12);
    let path = h.write_file(
        "mixed.rs",
        "\u{4e00}\u{4e01}\u{4e02}\u{4e03}\nab\nabcdefgh\n",
    );
    h.open_file(&path);
    {
        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        place_cursor(editor, 0, 6);
    }

    move_vertical(&mut h.stoat, 1, false);
    assert_eq!(
        focused_cursor_point(&mut h.stoat),
        Point::new(1, 2),
        "the short line clamps to its end",
    );

    move_vertical(&mut h.stoat, 1, false);
    assert_eq!(
        focused_cursor_point(&mut h.stoat),
        Point::new(2, 4),
        "and the column comes back on the line that can hold it",
    );
}

/// A goal column that overruns the landing line clamps onto that line's
/// break, and extending covers the same cell as moving does.
///
/// The two arms differ only in whether the anchor is left behind, never in
/// which cell the cursor ends up on. An extend that stopped a cell short
/// would make a select-mode motion cover less than the plain motion it
/// mirrors, so `vjd` and `jd` would disagree about the line break.
#[test]
fn vertical_move_and_extend_agree_on_the_cell_at_a_short_line_end() {
    let cell_after = |extend: bool| {
        let mut h = TestHarness::with_size(40, 12);
        let path = h.write_file("short.rs", "abcdef\nxy\n");
        h.open_file(&path);
        {
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            place_cursor(editor, 0, 5);
        }
        move_vertical(&mut h.stoat, 1, extend);
        focused_cursor_point(&mut h.stoat)
    };

    assert_eq!(
        cell_after(false),
        Point::new(1, 2),
        "the goal overruns \"xy\", so j lands on its line break",
    );
    assert_eq!(
        cell_after(true),
        cell_after(false),
        "and vj lands there too"
    );
}

/// The same agreement where the landing line's last cell is a decomposed
/// accent, which spans more than one codepoint.
///
/// A cell and a codepoint are the same distance on the ASCII line above, so
/// only a line like this one distinguishes a landing measured in cells from
/// one measured in codepoints. A codepoint-sized offset from the line break
/// falls inside the cluster, and the anchor layer resolves that to the
/// cluster's start, leaving the cursor a whole cell short.
#[test]
fn vertical_move_and_extend_agree_at_an_accented_line_end() {
    // Line 0 is "a" then e-plus-combining-acute: four bytes, two cells.
    let cell_after = |extend: bool| {
        let mut h = TestHarness::with_size(40, 12);
        let path = h.write_file("accent.rs", "ae\u{301}\nabcdefgh\n");
        h.open_file(&path);
        {
            let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
            place_cursor(editor, 1, 6);
        }
        move_vertical(&mut h.stoat, -1, extend);
        focused_cursor_point(&mut h.stoat)
    };

    assert_eq!(
        cell_after(false),
        Point::new(0, 4),
        "the goal overruns the accented line, so k lands on its line break",
    );
    assert_eq!(
        cell_after(true),
        cell_after(false),
        "and vk lands there too, not back on the accented cluster",
    );
}

#[test]
fn count_vertical_motion_clamps_at_buffer_edge() {
    let mut h = TestHarness::with_size(40, 12);
    let path = h.write_file("short.rs", "a\nb\nc");
    h.open_file(&path);

    h.stoat.pending_count = Some(10000);
    h.type_keys("j");
    assert_eq!(
        focused_head_row(&mut h.stoat),
        2,
        "an overshooting count-down clamps to the last line",
    );

    h.stoat.pending_count = Some(10000);
    h.type_keys("k");
    assert_eq!(
        focused_head_row(&mut h.stoat),
        0,
        "an overshooting count-up clamps to the first line",
    );
}

/// A harness whose focused editor soft-wraps a long first line into several
/// display rows, followed by a short line, with the cursor at the start of
/// the short buffer line (row 1, column 0). Wrap is set directly so no render
/// clobbers it.
fn wrapped_pane_cursor_on_short_line() -> TestHarness {
    let mut h = TestHarness::with_size(40, 12);
    let path = h.write_file("wrap.rs", &format!("{}\nshort\n", "a".repeat(30)));
    h.open_file(&path);
    let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
    editor.viewport_rows = Some(10);
    editor.display_map.set_wrap_width(Some(10));
    set_cursor_row(editor, 1);
    h
}

#[test]
fn move_up_under_wrap_lands_on_the_buffer_line_not_its_wrapped_tail() {
    let mut h = wrapped_pane_cursor_on_short_line();
    move_vertical(&mut h.stoat, -1, false);
    assert_eq!(
        focused_cursor_point(&mut h.stoat),
        Point::new(0, 0),
        "k moves up one buffer line to column 0, not into the wrapped tail",
    );
}

#[test]
fn vertical_motion_under_wrap_round_trips() {
    let mut h = wrapped_pane_cursor_on_short_line();
    move_vertical(&mut h.stoat, -1, false);
    assert_eq!(
        focused_cursor_point(&mut h.stoat),
        Point::new(0, 0),
        "k reaches the long line",
    );
    move_vertical(&mut h.stoat, 1, false);
    assert_eq!(
        focused_cursor_point(&mut h.stoat),
        Point::new(1, 0),
        "j returns to the short line",
    );
}

#[test]
fn count_up_under_wrap_crosses_the_whole_wrapped_line() {
    let mut h = wrapped_pane_cursor_on_short_line();
    h.stoat.pending_count = Some(1);
    move_vertical(&mut h.stoat, -1, false);
    assert_eq!(
        focused_cursor_point(&mut h.stoat),
        Point::new(0, 0),
        "1k crosses the entire wrapped line as one buffer-line step",
    );
}

#[test]
fn extend_up_under_wrap_extends_the_head_by_a_buffer_line() {
    let mut h = wrapped_pane_cursor_on_short_line();
    move_vertical(&mut h.stoat, -1, true);
    assert_eq!(
        focused_head_row(&mut h.stoat),
        0,
        "extend-up moves the head up one buffer line under wrap",
    );
}

fn set_range(h: &mut TestHarness, start: usize, end: usize) {
    let editor = focused_editor_mut(&mut h.stoat).expect("editor");
    let snapshot = editor.display_map.snapshot();
    let buf = snapshot.buffer_snapshot();
    let start_anchor = buf.anchor_at(start, Bias::Right);
    let end_anchor = buf.anchor_at(end, Bias::Right);
    editor.selections.transform(buf, |sel| Selection {
        id: sel.id,
        start: start_anchor,
        end: end_anchor,
        reversed: false,
        goal: SelectionGoal::None,
    });
}

#[test]
fn extend_to_line_bounds_covers_full_lines() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\ndef\nghi\n");
    h.open_file(&path);
    // "bc\nd" spans the middle of line 0 through the middle of line 1.
    set_range(&mut h, 1, 5);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExtendToLineBounds);
    assert_eq!(h.selection_spans(), vec![(0, 8, false)]);
}

#[test]
fn shrink_to_line_bounds_trims_partial_lines() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\ndef\nghi\n");
    h.open_file(&path);
    // Line 0 from its middle through line 2's middle: only line 1 is whole.
    set_range(&mut h, 1, 9);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ShrinkToLineBounds);
    assert_eq!(h.selection_spans(), vec![(4, 8, false)]);
}

#[test]
fn shrink_to_line_bounds_within_one_line_is_noop() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abcdef\n");
    h.open_file(&path);
    set_range(&mut h, 1, 4);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ShrinkToLineBounds);
    assert_eq!(h.selection_spans(), vec![(1, 4, false)]);
}

#[test]
fn ensure_selections_forward_orients_every_selection() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abcdef\n");
    h.open_file(&path);
    {
        let editor = focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf = snapshot.buffer_snapshot();
        let fwd_start = buf.anchor_at(0, Bias::Right);
        let fwd_end = buf.anchor_at(2, Bias::Right);
        editor.selections.transform(buf, |sel| Selection {
            id: sel.id,
            start: fwd_start,
            end: fwd_end,
            reversed: false,
            goal: SelectionGoal::None,
        });
        let reversed = Selection {
            id: 0,
            start: buf.anchor_at(3, Bias::Right),
            end: buf.anchor_at(5, Bias::Right),
            reversed: true,
            goal: SelectionGoal::None,
        };
        editor.selections.extend_with_fresh_ids(vec![reversed], buf);
    }
    assert_eq!(h.selection_spans(), vec![(0, 2, false), (3, 5, true)]);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::EnsureSelectionsForward);
    assert_eq!(h.selection_spans(), vec![(0, 2, false), (3, 5, false)]);
}

fn buffer_string(h: &mut TestHarness) -> String {
    let editor = focused_editor_mut(&mut h.stoat).expect("editor");
    let snapshot = editor.display_map.snapshot();
    snapshot.buffer_snapshot().rope().to_string()
}

fn set_three_single_char_selections(h: &mut TestHarness) {
    set_selections(h, &[(0, 1), (1, 2), (2, 3)]);
}

fn set_selections(h: &mut TestHarness, ranges: &[(usize, usize)]) {
    let (&(first_start, first_end), rest) = ranges.split_first().expect("at least one range");
    let editor = focused_editor_mut(&mut h.stoat).expect("editor");
    let snapshot = editor.display_map.snapshot();
    let buf = snapshot.buffer_snapshot();

    editor.selections.transform(buf, |sel| Selection {
        id: sel.id,
        start: buf.anchor_at(first_start, Bias::Right),
        end: buf.anchor_at(first_end, Bias::Right),
        reversed: false,
        goal: SelectionGoal::None,
    });

    for &(start, end) in rest {
        let sel = Selection {
            id: 0,
            start: buf.anchor_at(start, Bias::Right),
            end: buf.anchor_at(end, Bias::Right),
            reversed: false,
            goal: SelectionGoal::None,
        };
        editor.selections.extend_with_fresh_ids(vec![sel], buf);
    }
}

/// Each edit changes the text's length, so every selection after the first
/// has to move by the running total of the changes before it. Single-digit
/// increments cannot see this, since their replacements are the same length
/// as what they replace.
#[test]
fn incrementing_several_cursors_leaves_each_over_its_own_number() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "9 9 9\n");
    h.open_file(&path);
    set_selections(&mut h, &[(0, 1), (2, 3), (4, 5)]);

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Increment);

    assert_eq!(buffer_string(&mut h), "10 10 10\n");
    assert_eq!(
        h.selection_spans(),
        vec![(0, 2, false), (3, 5, false), (6, 8, false)],
    );
}

/// A decimal written with a leading zero is a fixed-width field, so it
/// keeps that width instead of collapsing to the shortest form.
///
/// Crossing zero moves the width by one, because the sign occupies a column
/// of its own. A literal without a leading zero is left to size itself.
#[test]
fn incrementing_a_zero_padded_decimal_keeps_its_width() {
    for (text, delta, want) in [
        ("007", 1, "008"),
        ("-08", 1, "-07"),
        ("-01", 1, "00"),
        ("01", -2, "-01"),
        ("09", 1, "10"),
        ("7", 1, "8"),
    ] {
        assert_eq!(
            compute_number_delta(text, NumberKind::Decimal, delta).as_deref(),
            Some(want),
            "{text} incremented by {delta}",
        );
    }
}

/// Casing a ligature yields two ASCII letters for three bytes, so the
/// selections after it shift left rather than right.
#[test]
fn uppercasing_several_cursors_leaves_each_over_its_own_word() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "ﬁ ﬁ\n");
    h.open_file(&path);
    set_selections(&mut h, &[(0, 3), (4, 7)]);

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchToUppercase);

    assert_eq!(buffer_string(&mut h), "FI FI\n");
    assert_eq!(h.selection_spans(), vec![(0, 2, false), (3, 5, false)]);
}

#[test]
fn rotate_selection_contents_forward_and_back() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    set_three_single_char_selections(&mut h);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RotateSelectionContentsForward);
    assert_eq!(buffer_string(&mut h), "cab\n");
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RotateSelectionContentsBackward);
    assert_eq!(buffer_string(&mut h), "abc\n");
}

#[test]
fn rotate_selection_contents_backward_shifts_left() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    set_three_single_char_selections(&mut h);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RotateSelectionContentsBackward);
    assert_eq!(buffer_string(&mut h), "bca\n");
}

/// A count is a rotation distance, not a group size.
///
/// Three selections rotated by two land each fragment two places on. That
/// is what tells this apart from rotating count-sized groups by one, which
/// would pair the first two selections and leave the third alone. The
/// distance is clamped to the number of selections, so a count equal to
/// that number is a whole turn and changes nothing.
#[test]
fn rotating_contents_by_a_count_moves_each_fragment_that_far() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    set_three_single_char_selections(&mut h);

    h.stoat.pending_count = Some(2);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RotateSelectionContentsForward);
    assert_eq!(buffer_string(&mut h), "bca\n");
    assert_eq!(
        h.selection_spans(),
        vec![(0, 1, false), (1, 2, false), (2, 3, false)],
    );

    h.stoat.pending_count = Some(3);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RotateSelectionContentsForward);
    assert_eq!(
        buffer_string(&mut h),
        "bca\n",
        "a count equal to the selection count is a whole turn",
    );
}

#[test]
fn join_selections_space_joins_two_lines_and_selects_space() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "ab\ncd\n");
    h.open_file(&path);
    set_range(&mut h, 0, 5);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JoinSelectionsSpace);
    assert_eq!(buffer_string(&mut h), "ab cd\n");
    assert_eq!(h.selection_spans(), vec![(2, 3, false)]);
}

/// Joining several lines leaves selections that are distinct, so removing
/// the primary one removes one of them.
///
/// The collection deletes the primary by id, so selections sharing an id
/// are one selection as far as that goes. Three joined spaces built from
/// the same id would all match the primary's and be removed together,
/// emptying a collection that must never be empty, and the next read of the
/// newest selection panics rather than misbehaving quietly.
#[test]
fn joined_spaces_survive_removing_the_primary_selection() {
    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "a\nb\nc\nd\n");
    h.open_file(&path);
    set_range(&mut h, 0, 7);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JoinSelectionsSpace);
    assert_eq!(buffer_string(&mut h), "a b c d\n");
    assert_eq!(
        h.selection_spans().len(),
        3,
        "each joined space is its own selection",
    );

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RemovePrimarySelection);
    assert_eq!(
        h.selection_spans().len(),
        2,
        "removing the primary removes one of them, not all of them",
    );
}

#[test]
fn join_selections_space_single_line_joins_with_next() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "ab\ncd\n");
    h.open_file(&path);
    set_range(&mut h, 0, 1);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JoinSelectionsSpace);
    assert_eq!(buffer_string(&mut h), "ab cd\n");
    assert_eq!(h.selection_spans(), vec![(2, 3, false)]);
}

#[test]
fn join_selections_drops_second_comment_token() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "// foo\n// bar\n");
    h.open_file(&path);
    h.type_keys("%");
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JoinSelectionsSpace);
    assert_eq!(buffer_string(&mut h), "// foo bar\n");
}

#[test]
fn join_selections_drops_the_second_doc_comment_token_whole() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "/// foo\n/// bar\n");
    h.open_file(&path);
    h.type_keys("%");
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JoinSelectionsSpace);
    assert_eq!(buffer_string(&mut h), "/// foo bar\n");
}

#[test]
fn join_selections_keeps_a_token_the_running_one_does_not_match() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "// foo\n/// bar\n");
    h.open_file(&path);
    h.type_keys("%");
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JoinSelectionsSpace);
    assert_eq!(buffer_string(&mut h), "// foo /// bar\n");
}

#[test]
fn join_selections_joins_without_selecting_the_space() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "ab\ncd\n");
    h.open_file(&path);
    set_range(&mut h, 0, 5);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JoinSelections);
    assert_eq!(buffer_string(&mut h), "ab cd\n");
    assert_ne!(h.selection_spans(), vec![(2, 3, false)]);
}

/// `(` then `filler` then `)`, so a scan from either bracket has the whole
/// filler to cross before reaching its partner.
fn spaced_brackets(filler: usize) -> (Rope, usize) {
    let text = format!("({})", "x".repeat(filler));
    let len = text.len();
    (Rope::from(text.as_str()), len)
}

#[test]
fn a_bracket_partner_beyond_the_cap_is_not_matched() {
    let (rope, len) = spaced_brackets(MAX_PAIR_SCAN + 100);

    assert_eq!(
        scan_bracket_match(&rope, 0, '(', '(', ')', true, &PairScan::around(None, 0)),
        None,
        "scanning forward, the close is past where the scan gives up"
    );
    assert_eq!(
        scan_bracket_match(
            &rope,
            len - 1,
            ')',
            '(',
            ')',
            false,
            &PairScan::around(None, 0)
        ),
        None,
        "and scanning back, so is the open"
    );
}

#[test]
fn a_bracket_partner_within_the_cap_is_still_matched() {
    let filler = MAX_PAIR_SCAN - 100;
    let (rope, len) = spaced_brackets(filler);

    assert_eq!(
        scan_bracket_match(&rope, 0, '(', '(', ')', true, &PairScan::around(None, 0)),
        Some(1 + filler),
        "a close inside the cap is where it always was"
    );
    assert_eq!(
        scan_bracket_match(
            &rope,
            len - 1,
            ')',
            '(',
            ')',
            false,
            &PairScan::around(None, 0)
        ),
        Some(0),
        "and so is the open"
    );
}

/// The recorded ops of the focused buffer, newest last.
fn focused_buffer_ops(h: &TestHarness) -> Vec<crate::buffer::BufferOp> {
    let ws = h.stoat.active_workspace();
    let View::Editor(editor_id) = ws.panes.pane(ws.panes.focus()).view else {
        panic!("focused pane is not an editor");
    };
    let buffer_id = ws.editors[editor_id].buffer_id;
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let guard = buffer.read().expect("poisoned");
    guard.history().ops
}

/// Several selections take one batched edit where they used to take one
/// call each. A batch has to leave behind what those calls did. The
/// recorded ops are what says so, carrying the same ranges and the same
/// replacements in the same back-to-front order.
#[test]
fn switch_case_over_three_selections_records_what_three_edits_would() {
    use crate::buffer::BufferOp;

    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "ab\ncd\nef\n");
    h.open_file(&path);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::AddSelectionBelow);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::AddSelectionBelow);
    assert_eq!(
        h.selection_spans(),
        vec![(0, 1, false), (3, 4, false), (6, 7, false)],
        "one cursor per row"
    );

    let before = focused_buffer_ops(&h).len();
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchCase);

    let ops = focused_buffer_ops(&h);
    assert_eq!(
        &ops[before..],
        &[
            BufferOp::Edit {
                old: 6..7,
                text: "E".to_owned()
            },
            BufferOp::Edit {
                old: 3..4,
                text: "C".to_owned()
            },
            BufferOp::Edit {
                old: 0..1,
                text: "A".to_owned()
            },
        ],
    );
}

use super::*;
use crate::{
    action_handlers::dispatch,
    pane::View,
    test_harness::{
        editor,
        editor::{focused_buffer_path, focused_cursor_point, focused_head_row, place_cursor},
        stoat, TestHarness,
    },
};
use stoat_action::{
    AddSelectionBelow, CollapseSelection, ExtendDown, ExtendLeft, ExtendNextWordEnd,
    ExtendNextWordStart, ExtendPrevWordEnd, ExtendPrevWordStart, ExtendRight, ExtendToFileStart,
    ExtendToLastLine, ExtendToLineEnd, ExtendToLineStart, ExtendUp, FlipSelections, MoveDown,
    MoveLeft, MoveNextWordEnd, MoveNextWordStart, MovePrevWordEnd, MovePrevWordStart, MoveRight,
    MoveUp, SelectAll,
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
    dispatch(&mut h.stoat, &stoat_action::ExtendToLineBounds);
    assert_eq!(h.selection_spans(), vec![(0, 8, false)]);
}

#[test]
fn shrink_to_line_bounds_trims_partial_lines() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\ndef\nghi\n");
    h.open_file(&path);
    // Line 0 from its middle through line 2's middle: only line 1 is whole.
    set_range(&mut h, 1, 9);
    dispatch(&mut h.stoat, &stoat_action::ShrinkToLineBounds);
    assert_eq!(h.selection_spans(), vec![(4, 8, false)]);
}

#[test]
fn shrink_to_line_bounds_within_one_line_is_noop() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abcdef\n");
    h.open_file(&path);
    set_range(&mut h, 1, 4);
    dispatch(&mut h.stoat, &stoat_action::ShrinkToLineBounds);
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
    dispatch(&mut h.stoat, &stoat_action::EnsureSelectionsForward);
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

    dispatch(&mut h.stoat, &stoat_action::Increment);

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

    dispatch(&mut h.stoat, &stoat_action::SwitchToUppercase);

    assert_eq!(buffer_string(&mut h), "FI FI\n");
    assert_eq!(h.selection_spans(), vec![(0, 2, false), (3, 5, false)]);
}

#[test]
fn rotate_selection_contents_forward_and_back() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    set_three_single_char_selections(&mut h);
    dispatch(&mut h.stoat, &stoat_action::RotateSelectionContentsForward);
    assert_eq!(buffer_string(&mut h), "cab\n");
    dispatch(&mut h.stoat, &stoat_action::RotateSelectionContentsBackward);
    assert_eq!(buffer_string(&mut h), "abc\n");
}

#[test]
fn rotate_selection_contents_backward_shifts_left() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    set_three_single_char_selections(&mut h);
    dispatch(&mut h.stoat, &stoat_action::RotateSelectionContentsBackward);
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
    dispatch(&mut h.stoat, &stoat_action::RotateSelectionContentsForward);
    assert_eq!(buffer_string(&mut h), "bca\n");
    assert_eq!(
        h.selection_spans(),
        vec![(0, 1, false), (1, 2, false), (2, 3, false)],
    );

    h.stoat.pending_count = Some(3);
    dispatch(&mut h.stoat, &stoat_action::RotateSelectionContentsForward);
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
    dispatch(&mut h.stoat, &stoat_action::JoinSelectionsSpace);
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
    dispatch(&mut h.stoat, &stoat_action::JoinSelectionsSpace);
    assert_eq!(buffer_string(&mut h), "a b c d\n");
    assert_eq!(
        h.selection_spans().len(),
        3,
        "each joined space is its own selection",
    );

    dispatch(&mut h.stoat, &stoat_action::RemovePrimarySelection);
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
    dispatch(&mut h.stoat, &stoat_action::JoinSelectionsSpace);
    assert_eq!(buffer_string(&mut h), "ab cd\n");
    assert_eq!(h.selection_spans(), vec![(2, 3, false)]);
}

#[test]
fn join_selections_drops_second_comment_token() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "// foo\n// bar\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::JoinSelectionsSpace);
    assert_eq!(buffer_string(&mut h), "// foo bar\n");
}

#[test]
fn join_selections_drops_the_second_doc_comment_token_whole() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "/// foo\n/// bar\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::JoinSelectionsSpace);
    assert_eq!(buffer_string(&mut h), "/// foo bar\n");
}

#[test]
fn join_selections_keeps_a_token_the_running_one_does_not_match() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "// foo\n/// bar\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::JoinSelectionsSpace);
    assert_eq!(buffer_string(&mut h), "// foo /// bar\n");
}

#[test]
fn join_selections_joins_without_selecting_the_space() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "ab\ncd\n");
    h.open_file(&path);
    set_range(&mut h, 0, 5);
    dispatch(&mut h.stoat, &stoat_action::JoinSelections);
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
    dispatch(&mut h.stoat, &AddSelectionBelow);
    dispatch(&mut h.stoat, &AddSelectionBelow);
    assert_eq!(
        h.selection_spans(),
        vec![(0, 1, false), (3, 4, false), (6, 7, false)],
        "one cursor per row"
    );

    let before = focused_buffer_ops(&h).len();
    dispatch(&mut h.stoat, &stoat_action::SwitchCase);

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

#[test]
fn move_left_at_start_is_noop() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "hello");
    dispatch(&mut stoat, &MoveLeft);
    assert_eq!(editor::head_offsets(&mut stoat), vec![0]);
}

#[test]
fn move_right_advances_one_grapheme() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abc");
    dispatch(&mut stoat, &MoveRight);
    assert_eq!(editor::head_offsets(&mut stoat), vec![1]);
    dispatch(&mut stoat, &MoveRight);
    assert_eq!(editor::head_offsets(&mut stoat), vec![2]);
}

#[test]
fn move_right_at_end_is_noop() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abc");
    dispatch(&mut stoat, &MoveRight);
    dispatch(&mut stoat, &MoveRight);
    dispatch(&mut stoat, &MoveRight);
    assert_eq!(editor::head_offsets(&mut stoat), vec![2]);
    dispatch(&mut stoat, &MoveRight);
    assert_eq!(editor::head_offsets(&mut stoat), vec![2]);
}

#[test]
fn move_right_across_newline() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "ab\ncd");
    dispatch(&mut stoat, &MoveRight);
    dispatch(&mut stoat, &MoveRight);
    dispatch(&mut stoat, &MoveRight);
    assert_eq!(editor::head_offsets(&mut stoat), vec![3]);
}

#[test]
fn move_right_multibyte() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "héllo");
    dispatch(&mut stoat, &MoveRight);
    assert_eq!(editor::head_offsets(&mut stoat), vec![1]);
    dispatch(&mut stoat, &MoveRight);
    assert_eq!(editor::head_offsets(&mut stoat), vec![3]);
}

#[test]
fn move_down_advances_one_row() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abc\ndef\n");
    dispatch(&mut stoat, &MoveDown);
    assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(1, 0)]);
}

#[test]
fn move_up_at_first_row_is_noop() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abc\ndef");
    dispatch(&mut stoat, &MoveUp);
    assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(0, 0)]);
}

#[test]
fn move_down_at_last_row_is_noop() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abc");
    dispatch(&mut stoat, &MoveDown);
    assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(0, 0)]);
}

#[test]
fn move_down_preserves_goal_column() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "long line\nxx\nlong line\n");
    for _ in 0..7 {
        dispatch(&mut stoat, &MoveRight);
    }
    assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(0, 7)]);
    dispatch(&mut stoat, &MoveDown);
    assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(1, 2)]);
    dispatch(&mut stoat, &MoveDown);
    assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(2, 7)]);
}

#[test]
fn move_next_word_start_creates_selection() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar");
    dispatch(&mut stoat, &MoveNextWordStart);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 4, false)]);
    assert_eq!(editor::head_offsets(&mut stoat), vec![3]);
}

#[test]
fn extend_right_crosses_the_newline() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "ab\ncd");
    stoat.set_focused_mode("select".into());
    // From 'a', three extend-rights walk over the line's last char, across
    // the newline, and onto 'c' on the next line rather than clamping.
    dispatch(&mut stoat, &ExtendRight);
    dispatch(&mut stoat, &ExtendRight);
    dispatch(&mut stoat, &ExtendRight);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 4, false)]);
}

#[test]
fn move_next_word_start_repeated_snaps_tail() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar baz");
    dispatch(&mut stoat, &MoveNextWordStart);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 4, false)]);
    dispatch(&mut stoat, &MoveNextWordStart);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(4, 8, false)]);
}

#[test]
fn move_next_word_start_from_whitespace_advances_anchor() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, " foo bar");
    dispatch(&mut stoat, &MoveNextWordStart);
    // The anchor advances past the leading space onto the word start, so the
    // selection excludes the space and `dw` here would not eat it.
    assert_eq!(editor::selection_spans(&mut stoat), vec![(1, 5, false)]);
}

#[test]
fn move_next_word_start_from_blank_line_runs_through_word() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "\nfoo");
    dispatch(&mut stoat, &MoveNextWordStart);
    // Starting on the blank line, the anchor skips the newline and the head
    // runs through the following word to its end.
    assert_eq!(editor::selection_spans(&mut stoat), vec![(1, 4, false)]);
}

#[test]
fn move_next_word_end_creates_selection() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar");
    dispatch(&mut stoat, &MoveNextWordEnd);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, false)]);
}

#[test]
fn move_next_word_end_at_eof_is_noop() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo");
    for _ in 0..3 {
        dispatch(&mut stoat, &MoveRight);
    }
    assert_eq!(editor::head_offsets(&mut stoat), vec![2]);
    dispatch(&mut stoat, &MoveNextWordEnd);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 3, false)]);
}

#[test]
fn move_prev_word_start_creates_reversed_selection() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar");
    for _ in 0..6 {
        dispatch(&mut stoat, &MoveRight);
    }
    assert_eq!(editor::head_offsets(&mut stoat), vec![6]);
    dispatch(&mut stoat, &MovePrevWordStart);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(4, 7, true)]);
    assert_eq!(editor::head_offsets(&mut stoat), vec![4]);
}

#[test]
fn move_prev_word_start_from_word_boundary_retreats_anchor() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar");
    for _ in 0..4 {
        dispatch(&mut stoat, &MoveRight);
    }
    assert_eq!(editor::head_offsets(&mut stoat), vec![4]);
    dispatch(&mut stoat, &MovePrevWordStart);
    // On the word start 'b', the tail retreats past it, so the selection
    // ends at the word start rather than one cell past it.
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 4, true)]);
}

#[test]
fn move_prev_word_start_over_forward_selection_keeps_trailing_char() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar");
    // `e` makes a forward selection whose block cursor sits on the last
    // word char, one cell back from the head. `b` must scan from there
    // rather than the head, or it swallows the char after the cursor.
    dispatch(&mut stoat, &MoveNextWordEnd);
    dispatch(&mut stoat, &MovePrevWordStart);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, true)]);
    assert_eq!(editor::head_offsets(&mut stoat), vec![0]);
}

#[test]
fn move_prev_word_end_over_forward_selection_keeps_trailing_char() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar baz");
    for _ in 0..4 {
        dispatch(&mut stoat, &MoveRight);
    }
    dispatch(&mut stoat, &MoveNextWordEnd);
    dispatch(&mut stoat, &MovePrevWordEnd);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 7, true)]);
    assert_eq!(editor::head_offsets(&mut stoat), vec![2]);
}

#[test]
fn move_prev_word_start_at_start_is_noop() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar");
    dispatch(&mut stoat, &MovePrevWordStart);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 1, false)]);
}

#[test]
fn move_prev_word_end_lands_on_last_char_of_prev_word() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar");
    for _ in 0..6 {
        dispatch(&mut stoat, &MoveRight);
    }
    assert_eq!(editor::head_offsets(&mut stoat), vec![6]);
    dispatch(&mut stoat, &MovePrevWordEnd);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 7, true)]);
    assert_eq!(editor::head_offsets(&mut stoat), vec![2]);
}

#[test]
fn move_prev_word_end_at_start_is_noop() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar");
    dispatch(&mut stoat, &MovePrevWordEnd);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 1, false)]);
}

#[test]
fn count_next_word_start_selects_only_final_span() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abc def ghi jkl");
    stoat.pending_count = Some(3);
    dispatch(&mut stoat, &MoveNextWordStart);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(8, 12, false)]);
}

#[test]
fn count_next_word_start_overshoot_selects_last_word() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(
        &mut stoat,
        "one two three\nfour five six\nseven eight nine ten",
    );
    stoat.pending_count = Some(20);
    dispatch(&mut stoat, &MoveNextWordStart);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(45, 48, false)]);
}

/// A word motion that cannot advance still leaves a selection a block cursor
/// can sit on.
///
/// The raw range returns an empty span at the buffer end, since there is
/// nothing left to scan. Every other landing path repairs that to one cell
/// wide, and the anchoring here has to as well, or the cursor renders with no
/// width at all.
#[test]
fn a_word_motion_at_the_buffer_end_keeps_a_one_cell_cursor() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo\n");
    dispatch(&mut stoat, &MoveNextWordStart);
    dispatch(&mut stoat, &MoveNextWordStart);
    assert_eq!(
        editor::selection_spans(&mut stoat),
        vec![(3, 4, false)],
        "the cursor stays on the trailing newline rather than collapsing",
    );
}

#[test]
fn count_next_word_start_crosses_newlines() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(
        &mut stoat,
        "one two three\nfour five six\nseven eight nine ten",
    );
    stoat.pending_count = Some(5);
    dispatch(&mut stoat, &MoveNextWordStart);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(19, 24, false)]);
}

#[test]
fn count_next_word_end_selects_only_final_span() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar baz");
    stoat.pending_count = Some(2);
    dispatch(&mut stoat, &MoveNextWordEnd);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(3, 7, false)]);
}

#[test]
fn next_word_start_after_word_end_scans_from_cursor_cell() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar");
    // `e` lands a forward selection whose block cursor sits one cell back
    // from the head. The following `w` scans from that cursor cell, so it
    // selects the char after the cursor and the gap rather than the whole
    // next word.
    dispatch(&mut stoat, &MoveNextWordEnd);
    dispatch(&mut stoat, &MoveNextWordStart);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 4, false)]);
}

#[test]
fn count_prev_word_start_excludes_a_cursor_newline() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abc def\nghi");
    stoat.pending_count = Some(7);
    dispatch(&mut stoat, &MoveRight);
    assert_eq!(editor::head_offsets(&mut stoat), vec![7]);
    dispatch(&mut stoat, &MovePrevWordStart);
    // `b` from the newline retreats onto the word start, excluding the
    // newline from the selection.
    assert_eq!(editor::selection_spans(&mut stoat), vec![(4, 7, true)]);
}

#[test]
fn move_right_with_multiple_cursors_advances_each() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abc\ndef\nghi\n");
    dispatch(&mut stoat, &AddSelectionBelow);
    assert_eq!(editor::head_offsets(&mut stoat), vec![0, 4]);
    dispatch(&mut stoat, &MoveRight);
    assert_eq!(editor::head_offsets(&mut stoat), vec![1, 5]);
}

#[test]
fn move_next_word_start_multi_cursor_independent() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar\nbaz qux\n");
    dispatch(&mut stoat, &AddSelectionBelow);
    assert_eq!(editor::head_offsets(&mut stoat), vec![0, 8]);
    dispatch(&mut stoat, &MoveNextWordStart);
    assert_eq!(
        editor::selection_spans(&mut stoat),
        vec![(0, 4, false), (8, 12, false)]
    );
}

#[test]
fn add_selection_below_with_no_editor_focus_is_noop() {
    let mut stoat = stoat();
    {
        let ws = stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        ws.panes.pane_mut(focused).view = View::Label("nothing".into());
    }
    assert_eq!(dispatch(&mut stoat, &AddSelectionBelow), UpdateEffect::None);
}

#[test]
fn add_selection_below_adds_cursor_on_next_display_row() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abc\ndef\nghi\n");

    assert_eq!(
        dispatch(&mut stoat, &AddSelectionBelow),
        UpdateEffect::Redraw
    );

    let positions = editor::cursor_display_positions(&mut stoat);
    assert_eq!(positions, vec![(0, 0), (1, 0)]);
}

#[test]
fn add_selection_below_at_last_row_is_noop() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abc");

    assert_eq!(dispatch(&mut stoat, &AddSelectionBelow), UpdateEffect::None);
    assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(0, 0)]);
}

#[test]
fn add_selection_below_copies_each_selection_skipping_short_lines() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "long line\nxx\nlong line\n");

    {
        let ws = stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        let editor_id = match ws.panes.pane(focused).view {
            View::Editor(id) => id,
            _ => unreachable!(),
        };
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buffer = snapshot.buffer_snapshot();
        let offset = buffer.rope().point_to_offset(Point::new(0, 7));
        let anchor = buffer.anchor_at(offset, Bias::Right);
        editor
            .selections
            .insert_cursor(anchor, SelectionGoal::Column(7), buffer);
    }

    assert_eq!(
        dispatch(&mut stoat, &AddSelectionBelow),
        UpdateEffect::Redraw
    );
    // The column-0 cursor copies onto row 1. The column-7 cursor cannot fit
    // on the short row 1, so it skips to row 2 rather than clamping.
    let positions = editor::cursor_display_positions(&mut stoat);
    assert_eq!(positions, vec![(0, 0), (0, 7), (1, 0), (2, 7)]);
}

#[test]
fn add_selection_below_under_wrap_lands_on_the_next_buffer_line() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, &format!("{}\nshort\n", "a".repeat(30)));

    {
        let ws = stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        let editor_id = match ws.panes.pane(focused).view {
            View::Editor(id) => id,
            _ => unreachable!(),
        };
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        editor.viewport_rows = Some(10);
        editor.display_map.set_wrap_width(Some(10));
    }

    assert_eq!(
        dispatch(&mut stoat, &AddSelectionBelow),
        UpdateEffect::Redraw
    );
    assert_eq!(
        editor::cursor_buffer_positions(&mut stoat),
        vec![(0, 0), (1, 0)],
        "the copy lands on the next buffer line, not the long line's wrapped tail",
    );
}

#[test]
fn extend_right_grows_selection_from_cursor() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abc");
    dispatch(&mut stoat, &ExtendRight);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 2, false)]);
}

#[test]
fn extend_right_further_keeps_tail() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abcdef");
    dispatch(&mut stoat, &ExtendRight);
    dispatch(&mut stoat, &ExtendRight);
    dispatch(&mut stoat, &ExtendRight);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 4, false)]);
}

#[test]
fn extend_right_at_end_is_noop() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "ab");
    dispatch(&mut stoat, &MoveRight);
    dispatch(&mut stoat, &MoveRight);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(1, 2, false)]);
    dispatch(&mut stoat, &ExtendRight);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(1, 2, false)]);
}

#[test]
fn extend_left_across_tail_flips_reversed() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abcdef");
    dispatch(&mut stoat, &MoveRight);
    dispatch(&mut stoat, &MoveRight);
    dispatch(&mut stoat, &ExtendRight);
    dispatch(&mut stoat, &ExtendRight);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 5, false)]);
    dispatch(&mut stoat, &ExtendLeft);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 4, false)]);
    dispatch(&mut stoat, &ExtendLeft);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 3, false)]);
    // Crossing the 1-wide tail flips the range and steps the tail forward
    // so the anchor's cell stays covered (Helix shrink-then-flip).
    dispatch(&mut stoat, &ExtendLeft);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(1, 3, true)]);
    dispatch(&mut stoat, &ExtendLeft);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, true)]);
}

#[test]
fn extend_down_preserves_goal_column() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "long line\nxx\nlong line\n");
    for _ in 0..7 {
        dispatch(&mut stoat, &MoveRight);
    }
    assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(0, 7)]);
    dispatch(&mut stoat, &ExtendDown);
    assert_eq!(
        editor::cursor_display_positions(&mut stoat),
        vec![(1, 2)],
        "the goal overruns \"xx\", so the cell is its line break, as for a move",
    );
    dispatch(&mut stoat, &ExtendDown);
    assert_eq!(
        editor::cursor_display_positions(&mut stoat),
        vec![(2, 7)],
        "and the goal survives the short line rather than the cell it sat on",
    );
}

#[test]
fn extend_down_at_last_row_is_noop() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abc");
    dispatch(&mut stoat, &ExtendDown);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 1, false)]);
}

#[test]
fn extend_up_from_second_line_grows_backward() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abc\ndef\n");
    dispatch(&mut stoat, &MoveDown);
    dispatch(&mut stoat, &MoveRight);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(5, 6, false)]);
    dispatch(&mut stoat, &ExtendUp);
    assert_eq!(
        editor::selection_spans(&mut stoat),
        vec![(1, 6, true)],
        "crossing the tail keeps the cell the cursor was on covered",
    );
}

#[test]
fn extend_next_word_start_grows_selection_from_cursor() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar");
    dispatch(&mut stoat, &ExtendNextWordStart);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 4, false)]);
}

#[test]
fn extend_next_word_start_repeated_keeps_tail() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar baz");
    dispatch(&mut stoat, &ExtendNextWordStart);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 4, false)]);
    dispatch(&mut stoat, &ExtendNextWordStart);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 8, false)]);
}

#[test]
fn extend_next_word_end_grows_selection_from_cursor() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar");
    dispatch(&mut stoat, &ExtendNextWordEnd);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, false)]);
}

#[test]
fn extend_prev_word_start_keeps_tail_at_cursor() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar");
    for _ in 0..6 {
        dispatch(&mut stoat, &MoveRight);
    }
    assert_eq!(editor::selection_spans(&mut stoat), vec![(6, 7, false)]);
    dispatch(&mut stoat, &ExtendPrevWordStart);
    assert_eq!(
        editor::selection_spans(&mut stoat),
        vec![(4, 7, true)],
        "crossing the tail keeps the r the cursor was on covered"
    );
}

#[test]
fn extend_prev_word_end_keeps_tail_at_cursor() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar");
    for _ in 0..6 {
        dispatch(&mut stoat, &MoveRight);
    }
    assert_eq!(editor::selection_spans(&mut stoat), vec![(6, 7, false)]);
    dispatch(&mut stoat, &ExtendPrevWordEnd);
    assert_eq!(
        editor::selection_spans(&mut stoat),
        vec![(2, 7, true)],
        "crossing the tail keeps the r the cursor was on covered"
    );
}

#[test]
fn extend_right_with_multiple_cursors_grows_each() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abc\ndef\nghi\n");
    dispatch(&mut stoat, &AddSelectionBelow);
    assert_eq!(editor::head_offsets(&mut stoat), vec![0, 4]);
    dispatch(&mut stoat, &ExtendRight);
    assert_eq!(
        editor::selection_spans(&mut stoat),
        vec![(0, 2, false), (4, 6, false)]
    );
}

#[test]
fn extend_to_line_end_grows_forward() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar");
    dispatch(&mut stoat, &ExtendToLineEnd);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 7, false)]);
}

#[test]
fn extend_to_line_start_from_mid_reverses() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar");
    dispatch(&mut stoat, &MoveRight);
    dispatch(&mut stoat, &MoveRight);
    dispatch(&mut stoat, &MoveRight);
    dispatch(&mut stoat, &ExtendToLineStart);
    assert_eq!(
        editor::selection_spans(&mut stoat),
        vec![(0, 4, true)],
        "crossing the tail keeps the cell the cursor was on covered"
    );
}

#[test]
fn extend_to_last_line_grows_forward() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abc\ndef\nghi\n");
    dispatch(&mut stoat, &ExtendToLastLine);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 9, false)]);
}

#[test]
fn extend_to_file_start_reverses_from_end() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abcdef");
    dispatch(&mut stoat, &MoveRight);
    dispatch(&mut stoat, &MoveRight);
    dispatch(&mut stoat, &MoveRight);
    dispatch(&mut stoat, &ExtendToFileStart);
    assert_eq!(
        editor::selection_spans(&mut stoat),
        vec![(0, 4, true)],
        "crossing the tail keeps the cell the cursor was on covered"
    );
}

#[test]
fn collapse_selection_shrinks_to_head() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abcdef");
    dispatch(&mut stoat, &ExtendRight);
    dispatch(&mut stoat, &ExtendRight);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, false)]);
    dispatch(&mut stoat, &CollapseSelection);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 3, false)]);
}

#[test]
fn collapse_selection_preserves_reversed_head() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar");
    for _ in 0..6 {
        dispatch(&mut stoat, &MoveRight);
    }
    dispatch(&mut stoat, &MovePrevWordStart);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(4, 7, true)]);
    dispatch(&mut stoat, &CollapseSelection);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(4, 5, false)]);
}

#[test]
fn collapse_selection_multi_cursor_collapses_each() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abc\ndef\nghi\n");
    dispatch(&mut stoat, &AddSelectionBelow);
    dispatch(&mut stoat, &ExtendRight);
    assert_eq!(
        editor::selection_spans(&mut stoat),
        vec![(0, 2, false), (4, 6, false)]
    );
    dispatch(&mut stoat, &CollapseSelection);
    assert_eq!(
        editor::selection_spans(&mut stoat),
        vec![(1, 2, false), (5, 6, false)]
    );
}

#[test]
fn flip_selections_toggles_reversed() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abcdef");
    dispatch(&mut stoat, &ExtendRight);
    dispatch(&mut stoat, &ExtendRight);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, false)]);
    dispatch(&mut stoat, &FlipSelections);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, true)]);
    dispatch(&mut stoat, &FlipSelections);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, false)]);
}

#[test]
fn flip_selections_on_bare_cursor_toggles_reversed() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abc");
    dispatch(&mut stoat, &MoveRight);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(1, 2, false)]);
    dispatch(&mut stoat, &FlipSelections);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(1, 2, true)]);
}

#[test]
fn select_all_replaces_all_selections() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abc\ndef\n");
    dispatch(&mut stoat, &AddSelectionBelow);
    assert_eq!(editor::head_offsets(&mut stoat), vec![0, 4]);
    dispatch(&mut stoat, &SelectAll);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 8, false)]);
}

#[test]
fn select_all_on_empty_buffer() {
    let mut stoat = stoat();
    dispatch(&mut stoat, &SelectAll);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 1, false)]);
}

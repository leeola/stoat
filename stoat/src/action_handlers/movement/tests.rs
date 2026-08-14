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

/// Collapsing ends the run of vertical motion the goal belongs to, so the
/// column the cursor came from stops following it.
#[test]
fn collapsing_drops_the_column_a_vertical_motion_was_holding() {
    let mut h = TestHarness::with_size(40, 12);
    let path = h.write_file("s.txt", "aaaaaaaa\nbb\ncccccccc\n");
    h.open_file(&path);
    {
        let editor = focused_editor_mut(&mut h.stoat).expect("focused editor");
        place_cursor(editor, 0, 7);
    }

    move_vertical(&mut h.stoat, 1, false);
    assert_eq!(
        focused_cursor_point(&mut h.stoat),
        Point::new(1, 2),
        "the short line clamps to its break",
    );

    dispatch(&mut h.stoat, &CollapseSelection);
    move_vertical(&mut h.stoat, 1, false);
    assert_eq!(
        focused_cursor_point(&mut h.stoat),
        Point::new(2, 2),
        "the next row is entered at the cursor's own column, not the old goal",
    );
}

/// Two cursors that clamp onto the same cell become one selection. The goal
/// each one held describes a span that no longer exists.
#[test]
fn merging_two_cursors_drops_the_column_they_were_holding() {
    let mut h = TestHarness::with_size(40, 12);
    let path = h.write_file("s.txt", "aaaaaaaa\nbb\ncccccccc\n");
    h.open_file(&path);
    set_selections(&mut h, &[(6, 7), (7, 8)]);

    move_vertical(&mut h.stoat, 1, false);
    assert_eq!(
        h.selection_spans(),
        vec![(11, 12, false)],
        "both clamp onto the line break and merge",
    );

    move_vertical(&mut h.stoat, 1, false);
    assert_eq!(
        focused_cursor_point(&mut h.stoat),
        Point::new(2, 2),
        "the survivor holds no column of its own",
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

/// The selections hold still while the text moves through them. A primary that
/// stays put therefore ends up over a fragment the user never chose, so it
/// travels with its own text instead.
///
/// `set_selections` mints ascending and the collection reads the primary off
/// the highest id, so the range listed last is the primary.
#[test]
fn rotating_contents_forward_carries_the_primary_with_its_text() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    set_selections(&mut h, &[(2, 3), (1, 2), (0, 1)]);

    dispatch(&mut h.stoat, &stoat_action::RotateSelectionContentsForward);
    assert_eq!(buffer_string(&mut h), "cab\n");

    dispatch(&mut h.stoat, &stoat_action::KeepPrimarySelection);
    assert_eq!(
        h.selection_spans(),
        vec![(1, 2, false)],
        "the primary held a, which the rotation moved one place on",
    );
}

/// Rotating the other way carries the primary the other way.
#[test]
fn rotating_contents_backward_carries_the_primary_with_its_text() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    set_three_single_char_selections(&mut h);

    dispatch(&mut h.stoat, &stoat_action::RotateSelectionContentsBackward);
    assert_eq!(buffer_string(&mut h), "bca\n");

    dispatch(&mut h.stoat, &stoat_action::KeepPrimarySelection);
    assert_eq!(
        h.selection_spans(),
        vec![(1, 2, false)],
        "the primary held c, which the rotation moved one place back",
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

/// `l` reaches the position past the last character, then stops.
///
/// That position is the buffer end, where the cursor is zero-width. Stopping
/// one cell earlier is what used to make the end unreachable, leaving no way
/// to append after the last character.
#[test]
fn move_right_reaches_the_end_then_stops() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abc");
    for _ in 0..3 {
        dispatch(&mut stoat, &MoveRight);
    }
    assert_eq!(editor::head_offsets(&mut stoat), vec![3]);
    dispatch(&mut stoat, &MoveRight);
    assert_eq!(editor::head_offsets(&mut stoat), vec![3], "and stays there");
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

/// A trailing newline opens a line below the text, and `j` reaches it.
///
/// The cursor is zero-width there, having nothing to cover, which is what makes
/// the row a position rather than the padding it used to be.
#[test]
fn move_down_onto_the_line_a_trailing_newline_opens() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "ab\n");
    dispatch(&mut stoat, &MoveDown);
    assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(1, 0)]);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(3, 3, false)]);
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

/// A word-end motion with no word left ahead lands on the buffer end, where
/// the cursor is zero-width.
#[test]
fn move_next_word_end_at_eof_lands_on_the_end() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo");
    for _ in 0..3 {
        dispatch(&mut stoat, &MoveRight);
    }
    assert_eq!(editor::head_offsets(&mut stoat), vec![3]);
    dispatch(&mut stoat, &MoveNextWordEnd);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(3, 3, false)]);
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
    assert_eq!(editor::selection_spans(&mut stoat), vec![(3, 7, true)]);
    assert_eq!(editor::head_offsets(&mut stoat), vec![3]);
}

#[test]
fn move_prev_word_start_at_start_is_noop() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar");
    dispatch(&mut stoat, &MovePrevWordStart);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 1, false)]);
}

/// `gE` stops on the boundary past the previous word's last character, not on
/// the character itself.
///
/// From the `r` of "foo bar" the scan crosses back over "bar" and the space,
/// stopping where "foo" ended, so the cursor sits on the space at 3. Stepping
/// one further back onto the `o` is what used to make the short-word `gE`
/// disagree with the long-word one.
#[test]
fn move_prev_word_end_lands_past_the_prev_word() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo bar");
    for _ in 0..6 {
        dispatch(&mut stoat, &MoveRight);
    }
    assert_eq!(editor::head_offsets(&mut stoat), vec![6]);
    dispatch(&mut stoat, &MovePrevWordEnd);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(3, 7, true)]);
    assert_eq!(editor::head_offsets(&mut stoat), vec![3]);
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

/// A word motion that runs out of buffer lands on the end and stays there,
/// zero-width.
///
/// The buffer end is a cursor position of its own, so the motion arrives at it
/// rather than falling back onto the trailing newline. Re-covering that newline
/// leaves the position past it unreachable by any motion.
#[test]
fn a_word_motion_at_the_buffer_end_lands_zero_width() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "foo\n");
    dispatch(&mut stoat, &MoveNextWordStart);
    dispatch(&mut stoat, &MoveNextWordStart);
    assert_eq!(
        editor::selection_spans(&mut stoat),
        vec![(4, 4, false)],
        "the cursor sits past the trailing newline, on the buffer end",
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

/// Seed the focused editor with one selection over `start..end`, facing
/// backward so its head is at `start`.
fn set_reversed_selection(stoat: &mut Stoat, start: usize, end: usize) {
    let editor = focused_editor_mut(stoat).expect("editor");
    let snapshot = editor.display_map.snapshot();
    let buf = snapshot.buffer_snapshot();
    editor.selections.transform(buf, |sel| Selection {
        id: sel.id,
        start: buf.anchor_at(start, Bias::Right),
        end: buf.anchor_at(end, Bias::Right),
        reversed: true,
        goal: SelectionGoal::None,
    });
}

/// A copy keeps the source's width whichever way the source faces.
///
/// A reversed selection's tail is its exclusive end, so reading it as the copy's
/// anchor without stepping back spans one cell more than the source covers.
#[test]
fn add_selection_below_copies_a_reversed_selection_at_its_own_width() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abcdef\nghijkl\n");
    set_reversed_selection(&mut stoat, 1, 4);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(1, 4, true)]);

    assert_eq!(
        dispatch(&mut stoat, &AddSelectionBelow),
        UpdateEffect::Redraw
    );
    assert_eq!(
        editor::selection_spans(&mut stoat),
        vec![(1, 4, true), (8, 11, true)],
        "the copy covers three cells facing backward, like its source",
    );
}

/// A reversed selection ending at a line start copies one row down, not two.
///
/// Its tail sits on the next row, so leaving it unstepped counts that row into
/// the height and throws the copy a full selection further than the shape it
/// came from.
#[test]
fn add_selection_below_copies_a_reversed_selection_ending_at_a_line_start() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "abc\ndef\nghi\njkl\n");
    set_reversed_selection(&mut stoat, 1, 4);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(1, 4, true)]);

    assert_eq!(
        dispatch(&mut stoat, &AddSelectionBelow),
        UpdateEffect::Redraw
    );
    assert_eq!(
        editor::selection_spans(&mut stoat),
        vec![(1, 4, true), (5, 8, true)],
        "the copy lands on the next row, keeping the source's height",
    );
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

/// Extending onto the line a trailing newline opens does nothing, though a
/// plain motion reaches it.
///
/// That line holds no characters. Extending onto it only drags the selection
/// over the newline ending the line above, which stops reading as a selection
/// of anything.
#[test]
fn extend_down_onto_the_line_a_trailing_newline_opens_is_a_noop() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "ab\n");
    dispatch(&mut stoat, &ExtendDown);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 1, false)]);
}

/// A blank line in the middle of a buffer is an ordinary target. Only the line
/// past the last newline is off limits to an extend.
#[test]
fn extend_down_onto_a_blank_middle_line_still_extends() {
    let mut stoat = stoat();
    editor::seed_focused_buffer(&mut stoat, "ab\n\ncd\n");
    dispatch(&mut stoat, &ExtendDown);
    assert_eq!(
        editor::selection_spans(&mut stoat),
        vec![(0, 4, false)],
        "the span grows through the blank line's own newline",
    );
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
        vec![(3, 7, true)],
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

/// A fresh scratch holds an empty rope, so selecting all of it selects
/// nothing and the cursor stays zero-width where it is.
#[test]
fn select_all_on_empty_buffer() {
    let mut stoat = stoat();
    dispatch(&mut stoat, &SelectAll);
    assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 0, false)]);
}

#[test]
fn snapshot_add_selection_below() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("sample.txt", "abcd\nefgh\nijkl\n");

    h.open_file(&path);
    h.type_keys("C");
    h.assert_snapshot("add_selection_below");
}

#[test]
fn snapshot_split_selection_on_newline() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("sample.txt", "abc\ndef\nghi\n");

    h.open_file(&path);
    h.type_keys("% alt-s");
    h.assert_snapshot("split_selection_on_newline");
}

#[test]
fn snapshot_shift_c_adds_selection_below_styled() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("sample.txt", "abcd\nefgh\nijkl\n");

    h.open_file(&path);
    h.type_keys("shift-C");
    h.assert_snapshot("shift_c_adds_selection_below");
}

#[test]
fn add_selection_below_copies_selection_shape() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "foobar\nfoobar\n");
    h.open_file(&path);
    // Select `foo`, return to normal mode, then copy it downward.
    h.type_keys("v l l v");
    h.type_keys("shift-C");
    assert_eq!(h.selection_spans(), vec![(0, 3, false), (7, 10, false)]);
}

#[test]
fn add_selection_below_skips_too_short_line() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "foobar\nx\nfoobar\n");
    h.open_file(&path);
    h.type_keys("v l l v");
    h.type_keys("shift-C");
    assert_eq!(h.selection_spans(), vec![(0, 3, false), (9, 12, false)]);
}

/// A copied cursor lands in the column a vertical motion would reach.
///
/// Both answer "the same place, one line down", so they have to agree about
/// what a column is. A tab is one byte and several cells, so a copy working
/// in bytes lands near the start of the line below where the motion lands
/// past the indent, and the two only diverge on lines that hold one.
#[test]
fn add_selection_below_lands_where_moving_down_lands() {
    let text = "\tfoo\nabcdefgh\n";

    let moved = {
        let mut h = TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", text);
        h.open_file(&path);
        h.type_keys("l j");
        h.selection_spans()
    };

    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", text);
    h.open_file(&path);
    h.type_keys("l");
    h.type_keys("shift-C");
    let copied = h.selection_spans();

    assert_eq!(copied.len(), 2, "the source and its copy, got {copied:?}",);
    assert_eq!(
        copied[1], moved[0],
        "the copy sits where moving down from the same cursor sits",
    );
}

#[test]
fn count_prefix_add_selection_below_inserts_n_cursors() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "a\nb\nc\nd\ne\n");
    h.open_file(&path);
    h.type_keys("3 shift-C");
    let spans = h.selection_spans();
    assert_eq!(
        spans.len(),
        4,
        "3C from 1 cursor should leave 4 cursors total (got {spans:?})"
    );
}

/// A copied cursor's start holds its ground when text arrives on it.
///
/// Which side of an insertion an endpoint lands on is the anchor's bias, and
/// nothing about the offsets says which was used. Minting the copies' starts
/// the other way would let text typed at a copy's own offset push it along,
/// so the cursor would slide off the column it was copied to.
#[test]
fn a_copied_cursors_start_stays_before_text_inserted_at_it() {
    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "abcdef\nabcdef\n");
    h.open_file(&path);
    h.type_keys("l l l");
    dispatch(&mut h.stoat, &AddSelectionBelow);
    assert_eq!(
        h.selection_spans(),
        vec![(3, 4, false), (10, 11, false)],
        "one cursor per row at column three",
    );

    // Onto the copy's own start, which is the offset whose bias decides
    // whether the cursor is pushed along or stays where it was put.
    {
        let ws = h.stoat.active_workspace();
        let editor_id = match ws.panes.pane(ws.panes.focus()).view {
            crate::pane::View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let buffer_id = ws.editors[editor_id].buffer_id;
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        buffer.write().expect("poisoned").edit(10..10, "XY");
    }

    assert_eq!(
        h.selection_spans(),
        vec![(3, 4, false), (10, 13, false)],
        "the copy's start stayed put and the insertion landed inside it",
    );
}

#[test]
fn count_prefix_add_selection_above_inserts_n_cursors() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "a\nb\nc\nd\ne\n");
    h.open_file(&path);
    h.type_keys("4 j");
    h.type_keys("3 alt-shift-C");
    let spans = h.selection_spans();
    assert_eq!(
        spans.len(),
        4,
        "3 Alt-C from 1 cursor should leave 4 cursors total (got {spans:?})"
    );
}

#[test]
fn count_prefix_add_selection_below_clamps_at_buffer_end() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "a\nb\nc\n");
    h.open_file(&path);
    h.type_keys("9 9 shift-C");
    let spans = h.selection_spans();
    assert!(
        spans.len() <= 4,
        "huge count should clamp at buffer end (3 lines means at most 3 cursors below the start, got {spans:?})"
    );
    assert!(
        spans.len() > 1,
        "should have added at least one cursor below (got {spans:?})"
    );
}

#[test]
fn snapshot_move_right() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "hello world\n");
    h.open_file(&path);
    h.type_keys("l l l");
    h.assert_snapshot("snapshot_move_right");
}

#[test]
fn snapshot_move_down() {
    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "abc\ndef\nghi\n");
    h.open_file(&path);
    h.type_keys("j j");
    h.assert_snapshot("snapshot_move_down");
}

#[test]
fn snapshot_select_mode_forward_char_cursor() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abcdef\n");
    h.open_file(&path);
    h.type_keys("v l l");
    h.assert_snapshot("select_mode_forward_char_cursor");
}

#[test]
fn snapshot_select_mode_find_cursor() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abcdef\n");
    h.open_file(&path);
    h.type_keys("v f e");
    h.assert_snapshot("select_mode_find_cursor");
}

#[test]
fn snapshot_select_mode_vertical_cursor() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abcdef\nghijkl\n");
    h.open_file(&path);
    h.type_keys("l l v j");
    h.assert_snapshot("select_mode_vertical_cursor");
}

#[test]
fn snapshot_select_mode_goto_first_nonws_cursor() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "  abc\n");
    h.open_file(&path);
    h.type_keys("v g i");
    h.assert_snapshot("select_mode_goto_first_nonws_cursor");
}

#[test]
fn snapshot_select_mode_goto_window_cursor() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abc\ndef\nghi");
    h.open_file(&path);
    h.type_keys("v g b");
    h.assert_snapshot("select_mode_goto_window_cursor");
}

#[test]
fn snapshot_word_forward() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "foo bar baz\n");
    h.open_file(&path);
    h.type_keys("w");
    h.assert_snapshot("snapshot_word_forward");
}

#[test]
fn snapshot_word_end() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "foo bar baz\n");
    h.open_file(&path);
    h.type_keys("e");
    h.assert_snapshot("snapshot_word_end");
}

#[test]
fn snapshot_word_backward() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "foo bar baz\n");
    h.open_file(&path);
    h.type_keys("l l l l l l l");
    h.type_keys("b");
    h.assert_snapshot("snapshot_word_backward");
}

#[test]
fn snapshot_word_forward_repeated() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "foo bar baz\n");
    h.open_file(&path);
    h.type_keys("w w");
    h.assert_snapshot("snapshot_word_forward_repeated");
}

#[test]
fn snapshot_multi_cursor_move_right() {
    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "abc\ndef\nghi\n");
    h.open_file(&path);
    h.type_keys("C l l");
    h.assert_snapshot("snapshot_multi_cursor_move_right");
}

#[test]
fn snapshot_goto_line_start() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "foo bar baz\n");
    h.open_file(&path);
    h.type_keys("w w");
    h.type_keys("home");
    h.assert_snapshot("snapshot_goto_line_start");
}

#[test]
fn snapshot_goto_line_end() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "foo bar baz\n");
    h.open_file(&path);
    h.type_keys("end");
    h.assert_snapshot("snapshot_goto_line_end");
}

#[test]
fn snapshot_goto_line_end_empty_line() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\n\nxyz\n");
    h.open_file(&path);
    h.type_keys("j");
    h.type_keys("end");
    h.assert_snapshot("snapshot_goto_line_end_empty_line");
}

#[test]
fn goto_line_end_lands_on_last_visible_char() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    h.type_keys("end");
    // The block cursor rests on the last visible char, not the newline.
    assert_eq!(h.selection_spans()[0], (2, 3, false));
}

#[test]
fn goto_line_end_on_empty_line_stays_at_column_zero() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "x\n\ny\n");
    h.open_file(&path);
    h.type_keys("j");
    h.type_keys("end");
    // An empty line has no visible char, so it stays at the line start.
    assert_eq!(h.head_offsets(), vec![2]);
}

#[test]
fn snapshot_goto_file_start() {
    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "abc\ndef\nghi\n");
    h.open_file(&path);
    h.type_keys("j j l l");
    h.type_keys("g k");
    h.assert_snapshot("snapshot_goto_file_start");
}

#[test]
fn snapshot_goto_last_line() {
    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "abc\ndef\nghi\n");
    h.open_file(&path);
    h.type_keys("g j");
    h.assert_snapshot("snapshot_goto_last_line");
}

#[test]
fn snapshot_goto_first_nonwhitespace() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "    foo bar\n");
    h.open_file(&path);
    h.type_keys("g i");
    h.assert_snapshot("snapshot_goto_first_nonwhitespace");
}

#[test]
fn snapshot_goto_first_nonwhitespace_empty_line() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\n\nxyz\n");
    h.open_file(&path);
    h.type_keys("j");
    h.type_keys("g i");
    h.assert_snapshot("snapshot_goto_first_nonwhitespace_empty_line");
}

#[test]
fn goto_h_jumps_to_line_start() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "    abc\n");
    h.open_file(&path);
    h.type_keys("l l l l l l");
    h.type_keys("g h");
    assert_eq!(h.primary_head_offset(), 0);
}

#[test]
fn goto_l_jumps_to_line_end() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc def\n");
    h.open_file(&path);
    h.type_keys("g l");
    // `gl` lands on the last visible char 'f', not the newline past it.
    assert_eq!(h.primary_head_offset(), 6);
}

#[test]
fn snapshot_extend_to_line_start() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "foo bar baz\n");
    h.open_file(&path);
    h.type_keys("w w");
    dispatch(&mut h.stoat, &ExtendToLineStart);
    h.assert_snapshot("snapshot_extend_to_line_start");
}

#[test]
fn snapshot_extend_to_line_end() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "foo bar baz\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &ExtendToLineEnd);
    h.assert_snapshot("snapshot_extend_to_line_end");
}

#[test]
fn snapshot_extend_to_file_start() {
    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "abc\ndef\nghi\n");
    h.open_file(&path);
    h.type_keys("j j l l");
    dispatch(&mut h.stoat, &ExtendToFileStart);
    h.assert_snapshot("snapshot_extend_to_file_start");
}

#[test]
fn snapshot_extend_to_last_line() {
    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "abc\ndef\nghi\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &ExtendToLastLine);
    h.assert_snapshot("snapshot_extend_to_last_line");
}

#[test]
fn snapshot_collapse_selection() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "foo bar baz\n");
    h.open_file(&path);
    h.type_keys("w w");
    h.type_keys(";");
    h.assert_snapshot("snapshot_collapse_selection");
}

#[test]
fn snapshot_flip_selections() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "foo bar baz\n");
    h.open_file(&path);
    h.type_keys("w");
    h.type_keys("alt-;");
    h.assert_snapshot("snapshot_flip_selections");
}

#[test]
fn snapshot_select_all() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\ndef\n");
    h.open_file(&path);
    h.type_keys("%");
    h.assert_snapshot("snapshot_select_all");
}

#[test]
fn snapshot_select_line_below_snaps_to_line() {
    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "abc\ndef\nghi\n");
    h.open_file(&path);
    h.type_keys("x");
    h.assert_snapshot("snapshot_select_line_below_snaps_to_line");
}

#[test]
fn snapshot_select_line_below_extends_on_repeat() {
    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "abc\ndef\nghi\n");
    h.open_file(&path);
    h.type_keys("x x");
    h.assert_snapshot("snapshot_select_line_below_extends_on_repeat");
}

#[test]
fn snapshot_select_line_cursor_on_line_end() {
    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    h.type_keys("x");
    h.assert_snapshot("snapshot_select_line_cursor_on_line_end");
}

#[test]
fn snapshot_select_line_last_line_no_trailing_newline() {
    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "abc");
    h.open_file(&path);
    h.type_keys("x");
    h.assert_snapshot("snapshot_select_line_last_line_no_trailing_newline");
}

#[test]
fn snapshot_select_line_on_blank_line() {
    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "abc\n\nxyz\n");
    h.open_file(&path);
    h.type_keys("j x");
    h.assert_snapshot("snapshot_select_line_on_blank_line");
}

#[test]
fn snapshot_keep_primary_selection() {
    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "abc\ndef\nghi\n");
    h.open_file(&path);
    h.type_keys("C");
    dispatch(&mut h.stoat, &stoat_action::KeepPrimarySelection);
    h.assert_snapshot("snapshot_keep_primary_selection");
}

#[test]
fn rotate_selections_forward_cycles_primary() {
    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "abc\ndef\nghi\n");
    h.open_file(&path);
    h.type_keys("C C");
    assert_eq!(h.head_offsets(), vec![0, 4, 8]);
    assert_eq!(h.primary_head_offset(), 8);

    dispatch(&mut h.stoat, &stoat_action::RotateSelectionsForward);
    assert_eq!(h.primary_head_offset(), 0);

    dispatch(&mut h.stoat, &stoat_action::RotateSelectionsForward);
    assert_eq!(h.primary_head_offset(), 4);

    dispatch(&mut h.stoat, &stoat_action::RotateSelectionsForward);
    assert_eq!(h.primary_head_offset(), 8);
}

#[test]
fn rotate_selections_backward_cycles_primary() {
    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "abc\ndef\nghi\n");
    h.open_file(&path);
    h.type_keys("C C");
    assert_eq!(h.primary_head_offset(), 8);

    dispatch(&mut h.stoat, &stoat_action::RotateSelectionsBackward);
    assert_eq!(h.primary_head_offset(), 4);

    dispatch(&mut h.stoat, &stoat_action::RotateSelectionsBackward);
    assert_eq!(h.primary_head_offset(), 0);

    dispatch(&mut h.stoat, &stoat_action::RotateSelectionsBackward);
    assert_eq!(h.primary_head_offset(), 8);
}

#[test]
fn rotate_single_selection_is_noop() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\ndef\n");
    h.open_file(&path);
    let before = h.primary_head_offset();
    dispatch(&mut h.stoat, &stoat_action::RotateSelectionsForward);
    assert_eq!(h.primary_head_offset(), before);
    dispatch(&mut h.stoat, &stoat_action::RotateSelectionsBackward);
    assert_eq!(h.primary_head_offset(), before);
}

#[test]
fn count_prefix_rotate_forward_cycles_n_positions() {
    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "abc\ndef\nghi\njkl\n");
    h.open_file(&path);
    h.type_keys("C C C");
    assert_eq!(h.head_offsets(), vec![0, 4, 8, 12]);
    assert_eq!(h.primary_head_offset(), 12);
    h.type_keys("2 )");
    assert_eq!(
        h.primary_head_offset(),
        4,
        "2 ) from primary at offset 12 should land on offset 4 (wraps from 12 -> 0 -> 4)"
    );
}

#[test]
fn count_prefix_rotate_backward_cycles_n_positions() {
    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "abc\ndef\nghi\njkl\n");
    h.open_file(&path);
    h.type_keys("C C C");
    assert_eq!(h.primary_head_offset(), 12);
    h.type_keys("2 (");
    assert_eq!(
        h.primary_head_offset(),
        4,
        "2 ( from primary at offset 12 should land on offset 4"
    );
}

#[test]
fn count_prefix_rotate_full_cycle_is_noop() {
    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "abc\ndef\nghi\njkl\n");
    h.open_file(&path);
    h.type_keys("C C C");
    let before = h.primary_head_offset();
    h.type_keys("4 )");
    assert_eq!(
        h.primary_head_offset(),
        before,
        "rotating by len cycles should leave the primary at the same offset"
    );
}

#[test]
fn snapshot_trim_selections_strips_whitespace() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "  hello  \n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::TrimSelections);
    h.assert_snapshot("snapshot_trim_selections_strips_whitespace");
}

#[test]
fn snapshot_trim_selections_all_whitespace_collapses_to_primary() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "   \n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::TrimSelections);
    h.assert_snapshot("snapshot_trim_selections_all_whitespace_collapses_to_primary");
}

fn focused_buffer_text(h: &mut TestHarness) -> String {
    let ws = h.stoat.active_workspace();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        crate::pane::View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    };
    let buffer_id = ws.editors[editor_id].buffer_id;
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let guard = buffer.read().expect("poisoned");
    guard.rope().to_string()
}

#[test]
fn switch_case_uppercases_lowercase_selection() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "hello\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::SwitchCase);
    assert_eq!(focused_buffer_text(&mut h), "HELLO\n");
}

#[test]
fn switch_case_lowercases_uppercase_selection() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "HELLO\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::SwitchCase);
    assert_eq!(focused_buffer_text(&mut h), "hello\n");
}

#[test]
fn switch_case_toggles_mixed_case() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "Hello World\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::SwitchCase);
    assert_eq!(focused_buffer_text(&mut h), "hELLO wORLD\n");
}

#[test]
fn switch_case_passes_through_non_letters() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc 123!\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::SwitchCase);
    assert_eq!(focused_buffer_text(&mut h), "ABC 123!\n");
}

#[test]
fn increment_seeks_forward_to_next_digit_on_line() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "let x = 42\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::Increment);
    assert_eq!(focused_buffer_text(&mut h), "let x = 43\n");
}

#[test]
fn increment_no_op_when_line_has_no_digit() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abc\n42\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::Increment);
    assert_eq!(
        focused_buffer_text(&mut h),
        "abc\n42\n",
        "seek should not cross newline"
    );
}

#[test]
fn increment_hex_preserves_lowercase_and_width() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "let x = 0x0f\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::Increment);
    assert_eq!(focused_buffer_text(&mut h), "let x = 0x10\n");
}

#[test]
fn increment_hex_grows_width_on_overflow() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "let x = 0xff\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::Increment);
    assert_eq!(focused_buffer_text(&mut h), "let x = 0x100\n");
}

#[test]
fn increment_hex_uses_uppercase_when_input_was_uppercase() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "let x = 0xFE\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::Increment);
    assert_eq!(focused_buffer_text(&mut h), "let x = 0xFF\n");
}

#[test]
fn decrement_binary_preserves_width() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "let x = 0b1010\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::Decrement);
    assert_eq!(focused_buffer_text(&mut h), "let x = 0b1001\n");
}

#[test]
fn increment_octal_preserves_width() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "let x = 0o17\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::Increment);
    assert_eq!(focused_buffer_text(&mut h), "let x = 0o20\n");
}

#[test]
fn decrement_hex_saturates_at_zero() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "let x = 0x00\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::Decrement);
    assert_eq!(focused_buffer_text(&mut h), "let x = 0x00\n");
}

#[test]
fn increment_hex_underscored_no_width_change() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "let x = 0xab_cd\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::Increment);
    assert_eq!(focused_buffer_text(&mut h), "let x = 0xab_ce\n");
}

#[test]
fn increment_hex_underscored_overflow_regroups_right() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "let x = 0xff_ff\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::Increment);
    assert_eq!(focused_buffer_text(&mut h), "let x = 0x1_00_00\n");
}

#[test]
fn decrement_binary_underscored_preserves_width() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "let x = 0b1010_1010\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::Decrement);
    assert_eq!(focused_buffer_text(&mut h), "let x = 0b1010_1001\n");
}

#[test]
fn decrement_hex_underscored_borrow_pads_left() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "let x = 0x10_00_00_00\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::Decrement);
    assert_eq!(focused_buffer_text(&mut h), "let x = 0x0f_ff_ff_ff\n");
}

#[test]
fn count_prefix_increment_adds_count() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "let x = 10\n");
    h.open_file(&path);
    h.type_keys("5 plus");
    assert_eq!(focused_buffer_text(&mut h), "let x = 15\n");
}

#[test]
fn count_prefix_decrement_subtracts_count() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "let x = 10\n");
    h.open_file(&path);
    h.type_keys("3 minus");
    assert_eq!(focused_buffer_text(&mut h), "let x = 7\n");
}

#[test]
fn count_prefix_increment_hex_uses_count() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "let x = 0x10\n");
    h.open_file(&path);
    h.type_keys("4 plus");
    assert_eq!(focused_buffer_text(&mut h), "let x = 0x14\n");
}

#[test]
fn select_mode_v_enters_then_h_extends_selection() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abcdef\n");
    h.open_file(&path);
    h.type_keys("3 l");
    let before = h.selection_spans();
    h.type_keys("v");
    assert_eq!(h.stoat.focused_mode(), "select");
    h.type_keys("h h");
    let after = h.selection_spans();
    assert_ne!(after, before, "selection should have extended");
    // Extending left past the 1-wide anchor flips the range and steps the
    // tail one cell forward (Helix shrink-then-flip), so `d`'s cell stays
    // covered. `bcd` is selected (1..4) with the cursor on `b`.
    assert_eq!(after[0].0, 1, "tail of extended selection at byte 1");
    assert_eq!(
        after[0].1, 4,
        "end covers `d`, cursor (reversed head) on `b`"
    );
}

#[test]
fn count_prefix_in_select_mode_extends_n_lines() {
    let mut h = TestHarness::with_size(30, 10);
    let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\n");
    h.open_file(&path);
    h.type_keys("v");
    assert_eq!(h.stoat.focused_mode(), "select");
    h.type_keys("3 j");
    let spans = h.selection_spans();
    assert_eq!(spans.len(), 1);
    assert_eq!(
        spans[0].0, 0,
        "anchor stays at byte 0 while head extends down"
    );
    assert_eq!(
        spans[0].1, 7,
        "3 j in select mode should extend the head three lines down"
    );
}

#[test]
fn select_mode_v_exits_back_to_normal() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    h.type_keys("v");
    assert_eq!(h.stoat.focused_mode(), "select");
    h.type_keys("v");
    assert_eq!(h.stoat.focused_mode(), "normal");
}

#[test]
fn select_mode_escape_exits_back_to_normal() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    h.type_keys("v");
    assert_eq!(h.stoat.focused_mode(), "select");
    h.type_keys("Escape");
    assert_eq!(h.stoat.focused_mode(), "normal");
}

#[test]
fn select_mode_semicolon_collapses_selection() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abcdef\n");
    h.open_file(&path);
    h.type_keys("v l l l");
    let before = h.selection_spans()[0];
    assert!(before.1 > before.0, "selection should be non-empty");
    h.type_keys(";");
    let after = h.selection_spans()[0];
    assert_eq!(
        (after.0, after.1),
        (3, 4),
        "; collapses to a 1-wide cursor on the last selected cell"
    );
}

#[test]
fn select_mode_alt_semicolon_flips_selection() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abcdef\n");
    h.open_file(&path);
    h.type_keys("v l l l");
    let before = h.selection_spans()[0];
    h.type_keys("Alt-;");
    let after = h.selection_spans()[0];
    assert_eq!(after.0, before.0, "tail/head ranges remain the same");
    assert_eq!(after.1, before.1);
    assert_ne!(after.2, before.2, "reversed flag flipped");
}

#[test]
fn select_mode_indent_indents_selection_lines() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abc\ndef\n");
    h.open_file(&path);
    h.type_keys("v j l");
    h.type_keys(">");
    assert_eq!(focused_buffer_text(&mut h), "\tabc\n\tdef\n");
}

#[test]
fn select_mode_delete_removes_selection() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abcdef\n");
    h.open_file(&path);
    h.type_keys("v l l l");
    h.type_keys("d");
    assert_eq!(focused_buffer_text(&mut h), "ef\n");
}

#[test]
fn repeated_delete_walks_forward_through_line() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abcdef\n");
    h.open_file(&path);
    h.type_keys("d d");
    assert_eq!(focused_buffer_text(&mut h), "cdef\n");
}

/// Deleting the last character leaves the cursor on the buffer end, where it
/// covers nothing, so a second delete has nothing to take.
///
/// Reaching back onto the new last character instead lets a held `d` eat the
/// line backwards from its end, which is the reported bug this model fixes.
#[test]
fn delete_at_the_buffer_end_stops_after_the_last_character() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abc");
    h.open_file(&path);
    h.type_keys("l l d d");
    assert_eq!(focused_buffer_text(&mut h), "ab");
}

#[test]
fn multi_cursor_delete_rewidens_every_cursor() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abc\ndef\n");
    h.open_file(&path);
    h.type_keys("shift-C");
    h.type_keys("d");
    assert_eq!(focused_buffer_text(&mut h), "bc\nef\n");
    assert_eq!(h.selection_spans(), vec![(0, 1, false), (3, 4, false)]);
}

#[test]
fn select_mode_tilde_switches_case() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abcdef\n");
    h.open_file(&path);
    h.type_keys("v l l l");
    h.type_keys("~");
    assert_eq!(focused_buffer_text(&mut h), "ABCDef\n");
}

#[test]
fn select_mode_undo_reverts_prior_edit() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abcdef\n");
    h.open_file(&path);
    h.type_keys("v l l l");
    h.type_keys("d");
    assert_eq!(focused_buffer_text(&mut h), "ef\n");
    h.type_keys("u");
    assert_eq!(focused_buffer_text(&mut h), "abcdef\n");
}

#[test]
fn select_mode_alt_o_expands_selection_to_enclosing_node() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn main() {}\n");
    h.open_file(&path);
    h.type_keys("l l l v");
    let before = h.selection_spans()[0];
    h.type_keys("Alt-o");
    let after = h.selection_spans()[0];
    assert_eq!(
        h.stoat.focused_mode(),
        "select",
        "Alt-o stays in select mode"
    );
    assert!(
        after.0 <= before.0 && after.1 > before.1,
        "expansion should cover and exceed the prior selection ({before:?} -> {after:?})"
    );
}

#[test]
fn select_mode_alt_i_shrinks_back_after_expand() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn main() {}\n");
    h.open_file(&path);
    h.type_keys("l l l v");
    let before = h.selection_spans();
    h.type_keys("Alt-o");
    assert_ne!(h.selection_spans(), before, "Alt-o should grow selection");
    h.type_keys("Alt-i");
    assert_eq!(
        h.stoat.focused_mode(),
        "select",
        "Alt-i stays in select mode"
    );
    assert_eq!(
        h.selection_spans(),
        before,
        "Alt-i should restore pre-expand selection"
    );
}

#[test]
fn select_mode_f_extends_forward_to_target_char() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abcdef\n");
    h.open_file(&path);
    h.type_keys("v");
    h.type_keys("f e");
    let (start, end, reversed) = h.selection_spans()[0];
    assert_eq!((start, end, reversed), (0, 5, false));
}

#[test]
fn select_mode_capital_f_extends_backward() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abcdef\n");
    h.open_file(&path);
    h.type_keys("4 l v");
    h.type_keys("F b");
    let (start, end, reversed) = h.selection_spans()[0];
    assert_eq!(
        (start, end, reversed),
        (1, 5, true),
        "crossing the tail keeps the e the cursor was on covered",
    );
}

/// An extend that lands exactly on its own tail still covers a cell.
///
/// Holding the tail and moving the head onto it would make the two
/// endpoints equal, and nothing downstream widens an empty selection, so
/// the block cursor would have no cell to paint at all.
#[test]
fn select_mode_extend_back_onto_the_tail_keeps_a_cell() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "ab cd ef\n");
    h.open_file(&path);
    h.type_keys("3 l v l l");
    assert_eq!(
        h.selection_spans()[0],
        (3, 6, false),
        "the tail sits at the c the backward word motion targets",
    );

    h.type_keys("b");
    assert_eq!(
        h.selection_spans()[0],
        (3, 4, false),
        "landing on the tail covers its cell rather than collapsing onto it",
    );
}

/// The same at a line start, where the target is the tail itself.
#[test]
fn select_mode_extend_to_line_start_at_column_zero_keeps_a_cell() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "foo bar\n");
    h.open_file(&path);
    h.type_keys("v");
    dispatch(&mut h.stoat, &ExtendToLineStart);
    assert_eq!(
        h.selection_spans()[0],
        (0, 1, false),
        "already at column 0, so the cursor keeps its cell",
    );
}

/// A find is a horizontal move, so it clears the column a prior vertical
/// move was holding.
///
/// Carrying that column past the find makes the next vertical move return
/// to where the cursor was before it, ignoring the column the find landed
/// on. Here `j` holds column 0, `f x` moves to column 1, and the second `j`
/// has to follow the find rather than snap back to column 0.
#[test]
fn select_mode_find_clears_the_vertical_goal_column() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "aaaa\nbxaa\ncccc\n");
    h.open_file(&path);
    h.type_keys("v j");
    h.type_keys("f x");
    assert_eq!(
        h.selection_spans()[0],
        (0, 7, false),
        "the find lands on the x at row 1 column 1",
    );

    h.type_keys("j");
    assert_eq!(
        h.selection_spans()[0],
        (0, 12, false),
        "the next row is entered at column 1, the column the find landed on",
    );
}

#[test]
fn select_mode_t_extends_till_next_char() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abcdef\n");
    h.open_file(&path);
    h.type_keys("v");
    h.type_keys("t e");
    let (start, end, reversed) = h.selection_spans()[0];
    assert_eq!((start, end, reversed), (0, 4, false));
}

#[test]
fn select_mode_capital_t_extends_till_prev_char() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abcdef\n");
    h.open_file(&path);
    h.type_keys("4 l v");
    h.type_keys("T b");
    let (start, end, reversed) = h.selection_spans()[0];
    assert_eq!(
        (start, end, reversed),
        (2, 5, true),
        "crossing the tail keeps the e the cursor was on covered",
    );
}

#[test]
fn normal_mode_f_selects_to_target() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abcdef\n");
    h.open_file(&path);
    h.type_keys("f e");
    let (start, end, _) = h.selection_spans()[0];
    assert_eq!(
        (start, end),
        (0, 5),
        "normal-mode find selects from the cursor to the target inclusive"
    );
}

#[test]
fn dfx_deletes_from_cursor_through_target() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "hello world\n");
    h.open_file(&path);
    h.type_keys("f o");
    h.type_keys("d");
    assert_eq!(focused_buffer_text(&mut h), " world\n");
}

#[test]
fn dt_deletes_up_to_but_not_the_target() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "hello world\n");
    h.open_file(&path);
    h.type_keys("t o");
    h.type_keys("d");
    assert_eq!(focused_buffer_text(&mut h), "o world\n");
}

#[test]
fn till_skips_an_adjacent_target() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "axbxc\n");
    h.open_file(&path);
    h.type_keys("t x");
    // The adjacent 'x' at 1 is skipped; till the second 'x' (at 3) selects
    // through 'b', ending before that 'x'.
    assert_eq!(h.selection_spans()[0], (0, 3, false));
}

#[test]
fn find_lands_on_the_adjacent_target() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "axbxc\n");
    h.open_file(&path);
    h.type_keys("f x");
    // `f` is never skipped, so it lands on the first, adjacent 'x'.
    assert_eq!(h.selection_spans()[0], (0, 2, false));
}

/// A buffer whose second statement spans two rows.
///
/// Both structural motions resolve their target from the node covering the
/// whole selection, so a vertical step has to leave the selection inside one
/// node that still has a parent or a sibling. Spanning two top-level items
/// instead resolves to the root, which has neither, and the motion does
/// nothing at all. A statement broken across two lines is the shape that
/// keeps them moving.
fn two_row_statement_source() -> &'static str {
    "fn m() {\n    let aaa =\n        1;\n    let bbbbbbbbbbbbbbbb = 2;\n    let ccccccccccccccccccccc = 3;\n}\n"
}

/// Moving to the next sibling is horizontal, so it drops the column a prior
/// vertical move was holding.
///
/// Here `j` holds column 8, the sibling extend moves the head to column 28,
/// and the second `j` has to follow the sibling rather than snap back.
#[test]
fn select_mode_next_sibling_clears_the_vertical_goal_column() {
    let mut h = TestHarness::with_size(60, 8);
    let path = h.write_file("s.rs", two_row_statement_source());
    h.open_file(&path);
    h.type_keys("j 8 l v j");
    assert_eq!(
        h.selection_spans()[0],
        (17, 32, false),
        "the head sits on row 2 column 8",
    );

    dispatch(&mut h.stoat, &stoat_action::ExtendSelectNextSibling);
    assert_eq!(
        h.selection_spans()[0],
        (17, 63, false),
        "the sibling extend moves the head to row 3 column 28",
    );

    h.type_keys("j");
    assert_eq!(
        h.selection_spans()[0],
        (17, 93, false),
        "the next row is entered at column 28, where the sibling extend left the head",
    );
}

/// The same for the move to a parent node's start, which goes backward.
#[test]
fn select_mode_parent_node_start_clears_the_vertical_goal_column() {
    let mut h = TestHarness::with_size(60, 8);
    let path = h.write_file("s.rs", two_row_statement_source());
    h.open_file(&path);
    h.type_keys("j 8 l v j");
    dispatch(&mut h.stoat, &stoat_action::ExtendMoveParentNodeStart);
    assert_eq!(
        h.selection_spans()[0],
        (7, 18, true),
        "the parent extend moves the head back to the block's brace on row 0",
    );

    h.type_keys("j");
    assert_eq!(
        h.selection_spans()[0],
        (16, 18, true),
        "the next row is entered at column 7, the brace's column, not the held column 8",
    );
}

#[test]
fn select_mode_alt_n_extends_to_next_sibling() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn a() {} fn b() {}\n");
    h.open_file(&path);
    h.type_keys("l l v");
    let before_offset = h.primary_head_offset();
    dispatch(&mut h.stoat, &stoat_action::ExtendSelectNextSibling);
    let head_after = h.primary_head_offset();
    let (start, end, _reversed) = h.selection_spans()[0];
    assert!(
        head_after > before_offset,
        "head should have moved forward across siblings"
    );
    assert!(end > start, "selection has non-empty range");
}

#[test]
fn select_mode_alt_p_extends_to_prev_sibling() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn a() {} fn b() {}\n");
    h.open_file(&path);
    h.type_keys("l l l l l l l l l l l l v");
    let before_offset = h.primary_head_offset();
    dispatch(&mut h.stoat, &stoat_action::ExtendSelectPrevSibling);
    let head_after = h.primary_head_offset();
    let (start, end, _reversed) = h.selection_spans()[0];
    assert!(
        head_after < before_offset,
        "head should have moved backward across siblings"
    );
    assert!(end > start, "selection has non-empty range");
}

#[test]
fn normal_mode_alt_n_still_collapses() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn a() {} fn b() {}\n");
    h.open_file(&path);
    h.type_keys("l l");
    dispatch(&mut h.stoat, &stoat_action::SelectNextSibling);
    let (start, end, _) = h.selection_spans()[0];
    assert!(end > start, "normal-mode sibling jump produces a range");
}

#[test]
fn select_mode_alt_b_extends_to_parent_node_start() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn main() { let x = 1; }\n");
    h.open_file(&path);
    h.type_keys("l l l l l l l l l l l l l l l l v");
    let before_offset = h.primary_head_offset();
    dispatch(&mut h.stoat, &stoat_action::ExtendMoveParentNodeStart);
    let head_after = h.primary_head_offset();
    let (start, end, reversed) = h.selection_spans()[0];
    assert!(
        head_after < before_offset,
        "head should have moved earlier in the buffer"
    );
    assert!(reversed, "head ahead of tail means selection is reversed");
    assert!(end > start, "selection has non-empty range");
}

#[test]
fn select_mode_alt_e_extends_to_parent_node_end() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn main() { let x = 1; }\n");
    h.open_file(&path);
    h.type_keys("l l l l l l l l l l l l l l l l v");
    let before_offset = h.primary_head_offset();
    dispatch(&mut h.stoat, &stoat_action::ExtendMoveParentNodeEnd);
    let head_after = h.primary_head_offset();
    let (start, end, reversed) = h.selection_spans()[0];
    assert!(
        head_after > before_offset,
        "head should have moved forward in the buffer"
    );
    assert!(!reversed, "head ahead of tail means selection is forward");
    assert!(end > start, "selection has non-empty range");
}

#[test]
fn normal_mode_alt_b_still_collapses() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn main() { let x = 1; }\n");
    h.open_file(&path);
    h.type_keys("l l l l l l l l l l l l l l l l");
    dispatch(&mut h.stoat, &stoat_action::MoveParentNodeStart);
    let (start, end, _) = h.selection_spans()[0];
    assert_eq!(
        end,
        start + 1,
        "normal-mode parent jump collapses to a 1-wide cursor"
    );
}

#[test]
fn select_mode_g_pipe_extends_to_column() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abcdefgh\n");
    h.open_file(&path);
    h.type_keys("v 5 g |");
    let (start, end, reversed) = h.selection_spans()[0];
    assert_eq!(
        (start, end, reversed),
        (0, 5, false),
        "head one past column 5 while tail stays at 0, cursor on offset 4"
    );
    assert_eq!(
        h.stoat.focused_mode(),
        "select",
        "back to select after the chord"
    );
}

#[test]
fn normal_mode_g_pipe_still_collapses() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abcdefgh\n");
    h.open_file(&path);
    h.type_keys("5 g |");
    assert_eq!(h.cursor_display_positions(), vec![(0, 4)]);
    assert_eq!(h.stoat.focused_mode(), "normal");
}

#[test]
fn select_mode_g_i_extends_to_first_nonwhitespace() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "    hello\n");
    h.open_file(&path);
    h.type_keys("8 l v");
    h.type_keys("g i");
    let (start, end, reversed) = h.selection_spans()[0];
    assert_eq!(
        (start, end, reversed),
        (4, 9, true),
        "crossing the tail keeps the o the cursor was on covered",
    );
    assert_eq!(h.stoat.focused_mode(), "select");
}

#[test]
fn select_mode_g_j_extends_to_last_line() {
    let mut h = TestHarness::with_size(30, 10);
    let path = h.write_file("s.txt", "a\nb\nc\nd\n");
    h.open_file(&path);
    h.type_keys("v");
    h.type_keys("g j");
    assert_eq!(
        h.primary_head_offset(),
        6,
        "block cursor lands on the last content line's char (`d`)"
    );
    assert_eq!(h.stoat.focused_mode(), "select");
}

#[test]
fn select_mode_g_k_extends_to_file_start() {
    let mut h = TestHarness::with_size(30, 10);
    let path = h.write_file("s.txt", "a\nb\nc\nd\n");
    h.open_file(&path);
    h.type_keys("j j j v");
    h.type_keys("g k");
    let (start, end, reversed) = h.selection_spans()[0];
    assert_eq!(
        (start, end, reversed),
        (0, 7, true),
        "crossing the tail keeps the cell the cursor was on covered"
    );
    assert_eq!(h.stoat.focused_mode(), "select");
}

#[test]
fn select_mode_g_t_extends_to_window_top() {
    let mut h = TestHarness::with_size(30, 10);
    let path = h.write_file("s.txt", "a\nb\nc\nd\ne\n");
    h.open_file(&path);
    h.type_keys("j j v");
    h.type_keys("g t");
    let (start, end, reversed) = h.selection_spans()[0];
    assert_eq!(
        (start, end, reversed),
        (0, 5, true),
        "head extended to row 0, and crossing the tail keeps the c on row 2 covered"
    );
    assert_eq!(h.stoat.focused_mode(), "select");
}

#[test]
fn normal_mode_g_i_still_collapses() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "    hello\n");
    h.open_file(&path);
    h.type_keys("8 l");
    h.type_keys("g i");
    let (start, end, _) = h.selection_spans()[0];
    assert_eq!((start, end), (4, 5));
    assert_eq!(h.stoat.focused_mode(), "normal");
}

#[test]
fn select_goto_escape_returns_to_select() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    h.type_keys("v g");
    assert_eq!(h.stoat.focused_mode(), "select_goto");
    h.type_keys("Escape");
    assert_eq!(h.stoat.focused_mode(), "select");
}

#[test]
fn repeat_last_motion_extends_in_select_mode() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "ababab\n");
    h.open_file(&path);
    h.type_keys("v");
    h.type_keys("f a");
    let after_first = h.selection_spans()[0];
    h.type_keys("Alt-.");
    let after_repeat = h.selection_spans()[0];
    assert!(
        after_repeat.1 > after_first.1,
        "Alt-. should extend further forward, got {after_first:?} -> {after_repeat:?}"
    );
    assert_eq!(after_repeat.0, 0, "tail still anchored at the start");
}

#[test]
fn switch_case_on_bare_cursor_toggles_char() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::SwitchCase);
    assert_eq!(focused_buffer_text(&mut h), "Abc\n");
}

#[test]
fn switch_to_uppercase_lower_selection() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "hello\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::SwitchToUppercase);
    assert_eq!(focused_buffer_text(&mut h), "HELLO\n");
}

#[test]
fn switch_to_uppercase_mixed_selection_is_idempotent_for_uppers() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "Hello World!\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::SwitchToUppercase);
    assert_eq!(focused_buffer_text(&mut h), "HELLO WORLD!\n");
}

#[test]
fn switch_to_lowercase_upper_selection() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "HELLO\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::SwitchToLowercase);
    assert_eq!(focused_buffer_text(&mut h), "hello\n");
}

#[test]
fn switch_to_lowercase_mixed_selection_is_idempotent_for_lowers() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "Hello World!\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::SwitchToLowercase);
    assert_eq!(focused_buffer_text(&mut h), "hello world!\n");
}

#[test]
fn switch_case_applies_to_each_split_cursor_range() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\ndef\nghi\n");
    h.open_file(&path);
    h.type_keys("% alt-s");
    dispatch(&mut h.stoat, &stoat_action::SwitchCase);
    assert_eq!(focused_buffer_text(&mut h), "ABC\nDEF\nGHI\n");
}

#[test]
fn increment_applies_to_each_split_cursor_range() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "1\n2\n3\n");
    h.open_file(&path);
    h.type_keys("2 shift-C");
    dispatch(&mut h.stoat, &stoat_action::Increment);
    assert_eq!(focused_buffer_text(&mut h), "2\n3\n4\n");
}

#[test]
fn delete_selection_removes_full_selection() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "hello world\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::DeleteSelection);
    assert_eq!(focused_buffer_text(&mut h), "");
}

#[test]
fn delete_selection_on_bare_cursor_deletes_char() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "hello\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::DeleteSelection);
    assert_eq!(focused_buffer_text(&mut h), "ello\n");
}

#[test]
fn delete_selection_removes_each_split_cursor_range() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\ndef\nghi\n");
    h.open_file(&path);
    h.type_keys("% alt-s");
    dispatch(&mut h.stoat, &stoat_action::DeleteSelection);
    assert_eq!(focused_buffer_text(&mut h), "\n\n\n");
}

#[test]
fn toggle_comments_rust_single_line_inserts_prefix() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "let x = 42;\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::ToggleComments);
    assert_eq!(focused_buffer_text(&mut h), "// let x = 42;\n");
}

#[test]
fn toggle_comments_rust_round_trip() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "let x = 42;\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::ToggleComments);
    dispatch(&mut h.stoat, &stoat_action::ToggleComments);
    assert_eq!(focused_buffer_text(&mut h), "let x = 42;\n");
}

/// Every prefix lands at the shallowest row's indent, so the block's own
/// indentation is preserved inside the comment rather than flattened.
///
/// Removal still works off each row's own prefix, which is what lets the
/// second toggle restore the original exactly.
#[test]
fn toggle_comments_rust_multi_line_selection() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn main() {\n    let x = 42;\n}\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::ToggleComments);
    assert_eq!(
        focused_buffer_text(&mut h),
        "// fn main() {\n//     let x = 42;\n// }\n",
        "prefix added at the shared column, indentation kept after it"
    );

    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::ToggleComments);
    assert_eq!(
        focused_buffer_text(&mut h),
        "fn main() {\n    let x = 42;\n}\n",
        "the round trip restores the original indentation"
    );
}

#[test]
fn toggle_comments_rust_skips_whitespace_only_lines() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "abc\n   \nxyz\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::ToggleComments);
    assert_eq!(
        focused_buffer_text(&mut h),
        "// abc\n   \n// xyz\n",
        "blank line in the middle stays uncommented"
    );
}

/// A mixed block commits to being commented, then uncomments as a whole.
///
/// Deciding per row would invert each one instead, leaving the block mixed
/// after every toggle and swapping its two halves forever. One uncommented
/// row is what makes the whole set count as uncommented.
#[test]
fn toggle_comments_rust_mixed_block_comments_all_then_uncomments_all() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "// abc\nxyz\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::ToggleComments);
    assert_eq!(
        focused_buffer_text(&mut h),
        "// // abc\n// xyz\n",
        "the one uncommented row commits the set to being commented"
    );

    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::ToggleComments);
    assert_eq!(
        focused_buffer_text(&mut h),
        "// abc\nxyz\n",
        "now that every row is commented the set uncomments"
    );
}

/// A blank row takes no edit in either direction, and it must not drag the
/// shared column left either.
#[test]
fn toggle_comments_rust_uncomment_skips_whitespace_only_lines() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "// abc\n   \n// xyz\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::ToggleComments);
    assert_eq!(
        focused_buffer_text(&mut h),
        "abc\n   \nxyz\n",
        "the blank line neither blocks the uncomment nor gets edited"
    );
}

/// Two selections reaching the same row comment it once.
///
/// The rows are deduped before any edit is built, so a row two selections
/// both reach costs one edit rather than one per selection. Without that,
/// the second edit would land on the offset the first already used and
/// stack a second prefix there.
///
/// The selections are set directly rather than driven by keys. Two that
/// share a row have to be disjoint in offsets to survive, since any pair
/// that overlaps is merged into one, which is a single selection again and
/// tests nothing.
#[test]
fn toggle_comments_rust_two_selections_reaching_a_row_comment_it_once() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "aaa\nbbb\nccc\n");
    h.open_file(&path);

    {
        let editor = focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        editor.selections.set_single_range(
            buf_snap.anchor_at(0, Bias::Left),
            buf_snap.anchor_at(5, Bias::Right),
            SelectionGoal::None,
        );
        editor.selections.extend_with_fresh_ids(
            vec![Selection {
                id: 0,
                start: buf_snap.anchor_at(5, Bias::Left),
                end: buf_snap.anchor_at(7, Bias::Right),
                reversed: false,
                goal: SelectionGoal::None,
            }],
            buf_snap,
        );
    }
    assert_eq!(
        h.selection_spans(),
        vec![(0, 5, false), (5, 7, false)],
        "row 1 is reached by both, row 2 by neither",
    );

    dispatch(&mut h.stoat, &stoat_action::ToggleComments);
    assert_eq!(
        focused_buffer_text(&mut h),
        "// aaa\n// bbb\nccc\n",
        "the shared row takes one prefix, not one per selection"
    );
}

#[test]
fn toggle_comments_rust_removes_prefix_without_trailing_space() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "//abc\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::ToggleComments);
    assert_eq!(
        focused_buffer_text(&mut h),
        "abc\n",
        "no-space branch must strip only the prefix, not eat `a`"
    );
}

#[test]
fn toggle_comments_toml_uses_hash() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.toml", "key = 1\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::ToggleComments);
    assert_eq!(focused_buffer_text(&mut h), "# key = 1\n");
}

#[test]
fn toggle_comments_json_no_op() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.json", "{}\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::ToggleComments);
    assert_eq!(
        focused_buffer_text(&mut h),
        "{}\n",
        "json has no line_comments, action no-ops"
    );
}

#[test]
fn indent_selection_inserts_tab_at_cursor_line() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::IndentSelection);
    assert_eq!(focused_buffer_text(&mut h), "\tabc\n");
}

#[test]
fn indent_selection_indents_every_covered_line() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\ndef\nghi\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::IndentSelection);
    assert_eq!(focused_buffer_text(&mut h), "\tabc\n\tdef\n\tghi\n");
}

#[test]
fn unindent_selection_removes_leading_tab() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "\tabc\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::UnindentSelection);
    assert_eq!(focused_buffer_text(&mut h), "abc\n");
}

#[test]
fn indent_selection_uses_space_indent_style() {
    let mut h = TestHarness::with_size(20, 5);
    // The 2-space indent of the second line makes the buffer space-styled.
    let path = h.write_file("s.txt", "a\n  b\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::IndentSelection);
    assert_eq!(focused_buffer_text(&mut h), "  a\n  b\n");
}

#[test]
fn indent_selection_skips_blank_lines() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\n\ndef\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::IndentSelection);
    assert_eq!(focused_buffer_text(&mut h), "\tabc\n\n\tdef\n");
}

#[test]
fn unindent_selection_removes_one_space_indent_width() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "  a\n  b\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::UnindentSelection);
    assert_eq!(focused_buffer_text(&mut h), "a\n  b\n");
}

#[test]
fn unindent_selection_no_leading_whitespace_is_noop() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::UnindentSelection);
    assert_eq!(focused_buffer_text(&mut h), "abc\n");
}

#[test]
fn count_prefix_indent_inserts_n_tabs() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    h.type_keys("3 >");
    assert_eq!(focused_buffer_text(&mut h), "\t\t\tabc\n");
}

#[test]
fn count_prefix_unindent_removes_n_tabs() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "\t\t\tabc\n");
    h.open_file(&path);
    h.type_keys("2 <");
    assert_eq!(focused_buffer_text(&mut h), "\tabc\n");
}

#[test]
fn count_prefix_unindent_removes_n_space_groups() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "        abc\n");
    h.open_file(&path);
    h.type_keys("2 <");
    assert_eq!(focused_buffer_text(&mut h), "abc\n");
}

#[test]
fn count_prefix_unindent_clamps_at_available_indent() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "\tabc\n");
    h.open_file(&path);
    h.type_keys("9 <");
    assert_eq!(focused_buffer_text(&mut h), "abc\n");
}

#[test]
fn indent_selection_dedupes_lines_across_multi_cursors() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\ndef\nghi\n");
    h.open_file(&path);
    h.type_keys("% alt-s");
    dispatch(&mut h.stoat, &stoat_action::IndentSelection);
    assert_eq!(focused_buffer_text(&mut h), "\tabc\n\tdef\n\tghi\n");
}

#[test]
fn align_selections_pads_shorter_lines_to_match_longest_head() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\ndefgh\nij\n");
    h.open_file(&path);
    h.type_keys("% alt-s");
    dispatch(&mut h.stoat, &stoat_action::AlignSelections);
    assert_eq!(focused_buffer_text(&mut h), "  abc\ndefgh\n   ij\n");
}

#[test]
fn align_from_select_mode_returns_to_normal() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\ndefgh\nij\n");
    h.open_file(&path);
    h.type_keys("% alt-s");
    h.stoat.set_focused_mode("select".into());
    assert_eq!(h.stoat.focused_mode(), "select");
    h.type_keys("&");
    assert_eq!(
        h.stoat.focused_mode(),
        "normal",
        "align exits select mode like Helix"
    );
    assert_eq!(focused_buffer_text(&mut h), "  abc\ndefgh\n   ij\n");
}

#[test]
fn align_selections_already_aligned_is_noop() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\ndef\nghi\n");
    h.open_file(&path);
    h.type_keys("% alt-s");
    let before = focused_buffer_text(&mut h);
    dispatch(&mut h.stoat, &stoat_action::AlignSelections);
    assert_eq!(focused_buffer_text(&mut h), before);
}

#[test]
fn align_selections_single_selection_is_noop() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "hello\n");
    h.open_file(&path);
    h.type_keys("%");
    let before = focused_buffer_text(&mut h);
    dispatch(&mut h.stoat, &stoat_action::AlignSelections);
    assert_eq!(focused_buffer_text(&mut h), before);
}

#[test]
fn align_selections_skips_multi_line_selection() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\ndefgh\nij\n");
    h.open_file(&path);
    h.type_keys("%");
    let before = focused_buffer_text(&mut h);
    dispatch(&mut h.stoat, &stoat_action::AlignSelections);
    assert_eq!(focused_buffer_text(&mut h), before);
}

#[test]
fn extending_two_cursors_into_each_other_leaves_one_selection() {
    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "aa\nbb\ncc\n");
    h.open_file(&path);

    // Cursors on the first two rows, each extended a row down, so the
    // second row belongs to both.
    h.type_keys("shift-C v j");
    assert_eq!(h.selection_spans(), vec![(0, 7, false)]);
}

#[test]
fn deleting_overlapping_selections_spares_the_text_outside_them() {
    let mut h = TestHarness::with_size(20, 6);
    let path = h.write_file("s.txt", "aa\nbb\ncc\n");
    h.open_file(&path);
    h.type_keys("shift-C v j");

    dispatch(&mut h.stoat, &stoat_action::DeleteSelection);
    assert_eq!(
        focused_buffer_text(&mut h),
        "c\n",
        "deleting the overlap twice consumed a character no selection covered"
    );
}

#[test]
fn undo_after_single_edit_restores_text() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "hello\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::DeleteSelection);
    assert_eq!(focused_buffer_text(&mut h), "");
    dispatch(&mut h.stoat, &stoat_action::Undo);
    assert_eq!(focused_buffer_text(&mut h), "hello\n");
}

#[test]
fn undo_consecutive_walks_history_back_to_origin() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::SwitchCase);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::SwitchCase);
    assert_eq!(focused_buffer_text(&mut h), "abc\n");
    dispatch(&mut h.stoat, &stoat_action::Undo);
    assert_eq!(focused_buffer_text(&mut h), "ABC\n");
    dispatch(&mut h.stoat, &stoat_action::Undo);
    assert_eq!(focused_buffer_text(&mut h), "abc\n");
}

#[test]
fn undo_past_end_of_history_is_noop() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "stays\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &stoat_action::Undo);
    let after_initial_undo = focused_buffer_text(&mut h);
    dispatch(&mut h.stoat, &stoat_action::Undo);
    assert_eq!(focused_buffer_text(&mut h), after_initial_undo);
}

#[test]
fn redo_after_undo_restores_edit() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "hello\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::DeleteSelection);
    assert_eq!(focused_buffer_text(&mut h), "");
    dispatch(&mut h.stoat, &stoat_action::Undo);
    assert_eq!(focused_buffer_text(&mut h), "hello\n");
    dispatch(&mut h.stoat, &stoat_action::Redo);
    assert_eq!(focused_buffer_text(&mut h), "");
}

/// An empty buffer holds one collapsed selection, because `min_width_1` has
/// no grapheme to widen over in either direction. Deleting it covers no
/// text, so the redo it would otherwise discard has to survive.
#[test]
fn deleting_a_collapsed_selection_keeps_the_redo() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "");
    h.open_file(&path);
    h.type_keys("i h e l l o escape");
    dispatch(&mut h.stoat, &stoat_action::Undo);
    assert_eq!(focused_buffer_text(&mut h), "", "undo empties the buffer");

    dispatch(&mut h.stoat, &stoat_action::DeleteSelection);
    dispatch(&mut h.stoat, &stoat_action::Redo);
    assert_eq!(
        focused_buffer_text(&mut h),
        "hello",
        "the redo outlives the delete"
    );
}

/// Replacing a collapsed selection writes as many characters as it covers,
/// which is none, so it must leave the redo alone for the same reason.
#[test]
fn replacing_a_collapsed_selection_keeps_the_redo() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "");
    h.open_file(&path);
    h.type_keys("i h e l l o escape");
    dispatch(&mut h.stoat, &stoat_action::Undo);
    assert_eq!(focused_buffer_text(&mut h), "", "undo empties the buffer");

    h.type_keys("r x");
    assert_eq!(focused_buffer_text(&mut h), "", "nothing to replace");

    dispatch(&mut h.stoat, &stoat_action::Redo);
    assert_eq!(
        focused_buffer_text(&mut h),
        "hello",
        "the redo outlives the replace"
    );
}

#[test]
fn redo_with_empty_redo_stack_is_noop() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "hello\n");
    h.open_file(&path);
    let before = focused_buffer_text(&mut h);
    dispatch(&mut h.stoat, &stoat_action::Redo);
    assert_eq!(focused_buffer_text(&mut h), before);
}

#[test]
fn count_prefix_undo_walks_back_n_steps() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::SwitchCase);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::SwitchCase);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::SwitchCase);
    assert_eq!(focused_buffer_text(&mut h), "ABC\n");
    h.type_keys("3 u");
    assert_eq!(focused_buffer_text(&mut h), "abc\n");
}

#[test]
fn count_prefix_redo_walks_forward_n_steps() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::SwitchCase);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::SwitchCase);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::SwitchCase);
    h.type_keys("3 u");
    assert_eq!(focused_buffer_text(&mut h), "abc\n");
    h.type_keys("3 U");
    assert_eq!(focused_buffer_text(&mut h), "ABC\n");
}

#[test]
fn count_prefix_undo_redo_round_trip_with_huge_count() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    h.type_keys("%");
    dispatch(&mut h.stoat, &stoat_action::SwitchCase);
    let after_edit = focused_buffer_text(&mut h);
    h.type_keys("9 9 u");
    h.type_keys("9 9 U");
    assert_eq!(
        focused_buffer_text(&mut h),
        after_edit,
        "huge undo + huge redo should round-trip back to post-edit state"
    );
}

fn install_diff_hunks(h: &mut TestHarness, line_starts: &[u32]) {
    use crate::diff_map::{DiffHunk, DiffHunkStatus, DiffMap};
    let hunks: Vec<DiffHunk> = line_starts
        .iter()
        .map(|&start| DiffHunk {
            status: DiffHunkStatus::Added,
            unstaged_lines: std::iter::once(start..(start + 1)).collect(),
            buffer_start_line: start,
            buffer_line_range: start..(start + 1),
            base_byte_range: 0..0,
            anchor_range: None,
            token_detail: None,
        })
        .collect();
    let dm = DiffMap::from_hunks(hunks, None);
    let ws = h.stoat.active_workspace();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        crate::pane::View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    };
    let buffer_id = ws.editors[editor_id].buffer_id;
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let mut guard = buffer.write().expect("poisoned");
    guard.diff_map = Some(dm);
}

#[test]
fn goto_next_change_jumps_forward() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\ng\nh\n");
    h.open_file(&path);
    install_diff_hunks(&mut h, &[2, 5]);

    dispatch(&mut h.stoat, &stoat_action::GotoNextChange);
    assert_eq!(h.primary_head_offset(), 4);
    dispatch(&mut h.stoat, &stoat_action::GotoNextChange);
    assert_eq!(h.primary_head_offset(), 10);
    dispatch(&mut h.stoat, &stoat_action::GotoNextChange);
    assert_eq!(h.primary_head_offset(), 10);
}

#[test]
fn goto_next_change_uses_a_background_populated_diff_map() {
    let mut h = TestHarness::with_size(20, 10);
    h.stage_review_scenario("/repo", &[("s.txt", "a\nb\nc\n", "a\nX\nc\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(Path::new("/repo/s.txt"));
    h.settle_diff_jobs();

    dispatch(&mut h.stoat, &stoat_action::GotoNextChange);
    assert_eq!(
        h.primary_head_offset(),
        2,
        "the background-populated diff map drives GotoNextChange to the modified row",
    );
}

#[test]
fn goto_prev_change_jumps_backward() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\ng\nh\n");
    h.open_file(&path);
    install_diff_hunks(&mut h, &[2, 5]);
    h.type_keys("g j");

    dispatch(&mut h.stoat, &stoat_action::GotoPrevChange);
    assert_eq!(h.primary_head_offset(), 10);
    dispatch(&mut h.stoat, &stoat_action::GotoPrevChange);
    assert_eq!(h.primary_head_offset(), 4);
    dispatch(&mut h.stoat, &stoat_action::GotoPrevChange);
    assert_eq!(h.primary_head_offset(), 4);
}

#[test]
fn count_prefix_goto_next_change_jumps_n_changes() {
    let mut h = TestHarness::with_size(20, 15);
    let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n");
    h.open_file(&path);
    install_diff_hunks(&mut h, &[2, 5, 8]);
    h.type_keys("2 ] g");
    assert_eq!(h.primary_head_offset(), 10);
}

#[test]
fn helix_bracket_c_jumps_to_next_change() {
    let mut h = TestHarness::with_size(20, 15);
    let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n");
    h.open_file(&path);
    install_diff_hunks(&mut h, &[2, 5, 8]);
    h.type_keys("] c");
    assert_eq!(h.primary_head_offset(), 4);
}

#[test]
fn helix_bracket_c_jumps_to_prev_change() {
    let mut h = TestHarness::with_size(20, 15);
    let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n");
    h.open_file(&path);
    install_diff_hunks(&mut h, &[2, 5, 8]);
    h.type_keys("g j");
    h.type_keys("[ c");
    assert_eq!(h.primary_head_offset(), 16);
}

#[test]
fn count_prefix_goto_prev_change_jumps_back_n_changes() {
    let mut h = TestHarness::with_size(20, 15);
    let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n");
    h.open_file(&path);
    install_diff_hunks(&mut h, &[2, 5, 8]);
    h.type_keys("g j");
    h.type_keys("2 [ g");
    assert_eq!(h.primary_head_offset(), 10);
}

#[test]
fn count_prefix_goto_next_change_clamps_at_last() {
    let mut h = TestHarness::with_size(20, 15);
    let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n");
    h.open_file(&path);
    install_diff_hunks(&mut h, &[2, 5, 8]);
    h.type_keys("9 ] g");
    assert_eq!(h.primary_head_offset(), 16);
}

#[test]
fn expand_selection_grows_from_cursor_to_token() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn main() {}\n");
    h.open_file(&path);
    h.type_keys("l l l");
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    let spans = h.selection_spans();
    assert_eq!(spans, [(3, 7, false)]);
}

#[test]
fn expand_selection_walks_to_parent_when_already_on_node() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn main() {}\n");
    h.open_file(&path);
    h.type_keys("l l l");
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    let first = h.selection_spans()[0];
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    let second = h.selection_spans()[0];
    assert!(
        second.0 <= first.0 && second.1 >= first.1 && second != first,
        "second expansion should cover at least the first ({first:?} -> {second:?})"
    );
}

#[test]
fn expand_selection_dives_into_injection_layer() {
    let mut h = TestHarness::with_size(60, 10);
    let path = h.write_file("s.md", "# Title\n\nSome **bold** text\n");
    h.open_file(&path);
    h.type_keys("j j 7 l");
    assert_eq!(
        h.primary_head_offset(),
        16,
        "test setup: cursor should be on 'b' in 'bold'"
    );
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    let snippet = "# Title\n\nSome **bold** text\n";
    let (start, end, _) = h.selection_spans()[0];
    assert!(end > start, "expansion produced empty range");
    let selected = &snippet[start..end];
    let inline_text = "Some **bold** text";
    assert!(
        selected.contains("bold") && selected.len() < inline_text.len(),
        "expected inner-grammar node containing 'bold' but tighter than the inline node \"{inline_text}\" ({}..{}), got {start}..{end} = {selected:?}",
        snippet.find(inline_text).unwrap(),
        snippet.find(inline_text).unwrap() + inline_text.len(),
    );
}

#[test]
fn select_sibling_pivots_within_injection_layer() {
    let mut h = TestHarness::with_size(60, 10);
    let path = h.write_file("s.md", "aaa **bbb** ccc *ddd* eee\n");
    h.open_file(&path);
    for _ in 0..6 {
        h.type_keys("l");
    }
    assert_eq!(
        h.primary_head_offset(),
        6,
        "test setup: cursor should be on first 'b' of 'bbb'"
    );
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    let initial = h.selection_spans()[0];
    dispatch(&mut h.stoat, &stoat_action::SelectNextSibling);
    let after = h.selection_spans()[0];
    assert_ne!(
        after, initial,
        "next sibling inside the inline injection must shift the selection: \
         with the host markdown grammar the entire inline content is one leaf"
    );
    let line_end = 25;
    assert!(
        after.1 <= line_end,
        "next sibling should not escape past the line end {line_end}, got {after:?}"
    );
}

#[test]
fn expand_selection_no_op_without_syntax_map() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.txt", "plain text content\n");
    h.open_file(&path);
    let before = h.selection_spans();
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    assert_eq!(h.selection_spans(), before);
}

#[test]
fn shrink_selection_restores_previous_after_expand() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn main() {}\n");
    h.open_file(&path);
    h.type_keys("l l l");
    let before = h.selection_spans();
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    assert_ne!(h.selection_spans(), before);
    dispatch(&mut h.stoat, &stoat_action::ShrinkSelection);
    assert_eq!(h.selection_spans(), before);
}

#[test]
fn shrink_walks_full_expansion_chain() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn main() {}\n");
    h.open_file(&path);
    h.type_keys("l l l");
    let step0 = h.selection_spans();
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    let step1 = h.selection_spans();
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    let step2 = h.selection_spans();
    assert_ne!(step1, step0);
    assert_ne!(step2, step1);
    dispatch(&mut h.stoat, &stoat_action::ShrinkSelection);
    assert_eq!(h.selection_spans(), step1);
    dispatch(&mut h.stoat, &stoat_action::ShrinkSelection);
    assert_eq!(h.selection_spans(), step0);
}

#[test]
fn shrink_with_no_history_is_noop() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn main() {}\n");
    h.open_file(&path);
    let before = h.selection_spans();
    dispatch(&mut h.stoat, &stoat_action::ShrinkSelection);
    assert_eq!(h.selection_spans(), before);
}

#[test]
fn count_prefix_expand_selection_walks_n_levels() {
    let mut h_count = TestHarness::with_size(40, 5);
    let path1 = h_count.write_file("s.rs", "fn main() {}\n");
    h_count.open_file(&path1);
    h_count.type_keys("l l l");
    h_count.type_keys("3 alt-o");
    let count_result = h_count.selection_spans();

    let mut h_loop = TestHarness::with_size(40, 5);
    let path2 = h_loop.write_file("s.rs", "fn main() {}\n");
    h_loop.open_file(&path2);
    h_loop.type_keys("l l l");
    for _ in 0..3 {
        dispatch(&mut h_loop.stoat, &stoat_action::ExpandSelection);
    }
    let loop_result = h_loop.selection_spans();

    assert_eq!(
        count_result, loop_result,
        "count-prefix expand should match repeated single expand"
    );
}

#[test]
fn count_prefix_shrink_selection_walks_back_n_levels() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn main() {}\n");
    h.open_file(&path);
    h.type_keys("l l l");
    let before = h.selection_spans();
    for _ in 0..3 {
        dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    }
    assert_ne!(h.selection_spans(), before);
    h.type_keys("3 alt-i");
    assert_eq!(
        h.selection_spans(),
        before,
        "3 alt-i should rewind 3 expansions to the original selection"
    );
}

#[test]
fn count_prefix_expand_selection_clamps_at_root() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn x() {}\n");
    h.open_file(&path);
    h.type_keys("l");
    h.type_keys("9 9 alt-o");
    let after_huge = h.selection_spans();
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    assert_eq!(
        h.selection_spans(),
        after_huge,
        "additional expand at root should be a no-op"
    );
}

#[test]
fn select_next_sibling_jumps_to_next_named_node() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn a() {}\nfn b() {}\n");
    h.open_file(&path);
    h.type_keys("l l l");
    // A 1-wide cursor already sits on the `a` identifier node, so a single
    // expand reaches the enclosing function_item.
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    let on_first_fn = h.selection_spans();
    dispatch(&mut h.stoat, &stoat_action::SelectNextSibling);
    let on_second_fn = h.selection_spans();
    assert_ne!(on_second_fn, on_first_fn);
    assert!(
        on_second_fn[0].0 >= on_first_fn[0].1,
        "next sibling should start at or after first sibling end ({on_first_fn:?} -> {on_second_fn:?})"
    );
}

#[test]
fn select_prev_sibling_walks_back() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn a() {}\nfn b() {}\n");
    h.open_file(&path);
    h.type_keys("l l l");
    // A 1-wide cursor already sits on the `a` identifier node, so a single
    // expand reaches the enclosing function_item.
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    let on_first_fn = h.selection_spans();
    dispatch(&mut h.stoat, &stoat_action::SelectNextSibling);
    dispatch(&mut h.stoat, &stoat_action::SelectPrevSibling);
    assert_eq!(h.selection_spans(), on_first_fn);
}

#[test]
fn select_all_siblings_fans_to_each_named_sibling() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
    h.open_file(&path);
    h.type_keys("l l l");
    // A 1-wide cursor already sits on the `a` identifier node, so a single
    // expand reaches the enclosing function_item (a zero-width cursor
    // needed two).
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    dispatch(&mut h.stoat, &stoat_action::SelectAllSiblings);
    let spans = h.selection_spans();
    assert_eq!(spans, vec![(0, 9, false), (10, 19, false), (20, 29, false)]);
}

#[test]
fn select_all_children_fans_to_each_named_child() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
    h.open_file(&path);
    h.type_keys("l l l");
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    dispatch(&mut h.stoat, &stoat_action::SelectAllChildren);
    let spans = h.selection_spans();
    assert_eq!(spans, vec![(0, 9, false), (10, 19, false), (20, 29, false)]);
}

#[test]
fn select_all_siblings_no_op_without_syntax_map() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.txt", "alpha beta gamma\n");
    h.open_file(&path);
    let before = h.selection_spans();
    dispatch(&mut h.stoat, &stoat_action::SelectAllSiblings);
    assert_eq!(h.selection_spans(), before);
}

#[test]
fn select_sibling_no_op_without_syntax_map() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.txt", "alpha beta gamma\n");
    h.open_file(&path);
    let before = h.selection_spans();
    dispatch(&mut h.stoat, &stoat_action::SelectNextSibling);
    assert_eq!(h.selection_spans(), before);
}

#[test]
fn count_prefix_select_sibling_walks_n_siblings() {
    let mut h_count = TestHarness::with_size(40, 5);
    let path1 = h_count.write_file("s.rs", "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\n");
    h_count.open_file(&path1);
    h_count.type_keys("l l l");
    dispatch(&mut h_count.stoat, &stoat_action::ExpandSelection);
    dispatch(&mut h_count.stoat, &stoat_action::ExpandSelection);
    h_count.type_keys("3 alt-n");
    let count_result = h_count.selection_spans();

    let mut h_loop = TestHarness::with_size(40, 5);
    let path2 = h_loop.write_file("s.rs", "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\n");
    h_loop.open_file(&path2);
    h_loop.type_keys("l l l");
    dispatch(&mut h_loop.stoat, &stoat_action::ExpandSelection);
    dispatch(&mut h_loop.stoat, &stoat_action::ExpandSelection);
    for _ in 0..3 {
        dispatch(&mut h_loop.stoat, &stoat_action::SelectNextSibling);
    }
    let loop_result = h_loop.selection_spans();

    assert_eq!(
        count_result, loop_result,
        "count-prefix select_sibling should match repeated single dispatch"
    );
}

#[test]
fn count_prefix_select_sibling_clamps_at_chain_end() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn a() {}\nfn b() {}\n");
    h.open_file(&path);
    h.type_keys("l l l");
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    h.type_keys("9 alt-n");
    let after_huge = h.selection_spans();
    dispatch(&mut h.stoat, &stoat_action::SelectNextSibling);
    assert_eq!(
        h.selection_spans(),
        after_huge,
        "next sibling at end-of-chain after huge count should be a no-op"
    );
}

#[test]
fn count_prefix_move_to_parent_walks_higher_than_single_step() {
    let mut h_single = TestHarness::with_size(40, 5);
    let p1 = h_single.write_file("s.rs", "fn main() { let x = (1 + 2); }\n");
    h_single.open_file(&p1);
    h_single.type_keys("l l l l l l l l l l l l l l l l l l l l l l");
    let starting = h_single.primary_head_offset();
    dispatch(&mut h_single.stoat, &stoat_action::MoveParentNodeStart);
    let single_offset = h_single.primary_head_offset();
    assert!(
        single_offset < starting,
        "1 Alt-b should move backward from {starting} (got {single_offset})"
    );

    let mut h_count = TestHarness::with_size(40, 5);
    let p2 = h_count.write_file("s.rs", "fn main() { let x = (1 + 2); }\n");
    h_count.open_file(&p2);
    h_count.type_keys("l l l l l l l l l l l l l l l l l l l l l l");
    h_count.type_keys("3 alt-b");
    let count_offset = h_count.primary_head_offset();
    assert!(
        count_offset < single_offset,
        "3 Alt-b should walk further up than 1 Alt-b ({single_offset} -> {count_offset})"
    );
}

#[test]
fn select_sibling_no_op_at_tree_edge() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn only() {}\n");
    h.open_file(&path);
    h.type_keys("l l l");
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    let before = h.selection_spans();
    dispatch(&mut h.stoat, &stoat_action::SelectNextSibling);
    assert_eq!(h.selection_spans(), before);
}

#[test]
fn move_parent_node_start_collapses_to_parent_start() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn main() { let x = 1; }\n");
    h.open_file(&path);
    h.type_keys("l l l l l l l l l l l l l l l l");
    let before_offset = h.primary_head_offset();
    dispatch(&mut h.stoat, &stoat_action::MoveParentNodeStart);
    let after_offset = h.primary_head_offset();
    assert!(
        after_offset < before_offset,
        "MoveParentNodeStart should move cursor left from {before_offset} to a smaller offset (got {after_offset})"
    );
    let spans = h.selection_spans();
    assert_eq!(spans.len(), 1);
    assert_eq!(
        spans[0].1,
        spans[0].0 + 1,
        "parent jump collapses to a 1-wide cursor"
    );
}

#[test]
fn move_parent_node_end_collapses_to_parent_end() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn main() { let x = 1; }\n");
    h.open_file(&path);
    h.type_keys("l l l l l l l l l l l l l l l l");
    let before_offset = h.primary_head_offset();
    dispatch(&mut h.stoat, &stoat_action::MoveParentNodeEnd);
    let after_offset = h.primary_head_offset();
    assert!(
        after_offset > before_offset,
        "MoveParentNodeEnd should move cursor right from {before_offset} to a larger offset (got {after_offset})"
    );
    let spans = h.selection_spans();
    assert_eq!(spans.len(), 1);
    assert_eq!(
        spans[0].1,
        spans[0].0 + 1,
        "parent jump collapses to a 1-wide cursor"
    );
}

#[test]
fn jump_backward_restores_saved_position() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.txt", "abcdefghij\n");
    h.open_file(&path);
    h.type_keys("l l l");
    let saved = h.primary_head_offset();
    dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    h.type_keys("l l l");
    assert_ne!(h.primary_head_offset(), saved);
    dispatch(&mut h.stoat, &stoat_action::JumpBackward);
    assert_eq!(h.primary_head_offset(), saved);
}

#[test]
fn jump_forward_walks_back_after_jump_backward() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.txt", "abcdefghij\n");
    h.open_file(&path);
    h.type_keys("l l");
    let a = h.primary_head_offset();
    dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    h.type_keys("l l l");
    let b = h.primary_head_offset();
    dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    h.type_keys("l l");
    dispatch(&mut h.stoat, &stoat_action::JumpBackward);
    assert_eq!(h.primary_head_offset(), b);
    dispatch(&mut h.stoat, &stoat_action::JumpBackward);
    assert_eq!(h.primary_head_offset(), a);
    dispatch(&mut h.stoat, &stoat_action::JumpForward);
    assert_eq!(h.primary_head_offset(), b);
}

#[test]
fn jump_with_empty_jumplist_is_noop() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.txt", "hello\n");
    h.open_file(&path);
    let before = h.primary_head_offset();
    dispatch(&mut h.stoat, &stoat_action::JumpBackward);
    assert_eq!(h.primary_head_offset(), before);
    dispatch(&mut h.stoat, &stoat_action::JumpForward);
    assert_eq!(h.primary_head_offset(), before);
}

#[test]
fn count_prefix_jump_backward_walks_n_entries() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.txt", "abcdefghij\n");
    h.open_file(&path);
    h.type_keys("l");
    let a = h.primary_head_offset();
    dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    h.type_keys("l l");
    dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    h.type_keys("l l");
    dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    h.type_keys("l l");
    h.stoat.pending_count = Some(3);
    dispatch(&mut h.stoat, &stoat_action::JumpBackward);
    assert_eq!(
        h.primary_head_offset(),
        a,
        "3 jumps back from the third saved position should land on the first save"
    );
}

#[test]
fn count_prefix_jump_forward_walks_n_entries() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.txt", "abcdefghij\n");
    h.open_file(&path);
    h.type_keys("l");
    let _a = h.primary_head_offset();
    dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    h.type_keys("l l");
    let _b = h.primary_head_offset();
    dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    h.type_keys("l l");
    let c = h.primary_head_offset();
    dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    h.type_keys("l l");
    h.stoat.pending_count = Some(3);
    dispatch(&mut h.stoat, &stoat_action::JumpBackward);
    h.stoat.pending_count = Some(2);
    dispatch(&mut h.stoat, &stoat_action::JumpForward);
    assert_eq!(
        h.primary_head_offset(),
        c,
        "2 jumps forward from oldest should reach the third save"
    );
}

#[test]
fn count_prefix_jump_backward_past_history_is_noop() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.txt", "abcdefghij\n");
    h.open_file(&path);
    h.type_keys("l");
    dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    h.type_keys("l l");
    dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    h.type_keys("l l");
    let before = h.primary_head_offset();
    h.stoat.pending_count = Some(99);
    dispatch(&mut h.stoat, &stoat_action::JumpBackward);
    assert_eq!(
        h.primary_head_offset(),
        before,
        "a count past the history start is all-or-nothing, so nothing moves"
    );
}

#[test]
fn count_prefix_repeats_move_down() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\ng\nh\n");
    h.open_file(&path);
    h.type_keys("4 j");
    let positions = h.cursor_display_positions();
    assert_eq!(positions, vec![(4, 0)]);
}

#[test]
fn count_prefix_resets_after_motion() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\ng\nh\n");
    h.open_file(&path);
    h.type_keys("4 j");
    let after_count = h.cursor_display_positions();
    assert_eq!(after_count, vec![(4, 0)]);
    h.type_keys("j");
    let after_plain = h.cursor_display_positions();
    assert_eq!(after_plain, vec![(5, 0)]);
}

#[test]
fn find_next_char_jumps_forward() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abcdefg\n");
    h.open_file(&path);
    h.type_keys("f c");
    assert_eq!(h.primary_head_offset(), 2);
}

/// Each cursor scans from itself, so a find keeps a multi-cursor set.
///
/// Scanning once from the primary and stamping the result on every
/// selection makes them all identical, and identical spans merge, so the
/// set collapses to a single cursor on the primary's match.
#[test]
fn find_next_char_scans_from_each_cursor() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "ax\nax\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &AddSelectionBelow);
    assert_eq!(
        h.selection_spans(),
        vec![(0, 1, false), (3, 4, false)],
        "one cursor on each row",
    );

    h.type_keys("f x");
    assert_eq!(
        h.selection_spans(),
        vec![(0, 2, false), (3, 5, false)],
        "each cursor covers up to the x on its own row",
    );
}

/// A cursor whose row holds no match stays where it is rather than being
/// dragged onto another cursor's target.
#[test]
fn find_next_char_leaves_unmatched_cursors_alone() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abb\naxb\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &AddSelectionBelow);

    h.type_keys("f x");
    assert_eq!(
        h.selection_spans(),
        vec![(0, 1, false), (4, 6, false)],
        "row 0 has no x so its cursor holds, row 1 covers up to its x",
    );
}

#[test]
fn find_next_char_no_match_keeps_cursor() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abcdefg\n");
    h.open_file(&path);
    let before = h.primary_head_offset();
    h.type_keys("f z");
    assert_eq!(h.primary_head_offset(), before);
}

#[test]
fn find_prev_char_jumps_backward() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abcdefg\n");
    h.open_file(&path);
    h.type_keys("l l l l l l");
    h.type_keys("F b");
    assert_eq!(h.primary_head_offset(), 1);
}

#[test]
fn till_next_char_lands_one_before() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abcdefg\n");
    h.open_file(&path);
    h.type_keys("t c");
    assert_eq!(h.primary_head_offset(), 1);
}

#[test]
fn till_prev_char_lands_one_after() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abcdefg\n");
    h.open_file(&path);
    h.type_keys("l l l l l l");
    h.type_keys("T b");
    assert_eq!(h.primary_head_offset(), 2);
}

#[test]
fn repeat_last_motion_replays_find_next_char() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abcabcabc\n");
    h.open_file(&path);
    h.type_keys("f c");
    assert_eq!(h.primary_head_offset(), 2);
    h.type_keys("alt-.");
    assert_eq!(h.primary_head_offset(), 5);
    h.type_keys("alt-.");
    assert_eq!(h.primary_head_offset(), 8);
}

#[test]
fn repeat_last_motion_with_no_history_is_noop() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "hello\n");
    h.open_file(&path);
    let before = h.primary_head_offset();
    h.type_keys("alt-.");
    assert_eq!(h.primary_head_offset(), before);
}

#[test]
fn repeat_last_motion_uses_most_recent_find_kind() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abcabcabc\n");
    h.open_file(&path);
    h.type_keys("f c");
    assert_eq!(h.primary_head_offset(), 2);
    h.type_keys("F a");
    assert_eq!(h.primary_head_offset(), 0);
    h.type_keys("l l l l");
    assert_eq!(h.primary_head_offset(), 4);
    h.type_keys("alt-.");
    assert_eq!(h.primary_head_offset(), 3);
}

#[test]
fn find_aborts_on_escape() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abcdefg\n");
    h.open_file(&path);
    let before = h.primary_head_offset();
    h.type_keys("f");
    h.type_keys("Escape");
    h.type_keys("c");
    assert_eq!(h.primary_head_offset(), before);
}

#[test]
fn count_prefix_find_next_char_jumps_to_nth_match() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abcabcabc\n");
    h.open_file(&path);
    h.type_keys("3 f c");
    assert_eq!(h.primary_head_offset(), 8);
}

#[test]
fn count_prefix_till_next_char_lands_one_before_nth_match() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abcabcabc\n");
    h.open_file(&path);
    h.type_keys("2 t c");
    assert_eq!(h.primary_head_offset(), 4);
}

#[test]
fn count_prefix_find_prev_char_walks_back_n_matches() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abcabcabc\n");
    h.open_file(&path);
    h.type_keys("l l l l l l l l l");
    assert_eq!(h.primary_head_offset(), 9);
    h.type_keys("3 F a");
    assert_eq!(h.primary_head_offset(), 0);
}

#[test]
fn count_prefix_till_prev_char_lands_one_after_nth_match() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abcabcabc\n");
    h.open_file(&path);
    h.type_keys("l l l l l l l l l");
    h.type_keys("2 T a");
    assert_eq!(h.primary_head_offset(), 4);
}

#[test]
fn count_prefix_find_no_op_when_fewer_than_count_matches() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abcabc\n");
    h.open_file(&path);
    let before = h.primary_head_offset();
    h.type_keys("9 f c");
    assert_eq!(h.primary_head_offset(), before);
}

#[test]
fn count_prefix_repeat_last_motion_advances_n_matches() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abcabcabcabc\n");
    h.open_file(&path);
    h.type_keys("f c");
    assert_eq!(h.primary_head_offset(), 2);
    h.type_keys("3 alt-.");
    assert_eq!(h.primary_head_offset(), 11);
}

#[test]
fn snapshot_pending_count_appears_in_status_bar() {
    let mut h = TestHarness::with_size(40, 6);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    h.type_keys("4");
    h.assert_snapshot("snapshot_pending_count_appears_in_status_bar");
}

#[test]
fn bare_zero_jumps_to_line_start() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc def\n");
    h.open_file(&path);
    h.type_keys("l l l l");
    assert_eq!(h.primary_head_offset(), 4);
    h.type_keys("0");
    assert_eq!(h.primary_head_offset(), 0);
}

#[test]
fn zero_accumulates_into_pending_count() {
    let mut h = TestHarness::with_size(20, 50);
    let body: String = (0..50).map(|i| format!("line{i}\n")).collect();
    let path = h.write_file("s.txt", &body);
    h.open_file(&path);
    h.type_keys("4 0 j");
    let positions = h.cursor_display_positions();
    assert_eq!(positions[0].0, 40);
}

#[test]
fn count_prefix_repeats_move_right() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abcdefghij\n");
    h.open_file(&path);
    h.type_keys("4 l");
    assert_eq!(h.primary_head_offset(), 4);
}

#[test]
fn count_prefix_repeats_move_left() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abcdefghij\n");
    h.open_file(&path);
    h.type_keys("5 l");
    assert_eq!(h.primary_head_offset(), 5);
    h.type_keys("3 h");
    assert_eq!(h.primary_head_offset(), 2);
}

#[test]
fn count_prefix_repeats_next_word_start() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.txt", "alpha beta gamma delta\n");
    h.open_file(&path);
    h.type_keys("3 w");
    // "alpha beta gamma " is 17 bytes; "delta" starts at offset 17. Three
    // threaded `w` jumps advance the anchor onto the third word start (11)
    // and the head onto "delta", so the span is (11, 17) and the block
    // cursor sits one cell back, on the space at offset 16.
    assert_eq!(h.selection_spans(), vec![(11, 17, false)]);
    assert_eq!(h.cursor_display_positions(), vec![(0, 16)]);
}

#[test]
fn count_prefix_repeats_prev_word_start() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.txt", "alpha beta gamma delta\n");
    h.open_file(&path);
    h.type_keys("g j");
    h.type_keys("3 b");
    let positions = h.cursor_display_positions();
    assert_eq!(positions[0].0, 0, "should be back on row 0");
    assert!(
        positions[0].1 < 16,
        "3b from end should land before delta (got col {})",
        positions[0].1
    );
}

#[test]
fn count_prefix_repeats_next_long_word_start() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.txt", "foo.bar baz qux quux\n");
    h.open_file(&path);
    h.stoat.pending_count = Some(3);
    dispatch(&mut h.stoat, &stoat_action::MoveNextLongWordStart);
    assert_eq!(
        h.primary_head_offset(),
        15,
        "long-word treats `foo.bar` as one word, so 3W from offset 0 \
         selects up to `quux` (head at 16); the block cursor sits one \
         cell back, on the space at 15"
    );
}

#[test]
fn goto_line_number_jumps_to_count_line() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\ng\nh\n");
    h.open_file(&path);
    h.type_keys("5 G");
    let positions = h.cursor_display_positions();
    assert_eq!(positions, vec![(4, 0)]);
}

#[test]
fn goto_line_number_clamps_at_last_line() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "a\nb\nc\nd\n");
    h.open_file(&path);
    h.type_keys("9 9 G");
    let positions = h.cursor_display_positions();
    assert_eq!(positions, vec![(3, 0)]);
}

#[test]
fn goto_line_number_without_count_jumps_to_last_line() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "a\nb\nc\nd\ne\n");
    h.open_file(&path);
    h.type_keys("G");
    let with_g = h.cursor_display_positions();
    let mut h2 = TestHarness::with_size(20, 10);
    let path2 = h2.write_file("s.txt", "a\nb\nc\nd\ne\n");
    h2.open_file(&path2);
    h2.type_keys("g j");
    let with_gj = h2.cursor_display_positions();
    assert_eq!(with_g, with_gj);
}

#[test]
fn goto_column_jumps_to_count_column() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abcdefgh\n");
    h.open_file(&path);
    h.type_keys("5 g |");
    assert_eq!(h.cursor_display_positions(), vec![(0, 4)]);
}

#[test]
fn goto_column_without_count_lands_at_line_start() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abcdefgh\n");
    h.open_file(&path);
    h.type_keys("l l l l g |");
    assert_eq!(h.cursor_display_positions(), vec![(0, 0)]);
}

#[test]
fn goto_column_clamps_to_line_end() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    h.type_keys("9 9 g |");
    assert_eq!(h.cursor_display_positions(), vec![(0, 3)]);
}

#[test]
fn goto_column_stays_on_current_row() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abcdef\nghijkl\nmnopqr\n");
    h.open_file(&path);
    h.type_keys("j 4 g |");
    assert_eq!(h.cursor_display_positions(), vec![(1, 3)]);
}

#[test]
fn goto_column_walks_chars_not_bytes() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "αβγδε\n");
    h.open_file(&path);
    h.type_keys("3 g |");
    let offset = h.primary_head_offset();
    assert_eq!(
        offset, 4,
        "third column on a 2-byte-per-char line is byte 4"
    );
}

/// A column is a grapheme cluster, not a codepoint.
///
/// A letter carrying a combining mark is two codepoints and one column, so
/// counting codepoints walks into the middle of the cluster and stops a
/// column early. Here the third column is the `y`, not the `x`.
#[test]
fn goto_column_counts_graphemes_not_codepoints() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "e\u{301}xyz\n");
    h.open_file(&path);
    h.type_keys("3 g |");
    assert_eq!(h.cursor_display_positions(), vec![(0, 2)]);
    assert_eq!(
        h.primary_head_offset(),
        4,
        "the y, one past the two-codepoint cluster and the x",
    );
}

/// Every cursor takes the column on its own row.
///
/// Computing one target from the newest selection and stamping it on the
/// rest makes every span identical, and identical spans merge, so the set
/// collapses to a single cursor.
#[test]
fn goto_column_lands_each_cursor_on_its_own_row() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abcdef\nghijkl\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &AddSelectionBelow);
    assert_eq!(
        h.selection_spans(),
        vec![(0, 1, false), (7, 8, false)],
        "one cursor on each row",
    );

    h.type_keys("3 g |");
    assert_eq!(
        h.selection_spans(),
        vec![(2, 3, false), (9, 10, false)],
        "each cursor lands on column 3 of its own row",
    );
}

#[test]
fn count_survives_setmode_chord() {
    let mut split = TestHarness::with_size(20, 5);
    let split_path = split.write_file("s.txt", "abcdefgh\n");
    split.open_file(&split_path);
    split.type_keys("5 g");
    split.type_keys("|");
    let mut chord = TestHarness::with_size(20, 5);
    let chord_path = chord.write_file("s.txt", "abcdefgh\n");
    chord.open_file(&chord_path);
    chord.type_keys("5 g |");
    assert_eq!(
        split.cursor_display_positions(),
        chord.cursor_display_positions()
    );
}

#[test]
fn goto_next_paragraph_jumps_from_paragraph_start() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "alpha\nbeta\n\ngamma\ndelta\n");
    h.open_file(&path);
    h.type_keys("] p");
    assert_eq!(h.cursor_display_positions(), vec![(3, 0)]);
}

#[test]
fn goto_next_paragraph_jumps_from_middle_of_paragraph() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "alpha\nbeta\n\ngamma\ndelta\n");
    h.open_file(&path);
    h.type_keys("j ] p");
    assert_eq!(h.cursor_display_positions(), vec![(3, 0)]);
}

#[test]
fn goto_next_paragraph_no_op_at_buffer_end() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "alpha\nbeta\n");
    h.open_file(&path);
    h.type_keys("j ] p");
    assert_eq!(h.cursor_display_positions(), vec![(1, 0)]);
}

#[test]
fn goto_next_paragraph_walks_through_multiple_blanks() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "alpha\n\n\n\nbeta\n");
    h.open_file(&path);
    h.type_keys("] p");
    assert_eq!(h.cursor_display_positions(), vec![(4, 0)]);
}

#[test]
fn goto_prev_paragraph_jumps_from_paragraph_start() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "alpha\nbeta\n\ngamma\ndelta\n");
    h.open_file(&path);
    h.type_keys("j j j [ p");
    assert_eq!(h.cursor_display_positions(), vec![(0, 0)]);
}

#[test]
fn goto_prev_paragraph_jumps_from_middle_of_paragraph() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "alpha\nbeta\n\ngamma\ndelta\n");
    h.open_file(&path);
    h.type_keys("j j j j [ p");
    assert_eq!(h.cursor_display_positions(), vec![(3, 0)]);
}

#[test]
fn goto_prev_paragraph_no_op_at_buffer_start() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "alpha\nbeta\n\ngamma\n");
    h.open_file(&path);
    h.type_keys("[ p");
    assert_eq!(h.cursor_display_positions(), vec![(0, 0)]);
}

#[test]
fn goto_next_paragraph_from_empty_line_lands_on_following_paragraph() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "alpha\n\nbeta\n");
    h.open_file(&path);
    h.type_keys("j ] p");
    assert_eq!(h.cursor_display_positions(), vec![(2, 0)]);
}

#[test]
fn count_prefix_goto_next_paragraph_jumps_n_paragraphs() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "a\n\nb\n\nc\n\nd\n");
    h.open_file(&path);
    h.type_keys("3 ] p");
    assert_eq!(h.cursor_display_positions(), vec![(6, 0)]);
}

#[test]
fn count_prefix_goto_prev_paragraph_jumps_back_n_paragraphs() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "a\n\nb\n\nc\n\nd\n");
    h.open_file(&path);
    h.type_keys("6 j");
    assert_eq!(h.cursor_display_positions(), vec![(6, 0)]);
    h.type_keys("3 [ p");
    assert_eq!(h.cursor_display_positions(), vec![(0, 0)]);
}

#[test]
fn count_prefix_goto_next_paragraph_clamps_at_last_paragraph() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "a\n\nb\n");
    h.open_file(&path);
    h.type_keys("9 ] p");
    assert_eq!(h.cursor_display_positions(), vec![(2, 0)]);
}

#[test]
fn match_brackets_jumps_open_to_close() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "(abc)\n");
    h.open_file(&path);
    h.type_keys("m m");
    assert_eq!(h.primary_head_offset(), 4);
}

#[test]
fn match_brackets_jumps_close_to_open() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "(abc)\n");
    h.open_file(&path);
    h.type_keys("4 l m m");
    assert_eq!(h.primary_head_offset(), 0);
}

#[test]
fn match_brackets_handles_nesting() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "((a)(b))\n");
    h.open_file(&path);
    h.type_keys("m m");
    assert_eq!(h.primary_head_offset(), 7);
}

#[test]
fn match_brackets_handles_inner_close_to_inner_open() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "((a)(b))\n");
    h.open_file(&path);
    h.type_keys("3 l m m");
    assert_eq!(h.primary_head_offset(), 1);
}

#[test]
fn match_brackets_supports_brackets_and_braces() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "[x]{y}\n");
    h.open_file(&path);
    h.type_keys("m m");
    assert_eq!(h.primary_head_offset(), 2);
    h.type_keys("l m m");
    assert_eq!(h.primary_head_offset(), 5);
}

/// Every cursor jumps to the partner of its own bracket.
///
/// Resolving one partner from the newest selection and landing it on the
/// rest makes every span identical, and identical spans merge, so the set
/// collapses to a single cursor.
#[test]
fn match_brackets_pairs_each_cursor_separately() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "(a)\n(b)\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &AddSelectionBelow);
    assert_eq!(
        h.selection_spans(),
        vec![(0, 1, false), (4, 5, false)],
        "one cursor on each opening paren",
    );

    h.type_keys("m m");
    assert_eq!(
        h.selection_spans(),
        vec![(2, 3, false), (6, 7, false)],
        "each cursor lands on the closing paren of its own pair",
    );
}

/// A cursor on no bracket holds its place while the others jump.
#[test]
fn match_brackets_keeps_cursors_with_no_pair() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "(a)\nxyz\n");
    h.open_file(&path);
    dispatch(&mut h.stoat, &AddSelectionBelow);

    h.type_keys("m m");
    assert_eq!(
        h.selection_spans(),
        vec![(2, 3, false), (4, 5, false)],
        "row 0 jumps to its closing paren, row 1 has no bracket and stays",
    );
}

/// The same for a language shipping a brackets query, which resolves its
/// partner through the syntax tree rather than the text scan.
#[test]
fn match_brackets_pairs_each_cursor_separately_with_a_query() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.rs", "fn a() {}\nfn b() {}\n");
    h.open_file(&path);
    h.type_keys("4 l");
    dispatch(&mut h.stoat, &AddSelectionBelow);
    assert_eq!(
        h.selection_spans(),
        vec![(4, 5, false), (14, 15, false)],
        "one cursor on each opening paren",
    );

    h.type_keys("m m");
    assert_eq!(
        h.selection_spans(),
        vec![(5, 6, false), (15, 16, false)],
        "each cursor lands on the closing paren of its own pair",
    );
}

#[test]
fn match_brackets_no_op_off_bracket() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "(abc)\n");
    h.open_file(&path);
    h.type_keys("l m m");
    assert_eq!(h.primary_head_offset(), 1);
}

#[test]
fn match_brackets_from_inside_jumps_to_enclosing() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn a() { let x = 1; }\n");
    h.open_file(&path);
    // Cursor at offset 9 (`let`), between the braces but on no delimiter.
    h.type_keys("9 l m m");
    assert_eq!(
        h.primary_head_offset(),
        20,
        "from inside the braces, mm lands on the closing brace"
    );
    // Now on `}`, so mm returns to the opening brace.
    h.type_keys("m m");
    assert_eq!(
        h.primary_head_offset(),
        7,
        "on the closing brace, mm returns to the opening brace"
    );
}

#[test]
fn match_brackets_no_op_unbalanced() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "(abc\n");
    h.open_file(&path);
    h.type_keys("m m");
    assert_eq!(h.primary_head_offset(), 0);
}

#[test]
fn match_brackets_with_multibyte_inside() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "(αβγ)\n");
    h.open_file(&path);
    h.type_keys("m m");
    assert_eq!(h.primary_head_offset(), 7);
}

#[test]
fn match_brackets_skips_brace_in_string() {
    let mut h = TestHarness::with_size(60, 5);
    let path = h.write_file("s.rs", "fn f() { \"}\" ; }\n");
    h.open_file(&path);
    h.type_keys("7 l");
    assert_eq!(h.primary_head_offset(), 7, "cursor on the opening `{{`");
    h.type_keys("m m");
    assert_eq!(
        h.primary_head_offset(),
        15,
        "naive scan would land on the `}}` inside the string at offset 10"
    );
}

#[test]
fn match_brackets_skips_brace_in_block_comment() {
    let mut h = TestHarness::with_size(60, 5);
    let path = h.write_file("s.rs", "fn f() { /* } */ }\n");
    h.open_file(&path);
    h.type_keys("7 l");
    assert_eq!(h.primary_head_offset(), 7, "cursor on the opening `{{`");
    h.type_keys("m m");
    assert_eq!(
        h.primary_head_offset(),
        17,
        "naive scan would land on the `}}` inside the block comment at offset 12"
    );
}

#[test]
fn match_brackets_from_inside_string_jumps_to_quote() {
    let mut h = TestHarness::with_size(60, 5);
    let path = h.write_file("s.rs", "fn f() { let s = \"()\"; }\n");
    h.open_file(&path);
    h.type_keys("1 8 l");
    assert_eq!(
        h.primary_head_offset(),
        18,
        "cursor on the `(` inside the string"
    );
    h.type_keys("m m");
    assert_eq!(
        h.primary_head_offset(),
        20,
        "the string quotes are a captured pair, so mm jumps to the closing quote"
    );
}

#[test]
fn match_brackets_char_literal_paren_resolves_to_enclosing() {
    let mut h = TestHarness::with_size(60, 5);
    let path = h.write_file("s.rs", "fn f() { let c = '('; }\n");
    h.open_file(&path);
    h.type_keys("1 8 l");
    assert_eq!(
        h.primary_head_offset(),
        18,
        "cursor on the `(` inside the char literal"
    );
    h.type_keys("m m");
    assert_eq!(
        h.primary_head_offset(),
        22,
        "the char literal `(` is not a captured delimiter, so mm resolves to the enclosing brace"
    );
}

#[test]
fn match_brackets_scanner_fallback_without_query() {
    let mut h = TestHarness::with_size(60, 5);
    let path = h.write_file("t.toml", "[table]\n");
    h.open_file(&path);
    assert_eq!(h.primary_head_offset(), 0, "cursor starts on the `[`");
    h.type_keys("m m");
    assert_eq!(
        h.primary_head_offset(),
        6,
        "toml has no brackets query, so the scanner matches `[` to `]`"
    );
}

#[test]
fn count_prefix_word_clamps_at_buffer_edge() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    h.type_keys("9 9 w");
    let offset = h.primary_head_offset();
    assert!(
        offset <= 4,
        "huge word count should clamp at buffer end (got {offset})"
    );
}

#[test]
fn count_prefix_clamps_at_end_of_buffer() {
    let mut h = TestHarness::with_size(30, 5);
    let path = h.write_file("s.txt", "abc\n");
    h.open_file(&path);
    h.type_keys("9 9 l");
    let offset = h.primary_head_offset();
    assert!(
        offset <= 4,
        "move_right with huge count should clamp at buffer end (got {offset})"
    );
}

#[test]
fn count_prefix_no_op_when_binding_exists() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "a\nb\nc\nd\n");
    h.open_file(&path);
    h.type_keys("j");
    let positions = h.cursor_display_positions();
    assert_eq!(positions, vec![(1, 0)]);
}

#[test]
fn count_prefix_extends_select_line_below() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "a\nb\nc\nd\ne\n");
    h.open_file(&path);
    h.type_keys("3 x");
    let spans = h.selection_spans();
    assert_eq!(spans.len(), 1);
    let three_lines_len = "a\nb\nc\n".len();
    assert_eq!(
        (spans[0].0, spans[0].1),
        (0, three_lines_len),
        "3x from line 0 should select three lines"
    );
}

#[test]
fn select_mode_x_selects_line_below() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "a\nb\nc\n");
    h.open_file(&path);
    h.type_keys("v x");
    assert_eq!(
        h.selection_spans(),
        vec![(0, 2, false)],
        "vx selects the first line including its newline"
    );
    assert_eq!(h.stoat.focused_mode(), "select", "x stays in select mode");

    h.type_keys("x");
    assert_eq!(
        h.selection_spans(),
        vec![(0, 4, false)],
        "a second x extends to the line below"
    );
}

#[test]
fn count_prefix_extends_already_line_shaped_select_line_below() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "a\nb\nc\nd\ne\n");
    h.open_file(&path);
    h.type_keys("x");
    h.type_keys("2 x");
    let spans = h.selection_spans();
    assert_eq!(spans.len(), 1);
    let three_lines_len = "a\nb\nc\n".len();
    assert_eq!(
        (spans[0].0, spans[0].1),
        (0, three_lines_len),
        "x then 2x should grow to three lines total"
    );
}

#[test]
fn count_prefix_select_line_below_clamps_at_buffer_end() {
    let mut h = TestHarness::with_size(20, 10);
    let path = h.write_file("s.txt", "a\nb\n");
    h.open_file(&path);
    h.type_keys("9 9 x");
    let spans = h.selection_spans();
    assert_eq!(spans.len(), 1);
    let buffer_len = "a\nb\n".len();
    assert_eq!(
        (spans[0].0, spans[0].1),
        (0, buffer_len),
        "huge count should clamp at buffer end"
    );
}

#[test]
fn save_selection_truncates_forward_history() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.txt", "abcdefghij\n");
    h.open_file(&path);
    h.type_keys("l");
    dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    h.type_keys("l l");
    dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    dispatch(&mut h.stoat, &stoat_action::JumpBackward);
    dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    let after_save = h.primary_head_offset();
    dispatch(&mut h.stoat, &stoat_action::JumpForward);
    assert_eq!(
        h.primary_head_offset(),
        after_save,
        "JumpForward after a fresh save should be a no-op (forward history was truncated)"
    );
}

#[test]
fn move_to_parent_bound_no_op_without_syntax_map() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.txt", "alpha beta gamma\n");
    h.open_file(&path);
    let before = h.selection_spans();
    dispatch(&mut h.stoat, &stoat_action::MoveParentNodeStart);
    assert_eq!(h.selection_spans(), before);
}

#[test]
fn shrink_after_cursor_move_does_not_restore_old_chain() {
    let mut h = TestHarness::with_size(40, 5);
    let path = h.write_file("s.rs", "fn main() {}\n");
    h.open_file(&path);
    h.type_keys("l l l");
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    h.type_keys("l l");
    let after_move = h.selection_spans();
    dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
    assert_ne!(h.selection_spans(), after_move);
    dispatch(&mut h.stoat, &stoat_action::ShrinkSelection);
    assert_eq!(h.selection_spans(), after_move);
    dispatch(&mut h.stoat, &stoat_action::ShrinkSelection);
    assert_eq!(h.selection_spans(), after_move);
}

#[test]
fn goto_next_change_no_op_without_diff_map() {
    let mut h = TestHarness::with_size(20, 5);
    let path = h.write_file("s.txt", "a\nb\nc\n");
    h.open_file(&path);
    let before = h.primary_head_offset();
    dispatch(&mut h.stoat, &stoat_action::GotoNextChange);
    assert_eq!(h.primary_head_offset(), before);
}

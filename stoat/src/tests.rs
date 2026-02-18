//! Integration tests for Stoat v4.
//!
//! These tests validate the GPUI Entity pattern and demonstrate proper usage
//! of `Context<Self>` for editor operations.

use crate::Stoat;
use gpui::TestAppContext;
use text::Point;

// ===== Editing Tests =====

#[gpui::test]
fn insert_text_in_insert_mode(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("", cx);

    stoat.update(|s, cx| {
        s.enter_insert_mode(cx);
        s.insert_text("hello", cx);
    });

    assert_eq!(stoat.buffer_text(), "hello");
    assert_eq!(stoat.cursor_position(), Point::new(0, 5));
}

#[gpui::test]
fn insert_multiple_lines(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("", cx);

    stoat.update(|s, cx| {
        s.enter_insert_mode(cx);
        s.insert_text("first", cx);
        s.new_line(cx);
        s.insert_text("second", cx);
    });

    assert_eq!(stoat.buffer_text(), "first\nsecond");
    assert_eq!(stoat.cursor_position(), Point::new(1, 6));
}

#[gpui::test]
fn delete_left_removes_character(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        // Move to end of text
        s.set_cursor_position(Point::new(0, 5));
        s.delete_left(cx);
    });

    assert_eq!(stoat.buffer_text(), "hell");
    assert_eq!(stoat.cursor_position(), Point::new(0, 4));
}

#[gpui::test]
fn delete_left_at_line_start_is_noop(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 0));
        s.delete_left(cx);
    });

    assert_eq!(stoat.buffer_text(), "hello");
    assert_eq!(stoat.cursor_position(), Point::new(0, 0));
}

#[gpui::test]
fn delete_right_removes_character(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 0));
        s.delete_right(cx);
    });

    assert_eq!(stoat.buffer_text(), "ello");
    assert_eq!(stoat.cursor_position(), Point::new(0, 0));
}

#[gpui::test]
fn delete_right_at_line_end_merges_lines(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello\nworld", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 5));
        s.delete_right(cx);
    });

    assert_eq!(stoat.buffer_text(), "helloworld");
    assert_eq!(stoat.cursor_position(), Point::new(0, 5));
}

#[gpui::test]
fn delete_left_handles_multibyte_utf8_characters(cx: &mut TestAppContext) {
    // Test 2-byte character (non-breaking space U+00A0)
    // "test" = 4 bytes, "\u{00A0}" = 2 bytes, total = 6 bytes
    let mut stoat = Stoat::test_with_text("test\u{00A0}", cx);
    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 6)); // After all bytes
        s.delete_left(cx);
    });
    assert_eq!(stoat.buffer_text(), "test");
    assert_eq!(stoat.cursor_position(), Point::new(0, 4)); // Cursor at byte 4

    // Test 2-byte character (Latin accented)
    // "caf" = 3 bytes, "é" = 2 bytes, total = 5 bytes
    let mut stoat = Stoat::test_with_text("café", cx);
    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 5)); // After all bytes
        s.delete_left(cx);
    });
    assert_eq!(stoat.buffer_text(), "caf");
    assert_eq!(stoat.cursor_position(), Point::new(0, 3)); // Cursor at byte 3

    // Test 3-byte character (Chinese)
    // "hello" = 5 bytes, "中" = 3 bytes, total = 8 bytes
    let mut stoat = Stoat::test_with_text("hello中", cx);
    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 8)); // After all bytes
        s.delete_left(cx);
    });
    assert_eq!(stoat.buffer_text(), "hello");
    assert_eq!(stoat.cursor_position(), Point::new(0, 5)); // Cursor at byte 5

    // Test 4-byte character (mathematical symbol U+1D400)
    // "test" = 4 bytes, "\u{1D400}" = 4 bytes, total = 8 bytes
    let mut stoat = Stoat::test_with_text("test\u{1D400}", cx);
    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 8)); // After all bytes
        s.delete_left(cx);
    });
    assert_eq!(stoat.buffer_text(), "test");
    assert_eq!(stoat.cursor_position(), Point::new(0, 4)); // Cursor at byte 4
}

// ===== Movement Tests =====

#[gpui::test]
fn move_left_decreases_column(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 3));
        s.move_left(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(0, 2));
}

#[gpui::test]
fn move_left_at_line_start_is_noop(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 0));
        s.move_left(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(0, 0));
}

#[gpui::test]
fn move_right_increases_column(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 0));
        s.move_right(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(0, 1));
}

#[gpui::test]
fn move_right_at_line_end_is_noop(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 5));
        s.move_right(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(0, 5));
}

#[gpui::test]
fn move_left_handles_multibyte_utf8_characters(cx: &mut TestAppContext) {
    // Test 2-byte character (non-breaking space U+00A0)
    // "test" = 4 bytes, "\u{00A0}" = 2 bytes, total = 6 bytes
    let mut stoat = Stoat::test_with_text("test\u{00A0}", cx);
    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 6)); // After all bytes
        s.move_left(cx);
    });
    assert_eq!(stoat.cursor_position(), Point::new(0, 4)); // Should move to before the 2-byte char

    // Test 3-byte character (Chinese)
    let mut stoat = Stoat::test_with_text("hello中", cx);
    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 8)); // After 3-byte char
        s.move_left(cx);
    });
    assert_eq!(stoat.cursor_position(), Point::new(0, 5)); // Should move to before the 3-byte char
}

#[gpui::test]
fn move_right_handles_multibyte_utf8_characters(cx: &mut TestAppContext) {
    // Test 2-byte character (non-breaking space U+00A0)
    let mut stoat = Stoat::test_with_text("test\u{00A0}", cx);
    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 4)); // Before 2-byte char
        s.move_right(cx);
    });
    assert_eq!(stoat.cursor_position(), Point::new(0, 6)); // Should move to after the 2-byte char

    // Test 3-byte character (Chinese)
    let mut stoat = Stoat::test_with_text("hello中", cx);
    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 5)); // Before 3-byte char
        s.move_right(cx);
    });
    assert_eq!(stoat.cursor_position(), Point::new(0, 8)); // Should move to after the 3-byte char
}

#[gpui::test]
fn move_up_decreases_row(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("line1\nline2\nline3", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(2, 0));
        s.move_up(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(1, 0));
}

#[gpui::test]
fn move_up_at_first_line_is_noop(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("line1\nline2", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 0));
        s.move_up(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(0, 0));
}

#[gpui::test]
fn move_down_increases_row(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("line1\nline2\nline3", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 0));
        s.move_down(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(1, 0));
}

#[gpui::test]
fn move_down_at_last_line_is_noop(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("line1\nline2", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(1, 0));
        s.move_down(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(1, 0));
}

#[gpui::test]
fn move_to_line_start(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello world", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 5));
        s.move_to_line_start(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(0, 0));
}

#[gpui::test]
fn move_to_line_end(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello world", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 0));
        s.move_to_line_end(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(0, 11));
}

// ===== Mode Tests =====

#[gpui::test]
fn mode_switching_normal_to_insert(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test(cx);

    assert_eq!(stoat.mode(), "normal");

    stoat.update(|s, cx| {
        s.enter_insert_mode(cx);
    });

    assert_eq!(stoat.mode(), "insert");
}

#[gpui::test]
fn mode_switching_insert_to_normal(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test(cx);

    stoat.update(|s, cx| {
        s.enter_insert_mode(cx);
        s.enter_normal_mode(cx);
    });

    assert_eq!(stoat.mode(), "normal");
}

#[gpui::test]
fn mode_switching_to_space_mode(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test(cx);

    assert_eq!(stoat.mode(), "normal");

    stoat.update(|s, cx| {
        s.enter_space_mode(cx);
    });

    assert_eq!(stoat.mode(), "space");
}

#[gpui::test]
fn mode_switching_to_pane_mode(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test(cx);

    assert_eq!(stoat.mode(), "normal");

    stoat.update(|s, cx| {
        s.enter_pane_mode(cx);
    });

    assert_eq!(stoat.mode(), "pane");
}

// ===== Entity Pattern Tests =====

#[gpui::test]
fn entity_pattern_enables_self_updating(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("", cx);

    // The Context<Self> pattern allows Stoat methods to:
    // 1. Access their own entity handle
    // 2. Spawn self-updating async tasks
    // 3. Update internal state

    stoat.update(|s, cx| {
        s.enter_insert_mode(cx);
        s.insert_text("test", cx);
    });

    // Verify state updated correctly
    assert_eq!(stoat.buffer_text(), "test");
    assert_eq!(stoat.mode(), "insert");
}

#[gpui::test]
fn multiple_updates_in_sequence(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("", cx);

    // Multiple sequential updates should work
    stoat.update(|s, cx| {
        s.enter_insert_mode(cx);
    });

    stoat.update(|s, cx| {
        s.insert_text("hello", cx);
    });

    stoat.update(|s, cx| {
        s.new_line(cx);
        s.insert_text("world", cx);
    });

    stoat.update(|s, cx| {
        s.enter_normal_mode(cx);
    });

    assert_eq!(stoat.buffer_text(), "hello\nworld");
    assert_eq!(stoat.mode(), "normal");
}

#[gpui::test]
fn read_without_update(cx: &mut TestAppContext) {
    let stoat = Stoat::test_with_text("test", cx);

    // Can read state without updating - using helper methods
    let text = stoat.buffer_text();
    assert_eq!(text, "test");

    let mode = stoat.mode();
    assert_eq!(mode, "normal");

    let cursor = stoat.cursor_position();
    assert_eq!(cursor, Point::new(0, 0));
}

// ===== Word Motion Tests =====

#[gpui::test]
fn move_next_word_start_basic(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("fn foo() -> Result<()>", cx);

    stoat.update(|s, cx| {
        s.move_next_word_start(cx);
    });

    let selection = stoat.selection();
    assert_eq!(selection.start, Point::new(0, 0));
    assert_eq!(selection.end, Point::new(0, 3));
}

#[gpui::test]
fn move_next_word_start_skips_punctuation(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("fn foo() -> Result<()>", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 7));
        s.move_next_word_start(cx);
    });

    let selection = stoat.selection();
    assert_eq!(selection.start, Point::new(0, 7));
    assert_eq!(selection.end, Point::new(0, 9));
}

#[gpui::test]
fn move_prev_word_start_basic(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("fn foo() -> Result<()>", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 22));
        s.move_prev_word_start(cx);
    });

    let selection = stoat.selection();
    assert_eq!(selection.start, Point::new(0, 18));
    assert_eq!(selection.end, Point::new(0, 22));
    assert!(selection.reversed);
}

#[gpui::test]
fn move_prev_word_start_mid_token(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo identifier", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 8));
        s.move_prev_word_start(cx);
    });

    let selection = stoat.selection();
    assert_eq!(selection.start, Point::new(0, 4));
    assert_eq!(selection.end, Point::new(0, 8));
    assert!(selection.reversed);
}

#[gpui::test]
fn move_next_long_word_start_includes_punctuation(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo.bar", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 3));
        s.move_next_long_word_start(cx);
    });

    let selection = stoat.selection();
    assert_eq!(selection.start, Point::new(0, 3));
    assert_eq!(selection.end, Point::new(0, 7));
}

#[gpui::test]
fn move_next_long_word_start_operator(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("x + y", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 2));
        s.move_next_long_word_start(cx);
    });

    let selection = stoat.selection();
    assert_eq!(selection.start, Point::new(0, 2));
    assert_eq!(selection.end, Point::new(0, 4));
}

#[gpui::test]
fn move_prev_long_word_start_punctuation(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo.bar", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 4));
        s.move_prev_long_word_start(cx);
    });

    let selection = stoat.selection();
    assert_eq!(selection.start, Point::new(0, 0));
    assert_eq!(selection.end, Point::new(0, 4));
    assert!(selection.reversed);
}

#[gpui::test]
fn move_prev_long_word_start_brackets(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo()", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 5));
        s.move_prev_long_word_start(cx);
    });

    let selection = stoat.selection();
    assert_eq!(selection.start, Point::new(0, 0));
    assert_eq!(selection.end, Point::new(0, 5));
    assert!(selection.reversed);
}

#[gpui::test]
fn move_next_word_start_skip_whitespace(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("x   42", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 1));
        s.move_next_word_start(cx);
    });

    let selection = stoat.selection();
    assert_eq!(selection.start, Point::new(0, 1));
    assert_eq!(selection.end, Point::new(0, 4));
}

#[gpui::test]
fn move_next_word_start_skip_newlines(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("x\n\n  foo", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 1));
        s.move_next_word_start(cx);
    });

    let selection = stoat.selection();
    assert_eq!(selection.start, Point::new(0, 1));
    assert_eq!(selection.end, Point::new(2, 2));
}

// ===== File Navigation Tests =====

#[gpui::test]
fn move_to_file_start_from_middle(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo\nbar\nbaz", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(1, 2));
        s.move_to_file_start(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(0, 0));
}

#[gpui::test]
fn move_to_file_start_from_end(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo\nbar\nbaz", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(2, 3));
        s.move_to_file_start(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(0, 0));
}

#[gpui::test]
fn move_to_file_start_already_at_start(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo\nbar", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 0));
        s.move_to_file_start(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(0, 0));
}

#[gpui::test]
fn move_to_file_end_from_middle(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo\nbar\nbaz", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(1, 1));
        s.move_to_file_end(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(2, 3));
}

#[gpui::test]
fn move_to_file_end_from_start(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo\nbar\nbaz", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 0));
        s.move_to_file_end(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(2, 3));
}

#[gpui::test]
fn move_to_file_end_already_at_end(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo\nbar", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(1, 3));
        s.move_to_file_end(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(1, 3));
}

#[gpui::test]
fn move_to_file_end_single_line(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 2));
        s.move_to_file_end(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(0, 5));
}

#[gpui::test]
fn page_up_basic(cx: &mut TestAppContext) {
    // Create 40 lines of text
    let lines: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
    let mut stoat = Stoat::test_with_text(&lines.join("\n"), cx);

    stoat.update(|s, cx| {
        // Set viewport to 10 lines
        s.set_viewport_lines(10.0);
        // Start at line 20
        s.set_cursor_position(Point::new(20, 0));
        s.page_up(cx);
    });

    // Should move up by 10 lines
    assert_eq!(stoat.cursor_position(), Point::new(10, 0));
}

#[gpui::test]
fn page_up_near_start(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("line 0\nline 1\nline 2\nline 3\nline 4", cx);

    stoat.update(|s, cx| {
        s.set_viewport_lines(10.0);
        s.set_cursor_position(Point::new(2, 0));
        s.page_up(cx);
    });

    // Should clamp to start
    assert_eq!(stoat.cursor_position(), Point::new(0, 0));
}

#[gpui::test]
fn page_up_at_start(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("line 0\nline 1\nline 2", cx);

    stoat.update(|s, cx| {
        s.set_viewport_lines(10.0);
        s.set_cursor_position(Point::new(0, 0));
        s.page_up(cx);
    });

    // Should stay at start
    assert_eq!(stoat.cursor_position(), Point::new(0, 0));
}

#[gpui::test]
fn page_down_basic(cx: &mut TestAppContext) {
    // Create 40 lines of text
    let lines: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
    let mut stoat = Stoat::test_with_text(&lines.join("\n"), cx);

    stoat.update(|s, cx| {
        // Set viewport to 10 lines
        s.set_viewport_lines(10.0);
        // Start at line 5
        s.set_cursor_position(Point::new(5, 0));
        s.page_down(cx);
    });

    // Should move down by 10 lines
    assert_eq!(stoat.cursor_position(), Point::new(15, 0));
}

#[gpui::test]
fn page_down_near_end(cx: &mut TestAppContext) {
    let lines: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();
    let mut stoat = Stoat::test_with_text(&lines.join("\n"), cx);

    stoat.update(|s, cx| {
        s.set_viewport_lines(10.0);
        s.set_cursor_position(Point::new(15, 0));
        s.page_down(cx);
    });

    // Should clamp to last line (line 19)
    assert_eq!(stoat.cursor_position(), Point::new(19, 0));
}

#[gpui::test]
fn page_down_at_end(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("line 0\nline 1\nline 2", cx);

    stoat.update(|s, cx| {
        s.set_viewport_lines(10.0);
        s.set_cursor_position(Point::new(2, 0));
        s.page_down(cx);
    });

    // Should stay at end
    assert_eq!(stoat.cursor_position(), Point::new(2, 0));
}

// ==== DeleteLine tests ====

#[gpui::test]
fn delete_line_middle(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo\nbar\nbaz", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(1, 1));
        s.delete_line(cx);
    });

    assert_eq!(stoat.buffer_text(), "foo\nbaz");
    assert_eq!(stoat.cursor_position(), Point::new(1, 0));
}

#[gpui::test]
fn delete_line_first(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo\nbar\nbaz", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 2));
        s.delete_line(cx);
    });

    assert_eq!(stoat.buffer_text(), "bar\nbaz");
    assert_eq!(stoat.cursor_position(), Point::new(0, 0));
}

#[gpui::test]
fn delete_line_last(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo\nbar\nbaz", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(2, 2));
        s.delete_line(cx);
    });

    assert_eq!(stoat.buffer_text(), "foo\nbar\n");
    assert_eq!(stoat.cursor_position(), Point::new(2, 0));
}

#[gpui::test]
fn delete_line_single_line(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello world", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 5));
        s.delete_line(cx);
    });

    assert_eq!(stoat.buffer_text(), "");
    assert_eq!(stoat.cursor_position(), Point::new(0, 0));
}

#[gpui::test]
fn delete_line_empty(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo\n\nbar", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(1, 0));
        s.delete_line(cx);
    });

    assert_eq!(stoat.buffer_text(), "foo\nbar");
    assert_eq!(stoat.cursor_position(), Point::new(1, 0));
}

// ==== DeleteToEndOfLine tests ====

#[gpui::test]
fn delete_to_end_from_middle(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello world", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 6));
        s.delete_to_end_of_line(cx);
    });

    assert_eq!(stoat.buffer_text(), "hello ");
    assert_eq!(stoat.cursor_position(), Point::new(0, 6));
}

#[gpui::test]
fn delete_to_end_from_start(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello world", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 0));
        s.delete_to_end_of_line(cx);
    });

    assert_eq!(stoat.buffer_text(), "");
    assert_eq!(stoat.cursor_position(), Point::new(0, 0));
}

#[gpui::test]
fn delete_to_end_at_end(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello world", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 11));
        s.delete_to_end_of_line(cx);
    });

    assert_eq!(stoat.buffer_text(), "hello world");
    assert_eq!(stoat.cursor_position(), Point::new(0, 11));
}

#[gpui::test]
fn delete_to_end_multiline(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo\nbar\nbaz", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(1, 1));
        s.delete_to_end_of_line(cx);
    });

    assert_eq!(stoat.buffer_text(), "foo\nb\nbaz");
    assert_eq!(stoat.cursor_position(), Point::new(1, 1));
}

#[gpui::test]
fn delete_to_end_preserves_newline(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello\nworld", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 2));
        s.delete_to_end_of_line(cx);
    });

    assert_eq!(stoat.buffer_text(), "he\nworld");
    assert_eq!(stoat.cursor_position(), Point::new(0, 2));
}

// ==== Visual Mode tests ====

#[gpui::test]
fn enter_visual_from_normal(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        assert_eq!(s.mode(), "normal");
        s.enter_visual_mode(cx);
    });

    assert_eq!(stoat.mode(), "visual");
}

#[gpui::test]
fn escape_exits_visual_mode(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        s.enter_visual_mode(cx);
        assert_eq!(s.mode(), "visual");
        s.enter_normal_mode(cx);
    });

    assert_eq!(stoat.mode(), "normal");
}

#[gpui::test]
fn select_left_extends_selection(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 3));
        s.enter_visual_mode(cx);
        s.select_left(cx);
    });

    let selection = stoat.selection();
    assert!(!selection.is_empty());
    assert_eq!(selection.start, Point::new(0, 2));
    assert_eq!(selection.end, Point::new(0, 3));
}

#[gpui::test]
fn select_right_extends_selection(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 2));
        s.enter_visual_mode(cx);
        s.select_right(cx);
    });

    let selection = stoat.selection();
    assert!(!selection.is_empty());
    assert_eq!(selection.start, Point::new(0, 2));
    assert_eq!(selection.end, Point::new(0, 3));
}

#[gpui::test]
fn select_up_extends_selection(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo\nbar\nbaz", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(1, 1));
        s.enter_visual_mode(cx);
        s.select_up(cx);
    });

    let selection = stoat.selection();
    assert!(!selection.is_empty());
    assert_eq!(selection.start, Point::new(0, 1));
    assert_eq!(selection.end, Point::new(1, 1));
}

#[gpui::test]
fn select_down_extends_selection(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo\nbar\nbaz", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(1, 1));
        s.enter_visual_mode(cx);
        s.select_down(cx);
    });

    let selection = stoat.selection();
    assert!(!selection.is_empty());
    assert_eq!(selection.start, Point::new(1, 1));
    assert_eq!(selection.end, Point::new(2, 1));
}

#[gpui::test]
fn select_to_line_start_extends_selection(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello world", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 6));
        s.enter_visual_mode(cx);
        s.select_to_line_start(cx);
    });

    let selection = stoat.selection();
    assert!(!selection.is_empty());
    assert_eq!(selection.start, Point::new(0, 0));
    assert_eq!(selection.end, Point::new(0, 6));
}

#[gpui::test]
fn select_to_line_end_extends_selection(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello world", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 5));
        s.enter_visual_mode(cx);
        s.select_to_line_end(cx);
    });

    let selection = stoat.selection();
    assert!(!selection.is_empty());
    assert_eq!(selection.start, Point::new(0, 5));
    assert_eq!(selection.end, Point::new(0, 11));
}

#[gpui::test]
fn select_left_at_line_start_is_noop(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 0));
        s.enter_visual_mode(cx);
        s.select_left(cx);
    });

    let selection = stoat.selection();
    assert!(selection.is_empty());
    assert_eq!(stoat.cursor_position(), Point::new(0, 0));
}

#[gpui::test]
fn select_right_at_line_end_is_noop(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 5));
        s.enter_visual_mode(cx);
        s.select_right(cx);
    });

    let selection = stoat.selection();
    assert!(selection.is_empty());
    assert_eq!(stoat.cursor_position(), Point::new(0, 5));
}

// ===== Word Movement Tests =====

#[gpui::test]
fn move_word_left_to_previous_symbol(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello world", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 11)); // End of "world"
        s.move_word_left(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(0, 6)); // Start of "world"
}

#[gpui::test]
fn move_word_left_skips_whitespace(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo   bar", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 9)); // End of "bar"
        s.move_word_left(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(0, 6)); // Start of "bar"
}

#[gpui::test]
fn move_word_left_mid_symbol(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 3)); // Mid "hello"
        s.move_word_left(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(0, 0)); // Start of "hello"
}

#[gpui::test]
fn move_word_left_no_previous_symbol(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 0));
        s.move_word_left(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(0, 0)); // No change
}

#[gpui::test]
fn move_word_right_to_next_symbol(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello world", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 0)); // Start of "hello"
        s.move_word_right(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(0, 5)); // End of "hello"
}

#[gpui::test]
fn move_word_right_skips_whitespace(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo   bar", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 3)); // End of "foo"
        s.move_word_right(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(0, 9)); // End of "bar"
}

#[gpui::test]
fn move_word_right_mid_symbol(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 2)); // Mid "hello"
        s.move_word_right(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(0, 5)); // End of "hello"
}

#[gpui::test]
fn move_word_right_no_next_symbol(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 5));
        s.move_word_right(cx);
    });

    assert_eq!(stoat.cursor_position(), Point::new(0, 5)); // No change
}

// ===== Word Deletion Tests =====

#[gpui::test]
fn delete_word_left_removes_symbol(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello world", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 11)); // End of "world"
        s.delete_word_left(cx);
    });

    assert_eq!(stoat.buffer_text(), "hello ");
    assert_eq!(stoat.cursor_position(), Point::new(0, 6));
}

#[gpui::test]
fn delete_word_left_with_whitespace(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo   bar", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 9)); // End of "bar"
        s.delete_word_left(cx);
    });

    assert_eq!(stoat.buffer_text(), "foo   ");
    assert_eq!(stoat.cursor_position(), Point::new(0, 6));
}

#[gpui::test]
fn delete_word_left_mid_symbol(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 3)); // Mid "hello"
        s.delete_word_left(cx);
    });

    assert_eq!(stoat.buffer_text(), "lo");
    assert_eq!(stoat.cursor_position(), Point::new(0, 0));
}

#[gpui::test]
fn delete_word_left_no_previous_symbol(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 0));
        s.delete_word_left(cx);
    });

    assert_eq!(stoat.buffer_text(), "hello"); // No change
    assert_eq!(stoat.cursor_position(), Point::new(0, 0));
}

#[gpui::test]
fn delete_word_right_removes_symbol(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello world", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 0)); // Start of "hello"
        s.delete_word_right(cx);
    });

    assert_eq!(stoat.buffer_text(), " world");
    assert_eq!(stoat.cursor_position(), Point::new(0, 0));
}

#[gpui::test]
fn delete_word_right_with_whitespace(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo   bar", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 3)); // End of "foo"
        s.delete_word_right(cx);
    });

    assert_eq!(stoat.buffer_text(), "foo");
    assert_eq!(stoat.cursor_position(), Point::new(0, 3));
}

#[gpui::test]
fn delete_word_right_mid_symbol(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 2)); // Mid "hello"
        s.delete_word_right(cx);
    });

    assert_eq!(stoat.buffer_text(), "he");
    assert_eq!(stoat.cursor_position(), Point::new(0, 2));
}

#[gpui::test]
fn delete_word_right_no_next_symbol(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("hello", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 5));
        s.delete_word_right(cx);
    });

    assert_eq!(stoat.buffer_text(), "hello"); // No change
    assert_eq!(stoat.cursor_position(), Point::new(0, 5));
}

// ===== Table-Driven Tests (Using Cursor Notation DSL) =====

/// Table-driven test for basic movement operations.
///
/// This demonstrates how cursor notation enables concise, data-driven tests.
/// Each test case is a tuple of (input_state, expected_output, description).
#[gpui::test]
fn table_driven_move_left(cx: &mut TestAppContext) {
    let cases = [
        ("hel|lo", "he|llo", "basic move left"),
        ("|hello", "|hello", "at line start (noop)"),
        ("café|", "caf|é", "before multibyte char"),
        ("test\u{00A0}|", "test|\u{00A0}", "2-byte UTF-8 char"),
    ];

    for (input, expected, description) in cases {
        let mut stoat = Stoat::test_with_cursor_notation(input, cx)
            .unwrap_or_else(|e| panic!("Failed to parse input '{input}': {e}"));

        stoat.update(|s, cx| {
            s.move_left(cx);
        });

        stoat.assert_cursor_notation(expected);
        // Assertion message includes description for clarity
        assert_eq!(
            stoat.to_cursor_notation(),
            expected,
            "Failed: {description} (input: '{input}')"
        );
    }
}

/// Table-driven test for basic movement operations - right direction.
#[gpui::test]
fn table_driven_move_right(cx: &mut TestAppContext) {
    let cases = [
        ("|hello", "h|ello", "basic move right"),
        ("hello|", "hello|", "at line end (noop)"),
        ("test|", "test|", "single line at end"),
        (
            "|test\u{00A0}",
            "t|est\u{00A0}",
            "before multibyte sequence",
        ),
    ];

    for (input, expected, description) in cases {
        let mut stoat = Stoat::test_with_cursor_notation(input, cx)
            .unwrap_or_else(|e| panic!("Failed to parse input '{input}': {e}"));

        stoat.update(|s, cx| {
            s.move_right(cx);
        });

        assert_eq!(
            stoat.to_cursor_notation(),
            expected,
            "Failed: {description} (input: '{input}')"
        );
    }
}

/// Table-driven test for delete operations.
#[gpui::test]
fn table_driven_delete_left(cx: &mut TestAppContext) {
    let cases = [
        ("hello|", "hell|", "delete last char"),
        ("|hello", "|hello", "at start (noop)"),
        ("hel|lo", "he|lo", "delete middle char"),
        ("test\u{00A0}|", "test|", "delete 2-byte UTF-8"),
        ("hello中|", "hello|", "delete 3-byte UTF-8"),
    ];

    for (input, expected, description) in cases {
        let mut stoat = Stoat::test_with_cursor_notation(input, cx)
            .unwrap_or_else(|e| panic!("Failed to parse input '{input}': {e}"));

        stoat.update(|s, cx| {
            s.delete_left(cx);
        });

        assert_eq!(
            stoat.to_cursor_notation(),
            expected,
            "Failed: {description} (input: '{input}')"
        );
    }
}

/// Table-driven test demonstrating selection operations with cursor notation.
#[gpui::test]
fn table_driven_selections(cx: &mut TestAppContext) {
    let cases = [
        ("<|hello||>", "hello", "selection cursor at end"),
        ("<||hello|>", "hello", "selection cursor at start"),
        ("<|foo||> bar", "foo bar", "partial selection"),
        ("x <|test||> y", "x test y", "mid-line selection"),
    ];

    for (input, expected_text, description) in cases {
        let stoat = Stoat::test_with_cursor_notation(input, cx)
            .unwrap_or_else(|e| panic!("Failed to parse input '{input}': {e}"));

        assert_eq!(
            stoat.buffer_text(),
            expected_text,
            "Text mismatch for: {description}"
        );

        // Verify selection exists
        let selection = stoat.selection();
        assert!(
            !selection.is_empty(),
            "Selection should not be empty for: {description}"
        );
    }
}

// ===== Git Integration Tests =====

#[gpui::test]
fn test_with_git_repo(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test(cx).init_git();

    // Verify git repository was initialized by checking .git directory exists
    stoat.update(|s, _cx| {
        // For now, just verify the test setup works
        // Future: Add file writing and git operations here
        assert_eq!(s.mode(), "normal");
    });
}

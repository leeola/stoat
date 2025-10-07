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

// ===== Selection Tests =====

#[gpui::test]
fn select_next_symbol_basic(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("fn foo() -> Result<()>", cx);

    stoat.update(|s, cx| {
        s.select_next_symbol(cx);
    });

    // When cursor is at (0,0), which is the start of "fn", select_next_symbol
    // skips "fn" and selects "foo" - this matches vim `w` behavior
    let selection = stoat.selection();
    assert_eq!(selection.start, Point::new(0, 3));
    assert_eq!(selection.end, Point::new(0, 6));
}

#[gpui::test]
fn select_next_symbol_skips_punctuation(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("fn foo() -> Result<()>", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 7)); // After "fn foo("
        s.select_next_symbol(cx);
    });

    // Should skip ")" and select "Result"
    let selection = stoat.selection();
    assert_eq!(selection.start, Point::new(0, 12));
    assert_eq!(selection.end, Point::new(0, 18));
}

#[gpui::test]
fn select_prev_symbol_basic(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("fn foo() -> Result<()>", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 22)); // At end
        s.select_prev_symbol(cx);
    });

    // Should select "Result" with cursor on left (reversed)
    let selection = stoat.selection();
    assert_eq!(selection.start, Point::new(0, 12));
    assert_eq!(selection.end, Point::new(0, 18));
    assert!(selection.reversed);
}

#[gpui::test]
fn select_prev_symbol_mid_token(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo identifier", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 8)); // Middle of "identifier"
        s.select_prev_symbol(cx);
    });

    // Should select from start of "identifier" to cursor position
    let selection = stoat.selection();
    assert_eq!(selection.start, Point::new(0, 4));
    assert_eq!(selection.end, Point::new(0, 8));
    assert!(selection.reversed);
}

#[gpui::test]
fn select_next_token_includes_punctuation(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo.bar", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 3)); // At the dot
        s.select_next_token(cx);
    });

    // Should select the dot token
    let selection = stoat.selection();
    assert_eq!(selection.start, Point::new(0, 3));
    assert_eq!(selection.end, Point::new(0, 4));
}

#[gpui::test]
fn select_next_token_operator(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("x + y", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 2)); // At the plus
        s.select_next_token(cx);
    });

    // Should select the plus token
    let selection = stoat.selection();
    assert_eq!(selection.start, Point::new(0, 2));
    assert_eq!(selection.end, Point::new(0, 3));
}

#[gpui::test]
fn select_prev_token_punctuation(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo.bar", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 4)); // After the dot (at "b")
        s.select_prev_token(cx);
    });

    // Should select the dot token (reversed)
    let selection = stoat.selection();
    assert_eq!(selection.start, Point::new(0, 3));
    assert_eq!(selection.end, Point::new(0, 4));
    assert!(selection.reversed);
}

#[gpui::test]
fn select_prev_token_brackets(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("foo()", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 5)); // After "foo()"
        s.select_prev_token(cx);
    });

    // Should select closing paren (reversed)
    let selection = stoat.selection();
    assert_eq!(selection.start, Point::new(0, 4));
    assert_eq!(selection.end, Point::new(0, 5));
    assert!(selection.reversed);
}

#[gpui::test]
fn select_symbols_skip_whitespace(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("x   42", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 1)); // After "x"
        s.select_next_symbol(cx);
    });

    // Should skip spaces and select "42"
    let selection = stoat.selection();
    assert_eq!(selection.start, Point::new(0, 4));
    assert_eq!(selection.end, Point::new(0, 6));
}

#[gpui::test]
fn select_symbols_skip_newlines(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test_with_text("x\n\n  foo", cx);

    stoat.update(|s, cx| {
        s.set_cursor_position(Point::new(0, 1)); // After "x"
        s.select_next_symbol(cx);
    });

    // Should skip newlines/spaces and select "foo"
    let selection = stoat.selection();
    assert_eq!(selection.start, Point::new(2, 2));
    assert_eq!(selection.end, Point::new(2, 5));
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
    let lines: Vec<String> = (0..40).map(|i| format!("line {}", i)).collect();
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
    let lines: Vec<String> = (0..40).map(|i| format!("line {}", i)).collect();
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
    let lines: Vec<String> = (0..20).map(|i| format!("line {}", i)).collect();
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

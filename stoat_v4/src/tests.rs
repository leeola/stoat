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

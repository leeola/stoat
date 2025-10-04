//! Test utilities for Stoat editor

pub mod cursor_notation;

use crate::{actions::*, Stoat};
use gpui::{
    div, App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, Pixels, Render,
    Size, Styled, TestAppContext, Window,
};
use text::Point;

/// Default line height in pixels for test calculations
const DEFAULT_LINE_HEIGHT: f32 = 20.0;

/// Test-only view wrapper for Stoat that implements Render and registers action handlers.
///
/// This wrapper enables Stoat to be used in GPUI's windowed test environment,
/// allowing keystroke simulation through the full action dispatch pipeline.
struct StoatView {
    stoat: Stoat,
    focus_handle: FocusHandle,
}

impl StoatView {
    fn new(stoat: Stoat, cx: &mut Context<Self>) -> Self {
        Self {
            stoat,
            focus_handle: cx.focus_handle(),
        }
    }

    fn stoat(&self) -> &Stoat {
        &self.stoat
    }

    fn stoat_mut(&mut self) -> &mut Stoat {
        &mut self.stoat
    }
}

impl Focusable for StoatView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for StoatView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mode_str = self.stoat.mode().to_string();

        div()
            .id("stoat-test-view")
            .key_context({
                let mut ctx = gpui::KeyContext::new_with_defaults();
                ctx.add("Editor");
                ctx.set("mode", mode_str);
                ctx
            })
            .track_focus(&self.focus_handle)
            .size_full()
            // Movement actions
            .on_action(cx.listener(|view: &mut Self, _: &MoveLeft, _, cx| {
                view.stoat.move_cursor_left(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|view: &mut Self, _: &MoveRight, _, cx| {
                view.stoat.move_cursor_right(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|view: &mut Self, _: &MoveUp, _, cx| {
                view.stoat.move_cursor_up(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|view: &mut Self, _: &MoveDown, _, cx| {
                view.stoat.move_cursor_down(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|view: &mut Self, _: &MoveToLineStart, _, cx| {
                view.stoat.move_cursor_to_line_start();
                cx.notify();
            }))
            .on_action(cx.listener(|view: &mut Self, _: &MoveToLineEnd, _, cx| {
                view.stoat.move_cursor_to_line_end(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|view: &mut Self, _: &MoveToFileStart, _, cx| {
                view.stoat.move_cursor_to_file_start();
                cx.notify();
            }))
            .on_action(cx.listener(|view: &mut Self, _: &MoveToFileEnd, _, cx| {
                view.stoat.move_cursor_to_file_end(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|view: &mut Self, _: &PageUp, _, cx| {
                view.stoat.move_cursor_page_up(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|view: &mut Self, _: &PageDown, _, cx| {
                view.stoat.move_cursor_page_down(cx);
                cx.notify();
            }))
            // Edit actions
            .on_action(cx.listener(|view: &mut Self, action: &InsertText, _, cx| {
                view.stoat.insert_text(&action.0, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|view: &mut Self, _: &DeleteLeft, _, cx| {
                view.stoat.delete_left(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|view: &mut Self, _: &DeleteRight, _, cx| {
                view.stoat.delete_right(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|view: &mut Self, _: &DeleteLine, _, cx| {
                view.stoat.delete_line(cx);
                cx.notify();
            }))
            .on_action(
                cx.listener(|view: &mut Self, _: &DeleteToEndOfLine, _, cx| {
                    view.stoat.delete_to_end_of_line(cx);
                    cx.notify();
                }),
            )
            // Modal actions
            .on_action(cx.listener(|view: &mut Self, _: &EnterInsertMode, _, cx| {
                view.stoat.set_mode("insert");
                cx.notify();
            }))
            .on_action(cx.listener(|view: &mut Self, _: &EnterNormalMode, _, cx| {
                view.stoat.set_mode("normal");
                cx.notify();
            }))
            .on_action(cx.listener(|view: &mut Self, _: &EnterVisualMode, _, cx| {
                view.stoat.set_mode("visual");
                cx.notify();
            }))
            .on_action(cx.listener(|view: &mut Self, _: &EnterPaneMode, _, cx| {
                view.stoat.set_mode("pane");
                cx.notify();
            }))
            // Selection actions
            .on_action(cx.listener(|view: &mut Self, _: &SelectNextSymbol, _, cx| {
                view.stoat.select_next_symbol(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|view: &mut Self, _: &SelectPrevSymbol, _, cx| {
                view.stoat.select_prev_symbol(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|view: &mut Self, _: &SelectNextToken, _, cx| {
                view.stoat.select_next_token(cx);
                cx.notify();
            }))
            .on_action(cx.listener(|view: &mut Self, _: &SelectPrevToken, _, cx| {
                view.stoat.select_prev_token(cx);
                cx.notify();
            }))
            // Handle text input in insert mode as fallback (when no action matched)
            .on_key_down(
                cx.listener(|view: &mut Self, event: &gpui::KeyDownEvent, _, cx| {
                    // Only insert text in insert mode when no action matched
                    if view.stoat.mode() == "insert" {
                        if let Some(ref key_char) = event.keystroke.key_char {
                            // Only insert if no control/alt modifiers
                            if !event.keystroke.modifiers.control && !event.keystroke.modifiers.alt
                            {
                                view.stoat.insert_text(key_char, cx);
                                cx.notify();
                            }
                        }
                    }
                }),
            )
    }
}

/// Test wrapper for Stoat that provides convenient testing methods
pub struct StoatTest {
    view: gpui::Entity<StoatView>,
    _window: gpui::AnyWindowHandle,
    cx: gpui::VisualTestContext,
    line_height: f32,
}

impl Default for StoatTest {
    fn default() -> Self {
        Self::new()
    }
}

impl StoatTest {
    /// Create a new StoatTest instance with default settings
    pub fn new() -> Self {
        use std::ops::Deref;

        let mut cx = TestAppContext::single();

        // Bind default keys
        cx.update(|cx| {
            let bindings = crate::keymap::create_default_keymap()
                .bindings()
                .cloned()
                .collect::<Vec<_>>();
            cx.bind_keys(bindings);
        });

        // Create window with StoatView
        let window = cx.add_window(|window, cx| {
            let stoat = Stoat::new(cx);
            let view = StoatView::new(stoat, cx);

            // Focus the view
            window.focus(&view.focus_handle);

            view
        });

        let view = window.root(&mut cx).unwrap();

        let mut test = Self {
            view,
            _window: window.into(),
            cx: gpui::VisualTestContext::from_window(*window.deref(), &cx),
            line_height: DEFAULT_LINE_HEIGHT,
        };

        // Set default viewport size (24 lines, like a terminal)
        test.set_viewport_lines(24.0);

        test
    }

    /// Get the current buffer contents as a string
    pub fn text(&mut self) -> String {
        self.view
            .update_in(&mut self.cx, |view, _, cx| view.stoat().buffer_contents(cx))
    }

    /// Get the current cursor position as (row, column)
    pub fn cursor(&mut self) -> (u32, u32) {
        self.view.update_in(&mut self.cx, |view, _, _| {
            let pos = view.stoat().cursor_position();
            (pos.row, pos.column)
        })
    }

    /// Insert text at the current cursor position
    pub fn insert(&mut self, text: &str) {
        self.view.update(&mut self.cx, |view, cx| {
            view.stoat_mut().insert_text(text, cx);
        });
    }

    /// Set the window size in pixels
    pub fn set_window_size(&mut self, size: Size<Pixels>) {
        // Calculate how many lines fit in this pixel height
        let lines = size.height.0 / self.line_height;
        self.set_viewport_lines(lines);
    }

    /// Set the viewport height in lines
    pub fn set_viewport_lines(&mut self, lines: f32) {
        self.view.update(&mut self.cx, |view, _| {
            view.stoat_mut().set_visible_line_count(lines);
        });
    }

    /// Resize the viewport to the specified number of lines
    pub fn resize_lines(&mut self, lines: f32) {
        self.set_viewport_lines(lines);
    }

    /// Set the line height for pixel/line conversions
    pub fn set_line_height(&mut self, height: f32) {
        self.line_height = height;
    }

    /// Get the current line height
    pub fn line_height(&self) -> f32 {
        self.line_height
    }

    /// Get the current viewport size in lines
    pub fn viewport_lines(&mut self) -> Option<f32> {
        self.view
            .update_in(&mut self.cx, |view, _, _| view.stoat().visible_line_count())
    }

    /// Move cursor to specific position
    pub fn set_cursor(&mut self, row: u32, col: u32) {
        self.view.update(&mut self.cx, |view, _| {
            view.stoat_mut().set_cursor_position(Point::new(row, col));
        });
    }

    /// Assert the text content matches expected
    #[track_caller]
    pub fn assert_text(&mut self, expected: &str) {
        assert_eq!(self.text(), expected);
    }

    /// Assert the cursor position matches expected
    #[track_caller]
    pub fn assert_cursor(&mut self, row: u32, col: u32) {
        assert_eq!(self.cursor(), (row, col));
    }

    /// Get the current editor mode
    pub fn mode(&mut self) -> String {
        self.view
            .update_in(&mut self.cx, |view, _, _| view.stoat().mode().to_string())
    }

    /// Set the editor mode
    pub fn set_mode(&mut self, mode: &str) {
        self.view.update(&mut self.cx, |view, cx| {
            view.stoat_mut().set_mode(mode);
            cx.notify(); // Trigger re-render to update key context
        });
        self.cx.run_until_parked(); // Wait for render to complete
    }

    /// Get the current selection as (start_row, start_col, end_row, end_col)
    pub fn selection(&mut self) -> (u32, u32, u32, u32) {
        self.view.update_in(&mut self.cx, |view, _, _| {
            let selection = view.stoat().cursor_manager().selection();
            (
                selection.start.row,
                selection.start.column,
                selection.end.row,
                selection.end.column,
            )
        })
    }

    /// Check if there is an active selection
    pub fn has_selection(&mut self) -> bool {
        self.view.update_in(&mut self.cx, |view, _, _| {
            !view.stoat().cursor_manager().selection().is_empty()
        })
    }

    /// Assert the editor mode matches expected
    #[track_caller]
    pub fn assert_mode(&mut self, expected: &str) {
        assert_eq!(self.mode(), expected);
    }

    /// Assert the selection range matches expected
    #[track_caller]
    pub fn assert_selection(&mut self, start_row: u32, start_col: u32, end_row: u32, end_col: u32) {
        assert_eq!(self.selection(), (start_row, start_col, end_row, end_col));
    }

    /// Assert that no text is selected
    #[track_caller]
    pub fn assert_no_selection(&mut self) {
        assert!(
            !self.has_selection(),
            "Expected no selection, but selection exists"
        );
    }

    /// Start text selection at current cursor position
    pub fn start_selection(&mut self) {
        self.view.update(&mut self.cx, |view, _| {
            view.stoat_mut().cursor_manager_mut().start_selection();
        });
    }

    /// End text selection
    pub fn end_selection(&mut self) {
        self.view.update(&mut self.cx, |view, _| {
            view.stoat_mut().cursor_manager_mut().end_selection();
        });
    }

    /// Select the next token from cursor and return selected text
    pub fn select_next_token(&mut self) -> Option<String> {
        self.view.update_in(&mut self.cx, |view, _, cx| {
            view.stoat_mut().select_next_token(cx).map(|range| {
                let snapshot = view.stoat().buffer_snapshot(cx);
                snapshot.text_for_range(range).collect()
            })
        })
    }

    /// Set buffer text (plain, cursor at origin)
    ///
    /// Replaces entire buffer with the given text and resets cursor to (0, 0).
    /// Use this for initial test setup when you don't need specific cursor placement.
    ///
    /// The text is parsed as Rust code by default for tokenization. Use [`set_text_with_language`]
    /// if you need a different language.
    ///
    /// # Example
    /// ```ignore
    /// s.set_text("fn foo() {}");  // Text set, cursor at start, parsed as Rust
    /// ```
    pub fn set_text(&mut self, text: &str) {
        self.set_text_with_language(text, stoat_text_v3::Language::Rust);
    }

    /// Set buffer text with a specific language for parsing
    ///
    /// Replaces entire buffer with the given text, parses it with the specified language,
    /// and resets cursor to (0, 0).
    ///
    /// # Example
    /// ```ignore
    /// s.set_text_with_language("# Hello", stoat_text_v3::Language::Markdown);
    /// ```
    pub fn set_text_with_language(&mut self, text: &str, language: stoat_text_v3::Language) {
        self.view.update(&mut self.cx, |view, cx| {
            let stoat = view.stoat_mut();

            // Update language and parser if needed
            if stoat.current_language != language {
                stoat.current_language = language;
                stoat.parser =
                    stoat_text_v3::Parser::new(language).expect("Failed to create parser");
            }

            // Clear buffer and insert new text
            stoat.buffer.update(cx, |buf, _| {
                let len = buf.len();
                buf.edit([(0..len, text)]);
            });

            // Re-parse entire buffer after edit
            let buffer_snapshot = stoat.buffer.read(cx).snapshot();
            match stoat.parser.parse(text, &buffer_snapshot) {
                Ok(tokens) => {
                    stoat.token_map.replace_tokens(tokens, &buffer_snapshot);
                },
                Err(e) => {
                    eprintln!("Failed to parse in set_text: {}", e);
                },
            }

            // Reset cursor to origin
            stoat.set_cursor_position(Point::new(0, 0));
        });
    }

    /// Process input through the editor via GPUI's keystroke dispatch.
    ///
    /// Simulates key input to the editor using GPUI's full action dispatch pipeline.
    /// Keystrokes are parsed, matched against the keymap, and dispatched as actions
    /// to the focused element's handlers.
    ///
    /// # Example
    /// ```ignore
    /// s.input("h");   // Move left (dispatches MoveLeft action)
    /// s.input("i");   // Enter insert mode (dispatches EnterInsertMode)
    /// s.input("w");   // In normal mode: move forward one word
    /// ```
    pub fn input(&mut self, keys: &str) {
        self.cx.simulate_keystrokes(keys);
        self.cx.run_until_parked();
    }

    /// Assert current state matches cursor notation
    ///
    /// Compares current buffer text, cursor, and selection against
    /// the notation string representation.
    ///
    /// # Example
    /// ```ignore
    /// s.assert_cursor_notation("hello |world");
    /// ```
    #[track_caller]
    pub fn assert_cursor_notation(&mut self, expected: &str) {
        let actual = self.cursor_notation();
        assert_eq!(
            actual, expected,
            "\nExpected: {expected}\nActual:   {actual}"
        );
    }

    /// Get current state as cursor notation string
    ///
    /// Returns the buffer text with notation showing cursor and selection.
    /// Useful for debugging test failures.
    pub fn cursor_notation(&mut self) -> String {
        self.view.update_in(&mut self.cx, |view, _, cx| {
            let stoat = view.stoat();
            let snapshot = stoat.buffer_snapshot(cx);
            let text = snapshot.text();

            let cursor_mgr = stoat.cursor_manager();
            let cursor_point = cursor_mgr.position();
            let cursor_offset = snapshot.point_to_offset(cursor_point);

            let selection = cursor_mgr.selection();
            let selections = if !selection.is_empty() {
                let start_offset = snapshot.point_to_offset(selection.start);
                let end_offset = snapshot.point_to_offset(selection.end);
                let cursor_at_start = cursor_point == selection.start;

                vec![cursor_notation::Selection {
                    range: start_offset..end_offset,
                    cursor_at_start,
                }]
            } else {
                vec![]
            };

            let cursors = if selections.is_empty() {
                vec![cursor_offset]
            } else {
                vec![]
            };

            cursor_notation::format(&text, &cursors, &selections)
        })
    }

    /// Simulate scroll wheel event with line-based scrolling
    pub fn scroll_lines(&mut self, lines_x: f32, lines_y: f32) {
        self.scroll_lines_with_fast(lines_x, lines_y, false);
    }

    /// Simulate scroll wheel event with fast scrolling (Alt key held)
    pub fn scroll_lines_fast(&mut self, lines_x: f32, lines_y: f32) {
        self.scroll_lines_with_fast(lines_x, lines_y, true);
    }

    /// Simulate scroll wheel event with line-based scrolling and optional fast mode
    pub fn scroll_lines_with_fast(&mut self, lines_x: f32, lines_y: f32, fast_scroll: bool) {
        let delta = crate::ScrollDelta::Lines(gpui::point(lines_x, lines_y));
        self.view.update(&mut self.cx, |view, cx| {
            view.stoat_mut()
                .handle_scroll_event(&delta, fast_scroll, cx);
        });
    }

    /// Simulate trackpad scroll event with pixel-based scrolling
    pub fn scroll_pixels(&mut self, pixels_x: f32, pixels_y: f32) {
        self.scroll_pixels_with_fast(pixels_x, pixels_y, false);
    }

    /// Simulate trackpad scroll event with fast scrolling (Alt key held)
    pub fn scroll_pixels_fast(&mut self, pixels_x: f32, pixels_y: f32) {
        self.scroll_pixels_with_fast(pixels_x, pixels_y, true);
    }

    /// Simulate trackpad scroll event with pixel-based scrolling and optional fast mode
    pub fn scroll_pixels_with_fast(&mut self, pixels_x: f32, pixels_y: f32, fast_scroll: bool) {
        let delta =
            crate::ScrollDelta::Pixels(gpui::point(gpui::Pixels(pixels_x), gpui::Pixels(pixels_y)));
        self.view.update(&mut self.cx, |view, cx| {
            view.stoat_mut()
                .handle_scroll_event(&delta, fast_scroll, cx);
        });
    }

    /// Get the current scroll position as (x, y) in fractional lines
    pub fn scroll_position(&mut self) -> (f32, f32) {
        self.view.update_in(&mut self.cx, |view, _, _| {
            let pos = view.stoat().scroll_position();
            (pos.x, pos.y)
        })
    }

    /// Assert the scroll position matches expected values
    #[track_caller]
    pub fn assert_scroll_position(&mut self, expected_x: f32, expected_y: f32) {
        let (actual_x, actual_y) = self.scroll_position();
        assert_eq!(
            (actual_x, actual_y),
            (expected_x, expected_y),
            "Expected scroll position ({expected_x}, {expected_y}), got ({actual_x}, {actual_y})"
        );
    }

    /// Assert the scroll Y position matches expected value (most common for vertical scrolling)
    #[track_caller]
    pub fn assert_scroll_y(&mut self, expected_y: f32) {
        let (_, actual_y) = self.scroll_position();
        assert_eq!(
            actual_y, expected_y,
            "Expected scroll Y position {expected_y}, got {actual_y}"
        );
    }
}

impl Stoat {
    /// Create a new Stoat instance configured for testing
    pub fn test() -> StoatTest {
        StoatTest::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_text_insertion() {
        let mut s = Stoat::test();
        s.insert("Hello World");
        s.assert_text("Hello World");
        s.assert_cursor(0, 11);
    }

    #[test]
    fn cursor_positioning() {
        let mut s = Stoat::test();
        s.insert("Line 1\nLine 2\nLine 3");
        s.set_cursor(1, 3);
        s.assert_cursor(1, 3);
    }

    #[test]
    fn input_insert_mode() {
        let mut s = Stoat::test();
        s.input("i");
        s.input("h e l l o");
        s.assert_text("hello");
    }

    #[test]
    fn escape_exits_insert() {
        let mut s = Stoat::test();
        s.assert_mode("normal");
        s.input("i");
        s.assert_mode("insert");
        s.input("escape");
        s.assert_mode("normal");
    }

    #[test]
    fn page_down_moves_cursor() {
        let mut s = Stoat::test();
        s.set_text("Line 1\nLine 2\nLine 3\nLine 4\nLine 5\nLine 6\nLine 7\nLine 8\nLine 9\nLine 10\nLine 11\nLine 12\nLine 13\nLine 14\nLine 15\nLine 16\nLine 17\nLine 18\nLine 19\nLine 20\nLine 21\nLine 22\nLine 23\nLine 24\nLine 25\nLine 26\nLine 27\nLine 28\nLine 29\nLine 30");
        s.assert_cursor(0, 0);
        s.input("ctrl-d");
        let (row, _) = s.cursor();
        assert!(row > 0, "Cursor should have moved down");
    }

    #[test]
    fn pagedown_key_works() {
        let mut s = Stoat::test();
        s.set_text("Line 1\nLine 2\nLine 3\nLine 4\nLine 5\nLine 6\nLine 7\nLine 8\nLine 9\nLine 10\nLine 11\nLine 12\nLine 13\nLine 14\nLine 15\nLine 16\nLine 17\nLine 18\nLine 19\nLine 20\nLine 21\nLine 22\nLine 23\nLine 24\nLine 25\nLine 26\nLine 27\nLine 28\nLine 29\nLine 30");
        s.assert_cursor(0, 0);
        s.input("pagedown");
        let (row, _) = s.cursor();
        assert!(row > 0, "PageDown key should move cursor down");
    }

    #[test]
    fn viewport_sizing() {
        let mut s = Stoat::test();

        // Test line-based sizing
        s.resize_lines(30.0);
        assert_eq!(s.viewport_lines(), Some(30.0));

        // Test pixel-based sizing with default line height (20px)
        s.set_window_size(Size {
            width: Pixels(800.0),
            height: Pixels(600.0), // 600 / 20 = 30 lines
        });
        assert_eq!(s.viewport_lines(), Some(30.0));
    }

    #[test]
    fn line_height_conversion() {
        let mut s = Stoat::test();
        s.set_line_height(16.0);

        s.set_window_size(Size {
            width: Pixels(800.0),
            height: Pixels(480.0), // 480 / 16 = 30 lines
        });
        assert_eq!(s.viewport_lines(), Some(30.0));
    }

    #[test]
    fn editor_mode_handling() {
        let mut s = Stoat::test();

        // Default mode should be Normal
        s.assert_mode("normal");

        // Test mode switching
        s.set_mode("insert");
        s.assert_mode("insert");

        s.set_mode("visual");
        s.assert_mode("visual");

        s.set_mode("normal");
        s.assert_mode("normal");
    }

    #[test]
    fn selection_handling() {
        let mut s = Stoat::test();
        s.insert("Line 1\nLine 2\nLine 3");
        s.set_cursor(1, 2); // Position at "Line 2"

        // Initially no selection
        s.assert_no_selection();
        assert!(!s.has_selection());

        // Start selection
        s.start_selection();

        // Move cursor to extend selection
        s.set_cursor(2, 4); // Move to "Line 3"

        // Should now have selection
        assert!(s.has_selection());
        s.assert_selection(1, 2, 2, 4);

        // End selection
        s.end_selection();

        // Selection should still exist but not be actively selecting
        assert!(s.has_selection());
    }

    #[test]
    fn basic_scroll_handling() {
        let mut s = Stoat::test();

        // Add content to scroll through
        s.insert("Line 1\nLine 2\nLine 3\nLine 4\nLine 5\nLine 6\nLine 7\nLine 8\nLine 9\nLine 10");

        // Initially at origin
        s.assert_scroll_position(0.0, 0.0);

        // Scroll down 3 lines with mouse wheel (positive delta shows content below)
        s.scroll_lines(0.0, 3.0);
        s.assert_scroll_y(3.0);

        // Scroll up 1 line (negative delta shows content above)
        s.scroll_lines(0.0, -1.0);
        s.assert_scroll_y(2.0);

        // Scroll cannot go below 0
        s.scroll_lines(0.0, -10.0);
        s.assert_scroll_y(0.0);
    }

    #[test]
    fn fast_scroll_handling() {
        let mut s = Stoat::test();

        // Add content to scroll through
        s.insert("Line 1\nLine 2\nLine 3\nLine 4\nLine 5\nLine 6\nLine 7\nLine 8\nLine 9\nLine 10");

        // Regular scroll
        s.scroll_lines(0.0, 1.0);
        s.assert_scroll_y(1.0);

        // Reset
        s.scroll_lines(0.0, -1.0);
        s.assert_scroll_y(0.0);

        // Fast scroll should move 3x further (3.0 multiplier)
        s.scroll_lines_fast(0.0, 1.0);
        s.assert_scroll_y(3.0);
    }

    #[test]
    fn pixel_vs_line_scrolling() {
        let mut s = Stoat::test();

        // Add content to scroll through
        s.insert("Line 1\nLine 2\nLine 3\nLine 4\nLine 5\nLine 6\nLine 7\nLine 8\nLine 9\nLine 10");

        // Line-based scrolling
        s.scroll_lines(0.0, 2.0);
        let line_scroll_pos = s.scroll_position().1;

        // Reset
        s.scroll_lines(0.0, -2.0);
        s.assert_scroll_y(0.0);

        // Pixel-based scrolling - 40 pixels should equal 2 lines (20px line height)
        s.scroll_pixels(0.0, 40.0);
        s.assert_scroll_y(line_scroll_pos);
    }

    #[test]
    fn scroll_bounds_checking() {
        let mut s = Stoat::test();

        // Add some content to test upper bound
        s.insert("Line 1\nLine 2\nLine 3\nLine 4\nLine 5");
        let buffer_lines = 5;

        // Scroll past end of buffer
        s.scroll_lines(0.0, (buffer_lines + 10) as f32);

        // Should be clamped to maximum scroll (buffer_lines - 1)
        let (_, actual_y) = s.scroll_position();
        assert!(actual_y <= (buffer_lines - 1) as f32);

        // Scroll past beginning
        s.scroll_lines(0.0, -(buffer_lines + 10) as f32);
        s.assert_scroll_y(0.0);
    }
}

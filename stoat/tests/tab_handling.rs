//! Tab handling and cursor positioning tests.

use stoat::{actions::EditMode, Stoat};

#[test]
fn test_literal_tab_key_event() {
    Stoat::test()
        .type_keys("i") // Enter insert mode
        .assert_mode(EditMode::Insert)
        .type_keys("\t") // Type a literal tab
        .assert_text("\t");
}

#[test]
fn test_keyboard_input_with_literal_tab() {
    let mut editor = Stoat::new();

    // Enter insert mode and type text with tabs
    editor.keyboard_input("iHello\tWorld");
    assert_eq!(editor.buffer_contents(), "Hello\tWorld");
}

#[test]
fn test_tab_cursor_positioning() {
    let mut editor = Stoat::new();

    // Enter insert mode
    editor.keyboard_input("i");

    // Type some text with a tab
    editor.keyboard_input("abc\tdef");

    // The buffer should contain the text with a tab
    assert_eq!(editor.buffer_contents(), "abc\tdef");

    // Visual column position should account for tab width
    let (_, col) = editor.cursor_position();
    assert_eq!(
        col, 7,
        "Cursor should be at character position 7 after 'abc<tab>def'"
    );

    // The desired column should be at visual position 11 (3 chars + 5 tab spaces + 3 chars)
    let state = editor.engine().state();
    assert_eq!(
        state.cursor.desired_column, 11,
        "Desired column should be 11 after 'abc<tab>def'"
    );
}

#[test]
fn test_tab_display_column_tracking() {
    let mut editor = Stoat::new();

    // Enter insert mode
    editor.keyboard_input("i");

    // Type text: "abc<tab>def"
    // Visual layout (tab stops at 8): "abc     def"
    // Character positions: a=0, b=1, c=2, \t=3, d=4, e=5, f=6
    // Visual columns: a=0, b=1, c=2, tab=3-7, d=8, e=9, f=10
    editor.keyboard_input("abc");

    // After "abc", cursor is at character position 3, visual column 3
    let state = editor.engine().state();
    assert_eq!(
        state.cursor.desired_column, 3,
        "Desired column should be 3 after 'abc'"
    );

    // Type a tab
    editor.keyboard_input("\t");

    // After tab, cursor is at character position 4, visual column 8
    let state = editor.engine().state();
    assert_eq!(
        state.cursor.desired_column, 8,
        "Desired column should be 8 after 'abc<tab>'"
    );

    // Type more text
    editor.keyboard_input("def");

    // Desired column should now be 11 (8 + 3 characters)
    let state = editor.engine().state();
    assert_eq!(
        state.cursor.desired_column, 11,
        "Desired column should be 11 after 'abc<tab>def'"
    );
}

#[test]
fn test_tab_insertion_scenarios() {
    let mut editor = Stoat::new();

    // Test 1: Tab at beginning of line
    editor.keyboard_input("i");
    editor.keyboard_input("\t");
    assert_eq!(editor.buffer_contents(), "\t");
    let (_, col) = editor.cursor_position();
    assert_eq!(col, 1, "Tab at start should put cursor at position 1");

    // Test 2: Continue typing after tab
    editor.keyboard_input("hello");
    assert_eq!(editor.buffer_contents(), "\thello");
    let (_, col) = editor.cursor_position();
    assert_eq!(col, 6, "Should be at position 6 after tab+hello");

    // Clear and test another scenario
    editor = Stoat::new();
    editor.keyboard_input("i");

    // Test 3: Tab in middle of text
    editor.keyboard_input("ab");
    editor.keyboard_input("\t");
    editor.keyboard_input("cd");
    assert_eq!(editor.buffer_contents(), "ab\tcd");
    let (_, col) = editor.cursor_position();
    assert_eq!(col, 5, "Should be at position 5 after ab<tab>cd");

    // Test 4: Multiple tabs
    editor = Stoat::new();
    editor.keyboard_input("i");
    editor.keyboard_input("\t\t");
    assert_eq!(editor.buffer_contents(), "\t\t");
    let (_, col) = editor.cursor_position();
    assert_eq!(col, 2, "Should be at position 2 after two tabs");
}

#[test]
fn test_tab_stop_alignment() {
    let mut editor = Stoat::new();
    editor.keyboard_input("i");

    // Test tab stop alignment at different positions
    // Tab width is 8, so tab stops are at 0, 8, 16, 24, etc.

    // Position 0: tab should go to column 8
    editor.keyboard_input("\t");
    let state = editor.engine().state();
    assert_eq!(
        state.cursor.desired_column, 8,
        "Tab from position 0 should go to visual column 8"
    );

    // Clear and test from position 1
    editor = Stoat::new();
    editor.keyboard_input("i");
    editor.keyboard_input("a\t");
    let state = editor.engine().state();
    assert_eq!(
        state.cursor.desired_column, 8,
        "Tab from position 1 should go to visual column 8"
    );

    // Clear and test from position 7
    editor = Stoat::new();
    editor.keyboard_input("i");
    editor.keyboard_input("1234567\t");
    let state = editor.engine().state();
    assert_eq!(
        state.cursor.desired_column, 8,
        "Tab from position 7 should go to visual column 8"
    );

    // Clear and test from position 8
    editor = Stoat::new();
    editor.keyboard_input("i");
    editor.keyboard_input("12345678\t");
    let state = editor.engine().state();
    assert_eq!(
        state.cursor.desired_column, 16,
        "Tab from position 8 should go to visual column 16"
    );

    // Clear and test from position 9
    editor = Stoat::new();
    editor.keyboard_input("i");
    editor.keyboard_input("123456789\t");
    let state = editor.engine().state();
    assert_eq!(
        state.cursor.desired_column, 16,
        "Tab from position 9 should go to visual column 16"
    );
}

#[test]
fn test_tab_insertion_middle_of_text() {
    let mut editor = Stoat::new();

    // Build the text with a tab in the middle directly
    editor.keyboard_input("i");
    editor.keyboard_input("Hello");
    editor.keyboard_input("\t");
    editor.keyboard_input(" World");
    assert_eq!(editor.buffer_contents(), "Hello\t World");
}

#[test]
fn test_tab_backspace_cursor_position() {
    let mut editor = Stoat::new();

    // Enter insert mode and type text with tab
    editor.keyboard_input("i");
    editor.keyboard_input("ab\tc");
    assert_eq!(editor.buffer_contents(), "ab\tc");

    // Cursor should be at character position 4 (after 'c')
    let (_, col) = editor.cursor_position();
    assert_eq!(col, 4, "Should be at position 4 after 'ab<tab>c'");

    // Visual column should be 9 (2 chars + 6 tab spaces + 1 char)
    let state = editor.engine().state();
    assert_eq!(
        state.cursor.desired_column, 9,
        "Visual column should be 9 after 'ab<tab>c'"
    );

    // Backspace to delete 'c'
    editor.keyboard_input("<Backspace>");
    assert_eq!(editor.buffer_contents(), "ab\t");

    // Cursor should be at character position 3 (after tab)
    let (_, col) = editor.cursor_position();
    assert_eq!(col, 3, "Should be at position 3 after backspace");

    // Visual column should be 8 (2 chars + 6 tab spaces)
    let state = editor.engine().state();
    assert_eq!(
        state.cursor.desired_column, 8,
        "Visual column should be 8 after backspace"
    );

    // Backspace to delete tab
    editor.keyboard_input("<Backspace>");
    assert_eq!(editor.buffer_contents(), "ab");

    // Cursor should be at character position 2 (after 'b')
    let (_, col) = editor.cursor_position();
    assert_eq!(col, 2, "Should be at position 2 after deleting tab");

    // Visual column should be 2
    let state = editor.engine().state();
    assert_eq!(
        state.cursor.desired_column, 2,
        "Visual column should be 2 after deleting tab"
    );
}

#[test]
fn test_backspace_after_single_tab() {
    let mut editor = Stoat::new();

    // Enter insert mode and type just a tab
    editor.keyboard_input("i");
    editor.keyboard_input("\t");
    assert_eq!(editor.buffer_contents(), "\t");

    // Visual column should be 8
    let state = editor.engine().state();
    assert_eq!(
        state.cursor.desired_column, 8,
        "Visual column should be 8 after tab"
    );

    // Backspace to delete the tab
    editor.keyboard_input("<Backspace>");
    assert_eq!(editor.buffer_contents(), "");

    // Cursor should be at position 0
    let (_, col) = editor.cursor_position();
    assert_eq!(col, 0, "Should be at position 0 after deleting tab");

    // Visual column should be 0
    let state = editor.engine().state();
    assert_eq!(
        state.cursor.desired_column, 0,
        "Visual column should be 0 after deleting tab"
    );
}

#[test]
fn test_backspace_in_middle_of_tabs() {
    let mut editor = Stoat::new();

    // Test backspace with tabs - build text and then delete
    editor.keyboard_input("i");
    editor.keyboard_input("a\tb");
    assert_eq!(editor.buffer_contents(), "a\tb");

    // Backspace to delete 'b'
    editor.keyboard_input("<Backspace>");
    assert_eq!(editor.buffer_contents(), "a\t");

    // Backspace to delete tab
    editor.keyboard_input("<Backspace>");
    assert_eq!(editor.buffer_contents(), "a");

    // Add more complex case
    editor.keyboard_input("\tb\tc");
    assert_eq!(editor.buffer_contents(), "a\tb\tc");

    // Delete from end
    editor.keyboard_input("<Backspace>");
    assert_eq!(editor.buffer_contents(), "a\tb\t");

    editor.keyboard_input("<Backspace>");
    assert_eq!(editor.buffer_contents(), "a\tb");
}

#[test]
fn test_multiple_backspaces_with_tabs() {
    let mut editor = Stoat::new();

    // Enter insert mode and type text with tabs
    editor.keyboard_input("i");
    editor.keyboard_input("\t\tabc");
    assert_eq!(editor.buffer_contents(), "\t\tabc");

    // Delete all characters one by one
    editor.keyboard_input("<Backspace>"); // Delete 'c'
    assert_eq!(editor.buffer_contents(), "\t\tab");

    editor.keyboard_input("<Backspace>"); // Delete 'b'
    assert_eq!(editor.buffer_contents(), "\t\ta");

    editor.keyboard_input("<Backspace>"); // Delete 'a'
    assert_eq!(editor.buffer_contents(), "\t\t");

    editor.keyboard_input("<Backspace>"); // Delete second tab
    assert_eq!(editor.buffer_contents(), "\t");

    editor.keyboard_input("<Backspace>"); // Delete first tab
    assert_eq!(editor.buffer_contents(), "");
}

//! Text manipulation and operation tests.

use stoat::Stoat;

#[test]
fn test_text_insertion() {
    let mut editor = Stoat::new();

    // Enter insert mode and type text
    editor.keyboard_input("iHello");
    assert_eq!(editor.buffer_contents(), "Hello");

    // Type more text
    editor.keyboard_input(" World");
    assert_eq!(editor.buffer_contents(), "Hello World");
}

#[test]
fn test_text_deletion() {
    let mut editor = Stoat::new();

    // Test deletion with backspace in insert mode
    editor.keyboard_input("i");
    editor.keyboard_input("Hello World");
    assert_eq!(editor.buffer_contents(), "Hello World");

    // Now we're in insert mode at the end, can use backspace
    editor.keyboard_input("<Backspace>"); // Backspace to delete 'd'
    assert_eq!(editor.buffer_contents(), "Hello Worl");

    // Delete more characters
    editor.keyboard_input("<Backspace>"); // Delete 'l'
    assert_eq!(editor.buffer_contents(), "Hello Wor");
}

#[test]
fn test_undo_redo_placeholder() {
    // Placeholder for undo/redo functionality
    // Currently these operations are not implemented
    let mut editor = Stoat::with_text("Test");

    // These events don't crash but don't do anything yet
    editor.keyboard_input("u"); // Would be undo
    assert_eq!(editor.buffer_contents(), "Test");

    // Redo also doesn't do anything yet
    editor.keyboard_input("\x12"); // Ctrl+R would be redo
    assert_eq!(editor.buffer_contents(), "Test");
}

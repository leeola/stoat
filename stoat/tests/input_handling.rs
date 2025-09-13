//! Input handling and keyboard processing tests.

use stoat::{actions::EditMode, Stoat};

#[test]
fn test_literal_space_key_event() {
    Stoat::test()
        .type_keys("i") // Enter insert mode
        .assert_mode(EditMode::Insert)
        .type_keys(" ") // Type a literal space
        .assert_text(" ");
}

#[test]
fn test_keyboard_input_with_literal_space() {
    let mut editor = Stoat::new();
    editor.keyboard_input("iHello World");
    assert_eq!(editor.buffer_contents(), "Hello World");
}

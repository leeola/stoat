//! Basic editing functionality tests.

use stoat::{actions::EditMode, Stoat};

#[test]
fn stoat_new_creates_empty_editor() {
    Stoat::test()
        .assert_text("")
        .assert_cursor(0, 0)
        .assert_mode(EditMode::Normal)
        .assert_dirty(false);
}

#[test]
fn stoat_with_text_initializes_content() {
    Stoat::test()
        .with_text("Initial content")
        .assert_text("Initial content")
        .assert_cursor(0, 0)
        .assert_mode(EditMode::Normal)
        .assert_dirty(false);
}

#[test]
fn new_engine_starts_empty() {
    Stoat::test()
        .assert_text("")
        .assert_cursor(0, 0)
        .assert_mode(EditMode::Normal)
        .assert_dirty(false);
}

#[test]
fn with_text_sets_initial_content() {
    Stoat::test()
        .with_text("Hello, world!")
        .assert_text("Hello, world!");
}

#[test]
fn snapshot_and_restore() {
    let mut stoat = Stoat::with_text("Original");
    let snapshot = stoat.engine().snapshot();

    stoat.keyboard_input("iX");
    assert_ne!(stoat.buffer_contents(), "Original");

    stoat.engine_mut().set_state(snapshot);
    assert_eq!(stoat.buffer_contents(), "Original");
    assert_eq!(stoat.engine().mode(), EditMode::Normal);
}

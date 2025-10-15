//! Tests for file writing functionality.
//!
//! This module tests the [`write_file`](crate::Stoat::write_file) action, which writes
//! the current buffer contents to disk. Tests use [`TestStoat`](crate::test::TestStoat)
//! with git integration to verify actual filesystem operations.

use crate::{actions::*, Stoat};
use gpui::TestAppContext;

#[gpui::test]
fn writes_buffer_to_disk(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test(cx).init_git();

    // Set file path in test repo
    let file_path = stoat.repo_path().unwrap().join("test.txt");
    stoat.set_file_path(file_path.clone());

    // Write content to buffer using action dispatch
    stoat.dispatch(EnterInsertMode);
    stoat.dispatch(InsertText("Hello from Stoat!".to_string()));

    // Call write_file action using dispatch
    stoat.dispatch(WriteFile);

    // Verify file exists on disk
    assert!(file_path.exists(), "File should exist after write");

    // Verify file contents match buffer
    let contents = std::fs::read_to_string(&file_path).expect("Failed to read file");
    assert_eq!(contents, "Hello from Stoat!");
}

#[gpui::test]
#[should_panic(expected = "WriteFile action failed: No file path set for current buffer")]
fn write_fails_without_file_path(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test(cx);

    // Write content to buffer but don't set file path
    stoat.dispatch(EnterInsertMode);
    stoat.dispatch(InsertText("Hello".to_string()));

    // Call write_file action - should panic
    stoat.dispatch(WriteFile);
}

#[gpui::test]
fn writes_multiline_content(cx: &mut TestAppContext) {
    let mut stoat = Stoat::test(cx).init_git();

    let file_path = stoat.repo_path().unwrap().join("multiline.txt");
    stoat.set_file_path(file_path.clone());

    // Write multiline content using action dispatch
    stoat.dispatch(EnterInsertMode);
    stoat.dispatch(InsertText("Line 1".to_string()));
    stoat.dispatch(NewLine);
    stoat.dispatch(InsertText("Line 2".to_string()));
    stoat.dispatch(NewLine);
    stoat.dispatch(InsertText("Line 3".to_string()));

    // Write to disk using action dispatch
    stoat.dispatch(WriteFile);

    // Verify file contents
    let contents = std::fs::read_to_string(&file_path).expect("Failed to read file");
    assert_eq!(contents, "Line 1\nLine 2\nLine 3");
}

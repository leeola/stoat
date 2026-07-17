//! End-to-end tests for the completion pipeline.
//!
//! Drives the full user-visible flow -- key input, debounce, source
//! fetch, popup render -- via [`crate::test_harness::TestHarness`]
//! with [`crate::host::FakeFs`] / [`crate::host::FakeLsp`]. Snapshots
//! the rendered terminal frame via [`TestHarness::assert_snapshot`]
//! so any regression in the pipeline surfaces visually.
//!
//! Sibling test modules cover narrower seams:
//! - [`crate::render::completion::tests`] -- renderer with synthetic popup state.
//! - [`crate::completion::request::harness_tests`] -- trigger / debounce / cancellation
//!   programmatic asserts.
//! - [`crate::completion::accept::tests`] -- acceptance, buffer text + cursor programmatic asserts.

#![cfg(test)]

use crate::{
    action_handlers::dispatch, completion::request::COMPLETION_DEBOUNCE, test_harness::TestHarness,
};
use lsp_types::{CompletionOptions, ServerCapabilities};
use std::path::PathBuf;
use stoat_action::OpenFile;

fn enable_lsp_completion(h: &TestHarness) {
    h.fake_lsp().set_capabilities(ServerCapabilities {
        completion_provider: Some(CompletionOptions::default()),
        ..ServerCapabilities::default()
    });
}

fn open_scratch(h: &mut TestHarness, contents: &str) -> PathBuf {
    let path = PathBuf::from("/ws/buf.rs");
    h.fake_fs()
        .insert_files(std::iter::once((path.clone(), contents.as_bytes())));
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/ws");
    dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();
    path
}

#[test]
fn snapshot_completion_path_dot_slash() {
    let mut h = TestHarness::with_size(60, 16);
    h.fake_fs().insert_file("/ws/lib.rs", b"");
    h.fake_fs().insert_file("/ws/main.rs", b"");
    h.fake_fs().insert_dir("/ws/src");
    open_scratch(&mut h, "");

    h.type_keys("i");
    h.type_text("./");
    h.advance_clock(COMPLETION_DEBOUNCE);

    h.assert_snapshot("completion_e2e_path_dot_slash");
}

#[test]
fn snapshot_completion_lsp_identifier() {
    let mut h = TestHarness::with_size(60, 16);
    enable_lsp_completion(&h);
    open_scratch(&mut h, "");
    h.fake_lsp()
        .set_completions("/ws/buf.rs", 0, 3, &["foobar", "foobaz", "foobiz"]);

    h.type_keys("i");
    h.type_text("foo");
    h.advance_clock(COMPLETION_DEBOUNCE);

    h.assert_snapshot("completion_e2e_lsp_identifier");
}

#[test]
fn snapshot_completion_word_buffer() {
    let mut h = TestHarness::with_size(60, 16);
    open_scratch(&mut h, "alpha beta alphanumeric alphabet\n\n");
    let end_offset = focused_buffer_len(&mut h);
    crate::action_handlers::movement::jump_to_offset(&mut h.stoat, end_offset);
    h.type_keys("i");
    h.type_text("al");
    h.advance_clock(COMPLETION_DEBOUNCE);

    h.assert_snapshot("completion_e2e_word_buffer");
}

fn focused_buffer_len(h: &mut TestHarness) -> usize {
    let ws = h.stoat.active_workspace_mut();
    let crate::pane::FocusTarget::SplitPane = ws.focus else {
        panic!("not a split pane");
    };
    let pane_id = ws.panes.focus();
    let crate::pane::View::Editor(editor_id) = ws.panes.pane(pane_id).view else {
        panic!("not an editor pane");
    };
    let buffer_id = ws.editors.get(editor_id).unwrap().buffer_id;
    let buffer = ws.buffers.get(buffer_id).unwrap();

    buffer.read().unwrap().rope().len()
}

#[test]
fn whitespace_prefix_does_not_open_popup() {
    let mut h = TestHarness::default();
    enable_lsp_completion(&h);
    h.fake_lsp().set_completions("/ws/buf.rs", 0, 0, &["foo"]);
    open_scratch(&mut h, "");

    h.type_keys("i");
    h.type_text("   ");
    h.advance_clock(COMPLETION_DEBOUNCE);

    assert!(
        h.stoat.pending_completion.is_none(),
        "whitespace prefix must not arm the popup",
    );
}

#[test]
fn completion_merges_items_from_every_server() {
    use crate::lsp::registry::ServerSelector;

    let mut h = TestHarness::with_size(60, 16);
    let server_a = std::sync::Arc::new(crate::host::FakeLsp::new());
    let server_b = std::sync::Arc::new(crate::host::FakeLsp::new());
    for server in [&server_a, &server_b] {
        server.set_capabilities(ServerCapabilities {
            completion_provider: Some(CompletionOptions::default()),
            ..ServerCapabilities::default()
        });
    }
    server_a.set_completions("/ws/buf.rs", 0, 3, &["foo_a"]);
    server_b.set_completions("/ws/buf.rs", 0, 3, &["foo_b"]);
    h.stoat.lsp_registry.insert("ra".into(), server_a.clone());
    h.stoat
        .lsp_registry
        .insert("tailwind".into(), server_b.clone());
    h.stoat.lsp_registry.set_selectors(
        "rust".into(),
        vec![
            ServerSelector::all("ra".into()),
            ServerSelector::all("tailwind".into()),
        ],
    );

    open_scratch(&mut h, "");
    h.type_keys("i");
    h.type_text("foo");
    h.advance_clock(COMPLETION_DEBOUNCE);

    let popup = h.stoat.pending_completion.as_ref().expect("popup open");
    let labels: Vec<&str> = popup.items.iter().map(|item| item.label.as_str()).collect();
    assert!(
        labels.contains(&"foo_a"),
        "server A item merged: {labels:?}"
    );
    assert!(
        labels.contains(&"foo_b"),
        "server B item merged: {labels:?}"
    );
}

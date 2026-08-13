//! The fixtures a test shares with tests in other modules.
//!
//! A subsystem that moved out of app.rs took its tests with it, and those tests
//! still need what app.rs's own tests need: an open modal's real layout, a
//! scratch buffer on screen, the APC batches a frame queued. Leaving them inside
//! app.rs's private test module would mean widening app internals just so a test
//! elsewhere could reach them, so they live here instead, where every module's
//! tests can name them and nothing production-facing changes.
//!
//! Everything here reads the app through state that is already visible, so
//! nothing widened to make the module possible.

use crate::{
    action_handlers,
    app::{modal_split_percent, modal_zoom_steps, ModalKind},
    editor_state::EditorId,
    pane::FocusTarget,
    test_harness::TestHarness,
};
use crossterm::event::{Event, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use std::path::PathBuf;
use stoat_action::OpenFile;
use stoat_config::MinimapMode;
use stoatty_protocol::command;
use tokio::sync::mpsc::UnboundedReceiver;

/// The open help modal's layout, sized from the same content and zoom the
/// renderer would read, so a test rect matches the painted one.
pub(crate) fn help_layout(h: &TestHarness) -> crate::render::help::HelpLayout {
    let help = h.stoat.help.as_ref().expect("the help modal is open");
    crate::render::help::help_layout(
        h.stoat.size(),
        crate::render::help::help_content_rows(help),
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::Help),
    )
    .expect("the help modal fits the test viewport")
}

/// The open palette's sizing inputs, so a test lays its box out exactly as
/// the renderer would.
pub(crate) fn palette_sizing(h: &TestHarness) -> (u16, i8) {
    let palette = h
        .stoat
        .command_palette
        .as_ref()
        .expect("the palette is open");
    (
        palette.list_rows_hint(),
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::Palette),
    )
}

/// The open finder's layout, sized from the same content and zoom the
/// renderer would read, so a test rect matches the painted one.
pub(crate) fn finder_layout(h: &TestHarness) -> crate::render::file_finder::FinderLayout {
    let finder = h.stoat.file_finder.as_ref().expect("the finder is open");
    crate::render::file_finder::file_finder_layout(
        h.stoat.size(),
        finder.content_size,
        modal_zoom_steps(&h.stoat.modal_zoom, ModalKind::FileFinder),
        modal_split_percent(&h.stoat.modal_split, ModalKind::FileFinder),
    )
    .expect("the finder fits the test terminal")
}

/// Drain every APC batch currently queued on `rx` into one decoded command
/// list. A plain editor pane fills its pages asynchronously, so one
/// `emit_smooth_scroll` pushes the region/scroll batch plus a fill batch per
/// page. Draining folds them together so a test reads the whole emit at once.
pub(crate) fn drain_apc(rx: &mut UnboundedReceiver<Vec<u8>>) -> Vec<command::Command> {
    let mut cmds = Vec::new();
    while let Ok(batch) = rx.try_recv() {
        cmds.extend(command::decode_stream(&batch));
    }
    cmds
}

/// Open a buffer long enough to fill a per-pane minimap strip, with the strip's
/// rect already reserved as a paint leaves it.
pub(crate) fn open_with_minimap_strip(h: &mut TestHarness) -> EditorId {
    h.stoat.settings.editor_minimap = Some(MinimapMode::PerPane);
    let body: String = (0..60)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    open_scratch_file(h, &body);
    let editor_id = h.stoat.focused_editor_ids().expect("editor").0;
    let editor = &mut h.stoat.active_workspace_mut().editors[editor_id];
    editor.minimap_rect = Some(Rect::new(72, 0, 8, 10));
    editor.viewport_rows = Some(20);
    editor_id
}

/// Open a buffer holding `contents`, for a test that needs a file on screen
/// rather than a particular one. Returns the path it was written to.
pub(crate) fn open_scratch_file(h: &mut TestHarness, contents: &str) -> PathBuf {
    let path = PathBuf::from("/ws/buf.txt");
    h.fake_fs()
        .insert_files(std::iter::once((path.clone(), contents.as_bytes())));
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/ws");
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
    h.settle();
    path
}

/// Write `files` under a fake git root and make that root the active
/// workspace's, for a test whose subject is which file the editor picked.
/// Returns the root the relative names hang off.
///
/// Nothing is opened. The name of the root is arbitrary, and is kept as it is
/// only so the LSP tests that grew this helper go on writing the same paths.
pub(crate) fn seed(h: &mut TestHarness, files: &[(&str, &str)]) -> PathBuf {
    let root = PathBuf::from("/lsp-did-open-test");
    h.fake_fs().insert_files(
        files
            .iter()
            .map(|(rel, content)| (root.join(rel), content.as_bytes())),
    );
    h.stoat.active_workspace_mut().git_root = root.clone();
    root
}

/// Open `path` in the focused pane and settle, so whatever the open armed has
/// resolved before the test asserts.
pub(crate) fn open_buffer(h: &mut TestHarness, path: PathBuf) {
    action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
    h.settle();
}

/// The screen rect of whatever holds focus, split pane or dock, so a test aims
/// a click at the same cells the paint used.
pub(crate) fn focused_editor_pane_area(h: &TestHarness) -> Rect {
    let ws = h.stoat.active_workspace();
    match ws.focus {
        FocusTarget::SplitPane => ws.panes.pane(ws.panes.focus()).area,
        FocusTarget::Dock(dock_id) => ws.docks.get(dock_id).expect("dock").area,
    }
}

/// A synthetic mouse event at a screen cell, for a test driving the router
/// through the same entry the terminal uses.
pub(crate) fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

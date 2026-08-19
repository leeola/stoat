use super::focused_pane_jumplist;
use crate::{
    app::{Stoat, UpdateEffect},
    keymap_state::{active_modal, ActiveModal},
    pane::{FocusTarget, View},
};

/// Step the open list modal's selection by `delta` rows.
///
/// One verb for every small picker, since they differ only in which state
/// holds the list. A modal with no list, or none open at all, does nothing.
pub(crate) fn picker_step(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    /// Every modal's selection speaks the same delta, so one dispatch serves
    /// a key that steps one row and a click that jumps to a chosen one.
    macro_rules! shift {
        ($field:expr) => {
            match $field {
                Some(picker) => {
                    picker.move_selection(delta);
                    UpdateEffect::Redraw
                },
                None => UpdateEffect::None,
            }
        };
    }

    match active_modal(stoat) {
        Some(ActiveModal::Jumplist) => shift!(stoat.jumplist_picker.as_mut()),
        Some(ActiveModal::Diagnostics) => shift!(stoat.diagnostics_picker.as_mut()),
        Some(ActiveModal::Location) => shift!(stoat.location_picker.as_mut()),
        Some(ActiveModal::WorkspacePicker) => shift!(stoat.workspace_picker.as_mut()),
        Some(ActiveModal::CodeSearch) => shift!(stoat.code_search.as_mut()),
        Some(ActiveModal::FileFinder) => super::file_finder_move_selection(stoat, delta),
        Some(ActiveModal::SymbolFinder) => {
            crate::symbol_finder::symbol_finder_move_selection(stoat, delta)
        },
        Some(ActiveModal::CommitPicker) => super::review_walk::commit_picker_step(stoat, delta),
        Some(ActiveModal::Palette) => {
            super::palette::palette_move_selection(stoat, delta).unwrap_or(UpdateEffect::Redraw)
        },
        Some(ActiveModal::Help) => super::help::help_move(stoat, delta),
        _ => UpdateEffect::None,
    }
}

/// Page the open list modal's selection by half its visible rows in `dir`.
pub(super) fn picker_page(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    match active_modal(stoat) {
        Some(ActiveModal::Jumplist) => jumplist_picker_page(stoat, dir),
        Some(ActiveModal::Diagnostics) => diagnostics_picker_page(stoat, dir),
        Some(ActiveModal::Location) => location_picker_page(stoat, dir),
        Some(ActiveModal::WorkspacePicker) => super::workspace::workspace_picker_page(stoat, dir),
        Some(ActiveModal::FileFinder) => super::file_finder::file_finder_page(stoat, dir),
        Some(ActiveModal::SymbolFinder) => crate::symbol_finder::symbol_finder_page(stoat, dir),
        Some(ActiveModal::CommitPicker) => super::review_walk::commit_picker_page(stoat, dir),
        Some(ActiveModal::CodeSearch) => super::code_search::code_search_page(stoat, dir),
        Some(ActiveModal::Palette) => super::palette::palette_page(stoat, dir),
        Some(ActiveModal::Help) => super::help::help_page(stoat, dir),
        _ => UpdateEffect::None,
    }
}

/// Fill the open list modal's prompt from its selection.
///
/// Only a modal whose prompt names what the list holds has something to fill.
/// The rest do nothing rather than reporting an error.
pub(super) fn picker_complete(stoat: &mut Stoat) -> UpdateEffect {
    match active_modal(stoat) {
        Some(ActiveModal::WorkspacePicker) => super::workspace::workspace_picker_complete(stoat),
        Some(ActiveModal::FileFinder) => super::file_finder::file_finder_complete(stoat),
        Some(ActiveModal::SymbolFinder) => crate::symbol_finder::symbol_finder_complete(stoat),
        Some(ActiveModal::Palette) => super::palette::palette_complete(stoat),
        Some(ActiveModal::Help) => super::help::help_complete(stoat),
        _ => UpdateEffect::None,
    }
}

/// Scroll the open list modal's preview pane half a pane in `dir`.
///
/// Each modal holds its preview differently. Help and the commit picker keep
/// their own row offset, since neither preview is a buffer. The rest back one
/// with a real editor and scroll it the way an editor pane scrolls.
///
/// The pane's height comes from the same surfaces lookup the pointer uses, so
/// a half-pane step matches the pane on screen.
pub(super) fn picker_detail(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    let Some(rows) = crate::mouse::modal_preview_rows(stoat) else {
        return UpdateEffect::None;
    };
    let step = (rows / 2).max(1) as i32 * dir;

    let editor = match active_modal(stoat) {
        Some(ActiveModal::Help) => {
            return match dir > 0 {
                true => super::help::help_scroll_detail_down(stoat),
                false => super::help::help_scroll_detail_up(stoat),
            };
        },
        Some(ActiveModal::CommitPicker) => {
            let Some(picker) = stoat.commit_picker.as_mut() else {
                return UpdateEffect::None;
            };
            let rows = step.unsigned_abs() as usize;
            picker.preview_scroll = match dir > 0 {
                true => picker.preview_scroll.saturating_add(rows),
                false => picker.preview_scroll.saturating_sub(rows),
            };
            return UpdateEffect::Redraw;
        },
        Some(ActiveModal::FileFinder) => stoat
            .file_finder
            .as_ref()
            .map(|f| f.active_core_ref().preview.editor),
        Some(ActiveModal::CodeSearch) => stoat.code_search.as_ref().map(|f| f.preview.editor),
        Some(ActiveModal::SymbolFinder) => stoat.symbol_finder.as_ref().map(|f| f.preview.editor),
        Some(ActiveModal::Palette) => stoat
            .command_palette
            .as_ref()
            .and_then(|p| p.arg_picker.as_ref())
            .map(|p| p.active_core_ref().preview.editor),
        _ => None,
    };

    let Some(editor_id) = editor else {
        return UpdateEffect::None;
    };
    let Some(editor) = stoat.active_workspace_mut().editors.get_mut(editor_id) else {
        return UpdateEffect::None;
    };
    let scroll_row = editor.scroll_row.saturating_add_signed(step);
    editor.scroll_row = scroll_row;
    editor.scroll_offset = scroll_row as f32;
    editor.scroll_glide = crate::editor_state::ScrollGlide::Page;
    UpdateEffect::Redraw
}

/// Move the open list modal's selection to its first or last row.
///
/// Only help binds these today. Every other picker keeps its prompt in insert
/// mode, where g and G reach the list as typed text.
pub(super) fn picker_end(stoat: &mut Stoat, last: bool) -> UpdateEffect {
    match (active_modal(stoat), last) {
        (Some(ActiveModal::Help), false) => super::help::help_jump_first(stoat),
        (Some(ActiveModal::Help), true) => super::help::help_jump_last(stoat),
        _ => UpdateEffect::None,
    }
}

/// Page the jumplist picker's selection by half its visible rows in `dir`.
pub(super) fn jumplist_picker_page(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    if let Some(picker) = stoat.jumplist_picker.as_mut() {
        picker.page(dir);
    }
    UpdateEffect::Redraw
}

pub(super) fn jumplist_picker_close(stoat: &mut Stoat) -> UpdateEffect {
    stoat.jumplist_picker = None;
    UpdateEffect::Redraw
}

/// Jump to the location under the jumplist picker's selection, positioning the
/// walk cursor at the chosen entry so a later backward/forward resumes from it.
/// An empty picker just closes.
pub(super) fn jumplist_picker_select(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.jumplist_picker.take() else {
        return UpdateEffect::None;
    };
    let idx = picker.selected();
    let Some(entry) =
        focused_pane_jumplist(stoat).and_then(|jumplist| jumplist.entries().get(idx).cloned())
    else {
        return UpdateEffect::Redraw;
    };
    super::jump::apply_jump_entry(stoat, entry);
    if let Some(jumplist) = focused_pane_jumplist(stoat) {
        jumplist.set_cursor(idx);
    }
    UpdateEffect::Redraw
}

/// Page the diagnostics picker's selection by half its visible rows in `dir`.
pub(super) fn diagnostics_picker_page(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    if let Some(picker) = stoat.diagnostics_picker.as_mut() {
        picker.page(dir);
    }
    UpdateEffect::Redraw
}

pub(super) fn diagnostics_picker_close(stoat: &mut Stoat) -> UpdateEffect {
    stoat.diagnostics_picker = None;
    UpdateEffect::Redraw
}

/// Move the focused editor's cursor to the diagnostic under the picker's
/// selection. Workspace-scope entries carry a target path and a sentinel
/// offset, so the file is opened first and the byte offset recomputed
/// from the entry's `(line, column)`. An empty picker just closes.
pub(super) fn diagnostics_picker_select(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.diagnostics_picker.take() else {
        return UpdateEffect::None;
    };
    let idx = picker.selected();
    let Some(entry) = picker.entries().get(idx) else {
        return UpdateEffect::Redraw;
    };
    let path = entry.path.clone();
    let line = entry.line.saturating_sub(1);
    let column = entry.column.saturating_sub(1);
    let local_offset = entry.offset;
    let encoding = entry.encoding;

    super::jump::push_jump(stoat);
    let offset = match path {
        Some(path) => {
            super::file::open_file(stoat, &path);
            stoat
                .offset_for_focused_point(line, column, encoding)
                .unwrap_or(0)
        },
        None => local_offset,
    };
    stoat.collapse_focused_cursor_to(offset);
    UpdateEffect::Redraw
}

/// Page the goto-location picker's selection by half its visible rows in `dir`.
pub(super) fn location_picker_page(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    if let Some(picker) = stoat.location_picker.as_mut() {
        picker.page(dir);
    }
    UpdateEffect::Redraw
}

pub(super) fn location_picker_close(stoat: &mut Stoat) -> UpdateEffect {
    stoat.location_picker = None;
    UpdateEffect::Redraw
}

/// Jump the focused editor to the goto candidate under the picker's
/// selection, reusing the same apply path a single-location goto takes.
/// An empty picker just closes.
pub(crate) fn location_picker_select(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.location_picker.take() else {
        return UpdateEffect::None;
    };
    let Some(entry) = picker.entries().get(picker.selected()).cloned() else {
        return UpdateEffect::Redraw;
    };
    super::lsp::apply_jump(stoat, &entry.path, entry.offset);
    UpdateEffect::Redraw
}

/// Drive [`ActionKind::OpenJumplistPicker`]. Builds a snapshot of the focused
/// pane's jumplist and stores it in [`Stoat::jumplist_picker`]. No-op when the
/// jumplist is empty or focus is on a dock.
pub(super) fn open_jumplist_picker(stoat: &mut Stoat) -> UpdateEffect {
    let Some(jumplist) = focused_pane_jumplist(stoat) else {
        return UpdateEffect::None;
    };
    if jumplist.entries().is_empty() {
        return UpdateEffect::None;
    }
    let jumplist = jumplist.clone();
    let picker =
        crate::jumplist_picker::JumplistPicker::new(&jumplist, &stoat.active_workspace().buffers);
    stoat.jumplist_picker = Some(picker);
    UpdateEffect::Redraw
}

/// Drive [`ActionKind::OpenWorkspaceDiagnosticsPicker`].
/// Snapshots every `(path, diagnostic)` pair currently in
/// `Stoat::diagnostics` and stores the workspace-scope picker
/// on `Stoat::diagnostics_picker`. No-op when the workspace
/// has no diagnostics loaded.
pub(super) fn open_workspace_diagnostics_picker(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.diagnostics.iter().next().is_none() {
        return UpdateEffect::None;
    }
    let encodings = stoat.lsp_registry.offset_encodings();
    let picker =
        crate::diagnostics_picker::DiagnosticsPicker::workspace(&stoat.diagnostics, &encodings);
    if picker.entries().is_empty() {
        return UpdateEffect::None;
    }
    stoat.diagnostics_picker = Some(picker);
    UpdateEffect::Redraw
}

/// Drive [`ActionKind::OpenDiagnosticsPicker`]. Snapshots the
/// focused buffer's diagnostic list from `Stoat::diagnostics`
/// and stores the picker on `Stoat::diagnostics_picker`. No-op
/// when the focused pane is not an editor, the buffer has no
/// path on disk, or the path has no diagnostics.
pub(super) fn open_diagnostics_picker(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let editor_id = match ws.focus {
        FocusTarget::SplitPane => match ws.panes.pane(ws.panes.focus()).view {
            View::Editor(id) => id,
            _ => return UpdateEffect::None,
        },
        FocusTarget::Dock(_) => return UpdateEffect::None,
    };
    let buffer_id = match ws.editors.get(editor_id) {
        Some(e) => e.buffer_id,
        None => return UpdateEffect::None,
    };
    let path = match ws.buffers.path_for(buffer_id) {
        Some(p) => p.to_path_buf(),
        None => return UpdateEffect::None,
    };
    let encodings = stoat.lsp_registry.offset_encodings();
    let diagnostics: Vec<(crate::host::OffsetEncoding, lsp_types::Diagnostic)> = stoat
        .diagnostics
        .attributed(&path)
        .map(|(server, diag)| {
            let encoding = encodings
                .get(server)
                .copied()
                .unwrap_or(crate::host::OffsetEncoding::Utf16);
            (encoding, diag.clone())
        })
        .collect();
    if diagnostics.is_empty() {
        return UpdateEffect::None;
    }
    let ws = stoat.active_workspace_mut();
    let buffer = match ws.buffers.get(buffer_id) {
        Some(b) => b,
        None => return UpdateEffect::None,
    };
    let picker = {
        let guard = buffer.read().expect("buffer poisoned");
        crate::diagnostics_picker::DiagnosticsPicker::new(&diagnostics, &guard)
    };
    stoat.diagnostics_picker = Some(picker);
    UpdateEffect::Redraw
}

#[cfg(test)]
mod tests {
    use super::{super::dispatch, *};
    use crate::test_harness::{editor, keys, stoat};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use stoat_action::{MoveDown, SaveSelection};

    /// One verb set serves every list modal, so the verbs reach the dispatch
    /// whether or not a modal is open. With none open there is no list to
    /// step, and reporting a redraw repaints the frame for nothing.
    #[test]
    fn a_picker_verb_with_no_modal_open_is_noop() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "alpha\nbeta\n");
        assert_eq!(
            (
                dispatch(&mut stoat, &stoat_action::PickerNext),
                dispatch(&mut stoat, &stoat_action::PickerPrev),
                dispatch(&mut stoat, &stoat_action::PickerPageDown),
                dispatch(&mut stoat, &stoat_action::PickerComplete),
            ),
            (
                UpdateEffect::None,
                UpdateEffect::None,
                UpdateEffect::None,
                UpdateEffect::None
            ),
            "no open list leaves every picker verb with nothing to do"
        );
    }

    #[test]
    fn open_jumplist_picker_with_empty_jumplist_is_noop() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "alpha\nbeta\n");
        assert_eq!(
            dispatch(&mut stoat, &stoat_action::OpenJumplistPicker),
            UpdateEffect::None
        );
        assert!(stoat.jumplist_picker.is_none());
    }

    #[test]
    fn open_jumplist_picker_opens_modal_when_jumplist_has_entries() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "alpha\nbeta\ngamma\n");
        dispatch(&mut stoat, &SaveSelection);
        dispatch(&mut stoat, &MoveDown);
        dispatch(&mut stoat, &SaveSelection);
        assert_eq!(
            dispatch(&mut stoat, &stoat_action::OpenJumplistPicker),
            UpdateEffect::Redraw
        );
        let picker = stoat.jumplist_picker.as_ref().expect("modal open");
        assert_eq!(picker.entries().len(), 2);
    }

    #[test]
    fn jumplist_picker_enter_jumps_focused_cursor() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("alpha\nbeta\ngamma\n");
        dispatch(&mut h.stoat, &SaveSelection);
        dispatch(&mut h.stoat, &MoveDown);
        dispatch(&mut h.stoat, &MoveDown);
        dispatch(&mut h.stoat, &SaveSelection);
        dispatch(&mut h.stoat, &stoat_action::OpenJumplistPicker);
        h.stoat.update(Event::Key(keys::key(KeyCode::Up)));
        assert_eq!(
            h.stoat.update(Event::Key(keys::key(KeyCode::Enter))),
            UpdateEffect::Redraw
        );
        assert!(h.stoat.jumplist_picker.is_none());
        assert_eq!(editor::head_offsets(&mut h.stoat), vec![0]);
    }

    #[test]
    fn jumplist_picker_esc_closes_without_jumping() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("alpha\nbeta\n");
        dispatch(&mut h.stoat, &MoveDown);
        let before = editor::head_offsets(&mut h.stoat);
        dispatch(&mut h.stoat, &SaveSelection);
        dispatch(&mut h.stoat, &stoat_action::OpenJumplistPicker);
        assert_eq!(
            h.stoat.update(Event::Key(keys::key(KeyCode::Esc))),
            UpdateEffect::Redraw
        );
        assert!(h.stoat.jumplist_picker.is_none());
        assert_eq!(editor::head_offsets(&mut h.stoat), before);
    }

    #[test]
    fn jumplist_picker_ctrl_c_closes_without_jumping() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("alpha\nbeta\n");
        dispatch(&mut h.stoat, &SaveSelection);
        dispatch(&mut h.stoat, &stoat_action::OpenJumplistPicker);
        let event = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(h.stoat.update(Event::Key(event)), UpdateEffect::Redraw);
        assert!(h.stoat.jumplist_picker.is_none());
    }
}

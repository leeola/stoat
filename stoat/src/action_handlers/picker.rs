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
pub(super) fn picker_step(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    let forward = delta > 0;
    match active_modal(stoat) {
        Some(ActiveModal::Jumplist) => match forward {
            true => jumplist_picker_next(stoat),
            false => jumplist_picker_prev(stoat),
        },
        Some(ActiveModal::Diagnostics) => match forward {
            true => diagnostics_picker_next(stoat),
            false => diagnostics_picker_prev(stoat),
        },
        Some(ActiveModal::Location) => match forward {
            true => location_picker_next(stoat),
            false => location_picker_prev(stoat),
        },
        Some(ActiveModal::WorkspacePicker) => match forward {
            true => super::workspace::workspace_picker_next(stoat),
            false => super::workspace::workspace_picker_prev(stoat),
        },
        Some(ActiveModal::FileFinder) => super::file_finder_move_selection(stoat, delta),
        Some(ActiveModal::SymbolFinder) => {
            crate::symbol_finder::symbol_finder_move_selection(stoat, delta)
        },
        Some(ActiveModal::CommitPicker) => super::review_walk::commit_picker_step(stoat, delta),
        Some(ActiveModal::CodeSearch) => match forward {
            true => super::code_search::code_search_next(stoat),
            false => super::code_search::code_search_prev(stoat),
        },
        Some(ActiveModal::Palette) => match forward {
            true => super::prompt::palette_select_next(stoat),
            false => super::prompt::palette_select_prev(stoat),
        },
        Some(ActiveModal::Help) => match forward {
            true => super::help::help_select_next(stoat),
            false => super::help::help_select_prev(stoat),
        },
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

pub(super) fn jumplist_picker_next(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(picker) = stoat.jumplist_picker.as_mut() {
        picker.select_next();
    }
    UpdateEffect::Redraw
}

pub(super) fn jumplist_picker_prev(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(picker) = stoat.jumplist_picker.as_mut() {
        picker.select_prev();
    }
    UpdateEffect::Redraw
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

pub(super) fn diagnostics_picker_next(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(picker) = stoat.diagnostics_picker.as_mut() {
        picker.select_next();
    }
    UpdateEffect::Redraw
}

pub(super) fn diagnostics_picker_prev(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(picker) = stoat.diagnostics_picker.as_mut() {
        picker.select_prev();
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

pub(super) fn location_picker_next(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(picker) = stoat.location_picker.as_mut() {
        picker.select_next();
    }
    UpdateEffect::Redraw
}

pub(super) fn location_picker_prev(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(picker) = stoat.location_picker.as_mut() {
        picker.select_prev();
    }
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

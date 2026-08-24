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
        Some(ActiveModal::Jumplist) => {
            shift!(stoat.jumplist_picker.as_mut().map(|p| &mut p.picker))
        },
        Some(ActiveModal::Diagnostics) => {
            shift!(stoat.diagnostics_picker.as_mut().map(|p| &mut p.picker))
        },
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

/// Delete the open list modal's selected row and whatever it stands for.
///
/// Only the workspace picker lists rows with a life outside the modal. The
/// rest hold views onto state the picker does not own, so they do nothing
/// rather than reporting an error.
pub(super) fn picker_delete(stoat: &mut Stoat) -> UpdateEffect {
    match active_modal(stoat) {
        Some(ActiveModal::WorkspacePicker) => super::workspace::workspace_picker_delete(stoat),
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
        _ => crate::mouse::modal_preview_editor(stoat),
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
    if let Some(picker) = stoat.jumplist_picker.take() {
        picker.dispose(stoat.active_workspace_mut());
    }
    UpdateEffect::Redraw
}

/// Jump to the location under the jumplist picker's selection, positioning the
/// walk cursor at the chosen entry so a later backward/forward resumes from it.
/// An empty picker just closes.
pub(super) fn jumplist_picker_select(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.jumplist_picker.take() else {
        return UpdateEffect::None;
    };
    let idx = picker.selected_index();
    picker.dispose(stoat.active_workspace_mut());
    let Some(idx) = idx else {
        return UpdateEffect::Redraw;
    };
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
    if let Some(picker) = stoat.diagnostics_picker.take() {
        picker.dispose(stoat.active_workspace_mut());
    }
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
    let target = picker.selected_entry().map(|entry| {
        (
            entry.path.clone(),
            entry.line.saturating_sub(1),
            entry.column.saturating_sub(1),
            entry.offset,
            entry.encoding,
        )
    });
    picker.dispose(stoat.active_workspace_mut());
    let Some((path, line, column, local_offset, encoding)) = target else {
        return UpdateEffect::Redraw;
    };

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

/// Re-rank the diagnostics picker for what is typed and sync its preview.
///
/// Driven once per frame beside the other picker syncs, so typing narrows the
/// list and the pane follows the selection without an action of its own.
pub(crate) fn sync_diagnostics_picker(stoat: &mut Stoat) {
    if stoat.diagnostics_picker.is_none() {
        return;
    }
    let query = {
        let ws = stoat.active_workspace();
        stoat
            .diagnostics_picker
            .as_ref()
            .expect("diagnostics_picker present")
            .picker
            .input
            .text(ws)
    };

    let active_idx = stoat.active_workspace;
    let ws = &mut stoat.workspaces[active_idx];
    let fs_host = &*stoat.fs_host;
    let language_registry = &stoat.language_registry;
    let Some(picker) = stoat.diagnostics_picker.as_mut() else {
        return;
    };
    picker.picker.refilter(&query);
    if picker.picker.preview_current() {
        return;
    }

    // Resolving reads the workspace and the sync writes it, so the target is
    // in hand before the picker takes its mutable borrow.
    let scope = picker.scope();
    let target = picker
        .selected_entry()
        .and_then(|entry| crate::diagnostics_picker::diagnostic_target(ws, scope, entry));
    picker
        .picker
        .sync_preview(ws, fs_host, language_registry, target);
}

/// Re-rank the location picker for what is typed and sync its preview.
///
/// Driven once per frame beside the other picker syncs, so typing narrows the
/// candidates and the pane follows the selection without an action of its own.
pub(crate) fn sync_location_picker(stoat: &mut Stoat) {
    if stoat.location_picker.is_none() {
        return;
    }
    let query = {
        let ws = stoat.active_workspace();
        stoat
            .location_picker
            .as_ref()
            .expect("location_picker present")
            .input
            .text(ws)
    };

    let active_idx = stoat.active_workspace;
    let ws = &mut stoat.workspaces[active_idx];
    let fs_host = &*stoat.fs_host;
    let language_registry = &stoat.language_registry;
    let Some(picker) = stoat.location_picker.as_mut() else {
        return;
    };
    picker.refilter(&query);
    if picker.preview_current() {
        return;
    }

    // Resolving reads the workspace and the sync writes it, so the target is
    // in hand before the picker takes its mutable borrow.
    let target = picker
        .selected_entry()
        .and_then(|entry| crate::location_picker::location_target(ws, entry));
    picker.sync_preview(ws, fs_host, language_registry, target);
}

pub(super) fn location_picker_close(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(picker) = stoat.location_picker.take() {
        picker.dispose(stoat.active_workspace_mut());
    }
    UpdateEffect::Redraw
}

/// Jump the focused editor to the goto candidate under the picker's
/// selection, reusing the same apply path a single-location goto takes.
/// An empty picker just closes.
pub(crate) fn location_picker_select(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.location_picker.take() else {
        return UpdateEffect::None;
    };
    let entry = picker.selected_entry().cloned();
    picker.dispose(stoat.active_workspace_mut());
    let Some(entry) = entry else {
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
    let entries = crate::jumplist_picker::JumplistPicker::entries_from(
        &jumplist,
        &stoat.active_workspace().buffers,
    );

    let executor = stoat.executor.clone();
    stoat.set_focused_mode("insert".into());
    let ws = stoat.active_workspace_mut();
    let input = crate::input_view::InputView::create(
        ws,
        executor.clone(),
        crate::input_view::SubmitTarget::JumplistPicker,
        "",
        "insert",
        1,
    );
    let preview = crate::picker::Preview::new(ws, executor);
    stoat.jumplist_picker = Some(crate::jumplist_picker::JumplistPicker::from_entries(
        entries,
        jumplist.cursor(),
        input,
        preview,
    ));
    UpdateEffect::Redraw
}

/// Re-rank the jumplist picker for what is typed and sync its preview.
///
/// Driven once per frame beside the other picker syncs, so typing narrows the
/// list and the pane follows the selection without an action of its own.
pub(crate) fn sync_jumplist_picker(stoat: &mut Stoat) {
    if stoat.jumplist_picker.is_none() {
        return;
    }
    let query = {
        let ws = stoat.active_workspace();
        stoat
            .jumplist_picker
            .as_ref()
            .expect("jumplist_picker present")
            .picker
            .input
            .text(ws)
    };

    let active_idx = stoat.active_workspace;
    let ws = &mut stoat.workspaces[active_idx];
    let fs_host = &*stoat.fs_host;
    let language_registry = &stoat.language_registry;
    let Some(picker) = stoat.jumplist_picker.as_mut() else {
        return;
    };
    picker.picker.refilter(&query);
    if picker.picker.preview_current() {
        return;
    }

    // Resolving reads the workspace and the sync writes it, so the target is
    // in hand before the picker takes its mutable borrow.
    let target = picker
        .picker
        .selected_entry()
        .and_then(|entry| crate::jumplist_picker::jumplist_target(ws, entry));
    picker
        .picker
        .sync_preview(ws, fs_host, language_registry, target);
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
    let entries = crate::diagnostics_picker::DiagnosticsPicker::workspace_entries(
        &stoat.diagnostics,
        &encodings,
    );
    if entries.is_empty() {
        return UpdateEffect::None;
    }
    stoat.diagnostics_picker = Some(build_diagnostics_picker(
        stoat,
        entries,
        crate::diagnostics_picker::PickerScope::Workspace,
    ));
    UpdateEffect::Redraw
}

/// Wrap `entries` in a diagnostics picker with its prompt focused.
///
/// The input opens in insert mode so the reader narrows the list by typing,
/// the way every other target list works.
fn build_diagnostics_picker(
    stoat: &mut Stoat,
    entries: Vec<crate::diagnostics_picker::DiagnosticsEntry>,
    scope: crate::diagnostics_picker::PickerScope,
) -> crate::diagnostics_picker::DiagnosticsPicker {
    let executor = stoat.executor.clone();
    stoat.set_focused_mode("insert".into());
    let ws = stoat.active_workspace_mut();
    let input = crate::input_view::InputView::create(
        ws,
        executor.clone(),
        crate::input_view::SubmitTarget::DiagnosticsPicker,
        "",
        "insert",
        1,
    );
    let preview = crate::picker::Preview::new(ws, executor);
    crate::diagnostics_picker::DiagnosticsPicker::from_entries(entries, scope, input, preview)
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
    let entries = {
        let guard = buffer.read().expect("buffer poisoned");
        crate::diagnostics_picker::DiagnosticsPicker::local_entries(&diagnostics, &guard)
    };
    stoat.diagnostics_picker = Some(build_diagnostics_picker(
        stoat,
        entries,
        crate::diagnostics_picker::PickerScope::Local(path),
    ));
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

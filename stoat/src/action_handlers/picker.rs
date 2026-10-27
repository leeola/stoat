use super::focused_pane_jumplist;
use crate::{
    app::{Stoat, UpdateEffect},
    buffer_registry::OpenOrigin,
    file_finder::OpenIntent,
    keymap_state::{active_modal, ActiveModal},
    pane::{FocusTarget, View},
    pane_finder::PaneTarget,
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
        Some(ActiveModal::WalkthroughPicker) => {
            shift!(stoat.walkthrough_picker.as_mut().map(|p| &mut p.picker))
        },
        Some(ActiveModal::PaneFinder) => shift!(stoat.pane_finder.as_mut().map(|f| &mut f.picker)),
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
        Some(ActiveModal::WalkthroughPicker) => walkthrough_picker_page(stoat, dir),
        Some(ActiveModal::PaneFinder) => pane_finder_page(stoat, dir),
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

/// Turn the open list modal's filter into a rename of its selected row.
///
/// Only the workspace picker's rows carry a name of their own. The rest list
/// files, symbols, and actions whose names the picker does not own, so they do
/// nothing rather than reporting an error.
pub(super) fn picker_rename(stoat: &mut Stoat) -> UpdateEffect {
    match active_modal(stoat) {
        Some(ActiveModal::WorkspacePicker) => {
            super::workspace::workspace_picker_rename_start(stoat)
        },
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
    editor.scroll_frac = 0.0;
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

/// Move the open list modal's selection to the row at 0-based `index`, clamped
/// to its last row.
///
/// A count before G lands here, so only help reaches it, for the reason
/// [`picker_end`] gives.
pub(super) fn picker_nth(stoat: &mut Stoat, index: usize) -> UpdateEffect {
    match active_modal(stoat) {
        Some(ActiveModal::Help) => super::help::help_jump_nth(stoat, index),
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
    let Some(mut picker) = stoat.jumplist_picker.take() else {
        return UpdateEffect::None;
    };
    let query = picker.picker.input.text(stoat.active_workspace());
    picker.picker.settle_rank(&query);
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

/// Page the walkthrough picker's selection by half its visible rows in `dir`.
pub(super) fn walkthrough_picker_page(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    if let Some(picker) = stoat.walkthrough_picker.as_mut() {
        picker.page(dir);
    }
    UpdateEffect::Redraw
}

pub(super) fn walkthrough_picker_close(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(picker) = stoat.walkthrough_picker.take() {
        picker.dispose(stoat.active_workspace_mut());
    }
    UpdateEffect::Redraw
}

/// Play the tour under the walkthrough picker's selection in the walkthrough
/// mode, so its stepping keys work at once. An empty picker just closes.
///
/// A tour that fails to open reports why and leaves the tour that was open in
/// place, which the mode then steps.
pub(super) fn walkthrough_picker_select(stoat: &mut Stoat) -> UpdateEffect {
    let Some(mut picker) = stoat.walkthrough_picker.take() else {
        return UpdateEffect::None;
    };
    let query = picker.picker.input.text(stoat.active_workspace());
    picker.picker.settle_rank(&query);
    let slug = picker.selected_slug().map(str::to_owned);
    picker.dispose(stoat.active_workspace_mut());
    let Some(slug) = slug else {
        return UpdateEffect::Redraw;
    };

    super::walkthrough::open(stoat, &slug);
    if stoat.active_workspace().walkthrough.is_some() {
        stoat.set_focused_mode("walkthrough".into());
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
    let Some(mut picker) = stoat.diagnostics_picker.take() else {
        return UpdateEffect::None;
    };
    let query = picker.picker.input.text(stoat.active_workspace());
    picker.picker.settle_rank(&query);
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
            super::file::open_file(stoat, &path, OpenOrigin::Visited);
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
    picker.picker.pump_rank();
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
    picker.pump_rank();
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
    let Some(mut picker) = stoat.location_picker.take() else {
        return UpdateEffect::None;
    };
    let query = picker.input.text(stoat.active_workspace());
    picker.settle_rank(&query);
    let entry = picker.selected_entry().cloned();
    picker.dispose(stoat.active_workspace_mut());
    let Some(entry) = entry else {
        return UpdateEffect::Redraw;
    };
    super::lsp::apply_jump(stoat, &entry.path, entry.offset, entry.block);
    UpdateEffect::Redraw
}

/// Drive [`ActionKind::OpenJumplistPicker`]. Builds a snapshot of the focused
/// pane's jumplist and stores it in [`Stoat::jumplist_picker`]. No-op when the
/// jumplist is empty or focus is on a dock.
///
/// The editor under the picker resets to normal mode, as the file finder's
/// does. Focus returns to that editor on close, and the editor keeps the mode
/// it held.
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
    let redraw = stoat.redraw_notify.clone();
    stoat.set_focused_mode("normal".into());
    let ws = stoat.active_workspace_mut();
    let input = crate::input_view::InputView::create(
        ws,
        executor.clone(),
        crate::input_view::SubmitTarget::JumplistPicker,
        "",
        "insert",
    );
    let preview = crate::picker::Preview::new(ws, executor.clone());
    stoat.jumplist_picker = Some(crate::jumplist_picker::JumplistPicker::from_entries(
        entries,
        jumplist.cursor(),
        input,
        preview,
        executor,
        redraw,
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

/// Drive [`ActionKind::OpenWalkthroughPicker`]. Lists the workspace's stored
/// walkthroughs in [`Stoat::walkthrough_picker`], with the selection on the
/// tour after the one that is open.
///
/// A workspace with no walkthroughs, or a store that fails to load, reports
/// why in the status line and opens nothing.
///
/// The editor under the picker resets to normal mode, as the jumplist
/// picker's does.
pub(super) fn open_walkthrough_picker(stoat: &mut Stoat) -> UpdateEffect {
    let loaded = {
        let ws = stoat.active_workspace();
        crate::walkthrough::store::load_all(stoat.fs_host.as_ref(), &ws.git_root)
    };
    let tours = match loaded {
        Ok(tours) if tours.is_empty() => {
            stoat.set_status("no walkthroughs in this workspace");
            return UpdateEffect::Redraw;
        },
        Ok(tours) => tours,
        Err(error) => {
            stoat.set_status(format!("{error}"));
            return UpdateEffect::Redraw;
        },
    };
    let playing = stoat
        .active_workspace()
        .walkthrough
        .as_ref()
        .map(|run| run.walkthrough.slug.clone());

    let executor = stoat.executor.clone();
    let redraw = stoat.redraw_notify.clone();
    stoat.set_focused_mode("normal".into());
    let ws = stoat.active_workspace_mut();
    let input = crate::input_view::InputView::create(
        ws,
        executor.clone(),
        crate::input_view::SubmitTarget::WalkthroughPicker,
        "",
        "insert",
    );
    let preview = crate::picker::Preview::new(ws, executor.clone());
    stoat.walkthrough_picker = Some(crate::walkthrough_picker::WalkthroughPicker::from_tours(
        tours,
        playing.as_deref(),
        input,
        preview,
        executor,
        redraw,
    ));
    UpdateEffect::Redraw
}

/// Re-rank the walkthrough picker for what is typed and sync its preview.
///
/// Driven once per frame beside the other picker syncs, so typing narrows the
/// list and the pane follows the selection without an action of its own.
pub(crate) fn sync_walkthrough_picker(stoat: &mut Stoat) {
    if stoat.walkthrough_picker.is_none() {
        return;
    }
    let query = {
        let ws = stoat.active_workspace();
        stoat
            .walkthrough_picker
            .as_ref()
            .expect("walkthrough_picker present")
            .picker
            .input
            .text(ws)
    };

    let active_idx = stoat.active_workspace;
    let ws = &mut stoat.workspaces[active_idx];
    let fs_host = &*stoat.fs_host;
    let language_registry = &stoat.language_registry;
    let Some(picker) = stoat.walkthrough_picker.as_mut() else {
        return;
    };
    picker.picker.refilter(&query);
    if picker.picker.preview_current() {
        return;
    }

    let target = picker
        .picker
        .selected_entry()
        .map(crate::walkthrough_picker::walkthrough_target);
    picker
        .picker
        .sync_preview(ws, fs_host, language_registry, target);
}

/// Drive [`ActionKind::OpenPaneFinder`]. Lists every view the workspace holds
/// in [`Stoat::pane_finder`].
///
/// A workspace with nothing to list reports it in the status line and opens
/// nothing.
///
/// The editor under the finder resets to normal mode, as the jumplist
/// picker's does.
pub(super) fn open_pane_finder(stoat: &mut Stoat) -> UpdateEffect {
    let rows = crate::pane_finder::rows(stoat.active_workspace());
    if rows.is_empty() {
        stoat.set_status("no panes to list");
        return UpdateEffect::Redraw;
    }

    let executor = stoat.executor.clone();
    let redraw = stoat.redraw_notify.clone();
    stoat.set_focused_mode("normal".into());
    let ws = stoat.active_workspace_mut();
    let input = crate::input_view::InputView::create(
        ws,
        executor.clone(),
        crate::input_view::SubmitTarget::PaneFinder,
        "",
        "insert",
    );
    let preview = crate::picker::Preview::new(ws, executor.clone());
    stoat.pane_finder = Some(crate::pane_finder::PaneFinder::from_rows(
        rows, input, preview, executor, redraw,
    ));
    UpdateEffect::Redraw
}

/// Re-rank the pane finder for what is typed and sync its preview.
///
/// Driven once per frame beside the other picker syncs, so typing narrows the
/// list and the pane follows the selection without an action of its own.
pub(crate) fn sync_pane_finder(stoat: &mut Stoat) {
    if stoat.pane_finder.is_none() {
        return;
    }
    let query = {
        let ws = stoat.active_workspace();
        stoat
            .pane_finder
            .as_ref()
            .expect("pane_finder present")
            .picker
            .input
            .text(ws)
    };

    let active_idx = stoat.active_workspace;
    let ws = &mut stoat.workspaces[active_idx];
    let fs_host = &*stoat.fs_host;
    let language_registry = &stoat.language_registry;
    let Some(finder) = stoat.pane_finder.as_mut() else {
        return;
    };
    finder.picker.refilter(&query);
    finder.picker.pump_rank();
    if finder.picker.preview_current() {
        return;
    }

    // Resolving reads the workspace and the sync writes it, so the target is
    // in hand before the picker takes its mutable borrow.
    let target = finder
        .picker
        .selected_entry()
        .and_then(|entry| crate::pane_finder::pane_target(ws, entry));
    finder
        .picker
        .sync_preview(ws, fs_host, language_registry, target);
}

/// Page the pane finder's selection by half its visible rows in `dir`.
pub(super) fn pane_finder_page(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    if let Some(finder) = stoat.pane_finder.as_mut() {
        finder.page(dir);
    }
    UpdateEffect::Redraw
}

pub(super) fn pane_finder_close(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(finder) = stoat.pane_finder.take() {
        finder.dispose(stoat.active_workspace_mut());
    }
    UpdateEffect::Redraw
}

/// Bring the view under the pane finder's selection back on screen. An empty
/// finder just closes.
///
/// A buffer opens in the focused pane, as the buffer picker opens one. A
/// terminal or an agent takes focus where it shows, or comes into the focused
/// pane from behind the pane that covers it. Any other view takes focus at the
/// pane, tab, or dock that shows it, and a view closed since the finder opened
/// reports it in the status line.
pub(super) fn pane_finder_select(stoat: &mut Stoat) -> UpdateEffect {
    let Some(mut finder) = stoat.pane_finder.take() else {
        return UpdateEffect::None;
    };
    let query = finder.picker.input.text(stoat.active_workspace());
    finder.picker.settle_rank(&query);
    let target = finder.selected_row().map(|row| row.target.clone());
    finder.dispose(stoat.active_workspace_mut());
    let Some(target) = target else {
        return UpdateEffect::Redraw;
    };

    // FIXME: A view that only a detached pane shows has no location. Its row
    // reports `pane closed`, and a terminal's row shows the session in the
    // focused pane a second time.
    let view = match target {
        PaneTarget::Buffer { path, .. } => {
            return super::file_finder::dispatch_open_file(stoat, path);
        },
        PaneTarget::View(View::Terminal(id) | View::Agent(id)) => {
            return super::terminal::show_terminal(stoat, id, OpenIntent::Replace);
        },
        PaneTarget::View(view) => view,
    };
    let Some(at) = stoat.active_workspace().view_location(&view) else {
        stoat.set_status("pane closed");
        return UpdateEffect::Redraw;
    };
    stoat.reveal_location(at);
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
///
/// The editor under the picker resets to normal mode, as the file finder's
/// does. The picker opens from the `space l` leader, and the editor keeps the
/// mode it held when focus returns to it on close.
fn build_diagnostics_picker(
    stoat: &mut Stoat,
    entries: Vec<crate::diagnostics_picker::DiagnosticsEntry>,
    scope: crate::diagnostics_picker::PickerScope,
) -> crate::diagnostics_picker::DiagnosticsPicker {
    let executor = stoat.executor.clone();
    let redraw = stoat.redraw_notify.clone();
    stoat.set_focused_mode("normal".into());
    let ws = stoat.active_workspace_mut();
    let input = crate::input_view::InputView::create(
        ws,
        executor.clone(),
        crate::input_view::SubmitTarget::DiagnosticsPicker,
        "",
        "insert",
    );
    let preview = crate::picker::Preview::new(ws, executor.clone());
    crate::diagnostics_picker::DiagnosticsPicker::from_entries(
        entries, scope, input, preview, executor, redraw,
    )
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
    use crate::{
        run::OutputBlock,
        test_fixture::{open_scratch_file, store_two_tours},
        test_harness::{editor, keys, stoat, TestHarness},
    };
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use stoat_action::{Action, MoveDown, SaveSelection};

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

    /// A harness over [`store_two_tours`] with the walkthrough picker open.
    fn walkthrough_picker_over_two_tours() -> TestHarness {
        let mut h = Stoat::test();
        store_two_tours(&mut h);
        dispatch(&mut h.stoat, &stoat_action::OpenWalkthroughPicker);
        h
    }

    fn playing_slug(h: &TestHarness) -> Option<&str> {
        let run = h.stoat.active_workspace().walkthrough.as_ref()?;
        Some(run.walkthrough.slug.as_str())
    }

    #[test]
    fn open_walkthrough_picker_lists_the_tours_in_slug_order() {
        let h = walkthrough_picker_over_two_tours();
        let picker = h.stoat.walkthrough_picker.as_ref().expect("modal open");
        let rows: Vec<_> = picker
            .entries()
            .iter()
            .map(|row| {
                (
                    row.slug.as_str(),
                    row.title.as_str(),
                    row.stops,
                    &*row.preview,
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                (
                    "alpha",
                    "Alpha tour",
                    1,
                    "# Alpha tour\n\nWhere it **starts**.\n\n1 stop\n\n1. entry: a.rs 1:1-1:11\n"
                ),
                (
                    "zebra",
                    "Zebra tour",
                    2,
                    "# Zebra tour\n\n2 stops\n\n1. a.rs: a.rs 1:1-1:11\n2. a.rs: a.rs 2:1-2:11\n"
                ),
            ]
        );
    }

    #[test]
    fn open_walkthrough_picker_with_no_tours_reports_it() {
        let mut h = Stoat::test();
        dispatch(&mut h.stoat, &stoat_action::OpenWalkthroughPicker);
        assert_eq!(
            (
                h.stoat.walkthrough_picker.is_none(),
                h.stoat.pending_message.as_deref()
            ),
            (true, Some("no walkthroughs in this workspace")),
        );
    }

    /// The reader picked a tour to read, so its stepping keys work at once.
    #[test]
    fn enter_plays_the_picked_tour_in_walkthrough_mode() {
        let mut h = walkthrough_picker_over_two_tours();
        h.type_keys("down enter");
        assert_eq!(
            (
                h.stoat.walkthrough_picker.is_none(),
                playing_slug(&h),
                h.stoat.focused_mode()
            ),
            (true, Some("zebra"), "walkthrough"),
        );
    }

    /// A reader walks the tours in a row, so Enter alone moves on to the next
    /// one, and the last wraps to the first.
    #[test]
    fn the_selection_starts_after_the_playing_tour() {
        let selected_with = |slug: &str| {
            let mut h = Stoat::test();
            store_two_tours(&mut h);
            dispatch(
                &mut h.stoat,
                &stoat_action::WalkthroughOpen {
                    slug: slug.to_owned(),
                },
            );
            dispatch(&mut h.stoat, &stoat_action::OpenWalkthroughPicker);
            let picker = h.stoat.walkthrough_picker.as_ref().expect("modal open");
            (
                picker.selected_slug().map(str::to_owned),
                picker.playing_idx(),
            )
        };
        assert_eq!(
            [selected_with("alpha"), selected_with("zebra")],
            [
                (Some("zebra".to_owned()), Some(0)),
                (Some("alpha".to_owned()), Some(1))
            ],
        );
    }

    #[test]
    fn escape_closes_without_opening() {
        let mut h = walkthrough_picker_over_two_tours();
        h.type_keys("escape");
        assert_eq!(
            (h.stoat.walkthrough_picker.is_none(), playing_slug(&h)),
            (true, None),
        );
    }

    /// The view the focused pane shows.
    fn focused_view(h: &TestHarness) -> View {
        let ws = h.stoat.active_workspace();
        ws.panes.pane(ws.panes.focus()).view.clone()
    }

    #[test]
    fn open_pane_finder_with_nothing_to_list_reports_it() {
        let mut h = Stoat::test();
        dispatch(&mut h.stoat, &stoat_action::OpenPaneFinder);
        assert_eq!(
            (
                h.stoat.pane_finder.is_none(),
                h.stoat.pending_message.as_deref()
            ),
            (true, Some("no panes to list")),
        );
    }

    /// A run takes focus through its place, and a terminal through
    /// `show_terminal`, and both lead back to the parked tab that shows them.
    #[test]
    fn enter_on_a_run_or_terminal_row_switches_to_the_tab_that_shows_it() {
        let lands_on_its_tab = |open: &dyn Action, query: &str| {
            let mut h = Stoat::test();
            dispatch(&mut h.stoat, open);
            let shown = focused_view(&h);
            dispatch(&mut h.stoat, &stoat_action::NewTab);
            dispatch(&mut h.stoat, &stoat_action::OpenPaneFinder);

            h.type_text(query);
            h.type_keys("enter");
            (
                h.stoat.pane_finder.is_none(),
                h.stoat.active_workspace().active_tab,
                focused_view(&h) == shown,
            )
        };
        assert_eq!(
            [
                lands_on_its_tab(&stoat_action::OpenRun, "run"),
                lands_on_its_tab(&stoat_action::Terminal, "term"),
            ],
            [(true, 0, true); 2],
        );
    }

    /// A buffer row opens where the reader stands, as the buffer picker opens
    /// one, rather than moving focus to a pane that shows it.
    #[test]
    fn enter_on_a_buffer_row_opens_it_in_the_focused_pane() {
        let mut h = Stoat::test();
        let path = open_scratch_file(&mut h, "");
        dispatch(&mut h.stoat, &stoat_action::SplitNewRight);
        let split = h.stoat.active_workspace().panes.focus();
        dispatch(&mut h.stoat, &stoat_action::OpenPaneFinder);

        h.type_keys("enter");

        let (_, buffer_id) = h.stoat.focused_editor_ids().expect("focused editor");
        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                h.stoat.pane_finder.is_none(),
                ws.panes.focus(),
                ws.buffers.path_for(buffer_id)
            ),
            (true, split, Some(path.as_path())),
        );
    }

    /// A terminal row previews the screen as plain text, since program output
    /// has no language to highlight it with.
    #[test]
    fn a_terminal_row_previews_the_screen() {
        let mut h = Stoat::test();
        dispatch(&mut h.stoat, &stoat_action::Terminal);
        let View::Terminal(term_id) = focused_view(&h) else {
            panic!("the terminal action shows a shell");
        };
        h.stoat.active_workspace_mut().terms[term_id]
            .term
            .feed(b"hello");
        dispatch(&mut h.stoat, &stoat_action::OpenPaneFinder);

        h.snapshot();

        assert_eq!(finder_preview(&h), ("hello".to_string(), None));
    }

    /// A run row previews each block as its command line and the output rows
    /// the run pane shows, with a blank line between blocks and no language.
    #[test]
    fn a_run_row_previews_its_blocks() {
        let mut h = Stoat::test();
        let run_id = h.open_run();
        let run = &mut h.stoat.active_workspace_mut().runs[run_id];
        for (command, output) in [("echo hi", "hi\r\n"), ("pwd", "/ws"), ("sleep 9", "")] {
            let mut block = OutputBlock::new(command.into(), run.cwd.clone(), 80);
            block.feed(output.as_bytes());
            run.blocks.push(block);
        }
        dispatch(&mut h.stoat, &stoat_action::OpenPaneFinder);

        h.snapshot();

        assert_eq!(
            finder_preview(&h),
            ("$ echo hi\nhi\n\n$ pwd\n/ws\n\n$ sleep 9".to_string(), None),
        );
    }

    /// The pane finder's preview as its text and the name of the language that
    /// paints it.
    fn finder_preview(h: &TestHarness) -> (String, Option<&'static str>) {
        let preview = h
            .stoat
            .pane_finder
            .as_ref()
            .expect("finder open")
            .picker
            .preview
            .buffer;
        let buffers = &h.stoat.active_workspace().buffers;
        let text = buffers
            .get(preview)
            .expect("preview buffer")
            .read()
            .expect("preview buffer poisoned")
            .rope()
            .to_string();
        (
            text,
            buffers.language_for(preview).map(|language| language.name),
        )
    }
}

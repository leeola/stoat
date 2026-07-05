use crate::app::{Stoat, UpdateEffect};
use stoat_action::OpenFile;

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

pub(super) fn jumplist_picker_close(stoat: &mut Stoat) -> UpdateEffect {
    stoat.jumplist_picker = None;
    UpdateEffect::Redraw
}

/// Jump the focused editor to the location under the jumplist picker's
/// selection, recording the picker's cursor index so the jumplist walk
/// resumes from the chosen entry. An empty picker just closes.
pub(super) fn jumplist_picker_select(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.jumplist_picker.take() else {
        return UpdateEffect::None;
    };
    let idx = picker.selected();
    let Some(entry) = picker.entries().get(idx) else {
        return UpdateEffect::Redraw;
    };
    let offset = entry.offset;
    stoat.jump_focused_to_offset(offset, idx);
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

    let offset = match path {
        Some(path) => {
            super::file::open_file(stoat, &path);
            stoat.offset_for_focused_point(line, column).unwrap_or(0)
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

pub(super) fn location_picker_close(stoat: &mut Stoat) -> UpdateEffect {
    stoat.location_picker = None;
    UpdateEffect::Redraw
}

/// Jump the focused editor to the goto candidate under the picker's
/// selection, reusing the same apply path a single-location goto takes.
/// An empty picker just closes.
pub(super) fn location_picker_select(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.location_picker.take() else {
        return UpdateEffect::None;
    };
    let Some(entry) = picker.entries().get(picker.selected()).cloned() else {
        return UpdateEffect::Redraw;
    };
    super::lsp::apply_jump(stoat, &entry.path, entry.offset);
    UpdateEffect::Redraw
}

pub(super) fn global_search_picker_next(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(picker) = stoat.global_search.as_mut() {
        picker.select_next();
    }
    UpdateEffect::Redraw
}

pub(super) fn global_search_picker_prev(stoat: &mut Stoat) -> UpdateEffect {
    if let Some(picker) = stoat.global_search.as_mut() {
        picker.select_prev();
    }
    UpdateEffect::Redraw
}

pub(super) fn global_search_picker_close(stoat: &mut Stoat) -> UpdateEffect {
    stoat.global_search = None;
    UpdateEffect::Redraw
}

/// Open the file under the global-search picker's selection and jump to
/// the match. An empty picker just closes.
pub(super) fn global_search_picker_select(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.global_search.take() else {
        return UpdateEffect::None;
    };
    let Some(m) = picker.matches().get(picker.selected()) else {
        return UpdateEffect::Redraw;
    };
    let path = m.path.clone();
    let offset = m.offset;

    super::dispatch(stoat, &OpenFile { path });
    stoat.jump_focused_to_match_offset(offset);
    UpdateEffect::Redraw
}

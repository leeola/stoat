use crate::app::{Stoat, UpdateEffect};

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

/// Jump to the location under the jumplist picker's selection, positioning the
/// walk cursor at the chosen entry so a later backward/forward resumes from it.
/// An empty picker just closes.
pub(super) fn jumplist_picker_select(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.jumplist_picker.take() else {
        return UpdateEffect::None;
    };
    let idx = picker.selected();
    let Some(entry) = super::focused_pane_jumplist(stoat)
        .and_then(|jumplist| jumplist.entries().get(idx).cloned())
    else {
        return UpdateEffect::Redraw;
    };
    super::jump::apply_jump_entry(stoat, entry);
    if let Some(jumplist) = super::focused_pane_jumplist(stoat) {
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

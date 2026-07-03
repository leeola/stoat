use crate::{
    command_palette::{ArgPicker, CommandPalette, PaletteScope},
    file_finder::display_row,
    input_view::InputView,
    render::{
        editor::render_editor,
        text::{wrap_text, write_str, write_str_clipped},
    },
    workspace::Workspace,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Borders, Clear, Widget},
};

const LIST_ROWS: u16 = 10;
const DOC_ROWS: u16 = 6;

/// The on-screen rectangles of the command-palette filter modal, derived from a
/// terminal `area` by [`palette_filter_layout`].
///
/// The box height is constant rather than content-sized, so the modal stays put
/// as the selection and filter change. Shared by the renderer and the
/// smooth-scroll emit so the pooled list region matches the painted one exactly.
pub(crate) struct PaletteFilterLayout {
    /// The bordered modal box.
    pub(crate) modal: Rect,
    /// Inside the border: prompt/input, separator, list, doc separator, doc.
    pub(crate) inner: Rect,
    /// The scrolling result list, also the smooth-scroll pool region.
    pub(crate) list: Rect,
    /// The selected entry's documentation, below a separator under the list.
    pub(crate) doc: Rect,
}

/// Lay out the command-palette filter modal within `area`, or `None` when
/// `area` is too small to host it.
///
/// The box height is the constant `1+1+1+LIST_ROWS+1+DOC_ROWS+1`, clamped to
/// `area.height - 4`. When clamped the list keeps its rows and the doc region
/// shrinks first, since the list is the primary content.
pub(crate) fn palette_filter_layout(area: Rect) -> Option<PaletteFilterLayout> {
    if area.width < 30 || area.height < 10 {
        return None;
    }

    let box_width = 80u16.min(area.width.saturating_sub(4));
    if box_width < 20 {
        return None;
    }

    let full_height = 1 + 1 + 1 + LIST_ROWS + 1 + DOC_ROWS + 1;
    let box_height = full_height.min(area.height.saturating_sub(4));

    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let modal = Rect::new(x, y, box_width, box_height);
    let inner = Block::default().borders(Borders::ALL).inner(modal);

    let body = inner.height.saturating_sub(3);
    let list_height = LIST_ROWS.min(body);
    let doc_height = DOC_ROWS.min(body.saturating_sub(list_height));

    let list = Rect::new(inner.x, inner.y + 2, inner.width, list_height);
    let doc = Rect::new(inner.x, list.y + list_height + 1, inner.width, doc_height);

    Some(PaletteFilterLayout {
        modal,
        inner,
        list,
        doc,
    })
}

pub(crate) fn render_command_palette(
    palette: &mut CommandPalette,
    ws: &mut Workspace,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: Option<&mut stoatty_widgets::ApcScene>,
) {
    if palette.arg_picker.is_some() && palette.arg_source().is_some() {
        render_palette_arg_picker(palette, ws, theme, area, buf, scene);
        return;
    }

    let scope = palette.scope();
    if palette.command.is_none()
        && let Some(layout) = palette_filter_layout(area)
    {
        palette.viewport_rows = Some(layout.list.height as usize);
    }

    render_palette_filter(
        &palette.input,
        &palette.filtered,
        &palette.match_indices,
        palette.selected,
        scope,
        ws,
        theme,
        area,
        buf,
        scene,
    );
}

/// Render the inline file picker shown while collecting a `Files` argument
/// (e.g. `:o `).
///
/// Reuses the filter modal's box and `:` input row unchanged, then replaces the
/// command list + doc body with a result list beside a live preview, mirroring
/// the standalone file finder. State is synced before the frame by
/// [`crate::action_handlers::sync_palette_picker`], so this only paints.
fn render_palette_arg_picker(
    palette: &mut CommandPalette,
    ws: &mut Workspace,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    mut scene: Option<&mut stoatty_widgets::ApcScene>,
) {
    let Some(layout) = palette_filter_layout(area) else {
        return;
    };
    let entry = palette.command.expect("arg picker requires a command");

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PALETTE);
    let title = format!(" {} ", entry.def.name());
    Clear.render(layout.modal, buf);
    crate::render::chrome::modal_frame(
        buf,
        layout.modal,
        Some(title.as_str()),
        modal_style,
        theme,
        scene.as_deref_mut(),
    );

    let inner = layout.inner;
    let prompt_style = theme.get(crate::theme::scope::UI_PROMPT);
    let separator_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);

    let input_row = inner.y;
    write_str(buf, inner.x, input_row, ":", prompt_style);
    let input_area = Rect::new(inner.x + 2, input_row, inner.width.saturating_sub(2), 1);
    palette.input.render(
        &mut ws.editors,
        input_area,
        true,
        "prompt",
        theme,
        &std::collections::BTreeMap::new(),
        buf,
    );

    let separator_row = inner.y + 1;
    crate::render::chrome::hline(
        buf,
        inner.x,
        separator_row,
        inner.width,
        separator_style,
        scene.as_deref_mut(),
    );

    let body_top = inner.y + 2;
    let body_height = (inner.y + inner.height).saturating_sub(body_top);
    if body_height == 0 {
        return;
    }
    let (list, preview) = arg_body_split(inner.x, body_top, inner.width, body_height);

    let Some(picker) = palette.arg_picker.as_mut() else {
        return;
    };
    if let Some(preview_rect) = preview {
        crate::render::chrome::vline(
            buf,
            list.x + list.width,
            list.y,
            list.height,
            separator_style,
            scene,
        );
        render_arg_preview(picker, preview_rect, theme, ws, buf);
    }

    picker.picklist.viewport_rows = Some(list.height as usize);
    paint_arg_rows(picker, list, theme, buf);
}

/// Split the picker body into a result list and an optional preview pane.
///
/// The preview only appears when the body is wide enough to host a useful list
/// and preview side by side. Below that the list takes the full width.
fn arg_body_split(x: u16, y: u16, width: u16, height: u16) -> (Rect, Option<Rect>) {
    if width >= 50 {
        let list_width = (width * 40 / 100).max(20);
        let preview_width = width.saturating_sub(list_width + 1);
        (
            Rect::new(x, y, list_width, height),
            Some(Rect::new(x + list_width + 1, y, preview_width, height)),
        )
    } else {
        (Rect::new(x, y, width, height), None)
    }
}

/// Paint the picker's result rows into `area`, one repo-relative path per row,
/// with the selected row and fuzzy-match characters highlighted.
fn paint_arg_rows(picker: &ArgPicker, area: Rect, theme: &crate::theme::Theme, buf: &mut Buffer) {
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }
    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let selected_style = theme.get(crate::theme::scope::UI_SELECTION);
    let match_style = theme.get(crate::theme::scope::UI_SEARCH_MATCH);

    let picklist = &picker.picklist;
    let start_row = picklist.selected.saturating_sub(rows.saturating_sub(1));
    let end_x = area.x + area.width;
    let label_x = area.x + 1;

    for (row_idx, (&idx, indices)) in picklist
        .filtered
        .iter()
        .zip(picklist.match_indices.iter())
        .skip(start_row)
        .take(rows)
        .enumerate()
    {
        let row = area.y + row_idx as u16;
        let is_selected = start_row + row_idx == picklist.selected;
        let style = if is_selected {
            selected_style
        } else {
            row_style
        };
        for col in area.x..end_x {
            buf[(col, row)].set_char(' ').set_style(style);
        }
        let label = display_row(&picklist.base[idx], &picker.git_root);
        write_str_clipped(buf, label_x, row, &label, style, end_x);
        for (label_col, _) in label.chars().enumerate() {
            let col = label_x + label_col as u16;
            if col >= end_x {
                break;
            }
            if indices.binary_search(&(label_col as u32)).is_ok() {
                buf[(col, row)].set_style(match_style);
            }
        }
    }
}

fn render_arg_preview(
    picker: &ArgPicker,
    area: Rect,
    theme: &crate::theme::Theme,
    ws: &mut Workspace,
    buf: &mut Buffer,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let fallback = theme.get(crate::theme::scope::UI_TEXT);
    if let Some(editor) = ws.editors.get_mut(picker.preview.editor) {
        render_editor(editor, area, fallback, theme, buf, false);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_palette_filter(
    input: &InputView,
    filtered: &[&'static stoat_action::registry::RegistryEntry],
    match_indices: &[Vec<u32>],
    selected: usize,
    scope: PaletteScope,
    ws: &mut Workspace,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    mut scene: Option<&mut stoatty_widgets::ApcScene>,
) {
    let Some(layout) = palette_filter_layout(area) else {
        return;
    };

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PALETTE);
    let title = match scope {
        PaletteScope::Active => " command palette (applicable) ",
        PaletteScope::All => " command palette (all) ",
    };
    Clear.render(layout.modal, buf);
    crate::render::chrome::modal_frame(
        buf,
        layout.modal,
        Some(title),
        modal_style,
        theme,
        scene.as_deref_mut(),
    );

    let inner = layout.inner;
    let prompt_style = theme.get(crate::theme::scope::UI_PROMPT);
    let separator_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);

    let input_row = inner.y;
    write_str(buf, inner.x, input_row, ":", prompt_style);

    let input_area = Rect::new(inner.x + 2, input_row, inner.width.saturating_sub(2), 1);
    input.render(
        &mut ws.editors,
        input_area,
        true,
        "prompt",
        theme,
        &std::collections::BTreeMap::new(),
        buf,
    );

    let separator_row = inner.y + 1;
    crate::render::chrome::hline(
        buf,
        inner.x,
        separator_row,
        inner.width,
        separator_style,
        scene.as_deref_mut(),
    );

    let list = layout.list;
    let scroll = selected.saturating_sub(list.height.saturating_sub(1) as usize);
    paint_palette_rows(filtered, match_indices, selected, list, scroll, theme, buf);

    let doc = layout.doc;
    if doc.height > 0 {
        let doc_separator_row = doc.y - 1;
        crate::render::chrome::hline(
            buf,
            inner.x,
            doc_separator_row,
            inner.width,
            separator_style,
            scene,
        );
        let doc_lines = filtered
            .get(selected)
            .map(|e| wrap_text(e.def.long_desc(), inner.width as usize))
            .unwrap_or_default();
        let doc_style = theme.get(crate::theme::scope::UI_TEXT_DIM);
        for (i, line) in doc_lines.iter().take(doc.height as usize).enumerate() {
            write_str(buf, doc.x, doc.y + i as u16, line, doc_style);
        }
    }
}

/// Paint command-palette result rows into `area` starting at `start_row`, one
/// row per entry, with the selected row, fuzzy-match characters, and each
/// entry's short description highlighted.
///
/// Shared by the live list, which derives `start_row` from the selection, and
/// the smooth-scroll pool, which paints absolute pages, so both render
/// identical rows.
pub(crate) fn paint_palette_rows(
    filtered: &[&'static stoat_action::registry::RegistryEntry],
    match_indices: &[Vec<u32>],
    selected: usize,
    area: Rect,
    start_row: usize,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }

    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let selected_style = theme.get(crate::theme::scope::UI_SELECTION);
    let desc_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let match_style = theme.get(crate::theme::scope::UI_SEARCH_MATCH);

    let name_col_width: usize = filtered
        .iter()
        .skip(start_row)
        .take(rows)
        .map(|e| e.def.name().len())
        .max()
        .unwrap_or(0);

    let end_x = area.x + area.width;
    let empty_indices: Vec<u32> = Vec::new();

    for (row_idx, entry) in filtered.iter().skip(start_row).take(rows).enumerate() {
        let abs = start_row + row_idx;
        let row = area.y + row_idx as u16;
        let is_selected = abs == selected;
        let style = if is_selected {
            selected_style
        } else {
            row_style
        };

        for col in area.x..end_x {
            buf[(col, row)].set_char(' ').set_style(style);
        }

        let name = entry.def.name();
        let name_x = area.x + 1;
        write_str(buf, name_x, row, name, style);
        let indices = match_indices.get(abs).unwrap_or(&empty_indices);
        for (name_col, _) in name.chars().enumerate() {
            let col = name_x + name_col as u16;
            if col >= end_x {
                break;
            }
            if indices.binary_search(&(name_col as u32)).is_ok() {
                buf[(col, row)].set_style(match_style);
            }
        }
        let desc_col = area.x + 1 + name_col_width as u16 + 2;
        if desc_col < end_x {
            let desc_style = if is_selected { style } else { desc_style };
            write_str(buf, desc_col, row, entry.def.short_desc(), desc_style);
        }
    }
}

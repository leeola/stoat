use crate::{
    command_palette::{CommandPalette, PaletteScope},
    input_view::InputView,
    render::text::{wrap_text, write_str, write_str_clipped},
    workspace::Workspace,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Borders, Clear, Widget},
};
use stoat_action::registry::RegistryEntry;

const LIST_ROWS: u16 = 10;
const DOC_ROWS: u16 = 6;

/// Box rows the chrome takes around the list. Two borders, the `:` input, the
/// separator under it, the doc separator, and the doc region itself.
const CHROME_ROWS: u16 = 5 + DOC_ROWS;

/// Widest the box may be, and the width it holds at on any terminal with room.
///
/// Hard cap, not a preference. The minimap band is left visible while the
/// palette is open precisely because this keeps the box's right edge at
/// `area.width / 2 + 40`, disjoint from the band at every width where a band
/// exists. See the note on `modal_overlay_open` in [`crate::render`].
const MAX_WIDTH: u16 = 80;

/// The on-screen rectangles of the command-palette filter modal, derived from a
/// terminal `area` by [`palette_filter_layout`].
///
/// The box height follows the candidate set measured when the palette opened,
/// never the filtered rows, so the modal stays put as the selection and filter
/// change. Shared by the renderer and the smooth-scroll emit so the pooled list
/// region matches the painted one exactly.
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
/// `content_rows` is the rows the caller's whole candidate list would need,
/// measured from the unfiltered set when the palette opened. The box holds at
/// `LIST_ROWS` for a short list and grows for a long one, up to half the area
/// so the palette never swallows the screen. `zoom` steps it further, in tenths
/// of the area.
///
/// Only the height responds to either. The width stays at [`MAX_WIDTH`], since
/// the minimap band beside the palette depends on that cap.
///
/// A box too short for everything shrinks the doc region first and the list
/// last, since the list is the primary content.
pub(crate) fn palette_filter_layout(
    area: Rect,
    content_rows: u16,
    zoom: i8,
) -> Option<PaletteFilterLayout> {
    if area.width < 30 || area.height < 10 {
        return None;
    }

    let box_width = MAX_WIDTH.min(area.width.saturating_sub(4));
    if box_width < 20 {
        return None;
    }

    let content_height = content_rows
        .saturating_add(CHROME_ROWS)
        .min(area.height / 2);
    let sized = crate::render::chrome::modal_box(
        area,
        (box_width, content_height),
        (box_width, LIST_ROWS + CHROME_ROWS),
        (20, 6),
        zoom,
    )?;

    // modal_box zooms both dimensions, but a wider palette would paint over the
    // minimap band left visible beside it, so the width is put back afterwards
    // and the box re-centered around it.
    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let modal = Rect::new(x, sized.y, box_width, sized.height);
    let inner = Block::default().borders(Borders::ALL).inner(modal);

    let body = inner.height.saturating_sub(3);
    let list_height = body.saturating_sub(DOC_ROWS).max(LIST_ROWS).min(body);
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

/// The arg-picker result-list rect, which is the smooth-scroll pool region for
/// argument mode (`:o `/`:cd `/`:b `). `None` when the modal does not fit or the
/// body has no rows.
///
/// Shared with [`render_palette_arg_picker`] through [`arg_body_split`] so the
/// pooled region and the painted list are the same rect.
pub(crate) fn palette_arg_list_rect(area: Rect, content_rows: u16, zoom: i8) -> Option<Rect> {
    palette_arg_body(area, content_rows, zoom).map(|(list, _)| list)
}

/// The arg-picker body split into its result-list rect and optional preview
/// rect, sharing [`arg_body_split`] with the painter so hit-testing and
/// rendering agree. `None` when the modal does not fit or the body has no rows.
pub(crate) fn palette_arg_body(
    area: Rect,
    content_rows: u16,
    zoom: i8,
) -> Option<(Rect, Option<Rect>)> {
    let layout = palette_filter_layout(area, content_rows, zoom)?;
    arg_body_split(layout.inner)
}

/// Split the arg-picker body below the `:` input into a result-list rect and an
/// optional preview rect. `None` when the body has no rows.
fn arg_body_split(inner: Rect) -> Option<(Rect, Option<Rect>)> {
    let body_top = inner.y + 2;
    let body_height = (inner.y + inner.height).saturating_sub(body_top);
    if body_height == 0 {
        return None;
    }
    Some(crate::render::picker::split_list_preview(
        inner.x,
        body_top,
        inner.width,
        body_height,
        50,
        20,
    ))
}

pub(crate) fn render_command_palette(
    palette: &mut CommandPalette,
    ws: &mut Workspace,
    theme: &crate::theme::Theme,
    area: Rect,
    zoom: i8,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    if palette.arg_picker.is_some() && palette.arg_source().is_some() {
        render_palette_arg_picker(palette, ws, theme, area, zoom, buf, scene);
        return;
    }

    // A parsed command whose argument has no inline picker (ValueSource::None)
    // shows a hint naming the argument instead of the emptied command list. Key
    // on the source, not picker presence -- a stale picker from an earlier
    // source may linger a frame and must not win.
    if let Some(entry) = palette.command
        && palette.arg_source().is_none()
    {
        render_palette_free_arg(palette, entry, ws, theme, area, zoom, buf, scene);
        return;
    }

    let scope = palette.scope();
    let content_rows = palette.list_rows_hint();
    if palette.command.is_none()
        && let Some(layout) = palette_filter_layout(area, content_rows, zoom)
    {
        palette.viewport_rows = Some(layout.list.height as usize);
    }

    render_palette_filter(
        &palette.input,
        &palette.filtered,
        &palette.match_indices,
        palette.selected,
        scope,
        content_rows,
        zoom,
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
    zoom: i8,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    let entry = palette.command.expect("arg picker requires a command");
    let Some(layout) =
        render_palette_command_prelude(palette, entry, ws, theme, area, zoom, buf, &mut *scene)
    else {
        return;
    };
    let inner = layout.inner;
    let separator_style = theme.get(crate::theme::scope::UI_BORDER_INACTIVE);

    let Some((list, preview)) = arg_body_split(inner) else {
        return;
    };

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
        crate::render::picker::render_picker_preview(
            &picker.active_core_ref().preview,
            preview_rect,
            theme,
            ws,
            buf,
        );
    }

    let prefix = picker
        .browse
        .as_ref()
        .map(|browse| browse.typed_dir.clone())
        .unwrap_or_default();
    picker.active_core().picklist.viewport_rows = Some(list.height as usize);
    let rows = list.height as usize;
    let core = picker.active_core_ref();
    let start_row = core
        .picklist
        .selected
        .saturating_sub(rows.saturating_sub(1));
    crate::render::picker::paint_path_rows(
        &core.picklist,
        &core.git_root,
        &prefix,
        list,
        start_row,
        theme,
        buf,
    );
}

/// Render the body for a free-typed argument, a command whose first argument
/// has no inline picker (`ValueSource::None`, e.g. `:RenameWorkspace `).
///
/// Names the parameter being collected -- its name and description, then the
/// command's long description -- so the modal describes what to type instead of
/// showing the emptied command list. State is synced before the frame, so this
/// only paints.
#[allow(clippy::too_many_arguments)]
fn render_palette_free_arg(
    palette: &mut CommandPalette,
    entry: &'static RegistryEntry,
    ws: &mut Workspace,
    theme: &crate::theme::Theme,
    area: Rect,
    zoom: i8,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    let Some(layout) =
        render_palette_command_prelude(palette, entry, ws, theme, area, zoom, buf, scene)
    else {
        return;
    };
    let inner = layout.inner;

    let body_top = inner.y + 2;
    let body_bottom = inner.y + inner.height;
    if body_top >= body_bottom {
        return;
    }

    let text_style = theme.get(crate::theme::scope::UI_TEXT);
    if let Some(param) = entry.def.params().first() {
        let hint = format!("{}  {}", param.name, param.description);
        write_str_clipped(
            buf,
            inner.x + 1,
            body_top,
            &hint,
            text_style,
            inner.x + inner.width,
        );
    }

    let doc_top = body_top + 2;
    if doc_top < body_bottom {
        let doc_style = theme.get(crate::theme::scope::UI_TEXT_DIM);
        let doc_lines = wrap_text(
            entry.def.long_desc(),
            inner.width.saturating_sub(1) as usize,
        );
        for (i, line) in doc_lines
            .iter()
            .take((body_bottom - doc_top) as usize)
            .enumerate()
        {
            write_str(buf, inner.x + 1, doc_top + i as u16, line, doc_style);
        }
    }
}

/// Render the shared chrome of a command-argument modal. The chrome is the
/// command-titled frame, the `:` prompt with the live input, and the separator
/// beneath it.
///
/// Returns the modal layout so the caller paints its body -- an inline picker or
/// the free-typed argument hint -- or `None` when the modal does not fit `area`.
///
/// See also:
/// - [`render_palette_arg_picker`] for the inline-picker body.
/// - [`render_palette_free_arg`] for the free-typed argument body.
#[allow(clippy::too_many_arguments)]
fn render_palette_command_prelude(
    palette: &mut CommandPalette,
    entry: &'static RegistryEntry,
    ws: &mut Workspace,
    theme: &crate::theme::Theme,
    area: Rect,
    zoom: i8,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) -> Option<PaletteFilterLayout> {
    let layout = palette_filter_layout(area, palette.list_rows_hint(), zoom)?;

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PALETTE);
    let title = format!(" {} ", entry.command_name);
    Clear.render(layout.modal, buf);
    crate::render::chrome::modal_frame(
        buf,
        layout.modal,
        Some(title.as_str()),
        modal_style,
        theme,
        &mut *scene,
    );

    let inner = layout.inner;
    let prompt_style = theme.get(crate::theme::scope::UI_PROMPT);
    let separator_style = theme.get(crate::theme::scope::UI_BORDER_INACTIVE);

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
        Some(scene),
    );

    Some(layout)
}

#[allow(clippy::too_many_arguments)]
fn render_palette_filter(
    input: &InputView,
    filtered: &[&'static RegistryEntry],
    match_indices: &[Vec<u32>],
    selected: usize,
    scope: PaletteScope,
    content_rows: u16,
    zoom: i8,
    ws: &mut Workspace,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    let Some(layout) = palette_filter_layout(area, content_rows, zoom) else {
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
        &mut *scene,
    );

    let inner = layout.inner;
    let prompt_style = theme.get(crate::theme::scope::UI_PROMPT);
    let separator_style = theme.get(crate::theme::scope::UI_BORDER_INACTIVE);

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
        Some(&mut *scene),
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
            Some(scene),
        );
        let doc_lines = filtered
            .get(selected)
            .map(|e| wrap_text(e.def.long_desc(), inner.width.saturating_sub(1) as usize))
            .unwrap_or_default();
        let doc_style = theme.get(crate::theme::scope::UI_TEXT_DIM);
        for (i, line) in doc_lines.iter().take(doc.height as usize).enumerate() {
            write_str(buf, doc.x + 1, doc.y + i as u16, line, doc_style);
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
    filtered: &[&'static RegistryEntry],
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
        .map(|e| e.command_name.len())
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

        let name = entry.command_name.as_str();
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

#[cfg(test)]
mod tests {
    use super::{palette_filter_layout, DOC_ROWS, LIST_ROWS, MAX_WIDTH};
    use ratatui::layout::Rect;

    fn layout(area: Rect, content_rows: u16, zoom: i8) -> super::PaletteFilterLayout {
        palette_filter_layout(area, content_rows, zoom).expect("the area hosts the palette")
    }

    #[test]
    fn a_short_command_list_keeps_the_recommended_box() {
        let laid = layout(Rect::new(0, 0, 200, 60), 4, 0);
        assert_eq!(
            (laid.modal.width, laid.modal.height),
            (MAX_WIDTH, 21),
            "a list under LIST_ROWS leaves the box at its recommended size"
        );
        assert_eq!(
            (laid.list.height, laid.doc.height),
            (LIST_ROWS, DOC_ROWS),
            "and the list and doc keep their fixed shares"
        );
    }

    #[test]
    fn a_long_command_list_grows_the_list_not_the_doc() {
        let laid = layout(Rect::new(0, 0, 200, 60), 18, 0);
        assert_eq!(
            laid.modal.height, 29,
            "eighteen rows plus eleven chrome rows size the box"
        );
        assert_eq!(
            (laid.list.height, laid.doc.height),
            (18, DOC_ROWS),
            "the extra rows all land in the list, leaving the doc at its share"
        );
    }

    #[test]
    fn the_box_never_takes_more_than_half_the_area() {
        // A `:o ` over thousands of files would otherwise swallow the screen.
        let laid = layout(Rect::new(0, 0, 200, 60), u16::MAX - 100, 0);
        assert_eq!(
            laid.modal.height, 30,
            "a list larger than the screen stops at half the area"
        );
    }

    #[test]
    fn zoom_moves_the_height_alone() {
        let area = Rect::new(0, 0, 200, 60);
        let grown = layout(area, 4, 2);
        let shrunk = layout(area, 4, -1);

        assert_eq!(
            (grown.modal.height, shrunk.modal.height),
            (33, 15),
            "each step moves the height by a tenth of the area"
        );
        assert_eq!(
            (grown.modal.width, shrunk.modal.width),
            (MAX_WIDTH, MAX_WIDTH),
            "but the width holds at the cap the minimap band depends on"
        );
    }

    #[test]
    fn a_box_too_short_for_everything_gives_up_the_doc_first() {
        // The list is the primary content, so a cramped terminal keeps its rows
        // and drops the documentation pane.
        let laid = layout(Rect::new(0, 0, 200, 16), 4, 0);
        assert_eq!(
            laid.modal.height, 12,
            "the box takes the area less its margin"
        );
        assert_eq!(
            (laid.list.height, laid.doc.height),
            (7, 0),
            "every remaining body row goes to the list"
        );
    }
}

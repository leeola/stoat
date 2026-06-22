use crate::{
    command_palette::{CommandPalette, PalettePhase, PaletteScope},
    input_view::InputView,
    render::text::{wrap_text, write_str},
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
) {
    palette.refilter_from_input(ws);
    let scope = palette.scope();

    match &mut palette.phase {
        PalettePhase::Filter {
            input,
            filtered,
            match_indices,
            selected,
        } => render_palette_filter(
            input,
            filtered,
            match_indices,
            *selected,
            scope,
            ws,
            theme,
            area,
            buf,
        ),
        PalettePhase::CollectArgs {
            entry,
            collected,
            current,
            input,
            error,
        } => render_palette_collect_args(
            PaletteCollect {
                entry,
                collected,
                current: *current,
                input,
                error: error.as_deref(),
            },
            ws,
            theme,
            area,
            buf,
        ),
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

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(modal_style)
        .title(title)
        .title_style(modal_style);
    block.render(layout.modal, buf);

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
    for col in inner.x..inner.x + inner.width {
        buf[(col, separator_row)]
            .set_char('─')
            .set_style(separator_style);
    }

    let list = layout.list;
    let scroll = selected.saturating_sub(list.height.saturating_sub(1) as usize);
    paint_palette_rows(filtered, match_indices, selected, list, scroll, theme, buf);

    let doc = layout.doc;
    if doc.height > 0 {
        let doc_separator_row = doc.y - 1;
        for col in inner.x..inner.x + inner.width {
            buf[(col, doc_separator_row)]
                .set_char('─')
                .set_style(separator_style);
        }
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

/// Per-frame state bundled so `render_palette_collect_args` stays under the
/// argument-count threshold.
struct PaletteCollect<'a> {
    entry: &'static stoat_action::registry::RegistryEntry,
    collected: &'a [stoat_action::ParamValue],
    current: usize,
    input: &'a InputView,
    error: Option<&'a str>,
}

fn render_palette_collect_args(
    state: PaletteCollect<'_>,
    ws: &mut Workspace,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    let PaletteCollect {
        entry,
        collected,
        current,
        input,
        error,
    } = state;
    if area.width < 30 || area.height < 10 {
        return;
    }

    let box_width = 80u16.min(area.width.saturating_sub(4));
    if box_width < 20 {
        return;
    }
    let inner_width = box_width.saturating_sub(2) as usize;

    let params = entry.def.params();
    let current_param = &params[current];
    let body_lines = wrap_text(current_param.description, inner_width);
    let body_height = body_lines.len() as u16;
    let doc_height = 1 + body_height;

    let collected_lines = collected.len() as u16;
    let error_lines: u16 = if error.is_some() { 1 } else { 0 };
    let box_height = 1 + collected_lines + 1 + error_lines + 1 + doc_height + 1;
    if box_height > area.height {
        return;
    }

    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let palette_area = Rect::new(x, y, box_width, box_height);

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PALETTE);
    let title = format!(" {} ", entry.def.name());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(modal_style)
        .title(title)
        .title_style(modal_style);
    let inner = block.inner(palette_area);
    block.render(palette_area, buf);

    let label_style = theme.get(crate::theme::scope::UI_PROMPT);
    let error_style = theme.get(crate::theme::scope::UI_ERROR);
    let muted_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);

    let mut row = inner.y;

    for (i, value) in collected.iter().enumerate() {
        let label = format!("{}: ", params[i].name);
        write_str(buf, inner.x, row, &label, muted_style);
        let value_col = inner.x + label.chars().count() as u16;
        write_str(buf, value_col, row, &format_param_value(value), muted_style);
        row += 1;
    }

    let label = format!("{}: ", current_param.name);
    write_str(buf, inner.x, row, &label, label_style);
    let value_col = inner.x + label.chars().count() as u16;
    let input_area = Rect::new(
        value_col,
        row,
        (inner.x + inner.width).saturating_sub(value_col),
        1,
    );
    input.render(
        &mut ws.editors,
        input_area,
        true,
        "prompt",
        theme,
        &std::collections::BTreeMap::new(),
        buf,
    );
    row += 1;

    if let Some(msg) = error {
        write_str(buf, inner.x, row, msg, error_style);
        row += 1;
    }

    let separator_row = row;
    for col in inner.x..inner.x + inner.width {
        buf[(col, separator_row)]
            .set_char('─')
            .set_style(muted_style);
    }
    let doc_top = separator_row + 1;

    let header = format!(
        "{} ({}{})",
        current_param.name,
        current_param.kind,
        if current_param.required {
            ", required"
        } else {
            ""
        },
    );
    write_str(buf, inner.x, doc_top, &header, muted_style);

    let body_top = doc_top + 1;
    let body_body_style = theme.get(crate::theme::scope::UI_TEXT_DIM);
    for (i, line) in body_lines.iter().enumerate() {
        write_str(buf, inner.x, body_top + i as u16, line, body_body_style);
    }
}

fn format_param_value(v: &stoat_action::ParamValue) -> String {
    match v {
        stoat_action::ParamValue::String(s) => s.clone(),
        stoat_action::ParamValue::Number(n) => n.to_string(),
        stoat_action::ParamValue::Bool(b) => b.to_string(),
    }
}

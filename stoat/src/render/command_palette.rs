use crate::{
    command_palette::{CommandPalette, PalettePhase},
    input_view::InputView,
    render::text::{wrap_text, write_str},
    workspace::Workspace,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Borders, Widget},
};

pub(crate) fn render_command_palette(
    palette: &mut CommandPalette,
    ws: &mut Workspace,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    palette.refilter_from_input(ws);

    match &mut palette.phase {
        PalettePhase::Filter {
            input,
            filtered,
            selected,
        } => render_palette_filter(input, filtered, *selected, ws, theme, area, buf),
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

fn render_palette_filter(
    input: &InputView,
    filtered: &[&'static stoat_action::registry::RegistryEntry],
    selected: usize,
    ws: &mut Workspace,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    if area.width < 30 || area.height < 10 {
        return;
    }

    let box_width = 80u16.min(area.width.saturating_sub(4));
    if box_width < 20 {
        return;
    }
    let inner_width = box_width.saturating_sub(2) as usize;
    let max_rows = 10u16;
    let row_count = (filtered.len() as u16).min(max_rows).max(1);

    let doc_lines: Vec<String> = filtered
        .get(selected)
        .map(|e| wrap_text(e.def.long_desc(), inner_width))
        .unwrap_or_default();
    let doc_height = doc_lines.len() as u16;
    let doc_section: u16 = if doc_height == 0 { 0 } else { doc_height + 1 };

    let box_height = 1 + 1 + 1 + row_count + doc_section + 1;
    if box_height > area.height {
        return;
    }

    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let palette_area = Rect::new(x, y, box_width, box_height);

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PALETTE);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(modal_style)
        .title(" command palette ")
        .title_style(modal_style);
    let inner = block.inner(palette_area);
    block.render(palette_area, buf);

    let prompt_style = theme.get(crate::theme::scope::UI_PROMPT);
    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let selected_style = theme.get(crate::theme::scope::UI_SELECTION);
    let desc_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);

    let input_row = inner.y;
    write_str(buf, inner.x, input_row, ":", prompt_style);

    let input_area = Rect::new(inner.x + 2, input_row, inner.width.saturating_sub(2), 1);
    input.render(&mut ws.editors, input_area, true, "prompt", theme, buf);

    let separator_row = inner.y + 1;
    let separator_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    for col in inner.x..inner.x + inner.width {
        buf[(col, separator_row)]
            .set_char('─')
            .set_style(separator_style);
    }

    let list_top = inner.y + 2;
    let name_col_width: usize = filtered
        .iter()
        .take(max_rows as usize)
        .map(|e| e.def.name().len())
        .max()
        .unwrap_or(0);

    for (i, entry) in filtered.iter().take(max_rows as usize).enumerate() {
        let row = list_top + i as u16;
        let is_selected = i == selected;
        let style = if is_selected {
            selected_style
        } else {
            row_style
        };

        for col in inner.x..inner.x + inner.width {
            buf[(col, row)].set_char(' ').set_style(style);
        }

        let name = entry.def.name();
        write_str(buf, inner.x + 1, row, name, style);
        let desc_col = inner.x + 1 + name_col_width as u16 + 2;
        if desc_col < inner.x + inner.width {
            let desc_style = if is_selected { style } else { desc_style };
            write_str(buf, desc_col, row, entry.def.short_desc(), desc_style);
        }
    }

    if doc_section > 0 {
        let doc_separator_row = list_top + row_count;
        for col in inner.x..inner.x + inner.width {
            buf[(col, doc_separator_row)]
                .set_char('─')
                .set_style(separator_style);
        }
        let doc_top = doc_separator_row + 1;
        let doc_style = theme.get(crate::theme::scope::UI_TEXT_DIM);
        for (i, line) in doc_lines.iter().enumerate() {
            write_str(buf, inner.x, doc_top + i as u16, line, doc_style);
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
    input.render(&mut ws.editors, input_area, true, "prompt", theme, buf);
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

use crate::{
    help::Help,
    render::{
        pane::mode_segment,
        text::{wrap_text, write_str, write_str_clipped},
    },
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{Block, Borders, Widget},
};

pub(crate) fn render_help(
    help: &Help,
    stoat_mode: &str,
    ws: &mut crate::workspace::Workspace,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    use crate::help::{help_input_mode, HelpInput, HelpScope};
    let input_mode = help_input_mode(stoat_mode);

    if area.width < 40 || area.height < 12 {
        return;
    }

    let box_width = 120u16.min(area.width.saturating_sub(4));
    let box_height = 36u16.min(area.height.saturating_sub(4));
    if box_width < 40 || box_height < 12 {
        return;
    }

    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let help_area = Rect::new(x, y, box_width, box_height);

    let title = match help.scope() {
        HelpScope::Active => format!(" help: active ({}) ", help.snapshot_mode()),
        HelpScope::All => " help: all actions ".to_string(),
    };
    let modal_style = theme.get(crate::theme::scope::UI_MODAL_HELP);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(modal_style)
        .title(title)
        .title_style(modal_style);
    let inner = block.inner(help_area);
    block.render(help_area, buf);

    let prompt_style = theme.get(crate::theme::scope::UI_PROMPT);
    let input_style = theme.get(crate::theme::scope::UI_TEXT);
    let cursor_style = theme.get(crate::theme::scope::UI_CURSOR_INPUT);
    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let key_style = theme.get(crate::theme::scope::UI_KEY_LABEL);
    let selected_style = theme.get(crate::theme::scope::UI_SELECTION);
    let heading = theme.get(crate::theme::scope::UI_HEADING);

    let search_row = inner.y;
    let prompt = match input_mode {
        HelpInput::Insert => "> ",
        HelpInput::Normal => ": ",
    };
    write_str(buf, inner.x, search_row, prompt, prompt_style);
    let input_text = help.input_text(ws);
    write_str(buf, inner.x + 2, search_row, &input_text, input_style);
    if matches!(input_mode, HelpInput::Insert) {
        let cursor_col = inner.x + 2 + help.input_cursor_column(ws) as u16;
        if cursor_col < inner.x + inner.width {
            buf[(cursor_col, search_row)]
                .set_char(' ')
                .set_style(cursor_style);
        }
    }
    let status_row = search_row + 1;
    let status_base = theme.get(crate::theme::scope::UI_STATUSBAR_FOCUSED);
    for col in inner.x..inner.x + inner.width {
        buf[(col, status_row)].set_char(' ').set_style(status_base);
    }
    let mode_str = match input_mode {
        HelpInput::Insert => "insert",
        HelpInput::Normal => "normal",
    };
    let (mode_label, mode_bg) = mode_segment(mode_str, theme);
    let mode_style = theme.get(crate::theme::scope::UI_MODE_LABEL).bg(mode_bg);
    let chip = format!(" {mode_label} ");
    write_str(buf, inner.x, status_row, &chip, mode_style);

    let body_top = status_row + 1;
    let body_height = (inner.y + inner.height).saturating_sub(body_top);
    if body_height == 0 {
        return;
    }

    let body_width = inner.width;
    let list_width = (body_width * 42 / 100).max(20);
    let detail_width = body_width.saturating_sub(list_width + 1);
    let list_rect = Rect::new(inner.x, body_top, list_width, body_height);
    let detail_rect = Rect::new(
        inner.x + list_width + 1,
        body_top,
        detail_width,
        body_height,
    );

    for row in list_rect.y..list_rect.y + list_rect.height {
        buf[(list_rect.x + list_rect.width, row)]
            .set_char('│')
            .set_style(muted);
    }

    render_help_list(
        help,
        list_rect,
        buf,
        row_style,
        selected_style,
        key_style,
        muted,
    );
    render_help_detail(help, detail_rect, buf, heading, row_style, muted, key_style);
}

fn render_help_list(
    help: &Help,
    area: Rect,
    buf: &mut Buffer,
    row_style: Style,
    selected_style: Style,
    key_style: Style,
    muted: Style,
) {
    let filtered = help.filtered();
    let entries = help.entries();
    let selected = help.selected();
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }
    let scroll = selected.saturating_sub(rows.saturating_sub(1));

    let key_col_width: usize = filtered
        .iter()
        .skip(scroll)
        .take(rows)
        .map(|&i| {
            entries[i]
                .key_label
                .as_deref()
                .map(|s| s.chars().count())
                .unwrap_or(0)
        })
        .max()
        .unwrap_or(0)
        .min(10);

    let row_end = area.x + area.width;
    for (row_idx, &entry_idx) in filtered.iter().skip(scroll).take(rows).enumerate() {
        let row = area.y + row_idx as u16;
        let entry = &entries[entry_idx];
        let is_selected = entry_idx == *filtered.get(selected).unwrap_or(&usize::MAX);
        let base = if is_selected {
            selected_style
        } else {
            row_style
        };

        for col in area.x..row_end {
            buf[(col, row)].set_char(' ').set_style(base);
        }

        let key_text = entry.key_label.as_deref().unwrap_or("");
        let padded = format!("{key_text:>width$}", width = key_col_width);
        let key_display_style = if is_selected { base } else { key_style };
        write_str_clipped(buf, area.x + 1, row, &padded, key_display_style, row_end);
        let name_col = area.x + 1 + key_col_width as u16 + 2;
        if name_col < row_end {
            write_str_clipped(buf, name_col, row, entry.def.name(), base, row_end);
        }
        let name_w = entry.def.name().chars().count() as u16;
        let desc_col = name_col + name_w + 2;
        if desc_col < row_end {
            let desc_style = if is_selected { base } else { muted };
            write_str_clipped(
                buf,
                desc_col,
                row,
                entry.def.short_desc(),
                desc_style,
                row_end,
            );
        }
    }
}

fn render_help_detail(
    help: &Help,
    area: Rect,
    buf: &mut Buffer,
    heading: Style,
    body_style: Style,
    muted: Style,
    key_style: Style,
) {
    let Some(entry) = help.selected_entry() else {
        return;
    };
    let width = area.width.saturating_sub(1) as usize;
    if width == 0 || area.height == 0 {
        return;
    }

    let mut lines: Vec<(String, Style)> = Vec::new();
    lines.push((entry.def.name().to_string(), heading));
    if let Some(label) = entry.key_label.as_deref() {
        lines.push((format!("bound: {label}"), key_style));
    } else {
        lines.push(("(unbound)".to_string(), muted));
    }
    lines.push((entry.def.short_desc().to_string(), body_style));
    lines.push((String::new(), body_style));

    for wrapped in wrap_text(entry.def.long_desc(), width) {
        lines.push((wrapped, body_style));
    }

    let params = entry.def.params();
    if !params.is_empty() {
        lines.push((String::new(), body_style));
        lines.push(("Parameters:".to_string(), heading));
        for p in params {
            let required = if p.required { "*" } else { "" };
            let head = format!("  {}{}: {}: {}", p.name, required, p.kind, p.description);
            for wrapped in wrap_text(&head, width) {
                lines.push((wrapped, body_style));
            }
        }
    }

    lines.push((String::new(), body_style));
    lines.push(("Example:".to_string(), heading));
    let example = format_example(entry);
    lines.push((format!("  {example}"), muted));

    let scroll = help.detail_scroll() as usize;
    let rows = area.height as usize;
    let end_x = area.x + area.width;
    for (row_idx, (text, style)) in lines.iter().skip(scroll).take(rows).enumerate() {
        let row = area.y + row_idx as u16;
        write_str_clipped(buf, area.x + 1, row, text, *style, end_x);
    }
}

fn format_example(entry: &crate::help::HelpEntry) -> String {
    let name = entry.def.name();
    let params = entry.def.params();
    if params.is_empty() {
        return format!("{name}()");
    }
    if !entry.bound_args.is_empty() {
        let args: Vec<String> = entry
            .bound_args
            .iter()
            .filter_map(crate::help::format_arg)
            .collect();
        return format!("{name}({})", args.join(", "));
    }
    let placeholders: Vec<String> = params.iter().map(|p| p.name.to_string()).collect();
    format!("{name}({})", placeholders.join(", "))
}

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
    widgets::{Block, Borders, Clear, Widget},
};

/// The on-screen rectangles of the help modal, derived from a terminal `area`
/// by [`help_layout`].
///
/// Shared by the renderer and the smooth-scroll emit so the pooled list and
/// detail regions match the painted ones exactly.
pub(crate) struct HelpLayout {
    /// The bordered modal box.
    pub(crate) modal: Rect,
    /// Inside the border: search row, status row, then the body split.
    pub(crate) inner: Rect,
    /// The entry list, also a smooth-scroll pool region.
    pub(crate) list: Rect,
    /// The selected entry's detail, also a smooth-scroll pool region.
    pub(crate) detail: Rect,
}

/// Lay out the help modal within `area`, or `None` when `area` is too small to
/// host it.
pub(crate) fn help_layout(area: Rect) -> Option<HelpLayout> {
    if area.width < 40 || area.height < 12 {
        return None;
    }

    let box_width = 120u16.min(area.width.saturating_sub(4));
    let box_height = 36u16.min(area.height.saturating_sub(4));
    if box_width < 40 || box_height < 12 {
        return None;
    }

    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let modal = Rect::new(x, y, box_width, box_height);
    // The title rides the top border, so it does not shrink the inner rect.
    let inner = Block::default().borders(Borders::ALL).inner(modal);

    let body_top = inner.y + 2;
    let body_height = (inner.y + inner.height).saturating_sub(body_top);
    if body_height == 0 {
        return None;
    }

    let body_width = inner.width;
    let list_width = (body_width * 42 / 100).max(20);
    let detail_width = body_width.saturating_sub(list_width + 1);
    let list = Rect::new(inner.x, body_top, list_width, body_height);
    let detail = Rect::new(
        inner.x + list_width + 1,
        body_top,
        detail_width,
        body_height,
    );

    Some(HelpLayout {
        modal,
        inner,
        list,
        detail,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_help(
    help: &Help,
    stoat_mode: &str,
    ws: &mut crate::workspace::Workspace,
    theme: &crate::theme::Theme,
    mode_badges: &std::collections::BTreeMap<String, String>,
    area: Rect,
    buf: &mut Buffer,
) {
    use crate::help::{help_input_mode, HelpInput, HelpScope};
    let input_mode = help_input_mode(stoat_mode);

    let Some(layout) = help_layout(area) else {
        return;
    };

    let title = match help.scope() {
        HelpScope::Active => format!(" help: active ({}) ", help.snapshot_mode()),
        HelpScope::All => " help: all actions ".to_string(),
    };
    let modal_style = theme.get(crate::theme::scope::UI_MODAL_HELP);
    Clear.render(layout.modal, buf);
    Block::default()
        .borders(Borders::ALL)
        .border_style(modal_style)
        .title(title)
        .title_style(modal_style)
        .render(layout.modal, buf);

    let inner = layout.inner;
    let prompt_style = theme.get(crate::theme::scope::UI_PROMPT);
    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);

    let search_row = inner.y;
    let prompt = match input_mode {
        HelpInput::Insert => "> ",
        HelpInput::Normal => ": ",
    };
    write_str(buf, inner.x, search_row, prompt, prompt_style);
    let input_area = Rect::new(inner.x + 2, search_row, inner.width.saturating_sub(2), 1);
    help.input.render(
        &mut ws.editors,
        input_area,
        true,
        "prompt",
        theme,
        mode_badges,
        buf,
    );

    let status_row = search_row + 1;
    let status_base = theme.get(crate::theme::scope::UI_STATUSBAR_FOCUSED);
    for col in inner.x..inner.x + inner.width {
        buf[(col, status_row)].set_char(' ').set_style(status_base);
    }
    let mode_str = match input_mode {
        HelpInput::Insert => "insert",
        HelpInput::Normal => "normal",
    };
    let (mode_label, mode_bg) = mode_segment(mode_str, theme, mode_badges);
    let mode_style = theme.get(crate::theme::scope::UI_MODE_LABEL).bg(mode_bg);
    let chip = format!(" {mode_label} ");
    write_str(buf, inner.x, status_row, &chip, mode_style);

    let list_rect = layout.list;
    let detail_rect = layout.detail;

    for row in list_rect.y..list_rect.y + list_rect.height {
        buf[(list_rect.x + list_rect.width, row)]
            .set_char('│')
            .set_style(muted);
    }

    let list_scroll = help
        .selected()
        .saturating_sub(list_rect.height.saturating_sub(1) as usize);
    paint_help_list_rows(help, list_rect, list_scroll, theme, buf);
    paint_help_detail_rows(help, detail_rect, help.detail_scroll() as usize, theme, buf);
}

/// Paint help entry rows into `area` starting at `start_row`, one row per entry,
/// with the right-aligned key column, action name, and short description.
///
/// Shared by the live list, which derives `start_row` from the selection, and
/// the smooth-scroll pool, which paints absolute pages, so both render identical
/// rows.
pub(crate) fn paint_help_list_rows(
    help: &Help,
    area: Rect,
    start_row: usize,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let selected_style = theme.get(crate::theme::scope::UI_SELECTION);
    let key_style = theme.get(crate::theme::scope::UI_KEY_LABEL);
    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);

    let filtered = help.filtered();
    let entries = help.entries();
    let selected = help.selected();
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }

    let key_col_width: usize = filtered
        .iter()
        .skip(start_row)
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
    for (row_idx, &entry_idx) in filtered.iter().skip(start_row).take(rows).enumerate() {
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

/// Paint the selected help entry's detail into `area` starting at line
/// `start_row`: name, binding, description, parameters, and example.
///
/// Shared by the live detail pane, which derives `start_row` from
/// `detail_scroll`, and the smooth-scroll pool, which paints absolute pages.
pub(crate) fn paint_help_detail_rows(
    help: &Help,
    area: Rect,
    start_row: usize,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let heading = theme.get(crate::theme::scope::UI_HEADING);
    let body_style = theme.get(crate::theme::scope::UI_TEXT);
    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let key_style = theme.get(crate::theme::scope::UI_KEY_LABEL);

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

    let rows = area.height as usize;
    let end_x = area.x + area.width;
    for (row_idx, (text, style)) in lines.iter().skip(start_row).take(rows).enumerate() {
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

use crate::{
    render::text::write_str,
    run::{GridSelection, RunState},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, Borders, Widget},
};

pub(crate) fn render_run_pane(
    run_state: &RunState,
    editors: &mut slotmap::SlotMap<crate::editor_state::EditorId, crate::editor_state::EditorState>,
    theme: &crate::theme::Theme,
    area: Rect,
    is_focused: bool,
    buf: &mut Buffer,
) {
    if area.height < 2 || area.width < 4 {
        return;
    }

    let input_row = area.y + area.height - 1;
    let output_height = area.height.saturating_sub(1);

    let mut output_lines: Vec<OutputLine<'_>> = Vec::new();
    for block in &run_state.blocks {
        output_lines.push(OutputLine::CommandHeader(block.command.as_str()));
        for row_idx in 0..block.grid.line_count() {
            output_lines.push(OutputLine::GridRow(
                &block.grid,
                row_idx,
                block.selection.as_ref(),
            ));
        }
        if let Some(err) = &block.error {
            output_lines.push(OutputLine::Error(err.as_str()));
        }
        if block.finished {
            let status = block.exit_status.unwrap_or(-1);
            output_lines.push(OutputLine::Status(status));
        }
        output_lines.push(OutputLine::Blank);
    }

    let total = output_lines.len();
    let visible = output_height as usize;
    let start = total.saturating_sub(visible + run_state.scroll_offset);
    for (i, line) in output_lines.iter().skip(start).take(visible).enumerate() {
        let y = area.y + i as u16;
        match line {
            OutputLine::CommandHeader(cmd) => {
                let cmd_style = theme.get(crate::theme::scope::UI_BADGE_COMPLETE);
                write_str(buf, area.x, y, "$ ", cmd_style);
                let max_w = (area.width as usize).saturating_sub(2);
                let display: String = cmd.chars().take(max_w).collect();
                write_str(buf, area.x + 2, y, &display, cmd_style);
            },
            OutputLine::GridRow(grid, row_idx, selection) => {
                let row = grid.row(*row_idx);
                let w = (area.width as usize).min(grid.width() as usize);
                let row_u16 = u16::try_from(*row_idx).unwrap_or(u16::MAX);
                for (col, cell) in row.iter().enumerate().take(w) {
                    let col_u16 = u16::try_from(col).unwrap_or(u16::MAX);
                    let selected = selection.is_some_and(|sel| sel.contains(col_u16, row_u16));
                    let blank = cell.ch == ' '
                        && cell.fg.is_none()
                        && cell.bg.is_none()
                        && cell.modifiers.is_empty();
                    if blank && !selected {
                        continue;
                    }
                    let mut style = Style::default();
                    if let Some(fg) = cell.fg {
                        style = style.fg(fg);
                    }
                    if let Some(bg) = cell.bg {
                        style = style.bg(bg);
                    }
                    style = style.add_modifier(cell.modifiers);
                    if selected {
                        style = style.add_modifier(Modifier::REVERSED);
                    }
                    let x = area.x + col as u16;
                    if x < area.x + area.width {
                        buf[(x, y)].set_char(cell.ch).set_style(style);
                    }
                }
            },
            OutputLine::Error(msg) => {
                let max_w = area.width as usize;
                let display: String = msg.chars().take(max_w).collect();
                write_str(
                    buf,
                    area.x,
                    y,
                    &display,
                    theme.get(crate::theme::scope::UI_ERROR),
                );
            },
            OutputLine::Status(code) => {
                let label = if *code == 0 {
                    String::new()
                } else {
                    format!("[exit {}]", code)
                };
                if !label.is_empty() {
                    write_str(
                        buf,
                        area.x,
                        y,
                        &label,
                        theme.get(crate::theme::scope::UI_TEXT_MUTED),
                    );
                }
            },
            OutputLine::Blank => {},
        }
    }

    let prompt_style = theme.get(crate::theme::scope::UI_KEY_LABEL);
    write_str(buf, area.x, input_row, "$ ", prompt_style);

    let input_area = Rect::new(area.x + 2, input_row, area.width.saturating_sub(2), 1);
    run_state.input.render(
        editors,
        input_area,
        is_focused,
        "prompt",
        theme,
        &std::collections::BTreeMap::new(),
        buf,
    );
}

enum OutputLine<'a> {
    CommandHeader(&'a str),
    GridRow(&'a crate::run::VtermGrid, usize, Option<&'a GridSelection>),
    Error(&'a str),
    Status(i32),
    Blank,
}

pub(crate) fn render_modal_run(
    run_state: &RunState,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    if area.width < 20 || area.height < 8 {
        return;
    }

    let box_width = (area.width * 7 / 10).min(area.width.saturating_sub(4));
    let box_height = (area.height * 8 / 10).min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let modal_area = Rect::new(x, y, box_width, box_height);

    let title = {
        let raw = run_state
            .title
            .as_deref()
            .or_else(|| run_state.active_block().map(|b| b.command.as_str()))
            .unwrap_or("run");
        let max = (box_width as usize).saturating_sub(4);
        let display: String = raw.chars().take(max).collect();
        format!(" {display} ")
    };
    let modal_style = theme.get(crate::theme::scope::UI_MODAL_RUN);
    let border = Block::default()
        .borders(Borders::ALL)
        .border_style(modal_style)
        .title(title)
        .title_style(modal_style);
    let inner = border.inner(modal_area);
    border.render(modal_area, buf);

    let Some(active) = run_state.active_block() else {
        return;
    };

    let grid = &active.grid;
    let visible_rows = (inner.height as usize).saturating_sub(1);
    let total = grid.line_count();
    let start = total.saturating_sub(visible_rows + run_state.scroll_offset);
    let w = (inner.width as usize).min(grid.width() as usize);

    for (i, row_idx) in (start..total).take(visible_rows).enumerate() {
        let y = inner.y + i as u16;
        let row = grid.row(row_idx);
        for (col, cell) in row.iter().enumerate().take(w) {
            if cell.ch == ' ' && cell.fg.is_none() && cell.bg.is_none() && cell.modifiers.is_empty()
            {
                continue;
            }
            let mut style = Style::default();
            if let Some(fg) = cell.fg {
                style = style.fg(fg);
            }
            if let Some(bg) = cell.bg {
                style = style.bg(bg);
            }
            style = style.add_modifier(cell.modifiers);
            let cx = inner.x + col as u16;
            if cx < inner.x + inner.width {
                buf[(cx, y)].set_char(cell.ch).set_style(style);
            }
        }
    }

    let status_row = inner.y + inner.height.saturating_sub(1);
    let status = if active.finished {
        let code = active.exit_status.unwrap_or(-1);
        if code == 0 {
            "done -- press Escape to dismiss".to_owned()
        } else {
            format!("exited {} -- press Escape to dismiss", code)
        }
    } else {
        "running...".to_owned()
    };
    let status_style = if active.finished {
        theme.get(crate::theme::scope::UI_TEXT_MUTED)
    } else {
        theme.get(crate::theme::scope::UI_BADGE_ACTIVE)
    };
    write_str(buf, inner.x, status_row, &status, status_style);
}

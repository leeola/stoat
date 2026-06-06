use crate::{
    render::text::write_str,
    run::{BlockStatus, GridSelection, OutputBlock, RunState, TermColor, TermModifier},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};

fn term_color_to_ratatui(c: TermColor) -> Color {
    match c {
        TermColor::Reset => Color::Reset,
        TermColor::Black => Color::Black,
        TermColor::Red => Color::Red,
        TermColor::Green => Color::Green,
        TermColor::Yellow => Color::Yellow,
        TermColor::Blue => Color::Blue,
        TermColor::Magenta => Color::Magenta,
        TermColor::Cyan => Color::Cyan,
        TermColor::Gray => Color::Gray,
        TermColor::DarkGray => Color::DarkGray,
        TermColor::LightRed => Color::LightRed,
        TermColor::LightGreen => Color::LightGreen,
        TermColor::LightYellow => Color::LightYellow,
        TermColor::LightBlue => Color::LightBlue,
        TermColor::LightMagenta => Color::LightMagenta,
        TermColor::LightCyan => Color::LightCyan,
        TermColor::White => Color::White,
        TermColor::Indexed(i) => Color::Indexed(i),
        TermColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn term_modifier_to_ratatui(m: TermModifier) -> Modifier {
    let mut out = Modifier::empty();
    if m.contains(TermModifier::BOLD) {
        out |= Modifier::BOLD;
    }
    if m.contains(TermModifier::DIM) {
        out |= Modifier::DIM;
    }
    if m.contains(TermModifier::ITALIC) {
        out |= Modifier::ITALIC;
    }
    if m.contains(TermModifier::UNDERLINED) {
        out |= Modifier::UNDERLINED;
    }
    if m.contains(TermModifier::REVERSED) {
        out |= Modifier::REVERSED;
    }
    if m.contains(TermModifier::CROSSED_OUT) {
        out |= Modifier::CROSSED_OUT;
    }
    out
}

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
        output_lines.push(OutputLine::CommandHeader(block));
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
        output_lines.push(OutputLine::Status(block.status()));
        output_lines.push(OutputLine::Blank);
    }

    let total = output_lines.len();
    let visible = output_height as usize;
    let start = total.saturating_sub(visible + run_state.scroll_offset);
    for (i, line) in output_lines.iter().skip(start).take(visible).enumerate() {
        let y = area.y + i as u16;
        match line {
            OutputLine::CommandHeader(block) => {
                let cmd_style = theme.get(crate::theme::scope::UI_BADGE_COMPLETE);
                let meta_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
                write_str(buf, area.x, y, "$ ", cmd_style);

                let total = area.width as usize;
                let meta = block.header_meta();
                let meta_w = meta.chars().count();
                let meta_fits = meta_w > 0 && meta_w + 3 <= total;
                let cmd_avail = if meta_fits {
                    total - 2 - meta_w - 1
                } else {
                    total.saturating_sub(2)
                };
                let display: String = block.command.chars().take(cmd_avail).collect();
                write_str(buf, area.x + 2, y, &display, cmd_style);
                if meta_fits {
                    let meta_x = area.x + (total - meta_w) as u16;
                    write_str(buf, meta_x, y, &meta, meta_style);
                }
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
                        style = style.fg(term_color_to_ratatui(fg));
                    }
                    if let Some(bg) = cell.bg {
                        style = style.bg(term_color_to_ratatui(bg));
                    }
                    style = style.add_modifier(term_modifier_to_ratatui(cell.modifiers));
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
            OutputLine::Status(status) => {
                write_str(
                    buf,
                    area.x,
                    y,
                    &status.label(),
                    theme.get(status_scope(*status)),
                );
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
    CommandHeader(&'a OutputBlock),
    GridRow(&'a crate::run::VtermGrid, usize, Option<&'a GridSelection>),
    Error(&'a str),
    Status(BlockStatus),
    Blank,
}

fn status_scope(status: BlockStatus) -> &'static str {
    use crate::theme::scope;
    match status {
        BlockStatus::Running => scope::UI_BADGE_ACTIVE,
        BlockStatus::Succeeded => scope::UI_BADGE_COMPLETE,
        BlockStatus::Failed(_) => scope::UI_BADGE_ERROR,
    }
}

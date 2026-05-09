use crate::{permission_prompt::ApprovalModal, render::text::write_str};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Borders, Widget},
};

const BOX_WIDTH: u16 = 70;
const BOX_HEIGHT: u16 = 13;
const MIN_WIDTH: u16 = 50;
const MIN_HEIGHT: u16 = 8;
const INPUT_ROWS: u16 = 6;

pub(crate) fn render_permission_prompt(
    modal: &ApprovalModal,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        return;
    }
    let box_width = BOX_WIDTH.min(area.width.saturating_sub(4));
    let box_height = BOX_HEIGHT.min(area.height);
    if box_width < MIN_WIDTH || box_height < MIN_HEIGHT {
        return;
    }
    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let modal_area = Rect::new(x, y, box_width, box_height);

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PICKER);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(modal_style)
        .title(" permission required ")
        .title_style(modal_style);
    let inner = block.inner(modal_area);
    block.render(modal_area, buf);

    let prompt_style = theme.get(crate::theme::scope::UI_PROMPT);
    let text_style = theme.get(crate::theme::scope::UI_TEXT);
    let muted_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let highlight_style = theme.get(crate::theme::scope::UI_SELECTION);

    let max_w = inner.width as usize;

    let title = format!("Tool: {}", modal.tool());
    write_str(
        buf,
        inner.x,
        inner.y,
        &truncate(&title, max_w),
        prompt_style,
    );

    write_str(
        buf,
        inner.x,
        inner.y + 1,
        &truncate("Input:", max_w),
        muted_style,
    );

    let input_top = inner.y + 2;
    let input_bottom = input_top + INPUT_ROWS;
    for (i, line) in wrap(modal.input(), max_w)
        .into_iter()
        .take(INPUT_ROWS as usize)
        .enumerate()
    {
        write_str(buf, inner.x, input_top + i as u16, &line, text_style);
    }

    let buttons = ApprovalModal::buttons();
    let focused = modal.focused_button();
    let separator_y = input_bottom + 1;
    if separator_y < inner.y + inner.height {
        let line: String = "-".repeat(max_w);
        write_str(buf, inner.x, separator_y, &line, muted_style);
    }
    let button_y = separator_y + 1;
    if button_y < inner.y + inner.height {
        let mut x_cursor = inner.x;
        for (i, button) in buttons.iter().enumerate() {
            let label = format!(" {} ", button.label);
            let style = if i == focused {
                highlight_style
            } else {
                text_style
            };
            let label_w = label.chars().count() as u16;
            if x_cursor + label_w > inner.x + inner.width {
                break;
            }
            write_str(buf, x_cursor, button_y, &label, style);
            x_cursor += label_w + 1;
        }
    }
}

fn truncate(s: &str, max_w: usize) -> String {
    s.chars().take(max_w).collect()
}

fn wrap(s: &str, max_w: usize) -> Vec<String> {
    if max_w == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut count = 0;
    for c in s.chars() {
        if c == '\n' {
            lines.push(std::mem::take(&mut current));
            count = 0;
            continue;
        }
        if count >= max_w {
            lines.push(std::mem::take(&mut current));
            count = 0;
        }
        current.push(c);
        count += 1;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

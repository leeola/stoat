use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{Block, Borders, Clear, Widget},
};
use std::collections::HashMap;

pub(crate) struct HintsFooter {
    pub(crate) text: String,
    pub(crate) style: Style,
}

pub(crate) fn render_hints(
    mode: &str,
    bindings: &[(&str, String)],
    footer: Option<&HintsFooter>,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    if bindings.is_empty() || area.width < 10 || area.height < 4 {
        return;
    }

    let rows = group_by_action(bindings);

    let key_width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    let action_width = rows.iter().map(|(_, a)| a.len()).max().unwrap_or(0);
    let gap = 3;
    let bindings_width = key_width + gap + action_width;
    let border_pad = 2;
    let title_width = mode.len() + 4;
    let footer_width = footer.map(|f| f.text.len()).unwrap_or(0);
    let content_width = bindings_width.max(title_width).max(footer_width);
    let extra_rows = footer.map(|_| 2).unwrap_or(0);
    let box_width = (content_width + border_pad) as u16;
    if box_width > area.width {
        return;
    }

    let chrome = border_pad + extra_rows;
    let capacity = (area.height as usize).saturating_sub(chrome);
    if capacity == 0 {
        return;
    }

    let total = rows.len();
    let (visible, hidden) = if total <= capacity {
        (total, 0)
    } else {
        let shown = capacity - 1;
        (shown, total - shown)
    };
    let content_rows = visible + usize::from(hidden > 0);
    let box_height = (content_rows + chrome) as u16;

    let x = area.x + area.width.saturating_sub(box_width);
    let y = area.y + area.height.saturating_sub(box_height);
    let help_area = Rect::new(x, y, box_width, box_height);

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_HINTS);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(modal_style)
        .title(format!(" {mode} "))
        .title_style(modal_style);
    let inner = block.inner(help_area);
    Clear.render(help_area, buf);
    block.render(help_area, buf);

    let key_style = theme.get(crate::theme::scope::UI_KEY_LABEL);
    let action_style = theme.get(crate::theme::scope::UI_TEXT);

    for (i, (key, action)) in rows.iter().take(visible).enumerate() {
        let row = inner.y + i as u16;
        if row >= inner.y + inner.height {
            break;
        }
        let padded_key = format!("{key:>width$}", width = key_width);
        let line = format!("{padded_key}   {action}");

        for (j, ch) in line.chars().enumerate() {
            let col = inner.x + j as u16;
            if col >= inner.x + inner.width {
                break;
            }
            let style = if j < key_width {
                key_style
            } else {
                action_style
            };
            buf[(col, row)].set_char(ch).set_style(style);
        }
    }

    if hidden > 0 {
        let more_row = inner.y + visible as u16;
        let more_text = format!("+{hidden} more");
        let more_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
        for (j, ch) in more_text.chars().enumerate() {
            let col = inner.x + j as u16;
            if col >= inner.x + inner.width {
                break;
            }
            buf[(col, more_row)].set_char(ch).set_style(more_style);
        }
    }

    if let Some(footer) = footer {
        let sep_row = inner.y + content_rows as u16;
        let text_row = sep_row + 1;
        if sep_row < inner.y + inner.height {
            let sep_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
            for col_offset in 0..inner.width {
                let col = inner.x + col_offset;
                buf[(col, sep_row)].set_char('─').set_style(sep_style);
            }
        }
        if text_row < inner.y + inner.height {
            for (j, ch) in footer.text.chars().enumerate() {
                let col = inner.x + j as u16;
                if col >= inner.x + inner.width {
                    break;
                }
                buf[(col, text_row)].set_char(ch).set_style(footer.style);
            }
        }
    }
}

/// Collapses entries that share an action description, joining their keys with
/// `", "` in first-seen order. Ensures each action appears on exactly one row.
fn group_by_action<'a>(bindings: &'a [(&str, String)]) -> Vec<(String, &'a str)> {
    let mut rows: Vec<(String, &'a str)> = Vec::new();
    let mut index: HashMap<&'a str, usize> = HashMap::new();
    for (key, action) in bindings {
        let action = action.as_str();
        if let Some(&i) = index.get(action) {
            let row = &mut rows[i];
            row.0.push_str(", ");
            row.0.push_str(key);
        } else {
            index.insert(action, rows.len());
            rows.push((key.to_string(), action));
        }
    }
    rows
}

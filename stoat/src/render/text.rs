use ratatui::{buffer::Buffer, style::Style};

pub(crate) fn write_cell(buf: &mut Buffer, x: u16, y: u16, ch: char, style: Style) {
    if x < buf.area.x + buf.area.width && y < buf.area.y + buf.area.height {
        buf[(x, y)].set_char(ch).set_style(style);
    }
}

pub(crate) fn write_str(buf: &mut Buffer, x: u16, y: u16, s: &str, style: Style) {
    for (i, ch) in s.chars().enumerate() {
        let col = x + i as u16;
        if col >= buf.area.x + buf.area.width {
            break;
        }
        if y >= buf.area.y + buf.area.height {
            break;
        }
        buf[(col, y)].set_char(ch).set_style(style);
    }
}

pub(crate) fn write_str_clipped(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    s: &str,
    style: Style,
    end_x: u16,
) {
    for (i, ch) in s.chars().enumerate() {
        let col = x + i as u16;
        if col >= end_x || col >= buf.area.x + buf.area.width {
            break;
        }
        if y >= buf.area.y + buf.area.height {
            break;
        }
        buf[(col, y)].set_char(ch).set_style(style);
    }
}

pub(crate) fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let trimmed_start = text.trim_start();
    if trimmed_start.is_empty() {
        return Vec::new();
    }
    let indent_byte_len = text.len() - trimmed_start.len();
    let indent = text[..indent_byte_len].to_string();
    let indent_w = indent.chars().count();
    if indent_w >= width {
        return vec![text.to_string()];
    }

    let mut lines = Vec::new();
    let mut current = indent.clone();
    let mut current_w = indent_w;
    for word in trimmed_start.split_whitespace() {
        let needs_space = current_w > indent_w;
        let word_w = word.chars().count();
        let add_w = word_w + usize::from(needs_space);
        if current_w + add_w <= width {
            if needs_space {
                current.push(' ');
            }
            current.push_str(word);
            current_w += add_w;
        } else {
            lines.push(std::mem::take(&mut current));
            current = indent.clone();
            current.push_str(word);
            current_w = indent_w + word_w;
        }
    }
    if current_w > indent_w {
        lines.push(current);
    }
    lines
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        format!(
            "{}...",
            s.chars().take(max.saturating_sub(3)).collect::<String>()
        )
    }
}

pub(crate) fn truncate_to_cols(text: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > max_cols {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

pub(crate) fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    match parts.len() {
        0 => p.to_string(),
        1 => parts[0].to_string(),
        n => format!("{}/{}", parts[n - 2], parts[n - 1]),
    }
}

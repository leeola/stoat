use crate::{
    app::Stoat,
    pane::{FocusTarget, View},
    render::{editor::render_editor, layout::split_pane_status},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Borders, Widget},
};

/// Paint either the workspace-symbol input modal or the result
/// picker, whichever is active. Anchored to the focused editor's
/// cursor.
pub(crate) fn render_workspace_symbol(stoat: &mut Stoat, buf: &mut Buffer) {
    if stoat.workspace_symbol_input.is_some() {
        render_input(stoat, buf);
        return;
    }
    if let Some(picker) = stoat.pending_workspace_symbol_picker.as_ref() {
        if !picker.entries.is_empty() {
            render_picker(stoat, buf);
        }
    }
}

fn render_input(stoat: &mut Stoat, buf: &mut Buffer) {
    let anchor_offset = stoat
        .workspace_symbol_input
        .as_ref()
        .map(|s| s.anchor_offset)
        .expect("checked by caller");

    let content_area = match focused_editor_content_area(stoat) {
        Some(area) => area,
        None => return,
    };

    let cursor_screen = match cursor_screen_position(stoat, content_area, anchor_offset) {
        Some(p) => p,
        None => return,
    };

    let modal_style = stoat.theme.get(crate::theme::scope::UI_MODAL_HINTS);

    let popup_width = (content_area.width / 3).max(20).min(content_area.width);
    let popup_height: u16 = 3;

    let popup_x = cursor_screen
        .0
        .min(content_area.x + content_area.width.saturating_sub(popup_width));
    let popup_y = if cursor_screen.1 >= content_area.y + popup_height {
        cursor_screen.1 - popup_height
    } else if cursor_screen.1 + 1 + popup_height <= content_area.y + content_area.height {
        cursor_screen.1 + 1
    } else {
        content_area.y
    };

    let popup_area = Rect {
        x: popup_x,
        y: popup_y,
        width: popup_width,
        height: popup_height,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(modal_style)
        .title(" workspace symbol ")
        .title_style(modal_style);
    let inner = block.inner(popup_area);
    block.render(popup_area, buf);

    let editor_id = stoat
        .workspace_symbol_input
        .as_ref()
        .map(|s| s.input.editor_id)
        .expect("checked by caller");
    let theme = stoat.theme.clone();
    let ws = stoat.active_workspace_mut();
    if let Some(editor) = ws.editors.get_mut(editor_id) {
        render_editor(editor, inner, modal_style, &theme, buf, true);
    }
}

fn render_picker(stoat: &mut Stoat, buf: &mut Buffer) {
    let picker = match &stoat.pending_workspace_symbol_picker {
        Some(p) if !p.entries.is_empty() => p.clone(),
        _ => return,
    };

    let content_area = match focused_editor_content_area(stoat) {
        Some(area) => area,
        None => return,
    };

    let cursor_screen = match cursor_screen_position(stoat, content_area, picker.anchor_offset) {
        Some(p) => p,
        None => return,
    };

    let modal_style = stoat.theme.get(crate::theme::scope::UI_MODAL_HINTS);

    let interior_width = content_area.width.saturating_sub(2);
    if interior_width == 0 {
        return;
    }
    let visible_count = picker.entries.len().min(9);
    let body: Vec<String> = picker
        .entries
        .iter()
        .take(visible_count)
        .enumerate()
        .map(|(i, e)| {
            let raw = format!("{}. {}", i + 1, e.title);
            truncate_to_width(&raw, interior_width as usize)
        })
        .collect();
    let max_line_width = body.iter().map(|s| s.chars().count()).max().unwrap_or(0) as u16;
    let popup_width = (max_line_width + 2).clamp(3, content_area.width.max(3));
    let popup_height = (body.len() as u16 + 2).clamp(3, content_area.height.max(3));

    let popup_x = cursor_screen
        .0
        .min(content_area.x + content_area.width.saturating_sub(popup_width));
    let popup_y = if cursor_screen.1 >= content_area.y + popup_height {
        cursor_screen.1 - popup_height
    } else if cursor_screen.1 + 1 + popup_height <= content_area.y + content_area.height {
        cursor_screen.1 + 1
    } else {
        content_area.y
    };

    let popup_area = Rect {
        x: popup_x,
        y: popup_y,
        width: popup_width,
        height: popup_height,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(modal_style)
        .title(" workspace symbols ")
        .title_style(modal_style);
    let inner = block.inner(popup_area);
    block.render(popup_area, buf);

    for (row_idx, line) in body.iter().enumerate() {
        let row = inner.y + row_idx as u16;
        if row >= inner.y + inner.height {
            break;
        }
        for (col_idx, ch) in line.chars().enumerate() {
            let col = inner.x + col_idx as u16;
            if col >= inner.x + inner.width {
                break;
            }
            buf[(col, row)].set_char(ch).set_style(modal_style);
        }
    }
}

fn focused_editor_content_area(stoat: &Stoat) -> Option<Rect> {
    let ws = stoat.active_workspace();
    let FocusTarget::SplitPane(pane_id) = ws.focus else {
        return None;
    };
    let pane = ws.panes.pane(pane_id);
    if !matches!(pane.view, View::Editor(_)) {
        return None;
    }
    let (content, _) = split_pane_status(pane.area);
    Some(content)
}

fn cursor_screen_position(
    stoat: &mut Stoat,
    content_area: Rect,
    anchor_offset: usize,
) -> Option<(u16, u16)> {
    let ws = stoat.active_workspace_mut();
    let FocusTarget::SplitPane(pane_id) = ws.focus else {
        return None;
    };
    let pane = ws.panes.pane(pane_id);
    let View::Editor(editor_id) = pane.view else {
        return None;
    };
    let editor = ws.editors.get_mut(editor_id)?;
    if editor.review_view.is_some() {
        return None;
    }
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    if anchor_offset > rope.len() {
        return None;
    }
    let point = rope.offset_to_point(anchor_offset);
    let display = snapshot.buffer_to_display(point);
    if display.row < editor.scroll_row {
        return None;
    }
    let visible_rows = content_area.height as u32;
    if display.row >= editor.scroll_row + visible_rows {
        return None;
    }
    let y = content_area.y + (display.row - editor.scroll_row) as u16;
    let x = content_area.x + display.column as u16;
    if x >= content_area.x + content_area.width || y >= content_area.y + content_area.height {
        return None;
    }
    Some((x, y))
}

fn truncate_to_width(line: &str, width: usize) -> String {
    if line.chars().count() <= width {
        return line.to_string();
    }
    line.chars().take(width).collect()
}

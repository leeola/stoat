use crate::{
    app::Stoat,
    pane::{FocusTarget, View},
    render::layout::split_pane_status,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Clear, Widget},
};

/// Paint the hover popup, if any, anchored to the focused editor's
/// primary cursor. Renders above the cursor when there is room above,
/// otherwise below. Truncates lines that exceed the popup's interior
/// width; truncates the trailing rows when the popup would extend past
/// the focused pane.
///
/// No-op when [`Stoat::pending_hover`] is `None`, when the focused
/// pane is not an editor, or when the cursor is off-screen.
pub(crate) fn render_hover(
    stoat: &mut Stoat,
    buf: &mut Buffer,
    scene: Option<&mut stoatty_widgets::ApcScene>,
) {
    let popup = match &stoat.pending_hover {
        Some(p) => p.clone(),
        None => return,
    };

    let ws = stoat.active_workspace_mut();
    let FocusTarget::SplitPane(pane_id) = ws.focus else {
        return;
    };

    let pane = ws.panes.pane(pane_id);
    let View::Editor(editor_id) = pane.view else {
        return;
    };
    let pane_area = pane.area;
    let (content_area, _) = split_pane_status(pane_area);

    let editor = match ws.editors.get_mut(editor_id) {
        Some(e) => e,
        None => return,
    };

    let cursor_screen = match cursor_screen_position(editor, content_area, popup.anchor_offset) {
        Some(p) => p,
        None => return,
    };

    let modal_style = stoat.theme.get(crate::theme::scope::UI_MODAL_HINTS);

    let interior_width = content_area.width.saturating_sub(2);
    if interior_width == 0 {
        return;
    }
    let body: Vec<String> = popup
        .lines
        .iter()
        .map(|line| truncate_to_width(line, interior_width as usize))
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

    Clear.render(popup_area, buf);
    let inner = crate::render::chrome::modal_frame(
        buf,
        popup_area,
        Some(" hover "),
        modal_style,
        &stoat.theme,
        scene,
    );

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

fn cursor_screen_position(
    editor: &mut crate::editor_state::EditorState,
    content_area: Rect,
    anchor_offset: usize,
) -> Option<(u16, u16)> {
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

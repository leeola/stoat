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

/// Paint the rename input modal anchored to the focused editor's
/// cursor. Renders the embedded [`crate::input_view::InputView`]
/// inside a bordered popup titled "rename".
///
/// No-op when there is no rename input open or the focused pane is
/// not an editor.
pub(crate) fn render_rename_input(stoat: &mut Stoat, buf: &mut Buffer) {
    if stoat.rename_input.is_none() {
        return;
    }
    let anchor_offset = stoat
        .rename_input
        .as_ref()
        .map(|s| s.anchor_offset)
        .expect("checked above");

    let (content_area, focus_pane_id) = match stoat.active_workspace().focus {
        FocusTarget::SplitPane(pane_id) => {
            let pane = stoat.active_workspace().panes.pane(pane_id);
            let (content, _) = split_pane_status(pane.area);
            (content, pane_id)
        },
        _ => return,
    };
    let _ = focus_pane_id;

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
        .title(" rename ")
        .title_style(modal_style);
    let inner = block.inner(popup_area);
    block.render(popup_area, buf);

    let editor_id = stoat
        .rename_input
        .as_ref()
        .map(|s| s.input.editor_id)
        .expect("checked above");
    let theme = stoat.theme.clone();
    let ws = stoat.active_workspace_mut();
    if let Some(editor) = ws.editors.get_mut(editor_id) {
        render_editor(editor, inner, modal_style, &theme, buf, true);
    }
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

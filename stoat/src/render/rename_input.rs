use crate::{
    app::Stoat,
    pane::{FocusTarget, View},
    render::{editor::render_editor, layout::split_pane_status},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Clear, Widget},
};

/// Paint the rename input modal anchored to the focused editor's
/// cursor. Renders the embedded [`crate::input_view::InputView`]
/// inside a bordered popup titled "rename".
///
/// No-op when there is no rename input open or the focused pane is
/// not an editor.
pub(crate) fn render_rename_input(
    stoat: &mut Stoat,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    if stoat.rename_input.is_none() {
        return;
    }
    let anchor_offset = stoat
        .rename_input
        .as_ref()
        .map(|s| s.anchor_offset)
        .expect("checked above");

    let (content_area, focus_pane_id) = match stoat.active_workspace().focus {
        FocusTarget::SplitPane => {
            let pane_id = stoat.active_workspace().panes.focus();
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

    Clear.render(popup_area, buf);
    let inner = crate::render::chrome::modal_frame(
        buf,
        popup_area,
        Some(" rename "),
        modal_style,
        &stoat.theme,
        scene,
    );

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
    let FocusTarget::SplitPane = ws.focus else {
        return None;
    };
    let pane_id = ws.panes.focus();
    let pane = ws.panes.pane(pane_id);
    let View::Editor(editor_id) = pane.view else {
        return None;
    };
    let editor = ws.editors.get_mut(editor_id)?;
    if editor.review_view.is_some() {
        return None;
    }
    crate::render::hover::cursor_screen_position(editor, content_area, anchor_offset)
}

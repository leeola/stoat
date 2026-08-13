use crate::{
    app::Stoat,
    render::{cursor_popup, editor::render_editor},
};
use ratatui::buffer::Buffer;

/// Paint the rename input modal anchored to the focused editor's
/// cursor. Renders the embedded [`crate::input_view::InputView`]
/// inside a bordered popup titled "rename".
///
/// No-op when there is no rename input open or the focused pane is
/// not an editor.
pub(crate) fn render_rename_input(
    stoat: &mut Stoat,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    if stoat.rename_input.is_none() {
        return;
    }
    let anchor_offset = stoat
        .rename_input
        .as_ref()
        .map(|s| s.anchor_offset)
        .expect("checked above");

    let Some((content_area, cursor_screen)) =
        cursor_popup::focused_editor_popup_ctx(stoat, anchor_offset)
    else {
        return;
    };

    let modal_style = stoat.theme.get(crate::theme::scope::UI_MODAL_HINTS);

    // The rename box is sized to the pane rather than to its content, since
    // what the user is about to type has no width yet. Stated as an interior,
    // because popup_rect adds the border back.
    let interior_width = (content_area.width / 3)
        .max(20)
        .min(content_area.width)
        .saturating_sub(2);
    let popup_area =
        cursor_popup::popup_rect(content_area, cursor_screen, (interior_width, 1), true);

    crate::render::clear_themed(popup_area, buf, &stoat.theme);
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
    let chrome = stoat.chrome.as_ref().map(|(_, chrome)| chrome);

    // `active_workspace_mut` borrows the whole of `stoat`, which would end the
    // chrome read above. Indexing the field keeps the two disjoint.
    let ws = &mut stoat.workspaces[stoat.active_workspace];
    if let Some(editor) = ws.editors.get_mut(editor_id) {
        render_editor(editor, inner, modal_style, &theme, chrome, buf, true);
    }
}

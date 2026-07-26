use crate::{
    app::Stoat,
    pane::{FocusTarget, View},
    render::{cursor_popup, layout::split_pane_status, text::truncate_to_width},
};
use ratatui::{
    buffer::Buffer,
    widgets::{Clear, Widget},
};

/// Paint the code-action picker, if any, anchored to the focused
/// editor's primary cursor. Renders a 9-row viewport over the
/// picker's `entries` that follows `selected_idx`; visible rows
/// are numbered 1..=9 by position. A `start-end / total` footer
/// appears when entries exceed the window. Clamps width and height
/// to the focused pane.
///
/// No-op when the picker has no entries (still in flight) or when
/// the focused pane is not an editor.
pub(crate) fn render_code_action(
    stoat: &mut Stoat,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    let anchor_offset = match &stoat.pending_code_action_picker {
        Some(p) if !p.entries.is_empty() => p.anchor_offset,
        _ => return,
    };

    let (content_area, cursor_screen) = {
        let ws = stoat.active_workspace_mut();
        let FocusTarget::SplitPane = ws.focus else {
            return;
        };
        let pane_id = ws.panes.focus();
        let pane = ws.panes.pane(pane_id);
        let View::Editor(editor_id) = pane.view else {
            return;
        };
        let (content_area, _) = split_pane_status(pane.area);
        let editor = match ws.editors.get_mut(editor_id) {
            Some(e) => e,
            None => return,
        };
        let cursor_screen =
            match cursor_popup::cursor_screen_position(editor, content_area, anchor_offset) {
                Some(p) => p,
                None => return,
            };
        (content_area, cursor_screen)
    };

    let modal_style = stoat.theme.get(crate::theme::scope::UI_MODAL_HINTS);
    let selected_style = stoat.theme.get(crate::theme::scope::UI_SELECTION);

    let interior_width = content_area.width.saturating_sub(2);
    if interior_width == 0 {
        return;
    }
    let picker = match stoat.pending_code_action_picker.as_ref() {
        Some(p) => p,
        None => return,
    };
    let total = picker.entries.len();
    let viewport_top =
        crate::render::symbol_picker::viewport_top_for_picker(picker.selected_idx, total);
    let window = crate::render::symbol_picker::VISIBLE_WINDOW;
    let visible_count = total.saturating_sub(viewport_top).min(window);
    let body: Vec<String> = picker
        .entries
        .iter()
        .skip(viewport_top)
        .take(visible_count)
        .enumerate()
        .map(|(i, e)| {
            let raw = format!("{}. {}", i + 1, e.title());
            truncate_to_width(&raw, interior_width as usize)
        })
        .collect();
    let footer = (total > window).then(|| {
        truncate_to_width(
            &format!(
                "{}-{} / {}",
                viewport_top + 1,
                viewport_top + visible_count,
                total
            ),
            interior_width as usize,
        )
    });
    let size = cursor_popup::content_size(&body, footer.as_ref());
    let popup_area = cursor_popup::popup_rect(content_area, cursor_screen, size, true);

    Clear.render(popup_area, buf);
    let inner = crate::render::chrome::modal_frame(
        buf,
        popup_area,
        Some(" code action "),
        modal_style,
        &stoat.theme,
        scene,
    );

    cursor_popup::paint_numbered_rows(
        &body,
        footer.as_ref(),
        picker.selected_idx.checked_sub(viewport_top),
        inner,
        (modal_style, selected_style),
        buf,
    );
}

use crate::{
    app::Stoat,
    pane::{FocusTarget, View},
    render::{cursor_popup, layout::split_pane_status, text::truncate_to_width},
};
use ratatui::{
    buffer::Buffer,
    widgets::{Clear, Widget},
};

/// Visible-window size for the symbol picker. The list scrolls so
/// `selected_idx` stays inside this window; visible rows are
/// numbered 1..=9 by their position in the window.
pub(crate) const VISIBLE_WINDOW: usize = 9;

/// Paint the document-symbol picker, if any, anchored to the focused
/// editor's primary cursor. Renders a 9-row viewport over the
/// picker's `entries` that follows `selected_idx`; visible rows are
/// numbered 1..=9. Clamps width and height to the focused pane.
pub(crate) fn render_symbol_picker(
    stoat: &mut Stoat,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    let picker = match &stoat.pending_symbol_picker {
        Some(p) if !p.entries.is_empty() => p.clone(),
        _ => return,
    };

    let ws = stoat.active_workspace_mut();
    let FocusTarget::SplitPane = ws.focus else {
        return;
    };
    let pane_id = ws.panes.focus();

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

    let cursor_screen =
        match cursor_popup::cursor_screen_position(editor, content_area, picker.anchor_offset) {
            Some(p) => p,
            None => return,
        };

    let modal_style = stoat.theme.get(crate::theme::scope::UI_MODAL_HINTS);
    let selected_style = stoat.theme.get(crate::theme::scope::UI_SELECTION);

    let interior_width = content_area.width.saturating_sub(2);
    if interior_width == 0 {
        return;
    }
    let total = picker.entries.len();
    let viewport_top = viewport_top_for(picker.selected_idx, total, VISIBLE_WINDOW);
    let visible_count = total.saturating_sub(viewport_top).min(VISIBLE_WINDOW);
    let body: Vec<String> = picker
        .entries
        .iter()
        .skip(viewport_top)
        .take(visible_count)
        .enumerate()
        .map(|(i, e)| {
            let raw = format!("{}. {}", i + 1, e.title);
            truncate_to_width(&raw, interior_width as usize)
        })
        .collect();
    let footer = (total > VISIBLE_WINDOW).then(|| {
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
        Some(" symbols "),
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

/// Return the index of the entry that should appear at the top of
/// the visible window so `selected` stays inside `[top, top + window)`.
/// Sticky-scrolls: keeps the prior viewport when possible, only
/// scrolling when `selected` falls outside the window.
fn viewport_top_for(selected: usize, total: usize, window: usize) -> usize {
    if total <= window {
        return 0;
    }
    let max_top = total - window;
    if selected < window {
        0
    } else {
        (selected + 1).saturating_sub(window).min(max_top)
    }
}

/// `viewport_top_for` over the picker's [`VISIBLE_WINDOW`]. Exposed
/// for the app's key-handler so the digit-key shortcut can rebase
/// onto the current viewport.
pub(crate) fn viewport_top_for_picker(selected: usize, total: usize) -> usize {
    viewport_top_for(selected, total, VISIBLE_WINDOW)
}

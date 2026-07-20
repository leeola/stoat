use crate::{
    app::Stoat,
    pane::{FocusTarget, View},
    render::{
        hover::{cursor_screen_position, truncate_to_width},
        layout::split_pane_status,
    },
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Modifier,
    widgets::{Clear, Widget},
};

/// Paint the signature-help popup anchored to the focused editor's primary
/// cursor, emphasizing the active parameter within the signature line and
/// showing the signature's documentation dimmed below it.
///
/// No-op when there is no pending signature help, when the completion popup is
/// open (it takes precedence so the two never overlap), when the focused pane
/// is not an editor, or when the cursor is off-screen.
pub(crate) fn render_signature_help(
    stoat: &mut Stoat,
    buf: &mut Buffer,
    scene: Option<&mut stoatty_widgets::ApcScene>,
) {
    if stoat.pending_completion.is_some() {
        return;
    }
    let anchor_offset = match &stoat.pending_signature_help {
        Some(p) => p.anchor_offset,
        None => return,
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
        let cursor_screen = match cursor_screen_position(editor, content_area, anchor_offset) {
            Some(p) => p,
            None => return,
        };
        (content_area, cursor_screen)
    };

    let modal_style = stoat.theme.get(crate::theme::scope::UI_MODAL_HINTS);

    let interior_width = content_area.width.saturating_sub(2);
    if interior_width == 0 {
        return;
    }

    let popup = match stoat.pending_signature_help.as_ref() {
        Some(p) => p,
        None => return,
    };
    let label = truncate_to_width(&popup.label, interior_width as usize);
    let doc = popup
        .doc
        .as_ref()
        .map(|d| truncate_to_width(d, interior_width as usize));

    let max_line_width = label
        .chars()
        .count()
        .max(doc.as_ref().map(|d| d.chars().count()).unwrap_or(0)) as u16;
    let line_count = 1 + doc.is_some() as u16;
    let popup_width = (max_line_width + 2).clamp(3, content_area.width.max(3));
    let popup_height = (line_count + 2).clamp(3, content_area.height.max(3));

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
        Some(" signature "),
        modal_style,
        &stoat.theme,
        scene,
    );

    let label_row = inner.y;
    for (col_idx, ch) in label.chars().enumerate() {
        let col = inner.x + col_idx as u16;
        if col >= inner.x + inner.width {
            break;
        }
        let style = match &popup.active_param {
            Some(range) if range.contains(&col_idx) => modal_style.add_modifier(Modifier::BOLD),
            _ => modal_style,
        };
        buf[(col, label_row)].set_char(ch).set_style(style);
    }

    if let Some(doc) = &doc {
        let doc_row = inner.y + 1;
        if doc_row < inner.y + inner.height {
            let doc_style = modal_style.add_modifier(Modifier::DIM);
            for (col_idx, ch) in doc.chars().enumerate() {
                let col = inner.x + col_idx as u16;
                if col >= inner.x + inner.width {
                    break;
                }
                buf[(col, doc_row)].set_char(ch).set_style(doc_style);
            }
        }
    }
}

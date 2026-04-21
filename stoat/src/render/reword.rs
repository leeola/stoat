use crate::{
    editor_state::EditorState,
    pane::Pane,
    render::{
        editor::render_editor,
        layout::split_pane_status,
        pane::render_overlay_status,
        text::{truncate_to_cols, write_str},
        FrameCtx,
    },
};
use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};

/// Modal reword editor: bordered frame with a header, an original-message
/// reference line, and the editable commit message rendered through the
/// real [`render_editor`] so the user gets full normal/insert modal
/// editing (motions, multi-line, selections).
///
/// `current_mode` is the live `Stoat::mode` string and is shown in the
/// help footer so users can see whether they're in the normal or insert
/// sub-mode.
pub(crate) fn render_reword(
    pane: &Pane,
    is_focused: bool,
    editor: &mut EditorState,
    cherry_picked_commit: &str,
    original_message: &str,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
) {
    let current_mode = frame.mode;
    let theme = frame.theme;
    let (inner, status_area) = split_pane_status(pane.area);
    render_overlay_status(status_area, is_focused, frame, "reword", buf);
    if inner.width < 10 || inner.height < 4 {
        return;
    }

    let header_style = theme
        .get(crate::theme::scope::VCS_REBASE_REWORD)
        .add_modifier(Modifier::BOLD);
    let dim = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let body_style = theme.get(crate::theme::scope::UI_TEXT);

    let short = cherry_picked_commit.chars().take(7).collect::<String>();
    write_str(
        buf,
        inner.x,
        inner.y,
        &format!("reword {short} [{current_mode}]"),
        header_style,
    );
    let help = if current_mode == "reword_insert" {
        "Escape normal   Ctrl-s save   (empty message aborts)"
    } else {
        "i insert   h/j/k/l move   Ctrl-s save   Escape abort"
    };
    write_str(buf, inner.x, inner.y + 1, help, dim);
    write_str(
        buf,
        inner.x,
        inner.y + 2,
        &truncate_to_cols(
            &format!(
                "original: {}",
                original_message.lines().next().unwrap_or("")
            ),
            inner.width as usize,
        ),
        dim,
    );

    let editor_top = inner.y + 4;
    if editor_top >= inner.y + inner.height {
        return;
    }
    let editor_rect = Rect {
        x: inner.x,
        y: editor_top,
        width: inner.width,
        height: inner.y + inner.height - editor_top,
    };
    render_editor(editor, editor_rect, body_style, theme, buf, is_focused);
}

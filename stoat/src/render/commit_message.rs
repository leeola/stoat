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
    theme::scope,
};
use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};
use std::path::PathBuf;
use stoat_widgets::ApcScene;

/// Paint the commit message screen over `pane`.
///
/// The screen shows a header with the editor's mode, a help row that follows
/// the mode, the staged paths, and the message editor. The message renders
/// through the real [`render_editor`], so it takes full modal editing.
pub(crate) fn render_commit_message(
    pane: &Pane,
    is_focused: bool,
    editor: &mut EditorState,
    staged: &[PathBuf],
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let current_mode = frame.mode;
    let theme = frame.theme;
    let (inner, status_area) = split_pane_status(pane.area);
    render_overlay_status(status_area, is_focused, frame.screen, frame, buf, scene);
    if inner.width < 10 || inner.height < 4 {
        return;
    }

    let header_style = theme
        .get(scope::VCS_COMMIT_SUMMARY)
        .add_modifier(Modifier::BOLD);
    let dim = theme.get(scope::UI_TEXT_MUTED);
    let body_style = theme.get(scope::UI_TEXT);

    write_str(
        buf,
        inner.x,
        inner.y,
        &format!("commit [{current_mode}]"),
        header_style,
    );
    let help = if current_mode == "insert" {
        "Escape normal   Ctrl-s commit   (empty message aborts)"
    } else {
        "i insert   h/j/k/l move   Ctrl-s commit   Escape abort"
    };
    write_str(buf, inner.x, inner.y + 1, help, dim);
    let names: Vec<String> = staged
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    write_str(
        buf,
        inner.x,
        inner.y + 2,
        &truncate_to_cols(
            &format!("{} staged: {}", staged.len(), names.join(", ")),
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
    render_editor(
        editor,
        editor_rect,
        body_style,
        theme,
        Some(frame.chrome),
        buf,
        is_focused,
    );
}

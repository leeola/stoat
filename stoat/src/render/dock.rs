use crate::{
    pane::{DockPanel, View},
    render::{editor::render_editor, FrameCtx, PaneCtx},
};
use ratatui::{
    buffer::Buffer,
    widgets::{Block, Borders, Clear, Widget},
};

pub(crate) fn render_dock_minimized(
    dock: &DockPanel,
    is_focused: bool,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let area = dock.area;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let style = if is_focused {
        theme.get(crate::theme::scope::UI_BORDER_FOCUSED)
    } else {
        theme.get(crate::theme::scope::UI_BORDER_INACTIVE)
    };
    for y in area.y..area.y + area.height {
        if let Some(cell) = buf.cell_mut((area.x, y)) {
            cell.set_char('│').set_style(style);
        }
    }
}

pub(crate) fn render_dock_open(
    dock: &DockPanel,
    is_focused: bool,
    ctx: PaneCtx<'_>,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
) {
    let area = dock.area;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let theme = frame.theme;
    let border_style = if is_focused {
        theme.get(crate::theme::scope::UI_BORDER_FOCUSED)
    } else {
        theme.get(crate::theme::scope::UI_BORDER_INACTIVE)
    };

    Clear.render(area, buf);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(area);
    block.render(area, buf);

    let PaneCtx { editors, .. } = ctx;

    if let View::Editor(editor_id) = &dock.view
        && let Some(editor) = editors.get_mut(*editor_id)
    {
        render_editor(editor, inner, border_style, theme, buf, is_focused);
    }
}

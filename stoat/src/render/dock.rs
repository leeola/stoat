use crate::{
    pane::{DockPanel, View},
    render::{editor::render_editor, FrameCtx, PaneCtx},
};
use ratatui::{
    buffer::Buffer,
    widgets::{Clear, Widget},
};

pub(crate) fn render_dock_minimized(
    dock: &DockPanel,
    is_focused: bool,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: Option<&mut stoatty_widgets::ApcScene>,
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
    crate::render::chrome::vline(buf, area.x, area.y, area.height, style, scene);
}

pub(crate) fn render_dock_open(
    dock: &DockPanel,
    is_focused: bool,
    ctx: PaneCtx<'_>,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: Option<&mut stoatty_widgets::ApcScene>,
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
    let inner = crate::render::chrome::modal_frame(buf, area, None, border_style, theme, scene);

    let PaneCtx { editors, .. } = ctx;

    if let View::Editor(editor_id) = &dock.view
        && let Some(editor) = editors.get_mut(*editor_id)
    {
        render_editor(editor, inner, border_style, theme, buf, is_focused);
    }
}

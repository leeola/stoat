use crate::{
    file_finder::{FileFinder, FinderScope},
    render::text::write_str,
    workspace::Workspace,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Borders, Clear, Widget},
};

/// The on-screen rectangles of the file finder modal, derived from a terminal
/// `area` by [`file_finder_layout`].
///
/// Shared by the renderer and the smooth-scroll emit so the pooled list region
/// matches the painted one exactly.
pub(crate) struct FinderLayout {
    /// The bordered modal box.
    pub(crate) modal: Rect,
    /// Inside the border: prompt, input, separator, and body.
    pub(crate) inner: Rect,
    /// The result list, also the smooth-scroll pool region.
    pub(crate) list: Rect,
    /// The preview pane, present only when the body is wide enough.
    pub(crate) preview: Option<Rect>,
}

/// Lay out the file finder modal within `area`, or `None` when `area` is too
/// small to host it.
pub(crate) fn file_finder_layout(area: Rect) -> Option<FinderLayout> {
    if area.width < 40 || area.height < 12 {
        return None;
    }

    let box_width = 120u16.min(area.width.saturating_sub(4));
    let box_height = 32u16.min(area.height.saturating_sub(4));
    if box_width < 40 || box_height < 12 {
        return None;
    }

    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let modal = Rect::new(x, y, box_width, box_height);
    // The title rides the top border, so it does not shrink the inner rect.
    let inner = Block::default().borders(Borders::ALL).inner(modal);

    let body_top = inner.y + 2;
    let body_height = (inner.y + inner.height).saturating_sub(body_top);
    if body_height == 0 {
        return None;
    }
    let body_width = inner.width;

    let (list, preview) = crate::render::picker::split_list_preview(
        inner.x,
        body_top,
        body_width,
        body_height,
        80,
        24,
    );

    Some(FinderLayout {
        modal,
        inner,
        list,
        preview,
    })
}

pub(crate) fn render_file_finder(
    finder: &mut FileFinder,
    ws: &mut Workspace,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    mut scene: Option<&mut stoatty_widgets::ApcScene>,
) {
    let Some(layout) = file_finder_layout(area) else {
        return;
    };

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PALETTE);
    let title = match finder.scope() {
        FinderScope::All => " file finder (all) ",
        FinderScope::Modified => " file finder (modified) ",
        FinderScope::Buffers => " file finder (buffers) ",
    };
    Clear.render(layout.modal, buf);
    crate::render::chrome::modal_frame(
        buf,
        layout.modal,
        Some(title),
        modal_style,
        theme,
        scene.as_deref_mut(),
    );

    let inner = layout.inner;
    let prompt_style = theme.get(crate::theme::scope::UI_PROMPT);
    let muted_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);

    let input_row = inner.y;
    write_str(buf, inner.x, input_row, ">", prompt_style);
    let input_area = Rect::new(inner.x + 2, input_row, inner.width.saturating_sub(2), 1);
    finder.input.render(
        &mut ws.editors,
        input_area,
        true,
        "prompt",
        theme,
        &std::collections::BTreeMap::new(),
        buf,
    );

    let separator_row = inner.y + 1;
    crate::render::chrome::hline(
        buf,
        inner.x,
        separator_row,
        inner.width,
        muted_style,
        scene.as_deref_mut(),
    );

    if let Some(preview_rect) = layout.preview {
        crate::render::chrome::vline(
            buf,
            layout.list.x + layout.list.width,
            layout.list.y,
            layout.list.height,
            muted_style,
            scene,
        );
        render_preview(finder, preview_rect, theme, ws, buf);
    }

    finder.active_core().picklist.viewport_rows = Some(layout.list.height as usize);
    render_list(finder, layout.list, theme, buf);
}

fn render_list(finder: &FileFinder, area: Rect, theme: &crate::theme::Theme, buf: &mut Buffer) {
    let rows = area.height as usize;
    let start_row = finder
        .active_core_ref()
        .picklist
        .selected
        .saturating_sub(rows.saturating_sub(1));
    paint_finder_rows(finder, area, start_row, theme, buf);
}

/// Paint finder result rows into `area` starting at `start_row`.
///
/// A thin adapter over [`crate::render::picker::paint_path_rows`], kept because
/// the smooth-scroll pool paints pages through a `&FileFinder`.
pub(crate) fn paint_finder_rows(
    finder: &FileFinder,
    area: Rect,
    start_row: usize,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let core = finder.active_core_ref();
    crate::render::picker::paint_path_rows(
        &core.picklist,
        &core.git_root,
        area,
        start_row,
        theme,
        buf,
    );
}

fn render_preview(
    finder: &FileFinder,
    area: Rect,
    theme: &crate::theme::Theme,
    ws: &mut Workspace,
    buf: &mut Buffer,
) {
    crate::render::picker::render_picker_preview(
        &finder.active_core_ref().preview,
        area,
        theme,
        ws,
        buf,
    );
}

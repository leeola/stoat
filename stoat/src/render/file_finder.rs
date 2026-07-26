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
///
/// `content` is the cells the caller's list would need in full. Below the
/// recommended 120x32 it changes nothing, and a data-heavy caller passes
/// [`u16::MAX`] to ask for the whole area. `zoom` is the caller's step count
/// from [`modal_zoom`](crate::app::Stoat::modal_zoom).
///
/// Code search and the commit picker share this layout, so all three modals
/// keep one box rule and one list/preview split.
pub(crate) fn file_finder_layout(
    area: Rect,
    content: (u16, u16),
    zoom: i8,
) -> Option<FinderLayout> {
    let modal = crate::render::chrome::modal_box(area, content, (120, 32), (40, 12), zoom)?;
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
    zoom: i8,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    let Some(layout) = file_finder_layout(area, finder.content_size, zoom) else {
        return;
    };

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PALETTE);
    let title: String = if finder.browse.is_some() {
        " file finder (browse) ".to_string()
    } else {
        match finder.scope() {
            FinderScope::All => " file finder (all) ".to_string(),
            FinderScope::Modified => " file finder (modified) ".to_string(),
            FinderScope::Buffers => " file finder (buffers) ".to_string(),
            FinderScope::Named(name) => format!(" file finder ({name}) "),
            FinderScope::AllWorkspaces => " file finder (all workspaces) ".to_string(),
        }
    };
    Clear.render(layout.modal, buf);
    crate::render::chrome::modal_frame(
        buf,
        layout.modal,
        Some(&title),
        modal_style,
        theme,
        &mut *scene,
    );

    let inner = layout.inner;
    let prompt_style = theme.get(crate::theme::scope::UI_PROMPT);
    let separator_style = theme.get(crate::theme::scope::UI_BORDER_INACTIVE);

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
        separator_style,
        Some(&mut *scene),
    );

    if let Some(preview_rect) = layout.preview {
        crate::render::chrome::vline(
            buf,
            layout.list.x + layout.list.width,
            layout.list.y,
            layout.list.height,
            separator_style,
            scene,
        );
        render_preview(finder, preview_rect, theme, ws, buf);
    }

    finder.active_core().picklist.viewport_rows = Some(layout.list.height as usize);
    render_list(finder, layout.list, theme, buf);
}

fn render_list(finder: &FileFinder, area: Rect, theme: &crate::theme::Theme, buf: &mut Buffer) {
    let rows = area.height as usize;
    let start_row =
        crate::render::picker::window_start(finder.active_core_ref().picklist.selected, rows);
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
    let prefix = finder
        .browse
        .as_ref()
        .map(|browse| browse.typed_dir.as_str())
        .unwrap_or_default();
    crate::render::picker::paint_path_rows(
        &core.picklist,
        &core.git_root,
        prefix,
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

#[cfg(test)]
mod tests {
    use super::file_finder_layout;
    use ratatui::layout::Rect;

    /// Every caller of this layout must fit inside the same area, so the box a
    /// short list gets and the box a data-heavy modal gets are checked together.
    fn box_of(area: Rect, content: (u16, u16)) -> Rect {
        file_finder_layout(area, content, 0)
            .expect("the area hosts the finder")
            .modal
    }

    #[test]
    fn a_short_list_keeps_the_recommended_box() {
        let area = Rect::new(0, 0, 200, 60);
        assert_eq!(
            box_of(area, (120, 9)),
            Rect::new(40, 14, 120, 32),
            "content under the recommended size leaves the box at 120x32, centered"
        );
    }

    #[test]
    fn a_long_list_grows_the_box_to_fit_it() {
        let area = Rect::new(0, 0, 200, 60);
        assert_eq!(
            box_of(area, (120, 44)),
            Rect::new(40, 8, 120, 44),
            "a list past the recommended height takes the rows it asks for"
        );
    }

    #[test]
    fn a_data_heavy_modal_fills_the_area_less_its_margin() {
        // Code search and the commit picker declare u16::MAX rather than
        // measuring, so they land on the largest box the margin allows.
        let area = Rect::new(0, 0, 200, 60);
        assert_eq!(
            box_of(area, (u16::MAX, u16::MAX)),
            Rect::new(2, 2, 196, 56),
            "a max declaration clamps to the area less the full margin"
        );
    }

    #[test]
    fn an_area_too_small_for_the_minimum_hosts_nothing() {
        assert!(
            file_finder_layout(Rect::new(0, 0, 41, 60), (120, 32), 0).is_none(),
            "a width under the 40-column minimum plus its thinnest margin fails"
        );
        assert!(
            file_finder_layout(Rect::new(0, 0, 200, 13), (120, 32), 0).is_none(),
            "so does a height under the 12-row minimum plus its thinnest margin"
        );
    }
}

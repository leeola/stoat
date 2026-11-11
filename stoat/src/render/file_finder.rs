use crate::{
    display_map::DisplayPoint,
    editor_state::EditorState,
    file_finder::{FileFinder, FinderScope},
    picker::{write_row_display, PreviewSource},
    render::{
        picker::{FilterHeader, Footer, FILTER_PROMPT},
        text::write_str_clipped,
    },
    workspace::Workspace,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Borders},
};
use std::path::Path;
use unicode_width::UnicodeWidthStr;

/// The on-screen rectangles of the file finder modal, derived from a terminal
/// `area` by [`file_finder_layout`] or [`file_finder_layout_with_chrome`].
///
/// Shared by the renderer and the smooth-scroll emit so the pooled list region
/// matches the painted one exactly.
pub(crate) struct FinderLayout {
    /// The bordered modal box.
    pub(crate) modal: Rect,
    /// Inside the border: prompt, input, separator, and body.
    pub(crate) inner: Rect,
    /// The result list, also the smooth-scroll pool region. It is empty when
    /// the finder collapses to its header for a query with no match.
    pub(crate) list: Rect,
    /// The preview pane's editor, present only when the body is wide enough.
    pub(crate) preview: Option<Rect>,
    /// The path header over the preview and the rule under it, which only
    /// [`file_finder_layout_with_chrome`] sets. The preview starts below it.
    pub(crate) preview_header: Option<Rect>,
    /// The footer row of key hints and page dots, which only
    /// [`file_finder_layout_with_chrome`] sets. The list and the preview end
    /// above it.
    pub(crate) footer: Option<Rect>,
}

/// Rows the finder's preview gives its path header, the header row and the
/// rule under it.
const PREVIEW_HEADER_ROWS: u16 = 2;

/// Rows of a finder box that shows no list, which are the two border rows and
/// the filter header's query row and rule.
const HEADER_BOX_ROWS: u16 = 4;

/// The finder's footer hints, each a bound key and the word for what it does.
const FINDER_HINTS: [(&str, &str); 4] = [
    ("Enter", "open"),
    ("C-d/C-u", "preview"),
    ("C-f/C-b", "page"),
    ("S-Tab", "scope"),
];

/// Lay out the file finder modal within `area`, or `None` when `area` is too
/// small to host it.
///
/// `content` is the cells the caller's list needs in full. Below the
/// recommended box, [`list_modal_width`](crate::render::chrome::list_modal_width)
/// by 32 rows, it changes nothing, and a data-heavy caller passes [`u16::MAX`]
/// to ask for the whole area. `zoom` is the caller's step count
/// from [`modal_zoom`](crate::app::Stoat::modal_zoom), and `list_percent` the
/// width its list takes from
/// [`modal_split`](crate::app::Stoat::modal_split).
///
/// Code search and the commit picker share this layout, so all three modals
/// keep one box rule and one list/preview split.
pub(crate) fn file_finder_layout(
    area: Rect,
    content: (u16, u16),
    zoom: i8,
    list_percent: u16,
) -> Option<FinderLayout> {
    let modal = crate::render::chrome::modal_box(
        area,
        content,
        (crate::render::chrome::list_modal_width(area), 32),
        (40, 12),
        zoom,
    )?;
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
        crate::render::picker::MIN_PANE_COLUMNS,
        list_percent,
    );

    Some(FinderLayout {
        modal,
        inner,
        list,
        preview,
        preview_header: None,
        footer: None,
    })
}

/// Lay out the file finder as [`file_finder_layout`] does, plus the rows only
/// the finder shows, which are its footer and the path header over its
/// preview.
///
/// The footer takes the inner rect's last row, so the list and the preview end
/// above it. The preview then covers only the editor below its header, which is
/// the rect the pooled preview page fills and a wheel scrolls. A body of one row
/// keeps no footer, and a preview too short to keep an editor row under the
/// header gets no header.
///
/// `no_match` collapses the box to its header rows, as
/// [`FileFinder::shows_no_match`] asks. The box keeps the full box's top and
/// width, so the query row stays in place while a query loses and regains its
/// matches. The list is then an empty rect, and there is no preview, preview
/// header, or footer.
pub(crate) fn file_finder_layout_with_chrome(
    area: Rect,
    content: (u16, u16),
    zoom: i8,
    list_percent: u16,
    no_match: bool,
) -> Option<FinderLayout> {
    let mut layout = file_finder_layout(area, content, zoom, list_percent)?;
    if no_match {
        let modal = Rect {
            height: HEADER_BOX_ROWS,
            ..layout.modal
        };
        let inner = Block::default().borders(Borders::ALL).inner(modal);
        return Some(FinderLayout {
            modal,
            inner,
            list: Rect {
                y: inner.y + inner.height,
                height: 0,
                ..inner
            },
            preview: None,
            preview_header: None,
            footer: None,
        });
    }

    if layout.list.height > 1 {
        let inner = layout.inner;
        layout.footer = Some(Rect::new(
            inner.x,
            inner.y + inner.height - 1,
            inner.width,
            1,
        ));
        layout.list.height -= 1;
        if let Some(preview) = layout.preview.as_mut() {
            preview.height -= 1;
        }
    }
    if let Some(preview) = layout.preview.filter(|p| p.height > PREVIEW_HEADER_ROWS) {
        layout.preview_header = Some(Rect {
            height: PREVIEW_HEADER_ROWS,
            ..preview
        });
        layout.preview = Some(Rect {
            y: preview.y + PREVIEW_HEADER_ROWS,
            height: preview.height - PREVIEW_HEADER_ROWS,
            ..preview
        });
    }
    Some(layout)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_file_finder(
    finder: &mut FileFinder,
    ws: &mut Workspace,
    home: Option<&Path>,
    theme: &crate::theme::Theme,
    chrome: &crate::render::editor::ResolvedChrome,
    area: Rect,
    zoom: i8,
    list_percent: u16,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let Some(layout) = file_finder_layout_with_chrome(
        area,
        finder.content_size,
        zoom,
        list_percent,
        finder.shows_no_match(),
    ) else {
        return;
    };

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PALETTE);
    crate::render::clear_modal(layout.modal, buf, theme, modal_style);
    crate::render::chrome::modal_frame(buf, layout.modal, None, modal_style, theme, &mut *scene);

    let inner = layout.inner;
    let separator_style = theme.get(crate::theme::scope::UI_BORDER_INACTIVE);

    let label = finder_label(finder);
    let chip = finder
        .browse
        .is_none()
        .then(|| format!("S-Tab \u{00B7} {}", scope_words(&finder.next_scope())));
    let chips = chip.as_deref().map(|chip| [(chip, true)]);
    let count = {
        let list = &finder.active_core_ref().picklist;
        (list.filtered.len(), list.base.len())
    };
    crate::render::picker::filter_header(
        buf,
        inner,
        &FilterHeader {
            label: &label,
            prompt: FILTER_PROMPT,
            count: Some(count),
            chips: chips.as_ref().map_or(&[][..], |chips| &chips[..]),
        },
        &finder.input,
        ws,
        theme,
        chrome,
        &mut *scene,
    );

    if let Some(preview_rect) = layout.preview {
        // The preview's editor paints only the cells it has content for, so
        // the pane takes the editor surface first, as a pane does and as the
        // pages that pool it blank to.
        if scene.live() {
            crate::render::clear_surface(preview_rect, buf, theme);
            if let Some(header) = layout.preview_header {
                crate::render::clear_surface(header, buf, theme);
            }
        }
        crate::render::chrome::vline(
            buf,
            layout.list.x + layout.list.width,
            layout.list.y,
            layout.list.height,
            separator_style,
            scene,
        );
        crate::render::picker::render_picker_preview(
            &finder.active_core_ref().preview,
            preview_rect,
            theme,
            chrome,
            ws,
            buf,
        );
        if let Some(header) = layout.preview_header {
            paint_preview_header(finder, header, home, theme, ws, buf, scene);
        }
    }

    finder.active_core().picklist.viewport_rows = Some(layout.list.height as usize);
    render_list(finder, home, layout.list, theme, buf, scene);

    if let Some(footer) = layout.footer {
        let list = &finder.active_core_ref().picklist;
        let pages = crate::render::picker::page_of(
            list.selected,
            list.filtered.len(),
            layout.list.height as usize,
        );
        let parts = Footer {
            hints: &FINDER_HINTS,
            pages,
            ..Footer::default()
        };
        crate::render::picker::modal_footer(buf, footer, &parts, theme, scene);
    }
}

/// The finder's header label, which names the list on show unless it is the
/// plain list of every file.
///
/// The words after `Files` name the scope or browse mode, and then a fallback
/// of ignored files or buffers listed over a diff.
fn finder_label(finder: &FileFinder) -> String {
    let scope = match (finder.browse.is_some(), finder.scope()) {
        (true, _) => Some("browse"),
        (false, FinderScope::All) => None,
        (false, scope) => Some(scope_words(scope)),
    };
    let suffix = if finder.fallback_active() {
        Some("ignored")
    } else if finder.diff_paths.is_some()
        && matches!(
            finder.scope(),
            FinderScope::Buffers | FinderScope::ModifiedBuffers
        )
    {
        Some("diff")
    } else {
        None
    };
    match (scope, suffix) {
        (None, None) => "Files".to_owned(),
        (Some(words), None) | (None, Some(words)) => format!("Files \u{00B7} {words}"),
        (Some(scope), Some(suffix)) => format!("Files \u{00B7} {scope}, {suffix}"),
    }
}

/// The words the finder's header gives `scope`, in its label and in the chip
/// that names where Shift-Tab goes next.
fn scope_words(scope: &FinderScope) -> &str {
    match scope {
        FinderScope::All => "all files",
        FinderScope::Modified => "git only",
        FinderScope::Diff => "diff",
        FinderScope::Buffers => "buffers",
        FinderScope::ModifiedBuffers => "modified buffers",
        FinderScope::Named(name) => name.as_str(),
        FinderScope::AllWorkspaces => "all workspaces",
    }
}

fn render_list(
    finder: &FileFinder,
    home: Option<&Path>,
    area: Rect,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let rows = area.height as usize;
    let start_row =
        crate::render::picker::window_start(finder.active_core_ref().picklist.selected, rows);
    paint_finder_rows(finder, home, area, start_row, theme, true, buf, scene);
}

/// Paint finder result rows into `area` starting at `start_row`.
///
/// A thin adapter over [`crate::render::picker::paint_path_rows`], kept because
/// the smooth-scroll pool paints pages through a `&FileFinder`. A page passes
/// `panels` false, as that function describes.
///
/// The rows paint muted while a fallback of ignored files stands in for the
/// list, so they read apart from the files the list holds.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_finder_rows(
    finder: &FileFinder,
    home: Option<&Path>,
    area: Rect,
    start_row: usize,
    theme: &crate::theme::Theme,
    panels: bool,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let core = finder.active_core_ref();
    let prefix = row_prefix(finder);
    let row_style = if finder.fallback_active() {
        theme.get(crate::theme::scope::UI_TEXT_MUTED)
    } else {
        theme.get(crate::theme::scope::UI_TEXT)
    };
    crate::render::picker::paint_path_rows(
        &core.picklist,
        &core.git_root,
        home,
        prefix,
        &finder.modified_paths,
        area,
        start_row,
        theme,
        row_style,
        panels,
        buf,
        scene,
    );
}

/// The typed directory that browse-mode rows start with, empty outside browse.
fn row_prefix(finder: &FileFinder) -> &str {
    finder
        .browse
        .as_ref()
        .map(|browse| browse.typed_dir.as_str())
        .unwrap_or_default()
}

/// Paint the path header over the finder's preview into `header`, which is the
/// shown source's directory and name, the lines the pane shows at the right,
/// and the rule under them.
///
/// Under a live scene the header sits on the editor surface, which the
/// preview below it shares, and a plain terminal keeps the modal's fill under
/// both. A blank pane leaves the header row empty, and a source with no path
/// shows only its lines.
fn paint_preview_header(
    finder: &FileFinder,
    header: Rect,
    home: Option<&Path>,
    theme: &crate::theme::Theme,
    ws: &mut Workspace,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let (y, end) = (header.y, header.x + header.width);
    let core = finder.active_core_ref();
    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);

    let mut path_end = end.saturating_sub(1);
    let lines = core
        .preview
        .source()
        .and_then(|_| ws.editors.get_mut(core.preview.editor))
        .and_then(shown_lines);
    if let Some((top, bottom)) = lines {
        let text = format!("lines {top}\u{2013}{bottom}");
        let x = path_end.saturating_sub(text.width() as u16);
        write_str_clipped(buf, x, y, &text, muted, end);
        path_end = x.saturating_sub(2);
    }

    let path = core.preview.source().and_then(|source| match source {
        PreviewSource::File(path) | PreviewSource::Directory(path) => Some(path.as_path()),
        PreviewSource::Buffer(id) => ws.buffers.path_for(*id),
        PreviewSource::Terminal(_) | PreviewSource::Markdown(_) | PreviewSource::Text(_) => None,
    });
    if let Some(path) = path {
        let mut label = row_prefix(finder).to_owned();
        write_row_display(
            &mut label,
            path,
            &core.git_root,
            core.picklist.display_roots.as_deref(),
            home,
        );
        let split = label
            .char_indices()
            .nth(crate::render::picker::name_start(&label))
            .map_or(label.len(), |(byte, _)| byte);
        let (dir, name) = label.split_at(split);

        let text = theme.get(crate::theme::scope::UI_TEXT);
        let x = header.x + 1;
        // The name is what the header is for, so a row too narrow for both
        // keeps it and drops the directory.
        let fits = x as usize + dir.width() + 1 + name.width() <= path_end as usize;
        match dir.is_empty() || !fits {
            true => write_str_clipped(buf, x, y, name, text, path_end),
            false => {
                write_str_clipped(buf, x, y, dir, muted, path_end);
                let name_x = x + dir.width() as u16 + 1;
                write_str_clipped(buf, name_x, y, name, text, path_end);
            },
        }
    }

    crate::render::chrome::hline(
        buf,
        header.x,
        y + 1,
        header.width,
        theme.get(crate::theme::scope::UI_BORDER_INACTIVE),
        scene,
    );
}

/// The first and last buffer lines, counted from 1, that `editor` showed in
/// its last frame, or `None` before its first layout.
fn shown_lines(editor: &mut EditorState) -> Option<(u32, u32)> {
    let rows = editor.viewport_rows?;
    let snapshot = editor.display_map.snapshot();
    let last = editor
        .scroll_row
        .saturating_add(rows.saturating_sub(1))
        .min(snapshot.max_point().row);
    let top = snapshot.display_to_buffer(DisplayPoint::new(editor.scroll_row, 0))?;
    let bottom = snapshot.display_to_buffer(DisplayPoint::new(last, 0))?;
    Some((top.row + 1, bottom.row + 1))
}

#[cfg(test)]
mod tests {
    use super::{file_finder_layout, file_finder_layout_with_chrome};
    use crate::render::picker::DEFAULT_LIST_PERCENT;
    use ratatui::layout::Rect;

    /// Every caller of this layout must fit inside the same area, so the box a
    /// short list gets and the box a data-heavy modal gets are checked together.
    fn box_of(area: Rect, content: (u16, u16)) -> Rect {
        file_finder_layout(area, content, 0, DEFAULT_LIST_PERCENT)
            .expect("the area hosts the finder")
            .modal
    }

    #[test]
    fn a_short_list_keeps_the_recommended_box() {
        let area = Rect::new(0, 0, 200, 60);
        assert_eq!(
            box_of(area, (120, 9)),
            Rect::new(20, 14, 160, 32),
            "content under the recommended size leaves the box at 160x32, centered"
        );
    }

    #[test]
    fn a_long_list_grows_the_box_to_fit_it() {
        let area = Rect::new(0, 0, 200, 60);
        assert_eq!(
            box_of(area, (120, 44)),
            Rect::new(20, 8, 160, 44),
            "a list past the recommended height takes the rows it asks for"
        );
    }

    /// The full box for this content is 160x32 at (20, 14), so the collapsed
    /// box keeps that top and width.
    #[test]
    fn a_finder_with_no_match_keeps_its_header_rows_alone() {
        let layout = file_finder_layout_with_chrome(
            Rect::new(0, 0, 200, 60),
            (120, 9),
            0,
            DEFAULT_LIST_PERCENT,
            true,
        )
        .expect("the area hosts the finder");
        assert_eq!(
            (
                layout.modal,
                layout.inner,
                layout.list,
                layout.preview,
                layout.preview_header,
                layout.footer,
            ),
            (
                Rect::new(20, 14, 160, 4),
                Rect::new(21, 15, 158, 2),
                Rect::new(21, 17, 158, 0),
                None,
                None,
                None,
            ),
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
            file_finder_layout(Rect::new(0, 0, 41, 60), (120, 32), 0, DEFAULT_LIST_PERCENT)
                .is_none(),
            "a width under the 40-column minimum plus its thinnest margin fails"
        );
        assert!(
            file_finder_layout(Rect::new(0, 0, 200, 13), (120, 32), 0, DEFAULT_LIST_PERCENT)
                .is_none(),
            "so does a height under the 12-row minimum plus its thinnest margin"
        );
    }
}

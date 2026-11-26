use crate::{
    display_map::DisplayPoint,
    editor_state::EditorState,
    file_finder::{FileFinder, FinderScope},
    picker::{write_row_display, PreviewSource},
    render::{
        paint::style_rgb,
        picker::{FilterHeader, Footer, PathRows, RowMarks, RowStart, FILTER_PROMPT},
        pixel_rows::RowMetrics,
        text::{clip_to_width, write_str_clipped},
        TypeScale,
    },
    theme::TypeRole,
    workspace::Workspace,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{Block, Borders, StatefulWidget},
};
use std::path::Path;
use stoat_widgets::{
    bar::Bar,
    text_run::{self, TextRun},
};
use unicode_width::UnicodeWidthStr;

/// The on-screen rectangles of the file finder modal, derived from a terminal
/// `area` by [`file_finder_layout`] or [`finder_layout`].
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
    /// The preview pane's editor, present only when the body is wide enough.
    pub(crate) preview: Option<Rect>,
    /// The path header over the preview and the rule under it, which only
    /// [`finder_layout`] sets. The preview starts below it.
    pub(crate) preview_header: Option<Rect>,
    /// The key line, on the inner rect's last row and the box's bottom row,
    /// with the list and the preview ending above it. A body of one row keeps
    /// none.
    pub(crate) footer: Option<Rect>,
    /// The plate's width before zoom steps, in columns.
    ///
    /// [`finder_layout`] answers the fitted width, which the finder keeps as
    /// [`FileFinder::plate_width`] so its plate does not narrow while it is
    /// open. [`file_finder_layout`] answers its modal's width.
    pub(crate) plate_base: u16,
}

/// The inner rect of a box of the finder family, its body under the query row
/// and its rule, and its key line.
struct BodyRows {
    inner: Rect,
    top: u16,
    height: u16,
    footer: Option<Rect>,
}

/// Rows the finder's preview gives its path header, the header row and the
/// rule under it.
const PREVIEW_HEADER_ROWS: u16 = 2;

/// The sixteenths of a cell that the shown lines of the preview's path header end
/// inside the header under the plate look, and that the path stops short of them.
const PREVIEW_HEADER_INSET: u16 = 14;

/// The finder's footer hints, each a bound key and the word for what it does.
///
/// The line names the keys that open a file, step the scope, and close the
/// finder. The preview and page keys stay bound with no hint.
const FINDER_HINTS: [(&str, &str); 3] = [("Enter", "open"), ("S-Tab", "scope"), ("Esc", "close")];

/// The footer hints of the Buffer Finder's two scopes, where Enter switches to
/// a buffer that is already open.
const BUFFER_FINDER_HINTS: [(&str, &str); 3] =
    [("Enter", "switch"), ("S-Tab", "scope"), ("Esc", "close")];

/// Lay out the file finder modal within `area`, or `None` when `area` is too
/// small to host it.
///
/// `content` is the cells the caller's list needs in full. Below the
/// recommended box, [`list_modal_width`](crate::render::chrome::list_modal_width)
/// by 32 rows, it changes nothing, and a data-heavy caller passes [`u16::MAX`]
/// to ask for the whole area. `zoom` is the caller's step count
/// from [`modal_zoom`](crate::app::Stoat::modal_zoom), and `list_share` the
/// width its list takes from
/// [`modal_split`](crate::app::Stoat::modal_split).
///
/// The key line takes the inner rect's last row and the bottom border's row,
/// as [`footer_rect`](crate::render::picker::footer_rect) gives it, and the
/// list and the preview end above it.
///
/// Code search and the commit picker share this layout, so all three modals
/// keep one box rule, one key line, and one list/preview split.
pub(crate) fn file_finder_layout(
    area: Rect,
    content: (u16, u16),
    zoom: i8,
    list_share: u16,
) -> Option<FinderLayout> {
    let modal = crate::render::chrome::modal_box(
        area,
        content,
        (crate::render::chrome::list_modal_width(area), 32),
        (40, 12),
        zoom,
    )?;
    let body = body_rows(modal)?;

    let (list, preview) = crate::render::picker::split_list_preview(
        body.inner.x,
        body.top,
        body.inner.width,
        body.height,
        80,
        crate::render::picker::MIN_PANE_COLUMNS,
        list_share,
    );

    Some(FinderLayout {
        modal,
        inner: body.inner,
        list,
        preview,
        preview_header: None,
        footer: body.footer,
        plate_base: modal.width,
    })
}

/// Lay out the File Finder or the Buffer Finder within `area`, its plate
/// fitted to the widest label that its list shows, or `None` when `area` is
/// too small to host it.
///
/// The list takes the widest label among the rows on show, at least 30
/// columns, with its pad and mark cells, as [`list_columns`] gives them. The
/// preview takes one and a half times the list, and at least `preview_min`
/// columns. The plate does not narrow below [`FileFinder::plate_width`], the
/// widest plate the finder has drawn since it opened, and it stops one column
/// inside each side of `area`. `zoom` steps then move the width by a tenth of
/// the area each. The height follows the list's rows as the height of
/// [`file_finder_layout`] does.
///
/// A query that matches no file keeps this box, so the query row stays in
/// place while a query loses and regains its matches.
///
/// A dragged `split` share replaces the list's fitted width, as
/// [`fitted_split`] describes. The preview's top two rows hold its path
/// header, so the preview rect covers only the editor below it, which is the
/// rect the pooled preview page fills and a wheel scrolls. A preview too short
/// to keep an editor row under the header gets no header.
///
/// [`list_columns`]: crate::render::picker::list_columns
/// [`fitted_split`]: crate::render::picker::fitted_split
pub(crate) fn finder_layout(
    area: Rect,
    finder: &FileFinder,
    home: Option<&Path>,
    zoom: i8,
    split: Option<u16>,
    preview_min: u16,
) -> Option<FinderLayout> {
    let rows =
        crate::render::chrome::modal_box(area, (0, finder.content_rows), (0, 32), (40, 12), zoom)?;
    let list = {
        let list_rows = body_rows(rows)?.height;
        let widest = crate::render::picker::widest_label(&finder_rows(finder, home), list_rows);
        crate::render::picker::list_columns(widest)
    };

    let fitted = crate::render::picker::fitted_plate_width(list, preview_min);
    let base =
        crate::render::chrome::fitted_width(area.width, finder.plate_width.max(fitted), 0, 0);
    let width = crate::render::chrome::fitted_width(area.width, base, 40, zoom);
    let modal = Rect {
        x: area.x + (area.width - width) / 2,
        width,
        ..rows
    };
    let body = body_rows(modal)?;

    let (list, preview) = crate::render::picker::fitted_split(
        body.inner.x,
        body.top,
        body.inner.width,
        body.height,
        list,
        preview_min,
        split,
    );
    let (preview_header, preview) = match preview.filter(|p| p.height > PREVIEW_HEADER_ROWS) {
        Some(preview) => (
            Some(Rect {
                height: PREVIEW_HEADER_ROWS,
                ..preview
            }),
            Some(Rect {
                y: preview.y + PREVIEW_HEADER_ROWS,
                height: preview.height - PREVIEW_HEADER_ROWS,
                ..preview
            }),
        ),
        None => (None, preview),
    };

    Some(FinderLayout {
        modal,
        inner: body.inner,
        list,
        preview,
        preview_header,
        footer: body.footer,
        plate_base: base,
    })
}

/// The [`BodyRows`] of a finder-family box `modal`, or `None` when the box
/// holds no body row.
///
/// The body starts two rows into the inner rect, under the query row and its
/// rule. The key line takes the inner rect's last row and the bottom border's
/// row, as [`footer_rect`](crate::render::picker::footer_rect) gives them,
/// once the body has more than one row.
fn body_rows(modal: Rect) -> Option<BodyRows> {
    // The title rides the top border, so it does not shrink the inner rect.
    let inner = Block::default().borders(Borders::ALL).inner(modal);

    let top = inner.y + 2;
    let height = (inner.y + inner.height).saturating_sub(top);
    if height == 0 {
        return None;
    }
    let footer = (height > 1).then(|| crate::render::picker::footer_rect(modal));
    Some(BodyRows {
        inner,
        top,
        height: height - u16::from(footer.is_some()),
        footer,
    })
}

/// Paint the File Finder or the Buffer Finder into `area`, laid out as
/// [`finder_layout`] lays it out.
///
/// The paint keeps the plate's width before zoom steps in
/// [`FileFinder::plate_width`], so the next frame draws a plate at least as
/// wide while the finder stays open.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_file_finder(
    finder: &mut FileFinder,
    ws: &mut Workspace,
    home: Option<&Path>,
    theme: &crate::theme::Theme,
    chrome: &crate::render::editor::ResolvedChrome,
    area: Rect,
    zoom: i8,
    split: Option<u16>,
    preview_min: u16,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let Some(layout) = finder_layout(area, finder, home, zoom, split, preview_min) else {
        return;
    };
    finder.plate_width = layout.plate_base;
    let no_match = finder.shows_no_match();

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PALETTE);
    crate::render::chrome::list_plate(buf, layout.modal, modal_style, theme, &mut *scene);

    let inner = layout.inner;
    let separator_style = theme.get(crate::theme::scope::UI_BORDER_INACTIVE);

    let label = finder_label(finder);
    let cycle = finder.scope_cycle();
    let chips: Vec<(&str, bool)> = match finder.browse {
        Some(_) => Vec::new(),
        None => cycle
            .iter()
            .map(|scope| (scope_words(scope), scope == finder.scope()))
            .collect(),
    };
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
            chips: &chips,
        },
        &finder.input,
        ws,
        theme,
        chrome,
        &mut *scene,
    );

    let plate = crate::render::chrome::plate_colors(theme, scene);
    if let Some(plate) = plate {
        let preview_pane = layout.preview.map(|preview| match layout.preview_header {
            Some(header) => header.union(preview),
            None => preview,
        });
        crate::render::chrome::list_islands(layout.list, preview_pane, plate, theme, buf, scene);
    }
    if let Some(preview_rect) = layout.preview {
        if plate.is_none() {
            // The preview's editor paints only the cells it has content for,
            // so the pane takes the editor surface first, as a pane does and
            // as the pages that pool it blank to.
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
        }
        match no_match {
            true => {
                let pane = layout
                    .preview_header
                    .map_or(preview_rect, |header| header.union(preview_rect));
                paint_no_preview(pane, theme, buf);
            },
            false => {
                crate::render::picker::render_picker_preview(
                    &mut finder.active_core().preview,
                    preview_rect,
                    theme,
                    chrome,
                    ws,
                    buf,
                    scene,
                );
                if let Some(header) = layout.preview_header {
                    paint_preview_header(finder, header, home, theme, ws, buf, scene);
                }
            },
        }
    }

    finder.row_metrics = RowMetrics::of(theme, scene);
    let whole = match no_match {
        true => {
            paint_no_match(&finder.input.text(ws), layout.list, theme, buf);
            0
        },
        false => render_list(finder, home, layout.list, theme, buf, scene),
    };
    finder.active_core().picklist.viewport_rows = Some(whole.max(1));

    if let Some(footer) = layout.footer {
        let hints: &[(&str, &str)] = match lists_buffers(finder) {
            true => &BUFFER_FINDER_HINTS,
            false => &FINDER_HINTS,
        };
        let parts = Footer {
            hints,
            ..Footer::default()
        };
        crate::render::picker::modal_footer(buf, footer, &parts, plate, theme, scene);
    }
}

/// Paint the list of a finder whose query matches no file.
///
/// The list's first row reads `no file matches ` in `ui.text.muted`, then the
/// query's first line in `ui.text`. The row clips at the list's right edge.
fn paint_no_match(query: &str, list: Rect, theme: &crate::theme::Theme, buf: &mut Buffer) {
    const NO_MATCH: &str = "no file matches ";
    if list.is_empty() {
        return;
    }
    let end = list.x + list.width;
    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    write_str_clipped(buf, list.x + 1, list.y, NO_MATCH, muted, end);
    let query = query.lines().next().unwrap_or_default();
    let query_x = list.x + 1 + NO_MATCH.len() as u16;
    let text = theme.get(crate::theme::scope::UI_TEXT);
    write_str_clipped(buf, query_x, list.y, query, text, end);
}

/// Paint `no preview` in `ui.text.disabled` at the center of `pane`.
///
/// `pane` holds the cells of the preview's Island, its path header among
/// them. A finder shows this in place of a preview while its query matches no
/// file.
fn paint_no_preview(pane: Rect, theme: &crate::theme::Theme, buf: &mut Buffer) {
    const NO_PREVIEW: &str = "no preview";
    if pane.is_empty() {
        return;
    }
    let x = pane.x + pane.width.saturating_sub(NO_PREVIEW.len() as u16) / 2;
    let y = pane.y + pane.height / 2;
    let disabled = theme.get(crate::theme::scope::UI_TEXT_DISABLED);
    write_str_clipped(buf, x, y, NO_PREVIEW, disabled, pane.x + pane.width);
}

/// The finder's header label, its name and the words the pills do not say.
///
/// The name is `Buffers` in the Buffer Finder's two scopes and `Files` in
/// every other scope. The pills name the scope, so after the name the label
/// adds only a browse, and then a fallback of ignored files or buffers listed
/// over a diff.
fn finder_label(finder: &FileFinder) -> String {
    let name = match lists_buffers(finder) {
        true => "Buffers",
        false => "Files",
    };
    let scope = finder.browse.is_some().then_some("browse");
    let suffix = if finder.fallback_active() {
        Some("ignored")
    } else if finder.scopes.diff_paths.is_some() && lists_buffers(finder) {
        Some("diff")
    } else {
        None
    };
    match (scope, suffix) {
        (None, None) => name.to_owned(),
        (Some(words), None) | (None, Some(words)) => format!("{name} \u{00B7} {words}"),
        (Some(scope), Some(suffix)) => format!("{name} \u{00B7} {scope}, {suffix}"),
    }
}

/// Whether `finder` shows one of the Buffer Finder's two scopes, which list
/// the open buffers.
fn lists_buffers(finder: &FileFinder) -> bool {
    matches!(
        finder.scope(),
        FinderScope::Buffers | FinderScope::ModifiedBuffers
    )
}

/// The word in the pill that names `scope` in the finder's header.
///
/// The pill of [`FinderScope::AllWorkspaces`] reads `workspaces`, since `all
/// workspaces` beside `all` reads as the same scope twice.
pub(crate) fn scope_words(scope: &FinderScope) -> &str {
    match scope {
        FinderScope::All => "all",
        FinderScope::Modified | FinderScope::ModifiedBuffers => "modified",
        FinderScope::Diff => "diff",
        FinderScope::Buffers => "buffers",
        FinderScope::Named(name) => name.as_str(),
        FinderScope::AllWorkspaces => "workspaces",
    }
}

/// Paint the finder's results into `area` as the window that
/// [`list_window`] derives shows them, and return the count of results that
/// the window shows whole.
///
/// A pixel list, as [`FileFinder::row_metrics`] records one, paints no row
/// here. Its rows stand off the cell grid, so the finder's resident pool draws
/// them over the blank list.
///
/// [`list_window`]: crate::render::picker::list_window
fn render_list(
    finder: &FileFinder,
    home: Option<&Path>,
    area: Rect,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) -> usize {
    let pixels = finder.row_metrics;
    let window = crate::render::picker::list_window(
        &finder_rows(finder, home),
        area.width,
        area.height,
        pixels,
    );
    if let Some(metrics) = pixels {
        return window.whole(metrics.extent(area.height));
    }
    let start = RowStart {
        result: window.first,
        skip: 0,
    };
    paint_finder_rows(finder, home, area, start, theme, true, None, buf, scene);
    window.whole(u32::from(area.height))
}

/// The finder's results as its list paints them, the active picker's rows
/// under the typed directory of a browse.
///
/// The live list, its pooled pages, and the hit test all read the rows through
/// this, so they measure every result alike.
pub(crate) fn finder_rows<'a>(finder: &'a FileFinder, home: Option<&'a Path>) -> PathRows<'a> {
    let core = finder.active_core_ref();
    PathRows {
        picklist: &core.picklist,
        git_root: &core.git_root,
        home,
        prefix: row_prefix(finder),
    }
}

/// Paint finder result rows into `area` from `start`.
///
/// A thin adapter over [`crate::render::picker::paint_path_rows`], kept because
/// the smooth-scroll pool paints pages through a `&FileFinder`. A page passes
/// `panels` false, and the metrics of a pixel list as `pixels`, as that
/// function describes.
///
/// The rows paint muted while a fallback of ignored files stands in for the
/// list, so they read apart from the files the list holds.
///
/// In the Buffer Finder's two scopes the row of an unsaved buffer ends in a
/// dot. In every other scope the row of a modified file ends in an `M`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_finder_rows(
    finder: &FileFinder,
    home: Option<&Path>,
    area: Rect,
    start: RowStart,
    theme: &crate::theme::Theme,
    panels: bool,
    pixels: Option<RowMetrics>,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let row_style = if finder.fallback_active() {
        theme.get(crate::theme::scope::UI_TEXT_MUTED)
    } else {
        theme.get(crate::theme::scope::UI_TEXT)
    };
    let marks = match lists_buffers(finder) {
        true => RowMarks::Dirty(&finder.scopes.dirty_buffer_paths),
        false => RowMarks::Modified(&finder.scopes.modified_paths),
    };
    crate::render::picker::paint_path_rows(
        &finder_rows(finder, home),
        marks,
        area,
        start,
        theme,
        row_style,
        panels,
        pixels,
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
/// The directory reads in `ui.text.muted` and the name in `ui.text` with no
/// cell between them, and the extension that [`chrome::tagged_name`] splits off
/// takes its tag's color, as a finder row draws it. The lines read in
/// `ui.text.muted`. A row too narrow for the whole path keeps the name.
///
/// The rule draws in `ui.divider`, the color of a rule on the editor surface,
/// and in `ui.border.inactive` when the theme gives the divider no color. In
/// cells it runs through the header's second row.
///
/// Under the plate look the header is the boards' 24 px band, which starts at
/// the Island's top 4 px over its first row, and it writes no cell. Its text
/// draws as runs 1 px over that row, the path at the [`TypeRole::Ui`] size and
/// the lines at the [`TypeRole::Caption`] size, and the rule draws 2 px into
/// the second row. A part whose color has no RGB value draws nothing.
///
/// Under a live scene the header sits on the editor surface, which the
/// preview below it shares, and a plain terminal keeps the modal's fill under
/// both. A blank pane leaves the header row empty, and a source with no path
/// shows only its lines.
///
/// [`chrome::tagged_name`]: crate::render::chrome::tagged_name
fn paint_preview_header(
    finder: &FileFinder,
    header: Rect,
    home: Option<&Path>,
    theme: &crate::theme::Theme,
    ws: &mut Workspace,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let core = finder.active_core_ref();
    let lines = core
        .preview
        .source()
        .and_then(|_| ws.editors.get_mut(core.preview.editor))
        .and_then(shown_lines)
        .map(|(top, bottom)| format!("lines {top}\u{2013}{bottom}"));

    let path = core.preview.source().and_then(|source| match source {
        PreviewSource::File(path) | PreviewSource::Directory(path) => Some(path.as_path()),
        PreviewSource::Buffer(id) => ws.buffers.path_for(*id),
        PreviewSource::Terminal(_) | PreviewSource::Markdown(_) | PreviewSource::Text(_) => None,
    });
    let label = path.map(|path| {
        let mut label = row_prefix(finder).to_owned();
        write_row_display(
            &mut label,
            path,
            &core.git_root,
            core.picklist.display_roots.as_deref(),
            home,
        );
        label
    });
    let parts = label
        .as_deref()
        .zip(path)
        .map(|(label, path)| HeaderPath::of(label, path, theme));

    let rule = theme
        .try_get(crate::theme::scope::UI_DIVIDER)
        .filter(|style| style.fg.is_some())
        .unwrap_or_else(|| theme.get(crate::theme::scope::UI_BORDER_INACTIVE));
    match crate::render::chrome::plate_colors(theme, scene) {
        Some(_) => {
            paint_plate_preview_header(header, parts, lines.as_deref(), rule, theme, buf, scene)
        },
        None => {
            paint_cell_preview_header(header, parts, lines.as_deref(), theme, buf);
            crate::render::chrome::hline(buf, header.x, header.y + 1, header.width, rule, scene);
        },
    }
}

/// The path that the preview's header names, split into the parts that take
/// their own colors.
struct HeaderPath<'a> {
    /// The directory, in `ui.text.muted`.
    dir: (&'a str, Style),
    /// The name before the extension that its type tag colors, in `ui.text`.
    stem: (&'a str, Style),
    /// The extension in its tag's color, empty for a file with no tag.
    suffix: (&'a str, Style),
}

impl<'a> HeaderPath<'a> {
    /// Split `label`, the header's text for the source at `path`.
    fn of(label: &'a str, path: &Path, theme: &crate::theme::Theme) -> Self {
        let split = label
            .char_indices()
            .nth(crate::render::picker::name_start(label))
            .map_or(label.len(), |(byte, _)| byte);
        let (dir, name) = label.split_at(split);
        let text = theme.get(crate::theme::scope::UI_TEXT);
        let tagged = crate::render::chrome::tagged_name(path)
            .filter(|tagged| name.ends_with(tagged.suffix.as_str()));
        let (stem, suffix) = match &tagged {
            Some(tagged) => name.split_at(name.len() - tagged.suffix.len()),
            None => (name, ""),
        };
        let tag = tagged.map_or(text, |tagged| theme.get(tagged.tag.scope));
        HeaderPath {
            dir: (dir, theme.get(crate::theme::scope::UI_TEXT_MUTED)),
            stem: (stem, text),
            suffix: (suffix, tag),
        }
    }

    /// The parts to draw, with the directory where `with_dir` keeps it.
    fn parts(&self, with_dir: bool) -> [(&'a str, Style); 3] {
        let dir = match with_dir {
            true => self.dir,
            false => ("", self.dir.1),
        };
        [dir, self.stem, self.suffix]
    }

    /// The chars of the whole path.
    fn chars(&self) -> usize {
        self.parts(true)
            .iter()
            .map(|(text, _)| text.chars().count())
            .sum()
    }
}

/// Write the path header's text into the cells of `header`'s first row, the
/// lines one cell from its right edge and the path from its second cell.
fn paint_cell_preview_header(
    header: Rect,
    path: Option<HeaderPath<'_>>,
    lines: Option<&str>,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let (y, end) = (header.y, header.x + header.width);
    let mut path_end = end.saturating_sub(1);
    if let Some(text) = lines {
        let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
        let x = path_end.saturating_sub(text.width() as u16);
        write_str_clipped(buf, x, y, text, muted, end);
        path_end = x.saturating_sub(2);
    }

    let Some(path) = path else {
        return;
    };
    let mut x = header.x + 1;
    let fits = usize::from(x)
        + path
            .parts(true)
            .iter()
            .map(|(text, _)| text.width())
            .sum::<usize>()
        <= usize::from(path_end);
    for (text, style) in path.parts(fits) {
        write_str_clipped(buf, x, y, text, style, path_end);
        x = x.saturating_add(text.width() as u16);
    }
}

/// Draw the path header over `header` as the plate's runs and rule, as
/// [`paint_preview_header`] describes them.
///
/// The text starts a cell in and stands [`PREVIEW_HEADER_INSET`] sixteenths
/// from the lines, which end that far inside the header's right edge.
#[allow(clippy::too_many_arguments)]
fn paint_plate_preview_header(
    header: Rect,
    path: Option<HeaderPath<'_>>,
    lines: Option<&str>,
    rule: Style,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let (ui, caption) = (
        theme.type_scale(TypeRole::Ui),
        theme.type_scale(TypeRole::Caption),
    );
    let width = header.width.saturating_mul(16);
    let label_col = lines.map(|text| {
        let advance = text_run::advance_sixteenths(text.chars().count(), caption);
        width.saturating_sub(PREVIEW_HEADER_INSET + advance)
    });
    let path_end = label_col
        .unwrap_or(width)
        .saturating_sub(PREVIEW_HEADER_INSET);
    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let origin = Rect::new(header.x, header.y, 1, 1);

    scene.nudge([1, -1]);
    {
        let mut run = |col: u16, text: &str, scale: u16, style: Style| {
            if let (Some(color), false) = (style_rgb(style.fg), text.is_empty()) {
                TextRun {
                    col: col as i16,
                    row: 0,
                    scale,
                    color,
                    bg: None,
                    bold: false,
                    italic: false,
                    follow: 0,
                    anchor: None,
                    text,
                }
                .render(origin, buf, scene);
            }
        };
        if let Some(path) = path {
            let mut col: u16 = 16;
            let fits = col + text_run::advance_sixteenths(path.chars(), ui) <= path_end;
            for (text, style) in path.parts(fits) {
                let room = usize::from(path_end.saturating_sub(col)) * 16 / usize::from(ui.max(1));
                let shown = clip_to_width(text, room);
                run(col, shown, ui, style);
                col = col.saturating_add(text_run::advance_sixteenths(shown.chars().count(), ui));
            }
        }
        if let (Some(text), Some(col)) = (lines, label_col) {
            run(col, text, caption, muted);
        }
    }

    scene.nudge([0, 2]);
    if let Some(color) = style_rgb(rule.fg) {
        Bar {
            x: 0,
            y: 0,
            width: width.saturating_add(5),
            height: 1,
            color,
            radius: 0,
            corners: 0,
        }
        .render(
            Rect::new(header.x, header.y + 1, header.width, 1),
            buf,
            scene,
        );
    }
    scene.nudge([0, 0]);
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
    use super::{
        file_finder_layout, finder_layout, paint_preview_header, render_file_finder, FinderLayout,
    };
    use crate::{
        file_finder::FileFinder,
        render::{
            chrome::{ISLAND_BOTTOM, ISLAND_GAP, ISLAND_SIDE, ISLAND_TOP},
            editor::ResolvedChrome,
            picker::{preview_min_columns, DEFAULT_LIST_SHARE},
        },
        test_fixture::{column_rules, plate_islands},
        theme::{scope, Theme},
        workspace::Workspace,
    };
    use ratatui::{
        buffer::{Buffer, Cell},
        layout::Rect,
    };
    use std::{ops::Range, path::PathBuf};
    use stoat_widgets::ApcScene;
    use stoatty_protocol::command::{decode_stream, BarCommand, Command, TextRunCommand};

    /// Every caller of this layout must fit inside the same area, so the box a
    /// short list gets and the box a data-heavy modal gets are checked together.
    fn box_of(area: Rect, content: (u16, u16)) -> Rect {
        file_finder_layout(area, content, 0, DEFAULT_LIST_SHARE)
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

    /// A query that matches no file keeps the full box. The list's first row
    /// reads `no file matches ` in the muted color and then the query in the
    /// text color. The preview's cells, its path header's among them, read
    /// `no preview` in the disabled color, centered.
    #[test]
    fn a_finder_with_no_match_keeps_its_full_box() {
        let mut h = crate::Stoat::test();
        let root = PathBuf::from("/no-match");
        h.fake_fs()
            .insert_files([(root.join("a.rs"), "fn a() {}".as_bytes())]);
        h.stoat.active_workspace_mut().git_root = root;
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFileFinder);
        h.settle();
        h.type_text("zzqxj");
        h.settle();
        let mut finder = h.stoat.file_finder.take().expect("the finder is open");
        let theme = h.stoat.theme.clone();
        let chrome = ResolvedChrome::resolve(&theme);
        let area = Rect::new(0, 0, 200, 40);
        let mut buf = Buffer::empty(area);
        render_file_finder(
            &mut finder,
            h.stoat.active_workspace_mut(),
            None,
            &theme,
            &chrome,
            area,
            0,
            None,
            preview_min_columns(None),
            &mut buf,
            &mut ApcScene::new(),
        );

        let layout = finder_layout(area, &finder, None, 0, None, preview_min_columns(None))
            .expect("the finder fits");
        let preview = layout.preview.expect("a preview beside the list");
        let pane = layout
            .preview_header
            .map_or(preview, |header| header.union(preview));
        let (list, middle) = (layout.list, pane.y + pane.height / 2);
        let note_x = pane.x + (pane.width - 10) / 2;
        let read = |x: u16, y: u16, len: u16| {
            (x..x + len)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
        };
        let fg = |name: &str| theme.get(name).fg;
        assert_eq!(
            (
                finder.shows_no_match(),
                read(list.x + 1, list.y, 21),
                [buf[(list.x + 1, list.y)].fg, buf[(list.x + 17, list.y)].fg],
                read(note_x, middle, 10),
                buf[(note_x, middle)].fg,
            ),
            (
                true,
                "no file matches zzqxj".to_owned(),
                [fg(scope::UI_TEXT_MUTED), fg(scope::UI_TEXT)].map(Option::unwrap),
                "no preview".to_owned(),
                fg(scope::UI_TEXT_DISABLED).unwrap(),
            ),
        );
    }

    /// The header ends in one pill per scope, `all`, `modified`, and
    /// `workspaces` for a finder over files. Under a dead scene the active
    /// scope's pill takes the `ui.chip` background, and each other pill reads in
    /// `ui.text.muted` with no chip background. Shift-Tab moves the chip
    /// background to `modified`.
    #[test]
    fn the_header_names_each_scope_and_boxes_the_active_one() {
        let mut h = crate::Stoat::test();
        let root = PathBuf::from("/pills");
        h.fake_fs()
            .insert_files([(root.join("a.rs"), "fn a() {}".as_bytes())]);
        h.stoat.active_workspace_mut().git_root = root;
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFileFinder);
        h.settle();
        let mut finder = h.stoat.file_finder.take().expect("the finder is open");
        let theme = h.stoat.theme.clone();
        let chip = theme.get(scope::UI_CHIP).bg.expect("a chip background");
        let muted = theme.get(scope::UI_TEXT_MUTED).fg.expect("a muted color");

        let before = header_end(&mut finder, h.stoat.active_workspace_mut(), &theme);
        finder.toggle_scope();
        let after = header_end(&mut finder, h.stoat.active_workspace_mut(), &theme);

        let text: String = before.iter().map(Cell::symbol).collect();
        let chipped = |cells: &[Cell]| cells.iter().all(|cell| cell.bg == chip);
        let plain = before[6..16]
            .iter()
            .all(|cell| cell.fg == muted && cell.bg != chip);
        assert_eq!(
            (text, chipped(&before[..5]), plain, chipped(&after[6..16])),
            (" all   modified   workspaces ".to_owned(), true, true, true),
        );
    }

    /// The Buffer Finder names itself `Buffers` and pills its two scopes. The
    /// rows of the two unsaved buffers end in a dot, a glyph under a dead scene,
    /// and the row of the saved one ends blank. The key line names the Buffer
    /// Finder's own keys.
    #[test]
    fn the_buffer_finder_reads_buffers_and_names_its_keys() {
        let mut h = crate::Stoat::test();
        let root = PathBuf::from("/buffers");
        h.fake_fs().insert_files([
            (root.join("a.rs"), "fn a() {}".as_bytes()),
            (root.join("b.rs"), "fn b() {}".as_bytes()),
            (root.join("c.rs"), "fn c() {}".as_bytes()),
        ]);
        h.stoat.active_workspace_mut().git_root = root.clone();
        for name in ["a.rs", "b.rs", "c.rs"] {
            let path = root.join(name);
            crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFile { path });
        }
        h.settle();
        for name in ["a.rs", "c.rs"] {
            let buffers = &h.stoat.active_workspace().buffers;
            let id = buffers
                .id_for_path(&root.join(name))
                .expect("an open buffer");
            let buffer = buffers.get(id).expect("a buffer");
            buffer.write().expect("poisoned").edit(0..0, "// ");
        }
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenBufferPicker);
        h.settle();
        let mut finder = h.stoat.file_finder.take().expect("the finder is open");
        let theme = h.stoat.theme.clone();

        let (buf, layout) = painted_finder(&mut finder, h.stoat.active_workspace_mut(), &theme);

        let text = |y: u16, xs: Range<u16>| xs.map(|x| buf[(x, y)].symbol()).collect::<String>();
        let (inner, list) = (layout.inner, layout.list);
        let header = text(inner.y, inner.left()..inner.right());
        let rows: Vec<(String, String)> = (list.y..list.y + 3)
            .map(|y| {
                let label = text(y, list.left()..list.right() - 1).trim().to_owned();
                (label, buf[(list.right() - 1, y)].symbol().to_owned())
            })
            .collect();
        let footer = layout.footer.expect("a key line");
        let keys = text(footer.y, footer.left()..footer.right());
        assert_eq!(
            (
                header.starts_with("Buffers "),
                header.ends_with(" buffers   modified "),
                rows,
                keys.trim_end(),
            ),
            (
                true,
                true,
                vec![
                    ("a.rs".to_owned(), "\u{25cf}".to_owned()),
                    ("b.rs".to_owned(), " ".to_owned()),
                    ("c.rs".to_owned(), "\u{25cf}".to_owned()),
                ],
                " Enter switch  S-Tab scope  Esc close",
            ),
            "the header reads {header:?}",
        );
    }

    /// The File Finder's key line names its open, scope, and close keys.
    #[test]
    fn the_file_finder_key_line_reads_open_scope_and_close() {
        let mut h = crate::Stoat::test();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFileFinder);
        h.settle();
        let mut finder = h.stoat.file_finder.take().expect("the finder is open");
        let theme = h.stoat.theme.clone();

        let (buf, layout) = painted_finder(&mut finder, h.stoat.active_workspace_mut(), &theme);
        let footer = layout.footer.expect("a key line");
        let keys: String = (footer.left()..footer.right())
            .map(|x| buf[(x, footer.y)].symbol())
            .collect();
        assert_eq!(keys.trim_end(), " Enter open  S-Tab scope  Esc close");
    }

    /// The 29 cells that end the header row of `finder`, painted into a 200 by
    /// 40 area under a dead scene.
    fn header_end(finder: &mut FileFinder, ws: &mut Workspace, theme: &Theme) -> Vec<Cell> {
        let (buf, layout) = painted_finder(finder, ws, theme);
        let end = layout.inner.right();
        (end - 29..end)
            .map(|x| buf[(x, layout.inner.y)].clone())
            .collect()
    }

    /// Paint `finder` into a 200 by 40 area under a dead scene, and return the
    /// cells and the finder's layout in that area.
    fn painted_finder(
        finder: &mut FileFinder,
        ws: &mut Workspace,
        theme: &Theme,
    ) -> (Buffer, FinderLayout) {
        let area = Rect::new(0, 0, 200, 40);
        let chrome = ResolvedChrome::resolve(theme);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();
        scene.set_live(false);
        render_file_finder(
            finder,
            ws,
            None,
            theme,
            &chrome,
            area,
            0,
            None,
            preview_min_columns(None),
            &mut buf,
            &mut scene,
        );
        let layout = finder_layout(area, finder, None, 0, None, preview_min_columns(None))
            .expect("the finder fits");
        (buf, layout)
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
            file_finder_layout(Rect::new(0, 0, 41, 60), (120, 32), 0, DEFAULT_LIST_SHARE).is_none(),
            "a width under the 40-column minimum plus its thinnest margin fails"
        );
        assert!(
            file_finder_layout(Rect::new(0, 0, 200, 13), (120, 32), 0, DEFAULT_LIST_SHARE)
                .is_none(),
            "so does a height under the 12-row minimum plus its thinnest margin"
        );
    }

    /// Under the plate look the list and the preview with its path header draw
    /// as Islands that face each other across the separator column, which
    /// draws no rule.
    #[test]
    fn under_the_plate_the_list_and_the_preview_draw_as_islands() {
        let mut h = crate::Stoat::test();
        let root = PathBuf::from("/islands");
        h.fake_fs()
            .insert_files([(root.join("a.rs"), "fn a() {}".as_bytes())]);
        h.stoat.active_workspace_mut().git_root = root;
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFileFinder);
        h.settle();
        let mut finder = h.stoat.file_finder.take().expect("the finder is open");
        let theme = h.stoat.theme.clone();
        let chrome = ResolvedChrome::resolve(&theme);
        let area = Rect::new(0, 0, 200, 40);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();
        scene.set_nudge_protocol(true);
        render_file_finder(
            &mut finder,
            h.stoat.active_workspace_mut(),
            None,
            &theme,
            &chrome,
            area,
            0,
            None,
            preview_min_columns(None),
            &mut buf,
            &mut scene,
        );

        let layout = finder_layout(area, &finder, None, 0, None, preview_min_columns(None))
            .expect("the finder fits");
        let preview = layout.preview.expect("a preview beside the list");
        let pane = layout
            .preview_header
            .map_or(preview, |header| header.union(preview));
        assert_eq!(
            (
                plate_islands(&theme, &scene),
                column_rules(&scene, layout.list.right())
            ),
            (
                vec![
                    (
                        layout.list,
                        [ISLAND_TOP, ISLAND_GAP, ISLAND_BOTTOM, ISLAND_SIDE]
                    ),
                    (pane, [ISLAND_TOP, ISLAND_SIDE, ISLAND_BOTTOM, ISLAND_GAP]),
                ],
                0,
            ),
        );
    }

    /// The commands that the preview header of a finder over `src/a.rs` of 3
    /// lines draws into `header` under the plate look, without the ends of the
    /// text runs, and whether every cell stays blank.
    fn plate_header_commands(header: Rect) -> (Vec<Command>, bool) {
        let mut h = crate::test_harness::TestHarness::with_size(200, 40);
        let root = PathBuf::from("/header");
        h.fake_fs()
            .insert_files([(root.join("src/a.rs"), "one\ntwo\nthree".as_bytes())]);
        h.stoat.active_workspace_mut().git_root = root;
        h.type_keys("space p");
        h.settle();
        h.stoat.render();
        let finder = h.stoat.file_finder.take().expect("the finder is open");
        let src = r##"theme t {
            ui.surface.ground.bg = "#0c0e11";
            ui.background.bg = "#17191e";
            ui.text.fg = "#010101";
            ui.text.muted.fg = "#020202";
            ui.tag.rust.fg = "#030303";
            ui.divider.fg = "#040404";
        }"##;
        let (config, _) = stoat_config::parse(src);
        let theme = Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads");
        let area = Rect::new(0, 0, 80, 10);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();
        scene.set_nudge_protocol(true);
        paint_preview_header(
            &finder,
            header,
            None,
            &theme,
            h.stoat.active_workspace_mut(),
            &mut buf,
            &mut scene,
        );
        let commands = decode_stream(scene.bytes())
            .into_iter()
            .filter(|command| *command != Command::TextRunEnd)
            .collect();
        (commands, buf == Buffer::empty(area))
    }

    fn header_run(col: i16, text: &str, color: [u8; 3]) -> Command {
        Command::TextRun(TextRunCommand {
            col,
            row: 80,
            scale: 256,
            color,
            bg: None,
            bold: false,
            italic: false,
            follow: 0,
            anchor: None,
            text: text.to_owned(),
        })
    }

    fn header_rule(width: u16) -> Command {
        Command::Bar(BarCommand {
            x: 640,
            y: 96,
            width,
            height: 1,
            color: [4, 4, 4],
            occludes: false,
            hatch: None,
            radius: 0,
            corners: 0,
        })
    }

    /// Under the plate look the preview header draws its path from col 16 of
    /// its first row and its shown lines ending 14 sixteenths inside it, both
    /// 1 px right and 1 px over the row, and its rule 2 px into its second row
    /// and 5 sixteenths past its right edge. Its rows hold no glyph.
    #[test]
    fn a_plate_preview_header_draws_its_path_and_lines_as_nudged_runs() {
        assert_eq!(
            plate_header_commands(Rect::new(40, 5, 30, 2)),
            (
                vec![
                    Command::Nudge([1, -1]),
                    header_run(656, "src/", [2, 2, 2]),
                    header_run(720, "a", [1, 1, 1]),
                    header_run(736, ".rs", [3, 3, 3]),
                    header_run(962, "lines 1\u{2013}3", [2, 2, 2]),
                    Command::Nudge([0, 2]),
                    header_rule(485),
                    Command::Nudge([0, 0]),
                ],
                true,
            )
        );
    }

    /// A plate header 13 cells wide puts the lines at col 50 and stops the path
    /// at col 36, where `src/a.rs` does not fit, so it keeps the name and clips
    /// it there, after `a`.
    #[test]
    fn a_narrow_plate_preview_header_keeps_the_name() {
        assert_eq!(
            plate_header_commands(Rect::new(40, 5, 13, 2)).0,
            [
                Command::Nudge([1, -1]),
                header_run(656, "a", [1, 1, 1]),
                header_run(690, "lines 1\u{2013}3", [2, 2, 2]),
                Command::Nudge([0, 2]),
                header_rule(213),
                Command::Nudge([0, 0]),
            ]
        );
    }
}

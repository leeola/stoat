use crate::{
    command_palette::{ArgPicker, CommandPalette},
    input_view::InputView,
    render::{
        chrome::{ISLAND_BOTTOM, ISLAND_GAP, ISLAND_SIDE, ISLAND_TOP},
        cmd_line::{self, CmdLine, CmdStatus},
        picker::{Footer, PathRows, RowMarks, RowStart, FOOTER_ROWS},
        text::{wrap_text, write_str, write_str_clipped},
    },
    workspace::Workspace,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{Block, Borders},
};
use std::path::Path;
use stoat_action::registry::RegistryEntry;

const LIST_ROWS: u16 = 10;
const DOC_ROWS: u16 = 6;

/// Box rows the chrome takes around the list. Two borders, the Cmd Line's first
/// row, the separator under it, the doc separator, the doc region itself, and
/// the key line's row inside the bottom border.
///
/// A Cmd Line that wraps onto more rows takes them from the body, so the box
/// keeps the height it opened with.
const CHROME_ROWS: u16 = 6 + DOC_ROWS;

/// The palette's key line, each a bound key and the word for what it does.
const PALETTE_HINTS: [(&str, &str); 4] = [
    ("Enter", "run"),
    ("Tab", "complete"),
    ("S-Tab", "scope"),
    ("C-Up/C-Down", "history"),
];

/// The on-screen rectangles of the command-palette filter modal, derived from a
/// terminal `area` by [`palette_filter_layout`].
///
/// The box height follows the candidate set measured when the palette opened,
/// never the filtered rows, so the modal stays put as the selection and filter
/// change. Shared by the renderer and the smooth-scroll emit so the pooled list
/// region matches the painted one exactly.
pub(crate) struct PaletteFilterLayout {
    /// The bordered modal box.
    pub(crate) modal: Rect,
    /// Inside the border: prompt/input, separator, list, doc separator, doc.
    pub(crate) inner: Rect,
    /// The scrolling result list, also the smooth-scroll pool region.
    pub(crate) list: Rect,
    /// The selected entry's documentation, below a separator under the list.
    pub(crate) doc: Rect,
    /// The key line under the doc, as
    /// [`footer_rect`](crate::render::picker::footer_rect) gives it.
    pub(crate) footer: Rect,
}

/// The on-screen rectangles of the palette's argument picker, derived from a
/// terminal `area` by [`palette_arg_layout`].
///
/// Shared by the renderer, the hit test, and the smooth-scroll emit, so the
/// pooled list region matches the painted one exactly.
pub(crate) struct PaletteArgLayout {
    /// The bordered modal box.
    pub(crate) modal: Rect,
    /// Inside the border, the Cmd Line, the row under it, and the body.
    pub(crate) inner: Rect,
    /// The result list, also the smooth-scroll pool region.
    pub(crate) list: Rect,
    /// The preview beside the list, present only when the body is wide enough.
    pub(crate) preview: Option<Rect>,
    /// The key line under the body, as
    /// [`footer_rect`](crate::render::picker::footer_rect) gives it.
    pub(crate) footer: Rect,
    /// The plate's width before zoom steps, in columns, which the palette
    /// keeps as [`CommandPalette::arg_plate_width`] so the plate does not
    /// narrow while the palette is open.
    pub(crate) plate_base: u16,
}

/// Lay out the command-palette filter modal within `area`, or `None` when
/// `area` is too small to host it.
///
/// `content_rows` is the rows the caller's whole candidate list would need,
/// measured from the unfiltered set when the palette opened. The box holds at
/// `LIST_ROWS` for a short list and grows for a long one, up to half the area
/// so the palette never swallows the screen. `zoom` steps it further, in tenths
/// of the area.
///
/// The width follows the area rather than the candidates, at
/// [`list_modal_width`](crate::render::chrome::list_modal_width), and `zoom`
/// steps it as it steps the height.
///
/// A box too short for everything shrinks the doc region first and the list
/// last, since the list is the primary content.
///
/// The body starts under the `header_rows` rows of the Cmd Line, as
/// [`CommandPalette::header_rows`] holds them, and the row under them. The box
/// does not follow `header_rows`, so a wrapped header takes its rows from the
/// body, and the doc region gives up its rows first.
pub(crate) fn palette_filter_layout(
    area: Rect,
    content_rows: u16,
    zoom: i8,
    header_rows: u16,
) -> Option<PaletteFilterLayout> {
    if area.width < 30 || area.height < 10 {
        return None;
    }

    let width = crate::render::chrome::list_modal_width(area);
    let content_height = content_rows
        .saturating_add(CHROME_ROWS)
        .min(area.height / 2);
    let modal = crate::render::chrome::modal_box(
        area,
        (width, content_height),
        (width, LIST_ROWS + CHROME_ROWS),
        (20, 6),
        zoom,
    )?;
    let inner = Block::default().borders(Borders::ALL).inner(modal);

    // The header, its separator, the doc separator, and the key line's row.
    let body = inner
        .height
        .saturating_sub(2 + header_rows + FOOTER_ROWS - 1);
    let list_height = body.saturating_sub(DOC_ROWS).max(LIST_ROWS).min(body);
    let doc_height = DOC_ROWS.min(body.saturating_sub(list_height));

    let list = Rect::new(inner.x, inner.y + 1 + header_rows, inner.width, list_height);
    let doc = Rect::new(inner.x, list.y + list_height + 1, inner.width, doc_height);

    Some(PaletteFilterLayout {
        modal,
        inner,
        list,
        doc,
        footer: crate::render::picker::footer_rect(modal),
    })
}

/// Lay out the palette's argument picker (`:o `, `:cd `, `:b `) within `area`,
/// its plate fitted to the widest label that its list shows, or `None` when
/// `area` is too small to host it or no argument picker is open.
///
/// The plate follows the rule of the finder's plate. The list takes the widest
/// label among the rows on show, at least 30 columns, with its pad and mark
/// cells, and the preview takes one and a half times the list and at least
/// `preview_min` columns. The plate does not narrow below
/// [`CommandPalette::arg_plate_width`], the widest plate the picker has drawn
/// since the palette opened, and it stops one column inside each side of
/// `area`. `zoom` steps then move the width by a tenth of the area each.
///
/// The height is the height of [`palette_filter_layout`]. The palette's
/// separator takes no drag, so the list keeps its fitted width.
///
/// The body starts under the [`CommandPalette::header_rows`] rows of the Cmd
/// Line and the row under them. The widest label comes from the body rows
/// under a one-row header, because the header's rows follow the plate's width.
pub(crate) fn palette_arg_layout(
    area: Rect,
    palette: &CommandPalette,
    home: Option<&Path>,
    zoom: i8,
    preview_min: u16,
) -> Option<PaletteArgLayout> {
    let picker = palette.arg_picker.as_ref()?;
    let rows =
        palette_filter_layout(area, palette.list_rows_hint(), zoom, palette.header_rows)?.modal;
    let list = {
        let inner = Block::default().borders(Borders::ALL).inner(rows);
        let (_, list_rows) = arg_body_rows(inner, 1)?;
        let widest = crate::render::picker::widest_label(&arg_rows(picker, home), list_rows);
        crate::render::picker::list_columns(widest)
    };

    let fitted = crate::render::picker::fitted_plate_width(list, preview_min);
    let base =
        crate::render::chrome::fitted_width(area.width, palette.arg_plate_width.max(fitted), 0, 0);
    let width = crate::render::chrome::fitted_width(area.width, base, 20, zoom);
    let modal = Rect {
        x: area.x + (area.width - width) / 2,
        width,
        ..rows
    };
    let inner = Block::default().borders(Borders::ALL).inner(modal);

    let (top, height) = arg_body_rows(inner, palette.header_rows)?;
    let (list, preview) = crate::render::picker::fitted_split(
        inner.x,
        top,
        inner.width,
        height,
        list,
        preview_min,
        None,
    );
    Some(PaletteArgLayout {
        modal,
        inner,
        list,
        preview,
        footer: crate::render::picker::footer_rect(modal),
        plate_base: base,
    })
}

/// The top row and the rows of the argument picker's body inside `inner`,
/// below the `header_rows` rows of the Cmd Line and the row under them and
/// above the key line, or `None` when the body has no rows.
fn arg_body_rows(inner: Rect, header_rows: u16) -> Option<(u16, u16)> {
    let top = inner.y + 1 + header_rows;
    let height = (inner.y + inner.height)
        .saturating_sub(FOOTER_ROWS - 1)
        .saturating_sub(top);
    (height > 0).then_some((top, height))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_command_palette(
    palette: &mut CommandPalette,
    ws: &mut Workspace,
    home: Option<&Path>,
    theme: &crate::theme::Theme,
    chrome: &crate::render::editor::ResolvedChrome,
    area: Rect,
    zoom: i8,
    preview_min: u16,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    if palette.arg_picker.is_some() && palette.arg_source().is_some() {
        render_palette_arg_picker(
            palette,
            ws,
            home,
            theme,
            chrome,
            area,
            zoom,
            preview_min,
            buf,
            scene,
        );
        return;
    }

    // A parsed command whose argument has no inline picker (ValueSource::None)
    // shows a hint naming the argument instead of the emptied command list. Key
    // on the source, not picker presence -- a stale picker from an earlier
    // source may linger a frame and must not win.
    if let Some(entry) = palette.command
        && palette.arg_source().is_none()
    {
        render_palette_free_arg(palette, entry, ws, home, theme, area, zoom, buf, scene);
        return;
    }

    let content_rows = palette.list_rows_hint();
    render_palette_filter(
        &palette.input,
        &palette.filtered,
        &palette.match_indices,
        palette.selected,
        palette.group,
        content_rows,
        zoom,
        &mut palette.header_rows,
        ws,
        home,
        theme,
        area,
        buf,
        scene,
    );

    if palette.command.is_none()
        && let Some(layout) = palette_filter_layout(area, content_rows, zoom, palette.header_rows)
    {
        palette.viewport_rows = Some(layout.list.height as usize);
    }
}

/// Render the inline file picker shown while collecting a `Files` argument
/// (e.g. `:o `).
///
/// Keeps the filter modal's height and its Cmd Line, then replaces the
/// command list + doc body with a result list beside a live preview, mirroring
/// the standalone file finder. The plate's width follows
/// [`palette_arg_layout`], and the paint keeps it in
/// [`CommandPalette::arg_plate_width`], so the next frame draws a plate at
/// least as wide. State is synced before the frame by
/// [`crate::action_handlers::sync_palette_picker`], so this only paints.
#[allow(clippy::too_many_arguments)]
fn render_palette_arg_picker(
    palette: &mut CommandPalette,
    ws: &mut Workspace,
    home: Option<&Path>,
    theme: &crate::theme::Theme,
    chrome: &crate::render::editor::ResolvedChrome,
    area: Rect,
    zoom: i8,
    preview_min: u16,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let fallback_active = palette
        .arg_picker
        .as_ref()
        .is_some_and(|picker| picker.fallback_active());
    let status = palette
        .arg_picker
        .as_ref()
        .map(|picker| {
            let list = &picker.active_core_ref().picklist;
            let ignored = match fallback_active {
                true => " ignored",
                false => "",
            };
            let rest = format!(" of {}{ignored}", grouped(list.base.len()));
            count_status(list.filtered.len(), &rest, theme)
        })
        .unwrap_or_default();
    let Some(layout) = palette_arg_layout(area, palette, home, zoom, preview_min) else {
        return;
    };
    palette.arg_plate_width = layout.plate_base;
    palette.header_rows = render_palette_prelude(
        &palette.input,
        CmdStatus::Text(&status),
        layout.modal,
        layout.inner,
        home,
        ws,
        theme,
        buf,
        &mut *scene,
    );
    let Some(layout) = palette_arg_layout(area, palette, home, zoom, preview_min) else {
        return;
    };
    let separator_style = theme.get(crate::theme::scope::UI_BORDER_INACTIVE);
    let (list, preview) = (layout.list, layout.preview);

    let Some(picker) = palette.arg_picker.as_mut() else {
        return;
    };
    let plate = crate::render::chrome::plate_colors(theme, scene);
    if let Some(plate) = plate {
        crate::render::chrome::list_islands(list, preview, plate, theme, buf, scene);
    }
    if let Some(preview_rect) = preview {
        if plate.is_none() {
            crate::render::chrome::vline(
                buf,
                list.x + list.width,
                list.y,
                list.height,
                separator_style,
                scene,
            );
        }
        crate::render::picker::render_picker_preview(
            &mut picker.active_core().preview,
            preview_rect,
            theme,
            chrome,
            ws,
            buf,
            scene,
        );
    }

    let window =
        crate::render::picker::list_window(&arg_rows(picker, home), list.width, list.height);
    picker.active_core().picklist.viewport_rows = Some(window.whole(list.height).max(1));
    let row_style = if fallback_active {
        theme.get(crate::theme::scope::UI_TEXT_MUTED)
    } else {
        theme.get(crate::theme::scope::UI_TEXT)
    };
    crate::render::picker::paint_path_rows(
        &arg_rows(picker, home),
        RowMarks::Modified(&[]),
        list,
        RowStart {
            result: window.first,
            skip: 0,
        },
        theme,
        row_style,
        true,
        buf,
        scene,
    );

    let parts = Footer {
        hints: &PALETTE_HINTS,
        ..Footer::default()
    };
    crate::render::picker::modal_footer(buf, layout.footer, &parts, plate, theme, scene);
}

/// The results of the argument picker as its list paints them, the active
/// picker's rows under the typed directory of a browse.
///
/// The live list, its pooled pages, and the hit test all read the rows through
/// this, so they measure every result alike.
pub(crate) fn arg_rows<'a>(picker: &'a ArgPicker, home: Option<&'a Path>) -> PathRows<'a> {
    let core = picker.active_core_ref();
    let prefix = picker
        .browse
        .as_ref()
        .map(|browse| browse.typed_dir.as_str())
        .unwrap_or_default();
    PathRows {
        picklist: &core.picklist,
        git_root: &core.git_root,
        home,
        prefix,
    }
}

/// Render the body for a free-typed argument, one with no inline picker
/// (`ValueSource::None`, e.g. `:RenameWorkspace `).
///
/// Shows the command's usage line, the parameter being collected with its
/// description, then the command's long description, so the modal describes
/// what to type instead of showing the emptied command list. State is synced
/// before the frame, so this only paints.
#[allow(clippy::too_many_arguments)]
fn render_palette_free_arg(
    palette: &mut CommandPalette,
    entry: &'static RegistryEntry,
    ws: &mut Workspace,
    home: Option<&Path>,
    theme: &crate::theme::Theme,
    area: Rect,
    zoom: i8,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let rows_hint = palette.list_rows_hint();
    let Some(layout) = palette_filter_layout(area, rows_hint, zoom, palette.header_rows) else {
        return;
    };
    palette.header_rows = render_palette_prelude(
        &palette.input,
        CmdStatus::None,
        layout.modal,
        layout.inner,
        home,
        ws,
        theme,
        buf,
        scene,
    );
    let inner = layout.inner;

    let body_top = inner.y + 1 + palette.header_rows;
    let body_bottom = layout.footer.y;
    if body_top >= body_bottom {
        return;
    }
    let plate = crate::render::chrome::plate_colors(theme, scene);
    if let Some(plate) = plate {
        let body = Rect::new(inner.x, body_top, inner.width, body_bottom - body_top);
        let edges = [ISLAND_TOP, ISLAND_SIDE, ISLAND_BOTTOM, ISLAND_SIDE];
        crate::render::chrome::modal_island(body, edges, plate, theme, buf, scene);
    }

    let text_style = theme.get(crate::theme::scope::UI_TEXT);
    let end_x = inner.x + inner.width;
    write_str_clipped(
        buf,
        inner.x + 1,
        body_top,
        &usage_line(entry),
        text_style,
        end_x,
    );
    if let Some(param) = palette.arg_param()
        && body_top + 1 < body_bottom
    {
        let hint = format!("{}  {}", param.name, param.description);
        write_str_clipped(buf, inner.x + 1, body_top + 1, &hint, text_style, end_x);
    }

    let doc_top = body_top + 3;
    if doc_top < body_bottom {
        let doc_style = theme.get(crate::theme::scope::UI_TEXT_DIM);
        let doc_lines = wrap_text(
            entry.def.long_desc(),
            inner.width.saturating_sub(1) as usize,
        );
        for (i, line) in doc_lines
            .iter()
            .take((body_bottom - doc_top) as usize)
            .enumerate()
        {
            write_str(buf, inner.x + 1, doc_top + i as u16, line, doc_style);
        }
    }

    let parts = Footer {
        hints: &PALETTE_HINTS,
        ..Footer::default()
    };
    crate::render::picker::modal_footer(buf, layout.footer, &parts, plate, theme, scene);
}

/// Render the chrome every palette modal shares, which is the frame over
/// `modal` and the Cmd Line at `inner`'s top, and answer the Cmd Line's rows.
///
/// The caller lays the box out and paints its body. The command list and the
/// free-typed argument hint take the box of [`palette_filter_layout`], and an
/// inline picker takes the box of [`palette_arg_layout`]. The caller keeps the
/// answer in [`CommandPalette::header_rows`] and lays its body out under those
/// rows, because a long input wraps onto more rows than the last frame drew.
///
/// The Cmd Line names the workspace's root, abbreviated against `home`, and
/// shows the live input with its caret and `status` at its right end.
///
/// See also:
/// - [`cmd_line::cmd_line`] for the row itself.
/// - [`render_palette_filter`] for the command-list body.
/// - [`render_palette_arg_picker`] for the inline-picker body.
/// - [`render_palette_free_arg`] for the free-typed argument body.
#[allow(clippy::too_many_arguments)]
fn render_palette_prelude(
    input: &InputView,
    status: CmdStatus<'_>,
    modal: Rect,
    inner: Rect,
    home: Option<&Path>,
    ws: &mut Workspace,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) -> u16 {
    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PALETTE);
    crate::render::chrome::list_plate(buf, modal, modal_style, theme, &mut *scene);

    let cwd = crate::run::abbreviate_path(&ws.git_root, home);
    let (text, caret) = input.caret(ws);
    let input = [(text, theme.get(crate::theme::scope::UI_TEXT))];
    let line = CmdLine {
        cwd: &cwd,
        input: &input,
        caret: Some(caret),
        status,
    };
    cmd_line::cmd_line(buf, inner, &line, theme, scene);

    let plate = crate::render::chrome::plate_colors(theme, scene).is_some();
    cmd_line::cmd_line_rows(&line, inner.width, plate, theme)
}

/// The Cmd Line status that counts `shown` entries, the count and then `rest`,
/// in `ui.text.muted`.
///
/// The count groups its thousands as [`grouped`] does. A shown count of 0
/// paints its `0` in `diff.deleted`'s foreground, which flags a query that
/// matches nothing.
fn count_status(shown: usize, rest: &str, theme: &crate::theme::Theme) -> Vec<(String, Style)> {
    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    if shown > 0 {
        return vec![(format!("{}{rest}", grouped(shown)), muted)];
    }

    let zero = match theme.get(crate::theme::scope::DIFF_DELETED).fg {
        Some(fg) => muted.fg(fg),
        None => muted,
    };
    vec![("0".to_owned(), zero), (rest.to_owned(), muted)]
}

/// `count` with its thousands grouped by `,`, as the boards write `1,284`.
fn grouped(count: usize) -> String {
    let digits = count.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Render the command list and the selected command's doc under the Cmd Line.
///
/// `header_rows` holds the rows that the Cmd Line took in the last frame, and
/// the paint stores there the rows that it draws.
#[allow(clippy::too_many_arguments)]
fn render_palette_filter(
    input: &InputView,
    filtered: &[&'static RegistryEntry],
    match_indices: &[Vec<u32>],
    selected: usize,
    group: Option<&str>,
    content_rows: u16,
    zoom: i8,
    header_rows: &mut u16,
    ws: &mut Workspace,
    home: Option<&Path>,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let name_offset = group.map_or(0, |group| group.len() + 1);
    let Some(layout) = palette_filter_layout(area, content_rows, zoom, *header_rows) else {
        return;
    };
    let status = count_status(filtered.len(), " commands", theme);
    *header_rows = render_palette_prelude(
        input,
        CmdStatus::Text(&status),
        layout.modal,
        layout.inner,
        home,
        ws,
        theme,
        buf,
        &mut *scene,
    );
    let Some(layout) = palette_filter_layout(area, content_rows, zoom, *header_rows) else {
        return;
    };

    let inner = layout.inner;
    let separator_style = theme.get(crate::theme::scope::UI_BORDER_INACTIVE);

    let list = layout.list;
    let doc = layout.doc;
    let plate = crate::render::chrome::plate_colors(theme, scene);
    if let Some(plate) = plate {
        let list_bottom = match doc.is_empty() {
            true => ISLAND_BOTTOM,
            false => ISLAND_GAP,
        };
        let list_edges = [ISLAND_TOP, ISLAND_SIDE, list_bottom, ISLAND_SIDE];
        crate::render::chrome::modal_island(list, list_edges, plate, theme, buf, scene);
        let doc_edges = [ISLAND_GAP, ISLAND_SIDE, ISLAND_BOTTOM, ISLAND_SIDE];
        crate::render::chrome::modal_island(doc, doc_edges, plate, theme, buf, scene);
    }

    let scroll = selected.saturating_sub(list.height.saturating_sub(1) as usize);
    paint_palette_rows(
        filtered,
        match_indices,
        selected,
        name_offset,
        list,
        scroll,
        theme,
        true,
        buf,
        &mut *scene,
    );

    if doc.height > 0 {
        if plate.is_none() {
            let doc_separator_row = doc.y - 1;
            crate::render::chrome::hline(
                buf,
                inner.x,
                doc_separator_row,
                inner.width,
                separator_style,
                scene,
            );
        }
        let lines = filtered
            .get(selected)
            .map(|e| doc_lines(e, inner.width.saturating_sub(1) as usize))
            .unwrap_or_default();
        let doc_style = theme.get(crate::theme::scope::UI_TEXT_DIM);
        paint_doc_lines(buf, doc, &lines, doc_style);
    }

    let parts = Footer {
        hints: &PALETTE_HINTS,
        ..Footer::default()
    };
    crate::render::picker::modal_footer(buf, layout.footer, &parts, plate, theme, scene);
}

/// Paint `lines` into the doc region `doc`, one per row from its top, each
/// clipped at the region's right edge.
fn paint_doc_lines(buf: &mut Buffer, doc: Rect, lines: &[String], style: Style) {
    for (i, line) in lines.iter().take(doc.height as usize).enumerate() {
        write_str_clipped(
            buf,
            doc.x + 1,
            doc.y + i as u16,
            line,
            style,
            doc.x + doc.width,
        );
    }
}

/// The help of `entry` as doc-region lines, the way a CLI's help reads.
///
/// The lines are its [`usage_line`], an `alias:` line when it has aliases, one
/// line per parameter with its description, a blank line after any parameter,
/// then its long description wrapped to `width`. Only the long description
/// wraps, so the caller clips the rest at the region's edge.
fn doc_lines(entry: &RegistryEntry, width: usize) -> Vec<String> {
    let mut lines = vec![usage_line(entry)];
    let aliases = entry.def.aliases();
    if !aliases.is_empty() {
        lines.push(format!("alias: {}", aliases.join(", ")));
    }
    let params = entry.def.params();
    lines.extend(
        params
            .iter()
            .map(|param| format!("  {}  {}", param.name, param.description)),
    );
    if !params.is_empty() {
        lines.push(String::new());
    }
    lines.extend(wrap_text(entry.def.long_desc(), width));
    lines
}

/// The command name of `entry` followed by each parameter, as `<name>` when
/// the parameter is required and `[name]` when it is optional.
fn usage_line(entry: &RegistryEntry) -> String {
    let mut line = entry.command_name.clone();
    for param in entry.def.params() {
        let (open, close) = match param.required {
            true => ('<', '>'),
            false => ('[', ']'),
        };
        line.push(' ');
        line.push(open);
        line.push_str(param.name);
        line.push(close);
    }
    line
}

/// Paint command-palette result rows into `area` starting at `start_row`, one
/// row per entry, with the selected row, fuzzy-match characters, and each
/// entry's short description highlighted.
///
/// Shared by the live list, which derives `start_row` from the selection, and
/// the smooth-scroll pool, which paints absolute pages, so both render
/// identical rows. The selected row draws through
/// [`chrome::selected_row`](crate::render::chrome::selected_row) with
/// `panels`, which a pool page passes false.
///
/// Each name leaves out its first `name_offset` bytes, the group token and
/// space a grouped list's title already shows. A match position indexes the
/// whole name, so one inside the left-out part paints nothing.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_palette_rows(
    filtered: &[&'static RegistryEntry],
    match_indices: &[Vec<u32>],
    selected: usize,
    name_offset: usize,
    area: Rect,
    start_row: usize,
    theme: &crate::theme::Theme,
    panels: bool,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }

    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let desc_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let match_style = theme.list_match();

    let name_col_width: usize = filtered
        .iter()
        .skip(start_row)
        .take(rows)
        .map(|e| shown_name(e, name_offset).len())
        .max()
        .unwrap_or(0);

    let end_x = area.x + area.width;
    let empty_indices: Vec<u32> = Vec::new();

    for (row_idx, entry) in filtered.iter().skip(start_row).take(rows).enumerate() {
        let abs = start_row + row_idx;
        let row = area.y + row_idx as u16;
        let is_selected = abs == selected;
        let style = match is_selected {
            true => crate::render::chrome::selected_row(
                Rect::new(area.x, row, area.width, 1),
                row_style,
                panels,
                theme,
                buf,
                scene,
            ),
            false => {
                for col in area.x..end_x {
                    buf[(col, row)].set_char(' ').set_style(row_style);
                }
                row_style
            },
        };

        let name = shown_name(entry, name_offset);
        let name_x = area.x + 1;
        write_str_clipped(buf, name_x, row, name, style, end_x);
        let indices = match_indices.get(abs).unwrap_or(&empty_indices);
        for (name_col, _) in name.chars().enumerate() {
            let col = name_x + name_col as u16;
            if col >= end_x {
                break;
            }
            if indices
                .binary_search(&((name_offset + name_col) as u32))
                .is_ok()
            {
                buf[(col, row)].set_style(match_style);
            }
        }
        let desc_col = area.x + 1 + name_col_width as u16 + 2;
        if desc_col < end_x {
            let desc_style = if is_selected { style } else { desc_style };
            write_str_clipped(
                buf,
                desc_col,
                row,
                entry.def.short_desc(),
                desc_style,
                end_x,
            );
        }
    }
}

/// The part of `entry`'s command name a row shows, past its first `offset`
/// bytes.
fn shown_name(entry: &RegistryEntry, offset: usize) -> &str {
    entry.command_name.get(offset..).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::{
        count_status, doc_lines, grouped, paint_doc_lines, paint_palette_rows,
        palette_filter_layout, render_command_palette, usage_line, DOC_ROWS, ISLAND_BOTTOM,
        ISLAND_GAP, ISLAND_SIDE, ISLAND_TOP, LIST_ROWS,
    };
    use crate::{
        render::editor::ResolvedChrome,
        test_fixture::{plate_islands, row_rules},
        theme::Theme,
    };
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Style},
    };
    use stoat_action::registry;

    fn layout(area: Rect, content_rows: u16, zoom: i8) -> super::PaletteFilterLayout {
        palette_filter_layout(area, content_rows, zoom, 1).expect("the area hosts the palette")
    }

    /// A Cmd Line count groups its thousands with `,`, as the boards write
    /// `1,284`, and reads muted. A count of nothing paints its `0` in the
    /// deleted color.
    #[test]
    fn a_count_status_groups_its_thousands_and_flags_a_zero() {
        let theme = {
            let src = r##"theme t {
                ui.text.muted.fg = "#050505";
                diff.deleted.fg = "#080808";
            }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let muted = Style::default().fg(Color::Rgb(5, 5, 5));
        let deleted = Style::default().fg(Color::Rgb(8, 8, 8));

        assert_eq!(
            (
                [0, 999, 1_284, 1_234_567].map(grouped),
                count_status(1_284, " commands", &theme),
                count_status(0, " of 12", &theme),
            ),
            (
                ["0", "999", "1,284", "1,234,567"].map(String::from),
                vec![("1,284 commands".to_owned(), muted)],
                vec![("0".to_owned(), deleted), (" of 12".to_owned(), muted)],
            ),
        );
    }

    /// Paint one row of `list_width` cells at x=2 inside a buffer with room to
    /// spare on either side, and report every cell the painter touched outside
    /// the list rect it was handed.
    fn cells_painted_outside(
        entry: &'static registry::RegistryEntry,
        list_width: u16,
    ) -> Vec<(u16, u16)> {
        let area = Rect::new(0, 0, list_width + 20, 3);
        let list = Rect::new(2, 1, list_width, 1);
        let mut buf = Buffer::empty(area);
        paint_palette_rows(
            &[entry],
            &[Vec::new()],
            0,
            0,
            list,
            0,
            &Theme::empty(),
            false,
            &mut buf,
            &mut stoat_widgets::ApcScene::new(),
        );

        let pristine = Buffer::empty(area);
        (area.y..area.y + area.height)
            .flat_map(|y| (area.x..area.x + area.width).map(move |x| (x, y)))
            .filter(|&(x, y)| !list.contains((x, y).into()))
            .filter(|&(x, y)| buf[(x, y)] != pristine[(x, y)])
            .collect()
    }

    /// A row wider than the list it is painted into must stop at the list's
    /// edge. The list spans the full inner width of the modal, so its edge is
    /// the border column, and anything past that lands on the editor behind.
    #[test]
    fn an_overlong_row_paints_nothing_outside_the_list() {
        let entry = registry::all()
            .max_by_key(|e| e.command_name.len() + e.def.short_desc().len())
            .expect("the registry has entries");
        let name_len = entry.command_name.len() as u16;
        assert!(
            entry.def.short_desc().len() > 1,
            "the widest entry needs a description to overrun with"
        );

        // The name fits with the two-column gap after it, so the description
        // starts on screen and runs off the end.
        assert_eq!(
            cells_painted_outside(entry, name_len + 4),
            Vec::new(),
            "a description longer than the room left for it stays inside"
        );

        // Narrower than the name itself, which suppresses the description
        // entirely and leaves the name as the only thing that can overrun.
        assert_eq!(
            cells_painted_outside(entry, name_len),
            Vec::new(),
            "and so does a name wider than the whole list"
        );
    }

    #[test]
    fn a_short_command_list_keeps_the_recommended_box() {
        let laid = layout(Rect::new(0, 0, 200, 60), 4, 0);
        assert_eq!(
            (laid.modal.width, laid.modal.height),
            (160, 22),
            "a list under LIST_ROWS leaves the box at its recommended size"
        );
        assert_eq!(
            (laid.list.height, laid.doc.height),
            (LIST_ROWS, DOC_ROWS),
            "and the list and doc keep their fixed shares"
        );
    }

    #[test]
    fn a_long_command_list_grows_the_list_not_the_doc() {
        let laid = layout(Rect::new(0, 0, 200, 60), 18, 0);
        assert_eq!(
            laid.modal.height, 30,
            "eighteen rows plus twelve chrome rows size the box"
        );
        assert_eq!(
            (laid.list.height, laid.doc.height),
            (18, DOC_ROWS),
            "the extra rows all land in the list, leaving the doc at its share"
        );
    }

    #[test]
    fn the_box_never_takes_more_than_half_the_area() {
        // A `:o ` over thousands of files would otherwise swallow the screen.
        let laid = layout(Rect::new(0, 0, 200, 60), u16::MAX - 100, 0);
        assert_eq!(
            laid.modal.height, 30,
            "a list larger than the screen stops at half the area"
        );
    }

    #[test]
    fn zoom_moves_both_dimensions() {
        let size = |zoom| {
            let modal = layout(Rect::new(0, 0, 200, 60), 4, zoom).modal;
            (modal.width, modal.height)
        };

        assert_eq!(
            (size(2), size(-1)),
            ((198, 34), (140, 16)),
            "each step moves both by a tenth of the area, and growth stops at the thin margin"
        );
    }

    #[test]
    fn a_box_too_short_for_everything_gives_up_the_doc_first() {
        // The list is the primary content, so a cramped terminal keeps its rows
        // and drops the documentation pane.
        let laid = layout(Rect::new(0, 0, 200, 16), 4, 0);
        assert_eq!(
            laid.modal.height, 12,
            "the box takes the area less its margin"
        );
        assert_eq!(
            (laid.list.height, laid.doc.height),
            (6, 0),
            "every remaining body row goes to the list"
        );
    }

    /// A Cmd Line that wraps onto a second row lowers the list by that row. The
    /// box keeps its height, so the doc gives up the row.
    #[test]
    fn a_wrapped_header_lowers_the_list_and_takes_the_doc_rows_first() {
        let area = Rect::new(0, 0, 200, 60);
        let one = layout(area, 4, 0);
        let two = palette_filter_layout(area, 4, 0, 2).expect("the area hosts the palette");

        let inner = one.inner;
        let list_y = inner.y + 3;
        assert_eq!(
            (two.modal, two.inner, two.list, two.doc, two.footer),
            (
                one.modal,
                inner,
                Rect::new(inner.x, list_y, inner.width, LIST_ROWS),
                Rect::new(inner.x, list_y + LIST_ROWS + 1, inner.width, DOC_ROWS - 1),
                one.footer,
            ),
            "the box holds, the list starts a row lower, and the doc gives up the row"
        );
    }

    /// A usage line names the command, then each parameter in angle brackets
    /// when required and in square brackets when optional.
    #[test]
    fn a_usage_line_brackets_each_parameter_by_whether_it_is_required() {
        let usage = |name| usage_line(registry::lookup_alias(name).expect("a command"));
        assert_eq!(
            ["git log", "git review", "quit"].map(usage),
            ["git log [rev]", "git review <reference>", "quit"],
        );
    }

    /// A doc line wider than the doc region stops at the region's right edge,
    /// so it paints nothing onto the border or the editor behind.
    #[test]
    fn a_wide_doc_line_stops_at_the_region_edge() {
        let area = Rect::new(0, 0, 40, 3);
        let doc = Rect::new(2, 1, 20, 1);
        let mut buf = Buffer::empty(area);
        paint_doc_lines(&mut buf, doc, &["x".repeat(60)], Style::default());
        let painted: Vec<u16> = (0..area.width)
            .filter(|&x| buf[(x, 1)].symbol() == "x")
            .collect();
        assert_eq!(painted, (doc.x + 1..doc.x + doc.width).collect::<Vec<_>>());
    }

    /// A command's doc opens with its usage, its aliases, one line per
    /// parameter, and a blank line before its long description.
    #[test]
    fn a_doc_reads_usage_aliases_and_parameters_before_the_description() {
        let entry = registry::lookup_alias("git log").expect("a command");
        let param = &entry.def.params()[0];
        let lines = doc_lines(entry, 60);
        assert_eq!(
            (&lines[..4], lines.len() > 4),
            (
                &[
                    "git log [rev]".to_string(),
                    "alias: git ls".to_string(),
                    format!("  rev  {}", param.description),
                    String::new(),
                ][..],
                true,
            ),
        );
    }

    /// Under the plate look the command list and the doc pane draw as Islands
    /// stacked across the separator row, which draws no rule.
    #[test]
    fn under_the_plate_the_list_and_the_doc_pane_draw_as_islands() {
        let mut h = crate::Stoat::test();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCommandPalette);
        let mut palette = h.stoat.command_palette.take().expect("the palette is open");
        let theme = h.stoat.theme.clone();
        let chrome = ResolvedChrome::resolve(&theme);
        let area = Rect::new(0, 0, 200, 40);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        scene.set_nudge_protocol(true);
        render_command_palette(
            &mut palette,
            h.stoat.active_workspace_mut(),
            None,
            &theme,
            &chrome,
            area,
            0,
            crate::render::picker::preview_min_columns(None),
            &mut buf,
            &mut scene,
        );

        let layout = layout(area, palette.list_rows_hint(), 0);
        assert_eq!(
            (
                plate_islands(&theme, &scene),
                row_rules(&scene, layout.doc.y - 1)
            ),
            (
                vec![
                    (
                        layout.list,
                        [ISLAND_TOP, ISLAND_SIDE, ISLAND_GAP, ISLAND_SIDE]
                    ),
                    (
                        layout.doc,
                        [ISLAND_GAP, ISLAND_SIDE, ISLAND_BOTTOM, ISLAND_SIDE]
                    ),
                ],
                0,
            ),
        );
    }
}

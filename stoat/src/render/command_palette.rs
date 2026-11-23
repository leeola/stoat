use crate::{
    command_palette::{ArgPicker, CommandPalette, ResolvedCommand},
    input_view::InputView,
    keymap::ActionKeys,
    render::{
        chrome::{ISLAND_BOTTOM, ISLAND_GAP, ISLAND_SIDE, ISLAND_TOP},
        cmd_line::{self, CmdLine, CmdStatus},
        paint::style_rgb,
        picker::{Footer, PathRows, RowMarks, RowStart, FOOTER_ROWS},
        text::{wrap_text, write_str, write_str_clipped},
    },
    theme::TypeRole,
    workspace::Workspace,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, Borders, StatefulWidget},
};
use std::path::Path;
use stoat_action::registry::RegistryEntry;
use stoat_widgets::{
    bar::Bar,
    text_run::{self, TextRun},
};

const LIST_ROWS: u16 = 10;
const DOC_ROWS: u16 = 6;

/// The cells a root row's name column takes at least, the CmdRoot board's
/// `width: 13ch`.
const NAME_COLUMNS: u16 = 13;

/// The cells a root row's alias column takes at least, the board's
/// `width: 3ch`.
const ALIAS_COLUMNS: u16 = 3;

/// The cells of a root row's signature column, the board's `width: 28ch`.
const SIGNATURE_COLUMNS: u16 = 28;

/// The sixteenths between a Source Tag's box and its label on each side, the
/// board's `padding: 0 5px`.
const SOURCE_TAG_INSET: u16 = 10;

/// The height of a Source Tag's box, in sixteenths of a row, one in from the
/// row's top. It is the whole sixteenth nearest the board's
/// `line-height: 16px` in an 18 px row.
const SOURCE_TAG_HEIGHT: u16 = 14;

/// The radius a Source Tag's box rounds its corners at, in logical pixels,
/// the board's `border-radius: 4px`.
const SOURCE_TAG_RADIUS: u8 = 4;

/// The label and the color scope of the Source Tag that ends every root row,
/// because every command in the registry ships with stoat.
const BUILT_IN_TAG: (&str, &str) = ("built-in", crate::theme::scope::UI_CMD_SOURCE_BUILTIN);

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
        palette.resolved.as_ref(),
        &palette.keys,
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
        palette.resolved.as_ref(),
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
        palette.resolved.as_ref(),
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
/// shows the live input with its caret and `status` at its right end. A
/// `resolved` command token draws with its ghost, as [`input_spans`] gives
/// them.
///
/// See also:
/// - [`cmd_line::cmd_line`] for the row itself.
/// - [`render_palette_filter`] for the command-list body.
/// - [`render_palette_arg_picker`] for the inline-picker body.
/// - [`render_palette_free_arg`] for the free-typed argument body.
#[allow(clippy::too_many_arguments)]
fn render_palette_prelude(
    input: &InputView,
    resolved: Option<&ResolvedCommand>,
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
    let (input, caret) = input_spans(&text, caret, resolved, theme);
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

/// The palette input `text` as Cmd Line spans, and the display caret for the
/// typed caret at char `caret`.
///
/// A `resolved` head draws in `ui.cmd.command`, its ghost follows in
/// `ui.cmd.ghost`, and the rest of the text draws in `ui.text`. A theme that
/// leaves `ui.cmd.command` unset draws the head as the rest, and a theme that
/// leaves `ui.cmd.ghost` unset draws no ghost.
///
/// The ghost is display only, so the caret stays on the typed text. A caret
/// past the head moves right by the ghost's chars, and a caret at the head's
/// end covers the ghost's first char, as a shell draws a suggestion.
fn input_spans(
    text: &str,
    caret: usize,
    resolved: Option<&ResolvedCommand>,
    theme: &crate::theme::Theme,
) -> (Vec<(String, Style)>, usize) {
    let plain = theme.get(crate::theme::scope::UI_TEXT);
    let Some(resolved) = resolved else {
        return (vec![(text.to_owned(), plain)], caret);
    };

    let split = text
        .char_indices()
        .nth(resolved.head_chars)
        .map_or(text.len(), |(at, _)| at);
    let (head, rest) = text.split_at(split);
    let command = theme
        .try_get(crate::theme::scope::UI_CMD_COMMAND)
        .unwrap_or(plain);
    let mut spans = vec![(head.to_owned(), command)];

    let mut shown = caret;
    let ghost = theme
        .try_get(crate::theme::scope::UI_CMD_GHOST)
        .filter(|_| !resolved.ghost.is_empty());
    if let Some(ghost) = ghost {
        spans.push((resolved.ghost.clone(), ghost));
        if caret > resolved.head_chars {
            shown += resolved.ghost.chars().count();
        }
    }

    if !rest.is_empty() {
        spans.push((rest.to_owned(), plain));
    }
    (spans, shown)
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
/// the paint stores there the rows that it draws. The doc names the keys of
/// `keys` that run the selected command's action.
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
    resolved: Option<&ResolvedCommand>,
    keys: &ActionKeys,
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
        resolved,
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
        let width = usize::from(inner.width.saturating_sub(1));
        let lines = filtered
            .get(selected)
            .map(|entry| doc_lines(entry, keys, width, theme))
            .unwrap_or_default();
        paint_doc_lines(buf, doc, &lines, theme, scene);
    }

    let parts = Footer {
        hints: &PALETTE_HINTS,
        ..Footer::default()
    };
    crate::render::picker::modal_footer(buf, layout.footer, &parts, plate, theme, scene);
}

/// Paint `lines` into the doc region `doc`, one per row from its top, each
/// line's spans one after another from the region's second cell, clipped at
/// its right edge.
///
/// The first line ends in the built-in Source Tag one cell after its text, as
/// [`paint_doc_tag`] places it.
fn paint_doc_lines(
    buf: &mut Buffer,
    doc: Rect,
    lines: &[Vec<(String, Style)>],
    theme: &crate::theme::Theme,
    scene: &mut stoat_widgets::ApcScene,
) {
    let end = doc.x + doc.width;
    for (row, line) in (doc.y..doc.bottom()).zip(lines) {
        let mut x = doc.x + 1;
        for (text, style) in line {
            write_str_clipped(buf, x, row, text, *style, end);
            x = x.saturating_add(text.chars().count() as u16);
        }

        if row == doc.y {
            paint_doc_tag(buf, x.saturating_add(1), row, end, theme, scene);
        }
    }
}

/// Paint the built-in Source Tag of a doc region's first line from cell `x`
/// of `row`, as [`paint_source_tag`] draws it.
///
/// A theme that leaves the tag's scope unset draws no tag, and neither does a
/// tag that does not end one cell inside `end`, the region's right edge.
fn paint_doc_tag(
    buf: &mut Buffer,
    x: u16,
    row: u16,
    end: u16,
    theme: &crate::theme::Theme,
    scene: &mut stoat_widgets::ApcScene,
) {
    let (label, scope) = BUILT_IN_TAG;
    let Some(style) = theme.try_get(scope) else {
        return;
    };
    let label_cells = label.chars().count() as u16;
    let cells = source_tag_width(label, theme).div_ceil(16).max(label_cells);
    if x.saturating_add(cells) >= end {
        return;
    }

    // A dead scene ends the label one cell inside the rect it paints into, so
    // a rect one cell wider than the label puts the label where the live box
    // starts.
    let rect = Rect::new(x, row, label_cells + 1, 1);
    paint_source_tag(buf, rect, 0, label, style, theme, scene);
}

/// The help of `entry` as doc-region lines, each a run of styled spans.
///
/// The first line names the command in bold and its aliases after a blank
/// cell, and [`paint_doc_lines`] ends it in the command's Source Tag. The
/// [`usage_line`] follows, then the long description wrapped to `width`, and
/// last the action the command runs with the keys of `keys` that reach it.
/// The argument stage names the parameter that it collects, so no line names
/// a parameter.
fn doc_lines(
    entry: &RegistryEntry,
    keys: &ActionKeys,
    width: usize,
    theme: &crate::theme::Theme,
) -> Vec<Vec<(String, Style)>> {
    let text = theme.get(crate::theme::scope::UI_TEXT);
    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let dim = theme.get(crate::theme::scope::UI_TEXT_DIM);
    let key_label = theme.get(crate::theme::scope::UI_KEY_LABEL);

    let mut name = vec![(
        entry.command_name.clone(),
        text.add_modifier(Modifier::BOLD),
    )];
    let aliases = entry.def.aliases();
    if !aliases.is_empty() {
        name.push((format!(" {}", aliases.join(", ")), muted));
    }

    let mut lines = vec![name, vec![(usage_line(entry), text)]];
    lines.extend(
        wrap_text(entry.def.long_desc(), width)
            .into_iter()
            .map(|line| vec![(line, dim)]),
    );

    let action = entry.def.name();
    let mut runs = vec![
        ("runs action ".to_owned(), muted),
        (action.to_owned(), key_label),
    ];
    if let Some(bound) = keys.get(action).filter(|bound| !bound.is_empty()) {
        runs.push((" \u{b7} bound to ".to_owned(), muted));
        runs.push((bound.join(", "), key_label));
    }
    lines.push(runs);
    lines
}

/// The command name of `entry` followed by its [`signature`].
fn usage_line(entry: &RegistryEntry) -> String {
    let signature = signature(entry);
    match signature.is_empty() {
        true => entry.command_name.clone(),
        false => format!("{} {signature}", entry.command_name),
    }
}

/// Each parameter of `entry`, as `<name>` when the parameter is required and
/// `[name]` when it is optional, one space apart.
fn signature(entry: &RegistryEntry) -> String {
    let params: Vec<String> = entry
        .def
        .params()
        .iter()
        .map(|param| match param.required {
            true => format!("<{}>", param.name),
            false => format!("[{}]", param.name),
        })
        .collect();
    params.join(" ")
}

/// Paint command-palette result rows into `area` starting at `start_row`, one
/// row per entry, with the selected row and the fuzzy-match characters
/// highlighted.
///
/// Shared by the live list, which derives `start_row` from the selection, and
/// the smooth-scroll pool, which paints absolute pages, so both render
/// identical rows. The selected row draws through
/// [`chrome::selected_row`](crate::render::chrome::selected_row) with
/// `panels`, which a pool page passes false.
///
/// A row reads the command's name in bold, its first alias, its
/// [`signature`], and its short description, one cell apart, and ends in its
/// Source Tag as [`paint_source_tag`] draws it. The name and alias columns
/// take [`NAME_COLUMNS`] and [`ALIAS_COLUMNS`] cells and grow to the longest
/// name and alias on show, because a cut name names nothing. The signature
/// clips at [`SIGNATURE_COLUMNS`], because the doc region shows the whole
/// usage line.
///
/// Each name leaves out its first `name_offset` bytes, the group token and
/// space a grouped list's title already shows, and so does an alias that
/// starts with them. A match position indexes the whole name, so one inside
/// the left-out part paints nothing.
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
    let name_style = row_style.add_modifier(Modifier::BOLD);
    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let dim = theme.get(crate::theme::scope::UI_TEXT_DIM);
    let match_style = theme.list_match();
    let keeps_columns = theme
        .try_get(crate::theme::scope::UI_LIST_SELECTED)
        .is_some();

    let on_show = || filtered.iter().skip(start_row).take(rows);
    let widest = |cells: fn(&RegistryEntry, usize) -> &str, least: u16| {
        on_show()
            .map(|entry| cells(entry, name_offset).chars().count())
            .max()
            .unwrap_or(0)
            .max(usize::from(least)) as u16
    };
    let name_cols = widest(shown_name, NAME_COLUMNS);
    let alias_cols = widest(shown_alias, ALIAS_COLUMNS);

    let (tag_label, tag_scope) = BUILT_IN_TAG;
    let tag = theme.try_get(tag_scope).map(|style| {
        let start = area
            .width
            .saturating_sub(1)
            .saturating_mul(16)
            .saturating_sub(source_tag_width(tag_label, theme));
        (start, style)
    });
    let end_x = area.x + area.width;
    let desc_end = match tag {
        Some((start, _)) => area.x + (start / 16).saturating_sub(1),
        None => end_x.saturating_sub(1),
    };
    let empty_indices: Vec<u32> = Vec::new();

    for (row_idx, entry) in on_show().enumerate() {
        let abs = start_row + row_idx;
        let row = Rect::new(area.x, area.y + row_idx as u16, area.width, 1);
        let is_selected = abs == selected;
        let style = match is_selected {
            true => crate::render::chrome::selected_row(row, row_style, panels, theme, buf, scene),
            false => {
                for col in row.left()..row.right() {
                    buf[(col, row.y)].set_char(' ').set_style(row_style);
                }
                row_style
            },
        };
        let column = |own: Style| match (is_selected, keeps_columns) {
            (false, _) => own,
            (true, true) => Style {
                bg: style.bg,
                ..own
            },
            (true, false) => style,
        };

        let name = shown_name(entry, name_offset);
        let name_x = area.x + 1;
        write_str_clipped(buf, name_x, row.y, name, column(name_style), end_x);
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
                buf[(col, row.y)].set_style(match_style);
            }
        }

        let alias_x = name_x.saturating_add(name_cols + 1);
        let alias = shown_alias(entry, name_offset);
        write_str_clipped(buf, alias_x, row.y, alias, column(muted), end_x);

        let signature_x = alias_x.saturating_add(alias_cols + 1);
        let signature_end = signature_x.saturating_add(SIGNATURE_COLUMNS).min(end_x);
        let signature = signature(entry);
        write_str_clipped(
            buf,
            signature_x,
            row.y,
            &signature,
            column(muted),
            signature_end,
        );

        let desc_x = signature_x.saturating_add(SIGNATURE_COLUMNS + 1);
        let desc = entry.def.short_desc();
        write_str_clipped(buf, desc_x, row.y, desc, column(dim), desc_end);

        if let Some((start, style)) = tag {
            paint_source_tag(buf, row, start, tag_label, style, theme, scene);
        }
    }
}

/// The part of `entry`'s command name a row shows, past its first `offset`
/// bytes.
fn shown_name(entry: &RegistryEntry, offset: usize) -> &str {
    entry.command_name.get(offset..).unwrap_or("")
}

/// The first alias of `entry` as a row shows it, or `""` for a command with no
/// alias.
///
/// An alias that starts with the first `offset` bytes of the command name, the
/// group token and its space, leaves them out as [`shown_name`] does.
fn shown_alias(entry: &RegistryEntry, offset: usize) -> &str {
    let alias = entry.def.aliases().first().copied().unwrap_or("");
    entry
        .command_name
        .get(..offset)
        .and_then(|token| alias.strip_prefix(token))
        .unwrap_or(alias)
}

/// The width of a Source Tag's box for `label`, in sixteenths of a cell.
///
/// The box holds the label at the [`TypeRole::Small`] size with
/// [`SOURCE_TAG_INSET`] on each side.
fn source_tag_width(label: &str, theme: &crate::theme::Theme) -> u16 {
    let small = theme.type_scale(TypeRole::Small);
    text_run::advance_sixteenths(label.chars().count(), small) + 2 * SOURCE_TAG_INSET
}

/// Paint the Source Tag `label` in `style` over `row`, its box from `start`
/// sixteenths, as [`source_tag_width`] sizes it.
///
/// A live scene draws the box as a rounded [`Bar`] when the style's background
/// is RGB, and the label as a [`TypeRole::Small`] run over it with no
/// background. A pool page keeps the same form, because it captures bars and
/// runs but no panel. A dead scene, or a label color outside RGB, writes the
/// label in cells in `style`, in the cells that end one cell inside the row's
/// right edge.
fn paint_source_tag(
    buf: &mut Buffer,
    row: Rect,
    start: u16,
    label: &str,
    style: Style,
    theme: &crate::theme::Theme,
    scene: &mut stoat_widgets::ApcScene,
) {
    let Some(color) = style_rgb(style.fg).filter(|_| scene.live()) else {
        let cells = label.chars().count() as u16;
        let x = row.right().saturating_sub(1 + cells).max(row.x);
        write_str_clipped(buf, x, row.y, label, style, row.right());
        return;
    };

    // The box goes into the scene before the label, so the label draws over
    // it. It goes alone, because a bar batch drops the corner radius.
    if let Some(fill) = style_rgb(style.bg) {
        let tag_box = Bar {
            x: start as i16,
            y: 1,
            width: source_tag_width(label, theme),
            height: SOURCE_TAG_HEIGHT,
            color: fill,
            radius: SOURCE_TAG_RADIUS,
            corners: 0,
        };
        StatefulWidget::render(tag_box, row, buf, scene);
    }
    let run = TextRun {
        col: (start + SOURCE_TAG_INSET) as i16,
        row: 0,
        scale: theme.type_scale(TypeRole::Small),
        color,
        bg: None,
        text: label,
        bold: false,
        italic: false,
        follow: 0,
        anchor: None,
    };
    StatefulWidget::render(run, row, buf, scene);
}

#[cfg(test)]
mod tests {
    use super::{
        count_status, doc_lines, grouped, input_spans, paint_doc_lines, paint_palette_rows,
        palette_filter_layout, render_command_palette, usage_line, wrap_text, DOC_ROWS,
        ISLAND_BOTTOM, ISLAND_GAP, ISLAND_SIDE, ISLAND_TOP, LIST_ROWS,
    };
    use crate::{
        command_palette::ResolvedCommand,
        keymap::ActionKeys,
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
    use stoatty_protocol::command::{decode_stream, BarCommand, Command, TextRunCommand};

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

    /// The ghost follows the head in its own color, and the caret stays on the
    /// typed text. A caret past the head moves right by the ghost, a caret at
    /// the head's end covers the ghost's first char, and a theme without the
    /// ghost's scope draws no ghost.
    #[test]
    fn the_ghost_follows_the_head_and_the_caret_stays_on_the_typed_text() {
        let theme = {
            let src = r##"theme t {
                ui.text.fg = "#040404";
                ui.cmd.command.fg = "#050505";
                ui.cmd.ghost.fg = "#060606";
            }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let open = ResolvedCommand {
            head_chars: 1,
            ghost: "pen".to_owned(),
        };
        let spans = |parts: &[(&str, u8)]| -> Vec<(String, Style)> {
            parts
                .iter()
                .map(|&(text, gray)| {
                    (
                        text.to_owned(),
                        Style::default().fg(Color::Rgb(gray, gray, gray)),
                    )
                })
                .collect()
        };
        let plain = |text: &str| {
            (
                text.to_owned(),
                Theme::empty().get(crate::theme::scope::UI_TEXT),
            )
        };

        assert_eq!(
            [
                input_spans("o src", 5, Some(&open), &theme),
                input_spans("o", 1, Some(&open), &theme),
                input_spans("o src", 5, Some(&open), &Theme::empty()),
            ],
            [
                (spans(&[("o", 5), ("pen", 6), (" src", 4)]), 8),
                (spans(&[("o", 5), ("pen", 6)]), 1),
                (vec![plain("o"), plain(" src")], 5),
            ],
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

        // The name fits with the gap after it, so the alias starts on screen
        // and the columns after the name run off the end.
        assert_eq!(
            cells_painted_outside(entry, name_len + 4),
            Vec::new(),
            "columns longer than the room left for them stay inside"
        );

        // Narrower than the name itself, which puts every column after the
        // name past the edge, so only the name runs past the list's end.
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

    /// Paint the rows of the commands named `names` into the list `list` under
    /// `theme`, with no row selected, by a scene that is live where `live`
    /// holds.
    fn painted_rows(
        names: &[&str],
        name_offset: usize,
        list: Rect,
        theme: &Theme,
        live: bool,
    ) -> (Buffer, stoat_widgets::ApcScene) {
        let entries: Vec<&'static registry::RegistryEntry> = names
            .iter()
            .map(|name| registry::lookup_alias(name).expect("a command"))
            .collect();
        let mut buf = Buffer::empty(list);
        let mut scene = stoat_widgets::ApcScene::new();
        scene.set_live(live);
        paint_palette_rows(
            &entries,
            &vec![Vec::new(); entries.len()],
            usize::MAX,
            name_offset,
            list,
            0,
            theme,
            false,
            &mut buf,
            &mut scene,
        );
        (buf, scene)
    }

    /// The text of `buf`'s row `y`.
    fn row_text(buf: &Buffer, y: u16) -> String {
        (buf.area.left()..buf.area.right())
            .map(|x| buf[(x, y)].symbol())
            .collect()
    }

    /// A row of `width` blanks with each of `parts` from its cell.
    fn placed(width: usize, parts: &[(usize, &str)]) -> String {
        let mut row = " ".repeat(width);
        for &(at, text) in parts {
            row.replace_range(at..at + text.len(), text);
        }
        row
    }

    /// A theme that sets only the built-in Source Tag's scope.
    fn tag_theme() -> Theme {
        let src = r##"theme t { ui.cmd.source.builtin = { fg: "#010203", bg: "#040506" }; }"##;
        let (config, _) = stoat_config::parse(src);
        Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
    }

    /// A root row reads the command's name from its second cell, its first
    /// alias, its signature, and its short description, one cell apart in
    /// columns of 13, 3, and 28 cells.
    #[test]
    fn a_root_row_reads_its_name_alias_signature_and_description() {
        let list = Rect::new(0, 0, 100, 1);
        let (buf, _) = painted_rows(&["open"], 0, list, &Theme::empty(), true);
        assert_eq!(
            row_text(&buf, 0),
            placed(
                100,
                &[(1, "open"), (15, "o"), (19, "<path>"), (48, "open a file")]
            ),
        );
    }

    /// The name and alias columns grow to the longest name and alias on show,
    /// so the alias and the signature start past a long name.
    #[test]
    fn a_long_name_and_a_long_alias_widen_their_columns() {
        let names = ["git log", "walkthrough-next-annotation"];
        let (buf, _) = painted_rows(&names, 0, Rect::new(0, 0, 100, 2), &Theme::empty(), true);
        assert_eq!(
            &row_text(&buf, 0)[..65],
            placed(65, &[(1, "git log"), (29, "git ls"), (36, "[rev]")]),
        );
    }

    /// Under a group the name and an alias that starts with the group token
    /// leave the token and its space out.
    #[test]
    fn a_grouped_row_leaves_the_group_token_out_of_its_name_and_alias() {
        let list = Rect::new(0, 0, 100, 1);
        let (buf, _) = painted_rows(&["git log"], 4, list, &Theme::empty(), true);
        assert_eq!(
            &row_text(&buf, 0)[..19],
            placed(19, &[(1, "log"), (15, "ls")])
        );
    }

    /// A theme that sets the built-in tag's scope ends a row in a rounded box
    /// in the scope's background, one cell inside the list's right edge, and
    /// the label over the box in the scope's foreground.
    #[test]
    fn a_root_row_ends_in_its_source_tag() {
        let list = Rect::new(0, 0, 100, 1);
        let (_, scene) = painted_rows(&["open"], 0, list, &tag_theme(), true);
        let commands: Vec<Command> = decode_stream(scene.bytes())
            .into_iter()
            .filter(|command| *command != Command::TextRunEnd)
            .collect();
        assert_eq!(
            commands,
            [
                Command::Bar(BarCommand {
                    x: 1436,
                    y: 1,
                    width: 148,
                    height: 14,
                    color: [4, 5, 6],
                    occludes: false,
                    hatch: None,
                    radius: 4,
                    corners: 0,
                }),
                Command::TextRun(TextRunCommand {
                    col: 1446,
                    row: 0,
                    scale: 256,
                    color: [1, 2, 3],
                    bg: None,
                    bold: false,
                    italic: false,
                    follow: 0,
                    anchor: None,
                    text: "built-in".to_owned(),
                }),
            ],
        );
    }

    /// A dead scene writes the tag's label in cells in the scope's style, in
    /// the cells that end one cell inside the list's right edge.
    #[test]
    fn a_dead_scene_writes_the_source_tag_in_cells() {
        let list = Rect::new(0, 0, 100, 1);
        let (buf, _) = painted_rows(&["open"], 0, list, &tag_theme(), false);
        let styles: Vec<(Color, Color)> =
            (91..99).map(|x| (buf[(x, 0)].fg, buf[(x, 0)].bg)).collect();
        assert_eq!(
            (&row_text(&buf, 0)[89..], styles),
            (
                "  built-in ",
                vec![(Color::Rgb(1, 2, 3), Color::Rgb(4, 5, 6)); 8],
            ),
        );
    }

    /// A theme that leaves the tag's scope unset draws no tag, in the scene or
    /// in cells, and a long description stops one cell inside the list's right
    /// edge, the inset that a tag keeps.
    #[test]
    fn a_theme_without_the_tag_scope_draws_no_tag() {
        let list = Rect::new(0, 0, 60, 1);
        let (buf, scene) = painted_rows(&["git log"], 0, list, &Theme::empty(), true);
        assert_eq!(
            (decode_stream(scene.bytes()), row_text(&buf, 0)),
            (
                Vec::new(),
                placed(
                    60,
                    &[
                        (1, "git log"),
                        (15, "git ls"),
                        (22, "[rev]"),
                        (51, "browse c")
                    ]
                ),
            ),
        );
    }

    /// A theme that leaves `ui.list.selected` unset paints the whole selected
    /// row in its selected style, so every column reads in the selection's
    /// colors, as `default_dark` selects in black on its accent.
    #[test]
    fn a_selected_row_without_the_list_scope_takes_one_style() {
        let theme = {
            let src = r##"theme t {
                ui.selection = { fg: "#000000", bg: "#7fb0ff" };
                ui.text.muted.fg = "#050505";
                ui.text.dim.fg = "#060606";
            }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let entries = [registry::lookup_alias("open").expect("a command")];
        let list = Rect::new(0, 0, 100, 1);
        let mut buf = Buffer::empty(list);
        paint_palette_rows(
            &entries,
            &[Vec::new()],
            0,
            0,
            list,
            0,
            &theme,
            false,
            &mut buf,
            &mut stoat_widgets::ApcScene::new(),
        );

        let colors: Vec<(Color, Color)> = [1, 15, 19, 48]
            .map(|x| (buf[(x, 0)].fg, buf[(x, 0)].bg))
            .to_vec();
        assert_eq!(
            colors,
            vec![(Color::Rgb(0, 0, 0), Color::Rgb(0x7f, 0xb0, 0xff)); 4]
        );
    }

    /// A doc line wider than the doc region stops at the region's right edge,
    /// so it paints nothing onto the border or the editor behind.
    #[test]
    fn a_wide_doc_line_stops_at_the_region_edge() {
        let area = Rect::new(0, 0, 40, 3);
        let doc = Rect::new(2, 1, 20, 1);
        let mut buf = Buffer::empty(area);
        let lines = [vec![("x".repeat(60), Style::default())]];
        paint_doc_lines(
            &mut buf,
            doc,
            &lines,
            &Theme::empty(),
            &mut stoat_widgets::ApcScene::new(),
        );
        let painted: Vec<u16> = (0..area.width)
            .filter(|&x| buf[(x, 1)].symbol() == "x")
            .collect();
        assert_eq!(painted, (doc.x + 1..doc.x + doc.width).collect::<Vec<_>>());
    }

    /// A command's doc names the command and its aliases, reads its usage line
    /// and its long description, and ends in the action that it runs with the
    /// keys bound to it, or with the action alone where no key reaches it.
    #[test]
    fn a_doc_reads_the_name_the_usage_the_description_and_the_action() {
        let entry = registry::lookup_alias("git log").expect("a command");
        let texts = |keys: &ActionKeys| -> Vec<Vec<String>> {
            doc_lines(entry, keys, 60, &Theme::empty())
                .into_iter()
                .map(|line| line.into_iter().map(|(text, _)| text).collect())
                .collect()
        };
        let line =
            |parts: &[&str]| -> Vec<String> { parts.iter().map(|part| part.to_string()).collect() };
        let bound = ActionKeys::from([("GitLs".to_owned(), vec!["Spc g l".to_owned()])]);

        let mut expected = vec![line(&["git log", " git ls"]), line(&["git log [rev]"])];
        expected.extend(
            wrap_text(entry.def.long_desc(), 60)
                .into_iter()
                .map(|text| vec![text]),
        );
        expected.push(line(&[
            "runs action ",
            "GitLs",
            " \u{b7} bound to ",
            "Spc g l",
        ]));
        assert_eq!(
            (texts(&bound), texts(&ActionKeys::new()).last().cloned()),
            (expected, Some(line(&["runs action ", "GitLs"]))),
        );
    }

    /// The doc's first line ends in the Source Tag one cell after its text, in
    /// cells under a dead scene. A doc where the tag does not end one cell
    /// inside the right edge draws no tag.
    #[test]
    fn a_doc_ends_its_first_line_in_the_source_tag() {
        let entry = registry::lookup_alias("open").expect("a command");
        let first_row = |width: u16| {
            let theme = tag_theme();
            let lines = doc_lines(entry, &ActionKeys::new(), usize::from(width), &theme);
            let mut buf = Buffer::empty(Rect::new(0, 0, 40, 6));
            let mut scene = stoat_widgets::ApcScene::new();
            scene.set_live(false);
            let doc = Rect::new(0, 0, width, 6);
            paint_doc_lines(&mut buf, doc, &lines, &theme, &mut scene);
            row_text(&buf, 0)
        };

        assert_eq!(
            [first_row(40), first_row(22)],
            [
                placed(40, &[(1, "open o, edit"), (14, "built-in")]),
                placed(40, &[(1, "open o, edit")]),
            ],
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

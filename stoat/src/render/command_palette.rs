use crate::{
    command_palette::{
        is_action_row, split_head, typed_head, ArgPicker, Availability, CommandPalette, Namespace,
        PaletteScope, ResolvedCommand, ACTION_TOKEN,
    },
    input_history,
    input_view::InputView,
    keymap::ActionKeys,
    render::{
        chrome::{ISLAND_BOTTOM, ISLAND_GAP, ISLAND_SIDE, ISLAND_TOP},
        cmd_line::{self, CmdLine, CmdStatus},
        paint::style_rgb,
        picker::{Footer, PathRows, RowMarks, RowStart, FOOTER_ROWS},
        text::{pre_wrap_rows, wrap_text, write_str, write_str_clipped},
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
use std::{borrow::Cow, ops::Range, path::Path};
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

/// The cells that each name column of an `action` list row takes at least,
/// the CmdAction board's `width: 22ch`.
const ACTION_NAME_COLUMNS: u16 = 22;

/// The cells of an `action` list row's key column, the board's `width: 18ch`.
const ACTION_KEY_COLUMNS: u16 = 18;

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

/// The label and the color scope of the Source Tag that names the internal
/// actions, on the `action` row and in an internal action's doc.
const INTERNAL_TAG: (&str, &str) = ("internal", crate::theme::scope::UI_CMD_SOURCE_INTERNAL);

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
    ("\u{2191}", "history"),
];

/// The cell of a History row that holds its U+21BA mark, the row's second.
const HISTORY_MARK_COLUMN: u16 = 1;

/// The cell of a History row that its line starts in, the row's fifth.
///
/// Each next cell row of a wrapped line starts in this cell too.
const HISTORY_LINE_COLUMN: u16 = 4;

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

/// The parts of the palette's key line in one stage, as
/// [`palette_footer_parts`] gives them.
struct PaletteFooterParts {
    /// The key hints, each a bound key and the word for what it does. A word
    /// that names the walk's needle is owned.
    hints: Vec<(&'static str, Cow<'static, str>)>,
    /// The note's spans, empty for a stage with no note.
    note: Vec<(&'static str, Style)>,
}

/// The stage of the palette whose keys its key line names.
enum KeyStage<'a> {
    /// A list of commands, the root or a group. `typed` holds while the input
    /// holds text, and `recallable` while the history holds a line.
    Commands {
        scope: PaletteScope,
        typed: bool,
        recallable: bool,
    },
    /// The `action` list.
    Actions,
    /// History, with the needle of the walk that shows it.
    History { needle: &'a str },
}

/// One entry of History, a line of the palette history that the walk visits.
struct HistoryEntry<'a> {
    /// The index of the line in the palette history.
    index: usize,
    line: &'a str,
    /// The time the line last ran at, in unix seconds, or `None` for a line
    /// with no known time.
    at: Option<i64>,
    /// The chars of `line` that the words of the walk's needle mark, as the
    /// char ranges of [`input_history::word_spans`].
    marks: Vec<Range<usize>>,
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

/// Render the open command palette into `area`.
///
/// While the palette history walks, the palette shows History, as
/// [`render_palette_history`] paints it. The ages of its lines count from
/// `now`, the clock in unix seconds. Otherwise the palette shows the argument
/// picker, the free-typed argument's hint, or the command list, by the stage
/// that the input names.
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
    now: i64,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    if ws.palette_history.is_walking() {
        render_palette_history(palette, ws, home, theme, area, zoom, now, buf, scene);
        return;
    }
    palette.history_rows.clear();

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
    let status = filter_status(palette, theme);
    render_palette_filter(
        &palette.input,
        &palette.filtered,
        &palette.match_indices,
        palette.selected,
        palette.group,
        palette.namespace,
        palette.scope,
        content_rows,
        zoom,
        &mut palette.header_rows,
        &mut palette.doc_scroll,
        palette.resolved.as_ref(),
        &status,
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
        &[],
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
        &[],
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

/// Render History, the lines of the palette history that the walk visits,
/// under a Cmd Line that holds the recalled line and the walk's place.
///
/// History draws in the box of [`palette_filter_layout`]. One Island covers
/// the list rows, the doc rows, and the row between them, and
/// [`paint_history_rows`] paints the rows on it. The Cmd Line's status is the
/// chip `history N / M`. N is the place of the recalled line among the
/// matches, with the newest as 1, and M counts the matches. The words of a
/// needle stand in the chip one space apart, between the word `history` and
/// the count. Each word marks its chars in the rows and in the input, as
/// [`input_history::word_spans`] finds them. The rows that the paint draws go
/// to [`CommandPalette::history_rows`] for the pointer.
///
/// History draws on the live grid with no pool, because its rows differ in
/// height. Each step of the walk also changes the input and the chip with the
/// rows.
#[allow(clippy::too_many_arguments)]
fn render_palette_history(
    palette: &mut CommandPalette,
    ws: &mut Workspace,
    home: Option<&Path>,
    theme: &crate::theme::Theme,
    area: Rect,
    zoom: i8,
    now: i64,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let content_rows = palette.list_rows_hint();
    let Some(layout) = palette_filter_layout(area, content_rows, zoom, palette.header_rows) else {
        return;
    };

    let matches = ws.palette_history.matches();
    let place = ws
        .palette_history
        .cursor()
        .and_then(|cursor| matches.iter().position(|&index| index == cursor))
        .unwrap_or(0);
    let needle = ws.palette_history.needle();
    let chip = {
        let count = format!("{} / {}", place + 1, matches.len());
        let words = needle.split_whitespace().collect::<Vec<_>>().join(" ");
        match words.as_str() {
            "" => format!("history {count}"),
            words => format!("history \u{b7} {words} \u{b7} {count}"),
        }
    };
    let input_marks = input_history::word_spans(&palette.input.text(ws), needle);
    palette.header_rows = render_palette_prelude(
        &palette.input,
        palette.resolved.as_ref(),
        &input_marks,
        CmdStatus::Chip(&chip),
        layout.modal,
        layout.inner,
        home,
        ws,
        theme,
        buf,
        scene,
    );
    let Some(layout) = palette_filter_layout(area, content_rows, zoom, palette.header_rows) else {
        return;
    };

    let body = Rect::new(
        layout.list.x,
        layout.list.y,
        layout.list.width,
        layout.footer.y.saturating_sub(layout.list.y),
    );
    let plate = crate::render::chrome::plate_colors(theme, scene);
    if let Some(plate) = plate {
        let edges = [ISLAND_TOP, ISLAND_SIDE, ISLAND_BOTTOM, ISLAND_SIDE];
        crate::render::chrome::modal_island(body, edges, plate, theme, buf, scene);
    }

    let history = &ws.palette_history;
    let needle = history.needle();
    let entries: Vec<HistoryEntry<'_>> = matches
        .iter()
        .map(|&index| {
            let line = history.entries()[index].as_str();
            HistoryEntry {
                index,
                line,
                at: history.times()[index],
                marks: input_history::word_spans(line, needle),
            }
        })
        .collect();
    palette.history_rows = paint_history_rows(
        &entries,
        place,
        now,
        body,
        &palette.availability,
        theme,
        buf,
        scene,
    );

    let stage = palette_footer_parts(KeyStage::History { needle }, theme);
    let hints: Vec<(&str, &str)> = stage
        .hints
        .iter()
        .map(|(key, word)| (*key, word.as_ref()))
        .collect();
    let parts = Footer {
        hints: &hints,
        note: &stage.note,
        ..Footer::default()
    };
    crate::render::picker::modal_footer(buf, layout.footer, &parts, plate, theme, scene);
}

/// Paint History's `entries`, newest first, into `area`, with the entry at
/// `selected` as the selected row.
///
/// An entry's first row holds U+21BA in `ui.text.disabled` in its second cell
/// and its line from its fifth cell. The line's head draws bold in `ui.text`,
/// and the rest draws in `ui.text.dim`, as [`history_head_chars`] splits them.
/// The chars inside the entry's marks take
/// [`Theme::list_match`](crate::theme::Theme::list_match) over their style.
/// The line's age against `now`, as [`history_age`] words it, draws in
/// `ui.text.muted` on the first row. The age ends one cell inside the right
/// edge.
///
/// The line wraps as [`pre_wrap_rows`] breaks it, in the cells from the fifth
/// cell to one blank cell before the age. Each next row starts under the head.
/// A line with no time wraps one cell inside the right edge. The window starts
/// where [`history_window`] keeps the selected entry whole.
///
/// Answers the cells of each entry on show with the index of its line.
#[allow(clippy::too_many_arguments)]
fn paint_history_rows(
    entries: &[HistoryEntry<'_>],
    selected: usize,
    now: i64,
    area: Rect,
    availability: &Availability,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) -> Vec<(Rect, usize)> {
    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let head_style = row_style.add_modifier(Modifier::BOLD);
    let dim = theme.get(crate::theme::scope::UI_TEXT_DIM);
    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let mark = theme.get(crate::theme::scope::UI_TEXT_DISABLED);
    let matched = theme.list_match();
    let keeps_columns = theme
        .try_get(crate::theme::scope::UI_LIST_SELECTED)
        .is_some();

    let laid: Vec<_> = entries
        .iter()
        .map(|entry| {
            let chars: Vec<char> = entry.line.chars().collect();
            let age = entry.at.map(|at| history_age(now, at));
            let age_cells = age.as_ref().map_or(0, |age| age.chars().count() as u16 + 1);
            let room = area
                .width
                .saturating_sub(HISTORY_LINE_COLUMN + 1 + age_cells);
            let rows = pre_wrap_rows(&chars, usize::from(room), false);
            let head = history_head_chars(entry.line, availability);
            (chars, age, rows, head)
        })
        .collect();
    let heights: Vec<usize> = laid.iter().map(|(_, _, rows, _)| rows.len()).collect();
    let start = history_window(&heights, selected, usize::from(area.height));

    let mut drawn = Vec::new();
    let mut y = area.y;
    for (place, (entry, (chars, age, rows, head))) in
        entries.iter().zip(&laid).enumerate().skip(start)
    {
        if y >= area.bottom() {
            break;
        }
        let height = (rows.len() as u16).min(area.bottom() - y);
        let rect = Rect::new(area.x, y, area.width, height);
        let is_selected = place == selected;
        let style = match is_selected {
            true => crate::render::chrome::selected_row(rect, row_style, true, theme, buf, scene),
            false => {
                for row_y in rect.top()..rect.bottom() {
                    for col in rect.left()..rect.right() {
                        buf[(col, row_y)].set_char(' ').set_style(row_style);
                    }
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

        write_str(
            buf,
            area.x + HISTORY_MARK_COLUMN,
            y,
            "\u{21ba}",
            column(mark),
        );
        for (row_y, row) in (y..y + height).zip(rows) {
            let head_end = (*head).clamp(row.start, row.end);
            let head_text: String = chars[row.start..head_end].iter().collect();
            let rest: String = chars[head_end..row.end].iter().collect();
            let x = area.x + HISTORY_LINE_COLUMN;
            write_str(buf, x, row_y, &head_text, column(head_style));
            let rest_x = x + (head_end - row.start) as u16;
            write_str(buf, rest_x, row_y, &rest, column(dim));
            paint_history_marks(buf, (x, row_y), row, &entry.marks, matched);
        }
        if let Some(age) = age {
            let age_x = area.right().saturating_sub(1 + age.chars().count() as u16);
            write_str(buf, age_x, y, age, column(muted));
        }

        drawn.push((rect, entry.index));
        y += height;
    }
    drawn
}

/// Patch `style` over each cell of a History row whose char is inside `marks`.
///
/// The row shows the chars `row` of its line from cell `x` of screen row `y`.
fn paint_history_marks(
    buf: &mut Buffer,
    (x, y): (u16, u16),
    row: &Range<usize>,
    marks: &[Range<usize>],
    style: Style,
) {
    for mark in marks {
        for index in mark.start.max(row.start)..mark.end.min(row.end) {
            buf[(x + (index - row.start) as u16, y)].set_style(style);
        }
    }
}

/// The chars at the start of History line `line` that its head takes, the
/// head that [`split_head`] finds against `availability`, else the line's
/// first word.
fn history_head_chars(line: &str, availability: &Availability) -> usize {
    let head = match split_head(line, availability) {
        Some((_, head, _)) => head,
        None => line.split(' ').next().unwrap_or(""),
    };
    head.chars().count()
}

/// The age that a History row shows for a line that ran at `at`, with the
/// clock at `now`, both in unix seconds.
///
/// A line under a minute old reads `now`. An older line reads its whole
/// minutes, hours, days, 30-day months, or 365-day years, and an age of 24 to
/// 48 hours reads `yesterday`. That day is a span of hours and no calendar
/// day, so the rule needs no time zone. A time after `now` reads as an age of
/// 0.
fn history_age(now: i64, at: i64) -> String {
    let age = (now - at).max(0);
    match age {
        0..60 => "now".to_owned(),
        60..3_600 => format!("{}m", age / 60),
        3_600..86_400 => format!("{}h", age / 3_600),
        86_400..172_800 => "yesterday".to_owned(),
        172_800..2_592_000 => format!("{}d", age / 86_400),
        2_592_000..31_536_000 => format!("{}mo", age / 2_592_000),
        _ => format!("{}y", age / 31_536_000),
    }
}

/// The first History entry on show, so that the selected entry ends on or
/// above the list's last row.
///
/// The window starts at the newest entry from which the entries through
/// `selected` fit in `rows` rows. `heights` holds the rows of each entry,
/// newest first. A selected entry taller than the list starts the window
/// itself, and a selection past the last entry counts as the last.
fn history_window(heights: &[usize], selected: usize, rows: usize) -> usize {
    let Some(last) = heights.len().checked_sub(1) else {
        return 0;
    };
    let selected = selected.min(last);
    (0..=selected)
        .find(|&start| heights[start..=selected].iter().sum::<usize>() <= rows)
        .unwrap_or(selected)
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
/// `resolved` command token draws with its ghost, and the chars inside `marks`
/// draw marked, as [`input_spans`] gives them.
///
/// See also:
/// - [`cmd_line::cmd_line`] for the row itself.
/// - [`render_palette_filter`] for the command-list body.
/// - [`render_palette_arg_picker`] for the inline-picker body.
/// - [`render_palette_free_arg`] for the free-typed argument body.
/// - [`render_palette_history`] for History.
#[allow(clippy::too_many_arguments)]
fn render_palette_prelude(
    input: &InputView,
    resolved: Option<&ResolvedCommand>,
    marks: &[Range<usize>],
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
    let (input, caret) = input_spans(&text, caret, resolved, marks, theme);
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
/// A `resolved` head draws in `ui.cmd.command`, or in `ui.cmd.action` when it
/// names no command. Its ghost follows in `ui.cmd.ghost`, and the rest of the
/// text draws in `ui.text`. A theme that leaves `ui.cmd.action` unset draws an
/// action's head as a command's. A theme that leaves `ui.cmd.command` unset
/// draws a command's head as the rest, and one that leaves `ui.cmd.ghost`
/// unset draws no ghost.
///
/// The ghost is display only, so the caret stays on the typed text. A caret
/// past the head moves right by the ghost's chars, and a caret at the head's
/// end covers the ghost's first char, as a shell draws a suggestion.
///
/// The chars inside `marks`, char ranges of `text`, take
/// [`Theme::list_match`](crate::theme::Theme::list_match) over the style they
/// draw in. So a mark wins over the head's token color. The ghost takes no
/// mark, because it is no part of the typed text.
fn input_spans(
    text: &str,
    caret: usize,
    resolved: Option<&ResolvedCommand>,
    marks: &[Range<usize>],
    theme: &crate::theme::Theme,
) -> (Vec<(String, Style)>, usize) {
    let plain = theme.get(crate::theme::scope::UI_TEXT);
    let mark = theme.list_match();
    let Some(resolved) = resolved else {
        return (marked_spans(text, 0, plain, marks, mark), caret);
    };

    let split = text
        .char_indices()
        .nth(resolved.head_chars)
        .map_or(text.len(), |(at, _)| at);
    let (head, rest) = text.split_at(split);
    let command = theme
        .try_get(crate::theme::scope::UI_CMD_COMMAND)
        .unwrap_or(plain);
    let head_style = match resolved.entry.is_command() {
        true => command,
        false => theme
            .try_get(crate::theme::scope::UI_CMD_ACTION)
            .unwrap_or(command),
    };
    let mut spans = marked_spans(head, 0, head_style, marks, mark);

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

    spans.extend(marked_spans(rest, resolved.head_chars, plain, marks, mark));
    (spans, shown)
}

/// `text` as spans in `base`, with `mark` patched over each char inside
/// `marks`, and one span for each run of chars in one style.
///
/// `marks` holds char ranges of the input that `text` is part of. `first` is
/// the index in the input of the first char of `text`.
fn marked_spans(
    text: &str,
    first: usize,
    base: Style,
    marks: &[Range<usize>],
    mark: Style,
) -> Vec<(String, Style)> {
    let mut spans: Vec<(String, Style)> = Vec::new();
    for (index, ch) in (first..).zip(text.chars()) {
        let style = match marks.iter().any(|range| range.contains(&index)) {
            true => base.patch(mark),
            false => base,
        };
        match spans.last_mut() {
            Some((run, run_style)) if *run_style == style => run.push(ch),
            _ => spans.push((ch.to_string(), style)),
        }
    }
    spans
}

/// The Cmd Line status over `palette`'s list.
///
/// At the root it counts the commands on show. Then it names the `action` row
/// and the internal actions that the scope offers behind it, as `11 commands ·
/// action for 1,204 more`. Under the `action` token it counts the actions on
/// show out of those that the scope offers, as `8 of 1,204 actions`. The
/// `action` token reads in the `ui.cmd.action` foreground.
fn filter_status(palette: &CommandPalette, theme: &crate::theme::Theme) -> Vec<(String, Style)> {
    let more = grouped(palette.action_offering);
    if palette.namespace == Namespace::Actions {
        let rest = format!(" of {more} actions");
        return count_status(palette.filtered.len(), &rest, theme);
    }

    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let token = match theme
        .try_get(crate::theme::scope::UI_CMD_ACTION)
        .and_then(|style| style.fg)
    {
        Some(fg) => muted.fg(fg),
        None => muted,
    };
    let shown = palette
        .filtered
        .iter()
        .filter(|entry| !is_action_row(entry))
        .count();
    let mut spans = count_status(shown, " commands", theme);
    spans.extend([
        (" \u{b7} ".to_owned(), muted),
        (ACTION_TOKEN.to_owned(), token),
        (format!(" for {more} more"), muted),
    ]);
    spans
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
/// the paint stores there the rows that it draws. The Cmd Line ends in
/// `status`. The rows of the `action` list that `namespace` names, and the
/// doc, name the keys of `keys` that run each action.
///
/// The doc shows from line `doc_scroll` of the selected entry's doc. The
/// paint holds `doc_scroll` where the doc's last line stands on the doc
/// region's last row, so the doc never scrolls past its end.
///
/// The key line names the keys of the list that `namespace` names, as
/// [`palette_footer_parts`] gives them under `scope`.
#[allow(clippy::too_many_arguments)]
fn render_palette_filter(
    input: &InputView,
    filtered: &[&'static RegistryEntry],
    match_indices: &[Vec<u32>],
    selected: usize,
    group: Option<&str>,
    namespace: Namespace,
    scope: PaletteScope,
    content_rows: u16,
    zoom: i8,
    header_rows: &mut u16,
    doc_scroll: &mut u16,
    resolved: Option<&ResolvedCommand>,
    status: &[(String, Style)],
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
    *header_rows = render_palette_prelude(
        input,
        resolved,
        &[],
        CmdStatus::Text(status),
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
        namespace,
        keys,
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
        let entry = filtered.get(selected).copied();
        let lines = entry
            .map(|entry| doc_lines(entry, keys, width, theme))
            .unwrap_or_default();
        let last_top = lines.len().saturating_sub(usize::from(doc.height));
        *doc_scroll = (*doc_scroll).min(last_top as u16);

        // The Source Tag ends the name line, which a scroll moves off the doc.
        let tag = entry.map(doc_tag).filter(|_| *doc_scroll == 0);
        let shown = &lines[usize::from(*doc_scroll)..];
        paint_doc_lines(buf, doc, shown, tag, theme, scene);
    }

    let key_stage = match namespace {
        Namespace::Commands => KeyStage::Commands {
            scope,
            typed: !input.text(ws).is_empty(),
            recallable: !ws.palette_history.entries().is_empty(),
        },
        Namespace::Actions => KeyStage::Actions,
    };
    let stage = palette_footer_parts(key_stage, theme);
    let hints: Vec<(&str, &str)> = stage
        .hints
        .iter()
        .map(|(key, word)| (*key, word.as_ref()))
        .collect();
    let parts = Footer {
        hints: &hints,
        note: &stage.note,
        ..Footer::default()
    };
    crate::render::picker::modal_footer(buf, layout.footer, &parts, plate, theme, scene);
}

/// The key hints and the note of the palette's key line in `stage`.
///
/// A list of commands names its run, completion, history, and scope keys,
/// and the scope hint names the scopes that `S-Tab` reaches next. The history
/// hint reads U+2191 `last command` over an empty input and U+2191 `history`
/// over typed text. It drops out while the history holds no line. The
/// note names the two namespaces, with `:action` in `ui.key_label`, so a user
/// learns where the internal actions went.
///
/// The `action` list names its run, completion, doc scroll, and back keys.
/// History names its walk, run, edit, and back keys. With a needle that holds
/// a word, it names the matches and the needle that `Esc` restores. Neither
/// has a note.
fn palette_footer_parts(stage: KeyStage<'_>, theme: &crate::theme::Theme) -> PaletteFooterParts {
    let hint = |key, word: &'static str| (key, Cow::Borrowed(word));
    match stage {
        KeyStage::Commands {
            scope,
            typed,
            recallable,
        } => {
            let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
            let key = theme.get(crate::theme::scope::UI_KEY_LABEL);
            let mut hints = vec![hint("Enter", "run"), hint("Tab", "complete")];
            if recallable {
                let word = match typed {
                    true => "history",
                    false => "last command",
                };
                hints.push(hint("\u{2191}", word));
            }
            hints.push(hint("S-Tab", scope.next_names()));
            PaletteFooterParts {
                hints,
                note: vec![
                    ("root = commands \u{b7} ", muted),
                    (":action", key),
                    (" = internals", muted),
                ],
            }
        },
        KeyStage::Actions => PaletteFooterParts {
            hints: vec![
                hint("Enter", "run"),
                hint("Tab", "complete"),
                hint("C-d", "doc"),
                hint("Esc", "back"),
            ],
            note: Vec::new(),
        },
        KeyStage::History { needle } => {
            let hints = match needle.trim() {
                "" => vec![
                    hint("\u{2191}\u{2193}", "older / newer"),
                    hint("Enter", "run"),
                    hint("type", "edit it"),
                    hint("Esc", "back to commands"),
                ],
                needle => vec![
                    hint("\u{2191}\u{2193}", "older / newer match"),
                    hint("Enter", "run"),
                    ("Esc", Cow::Owned(format!("back to \"{needle}\""))),
                ],
            };
            PaletteFooterParts {
                hints,
                note: Vec::new(),
            }
        },
    }
}

/// Paint `lines` into the doc region `doc`, one per row from its top, each
/// line's spans one after another from the region's second cell, clipped at
/// its right edge.
///
/// The first line ends in `tag` one cell after its text, as [`paint_doc_tag`]
/// places it. The tag is a Source Tag's label and color scope, as
/// [`doc_tag`] gives them.
fn paint_doc_lines(
    buf: &mut Buffer,
    doc: Rect,
    lines: &[Vec<(String, Style)>],
    tag: Option<(&str, &str)>,
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

        if let Some((label, scope)) = tag.filter(|_| row == doc.y) {
            paint_doc_tag(
                buf,
                (x.saturating_add(1), row, end),
                label,
                scope,
                theme,
                scene,
            );
        }
    }
}

/// Paint the Source Tag `label` in the colors of `scope` on a doc region's
/// first line, from cell `x` of `row`, as [`paint_source_tag`] draws it.
///
/// A theme that leaves `scope` unset draws no tag. A tag that does not end one
/// cell inside `end`, the region's right edge, does not draw.
fn paint_doc_tag(
    buf: &mut Buffer,
    (x, row, end): (u16, u16, u16),
    label: &str,
    scope: &str,
    theme: &crate::theme::Theme,
    scene: &mut stoat_widgets::ApcScene,
) {
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
/// The first line names a command in bold and its aliases after a blank
/// cell, and [`paint_doc_lines`] ends it in the Source Tag of [`doc_tag`].
/// The [`usage_line`] follows, then the long description wrapped to `width`,
/// and last the action the command runs with the keys of `keys` that reach
/// it. The `action` row runs no action of its own, so its doc ends in the
/// description.
///
/// An internal action's first line reads its PascalCase name in
/// `ui.cmd.action`, the name that key bindings and config files use. Its
/// last line reads `bound to` and its keys. The doc of an internal action
/// that no key reaches ends in the description.
///
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
    let internal = !entry.is_command() && !is_action_row(entry);

    let name = match internal {
        true => vec![(entry.def.name().to_owned(), action_name_style(theme))],
        false => {
            let mut name = vec![(
                entry.command_name.clone(),
                text.add_modifier(Modifier::BOLD),
            )];
            let aliases = entry.def.aliases();
            if !aliases.is_empty() {
                name.push((format!(" {}", aliases.join(", ")), muted));
            }
            name
        },
    };

    let mut lines = vec![name, vec![(usage_line(entry), text)]];
    lines.extend(
        wrap_text(entry.def.long_desc(), width)
            .into_iter()
            .map(|line| vec![(line, dim)]),
    );
    if is_action_row(entry) {
        return lines;
    }

    let action = entry.def.name();
    let bound = keys.get(action).filter(|bound| !bound.is_empty());
    if internal {
        if let Some(bound) = bound {
            lines.push(vec![
                ("bound to ".to_owned(), muted),
                (bound.join(", "), key_label),
            ]);
        }
        return lines;
    }

    let mut runs = vec![
        ("runs action ".to_owned(), muted),
        (action.to_owned(), key_label),
    ];
    if let Some(bound) = bound {
        runs.push((" \u{b7} bound to ".to_owned(), muted));
        runs.push((bound.join(", "), key_label));
    }
    lines.push(runs);
    lines
}

/// The text that names `entry` in the palette's input, as [`typed_head`]
/// gives it, followed by its [`signature`].
///
/// An internal action reads under the `action` token, as in `action undo`,
/// because only that form runs it.
fn usage_line(entry: &RegistryEntry) -> String {
    let head = typed_head(entry);
    let signature = signature(entry);
    match signature.is_empty() {
        true => head,
        false => format!("{head} {signature}"),
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
/// The name of an entry that is no command, an internal action or the
/// `action` row, draws in `ui.cmd.action`. Under a live scene a 1 px rule in
/// `ui.divider` sets the `action` row off from the commands above it. No rule
/// draws when the row leads the whole list.
///
/// Under [`Namespace::Actions`] in `namespace`, a row reads the action's
/// PascalCase name, its kebab name, its keys in `keys`, and its short
/// description instead. [`paint_action_columns`] paints those columns, and
/// the row draws no Source Tag.
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
    namespace: Namespace,
    keys: &ActionKeys,
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
    let action_cols = (namespace == Namespace::Actions).then(|| {
        (
            widest(|entry, _| entry.def.name(), ACTION_NAME_COLUMNS),
            widest(shown_name, ACTION_NAME_COLUMNS),
        )
    });

    let action_style = action_name_style(theme);
    let divider = style_rgb(theme.get(crate::theme::scope::UI_DIVIDER).fg).filter(|_| scene.live());
    let tag_of = |entry: &RegistryEntry| {
        let (label, scope) = source_tag(entry)?;
        let style = theme.try_get(scope)?;
        let start = area
            .width
            .saturating_sub(1)
            .saturating_mul(16)
            .saturating_sub(source_tag_width(label, theme));
        Some((label, start, style))
    };
    let end_x = area.x + area.width;
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
        if is_action_row(entry)
            && abs > 0
            && let Some(color) = divider
        {
            let rule = Bar {
                x: 2,
                y: 0,
                width: area.width.saturating_mul(16).saturating_sub(4),
                height: 1,
                color,
                radius: 0,
                corners: 0,
            };
            StatefulWidget::render(rule, row, buf, scene);
        }

        let indices = match_indices.get(abs).unwrap_or(&empty_indices);
        if let Some(cols) = action_cols {
            paint_action_columns(buf, row, entry, indices, cols, keys, theme, column);
            continue;
        }

        let name = shown_name(entry, name_offset);
        let name_x = area.x + 1;
        let own_name = match entry.is_command() {
            true => name_style,
            false => action_style,
        };
        write_str_clipped(buf, name_x, row.y, name, column(own_name), end_x);
        let at = (name_x, row.y, end_x);
        paint_matches(buf, at, name, name_offset, indices, match_style);

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

        let tag = tag_of(entry);
        let desc_end = match tag {
            Some((_, start, _)) => area.x + (start / 16).saturating_sub(1),
            None => end_x.saturating_sub(1),
        };
        let desc_x = signature_x.saturating_add(SIGNATURE_COLUMNS + 1);
        let desc = entry.def.short_desc();
        write_str_clipped(buf, desc_x, row.y, desc, column(dim), desc_end);

        if let Some((label, start, style)) = tag {
            paint_source_tag(buf, row, start, label, style, theme, scene);
        }
    }
}

/// Paint the columns of internal action `entry`'s row in the `action` list.
///
/// The row reads the PascalCase name in `ui.cmd.action` and the kebab name in
/// `ui.text.muted`. The labels of the keys that `keys` binds to the action
/// follow in `ui.key_label`, and the short description ends the row in
/// `ui.text.dim`. The columns stand one cell apart.
///
/// `cols` holds the cells of the two name columns, which grow to the longest
/// name on show, as the root's name column grows. The keys clip at
/// [`ACTION_KEY_COLUMNS`], because the doc region lists them whole.
///
/// A position in `indices` indexes the kebab name, which the query ranks.
/// [`pascal_matches`] marks the same letters of the PascalCase name.
#[allow(clippy::too_many_arguments)]
fn paint_action_columns(
    buf: &mut Buffer,
    row: Rect,
    entry: &RegistryEntry,
    indices: &[u32],
    (pascal_cols, kebab_cols): (u16, u16),
    keys: &ActionKeys,
    theme: &crate::theme::Theme,
    column: impl Fn(Style) -> Style,
) {
    let end_x = row.right();
    let matched = theme.list_match();

    let pascal = entry.def.name();
    let pascal_x = row.x + 1;
    write_str_clipped(
        buf,
        pascal_x,
        row.y,
        pascal,
        column(action_name_style(theme)),
        end_x,
    );
    let pascal_indices = pascal_matches(&entry.command_name, indices);
    paint_matches(
        buf,
        (pascal_x, row.y, end_x),
        pascal,
        0,
        &pascal_indices,
        matched,
    );

    let kebab = entry.command_name.as_str();
    let kebab_x = pascal_x.saturating_add(pascal_cols + 1);
    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    write_str_clipped(buf, kebab_x, row.y, kebab, column(muted), end_x);
    paint_matches(buf, (kebab_x, row.y, end_x), kebab, 0, indices, matched);

    let keys_x = kebab_x.saturating_add(kebab_cols + 1);
    let keys_end = keys_x.saturating_add(ACTION_KEY_COLUMNS).min(end_x);
    let labels = keys
        .get(pascal)
        .map(|labels| labels.join(" \u{b7} "))
        .unwrap_or_default();
    let key_label = theme.get(crate::theme::scope::UI_KEY_LABEL);
    write_str_clipped(buf, keys_x, row.y, &labels, column(key_label), keys_end);

    let desc_x = keys_x.saturating_add(ACTION_KEY_COLUMNS + 1);
    let dim = theme.get(crate::theme::scope::UI_TEXT_DIM);
    let desc = entry.def.short_desc();
    write_str_clipped(
        buf,
        desc_x,
        row.y,
        desc,
        column(dim),
        end_x.saturating_sub(1),
    );
}

/// Paint `style` over each cell of `text` whose char sits at a position in
/// `indices`, where `text` reads from cell `x` of row `y`.
///
/// A row leaves out a name's first `offset` chars, and `indices` index the
/// whole name, so the text's char `i` sits at `offset + i`. No cell at or
/// past `end_x` takes the style.
fn paint_matches(
    buf: &mut Buffer,
    (x, y, end_x): (u16, u16, u16),
    text: &str,
    offset: usize,
    indices: &[u32],
    style: Style,
) {
    for col in 0..text.chars().count() {
        let cell = x + col as u16;
        if cell >= end_x {
            break;
        }
        if indices.binary_search(&((offset + col) as u32)).is_ok() {
            buf[(cell, y)].set_style(style);
        }
    }
}

/// The positions in an internal action's PascalCase name of the letters at
/// `indices` in its kebab name `kebab`.
///
/// The kebab name is the PascalCase name with a dash before each inner
/// capital. So a letter's PascalCase position is its kebab position less the
/// dashes before it. A matched dash has no letter to mark.
fn pascal_matches(kebab: &str, indices: &[u32]) -> Vec<u32> {
    let mut dashes = 0;
    kebab
        .chars()
        .zip(0u32..)
        .filter_map(|(ch, index)| {
            if ch == '-' {
                dashes += 1;
                return None;
            }
            indices
                .binary_search(&index)
                .is_ok()
                .then_some(index - dashes)
        })
        .collect()
}

/// The style of an internal action's name, `ui.cmd.action`, or a command's
/// bold name for a theme that leaves that scope unset.
fn action_name_style(theme: &crate::theme::Theme) -> Style {
    theme
        .try_get(crate::theme::scope::UI_CMD_ACTION)
        .unwrap_or_else(|| {
            theme
                .get(crate::theme::scope::UI_TEXT)
                .add_modifier(Modifier::BOLD)
        })
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

/// The label and the color scope of the Source Tag that ends `entry`'s row, or
/// `None` for an internal action, whose list names no source.
///
/// The `action` row reads `internal`. A command reads `built-in`, because every
/// command in the registry ships with stoat.
fn source_tag(entry: &RegistryEntry) -> Option<(&'static str, &'static str)> {
    if is_action_row(entry) {
        return Some(INTERNAL_TAG);
    }
    entry
        .is_command()
        .then_some(("built-in", crate::theme::scope::UI_CMD_SOURCE_BUILTIN))
}

/// The label and the color scope of the Source Tag that ends the first line
/// of `entry`'s doc.
///
/// The doc names the one action that the selected row names. So an internal
/// action's doc reads `internal`, although its row in the `action` list
/// draws no tag.
fn doc_tag(entry: &RegistryEntry) -> (&'static str, &'static str) {
    source_tag(entry).unwrap_or(INTERNAL_TAG)
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
        count_status, doc_lines, grouped, history_age, history_window, input_spans,
        paint_doc_lines, paint_history_rows, paint_palette_rows, palette_filter_layout,
        pascal_matches, render_command_palette, source_tag, usage_line, wrap_text, HistoryEntry,
        DOC_ROWS, ISLAND_BOTTOM, ISLAND_GAP, ISLAND_SIDE, ISLAND_TOP, LIST_ROWS,
    };
    use crate::{
        command_palette::{action_row, Availability, Namespace, ResolvedCommand},
        input_history,
        keymap::ActionKeys,
        render::editor::ResolvedChrome,
        test_fixture::{plate_islands, row_rules},
        theme::Theme,
    };
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Modifier, Style},
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
            entry: registry::lookup_alias("open").expect("a command"),
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
                input_spans("o src", 5, Some(&open), &[], &theme),
                input_spans("o", 1, Some(&open), &[], &theme),
                input_spans("o src", 5, Some(&open), &[], &Theme::empty()),
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
            Namespace::Commands,
            &ActionKeys::new(),
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
            Namespace::Commands,
            &ActionKeys::new(),
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
        let mut row = vec![' '; width];
        for &(at, text) in parts {
            for (cell, ch) in row[at..].iter_mut().zip(text.chars()) {
                *cell = ch;
            }
        }
        row.into_iter().collect()
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
            Namespace::Commands,
            &ActionKeys::new(),
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

    /// A 1 px rule in `ui.divider` runs along the top of the `action` row, 1 px
    /// inside the list on each side. The rule draws only where a row stands
    /// above the `action` row.
    #[test]
    fn a_rule_sets_the_action_row_off_from_the_commands() {
        let theme = {
            let src = r##"theme t { ui.divider.fg = "#070707"; }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let rules = |entries: &[&'static registry::RegistryEntry]| {
            let list = Rect::new(0, 0, 40, entries.len() as u16);
            let mut buf = Buffer::empty(list);
            let mut scene = stoat_widgets::ApcScene::new();
            let indices = vec![Vec::new(); entries.len()];
            paint_palette_rows(
                entries,
                &indices,
                usize::MAX,
                0,
                Namespace::Commands,
                &ActionKeys::new(),
                list,
                0,
                &theme,
                false,
                &mut buf,
                &mut scene,
            );
            decode_stream(scene.bytes())
                .into_iter()
                .filter_map(|command| match command {
                    Command::Bar(bar) => Some(bar),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let command = |name| registry::lookup_alias(name).expect("a command");
        let rows = [command("open"), command("quit"), action_row()];

        assert_eq!(
            (rules(&rows), rules(&[action_row()])),
            (
                vec![BarCommand {
                    x: 2,
                    y: 32,
                    width: 40 * 16 - 4,
                    height: 1,
                    color: [7, 7, 7],
                    occludes: false,
                    hatch: None,
                    radius: 0,
                    corners: 0,
                }],
                Vec::new(),
            ),
        );
    }

    /// The `action` row runs no action of its own, so its doc ends in its
    /// description with no `runs action` line.
    #[test]
    fn the_action_row_doc_ends_in_its_description() {
        let lines = doc_lines(action_row(), &ActionKeys::new(), 400, &Theme::empty());
        let texts: Vec<Vec<String>> = lines
            .into_iter()
            .map(|line| line.into_iter().map(|(text, _)| text).collect())
            .collect();
        assert_eq!(
            texts,
            [
                vec!["action".to_owned()],
                vec!["action <action> [args]".to_owned()],
                vec![action_row().def.long_desc().to_owned()],
            ],
        );
    }

    /// A row of the `action` list reads the PascalCase name, the kebab name,
    /// the keys joined by U+00B7, and the description. Each column starts one
    /// cell past the 22, 22, or 18 cells of the column before it.
    #[test]
    fn an_action_row_reads_both_names_its_keys_and_its_description() {
        let labels = ["Spc a h", "Spc a Lt"].map(String::from).to_vec();
        let keys = ActionKeys::from([("FocusLeft".to_owned(), labels)]);
        let buf = painted_action_rows(&["FocusLeft"], &keys);

        assert_eq!(
            row_text(&buf, 0),
            placed(
                100,
                &[
                    (1, "FocusLeft"),
                    (24, "focus-left"),
                    (47, "Spc a h \u{b7} Spc a Lt"),
                    (66, "focus pane left"),
                ],
            ),
        );
    }

    /// Both name columns of the `action` list grow to the longest name on
    /// show. The keys clip at their 18 cells, and a description stops one cell
    /// inside the row's right edge.
    #[test]
    fn the_action_name_columns_grow_and_the_keys_clip() {
        let labels = ["Spc a h", "Spc a Lt", "C-w h", "C-w Lt", "A-h"].map(String::from);
        let keys = ActionKeys::from([("FocusLeft".to_owned(), labels.to_vec())]);
        let buf = painted_action_rows(&["FocusLeft", "MergeConsecutiveSelections"], &keys);

        assert_eq!(
            [row_text(&buf, 0), row_text(&buf, 1)],
            [
                placed(
                    100,
                    &[
                        (1, "FocusLeft"),
                        (28, "focus-left"),
                        (57, "Spc a h \u{b7} Spc a Lt"),
                        (76, "focus pane left"),
                    ],
                ),
                placed(
                    100,
                    &[
                        (1, "MergeConsecutiveSelections"),
                        (28, "merge-consecutive-selections"),
                        (76, "merge consecutive selec"),
                    ],
                ),
            ],
        );
    }

    /// Paint the `action` list rows of the actions named `names` with the keys
    /// of `keys` into a list 100 cells wide, with no row selected.
    fn painted_action_rows(names: &[&str], keys: &ActionKeys) -> Buffer {
        let entries: Vec<&'static registry::RegistryEntry> = names
            .iter()
            .map(|name| registry::lookup(name).expect("a registered action"))
            .collect();
        let list = Rect::new(0, 0, 100, entries.len() as u16);
        let mut buf = Buffer::empty(list);
        paint_palette_rows(
            &entries,
            &vec![Vec::new(); entries.len()],
            usize::MAX,
            0,
            Namespace::Actions,
            keys,
            list,
            0,
            &Theme::empty(),
            false,
            &mut buf,
            &mut stoat_widgets::ApcScene::new(),
        );
        buf
    }

    /// A matched letter of the kebab name marks the same letter of the
    /// PascalCase name, back by the dashes before it. A matched dash marks
    /// nothing there.
    #[test]
    fn a_match_in_the_kebab_name_marks_the_same_letters_of_the_pascal_name() {
        assert_eq!(
            (
                pascal_matches("open-last-picker", &[10, 11, 12, 13]),
                pascal_matches("open-last-picker", &[3, 4, 5]),
            ),
            (vec![8, 9, 10, 11], vec![3, 4]),
        );
    }

    /// An internal action's doc reads its PascalCase name in `ui.cmd.action`,
    /// its usage under the `action` token, its long description, and the keys
    /// bound to it. The doc of an action that no key reaches ends in the
    /// description.
    #[test]
    fn the_doc_of_an_internal_action_names_it_and_its_keys() {
        let theme = {
            let src = r##"theme t {
                ui.cmd.action.fg = "#010101";
                ui.text.muted.fg = "#020202";
                ui.key_label.fg = "#030303";
                ui.text.dim.fg = "#040404";
            }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let fg = |gray| Style::default().fg(Color::Rgb(gray, gray, gray));
        let entry = registry::lookup("Undo").expect("Undo is registered");
        let bound = ActionKeys::from([("Undo".to_owned(), vec!["u".to_owned(), "C-z".to_owned()])]);

        let usage = theme.get(crate::theme::scope::UI_TEXT);
        let mut described = vec![
            vec![("Undo".to_owned(), fg(1))],
            vec![("action undo".to_owned(), usage)],
        ];
        described.extend(
            wrap_text(entry.def.long_desc(), 60)
                .into_iter()
                .map(|line| vec![(line, fg(4))]),
        );
        let mut keyed = described.clone();
        keyed.push(vec![
            ("bound to ".to_owned(), fg(2)),
            ("u, C-z".to_owned(), fg(3)),
        ]);

        assert_eq!(
            (
                doc_lines(entry, &bound, 60, &theme),
                doc_lines(entry, &ActionKeys::new(), 60, &theme),
            ),
            (keyed, described),
        );
    }

    /// A History age reads `now` under a minute, then whole minutes, hours,
    /// `yesterday`, days, months, and years. A time after the clock reads
    /// `now`.
    #[test]
    fn history_ages_fall_into_their_buckets() {
        let now = 1_000_000_000;
        let ages = [
            0, 59, 60, 3_599, 3_600, 86_399, 86_400, 172_799, 172_800, 2_591_999, 2_592_000,
            31_536_000, -5,
        ]
        .map(|age| history_age(now, now - age));
        assert_eq!(
            ages,
            [
                "now",
                "now",
                "1m",
                "59m",
                "1h",
                "23h",
                "yesterday",
                "yesterday",
                "2d",
                "29d",
                "1mo",
                "1y",
                "now"
            ],
        );
    }

    /// A History row holds U+21BA in its second cell and its line from its
    /// fifth, with the head bold and the rest after it. Its age ends one cell
    /// inside the right edge.
    #[test]
    fn a_history_row_draws_its_mark_its_head_and_its_age() {
        let entries = [HistoryEntry {
            index: 0,
            line: "cd /alpha",
            at: Some(0),
            marks: Vec::new(),
        }];
        let (buf, _) = painted_history(&entries, &Theme::empty());
        let bold: Vec<bool> = (4..7)
            .map(|x| buf[(x, 0)].modifier.contains(Modifier::BOLD))
            .collect();
        assert_eq!(
            (row_text(&buf, 0), bold),
            (
                placed(40, &[(1, "\u{21ba}"), (4, "cd /alpha"), (37, "2m")]),
                vec![true, true, false],
            ),
        );
    }

    /// A History line wraps after the last space that fits before its age, and
    /// each next row starts under its head. A word longer than the room breaks
    /// at the room's last cell. The row map holds the cells of each entry with
    /// the index of its line.
    #[test]
    fn a_long_history_line_wraps_under_its_head() {
        let long = format!("run {} {}", "a".repeat(20), "b".repeat(20));
        let entries = [
            HistoryEntry {
                index: 7,
                line: &long,
                at: Some(0),
                marks: Vec::new(),
            },
            HistoryEntry {
                index: 3,
                line: "cd /alpha",
                at: Some(0),
                marks: Vec::new(),
            },
        ];
        let (buf, rows) = painted_history(&entries, &Theme::empty());
        let word = "x".repeat(40);
        let (word_buf, _) = painted_history(
            &[HistoryEntry {
                index: 0,
                line: &word,
                at: Some(0),
                marks: Vec::new(),
            }],
            &Theme::empty(),
        );

        let first = format!("run {} ", "a".repeat(20));
        assert_eq!(
            (
                [row_text(&buf, 0), row_text(&buf, 1)],
                rows,
                [row_text(&word_buf, 0), row_text(&word_buf, 1)],
            ),
            (
                [
                    placed(40, &[(1, "\u{21ba}"), (4, &first), (37, "2m")]),
                    placed(40, &[(4, &"b".repeat(20))]),
                ],
                vec![(Rect::new(0, 0, 40, 2), 7), (Rect::new(0, 2, 40, 1), 3)],
                [
                    placed(40, &[(1, "\u{21ba}"), (4, &"x".repeat(32)), (37, "2m")]),
                    placed(40, &[(4, &"x".repeat(8))]),
                ],
            ),
        );
    }

    /// The History window starts at the newest entry that lets the selected
    /// entry end on or above the list's last row.
    #[test]
    fn the_history_window_keeps_the_selected_entry_whole() {
        let heights = [1, 2, 1, 3];
        assert_eq!(
            [0, 2, 3].map(|selected| history_window(&heights, selected, 4)),
            [0, 0, 2],
        );
    }

    /// A History row gives `ui.list.match` to the chars of each word of the
    /// needle at the word's first place in the line. No other cell takes it.
    #[test]
    fn a_history_row_marks_the_matched_words() {
        let line = "git log --graph stoat/src";
        let entries = [HistoryEntry {
            index: 0,
            line,
            at: None,
            marks: input_history::word_spans(line, "stoat git"),
        }];
        let (buf, _) = painted_history(&entries, &match_theme());
        assert_eq!(marked_cells(&buf, 0), [4, 5, 6, 20, 21, 22, 23, 24]);
    }

    /// The recalled input gives `ui.list.match` to the chars that the needle's
    /// words mark, and plain text to the runs between them. A mark wins over
    /// the head's command color, and the ghost takes no mark.
    #[test]
    fn the_recalled_input_marks_the_matched_words() {
        let line = "git log --graph stoat/src";
        let typed = "o src/main";
        let open = ResolvedCommand {
            entry: registry::lookup_alias("open").expect("a command"),
            head_chars: 1,
            ghost: "pen".to_owned(),
        };
        let marked = |text: &str, needle: &str, resolved: Option<&ResolvedCommand>| {
            let marks = input_history::word_spans(text, needle);
            input_spans(text, 0, resolved, &marks, &match_theme()).0
        };
        let spans = |parts: &[(&str, u8)]| -> Vec<(String, Style)> {
            parts
                .iter()
                .map(|&(text, gray)| {
                    let style = Style::default().fg(Color::Rgb(gray, gray, gray));
                    (text.to_owned(), style)
                })
                .collect()
        };

        assert_eq!(
            [
                marked(line, "stoat git", None),
                marked(typed, "main o", Some(&open)),
            ],
            [
                spans(&[("git", 7), (" log --graph ", 4), ("stoat", 7), ("/src", 4)]),
                spans(&[("o", 7), ("pen", 6), (" src/", 4), ("main", 7)]),
            ],
        );
    }

    /// A mark that crosses a wrap marks its chars on each row that it reaches.
    #[test]
    fn a_mark_across_a_wrap_marks_each_row() {
        let line = "x".repeat(40);
        let entries = [HistoryEntry {
            index: 0,
            line: &line,
            at: Some(0),
            marks: input_history::word_spans(&line, &"x".repeat(34)),
        }];
        let (buf, _) = painted_history(&entries, &match_theme());
        assert_eq!(
            (marked_cells(&buf, 0), marked_cells(&buf, 1)),
            ((4..36).collect(), vec![4, 5]),
        );
    }

    /// The cells of row `y` of `buf` that draw in the `ui.list.match` of
    /// [`match_theme`].
    fn marked_cells(buf: &Buffer, y: u16) -> Vec<u16> {
        (buf.area.left()..buf.area.right())
            .filter(|&x| buf[(x, y)].fg == Color::Rgb(7, 7, 7))
            .collect()
    }

    /// A theme whose `ui.text`, `ui.cmd.command`, `ui.cmd.ghost`, and
    /// `ui.list.match` read `#040404`, `#050505`, `#060606`, and `#070707`.
    fn match_theme() -> Theme {
        let src = r##"theme t {
            ui.text.fg = "#040404";
            ui.cmd.command.fg = "#050505";
            ui.cmd.ghost.fg = "#060606";
            ui.list.match.fg = "#070707";
        }"##;
        let (config, _) = stoat_config::parse(src);
        Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
    }

    /// Paint History `entries` in `theme` into a list 40 cells wide and 4 rows
    /// tall, two minutes after the time 0, with no entry selected.
    fn painted_history(
        entries: &[HistoryEntry<'_>],
        theme: &Theme,
    ) -> (Buffer, Vec<(Rect, usize)>) {
        let list = Rect::new(0, 0, 40, 4);
        let mut buf = Buffer::empty(list);
        let rows = paint_history_rows(
            entries,
            usize::MAX,
            125,
            list,
            &Availability::default(),
            theme,
            &mut buf,
            &mut stoat_widgets::ApcScene::new(),
        );
        (buf, rows)
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
            None,
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
            paint_doc_lines(&mut buf, doc, &lines, source_tag(entry), &theme, &mut scene);
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
            0,
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

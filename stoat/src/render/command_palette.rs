use crate::{
    command_palette::{
        is_action_row, split_head, typed_head, ArgList, ArgPicker, Availability, CliListKind,
        CommandPalette, HistorySpans, Namespace, PaletteCommand, PaletteScope, PickLoad, PickState,
        ResolvedCommand, ACTION_TOKEN,
    },
    file_finder::FinderScope,
    host::FsHost,
    input_history,
    input_view::InputView,
    keymap::ActionKeys,
    markdown::StyledLine,
    render::{
        chrome::{
            IslandFit, PlateColors, ISLAND_BOTTOM, ISLAND_GAP, ISLAND_SIDE, ISLAND_TOP,
            PLATE_EDGES, SELECTED_ROW_RADIUS,
        },
        cmd_line::{self, CmdLine, CmdStatus},
        paint::style_rgb,
        picker::{
            Footer, ListWindow, PathRows, RowMarks, RowStart, FOOTER_ROWS, PREVIEW_TEXT_COLUMNS,
        },
        pixel_rows::{self, PixelBar, Placer, RowMetrics, ROW_PAD},
        run_pane,
        text::{clip_to_width, pre_wrap_rows, wrap_text, write_str, write_str_clipped},
        TypeScale,
    },
    run::{cargo::CmdReport, vterm::BodyRow, Cmd},
    theme::{TypeRole, FULL_CELL_SCALE},
    user_command::{
        pick,
        run::{self, CliSpecState, LoadedCli},
        CommandSet, RunSpec, UserCommand,
    },
    workspace::Workspace,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, Borders, StatefulWidget},
};
use std::{borrow::Cow, collections::HashMap, ops::Range, path::Path};
use stoat_action::ValueSource;
use stoat_widgets::{
    bar::Bar,
    text_run::{self, TextRun},
};
use stoatty_protocol::command::Edge;

/// The fewest columns of a Run Block's plate, the boards' small plate, whose
/// 71 inner cells draw the board's 664 px.
const RUN_BLOCK_COLS: u16 = 73;

/// The most body rows that a Run Block shows, the newest rows of its Cmd's
/// output.
const RUN_BLOCK_ROWS: u16 = 16;

const LIST_ROWS: u16 = 10;
const DOC_ROWS: u16 = 6;

/// The cell rows of the palette's doc when it lays its lines out by pixels.
///
/// The doc's Island stands 2 px over its first row, as [`PIXEL_DOC_EDGE`]
/// sets it, and 4 px under its last, so 8 rows of 18 px make the board's
/// 150 px island. The box keeps the height of [`DOC_ROWS`], so the list gives
/// the doc its 2 rows.
const PIXEL_DOC_ROWS: u16 = 8;

/// The bottom edge of the list's Island when the doc lays its lines out by
/// pixels, 8 px under the list's last row.
///
/// With the doc's [`PIXEL_DOC_EDGE`] it leaves the board's 8 px between the
/// two Islands across the 18 px row between their cells.
const PIXEL_LIST_EDGE: Edge = Edge::Outset(8);

/// The top edge of a pixel doc's Island, 2 px over the doc's first row.
const PIXEL_DOC_EDGE: Edge = Edge::Outset(2);

/// The logical px from the top of a pixel doc's first row to its first line,
/// the board's 8 px of padding less the 2 px that [`PIXEL_DOC_EDGE`] stands
/// over the row.
const PIXEL_DOC_TOP: u16 = 6;

/// The logical px over the bottom of a pixel doc's last row that its lines
/// end by, the board's 8 px of padding less the 4 px that its Island stands
/// under that row.
const PIXEL_DOC_BOTTOM: u16 = 4;

/// The logical px between two items of a pixel doc, the board's `gap: 4px`.
const PIXEL_DOC_GAP: u16 = 4;

/// The logical px between the parts of a pixel doc's first line, the board's
/// `gap: 10px`.
const PIXEL_DOC_PART_GAP: u16 = 10;

/// Sixteenths from a pixel doc's left edge to its text, and from its text's
/// room to its right edge. That is the board's 12 px of padding less the 3 px
/// that the Island reaches past the doc's cells, one cell at a 9 px cell.
const PIXEL_DOC_COLUMN: u16 = 16;

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

/// The cells of a flag row's label column, the CmdRg board's `width: 26ch`.
/// On cell rows a longer label moves its own row's value and description
/// right, because no cell row truncates. A pixel row cuts the label at this
/// width, so its value and description keep their columns.
const FLAG_LABEL_COLUMNS: u16 = 26;

/// The cells of a flag row's value column, the CmdRg board's `width: 9ch`,
/// which holds the value name in angle brackets.
const FLAG_VALUE_COLUMNS: u16 = 9;

/// The cells that a subcommand row's name column takes at least, the
/// CmdRunStages board's `width: 8ch`. The column grows to the longest name on
/// show.
const SUBCOMMAND_COLUMNS: u16 = 8;

/// The sixteenths between a Source Tag's box and its label on each side, the
/// board's `padding: 0 5px` at a 9 px cell.
const SOURCE_TAG_INSET: u16 = 9;

/// The height of a Source Tag's box, in sixteenths of a row, one in from the
/// row's top. It is the whole sixteenth nearest the board's
/// `line-height: 16px` in an 18 px row.
const SOURCE_TAG_HEIGHT: u16 = 14;

/// The radius a Source Tag's box rounds its corners at, in logical pixels,
/// the board's `border-radius: 4px`.
const SOURCE_TAG_RADIUS: u8 = 4;

/// The logical px of space over the `action` row's rule, the CmdRoot board's
/// `margin-top: 4px`.
const ACTION_ROW_MARGIN: u16 = 4;

/// The logical px of the rule over the `action` row, the board's
/// `border-top: 1px`.
const ACTION_ROW_RULE: u16 = 1;

/// The logical px between the `action` row's rule and its box, the board's
/// `padding-top: 2px`.
const ACTION_ROW_PAD: u16 = 2;

/// The logical px that the box of a pixel row stands inside each end of the
/// list. The command list has no preview beside it, so both ends match.
const PIXEL_BOX_INSET: u16 = 1;

/// The logical px that the box of a pixel row stands inside the right end of
/// a list beside a preview, as the finder's list sets its box by the
/// preview's Island.
const PIXEL_PREVIEW_BOX_INSET: u16 = 3;

/// The logical px, across and down, that a pick list's preview stands inside
/// its Island, the CmdPr board's `padding: 8px 12px`.
const PICK_PREVIEW_PAD: [u8; 2] = [12, 8];

/// The logical px that a pixel row's last part ends inside its box, the
/// CmdRoot board's `padding: 0 8px`.
const PIXEL_ROW_PAD: u16 = 8;

/// Sixteenths from the list's left edge to a pixel row's first column, the
/// [`PIXEL_BOX_INSET`] and the [`PIXEL_ROW_PAD`] at a 9 px cell.
const PIXEL_FIRST_COLUMN: u16 = 16;

/// Sixteenths between two columns of a pixel row, the boards' 8 px gap at a
/// 9 px cell.
const PIXEL_COLUMN_GAP: u16 = 14;

/// The logical px that a pixel row's Source Tag stands under the top of the
/// row's box, which centers the board's 16 px chip in the 20 px box.
const PIXEL_TAG_DROP: u16 = 2;

/// The logical px of a pixel row's Source Tag, the board's `line-height: 16px`.
const PIXEL_TAG_HEIGHT: u16 = 16;

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

/// The key line of a file argument's list, each a bound key and the word for
/// what it does.
///
/// A file argument opens the file, as the File Finder does, so Enter reads
/// `open`. Every other argument list reads [`PALETTE_HINTS`].
const ARG_FILE_HINTS: [(&str, &str); 4] = [
    ("Enter", "open"),
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

/// Sixteenths from the list's left edge to the U+21BA mark of a pixel History
/// row, the 1 px that the row's box stands inside the list and the board's
/// 8 px of padding at a 9 px cell.
const HISTORY_PIXEL_MARK: u16 = 16;

/// Sixteenths from the list's left edge to the line of a pixel History row.
///
/// The mark takes the board's `width: 2ch` of two cells, and the line follows
/// it [`PIXEL_COLUMN_GAP`] later, the board's `gap: 8px`. Each line of a
/// wrapped entry starts here too.
const HISTORY_PIXEL_LINE: u16 = HISTORY_PIXEL_MARK + 2 * 16 + PIXEL_COLUMN_GAP;

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

impl PaletteFilterLayout {
    /// The list rows, the doc rows, and the row between them, as one rect.
    ///
    /// History and a pick list paint their rows on one Island over this rect,
    /// and the hit test reads their rows in it.
    pub(crate) fn body(&self) -> Rect {
        Rect::new(
            self.list.x,
            self.list.y,
            self.list.width,
            self.footer.y.saturating_sub(self.list.y),
        )
    }
}

/// The on-screen rectangles of the palette's argument picker, derived from a
/// terminal `area` by [`palette_arg_layout`].
///
/// Shared by the renderer, the hit test, and the smooth-scroll emit, so the
/// pooled list region matches the painted one exactly.
pub struct PaletteArgLayout {
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
}

/// The parts of the palette's key line in one stage, as
/// [`palette_footer_parts`] gives them.
struct PaletteFooterParts {
    /// The key hints, each a bound key and the word for what it does. A word
    /// that names the walk's needle or a pick's line is owned.
    hints: Vec<(&'static str, Cow<'static, str>)>,
    /// The note's spans, empty for a stage with no note. A span that names a
    /// pick's line is owned.
    note: Vec<(Cow<'static, str>, Style)>,
}

impl PaletteFooterParts {
    /// Paint the key line into `rect`, which
    /// [`footer_rect`](crate::render::picker::footer_rect) gives, over the
    /// plate of `plate`.
    fn paint(
        &self,
        rect: Rect,
        plate: Option<PlateColors>,
        theme: &crate::theme::Theme,
        buf: &mut Buffer,
        scene: &mut stoat_widgets::ApcScene,
    ) {
        let hints: Vec<(&str, &str)> = self
            .hints
            .iter()
            .map(|(key, word)| (*key, word.as_ref()))
            .collect();
        let note: Vec<(&str, Style)> = self
            .note
            .iter()
            .map(|(text, style)| (text.as_ref(), *style))
            .collect();
        let parts = Footer {
            hints: &hints,
            note: &note,
            ..Footer::default()
        };
        crate::render::picker::modal_footer(buf, rect, &parts, plate, theme, scene);
    }
}

/// The stage of the palette whose keys its key line names.
enum KeyStage<'a> {
    /// A list of commands, the root or a group. `typed` holds while the input
    /// holds text, and `recallable` while the history holds a line.
    Commands {
        scope: PaletteScope,
        /// The scopes that `S-Tab` steps through, as [`PaletteScope::cycle`]
        /// gives them.
        cycle: &'a [PaletteScope],
        typed: bool,
        recallable: bool,
    },
    /// The `action` list.
    Actions,
    /// History, with the needle of the walk that shows it.
    History { needle: &'a str },
    /// A pick list, with the shell line that Enter runs. `line` is the line of
    /// the config that declares the command, and `tag` the label and the
    /// color scope of the command's Source Tag.
    Pick {
        enter: String,
        line: u32,
        tag: (&'static str, &'static str),
    },
    /// The list of a `cli` command's flags or subcommands, as `kind` names it.
    Cli { kind: CliListKind },
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
///
/// The doc takes `doc_rows` rows, as [`doc_rows`] answers them, and the list
/// the rest of the body. The box's height counts [`DOC_ROWS`] whatever the doc
/// takes, so a doc of more rows takes them from the list.
pub(crate) fn palette_filter_layout(
    area: Rect,
    content_rows: u16,
    zoom: i8,
    header_rows: u16,
    doc_rows: u16,
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
    let list_height = body.saturating_sub(doc_rows).max(LIST_ROWS).min(body);
    let doc_height = doc_rows.min(body.saturating_sub(list_height));

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

/// The rows of the palette's doc, [`PIXEL_DOC_ROWS`] for a doc that lays its
/// lines out by `pixels` and [`DOC_ROWS`] for one on whole cells.
pub(crate) fn doc_rows(pixels: Option<RowMetrics>) -> u16 {
    match pixels {
        Some(_) => PIXEL_DOC_ROWS,
        None => DOC_ROWS,
    }
}

/// Lay out the palette's argument picker (`:o `, `:cd `, `:b `) within `area`,
/// its plate fitted to the widest label that its list shows, or `None` when
/// `area` is too small to host it or no argument picker is open.
///
/// The plate follows the rule of the finder's plate. The list takes the columns
/// of [`CommandPalette::arg_plate_fit`], measured when the list's rows last
/// changed, so a selection move keeps the plate. With no fit the list measures
/// the rows on show. Either way it takes the widest label, at least 30 columns,
/// with its pad and mark cells. The preview takes the fit's columns of text,
/// the typical line of the files on show, or [`PREVIEW_TEXT_COLUMNS`] with no
/// fit, fitted into grid columns by `preview_scale`. The plate takes at least
/// the columns of a body that shows a preview, and stops one column inside each
/// side of `area`. `zoom` steps then move the width by a tenth of the area
/// each.
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
    preview_scale: Option<(u16, u16)>,
) -> Option<PaletteArgLayout> {
    let picker = palette.arg_picker.as_ref()?;
    let rows = arg_box(area, palette, zoom)?;
    let (list, text) = match &palette.arg_plate_fit {
        Some(fit) => (fit.list, fit.preview_text),
        None => {
            let list_rows = arg_list_rows(rows)?;
            let widest = crate::render::picker::widest_label(&arg_rows(picker, home), list_rows);
            (
                crate::render::picker::list_columns(widest),
                PREVIEW_TEXT_COLUMNS,
            )
        },
    };
    let preview = crate::render::picker::preview_columns(text, preview_scale);

    let fitted = crate::render::picker::fitted_plate_width(list, preview);
    let base = crate::render::chrome::fitted_width(area.width, fitted, 0, 0);
    let width = crate::render::chrome::fitted_width(area.width, base, 20, zoom);
    let modal = Rect {
        x: area.x + (area.width - width) / 2,
        width,
        ..rows
    };
    let inner = Block::default().borders(Borders::ALL).inner(modal);

    let (top, height) = arg_body_rows(inner, palette.header_rows)?;
    let (list, preview) =
        crate::render::picker::fitted_split(inner.x, top, inner.width, height, list, preview, None);
    Some(PaletteArgLayout {
        modal,
        inner,
        list,
        preview,
        footer: crate::render::picker::footer_rect(modal),
    })
}

/// Measure the argument picker's plate against the rows its list shows now, as
/// [`palette_arg_layout`] lays it out within `area` at `zoom`.
///
/// The fit holds while the list's rows and its window hold, and measures again
/// once either changes. A palette with no argument picker, or a box with no
/// body row, keeps the fit it holds. A measure reads, through `fs_host`, the
/// start of each file on show that no earlier fit of the list read, as
/// [`plate_fit`](crate::render::picker::plate_fit) samples them.
pub(crate) fn refit_arg_plate(
    palette: &mut CommandPalette,
    area: Rect,
    home: Option<&Path>,
    zoom: i8,
    fs_host: &dyn FsHost,
) {
    let Some(picker) = &palette.arg_picker else {
        return;
    };
    let Some(list_rows) = arg_box(area, palette, zoom).and_then(arg_list_rows) else {
        return;
    };
    let held = palette.arg_plate_fit.take();
    let rows = arg_rows(picker, home);
    let fit = crate::render::picker::plate_fit(held, &rows, list_rows, fs_host);
    palette.arg_plate_fit = Some(fit);
}

/// The argument picker's box within `area` at `zoom` before the plate fits its
/// width, or `None` when `area` is too small to host it.
///
/// [`palette_arg_layout`] and [`refit_arg_plate`] both size the box here, so a
/// refit measures the window that the layout shows.
fn arg_box(area: Rect, palette: &CommandPalette, zoom: i8) -> Option<Rect> {
    let layout = palette_filter_layout(
        area,
        palette.list_rows_hint(),
        zoom,
        palette.header_rows,
        palette.doc_rows(),
    )?;
    Some(layout.modal)
}

/// The results that the argument picker's list shows in `rows`, its box before
/// the plate fits its width, or `None` when the box holds no body row.
///
/// The count comes from the body under a one-row Cmd Line, because the Cmd
/// Line's rows follow the plate's width.
fn arg_list_rows(rows: Rect) -> Option<u16> {
    let inner = Block::default().borders(Borders::ALL).inner(rows);
    arg_body_rows(inner, 1).map(|(_, list_rows)| list_rows)
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
/// A palette with a Run Block shows it, as [`render_palette_run_block`]
/// paints it. Every other stage stamps the plate it draws in
/// [`CommandPalette::plate`]. While the palette history walks, the palette
/// shows History, as
/// [`render_palette_history`] paints it. The ages of its lines count from
/// `now`, the clock in unix seconds. Otherwise the palette shows the argument
/// picker, the free-typed argument's hint, or the command list, by the stage
/// that the input names. The signature and the doc of a `cli` command read
/// its loaded help in `specs`. A pick list's preview takes the board's pad
/// inside its Island where `insets` says the terminal starts a page at its
/// pool's inset.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_command_palette(
    palette: &mut CommandPalette,
    ws: &mut Workspace,
    home: Option<&Path>,
    theme: &crate::theme::Theme,
    chrome: &crate::render::editor::ResolvedChrome,
    area: Rect,
    zoom: i8,
    preview_scale: Option<(u16, u16)>,
    now: i64,
    specs: &HashMap<String, CliSpecState>,
    insets: bool,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    if palette.block.is_some() {
        render_palette_run_block(palette, ws, home, theme, area, buf, scene);
        return;
    }
    if ws.palette_history.is_walking() {
        render_palette_history(palette, ws, home, theme, area, zoom, now, buf, scene);
        return;
    }
    palette.history_rows.clear();
    palette.history_spans.clear();
    palette.history_body = None;

    if palette.cli_list.is_some() {
        render_palette_cli(palette, ws, home, theme, area, zoom, specs, buf, scene);
        return;
    }

    if palette.pick.is_some() {
        render_palette_pick(
            palette, ws, home, theme, chrome, area, zoom, insets, buf, scene,
        );
        return;
    }

    if palette.arg_picker.is_some() && palette.arg_source().is_some() {
        render_palette_arg_picker(
            palette,
            ws,
            home,
            theme,
            chrome,
            area,
            zoom,
            preview_scale,
            buf,
            scene,
        );
        return;
    }

    // A parsed command whose argument has no inline picker (ValueSource::None)
    // shows a hint naming the argument instead of the emptied command list. Key
    // on the source, not picker presence -- a stale picker from an earlier
    // source may linger a frame and must not win.
    if let Some(command) = palette.command.clone()
        && palette.arg_source().is_none()
    {
        render_palette_free_arg(
            palette, &command, ws, home, theme, area, zoom, specs, buf, scene,
        );
        return;
    }

    let content_rows = palette.list_rows_hint();
    let status = filter_status(palette, theme);
    let cycle = PaletteScope::cycle(&palette.commands);
    let pixels = RowMetrics::of(theme, scene);
    palette.row_metrics = pixels;
    render_palette_filter(
        &palette.input,
        &palette.filtered,
        &palette.match_indices,
        palette.selected,
        palette.group,
        palette.namespace,
        palette.scope,
        &cycle,
        content_rows,
        zoom,
        &mut palette.header_rows,
        &mut palette.doc_scroll,
        palette.resolved.as_ref(),
        &status,
        &palette.keys,
        specs,
        pixels,
        ws,
        home,
        theme,
        area,
        buf,
        scene,
    );

    if let Some(layout) = palette_filter_layout(
        area,
        content_rows,
        zoom,
        palette.header_rows,
        palette.doc_rows(),
    ) {
        palette.plate = Some(layout.modal);
        if palette.command.is_none() {
            let rows = layout.list.height;
            palette.viewport_rows = Some(match pixels {
                Some(m) => command_window(&palette.filtered, palette.selected, rows, m)
                    .whole(m.extent(rows))
                    .max(1),
                None => usize::from(rows),
            });
        }
    }
}

/// Render the palette's Run Block, the stage that shows the Cmd of a `block`
/// command's Enter.
///
/// The plate keeps the top row and the width of the typing stage, as
/// [`run_block_plate_width`] widens it, and its height follows its rows. The
/// Cmd Line draws the Cmd's line with its first word as the token and no
/// caret, and the status of the Run pane's header. One Island holds the body,
/// the newest rows of the Cmd's report or of its grid, in the Run pane's row
/// styles, with a running Parsed Cmd's progress bar on the first row.
///
/// The body takes 1 to [`RUN_BLOCK_ROWS`] rows above the area's last row. It
/// grows with the output and holds its rows while the Cmd runs, and it takes
/// the rows of the finished body at the end. The key line names the keys that
/// act on the Cmd, which differ once it finishes.
fn render_palette_run_block(
    palette: &mut CommandPalette,
    ws: &Workspace,
    home: Option<&Path>,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let Some(block) = palette.block.as_mut() else {
        return;
    };
    let Some(cmd) = ws.cmds.get(block.cmd) else {
        return;
    };

    let width = run_block_plate_width(block.origin, area);
    let x = match width == block.origin.width {
        true => block.origin.x,
        false => area.x + area.width.saturating_sub(width) / 2,
    };
    let cwd = crate::run::abbreviate_path(&cmd.cwd, home);
    let token = cmd
        .command
        .chars()
        .take_while(|ch| !ch.is_whitespace())
        .count();
    let input = cmd_line::token_spans(&cmd.command, token, theme);
    let parts = run_pane::status_parts(cmd, &ws.terms, theme);
    let line = CmdLine {
        cwd: &cwd,
        input: &input,
        caret: None,
        status: CmdStatus::Parts(&parts),
    };
    let header_rows = cmd_line::cmd_line_rows(&line, width.saturating_sub(2), theme, scene);
    palette.header_rows = header_rows;

    let report = cmd.report_rows();
    let content = report
        .as_ref()
        .map_or_else(|| cmd.grid.rendered_line_count(), Vec::len);
    let content_rows = u16::try_from(content).unwrap_or(u16::MAX);
    block.rows = match cmd.finished {
        true => content_rows,
        false => block.rows.max(content_rows),
    };
    let room = area
        .bottom()
        .saturating_sub(1)
        .saturating_sub(block.origin.y + header_rows + 4)
        .max(1);
    let wanted = block.rows.clamp(1, RUN_BLOCK_ROWS);
    let progress = cmd
        .report
        .as_ref()
        .and_then(CmdReport::progress)
        .filter(|_| !cmd.finished);
    let pixels = block_pixels(theme, scene, wanted, room, progress.is_some());
    let (lines, rows) = match &pixels {
        Some(pixels) => (pixels.lines, pixels.fit.rows),
        None => (wanted.min(room), wanted.min(room)),
    };
    let extra = pixels
        .as_ref()
        .map_or(0, |pixels| u16::from(pixels.fit.extra_row));
    let modal = Rect::new(x, block.origin.y, width, header_rows + rows + 4 + extra);

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PALETTE);
    let inner = match &pixels {
        Some(pixels) => {
            let inner = Block::default().borders(Borders::ALL).inner(modal);
            let [top, right, _, left] = PLATE_EDGES;
            let edges = [top, right, pixels.fit.plate_bottom, left];
            crate::render::chrome::modal_plate(buf, inner, edges, pixels.plate, theme, scene);
            inner
        },
        None => crate::render::chrome::list_plate(buf, modal, modal_style, theme, &mut *scene),
    };
    cmd_line::cmd_line(buf, inner, &line, theme, scene);

    let body = Rect::new(inner.x, inner.y + 1 + header_rows, inner.width, rows);
    let plate = crate::render::chrome::plate_colors(theme, scene);
    if let Some(plate) = plate {
        let bottom = pixels
            .as_ref()
            .map_or(ISLAND_BOTTOM, |pixels| pixels.fit.bottom);
        let edges = [ISLAND_TOP, ISLAND_SIDE, bottom, ISLAND_SIDE];
        crate::render::chrome::modal_island(body, edges, plate, theme, buf, scene);
    }

    let first = content.saturating_sub(usize::from(lines));
    match &pixels {
        Some(pixels) => {
            let body_rows = report.as_deref();
            paint_block_pixel_lines(
                cmd,
                body_rows,
                (first, body),
                pixels,
                progress,
                theme,
                buf,
                scene,
            );
        },
        None => {
            let row_rect = |y| Rect::new(body.x + 1, y, body.width.saturating_sub(2), 1);
            let dim = theme.get(crate::theme::scope::UI_TEXT_DIM);
            for (y, index) in (body.y..body.bottom()).zip(first..content) {
                match &report {
                    Some(report) => {
                        run_pane::paint_body_row(buf, row_rect(y), &report[index], theme)
                    },
                    None => {
                        run_pane::paint_grid_row(buf, row_rect(y), y, &cmd.grid, index, None, dim)
                    },
                }
            }
            if let Some((done, total)) = progress {
                let row = row_rect(body.y);
                let width = row.width.saturating_mul(16);
                run_pane::paint_progress(row, 0, width, done, total, theme, buf, scene);
            }
        },
    }

    let located = cmd
        .finished_report()
        .and_then(CmdReport::first_location)
        .is_some();
    let hints: &[(&str, &str)] = match (cmd.finished, located) {
        (false, _) => &[
            ("C-j", "to Jobs"),
            ("C-o", "to Run pane"),
            ("C-c", "interrupt"),
            ("Esc", "hide (keeps running \u{2192} Jobs)"),
        ],
        (true, true) => &[
            ("Enter", "go to failure"),
            ("C-r", "run again"),
            ("C-o", "to Run pane"),
            ("Esc", "close"),
        ],
        (true, false) => &[
            ("Enter", "open output"),
            ("C-r", "run again"),
            ("C-o", "to Run pane"),
            ("Esc", "close"),
        ],
    };
    let footer = Footer {
        hints,
        ..Footer::default()
    };
    match &pixels {
        Some(pixels) => {
            let rect = Rect::new(modal.x + 1, body.bottom(), modal.width.saturating_sub(2), 1);
            let down = pixels.fit.footer_down;
            crate::render::picker::modal_footer_at(buf, rect, &footer, plate, down, theme, scene);
        },
        None => {
            let rect = crate::render::picker::footer_rect(modal);
            crate::render::picker::modal_footer(buf, rect, &footer, plate, theme, scene);
        },
    }
}

/// How a Run Block's Island lays its lines out at the board's pixels.
struct BlockPixels {
    metrics: RowMetrics,
    /// The cell height in logical px.
    height: u16,
    /// The lines on show, the newest of the body.
    lines: u16,
    fit: IslandFit,
    plate: PlateColors,
}

/// The pixel layout of a Run Block's Island under `scene`, for a body that
/// wants `wanted` lines, with `room` cell rows for the Island and its extra
/// row, or `None` where the Island keeps its cells.
///
/// The pixel form is the Run pane's, a host that states its cell and its scale
/// and reads `ink` frames, under the plate look, on a cell at most 25 logical
/// px high. A taller cell puts the Island's bottom inside its one row. The
/// Island is 12 px and 17 px for each line high at the 18 px cell, as the
/// cell height less 1 px, and 8 px more over a progress bar. Its top stands
/// `h - 14` px over its first row, so its bottom stands `26 - h + (h - 1) * n
/// + p` px under the top of that row. The lines on show are the most up to
/// `wanted` whose rows fit `room`.
fn block_pixels(
    theme: &crate::theme::Theme,
    scene: &stoat_widgets::ApcScene,
    wanted: u16,
    room: u16,
    progress: bool,
) -> Option<BlockPixels> {
    let metrics = RowMetrics::of(theme, scene).filter(|_| scene.inks())?;
    let plate = crate::render::chrome::plate_colors(theme, scene)?;
    let height = (f32::from(metrics.cell[1]) / metrics.scale).round() as u16;
    if !(1..=25).contains(&height) {
        return None;
    }

    let bar = u16::from(progress) * 8;
    let fit = |lines: u16| {
        let below = 26 - height + (height - 1) * lines + bar;
        crate::render::chrome::island_fit(height, below)
    };
    let fits = |lines: &u16| {
        let fit = fit(*lines);
        fit.rows + u16::from(fit.extra_row) <= room
    };
    let lines = (1..=wanted).rev().find(fits).unwrap_or(1);
    Some(BlockPixels {
        metrics,
        height,
        lines,
        fit: fit(lines),
        plate,
    })
}

/// Draw the newest lines of `cmd`'s body from line `first`, the rows of its
/// report or of its grid, and a running Cmd's `progress` bar, in the Island
/// over `body`, at the board's pixels.
///
/// The Island's top stands `h - 14` px over `body`'s first row, `h` its cell's
/// height in logical px, and pads 6 px. Line i draws `2 + p + (h - 1) * i` px
/// under the top of that row, `p` the 8 px of a progress bar's 2 px and its
/// 3 px over and under, through the Run pane's row painters and in its colors.
/// Each line starts 10 px inside the Island's side, and the bar draws 5 px
/// under the row's top from 10 px inside each side. The paint ends with no ink
/// and no nudge in force.
#[allow(clippy::too_many_arguments)]
fn paint_block_pixel_lines(
    cmd: &Cmd,
    report: Option<&[BodyRow]>,
    (first, body): (usize, Rect),
    pixels: &BlockPixels,
    progress: Option<(u32, u32)>,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let (m, h) = (pixels.metrics, pixels.height);
    let col = m.cols(crate::render::chrome::island_side_pad(m, 10));
    let ui = theme.type_scale(TypeRole::Ui);
    let dim = style_rgb(theme.get(crate::theme::scope::UI_TEXT_DIM).fg).unwrap_or_default();
    let colors = run_pane::CellColors {
        dim,
        surface: pixels.plate.surface,
    };
    let row_at = |px: u16| Rect::new(body.x, body.y + px / h, body.width, 1);
    let mut pen = run_pane::Pen::at(0);

    let bar = match progress {
        Some((done, total)) => {
            pen.nudge((5 % h) as i16, scene);
            pen.ink(None, scene);
            let width = body.width.saturating_mul(16).saturating_sub(2 * col);
            run_pane::paint_progress(row_at(5), col, width, done, total, theme, buf, scene);
            8
        },
        None => 0,
    };
    let content = report.map_or_else(|| cmd.grid.rendered_line_count(), <[BodyRow]>::len);
    for (line, index) in (first..content).enumerate() {
        let px = 2 + bar + (h - 1) * line as u16;
        pen.nudge((px % h) as i16, scene);
        let at = run_pane::RowAt::body(row_at(px), col, ui, &m);
        match report {
            Some(rows) => {
                run_pane::paint_pixel_body_row(&rows[index], &at, theme, dim, &mut pen, buf, scene)
            },
            None => run_pane::paint_pixel_grid_row(
                &cmd.grid, index, None, &at, colors, &mut pen, buf, scene,
            ),
        }
    }
    pen.ink(None, scene);
    scene.nudge([0, 0]);
}

/// The plate that a Run Block opens over when the palette painted no typing
/// stage, a box of [`RUN_BLOCK_COLS`] by 6 cells centered in `area`.
pub(crate) fn run_block_origin(area: Rect) -> Rect {
    let size = (RUN_BLOCK_COLS, 6);
    crate::render::chrome::modal_box(area, size, size, (20, 6), 0).unwrap_or(area)
}

/// The width of a Run Block's plate over `area`, for a stage that opened over
/// the plate `origin`.
///
/// It is the width of `origin`, at least [`RUN_BLOCK_COLS`] and at most the
/// area's width less 2. The plate never narrows while the palette is open.
pub(crate) fn run_block_plate_width(origin: Rect, area: Rect) -> u16 {
    origin
        .width
        .max(RUN_BLOCK_COLS)
        .min(area.width.saturating_sub(2))
}

/// Render the inline file picker shown while collecting a `Files` argument
/// (e.g. `:o `).
///
/// Keeps the filter modal's height and its Cmd Line, then replaces the
/// command list + doc body with a result list beside a live preview, mirroring
/// the standalone file finder. The plate's width follows
/// [`palette_arg_layout`]. State is synced before the frame by
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
    preview_scale: Option<(u16, u16)>,
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
            let mut spans = count_status(list.filtered.len(), &rest, theme);
            let scope = picker
                .scopes
                .as_ref()
                .map(|scopes| &scopes.scope)
                .filter(|scope| **scope != FinderScope::All);
            if let Some(scope) = scope {
                let words = crate::render::file_finder::scope_words(scope);
                let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
                spans.push((format!(" \u{b7} {words}"), muted));
            }
            spans
        })
        .unwrap_or_default();
    let Some(layout) = palette_arg_layout(area, palette, home, zoom, preview_scale) else {
        return;
    };
    palette.plate = Some(layout.modal);
    palette.header_rows = render_palette_prelude(
        &palette.input,
        palette.resolved.as_ref(),
        None,
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
    let Some(layout) = palette_arg_layout(area, palette, home, zoom, preview_scale) else {
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

    // A pixel list's rows stand off the cell grid, so the palette's resident
    // pool draws them over the blank list and the live list paints none.
    let pixels = RowMetrics::of(theme, scene);
    palette.row_metrics = pixels;
    let window = crate::render::picker::list_window(
        &arg_rows(picker, home),
        list.width,
        list.height,
        pixels,
    );
    let budget = match pixels {
        Some(metrics) => metrics.extent(list.height),
        None => u32::from(list.height),
    };
    picker.active_core().picklist.viewport_rows = Some(window.whole(budget).max(1));
    let row_style = if fallback_active {
        theme.get(crate::theme::scope::UI_TEXT_MUTED)
    } else {
        theme.get(crate::theme::scope::UI_TEXT)
    };
    if pixels.is_none() {
        crate::render::picker::paint_path_rows(
            &arg_rows(picker, home),
            RowMarks::Modified(picker.modified_paths()),
            list,
            RowStart {
                result: window.first,
                skip: 0,
            },
            theme,
            row_style,
            true,
            None,
            buf,
            scene,
        );
    }

    let hints = match picker.source() {
        ValueSource::Files => ARG_FILE_HINTS.to_vec(),
        _ => job_hints(palette, &PALETTE_HINTS),
    };
    let parts = Footer {
        hints: &hints,
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
/// what to type instead of showing the emptied command list. A user command
/// shows its usage line and the name of the argument in entry. State is
/// synced before the frame, so this only paints.
#[allow(clippy::too_many_arguments)]
fn render_palette_free_arg(
    palette: &mut CommandPalette,
    command: &PaletteCommand,
    ws: &mut Workspace,
    home: Option<&Path>,
    theme: &crate::theme::Theme,
    area: Rect,
    zoom: i8,
    specs: &HashMap<String, CliSpecState>,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let rows_hint = palette.list_rows_hint();
    let Some(layout) = palette_filter_layout(
        area,
        rows_hint,
        zoom,
        palette.header_rows,
        palette.doc_rows(),
    ) else {
        return;
    };
    palette.plate = Some(layout.modal);
    palette.header_rows = render_palette_prelude(
        &palette.input,
        palette.resolved.as_ref(),
        palette.arg_ghost.as_deref(),
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
    let (hint, entry) = match command {
        PaletteCommand::Action(entry) => (
            palette
                .arg_param()
                .map(|param| format!("{}  {}", param.name, param.description)),
            Some(*entry),
        ),
        PaletteCommand::User(_) => (palette.user_arg().map(|arg| arg.name.clone()), None),
    };
    let usage = usage_line(command, specs);
    write_str_clipped(buf, inner.x + 1, body_top, &usage, text_style, end_x);
    if let Some(hint) = hint
        && body_top + 1 < body_bottom
    {
        write_str_clipped(buf, inner.x + 1, body_top + 1, &hint, text_style, end_x);
    }

    let doc_top = body_top + 3;
    if let Some(entry) = entry
        && doc_top < body_bottom
    {
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

    let hints = job_hints(palette, &PALETTE_HINTS);
    let parts = Footer {
        hints: &hints,
        ..Footer::default()
    };
    crate::render::picker::modal_footer(buf, layout.footer, &parts, plate, theme, scene);
}

/// Render the list of a user command's `pick` argument, under a Cmd Line
/// whose status counts the rows and names the list command.
///
/// The list draws in the box of [`palette_filter_layout`], as History does,
/// over the list rows, the doc rows, and the row between them. A list with a
/// preview template splits that body as [`pick_body`] splits it. Its preview
/// draws at Preview Scale beside the list, as the argument picker's preview
/// draws.
///
/// [`paint_arg_rows`] paints the rows. A list command that failed shows its
/// text on the first row in `ui.error`. The status reads as [`pick_status`]
/// words it, in `ui.text.muted`. The list draws on the live grid with no pool,
/// and its rows step one at a time.
///
/// Under a scene whose lists lay their rows out by pixels, the rows are pixel
/// rows, and the list's page steps by the rows whose boxes fit. The selected
/// row's box stands [`PIXEL_PREVIEW_BOX_INSET`] px inside a list beside a
/// preview. A failed list's text draws as one run where a row's label starts.
/// Where `insets` says the terminal starts a page at its pool's inset, the
/// preview's page stands [`PICK_PREVIEW_PAD`] inside its Island, as
/// [`island_pad`](crate::render::chrome::island_pad) sets its pad.
///
/// The key line names the shell line that Enter runs and the refresh key, as
/// [`palette_footer_parts`] words the pick stage.
#[allow(clippy::too_many_arguments)]
fn render_palette_pick(
    palette: &mut CommandPalette,
    ws: &mut Workspace,
    home: Option<&Path>,
    theme: &crate::theme::Theme,
    chrome: &crate::render::editor::ResolvedChrome,
    area: Rect,
    zoom: i8,
    insets: bool,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let pixels = RowMetrics::of(theme, scene);
    palette.row_metrics = pixels;
    let content_rows = palette.list_rows_hint();
    let Some(layout) = palette_filter_layout(
        area,
        content_rows,
        zoom,
        palette.header_rows,
        palette.doc_rows(),
    ) else {
        return;
    };
    palette.plate = Some(layout.modal);
    let status = {
        let name = palette.user_arg().map_or("", |arg| arg.name.as_str());
        let text = palette
            .pick
            .as_ref()
            .map_or_else(String::new, |pick| pick_status(pick, name));
        vec![(text, theme.get(crate::theme::scope::UI_TEXT_MUTED))]
    };
    palette.header_rows = render_palette_prelude(
        &palette.input,
        palette.resolved.as_ref(),
        None,
        &[],
        CmdStatus::Text(&status),
        layout.modal,
        layout.inner,
        home,
        ws,
        theme,
        buf,
        scene,
    );
    let Some(layout) = palette_filter_layout(
        area,
        content_rows,
        zoom,
        palette.header_rows,
        palette.doc_rows(),
    ) else {
        return;
    };

    let previews = palette
        .pick
        .as_ref()
        .is_some_and(|pick| pick.spec.preview.is_some());
    let (list, preview) = pick_body(&layout, previews);
    let plate = crate::render::chrome::plate_colors(theme, scene);
    if let Some(plate) = plate {
        crate::render::chrome::list_islands(list, preview, plate, theme, buf, scene);
    }
    if let Some(pick) = palette.pick.as_mut() {
        if let Some(preview_rect) = preview {
            if plate.is_none() {
                let separator = theme.get(crate::theme::scope::UI_BORDER_INACTIVE);
                let x = list.x + list.width;
                crate::render::chrome::vline(buf, x, list.y, list.height, separator, scene);
            }
            pick.preview.pad = match pixels.filter(|_| insets) {
                Some(m) => crate::render::chrome::island_pad(m, PICK_PREVIEW_PAD),
                None => [0; 4],
            };
            crate::render::picker::render_picker_preview(
                &mut pick.preview,
                preview_rect,
                theme,
                chrome,
                ws,
                buf,
                scene,
            );
        }
        let end = match preview {
            Some(_) => PIXEL_PREVIEW_BOX_INSET,
            None => PIXEL_BOX_INSET,
        };
        let pixels = pixels.map(|m| (m, end));
        pick.list.viewport_rows = Some(match pixels {
            Some((m, _)) => m.fit(list.height),
            None => usize::from(list.height),
        });
        match &pick.load {
            PickLoad::Failed(text) => {
                let error = theme.get(crate::theme::scope::UI_ERROR);
                match pixels {
                    Some(pixels) => {
                        paint_pixel_arg_note(text, error, list, theme, pixels, buf, scene)
                    },
                    None => write_str_clipped(buf, list.x + 1, list.y, text, error, list.right()),
                }
            },
            PickLoad::Loading | PickLoad::Ready => {
                paint_arg_rows(&pick.list, None, list, theme, true, pixels, buf, scene);
            },
        }
    }

    let (line, tag) = match palette.command.as_ref() {
        Some(command @ PaletteCommand::User(user)) => (user.line, doc_tag(command)),
        _ => (0, ("config", crate::theme::scope::UI_CMD_SOURCE_CONFIG)),
    };
    let enter = palette.enter_line(ws).unwrap_or_else(|| "run".to_owned());
    let mut parts = palette_footer_parts(KeyStage::Pick { enter, line, tag }, theme);
    parts.hints = job_hints(palette, &parts.hints);
    parts.paint(layout.footer, plate, theme, buf, scene);
}

/// Render the argument entry of a `cli` command over the list of its flags
/// or its subcommands, as the palette's `cli_list` holds it.
///
/// The body takes the root list's layout, [`palette_filter_layout`], with the
/// list's Island over the doc's Island. [`paint_arg_rows`] paints the rows in
/// the columns of their kind, and the doc island shows the command's doc, as
/// [`doc_lines`] words it. The Cmd Line's status counts the kind's rows in the
/// help, as [`cli_status`] words it, and the key line names the keys of the
/// `cli` stage of [`palette_footer_parts`].
///
/// Under a scene whose lists lay their rows out by pixels, the rows are pixel
/// rows, and the doc island takes the pixel doc's edges and lines, as the
/// command list's doc does, from the doc's top.
#[allow(clippy::too_many_arguments)]
fn render_palette_cli(
    palette: &mut CommandPalette,
    ws: &mut Workspace,
    home: Option<&Path>,
    theme: &crate::theme::Theme,
    area: Rect,
    zoom: i8,
    specs: &HashMap<String, CliSpecState>,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let Some(PaletteCommand::User(user)) = palette.command.clone() else {
        return;
    };
    let Some(kind) = palette.cli_list.as_ref().map(|(_, kind, _)| *kind) else {
        return;
    };
    let pixels = RowMetrics::of(theme, scene);
    palette.row_metrics = pixels;
    let content_rows = palette.list_rows_hint();
    let Some(layout) = palette_filter_layout(
        area,
        content_rows,
        zoom,
        palette.header_rows,
        palette.doc_rows(),
    ) else {
        return;
    };
    palette.plate = Some(layout.modal);
    let status = cli_status(&user, kind, specs, theme);
    palette.header_rows = render_palette_prelude(
        &palette.input,
        palette.resolved.as_ref(),
        None,
        &[],
        CmdStatus::Text(&status),
        layout.modal,
        layout.inner,
        home,
        ws,
        theme,
        buf,
        scene,
    );
    let Some(layout) = palette_filter_layout(
        area,
        content_rows,
        zoom,
        palette.header_rows,
        palette.doc_rows(),
    ) else {
        return;
    };

    let plate = crate::render::chrome::plate_colors(theme, scene);
    if let Some(plate) = plate {
        let (list_bottom, doc_top) = match (layout.doc.is_empty(), pixels) {
            (true, _) => (ISLAND_BOTTOM, ISLAND_GAP),
            (false, Some(_)) => (PIXEL_LIST_EDGE, PIXEL_DOC_EDGE),
            (false, None) => (ISLAND_GAP, ISLAND_GAP),
        };
        let list_edges = [ISLAND_TOP, ISLAND_SIDE, list_bottom, ISLAND_SIDE];
        crate::render::chrome::modal_island(layout.list, list_edges, plate, theme, buf, scene);
        let doc_edges = [doc_top, ISLAND_SIDE, ISLAND_BOTTOM, ISLAND_SIDE];
        crate::render::chrome::modal_island(layout.doc, doc_edges, plate, theme, buf, scene);
    }
    if let Some((_, _, list)) = palette.cli_list.as_mut() {
        let pixels = pixels.map(|m| (m, PIXEL_BOX_INSET));
        list.viewport_rows = Some(match pixels {
            Some((m, _)) => m.fit(layout.list.height),
            None => usize::from(layout.list.height),
        });
        paint_arg_rows(
            list,
            Some(kind),
            layout.list,
            theme,
            true,
            pixels,
            buf,
            scene,
        );
    }

    if layout.doc.height > 0 {
        if plate.is_none() {
            let separator = theme.get(crate::theme::scope::UI_BORDER_INACTIVE);
            let inner = layout.inner;
            let row = layout.doc.y - 1;
            crate::render::chrome::hline(buf, inner.x, row, inner.width, separator, scene);
        }
        let command = PaletteCommand::User(user);
        let width = doc_chars(&layout, pixels.is_some(), theme);
        let text = doc_lines(&command, &palette.keys, width, theme, specs);
        let tag = Some(doc_tag(&command));
        match pixels {
            Some(m) => paint_doc_pixels(layout.doc, &text, (0, tag), theme, m, buf, scene),
            None => paint_doc_lines(buf, layout.doc, &text.lines, tag, &text.note, theme, scene),
        }
    }

    let mut parts = palette_footer_parts(KeyStage::Cli { kind }, theme);
    parts.hints = job_hints(palette, &parts.hints);
    parts.paint(layout.footer, plate, theme, buf, scene);
}

/// The status of the Cmd Line over the `kind` list of the `cli` command
/// `user`, whose help lands in `specs`.
///
/// Over the flags it reads `flags`, U+00B7, the count of the help's flags, and
/// the source, in `ui.text.muted`. Over the subcommands it reads `cli` in the
/// foreground of `ui.cmd.source.cli`, then ` subcommands`, U+00B7, the count,
/// and the source. The source reads `from completions` for a command that
/// declares completions, and `from --help` otherwise.
fn cli_status(
    user: &UserCommand,
    kind: CliListKind,
    specs: &HashMap<String, CliSpecState>,
    theme: &crate::theme::Theme,
) -> StyledLine {
    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let source = match &user.run {
        RunSpec::Cli {
            completions: Some(_),
            ..
        } => "completions",
        _ => "--help",
    };
    let spec = loaded_cli(user, specs).map(|loaded| &loaded.spec);
    match kind {
        CliListKind::Flags => {
            let count = grouped(spec.map_or(0, |spec| spec.flags.len()));
            vec![(format!("flags \u{b7} {count} from {source}"), muted)]
        },
        CliListKind::Subcommands => {
            let count = grouped(spec.map_or(0, |spec| spec.subcommands.len()));
            let cli = theme
                .try_get(crate::theme::scope::UI_CMD_SOURCE_CLI)
                .and_then(|style| style.fg)
                .map_or(muted, |fg| muted.fg(fg));
            vec![
                ("cli".to_owned(), cli),
                (format!(" subcommands \u{b7} {count} from {source}"), muted),
            ]
        },
    }
}

/// The list rect and the preview rect of a pick list in `layout`.
///
/// A list with `previews` splits the body as the argument picker splits its
/// body. The preview stands right of the list when the body is wide enough,
/// and the list takes the whole body otherwise. The paint and the hit test
/// both read the rects here.
pub(crate) fn pick_body(layout: &PaletteFilterLayout, previews: bool) -> (Rect, Option<Rect>) {
    let body = layout.body();
    match previews {
        true => crate::render::picker::split_list_preview(
            body.x,
            body.y,
            body.width,
            body.height,
            50,
            20,
            crate::render::picker::DEFAULT_LIST_SHARE,
        ),
        false => (body, None),
    }
}

/// The Cmd Line status of `pick`, the list of the argument named `name`.
///
/// A list with rows reads the count of the rows on show and the argument's
/// name, as `3 prs`. The name takes an `s` for any count but 1. A list whose
/// command runs reads `loading`, and one whose command failed reads `failed`.
/// A space, U+00B7, a space, `from`, and the head of the list command follow,
/// as [`pick::list_head`] gives it.
fn pick_status(pick: &PickState, name: &str) -> String {
    let count = match &pick.load {
        PickLoad::Loading => "loading".to_owned(),
        PickLoad::Failed(_) => "failed".to_owned(),
        PickLoad::Ready => {
            let shown = pick.list.filtered.len();
            let plural = match shown {
                1 => "",
                _ => "s",
            };
            format!("{} {name}{plural}", grouped(shown))
        },
    };
    format!("{count} \u{b7} from {}", pick::list_head(&pick.spec.list))
}

/// Paint the rows of `list` that match its query into `area`, one row for
/// each, with the selected row and the matched chars marked.
///
/// A row paints as [`paint_palette_rows`] paints the name of a root row. Its
/// label reads in `ui.text` from the row's second cell, and it clips at the
/// area's edge with no ellipsis. Its matched chars take
/// [`Theme::list_match`](crate::theme::Theme::list_match). The selected row
/// draws through [`chrome::selected_row`](crate::render::chrome::selected_row)
/// with `panels`. The window starts where
/// [`window_start`](crate::render::picker::window_start) keeps the selection
/// on show.
///
/// The rows of a `cli` command's list take the columns of their `shape`, with
/// the label in bold and the description in `ui.text.dim`. A flag row sets its
/// value name in angle brackets in `ui.cmd.flag.value` at
/// [`FLAG_LABEL_COLUMNS`] past the label's start, and its description
/// [`FLAG_VALUE_COLUMNS`] past that, one cell of gap after each. A subcommand
/// row sets its description one cell past the widest name on show, at least
/// [`SUBCOMMAND_COLUMNS`] wide. A pick row, with no shape, reads its label
/// alone.
///
/// With `pixels`, the metrics and the px that the selected row's box stands
/// inside the list's right end, the rows are pixel rows as
/// [`paint_pixel_arg_rows`] draws them, and they write no cell.
#[allow(clippy::too_many_arguments)]
fn paint_arg_rows(
    list: &ArgList,
    shape: Option<CliListKind>,
    area: Rect,
    theme: &crate::theme::Theme,
    panels: bool,
    pixels: Option<(RowMetrics, u16)>,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    if let Some(pixels) = pixels {
        paint_pixel_arg_rows(list, shape, area, theme, pixels, buf, scene);
        return;
    }
    let rows = usize::from(area.height);
    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let label_base = match shape {
        Some(_) => row_style.add_modifier(Modifier::BOLD),
        None => row_style,
    };
    let dim = theme.get(crate::theme::scope::UI_TEXT_DIM);
    let value_base = theme
        .try_get(crate::theme::scope::UI_CMD_FLAG_VALUE)
        .unwrap_or_else(|| theme.get(crate::theme::scope::UI_TEXT_MUTED));
    let match_style = theme.list_match();
    let keeps_columns = theme
        .try_get(crate::theme::scope::UI_LIST_SELECTED)
        .is_some();
    let start = crate::render::picker::window_start(list.selected, rows);
    let end_x = area.right();
    let mut derived = Vec::new();
    let mut matching = crate::fuzzy::Scratch::default();

    let on_show = || list.filtered.iter().enumerate().skip(start).take(rows);
    let name_cols = on_show()
        .filter_map(|(_, &index)| list.rows.get(index))
        .map(|row| row.label.chars().count())
        .max()
        .unwrap_or(0)
        .max(usize::from(SUBCOMMAND_COLUMNS)) as u16;

    for (y, (place, &index)) in (area.y..).zip(on_show()) {
        let Some(item) = list.rows.get(index) else {
            continue;
        };
        let row = Rect::new(area.x, y, area.width, 1);
        let is_selected = place == list.selected;
        let style = match is_selected {
            true => crate::render::chrome::selected_row(row, row_style, panels, theme, buf, scene),
            false => {
                for col in row.left()..row.right() {
                    buf[(col, y)].set_char(' ').set_style(row_style);
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

        let label_x = area.x + 1;
        write_str_clipped(buf, label_x, y, &item.label, column(label_base), end_x);
        let indices = list.row_indices(place, &mut derived, &mut matching);
        paint_matches(
            buf,
            (label_x, y, end_x),
            &item.label,
            0,
            indices,
            match_style,
        );

        let label_cols = item.label.chars().count() as u16;
        let desc_x = match shape {
            None => continue,
            Some(CliListKind::Subcommands) => label_x.saturating_add(name_cols + 1),
            Some(CliListKind::Flags) => {
                let value_x = label_x.saturating_add(label_cols.max(FLAG_LABEL_COLUMNS) + 1);
                let value = item
                    .value_name
                    .as_ref()
                    .map_or_else(String::new, |name| format!("<{name}>"));
                write_str_clipped(buf, value_x, y, &value, column(value_base), end_x);
                let value_cols = (value.chars().count() as u16).max(FLAG_VALUE_COLUMNS);
                value_x.saturating_add(value_cols + 1)
            },
        };
        write_str_clipped(buf, desc_x, y, &item.description, column(dim), end_x);
    }
}

/// Paint the rows of `list` into `area` as pixel rows laid out by `m`, in the
/// columns that [`paint_arg_rows`] gives them, with the selected row's box
/// `end` px inside the list's right end.
///
/// The walk writes no cell. It draws the rows that [`RowMetrics::fit`] counts,
/// from where [`window_start`](crate::render::picker::window_start) keeps the
/// selection among them, row k of the window k pitches under the list's top.
/// So no row's line draws past the list, which no pool clips. The selected
/// row's box draws as a bar in the selected background 1 px inside the list's
/// left end, and each row's line stands 1 px under its top.
///
/// A row's label draws at the cell size from [`PIXEL_FIRST_COLUMN`], with its
/// matched chars in the match style. A flag row cuts its label at
/// [`FLAG_LABEL_COLUMNS`] chars and sets its value name [`PIXEL_COLUMN_GAP`]
/// after that many cells, cut at [`FLAG_VALUE_COLUMNS`] chars. Its
/// description follows the gap after that many value chars. A subcommand
/// row's description follows the gap after the widest name on show, at least
/// [`SUBCOMMAND_COLUMNS`] cells. The value name and the description draw at
/// the [`TypeRole::Ui`] size, and a row's last column stops 8 px inside its
/// box.
fn paint_pixel_arg_rows(
    list: &ArgList,
    shape: Option<CliListKind>,
    area: Rect,
    theme: &crate::theme::Theme,
    (m, end): (RowMetrics, u16),
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let label_base = match shape {
        Some(_) => row_style.add_modifier(Modifier::BOLD),
        None => row_style,
    };
    let dim = theme.get(crate::theme::scope::UI_TEXT_DIM);
    let value_base = theme
        .try_get(crate::theme::scope::UI_CMD_FLAG_VALUE)
        .unwrap_or_else(|| theme.get(crate::theme::scope::UI_TEXT_MUTED));
    let match_style = theme.list_match();
    let selected_style = theme.list_selected(crate::theme::scope::UI_SELECTION);
    let keeps_columns = theme
        .try_get(crate::theme::scope::UI_LIST_SELECTED)
        .is_some();
    let fallback = style_rgb(Some(crate::render::themed_fg(theme))).unwrap_or([0xff; 3]);
    let ui = theme.type_scale(TypeRole::Ui);

    let left = m.cols(PIXEL_BOX_INSET);
    let box_end = area.width.saturating_mul(16).saturating_sub(m.cols(end));
    let row_end = arg_row_end(area, m, end);
    let fit = m.fit(area.height);
    let start = crate::render::picker::window_start(list.selected, fit);
    let on_show = || list.filtered.iter().enumerate().skip(start).take(fit);
    let name_cols = on_show()
        .filter_map(|(_, &index)| list.rows.get(index))
        .map(|row| row.label.chars().count())
        .max()
        .unwrap_or(0)
        .max(usize::from(SUBCOMMAND_COLUMNS)) as u16;
    let mut derived = Vec::new();
    let mut matching = crate::fuzzy::Scratch::default();

    let mut placer = Placer::default();
    for (place, &index) in on_show() {
        let Some(item) = list.rows.get(index) else {
            continue;
        };
        let top = (place - start) as u32 * m.pitch(1);
        let is_selected = place == list.selected;
        if let Some(color) = style_rgb(selected_style.bg).filter(|_| is_selected) {
            let bar = PixelBar {
                x: left as i16,
                y: top,
                width: box_end.saturating_sub(left),
                height: m.box16(1),
                color,
                radius: SELECTED_ROW_RADIUS,
            };
            pixel_rows::bar(&mut placer, &m, area, &bar, buf, scene);
        }
        let column = |own: Style| match is_selected && !keeps_columns {
            true => selected_style,
            false => own,
        };

        let anchor = placer.line(&m, area, top + m.px(ROW_PAD), scene);
        let label_chars = match shape {
            Some(CliListKind::Flags) => usize::from(FLAG_LABEL_COLUMNS),
            Some(CliListKind::Subcommands) => usize::from(name_cols),
            None => chars_within(row_end.saturating_sub(PIXEL_FIRST_COLUMN), FULL_CELL_SCALE),
        };
        let label = clip_to_width(&item.label, label_chars);
        let indices = list.row_indices(place, &mut derived, &mut matching);
        let spans = match_spans(label, 0, indices, column(label_base), match_style);
        let (col, scale) = (PIXEL_FIRST_COLUMN, FULL_CELL_SCALE);
        pixel_rows::emit_spans(&spans, col, scale, anchor, fallback, buf, scene);

        let desc_col = match shape {
            None => continue,
            Some(CliListKind::Subcommands) => {
                PIXEL_FIRST_COLUMN + name_cols.saturating_mul(16) + PIXEL_COLUMN_GAP
            },
            Some(CliListKind::Flags) => {
                let value_col = PIXEL_FIRST_COLUMN + FLAG_LABEL_COLUMNS * 16 + PIXEL_COLUMN_GAP;
                let value = item
                    .value_name
                    .as_ref()
                    .map_or_else(String::new, |name| format!("<{name}>"));
                let value = [(
                    clip_to_width(&value, usize::from(FLAG_VALUE_COLUMNS)),
                    column(value_base),
                )];
                pixel_rows::emit_spans(&value, value_col, ui, anchor, fallback, buf, scene);
                let value_width = text_run::advance_sixteenths(usize::from(FLAG_VALUE_COLUMNS), ui);
                value_col + value_width + PIXEL_COLUMN_GAP
            },
        };
        let room = chars_within(row_end.saturating_sub(desc_col), ui);
        let description = [(clip_to_width(&item.description, room), column(dim))];
        pixel_rows::emit_spans(&description, desc_col, ui, anchor, fallback, buf, scene);
    }
    placer.end(scene);
}

/// The sixteenth of a pixel arg row of `area` that the row's last column stops
/// at, 8 px inside the row's box, whose right edge stands `end` px inside the
/// list's right end.
fn arg_row_end(area: Rect, m: RowMetrics, end: u16) -> u16 {
    area.width
        .saturating_mul(16)
        .saturating_sub(m.cols(end) + m.cols(PIXEL_ROW_PAD))
}

/// Draw `text` in `style` as one run on the first line of the pixel arg list
/// `area` laid out by `m`, where a row's label starts, cut where a row whose
/// box stands `end` px inside the list's right end stops its last column.
///
/// A pick list whose command failed shows its text this way in place of its
/// rows.
fn paint_pixel_arg_note(
    text: &str,
    style: Style,
    area: Rect,
    theme: &crate::theme::Theme,
    (m, end): (RowMetrics, u16),
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let fallback = style_rgb(Some(crate::render::themed_fg(theme))).unwrap_or([0xff; 3]);
    let room = arg_row_end(area, m, end).saturating_sub(PIXEL_FIRST_COLUMN);
    let note = [(
        clip_to_width(text, chars_within(room, FULL_CELL_SCALE)),
        style,
    )];
    let mut placer = Placer::default();
    let anchor = placer.line(&m, area, m.px(ROW_PAD), scene);
    let (col, scale) = (PIXEL_FIRST_COLUMN, FULL_CELL_SCALE);
    pixel_rows::emit_spans(&note, col, scale, anchor, fallback, buf, scene);
    placer.end(scene);
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
/// to [`CommandPalette::history_rows`] for the pointer, or in the pixel form
/// their spans to [`CommandPalette::history_spans`] and the list's rect to
/// [`CommandPalette::history_body`].
///
/// History draws on the live grid, because its rows differ in height and each
/// step of the walk also changes the input and the chip with the rows. No pool
/// pages it. In the pixel form a pool with no page covers the list only so
/// that a press there reports the pointer's pixel.
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
    let pixels = RowMetrics::of(theme, scene);
    palette.row_metrics = pixels;
    let content_rows = palette.list_rows_hint();
    let Some(layout) = palette_filter_layout(
        area,
        content_rows,
        zoom,
        palette.header_rows,
        palette.doc_rows(),
    ) else {
        return;
    };
    palette.plate = Some(layout.modal);

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
        None,
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
    let Some(layout) = palette_filter_layout(
        area,
        content_rows,
        zoom,
        palette.header_rows,
        palette.doc_rows(),
    ) else {
        return;
    };

    let body = layout.body();
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
    (palette.history_rows, palette.history_spans) = paint_history_rows(
        &entries,
        place,
        now,
        body,
        &palette.availability,
        &palette.commands,
        theme,
        pixels,
        buf,
        scene,
    );
    palette.history_body = pixels.map(|_| body);

    let stage = palette_footer_parts(KeyStage::History { needle }, theme);
    stage.paint(layout.footer, plate, theme, buf, scene);
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
/// Answers the cells of each entry on show with the index of its line, and no
/// pixel span.
///
/// With `pixels` the rows are pixel rows laid out by those metrics, as
/// [`paint_pixel_history_rows`] draws them, and they write no cell. The line
/// wraps in the whole cells from [`HISTORY_PIXEL_LINE`] to
/// [`PIXEL_COLUMN_GAP`] before the age, which ends 8 px inside the row's box,
/// or to that end for a line with no time. The answer is then no cell and the
/// physical px span of each entry on show.
#[allow(clippy::too_many_arguments)]
fn paint_history_rows(
    entries: &[HistoryEntry<'_>],
    selected: usize,
    now: i64,
    area: Rect,
    availability: &Availability,
    commands: &CommandSet,
    theme: &crate::theme::Theme,
    pixels: Option<RowMetrics>,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) -> (Vec<(Rect, usize)>, HistorySpans) {
    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let head_style = row_style.add_modifier(Modifier::BOLD);
    let dim = theme.get(crate::theme::scope::UI_TEXT_DIM);
    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let mark = theme.get(crate::theme::scope::UI_TEXT_DISABLED);
    let matched = theme.list_match();
    let keeps_columns = theme
        .try_get(crate::theme::scope::UI_LIST_SELECTED)
        .is_some();
    let caption = theme.type_scale(TypeRole::Caption);

    let laid: Vec<LaidEntry<'_>> = entries
        .iter()
        .map(|entry| {
            let chars: Vec<char> = entry.line.chars().collect();
            let age = entry.at.map(|at| history_age(now, at));
            let room = match pixels {
                Some(m) => {
                    let end = history_pixel_end(area, m);
                    let line_end = match &age {
                        Some(age) => end
                            .saturating_sub(text_run::advance_sixteenths(
                                age.chars().count(),
                                caption,
                            ))
                            .saturating_sub(PIXEL_COLUMN_GAP),
                        None => end,
                    };
                    line_end.saturating_sub(HISTORY_PIXEL_LINE) / 16
                },
                None => {
                    let age_cells = age.as_ref().map_or(0, |age| age.chars().count() as u16 + 1);
                    area.width
                        .saturating_sub(HISTORY_LINE_COLUMN + 1 + age_cells)
                },
            };
            LaidEntry {
                entry,
                rows: pre_wrap_rows(&chars, usize::from(room), false),
                chars,
                age,
                head: history_head_chars(entry.line, availability, commands),
            }
        })
        .collect();
    let heights: Vec<usize> = laid
        .iter()
        .map(|laid| match pixels {
            Some(m) => m.pitch(laid.rows.len() as u32) as usize,
            None => laid.rows.len(),
        })
        .collect();
    let budget = match pixels {
        Some(m) => m.extent(area.height) as usize,
        None => usize::from(area.height),
    };
    let start = history_window(&heights, selected, budget);
    if let Some(m) = pixels {
        let spans = paint_pixel_history_rows(&laid, (start, selected), area, theme, m, buf, scene);
        return (Vec::new(), spans);
    }

    let mut drawn = Vec::new();
    let mut y = area.y;
    for (
        place,
        LaidEntry {
            entry,
            chars,
            age,
            rows,
            head,
        },
    ) in laid.iter().enumerate().skip(start)
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
    (drawn, Vec::new())
}

/// A History entry as its row lays it out.
struct LaidEntry<'a> {
    entry: &'a HistoryEntry<'a>,
    chars: Vec<char>,
    /// The age that the row shows, or `None` for a line with no known time.
    age: Option<String>,
    /// The char range of each line that the entry wraps onto.
    rows: Vec<Range<usize>>,
    /// The chars of the line's head, as [`history_head_chars`] counts them.
    head: usize,
}

/// The sixteenth of a pixel History row that its age ends at, 8 px inside
/// the row's box, which stands 1 px inside each end of `area`.
fn history_pixel_end(area: Rect, m: RowMetrics) -> u16 {
    area.width
        .saturating_mul(16)
        .saturating_sub(m.cols(PIXEL_BOX_INSET) + m.cols(PIXEL_ROW_PAD))
}

/// Paint the History entries `laid` from `start` into `area` as pixel rows
/// laid out by `m`, with the entry at `selected` as the selected row, and
/// answer the physical px span of each entry on show with its line's index.
///
/// The walk writes no cell. Each entry's box is its lines' 18 px each and a
/// pad of 1 px over and under them, 1 px from the next and 1 px inside each
/// end of `area`, and the selected entry's box draws as a bar in the selected
/// background. The mark draws on the first line at [`HISTORY_PIXEL_MARK`], and
/// each line of the wrap at [`HISTORY_PIXEL_LINE`], both at the cell size,
/// with the head and the rest of the line and the needle's marks as
/// [`history_row_spans`] splits them. The age draws as one
/// [`TypeRole::Caption`] run on the first line, ending where
/// [`history_pixel_end`] puts it.
///
/// An entry's span runs from its box's top to the next box's top, so the 1 px
/// under it is its own.
///
/// A line draws only when its whole 18 px stand inside `area`, because History
/// draws on the live scene, which no pool's scissor clips. The walk ends at
/// the first line that would pass `area`'s bottom, and an entry that draws no
/// line pushes no span. A selected entry taller than `area` draws its box down
/// to `area`'s bottom.
fn paint_pixel_history_rows(
    laid: &[LaidEntry<'_>],
    (start, selected): (usize, usize),
    area: Rect,
    theme: &crate::theme::Theme,
    m: RowMetrics,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) -> HistorySpans {
    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let head_style = row_style.add_modifier(Modifier::BOLD);
    let dim = theme.get(crate::theme::scope::UI_TEXT_DIM);
    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let mark = theme.get(crate::theme::scope::UI_TEXT_DISABLED);
    let matched = theme.list_match();
    let selected_style = theme.list_selected(crate::theme::scope::UI_SELECTION);
    let keeps_columns = theme
        .try_get(crate::theme::scope::UI_LIST_SELECTED)
        .is_some();
    let fallback = style_rgb(Some(crate::render::themed_fg(theme))).unwrap_or([0xff; 3]);
    let caption = theme.type_scale(TypeRole::Caption);
    let end = history_pixel_end(area, m);
    let left = m.cols(PIXEL_BOX_INSET);
    let line_height = u32::from(m.cell[1]);
    let height = u32::from(area.height) * line_height;

    let mut placer = Placer::default();
    let mut spans = Vec::new();
    let mut y = 0u32;
    for (place, laid) in laid.iter().enumerate().skip(start) {
        let line_top = |line: usize| y + m.px(ROW_PAD) + line as u32 * line_height;
        let fits = (0..laid.rows.len())
            .take_while(|&line| line_top(line) + line_height <= height)
            .count();
        if fits == 0 {
            break;
        }
        let lines = laid.rows.len() as u32;
        let is_selected = place == selected;
        if let Some(color) = style_rgb(selected_style.bg).filter(|_| is_selected) {
            // The window keeps a selected box whole unless the entry is
            // taller than the list, and that box stops in the whole
            // sixteenths over the list's bottom.
            let box_height = match fits == laid.rows.len() {
                true => m.box16(lines as u16),
                false => ((height - y) * 16 / line_height) as u16,
            };
            let bar = PixelBar {
                x: left as i16,
                y,
                width: area.width.saturating_mul(16).saturating_sub(2 * left),
                height: box_height,
                color,
                radius: SELECTED_ROW_RADIUS,
            };
            pixel_rows::bar(&mut placer, &m, area, &bar, buf, scene);
        }
        let column = |own: Style| match is_selected && !keeps_columns {
            true => selected_style,
            false => own,
        };
        let styles = (column(head_style), column(dim), matched);

        for (line, row) in laid.rows.iter().enumerate().take(fits) {
            let anchor = placer.line(&m, area, line_top(line), scene);
            if line == 0 {
                let mark_span = [("\u{21ba}", column(mark))];
                let (col, scale) = (HISTORY_PIXEL_MARK, FULL_CELL_SCALE);
                pixel_rows::emit_spans(&mark_span, col, scale, anchor, fallback, buf, scene);
            }
            let row_spans = history_row_spans(laid, row, styles);
            let (col, scale) = (HISTORY_PIXEL_LINE, FULL_CELL_SCALE);
            pixel_rows::emit_spans(&row_spans, col, scale, anchor, fallback, buf, scene);
            if let (0, Some(age)) = (line, &laid.age) {
                let age_x =
                    end.saturating_sub(text_run::advance_sixteenths(age.chars().count(), caption));
                let age_span = [(age.as_str(), column(muted))];
                pixel_rows::emit_spans(&age_span, age_x, caption, anchor, fallback, buf, scene);
            }
        }

        let pitch = m.pitch(lines);
        spans.push((y..y + pitch, laid.entry.index));
        y += pitch;
    }
    placer.end(scene);
    spans
}

/// The spans of the chars `row` of `laid`'s line, the head's chars in
/// `head`, the rest in `rest`, and each char inside one of the entry's marks
/// patched by `matched`.
fn history_row_spans(
    laid: &LaidEntry<'_>,
    row: &Range<usize>,
    (head, rest, matched): (Style, Style, Style),
) -> Vec<(String, Style)> {
    let style_at = |index: usize| {
        let own = match index < laid.head {
            true => head,
            false => rest,
        };
        match laid.entry.marks.iter().any(|mark| mark.contains(&index)) {
            true => own.patch(matched),
            false => own,
        }
    };
    let mut spans: Vec<(String, Style)> = Vec::new();
    for index in row.clone() {
        let style = style_at(index);
        match spans.last_mut() {
            Some((text, last)) if *last == style => text.push(laid.chars[index]),
            _ => spans.push((laid.chars[index].to_string(), style)),
        }
    }
    spans
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
/// head that [`split_head`] finds against `availability` and `commands`, else
/// the line's first word.
fn history_head_chars(line: &str, availability: &Availability, commands: &CommandSet) -> usize {
    let head = match split_head(line, availability, commands) {
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
/// `selected` fit in `rows`. `heights` holds the height of each entry, newest
/// first, in the unit of `rows`: cell rows for cell rows, or physical px for
/// pixel rows. A selected entry taller than the list starts the window
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
/// `resolved` command token draws with its ghost, and the `arg_ghost` of an
/// empty argument follows the typed text. The chars inside `marks` draw
/// marked. [`input_spans`] gives each of these spans.
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
    arg_ghost: Option<&str>,
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
    let (input, caret) = input_spans(&text, caret, resolved, arg_ghost, marks, theme);
    let line = CmdLine {
        cwd: &cwd,
        input: &input,
        caret: Some(caret),
        status,
    };
    cmd_line::cmd_line(buf, inner, &line, theme, scene);
    cmd_line::cmd_line_rows(&line, inner.width, theme, scene)
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
/// The `arg_ghost` of an empty argument follows the typed text in
/// `ui.text.muted`, under the `ui.cmd.ghost` rule of the head's ghost. It
/// moves no caret, because no typed char follows it. An input with no
/// `resolved` head has no argument in entry, so it draws no such ghost.
///
/// The chars inside `marks`, char ranges of `text`, take
/// [`Theme::list_match`](crate::theme::Theme::list_match) over the style they
/// draw in. So a mark wins over the head's token color. A ghost takes no
/// mark, because it is no part of the typed text.
fn input_spans(
    text: &str,
    caret: usize,
    resolved: Option<&ResolvedCommand>,
    arg_ghost: Option<&str>,
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
    let head_style = match &resolved.entry {
        PaletteCommand::Action(entry) if !entry.is_command() => theme
            .try_get(crate::theme::scope::UI_CMD_ACTION)
            .unwrap_or(command),
        _ => command,
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
    spans.extend(arg_ghost_span(arg_ghost, theme));
    (spans, shown)
}

/// The span of `ghost`, the default of an empty argument, in `ui.text.muted`.
/// `None` for no ghost, an empty one, or a theme that leaves `ui.cmd.ghost`
/// unset.
fn arg_ghost_span(ghost: Option<&str>, theme: &crate::theme::Theme) -> Option<(String, Style)> {
    theme.try_get(crate::theme::scope::UI_CMD_GHOST)?;
    let ghost = ghost.filter(|ghost| !ghost.is_empty())?;
    Some((
        ghost.to_owned(),
        theme.get(crate::theme::scope::UI_TEXT_MUTED),
    ))
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
/// At the root it counts the commands on show, the user commands among them.
/// Then it names the `action` row and the internal actions that the scope
/// offers behind it, as `11 commands · action for 1,204 more`. The Config and
/// Cli scopes list no `action` row, so their status ends after the count.
/// Under the `action` token it counts the actions on show out of those that
/// the scope offers, as `8 of 1,204 actions`. The `action` token reads in the
/// `ui.cmd.action` foreground.
fn filter_status(palette: &CommandPalette, theme: &crate::theme::Theme) -> Vec<(String, Style)> {
    let more = grouped(palette.action_offering);
    if palette.namespace == Namespace::Actions {
        let rest = format!(" of {more} actions");
        return count_status(palette.filtered.len(), &rest, theme);
    }

    let shown = palette
        .filtered
        .iter()
        .filter(|command| !names_action_row(command))
        .count();
    let mut spans = count_status(shown, " commands", theme);
    if !palette.scope.lists_registry() {
        return spans;
    }

    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let token = match theme
        .try_get(crate::theme::scope::UI_CMD_ACTION)
        .and_then(|style| style.fg)
    {
        Some(fg) => muted.fg(fg),
        None => muted,
    };
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
/// [`palette_footer_parts`] gives them under `scope`. The rows and the doc of
/// a `cli` command read its loaded help in `specs`.
///
/// With `pixels` the list's rows are a pixel list's, which the palette's
/// resident pool draws, so the list paints no row of its own.
#[allow(clippy::too_many_arguments)]
fn render_palette_filter(
    input: &InputView,
    filtered: &[PaletteCommand],
    match_indices: &[Vec<u32>],
    selected: usize,
    group: Option<&str>,
    namespace: Namespace,
    scope: PaletteScope,
    cycle: &[PaletteScope],
    content_rows: u16,
    zoom: i8,
    header_rows: &mut u16,
    doc_scroll: &mut u16,
    resolved: Option<&ResolvedCommand>,
    status: &[(String, Style)],
    keys: &ActionKeys,
    specs: &HashMap<String, CliSpecState>,
    pixels: Option<RowMetrics>,
    ws: &mut Workspace,
    home: Option<&Path>,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let name_offset = group.map_or(0, |group| group.len() + 1);
    let Some(layout) =
        palette_filter_layout(area, content_rows, zoom, *header_rows, doc_rows(pixels))
    else {
        return;
    };
    *header_rows = render_palette_prelude(
        input,
        resolved,
        None,
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
    let Some(layout) =
        palette_filter_layout(area, content_rows, zoom, *header_rows, doc_rows(pixels))
    else {
        return;
    };

    let inner = layout.inner;
    let separator_style = theme.get(crate::theme::scope::UI_BORDER_INACTIVE);

    let list = layout.list;
    let doc = layout.doc;
    let plate = crate::render::chrome::plate_colors(theme, scene);
    if let Some(plate) = plate {
        let (list_bottom, doc_top) = match (doc.is_empty(), pixels) {
            (true, _) => (ISLAND_BOTTOM, ISLAND_GAP),
            (false, Some(_)) => (PIXEL_LIST_EDGE, PIXEL_DOC_EDGE),
            (false, None) => (ISLAND_GAP, ISLAND_GAP),
        };
        let list_edges = [ISLAND_TOP, ISLAND_SIDE, list_bottom, ISLAND_SIDE];
        crate::render::chrome::modal_island(list, list_edges, plate, theme, buf, scene);
        let doc_edges = [doc_top, ISLAND_SIDE, ISLAND_BOTTOM, ISLAND_SIDE];
        crate::render::chrome::modal_island(doc, doc_edges, plate, theme, buf, scene);
    }

    if pixels.is_none() {
        let scroll = selected.saturating_sub(list.height.saturating_sub(1) as usize);
        paint_palette_rows(
            filtered,
            match_indices,
            selected,
            name_offset,
            namespace,
            keys,
            specs,
            list,
            RowStart {
                result: scroll,
                skip: 0,
            },
            theme,
            true,
            None,
            buf,
            &mut *scene,
        );
    }

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
        let entry = filtered.get(selected);
        let width = doc_chars(&layout, pixels.is_some(), theme);
        let text = entry
            .map(|entry| doc_lines(entry, keys, width, theme, specs))
            .unwrap_or_default();
        let last_top = match pixels {
            Some(m) => pixel_doc_last_top(&text, doc.height, m),
            None => text.lines.len().saturating_sub(usize::from(doc.height)),
        };
        *doc_scroll = (*doc_scroll).min(last_top as u16);

        // The Source Tag and its note end the name line, which a scroll moves
        // off the doc.
        let tag = entry.map(doc_tag).filter(|_| *doc_scroll == 0);
        let scroll = usize::from(*doc_scroll);
        match pixels {
            Some(m) => paint_doc_pixels(doc, &text, (scroll, tag), theme, m, buf, scene),
            None => {
                let shown = &text.lines[scroll..];
                paint_doc_lines(buf, doc, shown, tag, &text.note, theme, scene);
            },
        }
    }

    let key_stage = match namespace {
        Namespace::Commands => KeyStage::Commands {
            scope,
            cycle,
            typed: !input.text(ws).is_empty(),
            recallable: !ws.palette_history.entries().is_empty(),
        },
        Namespace::Actions => KeyStage::Actions,
    };
    let stage = palette_footer_parts(key_stage, theme);
    stage.paint(layout.footer, plate, theme, buf, scene);
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
///
/// A pick list names the shell line that Enter runs and its refresh key. Its
/// note names where the command comes from, the label of the command's Source
/// Tag in the foreground of the tag's scope, `config` in `ui.cmd.source.config`
/// or `script` in `ui.cmd.source.script`. Then ` command`, a space, U+00B7, a
/// space, and `config.stcfg:<line>` follow in `ui.text.muted`.
///
/// The list of a `cli` command names its run, completion, and history keys,
/// with `complete flag` over the flags. Its note reads `cli` in the
/// foreground of `ui.cmd.source.cli`, then ` command`, a space, U+00B7, a
/// space, and `auto-wrapped` in `ui.text.muted`.
fn palette_footer_parts(stage: KeyStage<'_>, theme: &crate::theme::Theme) -> PaletteFooterParts {
    let hint = |key, word: &'static str| (key, Cow::Borrowed(word));
    match stage {
        KeyStage::Commands {
            scope,
            cycle,
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
            hints.push(("S-Tab", Cow::Owned(scope.next_names(cycle))));
            PaletteFooterParts {
                hints,
                note: vec![
                    (Cow::Borrowed("root = commands \u{b7} "), muted),
                    (Cow::Borrowed(":action"), key),
                    (Cow::Borrowed(" = internals"), muted),
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
        KeyStage::Pick {
            enter,
            line,
            tag: (label, scope),
        } => {
            let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
            let source = theme
                .try_get(scope)
                .and_then(|style| style.fg)
                .map_or(muted, |fg| muted.fg(fg));
            PaletteFooterParts {
                hints: vec![("Enter", Cow::Owned(enter)), hint("C-r", "refresh list")],
                note: vec![
                    (Cow::Borrowed(label), source),
                    (
                        Cow::Owned(format!(" command \u{b7} config.stcfg:{line}")),
                        muted,
                    ),
                ],
            }
        },
        KeyStage::Cli { kind } => {
            let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
            let cli = theme
                .try_get(crate::theme::scope::UI_CMD_SOURCE_CLI)
                .and_then(|style| style.fg)
                .map_or(muted, |fg| muted.fg(fg));
            let complete = match kind {
                CliListKind::Flags => "complete flag",
                CliListKind::Subcommands => "complete",
            };
            PaletteFooterParts {
                hints: vec![
                    hint("Enter", "run"),
                    hint("Tab", complete),
                    hint("\u{2191}", "history"),
                ],
                note: vec![
                    (Cow::Borrowed("cli"), cli),
                    (Cow::Borrowed(" command \u{b7} auto-wrapped"), muted),
                ],
            }
        },
    }
}

/// The key hints `hints` of an argument stage of `palette`, with the pair
/// `C-Enter run as Job` straight after the first `Enter` pair while the
/// parsed command starts a Cmd, as [`CommandPalette::starts_cmd`] reads it.
///
/// The pair names the key that runs such a Cmd as a Job. The other pairs
/// keep their order.
fn job_hints<W: Clone + From<&'static str>>(
    palette: &CommandPalette,
    hints: &[(&'static str, W)],
) -> Vec<(&'static str, W)> {
    let mut hints = hints.to_vec();
    if palette.starts_cmd()
        && let Some(at) = hints.iter().position(|(key, _)| *key == "Enter")
    {
        hints.insert(at + 1, ("C-Enter", W::from("run as Job")));
    }
    hints
}

/// Paint `lines` into the doc region `doc`, one per row from its top, each
/// line's spans one after another from the region's second cell, clipped at
/// its right edge.
///
/// The first line ends in `tag` one cell after its text, as [`paint_doc_tag`]
/// places it. The tag is a Source Tag's label and color scope, as
/// [`doc_tag`] gives them. The spans of `note` follow the tag one cell after
/// its box, or one cell after the text when no tag draws, and only while
/// `tag` is on show.
fn paint_doc_lines(
    buf: &mut Buffer,
    doc: Rect,
    lines: &[StyledLine],
    tag: Option<(&str, &str)>,
    note: &[(String, Style)],
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
            let tag_x = x.saturating_add(1);
            let tag_end = paint_doc_tag(buf, (tag_x, row, end), label, scope, theme, scene);
            let mut x = tag_end.map_or(tag_x, |tag_end| tag_end.saturating_add(1));
            for (text, style) in note {
                write_str_clipped(buf, x, row, text, *style, end);
                x = x.saturating_add(text.chars().count() as u16);
            }
        }
    }
}

/// Paint the Source Tag `label` in the colors of `scope` on a doc region's
/// first line, from cell `x` of `row`, as [`paint_source_tag`] draws it.
///
/// Answers the first cell past the tag's box, whose width
/// [`source_tag_width`] gives, or `None` for a tag that does not draw. A
/// theme that leaves `scope` unset draws no tag. A tag that does not end one
/// cell inside `end`, the region's right edge, does not draw.
fn paint_doc_tag(
    buf: &mut Buffer,
    (x, row, end): (u16, u16, u16),
    label: &str,
    scope: &str,
    theme: &crate::theme::Theme,
    scene: &mut stoat_widgets::ApcScene,
) -> Option<u16> {
    let style = theme.try_get(scope)?;
    let label_cells = label.chars().count() as u16;
    let cells = source_tag_width(label, theme).div_ceil(16).max(label_cells);
    if x.saturating_add(cells) >= end {
        return None;
    }

    // A dead scene ends the label one cell inside the rect it paints into, so
    // a rect one cell wider than the label puts the label where the live box
    // starts.
    let rect = Rect::new(x, row, label_cells + 1, 1);
    paint_source_tag(buf, rect, 0, label, style, theme, scene);
    Some(x + cells)
}

/// Paint `text` into `doc` from its line `scroll` as a pixel doc laid out by
/// `m`, with `tag`, a Source Tag's label and scope, on the name line.
///
/// The doc writes no cell. Each line stands where [`pixel_doc_tops`] puts it
/// and draws from [`PIXEL_DOC_COLUMN`], cut where the room between the doc's
/// side pads ends. The name line draws as [`paint_doc_name_line`] does, and
/// every other line at [`TypeRole::Ui`].
fn paint_doc_pixels(
    doc: Rect,
    text: &DocLines,
    (scroll, tag): (usize, Option<(&str, &str)>),
    theme: &crate::theme::Theme,
    m: RowMetrics,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let ui = theme.type_scale(TypeRole::Ui);
    let fallback = style_rgb(Some(crate::render::themed_fg(theme))).unwrap_or([0xff; 3]);
    let room = doc
        .width
        .saturating_mul(16)
        .saturating_sub(2 * PIXEL_DOC_COLUMN);
    let mut placer = Placer::default();
    for (index, top) in pixel_doc_tops(text, scroll, doc.height, m) {
        if index == 0 {
            let line = DocLine { doc, top, m };
            paint_doc_name_line(&line, text, tag, theme, &mut placer, buf, scene);
            continue;
        }
        let anchor = placer.line(&m, doc, top, scene);
        let shown = clip_spans(&text.lines[index], chars_within(room, ui));
        pixel_rows::emit_spans(&shown, PIXEL_DOC_COLUMN, ui, anchor, fallback, buf, scene);
    }
    placer.end(scene);
}

/// Where a line of a pixel doc stands, in `doc` at `top` physical px from
/// its top, laid out by `m`.
struct DocLine {
    doc: Rect,
    top: u32,
    m: RowMetrics,
}

/// Paint the name line of `text` at `line`, its name at the cell size, then
/// its aliases at [`TypeRole::Ui`], `tag`, and `text`'s note, each
/// [`PIXEL_DOC_PART_GAP`] after the part before it.
///
/// The tag is the chip of a pixel row, 1 px under the line's top, in the
/// colors of its scope. A theme that leaves the scope unset draws no tag, and
/// neither does a doc whose room ends before the tag does. The note then
/// follows the part before the tag. The aliases' span leads with the blank
/// that the cell doc writes before them, which the gap stands in for.
fn paint_doc_name_line(
    line: &DocLine,
    text: &DocLines,
    tag: Option<(&str, &str)>,
    theme: &crate::theme::Theme,
    placer: &mut Placer,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let DocLine { doc, top, m } = *line;
    let ui = theme.type_scale(TypeRole::Ui);
    let fallback = style_rgb(Some(crate::render::themed_fg(theme))).unwrap_or([0xff; 3]);
    let room_end = doc
        .width
        .saturating_mul(16)
        .saturating_sub(PIXEL_DOC_COLUMN);
    let gap = m.cols(PIXEL_DOC_PART_GAP);
    let advance = |spans: &[(&str, Style)], scale| {
        let chars = spans.iter().map(|(text, _)| text.chars().count()).sum();
        text_run::advance_sixteenths(chars, scale)
    };
    let room_from = |x: u16, scale| chars_within(room_end.saturating_sub(x), scale);

    let spans = text.lines.first().map_or(&[][..], Vec::as_slice);
    let (name, aliases) = spans.split_at(spans.len().min(1));
    let name = clip_spans(name, room_from(PIXEL_DOC_COLUMN, FULL_CELL_SCALE));
    let name_end = PIXEL_DOC_COLUMN + advance(&name, FULL_CELL_SCALE);
    let alias_x = name_end + gap;
    let aliases: Vec<(&str, Style)> = aliases
        .iter()
        .map(|(alias, style)| (alias.trim_start(), *style))
        .collect();
    let aliases = clip_spans(&aliases, room_from(alias_x, ui));
    let mut end = match aliases.is_empty() {
        true => name_end,
        false => alias_x + advance(&aliases, ui),
    };

    let chip = tag.and_then(|(label, scope)| {
        let style = theme.try_get(scope)?;
        let (x, width) = (end + gap, source_tag_width(label, theme));
        (x + width <= room_end).then_some((label, style, x, width))
    });
    if let Some((_, style, x, width)) = chip
        && let Some(color) = style_rgb(style.bg)
    {
        let bar = PixelBar {
            x: x as i16,
            y: top + m.px(1),
            width,
            height: m.rows16(PIXEL_TAG_HEIGHT),
            color,
            radius: SOURCE_TAG_RADIUS,
        };
        pixel_rows::bar(placer, &m, doc, &bar, buf, scene);
    }

    let anchor = placer.line(&m, doc, top, scene);
    pixel_rows::emit_spans(
        &name,
        PIXEL_DOC_COLUMN,
        FULL_CELL_SCALE,
        anchor,
        fallback,
        buf,
        scene,
    );
    pixel_rows::emit_spans(&aliases, alias_x, ui, anchor, fallback, buf, scene);
    if let Some((label, style, x, width)) = chip {
        TextRun {
            col: (x + SOURCE_TAG_INSET) as i16,
            row: 0,
            scale: theme.type_scale(TypeRole::Small),
            color: style_rgb(style.fg).unwrap_or(fallback),
            bg: None,
            bold: false,
            italic: false,
            follow: 0,
            anchor: None,
            text: label,
        }
        .render(anchor, buf, scene);
        end = x + width;
    }
    let note_x = end + gap;
    let note = clip_spans(&text.note, room_from(note_x, ui));
    pixel_rows::emit_spans(&note, note_x, ui, anchor, fallback, buf, scene);
}

/// The index of each line of `text` that a pixel doc `doc_height` cell rows
/// tall shows from its line `scroll`, laid out by `m`, with the physical px
/// from the doc's top to the line's top.
///
/// The first line shown stands [`PIXEL_DOC_TOP`] under the doc's top, and
/// each next line 18 px under the line before it. A line that starts an item
/// stands [`PIXEL_DOC_GAP`] lower. The items start at the usage line, at the
/// description, and at a last line that [`DocLines::tail`] sets apart. The
/// lines stop before the first whose bottom passes [`PIXEL_DOC_BOTTOM`] over
/// the bottom of the doc's last row.
fn pixel_doc_tops(
    text: &DocLines,
    scroll: usize,
    doc_height: u16,
    m: RowMetrics,
) -> Vec<(usize, u32)> {
    let line_height = u32::from(m.cell[1]);
    let limit = (u32::from(doc_height) * line_height).saturating_sub(m.px(PIXEL_DOC_BOTTOM));
    let last = text.lines.len().saturating_sub(1);
    let starts_item = |index: usize| index == 1 || index == 2 || (text.tail && index == last);

    let mut tops = Vec::new();
    let mut y = m.px(PIXEL_DOC_TOP);
    for index in scroll..text.lines.len() {
        if index > scroll && starts_item(index) {
            y += m.px(PIXEL_DOC_GAP);
        }
        if y + line_height > limit {
            break;
        }
        tops.push((index, y));
        y += line_height;
    }
    tops
}

/// The first line of `text` from which a pixel doc `doc_height` cell rows
/// tall, laid out by `m`, shows every later line, where the doc's scroll
/// stops.
///
/// A doc too short for its last line stops its scroll at the first line.
fn pixel_doc_last_top(text: &DocLines, doc_height: u16, m: RowMetrics) -> usize {
    (0..text.lines.len())
        .find(|&scroll| {
            pixel_doc_tops(text, scroll, doc_height, m).len() == text.lines.len() - scroll
        })
        .unwrap_or(0)
}

/// The help of `command` as doc-region lines, each a run of styled spans.
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
/// A user command's doc reads its name in bold, its usage line, and its `doc`
/// text wrapped to `width`. Its last line names where the config declares it,
/// as the label of its Source Tag, ` command`, a space, U+00B7, a space, and
/// `config.stcfg:<line>`.
///
/// Beside the lines, the answer holds the note that stands after the Source
/// Tag, which [`cli_note`] words for a `cli` command from its help in
/// `specs`. Every other command has no note.
///
/// The argument stage names the parameter that it collects, so no line names
/// a parameter.
fn doc_lines(
    command: &PaletteCommand,
    keys: &ActionKeys,
    width: usize,
    theme: &crate::theme::Theme,
    specs: &HashMap<String, CliSpecState>,
) -> DocLines {
    let text = theme.get(crate::theme::scope::UI_TEXT);
    let muted = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let dim = theme.get(crate::theme::scope::UI_TEXT_DIM);
    let key_label = theme.get(crate::theme::scope::UI_KEY_LABEL);
    let usage = vec![(usage_line(command, specs), text)];
    let entry = match command {
        PaletteCommand::Action(entry) => *entry,
        PaletteCommand::User(user) => {
            let mut lines = vec![
                vec![(user.name.clone(), text.add_modifier(Modifier::BOLD))],
                usage,
            ];
            lines.extend(
                wrap_text(&user.doc, width)
                    .into_iter()
                    .map(|line| vec![(line, dim)]),
            );
            let (label, _) = doc_tag(command);
            let defined = format!("{label} command \u{b7} config.stcfg:{}", user.line);
            lines.push(vec![(defined, muted)]);
            return DocLines {
                lines,
                tail: true,
                note: cli_note(user, specs, muted),
            };
        },
    };
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

    let mut lines = vec![name, usage];
    lines.extend(
        wrap_text(entry.def.long_desc(), width)
            .into_iter()
            .map(|line| vec![(line, dim)]),
    );
    if is_action_row(entry) {
        return DocLines {
            lines,
            ..DocLines::default()
        };
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
        return DocLines {
            lines,
            tail: bound.is_some(),
            note: Vec::new(),
        };
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
    DocLines {
        lines,
        tail: true,
        note: Vec::new(),
    }
}

/// The help of a command as a doc's lines, as [`doc_lines`] words it.
#[derive(Debug, Default, PartialEq)]
struct DocLines {
    /// The doc's lines, each a run of styled spans. They are the name line,
    /// the usage line, the description's wrapped lines, and a last line apart
    /// from them.
    lines: Vec<StyledLine>,
    /// Whether the doc ends in a line apart from the description.
    ///
    /// That line names the action that a command runs, the keys that reach an
    /// internal action, or where a user command is defined. A pixel doc stands
    /// it apart from the description, as it stands each item apart.
    tail: bool,
    /// The spans that stand after the Source Tag on the name line.
    note: StyledLine,
}

/// The note that stands after the Source Tag on the first line of `user`'s
/// doc, in `muted`, or no span for a command that wraps no tool.
///
/// The note reads `wrapped from <source>`, where the source is the line that
/// the load of the tool's help reads, as [`run::help_source`] names it. Once
/// the help in `specs` lands, a space, U+00B7, a space, and the tool's version
/// follow, when the version run gave one. A tool that the load did not find
/// reads `<tool>: not found`.
fn cli_note(user: &UserCommand, specs: &HashMap<String, CliSpecState>, muted: Style) -> StyledLine {
    let RunSpec::Cli { tool, completions } = &user.run else {
        return Vec::new();
    };
    let text = match loaded_cli(user, specs) {
        Some(loaded) if loaded.missing => format!("{tool}: not found"),
        Some(LoadedCli {
            source,
            version: Some(version),
            ..
        }) => format!("wrapped from {source} \u{b7} {version}"),
        Some(loaded) => format!("wrapped from {}", loaded.source),
        None => format!(
            "wrapped from {}",
            run::help_source(tool, completions.as_deref())
        ),
    };
    vec![(text, muted)]
}

/// The help of the tool that `user` wraps, as it landed in `specs`, or `None`
/// for a command that wraps no tool and for a help that has not landed.
fn loaded_cli<'a>(
    user: &UserCommand,
    specs: &'a HashMap<String, CliSpecState>,
) -> Option<&'a LoadedCli> {
    if !matches!(user.run, RunSpec::Cli { .. }) {
        return None;
    }
    match specs.get(&user.name)? {
        CliSpecState::Ready(loaded) => Some(loaded),
        CliSpecState::Loading => None,
    }
}

/// The text that names `command` in the palette's input, as [`typed_head`]
/// gives it, followed by its [`signature`].
///
/// An internal action reads under the `action` token, as in `action undo`,
/// because only that form runs it.
fn usage_line(command: &PaletteCommand, specs: &HashMap<String, CliSpecState>) -> String {
    let head = typed_head(command);
    let signature = signature(command, specs);
    match signature.is_empty() {
        true => head,
        false => format!("{head} {signature}"),
    }
}

/// Each parameter of `command`, as `<name>` when the parameter is required and
/// `[name]` when it is optional, one space apart. A user command reads as its
/// [`UserCommand::signature`] writes it, and a `cli` command reads the usage
/// of its help in `specs` once that help lands with one.
fn signature(command: &PaletteCommand, specs: &HashMap<String, CliSpecState>) -> String {
    let entry = match command {
        PaletteCommand::Action(entry) => entry,
        PaletteCommand::User(user) => {
            return loaded_cli(user, specs)
                .and_then(|loaded| loaded.spec.usage.clone())
                .unwrap_or_else(|| user.signature());
        },
    };
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

/// Paint command-palette result rows into `area` from `start`, one row per
/// entry, with the selected row and the fuzzy-match characters highlighted.
///
/// Shared by the live list, which derives `start` from the selection, and
/// the smooth-scroll pool, which paints absolute pages, so both render
/// identical rows. The selected row draws through
/// [`chrome::selected_row`](crate::render::chrome::selected_row) with
/// `panels`, which a pool page passes false.
///
/// A row reads the command's name in bold, its first alias, its
/// [`signature`] with the loaded help of `specs`, and its short description,
/// one cell apart, and ends in its Source Tag as [`paint_source_tag`] draws
/// it. The name and alias columns take [`NAME_COLUMNS`] and [`ALIAS_COLUMNS`]
/// cells and grow to the longest name and alias on show, because a cut name
/// names nothing. The signature clips at [`SIGNATURE_COLUMNS`], because the
/// doc region shows the whole usage line.
///
/// The name of an entry that is no command, an internal action or the
/// `action` row, draws in `ui.cmd.action`. Under a live scene a 1 px rule in
/// `ui.divider` sets the `action` row off from the commands above it. No rule
/// draws when the row leads the whole list.
///
/// Under [`Namespace::Actions`] in `namespace`, a row reads the action's
/// PascalCase name, its kebab name, its keys in `keys`, and its short
/// description instead, as [`row_parts`] words them, and the row draws no
/// Source Tag. Its two name columns take [`ACTION_NAME_COLUMNS`] cells and
/// grow as the root's name column grows, and the keys clip at
/// [`ACTION_KEY_COLUMNS`].
///
/// Each name leaves out its first `name_offset` bytes, the group token and
/// space a grouped list's title already shows, and so does an alias that
/// starts with them. A match position indexes the whole name, so one inside
/// the left-out part paints nothing.
///
/// With `pixels` the rows are a pixel list's, laid out by those metrics from
/// `start`'s row with its first `skip` px above `area`, and they write no
/// cell. A row's box is 20 px high, 1 px from the next and 1 px inside each
/// end of the list. Each column starts [`PIXEL_COLUMN_GAP`] after the width of
/// the column before it, where a width counts chars at the column's own size:
/// the name and the alias at the cell size, and the signature, the kebab name,
/// the keys, and the description at [`TypeRole::Ui`]. The name columns take
/// the longest of the whole list, because two pages must agree. The tag is a
/// 16 px chip 2 px under the box's top that ends 8 px inside the box, and the
/// `action` row stands [`action_lead`] lower, its rule in that space.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_palette_rows(
    filtered: &[PaletteCommand],
    match_indices: &[Vec<u32>],
    selected: usize,
    name_offset: usize,
    namespace: Namespace,
    keys: &ActionKeys,
    specs: &HashMap<String, CliSpecState>,
    area: Rect,
    start: RowStart,
    theme: &crate::theme::Theme,
    panels: bool,
    pixels: Option<RowMetrics>,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let list = PaletteList {
        filtered,
        match_indices,
        selected,
        name_offset,
        namespace,
        keys,
        specs,
        theme,
    };
    if let Some(metrics) = pixels {
        paint_pixel_palette_rows(&list, area, start, metrics, buf, scene);
        return;
    }
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }

    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let keeps_columns = theme
        .try_get(crate::theme::scope::UI_LIST_SELECTED)
        .is_some();

    let on_show = || filtered.iter().skip(start.result).take(rows);
    let widest = |cells: fn(&PaletteCommand, usize) -> &str, least: u16| {
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
            widest(pascal_name, ACTION_NAME_COLUMNS),
            widest(shown_name, ACTION_NAME_COLUMNS),
        )
    });

    let divider = style_rgb(theme.get(crate::theme::scope::UI_DIVIDER).fg).filter(|_| scene.live());
    let end_x = area.x + area.width;

    for (row_idx, entry) in on_show().enumerate() {
        let abs = start.result + row_idx;
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
        if names_action_row(entry)
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

        let parts = row_parts(&list, abs, column);
        let name_x = area.x + 1;
        let (starts, ends) = match action_cols {
            Some((pascal_cols, kebab_cols)) => {
                let kebab_x = name_x.saturating_add(pascal_cols + 1);
                let keys_x = kebab_x.saturating_add(kebab_cols + 1);
                let keys_end = keys_x.saturating_add(ACTION_KEY_COLUMNS).min(end_x);
                let desc_x = keys_x.saturating_add(ACTION_KEY_COLUMNS + 1);
                (
                    [name_x, kebab_x, keys_x, desc_x],
                    [end_x, end_x, keys_end, end_x.saturating_sub(1)],
                )
            },
            None => {
                let alias_x = name_x.saturating_add(name_cols + 1);
                let signature_x = alias_x.saturating_add(alias_cols + 1);
                let signature_end = signature_x.saturating_add(SIGNATURE_COLUMNS).min(end_x);
                let desc_x = signature_x.saturating_add(SIGNATURE_COLUMNS + 1);
                let desc_end = match parts.tag {
                    Some((label, _)) => {
                        area.x + (cell_tag_start(label, area, theme) / 16).saturating_sub(1)
                    },
                    None => end_x.saturating_sub(1),
                };
                (
                    [name_x, alias_x, signature_x, desc_x],
                    [end_x, end_x, signature_end, desc_end],
                )
            },
        };
        write_columns(buf, row.y, &parts, starts, ends);

        if let Some((label, style)) = parts.tag {
            let start = cell_tag_start(label, area, theme);
            paint_source_tag(buf, row, start, label, style, theme, scene);
        }
    }
}

/// What one paint of the palette's list reads its rows from.
struct PaletteList<'a> {
    filtered: &'a [PaletteCommand],
    match_indices: &'a [Vec<u32>],
    selected: usize,
    name_offset: usize,
    namespace: Namespace,
    keys: &'a ActionKeys,
    specs: &'a HashMap<String, CliSpecState>,
    theme: &'a crate::theme::Theme,
}

/// The text of one row of the palette's list, which both of the list's forms
/// draw.
struct RowParts<'a> {
    /// The row's four columns from its left, each as spans of one style. A
    /// command reads its name, its first alias, its [`signature`], and its
    /// short description. A row of the `action` list reads its PascalCase
    /// name, its kebab name, its keys, and its short description.
    columns: [Vec<(Cow<'a, str>, Style)>; 4],
    /// The label and the style of the Source Tag that ends the row.
    tag: Option<(&'static str, Style)>,
}

/// The text of row `index` of `list`, each column in the style that `column`
/// gives the column's own style.
///
/// A command's name is bold, or in `ui.cmd.action` for an entry that is no
/// command, an internal action or the `action` row. Its alias and signature
/// read `ui.text.muted`, and its description `ui.text.dim`. A row of the
/// `action` list reads its PascalCase name in `ui.cmd.action`, its kebab name
/// in `ui.text.muted`, the labels of the keys that bind the action in
/// `ui.key_label`, and its description in `ui.text.dim`. The `action` list
/// draws no Source Tag.
///
/// Each char at a position in the row's match indices stands in a span of its
/// own, in the column's style patched by the list's match style. A position
/// indexes the kebab name in the `action` list, and [`pascal_matches`] marks
/// the same letters of the PascalCase name. The signature clips at
/// [`SIGNATURE_COLUMNS`] chars and the keys at [`ACTION_KEY_COLUMNS`],
/// because the doc region shows them whole.
fn row_parts<'a>(
    list: &PaletteList<'a>,
    index: usize,
    column: impl Fn(Style) -> Style,
) -> RowParts<'a> {
    let theme = list.theme;
    let entry = &list.filtered[index];
    let indices = list.match_indices.get(index).map_or(&[][..], Vec::as_slice);
    let matched = theme.list_match();
    let muted = column(theme.get(crate::theme::scope::UI_TEXT_MUTED));
    let dim = column(theme.get(crate::theme::scope::UI_TEXT_DIM));
    let desc = match entry {
        PaletteCommand::Action(action) => action.def.short_desc(),
        PaletteCommand::User(user) => user.doc.as_str(),
    };

    if list.namespace == Namespace::Actions {
        let PaletteCommand::Action(action) = entry else {
            return RowParts {
                columns: Default::default(),
                tag: None,
            };
        };
        let (pascal, kebab) = (action.def.name(), action.command_name.as_str());
        let labels = list
            .keys
            .get(pascal)
            .map(|labels| labels.join(" \u{b7} "))
            .unwrap_or_default();
        let key_label = column(theme.get(crate::theme::scope::UI_KEY_LABEL));
        let pascal_indices = pascal_matches(kebab, indices);
        return RowParts {
            columns: [
                match_spans(
                    pascal,
                    0,
                    &pascal_indices,
                    column(action_name_style(theme)),
                    matched,
                ),
                match_spans(kebab, 0, indices, muted, matched),
                vec![(Cow::Owned(clipped(labels, ACTION_KEY_COLUMNS)), key_label)],
                vec![(Cow::Borrowed(desc), dim)],
            ],
            tag: None,
        };
    }

    let own_name = match entry {
        PaletteCommand::Action(action) if !action.is_command() => action_name_style(theme),
        _ => theme
            .get(crate::theme::scope::UI_TEXT)
            .add_modifier(Modifier::BOLD),
    };
    let name = shown_name(entry, list.name_offset);
    let alias = shown_alias(entry, list.name_offset);
    let signature = clipped(signature(entry, list.specs), SIGNATURE_COLUMNS);
    RowParts {
        columns: [
            match_spans(name, list.name_offset, indices, column(own_name), matched),
            vec![(Cow::Borrowed(alias), muted)],
            vec![(Cow::Owned(signature), muted)],
            vec![(Cow::Borrowed(desc), dim)],
        ],
        tag: source_tag(entry).and_then(|(label, scope)| Some((label, theme.try_get(scope)?))),
    }
}

/// The spans of `text` in `own`, where each stretch of chars at positions in
/// `indices` stands in a span of its own in `own` patched by `matched`.
///
/// A row leaves out a name's first `offset` chars, and `indices` index the
/// whole name, so the text's char `i` sits at position `offset + i`.
pub(super) fn match_spans<'a>(
    text: &'a str,
    offset: usize,
    indices: &[u32],
    own: Style,
    matched: Style,
) -> Vec<(Cow<'a, str>, Style)> {
    let style = |hit: bool| match hit {
        true => own.patch(matched),
        false => own,
    };
    let mut spans = Vec::new();
    let (mut start, mut run) = (0, None);
    for (index, (byte, _)) in text.char_indices().enumerate() {
        let hit = indices.binary_search(&((offset + index) as u32)).is_ok();
        if let Some(was) = run.filter(|&was| was != hit) {
            spans.push((Cow::Borrowed(&text[start..byte]), style(was)));
            start = byte;
        }
        run = Some(hit);
    }
    if let Some(was) = run {
        spans.push((Cow::Borrowed(&text[start..]), style(was)));
    }
    spans
}

/// `text` cut to its first `chars` chars.
fn clipped(mut text: String, chars: u16) -> String {
    if let Some((end, _)) = text.char_indices().nth(usize::from(chars)) {
        text.truncate(end);
    }
    text
}

/// The sixteenth of a cell row of the list `area` that a Source Tag `label`
/// starts at, so its box ends one cell inside the list's right edge.
fn cell_tag_start(label: &str, area: Rect, theme: &crate::theme::Theme) -> u16 {
    area.width
        .saturating_sub(1)
        .saturating_mul(16)
        .saturating_sub(source_tag_width(label, theme))
}

/// Write the columns of `parts` into the cells of row `y`, each column from
/// its cell in `starts` and cut at its cell in `ends`.
fn write_columns(buf: &mut Buffer, y: u16, parts: &RowParts<'_>, starts: [u16; 4], ends: [u16; 4]) {
    for ((spans, mut x), end) in parts.columns.iter().zip(starts).zip(ends) {
        for (text, style) in spans {
            write_str_clipped(buf, x, y, text, *style, end);
            x = x.saturating_add(text.chars().count() as u16);
        }
    }
}

/// Paint the rows of `list` into `area` from `start` as the pixel rows that
/// [`paint_palette_rows`] describes, laid out by `m`.
fn paint_pixel_palette_rows(
    list: &PaletteList<'_>,
    area: Rect,
    start: RowStart,
    m: RowMetrics,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let theme = list.theme;
    let selected_style = theme.list_selected(crate::theme::scope::UI_SELECTION);
    let keeps_columns = theme
        .try_get(crate::theme::scope::UI_LIST_SELECTED)
        .is_some();
    let fallback = style_rgb(Some(crate::render::themed_fg(theme))).unwrap_or([0xff; 3]);
    let divider = style_rgb(theme.get(crate::theme::scope::UI_DIVIDER).fg);
    let small = theme.type_scale(TypeRole::Small);
    let columns = pixel_columns(list, theme.type_scale(TypeRole::Ui));

    let left = m.cols(PIXEL_BOX_INSET);
    let box_width = area.width.saturating_mul(16).saturating_sub(2 * left);
    let tag_end = area
        .width
        .saturating_mul(16)
        .saturating_sub(left + m.cols(PIXEL_ROW_PAD));
    let height = i64::from(area.height) * i64::from(m.cell[1]);
    let inside = |y: i64| (0..height).contains(&y);
    let mut placer = Placer::default();

    let mut y = -i64::from(start.skip);
    for index in start.result..list.filtered.len() {
        if y >= height {
            break;
        }
        let lead = action_lead(list.filtered, index, m);
        let rule = y + i64::from(m.px(ACTION_ROW_MARGIN));
        if let Some(color) = divider.filter(|_| lead > 0 && inside(rule)) {
            let bar = PixelBar {
                x: left as i16,
                y: rule as u32,
                width: box_width,
                height: 1,
                color,
                radius: 0,
            };
            pixel_rows::bar(&mut placer, &m, area, &bar, buf, scene);
        }
        let top = y + i64::from(lead);

        let is_selected = index == list.selected;
        if let Some(color) = style_rgb(selected_style.bg).filter(|_| is_selected && inside(top)) {
            let bar = PixelBar {
                x: left as i16,
                y: top as u32,
                width: box_width,
                height: m.box16(1),
                color,
                radius: SELECTED_ROW_RADIUS,
            };
            pixel_rows::bar(&mut placer, &m, area, &bar, buf, scene);
        }

        let column = |own: Style| match is_selected && !keeps_columns {
            true => selected_style,
            false => own,
        };
        let parts = row_parts(list, index, column);
        let tag = parts.tag.map(|(label, style)| {
            let width = source_tag_width(label, theme);
            (label, style, tag_end.saturating_sub(width), width)
        });
        let tag_top = top + i64::from(m.px(PIXEL_TAG_DROP));
        if let Some((_, style, x, width)) = tag
            && let Some(color) = style_rgb(style.bg).filter(|_| inside(tag_top))
        {
            let bar = PixelBar {
                x: x as i16,
                y: tag_top as u32,
                width,
                height: m.rows16(PIXEL_TAG_HEIGHT),
                color,
                radius: SOURCE_TAG_RADIUS,
            };
            pixel_rows::bar(&mut placer, &m, area, &bar, buf, scene);
        }

        let line = top + i64::from(m.px(ROW_PAD));
        if inside(line) {
            let anchor = placer.line(&m, area, line as u32, scene);
            let desc_end = match tag {
                Some((_, _, x, _)) => x.saturating_sub(PIXEL_COLUMN_GAP),
                None => tag_end,
            };
            let mut col = PIXEL_FIRST_COLUMN;
            for (spans, (scale, width)) in parts.columns.iter().zip(columns) {
                match width {
                    Some(width) => {
                        pixel_rows::emit_spans(spans, col, scale, anchor, fallback, buf, scene);
                        col = col.saturating_add(width + PIXEL_COLUMN_GAP);
                    },
                    None => {
                        let room = chars_within(desc_end.saturating_sub(col), scale);
                        let shown = clip_spans(spans, room);
                        pixel_rows::emit_spans(&shown, col, scale, anchor, fallback, buf, scene);
                    },
                }
            }
            if let Some((label, style, x, _)) = tag {
                TextRun {
                    col: (x + SOURCE_TAG_INSET) as i16,
                    row: 0,
                    scale: small,
                    color: style_rgb(style.fg).unwrap_or(fallback),
                    bg: None,
                    bold: false,
                    italic: false,
                    follow: 0,
                    anchor: None,
                    text: label,
                }
                .render(anchor, buf, scene);
            }
        }
        y += i64::from(row_pitch(list.filtered, index, m));
    }
    placer.end(scene);
}

/// The text scale of each column of a pixel row of `list`, and the width in
/// sixteenths of each but the description, which takes the rest of the row.
///
/// A width counts chars at the column's own scale: the name and the alias at
/// the cell size, and the rest at `ui`. The name and alias columns, and both
/// name columns of the `action` list, take the longest of the whole list.
fn pixel_columns(list: &PaletteList<'_>, ui: u16) -> [(u16, Option<u16>); 4] {
    let widest = |cells: fn(&PaletteCommand, usize) -> &str, least: u16| {
        list.filtered
            .iter()
            .map(|entry| cells(entry, list.name_offset).chars().count())
            .max()
            .unwrap_or(0)
            .max(usize::from(least))
    };
    let column =
        |scale: u16, chars: usize| (scale, Some(text_run::advance_sixteenths(chars, scale)));
    match list.namespace {
        Namespace::Commands => [
            column(FULL_CELL_SCALE, widest(shown_name, NAME_COLUMNS)),
            column(FULL_CELL_SCALE, widest(shown_alias, ALIAS_COLUMNS)),
            column(ui, usize::from(SIGNATURE_COLUMNS)),
            (ui, None),
        ],
        Namespace::Actions => [
            column(FULL_CELL_SCALE, widest(pascal_name, ACTION_NAME_COLUMNS)),
            column(ui, widest(shown_name, ACTION_NAME_COLUMNS)),
            column(ui, usize::from(ACTION_KEY_COLUMNS)),
            (ui, None),
        ],
    }
}

/// The spans of `spans` cut to their first `chars` chars.
fn clip_spans<S: AsRef<str>>(spans: &[(S, Style)], chars: usize) -> Vec<(&str, Style)> {
    let mut left = chars;
    spans
        .iter()
        .map_while(|(text, style)| {
            (left > 0).then(|| {
                let shown = clip_to_width(text.as_ref(), left);
                left -= shown.chars().count();
                (shown, *style)
            })
        })
        .collect()
}

/// The chars that a line of the palette's doc holds in `layout`.
///
/// A pixel doc holds the [`TypeRole::Ui`] chars between its two
/// [`PIXEL_DOC_COLUMN`] pads. A doc on cell rows holds the box's inner cells
/// less one.
fn doc_chars(layout: &PaletteFilterLayout, pixels: bool, theme: &crate::theme::Theme) -> usize {
    match pixels {
        true => {
            let room = layout
                .doc
                .width
                .saturating_mul(16)
                .saturating_sub(2 * PIXEL_DOC_COLUMN);
            chars_within(room, theme.type_scale(TypeRole::Ui))
        },
        false => usize::from(layout.inner.width.saturating_sub(1)),
    }
}

/// The most chars whose advance at `scale` fits in `room` sixteenths, as
/// [`text_run::advance_sixteenths`] rounds an advance.
fn chars_within(room: u16, scale: u16) -> usize {
    (usize::from(room) * 16 + 7) / usize::from(scale.max(1))
}

/// The physical px over row `index`'s box of `filtered` that the `action`
/// row takes for its margin, its rule, and the pad under the rule, or 0 for
/// any other row and for an `action` row that leads the list.
fn action_lead(filtered: &[PaletteCommand], index: usize, m: RowMetrics) -> u32 {
    match index > 0 && filtered.get(index).is_some_and(names_action_row) {
        true => m.px(ACTION_ROW_MARGIN) + m.px(ACTION_ROW_RULE) + m.px(ACTION_ROW_PAD),
        false => 0,
    }
}

/// The physical px from the top of row `index` of `filtered` to the top of
/// the row after it, in a pixel list laid out by `m`.
fn row_pitch(filtered: &[PaletteCommand], index: usize, m: RowMetrics) -> u32 {
    m.pitch(1) + action_lead(filtered, index, m)
}

/// The physical px from the top of the palette's pixel list to the top of the
/// space that row `index` takes, laid out by `m`.
///
/// The filter puts the `action` row, the one row with a lead, at the list's
/// end, or at its start where the row takes no lead. So every row's space
/// starts a whole [`RowMetrics::pitch`] of one line after the row above it.
pub(crate) fn command_top(index: usize, m: RowMetrics) -> u32 {
    (index as u32).saturating_mul(m.pitch(1))
}

/// The physical px that the box of row `index` of `filtered` covers in the
/// palette's pixel list, laid out by `m`, from the list's top.
///
/// The `action` row's box stands its [`action_lead`] under the top of its
/// space.
pub(crate) fn command_box(filtered: &[PaletteCommand], index: usize, m: RowMetrics) -> Range<u32> {
    let top = command_top(index, m) + action_lead(filtered, index, m);
    top..top + m.box_height(1)
}

/// The window of the palette's pixel list `list_rows` cells tall around row
/// `selected` of `filtered`, laid out by `m`.
///
/// The pool scrolls to the window's first row, the hit test reads the row
/// under a press from it, and a page step moves by the rows it holds whole,
/// so the three agree. A row takes its [`row_pitch`].
pub(crate) fn command_window(
    filtered: &[PaletteCommand],
    selected: usize,
    list_rows: u16,
    m: RowMetrics,
) -> ListWindow {
    crate::render::picker::window_from(filtered.len(), selected, m.extent(list_rows), |index| {
        row_pitch(filtered, index, m)
    })
}

/// Where a page of the palette's pixel list whose first physical px row is
/// `at` starts, laid out by `m`, or `None` at or past the list's end.
///
/// The page starts on the row whose space holds `at`, as [`command_top`]
/// places each space, with the px of that space above `at` skipped.
pub(crate) fn command_page_start(
    filtered: &[PaletteCommand],
    at: u32,
    m: RowMetrics,
) -> Option<RowStart> {
    let last = filtered.len().checked_sub(1)?;
    let result = ((at / m.pitch(1).max(1)) as usize).min(last);
    let skip = at - command_top(result, m);
    (skip < row_pitch(filtered, result, m)).then_some(RowStart {
        result,
        skip: skip as u16,
    })
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

/// The part of `command`'s name a row shows, past its first `offset` bytes.
fn shown_name(command: &PaletteCommand, offset: usize) -> &str {
    command.name().get(offset..).unwrap_or("")
}

/// The first alias of `command` as a row shows it, or `""` for a command with
/// no alias, a user command among them.
///
/// An alias that starts with the first `offset` bytes of the command name, the
/// group token and its space, leaves them out as [`shown_name`] does.
fn shown_alias(command: &PaletteCommand, offset: usize) -> &str {
    let PaletteCommand::Action(entry) = command else {
        return "";
    };
    let alias = entry.def.aliases().first().copied().unwrap_or("");
    entry
        .command_name
        .get(..offset)
        .and_then(|token| alias.strip_prefix(token))
        .unwrap_or(alias)
}

/// The PascalCase name of `command`, which the first column of the `action`
/// list shows. A user command, which that list never holds, gives its name.
fn pascal_name(command: &PaletteCommand, _offset: usize) -> &str {
    match command {
        PaletteCommand::Action(entry) => entry.def.name(),
        PaletteCommand::User(user) => &user.name,
    }
}

/// The label and the color scope of the Source Tag that ends `command`'s row, or
/// `None` for an internal action, whose list names no source.
///
/// The `action` row reads `internal`. A command reads `built-in`, because every
/// command in the registry ships with stoat. A user command reads `config`,
/// `script` for a block command whose `run` lists its steps, or `cli` for a
/// command that wraps a tool.
pub(crate) fn source_tag(command: &PaletteCommand) -> Option<(&'static str, &'static str)> {
    let entry = match command {
        PaletteCommand::Action(entry) => entry,
        PaletteCommand::User(user) => {
            return Some(match user.run {
                RunSpec::Cli { .. } => ("cli", crate::theme::scope::UI_CMD_SOURCE_CLI),
                RunSpec::Steps { script: true, .. } => {
                    ("script", crate::theme::scope::UI_CMD_SOURCE_SCRIPT)
                },
                RunSpec::Steps { script: false, .. } => {
                    ("config", crate::theme::scope::UI_CMD_SOURCE_CONFIG)
                },
            });
        },
    };
    if is_action_row(entry) {
        return Some(INTERNAL_TAG);
    }
    entry
        .is_command()
        .then_some(("built-in", crate::theme::scope::UI_CMD_SOURCE_BUILTIN))
}

/// The label and the color scope of the Source Tag that ends the first line
/// of `command`'s doc.
///
/// The doc names the one action that the selected row names. So an internal
/// action's doc reads `internal`, although its row in the `action` list
/// draws no tag.
fn doc_tag(command: &PaletteCommand) -> (&'static str, &'static str) {
    source_tag(command).unwrap_or(INTERNAL_TAG)
}

/// Whether `command` is the palette's `action` row.
fn names_action_row(command: &PaletteCommand) -> bool {
    matches!(command, PaletteCommand::Action(entry) if is_action_row(entry))
}

/// The width of a Source Tag's box for `label`, in sixteenths of a cell.
///
/// The box holds the label at the [`TypeRole::Small`] size with
/// [`SOURCE_TAG_INSET`] on each side.
pub(super) fn source_tag_width(label: &str, theme: &crate::theme::Theme) -> u16 {
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
pub(super) fn paint_source_tag(
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
        block_pixels, command_box, command_page_start, command_window, count_status, doc_lines,
        grouped, history_age, history_window, input_spans, paint_arg_rows, paint_doc_lines,
        paint_doc_pixels, paint_history_rows, paint_palette_rows, palette_arg_layout,
        palette_filter_layout, pascal_matches, pick_status, pixel_doc_last_top,
        render_command_palette, run_block_origin, source_tag, usage_line, wrap_text, DocLines,
        HistoryEntry, DOC_ROWS, ISLAND_BOTTOM, ISLAND_GAP, ISLAND_SIDE, ISLAND_TOP, LIST_ROWS,
        PIXEL_DOC_EDGE, PIXEL_DOC_ROWS, PIXEL_LIST_EDGE,
    };
    use crate::{
        cli_spec::{self, RG_HELP},
        command_palette::{
            action_row, ArgList, ArgRow, Availability, CliListKind, HistorySpans, Namespace,
            PaletteBlock, PaletteCommand, PickLoad, PickState, ResolvedCommand,
        },
        host::ShellOutput,
        input_history,
        keymap::ActionKeys,
        picker::Preview,
        render::{
            editor::ResolvedChrome, paint::style_rgb, picker::RowStart, pixel_rows::RowMetrics,
        },
        run::Cmd,
        test_fixture::{plate_islands, row_rules},
        test_harness::TestHarness,
        theme::Theme,
        user_command::{
            run::{CliSpecState, LoadedCli},
            CommandSet, PickSpec,
        },
    };
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Modifier, Style},
    };
    use std::{collections::HashMap, ops::Range, path::PathBuf, sync::Arc};
    use stoat_action::registry;
    use stoat_config::Template;
    use stoatty_protocol::command::{decode_stream, BarCommand, Command, Edge, TextRunCommand};

    /// Each of `entries` as a palette row.
    fn rows(entries: &[&'static registry::RegistryEntry]) -> Vec<PaletteCommand> {
        entries
            .iter()
            .map(|&entry| PaletteCommand::Action(entry))
            .collect()
    }

    fn layout(area: Rect, content_rows: u16, zoom: i8) -> super::PaletteFilterLayout {
        palette_filter_layout(area, content_rows, zoom, 1, DOC_ROWS)
            .expect("the area hosts the palette")
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
            entry: PaletteCommand::Action(registry::lookup_alias("open").expect("a command")),
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
                input_spans("o src", 5, Some(&open), None, &[], &theme),
                input_spans("o", 1, Some(&open), None, &[], &theme),
                input_spans("o src", 5, Some(&open), None, &[], &Theme::empty()),
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
            &[PaletteCommand::Action(entry)],
            &[Vec::new()],
            0,
            0,
            Namespace::Commands,
            &ActionKeys::new(),
            &HashMap::new(),
            list,
            RowStart { result: 0, skip: 0 },
            &Theme::empty(),
            false,
            None,
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
        let two =
            palette_filter_layout(area, 4, 0, 2, DOC_ROWS).expect("the area hosts the palette");

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
        let usage = |name| {
            usage_line(
                &PaletteCommand::Action(registry::lookup_alias(name).expect("a command")),
                &HashMap::new(),
            )
        };
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
            &rows(&entries),
            &vec![Vec::new(); entries.len()],
            usize::MAX,
            name_offset,
            Namespace::Commands,
            &ActionKeys::new(),
            &HashMap::new(),
            list,
            RowStart { result: 0, skip: 0 },
            theme,
            false,
            None,
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
                    x: 1438,
                    y: 1,
                    width: 146,
                    height: 14,
                    color: [4, 5, 6],
                    occludes: false,
                    hatch: None,
                    radius: 4,
                    corners: 0,
                }),
                Command::TextRun(TextRunCommand {
                    col: 1447,
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
            &rows(&entries),
            &[Vec::new()],
            0,
            0,
            Namespace::Commands,
            &ActionKeys::new(),
            &HashMap::new(),
            list,
            RowStart { result: 0, skip: 0 },
            &theme,
            false,
            None,
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
                &rows(entries),
                &indices,
                usize::MAX,
                0,
                Namespace::Commands,
                &ActionKeys::new(),
                &HashMap::new(),
                list,
                RowStart { result: 0, skip: 0 },
                &theme,
                false,
                None,
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

    /// The metrics of a 9 by 18 px cell at a display scale of 1.
    const M1: RowMetrics = RowMetrics {
        cell: [9, 18],
        scale: 1.0,
    };

    /// The commands, less the ends of their runs, of the rows of `entries`
    /// painted from `start` as pixel rows by [`M1`] into the list (0, 0,
    /// `width`, 3) under `theme`, with row `selected` selected.
    fn pixel_commands(
        entries: &[&'static registry::RegistryEntry],
        selected: usize,
        theme: &Theme,
        start: RowStart,
        width: u16,
    ) -> Vec<Command> {
        let list = Rect::new(0, 0, width, 3);
        let mut scene = stoat_widgets::ApcScene::new();
        paint_palette_rows(
            &rows(entries),
            &vec![Vec::new(); entries.len()],
            selected,
            0,
            Namespace::Commands,
            &ActionKeys::new(),
            &HashMap::new(),
            list,
            start,
            theme,
            false,
            Some(M1),
            &mut Buffer::empty(list),
            &mut scene,
        );
        decode_stream(scene.bytes())
            .into_iter()
            .filter(|command| *command != Command::TextRunEnd)
            .collect()
    }

    /// A pixel row's run of `text` at `col` sixteenths on the cell row at
    /// `row` sixteenths, at the test build's full-cell scale.
    fn pixel_run(row: i16, col: i16, text: &str, color: [u8; 3], bold: bool) -> Command {
        Command::TextRun(TextRunCommand {
            col,
            row,
            scale: 256,
            color,
            bg: None,
            bold,
            italic: false,
            follow: 0,
            anchor: None,
            text: text.to_owned(),
        })
    }

    /// A pixel root row draws its tag as a chip 2 px under the row's top that
    /// ends 8 px inside the box, and its text 1 px under the top. The columns
    /// start 14 sixteenths after the width of the column before them, the
    /// name and the alias columns 13 and 3 cells wide and the signature 28
    /// chars. A run whose foreground has no RGB value draws in white.
    #[test]
    fn a_pixel_root_row_sets_its_columns_eight_pixels_apart() {
        let entries = [registry::lookup_alias("open").expect("a command")];
        let white = [0xff; 3];
        let top = RowStart { result: 0, skip: 0 };
        assert_eq!(
            pixel_commands(&entries, usize::MAX, &tag_theme(), top, 100),
            [
                Command::Nudge([0, 2]),
                Command::Bar(BarCommand {
                    x: 1438,
                    y: 0,
                    width: 146,
                    height: 14,
                    color: [4, 5, 6],
                    occludes: false,
                    hatch: None,
                    radius: 4,
                    corners: 0,
                }),
                Command::Nudge([0, 1]),
                pixel_run(0, 16, "open", white, true),
                pixel_run(0, 238, "o", white, false),
                pixel_run(0, 300, "<path>", white, false),
                pixel_run(0, 762, "open a file", white, false),
                pixel_run(0, 1447, "built-in", [1, 2, 3], false),
                Command::Nudge([0, 0]),
            ],
        );
    }

    /// The `action` row stands 7 px under the row above it. The 4 px of its
    /// margin come first, then its 1 px rule in `ui.divider` 1 px inside each
    /// end of the list, then 2 px under the rule. Its rule draws on pixel row
    /// 25 and its text on pixel row 29, both on cell row 1.
    #[test]
    fn a_pixel_action_row_stands_seven_pixels_under_the_row_above_it() {
        let theme = {
            let src = r##"theme t { ui.divider.fg = "#070707"; }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let open = registry::lookup_alias("open").expect("a command");
        let white = [0xff; 3];
        let top = RowStart { result: 0, skip: 0 };
        assert_eq!(
            pixel_commands(&[open, action_row()], usize::MAX, &theme, top, 100),
            [
                Command::Nudge([0, 1]),
                pixel_run(0, 16, "open", white, true),
                pixel_run(0, 238, "o", white, false),
                pixel_run(0, 300, "<path>", white, false),
                pixel_run(0, 762, "open a file", white, false),
                Command::Nudge([0, 7]),
                Command::Bar(BarCommand {
                    x: 2,
                    y: 16,
                    width: 1596,
                    height: 1,
                    color: [7, 7, 7],
                    occludes: false,
                    hatch: None,
                    radius: 0,
                    corners: 0,
                }),
                Command::Nudge([0, 11]),
                pixel_run(16, 16, "action", white, true),
                pixel_run(16, 300, "<action> [args]", white, false),
                pixel_run(
                    16,
                    762,
                    "Stoat's internal actions, the lower-level API",
                    white,
                    false
                ),
                Command::Nudge([0, 0]),
            ],
        );
    }

    /// Under a theme that sets `ui.list.selected`, a selected pixel row's box
    /// is a rounded bar in its background 1 px inside each end of the list,
    /// and each column keeps its own color over the box.
    #[test]
    fn a_selected_pixel_row_keeps_its_column_colors_over_its_box() {
        let theme = {
            let src = r##"theme t {
                ui.list.selected = { fg: "#000000", bg: "#7f7f7f" };
                ui.text.muted.fg = "#050505";
            }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let entries = [registry::lookup_alias("open").expect("a command")];
        let (white, muted) = ([0xff; 3], [5, 5, 5]);
        let top = RowStart { result: 0, skip: 0 };
        assert_eq!(
            pixel_commands(&entries, 0, &theme, top, 100),
            [
                Command::Bar(BarCommand {
                    x: 2,
                    y: 0,
                    width: 1596,
                    height: 18,
                    color: [0x7f; 3],
                    occludes: false,
                    hatch: None,
                    radius: 4,
                    corners: 0,
                }),
                Command::Nudge([0, 1]),
                pixel_run(0, 16, "open", white, true),
                pixel_run(0, 238, "o", muted, false),
                pixel_run(0, 300, "<path>", muted, false),
                pixel_run(0, 762, "open a file", white, false),
                Command::Nudge([0, 0]),
            ],
        );
    }

    /// A pixel row's description stops 14 sixteenths before its tag. In a
    /// list 60 cells wide the tag of `open` starts at 798, so the description
    /// from 762 has room for the one char that ends by 784.
    #[test]
    fn a_pixel_description_stops_fourteen_sixteenths_before_the_tag() {
        let entries = [registry::lookup_alias("open").expect("a command")];
        let top = RowStart { result: 0, skip: 0 };
        let runs: Vec<(i16, String)> = pixel_commands(&entries, usize::MAX, &tag_theme(), top, 60)
            .into_iter()
            .filter_map(|command| match command {
                Command::TextRun(run) => Some((run.col, run.text)),
                _ => None,
            })
            .collect();
        assert_eq!(
            runs,
            [
                (16, "open".to_owned()),
                (238, "o".to_owned()),
                (300, "<path>".to_owned()),
                (762, "o".to_owned()),
                (807, "built-in".to_owned()),
            ],
        );
    }

    /// A page that starts partway into a row draws no part of the row above
    /// its top. From 3 px into the first of two rows, the first row's box, tag,
    /// and text all stand above the page, and the second row's tag and text
    /// draw 2 and 1 px into cell row 1. From 5 px into the `action` row's space
    /// the rule above the page draws nothing, and the text draws 3 px down.
    #[test]
    fn a_pixel_page_draws_no_part_above_its_top() {
        let theme = {
            let src = r##"theme t {
                ui.list.selected = { fg: "#000000", bg: "#7f7f7f" };
                ui.cmd.source.builtin = { fg: "#010203", bg: "#040506" };
                ui.divider.fg = "#070707";
            }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let open = registry::lookup_alias("open").expect("a command");
        let commands_of = |entries: &[&'static registry::RegistryEntry], result, skip| {
            pixel_commands(entries, 0, &theme, RowStart { result, skip }, 100)
        };
        let tag = Command::Bar(BarCommand {
            x: 1438,
            y: 16,
            width: 146,
            height: 14,
            color: [4, 5, 6],
            occludes: false,
            hatch: None,
            radius: 4,
            corners: 0,
        });
        let white = [0xff; 3];

        assert_eq!(
            (
                commands_of(&[open, open], 0, 3),
                commands_of(&[open, action_row()], 1, 5),
            ),
            (
                vec![
                    Command::Nudge([0, 2]),
                    tag,
                    Command::Nudge([0, 1]),
                    pixel_run(16, 16, "open", white, true),
                    pixel_run(16, 238, "o", white, false),
                    pixel_run(16, 300, "<path>", white, false),
                    pixel_run(16, 762, "open a file", white, false),
                    pixel_run(16, 1447, "built-in", [1, 2, 3], false),
                    Command::Nudge([0, 0]),
                ],
                vec![
                    Command::Nudge([0, 3]),
                    pixel_run(0, 16, "action", white, true),
                    pixel_run(0, 300, "<action> [args]", white, false),
                    pixel_run(
                        0,
                        762,
                        "Stoat's internal actions, the lower-level API",
                        white,
                        false
                    ),
                    Command::Nudge([0, 0]),
                ],
            ),
        );
    }

    /// The `action` row's box stands its 7 px lead under the top of its space
    /// where a row stands above it, and on the top of its space where it leads
    /// the list.
    #[test]
    fn the_action_row_box_stands_its_lead_under_its_space() {
        let open = registry::lookup_alias("open").expect("a command");
        let (under, leading) = (rows(&[open, action_row()]), rows(&[action_row(), open]));
        assert_eq!(
            [
                command_box(&under, 1, M1),
                command_box(&leading, 0, M1),
                command_box(&leading, 1, M1),
            ],
            [28..48, 0..20, 21..41],
        );
    }

    /// The `action` row takes the 28 px of its lead and its row. So with the
    /// `action` row selected, 9 commands and the `action` row in a list 11
    /// cells tall, 199 px, show the `action` row and 8 rows above it, and the 9
    /// of them stand whole.
    #[test]
    fn the_command_window_counts_the_lead_of_the_action_row() {
        let mut filtered: Vec<PaletteCommand> = registry::all()
            .filter(|entry| entry.is_command())
            .take(9)
            .map(PaletteCommand::Action)
            .collect();
        filtered.push(PaletteCommand::Action(action_row()));

        let window = command_window(&filtered, 9, 11, M1);
        assert_eq!((window.first, window.whole(M1.extent(11))), (1, 9));
    }

    /// A page starts on the row whose space holds its first pixel row, with
    /// the px of that space above the page skipped. The `action` row's space
    /// takes 28 px from pixel row 42, and a page past it starts on no row.
    #[test]
    fn a_pixel_page_starts_on_the_row_whose_space_holds_its_first_pixel() {
        let open = registry::lookup_alias("open").expect("a command");
        let filtered = rows(&[open, open, action_row()]);
        let start = |result, skip| Some(RowStart { result, skip });

        assert_eq!(
            [0, 30, 42, 69, 70].map(|at| command_page_start(&filtered, at, M1)),
            [start(0, 0), start(1, 9), start(2, 0), start(2, 27), None],
        );
    }

    /// A doc that lays its lines out by pixels takes 8 rows, and the box keeps
    /// its height. So the body of 19 rows gives the list 11 rows and the doc 8,
    /// under the row between them.
    #[test]
    fn a_pixel_palette_gives_its_doc_eight_rows() {
        let area = Rect::new(0, 0, 160, 50);
        let cell = palette_filter_layout(area, 200, 0, 1, DOC_ROWS).expect("the area hosts it");
        let laid =
            palette_filter_layout(area, 200, 0, 1, PIXEL_DOC_ROWS).expect("the area hosts it");
        assert_eq!(
            (laid.modal, laid.list.height, laid.doc.height, laid.doc.y),
            (cell.modal, 11, 8, laid.list.bottom() + 1),
        );
    }

    /// The nudge, the row, and the text of each run of `text` painted from its
    /// line `scroll` as a pixel doc into `doc` by [`M1`] under `theme`, with
    /// `tag` on the name line.
    fn pixel_doc_runs(
        text: &DocLines,
        (scroll, tag): (usize, Option<(&str, &str)>),
        doc: Rect,
        theme: &Theme,
    ) -> Vec<(i16, i16, String)> {
        let mut scene = stoat_widgets::ApcScene::new();
        let mut buf = Buffer::empty(doc);
        paint_doc_pixels(doc, text, (scroll, tag), theme, M1, &mut buf, &mut scene);
        let mut dy = 0;
        decode_stream(scene.bytes())
            .into_iter()
            .filter_map(|command| match command {
                Command::Nudge([_, y]) => {
                    dy = y;
                    None
                },
                Command::TextRun(run) => Some((dy, run.row, run.text)),
                _ => None,
            })
            .collect()
    }

    /// A pixel doc stands its items 4 px apart from 6 px under its top, and a
    /// wrapped line of the description 18 px under the line before it. So the
    /// doc of `git log`, whose description wraps to two lines, draws its name
    /// line 6 px into its first row and its usage 10 px into its second. The
    /// description draws 14 px into the third and fourth rows, and the line
    /// naming the action and the key `Spc g l` on the sixth row, pixel row 90.
    #[test]
    fn a_pixel_doc_sets_its_items_four_pixels_apart() {
        let entry = registry::lookup_alias("git log").expect("a command");
        let keys = ActionKeys::from([("GitLs".to_owned(), vec!["Spc g l".to_owned()])]);
        let command = PaletteCommand::Action(entry);
        let text = doc_lines(&command, &keys, 140, &Theme::empty(), &HashMap::new());
        let mut lines: Vec<(i16, i16)> =
            pixel_doc_runs(&text, (0, None), Rect::new(0, 0, 150, 8), &Theme::empty())
                .into_iter()
                .map(|(dy, row, _)| (dy, row))
                .collect();
        lines.dedup();

        assert_eq!(
            (text.lines.len(), lines),
            (5, vec![(6, 0), (10, 16), (14, 32), (14, 48), (0, 80)]),
        );
    }

    /// The parts of a pixel doc's name line stand 10 px, 18 sixteenths, apart.
    /// The name `open` draws at the cell size from sixteenth 16 and ends at 80,
    /// and its alias draws from 98 without the blank that leads it in cells.
    /// The tag's chip stands from 132, 1 px under the line's top, with its
    /// label 9 sixteenths in, and the note follows the chip's end at 278.
    #[test]
    fn a_pixel_doc_name_line_sets_its_parts_ten_pixels_apart() {
        let white = [0xff; 3];
        assert_eq!(
            name_line_commands(" o", 100),
            [
                Command::Nudge([0, 7]),
                Command::Bar(BarCommand {
                    x: 132,
                    y: 0,
                    width: 146,
                    height: 14,
                    color: [4, 5, 6],
                    occludes: false,
                    hatch: None,
                    radius: 4,
                    corners: 0,
                }),
                Command::Nudge([0, 6]),
                pixel_run(0, 16, "open", white, false),
                pixel_run(0, 98, "o", white, false),
                pixel_run(0, 141, "built-in", [1, 2, 3], false),
                pixel_run(0, 296, "note", white, false),
                Command::Nudge([0, 0]),
            ],
        );
    }

    /// The commands, less the ends of their runs, of a pixel doc `width` cells
    /// wide whose one line names `open` with `aliases`, under [`tag_theme`],
    /// with the built-in tag and the note `note` after it.
    fn name_line_commands(aliases: &str, width: u16) -> Vec<Command> {
        let plain = |text: &str| (text.to_owned(), Style::default());
        let text = DocLines {
            lines: vec![vec![plain("open"), plain(aliases)]],
            tail: false,
            note: vec![plain("note")],
        };
        let tag = ("built-in", crate::theme::scope::UI_CMD_SOURCE_BUILTIN);
        let doc = Rect::new(0, 0, width, 8);
        let mut scene = stoat_widgets::ApcScene::new();
        let mut buf = Buffer::empty(doc);
        paint_doc_pixels(
            doc,
            &text,
            (0, Some(tag)),
            &tag_theme(),
            M1,
            &mut buf,
            &mut scene,
        );
        decode_stream(scene.bytes())
            .into_iter()
            .filter(|command| *command != Command::TextRunEnd)
            .collect()
    }

    /// A pixel doc cuts its name line where its room ends. In a doc 10 cells
    /// wide the room ends at sixteenth 144, so the alias `opener` from 98 keeps
    /// the 2 chars that fit there. The tag would end at 294, so it draws
    /// nothing, and the note finds no room after the alias. In a doc 4 cells
    /// wide the room ends at 48, so the name keeps its first 2 chars and no
    /// part after it fits.
    #[test]
    fn a_narrow_pixel_doc_cuts_its_name_line_at_its_room() {
        let white = [0xff; 3];
        assert_eq!(
            [
                name_line_commands(" opener", 10),
                name_line_commands(" opener", 4),
            ],
            [
                vec![
                    Command::Nudge([0, 6]),
                    pixel_run(0, 16, "open", white, false),
                    pixel_run(0, 98, "op", white, false),
                    Command::Nudge([0, 0]),
                ],
                vec![
                    Command::Nudge([0, 6]),
                    pixel_run(0, 16, "op", white, false),
                    Command::Nudge([0, 0]),
                ],
            ],
        );
    }

    /// A scrolled pixel doc starts its first line at the top pad, with no gap
    /// over it, although the line starts the description. The lines from
    /// `one` stand 18 px apart from 6 px under the doc's top.
    #[test]
    fn a_scrolled_pixel_doc_starts_at_its_top_pad() {
        let runs = pixel_doc_runs(
            &five_line_doc(),
            (2, None),
            Rect::new(0, 0, 60, 6),
            &Theme::empty(),
        );
        let lines: Vec<(i16, i16, &str)> = runs
            .iter()
            .map(|(dy, row, text)| (*dy, *row, text.as_str()))
            .collect();
        assert_eq!(
            lines,
            [
                (6, 0, "one"),
                (6, 16, "two"),
                (6, 32, "three"),
                (6, 48, "four"),
                (6, 64, "five"),
            ],
        );
    }

    /// A doc of the name, the usage, a description of five lines, and a last
    /// line apart from them, as `text` words it.
    fn five_line_doc() -> DocLines {
        let line = |text: &str| vec![(text.to_owned(), Style::default())];
        DocLines {
            lines: [
                "name", "usage", "one", "two", "three", "four", "five", "last",
            ]
            .map(line)
            .to_vec(),
            tail: true,
            note: Vec::new(),
        }
    }

    /// A pixel doc draws a line only while its 18 px end at or over the 8 px
    /// pad at its Island's bottom. In a doc of 6 rows that is pixel row 104, so
    /// the doc draws its name, its usage, and three of the description's five
    /// lines, and not its last line.
    #[test]
    fn a_pixel_doc_stops_at_its_bottom_pad() {
        let texts: Vec<String> = pixel_doc_runs(
            &five_line_doc(),
            (0, None),
            Rect::new(0, 0, 60, 6),
            &Theme::empty(),
        )
        .into_iter()
        .map(|(_, _, text)| text)
        .collect();
        assert_eq!(texts, ["name", "usage", "one", "two", "three"]);
    }

    /// A pixel doc's scroll stops at the first line from which every later
    /// line shows. In a doc of 6 rows the lines from `two` show with the last
    /// line 4 px apart, ending at pixel row 100, where from `one` the last line
    /// would end at 118, past the pad at 104.
    #[test]
    fn a_pixel_doc_scroll_stops_where_its_last_line_shows() {
        assert_eq!(pixel_doc_last_top(&five_line_doc(), 6, M1), 3);
    }

    /// The `action` row runs no action of its own, so its doc ends in its
    /// description with no `runs action` line.
    #[test]
    fn the_action_row_doc_ends_in_its_description() {
        let lines = doc_lines(
            &PaletteCommand::Action(action_row()),
            &ActionKeys::new(),
            400,
            &Theme::empty(),
            &HashMap::new(),
        )
        .lines;
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
            &rows(&entries),
            &vec![Vec::new(); entries.len()],
            usize::MAX,
            0,
            Namespace::Actions,
            keys,
            &HashMap::new(),
            list,
            RowStart { result: 0, skip: 0 },
            &Theme::empty(),
            false,
            None,
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

        let specs = HashMap::new();
        assert_eq!(
            (
                doc_lines(&PaletteCommand::Action(entry), &bound, 60, &theme, &specs),
                doc_lines(
                    &PaletteCommand::Action(entry),
                    &ActionKeys::new(),
                    60,
                    &theme,
                    &specs
                ),
            ),
            (
                DocLines {
                    lines: keyed,
                    tail: true,
                    note: Vec::new(),
                },
                DocLines {
                    lines: described,
                    tail: false,
                    note: Vec::new(),
                },
            ),
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

    /// A pixel History row draws no cell. Its line stands 1 px under the
    /// row's top, the U+21BA mark at 16 sixteenths and the line at 62 with
    /// the head bold. The age ends 8 px inside the row's box, at 624 in a list
    /// 40 cells wide. The row's span is its 20 px box and the 1 px under it.
    #[test]
    fn a_pixel_history_row_draws_its_mark_its_line_and_its_age() {
        let entries = [HistoryEntry {
            index: 0,
            line: "cd /alpha",
            at: Some(0),
            marks: Vec::new(),
        }];
        let white = [0xff; 3];
        assert_eq!(
            pixel_history(&entries, usize::MAX, &Theme::empty(), 6),
            (
                vec![
                    Command::Nudge([0, 1]),
                    pixel_run(0, 16, "\u{21ba}", white, false),
                    pixel_run(0, 62, "cd", white, true),
                    pixel_run(0, 94, " /alpha", white, false),
                    pixel_run(0, 592, "2m", white, false),
                    Command::Nudge([0, 0]),
                ],
                vec![(0..21, 0)],
                false,
            ),
        );
    }

    /// A pixel History line wraps after the last space that fits before the
    /// age, and its next line stands 18 px under it. The selected entry's box
    /// is a rounded bar 1 px inside each end of the list over both lines and
    /// their pads. The next entry starts 1 px under that box, so its line
    /// stands on pixel row 40, 4 px into cell row 2.
    #[test]
    fn a_pixel_history_line_wraps_after_the_last_space_that_fits() {
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
        let theme = {
            let src = r##"theme t {
                ui.list.selected = { fg: "#000000", bg: "#7f7f7f" };
                ui.text.muted.fg = "#050505";
            }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let (white, muted) = ([0xff; 3], [5, 5, 5]);
        let (commands, spans, _) = pixel_history(&entries, 0, &theme, 6);

        assert_eq!(
            (commands, spans),
            (
                vec![
                    Command::Bar(BarCommand {
                        x: 2,
                        y: 0,
                        width: 636,
                        height: 34,
                        color: [0x7f; 3],
                        occludes: false,
                        hatch: None,
                        radius: 4,
                        corners: 0,
                    }),
                    Command::Nudge([0, 1]),
                    pixel_run(0, 16, "\u{21ba}", white, false),
                    pixel_run(0, 62, "run", white, true),
                    pixel_run(0, 110, &format!(" {} ", "a".repeat(20)), white, false),
                    pixel_run(0, 592, "2m", muted, false),
                    pixel_run(16, 62, &"b".repeat(20), white, false),
                    Command::Nudge([0, 4]),
                    pixel_run(32, 16, "\u{21ba}", white, false),
                    pixel_run(32, 62, "cd", white, true),
                    pixel_run(32, 94, " /alpha", white, false),
                    pixel_run(32, 592, "2m", muted, false),
                    Command::Nudge([0, 0]),
                ],
                vec![(0..39, 7), (39..60, 3)],
            ),
        );
    }

    /// A pixel History word longer than its room breaks at the room's last
    /// cell, 32 cells from the line's start before an age and 35 for a line
    /// with no time.
    #[test]
    fn a_pixel_history_word_longer_than_a_row_breaks_at_the_last_cell() {
        let word = "x".repeat(40);
        let line_chars = |at| {
            let entries = [HistoryEntry {
                index: 0,
                line: &word,
                at,
                marks: Vec::new(),
            }];
            let (commands, _, _) = pixel_history(&entries, usize::MAX, &Theme::empty(), 6);
            commands
                .into_iter()
                .filter_map(|command| match command {
                    Command::TextRun(run) if run.col == 62 => Some(run.text.len()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!([line_chars(Some(0)), line_chars(None)], [[32, 8], [35, 5]]);
    }

    /// A pixel History row gives `ui.list.match` to the chars of each word of
    /// the needle at the word's first place in the line, over the weight of
    /// the head `git log`, and splits its runs where a mark starts and ends.
    #[test]
    fn a_pixel_history_row_marks_the_matched_words() {
        let line = "git log --graph stoat/src";
        let entries = [HistoryEntry {
            index: 0,
            line,
            at: None,
            marks: input_history::word_spans(line, "stoat git"),
        }];
        let (commands, _, _) = pixel_history(&entries, usize::MAX, &match_theme(), 6);
        let (text, matched) = ([4; 3], [7; 3]);
        assert_eq!(
            commands,
            [
                Command::Nudge([0, 1]),
                pixel_run(0, 16, "\u{21ba}", text, false),
                pixel_run(0, 62, "git", matched, true),
                pixel_run(0, 110, " log", text, true),
                pixel_run(0, 174, " --graph ", text, false),
                pixel_run(0, 318, "stoat", matched, false),
                pixel_run(0, 398, "/src", text, false),
                Command::Nudge([0, 0]),
            ],
        );
    }

    /// The pixel History window counts each entry's box and the 1 px under it
    /// against the list's px. In a list 10 rows tall the boxes of nine
    /// one-line entries end at pixel row 188, past the 180 that the list
    /// holds, so with the ninth selected the window starts at the second. The
    /// tenth's line would end at pixel row 187, past the list, so it and the
    /// eleventh draw nothing.
    #[test]
    fn the_pixel_history_window_keeps_the_selected_entry_whole() {
        let lines: Vec<String> = (0..11).map(|index| format!("cd /{index}")).collect();
        let entries: Vec<HistoryEntry<'_>> = lines
            .iter()
            .enumerate()
            .map(|(index, line)| HistoryEntry {
                index,
                line,
                at: None,
                marks: Vec::new(),
            })
            .collect();
        let (_, spans, _) = pixel_history(&entries, 8, &Theme::empty(), 10);

        let shown: Vec<(Range<u32>, usize)> = (1..9u32)
            .map(|index| ((index - 1) * 21..index * 21, index as usize))
            .collect();
        assert_eq!(spans, shown);
    }

    /// A line of a pixel History entry whose 18 px would pass the list draws
    /// nothing, and the walk ends there. A list 2 rows tall, 36 px, shows the
    /// first 35 chars of a 40-char word on pixel rows 1 to 19, and its last 5
    /// would end on pixel row 37. The entry keeps its whole span.
    #[test]
    fn a_pixel_history_line_past_the_list_draws_nothing() {
        let word = "x".repeat(40);
        let entries = [HistoryEntry {
            index: 0,
            line: &word,
            at: None,
            marks: Vec::new(),
        }];
        let (commands, spans, _) = pixel_history(&entries, usize::MAX, &Theme::empty(), 2);
        let white = [0xff; 3];
        assert_eq!(
            (commands, spans),
            (
                vec![
                    Command::Nudge([0, 1]),
                    pixel_run(0, 16, "\u{21ba}", white, false),
                    pixel_run(0, 62, &"x".repeat(35), white, true),
                    Command::Nudge([0, 0]),
                ],
                vec![(0..39, 0)],
            ),
        );
    }

    /// A pixel History entry whose first line would pass the list draws
    /// nothing and pushes no span, so a press there names no entry. In a list
    /// 2 rows tall, 36 px, the selected 1-line entry draws its line from pixel
    /// row 1 in the selected style, one run, and the first line of a 2-line
    /// entry under it would span the pixel rows 22 to 40.
    #[test]
    fn a_pixel_history_entry_whose_first_line_passes_the_list_draws_nothing() {
        let word = "x".repeat(40);
        let entries = [
            HistoryEntry {
                index: 4,
                line: "cd /alpha",
                at: None,
                marks: Vec::new(),
            },
            HistoryEntry {
                index: 2,
                line: &word,
                at: None,
                marks: Vec::new(),
            },
        ];
        let (commands, spans, _) = pixel_history(&entries, 0, &Theme::empty(), 2);
        let white = [0xff; 3];
        assert_eq!(
            (commands, spans),
            (
                vec![
                    Command::Nudge([0, 1]),
                    pixel_run(0, 16, "\u{21ba}", white, false),
                    pixel_run(0, 62, "cd /alpha", white, false),
                    Command::Nudge([0, 0]),
                ],
                vec![(0..21, 4)],
            ),
        );
    }

    /// A pixel History line that ends exactly at the list's bottom draws. On a
    /// 10 by 20 px cell, rows stand 23 px apart, so in a list 16 rows tall,
    /// 320 px, the fourteenth one-line entry's line spans the pixel rows 300
    /// to 320, and the fifteenth's would pass the list.
    #[test]
    fn a_pixel_history_line_that_ends_on_the_list_s_bottom_draws() {
        let lines: Vec<String> = (0..15).map(|index| format!("cd /{index}")).collect();
        let entries: Vec<HistoryEntry<'_>> = lines
            .iter()
            .enumerate()
            .map(|(index, line)| HistoryEntry {
                index,
                line,
                at: None,
                marks: Vec::new(),
            })
            .collect();
        let list = Rect::new(0, 0, 40, 16);
        let metrics = RowMetrics {
            cell: [10, 20],
            scale: 1.0,
        };
        let (_, spans) = paint_history_rows(
            &entries,
            0,
            125,
            list,
            &Availability::default(),
            &CommandSet::default(),
            &Theme::empty(),
            Some(metrics),
            &mut Buffer::empty(list),
            &mut stoat_widgets::ApcScene::new(),
        );

        let last = spans.last().cloned();
        assert_eq!((spans.len(), last), (14, Some((299..322, 13))));
    }

    /// A selected pixel History entry taller than its list draws its box down
    /// to the list's bottom and no further. A 3-line entry in a list 2 rows
    /// tall, 36 px, boxes the list's 32 sixteenths and draws its first line,
    /// whose 18 px end at pixel row 19, where its second would end at 37.
    #[test]
    fn a_selected_pixel_history_entry_taller_than_its_list_boxes_down_to_its_bottom() {
        let word = "x".repeat(100);
        let entries = [HistoryEntry {
            index: 0,
            line: &word,
            at: None,
            marks: Vec::new(),
        }];
        let theme = {
            let src = r##"theme t { ui.list.selected = { fg: "#000000", bg: "#7f7f7f" }; }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let (commands, spans, _) = pixel_history(&entries, 0, &theme, 2);
        let white = [0xff; 3];
        assert_eq!(
            (commands, spans),
            (
                vec![
                    Command::Bar(BarCommand {
                        x: 2,
                        y: 0,
                        width: 636,
                        height: 32,
                        color: [0x7f; 3],
                        occludes: false,
                        hatch: None,
                        radius: 4,
                        corners: 0,
                    }),
                    Command::Nudge([0, 1]),
                    pixel_run(0, 16, "\u{21ba}", white, false),
                    pixel_run(0, 62, &"x".repeat(35), white, true),
                    Command::Nudge([0, 0]),
                ],
                vec![(0..57, 0)],
            ),
        );
    }

    /// The commands, less the ends of their runs, and the spans of History
    /// `entries` painted as pixel rows by [`M1`] into the list (0, 0, 40,
    /// `rows`) under `theme`, two minutes after the time 0, with entry
    /// `selected` selected, and whether the paint wrote a cell or answered
    /// one.
    fn pixel_history(
        entries: &[HistoryEntry<'_>],
        selected: usize,
        theme: &Theme,
        rows: u16,
    ) -> (Vec<Command>, HistorySpans, bool) {
        let list = Rect::new(0, 0, 40, rows);
        let mut buf = Buffer::empty(list);
        let mut scene = stoat_widgets::ApcScene::new();
        let (cells, spans) = paint_history_rows(
            entries,
            selected,
            125,
            list,
            &Availability::default(),
            &CommandSet::default(),
            theme,
            Some(M1),
            &mut buf,
            &mut scene,
        );
        let commands = decode_stream(scene.bytes())
            .into_iter()
            .filter(|command| *command != Command::TextRunEnd)
            .collect();
        let drew_cells = !cells.is_empty() || buf != Buffer::empty(list);
        (commands, spans, drew_cells)
    }

    /// The recalled input gives `ui.list.match` to the chars that the needle's
    /// words mark, and plain text to the runs between them. A mark wins over
    /// the head's command color, and the ghost takes no mark.
    #[test]
    fn the_recalled_input_marks_the_matched_words() {
        let line = "git log --graph stoat/src";
        let typed = "o src/main";
        let open = ResolvedCommand {
            entry: PaletteCommand::Action(registry::lookup_alias("open").expect("a command")),
            head_chars: 1,
            ghost: "pen".to_owned(),
        };
        let marked = |text: &str, needle: &str, resolved: Option<&ResolvedCommand>| {
            let marks = input_history::word_spans(text, needle);
            input_spans(text, 0, resolved, None, &marks, &match_theme()).0
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
        let (rows, _) = paint_history_rows(
            entries,
            usize::MAX,
            125,
            list,
            &Availability::default(),
            &CommandSet::default(),
            theme,
            None,
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
            &[],
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
            doc_lines(
                &PaletteCommand::Action(entry),
                keys,
                60,
                &Theme::empty(),
                &HashMap::new(),
            )
            .lines
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
            let DocLines { lines, note, .. } = doc_lines(
                &PaletteCommand::Action(entry),
                &ActionKeys::new(),
                usize::from(width),
                &theme,
                &HashMap::new(),
            );
            let mut buf = Buffer::empty(Rect::new(0, 0, 40, 6));
            let mut scene = stoat_widgets::ApcScene::new();
            scene.set_live(false);
            let doc = Rect::new(0, 0, width, 6);
            paint_doc_lines(
                &mut buf,
                doc,
                &lines,
                source_tag(&PaletteCommand::Action(entry)),
                &note,
                &theme,
                &mut scene,
            );
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

    /// The list of a file argument names its open, completion, scope, and
    /// history keys.
    #[test]
    fn the_file_argument_key_line_reads_open_complete_scope_and_history() {
        assert_eq!(
            arg_key_line(":o "),
            " Enter open  Tab complete  S-Tab scope  \u{2191} history",
        );
    }

    /// The key line of an argument stage names `C-Enter` after its `Enter`
    /// pair while the parsed command starts a Cmd, as the built-in `run` and a
    /// `block` command do and `cd` does not. The list of a `dir` argument
    /// names it too.
    #[test]
    fn the_typing_key_line_offers_run_as_job_for_a_cmd() {
        let key_line = |input: &str| {
            let mut h = TestHarness::with_size(160, 50);
            h.stoat.reload_user_config(concat!(
                "command greet { arg who = text; run = shell(\"echo {who}\"); }\n",
                "command look { arg at = dir; run = shell(\"ls {at}\"); }",
            ));
            crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCommandPalette);
            h.type_text(input);
            h.settle();
            h.snapshot();
            h.rendered_text()
                .replace('\u{2500}', " ")
                .lines()
                .find(|row| row.contains("Tab complete"))
                .expect("a key line")
                .to_owned()
        };
        let [greet, run, cd, look] = ["greet x", "run ls", "cd x", "look x"].map(key_line);

        assert_eq!(
            [
                greet.contains("Enter run  C-Enter run as Job  Tab complete"),
                run.contains("C-Enter run as Job"),
                cd.contains("C-Enter"),
                look.contains("Enter run  C-Enter run as Job  Tab complete"),
            ],
            [true, true, false, true],
            "{greet:?} {run:?} {cd:?} {look:?}",
        );
    }

    /// Every other argument list keeps the palette's key line, whose Enter
    /// runs the command.
    #[test]
    fn any_other_argument_keeps_the_palette_key_line() {
        assert_eq!(
            arg_key_line(":cd "),
            " Enter run  Tab complete  S-Tab scope  \u{2191} history",
        );
    }

    /// The key line of the palette after `keys`, painted into a 200 by 40 area
    /// under a dead scene, with its trailing blanks cut.
    fn arg_key_line(keys: &str) -> String {
        let mut h = crate::Stoat::test();
        h.type_text(keys);
        h.settle();
        let mut palette = h.stoat.command_palette.take().expect("the palette is open");
        let theme = h.stoat.theme.clone();
        let chrome = ResolvedChrome::resolve(&theme);
        let area = Rect::new(0, 0, 200, 40);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        scene.set_live(false);
        render_command_palette(
            &mut palette,
            h.stoat.active_workspace_mut(),
            None,
            &theme,
            &chrome,
            area,
            0,
            None,
            0,
            &HashMap::new(),
            false,
            &mut buf,
            &mut scene,
        );

        let footer = palette_arg_layout(area, &palette, None, 0, None)
            .expect("the palette fits")
            .footer;
        let text: String = (footer.left()..footer.right())
            .map(|x| buf[(x, footer.y)].symbol())
            .collect();
        text.trim_end().to_owned()
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
            None,
            0,
            &HashMap::new(),
            false,
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

    /// The user command `name` that the user config `source` declares.
    fn user_command(source: &str, name: &str) -> PaletteCommand {
        let config = stoat_config::parse(source).0.expect("the config parses");
        let empty = stoat_config::parse("").0.expect("an empty config parses");
        let (set, _) = CommandSet::build((&empty, ""), Some((&config, source)));
        PaletteCommand::User(set.get(name).expect("the command loads").clone())
    }

    /// The user command `greet` on line 1, with the argument `who` and the doc
    /// `say hello`.
    fn greet() -> PaletteCommand {
        user_command(
            r#"command greet { doc = "say hello"; arg who = text; run = shell("echo hello {who}"); output = notify; }"#,
            "greet",
        )
    }

    /// Paint the one row `command` into a list 100 cells wide under `theme`.
    fn painted_row(command: PaletteCommand, theme: &Theme) -> (Buffer, stoat_widgets::ApcScene) {
        let list = Rect::new(0, 0, 100, 1);
        let mut buf = Buffer::empty(list);
        let mut scene = stoat_widgets::ApcScene::new();
        scene.set_live(true);
        paint_palette_rows(
            &[command],
            &[Vec::new()],
            usize::MAX,
            0,
            Namespace::Commands,
            &ActionKeys::new(),
            &HashMap::new(),
            list,
            RowStart { result: 0, skip: 0 },
            theme,
            false,
            None,
            &mut buf,
            &mut scene,
        );
        (buf, scene)
    }

    /// The `cli` command `rg` on line 1, with the doc `search the tree`.
    fn rg() -> PaletteCommand {
        user_command(r#"command rg = cli("rg", doc: "search the tree");"#, "rg")
    }

    /// The text of `command`'s row, and the bars and the text runs of its row
    /// under a theme that sets the Source Tag scope `scope` to `#010203` on
    /// `#040506`, each with its color.
    fn user_row(command: PaletteCommand, scope: &str) -> (String, Vec<(String, [u8; 3])>) {
        let (buf, _) = painted_row(command.clone(), &Theme::empty());
        let theme = {
            let src = format!(r##"theme t {{ {scope} = {{ fg: "#010203", bg: "#040506" }}; }}"##);
            let (config, _) = stoat_config::parse(&src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let (_, scene) = painted_row(command, &theme);
        let tag = decode_stream(scene.bytes())
            .into_iter()
            .filter_map(|command| match command {
                Command::Bar(bar) => Some(("bar".to_owned(), bar.color)),
                Command::TextRun(run) => Some((run.text, run.color)),
                _ => None,
            })
            .collect();
        (row_text(&buf, 0), tag)
    }

    /// A user row reads its name, no alias, its signature, and its doc as its
    /// description. Its Source Tag reads `config` in `ui.cmd.source.config`.
    #[test]
    fn a_user_row_reads_its_name_signature_description_and_tag() {
        assert_eq!(
            user_row(greet(), "ui.cmd.source.config"),
            (
                placed(100, &[(1, "greet"), (19, "<who>"), (48, "say hello")]),
                vec![
                    ("bar".to_owned(), [4, 5, 6]),
                    ("config".to_owned(), [1, 2, 3])
                ],
            ),
        );
    }

    /// A wrap row reads its name, the free signature `[args...]`, and its doc
    /// as its description. Its Source Tag reads `cli` in `ui.cmd.source.cli`.
    #[test]
    fn a_wrap_row_reads_the_cli_tag_and_a_free_signature() {
        assert_eq!(
            user_row(rg(), "ui.cmd.source.cli"),
            (
                placed(
                    100,
                    &[(1, "rg"), (19, "[args...]"), (48, "search the tree")]
                ),
                vec![("bar".to_owned(), [4, 5, 6]), ("cli".to_owned(), [1, 2, 3])],
            ),
        );
    }

    /// The text of each line of `command`'s doc under `specs`, and the text of
    /// the note after its Source Tag.
    fn doc_texts(
        command: &PaletteCommand,
        specs: &HashMap<String, CliSpecState>,
    ) -> (Vec<String>, String) {
        let DocLines { lines, note, .. } =
            doc_lines(command, &ActionKeys::new(), 60, &Theme::empty(), specs);
        let text = |line: &[(String, Style)]| -> String {
            line.iter().map(|(text, _)| text.as_str()).collect()
        };
        (lines.iter().map(|line| text(line)).collect(), text(&note))
    }

    /// The last line names the Source Tag of the command, `config` for a block
    /// command and `cli` for a wrapped tool.
    #[test]
    fn the_doc_of_a_user_command_names_where_it_is_defined() {
        let specs = HashMap::new();
        assert_eq!(
            [doc_texts(&greet(), &specs).0, doc_texts(&rg(), &specs).0],
            [
                [
                    "greet",
                    "greet <who>",
                    "say hello",
                    "config command \u{b7} config.stcfg:1"
                ],
                [
                    "rg",
                    "rg [args...]",
                    "search the tree",
                    "cli command \u{b7} config.stcfg:1"
                ],
            ],
        );
    }

    /// The note after a `cli` command's Source Tag names where its help comes
    /// from, and the tool's version once the help lands. The usage line then
    /// reads the help's usage, and `[args...]` before the help lands.
    #[test]
    fn the_doc_of_a_wrap_command_names_the_help_source_and_version() {
        let ready = HashMap::from([(
            "rg".to_owned(),
            CliSpecState::Ready(Arc::new(LoadedCli {
                spec: cli_spec::parse_help(RG_HELP),
                source: "rg --help".to_owned(),
                version: Some("ripgrep 14.1.0".to_owned()),
                missing: false,
            })),
        )]);
        let usage_and_note = |specs| {
            let (lines, note) = doc_texts(&rg(), specs);
            (lines[1].clone(), note)
        };
        assert_eq!(
            [usage_and_note(&ready), usage_and_note(&HashMap::new())],
            [
                (
                    "rg [flags] <pattern> [path...]".to_owned(),
                    "wrapped from rg --help \u{b7} ripgrep 14.1.0".to_owned()
                ),
                (
                    "rg [args...]".to_owned(),
                    "wrapped from rg --help".to_owned()
                ),
            ],
        );
    }

    /// Before its help lands, a command that declares `completions` names that
    /// command as where its help comes from.
    #[test]
    fn the_doc_of_a_completions_command_names_that_command() {
        let jj = user_command(
            r#"command jj = cli("jj", completions: "jj util completion fish");"#,
            "jj",
        );
        assert_eq!(
            doc_texts(&jj, &HashMap::new()).1,
            "wrapped from jj util completion fish",
        );
    }

    /// The ghost of an empty argument follows the typed text in
    /// `ui.text.muted`, and the caret stays on the typed text. A theme without
    /// the ghost scope draws no ghost.
    #[test]
    fn an_argument_ghost_follows_the_typed_text() {
        let theme = {
            let src = r##"theme t {
                ui.text.fg = "#040404";
                ui.text.muted.fg = "#070707";
                ui.cmd.command.fg = "#050505";
                ui.cmd.ghost.fg = "#060606";
            }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let title = ResolvedCommand {
            entry: user_command(
                r#"command title { arg message = text(default: git.last_subject); run = shell("echo {message}"); }"#,
                "title",
            ),
            head_chars: 5,
            ghost: String::new(),
        };
        let gray = |text: &str, gray: u8| {
            (
                text.to_owned(),
                Style::default().fg(Color::Rgb(gray, gray, gray)),
            )
        };
        let plain = |text: &str| {
            (
                text.to_owned(),
                Theme::empty().get(crate::theme::scope::UI_TEXT),
            )
        };

        let typed = |ghost, theme| input_spans("title ", 6, Some(&title), ghost, &[], theme);

        assert_eq!(
            [
                typed(Some("fix the parser"), &theme),
                typed(Some(""), &theme),
                typed(Some("fix the parser"), &Theme::empty()),
            ],
            [
                (
                    vec![gray("title", 5), gray(" ", 4), gray("fix the parser", 7)],
                    6
                ),
                (vec![gray("title", 5), gray(" ", 4)], 6),
                (vec![plain("title"), plain(" ")], 6),
            ],
        );
    }

    /// A pick row reads its label from the row's second cell. Off a live
    /// scene, the cells of the selected row take the selected style, and a
    /// matched char takes the match color over it. A row that does not match
    /// the query is not on show. A list longer than its area shows the window
    /// that holds the selection.
    #[test]
    fn a_pick_row_paints_its_label_and_marks_the_selected_row() {
        let theme = {
            let src = r##"theme t {
                ui.selection = { fg: "#000000", bg: "#7fb0ff" };
                ui.text.fg = "#040404";
                ui.list.match.fg = "#070707";
            }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let mut list = ArgList::default();
        list.set_rows(
            ["#412 Fix it", "#409 Docs"]
                .map(|label| ArgRow {
                    label: label.to_owned(),
                    insert: String::new(),
                    value_name: None,
                    description: String::new(),
                })
                .to_vec(),
        );
        list.refilter("Docs");
        let area = Rect::new(0, 0, 20, 2);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        scene.set_live(false);
        paint_arg_rows(&list, None, area, &theme, true, None, &mut buf, &mut scene);

        list.refilter("");
        list.move_selection(1);
        let one_row = Rect::new(0, 0, 20, 1);
        let mut windowed = Buffer::empty(one_row);
        paint_arg_rows(
            &list,
            None,
            one_row,
            &theme,
            true,
            None,
            &mut windowed,
            &mut scene,
        );

        let cell = |x: u16| (buf[(x, 0)].fg, buf[(x, 0)].bg);
        let selected = Color::Rgb(0x7f, 0xb0, 0xff);
        assert_eq!(
            (
                row_text(&buf, 0),
                row_text(&buf, 1),
                cell(1),
                cell(6),
                row_text(&windowed, 0),
            ),
            (
                placed(20, &[(1, "#409 Docs")]),
                placed(20, &[]),
                (Color::Rgb(0, 0, 0), selected),
                (Color::Rgb(7, 7, 7), selected),
                placed(20, &[(1, "#409 Docs")]),
            ),
        );
    }

    /// The status names the load of a pick list and the head of its command.
    /// A count of 1 reads the argument's name with no `s`, and any other count
    /// with it.
    #[test]
    fn the_pick_status_names_its_load_and_its_command() {
        let state = |load| PickState {
            key: ("pr".to_owned(), 0),
            spec: Arc::new(PickSpec {
                list: "gh pr list --json number".to_owned(),
                show: Template::parse("{line}"),
                value: Template::parse("{line}"),
                preview: None,
            }),
            generation: 0,
            load,
            list: ArgList::default(),
            preview: Preview::test_dummy(),
            preview_lines: Vec::new(),
            previews: HashMap::new(),
        };
        let mut one = state(PickLoad::Ready);
        one.list.set_rows(vec![ArgRow {
            label: "a".to_owned(),
            insert: "a".to_owned(),
            value_name: None,
            description: String::new(),
        }]);
        one.list.refilter("");
        assert_eq!(
            [
                pick_status(&state(PickLoad::Loading), "pr"),
                pick_status(&state(PickLoad::Failed("x".to_owned())), "pr"),
                pick_status(&one, "pr"),
                pick_status(&state(PickLoad::Ready), "pr"),
            ],
            [
                "loading \u{b7} from gh pr list",
                "failed \u{b7} from gh pr list",
                "1 pr \u{b7} from gh pr list",
                "0 prs \u{b7} from gh pr list",
            ],
        );
    }

    /// A row of a `cli` command's list with the label `label`, the value name
    /// `value`, and the description `description`.
    fn cli_row(label: &str, value: Option<&str>, description: &str) -> ArgRow {
        ArgRow {
            label: label.to_owned(),
            insert: label.to_owned(),
            value_name: value.map(str::to_owned),
            description: description.to_owned(),
        }
    }

    /// `rows` painted as rows of `kind` into a list 80 cells wide and two rows
    /// high, under a theme that sets `ui.cmd.flag.value` to `#0a0b0c`. The
    /// first row holds the selection.
    fn painted_cli_rows(rows: &[ArgRow], kind: CliListKind) -> Buffer {
        let theme = {
            let src = r##"theme t { ui.cmd.flag.value.fg = "#0a0b0c"; }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let mut list = ArgList::default();
        list.set_rows(rows.to_vec());
        list.refilter("");
        let area = Rect::new(0, 0, 80, 2);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        paint_arg_rows(
            &list,
            Some(kind),
            area,
            &theme,
            false,
            None,
            &mut buf,
            &mut scene,
        );
        buf
    }

    /// The label reads bold from the row's second cell, the value name in
    /// `ui.cmd.flag.value` one cell past the label's column, and the
    /// description one cell past the value's column.
    #[test]
    fn a_flag_row_paints_its_three_columns() {
        let buf = painted_cli_rows(
            &[
                cli_row("-e, --regexp", Some("PATTERN"), "A pattern"),
                cli_row("-g, --glob", Some("GLOB"), "Include or exclude files"),
            ],
            CliListKind::Flags,
        );
        assert_eq!(
            (
                row_text(&buf, 1),
                buf[(1, 1)].modifier.contains(Modifier::BOLD),
                buf[(28, 1)].fg,
            ),
            (
                placed(
                    80,
                    &[
                        (1, "-g, --glob"),
                        (28, "<GLOB>"),
                        (38, "Include or exclude files")
                    ]
                ),
                true,
                Color::Rgb(10, 11, 12),
            ),
        );
    }

    /// A label wider than its column moves the value and the description of
    /// its own row right, and the other rows keep their columns.
    #[test]
    fn a_long_flag_label_moves_its_value_and_description_right() {
        let long = "-x, --a-flag-with-a-long-name";
        let buf = painted_cli_rows(
            &[
                cli_row("-g, --glob", Some("GLOB"), "short"),
                cli_row(long, Some("V"), "long"),
            ],
            CliListKind::Flags,
        );
        let value_x = 1 + long.len() + 1;
        assert_eq!(
            [row_text(&buf, 0), row_text(&buf, 1)],
            [
                placed(80, &[(1, "-g, --glob"), (28, "<GLOB>"), (38, "short")]),
                placed(80, &[(1, long), (value_x, "<V>"), (value_x + 10, "long")]),
            ],
        );
    }

    /// The description of a subcommand row starts one cell past a name column
    /// of 8 cells, the column's least width.
    #[test]
    fn a_subcommand_row_paints_its_name_and_description() {
        let buf = painted_cli_rows(
            &[
                cli_row("build", None, "Compile the current package"),
                cli_row("t", None, "alias for test"),
            ],
            CliListKind::Subcommands,
        );
        assert_eq!(
            [row_text(&buf, 0), row_text(&buf, 1)],
            [
                placed(80, &[(1, "build"), (10, "Compile the current package")]),
                placed(80, &[(1, "t"), (10, "alias for test")]),
            ],
        );
    }

    /// A pixel pick row's box is 20 px high and 1 px from the next. Its line
    /// stands 1 px under its top, the label from col 16. The selected second
    /// row's box is a rounded bar 1 px inside the list's left end and 1 px
    /// inside its right end, or 3 px inside it beside a preview.
    #[test]
    fn a_pixel_pick_row_is_twenty_pixels_high_and_one_pixel_from_the_next() {
        let list = arg_list(
            ["alpha", "beta"]
                .map(|label| cli_row(label, None, ""))
                .to_vec(),
            1,
        );
        let white = [0xff; 3];
        let bar = |width| {
            Command::Bar(BarCommand {
                x: 2,
                y: 16,
                width,
                height: 18,
                color: [0x7f; 3],
                occludes: false,
                hatch: None,
                radius: 4,
                corners: 0,
            })
        };
        let painted = |end| pixel_arg_commands(&list, None, 4, end);
        let rows = |bar| {
            vec![
                Command::Nudge([0, 1]),
                pixel_run(0, 16, "alpha", white, false),
                Command::Nudge([0, 3]),
                bar,
                Command::Nudge([0, 4]),
                pixel_run(16, 16, "beta", white, false),
                Command::Nudge([0, 0]),
            ]
        };

        assert_eq!([painted(1), painted(3)], [rows(bar(956)), rows(bar(953))]);
    }

    /// A pixel flag row draws its flag bold from col 16, its value name in
    /// `ui.cmd.flag.value` 14 sixteenths past the label's 26 cells, at col
    /// 446, and its description 14 sixteenths past the value's 9 chars, at col
    /// 604. The description stops 8 px inside the row's box, at sixteenth 944
    /// of a list 60 cells wide, so 21 of its chars show. A 30-char flag shows
    /// its first 26 chars, and the 10-char value `<PATTERNS>` its first 9, so
    /// both keep their columns.
    #[test]
    fn a_pixel_flag_row_sets_its_columns_eight_pixels_apart() {
        let long = "--a-flag-with-a-long-long-name";
        let list = arg_list(
            vec![
                cli_row("-g, --glob", Some("GLOB"), "Include or exclude files"),
                cli_row(long, Some("PATTERNS"), "Search"),
            ],
            0,
        );
        let (white, value) = ([0xff; 3], [0x0a, 0x0b, 0x0c]);

        assert_eq!(
            arg_runs(&pixel_arg_commands(&list, Some(CliListKind::Flags), 4, 1)),
            [
                (0, 16, "-g, --glob".to_owned(), white, true),
                (0, 446, "<GLOB>".to_owned(), value, false),
                (0, 604, "Include or exclude fi".to_owned(), white, false),
                (16, 16, long[..26].to_owned(), white, true),
                (16, 446, "<PATTERNS".to_owned(), value, false),
                (16, 604, "Search".to_owned(), white, false),
            ],
        );
    }

    /// A pixel pick row's label stops 8 px inside its box, at sixteenth 944 of
    /// a list 60 cells wide, so 58 chars of a 70-char label show from col 16.
    #[test]
    fn a_pixel_pick_label_stops_eight_pixels_inside_its_box() {
        let list = arg_list(vec![cli_row(&"x".repeat(70), None, "")], 0);
        assert_eq!(
            arg_runs(&pixel_arg_commands(&list, None, 4, 1)),
            [(0, 16, "x".repeat(58), [0xff; 3], false)],
        );
    }

    /// A pixel pick row draws the chars that its query matches in
    /// `ui.list.match`, each stretch of them as a run of its own.
    #[test]
    fn a_pixel_pick_row_marks_its_matched_chars() {
        let mut list = ArgList::default();
        list.set_rows(vec![cli_row("alpha", None, "")]);
        list.refilter("lph");
        let white = [0xff; 3];

        assert_eq!(
            arg_runs(&pixel_arg_commands(&list, None, 4, 1)),
            [
                (0, 16, "a".to_owned(), white, false),
                (0, 32, "lph".to_owned(), [7, 7, 7], false),
                (0, 80, "a".to_owned(), white, false),
            ],
        );
    }

    /// A pixel subcommand row draws its name bold from col 16, and its
    /// description 14 sixteenths past the name column, whose 8 cells end at
    /// sixteenth 144, at col 158.
    #[test]
    fn a_pixel_subcommand_row_sets_its_description_after_the_name_column() {
        let list = arg_list(
            vec![
                cli_row("test", None, "Run the tests"),
                cli_row("tree", None, "Show the tree"),
            ],
            0,
        );
        let runs = arg_runs(&pixel_arg_commands(
            &list,
            Some(CliListKind::Subcommands),
            4,
            1,
        ));
        let white = [0xff; 3];

        assert_eq!(
            runs[..2],
            [
                (0, 16, "test".to_owned(), white, true),
                (0, 158, "Run the tests".to_owned(), white, false),
            ],
        );
    }

    /// A pixel arg list shows the rows whose boxes fit whole, 9 of 21 px in
    /// 11 cell rows of 18 px, 17 in 20 cell rows, and 1 in a list of one row.
    /// Thirty rows with row 20 selected in 11 cell rows show rows 12 to 20, so
    /// the selection stands on the last row that fits.
    #[test]
    fn a_pixel_arg_list_counts_the_rows_that_fit() {
        let rows = (0..30)
            .map(|at| cli_row(&format!("row {at:02}"), None, ""))
            .collect();
        let list = arg_list(rows, 20);
        let labels: Vec<String> = arg_runs(&pixel_arg_commands(&list, None, 11, 1))
            .into_iter()
            .map(|(_, _, text, _, _)| text)
            .collect();

        let shown: Vec<String> = (12..=20).map(|at| format!("row {at:02}")).collect();
        assert_eq!(
            (M1.fit(11), M1.fit(20), M1.fit(1), labels),
            (9, 17, 1, shown)
        );
    }

    /// An [`ArgList`] of `rows`, matched against no query, with row `selected`
    /// selected.
    fn arg_list(rows: Vec<ArgRow>, selected: i32) -> ArgList {
        let mut list = ArgList::default();
        list.set_rows(rows);
        list.refilter("");
        list.move_selection(selected);
        list
    }

    /// The commands, less the ends of their runs, of the rows of `list` in the
    /// columns of `shape`, painted as pixel rows by [`M1`] into the list (0, 0,
    /// 60, `rows`) with the selected row's box `end` px inside the list's right
    /// end. The theme boxes the selected row in `#7f7f7f` and keeps each
    /// column's color, and it gives `ui.cmd.flag.value` `#0a0b0c` and
    /// `ui.list.match` `#070707`.
    fn pixel_arg_commands(
        list: &ArgList,
        shape: Option<CliListKind>,
        rows: u16,
        end: u16,
    ) -> Vec<Command> {
        let theme = {
            let src = r##"theme t {
                ui.list.selected = { fg: "#000000", bg: "#7f7f7f" };
                ui.cmd.flag.value.fg = "#0a0b0c";
                ui.list.match.fg = "#070707";
            }"##;
            let (config, _) = stoat_config::parse(src);
            Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
        };
        let area = Rect::new(0, 0, 60, rows);
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        let pixels = Some((M1, end));
        paint_arg_rows(
            list, shape, area, &theme, true, pixels, &mut buf, &mut scene,
        );
        decode_stream(scene.bytes())
            .into_iter()
            .filter(|command| *command != Command::TextRunEnd)
            .collect()
    }

    /// The text runs of `commands` as their row, col, text, color, and weight.
    fn arg_runs(commands: &[Command]) -> Vec<(i16, i16, String, [u8; 3], bool)> {
        commands
            .iter()
            .filter_map(|command| match command {
                Command::TextRun(run) => {
                    Some((run.row, run.col, run.text.clone(), run.color, run.bold))
                },
                _ => None,
            })
            .collect()
    }

    /// A pixel pick list whose command failed draws the command's text as one
    /// run in `ui.error` where a row's label starts, 1 px into the list's first
    /// row.
    #[test]
    fn a_failed_pixel_pick_list_draws_its_text_where_a_label_starts() {
        let failed = ShellOutput {
            stdout: Vec::new(),
            stderr: b"ls: no such dir\n".to_vec(),
            exit_code: 1,
        };
        let mut h = pick_palette(r#"pick("ls")"#, failed);
        let (commands, _) = pixel_palette(&mut h);
        let list = crate::mouse::boxed_modal_surfaces(&h.stoat)
            .expect("the palette is open")
            .list;
        let error = style_rgb(h.stoat.theme.get(crate::theme::scope::UI_ERROR).fg);

        let at = commands.iter().position(
            |command| matches!(command, Command::TextRun(run) if run.text == "ls: no such dir"),
        );
        let note = at.and_then(|at| match (&commands[at - 1], &commands[at]) {
            (Command::Nudge(nudge), Command::TextRun(run)) => {
                Some((*nudge, run.row, run.col, Some(run.color)))
            },
            _ => None,
        });
        assert_eq!(
            note,
            Some(([0, 1], list.y as i16 * 16, list.x as i16 * 16 + 16, error))
        );
    }

    /// A pixel pick list beside a preview boxes its selected row 1 px inside
    /// the list's left end and 3 px inside its right end, by the preview's
    /// Island.
    #[test]
    fn a_pixel_pick_list_beside_a_preview_boxes_its_row_three_pixels_in() {
        let rows = ShellOutput {
            stdout: b"a\nb\n".to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
        };
        let mut h = pick_palette(r#"pick("ls", preview: "cat {line}")"#, rows);
        let (commands, _) = pixel_palette(&mut h);
        let list = crate::mouse::boxed_modal_surfaces(&h.stoat)
            .expect("the palette is open")
            .list;

        let boxes: Vec<(i16, u16)> = commands
            .iter()
            .filter_map(|command| match command {
                Command::Bar(bar) if bar.y == list.y as i16 * 16 && bar.radius == 4 => {
                    Some((bar.x, bar.width))
                },
                _ => None,
            })
            .collect();
        assert_eq!(boxes, [(list.x as i16 * 16 + 2, list.width * 16 - 7)]);
    }

    /// A pixel `cli` list draws its flags as pixel rows, the first flag's label
    /// bold where a row's label starts, and pages by the rows that fit. Its doc
    /// island takes the pixel doc's edges, 8 px under the list's Island and
    /// 2 px over the doc's first row, and its name line draws as a run.
    #[test]
    fn a_pixel_cli_list_draws_pixel_rows_over_a_pixel_doc() {
        let mut h = TestHarness::with_size(120, 30);
        let help = ShellOutput {
            stdout: RG_HELP.as_bytes().to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
        };
        h.fake_shell.set_response("rg --help", help);
        h.stoat
            .reload_user_config(r#"command rg = cli("rg", output: notify);"#);
        h.type_text(":rg -");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();
        let (commands, islands) = pixel_palette(&mut h);
        let list = crate::mouse::boxed_modal_surfaces(&h.stoat)
            .expect("the palette is open")
            .list;
        let palette = h.stoat.command_palette.as_ref().expect("palette open");
        let (_, _, cli) = palette.cli_list.as_ref().expect("a cli list");

        let at = |row: u16, col: u16| {
            commands.iter().any(|command| {
                matches!(command, Command::TextRun(run)
                    if run.row == row as i16 * 16 && run.col == col as i16 * 16 + 16 && run.bold)
            })
        };
        let doc_name = commands.iter().any(|command| {
            matches!(command, Command::TextRun(run)
                if run.text == "rg" && run.row >= list.bottom() as i16 * 16)
        });
        let edges: Vec<[Edge; 4]> = islands.into_iter().map(|(_, edges)| edges).collect();
        assert_eq!(
            (at(list.y, list.x), cli.viewport_rows, doc_name, edges),
            (
                true,
                Some(M1.fit(list.height)),
                true,
                vec![
                    [ISLAND_TOP, ISLAND_SIDE, PIXEL_LIST_EDGE, ISLAND_SIDE],
                    [PIXEL_DOC_EDGE, ISLAND_SIDE, ISLAND_BOTTOM, ISLAND_SIDE],
                ],
            ),
        );
    }

    /// A 120 by 30 harness with the command `peek`, whose argument is the pick
    /// `pick`, open in the palette over the list of `ls`, which the fake shell
    /// answers with `output`.
    fn pick_palette(pick: &str, output: ShellOutput) -> TestHarness {
        let mut h = TestHarness::with_size(120, 30);
        h.stoat.reload_user_config(&format!(
            r#"command peek {{ arg f = {pick}; run = shell("cat {{f}}"); }}"#
        ));
        h.fake_shell.set_response("ls", output);
        h.type_text(":peek ");
        let _ = h.snapshot();
        h.settle();
        let _ = h.snapshot();
        h
    }

    /// The commands, less the ends of their runs, that the open palette of `h`
    /// paints into the harness's area under a live scene whose lists lay their
    /// rows out by [`M1`], for a terminal that starts a page at its pool's
    /// inset and reads `ink` frames, and the Islands among them.
    fn pixel_palette(h: &mut TestHarness) -> (Vec<Command>, Vec<(Rect, [Edge; 4])>) {
        let mut palette = h.stoat.command_palette.take().expect("the palette is open");
        let specs = std::mem::take(&mut h.stoat.cli_specs);
        let theme = h.stoat.theme.clone();
        let chrome = ResolvedChrome::resolve(&theme);
        let area = h.stoat.size();
        let mut buf = Buffer::empty(area);
        let mut scene = stoat_widgets::ApcScene::new();
        scene.set_nudge_protocol(true);
        scene.set_scaled_cell(Some(M1.cell));
        scene.set_pixel_rows(Some((M1.cell, M1.scale)));
        scene.set_ink_protocol(true);
        render_command_palette(
            &mut palette,
            h.stoat.active_workspace_mut(),
            None,
            &theme,
            &chrome,
            area,
            0,
            None,
            0,
            &specs,
            true,
            &mut buf,
            &mut scene,
        );
        h.stoat.command_palette = Some(palette);
        h.stoat.cli_specs = specs;
        let commands = decode_stream(scene.bytes())
            .into_iter()
            .filter(|command| *command != Command::TextRunEnd)
            .collect();
        (commands, plate_islands(&theme, &scene))
    }

    /// A harness whose palette shows a Run Block over a Cmd of `line` that
    /// printed `output`, which exited 0 where `finished` holds and else runs.
    fn run_block_over(line: &str, output: &[u8], finished: bool) -> TestHarness {
        let mut h = TestHarness::with_size(120, 40);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCommandPalette);
        let mut cmd = Cmd::new(line.to_owned(), PathBuf::from("/w"), 60);
        cmd.feed(output);
        cmd.finished = finished;
        cmd.exit_status = finished.then_some(0);
        let id = h.stoat.active_workspace_mut().cmds.insert(cmd);
        let origin = run_block_origin(h.stoat.size());
        let palette = h
            .stoat
            .command_palette
            .as_mut()
            .expect("the palette is open");
        palette.block = Some(PaletteBlock {
            cmd: id,
            origin,
            rows: 0,
        });
        h
    }

    /// The cells and the edges of the plate that `commands` declare, the
    /// backdrop panel in the ground of `theme`.
    fn plate_panel(commands: &[Command], theme: &Theme) -> Option<(Rect, [Edge; 4])> {
        let ground = style_rgb(crate::render::ground_surface(theme))?;
        commands.iter().find_map(|command| match command {
            Command::Panel(panel) if panel.backdrop && panel.fill == Some(ground) => {
                let cells = Rect::new(panel.left, panel.top, panel.width, panel.height);
                Some((cells, panel.edges))
            },
            _ => None,
        })
    }

    /// Each command of `commands` but the nudges, with the nudge in force where
    /// it stands.
    fn under_nudges(commands: &[Command]) -> Vec<([i16; 2], &Command)> {
        let mut nudge = [0, 0];
        commands
            .iter()
            .filter_map(|command| match command {
                Command::Nudge(offset) => {
                    nudge = *offset;
                    None
                },
                command => Some((nudge, command)),
            })
            .collect()
    }

    /// The pixel layout of a Run Block whose body wants `wanted` lines, with
    /// `room` rows, under the theme of a fresh harness and a scene that reads
    /// nudges and inks on a 9 px cell `height` px high at scale 1.
    fn laid_out(height: u16, wanted: u16, room: u16) -> Option<(u16, u16)> {
        let theme = TestHarness::with_size(80, 24).stoat.theme.clone();
        let mut scene = stoat_widgets::ApcScene::new();
        scene.set_nudge_protocol(true);
        scene.set_ink_protocol(true);
        scene.set_pixel_rows(Some(([9, height], 1.0)));
        block_pixels(&theme, &scene, wanted, room, false)
            .map(|pixels| (pixels.lines, pixels.fit.rows))
    }

    /// A Run Block shows the most lines whose Island keeps the plate over the
    /// area's last row. Seven 17 px lines fill seven 18 px rows, and the
    /// sixteenth line of a roomy plate ends in the fifteenth row.
    #[test]
    fn a_pixel_run_block_holds_the_lines_that_fit_its_room() {
        assert_eq!(
            [laid_out(18, 16, 7), laid_out(18, 16, 16)],
            [Some((7, 7)), Some((16, 15))],
        );
    }

    /// A cell over 25 logical px high puts the Island's bottom inside its one
    /// row, so a Run Block keeps its cells there.
    #[test]
    fn a_run_block_keeps_its_cells_on_a_cell_over_25_px_high() {
        assert_eq!(
            [laid_out(25, 1, 8), laid_out(26, 1, 8)],
            [Some((1, 1)), None],
        );
    }

    /// A finished Cmd's one line `one` stands in an Island 12 px and 17 px
    /// high, 10 px inside it, so the Island ends 7 px under its one row. The
    /// key line centers in the 24 px under the Island, and the plate ends where
    /// the key line does.
    #[test]
    fn a_pixel_run_block_island_is_twelve_pixels_and_seventeen_a_line_high() {
        let mut h = run_block_over("echo one", b"one", true);
        let (commands, islands) = pixel_palette(&mut h);
        let header_rows = h
            .stoat
            .command_palette
            .as_ref()
            .expect("the palette")
            .header_rows;
        let [(island, edges)] = islands[..] else {
            panic!("one Island, got {islands:?}");
        };

        let placed = under_nudges(&commands);
        let run = |text: &str| {
            placed.iter().find_map(|(at, command)| match command {
                Command::TextRun(run) if run.text == text => Some((*at, run.col, run.row)),
                _ => None,
            })
        };
        let body = (island.x as i16 * 16, island.y as i16 * 16);
        let plate = plate_panel(&commands, &h.stoat.theme);
        assert_eq!(
            (
                (island.height, edges[2]),
                run("one"),
                run("Enter").map(|(at, ..)| at),
                plate.map(|(cells, edges)| (cells.height, edges[2])),
            ),
            (
                (1, Edge::Outset(7)),
                Some(([0, 2], body.0 + 12, body.1)),
                Some([1, 10]),
                Some((header_rows + 3, Edge::Outset(13))),
            ),
        );
    }

    /// A running build's progress bar draws 5 px under the top of the Island's
    /// first row, from 10 px inside each side, and pushes the line under it
    /// 8 px down. The Island then ends 15 px under its row, past the room of
    /// one cell under the key line, so the plate takes one more row and ends
    /// 3 px into it.
    #[test]
    fn a_running_parsed_block_draws_its_bar_over_its_lines() {
        let output = b"    Building [=========>            ] 212/480: stoat\r";
        let mut h = run_block_over("cargo build", output, false);
        let (commands, islands) = pixel_palette(&mut h);
        let header_rows = h
            .stoat
            .command_palette
            .as_ref()
            .expect("the palette")
            .header_rows;
        let [(island, edges)] = islands[..] else {
            panic!("one Island, got {islands:?}");
        };

        let placed = under_nudges(&commands);
        let bars: Vec<(i16, i16, u16)> = placed
            .iter()
            .filter_map(|(at, command)| match command {
                Command::Bar(bar) if *at == [0, 5] && bar.height == 2 => {
                    Some((bar.x, bar.y, bar.width))
                },
                _ => None,
            })
            .collect();
        let line = placed.iter().find_map(|(at, command)| match command {
            Command::TextRun(run) if run.text.starts_with("Building") => Some(*at),
            _ => None,
        });
        let plate = plate_panel(&commands, &h.stoat.theme);
        let (x, y) = (island.x as i16 * 16 + 12, island.y as i16 * 16);
        let width = island.width * 16 - 24;
        let fill = (u32::from(width) * 212 / 480) as u16;
        assert_eq!(
            (
                bars,
                line,
                edges[2],
                plate.map(|(cells, edges)| (cells.height, edges[2]))
            ),
            (
                vec![(x, y, width), (x, y, fill)],
                Some([0, 10]),
                Edge::Outset(15),
                Some((header_rows + 4, Edge::Outset(3))),
            ),
        );
    }
}

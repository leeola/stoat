//! The MergeGrid layout of the three-way conflict view.
//!
//! The Merge board draws a conflicted file as three columns under a header row,
//! which are ours, the merged result, and theirs. Each column leads with a
//! change bar, a line number, and a divider. Each row takes the tint of the
//! side the merge takes it from, and a chip on the first row of each run names
//! how the merge settles it.
//!
//! The bars, hairlines, numbers, and chips are smaller than a cell, so they are
//! scene components, and the layout paints only under a live scene. Without one
//! the view keeps the cell layout of [`super::render_conflict_view`].

use super::{GridPaint, BAND_TINT};
use crate::{
    conflict_session::{ConflictDerived, ConflictLabels},
    display_map::{CachedHighlightEndpoints, DisplaySnapshot},
    merge_view::{AlignRow, ChunkState, MergeDoc, MergeRow},
    render::{
        editor,
        paint::{dim_rgb, fill_line_tint, render_side_text, style_rgb},
        review::{self, paint_highlighted_row, DiffDials, DiffRowCache, DiffRowKind},
    },
    review::ReviewSide,
    theme::{scope as s, Theme, TypeRole},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::StatefulWidget,
};
use std::fmt::Write;
use stoat_widgets::{
    bar::{Bar, BarBatch},
    panel::Panel,
    text_run::{self, TextRun, TextRunBatch},
    ApcScene,
};
use stoatty_protocol::command::{BorderStyle, PanelShadow};

/// Rows the header takes above the grid's rows.
pub(super) const HEADER_ROWS: u16 = 1;

/// The change bar's left edge and width, in sixteenths from its column's left
/// edge, the board's `margin-left: 4px; width: 3px`.
const BAR_X: i16 = 4;
const BAR_WIDTH: u16 = 3;

/// Where the number field starts, in sixteenths from its column's left edge,
/// right after the change bar.
const NUMBER_X: u16 = 7;

/// The board's `margin: 0 6px` on each side of the 1/16 divider after the
/// number field, in sixteenths.
const DIVIDER_MARGIN: u16 = 6;

/// The header cells' `padding: 0 8px` and `gap: 8px`, in sixteenths.
const HEADER_PAD: u16 = 8;
const HEADER_GAP: u16 = 8;

/// The chip's `padding: 0 6px`, in sixteenths.
const CHIP_PAD: u16 = 6;

/// How far the untaken side's text blends toward its surface, the board's
/// `opacity: 0.45`.
const UNTAKEN_FADE: f32 = 0.55;

/// The keys each side's header names, for taking a chunk or one line from it.
const OURS_KEYS: &str = "o \u{b7} O line";
const THEIRS_KEYS: &str = "t \u{b7} T line";

/// A resolved MergeGrid, which holds where its columns fall and the colors
/// they take.
pub(super) struct Grid {
    pub(super) cols: GridColumns,
    pub(super) colors: GridColors,
}

impl Grid {
    /// The grid over `area` for `doc` as `snapshot` holds its merged column, or
    /// `None` when the theme gives no RGB color for the surface or the text.
    ///
    /// The scene components take RGB colors alone, so such a theme keeps the
    /// cell layout.
    pub(super) fn resolve(
        area: Rect,
        snapshot: &DisplaySnapshot,
        doc: &MergeDoc,
        theme: &Theme,
    ) -> Option<Self> {
        let colors = GridColors::resolve(theme)?;
        let cols = GridColumns::compute(area, digits(snapshot, doc), number_scale(theme));
        Some(Self { cols, colors })
    }
}

/// The MergeGrid's three columns and the strip that leads each one.
///
/// The board sets its columns `1fr 1px 1.12fr 1px 1fr`, so each side takes
/// 100 / 312 of the width and the merged column the rest. Its two 1 px rules
/// are hairlines on the merged column's edges rather than cells of their own.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct GridColumns {
    pub(super) ours: Column,
    pub(super) merged: Column,
    pub(super) theirs: Column,
    /// The right edge of the number field, in sixteenths from a column's left
    /// edge. The numbers right-align against it.
    number_right: u16,
    /// The scale the numbers and chips draw at.
    number_scale: u16,
}

impl GridColumns {
    /// Lay the columns out over `area` for line numbers of `digits` digits at
    /// `number_scale`.
    ///
    /// The text starts on the first whole cell past the divider's margin, so it
    /// stays on the cell grid the buffer's text paints on.
    pub(super) fn compute(area: Rect, digits: u16, number_scale: u16) -> Self {
        let side = (u32::from(area.width) * 100 / 312) as u16;
        let merged = area.width.saturating_sub(2 * side);
        let number_right =
            NUMBER_X + text_run::advance_sixteenths(usize::from(digits), number_scale);
        let text_offset = (number_right + 2 * DIVIDER_MARGIN + 1).div_ceil(16);
        let column = |x: u16, width: u16| Column {
            x,
            width,
            text_x: x + text_offset.min(width),
        };

        Self {
            ours: column(area.x, side),
            merged: column(area.x + side, merged),
            theirs: column(area.x + side + merged, side),
            number_right,
            number_scale,
        }
    }

    /// The divider's left edge, in sixteenths from a column's left edge.
    fn divider_x(&self) -> i16 {
        (self.number_right + DIVIDER_MARGIN) as i16
    }

    /// The cells a chip reading `text` takes.
    ///
    /// The chip holds its text and the board's padding on each side, rounded
    /// up to whole cells.
    fn chip_cells(&self, text: &str) -> u16 {
        let advance = text_run::advance_sixteenths(text.chars().count(), self.number_scale);
        (advance + 2 * CHIP_PAD).div_ceil(16)
    }
}

/// One MergeGrid column, in cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Column {
    pub(super) x: u16,
    pub(super) width: u16,
    /// The first cell of the column's text, past its bar, number, and divider.
    pub(super) text_x: u16,
}

impl Column {
    pub(super) fn right(self) -> u16 {
        self.x + self.width
    }

    /// The cells from the text's start to `end`.
    fn text_cells(self, end: u16) -> usize {
        usize::from(end.saturating_sub(self.text_x))
    }
}

/// The colors the grid draws with, resolved once per paint.
///
/// Each scope the boards added falls back to a neighbor when a theme leaves it
/// unset, so a theme without those scopes keeps its look. The band takes the
/// conflict header's color, the chip takes the band, and the ghost takes muted
/// italic text. A missing tint is its side's color washed toward the surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct GridColors {
    pub(super) bg: [u8; 3],
    pub(super) text: [u8; 3],
    pub(super) muted: [u8; 3],
    pub(super) hairline: [u8; 3],
    pub(super) divider: [u8; 3],
    /// The rule under the header row.
    pub(super) rule: Option<[u8; 3]>,
    /// The merged column's header fill.
    pub(super) head: Option<[u8; 3]>,
    /// The fill of a cell whose side has no line on the row.
    pub(super) absent: Option<[u8; 3]>,
    /// The line color of the hatch over [`Self::absent`], which `diff.absent`'s
    /// foreground gives, or `None` for a solid fill.
    pub(super) absent_hatch: Option<[u8; 3]>,
    pub(super) ghost: Style,
    pub(super) ours: Tone,
    pub(super) theirs: Tone,
    /// The merged column's rows of an unsettled chunk.
    pub(super) band: Tone,
    pub(super) chip: Tone,
}

impl GridColors {
    /// The grid's colors in `theme`, or `None` when the surface, the text, or a
    /// side's color is not RGB.
    pub(super) fn resolve(theme: &Theme) -> Option<Self> {
        let bg = style_rgb(theme.try_get(s::UI_BACKGROUND).and_then(|style| style.bg))?;
        let text = style_rgb(theme.get(s::UI_TEXT).fg)?;
        let muted = style_rgb(theme.get(s::UI_TEXT_MUTED).fg)?;
        let divider = style_rgb(theme.get(s::UI_DIVIDER).fg).unwrap_or(muted);
        let hairline = style_rgb(theme.get(s::UI_BORDER_INACTIVE).fg).unwrap_or(divider);

        let tone = |style: Style| {
            let fg = style_rgb(style.fg)?;
            let tint = style_rgb(style.bg).unwrap_or_else(|| dim_rgb(fg, bg, BAND_TINT));
            Some(Tone { fg, tint })
        };
        let band = tone(
            theme
                .try_get(s::VCS_CONFLICT_BAND)
                .unwrap_or_else(|| theme.get(s::VCS_CONFLICT_HEADER)),
        )?;
        let [r, g, b] = muted;

        Some(Self {
            bg,
            text,
            muted,
            hairline,
            divider,
            rule: style_rgb(theme.get(s::UI_SURFACE_RAISED).bg),
            head: style_rgb(theme.get(s::VCS_CONFLICT_HEAD).bg),
            absent: style_rgb(theme.get(s::DIFF_ABSENT).bg),
            absent_hatch: style_rgb(theme.get(s::DIFF_ABSENT).fg),
            ghost: theme.try_get(s::VCS_CONFLICT_GHOST).unwrap_or_else(|| {
                Style::default()
                    .fg(Color::Rgb(r, g, b))
                    .add_modifier(Modifier::ITALIC)
            }),
            ours: tone(theme.get(s::VCS_CONFLICT_OURS))?,
            theirs: tone(theme.get(s::VCS_CONFLICT_THEIRS))?,
            band,
            chip: theme
                .try_get(s::VCS_CONFLICT_CHIP)
                .and_then(tone)
                .unwrap_or(band),
        })
    }
}

/// A side's bar and text color, with the tint its rows take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Tone {
    pub(super) fg: [u8; 3],
    pub(super) tint: [u8; 3],
}

/// What one grid row shows, after the Merge board's row kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MergeRowKind {
    /// A line both sides keep as the ancestor has it.
    Context,
    /// A line only ours changed or added, which the merge takes from ours.
    Ours,
    /// A line only theirs changed or added, which the merge takes from theirs.
    Theirs,
    /// A row of a chunk still on its conflict markers.
    Conflict,
    /// A row of a chunk resolved to the whole ours side.
    TookOurs,
    /// A row of a chunk resolved to the whole theirs side.
    TookTheirs,
    /// A row of a chunk resolved some other way, under the chip text it holds.
    ///
    /// The board draws no kind for such a chunk, so it keeps the conflict tint
    /// and lets the chip name the resolution.
    Settled(&'static str),
}

/// How one cell of a grid row draws.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CellLook {
    tint: Option<[u8; 3]>,
    bar: Option<[u8; 3]>,
    /// The cell's text fades toward its surface, as the side a chunk did not
    /// take does.
    faded: bool,
}

/// How a grid row's three cells draw, and the chip the first row of its run
/// carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RowLook {
    ours: CellLook,
    merged: CellLook,
    theirs: CellLook,
    /// The merged text draws as a ghost, since it is still the marker block.
    ghost: bool,
    chip: Option<(&'static str, Tone)>,
}

impl RowLook {
    /// The look of a `kind` row whose ours and theirs lines differ from their
    /// ancestor lines as `edited` says.
    fn of(kind: MergeRowKind, edited: [bool; 2], colors: &GridColors) -> Self {
        let lit = |tone: Tone| CellLook {
            tint: Some(tone.tint),
            bar: Some(tone.fg),
            faded: false,
        };
        let barred = |tone: Tone| CellLook {
            bar: Some(tone.fg),
            ..CellLook::default()
        };
        let banded = |tone: Tone, edited: bool| CellLook {
            tint: Some(tone.tint),
            bar: edited.then_some(tone.fg),
            faded: false,
        };
        let faded = CellLook {
            faded: true,
            ..CellLook::default()
        };
        let unsettled = |ghost: bool, chip: &'static str| Self {
            ours: banded(colors.ours, edited[0]),
            merged: lit(colors.band),
            theirs: banded(colors.theirs, edited[1]),
            ghost,
            chip: Some((chip, colors.chip)),
        };

        match kind {
            MergeRowKind::Context => Self::default(),
            MergeRowKind::Ours => Self {
                ours: lit(colors.ours),
                merged: barred(colors.ours),
                chip: Some(("auto \u{b7} ours", colors.ours)),
                ..Self::default()
            },
            MergeRowKind::Theirs => Self {
                merged: barred(colors.theirs),
                theirs: lit(colors.theirs),
                chip: Some(("auto \u{b7} theirs", colors.theirs)),
                ..Self::default()
            },
            MergeRowKind::Conflict => unsettled(true, "conflict \u{b7} o t b"),
            MergeRowKind::Settled(chip) => unsettled(false, chip),
            MergeRowKind::TookOurs => Self {
                ours: lit(colors.ours),
                merged: barred(colors.ours),
                theirs: faded,
                chip: Some(("\u{2713} took ours", colors.ours)),
                ..Self::default()
            },
            MergeRowKind::TookTheirs => Self {
                ours: faded,
                merged: barred(colors.theirs),
                theirs: lit(colors.theirs),
                chip: Some(("\u{2713} took theirs", colors.theirs)),
                ..Self::default()
            },
        }
    }
}

/// One header cell's texts and where they draw.
struct HeaderCell<'a> {
    column: Column,
    y: u16,
    /// The column's name and its color.
    name: (&'a str, [u8; 3]),
    label: &'a str,
    /// The keys the cell names against its right edge, or empty for none.
    keys: &'a str,
    muted: [u8; 3],
    /// The surface under the cell, which backs each run.
    bg: [u8; 3],
    scale: u16,
}

/// The state one grid paint threads through its rows.
struct RowPainter<'a> {
    cols: &'a GridColumns,
    colors: &'a GridColors,
    buf: &'a mut Buffer,
    numbers: TextRunBatch,
    bars: BarBatch,
    /// The absent cells that take the hatch, one rect per cell run, which go
    /// out as hatched bars after the rows.
    hatched: Vec<Rect>,
    /// One buffer for every number the paint formats, rather than one per row.
    scratch: String,
}

impl RowPainter<'_> {
    /// Paint `side`'s cell in `column` on row `y` as `look` says.
    ///
    /// The cell takes its fill, its bar, its number, and its text with its
    /// change spans in `spans`. A side with no line on the row fills with the
    /// absent color, under the absent hatch when the theme gives one, and draws
    /// nothing else.
    fn side(
        &mut self,
        column: Column,
        y: u16,
        side: Option<&ReviewSide>,
        look: CellLook,
        base: Style,
        spans: Style,
    ) {
        let Some(side) = side else {
            if let Some(absent) = self.colors.absent {
                fill_line_tint(self.buf, column.x, y, usize::from(column.width), absent);
                if self.colors.absent_hatch.is_some() {
                    self.hatched.push(Rect::new(column.x, y, column.width, 1));
                }
            }
            return;
        };

        let bg = self.fill(column, y, look.tint);
        if let Some(color) = look.bar {
            self.bar(column, y, color);
        }
        self.number(column, y, side.line_num, self.colors.muted, bg);

        let cells = column.text_cells(column.right());
        render_side_text(
            self.buf,
            column.text_x,
            y,
            &side.text,
            cells,
            base,
            &side.change_spans,
            spans,
            &side.moved_spans,
            base,
        );
        if look.faded {
            for x in column.text_x..column.right() {
                let cell = &mut self.buf[(x, y)];
                if let Some(fg) = style_rgb(Some(cell.fg)) {
                    let [r, g, b] = dim_rgb(fg, bg, UNTAKEN_FADE);
                    cell.set_fg(Color::Rgb(r, g, b));
                }
            }
        }
    }

    /// Wash `column`'s cells on row `y` in `tint`, answering the surface the
    /// cells stand on.
    fn fill(&mut self, column: Column, y: u16, tint: Option<[u8; 3]>) -> [u8; 3] {
        match tint {
            Some(tint) => {
                fill_line_tint(self.buf, column.x, y, usize::from(column.width), tint);
                tint
            },
            None => self.colors.bg,
        }
    }

    /// Push `column`'s change bar on row `y`.
    fn bar(&mut self, column: Column, y: u16, color: [u8; 3]) {
        self.bars.push(
            Rect::new(column.x, y, 1, 1),
            Bar {
                x: BAR_X,
                y: 0,
                width: BAR_WIDTH,
                height: 16,
                color,
            },
        );
    }

    /// Push `num` right-aligned in `column`'s number field on row `y`, backed
    /// by the surface `bg`.
    fn number(&mut self, column: Column, y: u16, num: u32, color: [u8; 3], bg: [u8; 3]) {
        self.scratch.clear();
        write!(self.scratch, "{num}").expect("writing to a String is infallible");
        let advance = text_run::advance_sixteenths(self.scratch.len(), self.cols.number_scale);
        self.numbers.push(
            Rect::new(column.x, y, 1, 1),
            TextRun {
                col: self.cols.number_right.saturating_sub(advance) as i16,
                row: 0,
                scale: self.cols.number_scale,
                color,
                bg: Some(bg),
                text: &self.scratch,
                follow: 0,
                anchor: None,
            },
        );
    }
}

/// The scale the grid's numbers and chips draw at, the board's 10 px text.
pub(super) fn number_scale(theme: &Theme) -> u16 {
    theme.type_scale(TypeRole::Small)
}

/// The digits a line number field holds for `doc`.
///
/// The field fits the merged column's last line and the last line of each
/// side, so the three columns line their text up at one offset.
pub(super) fn digits(snapshot: &DisplaySnapshot, doc: &MergeDoc) -> u16 {
    let last = |side: fn(&MergeRow) -> Option<&ReviewSide>| {
        doc.rows
            .iter()
            .rev()
            .find_map(side)
            .map_or(0, |side| side.line_num)
    };
    let lines = snapshot
        .buffer_snapshot()
        .line_count()
        .max(last(|row| row.ours.as_ref()))
        .max(last(|row| row.theirs.as_ref()));
    editor::gutter_digits(lines)
}

/// Classify plan row `row` of `doc`, whose chunks stand at `states`.
pub(super) fn row_kind(row: &AlignRow, doc: &MergeDoc, states: &[ChunkState]) -> MergeRowKind {
    if let Some(chunk) = row.chunk {
        return match states[chunk] {
            ChunkState::Unresolved => MergeRowKind::Conflict,
            ChunkState::Ours => MergeRowKind::TookOurs,
            ChunkState::Theirs => MergeRowKind::TookTheirs,
            ChunkState::Both => MergeRowKind::Settled("both"),
            ChunkState::Picked => MergeRowKind::Settled("picked"),
            ChunkState::AutoIndent => MergeRowKind::Settled("auto \u{b7} indent"),
            ChunkState::Manual => MergeRowKind::Settled("edited"),
        };
    }

    // A row outside a chunk shows one merge row on both sides.
    let Some(merge_row) = row.ours.or(row.theirs).map(|index| &doc.rows[index]) else {
        return MergeRowKind::Context;
    };
    let base = merge_row.base.as_ref();
    match (
        edited(base, merge_row.ours.as_ref()),
        edited(base, merge_row.theirs.as_ref()),
    ) {
        (true, _) => MergeRowKind::Ours,
        (false, true) => MergeRowKind::Theirs,
        (false, false) => MergeRowKind::Context,
    }
}

/// Paint the header row `area`.
///
/// The row holds each column's name and label, the side keys, the merged
/// column's fill, the rule under the row, and the hairlines through it. The
/// names and labels draw at `scale`, the board's 11 px chrome text. The merged
/// column names the file at `path`.
pub(super) fn paint_header(
    area: Rect,
    grid: &Grid,
    labels: &ConflictLabels,
    path: &str,
    scale: u16,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let Grid { cols, colors } = grid;
    let y = area.y;
    let merged_bg = match colors.head {
        Some(head) => {
            fill_line_tint(buf, cols.merged.x, y, usize::from(cols.merged.width), head);
            head
        },
        None => colors.bg,
    };

    let mut cell = |column: Column, name: (&str, [u8; 3]), label: &str, keys: &str, bg| {
        header_cell(
            HeaderCell {
                column,
                y,
                name,
                label,
                keys,
                muted: colors.muted,
                bg,
                scale,
            },
            buf,
            scene,
        )
    };
    cell(
        cols.ours,
        ("ours", colors.ours.fg),
        &labels.ours,
        OURS_KEYS,
        colors.bg,
    );
    cell(cols.merged, ("merged", colors.text), path, "", merged_bg);
    cell(
        cols.theirs,
        ("theirs", colors.theirs.fg),
        &labels.theirs,
        THEIRS_KEYS,
        colors.bg,
    );

    if let Some(color) = colors.rule {
        Bar {
            x: 0,
            y: 15,
            width: area.width * 16,
            height: 1,
            color,
        }
        .render(Rect::new(area.x, y, 1, 1), buf, scene);
    }
    let mut bars = BarBatch::default();
    push_hairlines(&mut bars, cols, y, 1, colors.hairline);
    bars.flush(scene);
}

/// Paint the grid's rows over `body` from display row `scroll_row`.
///
/// The cells take the tints, the absent fill, and the text. The scene takes
/// the bars, the dividers, the hairlines, the numbers, and each run's chip.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_rows(
    snapshot: &DisplaySnapshot,
    scroll_row: u32,
    body: Rect,
    grid: &Grid,
    doc: &MergeDoc,
    derived: &ConflictDerived,
    fallback_style: Style,
    theme: &Theme,
    buf: &mut Buffer,
    endpoint_cache: Option<&mut Option<CachedHighlightEndpoints>>,
    row_cache: Option<&mut Option<DiffRowCache>>,
    paint: GridPaint<'_>,
) {
    let Grid { cols, colors } = grid;
    let ConflictDerived { states, plan, .. } = derived;
    let end_row = (scroll_row + u32::from(body.height)).min(snapshot.line_count());
    if end_row <= scroll_row {
        return;
    }

    let endpoints = match endpoint_cache {
        Some(cache) => snapshot.highlighted_endpoints_cached(scroll_row..end_row, cache),
        None => snapshot.highlighted_endpoints(scroll_row..end_row),
    };
    // One replay for the loop, since the rows ascend.
    let mut row_cursor = snapshot.row_highlight_cursor(endpoints);
    let mut local_row_cache = None;
    let row_states = review::diff_row_states(
        snapshot,
        scroll_row..end_row,
        row_cache.unwrap_or(&mut local_row_cache),
    );
    let inlay_style = fallback_style.patch(theme.get(s::UI_VIRTUAL_INLAY));
    let span_style = |[r, g, b]: [u8; 3]| Style::default().fg(Color::Rgb(r, g, b));

    let mut painter = RowPainter {
        cols,
        colors,
        buf,
        numbers: TextRunBatch::default(),
        bars: BarBatch::default(),
        hatched: Vec::new(),
        scratch: String::new(),
    };
    let mut chips = Vec::new();

    for display_row in scroll_row..end_row {
        let y = body.y + (display_row - scroll_row) as u16;
        let index = display_row as usize;
        let plan_row = plan.get(index);
        let kind = plan_row.map_or(MergeRowKind::Context, |row| row_kind(row, doc, states));
        let ours = plan_row.and_then(|row| row.ours).map(|i| &doc.rows[i]);
        let theirs = plan_row.and_then(|row| row.theirs).map(|i| &doc.rows[i]);
        let look = RowLook::of(
            kind,
            [
                ours.is_some_and(|row| edited(row.base.as_ref(), row.ours.as_ref())),
                theirs.is_some_and(|row| edited(row.base.as_ref(), row.theirs.as_ref())),
            ],
            colors,
        );

        // A row past the plan, such as the empty line after the last newline,
        // pairs with no merge row, so its sides draw nothing at all.
        if plan_row.is_some() {
            painter.side(
                cols.ours,
                y,
                ours.and_then(|row| row.ours.as_ref()),
                look.ours,
                fallback_style,
                span_style(colors.ours.fg),
            );
            painter.side(
                cols.theirs,
                y,
                theirs.and_then(|row| row.theirs.as_ref()),
                look.theirs,
                fallback_style,
                span_style(colors.theirs.fg),
            );
        }

        let merged = cols.merged;
        let merged_bg = painter.fill(merged, y, look.merged.tint);
        if let Some(color) = look.merged.bar {
            painter.bar(merged, y, color);
        }
        let chip = look
            .chip
            .filter(|_| opens_run(plan, index, kind, doc, states))
            .map(|(text, tone)| (text, tone, cols.chip_cells(text)))
            .filter(|&(_, _, cells)| cells <= merged.right().saturating_sub(merged.text_x));
        let text_end = merged.right() - chip.map_or(0, |(_, _, cells)| cells);

        if let DiffRowKind::BufferRow { buffer_row } = row_states[index - scroll_row as usize].kind
        {
            // FIXME: A text run carries no weight, so the cursor row's number
            // draws at regular weight where the MergeGrid board draws it bold.
            let color = match paint.cursor_row == Some(display_row) {
                true => colors.text,
                false => colors.muted,
            };
            painter.number(merged, y, buffer_row + 1, color, merged_bg);
            paint_highlighted_row(
                snapshot,
                display_row,
                merged.text_x,
                y,
                merged.text_cells(text_end),
                painter.buf,
                fallback_style,
                fallback_style,
                fallback_style,
                inlay_style,
                &[],
                // No change span reaches a conflict row, so the span mark and
                // the tint that rides it never engage.
                None,
                None,
                None,
                DiffDials::shipped(),
                None,
                &mut row_cursor,
            );
            if look.ghost {
                for x in merged.text_x..text_end {
                    painter.buf[(x, y)].set_style(colors.ghost);
                }
            }
        }

        if let Some((text, tone, cells)) = chip {
            // A pool page captures no panel, so its chip box is the cells.
            let box_bg = match paint.panels {
                true => merged_bg,
                false => tone.tint,
            };
            for x in text_end..merged.right() {
                painter.buf[(x, y)]
                    .set_char(' ')
                    .set_bg(Color::Rgb(box_bg[0], box_bg[1], box_bg[2]));
            }
            chips.push((Rect::new(text_end, y, cells, 1), text, tone));
        }
    }

    let rows = (end_row - scroll_row) as u16;
    for column in [cols.ours, cols.merged, cols.theirs] {
        painter.bars.push(
            Rect::new(column.x, body.y, 1, 1),
            Bar {
                x: cols.divider_x(),
                y: 0,
                width: 1,
                height: rows * 16,
                color: colors.divider,
            },
        );
    }
    push_hairlines(&mut painter.bars, cols, body.y, rows, colors.hairline);

    let RowPainter {
        mut numbers,
        mut bars,
        hatched,
        buf,
        ..
    } = painter;
    if let (Some(fill), Some(hatch)) = (colors.absent, colors.absent_hatch) {
        for cells in hatched {
            Bar {
                x: 0,
                y: 0,
                width: cells.width * 16,
                height: 16,
                color: fill,
            }
            .render_hatched(hatch, cells, paint.scene);
        }
    }
    bars.flush(paint.scene);
    numbers.flush(paint.scene);
    for (area, text, tone) in chips {
        draw_chip(
            area,
            text,
            tone,
            cols.number_scale,
            paint.panels,
            buf,
            paint.scene,
        );
    }
}

/// Whether `side` is a line its ancestor line `base` does not hold, as an
/// added line or an edited one is.
fn edited(base: Option<&ReviewSide>, side: Option<&ReviewSide>) -> bool {
    match (base, side) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some(base), Some(side)) => side.text != base.text,
    }
}

/// Whether plan row `index` opens a run of rows of its `kind`, the row the
/// run's chip sits on.
fn opens_run(
    plan: &[AlignRow],
    index: usize,
    kind: MergeRowKind,
    doc: &MergeDoc,
    states: &[ChunkState],
) -> bool {
    let Some(prev) = index.checked_sub(1).and_then(|prev| plan.get(prev)) else {
        return true;
    };
    prev.chunk != plan[index].chunk || row_kind(prev, doc, states) != kind
}

/// Emit `cell`'s runs, all in from the column's edges by the board's padding.
///
/// The name leads, the label follows it, and the keys sit against the column's
/// right edge. A label too long for the room drops its tail, and keys with no
/// room after the name drop whole.
fn header_cell(cell: HeaderCell<'_>, buf: &mut Buffer, scene: &mut ApcScene) {
    let HeaderCell {
        column,
        y,
        name: (name, name_color),
        label,
        keys,
        muted,
        bg,
        scale,
    } = cell;
    let advance = |text: &str| text_run::advance_sixteenths(text.chars().count(), scale);
    let mut run = |col: u16, text: &str, color: [u8; 3]| {
        TextRun {
            col: col as i16,
            row: 0,
            scale,
            color,
            bg: Some(bg),
            text,
            follow: 0,
            anchor: None,
        }
        .render(Rect::new(0, y, 1, 1), buf, scene);
    };

    let start = column.x * 16 + HEADER_PAD;
    let end = (column.right() * 16).saturating_sub(HEADER_PAD);
    let label_x = start + advance(name) + HEADER_GAP;
    if start + advance(name) > end {
        return;
    }
    // FIXME: A text run carries no weight, so the column names draw at regular
    // weight where the MergeGrid board draws them bold.
    run(start, name, name_color);

    let keys_x = end.saturating_sub(advance(keys));
    let keys_fit = !keys.is_empty() && keys_x >= label_x;
    let label_end = match keys_fit {
        true => keys_x.saturating_sub(HEADER_GAP),
        false => end,
    };
    let label = fit(label, label_end.saturating_sub(label_x), scale);
    if !label.is_empty() {
        run(label_x, label, muted);
    }
    if keys_fit {
        run(keys_x, keys, muted);
    }
}

/// The longest leading part of `text` whose run at `scale` fits in `room`
/// sixteenths.
fn fit(text: &str, room: u16, scale: u16) -> &str {
    let mut end = 0;
    for (count, (index, ch)) in text.char_indices().enumerate() {
        if text_run::advance_sixteenths(count + 1, scale) > room {
            break;
        }
        end = index + ch.len_utf8();
    }
    &text[..end]
}

/// Push the two hairlines on the merged column's edges, over `rows` rows from
/// row `y`.
///
/// They sit just outside the merged column, so a chip that ends on the
/// column's right edge never covers one.
fn push_hairlines(bars: &mut BarBatch, cols: &GridColumns, y: u16, rows: u16, color: [u8; 3]) {
    let height = rows * 16;
    for (x, offset) in [(cols.merged.x, -1), (cols.merged.right(), 0)] {
        bars.push(
            Rect::new(x, y, 1, 1),
            Bar {
                x: offset,
                y: 0,
                width: 1,
                height,
                color,
            },
        );
    }
}

/// Draw a chip reading `text` over `area`, its text in `tone`'s color on its
/// tint.
///
/// With `panel` set the box is a rounded panel. Without it the cells beneath
/// already carry the box.
///
/// The panel is a backdrop, so the pane pool's composite covers it through a
/// glide. The page under the composite paints the chip too, so the chip
/// travels with its row and does not hold its settled place.
fn draw_chip(
    area: Rect,
    text: &str,
    tone: Tone,
    scale: u16,
    panel: bool,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    if panel {
        Panel {
            style: BorderStyle::None_,
            border: tone.tint,
            corner_radius: 1,
            fill: Some(tone.tint),
            shadow: PanelShadow::None_,
            inset_x: 0,
            inset_y: 0,
            corners: 0,
            fillets: 0,
            hatch: None,
            above_pools: false,
            backdrop: true,
            anchor: None,
        }
        .draw_components(area, scene);
    }
    TextRun {
        col: CHIP_PAD as i16,
        row: 0,
        scale,
        color: tone.fg,
        bg: Some(tone.tint),
        text,
        follow: 0,
        anchor: None,
    }
    .render(area, buf, scene);
}

#[cfg(test)]
mod tests;

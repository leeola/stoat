//! The MergeGrid layout of the three-way conflict view.
//!
//! The Merge board draws a conflicted file as three columns, which are ours,
//! the merged result, and theirs. Above them a column header names the three
//! sides, and a File Header names the file and counts its conflicts. Each
//! column leads with a change bar, a line number, and a divider. Each row takes
//! the tint of the side the merge takes it from, and a chip on the first row of
//! each run names how the merge settles it.
//!
//! The bars, hairlines, numbers, and chips are smaller than a cell, so they are
//! scene components, and the layout paints only under a live scene. Without one
//! the view keeps the cell layout of [`super::render_conflict_view`].

use super::{ConflictHover, GridPaint, BAND_TINT};
use crate::{
    conflict_session::{ConflictDerived, ConflictLabels, ConflictPosition},
    display_map::{CachedHighlightEndpoints, DisplaySnapshot},
    merge_view::{AlignRow, ChunkState, MergeDoc, MergeRow, Side},
    render::{
        diff_changes, editor,
        paint::{dim_rgb, fill_line_tint, render_side_text, style_rgb},
        review::{self, paint_highlighted_row, DiffDials, DiffRowCache, DiffRowKind},
        TypeScale,
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
use stoat_action::ActionKind;
use stoat_widgets::{
    bar::{Bar, BarBatch},
    panel::Panel,
    text_run::{self, TextRun, TextRunBatch},
    ApcScene,
};
use stoatty_protocol::command::{BorderStyle, Edge, PanelShadow};

/// Rows the header takes above the grid's rows, the column header and then the
/// File Header.
pub const HEADER_ROWS: u16 = 2;

/// The File Header tag's `padding: 0 5px`, in sixteenths.
const TAG_PAD: u16 = 10;

/// The change bar's left edge and width, in sixteenths from its column's left
/// edge, the board's `margin-left: 4px; width: 3px`.
///
/// A board px across is two sixteenths, so at 9 px cells the bar takes pixel
/// columns 4 to 6 of its column's first cell.
const BAR_X: i16 = 7;
const BAR_WIDTH: u16 = 6;

/// Where the number field starts, in sixteenths from its column's left edge,
/// right after the change bar, the board's 7 px.
const NUMBER_X: u16 = 14;

/// The right edge of the number field, in sixteenths from its column's left
/// edge, the end of the board's `width: 26px` field.
const NUMBER_RIGHT: u16 = 58;

/// The board's `margin: 0 6px` on each side of the 1/16 divider after the
/// number field, in sixteenths.
const DIVIDER_MARGIN: u16 = 10;

/// The header cells' `padding: 0 10px` and `gap: 8px`, in sixteenths.
const HEADER_PAD: u16 = 20;
const HEADER_GAP: u16 = 16;

/// The chip's `padding: 0 6px`, in sixteenths.
const CHIP_PAD: u16 = 12;

/// The board's `margin-right: 6px` between a chip and its column's right edge,
/// in sixteenths.
const CHIP_MARGIN: u16 = 12;

/// The chip's height, the board's `line-height: 16px` on an 18 px row, in
/// sixteenths down.
const CHIP_HEIGHT: u16 = 14;

/// The radius of the chip's corners, the board's `border-radius: 4px`, in
/// logical pixels.
const CHIP_RADIUS: u8 = 4;

/// The board's 2 px between the pick buttons an open chunk's chip offers, in
/// sixteenths.
const BUTTON_GAP: u16 = 4;

/// How far a hovered side's cells wash toward white, the board's
/// `background: rgba(255,255,255,0.03)`.
const HOVER_WASH: f32 = 0.03;

/// The radius of the hover ring's corners, the board's `border-radius: 2px`,
/// in logical pixels.
const RING_RADIUS: u8 = 2;

/// The hover pill's `padding: 0 6px`, in sixteenths.
const PILL_PAD: u16 = 12;

/// The board's `right: 6px` between the hover pill and the ring's right edge,
/// in logical pixels for the pill's box and in sixteenths for its cells.
const PILL_END_PX: u8 = 6;
const PILL_END: u16 = 12;

/// The hover pill's `bottom: 2px` and the 2 px its 14 px line leaves above
/// it on the 18 px row, in logical pixels.
const PILL_INSET: u8 = 2;

/// The radius of the hover pill's corners, the board's `border-radius: 4px`,
/// in logical pixels.
const PILL_RADIUS: u8 = 4;

/// The pick buttons an open chunk's chip offers, with the pick each takes, in
/// the order they stand.
const BUTTONS: [(&str, ActionKind); 3] = [
    ("ours", ActionKind::ConflictPickOurs),
    ("theirs", ActionKind::ConflictPickTheirs),
    ("both", ActionKind::ConflictPickBoth),
];

/// How far the untaken side's text blends toward its surface, the board's
/// `opacity: 0.45`.
const UNTAKEN_FADE: f32 = 0.55;

/// The keys each side's header names, for taking a chunk or one line from it.
const OURS_KEYS: &str = "click \u{b7} o \u{b7} O line";
const THEIRS_KEYS: &str = "click \u{b7} t \u{b7} T line";

/// The merged column's header label, which names the column as the file the
/// resolution writes.
const MERGED_LABEL: &str = "working tree \u{b7} you edit here";

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
pub struct GridColumns {
    pub ours: Column,
    pub merged: Column,
    pub theirs: Column,
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
    /// The numbers right-align against the board's field, and a number too
    /// long for it moves the field's edge out. The text starts on the first
    /// whole cell past the divider's margin, so it stays on the cell grid the
    /// buffer's text paints on.
    pub fn compute(area: Rect, digits: u16, number_scale: u16) -> Self {
        let side = (u32::from(area.width) * 100 / 312) as u16;
        let merged = area.width.saturating_sub(2 * side);
        let number_right = NUMBER_RIGHT
            .max(NUMBER_X + text_run::advance_sixteenths(usize::from(digits), number_scale));
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
    /// The cells hold the chip's box, its text and the board's padding on each
    /// side, and the board's margin after it, rounded up to whole cells.
    fn chip_cells(&self, text: &str) -> u16 {
        (chip_width(text, self.number_scale) + CHIP_MARGIN).div_ceil(16)
    }
}

/// One MergeGrid column, in cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Column {
    pub x: u16,
    pub(super) width: u16,
    /// The first cell of the column's text, past its bar, number, and divider.
    pub text_x: u16,
}

impl Column {
    pub fn right(self) -> u16 {
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
/// conflict header's color, the chip takes the band, each side's chip takes
/// its side, the both button takes the Fold Buttons' color and then the chip,
/// the tag takes the chip, the resolved tag takes the ours chip, and the ghost
/// takes muted italic text. A missing tint is its side's color washed toward
/// the surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct GridColors {
    pub(super) bg: [u8; 3],
    pub(super) text: [u8; 3],
    pub(super) muted: [u8; 3],
    pub(super) hairline: [u8; 3],
    pub(super) divider: [u8; 3],
    /// The rule under the header row, the divider's color.
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
    /// The chip of a run the merge takes from ours, or a chunk resolved to it.
    pub(super) chip_ours: Tone,
    /// The chip of a run the merge takes from theirs, or a chunk resolved to
    /// it.
    pub(super) chip_theirs: Tone,
    /// The both button among the pick buttons an open chunk's chip offers.
    pub(super) chip_both: Tone,
    /// The File Header's tag while the shown file holds an open conflict.
    pub(super) tag: Tone,
    /// The File Header's tag once the shown file holds no open conflict.
    pub(super) tag_resolved: Tone,
    /// The fill of the hover's pill, `ui.surface.chrome`'s background or the
    /// surface.
    pub(super) chrome: [u8; 3],
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
        let ours = tone(theme.get(s::VCS_CONFLICT_OURS))?;
        let theirs = tone(theme.get(s::VCS_CONFLICT_THEIRS))?;
        let scoped =
            |scope: &str, fallback: Tone| theme.try_get(scope).and_then(tone).unwrap_or(fallback);
        let chip = scoped(s::VCS_CONFLICT_CHIP, band);
        let chip_ours = scoped(s::VCS_CONFLICT_CHIP_OURS, ours);
        let [r, g, b] = muted;

        Some(Self {
            bg,
            text,
            muted,
            hairline,
            divider,
            rule: style_rgb(theme.get(s::UI_DIVIDER).fg)
                .or_else(|| style_rgb(theme.get(s::UI_SURFACE_RAISED).bg)),
            head: style_rgb(theme.get(s::VCS_CONFLICT_HEAD).bg),
            absent: style_rgb(theme.get(s::DIFF_ABSENT).bg),
            absent_hatch: style_rgb(theme.get(s::DIFF_ABSENT).fg),
            ghost: theme.try_get(s::VCS_CONFLICT_GHOST).unwrap_or_else(|| {
                Style::default()
                    .fg(Color::Rgb(r, g, b))
                    .add_modifier(Modifier::ITALIC)
            }),
            ours,
            theirs,
            band,
            chip,
            chip_ours,
            chip_theirs: scoped(s::VCS_CONFLICT_CHIP_THEIRS, theirs),
            chip_both: scoped(
                s::VCS_CONFLICT_CHIP_BOTH,
                theme
                    .try_get(s::DIFF_FOLD_BUTTON)
                    .and_then(tone)
                    .unwrap_or(chip),
            ),
            tag: scoped(s::VCS_CONFLICT_TAG, chip),
            tag_resolved: scoped(s::VCS_CONFLICT_TAG_RESOLVED, chip_ours),
            chrome: style_rgb(
                theme
                    .try_get(s::UI_SURFACE_CHROME)
                    .and_then(|style| style.bg),
            )
            .unwrap_or(bg),
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
                chip: Some(("auto \u{b7} ours", colors.chip_ours)),
                ..Self::default()
            },
            MergeRowKind::Theirs => Self {
                merged: barred(colors.theirs),
                theirs: lit(colors.theirs),
                chip: Some(("auto \u{b7} theirs", colors.chip_theirs)),
                ..Self::default()
            },
            MergeRowKind::Conflict => unsettled(true, "conflict \u{b7} o t b"),
            MergeRowKind::Settled(chip) => unsettled(false, chip),
            MergeRowKind::TookOurs => Self {
                ours: lit(colors.ours),
                merged: barred(colors.ours),
                theirs: faded,
                chip: Some(("\u{2713} took ours", colors.chip_ours)),
                ..Self::default()
            },
            MergeRowKind::TookTheirs => Self {
                ours: faded,
                merged: barred(colors.theirs),
                theirs: lit(colors.theirs),
                chip: Some(("\u{2713} took theirs", colors.chip_theirs)),
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
    /// The scale of the name and the label, the board's 12 px header text.
    text_scale: u16,
    /// The scale of the keys, the board's 11 px.
    keys_scale: u16,
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
        self.number(column, y, side.line_num, self.colors.muted, bg, false);

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
                radius: 0,
                corners: 0,
            },
        );
    }

    /// Push `num` right-aligned in `column`'s number field on row `y`, backed
    /// by the surface `bg`, in the bold face where `bold` is set.
    fn number(
        &mut self,
        column: Column,
        y: u16,
        num: u32,
        color: [u8; 3],
        bg: [u8; 3],
        bold: bool,
    ) {
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
                bold,
                italic: false,
                follow: 0,
                anchor: None,
            },
        );
    }
}

/// The scale the grid's numbers and chips draw at, the board's 10 px text.
pub fn number_scale(theme: &Theme) -> u16 {
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
/// names and labels draw at `text_scale`, the board's 12 px header text, and
/// the keys at `keys_scale`, its 11 px. Each side names its commit as `labels`
/// reads it, and the merged column names the working tree.
pub(super) fn paint_header(
    area: Rect,
    grid: &Grid,
    labels: &ConflictLabels,
    text_scale: u16,
    keys_scale: u16,
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
                text_scale,
                keys_scale,
            },
            buf,
            scene,
        )
    };
    cell(
        cols.ours,
        ("ours", colors.ours.fg),
        &labels.ours_label(),
        OURS_KEYS,
        colors.bg,
    );
    cell(
        cols.merged,
        ("merged", colors.text),
        MERGED_LABEL,
        "",
        merged_bg,
    );
    cell(
        cols.theirs,
        ("theirs", colors.theirs.fg),
        &labels.theirs_label(),
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
            radius: 0,
            corners: 0,
        }
        .render(Rect::new(area.x, y, 1, 1), buf, scene);
    }
    let mut bars = BarBatch::default();
    push_hairlines(&mut bars, cols, y, 1, colors.hairline);
    bars.flush(scene);
}

/// What the File Header names about the shown file.
pub(super) struct FileFacts<'a> {
    /// The file's path from the repository's root.
    pub(super) path: &'a str,
    /// The session's position, `None` for a paint no frame read one for.
    pub(super) position: Option<ConflictPosition>,
    /// The lines theirs adds and removes against ours.
    pub(super) counts: (u32, u32),
}

/// Paint the File Header row `area`, under the column header.
///
/// The row fills with the head color between a rule at its top and one at its
/// bottom, and the merged column's hairlines cross it. The merged column names
/// the file. The ours column ends in the file's tag and its counts against the
/// merged column, and the theirs column ends in the file's place in the
/// session.
///
/// The tag reads `conflict U+00B7 {total}` while every chunk is open,
/// `conflict U+00B7 {open} of {total} left` while some are, and
/// `U+2713 resolved` at none. With no position the row leaves out the tag, the
/// counts, and the place.
pub(super) fn paint_file_header(
    area: Rect,
    grid: &Grid,
    file: FileFacts<'_>,
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let Grid { cols, colors } = grid;
    let y = area.y;
    let fill = colors.head.unwrap_or(colors.bg);
    fill_line_tint(buf, area.x, y, usize::from(area.width), fill);

    let mut bars = BarBatch::default();
    if let Some(color) = colors.rule {
        for rule_y in [0, 15] {
            bars.push(
                Rect::new(area.x, y, 1, 1),
                Bar {
                    x: 0,
                    y: rule_y,
                    width: area.width * 16,
                    height: 1,
                    color,
                    radius: 0,
                    corners: 0,
                },
            );
        }
    }
    push_hairlines(&mut bars, cols, y, 1, colors.hairline);
    bars.flush(scene);

    let ui = theme.type_scale(TypeRole::Ui);
    let caption = theme.type_scale(TypeRole::Caption);
    let at = Rect::new(0, y, 1, 1);

    let mut col = cols.merged.x * 16 + HEADER_PAD;
    let path_end = (cols.merged.right() * 16).saturating_sub(HEADER_PAD);
    for (whole, style) in diff_changes::header_path(file.path, theme) {
        let text = fit(&whole, path_end.saturating_sub(col), ui);
        if text.is_empty() {
            break;
        }
        let color = style_rgb(style.fg).unwrap_or(colors.text);
        let bold = style.add_modifier.contains(Modifier::BOLD);
        header_text(col, text, ui, color, bold, fill).render(at, buf, scene);
        col += advance(text, ui);
    }

    let Some(position) = file.position else {
        return;
    };
    let (tag_text, tag) = match position.file_open {
        0 => ("\u{2713} resolved".to_string(), colors.tag_resolved),
        open if open == position.chunk_total => (format!("conflict \u{b7} {open}"), colors.tag),
        open => (
            format!("conflict \u{b7} {open} of {} left", position.chunk_total),
            colors.tag,
        ),
    };
    let (added, removed) = file.counts;
    let count_color = |scope: &str| style_rgb(theme.get(scope).fg).unwrap_or(colors.text);
    let counts: Vec<(String, [u8; 3])> = [
        (added, "+", count_color(s::DIFF_ADDED)),
        (removed, "\u{2212}", count_color(s::DIFF_DELETED)),
    ]
    .into_iter()
    .filter(|&(count, ..)| count > 0)
    .map(|(count, sign, color)| (format!("{sign}{count}"), color))
    .collect();

    let small = cols.number_scale;
    let tag_width = advance(&tag_text, small) + 2 * TAG_PAD;
    let group = counts.iter().fold(tag_width, |width, (text, _)| {
        width + HEADER_GAP + advance(text, caption)
    });
    let group_end = (cols.merged.x * 16).saturating_sub(HEADER_PAD);
    let ours_start = cols.ours.x * 16 + HEADER_PAD;
    if let Some(start) = group_end
        .checked_sub(group)
        .filter(|&start| start >= ours_start)
    {
        Bar {
            x: start as i16,
            y: 1,
            width: tag_width,
            height: CHIP_HEIGHT,
            color: tag.tint,
            radius: CHIP_RADIUS,
            corners: 0,
        }
        .render(at, buf, scene);
        TextRun {
            col: (start + TAG_PAD) as i16,
            row: 0,
            scale: small,
            color: tag.fg,
            bg: None,
            text: &tag_text,
            bold: false,
            italic: false,
            follow: 0,
            anchor: None,
        }
        .render(at, buf, scene);
        let mut col = start + tag_width;
        for (text, color) in &counts {
            col += HEADER_GAP;
            header_text(col, text, caption, *color, false, fill).render(at, buf, scene);
            col += advance(text, caption);
        }
    }

    let place = format!("file {} / {}", position.file_at, position.file_total);
    let disabled = style_rgb(theme.get(s::UI_TEXT_DISABLED).fg).unwrap_or(colors.muted);
    let place_x = (cols.theirs.right() * 16).saturating_sub(HEADER_PAD + advance(&place, caption));
    if place_x >= cols.theirs.x * 16 + HEADER_PAD {
        header_text(place_x, &place, caption, disabled, false, fill).render(at, buf, scene);
    }
}

/// A File Header run of `text` from `col`, on the row's fill `bg`.
fn header_text(
    col: u16,
    text: &str,
    scale: u16,
    color: [u8; 3],
    bold: bool,
    bg: [u8; 3],
) -> TextRun<'_> {
    TextRun {
        col: col as i16,
        row: 0,
        scale,
        color,
        bg: Some(bg),
        text,
        bold,
        italic: false,
        follow: 0,
        anchor: None,
    }
}

/// The sixteenths `text` advances at `scale`.
fn advance(text: &str, scale: u16) -> u16 {
    text_run::advance_sixteenths(text.chars().count(), scale)
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
    mut paint: GridPaint<'_>,
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
    let wrap_style = fallback_style.patch(editor::wrap_mark_style(theme));
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
    let mut buttons = None;
    let button_scale = theme.type_scale(TypeRole::Small);
    let button_cells = (buttons_width(button_scale) + CHIP_MARGIN).div_ceil(16);
    // A click pick settles the hovered chunk while the pointer rests, and a
    // settled chunk takes no pick from the pointer.
    let hover = paint.hover.filter(|hover| {
        matches!(
            states.get(hover.chunk),
            Some(ChunkState::Unresolved | ChunkState::Picked)
        )
    });
    let mut ring: Option<(u16, u16)> = None;

    for display_row in scroll_row..end_row {
        let y = body.y + (display_row - scroll_row) as u16;
        let index = display_row as usize;
        let plan_row = plan.get(index);
        let kind = plan_row.map_or(MergeRowKind::Context, |row| row_kind(row, doc, states));
        let ours = plan_row.and_then(|row| row.ours).map(|i| &doc.rows[i]);
        let theirs = plan_row.and_then(|row| row.theirs).map(|i| &doc.rows[i]);
        let mut look = RowLook::of(
            kind,
            [
                ours.is_some_and(|row| edited(row.base.as_ref(), row.ours.as_ref())),
                theirs.is_some_and(|row| edited(row.base.as_ref(), row.theirs.as_ref())),
            ],
            colors,
        );
        if let Some(hover) = hover.filter(|&hover| hovers(hover, plan_row)) {
            let cell = match hover.side {
                Side::Ours => &mut look.ours,
                Side::Theirs => &mut look.theirs,
            };
            let under = cell.tint.unwrap_or(colors.bg);
            cell.tint = Some(dim_rgb(under, [0xff, 0xff, 0xff], HOVER_WASH));
            ring = Some(ring.map_or((y, y), |(top, _)| (top, y)));
        }

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
        let chunk = plan_row.and_then(|row| row.chunk);
        let mark = look
            .chip
            .filter(|_| opens_run(plan, index, kind, doc, states))
            .map(|(text, tone)| match chunk {
                Some(chunk) if paint.chip_menu == Some(chunk) => RunMark::Buttons(button_cells),
                _ => RunMark::Chip(text, tone, cols.chip_cells(text)),
            })
            .filter(|mark| mark.cells() <= merged.right().saturating_sub(merged.text_x));
        let text_end = merged.right() - mark.as_ref().map_or(0, RunMark::cells);

        if let DiffRowKind::BufferRow { buffer_row } = row_states[index - scroll_row as usize].kind
        {
            let at_cursor = paint.cursor_row == Some(display_row);
            let color = match at_cursor {
                true => colors.text,
                false => colors.muted,
            };
            painter.number(merged, y, buffer_row + 1, color, merged_bg, at_cursor);
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
                wrap_style,
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

        match mark {
            Some(RunMark::Chip(text, tone, cells)) => {
                chips.push((Rect::new(text_end, y, cells, 1), text, tone, chunk));
            },
            Some(RunMark::Buttons(cells)) => buttons = Some(Rect::new(text_end, y, cells, 1)),
            None => {},
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
                radius: 0,
                corners: 0,
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
                radius: 0,
                corners: 0,
            }
            .render_hatched(hatch, cells, paint.scene);
        }
    }
    bars.flush(paint.scene);
    numbers.flush(paint.scene);
    for (area, text, tone, chunk) in chips {
        draw_chip(area, text, tone, cols.number_scale, buf, paint.scene);
        if let (Some(layout), Some(chunk)) = (paint.layout.as_deref_mut(), chunk) {
            layout.chips.push((area, chunk));
        }
    }
    if let Some(area) = buttons {
        let tones = [colors.chip_ours, colors.chip_theirs, colors.chip_both];
        let cells = draw_buttons(area, tones, button_scale, buf, paint.scene);
        if let Some(layout) = paint.layout.as_deref_mut() {
            layout.buttons = cells;
        }
    }
    if let (Some(hover), Some((top, bottom))) = (hover, ring) {
        let (column, tone) = match hover.side {
            Side::Ours => (cols.ours, colors.ours),
            Side::Theirs => (cols.theirs, colors.theirs),
        };
        let area = Rect::new(column.x, top, column.width, bottom - top + 1);
        let pill = Pill {
            text: pill_text(hover),
            color: tone.fg,
            fill: colors.chrome,
            scale: button_scale,
        };
        draw_hover(area, pill, buf, paint.scene);
    }
}

/// Whether plan row `row` lies under `hover`, a row of its chunk, or with Alt
/// held the one row whose line on the hovered side is the hovered line.
fn hovers(hover: ConflictHover, row: Option<&AlignRow>) -> bool {
    row.is_some_and(|row| {
        let line = match hover.side {
            Side::Ours => row.ours,
            Side::Theirs => row.theirs,
        };
        row.chunk == Some(hover.chunk)
            && hover
                .merge_row
                .is_none_or(|merge_row| line == Some(merge_row))
    })
}

/// The pick the hover's pill names, what a press takes and the key that takes
/// the same.
fn pill_text(hover: ConflictHover) -> &'static str {
    match (hover.merge_row.is_some(), hover.side) {
        (false, Side::Ours) => "click \u{b7} take ours \u{b7} o",
        (false, Side::Theirs) => "click \u{b7} take theirs \u{b7} t",
        (true, Side::Ours) => "alt-click \u{b7} take line \u{b7} O",
        (true, Side::Theirs) => "alt-click \u{b7} take line \u{b7} T",
    }
}

/// The pill that names a hover's pick, its text, its colors, and its scale.
struct Pill {
    text: &'static str,
    /// The text's color and the stroke around the pill.
    color: [u8; 3],
    fill: [u8; 3],
    scale: u16,
}

/// Draw the hover's ring over `area`, a hovered side's rows, and `pill` on the
/// area's last row against its right edge.
///
/// The ring is a stroke with no fill, since a filled panel hides the row tints
/// under it, so the cells under the ring carry the hover's wash instead. The
/// pill's cells hold its text, [`PILL_PAD`] at each side of it, and the
/// [`PILL_END`] the pill stands in from the ring's right edge.
fn draw_hover(area: Rect, pill: Pill, buf: &mut Buffer, scene: &mut ApcScene) {
    let stroke = |radius: u8, fill: Option<[u8; 3]>, edges: [Edge; 4]| Panel {
        style: BorderStyle::Rounded,
        border: pill.color,
        corner_radius: radius,
        fill,
        shadow: PanelShadow::None_,
        edges,
        corners: 0,
        fillets: 0,
        fillet_radius: 2,
        hatch: None,
        above_pools: false,
        backdrop: false,
        anchor: None,
    };
    stroke(RING_RADIUS, None, Edge::symmetric(0, 0)).draw_components(area, scene);

    let advance = text_run::advance_sixteenths(pill.text.chars().count(), pill.scale);
    let cells = (advance + 2 * PILL_PAD + PILL_END)
        .div_ceil(16)
        .min(area.width);
    let at = Rect::new(area.right() - cells, area.bottom() - 1, cells, 1);
    let edges = [
        Edge::Inset(PILL_INSET),
        Edge::Inset(PILL_END_PX),
        Edge::Inset(PILL_INSET),
        Edge::Inset(0),
    ];
    stroke(PILL_RADIUS, Some(pill.fill), edges).draw_components(at, scene);
    TextRun {
        col: PILL_PAD as i16,
        row: 0,
        scale: pill.scale,
        color: pill.color,
        bg: None,
        text: pill.text,
        bold: false,
        italic: false,
        follow: 0,
        anchor: None,
    }
    .render(at, buf, scene);
}

/// What stands at the right end of a run's first merged row.
enum RunMark {
    /// The chip that names the run, with its text, its tone, and its cells.
    Chip(&'static str, Tone, u16),
    /// The open chip menu's pick buttons in the chip's place, with their cells.
    Buttons(u16),
}

impl RunMark {
    /// The cells the mark takes from the end of the merged text.
    fn cells(&self) -> u16 {
        match *self {
            Self::Chip(_, _, cells) | Self::Buttons(cells) => cells,
        }
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
        text_scale,
        keys_scale,
    } = cell;
    let advance =
        |text: &str, scale: u16| text_run::advance_sixteenths(text.chars().count(), scale);
    let mut run = |col: u16, text: &str, scale: u16, color: [u8; 3], bold: bool| {
        TextRun {
            col: col as i16,
            row: 0,
            scale,
            color,
            bg: Some(bg),
            text,
            bold,
            italic: false,
            follow: 0,
            anchor: None,
        }
        .render(Rect::new(0, y, 1, 1), buf, scene);
    };

    let start = column.x * 16 + HEADER_PAD;
    let end = (column.right() * 16).saturating_sub(HEADER_PAD);
    let name_end = start + advance(name, text_scale);
    if name_end > end {
        return;
    }
    run(start, name, text_scale, name_color, true);

    let label_x = name_end + HEADER_GAP;
    let keys_x = end.saturating_sub(advance(keys, keys_scale));
    let keys_fit = !keys.is_empty() && keys_x >= label_x;
    let label_end = match keys_fit {
        true => keys_x.saturating_sub(HEADER_GAP),
        false => end,
    };
    let label = fit(label, label_end.saturating_sub(label_x), text_scale);
    if !label.is_empty() {
        run(label_x, label, text_scale, muted, false);
    }
    if keys_fit {
        run(keys_x, keys, keys_scale, muted, false);
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
                radius: 0,
                corners: 0,
            },
        );
    }
}

/// Draw a chip reading `text` over `area`, its text in `tone`'s color on a
/// rounded box in its tint.
///
/// The box ends [`CHIP_MARGIN`] before the area's right edge and stands in
/// from the row's top and bottom, as the board's 16 px chip sits on its 18 px
/// row. The box is a bar, which places in sixteenths and rides a pool page, so
/// the chip travels with its row through a glide.
fn draw_chip(
    area: Rect,
    text: &str,
    tone: Tone,
    scale: u16,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let width = chip_width(text, scale);
    let x = (area.width * 16).saturating_sub(CHIP_MARGIN + width);
    // The box renders alone and before the run, since a bar batch drops the
    // corner radius and the run's glyphs draw over the box.
    Bar {
        x: x as i16,
        y: 1,
        width,
        height: CHIP_HEIGHT,
        color: tone.tint,
        radius: CHIP_RADIUS,
        corners: 0,
    }
    .render(area, buf, scene);
    TextRun {
        col: (x + CHIP_PAD) as i16,
        row: 0,
        scale,
        color: tone.fg,
        bg: None,
        text,
        bold: false,
        italic: false,
        follow: 0,
        anchor: None,
    }
    .render(area, buf, scene);
}

/// The width of a chip's box reading `text` at `scale`, its text and the
/// board's padding on each side, in sixteenths.
fn chip_width(text: &str, scale: u16) -> u16 {
    text_run::advance_sixteenths(text.chars().count(), scale) + 2 * CHIP_PAD
}

/// Draw the open chip menu's pick buttons over `area`, the last ending where a
/// chip's box ends, and answer each button's cells with the pick it takes.
///
/// Each button is a rounded bar under its label at `scale`, as a chip is, in
/// its tone of `tones`. A button's cells are the ones whose centers it covers,
/// so no cell falls to two buttons across the gap between them.
fn draw_buttons(
    area: Rect,
    tones: [Tone; 3],
    scale: u16,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) -> Vec<(Rect, ActionKind)> {
    let mut x = (area.width * 16).saturating_sub(CHIP_MARGIN + buttons_width(scale));
    let mut cells = Vec::with_capacity(BUTTONS.len());
    for ((label, kind), tone) in BUTTONS.into_iter().zip(tones) {
        let width = chip_width(label, scale);
        // The box renders alone and before the run, as a chip's does.
        Bar {
            x: x as i16,
            y: 1,
            width,
            height: CHIP_HEIGHT,
            color: tone.tint,
            radius: CHIP_RADIUS,
            corners: 0,
        }
        .render(area, buf, scene);
        TextRun {
            col: (x + CHIP_PAD) as i16,
            row: 0,
            scale,
            color: tone.fg,
            bg: None,
            text: label,
            bold: false,
            italic: false,
            follow: 0,
            anchor: None,
        }
        .render(area, buf, scene);

        let start = area.x * 16 + x;
        let covered = (start + 7) / 16..(start + width + 7) / 16;
        cells.push((
            Rect::new(covered.start, area.y, covered.end - covered.start, 1),
            kind,
        ));
        x += width + BUTTON_GAP;
    }
    cells
}

/// The width of the open chip menu's pick buttons at `scale`, each a chip's
/// box around its label, with [`BUTTON_GAP`] between them, in sixteenths.
fn buttons_width(scale: u16) -> u16 {
    let boxes: u16 = BUTTONS
        .iter()
        .map(|&(label, _)| chip_width(label, scale))
        .sum();
    boxes + BUTTON_GAP * (BUTTONS.len() as u16 - 1)
}

#[cfg(test)]
mod tests {
    use super::{
        paint_file_header, paint_header, row_kind, FileFacts, Grid, GridColors, GridColumns,
        MergeRowKind, RowLook, Tone,
    };
    use crate::{
        app::DEFAULT_KEYMAP,
        conflict_session::{ConflictLabels, ConflictPosition},
        host::git::MergeState,
        merge_view::{ChunkState, MergeDoc},
        render::paint::style_rgb,
        test_fixture::render_conflict_view_grid::{
            rgb, AREA, CHIP_CONFLICT, CHIP_OURS, CHIP_THEIRS, DANGER, DIVIDER, MUTED, SPECIAL,
            SUCCESS, TEXT,
        },
        theme::Theme,
    };
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Modifier, Style},
    };
    use stoat_widgets::ApcScene;
    use stoatty_protocol::command::{decode_stream, Command};

    const MERGED_HEAD: [u8; 3] = [0x1b, 0x1d, 0x22];

    const CONFLICT: [u8; 3] = [0xf5, 0x9a, 0xa0];

    const TAG_CONFLICT: [u8; 3] = [0x3a, 0x20, 0x26];

    const DISABLED: [u8; 3] = [0x5b, 0x61, 0x6d];

    #[test]
    fn the_columns_split_the_width_one_to_one_point_one_two_to_one() {
        let layout = |width, digits, scale| {
            let cols = GridColumns::compute(Rect::new(1, 0, width, 10), digits, scale);
            [cols.ours, cols.merged, cols.theirs].map(|col| (col.x, col.width, col.text_x))
        };

        assert_eq!(
            [
                layout(158, 2, 208),
                layout(151, 2, 208),
                layout(158, 4, 256)
            ],
            [
                [(1, 50, 6), (51, 58, 56), (109, 50, 114)],
                [(1, 48, 6), (49, 55, 54), (104, 48, 109)],
                [(1, 50, 8), (51, 58, 58), (109, 50, 116)],
            ],
            "the sides take 100 / 312 each, the text starts on the cell past the bar, \
             the number field, and the divider's margins, and a number too long for \
             the field moves the text out",
        );
    }

    #[test]
    fn rows_classify_by_the_side_the_merge_takes() {
        let doc = MergeDoc::build(
            "keep\nmine\nyours\nboth\ntail\n",
            "keep\nMINE\nyours\nOURS\ntail\n",
            "keep\nmine\nYOURS\nTHEIRS\ntail\n",
            None,
        );
        let plan = doc.align(&[]);
        let kinds = |state| {
            plan.iter()
                .map(|row| row_kind(row, &doc, &[state]))
                .collect::<Vec<_>>()
        };

        use MergeRowKind::{Conflict, Context, Ours, Settled, Theirs, TookOurs, TookTheirs};
        assert_eq!(
            kinds(ChunkState::Unresolved),
            [Context, Ours, Theirs, Conflict, Conflict, Conflict, Conflict, Conflict, Context],
            "a one-sided edit takes its side, and the marker block is the conflict",
        );
        assert_eq!(
            [
                ChunkState::Ours,
                ChunkState::Theirs,
                ChunkState::Both,
                ChunkState::Picked,
                ChunkState::AutoIndent,
                ChunkState::Manual,
            ]
            .map(|state| kinds(state)[3]),
            [
                TookOurs,
                TookTheirs,
                Settled("both"),
                Settled("picked"),
                Settled("auto \u{b7} indent"),
                Settled("edited"),
            ],
            "a settled chunk's rows take the kind of its resolution",
        );
    }

    #[test]
    fn inserted_lines_classify_by_the_side_that_added_them() {
        let doc = MergeDoc::build("keep\n", "keep\nmine\n", "theirs\nkeep\n", None);
        let kinds: Vec<_> = doc
            .align(&[])
            .iter()
            .map(|row| row_kind(row, &doc, &[]))
            .collect();

        assert_eq!(
            kinds,
            [
                MergeRowKind::Theirs,
                MergeRowKind::Context,
                MergeRowKind::Ours
            ]
        );
    }

    /// The header names each side's commit and the merged column's working tree,
    /// each name and label at the Ui scale and the keys at the Caption scale, all
    /// 20 sixteenths in from their column's edges. The merged cell fills with the
    /// head color, and a rule in the divider's color underlines the row.
    ///
    /// At the Ui scale of 232 a character advances 14.5 sixteenths, and at the
    /// Caption scale of 216 it advances 13.5. So the ours label starts after the
    /// name's 58 and a gap of 16, and the 18 characters of its keys end 20 short
    /// of the column's 800.
    #[test]
    fn the_header_names_each_side_and_the_merged_column() {
        let (config, errors) = stoat_config::parse(DEFAULT_KEYMAP);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        let theme = Theme::from_config(&config.expect("the shipped config parses"), "ermine_night")
            .expect("ermine_night loads");
        let grid = Grid {
            cols: GridColumns::compute(AREA, 2, 208),
            colors: GridColors::resolve(&theme).expect("ermine_night gives RGB colors"),
        };
        let merge = MergeState {
            head_sha: "f00dfeed".to_string(),
            head_name: Some("feature".to_string()),
            message: String::new(),
            conflicted: 1,
        };
        let labels = ConflictLabels::new(Some("main".to_string()), Some(&merge));
        let mut buf = Buffer::empty(AREA);
        let mut scene = ApcScene::new();
        paint_header(
            Rect { height: 1, ..AREA },
            &grid,
            &labels,
            232,
            216,
            &mut buf,
            &mut scene,
        );

        let commands = decode_stream(scene.buffer());
        let runs: Vec<_> = commands
            .iter()
            .filter_map(|command| match command {
                Command::TextRun(run) => Some((run.col, run.text.clone(), run.scale, run.color)),
                _ => None,
            })
            .collect();
        let rule = commands.iter().find_map(|command| match command {
            Command::Bar(bar) if bar.y == 15 => Some((bar.x, bar.width, bar.height, bar.color)),
            _ => None,
        });
        let bg = |x: u16| style_rgb(Some(buf[(x, 0)].bg));

        assert_eq!(
            (runs, bg(grid.cols.merged.x), bg(grid.cols.ours.x), rule),
            (
                [
                    (20, "ours", 232, SUCCESS),
                    (94, "HEAD \u{b7} main", 232, MUTED),
                    (537, "click \u{b7} o \u{b7} O line", 216, MUTED),
                    (820, "merged", 232, TEXT),
                    (923, "working tree \u{b7} you edit here", 232, MUTED),
                    (1748, "theirs", 232, SPECIAL),
                    (1851, "feature \u{b7} f00dfee", 232, MUTED),
                    (2265, "click \u{b7} t \u{b7} T line", 216, MUTED),
                ]
                .map(|(col, text, scale, color)| (col, text.to_string(), scale, color))
                .to_vec(),
                Some(MERGED_HEAD),
                None,
                Some((0, AREA.width * 16, 1, DIVIDER)),
            ),
        );
    }

    /// A column too narrow for its label cuts the label's tail at the label's own
    /// scale. At 84 cells a side column is 26 cells. The ours column leaves 43
    /// sixteenths between its name and its keys, so two characters of 14.5 fit
    /// and a third does not. The longer name of the theirs column leaves 14, too
    /// few for one character, so its label draws no run. The merged column's 32
    /// cells leave room for 25 characters.
    #[test]
    fn a_narrow_column_cuts_its_label_at_the_label_scale() {
        let (config, errors) = stoat_config::parse(DEFAULT_KEYMAP);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        let theme = Theme::from_config(&config.expect("the shipped config parses"), "ermine_night")
            .expect("ermine_night loads");
        let area = Rect::new(0, 0, 84, 1);
        let grid = Grid {
            cols: GridColumns::compute(area, 2, 208),
            colors: GridColors::resolve(&theme).expect("ermine_night gives RGB colors"),
        };
        let merge = MergeState {
            head_sha: "f00dfeed".to_string(),
            head_name: Some("feature".to_string()),
            message: String::new(),
            conflicted: 1,
        };
        let labels = ConflictLabels::new(Some("main".to_string()), Some(&merge));
        let mut scene = ApcScene::new();
        paint_header(
            area,
            &grid,
            &labels,
            232,
            216,
            &mut Buffer::empty(area),
            &mut scene,
        );

        let cut: Vec<String> = decode_stream(scene.buffer())
            .into_iter()
            .filter_map(|command| match command {
                Command::TextRun(run) if run.color == MUTED && run.scale == 232 => Some(run.text),
                _ => None,
            })
            .collect();
        assert_eq!(cut, ["HE", "working tree \u{b7} you edit h"]);
    }

    /// The File Header runs a rule along its top and its bottom on the head color.
    /// It tags the shown file's open conflicts and counts what theirs changes,
    /// both against the merged column, names the file from the merged column's
    /// pad, and ends in the file's place in the session against the theirs
    /// column's right edge. The tag reads every open chunk as the total, some as
    /// how many of the total stay left, and none as resolved, in the resolved
    /// tag's colors. A zero count takes no run.
    ///
    /// At the test scale every character advances 16 sixteenths. So the tag of
    /// `conflict U+00B7 2` is 192 wide with 10 of pad each side, and the tag and
    /// its two counts, 16 apart, end 20 short of the merged column's 800.
    #[test]
    fn the_file_header_tags_the_open_conflicts_and_counts_the_file() {
        let tags = [2, 1, 0].map(|open| {
            let (commands, _) = file_header(AREA, open, (9, 4));
            let tag = commands.iter().find_map(|command| match command {
                Command::Bar(bar) if bar.radius > 0 => {
                    Some((bar.x, bar.y, bar.width, bar.height, bar.color))
                },
                _ => None,
            });
            let text = commands.iter().find_map(|command| match command {
                Command::TextRun(run) if run.bg.is_none() => {
                    Some((run.col, run.text.clone(), run.color))
                },
                _ => None,
            });
            (tag, text)
        });
        let (commands, buf) = file_header(AREA, 2, (9, 4));
        let rules: Vec<_> = commands
            .iter()
            .filter_map(|command| match command {
                Command::Bar(bar) if bar.width == AREA.width * 16 => Some((bar.y, bar.color)),
                _ => None,
            })
            .collect();
        let runs: Vec<_> = commands
            .into_iter()
            .filter_map(|command| match command {
                Command::TextRun(run) if run.bg.is_some() => {
                    Some((run.col, run.text, run.color, run.bold))
                },
                _ => None,
            })
            .collect();
        let one_count: Vec<_> = file_header(AREA, 2, (0, 3))
            .0
            .into_iter()
            .filter_map(|command| match command {
                Command::TextRun(run) if run.text.starts_with(['+', '\u{2212}']) => {
                    Some((run.col, run.text))
                },
                _ => None,
            })
            .collect();

        assert_eq!(
            (
                tags,
                rules,
                runs,
                style_rgb(Some(buf[(0, 0)].bg)),
                one_count
            ),
            (
                [
                    (
                        Some((472, 1, 212, 14, TAG_CONFLICT)),
                        Some((482, "conflict \u{b7} 2".to_string(), CONFLICT)),
                    ),
                    (
                        Some((312, 1, 372, 14, TAG_CONFLICT)),
                        Some((322, "conflict \u{b7} 1 of 2 left".to_string(), CONFLICT)),
                    ),
                    (
                        Some((504, 1, 180, 14, CHIP_OURS)),
                        Some((514, "\u{2713} resolved".to_string(), SUCCESS)),
                    ),
                ],
                vec![(0, DIVIDER), (15, DIVIDER)],
                vec![
                    (820, "src/".to_string(), MUTED, false),
                    (884, "notes".to_string(), TEXT, true),
                    (700, "+9".to_string(), SUCCESS, false),
                    (748, "\u{2212}4".to_string(), DANGER, false),
                    (2348, "file 1 / 1".to_string(), DISABLED, false),
                ],
                Some(MERGED_HEAD),
                vec![(748, "\u{2212}3".to_string())],
            ),
        );
    }

    /// A column too narrow for its part of the File Header drops that part whole.
    /// At 66 cells the ours column has too little room for the tag and its counts,
    /// and at 30 cells the theirs column has too little room for the place.
    #[test]
    fn a_narrow_column_drops_its_part_of_the_file_header() {
        let parts = [66, 30].map(|width| {
            let (commands, _) = file_header(Rect::new(0, 0, width, 13), 2, (9, 4));
            let tag = commands
                .iter()
                .any(|command| matches!(command, Command::Bar(bar) if bar.radius > 0));
            let place = commands.iter().any(
                |command| matches!(command, Command::TextRun(run) if run.text.starts_with("file")),
            );
            (tag, place)
        });

        assert_eq!(parts, [(false, true), (false, false)]);
    }

    /// Paint the File Header in `ermine_night` over the first row of `area` for
    /// `src/notes` with `open` of its two chunks open and `counts` lines theirs
    /// adds and removes, and answer the scene's commands and the cells.
    fn file_header(area: Rect, open: usize, counts: (u32, u32)) -> (Vec<Command>, Buffer) {
        let (config, errors) = stoat_config::parse(DEFAULT_KEYMAP);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        let theme = Theme::from_config(&config.expect("the shipped config parses"), "ermine_night")
            .expect("ermine_night loads");
        let grid = Grid {
            cols: GridColumns::compute(area, 2, 256),
            colors: GridColors::resolve(&theme).expect("ermine_night gives RGB colors"),
        };
        let position = ConflictPosition {
            operation: "merge",
            chunk_at: None,
            chunk_total: 2,
            file_at: 1,
            file_total: 1,
            file_open: open,
            chunks_open: open,
            files_open: usize::from(open > 0),
        };
        let facts = FileFacts {
            path: "src/notes",
            position: Some(position),
            counts,
        };
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();
        paint_file_header(
            Rect { height: 1, ..area },
            &grid,
            facts,
            &theme,
            &mut buf,
            &mut scene,
        );
        (decode_stream(scene.buffer()), buf)
    }

    /// Each chip takes the tone of its kind. A run or a chunk the merge takes from
    /// ours takes the ours chip, one from theirs the theirs chip, and an unsettled
    /// chunk the conflict chip.
    #[test]
    fn each_chip_takes_the_tone_of_its_kind() {
        let (config, errors) = stoat_config::parse(DEFAULT_KEYMAP);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        let theme = Theme::from_config(&config.expect("the shipped config parses"), "ermine_night")
            .expect("ermine_night loads");
        let colors = GridColors::resolve(&theme).expect("ermine_night gives RGB colors");
        let tint = |kind| {
            RowLook::of(kind, [false, false], &colors)
                .chip
                .map(|(_, tone)| tone.tint)
        };

        assert_eq!(
            [
                MergeRowKind::Ours,
                MergeRowKind::TookOurs,
                MergeRowKind::Theirs,
                MergeRowKind::TookTheirs,
                MergeRowKind::Conflict,
            ]
            .map(tint),
            [
                CHIP_OURS,
                CHIP_OURS,
                CHIP_THEIRS,
                CHIP_THEIRS,
                CHIP_CONFLICT
            ]
            .map(Some),
        );
    }

    #[test]
    fn a_theme_without_the_board_scopes_falls_back_to_their_neighbors() {
        let (config, errors) = stoat_config::parse(DEFAULT_KEYMAP);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        let theme = Theme::from_config(&config.expect("the shipped config parses"), "default_dark")
            .expect("default_dark loads");
        // default_dark's danger, #e06c75, washed 85% toward its surface, #282c34.
        let band = Tone {
            fg: [0xe0, 0x6c, 0x75],
            tint: [0x44, 0x36, 0x3e],
        };
        let colors = GridColors::resolve(&theme).expect("default_dark gives RGB colors");

        assert_eq!(
            (
                colors.band,
                colors.chip,
                [colors.chip_ours, colors.chip_theirs],
                colors.chip_both,
                [colors.tag, colors.tag_resolved],
                colors.ghost,
                colors.head,
                colors.chrome
            ),
            (
                band,
                band,
                [colors.ours, colors.theirs],
                Tone {
                    fg: [0x56, 0xb6, 0xc2],
                    tint: [0x33, 0x38, 0x42],
                },
                [band, colors.ours],
                Style::default()
                    .fg(rgb([0x63, 0x6d, 0x83]))
                    .add_modifier(Modifier::ITALIC),
                None,
                colors.bg,
            ),
            "the band takes the conflict header's color, the chip the band, each \
             side's chip its side, the both button the Fold Buttons' cyan on gray, \
             the tag the chip, the resolved tag the ours chip, the ghost muted \
             italic text, and the hover pill the surface",
        );
    }
}

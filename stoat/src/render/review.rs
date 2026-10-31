use super::{
    editor,
    paint::{
        brighten_rgb, brighten_style, dim_rgb, luma, paint_style_runs, render_side_num, style_rgb,
    },
    TEXT_SCALE_COMPACT,
};
use crate::{
    app::Stoat,
    diff_map::{ChangeKind, DiffHunk, DiffHunkStatus, DiffMap, MarkState, ProseChange},
    display_map::{
        highlights::HighlightStyle, syntax_theme::DiffTheme, tab_map, BaseLineLayout, Block,
        BlockPlacement, BlockRowKind, CachedHighlightEndpoints, DisplayPoint, DisplaySnapshot,
        RowHighlightCursor, WrapPoint,
    },
    editor_state::EditorState,
    host::DiffStatus,
    minimap::{self, ClassTable},
    theme::scope as s,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::StatefulWidget,
};
use std::{
    cmp::Ordering,
    collections::BTreeMap,
    fmt::Write,
    hash::{DefaultHasher, Hash, Hasher},
};
use stoat_text::Point;
use stoat_widgets::{
    bar::Bar,
    text_run::{self, TextRun},
    ApcScene,
};

/// Fraction an unchanged row's token foregrounds blend toward the editor
/// background, leaving 60% of the syntax color.
///
/// Context receding is what makes a changed row stand out, the changed row
/// itself painting its syntax colors at full strength. Nothing washes a whole
/// line, so the palette carries the emphasis and the eye lands on the rows
/// still at full strength.
pub(crate) const CONTEXT_SOFTEN: f32 = 0.40;

/// Fraction the unchanged chars of a token-refined row blend toward the editor
/// background, leaving 75% of the syntax color.
///
/// Lighter than [`CONTEXT_SOFTEN`] on purpose. The row is changed content, so
/// it must still read as ahead of the context around it, while the chars the
/// refinement marks pop hardest inside their own line.
pub(crate) const MODIFIED_ROW_SOFTEN: f32 = 0.25;

/// Furthest the diff view's soften may be turned down. Lands the scale on zero,
/// which disables softening outright and restores the paint from before it.
pub(crate) const DIFF_SOFTEN_MIN: i8 = -2;

/// Furthest the soften turns up, landing the context blend on [`SOFTEN_CAP`].
///
/// The largest level that still moves a context row. One step further scales
/// past the cap, where the trim takes the excess back and the row paints
/// exactly as it did a step earlier.
pub(crate) const DIFF_SOFTEN_MAX: i8 = 3;

/// Ceiling on a scaled soften fraction, leaving a twentieth of the syntax color.
///
/// Where the top level lands, and the readability floor the dial stops at. A
/// fraction of 1.0 blends a foreground into the background exactly, which
/// leaves text nobody reads.
const SOFTEN_CAP: f32 = 0.95;

/// Multiplier `level` applies to [`CONTEXT_SOFTEN`] and [`MODIFIED_ROW_SOFTEN`].
///
/// Level 0 returns 1.0, so an untouched session paints the shipped fractions
/// exactly. Each step is a half, so two steps down turn softening off and
/// three steps up reach the readability floor. The ends sit at different
/// distances because the scale starts at 1.0, which is nearer to zero than to
/// the 2.5 the floor needs.
///
/// A caller must skip [`soften_style`] entirely on a zero scale rather than pass
/// it a zero amount, because that call drops bold whatever amount it is given.
pub(crate) fn diff_soften_scale(level: i8) -> f32 {
    1.0 + 0.5 * f32::from(level.clamp(DIFF_SOFTEN_MIN, DIFF_SOFTEN_MAX))
}

/// Furthest the tint dial turns up, landing a changed char on its status color
/// outright.
///
/// Two steps of a half each. The top is where the syntax color is gone
/// entirely, which is the readable flat form the theme already tunes its
/// `diff.*` colors for, so nothing above it would say more.
pub(crate) const DIFF_TINT_MAX: i8 = 2;

/// Fraction a changed char's foreground shifts toward its status color at
/// `level`.
///
/// Level 0 is off and paints exactly the colors from before the dial, which is
/// where a session starts. Level 1 leans the syntax color halfway toward the
/// status hue, and level 2 replaces it. A level below zero is the same as off,
/// so the dial's bottom needs no separate floor.
///
/// The same fraction drains the color out of an unchanged row, so one dial
/// trades color between the two populations rather than only adding it.
pub(crate) fn diff_tint_amount(level: i8) -> f32 {
    0.5 * f32::from(level.clamp(0, DIFF_TINT_MAX))
}

/// The diff view's paint dials, resolved from their session levels for one
/// paint.
///
/// Every painter of a diff row reads each dial, so the dials travel as one
/// value. A further dial is then one more field rather than one more argument
/// on each painter.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct DiffDials {
    /// Multiplier on how far unchanged syntax recedes, per
    /// [`diff_soften_scale`]. `1.0` paints the shipped fractions and `0.0`
    /// turns softening off.
    pub(crate) soften_scale: f32,
    /// Fraction a changed row shifts toward its status color, per
    /// [`diff_tint_amount`]. `0.0` paints the syntax color untouched and `1.0`
    /// replaces it.
    pub(crate) tint_amount: f32,
    /// Whether every change span bolds, whatever its kind. Off leaves bold to a
    /// prose replacement alone, per [`mark_span`].
    pub(crate) bold: bool,
    /// Whether every change span underlines, whatever the theme. Off leaves the
    /// underline to a theme that does not blend and to a changed part of
    /// unstructured text, such as a string or a comment, per [`mark_span`].
    pub(crate) underline: bool,
}

impl DiffDials {
    /// Resolves the dials from the session's [`Stoat::diff_soften`] and
    /// [`Stoat::diff_tint`] levels and its [`Stoat::diff_bold`] and
    /// [`Stoat::diff_underline`] flags.
    pub(crate) fn from_stoat(stoat: &Stoat) -> Self {
        Self {
            soften_scale: diff_soften_scale(stoat.diff_soften),
            tint_amount: diff_tint_amount(stoat.diff_tint),
            bold: stoat.diff_bold,
            underline: stoat.diff_underline,
        }
    }

    /// The dials a fresh session paints with.
    ///
    /// A modal preview and a plain pane paint with these too, because no dial
    /// turns there.
    pub(crate) fn shipped() -> Self {
        Self {
            soften_scale: 1.0,
            tint_amount: 0.0,
            bold: false,
            underline: false,
        }
    }
}

/// Paint an editor as a side-by-side diff, with base (HEAD) text on the left and
/// the live syntax-highlighted buffer on the right, row-aligned through the
/// display map's deleted-block splicing.
///
/// The right column runs the same highlighted pipeline as a plain editor, so the
/// buffer stays fully editable and colored. The left column shows removed and
/// modified base lines (as spliced block rows) in the diff-deleted style and
/// mirrors unchanged lines dimmed. A refined modified line keeps the deleted
/// style for its changed chars alone. Added and modified new lines leave it
/// blank.
/// Line numbers are base-file lines on the left and buffer lines on the right.
/// With `current_line` set, the right column counts from that 1-based buffer
/// line, as a relative gutter does. The left column always numbers the base
/// file, because the cursor never sits on a base row.
///
/// Both columns soft-wrap at the narrower one's width, which the caller sets on
/// the display map. A continuation row carries text alone, with no number, no
/// status glyph, and no base line of its own.
///
/// Lays its columns out per [`DiffLayout::diff_view`], sized for the larger of
/// the two files' line counts. When a `scene` is threaded (a stoatty terminal)
/// the gutter paints with the rich sub-cell components, otherwise it falls
/// back to the ASCII gutter.
///
/// A focused pane paints its selections, cursors, and goto-word labels over the
/// right column, as a plain pane does through [`editor::paint_selections`] and
/// [`editor::paint_goto_word_labels`]. An unfocused pane paints no selection,
/// cursor, or label. With a `scene` and `delegates_cursor` the terminal draws
/// the primary cursor, so the paint leaves that cell to it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_diff_view(
    editor: &mut EditorState,
    inner: Rect,
    fallback_style: Style,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    is_focused: bool,
    goto_word_labels: Option<&BTreeMap<String, (usize, usize)>>,
    scene: Option<&mut ApcScene>,
    delegates_cursor: bool,
    dials: DiffDials,
    current_line: Option<u32>,
) {
    let stoatty = scene.is_some();
    // The base rows pair with the live ones only where there are two columns to
    // pair across, so the width decides it, and it has to be settled before the
    // snapshot because it changes which blocks the display map splices.
    editor
        .display_map
        .set_pair_modified_hunks(inner.width >= DIFF_TWO_COLUMN_MIN);
    let snapshot = editor.display_map.snapshot();
    let digits = editor
        .stack_gutter_digits
        .unwrap_or_else(|| diff_gutter_digits(&snapshot));
    let cols = DiffColumns::compute(
        inner,
        DiffLayout::diff_view(digits, diff_gutter_is_rich(stoatty, theme, fallback_style)),
    );
    editor.text_rect = Some(Rect {
        x: cols.right_text_x,
        y: inner.y,
        width: cols.right_content_w as u16,
        height: inner.height,
    });
    paint_diff_rows(
        &snapshot,
        editor.scroll_row,
        inner,
        digits,
        fallback_style,
        theme,
        buf,
        scene,
        0.0,
        dials,
        current_line,
        Some(&mut editor.highlight_endpoint_cache),
        Some(&mut editor.diff_row_cache),
    );

    let end_row = (editor.scroll_row + inner.height as u32).min(snapshot.line_count());
    let text = editor.text_rect.expect("set above");
    if !is_focused {
        return;
    }

    let visible = editor::visible_byte_range(
        &snapshot,
        snapshot.buffer_snapshot().rope(),
        editor.scroll_row,
        end_row,
    );
    editor::paint_selections(
        editor,
        &snapshot,
        text,
        end_row,
        &visible,
        theme,
        buf,
        stoatty && delegates_cursor,
    );
    if let Some(labels) = goto_word_labels {
        editor::paint_goto_word_labels(
            labels,
            &snapshot,
            text,
            editor.scroll_row,
            end_row,
            fallback_style,
            buf,
        );
    }
}

/// Minimum inner width for the two-column diff layout. Below this each text
/// half falls under about 43 columns after its gutters, too cramped to read
/// code, so the view collapses to a single unified column.
pub(crate) const DIFF_TWO_COLUMN_MIN: u16 = 100;

/// What one display row is, without borrowing the snapshot it came from.
///
/// [`BlockRowKind`] holds a reference to the block a row belongs to, which ties
/// it to one snapshot and keeps it out of a cache that spans frames. Neither the
/// diff body nor the conflict body reads that block, so only the discriminant,
/// the block row's offset, the row the block follows, and the buffer row survive
/// here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DiffRowKind {
    /// A row of a spliced block. A deleted base line is a block of its own, so
    /// `segment` is which of its wrapped rows this is, zero for the first. A
    /// paired base line's overflow block carries on past the segments its live
    /// line showed, so its rows start past zero.
    ///
    /// `after_row` is the buffer row a `Below` block sits below, and `None` for
    /// any other placement, which no base-line block takes. When a fold hides
    /// that row, the block paints after the fold's placeholder, so the walk
    /// counts the hidden rows up to it before the block's own base lines.
    Block {
        segment: u32,
        after_row: Option<u32>,
    },
    BufferRow {
        buffer_row: u32,
    },
}

/// One display row's derived state, as the diff and conflict bodies read it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct DiffRowState {
    pub(crate) kind: DiffRowKind,
    pub(crate) status: DiffStatus,
    pub(crate) staged: Option<MarkState>,
    /// Whether a base row sits beside this one in the left column.
    ///
    /// A modified hunk pairs base row i with live row i as far as its base text
    /// reaches, so its first rows carry one and its last do not. False for
    /// every other status: an unchanged row mirrors its own base line by a
    /// different route, and an added one has no base row at all.
    pub(crate) paired: bool,
    /// Whether this row is a soft-wrap continuation of the buffer row above.
    ///
    /// A continuation carries the row's text and nothing else: no line number,
    /// no status glyph, and no base line of its own.
    pub(crate) continuation: bool,
    /// Whether a `Modified` hunk with token detail covers this `Modified` row,
    /// so the chars outside its change spans are unchanged text.
    ///
    /// An empty detail counts, so a whitespace-only row reads as text whole.
    /// The test is the hunk and not a non-empty [`Self::change_spans`]. A wrap
    /// continuation of a refined line holds no span when the change falls on
    /// another of its rows, and its chars are still unchanged text. False for a
    /// block row.
    pub(crate) refined: bool,
    /// Whether the right column marks this block row with a removal tick.
    ///
    /// Only the first row of a pure removal ticks, and only when the live rows
    /// beside the gap carry no bar. False for every live row.
    pub(crate) removal_tick: bool,
    /// Display-column ranges to mark, each with its kind and its
    /// [`ProseChange`]. Empty for a row no hunk refines.
    ///
    /// Only the part of each span that falls on this display row, so a wrapped
    /// line's rows each mark their own cells.
    pub(crate) change_spans: Vec<(std::ops::Range<usize>, ChangeKind, ProseChange)>,
}

/// The visible rows' derived state, held across repaints.
///
/// Every entry costs a block-tree descent to classify the row and a hunk-tree
/// seek for its status, its staged flag and its change spans, and the diff and
/// conflict bodies asked for all of that per row per frame. None of it moves
/// while the buffer, the display map, the diff and the painted window stand
/// still, which is the ordinary case between two frames.
pub(crate) struct DiffRowCache {
    key: u64,
    rows: Vec<DiffRowState>,
}

/// Hash what the cached rows are derived from.
///
/// A change to any of these misses and rebuilds. The theme, the tints and the
/// column geometry are deliberately absent, none of them reaching the state
/// being cached.
///
/// The wrap version is separate from the map version because a width change
/// moves no fold, yet it moves which rows are continuations and where each
/// change span falls.
fn diff_row_key(
    scroll_row: u32,
    visible: u32,
    buffer_version: u64,
    map_version: usize,
    wrap_version: u64,
    diff_version: usize,
    paired: bool,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    scroll_row.hash(&mut hasher);
    visible.hash(&mut hasher);
    buffer_version.hash(&mut hasher);
    map_version.hash(&mut hasher);
    wrap_version.hash(&mut hasher);
    diff_version.hash(&mut hasher);
    paired.hash(&mut hasher);
    hasher.finish()
}

/// The derived state of `rows`, rebuilding into `cache` only when its key moved.
///
/// The rows are borrowed from the cache rather than handed back by value, a
/// copy per frame being the cost this exists to avoid. A caller with no cache of
/// its own passes a local one, which builds once and drops with the paint.
pub(crate) fn diff_row_states<'c>(
    snapshot: &DisplaySnapshot,
    rows: std::ops::Range<u32>,
    cache: &'c mut Option<DiffRowCache>,
) -> &'c [DiffRowState] {
    let key = diff_row_key(
        rows.start,
        rows.end - rows.start,
        snapshot.buffer_snapshot().version(),
        snapshot.version(),
        snapshot.wrap_snapshot().version(),
        snapshot.diff_map().map_or(0, |dm| dm.version()),
        snapshot.pairs_modified_hunks(),
    );
    if cache.as_ref().is_none_or(|c| c.key != key) {
        *cache = Some(DiffRowCache {
            key,
            rows: build_diff_row_states(snapshot, rows),
        });
    }
    &cache.as_ref().expect("filled just above").rows
}

fn build_diff_row_states(
    snapshot: &DisplaySnapshot,
    rows: std::ops::Range<u32>,
) -> Vec<DiffRowState> {
    let mut hunk_scratch: Vec<&DiffHunk> = Vec::new();
    rows.map(|display_row| {
        let buffer_row = match snapshot.classify_row(display_row) {
            BlockRowKind::BufferRow { buffer_row } => buffer_row,
            BlockRowKind::Block { block, line_index } => {
                let segment = block.0.first_segment + line_index;
                let after_row = match block.0.placement {
                    BlockPlacement::Below(row) => Some(row),
                    _ => None,
                };
                return DiffRowState {
                    kind: DiffRowKind::Block { segment, after_row },
                    status: DiffStatus::Unchanged,
                    staged: None,
                    paired: false,
                    continuation: segment > 0,
                    refined: false,
                    removal_tick: takes_removal_tick(snapshot, display_row, block),
                    change_spans: Vec::new(),
                };
            },
        };
        let mut change_spans = Vec::new();
        // A refined hunk narrows the change to the rows its runs name, so a row
        // inside one that names none has nothing to bar.
        let marked = write_buffer_row_change_spans(
            snapshot,
            display_row,
            buffer_row,
            &mut hunk_scratch,
            &mut change_spans,
        );
        let status = snapshot.line_diff_status(buffer_row);
        // Read before `paired_with_base`, which refills the scratch.
        let refined = status == DiffStatus::Modified
            && hunk_scratch
                .iter()
                .any(|hunk| hunk.status == DiffHunkStatus::Modified && hunk.token_detail.is_some());
        DiffRowState {
            kind: DiffRowKind::BufferRow { buffer_row },
            status,
            continuation: snapshot.is_wrap_continuation(display_row),
            refined,
            removal_tick: false,
            paired: status == DiffStatus::Modified
                && paired_with_base(snapshot, buffer_row, &mut hunk_scratch),
            staged: marked
                .then(|| {
                    snapshot
                        .diff_map()
                        .and_then(|dm| dm.staged_for_line(buffer_row))
                })
                .flatten(),
            change_spans,
        }
    })
    .collect()
}

/// Whether `display_row`, a row of `block`, is where the right column ticks a
/// pure removal.
///
/// Each removed base line is a block of its own. The blocks of one removal sit
/// together below one live row, so only the first row of the run ticks. The
/// tick marks a gap that the right column leaves blank. It stays off when the
/// live row above or below the gap carries a bar, because that bar already
/// marks the spot.
fn takes_removal_tick(snapshot: &DisplaySnapshot, display_row: u32, block: &Block) -> bool {
    if block.0.diff_status != Some(DiffHunkStatus::Deleted) {
        return false;
    }
    let opens_run = display_row == 0
        || matches!(
            snapshot.classify_row(display_row - 1),
            BlockRowKind::BufferRow { .. }
        );
    let above = block.0.placement.start();
    opens_run
        && snapshot.line_diff_status(above) == DiffStatus::Unchanged
        && snapshot.line_diff_status(above + 1) == DiffStatus::Unchanged
}

/// Whether the modified row `buffer_row` has a base row beside it.
///
/// A modified hunk's base rows pair with its live rows one for one from the
/// hunk's start, so a row is paired while its offset into the hunk is inside the
/// base text the hunk removed. Past that the live side outruns the base and the
/// left column is blank; the base rows past the live side block after the hunk
/// instead.
fn paired_with_base<'a>(
    snapshot: &'a DisplaySnapshot,
    buffer_row: u32,
    hunk_scratch: &mut Vec<&'a DiffHunk>,
) -> bool {
    if !snapshot.pairs_modified_hunks() {
        return false;
    }
    let Some(dm) = snapshot.diff_map() else {
        return false;
    };
    dm.hunks_in_range_into(buffer_row..buffer_row + 1, hunk_scratch);
    hunk_scratch.iter().any(|hunk| {
        hunk.status == DiffHunkStatus::Modified
            && buffer_row.saturating_sub(hunk.buffer_start_line) < dm.base_line_count(hunk)
    })
}

/// The digit count a diff-view gutter sizes its number field for.
///
/// The plain gutter's rule over the larger of the buffer's line count and the
/// base text's, since the left column numbers base lines, the right column
/// numbers buffer lines, and one field width serves both.
pub(crate) fn diff_gutter_digits(snapshot: &DisplaySnapshot) -> u16 {
    editor::gutter_digits(
        snapshot
            .buffer_line_count()
            .max(snapshot.diff_map().map_or(0, DiffMap::base_lines)),
    )
}

/// The digit count `editor`'s diff gutters size for: the count its changes
/// view stack shares, or [`diff_gutter_digits`] over its own snapshot.
pub(crate) fn diff_gutter_digits_for(editor: &mut EditorState) -> u16 {
    match editor.stack_gutter_digits {
        Some(digits) => digits,
        None => diff_gutter_digits(&editor.display_map.snapshot()),
    }
}

/// One diff body's gutter shape and the narrowest rect it still splits in two.
///
/// The two bodies that draw a diff want different gutters. The diff view reads
/// as an editor with a base column bolted on, so it leads with the line number
/// and rules the gutter off from the code. The review screen and the commit
/// preview lead with the chunk-status glyph, because a reviewer scans statuses
/// down the left edge, and they run the text straight after the number to buy
/// back a column.
///
/// A layout exists so both bodies resolve their columns through
/// [`DiffColumns`]. Click mapping reads the text rect the paint records from
/// the same geometry, so a second formula misplaces clicks with nothing
/// failing.
///
/// The cell offsets place the parts of a cell gutter. A rich gutter places its
/// parts at its sixteenth offsets instead, and no rich paint reads its cell
/// offsets.
#[derive(Clone, Copy)]
pub(crate) struct DiffLayout {
    /// Cells one side's gutter spans, its rule included.
    gutter_w: u16,
    /// The number field's first cell, from the side's start.
    num_off: u16,
    /// The status column's first cell, from the side's start.
    status_off: u16,
    /// The rule cell between the gutter and the text, from the side's start.
    /// Absent for a layout that runs the text straight after the gutter.
    sep_off: Option<u16>,
    /// Where a rich gutter places its parts, absent for a cell gutter.
    rich: Option<RichGutterOffsets>,
    /// Inner width below which both sides alias onto one unified column. Zero
    /// for a layout that always splits.
    two_column_min: u16,
}

/// Where a rich diff gutter places its parts, in sixteenths from the side's
/// first cell.
///
/// The plain editor's sub-cell gutter answers each offset for the same digit
/// count, so a diff side and a plain pane put the number, the bars and the
/// hairline at the same place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RichGutterOffsets {
    /// Where a right-aligned line number ends.
    number_right: u16,
    /// The change-kind bar's left edge.
    git_x: u16,
    /// The staged-state bar's left edge.
    staged_x: u16,
    /// The separator hairline's left edge.
    separator_x: u16,
}

impl DiffLayout {
    /// The diff view's gutter for `digits`-wide numbers: line number,
    /// change and staged status, then a rule, as wide as the plain editor's
    /// gutter.
    ///
    /// A cell gutter leaves its first cell blank, where the plain gutter
    /// draws its mark column, right-aligns the number in the next `digits`
    /// cells, puts the two status glyphs after it, and draws the rule in the
    /// plain gutter's gap cell. A `rich` gutter spans the sub-cell gutter's
    /// [`Gutter::cell_width`](stoat_widgets::gutter::Gutter::cell_width) and
    /// places its parts at that gutter's sixteenth offsets.
    pub(crate) fn diff_view(digits: u16, rich: bool) -> Self {
        let rich = rich.then(|| {
            let gutter = editor::rich_gutter(&[], digits, [0; 3], [0; 3], [0; 3]);
            let offsets = RichGutterOffsets {
                number_right: gutter.number_right_edge(),
                git_x: gutter.git_x(),
                staged_x: gutter.staged_x(),
                separator_x: gutter.separator_x(),
            };
            (gutter.cell_width(), offsets)
        });
        Self {
            gutter_w: rich.map_or(digits + 4, |(width, _)| width),
            num_off: 1,
            status_off: digits + 1,
            sep_off: Some(digits + 3),
            rich: rich.map(|(_, offsets)| offsets),
            two_column_min: DIFF_TWO_COLUMN_MIN,
        }
    }

    /// The review screen's and commit preview's gutter: a one-cell chunk-status
    /// glyph, then the line number, then the text.
    pub(crate) const REVIEW: Self = Self {
        gutter_w: 6,
        num_off: 1,
        status_off: 0,
        sep_off: None,
        rich: None,
        two_column_min: 0,
    };

    /// Offsets of the number, status, and rule columns from a side's start.
    fn offsets(&self) -> (u16, u16, Option<u16>) {
        (self.num_off, self.status_off, self.sep_off)
    }
}

/// Column geometry for one diff body, resolved once from the inner rect.
///
/// A wide rect splits into a base column on the left and a buffer column on the
/// right, each laid out per its [`DiffLayout`]. A rect narrower than the
/// layout's threshold aliases both sides onto one full-width column with no
/// mid-divider, so Block and buffer rows land in the same place and read as a
/// unified diff.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DiffColumns {
    /// The left side's first cell, where its gutter starts.
    pub(crate) left_x: u16,
    pub(crate) left_num_x: u16,
    pub(crate) status_left_x: u16,
    /// Absolute x of the rule between the left gutter and its text, absent for
    /// a layout with no rule.
    pub(crate) left_sep_x: Option<u16>,
    pub(crate) left_text_x: u16,
    pub(crate) left_content_w: usize,
    /// The right side's first cell, the left side's in the unified layout.
    pub(crate) right_x: u16,
    pub(crate) right_num_x: u16,
    pub(crate) status_right_x: u16,
    pub(crate) right_sep_x: Option<u16>,
    pub(crate) right_text_x: u16,
    pub(crate) right_content_w: usize,
    /// Absolute x of the mid-divider, absent in the unified layout.
    pub(crate) sep_x: Option<u16>,
    /// Where a rich gutter places its parts on either side, absent for a cell
    /// gutter.
    pub(crate) rich: Option<RichGutterOffsets>,
}

impl DiffColumns {
    pub(crate) fn compute(inner: Rect, layout: DiffLayout) -> Self {
        let gutter_w = layout.gutter_w as usize;
        let (num_off, status_off, sep_off) = layout.offsets();

        if inner.width < layout.two_column_min {
            let num_x = inner.x + num_off;
            let status_x = inner.x + status_off;
            let sep_x = sep_off.map(|off| inner.x + off);
            let text_x = inner.x + gutter_w as u16;
            let content_w = (inner.width as usize).saturating_sub(gutter_w);
            return Self {
                left_x: inner.x,
                left_num_x: num_x,
                status_left_x: status_x,
                left_sep_x: sep_x,
                left_text_x: text_x,
                left_content_w: content_w,
                right_x: inner.x,
                right_num_x: num_x,
                status_right_x: status_x,
                right_sep_x: sep_x,
                right_text_x: text_x,
                right_content_w: content_w,
                sep_x: None,
                rich: layout.rich,
            };
        }

        let full_w = inner.width as usize;
        let sep: usize = 1;
        let divider = divider_x(inner);
        let half_w = (divider - inner.x) as usize;
        let left_content_w = half_w.saturating_sub(gutter_w);
        let right_start = divider + sep as u16;
        let right_content_w = (full_w - half_w - sep).saturating_sub(gutter_w);
        Self {
            left_x: inner.x,
            left_num_x: inner.x + num_off,
            status_left_x: inner.x + status_off,
            left_sep_x: sep_off.map(|off| inner.x + off),
            left_text_x: inner.x + gutter_w as u16,
            left_content_w,
            right_x: right_start,
            right_num_x: right_start + num_off,
            status_right_x: right_start + status_off,
            right_sep_x: sep_off.map(|off| right_start + off),
            right_text_x: right_start + gutter_w as u16,
            right_content_w,
            sep_x: Some(divider),
            rich: layout.rich,
        }
    }
}

/// The diff view's mid-divider column in `inner`, absent below
/// [`DIFF_TWO_COLUMN_MIN`], where both sides share one column.
pub(crate) fn diff_divider_x(inner: Rect) -> Option<u16> {
    (inner.width >= DIFF_TWO_COLUMN_MIN).then(|| divider_x(inner))
}

/// The column that splits `inner` into two sides, the left one the narrower
/// when the width is even.
fn divider_x(inner: Rect) -> u16 {
    inner.x + inner.width.saturating_sub(1) / 2
}

/// Paint the diff body for the rows visible from `scroll_row`.
///
/// A wide inner rect lays out base text left and buffer text right. A narrow one
/// collapses to a single unified column. See [`DiffColumns`] for the geometry.
///
/// Text with no token color falls back to a color that says what changed. On
/// the base side that is the removed color. On the live side it is the added
/// color for an added or a modified row, and the moved color for a moved row.
/// A modified row's live side takes the added color and not the modified one,
/// so the pair reads as a removal and an addition.
///
/// A refined modified line names its changed chars, so on both sides it falls
/// back to the status color only inside its change spans. The chars outside
/// them are unchanged text, so they keep `fallback_style` and take no row
/// tint. An unchanged row keeps `fallback_style` whole.
///
/// Shared by the live [`render_diff_view`] and the off-loop smooth-scroll page
/// so both paint an identical grid. It takes owned parts and paints no cursor,
/// letting a pooled page render it on a blocking worker.
///
/// `current_line` is the 1-based buffer line that relative numbering counts
/// the buffer column from, as [`render_diff_view`] describes. `None` paints
/// absolute numbers.
///
/// `digits` sizes both gutters, as [`diff_gutter_digits`] answers it for the
/// snapshot, so the columns match the ones a caller fit the wrap width to.
///
/// `endpoint_cache` and `row_cache` are the editor's, when this paint has one
/// behind it. A pooled page has no editor at all and passes `None` for both,
/// resolving its endpoints and row state fresh.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_diff_rows(
    snapshot: &DisplaySnapshot,
    scroll_row: u32,
    inner: Rect,
    digits: u16,
    fallback_style: Style,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: Option<&mut ApcScene>,
    dim: f32,
    dials: DiffDials,
    current_line: Option<u32>,
    endpoint_cache: Option<&mut Option<CachedHighlightEndpoints>>,
    row_cache: Option<&mut Option<DiffRowCache>>,
) {
    let total_rows = snapshot.line_count();
    let visible = inner.height as u32;
    let end_row = (scroll_row + visible).min(total_rows);
    if end_row <= scroll_row {
        return;
    }

    // One buffer for every number this loop paints, rather than one per row.
    let mut num_text = String::new();

    // Rich mode replaces the ASCII gutter (status glyphs, line numbers, and the
    // separators) with sub-cell APC components. It engages only when a scene is
    // threaded and every gutter color resolves to RGB, so the two paths never
    // mix within one frame. `dim` fades the bars toward the background for a
    // pooled unfocused page, matching the cell dimming.
    let layout = DiffLayout::diff_view(
        digits,
        diff_gutter_is_rich(scene.is_some(), theme, fallback_style),
    );
    let mut rich = layout.rich.zip(scene).and_then(|(offsets, scene)| {
        resolve_diff_rich_colors(theme, fallback_style, dim).map(|colors| DiffRichGutter {
            scene,
            colors,
            offsets,
        })
    });

    let DiffColumns {
        left_x,
        left_num_x,
        status_left_x,
        left_sep_x,
        left_text_x,
        left_content_w,
        right_x,
        right_num_x,
        status_right_x,
        right_sep_x,
        right_text_x,
        right_content_w,
        sep_x,
        rich: _,
    } = DiffColumns::compute(inner, layout);

    let dim_style = theme.get(s::DIFF_CONTEXT);
    let del_style = theme.get(s::DIFF_DELETED);
    let add_style = fallback_style.patch(theme.get(s::DIFF_ADDED));
    let moved_style = fallback_style.patch(theme.get(s::DIFF_MOVED));
    let inlay_style = fallback_style.patch(theme.get(s::UI_VIRTUAL_INLAY));

    let tints = resolve_diff_tints(theme);
    let base_changes = snapshot
        .diff_map()
        .map(|dm| dm.base_change_spans())
        .unwrap_or_default();

    let mut base_line = base_line_at(snapshot, scroll_row);
    let mut next_buffer_row = lines_above(snapshot, scroll_row);
    // A moved-away seam's base lines take no row, so the walk adds them when it
    // reaches the buffer row the seam sits before. `base_line_at` already
    // counted the seams above the first visible buffer row.
    let seams: Vec<(u32, u32)> = snapshot
        .diff_map()
        .map(|dm| dm.moved_away_seams().collect())
        .unwrap_or_default();
    let mut seam_cursor = 0;
    let row_endpoints = match endpoint_cache {
        Some(cache) => snapshot.highlighted_endpoints_cached(scroll_row..end_row, cache),
        None => snapshot.highlighted_endpoints(scroll_row..end_row),
    };
    // One replay for the loop. The rows below ascend, and each opens its own
    // stream because the paint puts a gutter, a number and a status glyph
    // between them, so without this each row re-walks the endpoints above it.
    let mut row_cursor = snapshot.row_highlight_cursor(row_endpoints);
    let mut local_row_cache = None;
    let row_states = diff_row_states(
        snapshot,
        scroll_row..end_row,
        row_cache.unwrap_or(&mut local_row_cache),
    );
    let mut line_buf = String::new();
    // Reused across rows. No row keeps its hunks past itself, so the buffer
    // only ever grows to the widest row rather than being rebuilt for each one.
    let mut hunk_scratch: Vec<&DiffHunk> = Vec::new();
    // The rows of a wrapped base line paint one after another, so one entry
    // breaks each line once per paint rather than once per row.
    let mut base_layout: Option<(u32, BaseLineLayout)> = None;

    for display_row in scroll_row..end_row {
        let y = inner.y + (display_row - scroll_row) as u16;
        if y >= inner.y + inner.height {
            break;
        }

        if rich.is_none() {
            if let Some(sep_x) = sep_x {
                buf[(sep_x, y)].set_char('│').set_style(dim_style);
            }
            for gutter_sep in [left_sep_x, right_sep_x].into_iter().flatten() {
                if gutter_sep < inner.x + inner.width {
                    buf[(gutter_sep, y)].set_char('│').set_style(dim_style);
                }
            }
        }

        let row_state = &row_states[(display_row - scroll_row) as usize];
        match row_state.kind {
            DiffRowKind::Block { segment, after_row } => {
                // A fold that hid the block's row puts the block after the
                // placeholder, so the lines it hid up to that row count first.
                if let Some(row) = after_row
                    && row >= next_buffer_row
                {
                    base_line += hidden_base_lines(snapshot, next_buffer_row..row + 1)
                        + seam_lines_through(&seams, &mut seam_cursor, next_buffer_row, row);
                    next_buffer_row = row + 1;
                }
                // A block holds one base line, a row per wrapped segment of it.
                // The line's first row already stepped past it, so a
                // continuation reads the line before the counter.
                let continuation = row_state.continuation;
                let line_base = base_line.saturating_sub(u32::from(continuation));
                let text = snapshot
                    .diff_map()
                    .and_then(|dm| dm.base_line_text(line_base))
                    .unwrap_or("");
                paint_base_side(
                    snapshot,
                    &mut rich,
                    buf,
                    &mut num_text,
                    inner,
                    (
                        left_x,
                        left_num_x,
                        status_left_x,
                        left_text_x,
                        left_content_w,
                    ),
                    digits,
                    y,
                    line_base,
                    BaseRow::segment(snapshot, text, line_base, segment, &mut base_layout),
                    &base_changes,
                    tints.as_ref(),
                    (del_style, dim_style, fallback_style, moved_style),
                    theme,
                    dials,
                );
                // The unified layout paints this row's bars in its one status
                // column already, so only the two-column layout has a gap.
                if row_state.removal_tick && sep_x.is_some() {
                    draw_removal_tick(&mut rich, buf, inner, (right_x, status_right_x), y, theme);
                }
                if !continuation {
                    base_line += 1;
                }
            },
            DiffRowKind::BufferRow { buffer_row } => {
                // A continuation belongs to the line its first row numbered and
                // counted, so it paints text alone. `base_line_at` counts a
                // continuation top row's seams, so the walk skips them here.
                let continuation = row_state.continuation;
                let last_row = display_row + 1 >= total_rows
                    || !snapshot.is_wrap_continuation(display_row + 1);
                if !continuation {
                    // A fold hides unchanged lines or the unmarked rows of a
                    // refined hunk, so a row below a fold counts the base lines
                    // of the rows it hid.
                    base_line += hidden_base_lines(snapshot, next_buffer_row..buffer_row)
                        + seam_lines_through(&seams, &mut seam_cursor, next_buffer_row, buffer_row);
                    next_buffer_row = buffer_row + 1;
                    draw_diff_num(
                        &mut rich,
                        buf,
                        &mut num_text,
                        inner,
                        (right_x, right_num_x),
                        y,
                        editor::gutter_display_number(buffer_row + 1, current_line),
                        dim_style,
                        digits,
                    );
                }
                let changes = &row_state.change_spans;
                let staged = row_state.staged;
                let status = row_state.status;
                let soften_row = match status {
                    DiffStatus::Unchanged => tints.as_ref().map(|t| t.bg),
                    _ => None,
                };
                let soften_gaps = match status {
                    DiffStatus::Modified | DiffStatus::Moved => tints.as_ref().map(|t| t.bg),
                    _ => None,
                };
                // A changed row takes its own status color entire, and the
                // refined spans inside it lead by their span kind. A refined
                // row leaves its status to its spans, because the chars outside
                // them are unchanged text, as on the base side.
                let tint_row = match status {
                    DiffStatus::Added => tints.as_ref().map(|t| t.added),
                    DiffStatus::Modified if row_state.refined => None,
                    DiffStatus::Modified => tints.as_ref().map(|t| t.modified),
                    DiffStatus::Moved => tints.as_ref().map(|t| t.moved),
                    DiffStatus::Unchanged => None,
                };
                let row_fallback = match status {
                    DiffStatus::Added | DiffStatus::Modified => add_style,
                    DiffStatus::Moved => moved_style,
                    DiffStatus::Unchanged => fallback_style,
                };
                let gap_fallback = match row_state.refined {
                    true => fallback_style,
                    false => row_fallback,
                };
                paint_highlighted_row(
                    snapshot,
                    display_row,
                    right_text_x,
                    y,
                    right_content_w,
                    buf,
                    row_fallback,
                    gap_fallback,
                    moved_style,
                    inlay_style,
                    changes,
                    tints.as_ref(),
                    soften_row,
                    soften_gaps,
                    dials,
                    tint_row,
                    &mut row_cursor,
                );
                // The chip follows the line's text, which a wrapped line ends
                // on its last row.
                if status == DiffStatus::Moved
                    && last_row
                    && let Some((path, line)) =
                        move_chip_source(snapshot, buffer_row, &mut hunk_scratch)
                {
                    line_buf.clear();
                    snapshot.write_display_line(&mut line_buf, display_row);
                    render_move_chip(
                        buf,
                        right_text_x,
                        y,
                        line_buf.chars().count(),
                        right_content_w,
                        path.as_deref(),
                        line,
                        theme.get(s::DIFF_MOVED).add_modifier(Modifier::ITALIC),
                    );
                }
                if !continuation && let Some(state) = staged {
                    let change_scope = match status {
                        DiffStatus::Added => s::DIFF_ADDED,
                        DiffStatus::Modified => s::DIFF_MODIFIED,
                        DiffStatus::Moved => s::DIFF_MOVED,
                        DiffStatus::Unchanged => s::DIFF_CONTEXT,
                    };
                    draw_diff_status(
                        &mut rich,
                        buf,
                        inner,
                        (right_x, status_right_x),
                        y,
                        change_scope,
                        state,
                        false,
                        theme,
                    );
                }
                // An unchanged row mirrors its own line, which is the same text
                // on both sides. A paired modified row shows the base line it
                // replaced, painted as a removed row rather than a mirror. A
                // row past its hunk's base rows has nothing on the left.
                if status == DiffStatus::Unchanged {
                    // The line's first row already stepped past its base line,
                    // so a continuation mirrors the one before.
                    let line_base = base_line.saturating_sub(u32::from(continuation));
                    // The base line's own bytes, which its token spans index.
                    // The display line carries the live row's inlay hints and
                    // expanded tabs, which belong to the right column alone.
                    let (text, window, indent) = match snapshot
                        .diff_map()
                        .and_then(|dm| dm.base_line_text(line_base))
                    {
                        Some(text) => (
                            text,
                            mirror_window(snapshot, display_row, continuation, last_row, text),
                            snapshot.soft_wrap_indent(display_row) as usize,
                        ),
                        None => {
                            line_buf.clear();
                            snapshot.write_display_line(&mut line_buf, display_row);
                            (line_buf.as_str(), 0..line_buf.len(), 0)
                        },
                    };
                    if !continuation {
                        draw_diff_num(
                            &mut rich,
                            buf,
                            &mut num_text,
                            inner,
                            (left_x, left_num_x),
                            y,
                            line_base + 1,
                            dim_style,
                            digits,
                        );
                        if let Some((status, state)) = snapshot
                            .diff_map()
                            .and_then(|dm| dm.staged_mark_at_base_line(line_base))
                        {
                            draw_diff_status(
                                &mut rich,
                                buf,
                                inner,
                                (left_x, status_left_x),
                                y,
                                staged_mark_scope(status),
                                state,
                                status == DiffHunkStatus::Deleted,
                                theme,
                            );
                        }
                    }
                    // An unchanged line reads the same on both sides, so the
                    // live line's column at the window's start is the one the
                    // base line's tabs in the window expand from.
                    let column = match text[window.clone()].contains('\t') {
                        true => snapshot.visual_column(Point::new(buffer_row, window.start as u32)),
                        false => 0,
                    };
                    let token_spans = base_token_spans(snapshot, line_base);
                    paint_base_segment(
                        buf,
                        left_text_x + indent as u16,
                        y,
                        text,
                        window,
                        column,
                        left_content_w.saturating_sub(indent),
                        snapshot.tab_snapshot().tab_size(),
                        snapshot.tab_snapshot().max_expansion_column(),
                        token_spans,
                        dim_style,
                        dim_style,
                        dim_style,
                        &[],
                        tints.as_ref(),
                        soften_row,
                        None,
                        dials,
                        None,
                    );
                    if !continuation {
                        base_line += 1;
                    }
                } else if row_state.paired {
                    // The base line breaks beside its live line, a segment per
                    // live row. A row past its last segment shows nothing, and
                    // segments past the live rows sit in an overflow block below.
                    let line_base = base_line.saturating_sub(u32::from(continuation));
                    let text = snapshot
                        .diff_map()
                        .and_then(|dm| dm.base_line_text(line_base))
                        .unwrap_or("");
                    let line_row = match continuation {
                        true => {
                            display_row - snapshot.buffer_to_display(Point::new(buffer_row, 0)).row
                        },
                        false => 0,
                    };
                    paint_base_side(
                        snapshot,
                        &mut rich,
                        buf,
                        &mut num_text,
                        inner,
                        (
                            left_x,
                            left_num_x,
                            status_left_x,
                            left_text_x,
                            left_content_w,
                        ),
                        digits,
                        y,
                        line_base,
                        BaseRow::segment(snapshot, text, line_base, line_row, &mut base_layout),
                        &base_changes,
                        tints.as_ref(),
                        (del_style, dim_style, fallback_style, moved_style),
                        theme,
                        dials,
                    );
                    if !continuation {
                        base_line += 1;
                    }
                }
            },
        }
    }

    // One hairline per gutter spanning the visible rows, replacing the per-row
    // glyph. A gutter's hairline sits at the rich gutter's separator offset,
    // and the mid divider centers in its cell at +8 sixteenths.
    if let Some(rg) = rich.as_mut() {
        let height = (end_row - scroll_row) as u16 * 16;
        let gutter_rules =
            [left_x, right_x].map(|side| (side - inner.x) * 16 + rg.offsets.separator_x);
        let divider = sep_x.map(|sep| (sep - inner.x) * 16 + 8);
        for x in gutter_rules.into_iter().chain(divider) {
            if x / 16 < inner.width {
                Bar {
                    x: x as i16,
                    y: 0,
                    width: 1,
                    height,
                    color: rg.colors.dim,
                }
                .render(inner, buf, &mut *rg.scene);
            }
        }
    }
}

/// The colors one diff paint blends toward: the editor background its softening
/// recedes into, and the status color each changed char tints toward.
///
/// Resolved once per paint, and present only on an RGB theme. A theme whose
/// background is not an RGB color has no channels to blend, so it softens
/// nothing and marks its change spans with [`Modifier::UNDERLINED`] instead,
/// which is what an absent value means to every caller.
pub(crate) struct DiffTints {
    pub(crate) bg: [u8; 3],
    pub(crate) added: Color,
    pub(crate) deleted: Color,
    pub(crate) modified: Color,
    pub(crate) moved: Color,
}

/// Resolve the colors one paint blends toward, or `None` when the theme's
/// background is not an RGB color.
///
/// A `None` turns off softening for the whole frame and puts the diff view on
/// the underline fallback for change spans, which is how an indexed-color theme
/// still marks what changed.
///
/// The status colors come from [`DiffTheme`], so a tinted char, the gutter bars
/// and the status glyphs all read one palette. A status color the theme leaves
/// unset takes that palette's default, which is a named color no tint can blend
/// toward.
pub(crate) fn resolve_diff_tints(theme: &crate::theme::Theme) -> Option<DiffTints> {
    let bg = style_rgb(theme.try_get(s::UI_BACKGROUND).and_then(|st| st.bg))?;
    let status = DiffTheme::from_theme(theme);
    Some(DiffTints {
        bg,
        added: status.added,
        deleted: status.deleted,
        modified: status.modified,
        moved: status.moved,
    })
}

/// Derive the palette of a diff view's minimap strip from the colors the diff
/// paint gives its rows.
///
/// Each block holds the colors a diff row of its status paints each class in.
/// The strip keeps one summary per buffer, whatever view shows it, so the dials
/// reach the strip through this palette alone. A dial step declares a palette
/// and summarizes no row.
///
/// `syntax` is whether the diff paint shows syntax colors. Without them, every
/// class of a block takes the color of the block's uncovered text.
///
/// An entry blends as [`paint_highlighted_row`] blends a row with no refined
/// spans, through its `recede` and [`gap_style`]. A refined row's lighter gap
/// soften and its span colors are below the strip's resolution. So the modified
/// block takes the paint of an unrefined modified row, whose uncovered text
/// takes the added color.
///
/// The deleted block paints a base line as the base column paints a removed
/// line. Its uncovered text takes the deleted color, and its tokens tint toward
/// that color.
///
/// The edge classes stay as `table` gives them. A theme color that is not RGB
/// enters as its [`minimap::color_to_rgb`] approximation and blends like any
/// other, where the painter does not blend it.
pub(crate) fn diff_minimap_palette(
    table: &ClassTable,
    theme: &crate::theme::Theme,
    dials: DiffDials,
    syntax: bool,
) -> Vec<[u8; 3]> {
    let base = table.palette();
    let stride = usize::from(table.stride());
    let tints = resolve_diff_tints(theme);
    let soften_bg = tints
        .as_ref()
        .map(|t| t.bg)
        .filter(|_| dials.soften_scale > 0.0);
    let mut palette = base.to_vec();

    let context_soften = (CONTEXT_SOFTEN * dials.soften_scale).min(SOFTEN_CAP);
    for (k, entry) in palette[..stride].iter_mut().enumerate() {
        let mut color = match syntax {
            true => base[k],
            false => base[0],
        };
        if let Some(bg) = soften_bg {
            color = dim_rgb(color, bg, context_soften);
        }
        // The gray answers the tint dial rather than the soften one, so it
        // holds with softening off.
        if tints.is_some() {
            color = desaturate_rgb(color, dials.tint_amount);
        }
        *entry = color;
    }

    let row_fg = |scope: &str| theme.get(scope).fg.map_or(base[0], minimap::color_to_rgb);
    let added_fg = row_fg(s::DIFF_ADDED);
    let blocks = [
        (
            table.population_base(DiffStatus::Added),
            added_fg,
            tints.as_ref().map(|t| t.added),
        ),
        (
            table.population_base(DiffStatus::Modified),
            added_fg,
            tints.as_ref().map(|t| t.modified),
        ),
        (
            table.population_base(DiffStatus::Moved),
            row_fg(s::DIFF_MOVED),
            tints.as_ref().map(|t| t.moved),
        ),
        (
            table.deleted_base(),
            row_fg(s::DIFF_DELETED),
            tints.as_ref().map(|t| t.deleted),
        ),
    ];
    for (start, row_fallback, tint_row) in blocks {
        for (k, entry) in palette[usize::from(start)..][..stride]
            .iter_mut()
            .enumerate()
        {
            let mut color = match syntax && k > 0 {
                true => base[k],
                false => row_fallback,
            };
            if let Some(target) = tint_row {
                color = tint_rgb(color, minimap::color_to_rgb(target), dials.tint_amount);
            }
            // A changed row lifts only off a receding surround, so softening
            // off turns the lift off too.
            if let Some(bg) = soften_bg {
                color = brighten_rgb(color, bg);
            }
            *entry = color;
        }
    }
    palette
}

/// Mark a cell `style` as inside a change span.
///
/// An RGB theme leaves a code span as it is. The chars outside the spans recede
/// instead, so a changed char leads its line by being the one thing at full
/// strength, and the diff paints no background at all.
///
/// A `prose` replacement bolds on top of that. The receding alone is too weak
/// to find a changed char inside a string or a comment, where the whole literal
/// carries one color and no token boundary falls beside the edit. Only a
/// replacement bolds, because that is the case the reader compares char by
/// char. With `bold` set, every span of any kind bolds.
///
/// A replacement of a [`ProseChange::Part`] of its literal also underlines,
/// on every theme. Inside text of one color, the bold alone does not show
/// where a changed run starts and ends.
///
/// A theme that cannot blend has no receding to lead against, so it underlines
/// the span, which is the only mark left to it. With `underline` set, every
/// theme underlines the span.
///
/// `tint` carries the span's status color and the dial amount, and is `None`
/// where the dial is off. It is what makes a changed char's own color say
/// added, deleted, modified, or moved, rather than leaving the status to the
/// gutter bars alone.
///
/// The caller lifts the result per [`brighten_style`] where softening is on, so
/// a color this leaves too near the background still reads against it.
fn mark_span(
    style: Style,
    kind: &ChangeKind,
    prose: ProseChange,
    bold: bool,
    underline: bool,
    rgb: bool,
    tint: Option<(Color, f32)>,
) -> Style {
    let replaced = matches!(kind, ChangeKind::Replaced);
    let style = match !rgb || underline || (replaced && prose == ProseChange::Part) {
        true => style.add_modifier(Modifier::UNDERLINED),
        false => style,
    };
    let style = match bold || (replaced && prose != ProseChange::None) {
        true => style.add_modifier(Modifier::BOLD),
        false => style,
    };
    match tint {
        Some((target, amount)) => tint_style(style, target, amount),
        None => style,
    }
}

/// Recede `style` by blending its foreground toward `bg` by `amount` and
/// dropping bold.
///
/// A non-RGB foreground comes back unblended, because there is no channel to
/// interpolate. Bold goes regardless. Weight pulls the eye as hard as color, so
/// a softened run that keeps its bold still competes with the changed content
/// it sits behind.
fn soften_style(style: Style, bg: [u8; 3], amount: f32) -> Style {
    let style = match style_rgb(style.fg) {
        Some(fg) => {
            let [r, g, b] = dim_rgb(fg, bg, amount);
            style.fg(Color::Rgb(r, g, b))
        },
        None => style,
    };
    style.remove_modifier(Modifier::BOLD)
}

/// Shift `style`'s foreground toward `target` by `amount`, keeping its weight.
///
/// An amount at or under zero is the dial's off position and returns the style
/// as it was, so an untouched session paints exactly the shipped colors. A full
/// amount replaces the foreground outright, which is the flat form: the theme
/// tunes its `diff.*` colors per palette, so a changed char may take one whole
/// rather than only lean toward it.
///
/// A foreground or a target that is not an RGB color has no channels to
/// interpolate, so every amount above zero snaps it to the flat form rather
/// than leaving the char untinted.
fn tint_style(style: Style, target: Color, amount: f32) -> Style {
    if amount <= 0.0 {
        return style;
    }
    match (style_rgb(style.fg), style_rgb(Some(target))) {
        (Some(fg), Some(target_rgb)) => {
            let [r, g, b] = tint_rgb(fg, target_rgb, amount);
            style.fg(Color::Rgb(r, g, b))
        },
        _ => style.fg(target),
    }
}

/// Shift `fg` toward `target` by `amount`, per [`tint_style`] on two RGB colors.
fn tint_rgb(fg: [u8; 3], target: [u8; 3], amount: f32) -> [u8; 3] {
    if amount <= 0.0 {
        return fg;
    }
    if amount >= 1.0 {
        return target;
    }
    dim_rgb(fg, target, amount)
}

/// Drain `amount` of the color out of `style`'s foreground, toward the gray of
/// its own brightness.
///
/// The other half of the tint dial's trade. Changed content gains its status
/// color as the dial rises, and unchanged content loses the color it had, so
/// the top reads as status-colored rows on a gray field rather than as one more
/// palette competing with the rest.
///
/// Blending toward the foreground's own luma rather than a fixed gray is what
/// keeps a dark comment dark and a bright keyword bright, so the row still
/// reads as code with its emphasis intact.
///
/// An amount at or under zero returns the style as it was, and a foreground
/// that is not an RGB color has no channels to drain and returns unchanged.
fn desaturate_style(style: Style, amount: f32) -> Style {
    let Some(fg) = style_rgb(style.fg) else {
        return style;
    };
    let [r, g, b] = desaturate_rgb(fg, amount);
    style.fg(Color::Rgb(r, g, b))
}

/// Drain `amount` of the color out of `fg`, per [`desaturate_style`] on an RGB
/// color.
fn desaturate_rgb(fg: [u8; 3], amount: f32) -> [u8; 3] {
    if amount <= 0.0 {
        return fg;
    }
    let gray = [(luma(fg) * 255.0).round() as u8; 3];
    dim_rgb(fg, gray, amount)
}

/// The status color a change span tints toward.
///
/// `base_side` splits a novel and a replaced span. The left column carries the
/// base text, where both are text the change removed, so both read deleted. The
/// right column carries the live text, where novel content is what the change
/// added and replaced content is what changed. Only a moved span reads the same
/// from both sides.
fn span_tint_color(tints: &DiffTints, kind: &ChangeKind, base_side: bool) -> Color {
    match kind {
        ChangeKind::Moved => tints.moved,
        ChangeKind::Replaced if !base_side => tints.modified,
        ChangeKind::Novel if !base_side => tints.added,
        ChangeKind::Replaced | ChangeKind::Novel => tints.deleted,
    }
}

/// Resolve the style of a row cell that falls outside every change span.
///
/// The row tint lands here rather than on the style the span arm marks, so a
/// changed cell blends from its syntax color straight to its span color and
/// never through a second one.
///
/// A refined row's gaps recede under `soften_gaps` and take no contrast lift.
/// The lift exists to stand a changed cell off a receding surround, which a
/// gap cell is not. A row with no spans keeps the tint-then-lift order that
/// makes a wholly added or deleted line readable against the context.
fn gap_style(
    style: Style,
    tint_row: Option<Color>,
    tint_amount: f32,
    soften_gaps: Option<[u8; 3]>,
    soften_scale: f32,
    lift_bg: Option<[u8; 3]>,
) -> Style {
    let style = match tint_row {
        Some(target) => tint_style(style, target, tint_amount),
        None => style,
    };
    match soften_gaps.filter(|_| soften_scale > 0.0) {
        Some(bg) => soften_style(
            style,
            bg,
            (MODIFIED_ROW_SOFTEN * soften_scale).min(SOFTEN_CAP),
        ),
        None => match lift_bg.filter(|_| tint_row.is_some()) {
            Some(bg) => brighten_style(style, bg),
            None => style,
        },
    }
}

/// Paint base text with per-token syntax styles for the diff view's left
/// column.
///
/// A byte inside a token span takes that token's color. Bytes outside every
/// span fall back to `fallback`, the deletion or context color, so the gaps
/// between tokens still read as part of the diff. This function uses
/// `fallback` inside and outside the change spans. The diff view gives the
/// unchanged bytes of a refined base line the text color instead, and a
/// refined line that no change span reaches the context color.
///
/// `change_spans` mark the changed chars of a modified or moved base line, as
/// line-local base byte ranges tagged by [`ChangeKind`]. On a theme that can
/// blend (`tints`) a byte inside one keeps its syntax color, and the softening
/// of everything around it is what marks it. A theme that cannot blend
/// underlines the span instead, per [`mark_span`].
///
/// `soften_row` recedes the whole row behind the changed rows around it, by
/// blending every foreground toward the given background per [`soften_style`].
/// Pass the editor background for an unchanged row and `None` for a changed one.
///
/// `soften_gaps` recedes only the chars outside every change span, at the
/// lighter [`MODIFIED_ROW_SOFTEN`], so a refined row's changed chars lead their
/// own line. An empty `change_spans` no-ops it, which keeps a row the
/// refinement never reached at full strength.
///
/// `tint_row` shifts every char outside a change span toward the row's own
/// status color per [`tint_style`], so a whole line reads deleted or moved
/// rather than only the chars the refinement matched. `dials.tint_amount` shifts
/// the chars inside a change span toward their span kind's color instead, which
/// is what keeps a replaced run legible against the row around it. An amount of
/// zero leaves both inert, so the dial's off position paints exactly the colors
/// from before it.
///
/// The same amount drains the color out of an unchanged row per
/// [`desaturate_style`], so one dial trades color between the two populations
/// and its top reads as status-colored rows on a gray field.
///
/// A changed char whose color lands too near the background is then lifted per
/// [`brighten_style`], so a muted one still stands off the context receding
/// behind it. The lift rides on the softening, so a zero `dials.soften_scale`
/// turns it off with everything else.
///
/// A tab runs to its next stop every `tab_size` columns, as the right column
/// expands it, so a tab-indented base line keeps its indentation. The spaces a
/// tab expands to take the tab's own style.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_base_row(
    buf: &mut Buffer,
    start_x: u16,
    y: u16,
    text: &str,
    max_cols: usize,
    tab_size: u32,
    token_spans: &[(std::ops::Range<usize>, HighlightStyle)],
    fallback: Style,
    change_spans: &[(std::ops::Range<usize>, ChangeKind, ProseChange)],
    tints: Option<&DiffTints>,
    soften_row: Option<[u8; 3]>,
    soften_gaps: Option<[u8; 3]>,
    dials: DiffDials,
    tint_row: Option<Color>,
) {
    // The commits preview paints both of its columns here with no tab map to
    // cap a tab, so an uncapped stop keeps the two agreeing.
    paint_base_segment(
        buf,
        start_x,
        y,
        text,
        0..text.len(),
        0,
        max_cols,
        tab_size,
        u32::MAX,
        token_spans,
        fallback,
        fallback,
        fallback,
        change_spans,
        tints,
        soften_row,
        soften_gaps,
        dials,
        tint_row,
    );
}

/// Paint the bytes `window` of base line `text` into a column row, styled as
/// [`paint_base_row`] styles a whole line.
///
/// A soft-wrapped line shows one segment per row. The window's tabs expand from
/// `start_column`, the tab column the window starts at, so a segment keeps the
/// stops the whole line gives its tabs, as the right column's segments do. A
/// tab at or past `max_expansion_column` takes one column, as the tab map caps
/// it. `token_spans` and `change_spans` index the whole line.
///
/// `gap_fallback` takes the place of `fallback` for a byte outside every token
/// span and every change span. A refined line passes the text style there, so
/// its unchanged bytes read as unchanged text and its changed bytes keep the
/// removed color. A refined line with no change span passes the context style,
/// as the mirror of an unchanged line does. `moved_fallback` takes the place of
/// `fallback` inside a moved span. A removed line that the tree pass relocated
/// in part then shows the move without token colors.
#[allow(clippy::too_many_arguments)]
fn paint_base_segment(
    buf: &mut Buffer,
    start_x: u16,
    y: u16,
    text: &str,
    window: std::ops::Range<usize>,
    start_column: u32,
    max_cols: usize,
    tab_size: u32,
    max_expansion_column: u32,
    token_spans: &[(std::ops::Range<usize>, HighlightStyle)],
    fallback: Style,
    gap_fallback: Style,
    moved_fallback: Style,
    change_spans: &[(std::ops::Range<usize>, ChangeKind, ProseChange)],
    tints: Option<&DiffTints>,
    soften_row: Option<[u8; 3]>,
    soften_gaps: Option<[u8; 3]>,
    dials: DiffDials,
    tint_row: Option<Color>,
) {
    debug_assert!(
        token_spans.is_sorted_by_key(|(range, _)| range.start),
        "token_spans must be start-sorted for the monotonic cursor"
    );
    debug_assert!(
        change_spans.is_sorted_by_key(|(range, ..)| range.start),
        "change_spans must be start-sorted for the monotonic cursor"
    );

    let soften_gaps = soften_gaps.filter(|_| !change_spans.is_empty());
    let span_tints = tints.filter(|_| dials.tint_amount > 0.0);
    // Only a changed char lifts, and only against a receding surround: with
    // softening off nothing recedes, so nothing has to stand off it.
    let lift_bg = tints.map(|t| t.bg).filter(|_| dials.soften_scale > 0.0);
    // Each cursor starts past the spans that end before the window, where a
    // walk from the line's first byte stands at the window's first. A line's
    // spans do not overlap, so their ends ascend with their starts and the
    // search splits them exactly.
    let mut token_cursor = token_spans.partition_point(|(range, _)| range.end < window.start);
    let mut span_cursor = change_spans.partition_point(|(range, ..)| range.end < window.start);
    debug_assert!(
        token_spans[..token_cursor]
            .iter()
            .all(|(range, _)| range.end < window.start)
            && change_spans[..span_cursor]
                .iter()
                .all(|(range, ..)| range.end < window.start),
        "the cursors skip only spans that end before the window"
    );
    let mut style_at = |byte_idx: usize| {
        while change_spans
            .get(span_cursor)
            .is_some_and(|(r, ..)| r.end <= byte_idx)
        {
            span_cursor += 1;
        }
        let span_kind = change_spans
            .get(span_cursor)
            .filter(|(range, ..)| range.start <= byte_idx)
            .map(|(_, kind, _)| kind);

        while token_spans
            .get(token_cursor)
            .is_some_and(|(r, _)| r.end <= byte_idx)
        {
            token_cursor += 1;
        }
        let mut style = match token_spans.get(token_cursor) {
            Some((range, hs)) if range.start <= byte_idx => hs.to_ratatui_style(),
            _ => match span_kind {
                Some(ChangeKind::Moved) => moved_fallback,
                Some(_) => fallback,
                None => gap_fallback,
            },
        };
        if let Some(bg) = soften_row.filter(|_| dials.soften_scale > 0.0) {
            style = soften_style(
                style,
                bg,
                (CONTEXT_SOFTEN * dials.soften_scale).min(SOFTEN_CAP),
            );
        }
        // Unfiltered, because the gray answers the tint dial rather than the
        // soften one, so it holds with softening turned off.
        if soften_row.is_some() {
            style = desaturate_style(style, dials.tint_amount);
        }

        match change_spans.get(span_cursor) {
            Some((range, kind, prose)) if range.start <= byte_idx => {
                let tint = span_tints.map(|t| (span_tint_color(t, kind, true), dials.tint_amount));
                style = mark_span(
                    style,
                    kind,
                    *prose,
                    dials.bold,
                    dials.underline,
                    tints.is_some(),
                    tint,
                );
                if let Some(bg) = lift_bg {
                    style = brighten_style(style, bg);
                }
            },
            _ => {
                style = gap_style(
                    style,
                    tint_row,
                    dials.tint_amount,
                    soften_gaps,
                    dials.soften_scale,
                    lift_bg,
                );
            },
        }

        style
    };

    let start = window.start;
    let segment = &text[window];
    if !segment.contains('\t') {
        paint_style_runs(buf, start_x, y, segment, max_cols, |byte_idx| {
            style_at(start + byte_idx)
        });
        return;
    }
    let (expanded, source) = expand_tabs(segment, start_column, tab_size, max_expansion_column);
    paint_style_runs(buf, start_x, y, &expanded, max_cols, |byte_idx| {
        style_at(start + source[byte_idx])
    });
}

/// `text` with each tab expanded to its next stop, and the byte of `text` each
/// expanded byte came from.
///
/// `column` is the tab column `text` starts at, so a window of a wrapped line
/// expands its tabs to the stops the whole line gives them. The stop rule is the
/// display map's own, per [`tab_map::advance_column_for_char`], so a tab at or
/// past `max_expansion_column` takes one column, as it does in the right column.
///
/// Every space a tab becomes maps to the tab's byte, so the mapped offsets never
/// decrease and a monotonic span cursor still walks them.
fn expand_tabs(
    text: &str,
    mut column: u32,
    tab_size: u32,
    max_expansion_column: u32,
) -> (String, Vec<usize>) {
    let mut expanded = String::with_capacity(text.len());
    let mut source = Vec::with_capacity(text.len());
    for (byte_idx, ch) in text.char_indices() {
        let tab_start = column;
        tab_map::advance_column_for_char(&mut column, ch, tab_size, max_expansion_column);
        match ch {
            '\t' => {
                for _ in tab_start..column {
                    expanded.push(' ');
                }
            },
            _ => expanded.push(ch),
        }
        source.resize(expanded.len(), byte_idx);
    }
    (expanded, source)
}

/// The base line's syntax spans, empty where the editor paints no syntax color.
///
/// The left column's tokens come from the diff map rather than from the display
/// map, so the snapshot cannot withhold them the way it withholds the right
/// column's. Reading its flag here is what keeps the two columns agreeing.
fn base_token_spans(
    snapshot: &DisplaySnapshot,
    base_line: u32,
) -> &[(std::ops::Range<usize>, HighlightStyle)] {
    if !snapshot.syntax_highlighting() {
        return &[];
    }
    snapshot
        .diff_map()
        .and_then(|dm| dm.base_highlights_for_line(base_line))
        .unwrap_or(&[])
}

/// Paint the diff view's two-cell status column for a hunk row.
///
/// The first cell carries the change-kind bar in `change_scope`. The second
/// carries a state bar in the scope [`editor::mark_state_scope`] gives
/// `state`. Both use the `▎` bar, as the editor gutter's two bars do, except
/// that a `seam` change cell takes the gutter's `▔` deletion mark. A state
/// cell outside the buffer gets no paint.
fn paint_status_bars(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    change_scope: &str,
    state: MarkState,
    seam: bool,
    theme: &crate::theme::Theme,
) {
    if x >= buf.area.x + buf.area.width {
        return;
    }
    let change_mark = if seam { '▔' } else { '▎' };
    buf[(x, y)]
        .set_char(change_mark)
        .set_style(theme.get(change_scope));
    if x + 1 < buf.area.x + buf.area.width {
        buf[(x + 1, y)]
            .set_char('▎')
            .set_style(theme.get(editor::mark_state_scope(theme, state)));
    }
}

/// The part of a base line one left-column row shows.
struct BaseRow<'a> {
    /// The whole base line, which its token and change spans index.
    text: &'a str,
    /// The bytes of `text` this row shows.
    window: std::ops::Range<usize>,
    /// The tab column `window` starts at, which the row's tabs expand from.
    column: u32,
    /// Columns the row's text starts past, nonzero on a wrapped continuation.
    indent: usize,
    /// Whether this is the line's first row, the one that takes its number and
    /// its status bar.
    first: bool,
}

impl<'a> BaseRow<'a> {
    /// All of `text` on one row.
    fn whole(text: &'a str) -> Self {
        Self {
            text,
            window: 0..text.len(),
            column: 0,
            indent: 0,
            first: true,
        }
    }

    /// Row `segment` of base line `line`, whose text is `text`, broken by the
    /// geometry `snapshot` wraps by, which is the geometry the block rows of a
    /// base line broke by.
    ///
    /// `memo` holds the last line broken, under its number. The rows of a base
    /// line paint one after another, so they break their line once between
    /// them.
    ///
    /// A segment past the last one the geometry gives shows nothing, which
    /// happens only if the blocks and the snapshot disagree on the geometry.
    /// Without a geometry a base line is one row.
    fn segment(
        snapshot: &DisplaySnapshot,
        text: &'a str,
        line: u32,
        segment: u32,
        memo: &mut Option<(u32, BaseLineLayout)>,
    ) -> Self {
        let Some(wrap) = snapshot.wrap_geometry() else {
            return Self::whole(text);
        };
        let layout = match memo {
            Some((broken, layout)) if *broken == line => layout,
            _ => &mut memo.insert((line, wrap.layout(text))).1,
        };
        let (window, column) = layout
            .rows
            .get(segment as usize)
            .cloned()
            .unwrap_or((text.len()..text.len(), 0));
        Self {
            text,
            window,
            column,
            indent: match segment {
                0 => 0,
                _ => layout.indent as usize,
            },
            first: segment == 0,
        }
    }
}

/// Paint one row of a base line into the left column.
///
/// The row takes its part of the line's text with the base change-span washes.
/// A line the change removed whole reads in the removed style, and a refined
/// line reads in it only inside its change spans. The line's first row also
/// takes the line number and the staged status.
///
/// A refined line that no change span reaches paints as the mirror of an
/// unchanged line, dimmed and softened. A block the formatter re-indented then
/// reads as context, and only the lines with real changes stand out.
///
/// Both sides of the diff reach here. A block row is a base line with no live
/// row beside it, and a paired modified row is one that has both, so the two
/// paint the left column identically and differ only in where the text comes
/// from.
///
/// `columns` is `(side, number, status, text, content width)` for the left
/// side, `digits` the width of its number field, and `styles` is
/// `(removed, dim, text, moved)`.
#[allow(clippy::too_many_arguments)]
fn paint_base_side(
    snapshot: &DisplaySnapshot,
    rich: &mut Option<DiffRichGutter<'_>>,
    buf: &mut Buffer,
    num_text: &mut String,
    inner: Rect,
    columns: (u16, u16, u16, u16, usize),
    digits: u16,
    y: u16,
    base_line: u32,
    row: BaseRow<'_>,
    base_changes: &crate::diff_map::BaseChangeSpans,
    tints: Option<&DiffTints>,
    styles: (Style, Style, Style, Style),
    theme: &crate::theme::Theme,
    dials: DiffDials,
) {
    let (side_x, num_x, status_x, text_x, content_w) = columns;
    let (del_style, dim_style, text_style, moved_style) = styles;

    if row.first {
        draw_diff_num(
            rich,
            buf,
            num_text,
            inner,
            (side_x, num_x),
            y,
            base_line + 1,
            dim_style,
            digits,
        );
    }

    let token_spans = base_token_spans(snapshot, base_line);
    let changes = base_changes
        .get(&base_line)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let staged = snapshot
        .diff_map()
        .and_then(|dm| dm.base_line_staged(base_line));
    let refined = snapshot
        .diff_map()
        .is_some_and(|dm| dm.base_line_refined(base_line));
    // A base line the change removed whole is gone from the working tree, so
    // the row takes its status color entire. A refined line leaves that to its
    // spans, because the chars outside them are unchanged text. Moved wins over
    // deleted on the same test the gutter bar below uses, which keeps the two
    // agreeing.
    let moved = changes
        .iter()
        .any(|(_, k, _)| matches!(k, ChangeKind::Moved));
    let tint_row = match (refined, moved) {
        (true, _) => None,
        (false, true) => tints.map(|t| t.moved),
        (false, false) => tints.map(|t| t.deleted),
    };
    // The tree pass left this base line unmarked, so the row is context that
    // re-indented or moved with its block. It takes the mirror arm's dim
    // fallback and row soften.
    let quiet = refined && changes.is_empty();
    let gap_fallback = match (quiet, refined) {
        (true, _) => dim_style,
        (false, true) => text_style,
        (false, false) => del_style,
    };
    paint_base_segment(
        buf,
        text_x + row.indent as u16,
        y,
        row.text,
        row.window,
        row.column,
        content_w.saturating_sub(row.indent),
        snapshot.tab_snapshot().tab_size(),
        snapshot.tab_snapshot().max_expansion_column(),
        token_spans,
        del_style,
        gap_fallback,
        moved_style,
        changes,
        tints,
        tints.filter(|_| quiet).map(|t| t.bg),
        tints.map(|t| t.bg),
        dials,
        tint_row,
    );

    if !row.first {
        return;
    }
    // The row is there for the removal, so the removal's status wins over a
    // staged mark on the same base line.
    match staged {
        Some(state) => {
            let change_scope = if changes
                .iter()
                .any(|(_, k, _)| matches!(k, ChangeKind::Moved))
            {
                s::DIFF_MOVED
            } else {
                s::DIFF_DELETED
            };
            draw_diff_status(
                rich,
                buf,
                inner,
                (side_x, status_x),
                y,
                change_scope,
                state,
                false,
                theme,
            );
        },
        None => {
            if let Some((status, state)) = snapshot
                .diff_map()
                .and_then(|dm| dm.staged_mark_at_base_line(base_line))
            {
                draw_diff_status(
                    rich,
                    buf,
                    inner,
                    (side_x, status_x),
                    y,
                    staged_mark_scope(status),
                    state,
                    status == DiffHunkStatus::Deleted,
                    theme,
                );
            }
        },
    }
}

/// Base lines the rows above the viewport step past (display row `scroll_row`).
///
/// Each block holds one base line, and the block's first row steps past it.
/// The first row of an unchanged line steps past its base line, and so does the
/// first row of a paired modified one. A changed line's first row and every
/// soft-wrap continuation, in a block or in the live text, step none.
///
/// So the count is the blocks whose first row is above, plus the live lines
/// whose first row is above, less those with no base line beside them. The
/// block map, the wrap map, and the diff map each answer their part without a
/// per-row walk from the top.
///
/// A continuation top row counts its own line's first row, which is above it.
/// That is the base line the row shows, one less than the value returned.
///
/// A moved-away seam removed base lines that take no row at all, so the
/// seams above the top add their lines on top of that count.
///
/// A fold hides whole lines. It hides unchanged lines, or the unmarked rows of
/// a refined hunk in a changes view excerpt. [`lines_above`] counts each hidden
/// line, and the rows with no base line come off that count as the shown ones
/// do. A hidden modified row counts its index-paired base line in both
/// layouts, because no block shows that line, so the unified layout adds those
/// lines back through [`hidden_paired_base_lines`].
fn base_line_at(snapshot: &DisplaySnapshot, scroll_row: u32) -> u32 {
    let lines_above = lines_above(snapshot, scroll_row);
    let (changed, seams) = snapshot.diff_map().map_or((0, 0), |dm| {
        (
            dm.rows_without_base_before(lines_above, snapshot.pairs_modified_hunks()),
            dm.seam_base_lines_in(0..lines_above),
        )
    });
    let hidden_paired = match snapshot.pairs_modified_hunks() {
        true => 0,
        false => hidden_paired_base_lines(snapshot, 0..lines_above),
    };
    snapshot.blocks_above(scroll_row) + lines_above.saturating_sub(changed) + seams + hidden_paired
}

/// Buffer lines whose first row is above `scroll_row`.
///
/// The count is one past the line of the last wrap row above, which counts a
/// continuation top row's own line and never asks for a wrap row past the end.
/// That row resolves to a buffer row from its end, which a fold closes, so the
/// lines a fold hides count too.
fn lines_above(snapshot: &DisplaySnapshot, scroll_row: u32) -> u32 {
    match snapshot.buffer_rows_above(scroll_row) {
        0 => 0,
        rows => {
            let row = rows - 1;
            let end = WrapPoint::new(row, snapshot.wrap_snapshot().line_len(row));
            snapshot.wrap_to_buffer(end).row + 1
        },
    }
}

/// Base lines beside the buffer rows `rows`, which a fold hid from the walk of
/// [`paint_diff_rows`].
///
/// A row counts one when a base line index-pairs with it, as
/// [`rows_without_base_before`](crate::diff_map::DiffMap::rows_without_base_before)
/// decides under pairing. That holds in both layouts: a hidden modified row
/// hides its index-paired base line, since no block shows that line. A hidden
/// row of a modified hunk past the hunk's base rows counts none.
fn hidden_base_lines(snapshot: &DisplaySnapshot, rows: std::ops::Range<u32>) -> u32 {
    let hidden = rows.end.saturating_sub(rows.start);
    match snapshot.diff_map() {
        Some(dm) if hidden > 0 => {
            let without = dm.rows_without_base_before(rows.end, true)
                - dm.rows_without_base_before(rows.start, true);
            hidden - without
        },
        _ => hidden,
    }
}

/// Base lines index-paired with the modified rows a fold hides among the
/// buffer rows `rows`.
///
/// The unified layout counts every modified row as a row with no base line
/// beside it, because its hunk's base lines sit in blocks. A hidden row's
/// index-paired line sits in no block, so [`base_line_at`] adds these back in
/// that layout.
fn hidden_paired_base_lines(snapshot: &DisplaySnapshot, rows: std::ops::Range<u32>) -> u32 {
    let Some(dm) = snapshot.diff_map() else {
        return 0;
    };
    if rows.is_empty() {
        return 0;
    }

    let inlays = snapshot.inlay_snapshot();
    let max = snapshot.buffer_snapshot().rope().max_point();
    let span = inlays.to_inlay_point(Point::new(rows.start, 0))
        ..inlays.to_inlay_point(Point::new(rows.end, 0).min(max));
    snapshot
        .fold_snapshot()
        .folds_in_range(span)
        .into_iter()
        .map(|fold| {
            // A fold's first row stays shown with the placeholder, and every
            // row after it through the fold's end row hides.
            let start = (inlays.to_buffer_point(fold.range.start).row + 1).max(rows.start);
            let end = (inlays.to_buffer_point(fold.range.end).row + 1).min(rows.end);
            if start >= end {
                return 0;
            }
            let unified =
                dm.rows_without_base_before(end, false) - dm.rows_without_base_before(start, false);
            let paired =
                dm.rows_without_base_before(end, true) - dm.rows_without_base_before(start, true);
            unified - paired
        })
        .sum()
}

/// Base lines of the moved-away seams in `seams` at buffer rows `from..=through`.
///
/// `seams` is sorted by row, and `cursor` steps past every seam at a row up to
/// `through`. A seam above `from` was counted already, by an earlier step of
/// the walk of [`paint_diff_rows`] or by [`base_line_at`], so it adds nothing.
fn seam_lines_through(seams: &[(u32, u32)], cursor: &mut usize, from: u32, through: u32) -> u32 {
    let mut lines = 0;
    while let Some(&(row, seam_lines)) = seams.get(*cursor)
        && row <= through
    {
        if row >= from {
            lines += seam_lines;
        }
        *cursor += 1;
    }
    lines
}

/// Byte span of its buffer line that `display_row` shows.
///
/// An unchanged row's base line and live line are the same bytes, so the left
/// column mirrors a row by painting the bytes the right column shows on it. A
/// row whose line does not wrap answers the whole line.
///
/// `continuation` is whether the row continues the one above, and `last_row`
/// whether no row continues it below.
///
/// Between an edit and the diff that follows it, the stored hunks still mark a
/// changed row Unchanged, so the live row's wrap points index different bytes
/// than the base line `text` holds. Each point snaps down to a char boundary
/// of `text`, which leaves that row's mirror off by a few bytes until the next
/// diff repaints it, rather than slicing inside a character.
fn mirror_window(
    snapshot: &DisplaySnapshot,
    display_row: u32,
    continuation: bool,
    last_row: bool,
    text: &str,
) -> std::ops::Range<usize> {
    let text_start = |row: u32| {
        let column = snapshot
            .display_to_buffer(DisplayPoint::new(row, snapshot.soft_wrap_indent(row)))
            .map_or(0, |point| point.column as usize);
        let mut byte = column.min(text.len());
        while !text.is_char_boundary(byte) {
            byte -= 1;
        }
        byte
    };
    let start = match continuation {
        true => text_start(display_row),
        false => 0,
    };
    let end = match last_row {
        true => text.len(),
        false => text_start(display_row + 1),
    };
    start..end.max(start)
}

/// Paint one display row's syntax-highlighted chunks into a column starting at
/// `start_x`, clamped to `max_cols` and the buffer's right edge.
///
/// `change_spans` mark the changed chars of a modified or moved row, as
/// display-column ranges tagged by [`ChangeKind`]. On a theme that can blend
/// (`tints`) a cell inside one keeps its syntax color, and the softening of
/// everything around it is what marks it. A theme that cannot blend underlines
/// the span instead, per [`mark_span`]. Columns, not byte offsets, are used
/// because the chunks expand tabs, so the counter tracks display cells.
///
/// A cell with no token color takes `moved_fallback` inside a moved span,
/// `fallback_style` inside any other change span, and `gap_fallback` outside
/// every span. A refined row passes the text style as `gap_fallback`, so its
/// unchanged cells read as unchanged text.
///
/// An added hunk the tree pass relocated in part keeps its status while its
/// moved spans say moved. Without token colors, the span's own color is then
/// the only mark of the move at tint level 0. Above that level,
/// [`span_tint_color`] paints it.
///
/// `soften_row` recedes the whole row behind the changed rows around it, by
/// blending every foreground toward the given background per [`soften_style`].
/// Pass the editor background for an unchanged row and `None` for a changed one.
///
/// `soften_gaps` recedes only the cells outside every change span, at the
/// lighter [`MODIFIED_ROW_SOFTEN`], so a refined row's changed cells lead their
/// own line. An empty `change_spans` no-ops it, which keeps a row the
/// refinement never reached at full strength.
///
/// `tint_row` shifts every cell outside a change span toward the row's own
/// status color per [`tint_style`], so a whole line reads added, modified, or
/// moved rather than only the cells the refinement matched. `dials.tint_amount`
/// shifts the cells inside a change span toward their span kind's color
/// instead, which is what keeps a replaced run legible against the row around
/// it. An amount of zero leaves both inert, so the dial's off position paints
/// exactly the colors from before it.
///
/// The same amount drains the color out of an unchanged row per
/// [`desaturate_style`], so one dial trades color between the two populations
/// and its top reads as status-colored rows on a gray field.
///
/// A changed cell whose color lands too near the background is then lifted per
/// [`brighten_style`], so a muted one still stands off the context receding
/// behind it. The lift rides on the softening, so a zero `dials.soften_scale`
/// turns it off with everything else.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_highlighted_row(
    snapshot: &DisplaySnapshot,
    display_row: u32,
    start_x: u16,
    y: u16,
    max_cols: usize,
    buf: &mut Buffer,
    fallback_style: Style,
    gap_fallback: Style,
    moved_fallback: Style,
    inlay_style: Style,
    change_spans: &[(std::ops::Range<usize>, ChangeKind, ProseChange)],
    tints: Option<&DiffTints>,
    soften_row: Option<[u8; 3]>,
    soften_gaps: Option<[u8; 3]>,
    dials: DiffDials,
    tint_row: Option<Color>,
    row_cursor: &mut RowHighlightCursor,
) {
    debug_assert!(
        change_spans.is_sorted_by_key(|(range, ..)| range.start),
        "change_spans must be start-sorted for the monotonic cursor"
    );

    let soften_gaps = soften_gaps.filter(|_| !change_spans.is_empty());
    let span_tints = tints.filter(|_| dials.tint_amount > 0.0);
    // Only a changed cell lifts, and only against a receding surround: with
    // softening off nothing recedes, so nothing has to stand off it.
    let lift_bg = tints.map(|t| t.bg).filter(|_| dials.soften_scale > 0.0);
    let recede = |style: Style| {
        let style = match soften_row.filter(|_| dials.soften_scale > 0.0) {
            Some(bg) => soften_style(
                style,
                bg,
                (CONTEXT_SOFTEN * dials.soften_scale).min(SOFTEN_CAP),
            ),
            None => style,
        };
        // Unfiltered, because the gray answers the tint dial rather than the
        // soften one, so it holds with softening turned off.
        match soften_row.is_some() {
            true => desaturate_style(style, dials.tint_amount),
            false => style,
        }
    };
    let mut col = 0usize;
    let mut span_cursor = 0;
    for chunk in snapshot.row_chunks(display_row, row_cursor) {
        let token = match chunk.is_inlay {
            true => Some(inlay_style),
            false => chunk
                .highlight_style
                .as_ref()
                .map(|hs| hs.to_ratatui_style()),
        };
        let style = recede(token.unwrap_or(fallback_style));
        let moved_style = recede(token.unwrap_or(moved_fallback));
        // Resolved per chunk rather than per cell, because a chunk's cells
        // differ only in which side of a change span they fall on.
        let gap_style = gap_style(
            recede(token.unwrap_or(gap_fallback)),
            tint_row,
            dials.tint_amount,
            soften_gaps,
            dials.soften_scale,
            lift_bg,
        );

        for ch in chunk.text.chars() {
            if ch == '\n' || col >= max_cols {
                return;
            }
            let x = start_x + col as u16;
            if x >= buf.area.x + buf.area.width {
                return;
            }
            while change_spans
                .get(span_cursor)
                .is_some_and(|(r, ..)| r.end <= col)
            {
                span_cursor += 1;
            }
            let cell_style = match change_spans.get(span_cursor) {
                Some((range, kind, prose)) if range.start <= col => {
                    let tint =
                        span_tints.map(|t| (span_tint_color(t, kind, false), dials.tint_amount));
                    let span_style = match kind {
                        ChangeKind::Moved => moved_style,
                        _ => style,
                    };
                    let marked = mark_span(
                        span_style,
                        kind,
                        *prose,
                        dials.bold,
                        dials.underline,
                        tints.is_some(),
                        tint,
                    );
                    match lift_bg {
                        Some(bg) => brighten_style(marked, bg),
                        None => marked,
                    }
                },
                _ => gap_style,
            };
            buf[(x, y)].set_char(ch).set_style(cell_style);
            col += 1;
        }
    }
}

/// Write into `out` the display-column ranges to wash on `display_row`, one of
/// the rows buffer `buffer_row` shows in the diff view's right column, each
/// tagged with its [`ChangeKind`], taken from the buffer spans of any hunk
/// covering the row.
///
/// The token detail's byte ranges are absolute buffer offsets. Each is clamped
/// to the row and mapped through [`DisplaySnapshot::buffer_to_display`], so tab
/// expansion in the painted chunks stays aligned.
///
/// A soft-wrapped line spreads a span over several display rows, so each row
/// takes only its own part of it. A part that continues from the row above
/// starts past the continuation's indent, and one that runs on below ends at
/// the row's end.
///
/// Both vectors belong to the caller and are reused across the rows of one
/// paint, so each is cleared before being filled. A row no hunk refines leaves
/// `out` empty rather than carrying the previous row's spans.
///
/// Returns whether the row is marked, by the same rule the gutter applies: a
/// covering hunk the tree pass never narrowed marks every row it spans, and a
/// narrowed one marks only the rows its runs name. A caller reads this rather
/// than `out` being non-empty, so the bars and the gutter cannot disagree.
fn write_buffer_row_change_spans<'a>(
    snapshot: &'a DisplaySnapshot,
    display_row: u32,
    buffer_row: u32,
    hunks: &mut Vec<&'a DiffHunk>,
    out: &mut Vec<(std::ops::Range<usize>, ChangeKind, ProseChange)>,
) -> bool {
    out.clear();
    hunks.clear();

    let Some(diff_map) = snapshot.diff_map() else {
        return false;
    };
    diff_map.hunks_in_range_into(buffer_row..buffer_row + 1, hunks);
    if hunks.is_empty() {
        return false;
    }
    let marked = hunks.iter().any(|hunk| {
        !hunk.refined() || hunk.marked_rows.iter().any(|run| run.contains(&buffer_row))
    });

    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let line_start = rope.point_to_offset(Point::new(buffer_row, 0));
    let line_end = line_start + rope.line_len(buffer_row) as usize;
    let row_start = snapshot.soft_wrap_indent(display_row) as usize;
    let row_end = snapshot.line_len(display_row) as usize;

    for hunk in hunks.iter() {
        let Some(detail) = &hunk.token_detail else {
            continue;
        };
        for span in &detail.buffer_spans {
            let start = span.byte_range.start.max(line_start);
            let end = span.byte_range.end.min(line_end);
            if start >= end {
                continue;
            }
            let start = snapshot.buffer_to_display(rope.offset_to_point(start));
            let end = snapshot.buffer_to_display(rope.offset_to_point(end));
            let from = match start.row.cmp(&display_row) {
                Ordering::Less => row_start,
                Ordering::Equal => start.column as usize,
                Ordering::Greater => continue,
            };
            let to = match end.row.cmp(&display_row) {
                Ordering::Less => continue,
                Ordering::Equal => end.column as usize,
                Ordering::Greater => row_end,
            };
            if from < to {
                out.push((from..to, span.kind.clone(), span.prose));
            }
        }
    }
    // Spans arrive per hunk and are not otherwise ordered. Start-sorting them
    // makes the painter's monotonic span cursor correct.
    out.sort_by_key(|(range, ..)| range.start);
    marked
}

/// The origin of a moved buffer row, for the diff view's move chip.
///
/// Scans the hunks covering `buffer_row` for the first move span and returns its
/// first counterpart source as `(file name, 0-based line)`. The file name is
/// `None` for an intra-file move (no counterpart buffer), so the chip omits the
/// path. Returns `None` when the row is not part of a move.
fn move_chip_source<'a>(
    snapshot: &'a DisplaySnapshot,
    buffer_row: u32,
    hunks: &mut Vec<&'a DiffHunk>,
) -> Option<(Option<String>, u32)> {
    hunks.clear();
    let diff_map = snapshot.diff_map()?;
    diff_map.hunks_in_range_into(buffer_row..buffer_row + 1, hunks);
    for hunk in hunks.iter() {
        let Some(detail) = &hunk.token_detail else {
            continue;
        };
        for span in &detail.buffer_spans {
            let Some(meta) = &span.move_metadata else {
                continue;
            };
            let Some(source) = meta.sources.first() else {
                continue;
            };
            let path = source.buffer.as_ref().map(|b| {
                b.path
                    .file_name()
                    .unwrap_or(b.path.as_os_str())
                    .to_string_lossy()
                    .into_owned()
            });
            return Some((path, source.line_range.start));
        }
    }
    None
}

/// The RGB gutter colors the diff view's rich sub-cell components composite
/// with, the sixteenth offsets they sit at, and the reused scene they append
/// into.
struct DiffRichGutter<'a> {
    scene: &'a mut ApcScene,
    colors: DiffRichColors,
    offsets: RichGutterOffsets,
}

/// Line-number, separator, and status-bar colors for the diff rich gutter.
///
/// The per-row change bar color is read from the row's change scope, so the
/// resolver validates every change scope is RGB up front and this stores only
/// the dim, background, and four mark-state colors used directly. `dim_amount`
/// fades every bar toward the background on a pooled unfocused page.
#[derive(Clone, Copy)]
pub(crate) struct DiffRichColors {
    dim: [u8; 3],
    bg: [u8; 3],
    staged: [u8; 3],
    unstaged: [u8; 3],
    approved: [u8; 3],
    unapproved: [u8; 3],
    dim_amount: f32,
}

impl DiffRichColors {
    /// Fade `color` toward the background by the pooled-page dim amount.
    fn faded(&self, color: [u8; 3]) -> [u8; 3] {
        dim_rgb(color, self.bg, self.dim_amount)
    }
}

/// Extract every diff gutter color as RGB, or `None` when any is missing or not
/// an RGB color, which disables rich mode for the frame so the ASCII and rich
/// paths never mix.
///
/// An approval scope the theme leaves unstyled takes its staged counterpart
/// through [`editor::mark_state_scope`], so it never disables rich mode alone.
///
/// The smooth-scroll emit asks it too, because a pooled diff page lays its
/// numbers out as runs only when these colors resolve.
///
/// See also:
/// - [`diff_gutter_is_rich`] for the layout's half of the same decision.
pub(crate) fn resolve_diff_rich_colors(
    theme: &crate::theme::Theme,
    fallback_style: Style,
    dim_amount: f32,
) -> Option<DiffRichColors> {
    let bg = fallback_style
        .bg
        .or_else(|| theme.try_get(s::UI_BACKGROUND).and_then(|st| st.bg));
    for scope in [
        s::DIFF_ADDED,
        s::DIFF_MODIFIED,
        s::DIFF_MOVED,
        s::DIFF_DELETED,
        s::DIFF_CONTEXT,
    ] {
        style_rgb(theme.get(scope).fg)?;
    }
    // Pre-fade the directly-stored colors so the line numbers and separators
    // need no per-use dimming. The per-row change color fades at its call site.
    let bg = style_rgb(bg)?;
    Some(DiffRichColors {
        dim: dim_rgb(style_rgb(theme.get(s::DIFF_CONTEXT).fg)?, bg, dim_amount),
        bg,
        staged: dim_rgb(style_rgb(theme.get(s::DIFF_STAGED).fg)?, bg, dim_amount),
        unstaged: dim_rgb(style_rgb(theme.get(s::DIFF_UNSTAGED).fg)?, bg, dim_amount),
        approved: dim_rgb(
            style_rgb(
                theme
                    .get(editor::mark_state_scope(theme, MarkState::Approved))
                    .fg,
            )?,
            bg,
            dim_amount,
        ),
        unapproved: dim_rgb(
            style_rgb(
                theme
                    .get(editor::mark_state_scope(theme, MarkState::Unapproved))
                    .fg,
            )?,
            bg,
            dim_amount,
        ),
        dim_amount,
    })
}

/// Whether a diff view paints its gutter with the rich sub-cell components,
/// which also sets how wide the gutter is.
///
/// Rich takes a scene to append into and every gutter color as RGB, the test
/// [`resolve_diff_rich_colors`] runs. The layout and the paint ask the same
/// question, so the text column and the gutter it sits past always agree.
pub(crate) fn diff_gutter_is_rich(
    has_scene: bool,
    theme: &crate::theme::Theme,
    fallback_style: Style,
) -> bool {
    has_scene && resolve_diff_rich_colors(theme, fallback_style, 0.0).is_some()
}

/// Emit a right-aligned diff line number as a sub-cell run (rich) or paint the
/// ASCII number.
///
/// `side_x` is the side's first cell, which the rich offsets count from, and
/// `num_x` the cell gutter's number field, `digits` cells wide.
#[allow(clippy::too_many_arguments)]
fn draw_diff_num(
    rich: &mut Option<DiffRichGutter<'_>>,
    buf: &mut Buffer,
    scratch: &mut String,
    inner: Rect,
    (side_x, num_x): (u16, u16),
    y: u16,
    num: u32,
    dim_style: Style,
    digits: u16,
) {
    scratch.clear();
    write!(scratch, "{num}").expect("writing to a String is infallible");
    match rich {
        Some(rg) => {
            let advance = text_run::advance_sixteenths(scratch.len(), TEXT_SCALE_COMPACT);
            let right_edge = (side_x - inner.x) * 16 + rg.offsets.number_right;
            TextRun {
                col: right_edge.saturating_sub(advance) as i16,
                row: ((y - inner.y) * 16) as i16,
                scale: TEXT_SCALE_COMPACT,
                color: rg.colors.dim,
                bg: Some(rg.colors.bg),
                text: scratch,
                follow: 0,
                anchor: None,
            }
            .render(inner, buf, &mut *rg.scene);
        },
        None => render_side_num(buf, scratch, num_x, y, num, dim_style, digits.into()),
    }
}

/// Emit the change and staged bars (rich) or paint the ASCII status glyphs.
///
/// The bars sit where the editor's rich gutter puts them, five sixteenths
/// wide each, at the rich offsets from `side_x`, the side's first cell. The
/// glyphs take the cell gutter's status column at `status_x`. A `seam` change
/// bar is the short top-aligned bar the rich gutter draws for a deletion.
#[allow(clippy::too_many_arguments)]
fn draw_diff_status(
    rich: &mut Option<DiffRichGutter<'_>>,
    buf: &mut Buffer,
    inner: Rect,
    (side_x, status_x): (u16, u16),
    y: u16,
    change_scope: &str,
    state: MarkState,
    seam: bool,
    theme: &crate::theme::Theme,
) {
    match rich {
        Some(rg) => {
            let change = rg
                .colors
                .faded(style_rgb(theme.get(change_scope).fg).unwrap_or(rg.colors.dim));
            let staged_color = match state {
                MarkState::Staged => rg.colors.staged,
                MarkState::Unstaged => rg.colors.unstaged,
                MarkState::Approved => rg.colors.approved,
                MarkState::Unapproved => rg.colors.unapproved,
            };
            let side = (side_x - inner.x) * 16;
            let y0 = ((y - inner.y) * 16) as i16;
            Bar {
                x: (side + rg.offsets.git_x) as i16,
                y: y0,
                width: 5,
                height: if seam { 6 } else { 16 },
                color: change,
            }
            .render(inner, buf, &mut *rg.scene);
            Bar {
                x: (side + rg.offsets.staged_x) as i16,
                y: y0,
                width: 5,
                height: 16,
                color: staged_color,
            }
            .render(inner, buf, &mut *rg.scene);
        },
        None => paint_status_bars(buf, status_x, y, change_scope, state, seam, theme),
    }
}

/// Emit the removal tick (rich) or paint its ASCII glyphs in the right status
/// column of block row `y`.
///
/// The tick spans both status bars at the top edge of the row, where the
/// removed lines left the live side. It takes the deleted color and no staged
/// color, because the left column already shows the removal's staged state.
///
/// `side_x` is the side's first cell, which the rich offsets count from, and
/// `status_x` the cell gutter's status column.
fn draw_removal_tick(
    rich: &mut Option<DiffRichGutter<'_>>,
    buf: &mut Buffer,
    inner: Rect,
    (side_x, status_x): (u16, u16),
    y: u16,
    theme: &crate::theme::Theme,
) {
    match rich {
        Some(rg) => {
            let x0 = (side_x - inner.x) * 16 + rg.offsets.git_x;
            let y0 = ((y - inner.y) * 16) as i16;
            // The tick runs from the change bar's left edge through the staged
            // bar, both of `draw_diff_status`'s bars being five wide.
            Bar {
                x: x0 as i16,
                y: y0,
                width: rg.offsets.staged_x + 5 - rg.offsets.git_x,
                height: 2,
                color: rg
                    .colors
                    .faded(style_rgb(theme.get(s::DIFF_DELETED).fg).unwrap_or(rg.colors.dim)),
            }
            .render(inner, buf, &mut *rg.scene);
        },
        None => {
            let style = theme.get(s::DIFF_DELETED);
            for x in [status_x, status_x + 1] {
                if x < buf.area.x + buf.area.width {
                    buf[(x, y)].set_char('▔').set_style(style);
                }
            }
        },
    }
}

/// The scope a staged mark of `status` paints its change bar in.
fn staged_mark_scope(status: DiffHunkStatus) -> &'static str {
    match status {
        DiffHunkStatus::Added => s::DIFF_ADDED,
        DiffHunkStatus::Modified => s::DIFF_MODIFIED,
        DiffHunkStatus::Moved => s::DIFF_MOVED,
        DiffHunkStatus::Deleted => s::DIFF_DELETED,
    }
}

/// Paint a move-origin chip after the rendered side text to surface where
/// the moved hunk's counterpart lives.
///
/// A cross-file move (`path` is `Some`) paints `<- {path}:{line+1}`. An
/// intra-file move (`path` is `None`) paints a path-less `<- {line+1}`, since
/// repeating the row's own file name is noise. `text_cols` is the column count
/// already consumed by the row's text; the chip starts two columns later (so the
/// gap is visually obvious) and truncates if fewer columns remain. No-op when
/// `text_cols + 2 >= max_cols`.
#[allow(clippy::too_many_arguments)]
fn render_move_chip(
    buf: &mut Buffer,
    start_x: u16,
    y: u16,
    text_cols: usize,
    max_cols: usize,
    path: Option<&str>,
    line: u32,
    style: Style,
) {
    let chip_start_col = text_cols.saturating_add(2);
    if chip_start_col >= max_cols {
        return;
    }
    let chip = match path {
        Some(path) => format!("<- {}:{}", path, line + 1),
        None => format!("<- {}", line + 1),
    };
    let available = max_cols - chip_start_col;
    for (i, ch) in chip.chars().take(available).enumerate() {
        let x = start_x + (chip_start_col + i) as u16;
        if x >= buf.area.x + buf.area.width {
            break;
        }
        buf[(x, y)].set_char(ch).set_style(style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::{BufferId, TextBuffer},
        diff_map::{self, ChangeSpan, DiffHunk, DiffHunkStatus, DiffMap, MarkKind, TokenDetail},
        display_map::InlayKind,
        render::paint::FAINT_CONTRAST_FLOOR,
        theme::{scope::UI_SELECTION_EDITOR, Theme},
        workspace::diff::WorktreeBase,
    };
    use std::sync::{Arc, RwLock};
    use stoat_config::{LineNumbers, WrapMode};
    use stoat_language::structural_diff;
    use stoat_scheduler::{Executor, TestScheduler};
    use stoat_text::Bias;
    use stoatty_protocol::command::{encode_bar, BarCommand};

    fn buffer_text(buf: &Buffer, y: u16) -> String {
        (buf.area.x..buf.area.x + buf.area.width)
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    /// The review screen and the commit preview lay a side out as status glyph,
    /// line number, text, and split at every width.
    ///
    /// These are the values their two inline formulas produced before both
    /// folded into [`DiffColumns`]. The paint and the cursor read the same
    /// numbers, so a drift shows here rather than as a cursor off its column.
    #[test]
    fn the_review_layout_leads_with_the_status_glyph_and_never_unifies() {
        assert_eq!(
            DiffColumns::compute(Rect::new(3, 0, 121, 10), DiffLayout::REVIEW),
            DiffColumns {
                left_x: 3,
                left_num_x: 4,
                status_left_x: 3,
                left_sep_x: None,
                left_text_x: 9,
                left_content_w: 54,
                right_x: 64,
                right_num_x: 65,
                status_right_x: 64,
                right_sep_x: None,
                right_text_x: 70,
                right_content_w: 54,
                sep_x: Some(63),
                rich: None,
            },
            "a wide review body splits either side of the mid-divider"
        );

        assert_eq!(
            DiffColumns::compute(Rect::new(0, 0, 40, 5), DiffLayout::REVIEW),
            DiffColumns {
                left_x: 0,
                left_num_x: 1,
                status_left_x: 0,
                left_sep_x: None,
                left_text_x: 6,
                left_content_w: 13,
                right_x: 20,
                right_num_x: 21,
                status_right_x: 20,
                right_sep_x: None,
                right_text_x: 26,
                right_content_w: 14,
                sep_x: Some(19),
                rich: None,
            },
            "a review body stays two-column however narrow it gets"
        );
    }

    /// The diff view lays a side out as line number, status column, rule, text,
    /// and aliases both sides onto one column below [`DIFF_TWO_COLUMN_MIN`].
    ///
    /// A cell gutter for two digits spans the plain degraded gutter's six
    /// cells. A rich one spans the sub-cell gutter's four cells at the test
    /// scale, where a digit advances a whole cell, and keeps the cell offsets.
    #[test]
    fn the_diff_view_layout_rules_off_its_gutter_and_unifies_when_narrow() {
        let wide = Rect::new(3, 0, 121, 10);
        let cell = DiffColumns::compute(wide, DiffLayout::diff_view(2, false));
        assert_eq!(
            cell,
            DiffColumns {
                left_x: 3,
                left_num_x: 4,
                status_left_x: 6,
                left_sep_x: Some(8),
                left_text_x: 9,
                left_content_w: 54,
                right_x: 64,
                right_num_x: 65,
                status_right_x: 67,
                right_sep_x: Some(69),
                right_text_x: 70,
                right_content_w: 54,
                sep_x: Some(63),
                rich: None,
            },
            "a wide diff body puts base text left and buffer text right"
        );

        assert_eq!(
            DiffColumns::compute(wide, DiffLayout::diff_view(2, true)),
            DiffColumns {
                left_text_x: 7,
                left_content_w: 56,
                right_text_x: 68,
                right_content_w: 56,
                rich: Some(RichGutterOffsets {
                    number_right: 39,
                    git_x: 43,
                    staged_x: 50,
                    separator_x: 59,
                }),
                ..cell
            },
            "a rich gutter is narrower and places its parts in sixteenths"
        );

        assert_eq!(
            DiffColumns::compute(Rect::new(0, 0, 40, 5), DiffLayout::diff_view(2, false)),
            DiffColumns {
                left_x: 0,
                left_num_x: 1,
                status_left_x: 3,
                left_sep_x: Some(5),
                left_text_x: 6,
                left_content_w: 34,
                right_x: 0,
                right_num_x: 1,
                status_right_x: 3,
                right_sep_x: Some(5),
                right_text_x: 6,
                right_content_w: 34,
                sep_x: None,
                rich: None,
            },
            "a narrow diff body aliases both sides onto one column, no divider"
        );
    }

    /// The digit count fits the larger of the live and base line counts, at
    /// least two as the plain gutter's is.
    #[test]
    fn a_diff_gutter_counts_the_digits_of_the_longer_file() {
        let base: String = (0..120).map(|n| format!("{n}\n")).collect();
        let digits = |base: &str, text: &str| {
            diff_gutter_digits(&diff_editor(base, text).display_map.snapshot())
        };
        assert_eq!([digits(&base, "a\nb\n"), digits("a\n", "a\nb\n")], [3, 2]);
    }

    /// A diff side reserves exactly the plain editor's gutter for the same
    /// digit count, cell and rich alike, so toggling the view leaves the live
    /// text column where the plain pane had it.
    #[test]
    fn a_diff_side_reserves_the_plain_gutters_width() {
        let widths = [50, 1500].map(|lines| {
            let digits = editor::gutter_digits(lines);
            [false, true].map(|rich| {
                (
                    DiffLayout::diff_view(digits, rich).gutter_w,
                    editor::measure_gutter_width(digits, rich),
                )
            })
        });
        assert_eq!(
            widths,
            [[(6, 6), (4, 4)], [(8, 8), (6, 6)]],
            "each pair is the diff side's width beside the plain gutter's"
        );
    }

    /// A repaint that changed nothing reuses the derived rows, and a change to
    /// anything they were derived from rebuilds them.
    ///
    /// Reuse is what the cache is for, and every input in the key is there
    /// because the rows would otherwise be stale against it. A key missing one
    /// of them shows as a diff view that keeps painting the state before the
    /// edit, so each is moved on its own and checked to rebuild.
    #[test]
    fn row_states_are_reused_until_something_they_read_moves() {
        let mut editor = diff_editor("a\nb\nc\nd\n", "a\nB\nc\nD\n");
        let snapshot = editor.display_map.snapshot();
        let rows = 0..snapshot.line_count();

        let mut cache = None;
        diff_row_states(&snapshot, rows.clone(), &mut cache);
        let key = cache.as_ref().expect("built").key;

        // Marking the held entry with a status this text cannot produce for row
        // zero is what distinguishes reuse from a rebuild that lands on the same
        // answer. Comparing the rows alone would pass either way.
        cache.as_mut().expect("built").rows[0].status = DiffStatus::Moved;
        assert_eq!(
            diff_row_states(&snapshot, rows.clone(), &mut cache)[0].status,
            DiffStatus::Moved,
            "a repeat paint rebuilt the entry instead of reusing it",
        );
        assert_eq!(cache.as_ref().expect("held").key, key, "under the same key");

        // A narrower window is a different set of rows, so it must not be served
        // from the entry built for the wider one.
        diff_row_states(&snapshot, rows.start..rows.end - 1, &mut cache);
        assert_ne!(
            cache.as_ref().expect("held").key,
            key,
            "a shorter viewport rebuilds",
        );

        // The versions cannot be moved through the snapshot without rebuilding
        // one, so they are checked where the key is formed. Each is in it
        // because the rows go stale against it, and a key that dropped one would
        // keep painting the state from before that change.
        let base = diff_row_key(3, 20, 7, 11, 17, 13, true);
        for moved in [
            diff_row_key(4, 20, 7, 11, 17, 13, true),
            diff_row_key(3, 21, 7, 11, 17, 13, true),
            diff_row_key(3, 20, 8, 11, 17, 13, true),
            diff_row_key(3, 20, 7, 12, 17, 13, true),
            diff_row_key(3, 20, 7, 11, 18, 13, true),
            diff_row_key(3, 20, 7, 11, 17, 14, true),
        ] {
            assert_ne!(moved, base, "an input moved without changing the key");
        }
    }

    #[test]
    fn base_line_at_matches_the_reference_walk() {
        // The per-row walk, kept as the correctness oracle. A row carries a base
        // line when the left column paints one on it: the first row of every
        // block that opens a line, every unchanged row, and, where the layout
        // pairs, a modified row still inside its hunk's base text. A moved-away
        // seam's lines come before the buffer row it sits at, with no row of
        // their own. A soft-wrap continuation, of a block or of a live line, and
        // a paired line's overflow block belong to the line its first row
        // counted.
        //
        // The rows a fold hides count once the walk passes the last row of the
        // line the fold starts on, where the placeholder sits: each unchanged
        // row, and in both layouts each modified row still inside its hunk's
        // base text, whose line hides with it.
        fn reference(snapshot: &DisplaySnapshot, scroll_row: u32) -> u32 {
            let seams = |rows: std::ops::Range<u32>| {
                snapshot
                    .diff_map()
                    .map_or(0, |dm| dm.seam_base_lines_in(rows))
            };
            let in_base = |buffer_row: u32| {
                snapshot.diff_map().is_some_and(|dm| {
                    dm.hunks_in_range(buffer_row..buffer_row + 1)
                        .iter()
                        .any(|hunk| {
                            hunk.status == DiffHunkStatus::Modified
                                && buffer_row.saturating_sub(hunk.buffer_start_line)
                                    < dm.base_line_count(hunk)
                        })
                })
            };
            let carries = |buffer_row: u32, shown: bool| match snapshot.line_diff_status(buffer_row)
            {
                DiffStatus::Unchanged => true,
                DiffStatus::Modified => {
                    (!shown || snapshot.pairs_modified_hunks()) && in_base(buffer_row)
                },
                _ => false,
            };
            let next_shown = |row: u32| {
                (row + 1..snapshot.line_count())
                    .find_map(|row| match snapshot.classify_row(row) {
                        BlockRowKind::BufferRow { buffer_row } => Some(buffer_row),
                        BlockRowKind::Block { .. } => None,
                    })
                    .unwrap_or(snapshot.buffer_line_count())
            };

            let mut base_line = 0;
            for row in 0..scroll_row {
                let continuation = snapshot.is_wrap_continuation(row);
                match snapshot.classify_row(row) {
                    BlockRowKind::Block { block, line_index } => {
                        base_line +=
                            u32::from(!continuation && block.0.first_segment + line_index == 0)
                    },
                    BlockRowKind::BufferRow { buffer_row } => {
                        if !continuation {
                            base_line += seams(buffer_row..buffer_row + 1)
                                + u32::from(carries(buffer_row, true));
                        }
                        let last_row = row + 1 >= snapshot.line_count()
                            || !snapshot.is_wrap_continuation(row + 1);
                        if last_row {
                            let hidden = buffer_row + 1..next_shown(row);
                            base_line += seams(hidden.clone())
                                + hidden.filter(|&hidden| carries(hidden, false)).count() as u32;
                        }
                    },
                }
            }
            base_line
        }

        fn assert_agrees(snapshot: &DisplaySnapshot, fixture: &str) {
            for row in 0..snapshot.line_count() {
                assert_eq!(
                    base_line_at(snapshot, row),
                    reference(snapshot, row),
                    "base_line_at disagrees with the walk at row {row} for {fixture}"
                );
            }
        }

        // Each fixture is (base HEAD, buffer). Together they span a mid-file
        // modification with deletion, a deletion at the top, consecutive changes,
        // and a deletion at the tail, so the display carries added, deleted, and
        // modified hunks with deleted-base block rows spliced in. The last one's
        // unchanged, modified, added, and removed lines outrun the 20-column
        // wrap width.
        let fixtures = [
            ("a\nb\nc\nd\ne\nf\ng\nh\n", "a\nB\nc\nd\nINS\ng\nh\n"),
            ("x\ny\nz\nw\n", "z\nw\n"),
            ("1\n2\n3\n4\n5\n", "1\nTWO\nTHREE\n5\n"),
            ("p\nq\nr\ns\nt\n", "p\nq\nr\n"),
            (
                "keep\nthe quick brown fox jumps over the dog\nold text long enough to wrap\na removed line long enough to wrap\ntail\n",
                "keep\nthe quick brown fox jumps over the dog\nnew text long enough to wrap around\ntail\nan added line long enough to wrap\n",
            ),
            (
                "keep\nthe old line is much longer than the new one\ntail\n",
                "keep\nthe new line\ntail\n",
            ),
        ];
        // Both layouts, since pairing changes which rows carry a base line and
        // which of them block.
        let mut saw_blocks = false;
        let mut saw_pairs = false;
        let mut saw_wraps = false;
        let mut saw_block_wraps = false;
        let mut saw_overflow = false;
        for (base, text) in fixtures {
            let mut stacked_rows = 0;
            for pair in [false, true] {
                let mut editor = diff_editor(base, text);
                editor.display_map.set_pair_modified_hunks(pair);
                editor.display_map.set_wrap_width(Some(20));
                let snapshot = editor.display_map.snapshot();
                let total = snapshot.line_count();
                saw_blocks |= total > snapshot.buffer_line_count();
                saw_wraps |= (0..total).any(|row| snapshot.is_wrap_continuation(row));
                saw_block_wraps |= (0..total).any(|row| {
                    matches!(
                        snapshot.classify_row(row),
                        BlockRowKind::Block { line_index, .. } if line_index > 0
                    )
                });
                saw_overflow |= (0..total).any(|row| {
                    matches!(
                        snapshot.classify_row(row),
                        BlockRowKind::Block { block, .. } if block.0.first_segment > 0
                    )
                });
                match pair {
                    false => stacked_rows = total,
                    // Pairing puts a base row beside a live one rather than on a
                    // row of its own, so a fixture it reaches is shorter.
                    true => saw_pairs |= total < stacked_rows,
                }
                assert_agrees(&snapshot, &format!("{base:?}->{text:?} paired={pair}"));
            }
        }
        assert!(
            saw_blocks,
            "fixtures must splice deleted-base block rows to exercise the block case"
        );
        assert!(saw_pairs, "and pair rows to exercise the paired case");
        assert!(
            saw_wraps,
            "and wrap lines to exercise the continuation case"
        );
        assert!(
            saw_block_wraps,
            "and wrap a removed line to exercise a block's continuation"
        );
        assert!(
            saw_overflow,
            "and outrun a live line with its paired base line to exercise an overflow block"
        );

        // No plain-text diff produces a move, so its seam and its moved-to row
        // come from a hand-built map.
        for pair in [false, true] {
            let mut editor = moved_line_editor();
            editor.display_map.set_pair_modified_hunks(pair);
            assert_agrees(
                &editor.display_map.snapshot(),
                &format!("the moved line, paired={pair}"),
            );
        }

        // A fold inside a modified hunk hides rows whose index-paired base lines
        // hide with them. The hunks are hand-built, one with a base shorter than
        // its live side and one with a longer base.
        for (live, removed, fold) in [
            (4..16, 10, Point::new(5, 1)..Point::new(13, 2)),
            (4..14, 12, Point::new(5, 1)..Point::new(11, 2)),
        ] {
            for pair in [false, true] {
                let mut editor = folded_hunk_editor(live.clone(), removed, fold.clone());
                editor.display_map.set_pair_modified_hunks(pair);
                let snapshot = editor.display_map.snapshot();
                let shown_lines = (0..snapshot.line_count())
                    .filter(|&row| {
                        !snapshot.is_wrap_continuation(row)
                            && matches!(snapshot.classify_row(row), BlockRowKind::BufferRow { .. })
                    })
                    .count() as u32;
                assert!(
                    shown_lines < snapshot.buffer_line_count(),
                    "the fold hides rows of the hunk"
                );
                assert_agrees(
                    &snapshot,
                    &format!("a fold in {live:?} over {removed} base lines, paired={pair}"),
                );
            }
        }
    }

    /// A fold hides unchanged lines alone, so each row it keeps starts on the
    /// base line it starts on with no fold, in both layouts.
    #[test]
    fn a_fold_keeps_the_base_line_of_every_row_it_keeps() {
        let base: String = (1..=20).map(|n| format!("{n}\n")).collect();
        let text = base
            .replace("\n3\n", "\nTHREE\n")
            .replace("\n16\n", "\n")
            .replace("\n18\n", "\n18\nNEW\n");
        for pair in [false, true] {
            let mut plain = diff_editor(&base, &text);
            plain.display_map.set_pair_modified_hunks(pair);
            let mut folded = diff_editor(&base, &text);
            folded.display_map.set_pair_modified_hunks(pair);
            folded
                .display_map
                .fold(vec![Point::new(5, 1)..Point::new(11, 2)]);
            let (plain, folded) = (plain.display_map.snapshot(), folded.display_map.snapshot());

            let kept: Vec<(u32, u32, u32)> = (0..folded.line_count())
                .filter(|&row| !folded.is_wrap_continuation(row))
                .filter_map(|row| match folded.classify_row(row) {
                    BlockRowKind::BufferRow { buffer_row } => {
                        let plain_row = plain.buffer_to_display(Point::new(buffer_row, 0)).row;
                        Some((
                            buffer_row,
                            base_line_at(&folded, row),
                            base_line_at(&plain, plain_row),
                        ))
                    },
                    BlockRowKind::Block { .. } => None,
                })
                .collect();
            let rows: Vec<u32> = kept.iter().map(|&(row, ..)| row).collect();
            let (folded_lines, plain_lines): (Vec<_>, Vec<_>) = kept
                .iter()
                .map(|&(row, folded, plain)| ((row, folded), (row, plain)))
                .unzip();
            assert_eq!(
                (rows, folded_lines),
                ((0..=5).chain(12..=20).collect(), plain_lines),
                "paired={pair}",
            );
        }
    }

    /// The left column numbers a row below a fold by its own base line, as the
    /// right column numbers its buffer line.
    #[test]
    fn a_folded_diff_view_numbers_its_base_lines_past_the_fold() {
        let base: String = (1..=20).map(|n| format!("{n}\n")).collect();
        let text = base.replace("\n16\n", "\nSIXTEEN\n");
        let mut editor = diff_editor(&base, &text);
        editor
            .display_map
            .fold(vec![Point::new(0, 0)..Point::new(11, 2)]);
        let area = Rect::new(0, 0, 120, 10);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &rgb_diff_theme(),
            &mut buf,
            false,
            None,
            None,
            false,
            DiffDials::shipped(),
            None,
        );

        let expected: Vec<(String, String)> = [1, 13, 14, 15, 16, 17, 18, 19, 20, 21]
            .map(|n| (n.to_string(), n.to_string()))
            .to_vec();
        assert_eq!(number_pairs(&buf, area), expected);
    }

    /// A fold inside a modified hunk hides live rows, and the rows below it
    /// count the base lines index-paired with the rows it hid. The two-column
    /// layout paints those lines beside the hidden rows, and the unified layout
    /// hides them with the rows, so each one counts in both. The unified block
    /// of the run above the fold holds the lines of that run alone.
    #[test]
    fn a_fold_inside_a_modified_hunk_counts_the_base_lines_it_hid() {
        let mut editor = folded_hunk_editor(4..16, 10, Point::new(5, 1)..Point::new(13, 2));
        let mut render = |area: Rect| {
            let mut buf = Buffer::empty(area);
            render_diff_view(
                &mut editor,
                area,
                Style::default(),
                &rgb_diff_theme(),
                &mut buf,
                false,
                None,
                None,
                false,
                DiffDials::shipped(),
                None,
            );
            buf
        };

        let paired_area = Rect::new(0, 0, 120, 12);
        let paired: Vec<String> = number_pairs(&render(paired_area), paired_area)
            .into_iter()
            .map(|(left, _)| left)
            .collect();
        let unified_area = Rect::new(0, 0, 60, 24);
        let unified = render(unified_area);
        let cols = two_digit_columns(unified_area);
        let number_beside = |needle: &str| {
            let y = (0..unified_area.height)
                .find(|&y| {
                    line_text(&unified, y, cols.right_text_x..cols.right_text_x + 3).trim()
                        == needle
                })
                .expect("a row shows the needle");
            line_text(&unified, y, cols.left_num_x..cols.left_num_x + 2)
                .trim()
                .to_string()
        };
        assert_eq!(
            (paired, number_beside("b5"), number_beside("17")),
            (
                ["1", "2", "3", "4", "5", "6", "", "", "15", "16", "17", "18"]
                    .map(String::from)
                    .to_vec(),
                "5".to_string(),
                "15".to_string(),
            ),
            "the paired rows count their base lines, and in the unified layout \
             the first run's block starts at b5 and the mirror row past the fold \
             counts the lines hidden with the rows",
        );
    }

    /// In the unified layout a fold inside a modified hunk hides the base lines
    /// index-paired with the rows it hides, and each kept run shows its own
    /// base lines in a block above it. The base lines past the live side join
    /// the run that holds the last live row.
    #[test]
    fn a_fold_in_the_unified_layout_shows_each_kept_runs_base_lines_above_it() {
        let mut editor = folded_hunk_editor(4..14, 12, Point::new(5, 1)..Point::new(11, 2));
        let area = Rect::new(0, 0, 60, 24);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &rgb_diff_theme(),
            &mut buf,
            false,
            None,
            None,
            false,
            DiffDials::shipped(),
            None,
        );

        let cols = two_digit_columns(area);
        let text_at = |y: u16| {
            line_text(&buf, y, cols.right_text_x..cols.right_text_x + 4)
                .trim()
                .to_string()
        };
        let number_beside = |needle: &str| {
            let y = (0..area.height)
                .find(|&y| text_at(y) == needle)
                .expect("a row shows the needle");
            line_text(&buf, y, cols.left_num_x..cols.left_num_x + 2)
                .trim()
                .to_string()
        };
        assert_eq!(
            (
                (0..16).map(text_at).collect::<Vec<_>>(),
                number_beside("b13"),
                number_beside("15"),
            ),
            (
                [
                    "1", "2", "3", "4", "b5", "b6", "5", "6...", "b13", "b14", "b15", "b16", "13",
                    "14", "15", "16",
                ]
                .map(String::from)
                .to_vec(),
                "13".to_string(),
                "17".to_string(),
            ),
            "the first run's lines above it, the fold, then the last run's lines \
             and the excess above it",
        );
    }

    /// A fold that hides a paired row hides its base line whole, so the wrapped
    /// segments of that line take no rows after the fold's placeholder.
    #[test]
    fn a_hidden_paired_row_leaves_no_base_segments_past_the_fold() {
        let head = "keep\n";
        let removed = format!("A\n{}\nB\n", "x".repeat(120));
        let dm = DiffMap::from_hunks(
            [DiffHunk {
                status: DiffHunkStatus::Modified,
                unstaged_lines: std::iter::once(1..4).collect(),
                marked_rows: Vec::new(),
                buffer_start_line: 1,
                buffer_line_range: 1..4,
                base_byte_range: head.len()..head.len() + removed.len() - 1,
                anchor_range: None,
                token_detail: None,
            }],
            Some(Arc::new(format!("{head}{removed}tail\n"))),
        );
        let mut editor = diff_editor_with_map("keep\nA2\nshort\nB2\ntail\n", dm);
        editor
            .display_map
            .fold(vec![Point::new(1, 2)..Point::new(2, 5)]);
        let area = Rect::new(0, 0, 120, 6);
        let digits = diff_gutter_digits(&editor.display_map.snapshot());
        editor::fit_diff_view(&mut editor, area, WrapMode::EditorWidth, 80, digits, false);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &rgb_diff_theme(),
            &mut buf,
            false,
            None,
            None,
            false,
            DiffDials::shipped(),
            None,
        );

        let x = two_digit_columns(area).left_text_x;
        assert_eq!(
            (0..4)
                .map(|y| line_text(&buf, y, x..x + 4).trim().to_string())
                .collect::<Vec<_>>(),
            ["keep", "A", "B", "tail"],
            "the base column runs from the placeholder row to the row below the fold",
        );
    }

    /// A block whose row a fold hid paints after the fold's placeholder, and
    /// it shows its own base lines, because the walk counts the hidden rows up
    /// to the block's row first. The fold hides the hunk's first row too, so
    /// that row's index-paired `b7` hides with it and the block holds `b8`.
    #[test]
    fn a_block_below_a_hidden_row_paints_its_own_base_lines() {
        let head = "1\n2\n3\n4\n5\n6\n";
        let removed = "b7\nb8\n";
        let tail = "9\n10\n11\n12\n";
        let dm = DiffMap::from_hunks(
            [DiffHunk {
                status: DiffHunkStatus::Modified,
                unstaged_lines: std::iter::once(6..8).collect(),
                marked_rows: Vec::new(),
                buffer_start_line: 6,
                buffer_line_range: 6..8,
                base_byte_range: head.len()..head.len() + removed.len() - 1,
                anchor_range: None,
                token_detail: None,
            }],
            Some(Arc::new(format!("{head}{removed}{tail}"))),
        );
        let mut editor = diff_editor_with_map(&format!("{head}s7\ns8\n{tail}"), dm);
        editor
            .display_map
            .fold(vec![Point::new(2, 1)..Point::new(6, 2)]);
        let area = Rect::new(0, 0, 60, 12);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &rgb_diff_theme(),
            &mut buf,
            false,
            None,
            None,
            false,
            DiffDials::shipped(),
            None,
        );

        let cols = two_digit_columns(area);
        let row = |y: u16| {
            [
                cols.left_num_x..cols.left_num_x + 2,
                cols.left_text_x..cols.left_text_x + 4,
            ]
            .map(|cells| line_text(&buf, y, cells).trim().to_string())
        };
        assert_eq!(
            [3, 4, 5].map(row),
            [["8", "b8"], ["8", "s8"], ["9", "9"]].map(|cells| cells.map(String::from)),
            "the shown run's block past the placeholder, its live row, then the \
             mirror row below the hunk",
        );
    }

    /// With a cursor line, the right column counts each buffer line's distance
    /// from it and the cursor line keeps its own number. The left column still
    /// numbers the base file.
    #[test]
    fn the_diff_view_numbers_the_buffer_column_from_the_cursor_line() {
        let text: String = (1..=10).map(|n| format!("{n}\n")).collect();
        let mut editor = diff_editor(&text, &text);
        let area = Rect::new(0, 0, 120, 10);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &rgb_diff_theme(),
            &mut buf,
            false,
            None,
            None,
            false,
            DiffDials::shipped(),
            Some(5),
        );

        let expected: Vec<(String, String)> = (1..=10)
            .zip([4, 3, 2, 1, 5, 1, 2, 3, 4, 5])
            .map(|(left, right)| (left.to_string(), right.to_string()))
            .collect();
        assert_eq!(number_pairs(&buf, area), expected);
    }

    /// The `(left, right)` line numbers on each row of a cell-gutter diff view
    /// of files under a hundred lines, painted over `area` of `buf`.
    fn number_pairs(buf: &Buffer, area: Rect) -> Vec<(String, String)> {
        let cols = two_digit_columns(area);
        let num = |x: u16, y: u16| {
            (x..x + 2)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim()
                .to_string()
        };
        (area.y..area.y + area.height)
            .map(|y| (num(cols.left_num_x, y), num(cols.right_num_x, y)))
            .collect()
    }

    /// The cell columns a diff view paints over `area` for files under a
    /// hundred lines, whose gutters take two digits.
    fn two_digit_columns(area: Rect) -> DiffColumns {
        DiffColumns::compute(area, DiffLayout::diff_view(2, false))
    }

    /// The cell columns of a 120-wide diff view of files under a hundred
    /// lines, as the 120-wide renders with no scene lay it out.
    fn wide_columns() -> DiffColumns {
        two_digit_columns(Rect::new(0, 0, 120, 8))
    }

    /// The columns a harness's frame lays a diff view of files under a hundred
    /// lines out in, rich as the harness's theme resolves it under stoatty.
    fn harness_columns(h: &crate::test_harness::TestHarness) -> DiffColumns {
        let theme = &h.stoat.theme;
        let rich = diff_gutter_is_rich(
            h.stoat.stoatty,
            theme,
            theme.get(crate::theme::scope::UI_TEXT),
        );
        let area = h.rendered_buffer().area;
        DiffColumns::compute(area, DiffLayout::diff_view(2, rich))
    }

    /// The left text cells, up to the mid-divider in the two-column layout.
    fn left_text(cols: &DiffColumns) -> std::ops::Range<u16> {
        cols.left_text_x..cols.left_text_x + cols.left_content_w as u16
    }

    /// The right text cells.
    fn right_text(cols: &DiffColumns) -> std::ops::Range<u16> {
        cols.right_text_x..cols.right_text_x + cols.right_content_w as u16
    }

    /// The cells the left line number shows in.
    fn left_num(cols: &DiffColumns) -> std::ops::Range<u16> {
        number_cells(cols, cols.left_x, cols.left_num_x)
    }

    /// The cells the right line number shows in.
    fn right_num(cols: &DiffColumns) -> std::ops::Range<u16> {
        number_cells(cols, cols.right_x, cols.right_num_x)
    }

    /// The cells a line number shows in on the side that starts at `side_x`.
    ///
    /// A cell gutter writes the number into its field at `num_x`. A rich
    /// gutter's run ends at its sixteenth offset and composites from the cell
    /// it starts in, so a number of up to two digits lands in the cells before
    /// the offset's own cell, where the change bar composites.
    fn number_cells(cols: &DiffColumns, side_x: u16, num_x: u16) -> std::ops::Range<u16> {
        match cols.rich {
            Some(rich) => side_x..side_x + rich.number_right / 16,
            None => num_x..num_x + (cols.status_left_x - cols.left_num_x),
        }
    }

    /// A diff-view editor over `text`, diffed against `base`, with the view and
    /// its deleted-block splicing enabled.
    fn diff_editor(base: &str, text: &str) -> EditorState {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let mut tb = TextBuffer::with_text(BufferId::new(0), text);
        tb.diff_map = Some(DiffMap::from_structural_changes(
            structural_diff::diff(base, text),
            Arc::new(base.to_string()),
            text,
        ));
        let shared = Arc::new(RwLock::new(tb));
        let mut editor = EditorState::new(BufferId::new(0), shared, executor, crate::test_notify());
        editor.set_diff_view(true);
        editor
    }

    /// A diff-view editor over `text` with a hand-built diff map, for hunk shapes
    /// the structural differ will not synthesize from plain text (e.g. moves).
    fn diff_editor_with_map(text: &str, dm: DiffMap) -> EditorState {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let mut tb = TextBuffer::with_text(BufferId::new(0), text);
        tb.diff_map = Some(dm);
        let shared = Arc::new(RwLock::new(tb));
        let mut editor = EditorState::new(BufferId::new(0), shared, executor, crate::test_notify());
        editor.set_diff_view(true);
        editor
    }

    /// A diff-view editor over the lines `1` to `20`, with one refined
    /// `Modified` hunk over the rows `live` and folded over `fold`.
    ///
    /// The hunk's base holds `removed` lines named `b<n>` from the hunk's first
    /// line on, and its first and last rows are the marked runs. The rows above
    /// and below the hunk match the base.
    fn folded_hunk_editor(
        live: std::ops::Range<u32>,
        removed: u32,
        fold: std::ops::Range<Point>,
    ) -> EditorState {
        let text: String = (1..=20).map(|n| format!("{n}\n")).collect();
        let head: String = (1..=live.start).map(|n| format!("{n}\n")).collect();
        let lines: String = (live.start + 1..=live.start + removed)
            .map(|n| format!("b{n}\n"))
            .collect();
        let tail: String = (live.end + 1..=20).map(|n| format!("{n}\n")).collect();
        let dm = DiffMap::from_hunks(
            [DiffHunk {
                status: DiffHunkStatus::Modified,
                unstaged_lines: std::iter::once(live.clone()).collect(),
                marked_rows: vec![live.start..live.start + 1, live.end - 1..live.end],
                buffer_start_line: live.start,
                buffer_line_range: live,
                base_byte_range: head.len()..head.len() + lines.len() - 1,
                anchor_range: None,
                token_detail: Some(Arc::new(TokenDetail {
                    buffer_spans: Vec::new(),
                    base_spans: Vec::new(),
                })),
            }],
            Some(Arc::new(format!("{head}{lines}{tail}"))),
        );
        let mut editor = diff_editor_with_map(&text, dm);
        editor.display_map.fold(vec![fold]);
        editor
    }

    /// A diff-view editor where line `b` moved below `d`, in the shape the tree
    /// pass reports a move. A seam at row 1 still holds `b` in its base range,
    /// and a moved-to hunk covers row 3.
    fn moved_line_editor() -> EditorState {
        let moved = |start: u32, end: u32| DiffHunk {
            status: DiffHunkStatus::Moved,
            buffer_start_line: start,
            buffer_line_range: start..end,
            base_byte_range: 2..3,
            anchor_range: None,
            token_detail: None,
            unstaged_lines: Vec::new(),
            marked_rows: Vec::new(),
        };
        let dm = DiffMap::from_hunks(
            [moved(1, 1), moved(3, 4)],
            Some(Arc::new("a\nb\nc\nd\ne\n".to_string())),
        );
        diff_editor_with_map("a\nc\nd\nb\ne\n", dm)
    }

    /// The tints [`rgb_diff_theme`] resolves to, for the row painters that take
    /// them and for the status colors a tinted char lands on.
    fn rgb_tints() -> DiffTints {
        resolve_diff_tints(&rgb_diff_theme()).expect("an rgb theme resolves its tints")
    }

    /// A minimal theme whose diff colors resolve to RGB, so the change washes
    /// engage when rendering a hand-built diff editor off the harness.
    fn rgb_diff_theme() -> Theme {
        let src = r##"theme rgbtest {
            diff.context.fg  = "#808080";
            diff.added.fg    = "#00ff00";
            diff.modified.fg = "#ffff00";
            diff.deleted.fg  = "#ff0000";
            diff.moved.fg    = "#0000ff";
            diff.staged.fg   = "#00ffff";
            diff.unstaged.fg = "#ff00ff";
            diff.approved.fg = "#ff8000";
            diff.unapproved.fg = "#8000ff";
            ui.background.bg = "#282c34";
        }"##;
        let (config, _) = stoat_config::parse(src);
        Theme::from_config(&config.expect("theme config parses"), "rgbtest")
            .expect("rgb theme builds")
    }

    fn diff_editor_staged(base: &str, index: &str, text: &str) -> EditorState {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let mut tb = TextBuffer::with_text(BufferId::new(0), text);
        let index_changed: Vec<std::ops::Range<u32>> = DiffMap::from_structural_changes(
            structural_diff::diff(index, text),
            Arc::new(index.to_string()),
            text,
        )
        .hunks_in_range(0..u32::MAX)
        .iter()
        .map(|h| h.buffer_line_range.clone())
        .collect();
        tb.diff_map = Some(DiffMap::from_structural_changes_staged(
            structural_diff::diff(base, text),
            Arc::new(base.to_string()),
            text,
            &index_changed,
        ));
        let shared = Arc::new(RwLock::new(tb));
        let mut editor = EditorState::new(BufferId::new(0), shared, executor, crate::test_notify());
        editor.set_diff_view(true);
        editor
    }

    #[test]
    fn diff_view_marks_staged_and_unstaged_hunks_in_the_status_column() {
        use crate::theme::scope as sc;

        // HEAD a/b/c/d; buffer changes line 1 (B) and line 3 (D); the index
        // holds only the line-1 change, so line 1 is staged, line 3 is not.
        let mut editor = diff_editor_staged("a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nc\nD\n");
        let area = Rect::new(0, 0, 120, 8);
        let mut buf = Buffer::empty(area);
        let theme = rgb_diff_theme();
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &theme,
            &mut buf,
            true,
            None,
            None,
            true,
            DiffDials::shipped(),
            None,
        );

        // The right status column follows the right number field, with its change
        // bar first and its staged bar after.
        let change_col = wide_columns().status_right_x;
        let staged_col = change_col + 1;
        let staged_fg = theme.get(sc::DIFF_STAGED).fg.expect("staged fg");
        let unstaged_fg = theme.get(sc::DIFF_UNSTAGED).fg.expect("unstaged fg");

        let change_glyphs: String = (0..area.height)
            .map(|y| buf[(change_col, y)].symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(
            change_glyphs.contains('▎'),
            "changed rows show the change bar instead of +/-: {change_glyphs:?}"
        );

        let staged_colors: Vec<Color> = (0..area.height)
            .filter(|&y| buf[(staged_col, y)].symbol() == "▎")
            .map(|y| buf[(staged_col, y)].fg)
            .collect();
        assert!(
            staged_colors.contains(&staged_fg),
            "the staged hunk's bar uses the staged color: {staged_colors:?}"
        );
        assert!(
            staged_colors.contains(&unstaged_fg),
            "the unstaged hunk's bar uses the unstaged color: {staged_colors:?}"
        );
    }

    /// A diff-view editor over `text` against `base`, with the changes `base`
    /// holds over `from` as the map's staged marks.
    fn diff_editor_with_staged_marks(from: &str, base: &str, text: &str) -> EditorState {
        let mut dm = DiffMap::from_structural_changes(
            structural_diff::diff(base, text),
            Arc::new(base.to_string()),
            text,
        );
        dm.set_staged_marks(diff_map::staged_marks(from, base));
        diff_editor_with_map(text, dm)
    }

    /// Each row's left change cell, left staged cell, right change cell, and
    /// right staged cell as glyph and color, with `editor` painted 120 columns
    /// wide under [`rgb_diff_theme`].
    fn status_cells(editor: &mut EditorState) -> Vec<[(String, Color); 4]> {
        let area = Rect::new(0, 0, 120, 8);
        let digits = diff_gutter_digits(&editor.display_map.snapshot());
        let mut buf = Buffer::empty(area);
        render_diff_view(
            editor,
            area,
            Style::default(),
            &rgb_diff_theme(),
            &mut buf,
            true,
            None,
            None,
            true,
            DiffDials::shipped(),
            None,
        );

        let columns = DiffColumns::compute(area, DiffLayout::diff_view(digits, false));
        let cell = |x: u16, y: u16| (buf[(x, y)].symbol().to_string(), buf[(x, y)].fg);
        (0..area.height)
            .map(|y| {
                [
                    cell(columns.status_left_x, y),
                    cell(columns.status_left_x + 1, y),
                    cell(columns.status_right_x, y),
                    cell(columns.status_right_x + 1, y),
                ]
            })
            .collect()
    }

    /// A status glyph in `scope`'s color under [`rgb_diff_theme`].
    fn glyph(mark: &str, scope: &str) -> (String, Color) {
        let color = rgb_diff_theme().get(scope).fg.expect("the theme sets it");
        (mark.to_string(), color)
    }

    /// The index changed line 1 from HEAD and the buffer changed line 3 from
    /// the index. The left column marks the first beside its base line, and
    /// the second stays an unstaged removal with no staged mark.
    #[test]
    fn diff_view_left_column_marks_what_the_index_staged() {
        let cells = status_cells(&mut diff_editor_with_staged_marks(
            "a\nb\nc\nd\n",
            "a\nB\nc\nd\n",
            "a\nB\nc\nD\n",
        ));

        assert_eq!(
            cells[1][..2],
            [glyph("▎", s::DIFF_MODIFIED), glyph("▎", s::DIFF_STAGED)],
            "base line 1 takes the modified bar and the staged bar: {cells:?}",
        );
        assert_eq!(
            cells[3],
            [
                glyph("▎", s::DIFF_DELETED),
                glyph("▎", s::DIFF_UNSTAGED),
                glyph("▎", s::DIFF_MODIFIED),
                glyph("▎", s::DIFF_UNSTAGED),
            ],
            "the removed d keeps the removal's bars beside the live D's: {cells:?}",
        );
    }

    /// A staged deletion leaves no base line of its own, so it marks the base
    /// line after the removed text the way the gutter marks a deletion.
    #[test]
    fn diff_view_left_column_marks_a_staged_deletion_as_a_seam() {
        let cells = status_cells(&mut diff_editor_with_staged_marks(
            "a\nx\nb\n",
            "a\nb\n",
            "a\nb\n",
        ));
        assert_eq!(
            cells[1][..2],
            [glyph("▔", s::DIFF_DELETED), glyph("▎", s::DIFF_STAGED)],
            "the line after the removed text takes the deletion mark: {cells:?}",
        );
    }

    /// Under a range review both columns' state bars take the approval scopes,
    /// in the ASCII cells and in the rich bars alike.
    #[test]
    fn diff_view_state_bars_take_the_approval_scopes_under_a_review() {
        let (base, text) = ("a\nb\nc\nd\n", "a\nB\nc\nD\n");
        let dm = DiffMap::from_structural_changes_staged(
            structural_diff::diff(base, text),
            Arc::new(base.to_string()),
            text,
            &std::iter::once(3..4).collect::<Vec<_>>(),
        )
        .with_mark_kind(MarkKind::Approved);
        let mut editor = diff_editor_with_map(text, dm);

        let cells = status_cells(&mut editor);
        let row = |state| {
            [
                glyph("▎", s::DIFF_DELETED),
                glyph("▎", state),
                glyph("▎", s::DIFF_MODIFIED),
                glyph("▎", state),
            ]
        };
        assert_eq!(
            [cells[1].clone(), cells[3].clone()],
            [row(s::DIFF_APPROVED), row(s::DIFF_UNAPPROVED)],
            "row 1 reads approved and row 3 unapproved: {cells:?}",
        );

        let area = Rect::new(0, 0, 120, 8);
        let mut scene = ApcScene::new();
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &rgb_diff_theme(),
            &mut Buffer::empty(area),
            true,
            None,
            Some(&mut scene),
            true,
            DiffDials::shipped(),
            None,
        );
        let digits = diff_gutter_digits(&editor.display_map.snapshot());
        let columns = DiffColumns::compute(area, DiffLayout::diff_view(digits, true));
        let staged_x = columns.rich.expect("a rich layout").staged_x;
        let bytes = scene.buffer();
        let bars = |row: u16, scope: &str| {
            let color = style_rgb(rgb_diff_theme().get(scope).fg).expect("an rgb color");
            [columns.left_x, columns.right_x].map(|x| {
                let frame = encode_bar(&BarCommand {
                    x: (x * 16 + staged_x) as i16,
                    y: (row * 16) as i16,
                    width: 5,
                    height: 16,
                    color,
                });
                bytes
                    .windows(frame.len())
                    .any(|window| window == frame.as_slice())
            })
        };
        assert_eq!(
            [bars(1, s::DIFF_APPROVED), bars(3, s::DIFF_UNAPPROVED)],
            [[true, true], [true, true]],
            "each column's rich state bar takes the approval color",
        );
    }

    /// A pure removal leaves the right column blank where its lines were. The
    /// first row of the run marks the gap with a tick, and no other row does.
    #[test]
    fn a_pure_removal_ticks_the_right_column_once() {
        let cells = status_cells(&mut diff_editor("a\nx\ny\nb\n", "a\nb\n"));
        let right_symbols = |row: usize| [cells[row][2].0.as_str(), cells[row][3].0.as_str()];

        assert_eq!(
            cells[1][2..],
            [glyph("▔", s::DIFF_DELETED), glyph("▔", s::DIFF_DELETED)],
            "the first removed row ticks both right status cells: {cells:?}",
        );
        assert_eq!(
            [right_symbols(2), right_symbols(3)],
            [[" ", " "], [" ", " "]],
            "the second removed row and the row after the gap stay blank: {cells:?}",
        );
    }

    /// A removal beside a row that carries its own bar takes no tick, because
    /// that bar already marks the spot.
    #[test]
    fn a_removal_beside_a_barred_row_gets_no_tick() {
        let hunk = |status, end: u32, base_byte_range| DiffHunk {
            status,
            buffer_start_line: 1,
            buffer_line_range: 1..end,
            base_byte_range,
            anchor_range: None,
            token_detail: None,
            unstaged_lines: Vec::new(),
            marked_rows: Vec::new(),
        };
        let dm = DiffMap::from_hunks(
            [
                hunk(DiffHunkStatus::Deleted, 1, 2..4),
                hunk(DiffHunkStatus::Added, 2, 4..4),
            ],
            Some(Arc::new("a\nx\nc\n".to_string())),
        );
        let cells = status_cells(&mut diff_editor_with_map("a\nB\nc\n", dm));

        assert_eq!(
            [cells[1][2].0.as_str(), cells[1][3].0.as_str()],
            [" ", " "],
            "the removed x sits above the added B, so it takes no tick: {cells:?}",
        );
    }

    /// A base line the buffer then changed again is a removed row with a staged
    /// mark on it. The row is there for the removal, so it shows that status.
    #[test]
    fn diff_view_removed_row_keeps_its_status_over_a_staged_mark() {
        let cells = status_cells(&mut diff_editor_with_staged_marks(
            "a\nb\n", "a\nB\n", "a\nX\n",
        ));
        assert_eq!(
            cells[1][..2],
            [glyph("▎", s::DIFF_DELETED), glyph("▎", s::DIFF_UNSTAGED)],
            "the removed B takes the unstaged removal's bars: {cells:?}",
        );
    }

    /// A refined hunk narrows the change to its span rows, and the status bars
    /// follow. Without this a reindent bars every row it moved, which is the
    /// clutter the refinement exists to remove.
    #[test]
    fn a_refined_hunk_bars_only_its_marked_rows() {
        let cols = wide_columns();
        // One hunk over the whole block, refined to its middle row alone.
        let text = "a\nb\nc\nd\n";
        let mut editor = {
            let executor = Executor::new(Arc::new(TestScheduler::new()));
            let mut tb = TextBuffer::with_text(BufferId::new(0), text);
            tb.diff_map = Some(DiffMap::from_hunks(
                [DiffHunk {
                    status: DiffHunkStatus::Modified,
                    buffer_start_line: 0,
                    buffer_line_range: 0..4,
                    base_byte_range: 0..8,
                    anchor_range: None,
                    token_detail: None,
                    unstaged_lines: std::iter::once(0..4).collect(),
                    marked_rows: std::iter::once(2..3).collect(),
                }],
                Some(Arc::new(text.to_string())),
            ));
            let shared = Arc::new(RwLock::new(tb));
            let mut editor =
                EditorState::new(BufferId::new(0), shared, executor, crate::test_notify());
            editor.set_diff_view(true);
            editor
        };

        let area = Rect::new(0, 0, 120, 8);
        let mut buf = Buffer::empty(area);
        let theme = rgb_diff_theme();
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &theme,
            &mut buf,
            true,
            None,
            None,
            true,
            DiffDials::shipped(),
            None,
        );

        let change_col = cols.status_right_x;
        let barred: Vec<u16> = (0..area.height)
            .filter(|&y| buf[(change_col, y)].symbol() == "▎")
            .collect();
        // The diff view stacks base rows, so the screen row is found by what it
        // shows rather than assumed equal to the buffer row.
        let marked_screen_row = (0..area.height)
            .find(|&y| line_text(&buf, y, right_text(&cols)).trim() == "c")
            .expect("the marked buffer row is on screen");
        assert_eq!(
            barred,
            [marked_screen_row],
            "only the marked row bars, not every row the hunk spans"
        );
    }

    /// A base row of a refined hunk that no change span reaches paints as the
    /// mirror of an unchanged row, so a re-indented block's left column reads
    /// as context and only its marked row stands out.
    #[test]
    fn a_quiet_base_row_of_a_refined_hunk_paints_as_context() {
        let span = |byte_range| ChangeSpan {
            byte_range,
            kind: ChangeKind::Replaced,
            move_metadata: None,
            prose: ProseChange::None,
        };
        let dm = DiffMap::from_hunks(
            [DiffHunk {
                status: DiffHunkStatus::Modified,
                unstaged_lines: std::iter::once(0..4).collect(),
                marked_rows: std::iter::once(2..3).collect(),
                buffer_start_line: 0,
                buffer_line_range: 0..4,
                base_byte_range: 0..15,
                anchor_range: None,
                token_detail: Some(Arc::new(TokenDetail {
                    buffer_spans: vec![span(4..5)],
                    base_spans: vec![span(8..11)],
                })),
            }],
            Some(Arc::new("  a\n  b\n  c\n  d\ne\n".to_string())),
        );
        let mut editor = diff_editor_with_map("a\nb\nc\nd\ne\n", dm);
        let area = Rect::new(0, 0, 120, 8);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &rgb_diff_theme(),
            &mut buf,
            false,
            None,
            None,
            false,
            DiffDials::shipped(),
            None,
        );

        let text_x = two_digit_columns(area).left_text_x;
        let letter = |y: u16| {
            let x = (text_x..area.width)
                .find(|&x| buf[(x, y)].symbol() != " ")
                .expect("the row shows its base line");
            (buf[(x, y)].symbol(), buf[(x, y)].style())
        };
        let [a, b, c, d, e] = [0, 1, 2, 3, 4].map(letter);
        assert_eq!(
            ([a.0, b.0, c.0, d.0, e.0], [a.1, b.1, d.1], c.1 == e.1),
            (["a", "b", "c", "d", "e"], [e.1; 3], false),
            "the quiet rows paint as the mirror of e, and the marked row does not",
        );
    }

    #[test]
    fn diff_view_rich_gutter_emits_bars_and_suppresses_ascii() {
        fn contains(haystack: &[u8], needle: &[u8]) -> bool {
            haystack.windows(needle.len()).any(|w| w == needle)
        }

        let mut editor = diff_editor_staged("a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nc\nD\n");
        let area = Rect::new(0, 0, 120, 8);
        let theme = rgb_diff_theme();

        let mut rich_buf = Buffer::empty(area);
        let mut scene = ApcScene::new();
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &theme,
            &mut rich_buf,
            true,
            None,
            Some(&mut scene),
            true,
            DiffDials::shipped(),
            None,
        );

        assert!(
            !scene.buffer().is_empty(),
            "rich mode emits sub-cell components"
        );

        let scroll = editor.scroll_row;
        let total = editor.display_map.snapshot().line_count();
        let rows = ((area.height as u32).min(total.saturating_sub(scroll))) as u16;
        let dim = [128, 128, 128];
        let digits = diff_gutter_digits(&editor.display_map.snapshot());
        let cols = DiffColumns::compute(area, DiffLayout::diff_view(digits, true));
        let offsets = cols.rich.expect("a rich layout");

        // Each side's gutter separator and the mid divider paint as hairlines,
        // the separators at the rich gutter's offset and the divider centered
        // in its cell.
        let divider = cols.sep_x.expect("two columns") * 16 + 8;
        for x in [cols.left_x, cols.right_x]
            .map(|side| side * 16 + offsets.separator_x)
            .into_iter()
            .chain([divider])
        {
            let frame = encode_bar(&BarCommand {
                x: x as i16,
                y: 0,
                width: 1,
                height: rows * 16,
                color: dim,
            });
            assert!(
                contains(scene.buffer(), &frame),
                "separator hairline at sixteenth {x}"
            );
        }

        // A changed row emits a staged or unstaged bar in one of the two status
        // columns. Every row is searched rather than pinning the block layout.
        let staged = [[0, 255, 255], [255, 0, 255]];
        let staged_bar = (0..rows).any(|row| {
            [cols.left_x, cols.right_x].iter().any(|&side| {
                let x = side * 16 + offsets.staged_x;
                staged.iter().any(|&color| {
                    contains(
                        scene.buffer(),
                        &encode_bar(&BarCommand {
                            x: x as i16,
                            y: (row * 16) as i16,
                            width: 5,
                            height: 16,
                            color,
                        }),
                    )
                })
            })
        });
        assert!(staged_bar, "a changed row emits a staged/unstaged bar");

        let has = |b: &Buffer, sym: &str| {
            (0..area.height).any(|y| (0..area.width).any(|x| b[(x, y)].symbol() == sym))
        };
        assert!(
            !has(&rich_buf, "▎"),
            "rich mode paints no ASCII status glyphs"
        );
        assert!(!has(&rich_buf, "│"), "rich mode paints no ASCII separators");

        let mut ascii_buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &theme,
            &mut ascii_buf,
            true,
            None,
            None,
            true,
            DiffDials::shipped(),
            None,
        );
        assert!(has(&ascii_buf, "▎"), "the ASCII path paints status glyphs");
        assert!(has(&ascii_buf, "│"), "the ASCII path paints separators");
    }

    #[test]
    fn diff_rich_colors_fade_toward_the_background_with_the_dim_amount() {
        let theme = rgb_diff_theme();
        let full = resolve_diff_rich_colors(&theme, Style::default(), 0.0).expect("colors resolve");
        let faded =
            resolve_diff_rich_colors(&theme, Style::default(), 0.6).expect("colors resolve");

        assert_eq!(
            full.dim,
            [128, 128, 128],
            "no dim leaves the color unchanged"
        );
        assert_eq!(
            faded.dim,
            dim_rgb([128, 128, 128], full.bg, 0.6),
            "a pooled page fades the stored colors toward the background"
        );
        assert_eq!(faded.bg, full.bg, "the background itself is not faded");
    }

    #[test]
    fn diff_view_lays_out_base_left_buffer_right() {
        let cols = wide_columns();
        let mut editor = diff_editor("keep\nold\ntail\n", "keep\nnew\ntail\n");
        let area = Rect::new(0, 0, 120, 8);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &Theme::empty(),
            &mut buf,
            true,
            None,
            None,
            true,
            DiffDials::shipped(),
            None,
        );

        // Width 120 is wide enough for the two-column layout, with the left text
        // before the mid-divider at col 59 and the right text after the right
        // gutter.
        let left = |y| line_text(&buf, y, left_text(&cols));
        let right = |y| line_text(&buf, y, right_text(&cols));

        assert!(
            left(0).contains("keep") && right(0).contains("keep"),
            "row0 mirrors context: left={:?} right={:?}",
            left(0),
            right(0)
        );

        // The replaced line and its replacement share a row, which is what
        // makes the two sides read as aligned.
        assert!(
            left(1).contains("old") && right(1).contains("new"),
            "row1 pairs the base line with the one that replaced it: left={:?} right={:?}",
            left(1),
            right(1)
        );

        assert!(
            left(2).contains("tail") && right(2).contains("tail"),
            "row2 context mirrors both sides: left={:?} right={:?}",
            left(2),
            right(2)
        );

        assert_eq!(
            (left(3).trim(), right(3).trim()),
            ("", ""),
            "and the file ends there, with no row left over for the change",
        );

        assert_eq!(
            buf[(59, 0)].symbol(),
            "│",
            "the two columns are split by a separator"
        );
        assert_eq!(
            [wide_columns().left_sep_x, wide_columns().right_sep_x]
                .map(|x| buf[(x.expect("a gutter rule"), 0)].symbol()),
            ["│", "│"],
            "each side carries a gutter/code separator after its status column"
        );
    }

    /// A move leaves the left column no row for the line it took away, so the
    /// numbers jump past that line and every row below reads its own base line.
    /// The moved-to row paints nothing on the left.
    #[test]
    fn diff_view_numbers_the_left_column_past_a_moved_away_line() {
        let cols = wide_columns();
        let mut editor = moved_line_editor();
        let area = Rect::new(0, 0, 120, 6);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &Theme::empty(),
            &mut buf,
            true,
            None,
            None,
            true,
            DiffDials::shipped(),
            None,
        );

        let left_of = |right: &str| {
            let y = (0..area.height)
                .find(|&y| line_text(&buf, y, right_text(&cols)).trim() == right)
                .unwrap_or_else(|| panic!("{right:?} rendered on the right"));
            (
                line_text(&buf, y, left_num(&cols)).trim().to_string(),
                line_text(&buf, y, left_text(&cols)).trim().to_string(),
            )
        };
        assert_eq!(
            ["a", "c", "d", "b", "e"].map(left_of),
            [("1", "a"), ("3", "c"), ("4", "d"), ("", ""), ("5", "e")]
                .map(|(number, text)| (number.to_string(), text.to_string())),
            "the left numbers skip the moved-away line 2, and the moved-to row is blank"
        );
    }

    /// The rows a length difference costs belong at the end of the hunk, not
    /// scattered through it, so the base rows pair as far as they can and only
    /// the excess takes rows of its own.
    #[test]
    fn a_longer_base_hunk_pairs_then_trails_its_excess() {
        let cols = wide_columns();
        let mut editor = diff_editor("keep\nold1\nold2\nold3\ntail\n", "keep\nnew1\ntail\n");
        let area = Rect::new(0, 0, 120, 8);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &Theme::empty(),
            &mut buf,
            true,
            None,
            None,
            true,
            DiffDials::shipped(),
            None,
        );

        let left = |y| line_text(&buf, y, left_text(&cols));
        let right = |y| line_text(&buf, y, right_text(&cols));

        assert!(
            left(1).contains("old1") && right(1).contains("new1"),
            "row1 pairs the first base row with the live one: left={:?} right={:?}",
            left(1),
            right(1)
        );
        assert!(
            left(2).contains("old2") && right(2).trim().is_empty(),
            "row2 trails the first excess base row: left={:?} right={:?}",
            left(2),
            right(2)
        );
        assert!(
            left(3).contains("old3") && right(3).trim().is_empty(),
            "row3 the second: left={:?} right={:?}",
            left(3),
            right(3)
        );
        assert!(
            left(4).contains("tail") && right(4).contains("tail"),
            "row4 is context again: left={:?} right={:?}",
            left(4),
            right(4)
        );

        // The left numbers count the base file straight down the column, which
        // is what a reader checks an alignment against. The status glyphs share
        // the gutter, so only the number itself is read.
        let number = |y| {
            line_text(&buf, y, left_num(&cols))
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_owned()
        };
        assert_eq!(
            (0..5).map(number).collect::<Vec<_>>(),
            ["1", "2", "3", "4", "5"],
            "the base line numbers read 1..5 down the left column",
        );
    }

    /// The other way round: more live rows than base rows leaves the left column
    /// blank past the pairing, which the paint gives without a block.
    #[test]
    fn a_longer_live_hunk_blanks_its_trailing_left_rows() {
        let cols = wide_columns();
        let mut editor = diff_editor("keep\nold\ntail\n", "keep\nnew1\nnew2\ntail\n");
        let area = Rect::new(0, 0, 120, 8);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &Theme::empty(),
            &mut buf,
            true,
            None,
            None,
            true,
            DiffDials::shipped(),
            None,
        );

        let left = |y| line_text(&buf, y, left_text(&cols));
        let right = |y| line_text(&buf, y, right_text(&cols));

        assert!(
            left(1).contains("old") && right(1).contains("new1"),
            "row1 pairs the one base row with the first live one: left={:?} right={:?}",
            left(1),
            right(1)
        );
        assert!(
            left(2).trim().is_empty() && right(2).contains("new2"),
            "row2 has no base row to show: left={:?} right={:?}",
            left(2),
            right(2)
        );
        assert!(
            left(3).contains("tail") && right(3).contains("tail"),
            "row3 is context again: left={:?} right={:?}",
            left(3),
            right(3)
        );

        assert_eq!(
            line_text(&buf, 3, left_num(&cols)).trim(),
            "3",
            "and the base file has only three lines, so the context is its third",
        );
    }

    #[test]
    fn narrow_diff_renders_a_unified_single_column() {
        let mut editor = diff_editor("keep\nold\ntail\n", "keep\nnew\ntail\n");
        let area = Rect::new(0, 0, 40, 8);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &Theme::empty(),
            &mut buf,
            true,
            None,
            None,
            true,
            DiffDials::shipped(),
            None,
        );

        // The gutter/code separator is painted at col 5, past the blank mark
        // cell, the two-digit number and the two status cells, but a unified
        // view has no two-column mid-divider, so no rule appears in the text
        // region.
        let sep_col = area.x + 5;
        assert!(
            (0..area.height).any(|y| buf[(sep_col, y)].symbol() == "│"),
            "the gutter/code separator is painted in the unified view"
        );
        for y in 0..area.height {
            for x in (area.x + 6)..area.width {
                assert_ne!(
                    buf[(x, y)].symbol(),
                    "│",
                    "a unified diff paints no mid-divider past the gutter"
                );
            }
        }

        // The single text column sits past the number, status, and separator
        // gutter, and both sides render into it.
        let col = editor
            .text_rect
            .expect("the render records the text rect")
            .x;
        assert_eq!(col, area.x + 6, "unified text starts past the one gutter");

        let renders = |needle: &str| {
            (0..area.height).any(|y| {
                let row: String = (col..area.width)
                    .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect();
                row.contains(needle)
            })
        };
        assert!(renders("old"), "the deleted base line renders inline");
        assert!(renders("new"), "the added buffer line renders inline");
    }

    #[test]
    fn typing_in_diff_view_edits_the_real_buffer() {
        use crate::{action_handlers::focused_editor_mut, test_harness::TestHarness};

        let mut h = TestHarness::with_size(40, 8);
        let path = h.write_file("a.txt", "abc\n");
        h.open_file(&path);
        focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .set_diff_view(true);

        h.type_keys("i");
        h.type_text("X");

        let text = focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .display_map
            .snapshot()
            .buffer_snapshot()
            .rope()
            .to_string();
        assert!(
            text.starts_with('X'),
            "inserting in the diff view lands in the real buffer: {text:?}"
        );
    }

    #[test]
    fn diff_view_right_column_carries_syntax_colors() {
        use crate::{action_handlers::focused_editor_mut, test_harness::TestHarness};

        let mut h = TestHarness::with_size(120, 10);
        let path = h.write_file("a.rs", "fn main() {}\n");
        h.open_file(&path);
        h.stoat.drive_background();
        focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .set_diff_view(true);
        h.snapshot();

        // A syntax-highlighted right column paints more than one foreground color
        // across the row's tokens.
        let buf = h.rendered_buffer();
        let mut colors = std::collections::HashSet::new();
        for x in right_text(&harness_columns(&h)) {
            let cell = &buf[(x, 0)];
            if cell.symbol().trim().is_empty() {
                continue;
            }
            colors.insert(format!("{:?}", cell.style().fg));
        }
        assert!(
            colors.len() >= 2,
            "the right column is syntax highlighted with distinct token colors: {colors:?}"
        );
    }

    /// `editor` rendered unfocused and 120 wide on [`rgb_diff_theme`] over a
    /// pane `fallback`, with softening off so no soften or lift moves a color.
    fn render_over_fallback(editor: &mut EditorState, fallback: Style) -> Buffer {
        let area = Rect::new(0, 0, 120, 8);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            editor,
            area,
            fallback,
            &rgb_diff_theme(),
            &mut buf,
            false,
            None,
            None,
            true,
            DiffDials {
                soften_scale: 0.0,
                ..DiffDials::shipped()
            },
            None,
        );
        buf
    }

    /// The fg of the first right-column text cell on the row whose right column
    /// starts with `needle`.
    fn right_fg(buf: &Buffer, needle: &str) -> Option<Color> {
        let rx = two_digit_columns(buf.area).right_text_x;
        buf[(rx, row_holding(buf, rx..buf.area.width, needle))]
            .style()
            .fg
    }

    /// With syntax off, or in a file with no grammar, no token colors a live
    /// row. Its status color then says the row changed, as the removed color
    /// does on the base side.
    #[test]
    fn a_changed_live_row_falls_back_to_its_status_color() {
        let mut editor = diff_editor("a\nb\n", "a\nB\nc\n");
        let buf = render_over_fallback(&mut editor, Style::default().fg(Color::Rgb(1, 2, 3)));
        assert_eq!(
            ["a", "B", "c"].map(|needle| right_fg(&buf, needle)),
            [
                Some(Color::Rgb(1, 2, 3)),
                Some(Color::Rgb(0, 255, 0)),
                Some(Color::Rgb(0, 255, 0)),
            ],
            "the unchanged row keeps the pane color, and the modified and added \
             rows take the added color",
        );
    }

    /// A moved line keeps its own color on the live side, so with no token
    /// color it reads as a move and not as new text.
    #[test]
    fn a_moved_live_row_falls_back_to_the_moved_color() {
        let mut editor = moved_line_editor();
        let buf = render_over_fallback(&mut editor, Style::default().fg(Color::Rgb(1, 2, 3)));
        assert_eq!(
            ["b", "e"].map(|needle| right_fg(&buf, needle)),
            [Some(Color::Rgb(0, 0, 255)), Some(Color::Rgb(1, 2, 3))],
            "the moved row takes the moved color, and the unchanged row keeps \
             the pane color",
        );
    }

    /// A moved span inside an added row keeps its own color, so with no token
    /// color the relocated chars read as a move inside the new line.
    #[test]
    fn a_moved_span_in_an_added_row_falls_back_to_the_moved_color() {
        let dm = DiffMap::from_hunks(
            [DiffHunk {
                status: DiffHunkStatus::Added,
                unstaged_lines: Vec::new(),
                marked_rows: Vec::new(),
                buffer_start_line: 1,
                buffer_line_range: 1..2,
                base_byte_range: 0..0,
                anchor_range: None,
                token_detail: Some(Arc::new(TokenDetail {
                    buffer_spans: vec![ChangeSpan {
                        byte_range: 3..5,
                        kind: ChangeKind::Moved,
                        move_metadata: None,
                        prose: ProseChange::None,
                    }],
                    base_spans: Vec::new(),
                })),
            }],
            None,
        );
        let mut editor = diff_editor_with_map("aa\nbbxx\ncc\n", dm);
        let buf = render_over_fallback(&mut editor, Style::default().fg(Color::Rgb(1, 2, 3)));
        let rx = editor
            .text_rect
            .expect("the render records the text rect")
            .x;
        let y = row_holding(&buf, rx..buf.area.width, "bbxx");
        assert_eq!(
            (rx..rx + 4)
                .map(|x| buf[(x, y)].style().fg)
                .collect::<Vec<_>>(),
            [
                Some(Color::Rgb(0, 0, 255)),
                Some(Color::Rgb(0, 0, 255)),
                Some(Color::Rgb(0, 255, 0)),
                Some(Color::Rgb(0, 255, 0)),
            ],
            "the moved chars take the moved color and the rest of the added row \
             keeps the added color",
        );
    }

    /// A moved span inside a removed base line keeps its own color, so with no
    /// token color the relocated chars read as a move inside the removed line.
    #[test]
    fn a_moved_span_in_a_removed_base_line_falls_back_to_the_moved_color() {
        let dm = DiffMap::from_hunks(
            [DiffHunk {
                status: DiffHunkStatus::Deleted,
                unstaged_lines: Vec::new(),
                marked_rows: Vec::new(),
                buffer_start_line: 1,
                buffer_line_range: 1..1,
                base_byte_range: 3..8,
                anchor_range: None,
                token_detail: Some(Arc::new(TokenDetail {
                    buffer_spans: Vec::new(),
                    base_spans: vec![ChangeSpan {
                        byte_range: 3..5,
                        kind: ChangeKind::Moved,
                        move_metadata: None,
                        prose: ProseChange::None,
                    }],
                })),
            }],
            Some(Arc::new("aa\nbbxx\ncc\n".to_string())),
        );
        let mut editor = diff_editor_with_map("aa\ncc\n", dm);
        let buf = render_over_fallback(&mut editor, Style::default().fg(Color::Rgb(1, 2, 3)));
        let cols = two_digit_columns(buf.area);
        let y = row_holding(&buf, cols.left_text_x..cols.right_text_x, "bbxx");
        let lx = cols.left_text_x;
        assert_eq!(
            (lx..lx + 4)
                .map(|x| buf[(x, y)].style().fg)
                .collect::<Vec<_>>(),
            [
                Some(Color::Rgb(0, 0, 255)),
                Some(Color::Rgb(0, 0, 255)),
                Some(Color::Rgb(255, 0, 0)),
                Some(Color::Rgb(255, 0, 0)),
            ],
            "the moved chars take the moved color and the rest of the removed line \
             keeps the deleted color",
        );
    }

    #[test]
    fn diff_view_left_column_carries_base_token_colors() {
        use crate::{action_handlers::focused_editor_mut, test_harness::TestHarness};

        let mut h = TestHarness::with_size(120, 10);
        // The base carries rust keywords. The buffer differs, so the base line
        // renders as a deleted block in the left column.
        h.stage_review_scenario("/repo", &[("a.rs", "fn main() {}\n", "fn other() {}\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(std::path::Path::new("/repo/a.rs"));
        h.settle_diff_jobs();
        focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .set_diff_view(true);
        h.snapshot();

        let buf = h.rendered_buffer();
        let left = left_text(&harness_columns(&h));
        let mut colors = std::collections::HashSet::new();
        for y in 0..buf.area.height {
            for x in left.clone() {
                let cell = &buf[(x, y)];
                if cell.symbol().trim().is_empty() {
                    continue;
                }
                colors.insert(format!("{:?}", cell.style().fg));
            }
        }
        assert!(
            colors.len() >= 2,
            "the base column carries token colors plus the text fallback: {colors:?}"
        );
    }

    /// A width-120 diff-view harness over one `.rs` file diffed `base` ->
    /// `buffer`, rendered once. The default theme resolves diff colors to RGB, so
    /// the change washes engage.
    ///
    /// Width 120 stays above the two-column threshold, with the separator at col
    /// 59. [`harness_columns`] gives the columns either side of it.
    fn diff_harness(base: &str, buffer: &str) -> crate::test_harness::TestHarness {
        diff_harness_sized(120, base, buffer)
    }

    /// [`diff_harness`] at `width` columns. Below [`DIFF_TWO_COLUMN_MIN`] both
    /// sides share one column.
    fn diff_harness_sized(
        width: u16,
        base: &str,
        buffer: &str,
    ) -> crate::test_harness::TestHarness {
        use crate::{action_handlers::focused_editor_mut, test_harness::TestHarness};

        let mut h = TestHarness::with_size(width, 10);
        // The wash scans below address fixed side-by-side columns, so the pane must
        // span the full width. The single-minimap strip would reserve the right
        // edge and shift the columns.
        h.stoat.minimap_override = Some(false);
        // The row checks below read each live number as the row's own line.
        // Relative numbering counts the numbers from the cursor instead.
        h.stoat.settings.editor_line_numbers = Some(LineNumbers::Absolute);
        h.stage_review_scenario("/repo", &[("a.rs", base, buffer)]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(std::path::Path::new("/repo/a.rs"));
        h.settle_diff_jobs();
        focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .set_diff_view(true);
        h.snapshot();
        h
    }

    /// The glyphs of row `y` across `cols`, for locating a rendered line.
    fn line_text(buf: &Buffer, y: u16, cols: std::ops::Range<u16>) -> String {
        cols.map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    /// The first row whose glyphs across `cols` start with `needle`.
    fn row_holding(buf: &Buffer, cols: std::ops::Range<u16>, needle: &str) -> u16 {
        (0..buf.area.height)
            .find(|&y| line_text(buf, y, cols.clone()).starts_with(needle))
            .unwrap_or_else(|| panic!("no row across {cols:?} starts with {needle:?}"))
    }

    #[test]
    fn the_sides_bar_yields_to_a_cursor_on_the_top_row() {
        let mut h = diff_harness("a\nb\nc\nold\n", "a\nb\nc\nnew\n");
        assert_eq!(
            top_row_in_both_frames(&mut h).map(|row| row.contains("working tree")),
            [false, false],
            "the line under edit stays clear in both frames",
        );
    }

    #[test]
    fn the_diff_view_names_both_sides_on_its_top_row() {
        let mut h = diff_harness("a\nb\nc\nold\n", "a\nb\nc\nnew\n");
        h.type_keys("j");
        let head = top_row_in_both_frames(&mut h);

        h.stoat
            .active_workspace_mut()
            .set_worktree_base(WorktreeBase::Index);
        h.settle_diff_jobs();
        let index = top_row_in_both_frames(&mut h);

        assert_eq!(
            (
                head.each_ref()
                    .map(|row| row.contains("HEAD → working tree")),
                index
                    .each_ref()
                    .map(|row| row.contains("index → working tree")),
            ),
            ([true, true], [true, true]),
            "{head:?} then {index:?}",
        );
    }

    #[test]
    fn the_sides_bar_centers_on_the_divider() {
        let mut h = diff_harness("a\nb\nc\nold\n", "a\nb\nc\nnew\n");
        h.type_keys("j");
        assert_eq!(
            top_row_in_both_frames(&mut h).map(|row| row.chars().position(|c| c == '→')),
            [Some(59), Some(59)],
            "the arrow sits on the separator column",
        );
    }

    #[test]
    fn a_plain_pane_carries_no_sides_bar() {
        let mut h = diff_harness("a\nb\nc\nold\n", "a\nb\nc\nnew\n");
        crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .set_diff_view(false);
        h.type_keys("j");
        h.snapshot();
        assert!(
            !h.rendered_text().contains("working tree"),
            "{}",
            h.rendered_text()
        );
    }

    /// Row 0 of the stoatty frame and of the fallback frame, in that order.
    fn top_row_in_both_frames(h: &mut crate::test_harness::TestHarness) -> [String; 2] {
        [true, false].map(|stoatty| {
            h.stoat.stoatty = stoatty;
            h.snapshot();
            line_text(h.rendered_buffer(), 0, 0..120)
        })
    }

    /// `a` a's, a space, then `b` b's: a line that wraps after the space when
    /// the a's and the space fit the width and the whole line does not.
    fn long_line(a: usize, b: usize) -> String {
        format!("{} {}", "a".repeat(a), "b".repeat(b))
    }

    /// Row `y`'s trimmed glyphs across each of `cols`.
    fn cells<const N: usize>(buf: &Buffer, y: u16, cols: [std::ops::Range<u16>; N]) -> [String; N] {
        cols.map(|cols| line_text(buf, y, cols).trim().to_string())
    }

    /// At width 120 the right column wraps at 51, the narrower side's width,
    /// and the tail's row takes no line number.
    #[test]
    fn a_long_live_line_wraps_into_the_right_column() {
        let h = diff_harness("keep\n", &format!("keep\n{}\n", long_line(45, 44)));
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);
        let y = row_holding(buf, right_text(&cols), &"a".repeat(45));
        assert_eq!(
            [y, y + 1].map(|y| cells(buf, y, [right_num(&cols), right_text(&cols)])),
            [
                ["2".to_string(), "a".repeat(45)],
                [String::new(), "b".repeat(44)],
            ],
        );
    }

    /// The wrap fit sizes the gutters as the paint does, so the widest line the
    /// wrap keeps whole keeps one row, whichever gutter the harness paints. The
    /// wrap stops one cell short of the narrower text column.
    #[test]
    fn a_line_one_cell_short_of_the_text_column_keeps_one_row() {
        let width = {
            let cols = harness_columns(&diff_harness("old\n", "new\n"));
            cols.left_content_w.min(cols.right_content_w) - 1
        };
        let line = "a".repeat(width);
        let h = diff_harness("keep\ntail\n", &format!("keep\n{line}\ntail\n"));
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);
        let y = row_holding(buf, right_text(&cols), &line);
        assert_eq!(
            cells(buf, y + 1, [right_text(&cols)]),
            ["tail".to_string()],
            "the line's next row is the next line, not a continuation"
        );
    }

    #[test]
    fn an_unchanged_long_line_mirrors_its_wrapped_segments() {
        let long = long_line(45, 44);
        let h = diff_harness(&format!("{long}\nold\n"), &format!("{long}\nnew\n"));
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);
        let y = row_holding(buf, right_text(&cols), &"a".repeat(45));
        assert_eq!(
            [y, y + 1].map(|y| cells(buf, y, [left_text(&cols), right_text(&cols)])),
            [
                ["a".repeat(45), "a".repeat(45)],
                ["b".repeat(44), "b".repeat(44)],
            ],
            "each left row holds the segment the right row beside it holds",
        );
    }

    #[test]
    fn a_wrapped_row_keeps_the_base_numbers_in_step() {
        let long = long_line(45, 44);
        let h = diff_harness(&format!("{long}\nold\n"), &format!("{long}\nnew\n"));
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);
        let y = row_holding(buf, right_text(&cols), &"a".repeat(45));
        assert_eq!(
            (y..y + 3)
                .map(|y| cells(buf, y, [left_num(&cols), left_text(&cols)]))
                .collect::<Vec<_>>(),
            [
                ["1".to_string(), "a".repeat(45)],
                [String::new(), "b".repeat(44)],
                ["2".to_string(), "old".to_string()],
            ],
            "the continuation takes no base line, so the paired row reads line 2",
        );
    }

    /// The underline dial marks every span cell on any theme, which makes the
    /// span's cells readable off the grid.
    #[test]
    fn a_change_span_on_a_continuation_row_marks_only_its_own_cells() {
        // 44 a's wrap the line before `TWO`, whichever gutter the harness
        // paints, so the changed word starts the continuation row.
        let a = "a".repeat(44);
        let mut h = diff_harness(
            &format!("// {a} one two three\n"),
            &format!("// {a} one TWO three\n"),
        );
        h.stoat.diff_underline = true;
        h.snapshot();
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);
        let y = row_holding(buf, right_text(&cols), "TWO three");
        let underlined = |y| {
            (right_text(&cols))
                .filter(|&x| buf[(x, y)].modifier.contains(Modifier::UNDERLINED))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            [underlined(y - 1), underlined(y)],
            [vec![], (cols.right_text_x..cols.right_text_x + 3).collect()]
        );
    }

    /// The right status column is the one [`wide_columns`] gives at width 120.
    #[test]
    fn a_continuation_row_takes_no_status_bar() {
        let long = long_line(45, 44);
        let text = format!("keep\n{long}\n");
        let mut editor = diff_editor_staged("keep\n", &text, &text);
        editor.display_map.set_wrap_width(Some(51));
        let area = Rect::new(0, 0, 120, 8);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &rgb_diff_theme(),
            &mut buf,
            true,
            None,
            None,
            true,
            DiffDials::shipped(),
            None,
        );

        assert_eq!(
            (0..3)
                .map(|y| buf[(wide_columns().status_right_x, y)].symbol())
                .collect::<Vec<_>>(),
            [" ", "▎", " "],
            "the added line's first row carries the bar and its continuation none",
        );
    }

    /// A removed line's block takes the bar on its first row alone, as a live
    /// line does. The left status column sits at x 5 at width 120.
    #[test]
    fn a_block_continuation_takes_no_status_bar() {
        let base = format!("keep\n{}\n", long_line(45, 44));
        let mut editor = diff_editor_staged(&base, &base, "keep\n");
        editor.display_map.set_wrap_width(Some(51));
        let area = Rect::new(0, 0, 120, 8);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &rgb_diff_theme(),
            &mut buf,
            true,
            None,
            None,
            true,
            DiffDials::shipped(),
            None,
        );

        assert_eq!(
            (0..3)
                .map(|y| buf[(wide_columns().status_left_x, y)].symbol())
                .collect::<Vec<_>>(),
            [" ", "▎", " "],
        );
    }

    #[test]
    fn the_review_cursor_lands_on_the_wrapped_segment() {
        let long = long_line(45, 44);
        let mut editor = diff_editor(&format!("{long}\n"), &format!("{long}\n"));
        editor.display_map.set_wrap_width(Some(51));
        crate::test_harness::editor::place_cursor(&mut editor, 0, 50);
        let area = Rect::new(0, 0, 120, 8);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &Theme::empty(),
            &mut buf,
            true,
            None,
            Some(&mut ApcScene::new()),
            true,
            DiffDials::shipped(),
            None,
        );
        assert_eq!(
            editor.cursor_screen_cell,
            Some((wide_columns().right_text_x + 4, 1)),
            "byte 50 is the fifth cell of the continuation row",
        );
    }

    #[test]
    fn a_cursor_paints_below_a_deleted_block_on_the_top_row() {
        let mut editor = diff_editor("a\nd1\nd2\nd3\nb\nc\n", "a\nb\nc\n");
        editor.scroll_row = 1;
        crate::test_harness::editor::place_cursor(&mut editor, 1, 0);
        let area = Rect::new(0, 0, 120, 8);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &Theme::empty(),
            &mut buf,
            true,
            None,
            Some(&mut ApcScene::new()),
            true,
            DiffDials::shipped(),
            None,
        );

        assert!(
            editor
                .display_map
                .snapshot()
                .display_to_buffer(DisplayPoint::new(1, 0))
                .is_none(),
            "the pane's top row is a deleted line",
        );
        assert_eq!(
            editor.cursor_screen_cell,
            Some((wide_columns().right_text_x, 3)),
            "the cursor on `b` paints three rows below a top row that is a deleted line",
        );
    }

    /// Press `keys` on the refined `fn other() {}` line of a diff, and return the
    /// row's columns in the selection wash and in the cursor block.
    ///
    /// stoatty draws the primary cursor itself, so the harness turns it off and
    /// the cell fallback paints the block onto the grid. The shipped theme styles
    /// no `ui.cursor`, so the block is reverse video.
    fn wash_and_cursor_after(keys: &str) -> (Vec<u16>, Vec<u16>) {
        let mut h = diff_harness("fn main() {}\n", "fn other() {}\n");
        h.stoat.stoatty = false;
        h.type_keys(keys);
        h.snapshot();

        let sel_bg = h
            .stoat
            .theme
            .get(UI_SELECTION_EDITOR)
            .bg
            .expect("a selection background");
        let buf = h.rendered_buffer();
        let row = row_holding(buf, right_text(&harness_columns(&h)), "fn other");
        let washed = (0..buf.area.width)
            .filter(|&x| buf[(x, row)].bg == sel_bg)
            .collect();
        let cursor = (0..buf.area.width)
            .filter(|&x| buf[(x, row)].modifier.contains(Modifier::REVERSED))
            .collect();
        (washed, cursor)
    }

    #[test]
    fn a_word_end_motion_washes_the_selected_word() {
        assert_eq!(
            wash_and_cursor_after("e"),
            {
                let x = wide_columns().right_text_x;
                (vec![x], vec![x + 1])
            },
            "`e` selects `fn` with the cursor on the `n`",
        );
    }

    /// The base text beside the line is not the buffer the selection belongs
    /// to, so the wash stays on the live column.
    #[test]
    fn a_line_selection_washes_only_the_live_column() {
        assert_eq!(
            wash_and_cursor_after("x"),
            {
                let x = wide_columns().right_text_x;
                ((x..x + 13).collect(), vec![x + 13])
            },
            "`x` washes the thirteen glyphs and puts the cursor on the newline",
        );
    }

    /// Two words take one-letter labels, `a` for `fn` and `b` for `other`. A
    /// label covers the first letter of its word, so the row is found by the
    /// base text beside it.
    #[test]
    fn goto_word_labels_mark_each_word_of_the_live_column() {
        let mut h = diff_harness("fn main() {}\n", "fn other() {}\n");
        h.type_keys("g w");
        h.snapshot();

        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);
        let row = row_holding(buf, left_text(&cols), "fn main");
        let labels = (0..buf.area.width)
            .filter(|&x| {
                buf[(x, row)]
                    .modifier
                    .contains(Modifier::REVERSED | Modifier::BOLD)
            })
            .map(|x| (x, buf[(x, row)].symbol()))
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            [(cols.right_text_x, "a"), (cols.right_text_x + 3, "b")],
            "each word of the live column carries its label, and the base column none",
        );
    }

    #[test]
    fn the_wrap_override_turns_the_diff_view_wrap_off() {
        let mut h = diff_harness("keep\n", &format!("keep\n{}\ntail\n", long_line(45, 44)));
        h.stoat.toggle_wrap();
        h.snapshot();
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);
        let y = row_holding(buf, right_text(&cols), &"a".repeat(45));
        assert_eq!(
            [y, y + 1].map(|y| cells(buf, y, [right_num(&cols), right_text(&cols)])),
            [
                [
                    "2".to_string(),
                    long_line(45, 44)[..cols.right_content_w].to_string()
                ],
                ["3".to_string(), "tail".to_string()],
            ],
            "the line keeps one row, cut at the column edge",
        );
    }

    /// A removed lone blank line takes a numbered row of its own on the left,
    /// and so does the blank line that ends a removed run.
    #[test]
    fn a_removed_blank_line_at_a_runs_edge_takes_a_row() {
        let rows = |base: &str, count: u16| {
            let h = diff_harness(base, "keep\ntail\n");
            let buf = h.rendered_buffer();
            let cols = harness_columns(&h);
            let y = row_holding(buf, right_text(&cols), "keep");
            (y..y + count)
                .map(|y| {
                    cells(
                        buf,
                        y,
                        [
                            left_num(&cols),
                            left_text(&cols),
                            right_num(&cols),
                            right_text(&cols),
                        ],
                    )
                })
                .collect::<Vec<_>>()
        };
        let row = |cells: [&str; 4]| cells.map(String::from);

        assert_eq!(
            rows("keep\n\ntail\n", 3),
            [
                row(["1", "keep", "1", "keep"]),
                row(["2", "", "", ""]),
                row(["3", "tail", "2", "tail"]),
            ],
            "a lone blank line",
        );
        assert_eq!(
            rows("keep\ngone\n\ntail\n", 4),
            [
                row(["1", "keep", "1", "keep"]),
                row(["2", "gone", "", ""]),
                row(["3", "", "", ""]),
                row(["4", "tail", "2", "tail"]),
            ],
            "a run that ends in a blank line",
        );
    }

    /// The removed line's block takes a row per segment, and only its first row
    /// takes the base number, so the context below it reads line 3.
    #[test]
    fn a_removed_long_line_spans_two_block_rows() {
        let long = long_line(45, 44);
        let h = diff_harness(&format!("keep\n{long}\ntail\n"), "keep\ntail\n");
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);
        let y = row_holding(buf, left_text(&cols), &"a".repeat(45));
        assert_eq!(
            (y..y + 3)
                .map(|y| cells(buf, y, [left_num(&cols), left_text(&cols)]))
                .collect::<Vec<_>>(),
            [
                ["2".to_string(), "a".repeat(45)],
                [String::new(), "b".repeat(44)],
                ["3".to_string(), "tail".to_string()],
            ],
        );
    }

    /// A removed line's block hangs below every row of the line above it, so a
    /// wrapped line above a deletion stays whole.
    #[test]
    fn a_removed_line_below_a_wrapped_line_follows_its_last_row() {
        let long = long_line(45, 44);
        let h = diff_harness(&format!("{long}\ngone\ntail\n"), &format!("{long}\ntail\n"));
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);
        let y = row_holding(buf, right_text(&cols), &"a".repeat(45));
        assert_eq!(
            (y..y + 4)
                .map(|y| cells(
                    buf,
                    y,
                    [
                        left_num(&cols),
                        left_text(&cols),
                        right_num(&cols),
                        right_text(&cols)
                    ]
                ))
                .collect::<Vec<_>>(),
            [
                ["1".into(), "a".repeat(45), "1".into(), "a".repeat(45)],
                [String::new(), "b".repeat(44), String::new(), "b".repeat(44)],
                ["2".into(), "gone".into(), String::new(), String::new()],
                ["3".into(), "tail".into(), "2".into(), "tail".into()],
            ],
        );
    }

    /// Two removed lines paint their rows one after the other, and the second
    /// breaks into its own segments rather than the first line's.
    #[test]
    fn each_removed_line_breaks_into_its_own_segments() {
        let h = diff_harness(
            &format!("keep\nshort\n{}\ntail\n", long_line(45, 44)),
            "keep\ntail\n",
        );
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);
        let y = row_holding(buf, left_text(&cols), "short");
        assert_eq!(
            (y..y + 4)
                .map(|y| cells(buf, y, [left_num(&cols), left_text(&cols)]))
                .collect::<Vec<_>>(),
            [
                ["2".to_string(), "short".to_string()],
                ["3".to_string(), "a".repeat(45)],
                [String::new(), "b".repeat(44)],
                ["4".to_string(), "tail".to_string()],
            ],
        );
    }

    /// At 120 columns the base text wraps at 51 and the line fits. At 110 it
    /// wraps at 46, and both widths keep the two columns, so the wrap width
    /// alone is what moves.
    #[test]
    fn a_resize_resplits_the_standing_blocks() {
        let long = long_line(40, 7);
        let mut h = diff_harness(&format!("keep\n{long}\ntail\n"), "keep\ntail\n");
        let rows = |h: &crate::test_harness::TestHarness| {
            let buf = h.rendered_buffer();
            let left = left_text(&harness_columns(h));
            let y = row_holding(buf, left.clone(), &"a".repeat(40));
            [y, y + 1].map(|y| line_text(buf, y, left.clone()).trim().to_string())
        };
        assert_eq!(rows(&h), [long.clone(), "tail".to_string()]);

        // The 48-char line fits the left column at width 120 and not at 104,
        // whichever gutter the harness paints.
        h.resize(104, 10);
        h.snapshot();
        assert_eq!(
            rows(&h),
            ["a".repeat(40), "b".repeat(7)],
            "the standing block grows a row at the narrower width",
        );
    }

    /// A modified line and its base both outrun the columns, so each wraps
    /// beside the other row for row, with the numbers on the first row alone.
    #[test]
    fn a_paired_base_line_wraps_beside_its_live_line() {
        let live = format!("{} {}", "c".repeat(45), "d".repeat(44));
        let h = diff_harness(
            &format!("keep\n{}\ntail\n", long_line(45, 44)),
            &format!("keep\n{live}\ntail\n"),
        );
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);
        let y = row_holding(buf, left_text(&cols), &"a".repeat(45));
        assert_eq!(
            (y..y + 3)
                .map(|y| cells(
                    buf,
                    y,
                    [
                        left_num(&cols),
                        left_text(&cols),
                        right_num(&cols),
                        right_text(&cols)
                    ]
                ))
                .collect::<Vec<_>>(),
            [
                ["2".into(), "a".repeat(45), "2".into(), "c".repeat(45)],
                [String::new(), "b".repeat(44), String::new(), "d".repeat(44)],
                ["3".into(), "tail".into(), "3".into(), "tail".into()],
            ],
            "the base fits beside the live rows, so no block row follows",
        );
    }

    /// The base breaks into three segments beside a live line of one row, so
    /// its last two sit in block rows below that live row.
    #[test]
    fn a_taller_base_line_pads_below_its_live_row() {
        let base = format!("{} {} {}", "a".repeat(45), "b".repeat(45), "c".repeat(20));
        let h = diff_harness(
            &format!("keep\n{base}\ntail\n"),
            &format!("keep\n{}\ntail\n", "x".repeat(40)),
        );
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);
        let y = row_holding(buf, left_text(&cols), &"a".repeat(45));
        assert_eq!(
            (y..y + 4)
                .map(|y| cells(
                    buf,
                    y,
                    [
                        left_num(&cols),
                        left_text(&cols),
                        right_num(&cols),
                        right_text(&cols)
                    ]
                ))
                .collect::<Vec<_>>(),
            [
                ["2".into(), "a".repeat(45), "2".into(), "x".repeat(40)],
                [String::new(), "b".repeat(45), String::new(), String::new()],
                [String::new(), "c".repeat(20), String::new(), String::new()],
                ["3".into(), "tail".into(), "3".into(), "tail".into()],
            ],
        );
    }

    /// A base of three segments beside a live line of two rows overflows by
    /// one, and that block row follows the live line's last row.
    #[test]
    fn a_base_line_overflows_below_the_last_row_of_a_wrapped_live_line() {
        let base = format!("{} {} {}", "p".repeat(45), "q".repeat(45), "r".repeat(20));
        let h = diff_harness(
            &format!("keep\n{base}\ntail\n"),
            &format!("keep\n{}\ntail\n", long_line(45, 44)),
        );
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);
        let y = row_holding(buf, left_text(&cols), &"p".repeat(45));
        assert_eq!(
            (y..y + 4)
                .map(|y| cells(
                    buf,
                    y,
                    [
                        left_num(&cols),
                        left_text(&cols),
                        right_num(&cols),
                        right_text(&cols)
                    ]
                ))
                .collect::<Vec<_>>(),
            [
                ["2".into(), "p".repeat(45), "2".into(), "a".repeat(45)],
                [String::new(), "q".repeat(45), String::new(), "b".repeat(44)],
                [String::new(), "r".repeat(20), String::new(), String::new()],
                ["3".into(), "tail".into(), "3".into(), "tail".into()],
            ],
        );
    }

    #[test]
    fn a_shorter_base_line_leaves_the_live_continuation_blank_on_the_left() {
        let h = diff_harness(
            &format!("keep\n{}\ntail\n", "x".repeat(40)),
            &format!("keep\n{}\ntail\n", long_line(45, 44)),
        );
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);
        let y = row_holding(buf, right_text(&cols), &"a".repeat(45));
        assert_eq!(
            (y..y + 3)
                .map(|y| cells(
                    buf,
                    y,
                    [
                        left_num(&cols),
                        left_text(&cols),
                        right_num(&cols),
                        right_text(&cols)
                    ]
                ))
                .collect::<Vec<_>>(),
            [
                ["2".into(), "x".repeat(40), "2".into(), "a".repeat(45)],
                [String::new(), String::new(), String::new(), "b".repeat(44)],
                ["3".into(), "tail".into(), "3".into(), "tail".into()],
            ],
        );
    }

    /// A map built before the edit still calls the line Unchanged, so the left
    /// column mirrors the base line at the live row's wrap points. Byte 101 of
    /// the live line falls inside the base line's fifty-first character.
    #[test]
    fn a_stale_map_mirrors_a_wrapped_multibyte_line_on_char_boundaries() {
        let cols = wide_columns();
        let base = format!("{}\n", "\u{e9}".repeat(60));
        let live = format!("x{base}");
        let mut editor = diff_editor_with_map(
            &live,
            DiffMap::from_structural_changes(
                structural_diff::diff(&base, &base),
                Arc::new(base.clone()),
                &base,
            ),
        );
        editor.display_map.set_wrap_width(Some(51));
        let area = Rect::new(0, 0, 120, 8);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &rgb_diff_theme(),
            &mut buf,
            true,
            None,
            None,
            true,
            DiffDials::shipped(),
            None,
        );

        assert_eq!(
            [0, 1].map(|y| line_text(&buf, y, left_text(&cols)).trim().to_string()),
            ["\u{e9}".repeat(50), "\u{e9}".repeat(10)],
        );
    }

    /// An unchanged copy of a line and a removed one both break where the live
    /// line does, and each segment expands its tabs from the column it starts
    /// at. The second row starts off a stop, and the last row runs past column
    /// 256, where a tab takes one column.
    #[test]
    fn a_wrapped_tabbed_base_line_keeps_its_tab_stops_on_every_segment() {
        let cols = wide_columns();
        let line = "ab cd\t".repeat(40);
        let mut editor = diff_editor(
            &format!("{line}\nkeep\n{line}\n"),
            &format!("{line}\nkeep\n"),
        );
        editor.display_map.set_wrap_width(Some(51));
        let area = Rect::new(0, 0, 120, 14);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &rgb_diff_theme(),
            &mut buf,
            true,
            None,
            None,
            true,
            DiffDials::shipped(),
            None,
        );

        let column = |cols: std::ops::Range<u16>| {
            (0..area.height)
                .map(|y| line_text(&buf, y, cols.clone()).trim_end().to_string())
                .collect::<Vec<_>>()
        };
        let (left, right) = (column(left_text(&cols)), column(right_text(&cols)));
        assert_eq!(right[6], "keep", "the live line takes six rows");
        assert_eq!(
            [&left[..6], &left[7..13]],
            [&right[..6], &right[..6]],
            "the unchanged copy and the removed one read as the live rows",
        );
    }

    /// At width 80 both sides share one column, which wraps at the width past
    /// its one gutter.
    #[test]
    fn a_narrow_diff_view_wraps_at_its_single_column() {
        let h = diff_harness_sized(80, "keep\n", &format!("keep\n{}\n", long_line(65, 24)));
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);
        let y = row_holding(buf, right_text(&cols), &"a".repeat(65));
        assert_eq!(
            [y, y + 1].map(|y| cells(buf, y, [left_num(&cols), right_text(&cols)])),
            [
                ["2".to_string(), "a".repeat(65)],
                [String::new(), "b".repeat(24)],
            ],
        );
    }

    /// The changed word is marked by everything around it receding, not by a
    /// color behind it, so a refined row is one flat stretch of the editor
    /// background from end to end. See
    /// `diff_view_softens_the_unchanged_gaps_of_a_refined_row` for the receding
    /// half of that.
    #[test]
    fn diff_view_paints_no_background_on_a_refined_row() {
        // `main` becomes `other`, so only that one word changed on the line.
        let h = diff_harness("fn main() {}\n", "fn other() {}\n");
        let canvas = h
            .stoat
            .theme
            .try_get(crate::theme::scope::UI_BACKGROUND)
            .and_then(|s| s.bg)
            .expect("an rgb editor background");
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);

        let row = (0..buf.area.height)
            .find(|&y| line_text(buf, y, right_text(&cols)).contains("other"))
            .expect("the changed line rendered on the right");
        assert!(
            line_text(buf, row, left_text(&cols)).contains("main"),
            "and its base word beside it: {:?}",
            line_text(buf, row, left_text(&cols))
        );

        let painted = (0..buf.area.width)
            .filter(|&x| buf[(x, row)].bg != canvas)
            .collect::<Vec<_>>();
        assert_eq!(
            painted,
            Vec::<u16>::new(),
            "no cell of the row carries a background of its own",
        );
    }

    #[test]
    fn diff_view_softens_the_unchanged_gaps_of_a_refined_row() {
        // `main` becomes `other` on the first line, so that line is refined.
        // The second is a pure insertion, which nothing refines, so its
        // identical `fn` keyword stays at full strength to compare against.
        let h = diff_harness("fn main() {}\n", "fn other() {}\nfn extra() {}\n");

        let bg = style_rgb(
            h.stoat
                .theme
                .try_get(crate::theme::scope::UI_BACKGROUND)
                .and_then(|s| s.bg),
        )
        .expect("rgb background");
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);

        let row_with = |text: &str| {
            (0..buf.area.height)
                .find(|&y| line_text(buf, y, right_text(&cols)).contains(text))
                .unwrap_or_else(|| panic!("{text:?} rendered in the right column"))
        };
        let fg_of = |y: u16, glyph: &str| {
            let x = right_text(&cols)
                .find(|&x| buf[(x, y)].symbol() == glyph)
                .unwrap_or_else(|| panic!("{glyph:?} on row {y}"));
            buf[(x, y)].style().fg.expect("an fg")
        };

        let refined = row_with("fn other");
        let added = row_with("fn extra");

        let full_keyword = fg_of(added, "f");
        let [r, g, b] = dim_rgb(
            style_rgb(Some(full_keyword)).expect("rgb fg on the added row"),
            bg,
            MODIFIED_ROW_SOFTEN,
        );

        assert_eq!(
            (fg_of(refined, "f"), fg_of(refined, "o")),
            (Color::Rgb(r, g, b), fg_of(added, "e")),
            "the gaps blend toward the background while the changed word stays full"
        );
        assert_ne!(
            fg_of(refined, "f"),
            full_keyword,
            "the gap blend moves the color off full strength"
        );
    }

    /// Inside a string the whole literal carries one color, so the receding
    /// around a changed char is all that separates it. Bold gives the eye a
    /// mark that the color does not give. The underline shows where the changed
    /// run starts and ends among the unchanged words.
    #[test]
    fn diff_view_bolds_a_changed_char_inside_a_string() {
        // The two literals share most of their text, so the search reads them
        // as one edited string. Bold marks the case the reader compares char by
        // char, which is what a pair means and a delete beside an add does not.
        let h = diff_harness(
            "fn f() {\n    g(\"one aaa three\");\n}\n",
            "fn f() {\n    g(\"one zzz three\");\n}\n",
        );
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);

        let changed_word = ("zzz".to_string(), "aaa".to_string());
        assert_eq!(
            (
                row_cells_with(buf, &cols, "zzz", Modifier::BOLD),
                row_cells_with(buf, &cols, "zzz", Modifier::UNDERLINED),
            ),
            (changed_word.clone(), changed_word),
            "both columns bold and underline their changed word and nothing else of the literal"
        );
    }

    /// The search reads two similar comments as a pair rather than as two
    /// unrelated runs, and the pair has to reach the reader. Without it the row
    /// arrives gutter-marked with nothing receding and nothing bold, saying
    /// only that the line changed somewhere.
    #[test]
    fn diff_view_marks_the_changed_word_of_a_similar_comment() {
        let h = diff_harness("// step aaa here\n", "// step zzz here\n");
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);

        let changed_word = ("zzz".to_string(), "aaa".to_string());
        assert_eq!(
            (
                row_cells_with(buf, &cols, "zzz", Modifier::BOLD),
                row_cells_with(buf, &cols, "zzz", Modifier::UNDERLINED),
            ),
            (changed_word.clone(), changed_word),
            "each column marks its own changed word and leaves the rest of the comment alone"
        );
    }

    /// A comment whose every word changed keeps no text around the edit to
    /// set it apart from, so it bolds as a replacement and takes no underline.
    /// Its `//` marker survives the rewrite but carries no words of its own.
    #[test]
    fn a_comment_rewritten_as_a_whole_takes_no_underline() {
        let h = diff_harness("// aaa bbb\n", "// zzz yyy\n");
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);

        assert_eq!(
            (
                row_cells_with(buf, &cols, "zzz", Modifier::BOLD),
                row_cells_with(buf, &cols, "zzz", Modifier::UNDERLINED),
            ),
            (
                ("zzz yyy".to_string(), "aaa bbb".to_string()),
                (String::new(), String::new()),
            ),
            "both columns bold the rewritten words and underline nothing"
        );
    }

    /// Bold and the underline say "these chars differ from the ones beside them
    /// in the other text". Added prose has no counterpart to differ from, so it
    /// stays plain however unstructured it is.
    #[test]
    fn diff_view_neither_bolds_nor_underlines_an_added_comment() {
        // The buffer opens two changed runs against the base's one, so the
        // pairing pass claims `h` and leaves the comment with no counterpart.
        let h = diff_harness(
            "fn f() {\n    g();\n}\n",
            "fn f() {\n    h();\n    // note here\n}\n",
        );
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);

        assert_eq!(
            (
                row_cells_with(buf, &cols, "note here", Modifier::BOLD),
                row_cells_with(buf, &cols, "note here", Modifier::UNDERLINED),
            ),
            (
                (String::new(), String::new()),
                (String::new(), String::new()),
            ),
            "an added comment carries no bold and no underline"
        );
    }

    /// A renamed identifier already has a token boundary and a color change
    /// beside it, so bolding it would mark what the reader can see.
    #[test]
    fn diff_view_keeps_a_renamed_identifier_unbolded() {
        let h = diff_harness("fn alpha() {}\n", "fn beta() {}\n");
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);

        let row = (0..buf.area.height)
            .find(|&y| line_text(buf, y, right_text(&cols)).contains("beta"))
            .expect("the renamed line rendered on the right");
        let bold = right_text(&cols)
            .filter(|&x| buf[(x, row)].modifier.contains(Modifier::BOLD))
            .map(|x| buf[(x, row)].symbol().to_string())
            .collect::<String>();
        assert_eq!(bold, "", "nothing on a code row bolds");
    }

    /// The bold dial marks a change by weight whatever its kind, so it bolds the
    /// code rename that the prose rule leaves plain, and nothing else of the row.
    #[test]
    fn the_bold_dial_bolds_a_code_change_span() {
        assert_eq!(
            (
                renamed_row_cells_with(DiffDials::shipped(), Modifier::BOLD),
                renamed_row_cells_with(
                    DiffDials {
                        bold: true,
                        ..DiffDials::shipped()
                    },
                    Modifier::BOLD,
                ),
            ),
            (
                (String::new(), String::new()),
                ("beta".to_string(), "alpha".to_string()),
            ),
            "the dial alone bolds the rename, in each column and on nothing else"
        );
    }

    /// On a theme that blends, only the receding marks a change span. The
    /// underline dial adds an underline to every span there, and to nothing
    /// else of the row.
    #[test]
    fn the_underline_dial_underlines_a_change_span_on_an_rgb_theme() {
        assert_eq!(
            (
                renamed_row_cells_with(DiffDials::shipped(), Modifier::UNDERLINED),
                renamed_row_cells_with(
                    DiffDials {
                        underline: true,
                        ..DiffDials::shipped()
                    },
                    Modifier::UNDERLINED,
                ),
            ),
            (
                (String::new(), String::new()),
                ("beta".to_string(), "alpha".to_string()),
            ),
            "the dial alone underlines the rename, in each column and on nothing else"
        );
    }

    /// The glyphs that carry `modifier` on the changed row of an `alpha` to
    /// `beta` rename, right column then left, painted with `dials`.
    ///
    /// The harness diffs a `.rs` file, so the rename refines as code. A
    /// plain-text editor refines as prose, which bolds whatever the dial says.
    fn renamed_row_cells_with(dials: DiffDials, modifier: Modifier) -> (String, String) {
        let mut h = diff_harness("fn alpha() {}\n", "fn beta() {}\n");
        let theme = Arc::clone(&h.stoat.theme);
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        let area = Rect::new(0, 0, 120, 4);
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let mut buf = Buffer::empty(area);
        render_diff_view(
            editor, area, fallback, &theme, &mut buf, true, None, None, true, dials, None,
        );
        row_cells_with(&buf, &wide_columns(), "beta", modifier)
    }

    /// The glyphs that carry `modifier` on the row whose right column shows
    /// `needle`, right column then left, read across `cols`.
    fn row_cells_with(
        buf: &Buffer,
        cols: &DiffColumns,
        needle: &str,
        modifier: Modifier,
    ) -> (String, String) {
        let row = (0..buf.area.height)
            .find(|&y| line_text(buf, y, right_text(cols)).contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} rendered in the right column"));
        let marked = |cells: std::ops::Range<u16>| {
            cells
                .filter(|&x| buf[(x, row)].modifier.contains(modifier))
                .map(|x| buf[(x, row)].symbol().to_string())
                .collect::<String>()
        };
        (marked(right_text(cols)), marked(left_text(cols)))
    }

    /// A theme that cannot blend has no receding to lead a change with, so the
    /// view asks for the one mark left to it and underlines the changed chars.
    /// `paint_base_row_underlines_change_spans_on_a_theme_that_cannot_blend`
    /// pins the paint; this pins that the view asks for it.
    #[test]
    fn diff_view_underlines_a_change_span_on_a_theme_that_cannot_blend() {
        // `main` becomes `other`, so only that one word changed on the line.
        let mut h = diff_harness("fn main() {}\n", "fn other() {}\n");
        h.stoat.theme = Arc::new(Theme::empty());
        // Both theme installs move the paint generation, which is what tells a
        // replayed pane to paint under the new theme.
        h.stoat.paint_generation += 1;
        h.snapshot();
        let buf = h.rendered_buffer();

        let underlined = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .filter(|&pos| buf[pos].modifier.contains(Modifier::UNDERLINED))
            .map(|pos| buf[pos].symbol().to_string())
            .collect::<String>();
        assert_eq!(
            underlined, "mainother",
            "the changed word is underlined on both sides, and nothing else is"
        );
    }

    #[test]
    fn diff_view_softens_context_row_foregrounds() {
        // The first line is untouched, so it renders as a context row. The
        // second is a pure insertion, which nothing softens, so its identical
        // `fn` keyword is the full-strength reference.
        let h = diff_harness("fn keep() {}\n", "fn keep() {}\nfn add() {}\n");

        let bg = style_rgb(
            h.stoat
                .theme
                .try_get(crate::theme::scope::UI_BACKGROUND)
                .and_then(|s| s.bg),
        )
        .expect("rgb background");
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);

        let row_with = |text: &str| {
            (0..buf.area.height)
                .find(|&y| line_text(buf, y, right_text(&cols)).contains(text))
                .unwrap_or_else(|| panic!("{text:?} rendered in the right column"))
        };
        let leading_glyph = |y: u16| {
            let x = right_text(&cols)
                .find(|&x| !buf[(x, y)].symbol().trim().is_empty())
                .expect("a glyph on the row");
            (buf[(x, y)].symbol().to_string(), buf[(x, y)].style().fg)
        };

        let (added_glyph, added_fg) = leading_glyph(row_with("fn add"));
        let (context_glyph, context_fg) = leading_glyph(row_with("fn keep"));
        let [r, g, b] = dim_rgb(
            style_rgb(added_fg).expect("rgb fg on the added row"),
            bg,
            CONTEXT_SOFTEN,
        );

        assert_eq!(
            (added_glyph.as_str(), context_glyph.as_str(), context_fg),
            ("f", "f", Some(Color::Rgb(r, g, b))),
            "the context row's keyword blends the added row's color toward the background"
        );
        assert_ne!(
            context_fg, added_fg,
            "the blend moves the context color off the full-strength one"
        );
    }

    /// The level reaches the paint through the whole live chain, which is what
    /// makes it a knob rather than a field.
    #[test]
    fn the_soften_level_scales_how_far_a_context_row_recedes() {
        let mut h = diff_harness("fn keep() {}\n", "fn keep() {}\nfn add() {}\n");

        let bg = style_rgb(
            h.stoat
                .theme
                .try_get(crate::theme::scope::UI_BACKGROUND)
                .and_then(|s| s.bg),
        )
        .expect("rgb background");

        let context_fg = |h: &mut crate::test_harness::TestHarness| {
            h.snapshot();
            let buf = h.rendered_buffer();
            let cols = harness_columns(h);
            let y = (0..buf.area.height)
                .find(|&y| line_text(buf, y, right_text(&cols)).contains("fn keep"))
                .expect("the context row renders");
            let x = right_text(&cols)
                .find(|&x| !buf[(x, y)].symbol().trim().is_empty())
                .expect("a glyph on the row");
            buf[(x, y)].style().fg
        };

        let shipped = context_fg(&mut h);

        h.stoat.diff_soften = DIFF_SOFTEN_MIN;
        let stopped = context_fg(&mut h);

        h.stoat.diff_soften = DIFF_SOFTEN_MAX;
        let deepest = context_fg(&mut h);

        let blend = |amount: f32| {
            let full = style_rgb(stopped).expect("rgb fg with no soften applied");
            let [r, g, b] = dim_rgb(full, bg, amount);
            Some(Color::Rgb(r, g, b))
        };

        assert_eq!(
            (shipped, deepest),
            (
                blend(CONTEXT_SOFTEN),
                blend((CONTEXT_SOFTEN * diff_soften_scale(DIFF_SOFTEN_MAX)).min(SOFTEN_CAP)),
            ),
            "level 0 paints the shipped fraction and the top level lands on the floor",
        );
        assert_ne!(
            stopped, shipped,
            "the bottom level stops the blend rather than shrinking it",
        );
    }

    /// The top level is chosen to be the last one that still moves a context
    /// row. Raising either the level or the fraction past this point buys a
    /// keypress that paints nothing, so both constants are pinned together.
    #[test]
    fn the_deepest_soften_level_is_the_last_one_that_moves_a_context_row() {
        let below = CONTEXT_SOFTEN * diff_soften_scale(DIFF_SOFTEN_MAX - 1);
        let deepest = CONTEXT_SOFTEN * diff_soften_scale(DIFF_SOFTEN_MAX);

        assert!(
            below < SOFTEN_CAP,
            "the step before the top still has room to move: {below} is not under {SOFTEN_CAP}",
        );
        assert!(
            deepest >= SOFTEN_CAP,
            "the top step reaches the floor: {deepest} is under {SOFTEN_CAP}",
        );
    }

    #[test]
    fn diff_view_added_line_takes_no_line_wash() {
        // The second line is a pure insertion, so nothing is refined.
        let h = diff_harness("fn a() {}\n", "fn a() {}\nfn b() {}\n");

        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);

        let underlined = (0..buf.area.height).any(|y| {
            (0..buf.area.width).any(|x| buf[(x, y)].modifier.contains(Modifier::UNDERLINED))
        });
        assert!(!underlined, "a pure added line underlines nothing");

        let row = (0..buf.area.height)
            .find(|&y| line_text(buf, y, right_text(&cols)).contains("fn b"))
            .expect("added line rendered on the right");
        let context = (0..buf.area.height)
            .find(|&y| line_text(buf, y, right_text(&cols)).contains("fn a"))
            .expect("context line rendered on the right");
        assert!(
            (right_text(&cols)).all(|x| buf[(x, row)].bg == buf[(x, context)].bg),
            "the added line's cells carry the same background as an unchanged row"
        );
    }

    #[test]
    fn diff_view_deleted_line_takes_no_line_wash() {
        // `old` is deleted, so it renders as a base-only block row on the left.
        let h = diff_harness("keep\nold\ntail\n", "keep\ntail\n");

        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);

        let row = (0..buf.area.height)
            .find(|&y| line_text(buf, y, left_text(&cols)).contains("old"))
            .expect("deleted base line rendered on the left");
        let context = (0..buf.area.height)
            .find(|&y| line_text(buf, y, left_text(&cols)).contains("keep"))
            .expect("unchanged base line mirrored on the left");
        assert!(
            (left_text(&cols)).all(|x| buf[(x, row)].bg == buf[(x, context)].bg),
            "the deleted base line's cells carry the same background as a mirrored row"
        );
    }

    #[test]
    fn diff_view_unchanged_mirrored_base_row_carries_no_background() {
        // `keep` is unchanged and mirrored into the left column.
        let h = diff_harness("keep\nold\ntail\n", "keep\ntail\n");
        let canvas = h
            .stoat
            .theme
            .try_get(crate::theme::scope::UI_BACKGROUND)
            .and_then(|s| s.bg)
            .expect("an rgb editor background");
        let buf = h.rendered_buffer();
        // The gutter's last cell, where its rule sits, then the left text.
        let left = {
            let text = left_text(&harness_columns(&h));
            text.start - 1..text.end
        };

        let row = (0..buf.area.height)
            .find(|&y| line_text(buf, y, left.clone()).contains("keep"))
            .expect("unchanged base line mirrored on the left");
        assert!(
            left.clone().all(|x| buf[(x, row)].bg == canvas),
            "an unchanged mirrored base row paints no background of its own"
        );
    }

    /// An unchanged row's left half paints the base line's own bytes, which are
    /// what its token spans index. The live line's inlay hint belongs to the
    /// right column alone, so it neither shows on the left nor shifts the
    /// colors there.
    #[test]
    fn diff_view_mirrors_the_base_line_without_the_live_inlays() {
        let cols = wide_columns();
        let mut editor = diff_editor("keep\nold\ntail\n", "keep\nnew\ntail\n");
        let anchor = {
            let snapshot = editor.display_map.buffer_snapshot();
            snapshot.anchor_at(
                snapshot.rope().point_to_offset(Point::new(0, 4)),
                Bias::Right,
            )
        };
        editor.display_map.splice_inlays(
            Vec::new(),
            vec![(anchor, ": u32".to_string(), InlayKind::Hint)],
        );

        let area = Rect::new(0, 0, 120, 8);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &Theme::empty(),
            &mut buf,
            true,
            None,
            None,
            true,
            DiffDials::shipped(),
            None,
        );

        assert_eq!(
            (
                line_text(&buf, 0, right_text(&cols)).trim_end().to_string(),
                line_text(&buf, 0, left_text(&cols)).trim_end().to_string(),
            ),
            ("keep: u32".to_string(), "keep".to_string()),
            "the hint rides the live line on the right and stays off the mirrored base line"
        );
    }

    /// A moved span is marked the same way a replaced one is, by the receding
    /// around it, so it paints no background either.
    #[test]
    fn diff_view_moved_line_carries_no_background() {
        // A Moved hunk covering buffer line 1 ("bb", bytes 3..5); no base bytes,
        // so the line stays in place on the right rather than splicing a block.
        let dm = {
            let detail = Arc::new(TokenDetail {
                buffer_spans: vec![ChangeSpan {
                    byte_range: 3..5,
                    kind: ChangeKind::Moved,
                    move_metadata: None,
                    prose: ProseChange::None,
                }],
                base_spans: Vec::new(),
            });
            DiffMap::from_hunks(
                [DiffHunk {
                    status: DiffHunkStatus::Moved,
                    unstaged_lines: std::iter::once(1..2).collect(),
                    marked_rows: Vec::new(),
                    buffer_start_line: 1,
                    buffer_line_range: 1..2,
                    base_byte_range: 0..0,
                    anchor_range: None,
                    token_detail: Some(detail),
                }],
                None,
            )
        };
        let mut editor = diff_editor_with_map("aa\nbb\ncc\n", dm);
        let theme = rgb_diff_theme();
        let area = Rect::new(0, 0, 40, 5);
        let mut buf = Buffer::empty(area);
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        render_diff_view(
            &mut editor,
            area,
            fallback,
            &theme,
            &mut buf,
            true,
            None,
            None,
            true,
            DiffDials::shipped(),
            None,
        );

        // At this sub-threshold width the diff is unified, so the moved buffer
        // row "bb" paints at the single text column. It sits on display row 1.
        let rx = editor
            .text_rect
            .expect("the render records the text rect")
            .x;
        assert_eq!(
            buf[(rx, 1)].symbol(),
            "b",
            "the moved line renders on the right"
        );
        assert!(
            (rx..area.width).all(|x| buf[(x, 1)].bg == buf[(x, 0)].bg),
            "and every cell of it carries what an unchanged row carries",
        );
    }

    #[test]
    fn a_row_without_spans_clears_what_the_row_before_it_left() {
        let detail = Arc::new(TokenDetail {
            buffer_spans: vec![ChangeSpan {
                byte_range: 3..5,
                kind: ChangeKind::Replaced,
                move_metadata: None,
                prose: ProseChange::None,
            }],
            base_spans: Vec::new(),
        });
        let dm = DiffMap::from_hunks(
            [DiffHunk {
                status: DiffHunkStatus::Modified,
                unstaged_lines: std::iter::once(1..2).collect(),
                marked_rows: Vec::new(),
                buffer_start_line: 1,
                buffer_line_range: 1..2,
                base_byte_range: 0..0,
                anchor_range: None,
                token_detail: Some(detail),
            }],
            None,
        );
        let mut editor = diff_editor_with_map("aa\nbb\ncc\n", dm);
        let snapshot = editor.display_map.snapshot();

        let mut hunks = Vec::new();
        let mut spans = Vec::new();

        write_buffer_row_change_spans(&snapshot, 1, 1, &mut hunks, &mut spans);
        assert_eq!(
            spans,
            vec![(0..2, ChangeKind::Replaced, ProseChange::None)],
            "the modified row reports the span covering its changed bytes",
        );

        write_buffer_row_change_spans(&snapshot, 2, 2, &mut hunks, &mut spans);
        assert_eq!(
            spans,
            Vec::new(),
            "the unmodified row after it reports nothing of its own",
        );
    }

    /// A diff editor whose buffer line 1 ("bb") is a move whose counterpart is
    /// `source`, for exercising the move-origin chip.
    fn moved_editor(source: structural_diff::MoveSource) -> EditorState {
        let detail = Arc::new(TokenDetail {
            buffer_spans: vec![ChangeSpan {
                byte_range: 3..5,
                kind: ChangeKind::Moved,
                move_metadata: Some(Arc::new(structural_diff::MoveMetadata {
                    sources: vec![source],
                })),
                prose: ProseChange::None,
            }],
            base_spans: Vec::new(),
        });
        let dm = DiffMap::from_hunks(
            [DiffHunk {
                status: DiffHunkStatus::Moved,
                unstaged_lines: std::iter::once(1..2).collect(),
                marked_rows: Vec::new(),
                buffer_start_line: 1,
                buffer_line_range: 1..2,
                base_byte_range: 0..0,
                anchor_range: None,
                token_detail: Some(detail),
            }],
            None,
        );
        diff_editor_with_map("aa\nbb\ncc\n", dm)
    }

    #[test]
    fn diff_view_moved_row_shows_a_cross_file_origin_chip() {
        use structural_diff::{BufferRef, MoveSource, Side};

        let mut editor = moved_editor(MoveSource {
            buffer: Some(BufferRef {
                path: std::path::PathBuf::from("src/b.rs"),
                fingerprint: [0u8; 32],
            }),
            side: Side::Lhs,
            byte_range: 0..0,
            line_range: 3..4,
        });
        let area = Rect::new(0, 0, 60, 5);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &Theme::empty(),
            &mut buf,
            true,
            None,
            None,
            true,
            DiffDials::shipped(),
            None,
        );

        let row = buffer_text(&buf, 1);
        assert!(
            row.contains("<- b.rs:4"),
            "a cross-file moved row shows the origin file:line chip; got {row:?}"
        );
    }

    #[test]
    fn diff_view_moved_row_shows_an_intra_file_origin_chip() {
        use structural_diff::{MoveSource, Side};

        let mut editor = moved_editor(MoveSource {
            buffer: None,
            side: Side::Lhs,
            byte_range: 0..0,
            line_range: 4..5,
        });
        let area = Rect::new(0, 0, 60, 5);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &Theme::empty(),
            &mut buf,
            true,
            None,
            None,
            true,
            DiffDials::shipped(),
            None,
        );

        let row = buffer_text(&buf, 1);
        assert!(
            row.contains("<- 5") && !row.contains(':'),
            "an intra-file moved row shows a path-less chip; got {row:?}"
        );
    }

    /// At a wrap width of one, every line spans two rows, and the moved line
    /// `bb` sits on rows 2 and 3.
    #[test]
    fn a_wrapped_moved_line_carries_its_chip_on_its_last_row() {
        use structural_diff::{MoveSource, Side};

        let mut editor = moved_editor(MoveSource {
            buffer: None,
            side: Side::Lhs,
            byte_range: 0..0,
            line_range: 4..5,
        });
        editor.display_map.set_wrap_width(Some(1));
        let area = Rect::new(0, 0, 60, 8);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &Theme::empty(),
            &mut buf,
            true,
            None,
            None,
            true,
            DiffDials::shipped(),
            None,
        );

        assert_eq!(
            (0..area.height)
                .filter(|&y| buffer_text(&buf, y).contains("<- 5"))
                .collect::<Vec<_>>(),
            [3],
        );
    }

    /// A theme that can blend marks a change span by receding everything else,
    /// so the span itself is left exactly as it was: no background of its own,
    /// and no underline either.
    #[test]
    fn paint_base_row_leaves_change_spans_unwashed_on_an_rgb_theme() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        let change_spans = vec![
            (0..3, ChangeKind::Replaced, ProseChange::None),
            (3..6, ChangeKind::Moved, ProseChange::None),
        ];
        paint_base_row(
            &mut buf,
            0,
            0,
            "abcdefgh",
            8,
            4,
            &[],
            Style::default(),
            &change_spans,
            Some(&rgb_tints()),
            None,
            None,
            DiffDials::shipped(),
            None,
        );

        for x in 0..8 {
            assert_eq!(
                (buf[(x, 0)].bg, buf[(x, 0)].modifier),
                (Color::Reset, Modifier::empty()),
                "col {x} carries neither a background nor an underline",
            );
        }
    }

    /// A tab runs to its next stop, as the right column expands it, so a
    /// tab-indented base line keeps its indentation. The spaces take the tab's
    /// own style, and the text after them keeps its token's.
    #[test]
    fn paint_base_row_expands_a_tab_to_its_stop() {
        let (plain, token) = (Color::Rgb(9, 9, 9), Color::Rgb(1, 2, 3));
        let paint = |text: &str, token_spans: &[(std::ops::Range<usize>, HighlightStyle)]| {
            let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
            paint_base_row(
                &mut buf,
                0,
                0,
                text,
                10,
                4,
                token_spans,
                Style::default().fg(plain),
                &[],
                None,
                None,
                None,
                DiffDials::shipped(),
                None,
            );
            buf
        };

        let leading = paint(
            "\tfoo",
            &[(
                1..4,
                HighlightStyle {
                    foreground: Some(token),
                    ..HighlightStyle::default()
                },
            )],
        );
        assert_eq!(
            (0..7)
                .map(|x| (leading[(x, 0)].symbol(), leading[(x, 0)].fg))
                .collect::<Vec<_>>(),
            [
                (" ", plain),
                (" ", plain),
                (" ", plain),
                (" ", plain),
                ("f", token),
                ("o", token),
                ("o", token),
            ],
            "a leading tab fills four cells in the fallback style, then foo keeps its token"
        );

        let inner = paint("ab\tc", &[]);
        assert_eq!(
            line_text(&inner, 0, 0..5),
            "ab  c",
            "a tab after two columns runs only to the stop at column four"
        );
    }

    /// With no tints a change span underlines, so the underline shows how far
    /// the span reaches into the segment.
    #[test]
    fn spans_that_open_on_an_earlier_segment_still_mark_this_one() {
        let token = Color::Rgb(1, 2, 3);
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
        paint_base_segment(
            &mut buf,
            0,
            0,
            "abcdefgh",
            4..8,
            4,
            4,
            4,
            u32::MAX,
            &[(
                2..6,
                HighlightStyle {
                    foreground: Some(token),
                    ..HighlightStyle::default()
                },
            )],
            Style::default(),
            Style::default(),
            Style::default(),
            &[(3..7, ChangeKind::Replaced, ProseChange::None)],
            None,
            None,
            None,
            DiffDials::shipped(),
            None,
        );

        assert_eq!(
            (0..4)
                .map(|x| {
                    let cell = &buf[(x, 0)];
                    (
                        cell.symbol(),
                        cell.fg,
                        cell.modifier.contains(Modifier::UNDERLINED),
                    )
                })
                .collect::<Vec<_>>(),
            [
                ("e", token, true),
                ("f", token, true),
                ("g", Color::Reset, true),
                ("h", Color::Reset, false),
            ],
            "the token covers e and f, and the change span runs through g",
        );
    }

    /// A palette is free to put a color near its own background, and receding
    /// the context around such a char leaves it just as dim as the context. The
    /// lift is what keeps a changed char legible whatever the palette chose.
    #[test]
    fn a_faint_changed_char_lifts_toward_the_bright_pole() {
        let tints = rgb_tints();
        // Onedark's comment gray on its own background, the case that motivates
        // the floor, against one of its ordinary syntax colors.
        let faint = [92, 99, 112];
        let vivid = [200, 100, 50];

        let paint = |fg: [u8; 3], soften_scale: f32| {
            let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
            paint_base_row(
                &mut buf,
                0,
                0,
                "abcd",
                4,
                4,
                &[],
                Style::default().fg(Color::Rgb(fg[0], fg[1], fg[2])),
                &[(0..4, ChangeKind::Replaced, ProseChange::None)],
                Some(&tints),
                None,
                None,
                DiffDials {
                    soften_scale,
                    ..DiffDials::shipped()
                },
                None,
            );
            buf[(0, 0)].style().fg
        };

        let lifted = {
            let short = FAINT_CONTRAST_FLOOR - (luma(faint) - luma(tints.bg)).abs();
            let [r, g, b] = dim_rgb(faint, [255, 255, 255], short / FAINT_CONTRAST_FLOOR);
            [r, g, b]
        };
        assert_eq!(
            paint(faint, 1.0),
            Some(Color::Rgb(lifted[0], lifted[1], lifted[2])),
            "a faint char moves toward white by the share of the floor it falls short",
        );
        assert!(
            luma(lifted) > luma(faint),
            "and the move is away from the dark background, not toward it",
        );

        assert_eq!(
            paint(vivid, 1.0),
            Some(Color::Rgb(200, 100, 50)),
            "a color already past the floor paints exactly as the theme wrote it",
        );
        assert_eq!(
            paint(faint, 0.0),
            Some(Color::Rgb(92, 99, 112)),
            "with softening off nothing recedes, so nothing needs lifting",
        );
    }

    /// A wholly added or deleted row has no spans of its own, and its row tint
    /// is inert until the dial turns up. The lift has to reach it there too,
    /// since that is the row a muted palette hides most.
    #[test]
    fn a_faint_spanless_row_lifts_at_the_tint_dial_off_position() {
        let tints = rgb_tints();
        let faint = [92, 99, 112];

        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
        paint_base_row(
            &mut buf,
            0,
            0,
            "abcd",
            4,
            4,
            &[],
            Style::default().fg(Color::Rgb(faint[0], faint[1], faint[2])),
            &[],
            Some(&tints),
            None,
            None,
            DiffDials::shipped(),
            Some(tints.deleted),
        );

        let short = FAINT_CONTRAST_FLOOR - (luma(faint) - luma(tints.bg)).abs();
        let [r, g, b] = dim_rgb(faint, [255, 255, 255], short / FAINT_CONTRAST_FLOOR);
        assert_eq!(
            buf[(0, 0)].style().fg,
            Some(Color::Rgb(r, g, b)),
            "the row lifts even though the tint amount leaves its color alone",
        );
    }

    /// The lift measures the color the dial produced rather than the one the
    /// theme wrote, so a status color that is itself dark lifts too. Red reads
    /// dark by luma however saturated it is, which is why the deleted color at
    /// the top of the dial still moves off a dark background.
    #[test]
    fn the_lift_reads_the_color_the_tint_dial_produced() {
        let tints = rgb_tints();
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
        paint_base_row(
            &mut buf,
            0,
            0,
            "abcd",
            4,
            4,
            &[],
            Style::default().fg(Color::Rgb(200, 100, 50)),
            &[(0..4, ChangeKind::Novel, ProseChange::None)],
            Some(&tints),
            None,
            None,
            DiffDials {
                tint_amount: 1.0,
                ..DiffDials::shipped()
            },
            None,
        );

        let flat = style_rgb(Some(tints.deleted)).expect("an rgb status color");
        let short = FAINT_CONTRAST_FLOOR - (luma(flat) - luma(tints.bg)).abs();
        let [r, g, b] = dim_rgb(flat, [255, 255, 255], short / FAINT_CONTRAST_FLOOR);
        assert_eq!(
            buf[(0, 0)].style().fg,
            Some(Color::Rgb(r, g, b)),
            "the flat deleted color lifts, because the dial's output is what the \
             floor measures",
        );
    }

    /// The pole is whichever end the background is furthest from, so a light
    /// theme darkens a faint char rather than washing it out further.
    #[test]
    fn a_light_background_lifts_a_faint_char_toward_black() {
        let tints = DiffTints {
            bg: [250, 250, 250],
            ..rgb_tints()
        };
        let faint = [200, 200, 200];

        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
        paint_base_row(
            &mut buf,
            0,
            0,
            "abcd",
            4,
            4,
            &[],
            Style::default().fg(Color::Rgb(faint[0], faint[1], faint[2])),
            &[(0..4, ChangeKind::Replaced, ProseChange::None)],
            Some(&tints),
            None,
            None,
            DiffDials::shipped(),
            None,
        );

        let short = FAINT_CONTRAST_FLOOR - (luma(faint) - luma(tints.bg)).abs();
        let [r, g, b] = dim_rgb(faint, [0, 0, 0], short / FAINT_CONTRAST_FLOOR);
        assert_eq!(
            buf[(0, 0)].style().fg,
            Some(Color::Rgb(r, g, b)),
            "a faint char on a light theme moves toward black",
        );
        assert!(
            luma([r, g, b]) < luma(faint),
            "which is away from its background"
        );
    }

    /// The dial walks a changed char from its syntax color to its status color,
    /// which is what makes the color itself say modified or deleted rather than
    /// leaving that to the gutter bars.
    ///
    /// Softening is off throughout, which turns the contrast lift off with it,
    /// so what these expectations read is the dial's own arithmetic.
    #[test]
    fn the_tint_amount_walks_a_change_span_toward_its_status_color() {
        let tints = rgb_tints();
        let fg = [200, 100, 50];
        let change_spans = vec![
            (0..3, ChangeKind::Replaced, ProseChange::None),
            (3..6, ChangeKind::Novel, ProseChange::None),
        ];

        let paint = |amount: f32| {
            let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
            paint_base_row(
                &mut buf,
                0,
                0,
                "abcdefgh",
                8,
                4,
                &[],
                Style::default().fg(Color::Rgb(fg[0], fg[1], fg[2])),
                &change_spans,
                Some(&tints),
                None,
                None,
                DiffDials {
                    soften_scale: 0.0,
                    tint_amount: amount,
                    ..DiffDials::shipped()
                },
                None,
            );
            [0u16, 3, 6].map(|x| buf[(x, 0)].style().fg)
        };

        let blend = |target: Color| {
            let target = style_rgb(Some(target)).expect("an rgb status color");
            let [r, g, b] = dim_rgb(fg, target, 0.5);
            Some(Color::Rgb(r, g, b))
        };
        let syntax = Some(Color::Rgb(fg[0], fg[1], fg[2]));

        assert_eq!(
            paint(0.0),
            [syntax; 3],
            "the off position leaves every char its syntax color",
        );
        assert_eq!(
            paint(0.5),
            [blend(tints.deleted), blend(tints.deleted), syntax],
            "a mid level leans a replaced and a novel base span toward deleted, \
             and leaves the chars outside them alone",
        );
        assert_eq!(
            paint(1.0),
            [Some(tints.deleted), Some(tints.deleted), syntax],
            "the top level hands each base span the deleted color outright",
        );
    }

    /// Raising the dial adds color to the changed rows, so leaving the rest of
    /// the screen at full saturation trades one competing palette for two. The
    /// same fraction drains the unchanged rows instead, which is what makes the
    /// top read as status colors on a gray field.
    ///
    /// Softening is off throughout, which is the point. The gray answers the
    /// tint dial, so it holds with the soften dial turned all the way down.
    #[test]
    fn the_tint_amount_drains_an_unchanged_row_toward_its_own_gray() {
        let tints = rgb_tints();
        let fg = [200, 100, 50];

        let paint = |amount: f32| {
            let mut buf = Buffer::empty(Rect::new(0, 0, 6, 1));
            paint_base_row(
                &mut buf,
                0,
                0,
                "word",
                4,
                4,
                &[],
                Style::default().fg(Color::Rgb(fg[0], fg[1], fg[2])),
                &[],
                Some(&tints),
                Some(tints.bg),
                None,
                DiffDials {
                    soften_scale: 0.0,
                    tint_amount: amount,
                    ..DiffDials::shipped()
                },
                None,
            );
            buf[(0, 0)].style().fg
        };

        let gray = [(luma(fg) * 255.0).round() as u8; 3];
        let half = dim_rgb(fg, gray, 0.5);

        assert_eq!(
            (paint(0.0), paint(0.5), paint(1.0)),
            (
                Some(Color::Rgb(fg[0], fg[1], fg[2])),
                Some(Color::Rgb(half[0], half[1], half[2])),
                Some(Color::Rgb(gray[0], gray[1], gray[2])),
            ),
            "the off position keeps the syntax color, a mid level drains half \
             of it, and the top leaves the char its own brightness in gray",
        );
    }

    /// The gray is for unchanged rows alone. A changed row carries a row tint
    /// instead, and the two never meet on one row, so nothing the reader is
    /// looking for loses its color.
    #[test]
    fn a_changed_row_takes_no_gray_from_the_tint_amount() {
        let tints = rgb_tints();
        let fg = [200, 100, 50];

        let paint = |soften_gaps, tint_row| {
            let mut buf = Buffer::empty(Rect::new(0, 0, 6, 1));
            paint_base_row(
                &mut buf,
                0,
                0,
                "word",
                4,
                4,
                &[],
                Style::default().fg(Color::Rgb(fg[0], fg[1], fg[2])),
                &[(0..1, ChangeKind::Replaced, ProseChange::None)],
                Some(&tints),
                None,
                soften_gaps,
                DiffDials {
                    soften_scale: 0.0,
                    tint_amount: 1.0,
                    ..DiffDials::shipped()
                },
                tint_row,
            );
            buf[(2, 0)].style().fg
        };

        assert_eq!(
            (
                paint(Some(tints.bg), Some(tints.deleted)),
                paint(None, Some(tints.added)),
            ),
            (Some(tints.deleted), Some(tints.added)),
            "a refined row's gap cell and a span-less row both take their row \
             status outright, with no gray between",
        );
    }

    /// A refined line is mostly chars the refinement never matched, so the row
    /// status has to reach them too. The spans keep their own kinds over it,
    /// which is what proves the row tint lands beside them rather than under
    /// them. A tint folded in first leaves a span blended through two colors.
    ///
    /// Softening is off throughout, which turns the contrast lift off with it,
    /// so what these expectations read is the dial's own arithmetic.
    #[test]
    fn the_tint_amount_walks_a_refined_row_toward_its_row_status() {
        let tints = rgb_tints();
        let fg = [200, 100, 50];
        let change_spans = vec![
            (0..3, ChangeKind::Replaced, ProseChange::None),
            (3..6, ChangeKind::Novel, ProseChange::None),
        ];

        let paint = |amount: f32| {
            let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
            paint_base_row(
                &mut buf,
                0,
                0,
                "abcdefgh",
                8,
                4,
                &[],
                Style::default().fg(Color::Rgb(fg[0], fg[1], fg[2])),
                &change_spans,
                Some(&tints),
                None,
                None,
                DiffDials {
                    soften_scale: 0.0,
                    tint_amount: amount,
                    ..DiffDials::shipped()
                },
                Some(tints.deleted),
            );
            [0u16, 3, 6].map(|x| buf[(x, 0)].style().fg)
        };

        let blend = |target: Color| {
            let target = style_rgb(Some(target)).expect("an rgb status color");
            let [r, g, b] = dim_rgb(fg, target, 0.5);
            Some(Color::Rgb(r, g, b))
        };

        assert_eq!(
            paint(0.5),
            [
                blend(tints.deleted),
                blend(tints.deleted),
                blend(tints.deleted)
            ],
            "each base span leans toward deleted from the syntax color once, and \
             the char outside them leans toward the row's status",
        );
        assert_eq!(
            paint(1.0),
            [
                Some(tints.deleted),
                Some(tints.deleted),
                Some(tints.deleted)
            ],
            "the top level hands each base span the deleted color and every \
             other char the row status",
        );
    }

    /// A wholly added or deleted line carries no change spans, so the row
    /// itself has to take the status color. Without this the dial colors every
    /// refined line and leaves the plainest ones -- a pure insertion or
    /// deletion -- reading as unchanged text.
    ///
    /// Rendered with softening off, which turns the contrast lift off with it,
    /// so each row reads the status color exactly rather than a lifted one.
    #[test]
    fn a_full_tint_paints_a_span_less_row_its_row_status_color() {
        let tints = rgb_tints();
        let theme = rgb_diff_theme();
        let area = Rect::new(0, 0, 120, 6);
        let fallback = theme.get(crate::theme::scope::UI_TEXT);

        let mut editor = diff_editor("keep\ntail\n", "keep\nadded\ntail\n");
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            fallback,
            &theme,
            &mut buf,
            true,
            None,
            None,
            true,
            DiffDials {
                soften_scale: 0.0,
                tint_amount: 1.0,
                ..DiffDials::shipped()
            },
            None,
        );

        let rx = editor
            .text_rect
            .expect("the render records the text rect")
            .x;
        let row = (0..area.height)
            .find(|&y| buffer_text(&buf, y).contains("added"))
            .expect("the added line renders");
        let colors: Vec<_> = (rx..rx + 5).map(|x| buf[(x, row)].style().fg).collect();
        assert_eq!(
            colors,
            vec![Some(tints.added); 5],
            "every char of a pure insertion takes the added color",
        );

        let mut editor = diff_editor("keep\ngone\ntail\n", "keep\ntail\n");
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            fallback,
            &theme,
            &mut buf,
            true,
            None,
            None,
            true,
            DiffDials {
                soften_scale: 0.0,
                tint_amount: 1.0,
                ..DiffDials::shipped()
            },
            None,
        );

        let lx = two_digit_columns(area).left_text_x;
        let row = (0..area.height)
            .find(|&y| buffer_text(&buf, y).contains("gone"))
            .expect("the deleted base line renders");
        let colors: Vec<_> = (lx..lx + 4).map(|x| buf[(x, row)].style().fg).collect();
        assert_eq!(
            colors,
            vec![Some(tints.deleted); 4],
            "every char of a pure deletion takes the deleted color",
        );
    }

    /// A refined modified line names its changed chars, so the tint dial
    /// colors those alone on both sides, and the chars around them stay
    /// unchanged text in the pane color.
    ///
    /// Rendered with softening off, which turns the contrast lift off with it,
    /// so each cell reads its status color exactly rather than a lifted one.
    #[test]
    fn a_full_tint_leaves_the_unchanged_chars_of_a_refined_row_untinted() {
        let tints = rgb_tints();
        let theme = rgb_diff_theme();
        let area = Rect::new(0, 0, 120, 6);
        let fallback = Style::default().fg(Color::Rgb(200, 200, 200));

        let mut editor = diff_editor("keep\nold mid tail\n", "keep\nnew mid tail\n");
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            fallback,
            &theme,
            &mut buf,
            true,
            None,
            None,
            true,
            DiffDials {
                soften_scale: 0.0,
                tint_amount: 1.0,
                ..DiffDials::shipped()
            },
            None,
        );

        let row = (0..area.height)
            .find(|&y| buffer_text(&buf, y).contains("new mid tail"))
            .expect("the modified line renders");
        let rx = editor
            .text_rect
            .expect("the render records the text rect")
            .x;
        let lx = two_digit_columns(area).left_text_x;
        let colors = |x0: u16, len: u16| -> Vec<_> {
            (x0..x0 + len).map(|x| buf[(x, row)].style().fg).collect()
        };

        assert_eq!(
            (
                colors(rx, 3),
                colors(rx + 3, 9),
                colors(lx, 3),
                colors(lx + 3, 9),
            ),
            (
                vec![Some(tints.modified); 3],
                vec![fallback.fg; 9],
                vec![Some(tints.deleted); 3],
                vec![fallback.fg; 9],
            ),
            "the replaced word reads modified on the right and deleted on the \
             left, and both sides keep the pane color outside it",
        );
    }

    /// The colors of 12 cells of the refined pair `old mid tail` and `new mid
    /// tail`, from the column `x0` gives for the area.
    ///
    /// Rendered 120 wide with the tint dial and softening off, over a pane
    /// color of `(200, 200, 200)`.
    fn refined_pair_colors(x0: impl Fn(Rect) -> u16) -> Vec<Option<Color>> {
        let area = Rect::new(0, 0, 120, 6);
        let mut editor = diff_editor("keep\nold mid tail\n", "keep\nnew mid tail\n");
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default().fg(Color::Rgb(200, 200, 200)),
            &rgb_diff_theme(),
            &mut buf,
            true,
            None,
            None,
            true,
            DiffDials {
                soften_scale: 0.0,
                ..DiffDials::shipped()
            },
            None,
        );

        let row = (0..area.height)
            .find(|&y| buffer_text(&buf, y).contains("new mid tail"))
            .expect("the refined pair renders");
        let x0 = x0(area);
        (x0..x0 + 12).map(|x| buf[(x, row)].style().fg).collect()
    }

    /// With the dial off, a refined live line marks its changed chars with the
    /// added color and paints the rest as the unchanged text it is, as its base
    /// side does.
    #[test]
    fn a_refined_live_row_paints_its_unchanged_chars_in_the_text_color() {
        let colors = refined_pair_colors(|area| two_digit_columns(area).right_text_x);
        assert_eq!(
            (&colors[..3], &colors[3..]),
            (
                &[Some(Color::Rgb(0, 255, 0)); 3][..],
                &[Some(Color::Rgb(200, 200, 200)); 9][..],
            ),
            "the replaced word reads added, and the rest of the line reads as \
             the pane's text",
        );
    }

    /// With the dial off, a refined base line marks its changed chars red and
    /// paints the rest as the unchanged text it is, not as removed text.
    #[test]
    fn a_refined_base_row_paints_its_unchanged_chars_in_the_text_color() {
        let colors = refined_pair_colors(|area| two_digit_columns(area).left_text_x);
        assert_eq!(
            (&colors[..3], &colors[3..]),
            (
                &[Some(Color::Rgb(255, 0, 0)); 3][..],
                &[Some(Color::Rgb(200, 200, 200)); 9][..],
            ),
            "the replaced word reads removed, and the rest of the line reads as \
             the pane's text",
        );
    }

    /// A zero scale must skip the soften call rather than pass it a zero
    /// amount, because that call drops bold whatever amount it is given.
    #[test]
    fn a_zero_soften_scale_leaves_the_row_untouched() {
        let bold = Style::default()
            .fg(Color::Rgb(200, 100, 50))
            .add_modifier(Modifier::BOLD);
        let bg = [0, 0, 0];

        let paint = |scale: f32| {
            let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
            paint_base_row(
                &mut buf,
                0,
                0,
                "word",
                4,
                4,
                &[],
                bold,
                &[],
                Some(&rgb_tints()),
                Some(bg),
                None,
                DiffDials {
                    soften_scale: scale,
                    ..DiffDials::shipped()
                },
                None,
            );
            (buf[(0, 0)].style().fg, buf[(0, 0)].modifier)
        };

        assert_eq!(
            paint(0.0),
            (Some(Color::Rgb(200, 100, 50)), Modifier::BOLD),
            "a stopped soften keeps both the color and the weight",
        );

        let [r, g, b] = dim_rgb([200, 100, 50], bg, CONTEXT_SOFTEN * 2.0);
        assert_eq!(
            paint(2.0),
            (Some(Color::Rgb(r, g, b)), Modifier::empty()),
            "a doubled soften blends twice as far and drops the weight",
        );
    }

    /// The background of [`minimap_palette_theme`], as a theme source writes it
    /// and as the blends read it, and the theme's diff colors.
    const MINIMAP_BG_SOURCE: &str = "\"#282c34\"";
    const MINIMAP_BG: [u8; 3] = [0x28, 0x2c, 0x34];
    const MINIMAP_ADDED: [u8; 3] = [0x00, 0xff, 0x00];
    const MINIMAP_MODIFIED: [u8; 3] = [0xff, 0xff, 0x00];
    const MINIMAP_DELETED: [u8; 3] = [0xff, 0x00, 0x00];
    const MINIMAP_MOVED: [u8; 3] = [0x00, 0x00, 0xff];

    /// A theme with the diff colors above over `background`, a text color, and
    /// three syntax colors. The comment color is near the background, so every
    /// blend of the minimap palette moves some entry.
    fn minimap_palette_theme(background: &str) -> Theme {
        let src = format!(
            r##"theme t {{
                diff.added.fg     = "#00ff00";
                diff.modified.fg  = "#ffff00";
                diff.deleted.fg   = "#ff0000";
                diff.moved.fg     = "#0000ff";
                ui.background.bg  = {background};
                ui.text.fg        = "#abb2bf";
                syntax.keyword.fg = "#c678dd";
                syntax.comment.fg = "#5c6370";
                syntax.string.fg  = "#98c379";
            }}"##
        );
        let (config, errors) = stoat_config::parse(&src);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        Theme::from_config(&config.expect("the theme parses"), "t").expect("theme t exists")
    }

    /// The class table of [`minimap_palette_theme`] over `background`, and the
    /// diff minimap palette derived from it under `dials`.
    fn minimap_palette(
        background: &str,
        dials: DiffDials,
        syntax: bool,
    ) -> (ClassTable, Vec<[u8; 3]>) {
        let theme = minimap_palette_theme(background);
        let table = ClassTable::from_theme(&theme);
        let palette = diff_minimap_palette(&table, &theme, dials, syntax);
        (table, palette)
    }

    /// `table`'s syntax block with its uncovered-text entry set to `head`, and
    /// every entry passed through `blend`.
    fn blended_block(
        table: &ClassTable,
        head: [u8; 3],
        blend: impl Fn([u8; 3]) -> [u8; 3],
    ) -> Vec<[u8; 3]> {
        let mut block = table.palette()[..usize::from(table.stride())].to_vec();
        block[0] = head;
        block.into_iter().map(blend).collect()
    }

    /// `table`'s own palette with its unchanged block set to `unchanged`, and
    /// its added, modified, moved, and deleted blocks set to `changed`. A test
    /// then asserts every entry, the edge classes among them, at once.
    fn palette_with_blocks(
        table: &ClassTable,
        unchanged: Vec<[u8; 3]>,
        changed: [Vec<[u8; 3]>; 4],
    ) -> Vec<[u8; 3]> {
        let stride = usize::from(table.stride());
        let mut palette = table.palette().to_vec();
        palette[..stride].copy_from_slice(&unchanged);
        let starts = [
            table.population_base(DiffStatus::Added),
            table.population_base(DiffStatus::Modified),
            table.population_base(DiffStatus::Moved),
            table.deleted_base(),
        ];
        for (start, block) in starts.into_iter().zip(changed) {
            palette[usize::from(start)..][..stride].copy_from_slice(&block);
        }
        palette
    }

    #[test]
    fn a_minimap_block_takes_the_colors_its_diff_rows_paint() {
        let (table, palette) = minimap_palette(MINIMAP_BG_SOURCE, DiffDials::shipped(), true);
        let lifted = |head| blended_block(&table, head, |c| brighten_rgb(c, MINIMAP_BG));
        let receded = blended_block(&table, table.palette()[0], |c| {
            dim_rgb(c, MINIMAP_BG, CONTEXT_SOFTEN)
        });
        assert_eq!(
            palette,
            palette_with_blocks(
                &table,
                receded,
                [
                    lifted(MINIMAP_ADDED),
                    lifted(MINIMAP_ADDED),
                    lifted(MINIMAP_MOVED),
                    lifted(MINIMAP_DELETED),
                ],
            ),
            "an unchanged block recedes, a changed block lifts its tokens and its \
             row color, and a modified row's uncovered text takes the added color",
        );
    }

    #[test]
    fn the_top_tint_paints_changed_minimap_blocks_flat_and_drains_the_rest() {
        let dials = DiffDials {
            tint_amount: 1.0,
            ..DiffDials::shipped()
        };
        let (table, palette) = minimap_palette(MINIMAP_BG_SOURCE, dials, true);
        let flat = |status| vec![brighten_rgb(status, MINIMAP_BG); usize::from(table.stride())];
        let gray = blended_block(&table, table.palette()[0], |c| {
            [(luma(dim_rgb(c, MINIMAP_BG, CONTEXT_SOFTEN)) * 255.0).round() as u8; 3]
        });
        assert_eq!(
            palette,
            palette_with_blocks(
                &table,
                gray,
                [
                    flat(MINIMAP_ADDED),
                    flat(MINIMAP_MODIFIED),
                    flat(MINIMAP_MOVED),
                    flat(MINIMAP_DELETED),
                ],
            ),
        );
    }

    /// Softening off with the tint dial off blends nothing in the diff paint, and
    /// so does a background with no RGB at any dial.
    #[test]
    fn the_minimap_palette_blends_nothing_the_diff_paint_leaves_unblended() {
        let soften_off = DiffDials {
            soften_scale: 0.0,
            ..DiffDials::shipped()
        };
        let top_tint = DiffDials {
            tint_amount: 1.0,
            ..DiffDials::shipped()
        };
        for (background, dials) in [(MINIMAP_BG_SOURCE, soften_off), ("blue", top_tint)] {
            let (table, palette) = minimap_palette(background, dials, true);
            let raw = |head| blended_block(&table, head, |c| c);
            assert_eq!(
                palette,
                palette_with_blocks(
                    &table,
                    raw(table.palette()[0]),
                    [
                        raw(MINIMAP_ADDED),
                        raw(MINIMAP_ADDED),
                        raw(MINIMAP_MOVED),
                        raw(MINIMAP_DELETED),
                    ],
                ),
                "over {background}",
            );
        }
    }

    #[test]
    fn syntax_off_paints_each_minimap_block_in_its_row_color() {
        let (table, palette) = minimap_palette(MINIMAP_BG_SOURCE, DiffDials::shipped(), false);
        let stride = usize::from(table.stride());
        let flat = |row| vec![brighten_rgb(row, MINIMAP_BG); stride];
        assert_eq!(
            palette,
            palette_with_blocks(
                &table,
                vec![dim_rgb(table.palette()[0], MINIMAP_BG, CONTEXT_SOFTEN); stride],
                [
                    flat(MINIMAP_ADDED),
                    flat(MINIMAP_ADDED),
                    flat(MINIMAP_MOVED),
                    flat(MINIMAP_DELETED),
                ],
            ),
        );
    }

    #[test]
    fn paint_base_row_underlines_change_spans_on_a_theme_that_cannot_blend() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        let change_spans = vec![
            (0..3, ChangeKind::Replaced, ProseChange::None),
            (3..6, ChangeKind::Moved, ProseChange::None),
        ];
        paint_base_row(
            &mut buf,
            0,
            0,
            "abcdefgh",
            8,
            4,
            &[],
            Style::default(),
            &change_spans,
            None,
            None,
            None,
            DiffDials::shipped(),
            None,
        );

        for x in 0..6 {
            assert!(
                buf[(x, 0)].modifier.contains(Modifier::UNDERLINED),
                "col {x} underlines when the theme cannot blend"
            );
            assert_eq!(
                buf[(x, 0)].bg,
                Color::Reset,
                "and still paints no background"
            );
        }
        assert!(
            !buf[(6, 0)].modifier.contains(Modifier::UNDERLINED),
            "a char outside every span is not underlined"
        );
    }

    #[test]
    fn move_chip_paints_text_after_two_col_gap() {
        let area = Rect::new(0, 0, 50, 1);
        let mut buf = Buffer::empty(area);
        render_move_chip(&mut buf, 0, 0, 5, 50, Some("a.rs"), 0, Style::default());
        let text = buffer_text(&buf, 0);
        assert_eq!(&text[..7], "       ", "5-col text + 2-col gap before chip");
        assert_eq!(&text[7..16], "<- a.rs:1", "chip text follows the gap");
    }

    #[test]
    fn move_chip_no_op_when_text_fills_max_cols() {
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        render_move_chip(
            &mut buf,
            0,
            0,
            19,
            20,
            Some("long_name.rs"),
            100,
            Style::default(),
        );
        let text = buffer_text(&buf, 0);
        assert!(
            !text.contains("<-"),
            "chip must not paint when text fills max_cols; got {text:?}"
        );
    }

    #[test]
    fn move_chip_truncates_when_room_runs_out() {
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        render_move_chip(
            &mut buf,
            0,
            0,
            5,
            20,
            Some("long_name.rs"),
            99,
            Style::default(),
        );
        let text = buffer_text(&buf, 0);
        // text_cols=5 + 2-col gap = chip starts at col 7; max_cols=20 leaves 13 cols.
        // "<- long_name.rs:100" is 19 chars; truncated to 13: "<- long_name.".
        assert_eq!(&text[7..20], "<- long_name.", "chip truncates to fit");
    }

    #[test]
    fn move_chip_uses_one_based_line_number() {
        let area = Rect::new(0, 0, 30, 1);
        let mut buf = Buffer::empty(area);
        render_move_chip(&mut buf, 0, 0, 0, 30, Some("x.rs"), 41, Style::default());
        let text = buffer_text(&buf, 0);
        assert!(
            text.contains("<- x.rs:42"),
            "chip prints 1-based line number; got {text:?}"
        );
    }

    #[test]
    fn move_chip_intra_file_omits_the_path() {
        let area = Rect::new(0, 0, 30, 1);
        let mut buf = Buffer::empty(area);
        render_move_chip(&mut buf, 0, 0, 0, 30, None, 41, Style::default());
        let text = buffer_text(&buf, 0);
        assert!(
            text.contains("<- 42") && !text.contains(':'),
            "intra-file chip shows the 1-based line without a path; got {text:?}"
        );
    }
}

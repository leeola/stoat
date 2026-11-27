//! Renders stoat's surfaces into stoatty's smooth-scroll pool slots.
//!
//! [`stoat_widgets::pool`] owns the state machine: which region is declared,
//! which pages are buffered, and where each pool's scroll target sits. This
//! module supplies the two things that machine has no way to know, the page
//! bytes for each of stoat's surfaces and the pool id a surface scrolls under.
//!
//! Editor pools are keyed by [`crate::pane::Pane::index`], a stable per-pane id,
//! so a pane keeps the same pool across frames. The overlays and popups take
//! fixed ids from [`non_pane_pool`], reserved above every pane index so the two
//! never collide.
//!
//! A "page" is one region-sized screen of the document: `region.height` rows of
//! `region.width` columns, the page at index `p` starting at document row
//! `p * region.height`. [`page_top_row`] maps a page index to that row.

use crate::{
    blame::{BlameBase, BlameRows},
    code_search::CodeSearchFinder,
    command_palette::{ArgPicker, Namespace, PaletteCommand},
    commit_list::CommitListState,
    completion::CompletionPopup,
    conflict_session::ConflictViewState,
    diagnostic_spans::ResolvedDiag,
    display_map::{highlights::HighlightEndpoints, DisplayPoint, DisplaySnapshot},
    file_finder::FileFinder,
    help::Help,
    keymap::ActionKeys,
    render::{
        blame::{self, BlameColors, BlameLines, BLAME_COLUMN_CELLS},
        code_search::paint_match_rows,
        command_palette::{arg_rows, command_page_start, paint_palette_rows},
        commits::paint_commit_rows,
        completion::paint_completion_rows,
        conflict_view::{render_conflict_rows, GridPaint},
        editor::{
            draw_fallback_line_numbers, gutter_background, gutter_component_lines,
            gutter_diff_marks, gutter_digits, gutter_geometry, line_display_rows,
            measure_gutter_width, paint_chunk_rows, paint_diagnostic_spans, paint_line_diagnostics,
            paint_severity_column, rich_gutter, severity_rows, visible_byte_range, wrap_mark_style,
            DiagnosticPaintScratch, EolCursor, RichGutterColors, RowSeverity, SeverityColors,
        },
        file_finder::paint_finder_rows,
        help::{paint_help_detail_rows, paint_help_list_rows},
        paint::dim_rgb,
        picker::{
            page_start, paint_preview_numbers, paint_target_row, NumberColumn, RowMarks, RowStart,
        },
        pixel_rows::RowMetrics,
        review::{diff_gutter_digits, paint_diff_rows, BufferNumbering, DiffDials},
        serialize_buffer,
        symbol_finder::paint_symbol_rows,
        undercurl::{self, UndercurlBatch},
        walkthrough::Spotlight,
    },
    symbol_finder::SymbolFinder,
    user_command::run::CliSpecState,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::StatefulWidget,
};
use std::{collections::HashMap, ops::Range, path::Path, sync::Arc};
use stoat_widgets::{bar::Bar, ApcScene};
use stoatty_protocol::command::{encode_fill_decorations_scope, encode_fill_scope};

/// Pool ids for the non-pane smooth-scroll surfaces (overlays and popups).
///
/// Split-pane editor pools key on [`crate::pane::Pane::index`], a monotonic
/// `u32` counter from 1, so reserving every non-pane id at or above [`BASE`]
/// keeps them from ever colliding with a pane. The renderer composites pools in
/// ascending-id z-order, so these higher ids always composite above the
/// split-pane editors beneath them. Each surface's emit consumes its id from
/// here as it lands.
pub(crate) mod non_pane_pool {
    use stoatty_protocol::command::NON_PANE_POOL_BASE;

    /// First id reserved for non-pane surfaces. Panes occupy `[1, BASE)`.
    ///
    /// The compositor keys the same split off
    /// [`NON_PANE_POOL_BASE`](stoatty_protocol::command::NON_PANE_POOL_BASE) to
    /// decide which pools a modal box occludes, so both sides share that
    /// constant.
    pub(crate) const BASE: u32 = NON_PANE_POOL_BASE;
    pub(crate) const COMMITS: u32 = BASE;
    pub(crate) const FINDER: u32 = BASE + 1;
    pub(crate) const PALETTE: u32 = BASE + 2;
    pub(crate) const COMPLETION: u32 = BASE + 3;
    pub(crate) const HELP_LIST: u32 = BASE + 4;
    pub(crate) const HELP_DETAIL: u32 = BASE + 5;
    // No surface emit reads these two reserved ids.
    #[allow(dead_code)]
    pub(crate) const SYMBOL: u32 = BASE + 6;
    #[allow(dead_code)]
    pub(crate) const WORKSPACE_SYMBOL: u32 = BASE + 7;
    pub(crate) const HOVER: u32 = BASE + 8;
    pub(crate) const COMMIT_PICKER_LIST: u32 = BASE + 9;
    pub(crate) const COMMIT_PICKER_PREVIEW: u32 = BASE + 10;
    pub(crate) const CODE_SEARCH_LIST: u32 = BASE + 11;
    pub(crate) const SYMBOL_FINDER_LIST: u32 = BASE + 12;
    /// The open modal's preview pane. Modals are mutually exclusive, so one id
    /// serves every preview rather than one per modal.
    pub(crate) const MODAL_PREVIEW: u32 = BASE + 13;
    /// The changes view's minimap thumb, which no pool carries.
    ///
    /// It keys the thumb's change gate, so a strip that the changes view places
    /// a thumb on and an editor feeds after it gets the editor's thumb back.
    pub(crate) const DIFF_CHANGES_THUMB: u32 = BASE + 14;
    /// The Job Tray of the tab bar at the board's pixels, a region with no
    /// page where a press on a chip reports the pointer's pixel.
    pub(crate) const JOB_TRAY: u32 = BASE + 15;
    /// First id of the per-window status-bar partition. A detached pane's status
    /// row pools at `WINDOW_STATUS + pane.index`, so the partition is offset far
    /// enough above the fixed non-pane ids that pane indices never collide.
    pub(crate) const WINDOW_STATUS: u32 = BASE + 0x100;
}

/// Display row page `index` starts at, for a pool whose regions are
/// `region_height` rows tall.
///
/// Shared with the callers that resolve highlights across a run of pages, so the
/// span they resolve and the rows the pages paint cannot come apart.
pub(crate) fn page_top_row(index: u64, region_height: u16) -> u32 {
    index
        .saturating_mul(region_height as u64)
        .min(u32::MAX as u64) as u32
}

/// Render page `index` from `snapshot` and wrap it in the pool fill frames, so the
/// returned bytes are a self-contained fill the terminal applies to slot `index`.
///
/// The asynchronous editor-fill path runs this on a blocking worker and delivers
/// the frame through the APC channel, off the run loop. The bytes are one
/// [`encode_fill_scope`] batch wrapping the [`render_page_from_snapshot`] page.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_page_fill(
    snapshot: &DisplaySnapshot,
    pool: u32,
    index: u64,
    fallback_style: Style,
    region_width: u16,
    region_height: u16,
    gutter: &PageGutter,
    diff_view: bool,
    dim: f32,
    dials: DiffDials,
    endpoints: Arc<HighlightEndpoints>,
    live: Option<&crate::diff_map::LiveHunks<'_>>,
    spotlight: Option<&Spotlight>,
) -> Vec<u8> {
    let top_row = page_top_row(index, region_height);
    let bytes = render_page_from_snapshot(
        snapshot,
        top_row,
        fallback_style,
        region_width,
        region_height,
        gutter,
        diff_view,
        dim,
        dials,
        endpoints,
        live,
        spotlight,
    );

    let mut frame = Vec::with_capacity(bytes.len() + 16);
    encode_fill_scope(&mut frame, pool, index, |out| out.extend_from_slice(&bytes));
    frame
}

/// Render the rich gutter components of page `index` alone and wrap them in a
/// decorations-only fill, for a page whose cells the terminal already holds.
///
/// A plain editor page lays nothing over its cells but these components, so the
/// batch replaces everything [`render_page_fill`] put there for the gutter.
/// Empty when `gutter` has no rich colors, since the fallback gutter paints its
/// numbers into the cells.
///
/// A diff page's decorations are its whole gutter: the numbers, the bars, and
/// the separators of both columns. They repaint together, because the terminal
/// replaces a page's decorations as one set and keeps its cells.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_page_decorations(
    snapshot: &DisplaySnapshot,
    pool: u32,
    index: u64,
    region_width: u16,
    region_height: u16,
    gutter: &PageGutter,
    diff_view: bool,
    fallback_style: Style,
    dim: f32,
    dials: DiffDials,
    live: Option<&crate::diff_map::LiveHunks<'_>>,
) -> Vec<u8> {
    if gutter.rich.is_none() {
        return Vec::new();
    }

    let top_row = page_top_row(index, region_height);
    let area = Rect::new(0, 0, region_width, region_height);
    if diff_view {
        // The terminal keeps the cells this paint writes, so they go to a
        // buffer nothing reads.
        let mut scene = gutter.scene();
        paint_diff_rows(
            snapshot,
            top_row,
            area,
            diff_gutter_digits(snapshot),
            fallback_style,
            gutter.theme(),
            &mut page_buffer(area, gutter.theme()),
            Some(&mut scene),
            dim,
            dials,
            gutter
                .current_line
                .map_or(BufferNumbering::Absolute, BufferNumbering::FromLine),
            &[],
            None,
            None,
            false,
        );
        let mut frame = Vec::with_capacity(scene.bytes().len() + 32);
        encode_fill_decorations_scope(&mut frame, pool, index, |out| {
            out.extend_from_slice(scene.bytes())
        });
        return frame;
    }

    let end_row = top_row
        .saturating_add(region_height as u32)
        .min(snapshot.line_count());
    // The terminal keeps the page's cells, so the cells this paint writes go to
    // a buffer nothing reads. The rich gutter and the Blame Column write none.
    // Only the letters of a numberless gutter write any, in its one column,
    // which sits past the Blame Column on a blamed page.
    let mut cells = Buffer::empty(Rect {
        width: area.width.min(BLAME_COLUMN_CELLS + 1),
        ..area
    });
    let (_, components) = paint_page_gutter(
        snapshot,
        top_row,
        end_row,
        &mut cells,
        area,
        gutter,
        fallback_style,
        dim,
        live,
    );

    let mut frame = Vec::with_capacity(components.len() + 32);
    encode_fill_decorations_scope(&mut frame, pool, index, |out| {
        out.extend_from_slice(&components)
    });
    frame
}

/// Paint `region_height` document rows starting at display row `top_row` from an
/// owned [`DisplaySnapshot`] into a self-contained VT byte stream.
///
/// Takes a snapshot rather than `&mut EditorState` so a page can render off the
/// run-loop thread. A [`DisplaySnapshot`] is `Send` and carries everything the
/// text needs.
///
/// `endpoints` may span more rows than this page, which is what lets the pages
/// of one refill share a single resolve. It must cover them, since anything it
/// omits paints unhighlighted.
///
/// Paints the line-number gutter, then the text and syntax highlights inset past
/// it, then the diagnostic spans of [`PageGutter::diagnostics()`] over the text.
/// A page of the focused editor also prints each line's diagnostic message after
/// the text. These are the cells the live grid paints for these rows, minus the
/// cursor and selection a pooled page never carries. Rows past the document end
/// stay blank, and the [`serialize_buffer`] bytes fully repaint the slot
/// regardless of its prior contents.
///
/// A page with a picker preview's number column then marks the row of the line
/// that [`PageGutter::target_line`] sets, as the live preview marks it.
///
/// Inside stoatty the rich gutter's sub-cell components ride as APC frames
/// appended after the serialized cells, so the terminal captures them onto the
/// page slot. Every other terminal gets degraded cell numbers in the buffer.
///
/// The curly and dotted lines under the diagnostic spans follow the cells as
/// raw VT, ahead of those frames, since a ratatui cell holds no such line.
///
/// `live` offers hunks a caller already resolved, for a run of pages over one
/// snapshot that would otherwise each pay an anchor batch over every hunk in the
/// file. `None` resolves them here, which is what a lone page wants.
///
/// `spotlight` dims the page and lights the walkthrough annotation on it, as the
/// live grid does for the focused editor. A diff-view page takes none, since its
/// columns are not the display columns a spotlight's range maps to.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_page_from_snapshot(
    snapshot: &DisplaySnapshot,
    top_row: u32,
    fallback_style: Style,
    region_width: u16,
    region_height: u16,
    gutter: &PageGutter,
    diff_view: bool,
    dim: f32,
    dials: DiffDials,
    endpoints: Arc<HighlightEndpoints>,
    live: Option<&crate::diff_map::LiveHunks<'_>>,
    spotlight: Option<&Spotlight>,
) -> Vec<u8> {
    let area = Rect::new(0, 0, region_width, region_height);
    let mut buf = page_buffer(area, gutter.theme());

    // A diff-view page paints the two columns itself, including both line-number
    // gutters, so it bypasses the single-column page gutter. Inside stoatty its
    // rich sub-cell components ride as APC frames appended after the serialized
    // cells, faded to match the dimmed grid.
    if diff_view {
        let mut scene = gutter.rich.is_some().then(|| gutter.scene());
        paint_diff_rows(
            snapshot,
            top_row,
            area,
            diff_gutter_digits(snapshot),
            fallback_style,
            gutter.theme(),
            &mut buf,
            scene.as_mut(),
            dim,
            dials,
            gutter
                .current_line
                .map_or(BufferNumbering::Absolute, BufferNumbering::FromLine),
            &[],
            None,
            None,
            false,
        );
        dim_page(&mut buf, area, gutter.theme(), dim);
        let mut bytes = serialize_buffer(&buf);
        if let Some(mut scene) = scene {
            bytes.extend_from_slice(scene.buffer());
        }
        return bytes;
    }

    let end_row = top_row
        .saturating_add(region_height as u32)
        .min(snapshot.line_count());

    let (gutter_w, apc) = paint_page_gutter(
        snapshot,
        top_row,
        end_row,
        &mut buf,
        area,
        gutter,
        fallback_style,
        dim,
        live,
    );

    let mut undercurls = UndercurlBatch::default();
    if end_row > top_row {
        paint_chunk_rows(
            snapshot.highlighted_chunks_with_endpoints(top_row..end_row, endpoints),
            area.x + gutter_w,
            area.x + area.width,
            area.y,
            area.y + area.height,
            fallback_style,
            fallback_style.patch(gutter.theme().get(crate::theme::scope::UI_VIRTUAL_INLAY)),
            fallback_style.patch(wrap_mark_style(gutter.theme())),
            &mut buf,
        );

        if !gutter.diagnostics.spans.is_empty() {
            let rope = snapshot.buffer_snapshot().rope();
            let visible = visible_byte_range(snapshot, rope, top_row, end_row);
            let inner = Rect {
                x: area.x + gutter_w,
                y: area.y,
                width: area.width.saturating_sub(gutter_w),
                height: area.height,
            };
            paint_diagnostic_spans(
                gutter.diagnostics.overlapping(&visible),
                &mut DiagnosticPaintScratch::default(),
                visible,
                rope,
                snapshot,
                gutter.theme(),
                fallback_style,
                top_row,
                end_row,
                inner,
                area.x + area.width,
                area.y + area.height,
                &mut buf,
                Some(&mut undercurls),
                gutter.diagnostics.colors.as_ref(),
                dim,
            );
            if let Some(cursor) = gutter.diagnostics.eol {
                paint_line_diagnostics(
                    &gutter.diagnostics.spans,
                    &gutter.diagnostics.messages,
                    cursor,
                    gutter.theme(),
                    snapshot,
                    top_row,
                    end_row,
                    inner,
                    area.x + area.width,
                    &mut buf,
                );
            }
        }
    }

    // A text style that sets a background writes it into the cells the text
    // takes, so the mark comes after the text.
    let marked = gutter.number_column != NumberColumn::None && gutter.target_line.is_some();
    let mark = marked.then(|| {
        let mut scene = gutter.scene();
        paint_target_row(
            snapshot,
            top_row,
            area,
            gutter.target_line,
            gutter.number_column == NumberColumn::Scaled,
            gutter.theme(),
            &mut buf,
            &mut scene,
        );
        scene.buffer().clone()
    });

    dim_page(&mut buf, area, gutter.theme(), dim);
    if let Some(spotlight) = spotlight {
        dim_page(&mut buf, area, gutter.theme(), spotlight.dim);
        light_page_range(&mut buf, area, gutter_w, top_row, snapshot, spotlight);
    }
    undercurl::snapshot_cells(&buf, undercurls.spans_mut());
    let mut bytes = serialize_buffer(&buf);
    bytes.extend(undercurl::build(&buf, undercurls.spans()));
    bytes.extend_from_slice(&apc);
    if let Some(mark) = mark {
        bytes.extend_from_slice(&mark);
    }
    bytes
}

/// Blend the page's cells toward the theme background, dimming a pooled page for
/// an unfocused pane the same way the live grid dims. A no-op when `dim` is zero
/// or the theme background is not RGB.
fn dim_page(buf: &mut Buffer, area: Rect, theme: &crate::theme::Theme, dim: f32) {
    if dim > 0.0
        && let Some(bg) = crate::render::paint::style_rgb(
            theme
                .try_get(crate::theme::scope::UI_BACKGROUND)
                .and_then(|s| s.bg),
        )
    {
        crate::render::pane::dim_pane_content(buf, area, bg, dim);
    }
}

/// Recolor the cells of `spotlight`'s range that fall on this page, which
/// starts at display row `top_row` with its text `gutter_w` cells in.
///
/// This is the rule the live walkthrough spotlight paints on the grid, so the
/// composite a glide shows matches it. The first row lights from the range's
/// start and the last up to its end, and every row between lights from the
/// line's start to its end.
fn light_page_range(
    buf: &mut Buffer,
    area: Rect,
    gutter_w: u16,
    top_row: u32,
    snapshot: &DisplaySnapshot,
    spotlight: &Spotlight,
) {
    let (start, end) = spotlight_display_span(snapshot, spotlight);
    let [r, g, b] = spotlight.color;

    let text_x = u32::from(area.x + gutter_w);
    let right = u32::from(area.x + area.width);
    let page_end = top_row.saturating_add(u32::from(area.height));
    for row in start.row.max(top_row)..end.row.saturating_add(1).min(page_end) {
        let from = if row == start.row { start.column } else { 0 };
        let to = if row == end.row {
            end.column + 1
        } else {
            snapshot.line_len(row)
        };
        let y = area.y + (row - top_row) as u16;
        let columns = (text_x + from).min(right)..(text_x + to).min(right);
        for x in columns {
            buf[(x as u16, y)].set_fg(Color::Rgb(r, g, b));
        }
    }
}

/// The display points of the first and last character `spotlight` lights.
///
/// A page lights the rows between the two, which is also what decides whether a
/// spotlight move changes that page.
pub(crate) fn spotlight_display_span(
    snapshot: &DisplaySnapshot,
    spotlight: &Spotlight,
) -> (DisplayPoint, DisplayPoint) {
    let rope = snapshot.buffer_snapshot().rope();
    let display = |offset: usize| snapshot.buffer_to_display(rope.offset_to_point(offset));
    (
        display(*spotlight.range.start()),
        display(*spotlight.range.end()),
    )
}

/// The gutter inputs an off-run-loop editor page render needs to paint the
/// line-number gutter identically to the live render.
///
/// [`Self::rich`] is `Some` only inside stoatty with every gutter color RGB. The
/// page then emits sub-cell components as APC frames. Otherwise it paints
/// degraded cell numbers styled from [`Self::theme`].
#[derive(Clone)]
pub(crate) struct PageGutter {
    line_numbers: bool,
    /// 1-based cursor buffer line for Helix-style relative numbering, or `None`
    /// to paint absolute numbers. Resolved on the run loop so a pooled page
    /// numbers relative to the cursor exactly as the live render does.
    current_line: Option<u32>,
    severity: Arc<RowSeverity>,
    theme: Arc<crate::theme::Theme>,
    rich: Option<RichGutterColors>,
    /// Whether the terminal reads `text_runs` and `bars` batches, which the
    /// scene a page paints its rich gutter into takes from [`Self::scene`].
    batch: bool,
    /// Whether the terminal stores an icon a page declares, which a terminal
    /// at [`stoatty_protocol::PAGE_ICONS_PROTOCOL`] or later does. A page for
    /// an earlier one marks each diagnostic with the hint's dot.
    icons: bool,
    /// The diagnostics a plain editor page paints over its text.
    diagnostics: PageDiagnostics,
    /// The column the page numbers its lines in as a picker preview does, in
    /// place of the line-number gutter and the severity column.
    number_column: NumberColumn,
    /// The 0-based line a picker preview's result lands on, which a page with
    /// a number column marks, or `None` for no mark.
    target_line: Option<u32>,
    /// The Blame Column of a blamed pane's page, or `None` for a page that
    /// shows no column.
    blame: Option<PageBlame>,
    /// The 1-based cursor line a plain editor's page fills, with the fill
    /// color, or `None` for a page that fills no line.
    cursor_line: Option<(u32, [u8; 3])>,
}

/// The Blame Column inputs that a blamed pane's page paints from, resolved on
/// the run loop.
///
/// A page shows the column when the live grid does, which is when the pane
/// blames and the theme resolves the column's colors.
#[derive(Clone)]
pub(crate) struct PageBlame {
    /// The colors as the live grid paints them, dimmed by the pane's fade.
    pub(crate) colors: BlameColors,
    /// The runs as the live grid last resolved them, or `None` before a blame
    /// of the buffer lands.
    pub(crate) rows: Option<Arc<BlameRows>>,
    /// HEAD's text and blame of the file, or `None` for a file HEAD does not
    /// hold.
    pub(crate) base: Option<Arc<BlameBase>>,
    /// The moment the run loop built the page, which a run's date measures its
    /// age against.
    pub(crate) now_epoch: i64,
    /// The index of the commit under the focused pane's cursor, whose runs
    /// draw as the selection, or `None` to select nothing.
    pub(crate) selected: Option<u32>,
}

impl PageGutter {
    /// Bundle the gutter inputs resolved on the run loop for an editor page fill.
    ///
    /// `line_numbers` off yields a page with no number gutter, matching a live
    /// render with the gutter disabled. Its text starts at column zero, or at
    /// column one where `severity` holds a diagnostic, which takes the
    /// one-column severity gutter. `current_line` selects relative numbering,
    /// and `None` paints absolute.
    pub(crate) fn new(
        line_numbers: bool,
        severity: Arc<RowSeverity>,
        theme: Arc<crate::theme::Theme>,
        rich: Option<RichGutterColors>,
        current_line: Option<u32>,
    ) -> PageGutter {
        PageGutter {
            line_numbers,
            current_line,
            severity,
            theme,
            rich,
            batch: false,
            icons: false,
            diagnostics: PageDiagnostics::default(),
            number_column: NumberColumn::None,
            target_line: None,
            blame: None,
            cursor_line: None,
        }
    }

    /// This gutter at the terminal's batch protocol.
    ///
    /// Off until set, so a page sends one frame per run and bar unless the run
    /// loop learned that the terminal reads batches.
    pub(crate) fn batched(self, batch: bool) -> PageGutter {
        PageGutter { batch, ..self }
    }

    /// This gutter at the terminal's page icon protocol.
    ///
    /// Off until set, so a page marks a diagnostic with a dot unless the run
    /// loop learned that the terminal stores page icons.
    pub(crate) fn page_icons(self, icons: bool) -> PageGutter {
        PageGutter { icons, ..self }
    }

    /// This gutter with the Blame Column of `blame` left of it.
    ///
    /// The column takes its cells only when the page has the room that
    /// [`blame::blame_cells`] asks for, as the live grid does.
    pub(crate) fn blamed(self, blame: Option<PageBlame>) -> PageGutter {
        PageGutter { blame, ..self }
    }

    /// This gutter filling the rows of `cursor_line`, the 1-based line and the
    /// fill color, with one bar under the page's text.
    ///
    /// Off until set. Only a page whose gutter draws rich runs draws the bar,
    /// and the line's number composites over the fill.
    pub(crate) fn cursor_line(self, cursor_line: Option<(u32, [u8; 3])>) -> PageGutter {
        PageGutter {
            cursor_line,
            ..self
        }
    }

    /// This gutter numbering a picker preview's lines in `column`.
    ///
    /// [`NumberColumn::None`] until set. With any other column the page paints
    /// the column the live preview paints and no line-number gutter or
    /// severity column, whatever [`Self::new`] took.
    ///
    /// See also:
    /// - [`paint_preview_numbers`] for the column's numbers.
    pub(crate) fn number_column(self, column: NumberColumn) -> PageGutter {
        PageGutter {
            number_column: column,
            ..self
        }
    }

    /// This gutter marking 0-based `line` as the row a picker preview's result
    /// lands on.
    ///
    /// `None` until set. A page with a [`Self::number_column`] marks the row
    /// as the live preview marks it, and any other page ignores the line.
    ///
    /// See also:
    /// - [`paint_target_row`] for the mark.
    pub(crate) fn target_line(self, line: Option<u32>) -> PageGutter {
        PageGutter {
            target_line: line,
            ..self
        }
    }

    /// This gutter with the `diagnostics` that a plain editor page paints over
    /// its text, as the live grid paints them.
    ///
    /// Empty until set, so a page paints no diagnostic span. A diff page
    /// ignores it, since a diff view paints no diagnostics.
    pub(crate) fn diagnostics(self, diagnostics: PageDiagnostics) -> PageGutter {
        PageGutter {
            diagnostics,
            ..self
        }
    }

    /// A scene for a page's rich gutter to paint into, at this gutter's batch
    /// protocol.
    fn scene(&self) -> ApcScene {
        let mut scene = ApcScene::new();
        scene.set_batch_protocol(self.batch);
        scene
    }

    /// The theme resolved on the run loop, reused by a diff-view page to style
    /// its two columns off the loop.
    pub(crate) fn theme(&self) -> &crate::theme::Theme {
        &self.theme
    }
}

/// The diagnostics a plain editor page paints over its text, resolved on the
/// run loop.
///
/// A page renders off the loop from a [`DisplaySnapshot`], where the editor's
/// anchored span cache does not reach. The spans come from the list that the
/// gutter refresh resolves for each set and buffer version, so a page costs no
/// resolve of its own.
///
/// `colors` is `None` when a severity color is not RGB. The page then paints
/// each span's severity style alone and stamps no curly or dotted line over
/// it, as the live grid does.
///
/// A page prints each line's message after its text only with an `eol`
/// record, which the last live paint leaves when it printed the messages.
#[derive(Clone, Default)]
pub(crate) struct PageDiagnostics {
    /// Every diagnostic of the buffer, in start order.
    pub(crate) spans: Arc<[ResolvedDiag]>,
    pub(crate) colors: Option<SeverityColors>,
    /// The first line of each diagnostic's message, by its index.
    pub(crate) messages: Arc<[Box<str>]>,
    /// The cursor inputs of the last live paint's messages, or `None` where
    /// that paint printed none, as the live grid of an unfocused pane prints
    /// none.
    pub(crate) eol: Option<EolCursor>,
}

impl PageDiagnostics {
    /// The spans that start before the end of `bytes` and end past its start,
    /// in start order.
    fn overlapping(&self, bytes: &Range<usize>) -> Vec<ResolvedDiag> {
        let starts_before_end = self.spans.partition_point(|span| span.start < bytes.end);
        self.spans[..starts_before_end]
            .iter()
            .filter(|span| span.end > bytes.start)
            .copied()
            .collect()
    }
}

/// Paint the page's gutter, returning the cell columns it reserves and the rich
/// APC frames to append after the page cells.
///
/// A blamed page paints the Blame Column first and the line gutter past it, as
/// [`paint_page_line_gutter`] paints it. The column's surface is a bar in the
/// returned frames, as the live grid draws it under stoatty.
///
/// A gutter with [`PageGutter::number_column`] set paints a picker preview's
/// number column in its place, with the numbers in the returned frames.
///
/// A gutter with a [`PageGutter::cursor_line`] opens the frames with the bar
/// that fills the line's rows, so the gutter and the page's text draw over it.
#[allow(clippy::too_many_arguments)]
fn paint_page_gutter(
    snapshot: &DisplaySnapshot,
    top_row: u32,
    end_row: u32,
    buf: &mut Buffer,
    area: Rect,
    gutter: &PageGutter,
    fallback_style: Style,
    dim: f32,
    live: Option<&crate::diff_map::LiveHunks<'_>>,
) -> (u16, Vec<u8>) {
    if gutter.number_column != NumberColumn::None {
        let mut scene = gutter.scene();
        let scaled = gutter.number_column == NumberColumn::Scaled;
        let width = paint_preview_numbers(
            snapshot,
            top_row,
            area,
            gutter.theme(),
            buf,
            &mut scene,
            scaled,
            gutter.target_line,
        );
        return (width, scene.buffer().clone());
    }

    let mut frames = cursor_line_bar(snapshot, top_row..end_row, area, gutter, dim, buf);
    let blame_w = gutter.blame.as_ref().map_or(0, |_| {
        blame::blame_cells(area.width, line_gutter_width(snapshot, gutter))
    });
    let Some(blame) = gutter.blame.as_ref().filter(|_| blame_w > 0) else {
        let (width, components) = paint_page_line_gutter(
            snapshot,
            top_row,
            end_row,
            buf,
            area,
            gutter,
            fallback_style,
            dim,
            live,
        );
        frames.extend_from_slice(&components);
        return (width, frames);
    };

    let mut scene = gutter.scene();
    let visible = end_row.saturating_sub(top_row).min(u32::from(area.height));
    let (folded, _) = gutter_geometry(snapshot, top_row, visible);
    let lines = blame.rows.as_deref().map(|rows| BlameLines {
        rows,
        base: blame.base.as_deref(),
        now_epoch: blame.now_epoch,
        selected: blame.selected,
    });
    blame::paint_blame_column(
        lines.as_ref(),
        &folded,
        Rect {
            width: blame_w,
            ..area
        },
        &blame.colors,
        buf,
        Some(&mut scene),
    );

    let past_column = Rect {
        x: area.x + blame_w,
        width: area.width - blame_w,
        ..area
    };
    let (width, components) = paint_page_line_gutter(
        snapshot,
        top_row,
        end_row,
        buf,
        past_column,
        gutter,
        fallback_style,
        dim,
        live,
    );
    frames.extend_from_slice(scene.buffer());
    frames.extend_from_slice(&components);
    (blame_w + width, frames)
}

/// The APC bytes of the bar that fills the rows of the gutter's cursor line
/// that `rows` shows, across the page's whole width.
///
/// The bar fades toward the gutter's background by `dim`, as the gutter's
/// other colors fade. A gutter with no cursor line or no rich colors, or a
/// line outside `rows`, gives none.
fn cursor_line_bar(
    snapshot: &DisplaySnapshot,
    rows: Range<u32>,
    area: Rect,
    gutter: &PageGutter,
    dim: f32,
    buf: &mut Buffer,
) -> Vec<u8> {
    let (Some((line, fill)), Some(rich)) = (gutter.cursor_line, gutter.rich.as_ref()) else {
        return Vec::new();
    };
    let line_rows = line_display_rows(snapshot, line);
    let (first, last) = (line_rows.start.max(rows.start), line_rows.end.min(rows.end));
    if first >= last {
        return Vec::new();
    }

    let mut scene = gutter.scene();
    Bar {
        x: 0,
        y: ((first - rows.start) * 16) as i16,
        width: area.width * 16,
        height: ((last - first) * 16) as u16,
        color: dim_rgb(fill, rich.bg, dim),
        radius: 0,
        corners: 0,
    }
    .render(area, buf, &mut scene);
    scene.buffer().clone()
}

/// The cells that the line-number gutter or the severity column of `gutter`
/// reserves on a page of `snapshot`, measured before the paint as the live
/// grid measures them.
fn line_gutter_width(snapshot: &DisplaySnapshot, gutter: &PageGutter) -> u16 {
    match (gutter.line_numbers, gutter.severity.is_empty()) {
        (true, _) => measure_gutter_width(
            gutter_digits(snapshot.buffer_line_count()),
            gutter.rich.as_ref().map(|rich| rich.number_scale),
        ),
        (false, true) => 0,
        (false, false) => 1,
    }
}

/// Paint the page's line-number gutter into `area`, returning the cell columns
/// it reserves and the rich APC frames to append after the page cells.
///
/// In rich mode the sub-cell components draw into a scratch scene whose bytes
/// ride the fill, leaving the gutter cells in `buf` at the page background. In
/// fallback mode the degraded numbers paint into `buf` and no bytes are
/// returned.
///
/// With line numbers off, a page whose rows carry a diagnostic keeps the
/// one-column severity gutter of the live grid. It marks each diagnostic row
/// with a bar in the returned frames under RGB severity colors, and with a
/// letter in `buf` otherwise. A page with no diagnostic answers `(0, empty)`,
/// so its text starts at the left of `area`.
#[allow(clippy::too_many_arguments)]
fn paint_page_line_gutter(
    snapshot: &DisplaySnapshot,
    top_row: u32,
    end_row: u32,
    buf: &mut Buffer,
    area: Rect,
    gutter: &PageGutter,
    fallback_style: Style,
    dim: f32,
    live: Option<&crate::diff_map::LiveHunks<'_>>,
) -> (u16, Vec<u8>) {
    if !gutter.line_numbers {
        if gutter.severity.is_empty() {
            return (0, Vec::new());
        }
        let rows = severity_rows(snapshot, &gutter.severity, top_row..end_row, area.height);
        let mut scene = gutter.diagnostics.colors.is_some().then(|| gutter.scene());
        paint_severity_column(
            &rows,
            Rect { width: 1, ..area },
            gutter.diagnostics.colors.as_ref(),
            gutter_background(gutter.theme(), fallback_style),
            dim,
            gutter.theme(),
            buf,
            scene.as_mut(),
        );
        return (
            1,
            scene.map_or_else(Vec::new, |mut scene| scene.buffer().clone()),
        );
    }

    let visible = end_row.saturating_sub(top_row).min(area.height as u32);
    let (folded, width_digits) = gutter_geometry(snapshot, top_row, visible);

    let diff_marks = match live {
        Some(live) => crate::render::editor::gutter_diff_marks_from(live, &folded),
        None => gutter_diff_marks(snapshot, &folded),
    };

    match &gutter.rich {
        Some(rich) => {
            let mut lines = gutter_component_lines(
                &folded,
                &gutter.severity,
                &diff_marks,
                &rich.diff,
                &rich.colors,
                gutter.current_line,
                gutter
                    .cursor_line
                    .map(|(line, fill)| (line, dim_rgb(fill, rich.bg, dim))),
            );
            // A terminal before the page icon protocol drops the icons a page
            // declares. A page for such a terminal marks each diagnostic with
            // the hint's dot run in place of its icon.
            if !gutter.icons {
                for diagnostic in lines.iter_mut().filter_map(|line| line.diagnostic.as_mut()) {
                    diagnostic.kind = None;
                }
            }
            let widget = rich_gutter(
                &lines,
                width_digits,
                (rich.number_fg, rich.separator, rich.bg),
                rich.number_scale,
            );
            let mut scene = gutter.scene();
            // The components write only the scene, so `buf` keeps its cells.
            widget.draw_components(area, buf, &mut scene);
            (widget.cell_width(), scene.buffer().clone())
        },
        None => {
            let width = draw_fallback_line_numbers(
                &folded,
                width_digits,
                &gutter.severity,
                &diff_marks,
                gutter.current_line,
                area,
                &gutter.theme,
                buf,
            );
            (width, Vec::new())
        },
    }
}

/// Render conflict-view page `index` from owned parts and wrap it in the pool
/// fill frames, so the returned bytes are a self-contained fill the terminal
/// applies to slot `index`.
///
/// The conflict analogue of [`render_review_page_from_parts`]. It calls the same
/// [`render_conflict_rows`] the live grid calls, so a wheel glide keeps all
/// three columns rather than dropping to the bare center buffer the plain-editor
/// page would paint.
///
/// A page only exists inside stoatty, so it paints the MergeGrid the live grid
/// paints there. The grid's bars, numbers, and chip runs ride as APC frames
/// after the serialized cells, the way the diff gutter's components reach a
/// page.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_conflict_page_from_parts(
    snapshot: &DisplaySnapshot,
    state: &ConflictViewState,
    theme: &crate::theme::Theme,
    pool: u32,
    index: u64,
    fallback_style: Style,
    region_width: u16,
    region_height: u16,
) -> Vec<u8> {
    let scroll_row = index
        .saturating_mul(region_height as u64)
        .min(u32::MAX as u64) as u32;
    let area = Rect::new(0, 0, region_width, region_height);
    let mut buf = page_buffer(area, theme);

    // The live paint of the same frame leaves the layout current, so a page
    // reads the state it shares with the editor. One arriving without a current
    // layout builds its own copy, which is what every page used to do.
    let mut built = None;
    if state.derived_current(snapshot).is_none() {
        let mut copy = state.clone();
        copy.derived(snapshot);
        built = Some(copy);
    }
    let source = built.as_ref().unwrap_or(state);
    let (doc, derived) = source
        .derived_current(snapshot)
        .expect("the layout is current or was just built");

    let mut scene = ApcScene::new();
    render_conflict_rows(
        snapshot,
        doc,
        derived,
        scroll_row,
        area,
        fallback_style,
        theme,
        &mut buf,
        None,
        None,
        Some(GridPaint {
            scene: &mut scene,
            cursor_row: None,
            chip_menu: state.chip_menu,
            layout: None,
            hover: None,
        }),
    );
    let mut bytes = serialize_buffer(&buf);
    bytes.extend_from_slice(scene.buffer());

    let mut frame = Vec::with_capacity(bytes.len() + 16);
    encode_fill_scope(&mut frame, pool, index, |out| out.extend_from_slice(&bytes));
    frame
}

/// Render one page of a row-oriented surface into a fresh region-sized
/// [`Buffer`], returning the page's self-contained VT byte stream.
///
/// `paint` receives the page buffer, the page's area, the document row the page
/// starts at, and a scene. Every list and popup page below shares that shape and
/// differs only in which painter runs, so each one adapts to this rather than
/// keeping its own copy.
///
/// The scene's frames follow the serialized cells inside the page's fill, the
/// way the diff gutter's rich components reach a page. That is what makes a
/// list's selection bar or a commit graph glide with the rows instead of
/// staying pinned to the live grid.
fn render_rows_page(
    region_width: u16,
    region_height: u16,
    page: u64,
    theme: &crate::theme::Theme,
    paint: impl FnOnce(&mut Buffer, Rect, usize, &mut ApcScene),
) -> Vec<u8> {
    let area = Rect::new(0, 0, region_width, region_height);
    let mut buf = page_buffer(area, theme);
    let mut scene = ApcScene::new();

    let start_row = page
        .saturating_mul(region_height as u64)
        .min(usize::MAX as u64) as usize;
    paint(&mut buf, area, start_row, &mut scene);

    let mut bytes = serialize_buffer(&buf);
    bytes.extend_from_slice(scene.buffer());
    bytes
}

/// Render `region_height` rows of the file finder list starting at document
/// row `page * region_height` into a fresh region-sized [`Buffer`], returning
/// the page's self-contained VT byte stream.
///
/// Mirrors [`render_editor_page`] but paints finder result rows, so a pooled
/// page matches the live list at that scroll position. A wrapped label takes
/// more than one row, so `tops`, the document row of each measured result from
/// [`row_tops`], places the page's first row, and a page past them paints no
/// row. The finder is read-only here.
///
/// With `pixels` the page is a page of a pixel list, which starts at the
/// page's first physical px row and draws its rows as nudged runs and bars.
///
/// [`row_tops`]: crate::render::picker::row_tops
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_finder_page(
    finder: &FileFinder,
    home: Option<&Path>,
    tops: &[u32],
    page: u64,
    theme: &crate::theme::Theme,
    region_width: u16,
    region_height: u16,
    pixels: Option<RowMetrics>,
) -> Vec<u8> {
    render_rows_page(
        region_width,
        region_height,
        page,
        theme,
        |buf, area, _, scene| {
            let Some(start) = list_page_start(tops, page, region_height, pixels) else {
                return;
            };
            paint_finder_rows(finder, home, area, start, theme, false, pixels, buf, scene)
        },
    )
}

/// Where page `index` of a path list's pool starts, from the document rows
/// `tops` of each measured result, or `None` past the last of them.
///
/// The page of a pixel list starts at its first physical px row, as `pixels`
/// measures one.
fn list_page_start(
    tops: &[u32],
    index: u64,
    region_height: u16,
    pixels: Option<RowMetrics>,
) -> Option<RowStart> {
    let row = page_top_row(index, region_height);
    match pixels {
        Some(metrics) => metrics.start(tops, row.saturating_mul(u32::from(metrics.cell[1]))),
        None => page_start(tops, row),
    }
}

/// Render `region_height` rows of the code-search match list starting at row
/// `page * region_height` into a fresh region-sized [`Buffer`], returning the
/// page's self-contained VT byte stream.
///
/// Mirrors [`render_finder_page`] but paints match rows. The finder is
/// read-only here -- the page index alone selects the rows.
pub(crate) fn render_code_search_page(
    finder: &CodeSearchFinder,
    git_root: &Path,
    page: u64,
    theme: &crate::theme::Theme,
    region_width: u16,
    region_height: u16,
) -> Vec<u8> {
    render_rows_page(
        region_width,
        region_height,
        page,
        theme,
        |buf, area, start_row, scene| {
            paint_match_rows(finder, area, start_row, git_root, theme, false, buf, scene)
        },
    )
}

/// Render `region_height` rows of the symbol finder list starting at row
/// `page * region_height` into a fresh region-sized [`Buffer`], returning the
/// page's self-contained VT byte stream.
///
/// Mirrors [`render_finder_page`] but paints symbol rows. The finder is
/// read-only here -- the page index alone selects the rows.
pub(crate) fn render_symbol_finder_page(
    finder: &SymbolFinder,
    git_root: &Path,
    page: u64,
    theme: &crate::theme::Theme,
    region_width: u16,
    region_height: u16,
) -> Vec<u8> {
    render_rows_page(
        region_width,
        region_height,
        page,
        theme,
        |buf, area, start_row, scene| {
            paint_symbol_rows(finder, area, start_row, git_root, theme, false, buf, scene)
        },
    )
}

/// Render `region_height` rows of the command-palette result list starting at
/// row `page * region_height` into a fresh region-sized [`Buffer`], returning
/// the page's self-contained VT byte stream.
///
/// Mirrors [`render_finder_page`] but paints palette result rows. The page
/// index alone selects the rows, and the list is read-only here. Each name
/// leaves out its first `name_offset` bytes, as the live list does under a
/// group. The rows of the `action` list that `namespace` names read their
/// keys from `keys`, and a `cli` command's row reads its help in `specs`, as
/// the live list reads them.
///
/// With `pixels` the page is a page of a pixel list, which starts at the
/// page's first physical px row as [`command_page_start`] places it. A page
/// past the list's end paints no row.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_palette_page(
    filtered: &[PaletteCommand],
    match_indices: &[Vec<u32>],
    selected: usize,
    name_offset: usize,
    namespace: Namespace,
    keys: &ActionKeys,
    specs: &HashMap<String, CliSpecState>,
    page: u64,
    theme: &crate::theme::Theme,
    region_width: u16,
    region_height: u16,
    pixels: Option<RowMetrics>,
) -> Vec<u8> {
    render_rows_page(
        region_width,
        region_height,
        page,
        theme,
        |buf, area, start_row, scene| {
            let start = match pixels {
                Some(metrics) => {
                    let row = page_top_row(page, region_height);
                    let at = row.saturating_mul(u32::from(metrics.cell[1]));
                    let Some(start) = command_page_start(filtered, at, metrics) else {
                        return;
                    };
                    start
                },
                None => RowStart {
                    result: start_row,
                    skip: 0,
                },
            };
            paint_palette_rows(
                filtered,
                match_indices,
                selected,
                name_offset,
                namespace,
                keys,
                specs,
                area,
                start,
                theme,
                false,
                pixels,
                buf,
                scene,
            )
        },
    )
}

/// Render `region_height` rows of the palette's inline argument-picker list
/// starting at document row `page * region_height` into a fresh region-sized
/// [`Buffer`], returning the page's self-contained VT byte stream.
///
/// Mirrors [`render_finder_page`] but paints the picker's path rows through the
/// shared [`crate::render::picker::paint_path_rows`], so a pooled page matches
/// the live inline picker. `tops` places the page's first row, and `pixels`
/// makes it a page of a pixel list, as they do for the finder. The picker is
/// read-only here.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_arg_page(
    picker: &ArgPicker,
    home: Option<&Path>,
    tops: &[u32],
    page: u64,
    theme: &crate::theme::Theme,
    region_width: u16,
    region_height: u16,
    pixels: Option<RowMetrics>,
) -> Vec<u8> {
    render_rows_page(
        region_width,
        region_height,
        page,
        theme,
        |buf, area, _, scene| {
            let Some(start) = list_page_start(tops, page, region_height, pixels) else {
                return;
            };
            let row_style = if picker.fallback_active() {
                theme.get(crate::theme::scope::UI_TEXT_MUTED)
            } else {
                theme.get(crate::theme::scope::UI_TEXT)
            };
            crate::render::picker::paint_path_rows(
                &arg_rows(picker, home),
                RowMarks::Modified(picker.modified_paths()),
                area,
                start,
                theme,
                row_style,
                false,
                pixels,
                buf,
                scene,
            )
        },
    )
}

/// Render `region_height` rows of the completion popup list starting at row
/// `page * region_height` into a fresh region-sized [`Buffer`], returning the
/// page's self-contained VT byte stream.
///
/// Mirrors [`render_finder_page`] but paints completion rows; the page index
/// alone selects the rows, and the list is read-only here.
pub(crate) fn render_completion_page(
    popup: &CompletionPopup,
    selected_idx: Option<usize>,
    prefix: &str,
    page: u64,
    theme: &crate::theme::Theme,
    region_width: u16,
    region_height: u16,
) -> Vec<u8> {
    render_rows_page(
        region_width,
        region_height,
        page,
        theme,
        |buf, area, start_row, scene| {
            paint_completion_rows(
                popup,
                selected_idx,
                prefix,
                start_row,
                area,
                theme,
                false,
                buf,
                scene,
            )
        },
    )
}

/// Render `region_height` rows of the help entry list starting at row
/// `page * region_height` into a fresh region-sized [`Buffer`], returning the
/// page's self-contained VT byte stream.
///
/// Mirrors [`render_finder_page`] but paints help list rows; the page index
/// alone selects the rows, and the list is read-only here.
pub(crate) fn render_help_list_page(
    help: &Help,
    page: u64,
    theme: &crate::theme::Theme,
    region_width: u16,
    region_height: u16,
) -> Vec<u8> {
    render_rows_page(
        region_width,
        region_height,
        page,
        theme,
        |buf, area, start_row, scene| {
            paint_help_list_rows(help, area, start_row, theme, false, buf, scene)
        },
    )
}

/// Render `region_height` lines of the selected help entry's detail starting at
/// line `page * region_height` into a fresh region-sized [`Buffer`], returning
/// the page's self-contained VT byte stream.
///
/// Mirrors [`render_help_list_page`] but paints the detail body. The page index
/// alone selects the lines, and the detail is read-only here. `surface` picks
/// the editor surface over the raised fill, as [`paint_help_detail_rows`] reads
/// it.
pub(crate) fn render_help_detail_page(
    help: &Help,
    page: u64,
    theme: &crate::theme::Theme,
    surface: bool,
    region_width: u16,
    region_height: u16,
) -> Vec<u8> {
    render_rows_page(
        region_width,
        region_height,
        page,
        theme,
        |buf, area, start_row, _| {
            paint_help_detail_rows(help, area, start_row, theme, surface, buf)
        },
    )
}

/// Render `region_height` rows of the commit list starting at row
/// `page * region_height` into a fresh region-sized [`Buffer`], returning the
/// page's self-contained VT byte stream.
///
/// Mirrors [`render_finder_page`] but paints commit rows; the page index alone
/// selects the rows, and the list is read-only here.
pub(crate) fn render_commits_page(
    state: &CommitListState,
    page: u64,
    theme: &crate::theme::Theme,
    region_width: u16,
    region_height: u16,
) -> Vec<u8> {
    render_rows_page(
        region_width,
        region_height,
        page,
        theme,
        |buf, area, start_row, scene| {
            paint_commit_rows(state, area, start_row, theme, false, buf, scene)
        },
    )
}

/// Render `region_height` rows of the commit picker's table, graph column
/// included, starting at row `page * region_height`.
///
/// `lanes` is the graph's lane count from
/// [`CommitPicker::graph_lanes`](crate::commit_picker::CommitPicker::graph_lanes),
/// or `None` when the column is hidden. The page splits its own region by that
/// count rather than taking the live rects, since a page sits at the origin and
/// would have to translate them back anyway.
///
/// The graph's stroked lines ride as APC frames appended after the serialized
/// cells, the same way the diff gutter's rich components reach a page. That is
/// what makes them glide with the rows instead of staying pinned to the live
/// grid.
pub(crate) fn render_commit_picker_list_page(
    picker: &crate::commit_picker::CommitPicker,
    lanes: Option<u16>,
    page: u64,
    theme: &crate::theme::Theme,
    region_width: u16,
    region_height: u16,
) -> Vec<u8> {
    use crate::render::commit_picker::{
        graph_width, paint_commit_graph, paint_commit_picker_rows, picker_graph,
    };

    let graph_cells = lanes.map(graph_width).unwrap_or(0).min(region_width);

    render_rows_page(
        region_width,
        region_height,
        page,
        theme,
        |buf, area, start_row, scene| {
            let table = Rect::new(
                graph_cells,
                0,
                area.width.saturating_sub(graph_cells),
                area.height,
            );
            // The column header sits above the pool region, so the page hands the
            // painter a zero-height rect it will never write into.
            let header = Rect::new(table.x, 0, table.width, 0);
            paint_commit_picker_rows(picker, start_row, header, table, theme, false, buf, scene);

            if graph_cells > 0 {
                let graph = Rect::new(0, 0, graph_cells, area.height);
                paint_commit_graph(
                    &picker_graph(picker, theme),
                    start_row,
                    graph,
                    theme,
                    buf,
                    scene,
                );
            }
        },
    )
}

/// Render `region_height` rows of the commit picker's diff preview starting at
/// row `page * region_height`.
///
/// Thin because [`crate::render::commits::render_commit_preview`] already takes
/// a row offset and a scene. The preview's per-row separator bars ride that
/// scene onto the page, so they glide with the diff rows rather than staying
/// pinned to the live grid.
pub(crate) fn render_commit_picker_preview_page(
    doc: &crate::review_session::DiffDocument,
    page: u64,
    theme: &crate::theme::Theme,
    dials: crate::render::commits::PreviewDials,
    region_width: u16,
    region_height: u16,
) -> Vec<u8> {
    render_rows_page(
        region_width,
        region_height,
        page,
        theme,
        |buf, area, skip_rows, scene| {
            crate::render::commits::render_commit_preview(
                doc, theme, area, skip_rows, dials, buf, scene,
            )
        },
    )
}

/// A page-sized buffer blanked to the editor surface.
///
/// A page is composited beside the live grid, whose panes clear their content to
/// the same blank. A page left at Reset paints the hosting terminal's background
/// next to stoat's, and the two disagree for as long as the glide runs.
pub(crate) fn page_buffer(area: Rect, theme: &crate::theme::Theme) -> Buffer {
    Buffer::filled(area, crate::render::surface_blank(theme))
}

#[cfg(test)]
mod tests {
    use super::{PageBlame, PageGutter};
    use crate::{
        display_map::{highlights::HighlightEndpoints, DisplaySnapshot},
        render::{blame::BlameColors, review::DiffDials},
        theme::Theme,
        LineNumbers,
    };
    use lsp_types::{Diagnostic, DiagnosticSeverity};
    use ratatui::style::Style;
    use std::sync::Arc;

    /// A decoded bar as its place, its size, and its color.
    type BarDraw = (i16, i16, u16, u16, [u8; 3]);

    /// A decoded text run as its text, its place, its color, and whether it
    /// slants.
    type RunDraw = (String, i16, i16, [u8; 3], bool);

    /// Ten numbered lines that a page and a live paint both show.
    ///
    /// The combining mark on line zero occupies no cell of its own, so both
    /// paths must join it to the cell its base went into. Without it a byte
    /// comparison of the two passes over a paint loop that drops marks.
    const PAGE_DOC: &str = "l\u{301}ine zero\nline one\nline two\nline three\nline four\nline five\nline six\nline seven\nline eight\nline nine\n";

    /// The endpoints a page resolved for itself before a refill began sharing
    /// one set across its pages, which is the behavior these tests pin.
    fn page_endpoints(
        snapshot: &DisplaySnapshot,
        top_row: u32,
        height: u16,
    ) -> Arc<HighlightEndpoints> {
        snapshot.highlighted_endpoints(top_row..top_row + height as u32)
    }

    use stoatty_protocol::command::{decode, Command};

    /// `render_page_from_snapshot` must paint the same bytes as the existing pool
    /// path, an unfocused `render_editor` over the same rows. Covers the first
    /// page, a mid page, the partial last page, and a page past the document end,
    /// exercising the page offset and the bottom/right clipping.
    ///
    /// Each page paints unwrapped and wrapped at the pane width, so a
    /// continuation row's Wrap Mark lands on the page as it lands live.
    #[test]
    fn page_from_snapshot_matches_unfocused_render_editor() {
        use super::{page_buffer, render_page_from_snapshot, serialize_buffer, PageGutter, Rect};
        use crate::{
            action_handlers::{self, dispatch},
            render::editor::{render_editor_with_overlay, RowSeverity},
            theme::{scope, Theme},
            LineNumbers, Stoat,
        };
        use std::path::PathBuf;
        use stoat_action::OpenFile;
        use stoat_config::WrapMode;

        let mut h = Stoat::test();
        let root = PathBuf::from("/page-snapshot");
        let path = root.join("doc.txt");
        h.fake_fs().insert_file(&path, PAGE_DOC.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        // The mark's own color, so a page that paints it in another style shows.
        let theme = {
            let (config, _) =
                stoat_config::parse(r##"theme marked { ui.text.disabled.fg = "#5b616d"; }"##);
            Theme::from_config(&config.expect("the theme parses"), "marked")
                .expect("the theme builds")
        };
        let fallback = theme.get(scope::UI_TEXT);
        let chrome = crate::render::editor::ResolvedChrome::resolve(&theme);
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");

        // With line numbers on in fallback mode, the page's degraded cell gutter
        // must match the live render's so the settle handoff shows no shift.
        let gutter = PageGutter::new(
            true,
            Arc::new(RowSeverity::default()),
            Arc::new(theme.clone()),
            None,
            None,
        );

        let pages = [WrapMode::None, WrapMode::EditorWidth]
            .into_iter()
            .flat_map(|wrap| [0u32, 4, 8, 40].map(|top_row| (wrap, top_row)));
        for (wrap, top_row) in pages {
            let area = Rect::new(0, 0, 12, 4);
            let mut expected = page_buffer(area, &theme);
            let saved = editor.scroll_row;
            editor.scroll_row = top_row;
            render_editor_with_overlay(
                editor,
                area,
                fallback,
                &theme,
                &chrome,
                &mut expected,
                false,
                false,
                LineNumbers::Absolute,
                false,
                None,
                None,
                None,
                None,
                true,
                None,
                0.0,
                DiffDials::shipped(),
                wrap,
                80,
                0,
                &[],
                false,
                None,
                None,
                false,
            );
            editor.scroll_row = saved;
            let expected = serialize_buffer(&expected);

            let snapshot = editor.display_map.snapshot();
            let got = render_page_from_snapshot(
                &snapshot,
                top_row,
                fallback,
                12,
                4,
                &gutter,
                false,
                0.0,
                DiffDials::shipped(),
                page_endpoints(&snapshot, top_row, 4),
                None,
                None,
            );

            assert_eq!(got, expected, "{wrap:?} page at top_row {top_row}");
        }
    }

    /// A page paints the diagnostic spans of its rows as the unfocused live grid
    /// paints them, with the curly or dotted line that the frame stamps over each.
    ///
    /// The error on row 1 curls, the hint on row 5 dots, and the unused code from
    /// row 7 into row 8 mutes its text across the edge between two pages.
    #[test]
    fn a_page_paints_the_diagnostic_spans_of_the_unfocused_live_grid() {
        use lsp_types::DiagnosticTag;

        let unused = Diagnostic {
            tags: Some(vec![DiagnosticTag::UNNECESSARY]),
            ..diagnostic(DiagnosticSeverity::WARNING, (7, 0), (8, 4))
        };
        let diagnostics = vec![
            diagnostic(DiagnosticSeverity::ERROR, (1, 0), (1, 4)),
            diagnostic(DiagnosticSeverity::HINT, (5, 0), (5, 4)),
            unused,
        ];
        let pages = live_and_page_bytes(None, LineNumbers::Absolute, diagnostics, &[0, 4, 8]);

        for (top_row, (live, page)) in [0, 4, 8].into_iter().zip(&pages) {
            assert_eq!(page, live, "page at top_row {top_row}");
        }
        assert!(
            find(&pages[0].1, b"4:3").is_some(),
            "the error on row 1 curls"
        );
    }

    /// With line numbers off, a page keeps the one-column severity gutter of the
    /// live grid. Under a theme whose severity colors are not RGB, each
    /// diagnostic row takes its letter in column 0 on both, and the text starts
    /// at column 1.
    #[test]
    fn a_numberless_page_keeps_the_severity_column() {
        let errors = [1, 5]
            .map(|row| diagnostic(DiagnosticSeverity::ERROR, (row, 0), (row, 4)))
            .to_vec();
        let pages = live_and_page_bytes(Some(Theme::empty()), LineNumbers::Off, errors, &[0, 4]);

        for (top_row, (live, page)) in [0, 4].into_iter().zip(&pages) {
            assert_eq!(page, live, "page at top_row {top_row}");
        }
    }

    /// With line numbers off and RGB severity colors, a page marks each
    /// diagnostic row of its one-column gutter with a sub-cell bar in the
    /// severity color, as the live grid does under stoatty. The column's cells
    /// hold no letter.
    #[test]
    fn a_numberless_page_bars_its_diagnostic_rows() {
        use super::{render_page_from_snapshot, PageDiagnostics};
        use crate::{
            render::{
                editor::{ResolvedChrome, RowSeverity},
                paint::style_rgb,
            },
            term_screen::TermScreen,
            theme::scope,
        };
        use stoatty_protocol::command::{decode_stream, BarCommand};

        let (snapshot, numbered, fallback) = rich_gutter_page();
        let theme = numbered.theme.clone();
        let gutter = PageGutter::new(
            false,
            Arc::new(RowSeverity::for_rows([(1, DiagnosticSeverity::ERROR)])),
            theme.clone(),
            None,
            None,
        )
        .diagnostics(PageDiagnostics {
            colors: ResolvedChrome::resolve(&theme).severity,
            ..Default::default()
        });
        let bytes = render_page_from_snapshot(
            &snapshot,
            0,
            fallback,
            12,
            4,
            &gutter,
            false,
            0.0,
            DiffDials::shipped(),
            page_endpoints(&snapshot, 0, 4),
            None,
            None,
        );

        let bars: Vec<BarCommand> = decode_stream(&bytes)
            .into_iter()
            .filter_map(|command| match command {
                Command::Bar(bar) => Some(bar),
                _ => None,
            })
            .collect();
        let mut screen = TermScreen::new(4, 12);
        screen.feed(&bytes[..find(&bytes, b"\x1b_").unwrap_or(bytes.len())]);
        let column: Vec<char> = (0..4).map(|row| screen.row(row)[0].ch).collect();
        let error = style_rgb(theme.get(scope::UI_DIAGNOSTIC_ERROR).fg)
            .expect("the shipped error color is RGB");

        assert_eq!(
            (bars, column),
            (
                vec![BarCommand {
                    x: 0,
                    y: 16,
                    width: 6,
                    height: 16,
                    color: error,
                    occludes: false,
                    hatch: None,
                    radius: 0,
                    corners: 0,
                }],
                vec![' '; 4],
            ),
            "one bar on row 1 in the error color, and no letter in the column",
        );
    }

    /// A page from a blamed gutter starts its text and its gutter 18 cells
    /// further right than the page from the same gutter unblamed. With no
    /// blame landed, its frames carry the column's surface as one bar on each
    /// of the page's four rows.
    #[test]
    fn a_blamed_page_moves_its_text_past_the_blame_column() {
        use super::render_page_from_snapshot;
        use crate::term_screen::TermScreen;
        use stoatty_protocol::command::{decode_stream, BarCommand};

        let (snapshot, gutter, fallback) = rich_gutter_page();
        let blamed = gutter.clone().blamed(Some(surface_only_blame()));
        let page = |gutter: &PageGutter| {
            let bytes = render_page_from_snapshot(
                &snapshot,
                0,
                fallback,
                60,
                4,
                gutter,
                false,
                0.0,
                DiffDials::shipped(),
                page_endpoints(&snapshot, 0, 4),
                None,
                None,
            );
            let mut screen = TermScreen::new(4, 60);
            screen.feed(&bytes[..find(&bytes, b"\x1b_").unwrap_or(bytes.len())]);
            let row: String = screen.row(0).iter().map(|cell| cell.ch).collect();
            let bars: Vec<BarCommand> = decode_stream(&bytes)
                .into_iter()
                .filter_map(|command| match command {
                    Command::Bar(bar) => Some(bar),
                    _ => None,
                })
                .collect();
            (row, bars)
        };

        let ((plain_row, plain_bars), (blamed_row, blamed_bars)) = (page(&gutter), page(&blamed));
        assert!(!plain_bars.is_empty(), "the gutter draws its separator");
        let (surfaces, rest): (Vec<_>, Vec<_>) =
            blamed_bars.into_iter().partition(|bar| bar.x < 18 * 16);
        let shifted_row: String = " "
            .repeat(18)
            .chars()
            .chain(plain_row.chars().take(42))
            .collect();
        let shifted_bars: Vec<BarCommand> = plain_bars
            .into_iter()
            .map(|bar| BarCommand {
                x: bar.x + 18 * 16,
                ..bar
            })
            .collect();
        let surface = |row: i16| BarCommand {
            x: 0,
            y: row * 16,
            width: 281,
            height: 16,
            color: [1, 2, 3],
            occludes: false,
            hatch: None,
            radius: 0,
            corners: 0,
        };
        assert_eq!(
            (blamed_row, surfaces, rest),
            (
                shifted_row,
                (0..4).map(surface).collect::<Vec<_>>(),
                shifted_bars
            ),
            "the text and the gutter past the column, and a surface bar left of them on each row",
        );
    }

    /// A decorations refill of a blamed page whose numberless gutter marks a
    /// diagnostic with a letter writes the letter past the column, into the
    /// cells nothing reads, and carries the column's surface on each row.
    #[test]
    fn a_blamed_numberless_decorations_refill_carries_the_surface() {
        use super::render_page_decorations;
        use crate::render::editor::RowSeverity;
        use stoatty_protocol::command::decode_stream;

        let (snapshot, numbered, fallback) = rich_gutter_page();
        let gutter = PageGutter::new(
            false,
            Arc::new(RowSeverity::for_rows([(1, DiagnosticSeverity::ERROR)])),
            numbered.theme.clone(),
            numbered.rich.clone(),
            None,
        )
        .blamed(Some(surface_only_blame()));
        let bytes = render_page_decorations(
            &snapshot,
            1,
            0,
            60,
            4,
            &gutter,
            false,
            fallback,
            0.0,
            DiffDials::shipped(),
            None,
        );

        let surfaces: Vec<_> = decode_stream(&bytes)
            .into_iter()
            .filter_map(|command| match command {
                Command::Bar(bar) if bar.width == 281 => {
                    Some((bar.x, bar.y, bar.height, bar.color))
                },
                _ => None,
            })
            .collect();
        assert_eq!(surfaces, [0, 16, 32, 48].map(|y| (0, y, 16, [1, 2, 3])),);
    }

    /// A blamed page's frames hold the Blame Column that the live paint of the
    /// same rows holds, bar for bar and run for run. The file holds two
    /// commits and an uncommitted line, so the two pages cross three runs.
    /// Both paints select the second commit, so its lines draw as the
    /// selection.
    #[test]
    fn a_blamed_page_draws_the_column_the_live_paint_draws() {
        use super::{page_buffer, render_page_from_snapshot};
        use crate::{
            action_handlers,
            render::{
                blame::{BlameLines, BlamePaint},
                editor::{render_editor_with_overlay, ResolvedChrome, RowSeverity},
            },
            test_harness::TestHarness,
            theme::scope,
        };
        use ratatui::layout::Rect;
        use std::path::Path;
        use stoat_config::WrapMode;
        use stoat_widgets::ApcScene;

        let mut h = TestHarness::with_size(80, 24);
        h.seed_linear_history(
            "/repo",
            &[
                ("c1", "one", &[("a.rs", "one\ntwo\n")]),
                ("c2", "two", &[("a.rs", "one\ntwo\nthree\nfour\n")]),
            ],
        );
        h.stoat.active_workspace_mut().git_root = "/repo".into();
        h.fake_fs()
            .insert_file("/repo/a.rs", "one\ntwo\nthree\nfour\nfive\n");
        h.open_file(Path::new("/repo/a.rs"));
        h.type_keys("space h a");
        h.settle();
        h.stoat.render();

        let theme = h.stoat.theme.clone();
        let fallback = theme.get(scope::UI_TEXT);
        let chrome = ResolvedChrome::resolve(&theme);
        let colors = chrome
            .blame
            .clone()
            .expect("the shipped theme resolves the column");
        let now_epoch = crate::commit_picker::now_epoch(&h.stoat.executor);
        let (_, id) = h.stoat.focused_editor_ids().expect("a focused editor");
        let (rows, base, selected) = {
            let ws = h.stoat.active_workspace();
            let rows = ws.blame.rows(id).cloned().expect("the rows resolved");
            (
                rows,
                ws.blame.base(id).cloned(),
                ws.blame.commit_index(id, 2),
            )
        };
        assert_eq!(selected, Some(1), "line 3 holds the second commit");
        let gutter = PageGutter::new(
            true,
            Arc::new(RowSeverity::default()),
            theme.clone(),
            chrome.rich_gutter.clone(),
            None,
        )
        .blamed(Some(PageBlame {
            colors: colors.clone(),
            rows: Some(rows.clone()),
            base: base.clone(),
            now_epoch,
            selected,
        }));
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");

        for top_row in [0, 4] {
            let area = Rect::new(0, 0, 60, 4);
            let mut scene = ApcScene::new();
            let saved = editor.scroll_row;
            editor.scroll_row = top_row;
            render_editor_with_overlay(
                editor,
                area,
                fallback,
                &theme,
                &chrome,
                &mut page_buffer(area, &theme),
                false,
                false,
                LineNumbers::Absolute,
                false,
                None,
                None,
                None,
                Some(&mut scene),
                true,
                None,
                0.0,
                DiffDials::shipped(),
                WrapMode::None,
                80,
                0,
                &[],
                false,
                None,
                Some(BlamePaint {
                    colors: &colors,
                    lines: Some(BlameLines {
                        rows: &rows,
                        base: base.as_deref(),
                        now_epoch,
                        selected,
                    }),
                }),
                false,
            );
            editor.scroll_row = saved;

            let snapshot = editor.display_map.snapshot();
            let page = render_page_from_snapshot(
                &snapshot,
                top_row,
                fallback,
                60,
                4,
                &gutter,
                false,
                0.0,
                DiffDials::shipped(),
                page_endpoints(&snapshot, top_row, 4),
                None,
                None,
            );
            let live = blame_column(scene.buffer());
            assert!(
                !live.0.is_empty() && !live.1.is_empty(),
                "the live paint at top_row {top_row} draws bars and a label",
            );
            assert_eq!(blame_column(&page), live, "page at top_row {top_row}");
        }
    }

    /// The bars and the text runs that `bytes` draws in the Blame Column, in
    /// place order.
    fn blame_column(bytes: &[u8]) -> (Vec<BarDraw>, Vec<RunDraw>) {
        use stoatty_protocol::command::decode_stream;

        let (mut bars, mut runs) = (Vec::new(), Vec::new());
        for command in decode_stream(bytes) {
            match command {
                Command::Bar(bar) => bars.push((bar.x, bar.y, bar.width, bar.height, bar.color)),
                Command::Bars(group) => bars.extend(
                    group
                        .bars
                        .into_iter()
                        .map(|(x, y, width, height)| (x, y, width, height, group.color)),
                ),
                Command::TextRun(run) => {
                    runs.push((run.text, run.col, run.row, run.color, run.italic))
                },
                Command::TextRuns(group) => runs.extend(
                    group
                        .runs
                        .into_iter()
                        .map(|(col, row, text)| (text, col, row, group.color, group.italic)),
                ),
                _ => {},
            }
        }
        bars.retain(|&(x, ..)| x < 18 * 16);
        runs.retain(|&(_, col, ..)| col < 18 * 16);
        bars.sort_by_key(|&(x, y, ..)| (y, x));
        runs.sort_by_key(|&(_, col, row, ..)| (row, col));
        (bars, runs)
    }

    /// A page's Blame Column in test colors, before a blame of its file
    /// lands.
    fn surface_only_blame() -> PageBlame {
        PageBlame {
            colors: BlameColors {
                surface: [1, 2, 3],
                ages: [[4, 4, 4]; 4],
                divider: [5, 5, 5],
                author: [6, 6, 6],
                date: [7, 7, 7],
                uncommitted: [8, 8, 8],
                selected: [9, 9, 9],
                selected_surface: [10, 10, 10],
                selected_author: [11, 11, 11],
                selected_date: [12, 12, 12],
                scale: 256,
            },
            rows: None,
            base: None,
            now_epoch: 0,
            selected: None,
        }
    }

    /// The bytes of an unfocused live paint of [`PAGE_DOC`] and of the page that
    /// stands in for it, as one `(live, page)` pair for each of `top_rows`.
    ///
    /// The live bytes carry the lines that the frame stamps under the
    /// diagnostics. The file carries `diagnostics`, and the page takes its
    /// gutter and its diagnostics from what the live paint resolved. `theme`
    /// replaces the shipped theme when given.
    fn live_and_page_bytes(
        theme: Option<Theme>,
        line_numbers: LineNumbers,
        diagnostics: Vec<Diagnostic>,
        top_rows: &[u32],
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        use super::{
            page_buffer, render_page_from_snapshot, serialize_buffer, PageDiagnostics, Rect,
        };
        use crate::{
            action_handlers::{self, dispatch},
            render::{
                editor::{render_editor_with_overlay, ResolvedChrome},
                undercurl::{self, UndercurlBatch},
            },
            theme::scope,
            Stoat,
        };
        use std::path::PathBuf;
        use stoat_action::OpenFile;
        use stoat_config::WrapMode;

        let mut h = Stoat::test();
        let root = PathBuf::from("/page-diagnostics");
        let path = root.join("doc.txt");
        h.fake_fs().insert_file(&path, PAGE_DOC.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        h.seed_diagnostics(&path, diagnostics);

        let theme = theme.map_or_else(|| h.stoat.theme.clone(), Arc::new);
        let fallback = theme.get(scope::UI_TEXT);
        let chrome = ResolvedChrome::resolve(&theme);
        let set = h.stoat.diagnostics.clone();
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");

        top_rows
            .iter()
            .map(|&top_row| {
                let area = Rect::new(0, 0, 12, 4);
                let mut live = page_buffer(area, &theme);
                let mut undercurls = UndercurlBatch::default();
                undercurls.begin();
                let saved = editor.scroll_row;
                editor.scroll_row = top_row;
                render_editor_with_overlay(
                    editor,
                    area,
                    fallback,
                    &theme,
                    &chrome,
                    &mut live,
                    false,
                    false,
                    line_numbers,
                    false,
                    None,
                    None,
                    Some((&path, &set)),
                    None,
                    true,
                    Some(&mut undercurls),
                    0.0,
                    DiffDials::shipped(),
                    WrapMode::None,
                    80,
                    0,
                    &[],
                    false,
                    None,
                    None,
                    false,
                );
                editor.scroll_row = saved;
                undercurl::snapshot_cells(&live, undercurls.spans_mut());
                let mut expected = serialize_buffer(&live);
                expected.extend(undercurl::build(&live, undercurls.spans()));

                let cache = editor
                    .gutter_severity_cache
                    .as_ref()
                    .expect("the live paint resolves the diagnostics");
                let gutter = PageGutter::new(
                    line_numbers != LineNumbers::Off,
                    cache.map.clone(),
                    theme.clone(),
                    None,
                    None,
                )
                .diagnostics(PageDiagnostics {
                    spans: cache.spans.clone(),
                    colors: chrome.severity.clone(),
                    ..Default::default()
                });
                let snapshot = editor.display_map.snapshot();
                let page = render_page_from_snapshot(
                    &snapshot,
                    top_row,
                    fallback,
                    12,
                    4,
                    &gutter,
                    false,
                    0.0,
                    DiffDials::shipped(),
                    page_endpoints(&snapshot, top_row, 4),
                    None,
                    None,
                );
                (expected, page)
            })
            .collect()
    }

    /// A diagnostic of `severity` from the buffer point `start` to `end`.
    fn diagnostic(severity: DiagnosticSeverity, start: (u32, u32), end: (u32, u32)) -> Diagnostic {
        use lsp_types::{Position, Range};

        Diagnostic {
            range: Range::new(Position::new(start.0, start.1), Position::new(end.0, end.1)),
            severity: Some(severity),
            ..Default::default()
        }
    }

    #[test]
    fn pooled_page_places_wide_chars_like_the_live_grid() {
        use super::{page_buffer, render_page_from_snapshot, serialize_buffer, PageGutter, Rect};
        use crate::{
            action_handlers::{self, dispatch},
            render::editor::{render_editor_with_overlay, RowSeverity},
            theme::{scope, Theme},
            LineNumbers, Stoat,
        };
        use std::path::PathBuf;
        use stoat_action::OpenFile;
        use stoat_config::WrapMode;

        let mut h = Stoat::test();
        let root = PathBuf::from("/page-cjk");
        let path = root.join("doc.txt");
        h.fake_fs()
            .insert_file(&path, "\u{6c49}\u{5b57}ab\n".as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let theme = Theme::empty();
        let chrome = crate::render::editor::ResolvedChrome::resolve(&theme);
        let fallback = theme.get(scope::UI_TEXT);
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let gutter = PageGutter::new(
            true,
            Arc::new(RowSeverity::default()),
            Arc::new(theme.clone()),
            None,
            None,
        );

        let area = Rect::new(0, 0, 12, 2);
        let mut expected = page_buffer(area, &theme);
        render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &chrome,
            &mut expected,
            false,
            false,
            LineNumbers::Absolute,
            false,
            None,
            None,
            None,
            None,
            true,
            None,
            0.0,
            DiffDials::shipped(),
            WrapMode::None,
            80,
            0,
            &[],
            false,
            None,
            None,
            false,
        );
        let expected = serialize_buffer(&expected);

        let snapshot = editor.display_map.snapshot();
        let got = render_page_from_snapshot(
            &snapshot,
            0,
            fallback,
            12,
            2,
            &gutter,
            false,
            0.0,
            DiffDials::shipped(),
            page_endpoints(&snapshot, 0, 2),
            None,
            None,
        );

        assert_eq!(
            got, expected,
            "the pooled page places wide chars by display width like the live grid",
        );
    }

    /// A line far wider than the page carries wide chars past the right edge and
    /// spans several rope chunks, so the pooled fill's skip to the next newline
    /// must land on a char boundary and still reset the column for the next row.
    #[test]
    fn a_pooled_page_clips_an_over_wide_line_like_the_live_grid() {
        use super::{page_buffer, render_page_from_snapshot, serialize_buffer, PageGutter, Rect};
        use crate::{
            action_handlers::{self, dispatch},
            render::editor::{render_editor_with_overlay, RowSeverity},
            theme::{scope, Theme},
            LineNumbers, Stoat,
        };
        use std::path::PathBuf;
        use stoat_action::OpenFile;
        use stoat_config::WrapMode;

        let mut h = Stoat::test();
        let root = PathBuf::from("/page-over-wide");
        let path = root.join("doc.txt");
        let line = format!(
            "{}{}{}",
            "x".repeat(10),
            "\u{6c49}".repeat(40),
            "y".repeat(30),
        );
        h.fake_fs()
            .insert_file(&path, format!("{line}\nsecond line\n").as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let theme = Theme::empty();
        let fallback = theme.get(scope::UI_TEXT);
        let chrome = crate::render::editor::ResolvedChrome::resolve(&theme);
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let gutter = PageGutter::new(
            true,
            Arc::new(RowSeverity::default()),
            Arc::new(theme.clone()),
            None,
            None,
        );

        let area = Rect::new(0, 0, 20, 3);
        let mut expected = page_buffer(area, &theme);
        render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &chrome,
            &mut expected,
            false,
            false,
            LineNumbers::Absolute,
            false,
            None,
            None,
            None,
            None,
            true,
            None,
            0.0,
            DiffDials::shipped(),
            WrapMode::None,
            80,
            0,
            &[],
            false,
            None,
            None,
            false,
        );
        let expected = serialize_buffer(&expected);

        let snapshot = editor.display_map.snapshot();
        let got = render_page_from_snapshot(
            &snapshot,
            0,
            fallback,
            20,
            3,
            &gutter,
            false,
            0.0,
            DiffDials::shipped(),
            page_endpoints(&snapshot, 0, 3),
            None,
            None,
        );

        assert_eq!(
            got, expected,
            "the pooled page clips the over-wide line where the live grid does",
        );
    }

    /// A pooled page for an unfocused pane must dim exactly as the live grid does:
    /// threading `dim` changes the page bytes, and the change is precisely the
    /// shared cell blend, so a glide over an inactive pane shows no undimmed flash.
    #[test]
    fn a_dimmed_page_is_the_undimmed_page_with_the_cell_blend() {
        use super::{dim_page, render_page_from_snapshot, Buffer, PageGutter, Rect};
        use crate::{
            action_handlers::{self, dispatch},
            render::{editor::RowSeverity, paint::style_rgb, pane::dim_pane_content},
            theme::scope,
            Stoat,
        };
        use ratatui::style::{Color, Style};
        use std::path::PathBuf;
        use stoat_action::OpenFile;

        let mut h = Stoat::test();
        let root = PathBuf::from("/page-dim");
        let path = root.join("doc.txt");
        h.fake_fs()
            .insert_file(&path, b"alpha\nbravo\ncharlie\ndelta\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let theme = h.stoat.theme.clone();
        let bg = style_rgb(theme.try_get(scope::UI_BACKGROUND).and_then(|s| s.bg))
            .expect("default theme has an rgb background");
        let fallback = theme.get(scope::UI_TEXT);
        let gutter = PageGutter::new(
            true,
            Arc::new(RowSeverity::default()),
            theme.clone(),
            None,
            None,
        );
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();

        let undimmed = render_page_from_snapshot(
            &snapshot,
            0,
            fallback,
            12,
            4,
            &gutter,
            false,
            0.0,
            DiffDials::shipped(),
            page_endpoints(&snapshot, 0, 4),
            None,
            None,
        );
        let dimmed = render_page_from_snapshot(
            &snapshot,
            0,
            fallback,
            12,
            4,
            &gutter,
            false,
            0.5,
            DiffDials::shipped(),
            page_endpoints(&snapshot, 0, 4),
            None,
            None,
        );
        assert_ne!(undimmed, dimmed, "threading dim changes the page bytes");

        let area = Rect::new(0, 0, 12, 4);
        let mut via_page = Buffer::empty(area);
        via_page[(0, 0)]
            .set_char('x')
            .set_style(Style::default().fg(Color::Rgb(200, 100, 40)));
        let mut via_helper = via_page.clone();
        dim_page(&mut via_page, area, &theme, 0.5);
        dim_pane_content(&mut via_helper, area, bg, 0.5);
        assert_eq!(
            via_page, via_helper,
            "a page dims by exactly the live grid's cell blend"
        );
    }

    /// The focused editor's pooled page carries the walkthrough spotlight the
    /// live grid paints, the dim and the lit annotation, so a glide composites
    /// the colors the settled grid shows.
    #[test]
    fn a_lit_page_is_the_dimmed_page_with_the_annotation_recolored() {
        use super::{light_page_range, render_page_from_snapshot, Buffer, PageGutter, Rect};
        use crate::{
            action_handlers::{self, dispatch},
            render::{editor::RowSeverity, walkthrough::Spotlight},
            theme::scope,
            Stoat,
        };
        use ratatui::style::Color;
        use std::path::PathBuf;
        use stoat_action::OpenFile;

        let mut h = Stoat::test();
        let root = PathBuf::from("/page-lit");
        let path = root.join("doc.txt");
        h.fake_fs()
            .insert_file(&path, b"alpha\nbravo\ncharlie\ndelta\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let theme = h.stoat.theme.clone();
        let fallback = theme.get(scope::UI_TEXT);
        let gutter = PageGutter::new(true, Arc::new(RowSeverity::default()), theme, None, None);
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        // "bravo" is the second line, offsets 6 through 10.
        let spotlight = |dim: f32| Spotlight {
            range: 6..=10,
            color: [1, 2, 3],
            dim,
        };

        let page = |spotlight: Option<&Spotlight>, dim: f32| {
            render_page_from_snapshot(
                &snapshot,
                0,
                fallback,
                12,
                4,
                &gutter,
                false,
                dim,
                DiffDials::shipped(),
                page_endpoints(&snapshot, 0, 4),
                None,
                spotlight,
            )
        };
        let lit_page = page(Some(&spotlight(0.5)), 0.0);
        assert_ne!(
            lit_page,
            page(None, 0.5),
            "the annotation is recolored over the dim",
        );
        assert_ne!(
            lit_page,
            page(Some(&spotlight(0.0)), 0.0),
            "and the spotlight dims the rest of the page",
        );

        let area = Rect::new(0, 0, 12, 4);
        let gutter_w = 3;
        let mut lit = Buffer::empty(area);
        light_page_range(&mut lit, area, gutter_w, 0, &snapshot, &spotlight(0.5));
        let mut expected = Buffer::empty(area);
        for x in gutter_w..gutter_w + 5 {
            expected[(x, 1)].set_fg(Color::Rgb(1, 2, 3));
        }
        assert_eq!(
            lit, expected,
            "the five cells of bravo take the marker color and no other cell changes",
        );
    }

    /// A diff-view page must paint the same two columns as the live grid, so
    /// stoatty diff scrolling rides the pool path without a settle-handoff shift.
    #[test]
    fn diff_view_page_paints_the_two_column_body() {
        use super::{
            diff_gutter_digits, page_buffer, paint_diff_rows, render_page_from_snapshot,
            serialize_buffer, BufferNumbering, PageGutter, Rect,
        };
        use crate::{
            buffer::{BufferId, TextBuffer},
            diff_map::DiffMap,
            display_map::DisplayMap,
            multi_buffer::MultiBuffer,
            render::editor::RowSeverity,
            theme::{scope, Theme},
        };
        use std::sync::RwLock;
        use stoat_language::structural_diff;
        use stoat_scheduler::{Executor, TestScheduler};

        let base = "keep\nold\ntail\n";
        let text = "keep\nnew\ntail\n";
        let mut tb = TextBuffer::with_text(BufferId::new(0), text);
        tb.diff_map = Some(DiffMap::from_structural_changes(
            structural_diff::diff(base, text),
            Arc::new(base.to_string()),
            text,
        ));
        let shared = Arc::new(RwLock::new(tb));
        let multi = MultiBuffer::singleton(shared);
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let mut display_map = DisplayMap::new(multi, executor, crate::test_notify());
        display_map.set_show_deleted_blocks(true);
        let snapshot = display_map.snapshot();

        let theme = Theme::empty();
        let fallback = theme.get(scope::UI_TEXT);
        let gutter = PageGutter::new(
            true,
            Arc::new(RowSeverity::default()),
            Arc::new(theme.clone()),
            None,
            None,
        );
        let area = Rect::new(0, 0, 40, 8);

        let mut expected = page_buffer(area, &theme);
        paint_diff_rows(
            &snapshot,
            0,
            area,
            diff_gutter_digits(&snapshot),
            fallback,
            &theme,
            &mut expected,
            None,
            0.0,
            DiffDials::shipped(),
            BufferNumbering::Absolute,
            &[],
            None,
            None,
            false,
        );
        let expected = serialize_buffer(&expected);

        let got = render_page_from_snapshot(
            &snapshot,
            0,
            fallback,
            40,
            8,
            &gutter,
            true,
            0.0,
            DiffDials::shipped(),
            page_endpoints(&snapshot, 0, 8),
            None,
            None,
        );
        assert_eq!(
            got, expected,
            "the diff-view page paints the two-column body, not the single-column path"
        );
    }

    /// A conflict-view page must carry the same three columns as the live grid,
    /// so a wheel glide over the pool does not drop to the bare center buffer
    /// and back at settle. The grid's scene components ride the page after its
    /// cells.
    #[test]
    fn conflict_view_page_paints_the_three_column_body() {
        use super::{
            page_buffer, render_conflict_page_from_parts, render_conflict_rows, serialize_buffer,
            ApcScene, GridPaint, Rect,
        };
        use crate::{test_harness::TestHarness, theme::scope};
        use std::path::PathBuf;

        let mut h = TestHarness::with_size(150, 24);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_git()
            .add_repo("/repo")
            .with_fs(h.fake_fs())
            .conflicted_file(
                "f.txt",
                Some("a\nbase\nz\n"),
                Some("a\nOURS\nz\n"),
                Some("a\nTHEIRS\nz\n"),
            );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Conflict);
        h.settle();

        let theme = Arc::new(h.stoat.theme.clone());
        let fallback = theme.get(scope::UI_TEXT);
        let (snapshot, mut state) = {
            let editor =
                crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("center editor");
            (
                editor.display_map.snapshot(),
                editor.conflict_view.clone().expect("conflict view state"),
            )
        };

        let area = Rect::new(0, 0, 150, 8);
        let mut cells = page_buffer(area, &theme);
        let mut scene = ApcScene::new();
        {
            let (doc, derived) = Arc::make_mut(&mut state).derived(&snapshot);
            render_conflict_rows(
                &snapshot,
                doc,
                derived,
                0,
                area,
                fallback,
                &theme,
                &mut cells,
                None,
                None,
                Some(GridPaint {
                    scene: &mut scene,
                    cursor_row: None,
                    chip_menu: None,
                    layout: None,
                    hover: None,
                }),
            );
        }
        let decorations = scene.buffer().clone();
        let mut expected = serialize_buffer(&cells);
        expected.extend_from_slice(&decorations);

        let got =
            render_conflict_page_from_parts(&snapshot, &state, &theme, 7, 0, fallback, 150, 8);
        assert_eq!(
            (
                decorations.is_empty(),
                got.windows(expected.len()).any(|w| w == expected)
            ),
            (false, true),
            "the conflict page carries the grid's cells and components inside its fill frames",
        );
    }

    /// A pooled page baked with a cursor line paints relative numbers in its
    /// gutter, matching the live render so the settle handoff shows no shift.
    #[test]
    fn pooled_page_paints_relative_numbers() {
        use super::{paint_page_gutter, Buffer, PageGutter, Rect};
        use crate::{
            action_handlers::{self, dispatch},
            render::editor::RowSeverity,
            theme::Theme,
            Stoat,
        };
        use std::path::PathBuf;
        use stoat_action::OpenFile;

        let mut h = Stoat::test();
        let root = PathBuf::from("/page-relnum");
        let path = root.join("doc.txt");
        h.fake_fs()
            .insert_file(&path, b"one\ntwo\nthree\nfour\nfive");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let theme = Theme::empty();
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();

        // current_line 3 numbers every line by its distance from line 3, which
        // keeps its absolute number.
        let gutter = PageGutter::new(
            true,
            Arc::new(RowSeverity::default()),
            Arc::new(theme),
            None,
            Some(3),
        );
        let area = Rect::new(0, 0, 12, 5);
        let mut buf = Buffer::empty(area);
        let (width, _) = paint_page_gutter(
            &snapshot,
            0,
            5,
            &mut buf,
            area,
            &gutter,
            Style::default(),
            0.0,
            None,
        );

        let digits: Vec<String> = (0..5)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim()
                    .to_string()
            })
            .collect();
        assert_eq!(digits, ["2", "1", "3", "1", "2"]);
    }

    /// Split `bytes` into the sequence of decoded stoatty commands, ignoring the
    /// raw page VT that rides between `fill`/`fill_end` markers.
    fn commands(bytes: &[u8]) -> Vec<Command> {
        let mut out = Vec::new();
        let mut rest = bytes;
        while let Some(start) = find(rest, b"\x1b_") {
            let after = &rest[start..];
            let Some(end) = find(after, b"\x1b\\") else {
                break;
            };
            let frame = &after[..end + 2];
            if let Some(cmd) = decode(frame) {
                out.push(cmd);
            }
            rest = &after[end + 2..];
        }
        out
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// A refill resolves one set of endpoints across the pages it spawns, so
    /// each page now chunks against a set reaching past its own rows. That is
    /// only sound if a wider set paints the same bytes as the page's own, which
    /// is what this holds it to. A file with syntax highlighting is the case
    /// where endpoints exist to get wrong.
    #[test]
    fn a_shared_endpoint_set_paints_each_page_as_its_own_would() {
        use super::{page_top_row, render_page_fill, PageGutter};
        use crate::{
            action_handlers::{self, dispatch},
            buffer::BufferId,
            display_map::highlights::{
                HighlightStyle, HighlightStyleInterner, SemanticTokenHighlight,
            },
            render::editor::RowSeverity,
            theme::{scope, Theme},
            Stoat,
        };
        use ratatui::style::Color;
        use std::path::PathBuf;
        use stoat_action::OpenFile;

        let mut h = Stoat::test();
        let root = PathBuf::from("/shared-endpoints");
        let path = root.join("doc.rs");
        let source: String = (0..30)
            .map(|i| format!("fn f{i}() -> u32 {{ let x = {i}; x }}\n"))
            .collect();
        h.fake_fs().insert_file(&path, source.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let theme = Theme::empty();
        let fallback = theme.get(scope::UI_TEXT);
        let gutter = PageGutter::new(
            true,
            Arc::new(RowSeverity::default()),
            Arc::new(theme.clone()),
            None,
            None,
        );

        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");

        // A colored token on each row, so the shared set spans rows the page
        // being painted does not and there is something for a wrong span to
        // drop.
        let (tokens, interner) = {
            let mut interner = HighlightStyleInterner::default();
            let style_id = interner.push(HighlightStyle {
                foreground: Some(Color::Red),
                ..Default::default()
            });
            let snap = editor.display_map.snapshot().buffer_snapshot().clone();
            let tokens: Vec<SemanticTokenHighlight> = (0..30u32)
                .map(|row| {
                    let start = snap.rope().point_to_offset(stoat_text::Point::new(row, 0));
                    SemanticTokenHighlight {
                        range: snap.anchor_at(start, stoat_text::Bias::Right)
                            ..snap.anchor_at(start + 2, stoat_text::Bias::Left),
                        style: style_id,
                    }
                })
                .collect();
            (Arc::from(tokens), Arc::new(interner))
        };
        editor
            .display_map
            .set_semantic_token_highlights(BufferId::new(0), tokens, interner);

        let snapshot = editor.display_map.snapshot();

        let height = 4u16;
        let pages = [2u64, 3, 4];
        let shared = {
            let top = page_top_row(pages[0], height);
            let bottom = page_top_row(pages[pages.len() - 1], height) + height as u32;
            snapshot.highlighted_endpoints(top..bottom)
        };
        assert!(
            !shared.is_empty(),
            "the fixture must carry highlights for this to test anything",
        );

        for index in pages {
            let own = page_endpoints(&snapshot, page_top_row(index, height), height);
            let fill = |endpoints| {
                render_page_fill(
                    &snapshot,
                    5,
                    index,
                    fallback,
                    40,
                    height,
                    &gutter,
                    false,
                    0.0,
                    DiffDials::shipped(),
                    endpoints,
                    None,
                    None,
                )
            };
            assert_eq!(fill(shared.clone()), fill(own), "page {index}");
        }
    }

    /// A page painted from the refill's shared hunks matches one that resolved
    /// its own.
    ///
    /// Sharing them is what spares a run of pages an anchor batch over every
    /// hunk in the file per page. It is only sound because the answer does not
    /// vary by page, and a divergence would show as a diff gutter marking the
    /// wrong rows on some pages and not others.
    ///
    /// The buffer is edited after the diff was taken, so the hunks' stored rows
    /// and their live rows differ. Without that the two paths would agree for
    /// the wrong reason, both reading rows that never moved.
    #[test]
    fn shared_hunks_paint_the_page_that_resolving_per_page_does() {
        use super::{page_top_row, render_page_fill, PageGutter};
        use crate::{
            buffer::{BufferId, TextBuffer},
            diff_map::DiffMap,
            display_map::DisplayMap,
            multi_buffer::MultiBuffer,
            render::editor::RowSeverity,
            theme::{scope, Theme},
        };
        use std::sync::RwLock;
        use stoat_language::structural_diff;
        use stoat_scheduler::{Executor, TestScheduler};

        let base: String = (0..40).map(|i| format!("line {i}\n")).collect();
        let text: String = (0..40)
            .map(|i| {
                if i % 7 == 0 {
                    format!("edited {i}\n")
                } else {
                    format!("line {i}\n")
                }
            })
            .collect();

        let mut tb = TextBuffer::with_text(BufferId::new(0), &text);
        tb.diff_map = Some(DiffMap::from_structural_changes(
            structural_diff::diff(&base, &text),
            Arc::new(base),
            &text,
        ));
        // Inserting above every hunk drags each one's anchors down, so the live
        // rows are not the rows the diff recorded.
        tb.edit(0..0, "// header\n// header\n");
        let shared_buf = Arc::new(RwLock::new(tb));

        let multi = MultiBuffer::singleton(shared_buf);
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let mut display_map = DisplayMap::new(multi, executor, crate::test_notify());
        let snapshot = display_map.snapshot();

        let theme = Theme::empty();
        let fallback = theme.get(scope::UI_TEXT);
        let gutter = PageGutter::new(
            true,
            Arc::new(RowSeverity::default()),
            Arc::new(theme.clone()),
            None,
            None,
        );

        let diff_map = snapshot.diff_map().expect("the fixture carries a diff");
        let live = diff_map.live_hunks(snapshot.buffer_snapshot());
        assert!(
            live.gutter_mark_for_line(7).is_some() || live.gutter_mark_for_line(9).is_some(),
            "the fixture must place a mark on a shifted row for this to test anything",
        );

        let height = 6u16;
        for index in 0..5u64 {
            let fill = |live| {
                render_page_fill(
                    &snapshot,
                    5,
                    index,
                    fallback,
                    40,
                    height,
                    &gutter,
                    false,
                    0.0,
                    DiffDials::shipped(),
                    page_endpoints(&snapshot, page_top_row(index, height), height),
                    live,
                    None,
                )
            };
            assert_eq!(
                fill(Some(&live)),
                fill(None),
                "page {index} painted differently from shared hunks",
            );
        }
    }

    #[test]
    fn render_page_fill_wraps_the_page_in_fill_frames() {
        use super::{render_page_fill, render_page_from_snapshot, PageGutter};
        use crate::{
            action_handlers::{self, dispatch},
            render::editor::RowSeverity,
            theme::{scope, Theme},
            Stoat,
        };
        use std::path::PathBuf;
        use stoat_action::OpenFile;
        use stoatty_protocol::command::FillCommand;

        let mut h = Stoat::test();
        let root = PathBuf::from("/page-fill");
        let path = root.join("doc.txt");
        h.fake_fs()
            .insert_file(&path, b"alpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let fallback = Theme::empty().get(scope::UI_TEXT);
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        let gutter = PageGutter::new(
            false,
            Arc::new(RowSeverity::default()),
            Arc::new(Theme::empty()),
            None,
            None,
        );

        let frame = render_page_fill(
            &snapshot,
            7,
            2,
            fallback,
            12,
            3,
            &gutter,
            false,
            0.0,
            DiffDials::shipped(),
            page_endpoints(&snapshot, 6, 3),
            None,
            None,
        );

        let cmds = commands(&frame);
        assert!(
            cmds.contains(&Command::Fill(FillCommand { pool: 7, index: 2 })),
            "frame opens with the slot's fill, got {cmds:?}"
        );
        assert!(
            cmds.contains(&Command::FillEnd),
            "frame closes the fill, got {cmds:?}"
        );

        let page = render_page_from_snapshot(
            &snapshot,
            2 * 3,
            fallback,
            12,
            3,
            &gutter,
            false,
            0.0,
            DiffDials::shipped(),
            page_endpoints(&snapshot, 6, 3),
            None,
            None,
        );
        assert!(
            find(&frame, &page).is_some(),
            "the page bytes ride between the fill markers"
        );
    }

    #[test]
    fn rich_page_fill_carries_gutter_component_frames() {
        use super::render_page_fill;

        let (snapshot, gutter, fallback) = rich_gutter_page();
        let frame = render_page_fill(
            &snapshot,
            3,
            0,
            fallback,
            12,
            4,
            &gutter,
            false,
            0.0,
            DiffDials::shipped(),
            page_endpoints(&snapshot, 0, 4),
            None,
            None,
        );
        let cmds = commands(&frame);

        assert!(
            cmds.iter().any(|cmd| matches!(cmd, Command::TextRun(_))),
            "the rich page fill carries the scaled line-number runs, got {cmds:?}"
        );
        assert!(
            cmds.iter().any(|cmd| matches!(cmd, Command::Bar(_))),
            "the rich page fill carries the gutter separator bar, got {cmds:?}"
        );
    }

    /// A rich page lays its gutter out as scene components alone, so the page
    /// cells under the gutter keep the page background.
    #[test]
    fn a_rich_page_gutter_writes_no_cell() {
        use super::{page_buffer, paint_page_gutter};
        use ratatui::layout::Rect;

        let (snapshot, gutter, fallback) = rich_gutter_page();
        let area = Rect::new(0, 0, 12, 4);
        let mut buf = page_buffer(area, gutter.theme());
        let (width, components) = paint_page_gutter(
            &snapshot, 0, 4, &mut buf, area, &gutter, fallback, 0.0, None,
        );

        assert_eq!(
            (width > 0, components.is_empty(), buf),
            (true, false, page_buffer(area, gutter.theme())),
            "the gutter reserves its columns and rides the scene, writing no cell",
        );
    }

    /// The snapshot of a six-line document, the rich page gutter of the shipped
    /// theme, which numbers the document absolutely, and that theme's text style.
    fn rich_gutter_page() -> (DisplaySnapshot, PageGutter, Style) {
        use crate::{
            action_handlers::{self, dispatch},
            render::editor::{resolve_rich_gutter, RowSeverity},
            theme::scope,
            Stoat,
        };
        use std::path::PathBuf;
        use stoat_action::OpenFile;

        let mut h = Stoat::test();
        let root = PathBuf::from("/rich-page");
        let path = root.join("doc.txt");
        h.fake_fs()
            .insert_file(&path, b"alpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let theme = h.stoat.theme.clone();
        let fallback = theme.get(scope::UI_TEXT);
        let rich = resolve_rich_gutter(&theme, fallback)
            .expect("the shipped theme resolves the rich gutter colors");
        let gutter = PageGutter::new(
            true,
            Arc::new(RowSeverity::default()),
            theme,
            Some(rich),
            None,
        );

        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        (editor.display_map.snapshot(), gutter, fallback)
    }

    /// A decorations-only batch stands in for the components a whole fill lays
    /// over its cells, so for one page and cursor line the two carry the same
    /// bytes.
    #[test]
    fn a_decorations_batch_carries_the_components_a_whole_fill_lays_over_its_cells() {
        use super::{render_page_decorations, render_page_fill, PageGutter};
        use crate::{
            action_handlers::{self, dispatch},
            render::editor::{resolve_rich_gutter, RowSeverity},
            theme::scope,
            Stoat,
        };
        use std::path::PathBuf;
        use stoat_action::OpenFile;
        use stoatty_protocol::command::{encode_fill_decorations_into, encode_fill_end};

        let mut h = Stoat::test();
        let root = PathBuf::from("/rich-decorations");
        let path = root.join("doc.txt");
        let body: String = (0..12).map(|i| format!("line {i}\n")).collect();
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let theme = h.stoat.theme.clone();
        let fallback = theme.get(scope::UI_TEXT);
        let rich = resolve_rich_gutter(&theme, fallback)
            .expect("the shipped theme resolves the rich gutter colors");
        let gutter = PageGutter::new(
            true,
            Arc::new(RowSeverity::default()),
            theme.clone(),
            Some(rich),
            Some(6),
        );
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();

        let fill = render_page_fill(
            &snapshot,
            3,
            1,
            fallback,
            12,
            4,
            &gutter,
            false,
            0.0,
            DiffDials::shipped(),
            page_endpoints(&snapshot, 4, 4),
            None,
            None,
        );
        let decorations = render_page_decorations(
            &snapshot,
            3,
            1,
            12,
            4,
            &gutter,
            false,
            fallback,
            0.0,
            DiffDials::shipped(),
            None,
        );

        let mut open = Vec::new();
        encode_fill_decorations_into(&mut open, 3, 1);
        let close = encode_fill_end();
        let components = decorations
            .strip_prefix(open.as_slice())
            .and_then(|rest| rest.strip_suffix(close.as_slice()))
            .expect("the batch is one decorations-only scope for page 1");
        assert!(!components.is_empty(), "page 1 has numbers to lay out");
        assert!(
            fill.ends_with(&[components, close.as_slice()].concat()),
            "the whole fill of page 1 closes with the same components"
        );
    }

    /// A pooled page's rich gutter draws the plain editor's hunk and state
    /// lanes where the live gutter draws them, the hunk bar at the kind's full
    /// color on a row the tree pass reached and on one it has not.
    #[test]
    fn a_pooled_page_gutter_draws_the_hunk_and_state_lanes() {
        use super::{render_page_decorations, PageGutter};
        use crate::{
            action_handlers::{self, dispatch},
            diff_map::{DiffHunk, DiffHunkStatus, DiffMap, TokenDetail},
            minimap::color_to_rgb,
            render::editor::{resolve_rich_gutter, RowSeverity},
            theme::scope,
            Stoat,
        };
        use std::path::PathBuf;
        use stoat_action::OpenFile;
        use stoatty_protocol::command::{encode_bar, BarCommand};

        let mut h = Stoat::test();
        let root = PathBuf::from("/pooled-bar");
        let path = root.join("doc.txt");
        h.fake_fs().insert_file(&path, b"a\nB\nC\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let reached = DiffHunk {
            status: DiffHunkStatus::Modified,
            buffer_start_line: 1,
            buffer_line_range: 1..2,
            base_byte_range: 2..4,
            anchor_range: None,
            token_detail: Some(Arc::new(TokenDetail {
                buffer_spans: Vec::new(),
                base_spans: Vec::new(),
                tree: true,
            })),
            unstaged_lines: std::iter::once(1..2).collect(),
            marked_rows: std::iter::once(1..2).collect(),
            relocated_rows: Vec::new(),
        };
        let unreached = DiffHunk {
            buffer_start_line: 2,
            buffer_line_range: 2..3,
            base_byte_range: 4..6,
            token_detail: None,
            unstaged_lines: std::iter::once(2..3).collect(),
            marked_rows: Vec::new(),
            relocated_rows: Vec::new(),
            ..reached.clone()
        };
        let buffer_id = action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .buffer_id;
        h.stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .write()
            .expect("poisoned")
            .diff_map = Some(DiffMap::from_hunks(
            [reached, unreached],
            Some(Arc::new("a\nb\nc\n".to_string())),
        ));

        let theme = h.stoat.theme.clone();
        let fallback = theme.get(scope::UI_TEXT);
        let rich = resolve_rich_gutter(&theme, fallback)
            .expect("the shipped theme resolves the rich gutter colors");
        let gutter = PageGutter::new(
            true,
            Arc::new(RowSeverity::default()),
            theme.clone(),
            Some(rich),
            None,
        );
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let decorations = render_page_decorations(
            &editor.display_map.snapshot(),
            3,
            0,
            12,
            4,
            &gutter,
            false,
            fallback,
            0.0,
            DiffDials::shipped(),
            None,
        );

        let color = |scope: &str| color_to_rgb(theme.get(scope).fg.expect("the theme sets it"));
        let bar = |x, y, color| {
            let command = encode_bar(&BarCommand {
                x,
                y,
                width: 6,
                height: 16,
                color,
                occludes: false,
                hatch: None,
                radius: 0,
                corners: 0,
            });
            decorations
                .windows(command.len())
                .any(|window| window == command.as_slice())
        };
        let (modified, unstaged) = (color(scope::DIFF_MODIFIED), color(scope::DIFF_UNSTAGED));
        assert_eq!(
            [
                bar(0, 16, modified),
                bar(7, 16, unstaged),
                bar(0, 32, modified),
                bar(7, 32, unstaged),
            ],
            [true; 4],
            "both lanes on rows 1 and 2, the hunk bar at full color on both",
        );
    }

    /// A plain editor's page fills the rows of its cursor line with one bar
    /// across the page, ahead of the gutter's own bars, and a page with no
    /// cursor line draws no such bar.
    #[test]
    fn a_page_draws_its_cursor_line_as_one_bar() {
        use super::{render_page_decorations, PageGutter};
        use crate::{
            action_handlers::{self, dispatch},
            render::editor::{resolve_rich_gutter, RowSeverity},
            theme::scope,
            Stoat,
        };
        use std::path::PathBuf;
        use stoat_action::OpenFile;
        use stoatty_protocol::command::{decode_stream, Command};

        let mut h = Stoat::test();
        let root = PathBuf::from("/page-cursor-line");
        let path = root.join("doc.txt");
        h.fake_fs().insert_file(&path, b"a\nabcdefghij\nc\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let theme = h.stoat.theme.clone();
        let fallback = theme.get(scope::UI_TEXT);
        let rich = resolve_rich_gutter(&theme, fallback)
            .expect("the shipped theme resolves the rich gutter colors");
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        // Line 2 breaks into two rows at width 8.
        editor.display_map.set_wrap_width(Some(8));
        let snapshot = editor.display_map.snapshot();

        let fill = [0x1b, 0x1e, 0x24];
        let bars = |cursor_line| -> Vec<_> {
            let gutter = PageGutter::new(
                true,
                Arc::new(RowSeverity::default()),
                theme.clone(),
                Some(rich.clone()),
                None,
            )
            .cursor_line(cursor_line);
            let decorations = render_page_decorations(
                &snapshot,
                3,
                0,
                12,
                4,
                &gutter,
                false,
                fallback,
                0.0,
                DiffDials::shipped(),
                None,
            );
            decode_stream(&decorations)
                .into_iter()
                .filter_map(|command| match command {
                    Command::Bar(bar) => Some((bar.x, bar.y, bar.width, bar.height, bar.color)),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            bars(Some((2, fill))),
            [vec![(0, 16, 12 * 16, 32, fill)], bars(None)].concat(),
            "the fill spans the page over line 2's two rows, ahead of the gutter's own bars",
        );
    }

    /// A pooled page marks an error with its icon for a terminal that stores
    /// page icons. For a terminal that drops them, it marks the error with the
    /// hint's dot run.
    #[test]
    fn a_pooled_page_marks_a_diagnostic_with_an_icon_or_a_dot() {
        use super::render_page_decorations;
        use crate::{
            action_handlers::{self, dispatch},
            render::editor::{resolve_rich_gutter, RowSeverity},
            theme::scope,
            Stoat,
        };
        use lsp_types::DiagnosticSeverity;
        use std::path::PathBuf;
        use stoat_action::OpenFile;
        use stoatty_protocol::command::decode_stream;

        let mut h = Stoat::test();
        let root = PathBuf::from("/pooled-dot");
        let path = root.join("doc.txt");
        h.fake_fs().insert_file(&path, b"a\nb\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let theme = h.stoat.theme.clone();
        let fallback = theme.get(scope::UI_TEXT);
        let rich = resolve_rich_gutter(&theme, fallback)
            .expect("the shipped theme resolves the rich gutter colors");
        let gutter = PageGutter::new(
            true,
            Arc::new(RowSeverity::for_rows([(0, DiagnosticSeverity::ERROR)])),
            theme.clone(),
            Some(rich),
            None,
        );
        let snapshot = action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .display_map
            .snapshot();
        let marks = |gutter: &PageGutter| {
            let decorations = render_page_decorations(
                &snapshot,
                3,
                0,
                12,
                4,
                gutter,
                false,
                fallback,
                0.0,
                DiffDials::shipped(),
                None,
            );
            let commands = decode_stream(&decorations);
            let dots = commands
                .iter()
                .filter(|command| matches!(command, Command::TextRun(run) if run.text == "\u{b7}"))
                .count();
            let icons = commands
                .iter()
                .filter(|command| matches!(command, Command::Icon(_)))
                .count();
            (dots, icons)
        };

        assert_eq!(
            [marks(&gutter.clone().page_icons(true)), marks(&gutter)],
            [(0, 1), (1, 0)],
            "an icon where the terminal stores it, and a dot where it drops it",
        );
    }

    #[test]
    fn non_pane_pool_ids_are_distinct_and_above_the_base() {
        use super::non_pane_pool::{
            BASE, COMMITS, COMPLETION, FINDER, HELP_DETAIL, HELP_LIST, HOVER, PALETTE, SYMBOL,
            WINDOW_STATUS, WORKSPACE_SYMBOL,
        };
        use std::collections::BTreeSet;

        let ids = [
            COMMITS,
            FINDER,
            PALETTE,
            COMPLETION,
            HELP_LIST,
            HELP_DETAIL,
            SYMBOL,
            WORKSPACE_SYMBOL,
            HOVER,
            WINDOW_STATUS,
        ];
        assert!(
            ids.iter().all(|&id| id >= BASE),
            "every non-pane pool id sits at or above the base"
        );

        let unique: BTreeSet<u32> = ids.iter().copied().collect();
        assert_eq!(
            unique.len(),
            ids.len(),
            "non-pane pool ids must be pairwise distinct: {ids:?}"
        );
    }

    /// A pooled palette page leaves the group token out of each name, as the
    /// live list does under a group.
    #[test]
    fn a_palette_page_leaves_the_group_token_out() {
        use super::render_palette_page;
        use crate::{
            command_palette::{Namespace, PaletteCommand},
            keymap::ActionKeys,
            theme::Theme,
        };
        use std::collections::HashMap;
        use stoat_action::registry;

        let entry = registry::lookup_alias("git log").expect("git log is a command");
        let page = |name_offset| {
            let bytes = render_palette_page(
                &[PaletteCommand::Action(entry)],
                &[Vec::new()],
                0,
                name_offset,
                Namespace::Commands,
                &ActionKeys::new(),
                &HashMap::new(),
                0,
                &Theme::empty(),
                100,
                1,
                None,
            );
            String::from_utf8_lossy(&bytes).into_owned()
        };
        let (whole, grouped) = (page(0), page("git ".len()));
        assert_eq!(
            (
                whole.contains("git log"),
                grouped.contains("git log"),
                grouped.contains(entry.def.short_desc()),
            ),
            (true, false, true),
        );
    }

    /// A page of a picker's file preview numbers its lines as the live preview
    /// does, so a glide shows no shift as the pool composites over the pane.
    ///
    /// The three lines fill the first page and end on the first row of the page
    /// at row 2, and the page past the end draws no number. Under a dead scene
    /// the digits go into the column's cells, right-aligned before one blank
    /// cell. A page at Preview Scale draws the numbers and the rule that a live
    /// paint of the scaled column draws.
    #[test]
    fn a_preview_page_numbers_its_lines_as_the_live_preview_does() {
        use super::{page_buffer, render_page_from_snapshot, serialize_buffer, Rect};
        use crate::{
            render::{
                editor::{ResolvedChrome, RowSeverity},
                picker::{paint_preview_numbers, render_picker_preview, NumberColumn},
            },
            test_harness::TestHarness,
            theme::scope,
            workspace::Workspace,
        };
        use std::path::PathBuf;
        use stoat_widgets::ApcScene;
        use stoatty_protocol::command::decode_stream;

        let mut h = TestHarness::with_size(120, 30);
        let root = PathBuf::from("/preview-numbers");
        h.fake_fs()
            .insert_file(root.join("a.txt"), b"alpha\nbeta\ngamma");
        h.stoat.active_workspace_mut().git_root = root;
        h.type_keys("space p");
        h.settle();

        let theme = h.stoat.theme.clone();
        let fallback = theme.get(scope::UI_TEXT);
        let chrome = ResolvedChrome::resolve(&theme);
        let gutter = PageGutter::new(
            false,
            Arc::new(RowSeverity::default()),
            theme.clone(),
            None,
            None,
        )
        .number_column(NumberColumn::Grid);
        let area = Rect::new(0, 0, 16, 2);
        let mut finder = h.stoat.file_finder.take().expect("the finder is open");
        let editor_id = finder.active_core_ref().preview.editor;
        let preview = &mut finder.active_core().preview;
        let ws = h.stoat.active_workspace_mut();

        let mut paint = |ws: &mut Workspace, top_row: u32, live: bool| {
            if let Some(editor) = ws.editors.get_mut(editor_id) {
                editor.scroll_row = top_row;
            }
            let mut buf = page_buffer(area, &theme);
            let mut scene = ApcScene::new();
            scene.set_live(live);
            render_picker_preview(preview, area, &theme, &chrome, ws, &mut buf, &mut scene);
            (buf, scene)
        };
        let numbers = |bytes: &[u8]| {
            decode_stream(bytes)
                .into_iter()
                .filter_map(|command| match command {
                    Command::TextRun(run) => Some(run.text),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ")
        };

        for (top_row, shown) in [(0, "1 2"), (2, "3"), (7, "")] {
            let (live, mut scene) = paint(ws, top_row, true);
            let mut expected = serialize_buffer(&live);
            expected.extend_from_slice(scene.buffer());

            let snapshot = ws
                .editors
                .get_mut(editor_id)
                .expect("the preview's editor")
                .display_map
                .snapshot();
            let page = render_page_from_snapshot(
                &snapshot,
                top_row,
                fallback,
                area.width,
                area.height,
                &gutter,
                false,
                0.0,
                DiffDials::shipped(),
                page_endpoints(&snapshot, top_row, area.height),
                None,
                None,
            );

            assert_eq!(
                (numbers(&page), page == expected),
                (shown.to_owned(), true),
                "the page at row {top_row} numbers its lines and paints as the live preview",
            );
        }

        let (dead, _) = paint(ws, 0, false);
        let first_row: String = (0..area.width).map(|x| dead[(x, 0)].symbol()).collect();
        assert_eq!(
            first_row.trim_end(),
            "   1 alpha",
            "the first row under a dead scene"
        );

        let snapshot = ws
            .editors
            .get_mut(editor_id)
            .expect("the preview's editor")
            .display_map
            .snapshot();
        let scaled = render_page_from_snapshot(
            &snapshot,
            0,
            fallback,
            area.width,
            area.height,
            &gutter.clone().number_column(NumberColumn::Scaled),
            false,
            0.0,
            DiffDials::shipped(),
            page_endpoints(&snapshot, 0, area.height),
            None,
            None,
        );
        let mut live = ApcScene::new();
        let mut cells = page_buffer(area, &theme);
        paint_preview_numbers(
            &snapshot, 0, area, &theme, &mut cells, &mut live, true, None,
        );
        let live = live.buffer().clone();
        let marks = |bytes: &[u8]| {
            decode_stream(bytes)
                .into_iter()
                .filter(|command| matches!(command, Command::TextRun(_) | Command::Bar(_)))
                .collect::<Vec<_>>()
        };
        let page_marks = marks(&scaled);
        assert_eq!(
            (
                page_marks
                    .iter()
                    .filter(|command| matches!(command, Command::Bar(_)))
                    .count(),
                page_marks
            ),
            (1, marks(&live)),
            "a scaled page rules its numbers off where the live scaled column does",
        );
    }

    /// A page of a preview synced to a line marks that line's row as the live
    /// preview marks it, cell for cell and component for component.
    ///
    /// Line 1 of three, synced a third of the way down a 3-row pane, stays on
    /// row 1. A page at Preview Scale draws the mark's bar 6 sixteenths wide,
    /// the board's 2 px at the scaled cell, after the column's rule. A line
    /// past the file's end marks no row.
    #[test]
    fn a_page_marks_the_target_row_as_the_live_paint_does() {
        use super::{page_buffer, render_page_from_snapshot, serialize_buffer, Rect};
        use crate::{
            picker::PreviewSource,
            render::{
                editor::{ResolvedChrome, RowSeverity},
                picker::{render_picker_preview, NumberColumn},
            },
            test_harness::TestHarness,
            theme::scope,
        };
        use std::path::PathBuf;
        use stoat_widgets::ApcScene;
        use stoatty_protocol::command::decode_stream;

        let mut h = TestHarness::with_size(120, 30);
        let root = PathBuf::from("/preview-mark");
        let file = root.join("a.txt");
        h.fake_fs().insert_file(file.clone(), b"alpha\nbeta\ngamma");
        h.stoat.active_workspace_mut().git_root = root;
        h.type_keys("space p");
        h.settle();

        let theme = h.stoat.theme.clone();
        let fallback = theme.get(scope::UI_TEXT);
        let chrome = ResolvedChrome::resolve(&theme);
        let area = Rect::new(0, 0, 16, 3);
        let mut finder = h.stoat.file_finder.take().expect("the finder is open");
        let preview = &mut finder.active_core().preview;
        let index = h.stoat.active_workspace;
        let ws = &mut h.stoat.workspaces[index];
        let (fs, languages) = (&*h.stoat.fs_host, &h.stoat.language_registry);
        let source = PreviewSource::File(file);
        preview.sync_at_line(ws, fs, languages, source, 1, usize::from(area.height));

        let mut live = page_buffer(area, &theme);
        let mut scene = ApcScene::new();
        render_picker_preview(preview, area, &theme, &chrome, ws, &mut live, &mut scene);
        let mut expected = serialize_buffer(&live);
        expected.extend_from_slice(scene.buffer());

        let snapshot = ws
            .editors
            .get_mut(preview.editor)
            .expect("the preview's editor")
            .display_map
            .snapshot();
        let gutter = PageGutter::new(
            false,
            Arc::new(RowSeverity::default()),
            theme.clone(),
            None,
            None,
        );
        let page = |column: NumberColumn, target_line: Option<u32>| {
            render_page_from_snapshot(
                &snapshot,
                0,
                fallback,
                area.width,
                area.height,
                &gutter
                    .clone()
                    .number_column(column)
                    .target_line(target_line),
                false,
                0.0,
                DiffDials::shipped(),
                page_endpoints(&snapshot, 0, area.height),
                None,
                None,
            )
        };
        let bars = |bytes: &[u8]| {
            decode_stream(bytes)
                .into_iter()
                .filter_map(|command| match command {
                    Command::Bar(bar) => Some((bar.x, bar.y, bar.width, bar.height)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        let grid = page(NumberColumn::Grid, preview.target_line);
        assert_eq!(
            (bars(&grid), grid == expected),
            (vec![(0, 16, 4, 16)], true),
            "the grid page marks row 1 as the live preview does",
        );
        assert_eq!(
            bars(&page(NumberColumn::Scaled, preview.target_line)),
            vec![(77, 0, 3, 48), (0, 16, 6, 16)],
            "a scaled page rules its numbers off, then marks row 1 with the wider bar",
        );
        assert_eq!(
            bars(&page(NumberColumn::Grid, Some(9))),
            vec![],
            "a line past the file's end marks no row",
        );
    }
}

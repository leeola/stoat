//! Drives stoatty's region-scoped smooth-scroll pools for the visible editor
//! panes.
//!
//! Every visible editor pane gets its own pool in the terminal's recycled page
//! pool. Stoat declares the pane's on-screen rectangle as that pool's region,
//! renders the document a page at a time into off-grid pool slots, and reports
//! an absolute scroll target each time the pane scrolls. The terminal eases each
//! pool's visible offset toward its target at sub-cell granularity, so several
//! panes glide independently and at once while the chrome around them (status
//! bars, dividers) stays fixed.
//!
//! Pools are keyed by [`crate::pane::Pane::index`], a stable per-pane id, so a
//! pane keeps the same pool across frames. A pane that stops being a plain editor
//! -- closed, switched to another view, turned into a review, or hidden behind a
//! full-screen overlay -- is retired with `Gstoatty;pool_drop` so the terminal
//! frees its buffers and stops compositing it.
//!
//! A "page" is one region-sized screen of the document: `region.height` rows of
//! `region.width` columns, the page at index `p` starting at document row
//! `p * region.height`. Each pool is addressed by this page index, the same key
//! [`ScrollCommand::page`] and [`FillCommand::index`] carry.

use crate::{
    command_palette::ArgPicker,
    commit_list::CommitListState,
    completion::CompletionItem,
    conflict_session::ConflictViewState,
    display_map::DisplaySnapshot,
    file_finder::FileFinder,
    help::Help,
    render::{
        command_palette::paint_palette_rows,
        commits::paint_commit_rows,
        completion::paint_completion_rows,
        conflict_view::render_conflict_rows,
        editor::{
            draw_fallback_line_numbers, gutter_component_lines, gutter_diff_marks, gutter_geometry,
            rich_gutter, RichGutterColors,
        },
        file_finder::paint_finder_rows,
        help::{paint_help_detail_rows, paint_help_list_rows},
        review::{paint_diff_rows, render_review_rows},
    },
    review_session::ReviewViewState,
};
use lsp_types::DiagnosticSeverity;
use ratatui::{buffer::Buffer, layout::Rect, style::Style};
use std::{
    collections::{BTreeMap, HashMap},
    ops::Range,
    sync::Arc,
};
use stoat_action::registry::RegistryEntry;
use stoatty_protocol::command::{
    encode_fill_end_into, encode_fill_into, encode_minimap_view_into, encode_pool_drop_into,
    encode_pool_region_into, encode_reposition_into, encode_scroll_into, MinimapViewCommand,
    PoolRegionCommand, ScrollCommand,
};
use stoatty_widgets::ApcScene;

/// Pages kept buffered around each pool's visible page, the pool's working
/// window. Wide enough that the visible page and its straddle neighbour (when a
/// fractional scroll shows the bottom of one page and the top of the next) are
/// always present, plus slack so an in-flight ease never outruns the filled
/// slots.
const WINDOW_PAGES: u64 = 5;

/// Pool ids for the non-pane smooth-scroll surfaces (overlays and popups).
///
/// Split-pane editor pools key on [`crate::pane::Pane::index`], a monotonic
/// `u32` counter from 1, so reserving every non-pane id at or above [`BASE`]
/// keeps them from ever colliding with a pane. The renderer composites pools in
/// ascending-id z-order, so these higher ids always composite above the
/// split-pane editors beneath them. Each surface's emit consumes its id from
/// here as it lands.
// Reserved allocation table: the per-surface ids are consumed by the
// per-surface emit items as they land, so they read as unused until then.
#[allow(dead_code)]
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
    pub(crate) const SYMBOL: u32 = BASE + 6;
    pub(crate) const WORKSPACE_SYMBOL: u32 = BASE + 7;
    pub(crate) const HOVER: u32 = BASE + 8;
    /// First id of the per-window status-bar partition. A detached pane's status
    /// row pools at `WINDOW_STATUS + pane.index`, so the partition is offset far
    /// enough above the fixed non-pane ids that pane indices never collide.
    pub(crate) const WINDOW_STATUS: u32 = BASE + 0x100;
}

/// Per-app smooth-scroll emit state: what has been declared to the terminal for
/// each pool, so each frame emits only the deltas.
///
/// Held by [`crate::app::Stoat`] and threaded into [`emit_into`] (once per
/// visible editor pane) and [`SmoothScrollState::drop_absent`] at the frame seam.
/// Empty on construction; a pool is added on its first [`emit_into`] and removed
/// by [`SmoothScrollState::drop_absent`] when its pane goes away.
#[derive(Default)]
pub(crate) struct SmoothScrollState {
    pools: BTreeMap<u32, PoolEmitState>,
    /// `(pool, top_256, visible_lines)` the most recent [`MinimapViewCommand`]
    /// carried per strip id, so an unmoved viewport re-emits no thumb update.
    /// The pool is part of the value because single-minimap mode feeds one strip
    /// from different pools as focus moves, so a same-offset view from a new pool
    /// must still re-emit.
    minimap_views: HashMap<u32, (u32, u32, u16)>,
}

/// What has been declared to the terminal for one pool, so a frame re-emits only
/// the region (when it moves), the pages newly entering the window, and the
/// scroll target (when it moves).
#[derive(Default)]
struct PoolEmitState {
    /// Region declared on the most recent emit, in absolute grid cells. `None`
    /// until first declared. Re-emitted only when the rectangle changes (resize,
    /// split, focus move).
    region: Option<PoolRegionCommand>,
    /// Half-open page range `[start, end)` whose fills have been requested for the
    /// pool, `None` until the first request.
    ///
    /// Non-pane callers fill synchronously, so this equals what is filled. The
    /// editor caller fills asynchronously off-thread, so it tracks requests, not
    /// completions. The window is always contiguous, so a `Range` suffices.
    /// Re-requesting a page when it re-enters the window is correct -- it matches
    /// the terminal recycling slots that fall outside the window.
    requested: Option<Range<u64>>,
    /// `scroll_offset` the most recent [`ScrollCommand`] was computed from.
    /// Skips re-emitting an unchanged scroll target.
    last_scroll_offset: Option<f32>,
    /// Content version last seen for this pool. When the caller passes a
    /// different value the buffered pages are stale (the surface re-filtered or
    /// regenerated), so the window is refilled rather than composited as-is.
    content_version: u64,
}

impl SmoothScrollState {
    /// Retire every tracked pool whose id is not in `active`: emit its
    /// `Gstoatty;pool_drop` into `out` and forget it.
    ///
    /// Called once per frame with the ids of the panes that are pooled this
    /// frame, so a closed pane, a pane switched to another view, a review, or one
    /// hidden behind a full-screen overlay stops compositing and frees its
    /// terminal-side buffers. A later pane reusing the id re-declares from
    /// scratch.
    pub(crate) fn drop_absent(&mut self, out: &mut Vec<u8>, active: &[u32]) {
        let stale: Vec<u32> = self
            .pools
            .keys()
            .copied()
            .filter(|id| !active.contains(id))
            .collect();
        for id in stale {
            encode_pool_drop_into(out, id);
            self.pools.remove(&id);
        }
    }

    /// Append a `minimap_view` frame positioning `strip_id`'s thumb to `out`, but
    /// only when the viewport moved since the last emit for that strip.
    ///
    /// `pool` is the scroll pool feeding the strip this frame. `top_256` is the
    /// fractional top document row in 1/256ths of a line, and `visible` the
    /// viewport height in lines. The dedup keys on `strip_id` and includes `pool`
    /// in the stored value, so a strip fed by a new pool re-emits even at an
    /// unchanged offset.
    pub(crate) fn emit_minimap_view(
        &mut self,
        out: &mut Vec<u8>,
        strip_id: u32,
        pool: u32,
        top_256: u32,
        visible: u16,
    ) {
        if self.minimap_views.get(&strip_id) == Some(&(pool, top_256, visible)) {
            return;
        }
        encode_minimap_view_into(
            out,
            &MinimapViewCommand {
                strip_id,
                top_256,
                visible_lines: visible,
            },
        );
        self.minimap_views
            .insert(strip_id, (pool, top_256, visible));
    }
}

/// Append the smooth-scroll APC frames for one pool's current scroll position to
/// `out`, updating `state` to reflect what was emitted.
///
/// `region` is the pane's body rectangle in absolute grid cells, carrying the
/// pool id ([`PoolRegionCommand::pool`]) the pool is tracked under.
/// `scroll_offset` is the editor's fractional top visible document row. Its
/// integer part selects the page and its fraction drives the sub-row glide. The
/// closure `render_page` paints page `index` (document rows
/// `index * region.height ..`) into a region-sized [`Buffer`] and returns its
/// self-contained VT bytes.
///
/// `content_version` changes whenever the surface's content changes (a
/// re-filtered list, a regenerated diff); a value differing from the last emit
/// forces the buffered window to refill so a stale page is never composited.
/// Pass a constant for content that is stable while scrolling.
///
/// Emits, in order: a `pool_region` frame when the rectangle changed; a
/// `fill`/page-VT/`fill_end` triple for each page newly entering the buffered
/// window; a `reposition` frame when the new window is disjoint from the old, so
/// a far jump re-anchors near the destination instead of easing across the gap;
/// then a `scroll` frame carrying the precise target. A frame that needs none of
/// these appends nothing.
///
/// Returns the page indices that newly entered the buffered window this call, in
/// ascending order. A caller filling synchronously ignores them (the fill bytes
/// are already in `out`); the editor caller passes an empty-returning `render_page`
/// and fills these pages asynchronously off-thread instead.
pub(crate) fn emit_into(
    out: &mut Vec<u8>,
    state: &mut SmoothScrollState,
    region: PoolRegionCommand,
    scroll_offset: f32,
    content_version: u64,
    hold_when_idle: bool,
    mut render_page: impl FnMut(u64) -> Vec<u8>,
) -> Vec<u64> {
    let pool = region.pool;
    let entry = state.pools.entry(pool).or_default();

    // Pools composite only while the eased offset moves, so a content change seen
    // while the target is stationary can wait for the next move. Holding keeps the
    // stored version (suppressing the refill wipe) and narrows this emit to the
    // visible page, deferring the full-window prefill until the target shifts.
    //
    // Computed before the region and version wipes below reset last_scroll_offset,
    // so it reflects real scroll motion. A fresh entry has last_scroll_offset None,
    // so its first display counts as scrolling and still prefills the whole window.
    let scrolling = entry.last_scroll_offset != Some(scroll_offset);
    let hold = hold_when_idle && !scrolling;

    if entry.region != Some(region) {
        encode_pool_region_into(out, &region);
        entry.region = Some(region);
        // A fresh region invalidates the pool's slot contents. Force a refill.
        entry.requested = None;
        entry.last_scroll_offset = None;
    }

    let effective_version = if hold {
        entry.content_version
    } else {
        content_version
    };
    if entry.content_version != effective_version {
        // The surface changed under the pool. The buffered pages are stale.
        entry.requested = None;
        entry.last_scroll_offset = None;
        entry.content_version = effective_version;
    }

    let region_height = region.height.max(1) as u64;
    let page = scroll_offset.floor() as u64 / region_height;
    let window = if hold {
        page..page + 1
    } else {
        window_range(page)
    };

    let prev = entry.requested.clone();
    let jumped = prev.is_some_and(|p| p.end <= window.start || window.end <= p.start);

    let entered = refill(out, entry, pool, window, &mut render_page);

    // A jump whose new window does not overlap the old one is too far to ease
    // across an unbuffered gap. The reposition re-anchors the terminal's offset
    // near the destination; the scroll below still carries the precise target,
    // so the glide lands on `scroll_row` rather than the page boundary the
    // reposition alone would force.
    if jumped {
        encode_reposition_into(out, pool, page);
    }

    if entry.last_scroll_offset != Some(scroll_offset) {
        encode_scroll_into(out, &scroll_target(pool, scroll_offset, region.height));
        entry.last_scroll_offset = Some(scroll_offset);
    }

    entered
}

/// Request a fill for every page in `window` not already requested, record `window`
/// as the requested range, and return the newly-entered page indices in ascending
/// order.
///
/// Pages already covered by the previous window are not re-pushed, so a sub-page
/// scroll that does not change the window enters no pages and a one-page step enters
/// only the single page at the edge.
///
/// `render_page(index)` returning empty bytes requests the page without emitting a
/// fill frame. The editor caller fills asynchronously, so an empty render means "no
/// synchronous fill". A real render is never empty -- the serialized buffer always
/// carries cursor moves and cells -- so empty is an unambiguous sentinel.
fn refill(
    out: &mut Vec<u8>,
    entry: &mut PoolEmitState,
    pool: u32,
    window: Range<u64>,
    render_page: &mut impl FnMut(u64) -> Vec<u8>,
) -> Vec<u64> {
    let already = entry.requested.clone().unwrap_or(0..0);
    let mut entered = Vec::new();
    for index in window.clone() {
        if already.contains(&index) {
            continue;
        }
        entered.push(index);
        let bytes = render_page(index);
        if !bytes.is_empty() {
            encode_fill_into(out, pool, index);
            out.extend_from_slice(&bytes);
            encode_fill_end_into(out);
        }
    }
    entry.requested = Some(window);
    entered
}

/// The half-open page window centered on `page`, clamped at the document start.
///
/// Centering leaves pages buffered on both sides of the visible page so an ease
/// lagging behind a jump stays covered in either direction.
fn window_range(page: u64) -> Range<u64> {
    let start = page.saturating_sub(WINDOW_PAGES / 2);
    start..start + WINDOW_PAGES
}

/// Map a fractional top visible document row to pool `pool`'s scroll target, a
/// page index plus a sub-page fraction in 1/65536ths of a page.
///
/// `region_height` is the pool region's row count, the rows per page. The page
/// is the integer number of full regions scrolled past. The fraction is how far
/// into the next page the partial offset sits, carrying the sub-row part so the
/// terminal can ease the pool below a whole row.
fn scroll_target(pool: u32, scroll_offset: f32, region_height: u16) -> ScrollCommand {
    let height = region_height.max(1) as f32;
    let page = (scroll_offset / height).floor();
    let within = scroll_offset - page * height;
    let fraction = (within / height * 65536.0).round().clamp(0.0, 65535.0) as u16;
    ScrollCommand {
        pool,
        page: page as u64,
        fraction,
    }
}

/// Render page `index` from `snapshot` and wrap it in the pool fill frames, so the
/// returned bytes are a self-contained fill the terminal applies to slot `index`.
///
/// The asynchronous editor-fill path runs this on a blocking worker and delivers
/// the frame through the APC channel, off the run loop. The bytes are an
/// `encode_fill_into` marker, the [`render_page_from_snapshot`] page, then an
/// `encode_fill_end_into` terminator.
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
) -> Vec<u8> {
    let top_row = index
        .saturating_mul(region_height as u64)
        .min(u32::MAX as u64) as u32;
    let bytes = render_page_from_snapshot(
        snapshot,
        top_row,
        fallback_style,
        region_width,
        region_height,
        gutter,
        diff_view,
        dim,
    );

    let mut frame = Vec::with_capacity(bytes.len() + 16);
    encode_fill_into(&mut frame, pool, index);
    frame.extend_from_slice(&bytes);
    encode_fill_end_into(&mut frame);
    frame
}

/// Paint `region_height` document rows starting at display row `top_row` from an
/// owned [`DisplaySnapshot`] into a self-contained VT byte stream.
///
/// Takes a snapshot rather than `&mut EditorState` so a page can render off the
/// run-loop thread. A [`DisplaySnapshot`] is `Send` and carries everything the
/// text needs, and the uncached [`DisplaySnapshot::highlighted_chunks`] keeps the
/// render from touching the editor's shared endpoint cache.
///
/// Paints the line-number gutter, then the text and syntax highlights inset past
/// it -- the same cells the unfocused live grid paints for these rows, minus the
/// cursor and selection a pooled page never carries. Rows past the document end
/// stay blank, and the [`serialize_buffer`] bytes fully repaint the slot
/// regardless of its prior contents.
///
/// Inside stoatty the rich gutter's sub-cell components ride as APC frames
/// appended after the serialized cells, so the terminal captures them onto the
/// page slot. Every other terminal gets degraded cell numbers in the buffer.
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
) -> Vec<u8> {
    let area = Rect::new(0, 0, region_width, region_height);
    let mut buf = Buffer::empty(area);

    // A diff-view page paints the two columns itself, including both line-number
    // gutters, so it bypasses the single-column page gutter. Inside stoatty its
    // rich sub-cell components ride as APC frames appended after the serialized
    // cells, faded to match the dimmed grid.
    if diff_view {
        let mut scene = gutter.rich.is_some().then(ApcScene::new);
        paint_diff_rows(
            snapshot,
            top_row,
            area,
            fallback_style,
            gutter.theme(),
            &mut buf,
            scene.as_mut(),
            dim,
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

    let (gutter_w, apc) = paint_page_gutter(snapshot, top_row, end_row, &mut buf, area, gutter);

    if end_row > top_row {
        let right = area.x + area.width;
        let bottom = area.y + area.height;
        let text_x = area.x + gutter_w;
        let mut x = text_x;
        let mut y = area.y;
        let inlay_style =
            fallback_style.patch(gutter.theme().get(crate::theme::scope::UI_VIRTUAL_INLAY));
        'chunks: for chunk in snapshot.highlighted_chunks(top_row..end_row) {
            let style = if chunk.is_inlay {
                inlay_style
            } else {
                chunk
                    .highlight_style
                    .as_ref()
                    .map(|hs| hs.to_ratatui_style())
                    .unwrap_or(fallback_style)
            };
            for ch in chunk.text.chars() {
                if ch == '\n' {
                    y += 1;
                    x = text_x;
                    if y >= bottom {
                        break 'chunks;
                    }
                    continue;
                }
                if x >= right {
                    continue;
                }
                buf[(x, y)].set_char(ch).set_style(style);
                x += 1;
            }
        }
    }

    dim_page(&mut buf, area, gutter.theme(), dim);
    let mut bytes = serialize_buffer(&buf);
    bytes.extend_from_slice(&apc);
    bytes
}

/// Blend the page's cells toward the theme background, dimming a pooled page for
/// an unfocused pane the same way the live grid dims. A no-op when `dim` is zero
/// or the theme background is not RGB.
fn dim_page(buf: &mut Buffer, area: Rect, theme: &crate::theme::Theme, dim: f32) {
    if dim > 0.0
        && let Some(bg) = crate::render::review::style_rgb(
            theme
                .try_get(crate::theme::scope::UI_BACKGROUND)
                .and_then(|s| s.bg),
        )
    {
        crate::render::pane::dim_pane_content(buf, area, bg, dim);
    }
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
    severity: Arc<BTreeMap<u32, DiagnosticSeverity>>,
    theme: Arc<crate::theme::Theme>,
    rich: Option<RichGutterColors>,
}

impl PageGutter {
    /// Bundle the gutter inputs resolved on the run loop for an editor page fill.
    ///
    /// `line_numbers` off yields a gutterless page whose text starts at column
    /// zero, matching a live render with the gutter disabled. `current_line`
    /// selects relative numbering, and `None` paints absolute.
    pub(crate) fn new(
        line_numbers: bool,
        severity: Arc<BTreeMap<u32, DiagnosticSeverity>>,
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
        }
    }

    /// The theme resolved on the run loop, reused by a diff-view page to style
    /// its two columns off the loop.
    pub(crate) fn theme(&self) -> &crate::theme::Theme {
        &self.theme
    }
}

/// Paint the page's line-number gutter, returning the cell columns it reserves
/// and the rich APC frames to append after the page cells.
///
/// Returns `(0, empty)` when line numbers are off, so the page text then starts
/// at column zero exactly as before. In rich mode the sub-cell components draw
/// into a scratch scene whose bytes ride the fill, leaving the gutter cells in
/// `buf` at the page background. In fallback mode the degraded numbers paint
/// into `buf` and no bytes are returned.
fn paint_page_gutter(
    snapshot: &DisplaySnapshot,
    top_row: u32,
    end_row: u32,
    buf: &mut Buffer,
    area: Rect,
    gutter: &PageGutter,
) -> (u16, Vec<u8>) {
    if !gutter.line_numbers {
        return (0, Vec::new());
    }

    let visible = end_row.saturating_sub(top_row).min(area.height as u32);
    let (folded, width_digits) = gutter_geometry(snapshot, top_row, visible);

    let diff_marks = gutter_diff_marks(snapshot, &folded);

    match &gutter.rich {
        Some(rich) => {
            let lines = gutter_component_lines(
                &folded,
                &gutter.severity,
                &diff_marks,
                &rich.diff,
                &rich.colors,
                gutter.current_line,
            );
            let widget = rich_gutter(
                &lines,
                width_digits,
                rich.number_fg,
                rich.separator,
                rich.bg,
            );
            let mut scene = ApcScene::new();
            let mut scratch = Buffer::empty(area);
            widget.draw_components(area, &mut scratch, &mut scene);
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

/// Render review page `index` from owned parts and wrap it in the pool fill
/// frames, so the returned bytes are a self-contained fill the terminal applies
/// to slot `index`.
///
/// The review analogue of [`render_page_fill`]: it runs on a blocking worker
/// from a cloned [`ReviewViewState`] plus an owned [`DisplaySnapshot`] and
/// [`Theme`](crate::theme::Theme), all `Send`, so a pooled review page renders
/// off the run loop and matches the live diff at that scroll position.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_review_page_from_parts(
    snapshot: &DisplaySnapshot,
    view: &ReviewViewState,
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
    let mut buf = Buffer::empty(area);
    render_review_rows(
        snapshot,
        view,
        scroll_row,
        area,
        fallback_style,
        theme,
        &mut buf,
        None,
    );
    let bytes = serialize_buffer(&buf);

    let mut frame = Vec::with_capacity(bytes.len() + 16);
    encode_fill_into(&mut frame, pool, index);
    frame.extend_from_slice(&bytes);
    encode_fill_end_into(&mut frame);
    frame
}

/// Render conflict-view page `index` from owned parts and wrap it in the pool
/// fill frames, so the returned bytes are a self-contained fill the terminal
/// applies to slot `index`.
///
/// The conflict analogue of [`render_review_page_from_parts`]. It calls the same
/// [`render_conflict_rows`] the live grid calls, so a wheel glide keeps all
/// three columns rather than dropping to the bare center buffer the plain-editor
/// page would paint.
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
    let mut buf = Buffer::empty(area);
    render_conflict_rows(
        snapshot,
        state,
        scroll_row,
        area,
        fallback_style,
        theme,
        &mut buf,
    );
    let bytes = serialize_buffer(&buf);

    let mut frame = Vec::with_capacity(bytes.len() + 16);
    encode_fill_into(&mut frame, pool, index);
    frame.extend_from_slice(&bytes);
    encode_fill_end_into(&mut frame);
    frame
}

/// Render `region_height` rows of the file finder list starting at row
/// `page * region_height` into a fresh region-sized [`Buffer`], returning the
/// page's self-contained VT byte stream.
///
/// Mirrors [`render_editor_page`] but paints finder result rows, so a pooled
/// page matches the live list at that scroll position. The finder is read-only
/// here -- the page index alone selects the rows.
pub(crate) fn render_finder_page(
    finder: &FileFinder,
    page: u64,
    theme: &crate::theme::Theme,
    region_width: u16,
    region_height: u16,
) -> Vec<u8> {
    let area = Rect::new(0, 0, region_width, region_height);
    let mut buf = Buffer::empty(area);

    let start_row = page
        .saturating_mul(region_height as u64)
        .min(usize::MAX as u64) as usize;
    paint_finder_rows(finder, area, start_row, theme, &mut buf);

    serialize_buffer(&buf)
}

/// Render `region_height` rows of the command-palette result list starting at
/// row `page * region_height` into a fresh region-sized [`Buffer`], returning
/// the page's self-contained VT byte stream.
///
/// Mirrors [`render_finder_page`] but paints palette result rows; the page
/// index alone selects the rows, and the list is read-only here.
pub(crate) fn render_palette_page(
    filtered: &[&'static RegistryEntry],
    match_indices: &[Vec<u32>],
    selected: usize,
    page: u64,
    theme: &crate::theme::Theme,
    region_width: u16,
    region_height: u16,
) -> Vec<u8> {
    let area = Rect::new(0, 0, region_width, region_height);
    let mut buf = Buffer::empty(area);

    let start_row = page
        .saturating_mul(region_height as u64)
        .min(usize::MAX as u64) as usize;
    paint_palette_rows(
        filtered,
        match_indices,
        selected,
        area,
        start_row,
        theme,
        &mut buf,
    );

    serialize_buffer(&buf)
}

/// Render `region_height` rows of the palette's inline argument-picker list
/// starting at row `page * region_height` into a fresh region-sized [`Buffer`],
/// returning the page's self-contained VT byte stream.
///
/// Mirrors [`render_finder_page`] but paints the picker's path rows through the
/// shared [`crate::render::picker::paint_path_rows`], so a pooled page matches
/// the live inline picker. The picker is read-only here.
pub(crate) fn render_arg_page(
    picker: &ArgPicker,
    page: u64,
    theme: &crate::theme::Theme,
    region_width: u16,
    region_height: u16,
) -> Vec<u8> {
    let area = Rect::new(0, 0, region_width, region_height);
    let mut buf = Buffer::empty(area);

    let start_row = page
        .saturating_mul(region_height as u64)
        .min(usize::MAX as u64) as usize;
    let core = picker.active_core_ref();
    let prefix = picker
        .browse
        .as_ref()
        .map(|browse| browse.typed_dir.as_str())
        .unwrap_or_default();
    crate::render::picker::paint_path_rows(
        &core.picklist,
        &core.git_root,
        prefix,
        area,
        start_row,
        theme,
        &mut buf,
    );

    serialize_buffer(&buf)
}

/// Render `region_height` rows of the completion popup list starting at row
/// `page * region_height` into a fresh region-sized [`Buffer`], returning the
/// page's self-contained VT byte stream.
///
/// Mirrors [`render_finder_page`] but paints completion rows; the page index
/// alone selects the rows, and the list is read-only here.
pub(crate) fn render_completion_page(
    items: &[CompletionItem],
    selected_idx: usize,
    prefix: &str,
    page: u64,
    theme: &crate::theme::Theme,
    region_width: u16,
    region_height: u16,
) -> Vec<u8> {
    let area = Rect::new(0, 0, region_width, region_height);
    let mut buf = Buffer::empty(area);

    let start_row = page
        .saturating_mul(region_height as u64)
        .min(usize::MAX as u64) as usize;
    paint_completion_rows(
        items,
        selected_idx,
        prefix,
        start_row,
        area,
        theme,
        &mut buf,
    );

    serialize_buffer(&buf)
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
    let area = Rect::new(0, 0, region_width, region_height);
    let mut buf = Buffer::empty(area);

    let start_row = page
        .saturating_mul(region_height as u64)
        .min(usize::MAX as u64) as usize;
    paint_help_list_rows(help, area, start_row, theme, &mut buf);

    serialize_buffer(&buf)
}

/// Render `region_height` lines of the selected help entry's detail starting at
/// line `page * region_height` into a fresh region-sized [`Buffer`], returning
/// the page's self-contained VT byte stream.
///
/// Mirrors [`render_help_list_page`] but paints the detail body; the page index
/// alone selects the lines, and the detail is read-only here.
pub(crate) fn render_help_detail_page(
    help: &Help,
    page: u64,
    theme: &crate::theme::Theme,
    region_width: u16,
    region_height: u16,
) -> Vec<u8> {
    let area = Rect::new(0, 0, region_width, region_height);
    let mut buf = Buffer::empty(area);

    let start_row = page
        .saturating_mul(region_height as u64)
        .min(usize::MAX as u64) as usize;
    paint_help_detail_rows(help, area, start_row, theme, &mut buf);

    serialize_buffer(&buf)
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
    let area = Rect::new(0, 0, region_width, region_height);
    let mut buf = Buffer::empty(area);

    let start_row = page
        .saturating_mul(region_height as u64)
        .min(usize::MAX as u64) as usize;
    paint_commit_rows(state, area, start_row, theme, &mut buf);

    serialize_buffer(&buf)
}

/// Serialize every cell of `buf` to a self-contained VT byte stream via a
/// [`CrosstermBackend`] over an in-memory buffer.
///
/// Unlike the live render path, which diffs against the previous frame, this
/// emits all cells unconditionally so the bytes fully paint a pool slot
/// regardless of what that slot held before. Cursor moves are absolute, so the
/// stream is positioned for the page's top-left independent of the live grid
/// cursor.
pub(crate) fn serialize_buffer(buf: &Buffer) -> Vec<u8> {
    use ratatui::backend::{Backend, CrosstermBackend};

    let mut bytes = Vec::new();
    {
        let mut backend = CrosstermBackend::new(&mut bytes);
        let cells = buf.content.iter().enumerate().map(|(i, cell)| {
            let (x, y) = buf.pos_of(i);
            (x, y, cell)
        });
        // CrosstermBackend over a Vec<u8> writer is infallible; the Results are
        // surfaced only because the Backend trait is generic over fallible writers.
        let _ = backend.draw(cells);
        let _ = backend.flush();
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::{emit_into, scroll_target, window_range, SmoothScrollState, WINDOW_PAGES};
    use std::sync::Arc;
    use stoatty_protocol::command::{
        decode, Command, PoolDropCommand, PoolRegionCommand, RepositionCommand, ScrollCommand,
    };

    /// `render_page_from_snapshot` must paint the same bytes as the existing pool
    /// path, an unfocused `render_editor` over the same rows. Covers the first
    /// page, a mid page, the partial last page, and a page past the document end,
    /// exercising the page offset and the bottom/right clipping.
    #[test]
    fn page_from_snapshot_matches_unfocused_render_editor() {
        use super::{render_page_from_snapshot, serialize_buffer, Buffer, PageGutter, Rect};
        use crate::{
            action_handlers::{self, dispatch},
            render::editor::render_editor_with_overlay,
            theme::{scope, Theme},
            LineNumbers, Stoat,
        };
        use std::{collections::BTreeMap, path::PathBuf};
        use stoat_action::OpenFile;
        use stoat_config::WrapMode;

        let mut h = Stoat::test();
        let root = PathBuf::from("/page-snapshot");
        let path = root.join("doc.txt");
        h.fake_fs().insert_file(
            &path,
            b"line zero\nline one\nline two\nline three\nline four\nline five\nline six\nline seven\nline eight\nline nine\n",
        );
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let theme = Theme::empty();
        let fallback = theme.get(scope::UI_TEXT);
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");

        // With line numbers on in fallback mode, the page's degraded cell gutter
        // must match the live render's so the settle handoff shows no shift.
        let gutter = PageGutter::new(
            true,
            Arc::new(BTreeMap::new()),
            Arc::new(theme.clone()),
            None,
            None,
        );

        for top_row in [0u32, 4, 8, 40] {
            let area = Rect::new(0, 0, 12, 4);
            let mut expected = Buffer::empty(area);
            let saved = editor.scroll_row;
            editor.scroll_row = top_row;
            render_editor_with_overlay(
                editor,
                area,
                fallback,
                &theme,
                &mut expected,
                false,
                false,
                LineNumbers::Absolute,
                false,
                None,
                None,
                None,
                None,
                None,
                None,
                0.0,
                WrapMode::None,
                80,
            );
            editor.scroll_row = saved;
            let expected = serialize_buffer(&expected);

            let snapshot = editor.display_map.snapshot();
            let got =
                render_page_from_snapshot(&snapshot, top_row, fallback, 12, 4, &gutter, false, 0.0);

            assert_eq!(got, expected, "page at top_row {top_row}");
        }
    }

    /// A pooled page for an unfocused pane must dim exactly as the live grid does:
    /// threading `dim` changes the page bytes, and the change is precisely the
    /// shared cell blend, so a glide over an inactive pane shows no undimmed flash.
    #[test]
    fn a_dimmed_page_is_the_undimmed_page_with_the_cell_blend() {
        use super::{dim_page, render_page_from_snapshot, Buffer, PageGutter, Rect};
        use crate::{
            action_handlers::{self, dispatch},
            render::{pane::dim_pane_content, review::style_rgb},
            theme::scope,
            Stoat,
        };
        use ratatui::style::{Color, Style};
        use std::{collections::BTreeMap, path::PathBuf};
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
        let gutter = PageGutter::new(true, Arc::new(BTreeMap::new()), theme.clone(), None, None);
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();

        let undimmed =
            render_page_from_snapshot(&snapshot, 0, fallback, 12, 4, &gutter, false, 0.0);
        let dimmed = render_page_from_snapshot(&snapshot, 0, fallback, 12, 4, &gutter, false, 0.5);
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

    /// A diff-view page must paint the same two columns as the live grid, so
    /// stoatty diff scrolling rides the pool path without a settle-handoff shift.
    #[test]
    fn diff_view_page_paints_the_two_column_body() {
        use super::{
            paint_diff_rows, render_page_from_snapshot, serialize_buffer, Buffer, PageGutter, Rect,
        };
        use crate::{
            buffer::{BufferId, TextBuffer},
            diff_map::DiffMap,
            display_map::DisplayMap,
            multi_buffer::MultiBuffer,
            theme::{scope, Theme},
        };
        use std::{collections::BTreeMap, sync::RwLock};
        use stoat_language::structural_diff;
        use stoat_scheduler::{Executor, TestScheduler};

        let base = "keep\nold\ntail\n";
        let text = "keep\nnew\ntail\n";
        let mut tb = TextBuffer::with_text(BufferId::new(0), text);
        tb.diff_map = Some(DiffMap::from_structural_changes(
            structural_diff::diff(base, text),
            base,
            text,
        ));
        let shared = Arc::new(RwLock::new(tb));
        let multi = MultiBuffer::singleton(BufferId::new(0), shared);
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let mut display_map = DisplayMap::new(multi, executor);
        display_map.set_show_deleted_blocks(true);
        let snapshot = display_map.snapshot();

        let theme = Theme::empty();
        let fallback = theme.get(scope::UI_TEXT);
        let gutter = PageGutter::new(
            true,
            Arc::new(BTreeMap::new()),
            Arc::new(theme.clone()),
            None,
            None,
        );
        let area = Rect::new(0, 0, 40, 8);

        let mut expected = Buffer::empty(area);
        paint_diff_rows(
            &snapshot,
            0,
            area,
            fallback,
            &theme,
            &mut expected,
            None,
            0.0,
        );
        let expected = serialize_buffer(&expected);

        let got = render_page_from_snapshot(&snapshot, 0, fallback, 40, 8, &gutter, true, 0.0);
        assert_eq!(
            got, expected,
            "the diff-view page paints the two-column body, not the single-column path"
        );
    }

    /// A conflict-view page must carry the same three columns as the live grid,
    /// so a wheel glide over the pool does not drop to the bare center buffer
    /// and back at settle.
    #[test]
    fn conflict_view_page_paints_the_three_column_body() {
        use super::{
            render_conflict_page_from_parts, render_conflict_rows, serialize_buffer, Buffer, Rect,
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
        let (snapshot, state) = {
            let editor =
                crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("center editor");
            (
                editor.display_map.snapshot(),
                editor.conflict_view.clone().expect("conflict view state"),
            )
        };

        let area = Rect::new(0, 0, 150, 8);
        let mut expected = Buffer::empty(area);
        render_conflict_rows(&snapshot, &state, 0, area, fallback, &theme, &mut expected);
        let expected = serialize_buffer(&expected);

        let got =
            render_conflict_page_from_parts(&snapshot, &state, &theme, 7, 0, fallback, 150, 8);
        assert!(
            got.windows(expected.len()).any(|w| w == expected),
            "the conflict page carries the live three-column body inside its fill frames",
        );
    }

    /// A pooled page baked with a cursor line paints relative numbers in its
    /// gutter, matching the live render so the settle handoff shows no shift.
    #[test]
    fn pooled_page_paints_relative_numbers() {
        use super::{paint_page_gutter, Buffer, PageGutter, Rect};
        use crate::{
            action_handlers::{self, dispatch},
            theme::Theme,
            Stoat,
        };
        use std::{collections::BTreeMap, path::PathBuf};
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
            Arc::new(BTreeMap::new()),
            Arc::new(theme),
            None,
            Some(3),
        );
        let area = Rect::new(0, 0, 12, 5);
        let mut buf = Buffer::empty(area);
        let (width, _) = paint_page_gutter(&snapshot, 0, 5, &mut buf, area, &gutter);

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

    fn region(pool: u32, height: u16) -> PoolRegionCommand {
        PoolRegionCommand {
            pool,
            top: 1,
            left: 2,
            width: 76,
            height,
            window: 0,
        }
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

    #[test]
    fn scroll_target_splits_row_into_page_and_fraction() {
        assert_eq!(
            scroll_target(7, 0.0, 20),
            ScrollCommand {
                pool: 7,
                page: 0,
                fraction: 0
            }
        );
        assert_eq!(
            scroll_target(7, 20.0, 20),
            ScrollCommand {
                pool: 7,
                page: 1,
                fraction: 0
            }
        );
        assert_eq!(
            scroll_target(7, 30.0, 20),
            ScrollCommand {
                pool: 7,
                page: 1,
                fraction: 32768
            }
        );

        let fraction = |offset: f32| scroll_target(7, offset, 20).fraction;
        assert!(
            fraction(12.0) < fraction(12.5) && fraction(12.5) < fraction(13.0),
            "a sub-row offset lands strictly between the whole-row fractions"
        );
    }

    #[test]
    fn window_centers_and_clamps_at_start() {
        assert_eq!(window_range(0), 0..WINDOW_PAGES);
        assert_eq!(window_range(1), 0..WINDOW_PAGES);
        assert_eq!(window_range(10), 8..8 + WINDOW_PAGES);
    }

    #[test]
    fn first_emit_declares_region_fills_window_and_scrolls() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();
        let mut filled = Vec::new();
        emit_into(&mut out, &mut state, region(1, 20), 0.0, 0, false, |page| {
            filled.push(page);
            Vec::new()
        });

        assert_eq!(filled, (0..WINDOW_PAGES).collect::<Vec<_>>());
        let cmds = commands(&out);
        assert_eq!(cmds.first(), Some(&Command::PoolRegion(region(1, 20))));
        assert_eq!(
            cmds.last(),
            Some(&Command::Scroll(ScrollCommand {
                pool: 1,
                page: 0,
                fraction: 0
            }))
        );
    }

    #[test]
    fn emit_into_returns_newly_entered_pages() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();

        // page 2 (offset 40 / height 20) buffers window 0..5.
        let first = emit_into(&mut out, &mut state, region(1, 20), 40.0, 0, false, |_| {
            Vec::new()
        });
        assert_eq!(first, (0..WINDOW_PAGES).collect::<Vec<_>>());

        // A sub-page scroll within page 2 enters no new page.
        let same = emit_into(&mut out, &mut state, region(1, 20), 41.0, 0, false, |_| {
            Vec::new()
        });
        assert!(same.is_empty(), "sub-page scroll entered {same:?}");

        // Stepping to page 3 shifts the window to 1..6, entering only page 5.
        let stepped = emit_into(&mut out, &mut state, region(1, 20), 60.0, 0, false, |_| {
            Vec::new()
        });
        assert_eq!(stepped, vec![5]);
    }

    #[test]
    fn hold_defers_a_resting_content_change_until_the_target_moves() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();

        emit_into(&mut out, &mut state, region(1, 20), 40.0, 0, true, |_| {
            Vec::new()
        });

        // A content-version bump while the target rests enters no page and must
        // not render, so the buffered window is left untouched.
        let held = emit_into(&mut out, &mut state, region(1, 20), 40.0, 1, true, |_| {
            panic!("a resting hold must not refill")
        });
        assert!(held.is_empty(), "resting hold entered {held:?}");

        // A sub-page move applies the deferred bump, wiping the stale window and
        // refilling it even though the visible page did not change.
        let moved = emit_into(&mut out, &mut state, region(1, 20), 41.0, 1, true, |_| {
            Vec::new()
        });
        assert_eq!(moved, (0..WINDOW_PAGES).collect::<Vec<_>>());
    }

    #[test]
    fn moving_the_target_after_a_rest_reenters_the_deferred_window() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();

        emit_into(&mut out, &mut state, region(1, 20), 40.0, 0, true, |_| {
            Vec::new()
        });
        // Resting narrows the requested range to the visible page 2.
        emit_into(&mut out, &mut state, region(1, 20), 40.0, 0, true, |_| {
            Vec::new()
        });

        // Stepping to page 3 moves the target, refilling window 1..6 minus the
        // page 2 that resting kept requested.
        let moved = emit_into(&mut out, &mut state, region(1, 20), 60.0, 0, true, |_| {
            Vec::new()
        });
        assert_eq!(moved, vec![1, 3, 4, 5]);
    }

    #[test]
    fn hold_still_prefills_the_full_window_on_first_display() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();

        // A fresh entry has last_scroll_offset None, so its first display counts
        // as scrolling and hold does not suppress the prefill.
        let entered = emit_into(&mut out, &mut state, region(1, 20), 0.0, 0, true, |_| {
            Vec::new()
        });
        assert_eq!(entered, (0..WINDOW_PAGES).collect::<Vec<_>>());
    }

    #[test]
    fn a_region_change_while_holding_enters_one_page() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();

        emit_into(&mut out, &mut state, region(1, 20), 40.0, 0, true, |_| {
            Vec::new()
        });

        // The region changed under the resting pool, wiping its slots. Holding
        // refills only the visible page (offset 40 / height 22 = page 1).
        let entered = emit_into(&mut out, &mut state, region(1, 22), 40.0, 0, true, |_| {
            Vec::new()
        });
        assert_eq!(entered, vec![1]);
    }

    #[test]
    fn without_hold_a_resting_content_change_still_refills() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();

        emit_into(&mut out, &mut state, region(1, 20), 40.0, 0, false, |_| {
            Vec::new()
        });

        // Overlays pass hold_when_idle false, so a content change refills the full
        // window even while the target is stationary.
        let entered = emit_into(&mut out, &mut state, region(1, 20), 40.0, 1, false, |_| {
            Vec::new()
        });
        assert_eq!(entered, (0..WINDOW_PAGES).collect::<Vec<_>>());
    }

    #[test]
    fn empty_render_requests_pages_without_emitting_fills() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();
        let entered = emit_into(&mut out, &mut state, region(1, 20), 0.0, 0, false, |_| {
            Vec::new()
        });

        assert_eq!(entered, (0..WINDOW_PAGES).collect::<Vec<_>>());
        let cmds = commands(&out);
        assert!(
            !cmds.iter().any(|c| matches!(c, Command::Fill(_))),
            "an empty render emits no fill frame, got {cmds:?}"
        );
        assert!(
            cmds.iter().any(|c| matches!(c, Command::PoolRegion(_))),
            "the region is still declared, got {cmds:?}"
        );
    }

    #[test]
    fn render_page_fill_wraps_the_page_in_fill_frames() {
        use super::{render_page_fill, render_page_from_snapshot, PageGutter};
        use crate::{
            action_handlers::{self, dispatch},
            theme::{scope, Theme},
            Stoat,
        };
        use std::{collections::BTreeMap, path::PathBuf};
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
            Arc::new(BTreeMap::new()),
            Arc::new(Theme::empty()),
            None,
            None,
        );

        let frame = render_page_fill(&snapshot, 7, 2, fallback, 12, 3, &gutter, false, 0.0);

        let cmds = commands(&frame);
        assert!(
            cmds.contains(&Command::Fill(FillCommand { pool: 7, index: 2 })),
            "frame opens with the slot's fill, got {cmds:?}"
        );
        assert!(
            cmds.contains(&Command::FillEnd),
            "frame closes the fill, got {cmds:?}"
        );

        let page =
            render_page_from_snapshot(&snapshot, 2 * 3, fallback, 12, 3, &gutter, false, 0.0);
        assert!(
            find(&frame, &page).is_some(),
            "the page bytes ride between the fill markers"
        );
    }

    #[test]
    fn rich_page_fill_carries_gutter_component_frames() {
        use super::{render_page_fill, PageGutter};
        use crate::{
            action_handlers::{self, dispatch},
            render::editor::resolve_rich_gutter,
            theme::scope,
            Stoat,
        };
        use std::{collections::BTreeMap, path::PathBuf};
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
            Arc::new(BTreeMap::new()),
            theme.clone(),
            Some(rich),
            None,
        );

        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();

        let frame = render_page_fill(&snapshot, 3, 0, fallback, 12, 4, &gutter, false, 0.0);
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

    #[test]
    fn review_page_fill_wraps_and_matches_the_live_render() {
        use super::{render_review_page_from_parts, serialize_buffer};
        use crate::{
            render::review::{render_review, render_review_rows},
            theme::{scope, Theme},
            Stoat,
        };
        use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};
        use stoatty_protocol::command::FillCommand;

        let mut h = Stoat::test();
        h.open_review_from_texts(&[("a.rs", "fn a() { 1 }\n", "fn a() { 2 }\n")]);

        let theme = Theme::empty();
        let fallback = theme.get(scope::UI_TEXT);
        let editor_id = h.with_review(|s| s.view_editor).expect("review editor");
        let (width, height) = (40u16, 6u16);

        let (snapshot, view) = {
            let editor = h
                .stoat
                .active_workspace_mut()
                .editors
                .get_mut(editor_id)
                .expect("editor");
            (
                editor.display_map.snapshot(),
                editor.review_view.clone().expect("review view"),
            )
        };

        let frame =
            render_review_page_from_parts(&snapshot, &view, &theme, 3, 0, fallback, width, height);

        let cmds = commands(&frame);
        assert!(
            cmds.contains(&Command::Fill(FillCommand { pool: 3, index: 0 })),
            "frame opens with the slot's fill, got {cmds:?}"
        );
        assert!(
            cmds.contains(&Command::FillEnd),
            "frame closes the fill, got {cmds:?}"
        );

        // The async page bytes match what the live path paints for the same
        // page, so moving the render off-thread changed nothing on screen. A
        // pooled page carries document rows only, while the live path overlays
        // the caret on top of them, so the rows are what the two share.
        let area = Rect::new(0, 0, width, height);
        let mut rows_only = Buffer::empty(area);
        render_review_rows(
            &snapshot,
            &view,
            0,
            area,
            fallback,
            &theme,
            &mut rows_only,
            None,
        );
        assert!(
            find(&frame, &serialize_buffer(&rows_only)).is_some(),
            "the page's row bytes ride between the fill markers"
        );

        let mut live = Buffer::empty(area);
        {
            let editor = h
                .stoat
                .active_workspace_mut()
                .editors
                .get_mut(editor_id)
                .expect("editor");
            editor.scroll_row = 0;
            render_review(editor, area, fallback, &theme, &mut live, None);
        }

        let overlaid: Vec<_> = live
            .content
            .iter()
            .zip(rows_only.content.iter())
            .filter(|(painted, rows)| painted != rows)
            .collect();
        assert_eq!(
            overlaid.len(),
            1,
            "the live render adds the caret and nothing else to the pooled rows"
        );
        assert!(
            overlaid[0]
                .0
                .style()
                .add_modifier
                .contains(Modifier::REVERSED),
            "a theme styling no cursor scope still paints a visible caret"
        );
    }

    #[test]
    fn unchanged_scroll_emits_nothing_after_first() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();
        emit_into(&mut out, &mut state, region(1, 20), 5.0, 0, false, |_| {
            Vec::new()
        });

        out.clear();
        emit_into(&mut out, &mut state, region(1, 20), 5.0, 0, false, |_| {
            panic!("no page should be re-filled")
        });
        assert!(out.is_empty(), "stable frame emitted {} bytes", out.len());
    }

    #[test]
    fn sub_page_scroll_reuses_window_and_emits_only_scroll() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();
        emit_into(&mut out, &mut state, region(1, 20), 0.0, 0, false, |_| {
            Vec::new()
        });

        out.clear();
        let mut refilled = Vec::new();
        emit_into(&mut out, &mut state, region(1, 20), 3.0, 0, false, |page| {
            refilled.push(page);
            Vec::new()
        });
        assert!(
            refilled.is_empty(),
            "refilled within-window pages {refilled:?}"
        );
        assert_eq!(
            commands(&out),
            vec![Command::Scroll(ScrollCommand {
                pool: 1,
                page: 0,
                fraction: 9830
            })]
        );
    }

    #[test]
    fn far_jump_emits_reposition_then_precise_scroll() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();
        emit_into(&mut out, &mut state, region(1, 20), 0.0, 0, false, |_| {
            Vec::new()
        });

        out.clear();
        emit_into(&mut out, &mut state, region(1, 20), 170.0, 0, false, |_| {
            Vec::new()
        });

        let nav: Vec<Command> = commands(&out)
            .into_iter()
            .filter(|c| matches!(c, Command::Reposition(_) | Command::Scroll(_)))
            .collect();
        assert_eq!(
            nav,
            vec![
                Command::Reposition(RepositionCommand { pool: 1, page: 8 }),
                Command::Scroll(ScrollCommand {
                    pool: 1,
                    page: 8,
                    fraction: 32768,
                }),
            ],
            "a far jump re-anchors with a reposition, then targets the exact row"
        );
    }

    #[test]
    fn content_version_bump_forces_refill() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();
        emit_into(&mut out, &mut state, region(1, 20), 0.0, 1, false, |_| {
            Vec::new()
        });

        out.clear();
        emit_into(&mut out, &mut state, region(1, 20), 0.0, 1, false, |_| {
            panic!("unchanged content must not refill")
        });
        assert!(out.is_empty(), "stable frame emitted {} bytes", out.len());

        out.clear();
        let mut refilled = Vec::new();
        emit_into(&mut out, &mut state, region(1, 20), 0.0, 2, false, |page| {
            refilled.push(page);
            Vec::new()
        });
        assert_eq!(
            refilled,
            (0..WINDOW_PAGES).collect::<Vec<_>>(),
            "a content bump refills the whole window at the same scroll position"
        );
        assert!(
            commands(&out).contains(&Command::Scroll(ScrollCommand {
                pool: 1,
                page: 0,
                fraction: 0
            })),
            "a content bump re-emits the scroll target"
        );
    }

    #[test]
    fn region_change_forces_refill() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();
        emit_into(&mut out, &mut state, region(1, 20), 0.0, 0, false, |_| {
            Vec::new()
        });

        out.clear();
        let mut refilled = Vec::new();
        emit_into(&mut out, &mut state, region(1, 22), 0.0, 0, false, |page| {
            refilled.push(page);
            Vec::new()
        });
        assert_eq!(refilled, (0..WINDOW_PAGES).collect::<Vec<_>>());
        assert_eq!(
            commands(&out).first(),
            Some(&Command::PoolRegion(region(1, 22)))
        );
    }

    #[test]
    fn pools_scroll_independently() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();
        emit_into(&mut out, &mut state, region(1, 20), 0.0, 0, false, |_| {
            Vec::new()
        });
        emit_into(&mut out, &mut state, region(2, 20), 40.0, 0, false, |_| {
            Vec::new()
        });

        let cmds = commands(&out);
        assert!(cmds.contains(&Command::PoolRegion(region(1, 20))));
        assert!(cmds.contains(&Command::PoolRegion(region(2, 20))));
        assert!(cmds.contains(&Command::Scroll(ScrollCommand {
            pool: 2,
            page: 2,
            fraction: 0
        })));
    }

    #[test]
    fn drop_absent_retires_vanished_pools() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();
        emit_into(&mut out, &mut state, region(1, 20), 0.0, 0, false, |_| {
            Vec::new()
        });
        emit_into(&mut out, &mut state, region(2, 20), 0.0, 0, false, |_| {
            Vec::new()
        });

        out.clear();
        state.drop_absent(&mut out, &[1]);
        assert_eq!(
            commands(&out),
            vec![Command::PoolDrop(PoolDropCommand { pool: 2 })]
        );

        // Pool 2 is forgotten, so re-emitting it re-declares its region.
        out.clear();
        emit_into(&mut out, &mut state, region(2, 20), 0.0, 0, false, |_| {
            Vec::new()
        });
        assert!(commands(&out).contains(&Command::PoolRegion(region(2, 20))));
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
}

//! Frame-rate independent easing for the smooth-scroll and cursor animations.
//!
//! Every step here is a pure map from a current value, a target, and the frame
//! time elapsed to the next value. Nothing reads a window, a GPU context, or
//! the event loop, which is what lets the whole set be exercised against a
//! clock rather than a running terminal.

use crate::config::CursorAnimation;
use std::time::Duration;
use stoatty_term::{
    grid::{Grid, Overlay, PoolRegion},
    term::{Cursor, CursorShape, PoolView, Terminal},
};

/// The reference frame duration the easing factors are expressed against. A
/// factor closes that fraction of the remaining distance per baseline frame,
/// and [`ease_alpha`] rescales it to the frame time actually elapsed, so the
/// motion traces the same curve at any refresh rate.
pub(crate) const EASE_BASELINE_FRAME: Duration = Duration::from_micros(16_667);

/// Cap on the per-frame easing step. The first frame after an idle gap sees an
/// elapsed time spanning the whole gap, which would otherwise snap every ease
/// to its target in one step.
pub(crate) const MAX_EASE_DT: Duration = Duration::from_millis(40);

/// Rescale a per-baseline-frame easing factor to the elapsed frame time `dt`.
///
/// Compounds the per-frame decay continuously, so two half-length frames
/// advance an ease exactly as far as one baseline frame. At `dt` equal to
/// [`EASE_BASELINE_FRAME`] this returns `factor` unchanged.
pub(crate) fn ease_alpha(factor: f32, dt: Duration) -> f32 {
    let frames = dt.as_secs_f32() / EASE_BASELINE_FRAME.as_secs_f32();
    1.0 - (1.0 - factor).powf(frames)
}

/// Scale a per-baseline-frame minimum step to the elapsed frame time `dt`, so
/// an ease's floor speed is a velocity rather than a per-frame distance.
pub(crate) fn min_step(step: f32, dt: Duration) -> f32 {
    step * dt.as_secs_f32() / EASE_BASELINE_FRAME.as_secs_f32()
}

/// Step the animated cursor toward `target` by the elapsed frame time `dt`,
/// returning the new position and whether it has reached the target.
///
/// Closes a fixed fraction of the remaining distance per baseline frame,
/// rescaled to `dt`, the exponential ease-out that reads as smooth cursor
/// motion. Within a small epsilon it snaps onto the target so the animation
/// terminates.
pub(crate) fn ease(current: [f32; 2], target: [f32; 2], dt: Duration) -> ([f32; 2], bool) {
    const FACTOR: f32 = 0.35;
    const EPSILON: f32 = 0.01;

    let dx = target[0] - current[0];
    let dy = target[1] - current[1];
    if dx.abs() < EPSILON && dy.abs() < EPSILON {
        return (target, true);
    }

    let alpha = ease_alpha(FACTOR, dt);
    ([current[0] + dx * alpha, current[1] + dy * alpha], false)
}

/// Step the warp cursor's four corners toward the block at `target_cell` by
/// the elapsed frame time `dt`, returning the new corners and whether they
/// have settled.
///
/// Each corner eases toward the corresponding corner of the target cell's
/// block. A corner on the leading side of travel, its offset from the current
/// centroid pointing the same way as the centroid's path to the target, closes
/// a larger fraction of its gap than a trailing one, so the quad stretches along
/// the motion path and collapses back to a square as it arrives. Snaps onto the
/// exact target block and reports settled once every corner is within `EPSILON`.
pub(crate) fn ease_corners(
    current: [[f32; 2]; 4],
    target_cell: [f32; 2],
    dt: Duration,
) -> ([[f32; 2]; 4], bool) {
    const LEADING: f32 = 0.45;
    const TRAILING: f32 = 0.22;
    const EPSILON: f32 = 0.01;

    let target = block_corners(target_cell);

    let settled = (0..4).all(|i| {
        (target[i][0] - current[i][0]).abs() < EPSILON
            && (target[i][1] - current[i][1]).abs() < EPSILON
    });
    if settled {
        return (target, true);
    }

    let cur_centroid = centroid(current);
    let travel = [
        centroid(target)[0] - cur_centroid[0],
        centroid(target)[1] - cur_centroid[1],
    ];

    let mut next = current;
    for i in 0..4 {
        let offset = [
            current[i][0] - cur_centroid[0],
            current[i][1] - cur_centroid[1],
        ];
        let leading = offset[0] * travel[0] + offset[1] * travel[1] > 0.0;
        let alpha = ease_alpha(if leading { LEADING } else { TRAILING }, dt);
        next[i] = [
            current[i][0] + (target[i][0] - current[i][0]) * alpha,
            current[i][1] + (target[i][1] - current[i][1]) * alpha,
        ];
    }
    (next, false)
}

/// One frame's cursor render inputs from [`step_cursor`]. Holds the
/// ligature-break cell, the cursor block's four corners, and whether the
/// animation is still moving, all absent when the cursor is hidden.
type CursorStep = (Option<[f32; 2]>, Option<[[f32; 2]; 4]>, bool);

/// Advance the cursor animation by the elapsed frame time `dt` toward `target`
/// (the cursor's cell origin, or `None` when hidden), returning the cell for
/// the ligature break, the cursor block's four corners, and whether the
/// animation is still moving.
///
/// [`CursorAnimation::Block`] eases the single point `point` and derives a rigid
/// one-cell quad from it. [`CursorAnimation::Warp`] eases the four `corners`
/// independently so the block stretches along its path and collapses back to a
/// square as it arrives, taking the eased centroid as the ligature-break cell.
/// Only the state matching `animation` is advanced.
pub(crate) fn step_cursor(
    animation: CursorAnimation,
    point: &mut [f32; 2],
    corners: &mut [[f32; 2]; 4],
    target: Option<[f32; 2]>,
    dt: Duration,
) -> CursorStep {
    let Some(target) = target else {
        return (None, None, false);
    };
    match animation {
        CursorAnimation::Block => {
            let (next, settled) = ease(*point, target, dt);
            *point = next;
            (Some(next), Some(block_corners(next)), !settled)
        },
        CursorAnimation::Warp => {
            let (next, settled) = ease_corners(*corners, target, dt);
            *corners = next;
            (Some(centroid(next)), Some(next), !settled)
        },
    }
}

/// A popover's content overflow height in rows, or `None` when its content fits
/// its box.
///
/// This is how far the content can scroll. Content draws one line per `scale`
/// rows, so it occupies `lines * scale` rows; the overflow is that height beyond
/// the box, in rows, which the scroll offset is also measured in.
pub(crate) fn popover_overflow(overlay: &Overlay) -> Option<f32> {
    let content_rows = overlay.content.lines().count() * overlay.scale.max(1) as usize;
    let height = overlay.height as usize;
    (content_rows > height).then(|| (content_rows - height) as f32)
}

/// Refill `overflows` with one entry per overlay, reusing the previous answers while
/// the overlay list has not been rebuilt.
///
/// `epoch` is the grid's popovers epoch, which advances exactly when that list is
/// re-applied, so an unchanged epoch means every overlay's content, scale, and height
/// still hold and the stored answers stand. Each answer costs a walk of the whole
/// popover content, and an overflowing popover drives continuous redraws, so without
/// this the walk repeats at frame rate for as long as a tooltip is on screen.
///
/// The reused length matters as much as the values, since the caller sizes its
/// per-overlay scroll state from it.
pub(crate) fn refresh_popover_overflows(
    overlays: &[Overlay],
    epoch: u64,
    last_epoch: &mut Option<u64>,
    overflows: &mut Vec<Option<f32>>,
) {
    if *last_epoch == Some(epoch) {
        return;
    }

    *last_epoch = Some(epoch);
    overflows.clear();
    overflows.extend(overlays.iter().map(popover_overflow));
}

/// Advance the ping-pong popover scroll by the elapsed frame time `dt` toward
/// its current end, reversing direction when it settles.
///
/// `down` eases the offset toward `max` (the overflow bottom); once settled it
/// flips, easing back toward the top, so the content glides up and down while
/// the popover is visible.
pub(crate) fn step_popover_scroll(scroll: f32, down: bool, max: f32, dt: Duration) -> (f32, bool) {
    let target = if down { max } else { 0.0 };
    let (next, settled) = ease([scroll, 0.0], [target, 0.0], dt);
    let down = if settled { !down } else { down };
    (next[0], down)
}

/// Advance the grid's eased vertical scroll by the elapsed frame time `dt`.
///
/// The new `delta` (rows the content scrolled up) is added to the offset, so the
/// content starts that many rows lower, then the offset eases toward zero so it
/// glides up into place. Returns the new offset and whether it is still easing.
pub(crate) fn step_grid_scroll(scroll: f32, delta: usize, dt: Duration) -> (f32, bool) {
    let seeded = scroll + delta as f32;
    let (next, settled) = ease([seeded, 0.0], [0.0, 0.0], dt);
    (next[0], !settled)
}

/// Floor on the scrollback ease's per-baseline-frame step, in rows, so the
/// exponential tail locks in with a quick, even glide instead of crawling the
/// last sub-pixels into the target. A few pixels per frame at a typical cell
/// height; raise for a snappier lock-in, lower for a softer one.
pub(crate) const SCROLLBACK_MIN_STEP: f32 = 0.15;

/// Advance the eased scrollback position toward `target` by the elapsed frame
/// time `dt`.
///
/// `scroll` and `target` are positions in rows back from the live bottom: the
/// wheel advances `target` and this eases `scroll` toward it, so the history
/// window scrolls through each row and settles cell-aligned on the target.
///
/// Closes a fixed fraction of the remaining distance per baseline frame,
/// rescaled to `dt`, but never moves slower than [`SCROLLBACK_MIN_STEP`], so
/// the tail finishes crisply instead of crawling sub-pixel-by-sub-pixel into the
/// target. Returns the new position and whether it is still easing.
pub(crate) fn step_scrollback_scroll(scroll: f32, target: f32, dt: Duration) -> (f32, bool) {
    const FACTOR: f32 = 0.35;
    const EPSILON: f32 = 0.01;

    let remaining = target - scroll;
    if remaining.abs() < EPSILON {
        return (target, false);
    }

    let step = (remaining.abs() * ease_alpha(FACTOR, dt))
        .max(min_step(SCROLLBACK_MIN_STEP, dt))
        .min(remaining.abs());
    (scroll + step.copysign(remaining), true)
}

/// Advance the scroll region's eased vertical offset by the elapsed frame time
/// `dt`.
///
/// `delta` is the change in the region's declared scroll offset since the last
/// frame, signed: positive when the program scrolled the region's content down,
/// negative when up. It seeds the offset, which then eases toward zero so the
/// region's content glides into place. Returns the new offset and whether it is
/// still easing.
pub(crate) fn step_region_scroll(scroll: f32, delta: f32, dt: Duration) -> (f32, bool) {
    let seeded = scroll + delta;
    let (next, settled) = ease([seeded, 0.0], [0.0, 0.0], dt);
    (next[0], !settled)
}

/// Pages before a reposition target the live offset re-anchors to, so a
/// discontinuous jump lands with a one-page soft glide onto the destination
/// rather than appearing instantly. The app buffers from this many pages before
/// the target so the landing glide draws pooled content.
pub(crate) const REPOSITION_LAND_PAGES: f32 = 1.0;

/// Wall time the scroll target must hold steady before the pool hands its
/// region back to the live grid.
///
/// The follower catches a still-moving target every frame through the momentum
/// tail, so a bare convergence test reads as "settled" mid-glide. The live grid
/// stays frozen at its pre-scroll row until the app repaints at the true settle,
/// so handing off then snaps the view back to that stale row. Waiting for the
/// target to hold steady lets the settle repaint arrive first, so the region
/// only returns to a live grid that already matches the pool.
pub(crate) const HANDOFF_STABLE_TIME: Duration = Duration::from_millis(50);

/// Advance the document's eased smooth-scroll offset toward `target` by the
/// elapsed frame time `dt`.
///
/// `scroll` and `target` are app-declared absolute positions in document pages;
/// `page_rows` is the rows per page, so the snap epsilon and step floor are
/// expressed in on-screen rows rather than pages. Mirrors
/// [`step_scrollback_scroll`]: closes a fixed fraction of the remaining distance
/// per baseline frame, rescaled to `dt`, but never less than a row-sized floor,
/// capped at the remaining distance, so the tail lands exactly on the
/// (whole-row) target. A page-unit epsilon would snap a visible fraction of a
/// row when handing back to the live grid, reading as a one-line jump at the
/// end of the glide. Returns the new offset and whether it is still easing.
pub(crate) fn step_document_scroll(
    scroll: f32,
    target: f32,
    page_rows: f32,
    dt: Duration,
) -> (f32, bool) {
    const FACTOR: f32 = 0.7;
    const EPSILON_ROWS: f32 = 0.01;
    const MIN_STEP_ROWS: f32 = 0.15;

    let remaining = target - scroll;
    if (remaining * page_rows).abs() < EPSILON_ROWS {
        return (target, false);
    }

    let step = (remaining.abs() * ease_alpha(FACTOR, dt))
        .max(min_step(MIN_STEP_ROWS, dt) / page_rows)
        .min(remaining.abs());
    (scroll + step.copysign(remaining), true)
}

/// The document-page offset a reposition jump re-anchors to, one page to the
/// side the viewport travelled from.
///
/// `current` is the offset before the jump, which is what reveals the travel
/// direction. Landing on the side the viewport came from is what makes the
/// glide read as continuing the motion. A jump up re-anchors below the target
/// so content glides downward into place, and a jump down re-anchors above it.
/// Seeding the same side every time reverses half of all landings.
///
/// Clamped at zero, since no document exists above the first page.
pub(crate) fn reposition_scroll(current: f32, target: u64) -> f32 {
    let target = target as f32;
    let travelled_down = current < target;

    let landed = if travelled_down {
        target - REPOSITION_LAND_PAGES
    } else {
        target + REPOSITION_LAND_PAGES
    };

    landed.max(0.0)
}

/// The cursor's cell position for the renderer, or `None` when it is hidden.
pub(crate) fn cursor_position(cursor: Cursor) -> Option<[f32; 2]> {
    if cursor.shape == CursorShape::Hidden {
        None
    } else {
        Some([cursor.col as f32, cursor.row as f32])
    }
}

/// Whether the cursor cell falls within `region`.
pub(crate) fn cursor_in_region(cursor: Cursor, region: PoolRegion) -> bool {
    let col = cursor.col;
    let row = cursor.row;
    col >= region.left as usize
        && col < region.left as usize + region.width as usize
        && row >= region.top as usize
        && row < region.top as usize + region.height as usize
}

/// The four block corners [TL, TR, BL, BR] for a one-cell cursor block at
/// fractional cell origin `origin`.
pub(crate) fn block_corners(origin: [f32; 2]) -> [[f32; 2]; 4] {
    let [x, y] = origin;
    [[x, y], [x + 1.0, y], [x, y + 1.0], [x + 1.0, y + 1.0]]
}

/// The primary cursor's placement while it rides a gliding pool.
///
/// Frame-locked to the pool's eased content offset rather than eased toward the
/// VT cursor cell, so the cursor slides with the text under it.
#[derive(Clone, Copy)]
pub(crate) struct AnchoredCursor {
    /// Fractional cell position [col, row] the cursor draws at this frame.
    pub(crate) pos: [f32; 2],
    /// Whether [`Self::pos`] sits within the pool. The cursor hides once its line
    /// has scrolled off either edge.
    pub(crate) in_region: bool,
    /// The gliding pool's region, used to clip the drawn cursor to the pane.
    pub(crate) region: PoolRegion,
}

/// The screen position a cursor anchored to a document row draws at while its
/// pool glides, and whether it still falls within the pool.
///
/// `top` and `page_rows` are the pool region's top row and height, `row` and
/// `col` the cursor's document display row and column, and `scroll` the pool's
/// eased scroll in pages. The cursor rides the eased content, so it leaves the
/// region once its line scrolls past either edge.
pub(crate) fn anchored_cursor_pos(
    top: f32,
    page_rows: f32,
    row: f32,
    col: f32,
    scroll: f32,
) -> ([f32; 2], bool) {
    let y = top + row - scroll * page_rows;
    let in_region = y >= top && y < top + page_rows;
    ([col, y], in_region)
}

/// The vertical pixel shift a surface anchored to a gliding pool draws at.
///
/// `top_rows` is the document top row the anchored surface's layout assumed,
/// and `host_scroll_pages` the host pool's eased scroll in pages, which
/// `host_region_height` converts to rows. The gap between the two, scaled by
/// `cell_h`, is how far the host's content has travelled since that layout, and
/// therefore how far the surface must travel to stay over the same text.
///
/// Positive means the host has not yet eased down to the assumed top, so the
/// surface draws lower. The result is deliberately un-snapped, because the whole
/// point is to ride the ease sub-cell.
pub(crate) fn anchored_shift(
    top_rows: f32,
    host_scroll_pages: f32,
    host_region_height: f32,
    cell_h: f32,
) -> f32 {
    (top_rows - host_scroll_pages * host_region_height) * cell_h
}

/// One pool's resolved tie to a host that moved this frame.
///
/// Captured while the frame's pool list is still in hand, because the composite
/// build that needs it runs after that list has gone back to its scratch slot.
/// The pixel shift is left uncomputed here, since the cell height it scales by
/// is only resolved later.
#[derive(Clone, Copy)]
pub(crate) struct AnchorRide {
    /// The anchored pool this ride belongs to.
    pub(crate) pool: u32,
    /// The document top row the anchored pool's layout assumed.
    pub(crate) top_rows: f32,
    /// The host's eased scroll in pages, as of this frame.
    pub(crate) host_scroll: f32,
    /// The host's region, which both scales the shift and clips the ride.
    pub(crate) host_region: PoolRegion,
}

/// Move scissor rect `[x, y, width, height]` down by `dy_px`, clamping at the
/// top edge of the surface.
///
/// A ride that carries the rect above the surface keeps only the part still on
/// screen, so the height shrinks by whatever the clamp cut.
pub(crate) fn shift_scissor(scissor: [u32; 4], dy_px: f32) -> [u32; 4] {
    let [x, y, w, h] = scissor;
    let shifted = y as f32 + dy_px;
    let top = shifted.max(0.0) as u32;
    let cut = (top as f32 - shifted).max(0.0) as u32;
    [x, top, w, h.saturating_sub(cut)]
}

/// The overlap of two scissor rects, or a zero-height rect when they miss.
///
/// A ride is clipped to its host's region this way, so a surface that slides
/// past the pane edge is cut there rather than drawn over the neighbour.
pub(crate) fn intersect_scissor(a: [u32; 4], b: [u32; 4]) -> [u32; 4] {
    let x0 = a[0].max(b[0]);
    let y0 = a[1].max(b[1]);
    let x1 = (a[0] + a[2]).min(b[0] + b[2]);
    let y1 = (a[1] + a[3]).min(b[1] + b[3]);
    [x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0)]
}

/// The pixel scissor rect [x, y, width, height] covering pool `region`, laid out
/// on a `cw` by `ch` cell grid.
pub(crate) fn region_scissor(region: PoolRegion, cw: f32, ch: f32) -> [u32; 4] {
    let x0 = (region.left as f32 * cw) as u32;
    let y0 = (region.top as f32 * ch) as u32;
    let x1 = ((region.left as f32 + region.width as f32) * cw) as u32;
    let y1 = ((region.top as f32 + region.height as f32) * ch) as u32;
    [x0, y0, x1 - x0, y1 - y0]
}

/// Seed the settle flight on the first non-anchored frame after a glide.
///
/// Leaves `point` at its last anchored position so [`step_cursor`] eases from
/// there to the landing cell rather than teleporting. When that position slid
/// off the viewport during the glide, it is pulled to just beyond the edge it
/// exited (`rows` is the viewport height), so the flight is a consistent length
/// regardless of how far the content scrolled.
pub(crate) fn seed_settle_flight(
    was_anchored: &mut bool,
    point: &mut [f32; 2],
    corners: &mut [[f32; 2]; 4],
    rows: usize,
) {
    if !*was_anchored {
        return;
    }
    *was_anchored = false;
    point[1] = point[1].clamp(-1.0, rows as f32);
    *corners = block_corners(*point);
}

/// The centroid of a quad's four corners.
pub(crate) fn centroid(corners: [[f32; 2]; 4]) -> [f32; 2] {
    [
        (corners[0][0] + corners[1][0] + corners[2][0] + corners[3][0]) / 4.0,
        (corners[0][1] + corners[1][1] + corners[2][1] + corners[3][1]) / 4.0,
    ]
}

/// One pool's smooth-scroll animation state, held by [`State::pool_anims`].
pub(crate) struct PoolAnim {
    /// The live eased offset, in document pages, easing toward the pool's
    /// app-declared target. Tracks an absolute position rather than decaying.
    pub(crate) scroll: f32,
    /// The scroll target seen the previous frame, in document pages. A frame
    /// that changed it scrolled the document, so the change feeds the cursor
    /// sweep. Seeded to the creation target so a fresh pool does not sweep.
    pub(crate) last_scroll_target: f32,
    /// Wall time since [`Self::last_scroll_target`] last changed. The follower
    /// converges to a still-moving target every frame in the momentum tail, so
    /// convergence alone cannot separate an active glide from a settled pool.
    /// Once this reaches [`HANDOFF_STABLE_TIME`] the target has held steady
    /// and the region hands back to the live grid. A target still moving holds
    /// the pool composited. Seeded so a fresh at-rest pool hands off at once.
    pub(crate) target_stable_for: Duration,
    /// The region's pooled rows composed at [`Self::scroll`], sized to the
    /// region plus one straddle row. Reused across frames.
    pub(crate) document_grid: Grid,
    /// The integer document top row [`Self::document_grid`] was last composed
    /// at. With [`Self::last_version`] and [`Self::last_region_dims`] unchanged,
    /// the composed rows are identical this frame and only the sub-cell fraction
    /// moved, so the recompose is skipped. `None` until the first composed frame.
    pub(crate) last_top: Option<i64>,
    /// The pool content-version the grids were last composed at, so a fill that
    /// committed since forces a recompose even when the top row held steady.
    pub(crate) last_version: Option<u64>,
    /// The region dimensions (width, height) last composed at, so a resize that
    /// reshapes the grids forces a recompose.
    pub(crate) last_region_dims: Option<(u16, u16)>,
    /// Whether the last composed window was buffered. A skip frame reuses this
    /// verdict rather than re-testing, so an unbuffered pool stays degraded to
    /// the live grid without recomposing.
    pub(crate) last_buffered: bool,
    /// The sub-cell fraction [`Self::document_grid`] still shows from its last
    /// successful compose. While a frame's window is unbuffered, the region
    /// holds this last good composite at this offset instead of snapping back
    /// to the live grid. `None` until the first successful compose, and cleared
    /// on a resize (which reshapes the grid) or a settled handoff (after which
    /// the live grid owns the region).
    pub(crate) held_frac: Option<f32>,
}

impl PoolAnim {
    /// A fresh pool resting at `scroll`, so a newly declared pool shows at its
    /// current position rather than gliding in from the document origin.
    pub(crate) fn new(scroll: f32) -> PoolAnim {
        PoolAnim {
            scroll,
            last_scroll_target: scroll,
            target_stable_for: HANDOFF_STABLE_TIME,
            document_grid: Grid::new(0, 0),
            last_top: None,
            last_version: None,
            last_region_dims: None,
            last_buffered: false,
            held_frac: None,
        }
    }
}

/// A pool that is mid-glide and buffered this frame, so the renderer composites
/// it: which pool, its region, and the sub-cell fraction to shift its rows by.
pub(crate) struct ActivePool {
    pub(crate) id: u32,
    pub(crate) region: PoolRegion,
    pub(crate) frac: f32,
    /// Whether the pool's composed rows changed since the previous frame. When
    /// `false` the glide only advanced the sub-cell fraction, so the copy into
    /// the pool grid and the composite's instance rebuild are both skipped and
    /// only the shift is re-applied.
    pub(crate) content_changed: bool,
    /// Rows the composed content moved by, when moving is all it did.
    ///
    /// An eased scroll walks the top a row or three at a time for many frames,
    /// and every row it keeps was already shaped for the frame before, so the
    /// composite can slide its per-row caches by this and re-shape only what the
    /// move exposed.
    ///
    /// `None` when nothing can be carried. The first frame of a glide, a content
    /// version bump, and a resize each leave the previous rows describing
    /// something else.
    pub(crate) scrolled_rows: Option<isize>,
}

/// The outcome of stepping one pool's glide for a frame.
pub(crate) enum PoolStep {
    /// The target held steady long enough and the ease has arrived, so the pool
    /// hands its region back to the base grid and stops ticking.
    Settled,
    /// Still gliding, but this frame's window is unbuffered with no held
    /// composite, so the base grid shows through. The loop keeps ticking.
    Degraded,
    /// Still gliding with a composite ready at [`ActivePool::frac`].
    Gliding(ActivePool),
}

/// What a pool's composed rows rest on this frame, and whether any of it moved
/// since the frame that last composed them.
pub(crate) struct ComposeGate {
    /// Sub-cell fraction of the eased offset, the shift the composite rides at.
    pub(crate) frac: f32,
    /// Integer top document row the composed rows start at.
    top: i64,
    version: Option<u64>,
    region_dims: (u16, u16),
    /// Whether the rows have to be composed again.
    pub(crate) content_changed: bool,
    /// Rows the content moved since the last compose, or `None` when nothing
    /// carries across.
    pub(crate) scrolled_rows: Option<isize>,
}

/// Whether `anim`'s composed rows still describe the pool at `scroll`.
///
/// The rows rest on the integer top document row, the pooled page bytes, and
/// the region size, and on nothing else. While all three hold, the offset moved
/// only within a cell, so last frame's composite still describes the content
/// and neither the projection here nor the copy downstream has to run.
///
/// Both the glide and a pool riding someone else's glide ask this, since a ride
/// moves none of the three either.
pub(crate) fn compose_gate(
    anim: &PoolAnim,
    pool: &PoolView,
    terminal: &Terminal,
    scroll: f32,
) -> ComposeGate {
    let page_rows = (pool.region.height as f32).max(1.0);
    let doc_rows = scroll * page_rows;
    let top = doc_rows.floor() as i64;
    let version = terminal.pool_content_version(pool.id);
    let region_dims = (pool.region.width, pool.region.height);

    let carries = anim.last_version == version && anim.last_region_dims == Some(region_dims);
    ComposeGate {
        frac: doc_rows - top as f32,
        top,
        version,
        region_dims,
        content_changed: !carries || anim.last_top != Some(top),
        // The distance from where the rows were to where they land. A version
        // bump or a resize means the rows describe something else, so nothing
        // carries.
        scrolled_rows: match anim.last_top {
            Some(last) if carries => isize::try_from(top - last).ok(),
            _ => None,
        },
    }
}

impl PoolAnim {
    /// Record what `gate` read, so the next frame compares against this one.
    pub(crate) fn record_compose(&mut self, gate: &ComposeGate, buffered: bool) {
        self.last_top = Some(gate.top);
        self.last_version = gate.version;
        self.last_region_dims = Some(gate.region_dims);
        self.last_buffered = buffered;
    }
}

/// Advance `anim`'s ease one frame toward `pool`'s scroll target and compose the
/// pool's rows at the eased offset into `anim.document_grid`.
///
/// Both render paths share this step-and-compose. The primary composites the
/// result over the live grid, an aux window over its target-projected base. The
/// caller passes any pending `reposition` (it consumes it from the terminal) and
/// handles the cursor and z-ordered blit around this.
///
/// A [`PoolStep::Settled`] result means the region hands off to the base, so the
/// caller drops the pool from its composite set. The recompose is skipped while
/// only the sub-cell fraction moves, so a settled or shift-only pool costs no
/// projection.
pub(crate) fn advance_pool_glide(
    anim: &mut PoolAnim,
    pool: &PoolView,
    terminal: &Terminal,
    reposition: Option<u64>,
    dt: Duration,
) -> PoolStep {
    let page_rows = (pool.region.height as f32).max(1.0);

    // A frame that moved the target scrolled the document, so reset the stable
    // timer. A frame that left it steady advances toward the handoff.
    let target_pages = pool.scroll_target.pages();
    let jump_rows = (target_pages - anim.last_scroll_target) * page_rows;
    anim.last_scroll_target = target_pages;
    if jump_rows == 0.0 {
        anim.target_stable_for = anim.target_stable_for.saturating_add(dt);
    } else {
        anim.target_stable_for = Duration::ZERO;
    }

    // A reposition jump re-anchors the offset to a local neighbour of the
    // destination, so the ease lands softly within the freshly-buffered window
    // instead of dragging across the unbuffered gap. The neighbour sits on the
    // side the viewport travelled from, read here from the pre-reposition
    // offset, so the landing continues the motion instead of reversing it.
    if let Some(target) = reposition {
        anim.scroll = reposition_scroll(anim.scroll, target);
    }

    let (scroll, easing) = step_document_scroll(anim.scroll, target_pages, page_rows, dt);
    let scroll_settled = anim.target_stable_for >= HANDOFF_STABLE_TIME;
    anim.scroll = scroll;
    if !easing && scroll_settled {
        // The base owns the region once it hands off. Drop any held composite so
        // a later re-glide cannot resurrect content the base has since replaced.
        anim.held_frac = None;
        return PoolStep::Settled;
    }

    let gate = compose_gate(anim, pool, terminal, scroll);

    // A resize reshapes document_grid, so a held composite from the old
    // dimensions no longer fits the region.
    if anim.last_region_dims != Some(gate.region_dims) {
        anim.held_frac = None;
    }

    let buffered = if gate.content_changed {
        let composed = terminal
            .project_pool(pool.id, &mut anim.document_grid, scroll)
            .is_some();
        anim.record_compose(&gate, composed);
        composed
    } else {
        anim.last_buffered
    };

    if buffered {
        anim.held_frac = Some(gate.frac);
        PoolStep::Gliding(ActivePool {
            id: pool.id,
            region: pool.region,
            frac: gate.frac,
            content_changed: gate.content_changed,
            scrolled_rows: gate.scrolled_rows,
        })
    } else if let Some(held) = anim.held_frac {
        // The window is not buffered this frame. Re-push the last good composite
        // at its held offset so the region holds it instead of snapping back to
        // the base grid.
        PoolStep::Gliding(ActivePool {
            id: pool.id,
            region: pool.region,
            frac: held,
            content_changed: false,
            scrolled_rows: None,
        })
    } else {
        PoolStep::Degraded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stoatty_protocol::command::{
        encode_fill, encode_fill_end, encode_pool_region, FillCommand, PoolRegionCommand,
    };
    use stoatty_term::{
        grid::{DocumentOffset, Rgb},
        theme::Theme,
    };

    /// A pool eight rows tall, and a terminal that has declared it.
    fn gated_pool() -> (PoolView, Terminal) {
        let mut terminal = Terminal::new(8, 4, Theme::default());
        terminal.advance(&encode_pool_region(&PoolRegionCommand {
            pool: 1,
            top: 0,
            left: 0,
            width: 4,
            height: 8,
            window: 0,
        }));
        let view = PoolView {
            id: 1,
            region: PoolRegion {
                pool: 1,
                window: 0,
                top: 0,
                left: 0,
                width: 4,
                height: 8,
            },
            scroll_target: DocumentOffset::default(),
            cursor_anchor: None,
            anchor: None,
            content_version: 0,
        };
        (view, terminal)
    }

    /// The composed rows rest on the top row, the page bytes, and the region
    /// size. A glide between two frames moves none of those while it advances
    /// within one cell, and a ride moves none of them at all.
    #[test]
    fn a_move_inside_one_cell_composes_nothing_again() {
        let (view, terminal) = gated_pool();
        let mut anim = PoolAnim::new(0.0);

        // Eight rows to a page, so both of these rest on top row 2 and differ
        // only in how far into it they sit.
        let first = compose_gate(&anim, &view, &terminal, 0.25);
        assert!(first.content_changed, "nothing has been composed yet");
        assert_eq!((first.top, first.frac), (2, 0.0));
        anim.record_compose(&first, true);

        let drifted = compose_gate(&anim, &view, &terminal, 0.3);
        assert!(!drifted.content_changed, "the same rows at a new fraction");
        assert_eq!(drifted.scrolled_rows, Some(0), "and they moved no rows");
        assert!((drifted.frac - 0.4).abs() < 1e-5, "got {}", drifted.frac);
    }

    #[test]
    fn each_of_the_three_inputs_moving_composes_again() {
        let (view, mut terminal) = gated_pool();
        let mut anim = PoolAnim::new(0.0);
        anim.record_compose(&compose_gate(&anim, &view, &terminal, 0.0), true);

        let scrolled = compose_gate(&anim, &view, &terminal, 1.0);
        assert!(scrolled.content_changed, "a new top row");
        assert_eq!(scrolled.scrolled_rows, Some(8), "eight rows further down");

        let mut narrower = view;
        narrower.region.width = 2;
        assert!(
            compose_gate(&anim, &narrower, &terminal, 0.0).content_changed,
            "a resized region reshapes the rows",
        );

        fill_page(&mut terminal, 1, 0, b"x");
        let refilled = compose_gate(&anim, &view, &terminal, 0.0);
        assert!(refilled.content_changed, "new page bytes");
        assert_eq!(
            refilled.scrolled_rows, None,
            "and nothing of the old rows carries",
        );
    }

    fn fill_page(terminal: &mut Terminal, pool: u32, index: u64, text: &[u8]) {
        let mut stream = encode_fill(&FillCommand { pool, index });
        stream.extend_from_slice(text);
        stream.extend_from_slice(&encode_fill_end());
        terminal.advance(&stream);
    }

    fn popover(height: u16, scale: u8, content: &str) -> Overlay {
        Overlay {
            top: 0,
            left: 0,
            width: 4,
            height,
            fill: Rgb::new(0, 0, 0),
            border: Rgb::new(0, 0, 0),
            content_fg: Rgb::new(0, 0, 0),
            scale,
            offset: [0, 0],
            bold: false,
            content: content.to_owned(),
        }
    }

    #[test]
    fn cursor_in_region_uses_exclusive_far_edges() {
        let region = PoolRegion {
            pool: 0,
            top: 2,
            left: 3,
            width: 4,
            height: 5,
            window: 0,
        };
        let at = |col, row| {
            cursor_in_region(
                Cursor {
                    row,
                    col,
                    shape: CursorShape::Block,
                },
                region,
            )
        };
        assert!(at(3, 2), "near corner is inside");
        assert!(at(6, 6), "far interior cell is inside");
        assert!(!at(2, 2), "a column left of the region is outside");
        assert!(!at(7, 2), "the right edge is exclusive");
        assert!(!at(3, 7), "the bottom edge is exclusive");
    }

    #[test]
    fn anchored_cursor_rides_the_glide_and_hides_off_pane() {
        // A pool at top row 4 and 40 rows tall has eased a quarter page (10 rows)
        // down. The cursor's document row 20 draws 10 rows higher, still in pane.
        let (pos, in_region) = anchored_cursor_pos(4.0, 40.0, 20.0, 7.0, 0.25);
        assert_eq!(pos, [7.0, 14.0]);
        assert!(in_region);

        // Eased far enough, the line rides off the top edge and hides.
        let (pos, in_region) = anchored_cursor_pos(4.0, 40.0, 6.0, 7.0, 0.25);
        assert_eq!(pos, [7.0, 0.0]);
        assert!(!in_region);

        // A line below the pane's bottom edge is hidden too.
        let (_, in_region) = anchored_cursor_pos(0.0, 40.0, 45.0, 0.0, 0.0);
        assert!(!in_region);
    }

    #[test]
    fn anchored_shift_tracks_the_gap_between_the_assumed_and_eased_top() {
        // A 40-row host eased to a quarter page sits at document row 10. A
        // surface whose layout assumed that same top needs no shift.
        assert_eq!(anchored_shift(10.0, 0.25, 40.0, 16.0), 0.0);

        // Mid scroll-down the content still lags above the target, so the host
        // top is row 8 while the layout assumed 10. Two rows of shift draws the
        // surface lower, keeping it over the text it was laid out against.
        assert_eq!(anchored_shift(10.0, 0.2, 40.0, 16.0), 32.0);

        // A shift under one cell passes through rather than snapping, which is
        // what lets the surface ride the ease sub-cell. A 1/32-row gap is exact
        // in f32, so this pins the half-pixel rather than a rounding artifact.
        assert_eq!(anchored_shift(10.031_25, 0.25, 40.0, 16.0), 0.5);
    }

    #[test]
    fn shift_scissor_moves_the_rect_and_clamps_at_the_top_edge() {
        assert_eq!(shift_scissor([10, 40, 100, 60], 12.0), [10, 52, 100, 60]);

        // Carried above the surface, the rect keeps only what is still on
        // screen, so the height loses exactly what the clamp cut.
        assert_eq!(shift_scissor([10, 40, 100, 60], -55.0), [10, 0, 100, 45]);
    }

    #[test]
    fn intersect_scissor_overlaps_or_collapses() {
        assert_eq!(
            intersect_scissor([0, 0, 100, 100], [20, 30, 100, 100]),
            [20, 30, 80, 70]
        );

        // Disjoint rects collapse rather than wrapping around, so a ride past
        // the pane edge draws nothing instead of drawing everywhere.
        assert_eq!(
            intersect_scissor([0, 0, 10, 10], [50, 50, 10, 10]),
            [50, 50, 0, 0]
        );
    }

    #[test]
    fn ease_steps_toward_then_settles() {
        let (next, settled) = ease([0.0, 0.0], [4.0, 0.0], EASE_BASELINE_FRAME);
        assert!(next[0] > 0.0 && next[0] < 4.0);
        assert!(!settled);

        let (next, settled) = ease([3.999, 2.0], [4.0, 2.0], EASE_BASELINE_FRAME);
        assert_eq!(next, [4.0, 2.0]);
        assert!(settled);
    }

    /// Two half-length frames must advance an ease as far as one baseline
    /// frame, so animation speed is refresh-rate independent.
    #[test]
    fn ease_is_frame_rate_invariant() {
        let half = EASE_BASELINE_FRAME / 2;

        let (whole, _) = ease([0.0, 0.0], [4.0, 0.0], EASE_BASELINE_FRAME);
        let (halfway, _) = ease([0.0, 0.0], [4.0, 0.0], half);
        let (twice, _) = ease(halfway, [4.0, 0.0], half);

        assert!(
            (twice[0] - whole[0]).abs() < 1e-4,
            "two half frames ({}) land where one whole frame does ({})",
            twice[0],
            whole[0]
        );
        assert!(halfway[0] < whole[0], "a half frame advances less");
    }

    #[test]
    fn reposition_lands_on_the_side_the_viewport_travelled_from() {
        // A viewport travelling down to page 20 lands a page above it.
        let seeded = reposition_scroll(2.0, 20);
        assert_eq!(seeded, 19.0, "a downward jump lands above the target");
        let (next, _) = step_document_scroll(seeded, 20.0, 40.0, EASE_BASELINE_FRAME);
        assert!(
            next > seeded,
            "the landing glide continues downward, got {next}"
        );

        // A viewport travelling up to the same page lands a page below it.
        let seeded = reposition_scroll(90.0, 20);
        assert_eq!(seeded, 21.0, "an upward jump lands below the target");
        let (next, _) = step_document_scroll(seeded, 20.0, 40.0, EASE_BASELINE_FRAME);
        assert!(
            next < seeded,
            "the landing glide continues upward, got {next}"
        );
    }

    #[test]
    fn reposition_to_the_first_page_still_leaves_a_glide() {
        // Nothing exists above page 0, so an upward jump there has to land below
        // it. Seeding above would clamp onto the target and skip the glide.
        let seeded = reposition_scroll(50.0, 0);
        assert_eq!(seeded, 1.0, "an upward jump to page 0 lands a page below");

        let (next, easing) = step_document_scroll(seeded, 0.0, 40.0, EASE_BASELINE_FRAME);
        assert!(easing, "there is still a glide to run");
        assert!(next < seeded, "and it runs upward toward page 0");
    }

    #[test]
    fn seed_settle_flight_clamps_an_offscreen_start_to_the_edge() {
        let rows = 40usize;

        // A cursor that slid far below the viewport starts the flight from just
        // beyond the bottom edge, not from ninety rows down.
        let mut anchored = true;
        let mut point = [5.0, 90.0];
        let mut corners = block_corners(point);
        seed_settle_flight(&mut anchored, &mut point, &mut corners, rows);
        assert_eq!(
            point,
            [5.0, 40.0],
            "off the bottom clamps to just past the edge"
        );
        assert!(!anchored, "the anchor flag is cleared");

        // Above the top clamps to just above it.
        let mut anchored = true;
        let mut point = [5.0, -20.0];
        let mut corners = block_corners(point);
        seed_settle_flight(&mut anchored, &mut point, &mut corners, rows);
        assert_eq!(
            point,
            [5.0, -1.0],
            "off the top clamps to just past the edge"
        );

        // An on-screen position is the flight origin, left untouched.
        let mut anchored = true;
        let mut point = [5.0, 12.0];
        let mut corners = block_corners(point);
        seed_settle_flight(&mut anchored, &mut point, &mut corners, rows);
        assert_eq!(
            point,
            [5.0, 12.0],
            "an on-screen start eases from where it is"
        );

        // No anchor means no seeding.
        let mut anchored = false;
        let mut point = [5.0, 90.0];
        let mut corners = block_corners(point);
        seed_settle_flight(&mut anchored, &mut point, &mut corners, rows);
        assert_eq!(
            point,
            [5.0, 90.0],
            "without an anchor the position is untouched"
        );
    }

    #[test]
    fn settle_flight_eases_from_the_edge_toward_the_landing() {
        // Seeding from off-screen then stepping must move the cursor part-way,
        // not teleport it onto the landing cell.
        let mut anchored = true;
        let mut point = [5.0, 90.0];
        let mut corners = block_corners(point);
        seed_settle_flight(&mut anchored, &mut point, &mut corners, 40);

        let landing = [5.0, 22.0];
        let (_, _, easing) = step_cursor(
            CursorAnimation::Block,
            &mut point,
            &mut corners,
            Some(landing),
            EASE_BASELINE_FRAME,
        );
        assert!(easing, "the cursor is still in flight, not settled");
        assert!(
            point[1] < 40.0 && point[1] > landing[1],
            "it advanced from the edge toward the landing: {point:?}",
        );
    }

    #[test]
    fn ease_corners_leading_edge_outruns_trailing() {
        let rest = [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]];
        let (stepped, settled) = ease_corners(rest, [5.0, 0.0], EASE_BASELINE_FRAME);

        assert!(!settled, "a step toward a distant cell has not settled");

        let trailing = stepped[0][0] - rest[0][0];
        let leading = stepped[1][0] - rest[1][0];
        assert!(
            leading > trailing,
            "leading edge {leading} outruns trailing {trailing}"
        );
        assert!(
            stepped[1][0] - stepped[0][0] > 1.0,
            "the quad spans wider than one cell along the motion axis"
        );
    }

    #[test]
    fn ease_corners_snaps_onto_the_target_block() {
        let near = [[3.0, 2.0], [4.0, 2.0], [3.0, 3.0], [4.0, 2.995]];
        let (snapped, settled) = ease_corners(near, [3.0, 2.0], EASE_BASELINE_FRAME);

        assert!(settled, "within epsilon of the target reports settled");
        assert_eq!(
            snapped,
            [[3.0, 2.0], [4.0, 2.0], [3.0, 3.0], [4.0, 3.0]],
            "snaps onto the exact target block"
        );
    }

    /// Counting a popover's lines is the per-frame cost worth avoiding, so the refresh
    /// answers from the buffer while the grid's popovers epoch holds.
    ///
    /// The reuse case passes overlays that would compute differently, since a refresh
    /// that recomputed anyway would agree with the cache on unchanged input and the
    /// assertion would not distinguish the two.
    #[test]
    fn popover_overflows_refill_only_when_the_epoch_moves() {
        let mut last_epoch = None;
        let mut overflows = Vec::new();

        refresh_popover_overflows(
            &[popover(2, 1, "a\nb\nc\nd")],
            7,
            &mut last_epoch,
            &mut overflows,
        );
        assert_eq!(overflows, [Some(2.0)], "the first refresh fills the buffer");

        refresh_popover_overflows(&[popover(9, 1, "a")], 7, &mut last_epoch, &mut overflows);
        assert_eq!(
            overflows,
            [Some(2.0)],
            "the same epoch keeps the stored answer without walking the content"
        );

        refresh_popover_overflows(&[popover(9, 1, "a")], 8, &mut last_epoch, &mut overflows);
        assert_eq!(overflows, [None], "a re-applied overlay list is recounted");
    }

    /// The caller sizes its per-overlay scroll state from this buffer's length, so a
    /// shorter overlay list has to shorten it rather than leave stale trailing entries.
    #[test]
    fn a_shorter_overlay_list_shortens_the_overflows() {
        let mut last_epoch = None;
        let mut overflows = Vec::new();
        let two = [popover(1, 1, "a\nb"), popover(1, 1, "c\nd")];

        refresh_popover_overflows(&two, 1, &mut last_epoch, &mut overflows);
        assert_eq!(overflows, [Some(1.0), Some(1.0)], "one entry per overlay");

        refresh_popover_overflows(&two[..1], 2, &mut last_epoch, &mut overflows);
        assert_eq!(overflows, [Some(1.0)], "a closed popover drops its entry");

        refresh_popover_overflows(&[], 3, &mut last_epoch, &mut overflows);
        assert_eq!(overflows, [], "no overlays leaves no entries");
    }

    #[test]
    fn popover_overflow_reports_rows_past_the_box() {
        assert_eq!(
            popover_overflow(&popover(2, 1, "a\nb\nc\nd")),
            Some(2.0),
            "two lines past the box"
        );
        assert_eq!(
            popover_overflow(&popover(4, 1, "a\nb")),
            None,
            "fits the box"
        );
        assert_eq!(
            popover_overflow(&popover(2, 1, "a\nb")),
            None,
            "exactly fills the box"
        );
    }

    #[test]
    fn popover_overflow_accounts_for_content_scale() {
        // At scale 2 each line is two rows tall, so three lines span six rows
        // and overflow a four-row box by two even though the line count fits it.
        assert_eq!(
            popover_overflow(&popover(4, 2, "a\nb\nc")),
            Some(2.0),
            "scaled content overflows the box"
        );
        assert_eq!(
            popover_overflow(&popover(6, 2, "a\nb\nc")),
            None,
            "scaled content exactly fills the box"
        );
    }

    #[test]
    fn popover_scroll_ping_pongs_between_ends() {
        let (next, down) = step_popover_scroll(0.0, true, 2.0, EASE_BASELINE_FRAME);
        assert!(next > 0.0 && next < 2.0, "eases down from the top");
        assert!(down);

        let (next, down) = step_popover_scroll(1.999, true, 2.0, EASE_BASELINE_FRAME);
        assert_eq!(next, 2.0, "snaps onto the bottom");
        assert!(!down, "reverses at the bottom");

        let (next, down) = step_popover_scroll(0.001, false, 2.0, EASE_BASELINE_FRAME);
        assert_eq!(next, 0.0, "snaps onto the top");
        assert!(down, "reverses at the top");
    }

    #[test]
    fn grid_scroll_eases_a_delta_to_zero() {
        // A new delta seeds the offset and starts easing down toward zero.
        let (next, easing) = step_grid_scroll(0.0, 3, EASE_BASELINE_FRAME);
        assert!(next > 0.0 && next < 3.0, "eases from the seed");
        assert!(easing);

        // No new delta, within the snap epsilon: settles at zero.
        let (next, easing) = step_grid_scroll(0.005, 0, EASE_BASELINE_FRAME);
        assert_eq!(next, 0.0, "snaps onto zero");
        assert!(!easing);
    }

    #[test]
    fn scrollback_scroll_eases_toward_a_target() {
        // A target deeper in history eases toward it without overshooting.
        let (next, easing) = step_scrollback_scroll(0.0, 4.0, EASE_BASELINE_FRAME);
        assert!(next > 0.0 && next < 4.0, "eases toward the target");
        assert!(easing);

        // Within the snap epsilon of the target: settles on it.
        let (next, easing) = step_scrollback_scroll(3.999, 4.0, EASE_BASELINE_FRAME);
        assert_eq!(next, 4.0, "snaps onto the target");
        assert!(!easing);

        // Near the target the per-frame step is floored so the tail does not
        // crawl: from twice the floor out it advances by the floor itself, not
        // the smaller geometric step.
        let (next, easing) =
            step_scrollback_scroll(0.0, SCROLLBACK_MIN_STEP * 2.0, EASE_BASELINE_FRAME);
        assert!(
            (next - SCROLLBACK_MIN_STEP).abs() < 1e-5,
            "tail advances by the floor"
        );
        assert!(easing);
    }

    #[test]
    fn region_scroll_eases_a_signed_delta_to_zero() {
        // A positive delta (content scrolled down) seeds and eases toward zero.
        let (next, easing) = step_region_scroll(0.0, 3.0, EASE_BASELINE_FRAME);
        assert!(next > 0.0 && next < 3.0, "eases from the positive seed");
        assert!(easing);

        // A negative delta (content scrolled up) eases up from below zero.
        let (next, easing) = step_region_scroll(0.0, -3.0, EASE_BASELINE_FRAME);
        assert!(next < 0.0 && next > -3.0, "eases from the negative seed");
        assert!(easing);

        // No new delta, within the snap epsilon: settles at zero.
        let (next, easing) = step_region_scroll(0.005, 0.0, EASE_BASELINE_FRAME);
        assert_eq!(next, 0.0, "snaps onto zero");
        assert!(!easing);
    }

    #[test]
    fn document_scroll_eases_toward_a_target() {
        // A target ahead of the live offset eases toward it without overshooting.
        let (next, easing) = step_document_scroll(0.0, 4.0, 20.0, EASE_BASELINE_FRAME);
        assert!(next > 0.0 && next < 4.0, "eases toward the target");
        assert!(easing);

        // The row-sized min-step floor, capped at the remaining distance, lands
        // exactly on the whole-row target instead of snapping a visible fraction
        // of a row; the next frame then settles cleanly.
        let (next, easing) = step_document_scroll(4.0 - 0.001, 4.0, 20.0, EASE_BASELINE_FRAME);
        assert_eq!(next, 4.0, "min-step lands exactly on the target");
        assert!(easing);

        // Already within a sub-pixel (in rows) of the target: settles on it.
        let (next, easing) = step_document_scroll(4.0 - 0.0001, 4.0, 20.0, EASE_BASELINE_FRAME);
        assert_eq!(next, 4.0, "snaps onto the target");
        assert!(!easing);
    }
}

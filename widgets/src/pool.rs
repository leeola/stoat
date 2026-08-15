//! Drives stoatty's region-scoped smooth-scroll pools.
//!
//! A pool is a rectangle of the grid the terminal scrolls on its own. The
//! program declares the rectangle as a pool region, renders its content a page
//! at a time into off-grid pool slots, and reports an absolute scroll target
//! each time the surface scrolls. The terminal eases each pool's visible offset
//! toward its target at sub-cell granularity, so several surfaces glide
//! independently and at once while the chrome around them stays fixed.
//!
//! A "page" is one region-sized screen of the content: `region.height` rows of
//! `region.width` columns, the page at index `p` starting at content row
//! `p * region.height`. Each pool is addressed by this page index, the same key
//! [`ScrollCommand::page`] and `fill` carry.
//!
//! [`SmoothScrollState`] holds what has been declared per pool, so a frame emits
//! only what moved. Pools are keyed by an id the program picks and keeps stable
//! for the life of a surface. A surface that goes away is retired through
//! [`SmoothScrollState::drop_absent`], which frees the terminal's buffers for it.

use std::{
    collections::{BTreeMap, HashMap},
    ops::Range,
};
use stoatty_protocol::command::{
    encode_fill_scope, encode_minimap_view_into, encode_pool_drop_into, encode_pool_region_into,
    encode_reposition_into, encode_scroll_into, MinimapViewCommand, PoolRegionCommand,
    ScrollCommand,
};

/// Pages kept buffered around each pool's visible page, the pool's working
/// window. Wide enough that the visible page and its straddle neighbour (when a
/// fractional scroll shows the bottom of one page and the top of the next) are
/// always present, plus slack so an in-flight ease never outruns the filled
/// slots.
const WINDOW_PAGES: u64 = 5;

/// What has been declared to the terminal for each pool, so each frame emits
/// only the deltas.
///
/// One of these covers every pool a program drives. Hold it for the life of the
/// program and thread it into [`emit_into`], once per surface per frame, and
/// into [`Self::drop_absent`] at the frame seam. Empty on construction. A pool
/// is added on its first [`emit_into`] and removed by [`Self::drop_absent`] when
/// its surface goes away.
#[derive(Default)]
pub struct SmoothScrollState {
    pools: BTreeMap<u32, PoolEmitState>,
    /// What the most recent [`MinimapViewCommand`] carried per strip id, so an
    /// unmoved viewport neither re-emits a thumb update nor re-derives one.
    minimap_views: HashMap<u32, MinimapView>,
}

/// What a strip's thumb was last placed at, and what placed it there.
///
/// The pool belongs here because a program that feeds one strip from different
/// pools as focus moves must still re-emit a same-offset view from a new pool.
#[derive(PartialEq, Eq)]
struct MinimapView {
    pool: u32,
    top_256: u32,
    visible: u16,
    from: MinimapWindowInputs,
}

/// The inputs a strip's window was derived from.
///
/// Deriving that window converts display rows to content lines several times
/// over, each a clip and a tree descent, so a frame that would derive the same
/// window again is worth recognizing before doing the work rather than after.
///
/// The scroll offset compares by bits because this asks whether the inputs are
/// identical, not whether two numbers are numerically close.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MinimapWindowInputs {
    scroll_offset_bits: u32,
    map_version: u64,
    viewport_rows: u32,
}

impl MinimapWindowInputs {
    pub fn new(scroll_offset: f32, map_version: u64, viewport_rows: u32) -> Self {
        Self {
            scroll_offset_bits: scroll_offset.to_bits(),
            map_version,
            viewport_rows,
        }
    }
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
    /// A caller filling synchronously has this equal what is filled. One filling
    /// asynchronously off-thread has it track requests, not completions. The
    /// window is always contiguous, so a `Range` suffices. Re-requesting a page
    /// when it re-enters the window is correct -- it matches the terminal
    /// recycling slots that fall outside the window.
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
    /// Whether `pool`'s last emit already carried `version`.
    ///
    /// Lets a caller whose page bytes are expensive to produce decide not to
    /// produce them, rather than rendering a page for [`emit_into`] to discard
    /// on the same comparison. Only sound for a pool whose whole emit is
    /// determined by the version, which for the single-page window surfaces
    /// means folding the region into it, since skipping the call skips the
    /// region declaration too.
    ///
    /// False for a pool that has never emitted, so a first display is never
    /// mistaken for an unchanged one on a version that happens to be zero.
    pub fn already_emitted(&self, pool: u32, version: u64) -> bool {
        self.pools
            .get(&pool)
            .is_some_and(|entry| entry.content_version == version)
    }

    /// Retire every tracked pool whose id is not in `active`: emit its
    /// `Gstoatty;pool_drop` into `out` and forget it.
    ///
    /// Call once per frame with the ids of the surfaces pooled this frame, so a
    /// surface that closed, switched to something else, or went behind a
    /// full-screen overlay stops compositing and frees its terminal-side
    /// buffers. A later surface reusing the id re-declares from scratch.
    pub fn drop_absent(&mut self, out: &mut Vec<u8>, active: &[u32]) {
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
    /// `pool` is the scroll pool feeding the strip this frame, and `inputs` is
    /// what the window would be derived from. `window` produces the fractional
    /// top content row and the viewport height in lines, and runs only when
    /// `inputs` differ from the last emit's, since deriving a window costs
    /// several display-to-content conversions and identical inputs give an
    /// identical answer.
    ///
    /// Inputs that moved need not mean the window did, so a fresh window is
    /// still compared before anything is sent. Either way the new inputs are
    /// recorded, or the next frame would derive it again for the same reason.
    pub fn emit_minimap_view(
        &mut self,
        out: &mut Vec<u8>,
        strip_id: u32,
        pool: u32,
        inputs: MinimapWindowInputs,
        window: impl FnOnce() -> (f32, u16),
    ) {
        let held = self.minimap_views.get_mut(&strip_id);
        if held
            .as_ref()
            .is_some_and(|view| view.pool == pool && view.from == inputs)
        {
            return;
        }

        let (top, visible) = window();
        let top_256 = (top * 256.0) as u32;
        if let Some(view) = held
            && view.pool == pool
            && view.top_256 == top_256
            && view.visible == visible
        {
            view.from = inputs;
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
        self.minimap_views.insert(
            strip_id,
            MinimapView {
                pool,
                top_256,
                visible,
                from: inputs,
            },
        );
    }
}

/// Append the smooth-scroll APC frames for one pool's current scroll position to
/// `out`, updating `state` to reflect what was emitted.
///
/// `region` is the surface's rectangle in absolute grid cells, carrying the
/// pool id ([`PoolRegionCommand::pool`]) the pool is tracked under.
/// `scroll_offset` is the surface's fractional top visible content row. Its
/// integer part selects the page and its fraction drives the sub-row glide. The
/// closure `render_page` returns the self-contained VT bytes painting page
/// `index`, which covers content rows `index * region.height ..`.
///
/// `content_version` changes whenever the surface's content changes (a
/// re-filtered list, a regenerated diff); a value differing from the last emit
/// forces the buffered window to refill so a stale page is never composited.
/// Pass a constant for content that is stable while scrolling.
///
/// `hold_when_idle` narrows a frame whose target did not move to the visible
/// page and defers a content change until the target shifts. It suits a surface
/// whose content churns while it rests, since a pool composites only while its
/// eased offset moves.
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
/// are already in `out`); a caller filling asynchronously passes an
/// empty-returning `render_page` and fills these pages off-thread instead.
pub fn emit_into(
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
/// fill frame. A caller filling asynchronously uses that to mean "no synchronous
/// fill". A real render is never empty -- a serialized page always carries cursor
/// moves and cells -- so empty is an unambiguous sentinel.
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
            encode_fill_scope(out, pool, index, |out| out.extend_from_slice(&bytes));
        }
    }
    entry.requested = Some(window);
    entered
}

/// The half-open page window centered on `page`, clamped at the content start.
///
/// Centering leaves pages buffered on both sides of the visible page so an ease
/// lagging behind a jump stays covered in either direction.
fn window_range(page: u64) -> Range<u64> {
    let start = page.saturating_sub(WINDOW_PAGES / 2);
    start..start + WINDOW_PAGES
}

/// Map a fractional top visible content row to pool `pool`'s scroll target, a
/// page index plus a sub-page fraction in 1/65536ths of a page.
///
/// `region_height` is the pool region's row count, the rows per page. The page
/// is the integer number of full regions scrolled past. The fraction is how far
/// into the next page the partial offset sits, carrying the sub-row part so the
/// terminal eases the pool below a whole row.
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

#[cfg(test)]
mod tests {
    use super::{
        emit_into, scroll_target, window_range, MinimapWindowInputs, SmoothScrollState,
        WINDOW_PAGES,
    };
    use stoatty_protocol::command::{
        decode, Command, PoolDropCommand, PoolRegionCommand, RepositionCommand, ScrollCommand,
    };

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

    /// Deriving a strip's window converts display rows to content lines three
    /// times over, and an idle frame would derive the same window it derived
    /// last frame.
    ///
    /// Counting the derivations is the only way to see the saving, since a
    /// frame that derives needlessly still sends nothing and looks identical
    /// from the outside. Each input is moved on its own, because one left out of
    /// the record would leave the thumb reporting a window that no longer
    /// matches the viewport.
    #[test]
    fn an_unmoved_thumb_neither_derives_nor_emits() {
        /// Emit for strip 1 from pool 2, returning the bytes written.
        fn emit(
            state: &mut SmoothScrollState,
            derives: &mut u32,
            inputs: MinimapWindowInputs,
            window: (f32, u16),
        ) -> usize {
            let mut out = Vec::new();
            state.emit_minimap_view(&mut out, 1, 2, inputs, || {
                *derives += 1;
                window
            });
            out.len()
        }

        let mut state = SmoothScrollState::default();
        let mut derives = 0;
        let settled = MinimapWindowInputs::new(3.5, 7, 40);

        assert!(
            emit(&mut state, &mut derives, settled, (3.5, 40)) > 0,
            "the first frame places the thumb",
        );
        assert_eq!(derives, 1, "and derives the window to do it");

        assert_eq!(
            emit(&mut state, &mut derives, settled, (3.5, 40)),
            0,
            "an unmoved frame sends nothing",
        );
        assert_eq!(derives, 1, "and derives nothing either");

        // A scroll too small to move the thumb. The inputs moved, so the window
        // is derived, but nothing is sent.
        let nudged = MinimapWindowInputs::new(3.502, 7, 40);
        assert_eq!(
            emit(&mut state, &mut derives, nudged, (3.5, 40)),
            0,
            "a window that came out the same sends nothing",
        );
        assert_eq!(derives, 2, "though it had to be derived to know that");
        assert_eq!(
            emit(&mut state, &mut derives, nudged, (3.5, 40)),
            0,
            "and the frame after it is idle again",
        );
        assert_eq!(
            derives, 2,
            "the inputs that produced it were recorded, not just the ones that emitted",
        );

        for (moved, label) in [
            (MinimapWindowInputs::new(9.0, 7, 40), "a scroll"),
            (MinimapWindowInputs::new(3.502, 8, 40), "an edit"),
            (MinimapWindowInputs::new(3.502, 7, 41), "a resize"),
        ] {
            let before = derives;
            emit(&mut state, &mut derives, moved, (9.0, 41));
            assert_eq!(derives, before + 1, "{label} must derive the window again");
            // Back to a shared baseline so each input is judged on its own.
            state = SmoothScrollState::default();
            emit(&mut state, &mut derives, nudged, (3.5, 40));
            derives = 0;
        }
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

    /// The never-emitted answer is the one that matters. A caller skipping on
    /// `true` would never declare the pool at all if a first display could
    /// report its version as already sent.
    #[test]
    fn a_pool_reports_its_last_emitted_version_and_nothing_before_it() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();

        assert!(
            !state.already_emitted(1, 0),
            "an untracked pool has emitted no version, not version zero"
        );

        emit_into(&mut out, &mut state, region(1, 20), 0.0, 0, false, |_| {
            Vec::new()
        });
        assert!(state.already_emitted(1, 0));
        assert!(!state.already_emitted(1, 7));
        assert!(!state.already_emitted(2, 0), "pools answer for themselves");

        emit_into(&mut out, &mut state, region(1, 20), 0.0, 7, false, |_| {
            Vec::new()
        });
        assert!(state.already_emitted(1, 7));
        assert!(!state.already_emitted(1, 0));
    }

    #[test]
    fn without_hold_a_resting_content_change_still_refills() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();

        emit_into(&mut out, &mut state, region(1, 20), 40.0, 0, false, |_| {
            Vec::new()
        });

        // A pool passing hold_when_idle false refills the full window on a
        // content change even while the target is stationary.
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
}

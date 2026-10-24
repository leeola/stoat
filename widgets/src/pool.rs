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
    encode_reposition_into, encode_scroll_into, MinimapViewCommand, PoolKind, PoolRegionCommand,
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
    /// Whether the grid's geometry is still moving, set through
    /// [`Self::set_geometry_unsettled`].
    geometry_unsettled: bool,
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
    /// until first declared.
    ///
    /// Re-emitted only when it changes. A resize or a split moves the
    /// rectangle, which drops the pool's pages. A focus move changes a
    /// terminal pool's dim alone, which keeps them.
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
    /// Content version last seen for this pool.
    ///
    /// A different value from the caller means the surface changed under the
    /// buffered pages. Each page then refills once a composite reads it, and a
    /// page no composite reads keeps its fill until one does.
    content_version: u64,
    /// What the slot of each requested page holds.
    pages: Vec<PageStamp>,
}

/// The versions one requested page's slot holds, as the fills sent to it left
/// them.
///
/// A whole fill sets both. A decorations-only fill sets the decorations, so a
/// page that a move leaves behind keeps the version it holds until a later
/// move heads toward it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PageStamp {
    page: u64,
    /// The `page_cells` answer of [`emit_pages_into`] that the page's whole
    /// fill carried.
    cells: u64,
    /// The [`PageVersions::cells`] that the page's whole fill carried.
    surface: u64,
    /// The [`PageVersions::decorations`] that the page's latest fill carried.
    decorations: u64,
}

/// The two versions a pool's buffered pages answer to.
///
/// `cells` covers everything a page paints into its cells. A change refills
/// each buffered page once a composite reads it, so a press that moves the
/// view one row repaints the pages it shows and not the whole window.
/// `decorations` covers only the text runs, bars, and polylines laid over
/// those cells. A change of it alone re-sends just those, to the buffered
/// pages a scroll composites or heads toward, which costs far less than
/// painting them again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PageVersions {
    pub cells: u64,
    pub decorations: u64,
}

/// The pages one [`emit_pages_into`] call leaves for its caller to render.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Refill {
    /// Pages owed a whole fill, ascending. These are the pages newly entering
    /// the buffered window, the buffered pages whose own cells version moved,
    /// and the buffered pages a composite reads after a change of
    /// [`PageVersions::cells`].
    pub entered: Vec<u64>,
    /// Buffered pages owed a decorations-only fill, ascending. A page is listed
    /// when its decorations changed and the move composites or heads toward
    /// it.
    pub redecorated: Vec<u64>,
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

    /// Whether `pool`'s last emit declared `region`'s placement, so the
    /// terminal holds the pool at that rectangle with the pages the emits sent
    /// it.
    ///
    /// The dim does not count, since a re-declare that changes only the dim
    /// keeps the pages. False for a pool that has never emitted, and for one a
    /// drop retired.
    pub fn holds_region(&self, pool: u32, region: &PoolRegionCommand) -> bool {
        self.pools
            .get(&pool)
            .is_some_and(|entry| entry.region.is_some_and(|held| held.same_placement(region)))
    }

    /// Whether the terminal holds page `index` of `pool` at the content
    /// version that the pool's last emit carried.
    ///
    /// A caller that patches rows of a page it filled asks this first, since a
    /// patch lands only on a page the terminal still buffers. A page outside
    /// the buffered window and one a content change left stale answer false.
    pub fn holds_page(&self, pool: u32, index: u64) -> bool {
        self.pools.get(&pool).is_some_and(|entry| {
            entry
                .pages
                .iter()
                .any(|stamp| stamp.page == index && stamp.surface == entry.content_version)
        })
    }

    /// The region of each pool whose window was requested since the region
    /// last moved, as the pool's last emit declared it, in pool id order.
    ///
    /// For a caller that fills synchronously, these are the regions the
    /// terminal holds pages for. A pool that has never emitted has none, and
    /// neither has one that a drop retired, nor one whose moved region waits
    /// out a resize burst.
    pub fn filled_regions(&self) -> impl Iterator<Item = PoolRegionCommand> {
        self.pools
            .values()
            .filter(|entry| entry.requested.is_some())
            .filter_map(|entry| entry.region)
    }

    /// Declare `region` for a pool whose placement the terminal holds, and
    /// keep the pool's pages.
    ///
    /// For a caller that renders a held pool's page itself and sends only what
    /// changed. A new dim then reaches the terminal ahead of the compare, so a
    /// page that holds still fades all the same. A region that the pool
    /// already holds, one at another placement, and one of a pool that has
    /// never emitted send nothing here, since the next emit declares them.
    pub fn declare_region(&mut self, out: &mut Vec<u8>, region: PoolRegionCommand) {
        let Some(entry) = self.pools.get_mut(&region.pool) else {
            return;
        };
        let Some(held) = entry.region else {
            return;
        };
        if held == region || !held.same_placement(&region) {
            return;
        }
        encode_pool_region_into(out, &region);
        entry.region = Some(region);
    }

    /// Record `version` as the content of `pool`'s buffered pages, and send
    /// nothing.
    ///
    /// For a caller that rendered its page at `version`, compared it with the
    /// page it last sent, and found the two equal. An emit at `version` then
    /// sends nothing and refills no page, as though that page had gone out. A
    /// pool that has never emitted records nothing, since the terminal holds
    /// no page of it.
    pub fn record_content_version(&mut self, pool: u32, version: u64) {
        let Some(entry) = self.pools.get_mut(&pool) else {
            return;
        };
        entry.content_version = version;
        for page in &mut entry.pages {
            page.surface = version;
        }
    }

    /// The scroll target that `pool`'s last emit sent, which is where the
    /// terminal's next glide of the pool starts.
    ///
    /// `None` for a pool that has not emitted, so a caller reads it before
    /// the emit that moves the target.
    pub fn last_target(&self, pool: u32) -> Option<f32> {
        self.pools.get(&pool)?.last_scroll_offset
    }

    /// Hold back the fill of every pool whose rectangle moves, until the grid's
    /// geometry settles.
    ///
    /// A resize arrives as a burst, and each event hands every pool a rectangle
    /// that drops the terminal's slots the one before it filled, so a page
    /// rendered in between is thrown away before it composites. Set this while
    /// a burst is in flight.
    ///
    /// The caller owes the pools one emit once it clears this. A pool that held
    /// its fill back has no other way to get one, since a settled rectangle
    /// raises no event of its own.
    pub fn set_geometry_unsettled(&mut self, unsettled: bool) {
        self.geometry_unsettled = unsettled;
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

    /// Retire pool `id` and forget the thumb placements it fed.
    ///
    /// A tracked pool emits its `Gstoatty;pool_drop` into `out`. An untracked
    /// pool emits nothing, so a caller needs no check before the call.
    ///
    /// This is for an id that passes to other content between two frames,
    /// which [`Self::drop_absent`] does not see because the id stays active.
    /// The next emit under the id declares the pool from scratch, so no page
    /// of the earlier content stays in the terminal's slots. A strip that the
    /// earlier content fed gets its thumb again, even at an unchanged offset.
    pub fn drop_pool(&mut self, out: &mut Vec<u8>, id: u32) {
        if self.pools.remove(&id).is_some() {
            encode_pool_drop_into(out, id);
        }
        self.minimap_views.retain(|_, view| view.pool != id);
    }

    /// Retire every tracked pool and forget every thumb placement: emit a
    /// `Gstoatty;pool_drop` for each pool into `out`, then start over empty.
    ///
    /// This is the routine for a program whose terminal was driven by another
    /// program in between. That program's own reset dropped these pools
    /// terminal-side, so the tracking here describes buffers that no longer
    /// exist and every surface must re-declare from scratch.
    ///
    /// The thumb placements go too. They are kept so an unmoved viewport
    /// re-emits nothing, which is exactly wrong once the strip they placed is
    /// gone: a thumb at an unchanged offset still has to be placed again.
    pub fn drop_all(&mut self, out: &mut Vec<u8>) {
        for id in self.pools.keys() {
            encode_pool_drop_into(out, *id);
        }
        self.pools.clear();
        self.minimap_views.clear();
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
/// `content_version` changes whenever the surface's content changes, such as a
/// re-filtered list or a regenerated diff. After a change, a buffered page
/// refills once a composite reads it, which is a page a move eases through or a
/// page a rest shows. So a stale page is never composited, and a page that no
/// composite reads keeps its old fill until one does. Pass a constant for
/// content that is stable while scrolling.
///
/// `hold_when_idle` defers a content change on a frame whose target did not
/// move and rests on a whole row, until the target shifts. It suits a surface
/// whose content churns while it rests. The terminal composites nothing at a
/// whole-row rest, so the live grid shows the change at once. A rest between
/// two rows keeps compositing, so a change there refills the pages the
/// composite reads, which are the visible page and the page after it.
///
/// A frame whose target did not move keeps the page window the terminal holds
/// when that window covers the pages a composite reads. Otherwise it asks for
/// those pages alone.
///
/// A held pool whose rectangle moves renders no page at all while the caller
/// reports the geometry unsettled through
/// [`SmoothScrollState::set_geometry_unsettled`], since a resize throws away
/// every page it is given while it lasts.
///
/// Emits, in order: a `pool_region` frame when the region changed; a
/// `fill`/page-VT/`fill_end` triple for each page newly entering the buffered
/// window and each stale page a composite reads; a `reposition` frame when the
/// new window is disjoint from the old, so a far jump re-anchors near the
/// destination instead of easing across the gap; then a `scroll` frame carrying
/// the precise target. A frame that needs none of these appends nothing.
///
/// A region that changes only its dim keeps the buffered window, so the
/// terminal fades the pages it holds and no page enters.
///
/// Returns the page indices filled this call, in ascending order: the pages
/// newly entering the buffered window and the stale pages a composite reads. A
/// caller filling synchronously ignores them (the fill bytes are already in
/// `out`); a caller filling asynchronously passes an empty-returning
/// `render_page` and fills these pages off-thread instead.
pub fn emit_into(
    out: &mut Vec<u8>,
    state: &mut SmoothScrollState,
    region: PoolRegionCommand,
    scroll_offset: f32,
    content_version: u64,
    hold_when_idle: bool,
    render_page: impl FnMut(u64) -> Vec<u8>,
) -> Vec<u64> {
    let versions = PageVersions {
        cells: content_version,
        decorations: 0,
    };
    emit_pages_into(
        out,
        state,
        region,
        scroll_offset,
        u64::MAX,
        versions,
        |_| 0,
        hold_when_idle,
        render_page,
    )
    .entered
}

/// [`emit_into`] for a pool whose pages version their decorations apart from
/// their cells.
///
/// A change of `versions.cells` refills the pages a composite reads, as a
/// content version change does there. A change of `versions.decorations` alone
/// lists buffered pages in [`Refill::redecorated`], for the caller to send as
/// decorations-only fills. It lists only the pages the move composites or
/// heads toward, from the pages under the last target onward in the direction
/// the target moves. A page behind the move keeps its old decorations until a
/// later move heads back toward it. A page entering the window this call is
/// never listed there too, because its whole fill carries the decorations.
/// Neither is a page that a cells change left stale, since the whole fill it
/// is owed carries them. The hold rule defers a decoration change the way it
/// defers a cell change.
///
/// `content_rows` is how many rows the surface holds, so the window stops at
/// the last page a composite reads and the surface's end costs no blank page.
/// Pass `u64::MAX` for a surface with no known end.
///
/// `page_cells` answers a cells version for each page on top of
/// `versions.cells`, for content that changes on some pages and not others.
/// An emit whose target moves refills a buffered page whose answer differs
/// from the one its fill carried, and lists it in [`Refill::entered`]. A held
/// emit compares nothing, so the change waits for the target to move like any
/// other content change.
#[allow(clippy::too_many_arguments)]
pub fn emit_pages_into(
    out: &mut Vec<u8>,
    state: &mut SmoothScrollState,
    region: PoolRegionCommand,
    scroll_offset: f32,
    content_rows: u64,
    versions: PageVersions,
    page_cells: impl Fn(u64) -> u64,
    hold_when_idle: bool,
    mut render_page: impl FnMut(u64) -> Vec<u8>,
) -> Refill {
    let pool = region.pool;
    let unsettled = state.geometry_unsettled;
    let entry = state.pools.entry(pool).or_default();

    // A content change seen while the target rests on a whole row waits for the
    // next move. Holding keeps the stored version and the window the terminal
    // holds, so the next glide enters the pages its window gains and refills
    // the pages it composites.
    //
    // Computed before the region wipe below resets last_scroll_offset, so it
    // reflects real scroll motion. A fresh entry has last_scroll_offset None,
    // so its first display counts as scrolling and still prefills the whole window.
    let last_target = entry.last_scroll_offset;
    let scrolling = last_target != Some(scroll_offset);
    let idle = hold_when_idle && !scrolling;
    // A rest on a whole row hands the region back to the live grid, which
    // shows a change at once. A rest between two rows keeps compositing the
    // pool's pages, so a change there refills the pages the composite reads.
    let hold = idle && scroll_offset.fract() == 0.0;

    let region_changed = entry.region != Some(region);
    // A re-declare that changes only the dim keeps the terminal's slots.
    let pages_moved = !entry
        .region
        .is_some_and(|held| held.same_placement(&region));
    if region_changed {
        encode_pool_region_into(out, &region);
        entry.region = Some(region);
    }
    if pages_moved {
        // A fresh region invalidates the pool's slot contents. Force a refill.
        entry.requested = None;
        entry.pages.clear();
        entry.last_scroll_offset = None;
    }

    // A changed surface keeps the buffered pages, and each page's stamp
    // decides below whether a composite reads it stale.
    if !hold {
        entry.content_version = versions.cells;
    }

    let region_height = region.height.max(1) as u64;
    let top = scroll_offset.floor() as u64;
    let page = top / region_height;
    // A composite reads `region.height + 1` rows from the floored top row, so a
    // rest between two rows reads into the page after the visible one. A
    // resident pool composites at a rest on a whole row too, which shows rows
    // of the next page unless the top row starts a page.
    let needed = match (scroll_offset.fract() == 0.0, region.resident) {
        (false, _) => page..page + 2,
        (true, true) => page..top.div_ceil(region_height) + 1,
        (true, false) => page..page + 1,
    };
    let last_page = (content_rows / region_height).max(page + 1);
    let composited = match last_target {
        Some(from) if scrolling => pages_composited(from, scroll_offset, region_height),
        _ => needed.clone(),
    };
    let window = match (idle, &entry.requested) {
        (true, Some(held)) if held.start <= needed.start && needed.end <= held.end => held.clone(),
        (true, _) => needed,
        (false, _) => window_range(page, last_page),
    };

    let prev = entry.requested.clone();
    let jumped = prev.is_some_and(|p| p.end <= window.start || window.end <= p.start);

    // A page rendered into a rectangle the next resize event replaces is thrown
    // away, so a resting pool waits the burst out. A resident grid pool waits it
    // out while it glides too, since the terminal shows the live grid in its
    // place until a fill lands, and that grid holds the region's own content.
    // The live grid under a terminal pool is blank, so that pool fills at once.
    // A first display fills at once for every pool. A rectangle that moves for
    // any other reason has settled by the time the page draws: a picker's box
    // grows with its match count, and blanking that list would cost more than
    // the page it saves.
    let waits = match region.resident && region.kind == PoolKind::Grid {
        true => last_target.is_some(),
        false => idle,
    };
    let entered = if waits && pages_moved && unsettled {
        Vec::new()
    } else {
        refill(
            out,
            entry,
            pool,
            window.clone(),
            (!hold).then_some(&composited),
            &|page| PageStamp {
                page,
                cells: page_cells(page),
                surface: versions.cells,
                decorations: versions.decorations,
            },
            &mut render_page,
        )
    };

    // A page that did not enter keeps its cells and needs only the new
    // decorations. An entered page's fill carried them, so its stamp is
    // current. A page that a surface change left stale takes no runs, since
    // the whole fill a later move owes it carries them.
    let mut redecorated = Vec::new();
    if !hold {
        let ahead = pages_ahead(window, last_target, scroll_offset, region_height);
        for held in &mut entry.pages {
            if ahead.contains(&held.page)
                && held.decorations != versions.decorations
                && held.surface == versions.cells
            {
                held.decorations = versions.decorations;
                redecorated.push(held.page);
            }
        }
        redecorated.sort_unstable();
    }

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

    Refill {
        entered,
        redecorated,
    }
}

/// Request a fill for every page in `window` not already requested, record `window`
/// as the requested range, and return the newly-entered page indices in ascending
/// order.
///
/// Pages already covered by the previous window are not re-pushed, so a sub-page
/// scroll that does not change the window enters no pages and a one-page step enters
/// only the single page at the edge.
///
/// `compare` holds the pages a composite reads this call. With it, a covered
/// page whose `stamp` cells differ from the ones its fill carried enters again.
/// A covered page whose `stamp` surface differs enters again only when
/// `compare` holds it, so a page no composite reads keeps its older fill. With
/// `None`, every covered page stays as it is. An entered page records its
/// `stamp`.
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
    compare: Option<&Range<u64>>,
    stamp: &impl Fn(u64) -> PageStamp,
    render_page: &mut impl FnMut(u64) -> Vec<u8>,
) -> Vec<u64> {
    let already = entry.requested.clone().unwrap_or(0..0);
    let mut entered = Vec::new();
    for index in window.clone() {
        let buffered = already.contains(&index);
        if buffered && compare.is_none() {
            continue;
        }
        let fresh = stamp(index);
        let composited = compare.is_some_and(|pages| pages.contains(&index));
        let current = |held: &PageStamp| {
            held.page == index
                && held.cells == fresh.cells
                && (held.surface == fresh.surface || !composited)
        };
        if buffered && entry.pages.iter().any(current) {
            continue;
        }

        entered.push(index);
        let bytes = render_page(index);
        if !bytes.is_empty() {
            encode_fill_scope(out, pool, index, |out| out.extend_from_slice(&bytes));
        }
        entry.pages.retain(|held| held.page != index);
        entry.pages.push(fresh);
    }
    entry.pages.retain(|held| window.contains(&held.page));
    entry.requested = Some(window);
    entered
}

/// The pages of `window` that a move of the scroll target from `from` to `to`
/// composites or heads toward.
///
/// The terminal composites every row it eases through between the two
/// targets, and a moving composite reads `region_height + 1` rows from its
/// floored top row. The range therefore starts at the pages under those rows
/// at `from` and runs to the window's edge in the direction of the move. A
/// move without a direction, or a first target, covers the whole window.
///
/// A terminal that still trails an earlier target reads the page behind the
/// range until it catches up, within a frame or two. A change that arrives
/// while the pool moves therefore shows that page's older decorations for
/// those frames. A jump from a still view starts with the terminal on the last
/// target, so a page glide's change has no such page.
fn pages_ahead(window: Range<u64>, from: Option<f32>, to: f32, region_height: u64) -> Range<u64> {
    let Some(from) = from else {
        return window;
    };

    let top = from.floor() as u64;
    if to > from {
        top / region_height..window.end
    } else if to < from {
        window.start..(top + region_height) / region_height + 1
    } else {
        window
    }
}

/// The pages a move of the scroll target from `from` to `to` composites.
///
/// The terminal eases through every row between the two targets, and a
/// composite at each reads `region_height + 1` rows from its floored top row.
/// So the range runs from the page under the lower target's top row to the
/// page under the row past the higher target's last row, the straddle row.
fn pages_composited(from: f32, to: f32, region_height: u64) -> Range<u64> {
    let low = from.min(to).floor() as u64;
    let high = from.max(to).floor() as u64;
    low / region_height..(high + region_height) / region_height + 1
}

/// The half-open page window centered on `page`, clamped at the content start
/// and at `last_page`, the last page a composite reads.
///
/// Centering leaves pages buffered on both sides of the visible page so an ease
/// lagging behind a jump stays covered in either direction. A window clamped at
/// the end shifts back, so it still holds as many pages as the content has.
fn window_range(page: u64, last_page: u64) -> Range<u64> {
    let start = page.saturating_sub(WINDOW_PAGES / 2);
    let end = (start + WINDOW_PAGES).min(last_page.saturating_add(1));
    let start = end.saturating_sub(WINDOW_PAGES).min(start);
    start..end
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
        emit_into, emit_pages_into, scroll_target, window_range, MinimapWindowInputs, PageVersions,
        Refill, SmoothScrollState, WINDOW_PAGES,
    };
    use stoatty_protocol::command::{
        decode, Command, MinimapViewCommand, PoolDropCommand, PoolKind, PoolRegionCommand,
        RepositionCommand, ScrollCommand,
    };

    fn region(pool: u32, height: u16) -> PoolRegionCommand {
        PoolRegionCommand {
            pool,
            top: 1,
            left: 2,
            width: 76,
            height,
            window: 0,
            kind: PoolKind::Grid,
            dim: 0,
            resident: false,
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
    fn window_centers_and_clamps_at_both_ends() {
        assert_eq!(
            [
                window_range(0, u64::MAX),
                window_range(1, u64::MAX),
                window_range(10, u64::MAX),
                window_range(4, 5),
                window_range(5, 5),
                window_range(0, 1),
            ],
            [0..5, 0..5, 8..13, 1..6, 1..6, 0..2],
        );
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

        // A sub-page move applies the deferred bump and refills the pages its
        // composite reads, the visible page and the straddle page after it.
        let moved = emit_into(&mut out, &mut state, region(1, 20), 41.0, 1, true, |_| {
            Vec::new()
        });
        assert_eq!(moved, vec![2, 3]);
    }

    /// Emit pool 1 over 20-row pages at `offset` and `version`, holding at
    /// rest and filling asynchronously, and return the pages it fills.
    fn emit_held(state: &mut SmoothScrollState, offset: f32, version: u64) -> Vec<u64> {
        emit_into(
            &mut Vec::new(),
            state,
            region(1, 20),
            offset,
            version,
            true,
            |_| Vec::new(),
        )
    }

    /// A change that a move releases refills only the pages that move
    /// composites, and the next move finds them current.
    #[test]
    fn a_change_while_scrolling_refills_only_the_pages_the_move_composites() {
        let mut state = SmoothScrollState::default();
        emit_held(&mut state, 40.0, 0);
        emit_held(&mut state, 40.0, 0);
        assert_eq!(
            (
                emit_held(&mut state, 41.0, 1),
                emit_held(&mut state, 42.0, 1),
            ),
            (vec![2, 3], Vec::new()),
            "a move inside page 2 composites pages 2 and 3",
        );
    }

    /// Page 4 stays stale behind a move inside page 2, and refills once the
    /// glide to page 5 composites it, beside the pages the window gains.
    #[test]
    fn a_page_left_stale_refills_when_a_move_composites_it() {
        let mut state = SmoothScrollState::default();
        for (offset, version) in [(40.0, 0), (40.0, 0), (41.0, 1), (42.0, 1)] {
            emit_held(&mut state, offset, version);
        }
        assert_eq!(emit_held(&mut state, 100.0, 1), vec![4, 5, 6, 7]);
    }

    /// A far jump that carries a new version still re-anchors the terminal
    /// near its destination, so the glide does not ease across the gap.
    #[test]
    fn a_far_jump_that_releases_a_held_change_repositions() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();
        for _ in 0..2 {
            emit_into(&mut out, &mut state, region(1, 20), 0.0, 0, true, |_| {
                Vec::new()
            });
        }

        out.clear();
        emit_into(&mut out, &mut state, region(1, 20), 170.0, 1, true, |_| {
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
        );
    }

    /// The window stops at the page past the content's last row, the last
    /// page a composite reads, so a surface's end costs no blank page.
    #[test]
    fn the_window_stops_at_the_last_page_a_composite_reads() {
        let entered = |content_rows: u64, offset: f32| {
            emit_pages_into(
                &mut Vec::new(),
                &mut SmoothScrollState::default(),
                region(1, 20),
                offset,
                content_rows,
                PageVersions::default(),
                |_| 0,
                false,
                |_| Vec::new(),
            )
            .entered
        };
        assert_eq!(
            (entered(100, 80.0), entered(10, 0.0)),
            (vec![1, 2, 3, 4, 5], vec![0, 1]),
        );
    }

    /// Emit pool 1 at `offset` with decoration version `decorations`, holding at
    /// rest and filling asynchronously, the way an editor pane emits.
    fn emit_decorated(state: &mut SmoothScrollState, offset: f32, decorations: u64) -> Refill {
        let versions = PageVersions {
            cells: 0,
            decorations,
        };
        emit_pages_into(
            &mut Vec::new(),
            state,
            region(1, 20),
            offset,
            u64::MAX,
            versions,
            |_| 0,
            true,
            |_| Vec::new(),
        )
    }

    #[test]
    fn a_resting_decoration_change_waits_for_the_target_to_move() {
        let mut state = SmoothScrollState::default();
        emit_decorated(&mut state, 40.0, 0);

        assert_eq!(
            emit_decorated(&mut state, 40.0, 1),
            Refill::default(),
            "a resting pool composites nothing, so the new runs can wait"
        );
        // The rest kept the window 0..5 the terminal holds, so the move enters
        // nothing. It moves down from page 2, so pages 0 and 1 above it keep
        // their old runs.
        assert_eq!(
            emit_decorated(&mut state, 41.0, 1),
            Refill {
                entered: vec![],
                redecorated: vec![2, 3, 4],
            }
        );
    }

    /// A decoration change goes to the buffered pages a move composites or
    /// heads toward, from the pages under the last target onward in the
    /// direction the target moves. A page behind the move keeps its old
    /// decorations until a move heads back toward it, and a page that took the
    /// new ones takes nothing more.
    #[test]
    fn a_decoration_change_redecorates_only_the_pages_a_move_heads_toward() {
        let mut state = SmoothScrollState::default();
        let refills: Vec<(Vec<u64>, Vec<u64>)> = [
            (100.0, 0),
            (101.0, 1),
            (100.0, 2),
            (101.0, 2),
            (140.0, 2),
            (139.0, 3),
            (138.0, 4),
            (141.0, 5),
            (142.0, 5),
        ]
        .into_iter()
        .map(|(offset, decorations)| {
            let refill = emit_decorated(&mut state, offset, decorations);
            (refill.entered, refill.redecorated)
        })
        .collect();

        assert_eq!(
            refills,
            [
                ((3..8).collect(), vec![]),
                (vec![], vec![5, 6, 7]),
                (vec![], vec![3, 4, 5, 6]),
                (vec![], vec![7]),
                (vec![8, 9], vec![]),
                (vec![4], vec![5, 6, 7, 8]),
                (vec![], vec![4, 5, 6, 7]),
                (vec![9], vec![6, 7, 8]),
                (vec![], vec![]),
            ],
            "(entered, redecorated) per move of 20-row pages, down from page 5, \
             up past its straddle row, down to the page left behind, a step to \
             page 7, up across a page edge, up again, down from page 6, and on",
        );
    }

    #[test]
    fn moving_the_target_after_a_rest_enters_only_the_pages_the_window_gains() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();

        emit_into(&mut out, &mut state, region(1, 20), 40.0, 0, true, |_| {
            Vec::new()
        });
        // Resting keeps the window 0..5 the terminal holds.
        emit_into(&mut out, &mut state, region(1, 20), 40.0, 0, true, |_| {
            Vec::new()
        });

        // Stepping to page 3 moves the window to 1..6, which gains only page 5.
        let moved = emit_into(&mut out, &mut state, region(1, 20), 60.0, 0, true, |_| {
            Vec::new()
        });
        assert_eq!(moved, vec![5]);
    }

    /// A composite reads a row past the region, so a pool resting between two
    /// rows reads into the page after the visible one. A new rectangle drops
    /// the pages the terminal held, and the rest asks for both again.
    #[test]
    fn a_fractional_rest_refills_its_straddle_page_after_a_region_change() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();

        // The first display, then a rest, at 40.5 rows.
        for _ in 0..2 {
            emit_into(&mut out, &mut state, region(1, 20), 40.5, 0, true, |_| {
                Vec::new()
            });
        }

        // 40.5 rows over a 22-row region stands in page 1 and reads into page 2.
        let entered = emit_into(&mut out, &mut state, region(1, 22), 40.5, 0, true, |_| {
            Vec::new()
        });
        assert_eq!(entered, vec![1, 2]);
    }

    /// A rest between two rows keeps compositing, so a change there refills
    /// the two pages the composite reads, and a rest with no change refills
    /// nothing.
    #[test]
    fn a_change_at_a_fractional_rest_refills_the_composited_pages() {
        let mut state = SmoothScrollState::default();
        let mut emit = |version: u64| {
            emit_into(
                &mut Vec::new(),
                &mut state,
                region(1, 20),
                40.5,
                version,
                true,
                |_| Vec::new(),
            )
        };

        // The first display, then a rest, at 40.5 rows.
        emit(0);
        emit(0);

        // 40.5 rows over a 20-row region stands in page 2 and reads into page 3.
        assert_eq!((emit(1), emit(1)), (vec![2, 3], Vec::new()));
    }

    /// A page whose own cells version moves refills alone once the target
    /// moves, and the rest of the window keeps the pages the terminal holds.
    #[test]
    fn a_page_version_change_refills_only_that_page() {
        let mut state = SmoothScrollState::default();
        // Page 3's own version is `lit`, and every other page's is 0.
        let emit = |state: &mut SmoothScrollState, offset: f32, lit: u64| {
            emit_pages_into(
                &mut Vec::new(),
                state,
                region(1, 20),
                offset,
                u64::MAX,
                PageVersions::default(),
                |page| if page == 3 { lit } else { 0 },
                true,
                |_| Vec::new(),
            )
            .entered
        };

        assert_eq!(
            emit(&mut state, 40.0, 1),
            (0..WINDOW_PAGES).collect::<Vec<_>>(),
            "the first display fills the window",
        );
        assert_eq!(
            emit(&mut state, 40.0, 2),
            Vec::<u64>::new(),
            "a held emit compares nothing",
        );
        assert_eq!(
            emit(&mut state, 41.0, 2),
            vec![3],
            "the moving emit refills page 3 alone",
        );
        assert_eq!(
            emit(&mut state, 42.0, 2),
            Vec::<u64>::new(),
            "and page 3 holds its refilled version after",
        );
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

    /// A resize hands a pool a new rectangle per event, and each one drops the
    /// slots the page before it went into. A resting pool therefore renders
    /// nothing while the burst lasts.
    #[test]
    fn a_region_change_under_unsettled_geometry_defers_the_fill() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();

        emit_into(&mut out, &mut state, region(1, 20), 40.0, 0, true, |_| {
            Vec::new()
        });

        state.set_geometry_unsettled(true);
        let entered = emit_into(&mut out, &mut state, region(1, 22), 40.0, 0, true, |_| {
            Vec::new()
        });
        assert!(entered.is_empty(), "a moved rectangle enters no page");

        // The burst is still in flight, but this pool's rectangle held still,
        // so its slots survive and the page it owes goes out (offset 40 /
        // height 22 = page 1).
        let entered = emit_into(&mut out, &mut state, region(1, 22), 40.0, 0, true, |_| {
            Vec::new()
        });
        assert_eq!(
            entered,
            vec![1],
            "a rectangle that held still fills its page"
        );
    }

    /// A pool resting between two rows waits out a resize burst as a whole-row
    /// rest does, since the burst throws away the pages its composite reads.
    #[test]
    fn a_fractional_rest_under_unsettled_geometry_defers_the_fill() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();
        emit_into(&mut out, &mut state, region(1, 20), 40.5, 0, true, |_| {
            Vec::new()
        });

        state.set_geometry_unsettled(true);
        let entered = emit_into(&mut out, &mut state, region(1, 22), 40.5, 0, true, |_| {
            Vec::new()
        });
        assert_eq!(
            entered,
            Vec::<u64>::new(),
            "a moved rectangle enters no page"
        );
    }

    /// A picker's box grows with its match count, so a keystroke moves its
    /// rectangle as surely as a resize does. That rectangle has settled by the
    /// time the page draws, and blanking the list until the settle window
    /// closes would cost more than the page it saves.
    #[test]
    fn a_region_change_under_settled_geometry_fills_at_once() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();

        emit_into(&mut out, &mut state, region(1, 20), 0.0, 7, true, |_| {
            Vec::new()
        });

        let entered = emit_into(&mut out, &mut state, region(1, 22), 0.0, 8, true, |_| {
            Vec::new()
        });
        assert_eq!(entered, vec![0], "the moved rectangle fills its page");
    }

    /// Unsettled geometry holds back the page a moved rectangle would fill, not
    /// the pages a glide is about to composite.
    #[test]
    fn unsettled_geometry_still_refills_a_scrolling_pool() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();

        emit_into(&mut out, &mut state, region(1, 20), 0.0, 0, true, |_| {
            Vec::new()
        });

        state.set_geometry_unsettled(true);
        let entered = emit_into(&mut out, &mut state, region(1, 22), 40.0, 0, true, |_| {
            Vec::new()
        });
        assert_eq!(
            entered,
            (0..WINDOW_PAGES).collect::<Vec<_>>(),
            "a moving target refills its whole window"
        );
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

    /// A recorded version marks the buffered pages current, so an emit at it
    /// sends nothing and keeps every page. A later version still refills the
    /// page a composite reads.
    #[test]
    fn an_emit_at_a_recorded_version_sends_nothing() {
        let mut state = SmoothScrollState::default();
        let page = |_| b"page".to_vec();
        emit_into(
            &mut Vec::new(),
            &mut state,
            region(1, 20),
            0.0,
            0,
            false,
            page,
        );

        state.record_content_version(1, 7);
        let mut recorded = Vec::new();
        let held = emit_into(
            &mut recorded,
            &mut state,
            region(1, 20),
            0.0,
            7,
            false,
            page,
        );
        let moved = emit_into(
            &mut Vec::new(),
            &mut state,
            region(1, 20),
            0.0,
            8,
            false,
            page,
        );

        assert_eq!(
            (held, recorded, moved),
            (vec![], vec![], vec![0]),
            "the recorded version sends nothing, and the next one refills the shown page",
        );
    }

    #[test]
    fn without_hold_a_resting_content_change_refills_the_visible_page() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();

        emit_into(&mut out, &mut state, region(1, 20), 40.0, 0, false, |_| {
            Vec::new()
        });

        // A pool passing hold_when_idle false refills the page it shows on a
        // content change even while the target is stationary. The pages around
        // it wait for a composite to read them.
        let entered = emit_into(&mut out, &mut state, region(1, 20), 40.0, 1, false, |_| {
            Vec::new()
        });
        assert_eq!(entered, vec![2]);
    }

    /// A page counts as held while it sits in the buffered window at the
    /// content version of the pool's last emit.
    #[test]
    fn holds_page_answers_the_buffered_pages_at_the_current_content() {
        let mut state = SmoothScrollState::default();
        let held = |state: &SmoothScrollState| {
            (0..6)
                .map(|page| state.holds_page(1, page))
                .collect::<Vec<_>>()
        };
        emit_into(
            &mut Vec::new(),
            &mut state,
            region(1, 20),
            40.0,
            0,
            false,
            |_| Vec::new(),
        );
        let filled = held(&state);
        emit_into(
            &mut Vec::new(),
            &mut state,
            region(1, 20),
            40.0,
            1,
            false,
            |_| Vec::new(),
        );

        // The change at rest refills page 2, under the composite, and leaves
        // the rest of the window stale.
        assert_eq!(
            (filled, held(&state), state.holds_page(2, 2)),
            (
                vec![true, true, true, true, true, false],
                vec![false, false, true, false, false, false],
                false
            ),
        );
    }

    /// A resident pool composites at a rest on a whole row, so a content change
    /// there refills every page the rest shows.
    #[test]
    fn a_resting_resident_pool_refills_every_page_its_rest_shows() {
        let changed_at_rest = |offset: f32| {
            let mut state = SmoothScrollState::default();
            let resident = PoolRegionCommand {
                resident: true,
                ..region(1, 20)
            };
            let mut emit = |version| {
                emit_into(
                    &mut Vec::new(),
                    &mut state,
                    resident,
                    offset,
                    version,
                    false,
                    |_| Vec::new(),
                )
            };
            emit(0);
            emit(1)
        };

        // A top row of 45 shows rows 45 to 64 of the 20-row pages, on pages 2
        // and 3. A top row of 40 starts page 2 and shows no row past it.
        assert_eq!(
            (changed_at_rest(45.0), changed_at_rest(40.0)),
            (vec![2, 3], vec![2])
        );
    }

    /// A resident grid pool whose rectangle a resize burst moves fills nothing
    /// until the geometry settles, gliding or not, and its region counts as
    /// unfilled meanwhile. The settled emit fills the window a first display
    /// there fills. A terminal pool fills at once, since the live grid under it
    /// is blank.
    #[test]
    fn a_resident_grid_pool_waits_out_a_burst_while_it_glides() {
        let grid = PoolRegionCommand {
            resident: true,
            ..region(1, 20)
        };
        let terminal = PoolRegionCommand {
            kind: PoolKind::Terminal,
            ..grid
        };
        let moved = |region: PoolRegionCommand| PoolRegionCommand {
            width: 70,
            ..region
        };
        let emit = |state: &mut SmoothScrollState, region, offset| {
            emit_into(&mut Vec::new(), state, region, offset, 0, false, |_| {
                Vec::new()
            })
        };
        let burst = |region| {
            let mut state = SmoothScrollState::default();
            emit(&mut state, region, 0.0);
            state.set_geometry_unsettled(true);
            let entered = emit(&mut state, moved(region), 30.0);
            (state, entered)
        };

        let (mut state, held) = burst(grid);
        let during: Vec<_> = state.filled_regions().collect();
        state.set_geometry_unsettled(false);
        let settled = emit(&mut state, moved(grid), 30.0);
        let first = emit(&mut SmoothScrollState::default(), moved(grid), 30.0);
        assert_eq!(
            (
                held,
                during,
                settled,
                state.filled_regions().collect::<Vec<_>>(),
                burst(terminal).1,
            ),
            (vec![], vec![], first.clone(), vec![moved(grid)], first),
        );
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
    fn a_content_bump_at_rest_refills_the_visible_page() {
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
        let rescrolled = commands(&out)
            .iter()
            .any(|c| matches!(c, Command::Scroll(_)));
        assert_eq!(
            (refilled, rescrolled),
            (vec![0], false),
            "a content bump at rest refills the page it shows and keeps the target"
        );
    }

    #[test]
    fn a_decoration_change_redecorates_the_buffered_window() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();
        let first = PageVersions {
            cells: 1,
            decorations: 1,
        };
        emit_pages_into(
            &mut out,
            &mut state,
            region(1, 20),
            0.0,
            u64::MAX,
            first,
            |_| 0,
            true,
            |_| b"page".to_vec(),
        );

        out.clear();
        let moved = PageVersions {
            decorations: 2,
            ..first
        };
        let refill = emit_pages_into(
            &mut out,
            &mut state,
            region(1, 20),
            1.0,
            u64::MAX,
            moved,
            |_| 0,
            true,
            |_| panic!("a decoration change must not refill a page"),
        );

        assert_eq!(
            refill,
            Refill {
                entered: Vec::new(),
                redecorated: (0..WINDOW_PAGES).collect(),
            }
        );
        assert!(
            !commands(&out).iter().any(|c| matches!(c, Command::Fill(_))),
            "no fill frame for pages that keep their cells"
        );

        let again = emit_pages_into(
            &mut out,
            &mut state,
            region(1, 20),
            2.0,
            u64::MAX,
            moved,
            |_| 0,
            true,
            |_| panic!("an unchanged version must not refill a page"),
        );
        assert_eq!(again, Refill::default(), "the new runs went out once");
    }

    #[test]
    fn a_cell_change_refills_and_redecorates_nothing() {
        let mut state = SmoothScrollState::default();
        let first = PageVersions {
            cells: 1,
            decorations: 1,
        };
        emit_pages_into(
            &mut Vec::new(),
            &mut state,
            region(1, 20),
            0.0,
            u64::MAX,
            first,
            |_| 0,
            true,
            |_| Vec::new(),
        );

        let both = PageVersions {
            cells: 2,
            decorations: 2,
        };
        let refill = emit_pages_into(
            &mut Vec::new(),
            &mut state,
            region(1, 20),
            1.0,
            u64::MAX,
            both,
            |_| 0,
            true,
            |_| Vec::new(),
        );

        assert_eq!(
            refill,
            Refill {
                entered: vec![0, 1],
                redecorated: Vec::new(),
            },
            "the composited pages refill with the new decorations, and the stale \
             pages take no runs"
        );
    }

    /// A page that a cells change left stale takes no decorations-only fill,
    /// since the whole fill a later composite owes it carries the new runs.
    #[test]
    fn a_stale_page_takes_no_decorations_only_fill() {
        let mut state = SmoothScrollState::default();
        let mut emit = |offset: f32, cells: u64, decorations: u64| {
            emit_pages_into(
                &mut Vec::new(),
                &mut state,
                region(1, 20),
                offset,
                u64::MAX,
                PageVersions { cells, decorations },
                |_| 0,
                true,
                |_| Vec::new(),
            )
        };
        emit(0.0, 1, 1);
        assert_eq!(
            (emit(1.0, 2, 2), emit(2.0, 2, 3)),
            (
                Refill {
                    entered: vec![0, 1],
                    redecorated: Vec::new(),
                },
                Refill {
                    entered: Vec::new(),
                    redecorated: vec![0, 1],
                },
            ),
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

    /// A focus move changes a terminal pool's dim alone, which the terminal
    /// applies to the pages it holds, so the region goes out again and no page
    /// refills.
    #[test]
    fn a_dim_only_region_change_redeclares_without_a_refill() {
        let terminal = |dim| PoolRegionCommand {
            kind: PoolKind::Terminal,
            dim,
            resident: true,
            ..region(1, 20)
        };
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();
        emit_into(&mut out, &mut state, terminal(0), 0.0, 0, false, |_| {
            Vec::new()
        });

        out.clear();
        let mut refilled = Vec::new();
        emit_into(&mut out, &mut state, terminal(64), 0.0, 0, false, |page| {
            refilled.push(page);
            Vec::new()
        });

        assert_eq!(
            (
                refilled,
                commands(&out),
                state.holds_region(1, &terminal(0))
            ),
            (vec![], vec![Command::PoolRegion(terminal(64))], true),
            "the region goes out again with no fill, and the pool keeps its placement",
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

    /// An id that passes to other content goes with the thumbs it fed, and
    /// with nothing else, so the next emit declares the pool again and places
    /// only those thumbs again.
    #[test]
    fn drop_pool_retires_the_pool_and_the_thumbs_it_fed() {
        fn place_thumbs(state: &mut SmoothScrollState, out: &mut Vec<u8>) {
            let inputs = MinimapWindowInputs::new(0.0, 7, 20);
            state.emit_minimap_view(out, 5, 1, inputs, || (0.0, 20));
            state.emit_minimap_view(out, 6, 2, inputs, || (0.0, 20));
        }

        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();
        emit_into(&mut out, &mut state, region(1, 20), 0.0, 7, false, |_| {
            Vec::new()
        });
        place_thumbs(&mut state, &mut out);

        out.clear();
        state.drop_pool(&mut out, 1);
        state.drop_pool(&mut out, 3);
        assert_eq!(
            commands(&out),
            vec![Command::PoolDrop(PoolDropCommand { pool: 1 })],
            "the tracked pool drops, and the untracked one sends nothing"
        );
        assert!(!state.already_emitted(1, 7), "pool 1 is forgotten");

        out.clear();
        place_thumbs(&mut state, &mut out);
        assert_eq!(
            commands(&out),
            vec![Command::MinimapView(MinimapViewCommand {
                strip_id: 5,
                top_256: 0,
                visible_lines: 20,
            })],
            "the thumb of pool 1 is placed again, and the thumb of pool 2 stays"
        );
    }

    /// A program whose terminal was driven by another program in between comes
    /// back to a terminal that holds none of these pools, so every one has to
    /// go and every surface has to re-declare.
    #[test]
    fn drop_all_retires_every_pool() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();
        emit_into(&mut out, &mut state, region(1, 20), 0.0, 7, false, |_| {
            Vec::new()
        });
        emit_into(&mut out, &mut state, region(2, 20), 0.0, 7, false, |_| {
            Vec::new()
        });

        out.clear();
        state.drop_all(&mut out);
        assert_eq!(
            commands(&out),
            vec![
                Command::PoolDrop(PoolDropCommand { pool: 1 }),
                Command::PoolDrop(PoolDropCommand { pool: 2 }),
            ]
        );
        assert!(!state.already_emitted(1, 7), "pool 1 is forgotten");
        assert!(!state.already_emitted(2, 7), "pool 2 is forgotten");
    }
}

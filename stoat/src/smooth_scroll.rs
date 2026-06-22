//! Drives stoatty's region-scoped smooth-scroll pools for the visible editor
//! panes.
//!
//! When stoat runs inside stoatty (detected by the `STOATTY` env var) each
//! visible editor pane is handed to the terminal's recycled page pool as its own
//! pool: stoat declares the pane's on-screen rectangle as that pool's region,
//! renders the document a page at a time into off-grid pool slots, and reports an
//! absolute scroll target each time the pane scrolls. The terminal eases each
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
//! Everything here degrades to nothing outside stoatty: the APC frames are
//! ignored by other terminals, and the emit path is gated on the detected flag so
//! a non-stoatty run pushes no bytes at all.
//!
//! A "page" is one region-sized screen of the document: `region.height` rows of
//! `region.width` columns, the page at index `p` starting at document row
//! `p * region.height`. Each pool is addressed by this page index, the same key
//! [`ScrollCommand::page`] and [`FillCommand::index`] carry.

use crate::{editor_state::EditorState, render::editor::render_editor};
use ratatui::{buffer::Buffer, layout::Rect, style::Style};
use std::{collections::BTreeMap, ops::Range};
use stoatty_protocol::command::{
    encode_fill_end_into, encode_fill_into, encode_pool_drop_into, encode_pool_region_into,
    encode_scroll_into, PoolRegionCommand, ScrollCommand,
};

/// Pages kept buffered around each pool's visible page, the pool's working
/// window. Wide enough that the visible page and its straddle neighbour (when a
/// fractional scroll shows the bottom of one page and the top of the next) are
/// always present, plus slack so an in-flight ease never outruns the filled
/// slots.
const WINDOW_PAGES: u64 = 5;

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
    /// Half-open page range `[start, end)` currently filled into the pool. `None`
    /// until the first fill.
    filled: Option<Range<u64>>,
    /// `scroll_row` the most recent [`ScrollCommand`] was computed from. Skips
    /// re-emitting an unchanged scroll target.
    last_scroll_row: Option<u32>,
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
}

/// Append the smooth-scroll APC frames for one pool's current scroll position to
/// `out`, updating `state` to reflect what was emitted.
///
/// `region` is the pane's body rectangle in absolute grid cells, carrying the
/// pool id ([`PoolRegionCommand::pool`]) the pool is tracked under. `scroll_row`
/// is the editor's top visible document row. The closure `render_page` paints
/// page `index` (document rows `index * region.height ..`) into a region-sized
/// [`Buffer`] and returns its self-contained VT bytes.
///
/// Emits, in order: a `pool_region` frame when the rectangle changed; a
/// `fill`/page-VT/`fill_end` triple for each page newly entering the buffered
/// window; then a `scroll` frame when the target moved. A frame that needs none
/// of these appends nothing.
pub(crate) fn emit_into(
    out: &mut Vec<u8>,
    state: &mut SmoothScrollState,
    region: PoolRegionCommand,
    scroll_row: u32,
    mut render_page: impl FnMut(u64) -> Vec<u8>,
) {
    let pool = region.pool;
    let entry = state.pools.entry(pool).or_default();

    if entry.region != Some(region) {
        encode_pool_region_into(out, &region);
        entry.region = Some(region);
        // A fresh region invalidates the pool's slot contents; force a refill.
        entry.filled = None;
        entry.last_scroll_row = None;
    }

    let region_height = region.height.max(1) as u64;
    let page = scroll_row as u64 / region_height;
    let window = window_range(page);

    refill(out, entry, pool, window, &mut render_page);

    if entry.last_scroll_row != Some(scroll_row) {
        encode_scroll_into(out, &scroll_target(pool, scroll_row, region.height));
        entry.last_scroll_row = Some(scroll_row);
    }
}

/// Fill every page in `window` not already present in `entry.filled`, then record
/// `window` as the filled range.
///
/// Pages already covered by the previous window are not re-pushed, so a sub-page
/// scroll that does not change the window emits no fills and a one-page step
/// pushes only the single page entering at the edge.
fn refill(
    out: &mut Vec<u8>,
    entry: &mut PoolEmitState,
    pool: u32,
    window: Range<u64>,
    render_page: &mut impl FnMut(u64) -> Vec<u8>,
) {
    let already = entry.filled.clone().unwrap_or(0..0);
    for index in window.clone() {
        if already.contains(&index) {
            continue;
        }
        encode_fill_into(out, pool, index);
        out.extend_from_slice(&render_page(index));
        encode_fill_end_into(out);
    }
    entry.filled = Some(window);
}

/// The half-open page window centered on `page`, clamped at the document start.
///
/// Centering leaves pages buffered on both sides of the visible page so an ease
/// lagging behind a jump stays covered in either direction.
fn window_range(page: u64) -> Range<u64> {
    let start = page.saturating_sub(WINDOW_PAGES / 2);
    start..start + WINDOW_PAGES
}

/// Map a top visible document row to pool `pool`'s scroll target: a page index
/// plus a sub-page fraction in 1/65536ths of a page.
///
/// `region_height` is the pool region's row count, the rows per page. The page
/// is the integer number of full regions scrolled past; the fraction is how far
/// the partial region into the next page the row sits.
fn scroll_target(pool: u32, scroll_row: u32, region_height: u16) -> ScrollCommand {
    let height = region_height.max(1) as u64;
    let row = scroll_row as u64;
    let page = row / height;
    let within = row % height;
    let fraction = (within * 65536 / height) as u16;
    ScrollCommand {
        pool,
        page,
        fraction,
    }
}

/// Render `region_height` document rows of `editor` starting at row
/// `page * region_height` into a fresh region-sized [`Buffer`], returning the
/// page's self-contained VT byte stream.
///
/// Drives the editor through the same [`render_editor`] path the live frame
/// uses, so a pooled page matches what the editor would paint at that scroll
/// position. The editor's `scroll_row` is saved and restored around the render,
/// so this leaves the live scroll position unchanged; the render is unfocused so
/// no cursor or selection is painted into the pooled page.
pub(crate) fn render_editor_page(
    editor: &mut EditorState,
    page: u64,
    fallback_style: Style,
    theme: &crate::theme::Theme,
    region_width: u16,
    region_height: u16,
) -> Vec<u8> {
    let area = Rect::new(0, 0, region_width, region_height);
    let mut buf = Buffer::empty(area);

    let saved_scroll = editor.scroll_row;
    editor.scroll_row = page
        .saturating_mul(region_height as u64)
        .min(u32::MAX as u64) as u32;
    render_editor(editor, area, fallback_style, theme, &mut buf, false);
    editor.scroll_row = saved_scroll;

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
fn serialize_buffer(buf: &Buffer) -> Vec<u8> {
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
    use stoatty_protocol::command::{
        decode, Command, PoolDropCommand, PoolRegionCommand, ScrollCommand,
    };

    fn region(pool: u32, height: u16) -> PoolRegionCommand {
        PoolRegionCommand {
            pool,
            top: 1,
            left: 2,
            width: 76,
            height,
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
            scroll_target(7, 0, 20),
            ScrollCommand {
                pool: 7,
                page: 0,
                fraction: 0
            }
        );
        assert_eq!(
            scroll_target(7, 20, 20),
            ScrollCommand {
                pool: 7,
                page: 1,
                fraction: 0
            }
        );
        assert_eq!(
            scroll_target(7, 30, 20),
            ScrollCommand {
                pool: 7,
                page: 1,
                fraction: 32768
            }
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
        emit_into(&mut out, &mut state, region(1, 20), 0, |page| {
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
    fn unchanged_scroll_emits_nothing_after_first() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();
        emit_into(&mut out, &mut state, region(1, 20), 5, |_| Vec::new());

        out.clear();
        emit_into(&mut out, &mut state, region(1, 20), 5, |_| {
            panic!("no page should be re-filled")
        });
        assert!(out.is_empty(), "stable frame emitted {} bytes", out.len());
    }

    #[test]
    fn sub_page_scroll_reuses_window_and_emits_only_scroll() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();
        emit_into(&mut out, &mut state, region(1, 20), 0, |_| Vec::new());

        out.clear();
        let mut refilled = Vec::new();
        emit_into(&mut out, &mut state, region(1, 20), 3, |page| {
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
    fn region_change_forces_refill() {
        let mut state = SmoothScrollState::default();
        let mut out = Vec::new();
        emit_into(&mut out, &mut state, region(1, 20), 0, |_| Vec::new());

        out.clear();
        let mut refilled = Vec::new();
        emit_into(&mut out, &mut state, region(1, 22), 0, |page| {
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
        emit_into(&mut out, &mut state, region(1, 20), 0, |_| Vec::new());
        emit_into(&mut out, &mut state, region(2, 20), 40, |_| Vec::new());

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
        emit_into(&mut out, &mut state, region(1, 20), 0, |_| Vec::new());
        emit_into(&mut out, &mut state, region(2, 20), 0, |_| Vec::new());

        out.clear();
        state.drop_absent(&mut out, &[1]);
        assert_eq!(
            commands(&out),
            vec![Command::PoolDrop(PoolDropCommand { pool: 2 })]
        );

        // Pool 2 is forgotten, so re-emitting it re-declares its region.
        out.clear();
        emit_into(&mut out, &mut state, region(2, 20), 0, |_| Vec::new());
        assert!(commands(&out).contains(&Command::PoolRegion(region(2, 20))));
    }
}

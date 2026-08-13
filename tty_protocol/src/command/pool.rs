//! The pool commands drive the smooth-scroll document pools.
//!
//! A pool declares the rectangle it composites into, fills its pages, and reports
//! its scroll position and cursor. The terminal owns the easing between reported
//! positions, so a program scrolls smoothly without drawing every frame.

use crate::frame;

/// Declare the sub-rectangle a smooth-scroll document pool composites into.
///
/// The pool is `width` by `height` cells with its top-left at (`top`, `left`) in
/// absolute grid coordinates. Unlike [`ScrollRegionCommand`] it carries no
/// offset: the pool's scroll position rides [`ScrollCommand`] (page plus
/// fraction). The renderer composites the eased pool over this rectangle and
/// draws the rest of the grid -- any static chrome around it -- from the live
/// content, so a program need not own the whole viewport to smooth-scroll.
///
/// `pool` names which pool this declares. Pools scroll independently and
/// composite in ascending-id z-order, so a program can smooth-scroll several
/// regions at once (split panes side by side, a modal stacked over an editor).
/// Re-declaring an existing id updates that pool's rectangle;
/// [`Command::PoolDrop`] retires it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PoolRegionCommand {
    pub pool: u32,
    pub top: u16,
    pub left: u16,
    pub width: u16,
    pub height: u16,
    /// Which OS window the pool renders into. `0` is the primary grid, where the
    /// region's coordinates are grid-absolute. A nonzero `N` binds the pool to
    /// aux window `N`, where the coordinates are relative to that window's own
    /// grid.
    pub window: u32,
}

/// First pool id reserved for non-pane surfaces. Split-pane editor pools
/// occupy `[1, NON_PANE_POOL_BASE)`.
///
/// The two id ranges also encode a z-relationship the renderer relies on when
/// it composites pools against modal boxes. A pool below the base is
/// editor-pane content that sits *under* every box, so its eased composite is
/// occluded by any box it slides beneath. A pool at or above the base is a
/// box's own content, such as a finder or palette list easing, so it is never
/// occluded.
///
/// Shared here because the pool producer (stoat) and the compositor (stoatty)
/// must agree on the split.
pub const NON_PANE_POOL_BASE: u32 = 1 << 24;

/// Name the pool and document page a [`Command::Fill`] redirect paints into.
///
/// The open half of the `fill`/`fill_end` marker pair. A page is a full grid of
/// cells, far larger than the APC frame cap, so it cannot ride a frame payload:
/// this marker only names the target page, and the page's content streams as
/// ordinary VT + SGR bytes after the frame, committed when the redirect closes.
/// `pool` selects which pool's buffer receives the page; `index` is the app's
/// document page index, the same key the pool slot is addressed by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FillCommand {
    pub pool: u32,
    pub index: u64,
}

/// A smooth-scroll target as a document-page offset.
///
/// Names where the program wants pool [`Self::pool`]'s viewport: `page` is the
/// document page index (the same key the page pool is addressed by) and
/// `fraction` is the sub-page position within it, in 1/65536ths of a page. The
/// renderer eases the live offset toward this position rather than jumping, so
/// the program reports an absolute target and the terminal animates toward it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScrollCommand {
    pub pool: u32,
    pub page: u64,
    pub fraction: u16,
}

/// A pool's primary-cursor anchor, tracked for the duration of a glide.
///
/// While pool [`Self::pool`] eases toward a scroll target, the cursor rides the
/// content rather than the VT grid. `row` is the document display row the cursor
/// sits on, and `col` is its grid-absolute column. The terminal draws the cursor
/// at region row `row` minus the pool's eased document offset, hiding it while
/// that row lands outside the region, instead of easing the cursor toward its
/// last VT cell.
///
/// Pairs with [`ScrollCommand`], which moves the same pool's viewport. The anchor
/// ships once per glide tick, so the drawn cursor stays frame-locked to the eased
/// content offset.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PoolCursorCommand {
    pub pool: u32,
    pub row: u64,
    pub col: u16,
}

/// A discontinuous smooth-scroll jump to a document page.
///
/// `page` is the destination document page index in pool [`Self::pool`]. Unlike
/// [`ScrollCommand`], which the terminal eases toward across the buffered window,
/// this re-anchors the live offset to a local neighbour of the destination and
/// lands softly on it, so a jump too far to animate within the pool does not drag
/// across the unbuffered gap. The program pushes a window of pages around the
/// destination before sending it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RepositionCommand {
    pub pool: u32,
    pub page: u64,
}

/// Retire smooth-scroll pool [`Self::pool`], freeing the pages it buffered.
///
/// The payload of [`Command::PoolDrop`]: a single pool id. Sent when the surface
/// backing the pool goes away, so the terminal need not hold its buffers for a
/// pool that will never scroll again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PoolDropCommand {
    pub pool: u32,
}

/// Encode a [`PoolRegionCommand`] as a full `Gstoatty;pool_region` frame for an
/// emitter.
pub fn encode_pool_region(command: &PoolRegionCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_pool_region_into(&mut out, command);
    out
}

/// Append a `Gstoatty;pool_region` frame for `command` to `out` without
/// allocating.
pub fn encode_pool_region_into(out: &mut Vec<u8>, command: &PoolRegionCommand) {
    frame::begin(out, "pool_region");
    frame::push_arg(out, |w| {
        w.write_all(&command.pool.to_be_bytes())?;
        w.write_all(&command.top.to_be_bytes())?;
        w.write_all(&command.left.to_be_bytes())?;
        w.write_all(&command.width.to_be_bytes())?;
        w.write_all(&command.height.to_be_bytes())?;
        w.write_all(&command.window.to_be_bytes())
    });
    frame::end(out);
}

/// Encode a [`FillCommand`] as a full `Gstoatty;fill` open-marker frame.
///
/// The page index rides in a single fixed 8-byte big-endian argument; the
/// page's content streams as VT bytes after the frame, not as a frame argument.
pub fn encode_fill(command: &FillCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_fill_into(&mut out, command.pool, command.index);
    out
}

/// Append a `Gstoatty;fill` open-marker frame for page `index` of pool `pool`
/// to `out`.
pub fn encode_fill_into(out: &mut Vec<u8>, pool: u32, index: u64) {
    frame::begin(out, "fill");
    frame::push_arg(out, |w| {
        w.write_all(&pool.to_be_bytes())?;
        w.write_all(&index.to_be_bytes())
    });
    frame::end(out);
}

/// Append a whole fill batch for page `index` of pool `pool` to `out`, being
/// the open marker, whatever `page` writes, then the close marker.
///
/// Prefer this to the two markers by hand. An unclosed `fill` redirects every
/// byte that follows onto the page-painting context instead of the live screen,
/// and stays redirected until the next `fill` or `reset`, so the screen stops
/// updating and the symptom surfaces far from the emitter that dropped the
/// marker. A scope has no way to leave one out.
///
/// `page` writes the page's VT bytes straight into `out`, which is what a
/// serializer painting into the caller's buffer wants. Reach for
/// [`encode_fill_into`] and [`encode_fill_end_into`] only for content that
/// arrives across several writes rather than in one call.
pub fn encode_fill_scope(
    out: &mut Vec<u8>,
    pool: u32,
    index: u64,
    page: impl FnOnce(&mut Vec<u8>),
) {
    encode_fill_into(out, pool, index);
    page(out);
    encode_fill_end_into(out);
}

/// The `(pool, index)` a batch of bytes fills, or `None` when it is not a fill.
///
/// A fill batch is self-contained, holding the open marker, the page's VT
/// bytes, and the close marker. Only the marker names the page, so this decodes
/// the first frame and stops, never walking the page content behind it.
///
/// The point of naming it is that pool slots are last-writer-wins and no other
/// command reads a page's content, so a queued fill for a key that appears
/// again later is work nobody will ever see. A sender with a backlog can drop
/// the earlier one. `None` covers everything that has no such guarantee, from
/// a different command to bytes that do not open with a frame at all.
pub fn fill_batch_key(batch: &[u8]) -> Option<(u32, u64)> {
    let end = frame::first_frame_end(batch)?;
    let frame = frame::decode(&batch[..end])?;
    match frame.sub.as_str() {
        "fill" => decode_fill(&frame.args).map(|fill| (fill.pool, fill.index)),
        _ => None,
    }
}

/// Encode a [`Command::FillEnd`] as a full `Gstoatty;fill_end` close-marker
/// frame.
///
/// The frame carries no arguments; receiving it commits the page painted since
/// the matching [`Command::Fill`] onto its pool slot and restores the live grid.
pub fn encode_fill_end() -> Vec<u8> {
    let mut out = Vec::new();
    encode_fill_end_into(&mut out);
    out
}

/// Append an argument-less `Gstoatty;fill_end` close-marker frame to `out`.
pub fn encode_fill_end_into(out: &mut Vec<u8>) {
    frame::begin(out, "fill_end");
    frame::end(out);
}

/// Encode a [`ScrollCommand`] as a full `Gstoatty;scroll` frame for an emitter.
///
/// The page and sub-page fraction ride in a single fixed 10-byte argument.
pub fn encode_scroll(command: &ScrollCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_scroll_into(&mut out, command);
    out
}

/// Append a `Gstoatty;scroll` frame for `command` to `out` without allocating.
pub fn encode_scroll_into(out: &mut Vec<u8>, command: &ScrollCommand) {
    frame::begin(out, "scroll");
    frame::push_arg(out, |w| {
        w.write_all(&command.pool.to_be_bytes())?;
        w.write_all(&command.page.to_be_bytes())?;
        w.write_all(&command.fraction.to_be_bytes())
    });
    frame::end(out);
}

/// Encode a [`PoolCursorCommand`] as a full `Gstoatty;pool_cursor` frame.
///
/// The cursor anchor rides one fixed 14-byte big-endian argument holding the
/// pool, row, and column, the same shape as [`encode_scroll`]'s target.
pub fn encode_pool_cursor(command: &PoolCursorCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_pool_cursor_into(&mut out, command);
    out
}

/// Append a `Gstoatty;pool_cursor` frame for `command` to `out` without allocating.
pub fn encode_pool_cursor_into(out: &mut Vec<u8>, command: &PoolCursorCommand) {
    frame::begin(out, "pool_cursor");
    frame::push_arg(out, |w| {
        w.write_all(&command.pool.to_be_bytes())?;
        w.write_all(&command.row.to_be_bytes())?;
        w.write_all(&command.col.to_be_bytes())
    });
    frame::end(out);
}

/// Encode a [`RepositionCommand`] as a full `Gstoatty;reposition` frame.
///
/// The destination page index rides in a single fixed 8-byte big-endian
/// argument, the same shape as [`encode_fill`]'s page index.
pub fn encode_reposition(command: &RepositionCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_reposition_into(&mut out, command.pool, command.page);
    out
}

/// Append a `Gstoatty;reposition` frame for destination `page` of pool `pool`
/// to `out`.
pub fn encode_reposition_into(out: &mut Vec<u8>, pool: u32, page: u64) {
    frame::begin(out, "reposition");
    frame::push_arg(out, |w| {
        w.write_all(&pool.to_be_bytes())?;
        w.write_all(&page.to_be_bytes())
    });
    frame::end(out);
}

/// Encode a [`PoolDropCommand`] as a full `Gstoatty;pool_drop` frame for an
/// emitter.
pub fn encode_pool_drop(command: &PoolDropCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_pool_drop_into(&mut out, command.pool);
    out
}

/// Append a `Gstoatty;pool_drop` frame retiring pool `pool` to `out`.
pub fn encode_pool_drop_into(out: &mut Vec<u8>, pool: u32) {
    frame::begin(out, "pool_drop");
    frame::push_arg(out, |w| w.write_all(&pool.to_be_bytes()));
    frame::end(out);
}

pub(super) fn decode_pool_region(args: &[Vec<u8>]) -> Option<PoolRegionCommand> {
    let arg: &[u8; 16] = args.first()?.get(..16)?.try_into().ok()?;

    Some(PoolRegionCommand {
        pool: u32::from_be_bytes([arg[0], arg[1], arg[2], arg[3]]),
        top: u16::from_be_bytes([arg[4], arg[5]]),
        left: u16::from_be_bytes([arg[6], arg[7]]),
        width: u16::from_be_bytes([arg[8], arg[9]]),
        height: u16::from_be_bytes([arg[10], arg[11]]),
        window: u32::from_be_bytes([arg[12], arg[13], arg[14], arg[15]]),
    })
}

pub(super) fn decode_fill(args: &[Vec<u8>]) -> Option<FillCommand> {
    let arg: &[u8; 12] = args.first()?.get(..12)?.try_into().ok()?;

    Some(FillCommand {
        pool: u32::from_be_bytes([arg[0], arg[1], arg[2], arg[3]]),
        index: u64::from_be_bytes([
            arg[4], arg[5], arg[6], arg[7], arg[8], arg[9], arg[10], arg[11],
        ]),
    })
}

pub(super) fn decode_scroll(args: &[Vec<u8>]) -> Option<ScrollCommand> {
    let arg: &[u8; 14] = args.first()?.get(..14)?.try_into().ok()?;

    Some(ScrollCommand {
        pool: u32::from_be_bytes([arg[0], arg[1], arg[2], arg[3]]),
        page: u64::from_be_bytes([
            arg[4], arg[5], arg[6], arg[7], arg[8], arg[9], arg[10], arg[11],
        ]),
        fraction: u16::from_be_bytes([arg[12], arg[13]]),
    })
}

pub(super) fn decode_pool_cursor(args: &[Vec<u8>]) -> Option<PoolCursorCommand> {
    let arg: &[u8; 14] = args.first()?.get(..14)?.try_into().ok()?;

    Some(PoolCursorCommand {
        pool: u32::from_be_bytes([arg[0], arg[1], arg[2], arg[3]]),
        row: u64::from_be_bytes([
            arg[4], arg[5], arg[6], arg[7], arg[8], arg[9], arg[10], arg[11],
        ]),
        col: u16::from_be_bytes([arg[12], arg[13]]),
    })
}

pub(super) fn decode_reposition(args: &[Vec<u8>]) -> Option<RepositionCommand> {
    let arg: &[u8; 12] = args.first()?.get(..12)?.try_into().ok()?;

    Some(RepositionCommand {
        pool: u32::from_be_bytes([arg[0], arg[1], arg[2], arg[3]]),
        page: u64::from_be_bytes([
            arg[4], arg[5], arg[6], arg[7], arg[8], arg[9], arg[10], arg[11],
        ]),
    })
}

pub(super) fn decode_pool_drop(args: &[Vec<u8>]) -> Option<PoolDropCommand> {
    let arg: &[u8; 4] = args.first()?.get(..4)?.try_into().ok()?;

    Some(PoolDropCommand {
        pool: u32::from_be_bytes(*arg),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{decode, decode_stream, Command};

    #[test]
    fn pool_region_round_trips() {
        let command = PoolRegionCommand {
            pool: 4,
            top: 1,
            left: 2,
            width: 76,
            height: 22,
            window: 2,
        };

        assert_eq!(
            decode(&encode_pool_region(&command)),
            Some(Command::PoolRegion(command))
        );
    }

    #[test]
    fn rejects_wrong_length_pool_region_payload() {
        // The single arg here decodes to 3 bytes, not the 16 a pool region needs.
        assert!(decode(b"Gstoatty;pool_region;YWJj").is_none());
    }

    #[test]
    fn fill_round_trips() {
        let command = FillCommand {
            pool: 9,
            index: 4_000_000_000,
        };

        assert_eq!(decode(&encode_fill(&command)), Some(Command::Fill(command)));
    }

    #[test]
    fn fill_end_round_trips() {
        assert_eq!(decode(&encode_fill_end()), Some(Command::FillEnd));
    }

    /// The scope's whole point. An unclosed fill redirects the screen until the
    /// next marker, so the close has to be unskippable rather than remembered.
    #[test]
    fn a_fill_scope_wraps_its_page_in_both_markers() {
        let fill = FillCommand { pool: 9, index: 41 };
        let page = b"\x1b[Hpage";
        let mut out = Vec::new();
        encode_fill_scope(&mut out, fill.pool, fill.index, |bytes| {
            bytes.extend_from_slice(page)
        });

        let mut expected = encode_fill(&fill);
        expected.extend_from_slice(page);
        expected.extend(encode_fill_end());
        assert_eq!(out, expected, "open marker, page bytes, close marker");
        assert_eq!(
            decode_stream(&out),
            vec![Command::Fill(fill), Command::FillEnd]
        );
    }

    /// The page a batch fills is read from its opening marker alone.
    ///
    /// A sender with a backlog uses this to drop a fill a later one replaces,
    /// so answering for anything it cannot prove is a fill would drop bytes
    /// that were not safe to lose.
    #[test]
    fn a_batch_is_keyed_only_when_it_opens_with_a_fill() {
        let mut fill = Vec::new();
        encode_fill_into(&mut fill, 3, 41);
        // The page's own bytes, which the key must not walk into. The trailing
        // frame is a whole marker of its own and must not be read either.
        fill.extend_from_slice(b"\x1b[1;1Hpainted rows\x1b[0m");
        encode_fill_end_into(&mut fill);

        assert_eq!(fill_batch_key(&fill), Some((3, 41)));
        assert_eq!(
            fill_batch_key(&encode_fill_end()),
            None,
            "a batch that opens with another command names no page",
        );
        assert_eq!(
            fill_batch_key(b"\x1b[1;1Hjust vt bytes"),
            None,
            "and neither does one that opens with no frame at all",
        );

        let mut truncated = Vec::new();
        encode_fill_into(&mut truncated, 3, 41);
        truncated.truncate(truncated.len() - 1);
        assert_eq!(
            fill_batch_key(&truncated),
            None,
            "an unterminated marker is not a page this can name",
        );
    }

    #[test]
    fn rejects_wrong_length_fill_payload() {
        // The single arg here decodes to 3 bytes, not the 12 a fill index needs.
        assert!(decode(b"Gstoatty;fill;YWJj").is_none());
    }

    #[test]
    fn scroll_round_trips() {
        let command = ScrollCommand {
            pool: 3,
            page: 5_000_000_000,
            fraction: 40_000,
        };

        assert_eq!(
            decode(&encode_scroll(&command)),
            Some(Command::Scroll(command))
        );
    }

    #[test]
    fn rejects_wrong_length_scroll_payload() {
        // The single arg here decodes to 3 bytes, not the 14 a scroll offset needs.
        assert!(decode(b"Gstoatty;scroll;YWJj").is_none());
    }

    #[test]
    fn pool_cursor_round_trips() {
        let command = PoolCursorCommand {
            pool: 3,
            row: 5_000_000_000,
            col: 40_000,
        };

        assert_eq!(
            decode(&encode_pool_cursor(&command)),
            Some(Command::PoolCursor(command))
        );
    }

    #[test]
    fn rejects_wrong_length_pool_cursor_payload() {
        // The single arg here decodes to 3 bytes, not the 14 a cursor anchor needs.
        assert!(decode(b"Gstoatty;pool_cursor;YWJj").is_none());
    }

    #[test]
    fn reposition_round_trips() {
        let command = RepositionCommand {
            pool: 2,
            page: 6_000_000_000,
        };

        assert_eq!(
            decode(&encode_reposition(&command)),
            Some(Command::Reposition(command))
        );
    }

    #[test]
    fn rejects_wrong_length_reposition_payload() {
        // The single arg here decodes to 3 bytes, not the 12 a page index needs.
        assert!(decode(b"Gstoatty;reposition;YWJj").is_none());
    }

    #[test]
    fn pool_drop_round_trips() {
        let command = PoolDropCommand { pool: 7 };

        assert_eq!(
            decode(&encode_pool_drop(&command)),
            Some(Command::PoolDrop(command))
        );
    }

    #[test]
    fn rejects_wrong_length_pool_drop_payload() {
        // The single arg here decodes to 3 bytes, not the 4 a pool id needs.
        assert!(decode(b"Gstoatty;pool_drop;YWJj").is_none());
    }
}

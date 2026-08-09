//! What an unfocused editor pane's paint depends on, gathered so a frame can
//! ask whether it may replay the last one.
//!
//! Every visible pane is repainted in full on each redraw, and an unfocused one
//! additionally pays an O(cells) dim blend. Background activity therefore
//! repaints panes whose inputs did not move, at whatever rate that activity
//! ticks. Replaying the previous paint instead needs a key that is equal only
//! when the paint would be identical, which is what [`PaneCacheKey`] is.
//!
//! The set comes from reading `render_pane` and `render_editor_with_overlay`
//! for the unfocused editor case. `is_focused` reaches exactly two places in
//! the editor render, relative line numbering and an early return before the
//! selection and cursor pass. So an unfocused pane's *content* reads no
//! selections, no cursor, and not the editor mode. Its *status row* does read
//! the primary cursor and the buffer's dirty flag, which is why both are here.

use crate::{buffer::BufferId, display_map::PaintVersion};
use ratatui::layout::Rect;

/// Everything an unfocused editor pane's paint reads that can change between
/// two frames.
///
/// Two frames whose keys are equal paint identical cells, scene bytes, and
/// undercurl spans, which is the direction a replay needs. The converse is
/// weaker, since several parts move on work that changed no visible cell.
///
/// Nothing here is ordered. A key is only ever compared for equality, and
/// "later" would have no meaning across parts that count different things.
// Built by the replay path, which lands next. Kept apart from it so the audit
// this encodes is reviewable on its own.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PaneCacheKey {
    /// Which buffer, and how far its text has moved. The paint version does not
    /// carry this, because a display map answers only for the mapping it
    /// applies to whatever text it was handed.
    pub(crate) buffer: BufferId,
    pub(crate) buffer_version: u64,
    /// The five display layers and the display map's own settings, composed.
    /// Covers the wrap width too, since the width is installed on the map
    /// before the snapshot the pane paints from is taken.
    pub(crate) paint_version: PaintVersion,
    /// Which row the viewport starts at. Not visible to any layer version: the
    /// same text scrolled by one row is the same snapshot.
    pub(crate) scroll_row: u32,
    /// Where the pane is and how big it is, which decides every geometry the
    /// paint derives.
    pub(crate) area: Rect,
    /// The pane's position in the layout, which is the id its minimap strip is
    /// published under.
    pub(crate) pane_index: usize,
    /// True when the pane's status row was widened to run under the
    /// single-minimap band. A layout change can move this without moving the
    /// pane's own rect.
    pub(crate) status_widened: bool,
    /// The theme, the settings the renderer reads directly, and the search
    /// query, none of which passes through a display layer.
    pub(crate) paint_generation: u64,
    /// The diagnostics the gutter and the underlines are drawn from.
    pub(crate) diagnostics_version: u64,
    /// The diff the gutter indicators are drawn from.
    pub(crate) diff_version: usize,
    /// The minimap content published for this pane's buffer, which the strip
    /// paints from and which no display version covers.
    pub(crate) minimap_content_id: u32,
    /// Whether a minimap strip is drawn at all.
    pub(crate) minimap_enabled: bool,
    /// The primary cursor's line and column, read by the status row alone. The
    /// content pass returns before anything that would read a selection.
    pub(crate) cursor_pos: Option<(u32, u32)>,
    /// The buffer's unsaved marker, read by the status row.
    pub(crate) dirty: bool,
}

#[cfg(test)]
mod tests {
    use super::PaneCacheKey;
    use crate::{buffer::BufferId, display_map::PaintVersion};
    use ratatui::layout::Rect;

    fn key(paint_version: PaintVersion) -> PaneCacheKey {
        PaneCacheKey {
            buffer: BufferId::new(0),
            buffer_version: 1,
            paint_version,
            scroll_row: 0,
            area: Rect::new(0, 0, 40, 10),
            pane_index: 0,
            status_widened: false,
            paint_generation: 0,
            diagnostics_version: 0,
            diff_version: 0,
            minimap_content_id: 0,
            minimap_enabled: false,
            cursor_pos: Some((1, 1)),
            dirty: false,
        }
    }

    /// Every field distinguishes two keys, so none of them is along for the
    /// ride.
    ///
    /// A field that compared equal however it moved would be a paint input the
    /// key silently ignores, which is the failure that shows up as a pane
    /// frozen on stale content rather than as a test going red.
    #[test]
    fn each_field_tells_two_keys_apart() {
        let base = key(PaintVersion::default());
        let moved: [(&str, PaneCacheKey); 13] = [
            (
                "buffer",
                PaneCacheKey {
                    buffer: BufferId::new(1),
                    ..base
                },
            ),
            (
                "buffer_version",
                PaneCacheKey {
                    buffer_version: 2,
                    ..base
                },
            ),
            (
                "scroll_row",
                PaneCacheKey {
                    scroll_row: 1,
                    ..base
                },
            ),
            (
                "area",
                PaneCacheKey {
                    area: Rect::new(0, 0, 41, 10),
                    ..base
                },
            ),
            (
                "pane_index",
                PaneCacheKey {
                    pane_index: 1,
                    ..base
                },
            ),
            (
                "status_widened",
                PaneCacheKey {
                    status_widened: true,
                    ..base
                },
            ),
            (
                "paint_generation",
                PaneCacheKey {
                    paint_generation: 1,
                    ..base
                },
            ),
            (
                "diagnostics_version",
                PaneCacheKey {
                    diagnostics_version: 1,
                    ..base
                },
            ),
            (
                "diff_version",
                PaneCacheKey {
                    diff_version: 1,
                    ..base
                },
            ),
            (
                "minimap_content_id",
                PaneCacheKey {
                    minimap_content_id: 1,
                    ..base
                },
            ),
            (
                "minimap_enabled",
                PaneCacheKey {
                    minimap_enabled: true,
                    ..base
                },
            ),
            (
                "cursor_pos",
                PaneCacheKey {
                    cursor_pos: Some((2, 1)),
                    ..base
                },
            ),
            (
                "dirty",
                PaneCacheKey {
                    dirty: true,
                    ..base
                },
            ),
        ];

        for (field, other) in moved {
            assert_ne!(base, other, "{field} has to tell two keys apart");
        }

        assert_eq!(base, key(PaintVersion::default()), "and equal parts agree");
    }
}

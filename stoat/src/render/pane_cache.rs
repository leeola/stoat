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

use crate::{
    buffer::BufferId,
    buffer_registry::BufferRegistry,
    display_map::PaintVersion,
    editor_state::{EditorId, EditorState},
    pane::{Pane, View},
    render::{
        editor::editor_cursor_position, pane::pane_areas, undercurl::UndercurlBatch, FrameCtx,
    },
};
use ratatui::{
    buffer::{Buffer, Cell},
    layout::Rect,
};
use slotmap::SlotMap;
use stoat_config::{LineNumbers, WrapMode};
use stoat_widgets::ApcScene;

/// Everything an unfocused editor pane's paint reads that can change between
/// two frames.
///
/// Two frames whose keys are equal paint identical cells, scene bytes, and
/// undercurl spans, which is the direction a replay needs. The converse is
/// weaker, since several parts move on work that changed no visible cell.
///
/// Nothing here is ordered. A key is only ever compared for equality, and
/// "later" would have no meaning across parts that count different things.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PaneCacheKey {
    /// Which buffer, and how far its text has moved. The paint version does not
    /// carry this, because a display map answers only for the mapping it
    /// applies to whatever text it was handed.
    pub(crate) buffer: BufferId,
    pub(crate) buffer_version: u64,
    /// The five display layers and the display map's own settings, composed.
    pub(crate) paint_version: PaintVersion,
    /// What the pane resolves its wrap width from, before installing it on the
    /// display map.
    ///
    /// The install does move the paint version, so this looks redundant. It is
    /// not, because the key is built before the paint that would install it, so
    /// without these a width change would be answered one frame late.
    pub(crate) wrap: (Option<WrapMode>, WrapMode, u32),
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
    /// The theme and the search query, neither of which passes through a
    /// display layer.
    pub(crate) paint_generation: u64,
    /// The dim an unfocused pane's cells and minimap palette are blended by,
    /// as bits so the key stays comparable.
    ///
    /// Read straight off the settings rather than through the config install,
    /// so a value written into the field directly moves no generation. The same
    /// goes for the line numbering below. Anything the frame reads off settings
    /// without a version of its own belongs here rather than trusting the
    /// generation to have seen it.
    pub(crate) inactive_dim_bits: u32,
    pub(crate) line_numbers: LineNumbers,
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

/// One unfocused pane's last paint, held so a frame whose key still matches can
/// put it back instead of producing it again.
///
/// A pane paints into three channels and all three are reset per frame, so all
/// three are captured. Cells alone would replay the grid and lose the rich
/// gutter, the minimap strip, and the undercurls that ride beside it.
pub(crate) struct PaneCacheEntry {
    pub(crate) key: PaneCacheKey,
    /// The rectangles the pane painted, each with its cells row-major.
    ///
    /// Usually one, the pane's own rect. A pane whose status row runs on under
    /// the single-minimap band paints past its rect on that row alone, and
    /// taking the bounding box instead would capture and replay cells belonging
    /// to whatever sits to its right on every other row.
    regions: Vec<(Rect, Vec<Cell>)>,
    /// The pane's own slice of the frame's scene, which is contiguous because a
    /// pane paints in one go.
    scene: Vec<u8>,
    /// The spans as the pane pushed them. Their cell records are deliberately
    /// not kept, since those are filled from the finished buffer after every
    /// pane has painted.
    undercurls: Vec<(u16, u16, u16, [u8; 3])>,
}

/// The scene length and span count to hand [`PaneCacheEntry::capture`] once the
/// pane has painted.
#[derive(Clone, Copy)]
pub(crate) struct PaintMark {
    scene_len: usize,
    spans: usize,
}

impl PaintMark {
    /// Where the frame's two append-only channels stood before a pane painted.
    pub(crate) fn take(scene: &ApcScene, undercurls: &UndercurlBatch) -> Self {
        Self {
            scene_len: scene.bytes().len(),
            spans: undercurls.spans().len(),
        }
    }
}

impl PaneCacheEntry {
    /// Record what the pane just painted, given where the channels stood before
    /// it started.
    pub(crate) fn capture(
        key: PaneCacheKey,
        rects: &[Rect],
        buf: &Buffer,
        scene: &ApcScene,
        undercurls: &UndercurlBatch,
        mark: PaintMark,
    ) -> Self {
        Self {
            key,
            regions: rects
                .iter()
                .filter(|rect| rect.width > 0 && rect.height > 0)
                .map(|&rect| (rect, cells_in(buf, rect)))
                .collect(),
            scene: scene.bytes()[mark.scene_len..].to_vec(),
            undercurls: undercurls.spans()[mark.spans..]
                .iter()
                .map(|span| (span.x, span.y, span.len, span.color))
                .collect(),
        }
    }

    /// Put the recorded paint back into this frame's three channels.
    ///
    /// The scene append lands nowhere while the scene is dead, which is the
    /// same place the original paint's did, so a session without a listener
    /// replays exactly what it painted.
    pub(crate) fn replay(
        &self,
        buf: &mut Buffer,
        scene: &mut ApcScene,
        undercurls: &mut UndercurlBatch,
    ) {
        for (rect, cells) in &self.regions {
            let mut cells = cells.iter();
            for y in rect.top()..rect.bottom() {
                for x in rect.left()..rect.right() {
                    if let Some(cell) = cells.next() {
                        buf[(x, y)] = cell.clone();
                    }
                }
            }
        }
        scene.buffer().extend_from_slice(&self.scene);
        for &(x, y, len, color) in &self.undercurls {
            undercurls.push(x, y, len, color);
        }
    }
}

/// The key for `pane`, or `None` when the pane is not one this cache covers.
///
/// Only an editor pane showing an ordinary document qualifies. The review,
/// diff, and conflict views take their own branches out of the editor render
/// and read state this never audited, so they always paint.
pub(crate) fn pane_cache_key(
    pane: &Pane,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    frame: FrameCtx<'_>,
    paint_generation: u64,
) -> Option<PaneCacheKey> {
    let View::Editor(editor_id) = &pane.view else {
        return None;
    };
    let editor = editors.get_mut(*editor_id)?;
    if editor.review_view.is_some() || editor.diff_view || editor.conflict_view.is_some() {
        return None;
    }

    let buffer = editor.buffer_id;
    let (dirty, diff_version) = {
        let shared = buffers.get(buffer)?;
        let guard = shared.read().ok()?;
        (
            guard.dirty,
            guard.diff_map.as_ref().map_or(0, |dm| dm.version()),
        )
    };

    let (_, status) = pane_areas(pane.area, frame.minimap_band);
    let minimap_content_id = frame
        .minimap_chrome
        .as_ref()
        .and_then(|chrome| chrome.content.get(&(chrome.workspace, buffer)))
        .map_or(0, |content| content.content_id());

    Some(PaneCacheKey {
        buffer,
        buffer_version: editor.display_map.buffer_snapshot().version(),
        paint_version: editor.display_map.snapshot().paint_version(),
        wrap: (editor.wrap_override, frame.wrap_mode, frame.wrap_column),
        scroll_row: editor.scroll_row,
        area: pane.area,
        pane_index: pane.index as usize,
        status_widened: status.width > pane.area.width,
        paint_generation,
        inactive_dim_bits: frame.inactive_dim.to_bits(),
        line_numbers: frame.line_numbers,
        diagnostics_version: frame.diagnostics.version(),
        diff_version,
        minimap_content_id,
        minimap_enabled: frame.minimap_enabled,
        cursor_pos: editor_cursor_position(editor),
        dirty,
    })
}

fn cells_in(buf: &Buffer, rect: Rect) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(rect.width as usize * rect.height as usize);
    for y in rect.top()..rect.bottom() {
        for x in rect.left()..rect.right() {
            cells.push(buf[(x, y)].clone());
        }
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::PaneCacheKey;
    use crate::{buffer::BufferId, display_map::PaintVersion};
    use ratatui::layout::Rect;
    use stoat_config::{LineNumbers, WrapMode};

    fn key(paint_version: PaintVersion) -> PaneCacheKey {
        PaneCacheKey {
            buffer: BufferId::new(0),
            buffer_version: 1,
            paint_version,
            scroll_row: 0,
            wrap: (None, WrapMode::EditorWidth, 100),
            area: Rect::new(0, 0, 40, 10),
            pane_index: 0,
            status_widened: false,
            paint_generation: 0,
            inactive_dim_bits: 0,
            line_numbers: LineNumbers::Off,
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
        let moved: [(&str, PaneCacheKey); 16] = [
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
                "wrap",
                PaneCacheKey {
                    wrap: (Some(WrapMode::None), WrapMode::EditorWidth, 100),
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
                "inactive_dim_bits",
                PaneCacheKey {
                    inactive_dim_bits: 1,
                    ..base
                },
            ),
            (
                "line_numbers",
                PaneCacheKey {
                    line_numbers: LineNumbers::Absolute,
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

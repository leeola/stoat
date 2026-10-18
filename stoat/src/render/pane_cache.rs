//! What an editor pane's paint depends on, gathered so a frame decides whether
//! to replay the last one.
//!
//! Every visible pane is repainted in full on each redraw, and an unfocused one
//! additionally pays an O(cells) dim blend. Background activity therefore
//! repaints panes whose inputs did not move, at whatever rate that activity
//! ticks. Replaying the previous paint instead needs a key that is equal only
//! when the paint would be identical, which is what [`PaneCacheKey`] is.
//!
//! The set comes from reading `render_pane` and `render_editor_with_overlay`.
//! `is_focused` reaches exactly two places in the editor render, relative line
//! numbering and an early return before the selection and cursor pass. So an
//! unfocused pane's *content* reads no selections, no cursor, and not the
//! editor mode. A focused pane's content reads all three, and the cell under
//! the mouse, so the key holds the focus, the selection set, the insert flag,
//! and the hover cell. The goto-word labels and the pop-out cards a focused pane
//! draws over its content are never replayed.
//!
//! The *status row* reads the primary cursor and the buffer's dirty flag, which
//! is why both are here. Focused, it reads far more, from the mode label to the
//! spinner glyph, so the key also holds a hash of the segments the row paints.
//! The row names what waits on a held buffer, which no key part tracks, so a
//! held buffer never replays.
//!
//! The diff view's branch returns before its selection pass as well. Past what
//! a plain pane reads, it reads the diff dials and the names on its sides bar.
//! No version moves with either, so the key holds both.

use crate::{
    buffer::BufferId,
    buffer_registry::BufferRegistry,
    display_map::PaintVersion,
    editor_state::{EditorId, EditorState},
    pane::{Pane, View},
    render::{
        editor::editor_cursor_position,
        pane::{diff_sides_for, pane_areas, status_segments_area, StatusRow},
        review::DiffDials,
        undercurl::UndercurlBatch,
        FrameCtx,
    },
};
use lsp_types::MessageType;
use ratatui::{
    buffer::{Buffer, Cell},
    layout::Rect,
};
use slotmap::SlotMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use stoat_config::{LineNumbers, WrapMode};
use stoat_widgets::ApcScene;

/// Everything an editor pane's paint reads that changes from one frame to the
/// next.
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
    /// How much of the status row its segments hold, which a bottom-right
    /// badge over the row shortens. A badge that comes or goes moves nothing
    /// else here.
    pub(crate) status_segments_width: u16,
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
    /// The diff the gutter indicators are drawn from, and in the diff view both
    /// of its columns.
    pub(crate) diff_version: usize,
    /// Whether the editor shows the diff view, which paints the base beside the
    /// buffer and a sides bar over both.
    pub(crate) diff_view: bool,
    /// The diff view's dials, as the soften scale and the tint amount in bits,
    /// then the bold and the underline flags.
    ///
    /// A dial turn moves no generation, so the key holds the dials themselves.
    /// Outside the diff view nothing reads them, and they hold zeros there so a
    /// turn repaints no plain pane.
    pub(crate) diff_dials: (u32, u32, bool, bool),
    /// A hash of the two names the diff view's sides bar shows, zero outside the
    /// diff view.
    ///
    /// A switch of the diff base renames the sides with no generation of its
    /// own.
    pub(crate) diff_sides: u64,
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
    /// Whether the pane holds the focus. A focused editor numbers its lines
    /// from the cursor and paints its selections, which an unfocused one skips.
    pub(crate) focused: bool,
    /// The selection set the pane paints, by its identity. The cursor and
    /// every selection move with it.
    pub(crate) selections: u64,
    /// Whether a focused pane is in insert mode, where relative numbering
    /// gives way to absolute numbers. `false` for an unfocused pane, which
    /// numbers its lines absolutely in any mode.
    pub(crate) insert_mode: bool,
    /// The cell under the mouse, which a focused pane highlights. `None` for an
    /// unfocused pane, which paints no hover.
    pub(crate) hover_cell: Option<(u16, u16)>,
    /// Whether a live scene takes the rich gutter and the cursor, which a dead
    /// one leaves to the cells.
    pub(crate) scene_live: bool,
    /// A hash of the segments the status row paints, which every input the row
    /// reads passes through.
    pub(crate) status: u64,
    /// Whether the search query compiles under smart case, which decides the
    /// matches the pane paints. Read off the settings like the dim, so no
    /// generation moves with it.
    pub(crate) search_smart_case: bool,
}

/// One pane's last paint, held so a frame whose key still matches puts it
/// back instead of producing it again.
///
/// A pane paints into three channels and all three are reset per frame, so all
/// three are captured. Cells alone would replay the grid and lose the rich
/// gutter, the minimap strip, and the undercurls that ride beside it.
///
/// The storage is reused across captures rather than rebuilt, so a pane that
/// misses writes over what it held instead of allocating its rect again. The
/// render site therefore takes the entry out of the map before it knows
/// whether this frame captures anything, which is why `key` is `None` while no
/// capture fills it.
#[derive(Default)]
pub(crate) struct PaneCacheEntry {
    pub(crate) key: Option<PaneCacheKey>,
    /// The rectangles the pane painted, each with its cells row-major.
    ///
    /// Usually one, the pane's own rect. A pane whose status row runs on under
    /// the single-minimap band paints past its rect on that row alone, and
    /// taking the bounding box instead would capture and replay cells belonging
    /// to whatever sits to its right on every other row.
    regions: Vec<(Rect, Vec<Cell>)>,
    /// The pane's own slice of each of its two scene lanes, by lane id. Each
    /// slice is contiguous because a pane paints its lanes in one go.
    scene: [(u32, Vec<u8>); 2],
    /// The spans as the pane pushed them. Their cell records are deliberately
    /// not kept, since those are filled from the finished buffer after every
    /// pane has painted.
    undercurls: Vec<(u16, u16, u16, [u8; 3])>,
    /// The last key a frame built for the pane, and whether it differed from
    /// the key before it.
    ///
    /// Two moves in a row mark a run of frames that moves the key on every
    /// frame, such as a held motion key, where a capture never replays. See
    /// [`Self::capture_into`].
    seen: Option<PaneCacheKey>,
    moved: bool,
}

/// The scene lane lengths and span count to hand
/// [`PaneCacheEntry::capture_into`] once the pane has painted.
#[derive(Clone, Copy)]
pub(crate) struct PaintMark {
    /// Each lane the pane paints, with the lane's length before the paint.
    lanes: [(u32, usize); 2],
    spans: usize,
}

impl PaintMark {
    /// Where the frame's append-only channels stood before a pane painted into
    /// `lanes`.
    pub(crate) fn take(scene: &ApcScene, lanes: [u32; 2], undercurls: &UndercurlBatch) -> Self {
        Self {
            lanes: lanes.map(|id| (id, scene.lane_bytes(id).len())),
            spans: undercurls.spans().len(),
        }
    }
}

impl PaneCacheEntry {
    /// Record what the pane just painted, given where the channels stood before
    /// it started, and return whether it did.
    ///
    /// While the user holds a motion key or types, the focused pane's key moves
    /// on every frame. So does the key of every pane that shows the edited
    /// buffer. Such a pane repaints on each frame, and no frame replays the
    /// capture of the frame before it. A paint whose key moved on this frame
    /// and on the frame before it therefore skips the capture and drops what
    /// the entry held. A run of such frames pays for one capture, on its first
    /// frame. The first frame after the run whose key holds captures again.
    ///
    /// A move of the paint version alone is not a move. A paint moves its own
    /// paint version when it installs a wrap width or a diff pairing, so the
    /// frame after a width change moves that part with no input behind it.
    /// Every motion and keystroke moves some other part of the key.
    ///
    /// A capture writes over whatever this entry held, keeping its
    /// allocations. The render site passes the same rects in the same order
    /// every frame, so a region slot is matched by position and its held rect
    /// overwritten.
    pub(crate) fn capture_into(
        &mut self,
        key: PaneCacheKey,
        rects: &[Rect],
        buf: &Buffer,
        scene: &ApcScene,
        undercurls: &UndercurlBatch,
        mark: PaintMark,
    ) -> bool {
        let moved = self.seen.is_none_or(|seen| {
            seen != PaneCacheKey {
                paint_version: seen.paint_version,
                ..key
            }
        });
        let in_run = moved && self.moved;
        self.seen = Some(key);
        self.moved = moved;
        if in_run {
            self.key = None;
            return false;
        }

        self.key = Some(key);

        let painted = rects
            .iter()
            .filter(|rect| rect.width > 0 && rect.height > 0);
        let mut slots = 0;
        for &rect in painted {
            if slots == self.regions.len() {
                self.regions.push((rect, Vec::new()));
            }
            let (held, cells) = &mut self.regions[slots];
            *held = rect;
            cells_into(buf, rect, cells);
            slots += 1;
        }
        self.regions.truncate(slots);

        for (&(id, start), (held_id, bytes)) in mark.lanes.iter().zip(&mut self.scene) {
            *held_id = id;
            bytes.clear();
            bytes.extend_from_slice(&scene.lane_bytes(id)[start..]);
        }

        self.undercurls.clear();
        self.undercurls.extend(
            undercurls.spans()[mark.spans..]
                .iter()
                .map(|span| (span.x, span.y, span.len, span.color)),
        );
        true
    }

    /// Put the recorded paint back into this frame's three channels.
    ///
    /// Each scene slice goes back into the lane it came from, and the replay
    /// leaves lane 0 open, as a paint at the render site does.
    ///
    /// The scene append lands nowhere while the scene is dead, which is the
    /// same place the original paint's did, so a session without a listener
    /// replays exactly what it painted.
    pub(crate) fn replay(
        &mut self,
        buf: &mut Buffer,
        scene: &mut ApcScene,
        undercurls: &mut UndercurlBatch,
    ) {
        // The held key is the last one the pane built, so a replayed frame
        // held the key still, which ends any run of moving frames.
        self.moved = false;
        for (rect, cells) in &self.regions {
            let width = rect.width as usize;
            for (row, y) in (rect.top()..rect.bottom()).enumerate() {
                let start = buf.index_of(rect.left(), y);
                buf.content[start..start + width]
                    .clone_from_slice(&cells[row * width..(row + 1) * width]);
            }
        }
        for (id, bytes) in &self.scene {
            scene.set_lane(*id);
            scene.buffer().extend_from_slice(bytes);
        }
        scene.end_lane();
        for &(x, y, len, color) in &self.undercurls {
            undercurls.push(x, y, len, color);
        }
    }
}

/// The key for `pane`, or `None` when the pane is not one this cache covers.
///
/// An editor pane qualifies, focused or not, whether it shows a document or
/// the diff view. The conflict view takes its own branch out of the editor
/// render and reads state the key does not cover, so it always paints.
///
/// A buffer a command waits on always paints too. Its status row names what
/// waits, and that name changes when the first waiter goes while no part of
/// the key moves. A held buffer is rare and usually focused, so the paint it
/// costs is small.
///
/// A focused pane under goto-word labels, the LSP status card, or the LSP
/// error card always paints as well. The labels last a jump's few frames, and
/// the status card turns the spinner on every tick.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pane_cache_key(
    pane: &Pane,
    is_focused: bool,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    frame: FrameCtx<'_>,
    paint_generation: u64,
    scene_live: bool,
    status_row: &StatusRow,
) -> Option<PaneCacheKey> {
    let View::Editor(editor_id) = &pane.view else {
        return None;
    };
    let editor = editors.get_mut(*editor_id)?;
    if editor.conflict_view.is_some() {
        return None;
    }

    let buffer = editor.buffer_id;
    if frame.held_buffers.iter().any(|(held, _)| *held == buffer) {
        return None;
    }

    let error_card = frame
        .lsp_message
        .is_some_and(|(typ, _)| typ == MessageType::ERROR);
    if is_focused && (frame.goto_word_labels.is_some() || frame.lsp_status_open || error_card) {
        return None;
    }

    let (dirty, diff_version) = {
        let shared = buffers.get(buffer)?;
        let guard = shared.read().ok()?;
        (
            guard.dirty,
            guard.diff_map.as_ref().map_or(0, |dm| dm.version()),
        )
    };

    // The dials destructure whole, so a further dial fails to compile here
    // until the key holds it.
    let (diff_dials, diff_sides) = match editor.diff_view {
        true => {
            let DiffDials {
                soften_scale,
                tint_amount,
                bold,
                underline,
            } = frame.diff_dials;
            let mut sides = DefaultHasher::new();
            diff_sides_for(buffers.path_for(buffer), frame).hash(&mut sides);
            (
                (
                    soften_scale.to_bits(),
                    tint_amount.to_bits(),
                    bold,
                    underline,
                ),
                sides.finish(),
            )
        },
        false => ((0, 0, false, false), 0),
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
        status_segments_width: status_segments_area(status, frame.badge_cover).width,
        paint_generation,
        inactive_dim_bits: frame.inactive_dim.to_bits(),
        line_numbers: frame.line_numbers,
        diagnostics_version: buffers
            .path_for(buffer)
            .map_or(0, |path| frame.diagnostics.version_for(path)),
        diff_version,
        diff_view: editor.diff_view,
        diff_dials,
        diff_sides,
        minimap_content_id,
        minimap_enabled: frame.minimap_enabled,
        cursor_pos: editor_cursor_position(editor),
        dirty,
        focused: is_focused,
        selections: editor.selections.identity(),
        insert_mode: is_focused && frame.mode == "insert",
        hover_cell: frame.hover_cell.filter(|_| is_focused),
        scene_live,
        status: status_row.hash_value(),
        search_smart_case: frame.search_smart_case,
    })
}

/// Copy `rect`'s cells into `out`, row-major, reusing whatever it already
/// holds.
///
/// A rect's row is contiguous in the buffer, so each one is a single slice
/// copy rather than a cell at a time.
fn cells_into(buf: &Buffer, rect: Rect, out: &mut Vec<Cell>) {
    let width = rect.width as usize;
    // Resized rather than cleared and refilled, since every cell is written
    // below and a same-sized rect leaves the length alone.
    out.resize(width * rect.height as usize, Cell::EMPTY);
    for (row, y) in (rect.top()..rect.bottom()).enumerate() {
        let start = buf.index_of(rect.left(), y);
        out[row * width..(row + 1) * width].clone_from_slice(&buf.content[start..start + width]);
    }
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
            status_segments_width: 40,
            paint_generation: 0,
            inactive_dim_bits: 0,
            line_numbers: LineNumbers::Off,
            diagnostics_version: 0,
            diff_version: 0,
            diff_view: false,
            diff_dials: (0, 0, false, false),
            diff_sides: 0,
            minimap_content_id: 0,
            minimap_enabled: false,
            cursor_pos: Some((1, 1)),
            dirty: false,
            focused: false,
            selections: 0,
            insert_mode: false,
            hover_cell: None,
            scene_live: false,
            status: 0,
            search_smart_case: false,
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
        let moved: [(&str, PaneCacheKey); 27] = [
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
                "status_segments_width",
                PaneCacheKey {
                    status_segments_width: 29,
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
                "diff_view",
                PaneCacheKey {
                    diff_view: true,
                    ..base
                },
            ),
            (
                "diff_dials",
                PaneCacheKey {
                    diff_dials: (0, 0, false, true),
                    ..base
                },
            ),
            (
                "diff_sides",
                PaneCacheKey {
                    diff_sides: 1,
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
            (
                "focused",
                PaneCacheKey {
                    focused: true,
                    ..base
                },
            ),
            (
                "selections",
                PaneCacheKey {
                    selections: 1,
                    ..base
                },
            ),
            (
                "insert_mode",
                PaneCacheKey {
                    insert_mode: true,
                    ..base
                },
            ),
            (
                "hover_cell",
                PaneCacheKey {
                    hover_cell: Some((1, 1)),
                    ..base
                },
            ),
            (
                "scene_live",
                PaneCacheKey {
                    scene_live: true,
                    ..base
                },
            ),
            ("status", PaneCacheKey { status: 1, ..base }),
            (
                "search_smart_case",
                PaneCacheKey {
                    search_smart_case: true,
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

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
//! The *status row* is never replayed. It moves far more often than the
//! content does. A pending count, a status message, a waiter on a held buffer,
//! and each spinner tick move the row alone, and the spinner ticks ten times a
//! second while a server works. So the row paints on every frame, a replay
//! puts back the content alone, and nothing that only the row reads is in the
//! key.
//!
//! The diff view's branch returns before its selection pass as well. Past what
//! a plain pane reads, it reads the diff dials and the names on its sides bar.
//! No version moves with either, so the key holds both.
//!
//! A changes view paints one diff view per excerpt its window shows. Its key
//! holds the parts an editor key holds for the focused excerpt, and it folds
//! the placement and the versions of every shown excerpt into one hash. An
//! excerpt outside the window paints nothing, so a parse that lands there
//! replays the pane.

use crate::{
    buffer::BufferId,
    buffer_registry::BufferRegistry,
    diff_changes::{self, DiffChangesId, DiffChangesState},
    display_map::PaintVersion,
    editor_state::{EditorId, EditorState},
    pane::{Pane, PaneId, View},
    render::{
        diff_changes::stack_composited, pane::diff_sides_for, review::DiffDials,
        undercurl::UndercurlBatch, FrameCtx, Hold,
    },
    workspace::WorkspaceId,
};
use lsp_types::MessageType;
use ratatui::{
    buffer::{Buffer, Cell},
    layout::Rect,
};
use slotmap::SlotMap;
use std::{
    hash::{DefaultHasher, Hash, Hasher},
    sync::Arc,
};
use stoat_config::{LineNumbers, WrapMode};
use stoat_widgets::ApcScene;

/// Everything an editor pane's content paint reads that changes from one frame
/// to the next.
///
/// Two frames whose keys are equal paint identical content cells, content lane
/// bytes, and undercurl spans, which is the direction a replay needs. The
/// converse is weaker, since several parts move on work that changed no
/// visible cell.
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
    /// The theme and the settings the paint reads off the app, neither of which
    /// passes through a display layer.
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
    /// Whether the pane holds the focus. A focused editor numbers its lines
    /// from the cursor and paints its selections, which an unfocused one skips.
    pub(crate) focused: bool,
    /// The selection set the pane paints, by its identity. The cursor and
    /// every selection move with it.
    ///
    /// `None` for a changes view whose focused excerpt holds one bare cursor
    /// that the terminal draws. That cursor paints no cell, so a move of it
    /// inside the window replays the pane.
    pub(crate) selections: Option<u64>,
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
    /// Whether the terminal draws the pane's primary cursor, which the pane
    /// otherwise paints as a block. A modal input takes the terminal cursor
    /// whether or not it moves anything else in the pane, so this field
    /// repaints the cell when nothing else does.
    pub(crate) terminal_cursor: bool,
    /// The excerpts a changes view's window shows, hashed with the focused
    /// index, the window's rows, and whether the terminal composites the
    /// stack. Each one counts its buffer, placement, rows, path, fold,
    /// versions, and diagnostics. Zero for an editor pane.
    pub(crate) stack: u64,
}

/// One pane's last content paint, held so a frame whose key still matches puts
/// it back instead of producing it again.
///
/// A pane paints into three channels and all three are reset per frame, so all
/// three are captured. Cells alone would replay the grid and lose the rich
/// gutter, the minimap strip, and the undercurls that ride beside it.
///
/// The status row is not held. It has a rect and a lane of its own, which the
/// capture leaves out.
///
/// The storage is reused across captures rather than rebuilt, so a pane that
/// misses writes over what it held instead of allocating its rect again. The
/// render site therefore takes the entry out of the map before it knows
/// whether this frame captures anything, which is why `key` is `None` while no
/// capture fills it.
#[derive(Default)]
pub(crate) struct PaneCacheEntry {
    pub(crate) key: Option<PaneCacheKey>,
    /// The content rect the pane painted, with its cells row-major.
    ///
    /// The cells sit behind an `Arc`, since a [`Hold`] shares them with the
    /// job that puts them back into a frame buffer off the loop.
    region: (Rect, Arc<Vec<Cell>>),
    /// Which capture the entry holds, as the render tick of the frame that
    /// took it, so a [`Hold`] names the capture its buffer carries.
    ///
    /// A frame captures a pane at most once, and its panes never overlap, so
    /// no two captures of any pane share both a rect and a tick. The UI thread
    /// relies on that to tell two frames that show one capture in a rect.
    capture_seq: u64,
    /// The pane's own slice of its content lane, with the lane's id. The slice
    /// is contiguous because no other pane writes that lane.
    scene: (u32, Vec<u8>),
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

/// The content lane's length and the span count to hand
/// [`PaneCacheEntry::capture_into`] once the pane has painted.
#[derive(Clone, Copy)]
pub(crate) struct PaintMark {
    /// The pane's content lane, with the lane's length before the paint.
    lane: (u32, usize),
    spans: usize,
}

impl PaintMark {
    /// Where the frame's append-only channels stood before a pane painted its
    /// content into `lane`.
    pub(crate) fn take(scene: &ApcScene, lane: u32, undercurls: &UndercurlBatch) -> Self {
        Self {
            lane: (lane, scene.lane_bytes(lane).len()),
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
    /// allocations, except the cells that a [`Hold`] still shares. It takes
    /// `tick`, the render tick of the frame that painted, as its id.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn capture_into(
        &mut self,
        key: PaneCacheKey,
        rect: Rect,
        buf: &Buffer,
        scene: &ApcScene,
        undercurls: &UndercurlBatch,
        mark: PaintMark,
        tick: u64,
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

        let (held, cells) = &mut self.region;
        *held = rect;
        // While a hold shares the cells, a blank job reads them off the loop,
        // so the capture fills a fresh vector rather than writing under it.
        if Arc::get_mut(cells).is_none() {
            *cells = Arc::default();
        }
        cells_into(buf, rect, Arc::make_mut(cells));
        self.capture_seq = tick;

        let (id, start) = mark.lane;
        let (held_id, bytes) = &mut self.scene;
        *held_id = id;
        bytes.clear();
        bytes.extend_from_slice(&scene.lane_bytes(id)[start..]);

        self.undercurls.clear();
        self.undercurls.extend(
            undercurls.spans()[mark.spans..]
                .iter()
                .map(|span| (span.x, span.y, span.len, span.color)),
        );
        true
    }

    /// Put the recorded content paint back into this frame's three channels.
    ///
    /// The scene slice goes back into the lane it came from, and the replay
    /// leaves lane 0 open, as a paint at the render site does. The caller
    /// paints the status row.
    ///
    /// The scene append lands nowhere while the scene is dead, which is the
    /// same place the original paint's did, so a session without a listener
    /// replays exactly what it painted.
    ///
    /// With `copy` false, `buf` already holds the cells, from a [`Hold`] of
    /// this capture, so the replay leaves them and writes the scene and the
    /// undercurls alone.
    pub(crate) fn replay(
        &mut self,
        buf: &mut Buffer,
        scene: &mut ApcScene,
        undercurls: &mut UndercurlBatch,
        copy: bool,
    ) {
        // The held key is the last one the pane built, so a replayed frame
        // held the key still, which ends any run of moving frames.
        self.moved = false;

        if copy {
            let (rect, cells) = &self.region;
            let width = rect.width as usize;
            for (row, y) in (rect.top()..rect.bottom()).enumerate() {
                let start = buf.index_of(rect.left(), y);
                buf.content[start..start + width]
                    .clone_from_slice(&cells[row * width..(row + 1) * width]);
            }
        }

        let (id, bytes) = &self.scene;
        scene.set_lane(*id);
        scene.buffer().extend_from_slice(bytes);
        scene.end_lane();

        for &(x, y, len, color) in &self.undercurls {
            undercurls.push(x, y, len, color);
        }
    }

    /// The capture this entry holds, as the [`Hold`] that a frame buffer
    /// carries for `pane`.
    pub(crate) fn hold(&self, pane: (WorkspaceId, PaneId)) -> Hold {
        let (rect, cells) = &self.region;
        Hold {
            pane,
            rect: *rect,
            capture: self.capture_seq,
            cells: cells.clone(),
        }
    }

    /// Whether `hold` names the capture this entry holds.
    pub(crate) fn is_held_by(&self, hold: &Hold) -> bool {
        hold.capture == self.capture_seq && hold.rect == self.region.0
    }
}

/// The key for `pane`, or `None` when the pane is not one this cache covers.
///
/// An editor pane qualifies, focused or not, whether it shows a document or
/// the diff view, and so does a changes view pane. The conflict view takes its
/// own branch out of the editor render and reads state the key does not cover,
/// so it always paints.
///
/// A focused pane under goto-word labels, the LSP status card, or the LSP
/// error card always paints as well. The labels last a jump's few frames, and
/// the status card turns the spinner on every tick.
///
/// An editor that holds a screen row always paints too. The paint moves its
/// scroll, so a key read before the paint names a scroll that the paint left.
/// A capture under that key replays the moved rows on a later frame back at
/// that scroll.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pane_cache_key(
    pane: &Pane,
    is_focused: bool,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    diff_changes_views: &mut SlotMap<DiffChangesId, DiffChangesState>,
    frame: FrameCtx<'_>,
    paint_generation: u64,
    scene_live: bool,
) -> Option<PaneCacheKey> {
    let editor_id = match pane.view {
        View::Editor(editor_id) => editor_id,
        View::DiffChanges(id) => {
            let state = diff_changes_views.get_mut(id)?;
            return stack_cache_key(
                pane,
                is_focused,
                state,
                editors,
                buffers,
                frame,
                paint_generation,
                scene_live,
            );
        },
        _ => return None,
    };
    let editor = editors.get_mut(editor_id)?;
    if editor.conflict_view.is_some()
        || editor.held_screen_row.is_some()
        || editor.pending_center.is_some()
    {
        return None;
    }

    let error_card = frame
        .lsp_message
        .is_some_and(|(typ, _)| typ == MessageType::ERROR);
    if is_focused && (frame.goto_word_labels.is_some() || frame.lsp_status_open || error_card) {
        return None;
    }

    let buffer = editor.buffer_id;
    let diff_version = diff_version(buffers, buffer)?;

    let (diff_dials, diff_sides) = match editor.diff_view {
        true => {
            let mut sides = DefaultHasher::new();
            diff_sides_for(buffers.path_for(buffer), frame).hash(&mut sides);
            (dial_bits(frame.diff_dials), sides.finish())
        },
        false => ((0, 0, false, false), 0),
    };

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
        paint_generation,
        inactive_dim_bits: frame.inactive_dim.to_bits(),
        line_numbers: frame.line_numbers,
        diagnostics_version: diagnostics_version(buffers, buffer, frame),
        diff_version,
        diff_view: editor.diff_view,
        diff_dials,
        diff_sides,
        minimap_content_id,
        minimap_enabled: frame.minimap_enabled,
        focused: is_focused,
        selections: Some(editor.selections.identity()),
        insert_mode: is_focused && frame.mode == "insert",
        hover_cell: frame.hover_cell.filter(|_| is_focused),
        scene_live,
        terminal_cursor: frame.terminal_cursor_editor == Some(editor_id),
        stack: 0,
    })
}

/// The key for a changes view pane over `state`, or `None` on a frame whose
/// paint moves the stack's scroll.
///
/// The common parts come from the focused excerpt, the one excerpt that
/// paints selections, the cursor, and the hover. `stack` folds in what every
/// excerpt the window shows paints from, and their placement, which an
/// excerpt above the window moves when its rows change. It also folds in
/// [`stack_composited`], under which the paint leaves out the stack's runs and
/// bars, so a capture never replays across a change of it.
///
/// A held screen row, a pending landing, a cursor that moved out of the window
/// since the last paint, and a scroll past the stack's last page each move the
/// scroll inside the paint, so a key read before it names a scroll that the
/// paint leaves. An empty stack paints one line of text, and goto-word labels
/// over the focused excerpt last a jump's few frames, so neither gets a key.
///
/// A bare cursor that the terminal draws paints no cell, so its key leaves the
/// selections out, and a move of it inside the window replays the pane. The
/// replay then records the cursor where the paint records it, through
/// [`replay_cursor`](crate::render::diff_changes::replay_cursor).
#[allow(clippy::too_many_arguments)]
fn stack_cache_key(
    pane: &Pane,
    is_focused: bool,
    state: &mut DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    frame: FrameCtx<'_>,
    paint_generation: u64,
    scene_live: bool,
) -> Option<PaneCacheKey> {
    if state.held_screen_row.is_some()
        || state.pending_landing.is_some()
        || (is_focused && frame.goto_word_labels.is_some())
    {
        return None;
    }
    let focused_excerpt = state.focused_excerpt()?;
    let (editor_id, buffer) = (focused_excerpt.editor_id, focused_excerpt.buffer_id);
    let total = diff_changes::layout_stack(state, editors);
    if state.scroll > total.saturating_sub(state.viewport_rows) {
        return None;
    }
    let window = state.scroll..state.scroll + state.viewport_rows;
    let moved = diff_changes::focused_cursor(state, editors) != state.followed_cursor;
    if moved
        && diff_changes::cursor_stack_row(state, editors).is_some_and(|row| !window.contains(&row))
    {
        return None;
    }

    let mut stack = DefaultHasher::new();
    (
        state.focused,
        state.viewport_rows,
        stack_composited(pane, frame),
    )
        .hash(&mut stack);
    let ends = state
        .excerpts
        .iter()
        .skip(1)
        .map(|excerpt| excerpt.top)
        .chain([total]);
    for (excerpt, end) in state.excerpts.iter().zip(ends) {
        if end <= window.start || excerpt.top >= window.end {
            continue;
        }
        let shown = editors.get_mut(excerpt.editor_id)?;
        (
            excerpt.buffer_id,
            excerpt.top,
            end,
            &excerpt.rel_path,
            excerpt.folded_version,
            shown.display_map.buffer_snapshot().version(),
            shown.display_map.snapshot().paint_version(),
            diff_version(buffers, excerpt.buffer_id)?,
            diagnostics_version(buffers, excerpt.buffer_id, frame),
        )
            .hash(&mut stack);
    }

    let editor = editors.get_mut(editor_id)?;
    Some(PaneCacheKey {
        buffer,
        buffer_version: editor.display_map.buffer_snapshot().version(),
        paint_version: editor.display_map.snapshot().paint_version(),
        wrap: (editor.wrap_override, frame.wrap_mode, frame.wrap_column),
        scroll_row: state.scroll as u32,
        area: pane.area,
        pane_index: pane.index as usize,
        paint_generation,
        inactive_dim_bits: frame.inactive_dim.to_bits(),
        line_numbers: frame.line_numbers,
        diagnostics_version: diagnostics_version(buffers, buffer, frame),
        diff_version: diff_version(buffers, buffer)?,
        diff_view: true,
        diff_dials: dial_bits(frame.diff_dials),
        diff_sides: 0,
        minimap_content_id: frame
            .minimap_chrome
            .as_ref()
            .and_then(|chrome| chrome.diff_changes_content)
            .unwrap_or(0),
        minimap_enabled: frame.minimap_enabled,
        focused: is_focused,
        selections: match frame.terminal_cursor_editor == Some(editor_id)
            && scene_live
            && bare_cursor(editor)
        {
            true => None,
            false => Some(editor.selections.identity()),
        },
        insert_mode: is_focused && frame.mode == "insert",
        hover_cell: frame.hover_cell.filter(|_| is_focused),
        scene_live,
        terminal_cursor: frame.terminal_cursor_editor == Some(editor_id),
        stack: stack.finish(),
    })
}

/// Whether `editor` holds one selection that covers nothing past its cursor's
/// grapheme.
///
/// The selection pass washes a selection's cells but its cursor's, so such a
/// selection paints no cell once the terminal draws its cursor. A second
/// cursor paints a block, which is why one selection is the bound.
pub(crate) fn bare_cursor(editor: &EditorState) -> bool {
    let [selection] = editor.selections.all_anchors() else {
        return false;
    };
    let buffer = editor.display_map.buffer_snapshot();
    let rope = buffer.rope();
    let [start, end, tail, head] = [
        selection.start,
        selection.end,
        selection.tail(),
        selection.head(),
    ]
    .map(|anchor| buffer.resolve_anchor(&anchor));
    let cursor = stoat_text::cursor_offset(rope, tail, head);
    start == cursor && end <= rope.next_grapheme_boundary(cursor)
}

/// The version of `buffer`'s diff map, zero while it has none, or `None` when
/// the buffer is gone or its lock is poisoned.
fn diff_version(buffers: &BufferRegistry, buffer: BufferId) -> Option<usize> {
    let shared = buffers.get(buffer)?;
    let guard = shared.read().ok()?;
    Some(guard.diff_map.as_ref().map_or(0, |dm| dm.version()))
}

/// The version of the diagnostics published for `buffer`'s path, zero for a
/// buffer with no path.
fn diagnostics_version(buffers: &BufferRegistry, buffer: BufferId, frame: FrameCtx<'_>) -> u64 {
    buffers
        .path_for(buffer)
        .map_or(0, |path| frame.diagnostics.version_for(path))
}

/// The diff view's dials as the key holds them.
///
/// The soften scale and the tint amount go in as bits, then the bold and the
/// underline flags. The dials destructure whole, so a further dial fails to compile here until
/// the key holds it.
pub(crate) fn dial_bits(dials: DiffDials) -> (u32, u32, bool, bool) {
    let DiffDials {
        soften_scale,
        tint_amount,
        bold,
        underline,
    } = dials;
    (
        soften_scale.to_bits(),
        tint_amount.to_bits(),
        bold,
        underline,
    )
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
            focused: false,
            selections: Some(0),
            insert_mode: false,
            hover_cell: None,
            scene_live: false,
            terminal_cursor: false,
            stack: 0,
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
        let moved: [(&str, PaneCacheKey); 23] = [
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
                "focused",
                PaneCacheKey {
                    focused: true,
                    ..base
                },
            ),
            (
                "selections",
                PaneCacheKey {
                    selections: Some(1),
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
            (
                "terminal_cursor",
                PaneCacheKey {
                    terminal_cursor: true,
                    ..base
                },
            ),
            ("stack", PaneCacheKey { stack: 1, ..base }),
        ];

        for (field, other) in moved {
            assert_ne!(base, other, "{field} has to tell two keys apart");
        }

        assert_eq!(base, key(PaintVersion::default()), "and equal parts agree");
    }
}

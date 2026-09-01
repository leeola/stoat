//! The Kitty graphics store: transmissions in, decoded images out.
//!
//! A client streams an image as a run of frames and later asks the terminal to
//! place it. This module owns the first half. It assembles the chunks, decodes
//! the payload once the last one lands, keeps the pixels under an id, and
//! answers the client. Placing an image on the grid is a separate concern.
//!
//! Decoding runs on the thread feeding the terminal, holding its lock. An image
//! arrives rarely, so the stall it costs falls on a path that is otherwise idle,
//! and paying it here avoids a worker and its handoff for an event that happens
//! a handful of times a session. A pathological client can make that stall
//! repeated, which is what the byte caps here bound.
//!
//! Everything the store refuses, it refuses by answering. A client that gets an
//! error falls back, while one that gets silence waits.

use std::{collections::HashMap, sync::Arc};
use stoatty_protocol::{
    iterm::{Dimension, ItermFile},
    kitty::{
        Action, Compression, ControlData, DeleteKind, Format, GraphicsFrame, Medium, Response,
        ResponseResult,
    },
};

/// Largest base64 payload one transmission may accumulate.
///
/// The bound is on what arrives rather than on what it decodes to, because the
/// terminal must decide whether to keep reading before it knows either.
const MAX_TRANSMIT_BYTES: usize = 128 * 1024 * 1024;

/// Largest RGBA buffer one image may decode to.
///
/// A small compressed payload can describe an enormous image, so the encoded
/// cap above does not bound this one.
const MAX_DECODED_BYTES: usize = 64 * 1024 * 1024;

/// Largest total RGBA the store holds across every image.
const MAX_STORE_BYTES: usize = 256 * 1024 * 1024;

/// Images the store holds at once, whatever their sizes.
///
/// A client looping tiny images would pass the byte quota forever, so the count
/// is capped too.
const MAX_IMAGES: usize = 1024;

/// Bytes per pixel in the store's one internal format.
const RGBA: usize = 4;

/// Placements one screen may hold at once.
///
/// A client that places without deleting would grow the list for the session,
/// and every placement costs work in the projection that rebuilds them.
const MAX_PLACEMENTS: usize = 512;

/// Cell size assumed while the app has not reported the real one.
///
/// A placement sized from pixels needs some cell size to divide by. Guessing a
/// conventional one puts the image roughly right, where dividing by zero would
/// put nothing anywhere.
const FALLBACK_CELL: (u32, u32) = (8, 16);

/// One image placed on the screen.
///
/// The anchor is the cursor cell at the moment the placement was applied, not a
/// position in the scrollback. Scrolling shifts the anchor by exactly what the
/// cell grid shifted by, which is what keeps an image with the text it belongs
/// to without tracking a buffer position.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Placement {
    pub image: u32,
    pub placement: u32,
    /// Anchor row, as a signed value so a placement scrolled off the top is
    /// recognized rather than wrapping to the bottom.
    pub row: i32,
    pub col: usize,
    pub cols: usize,
    pub rows: usize,
    pub crop_x: u32,
    pub crop_y: u32,
    pub crop_w: u32,
    pub crop_h: u32,
    pub offset_x: u32,
    pub offset_y: u32,
    pub z: i32,
}

/// An image the store holds, decoded and ready to place.
///
/// The pixels are shared rather than copied because a placement pass reads them
/// per frame while the store keeps owning them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct DecodedImage {
    pub rgba: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
    /// Bumped whenever the id's pixels are replaced, so a consumer holding a
    /// placement can tell a re-transmission from the image it drew.
    pub generation: u64,
    /// The client's image number, or zero when it transmitted by id instead.
    ///
    /// Kept because a delete may name an image by number, which is only
    /// resolvable against what the transmission said.
    pub number: u32,
}

/// A transmission still arriving, held until the frame that ends it.
struct Pending {
    /// The control data of the frame that opened it. A continuation carries
    /// only `m` and `q`, so everything else is read from here.
    control: ControlData,
    /// The base64 as it arrived. Decoded once, joined, because a chunk of a
    /// base64 stream is not a chunk of the image.
    payload: Vec<u8>,
}

/// What the terminal must do after a graphics frame.
pub(crate) struct Applied {
    /// The reply to send the client, if any.
    pub response: Option<Response>,
    /// Where the cursor should land, when the frame placed an image and the
    /// client did not ask to keep the cursor still.
    pub cursor: Option<(usize, usize)>,
}

impl Applied {
    /// An outcome that only answers, leaving the cursor alone.
    fn reply(response: Option<Response>) -> Self {
        Self {
            response,
            cursor: None,
        }
    }
}

/// The screen state a graphics frame is applied against.
#[derive(Clone, Copy)]
pub(crate) struct Screen {
    /// The cursor cell, which a placement anchors to.
    pub cursor: (usize, usize),
    /// Pixel size of one cell, which sizing divides by.
    pub cell: (u32, u32),
}

/// Kitty images this terminal holds, and the transmission currently arriving.
#[derive(Default)]
pub(crate) struct ImageStore {
    images: HashMap<u32, DecodedImage>,
    /// Insertion order, so the quota evicts the oldest image first.
    order: Vec<u32>,
    /// Total RGBA the store holds, tracked rather than summed so an insert
    /// does not walk every image to find out whether it fits.
    bytes: usize,
    pending: Option<Pending>,
    /// Placements on each screen, the primary first and the alternate second.
    ///
    /// A screen is its own surface, so an image placed on one is not on the
    /// other. The pixels are shared, since they sit under an id rather than
    /// under a screen.
    placements: [Vec<Placement>; 2],
    /// Which screen is showing, indexing [`Self::placements`].
    screen: usize,
    /// Ids handed out for a client that transmits by number rather than by id.
    ///
    /// Counts down from the top so it cannot collide with the low ids a client
    /// picks for itself.
    next_assigned: u32,
    generation: u64,
}

impl ImageStore {
    pub(crate) fn new() -> Self {
        Self {
            next_assigned: u32::MAX,
            ..Self::default()
        }
    }

    /// Point the store at the alternate screen or back at the primary.
    ///
    /// Leaving the alternate screen drops its placements, since that screen and
    /// what a program drew on it are both gone. The pixels stay under their ids,
    /// because those belong to the client rather than to a screen.
    pub(crate) fn set_alt_screen(&mut self, alt: bool) {
        let screen = usize::from(alt);
        if screen == self.screen {
            return;
        }
        if !alt {
            self.placements[1].clear();
        }
        self.screen = screen;
    }

    /// Shift every placement on the showing screen up by `rows`, dropping the
    /// ones that have left it.
    ///
    /// The anchor is a screen row, so content scrolling under a placement means
    /// the placement moves with it. A placement whose whole footprint has passed
    /// the top is gone: nothing scrolls it back, since the history view does not
    /// redraw placements.
    pub(crate) fn scroll(&mut self, rows: i32, screen_rows: usize) {
        if rows == 0 {
            return;
        }
        let list = &mut self.placements[self.screen];
        for placement in list.iter_mut() {
            placement.row -= rows;
        }
        list.retain(|placement| {
            placement.row + placement.rows as i32 > 0 && placement.row < screen_rows as i32
        });
    }

    /// Drop placements a resize left outside the grid.
    ///
    /// A placement anchored past the new edge has no cell to sit on, and
    /// clamping it would move an image the client never asked to move.
    pub(crate) fn clamp_to(&mut self, rows: usize, cols: usize) {
        for list in &mut self.placements {
            list.retain(|placement| placement.row < rows as i32 && placement.col < cols);
        }
    }

    /// Drop every image and placement, for a full terminal reset.
    ///
    /// A reset returns the terminal to its start state, and images the client
    /// transmitted are part of what it built since. Kitty scopes this to a full
    /// reset rather than to an erase or a soft reset, so a program clearing the
    /// screen keeps the images it will place again.
    pub(crate) fn reset(&mut self) {
        self.images.clear();
        self.order.clear();
        self.bytes = 0;
        self.placements = [Vec::new(), Vec::new()];
        self.pending = None;
    }

    /// The pixels held under `id`, for the projection joining a placement with
    /// the image it names.
    pub(crate) fn image(&self, id: u32) -> Option<&DecodedImage> {
        self.images.get(&id)
    }

    /// Placements on the showing screen, for the projection to draw.
    pub(crate) fn placements(&self) -> &[Placement] {
        &self.placements[self.screen]
    }

    /// Place image `id` at `cursor`, or report why it cannot be placed.
    ///
    /// `cell` is the pixel size of one cell, which sizing falls back on when the
    /// client states neither dimension of the box.
    ///
    /// Returns where the cursor should land, which is past the placement's last
    /// row unless the client asked to keep it where it is.
    pub(crate) fn place(
        &mut self,
        control: &ControlData,
        id: u32,
        cursor: (usize, usize),
        cell: (u32, u32),
    ) -> Result<Option<(usize, usize)>, ResponseResult> {
        let Some(image) = self.images.get(&id) else {
            return Err(error("ENOENT", "no image with that id"));
        };

        let (cols, rows) = placement_cells(control, image, cell);
        let list = &mut self.placements[self.screen];
        if list.len() >= MAX_PLACEMENTS {
            return Err(error("ENOSPC", "too many placements"));
        }

        // A client placing over its own placement id replaces it rather than
        // stacking a second copy in the same spot.
        list.retain(|held| held.image != id || held.placement != control.placement);
        list.push(Placement {
            image: id,
            placement: control.placement,
            row: cursor.0 as i32,
            col: cursor.1,
            cols,
            rows,
            crop_x: control.src_x,
            crop_y: control.src_y,
            crop_w: control.src_w,
            crop_h: control.src_h,
            // An offset past the cell would put the image in a different cell
            // than the one it is anchored to, so it saturates at the boundary.
            offset_x: control.cell_x.min(cell.0.saturating_sub(1)),
            offset_y: control.cell_y.min(cell.1.saturating_sub(1)),
            z: control.z,
        });

        match control.cursor_policy {
            0 => Ok(Some((cursor.0 + rows - 1, cursor.1 + cols))),
            _ => Ok(None),
        }
    }

    /// Take an iTerm2 inline image: decode it, store it, and place it.
    ///
    /// Returns where the cursor should land, or `None` when the client asked it
    /// to stay and when nothing could be drawn. An inline image carries no id
    /// and no way to ask about it afterward, so a failure has nowhere to be
    /// reported and is dropped, which is what iTerm2 itself does.
    ///
    /// `screen_cells` is the grid's size, which a percentage dimension is a
    /// percentage of.
    pub(crate) fn apply_inline(
        &mut self,
        file: &ItermFile,
        screen: Screen,
        screen_cells: (usize, usize),
    ) -> Option<(usize, usize)> {
        // inline=0 asks the terminal to offer the file as a download, which is a
        // file-manager feature rather than a rendering one.
        if !file.inline {
            return None;
        }

        let bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &file.payload)
                .ok()?;
        let decoded = decode_any_format(&bytes).ok()?;

        let id = self.assign_id();
        let (cols, rows) = inline_cells(file, &decoded, screen.cell, screen_cells);
        self.store(id, 0, decoded).ok()?;

        let control = ControlData {
            cols: cols as u32,
            rows: rows as u32,
            cursor_policy: u8::from(file.do_not_move_cursor),
            ..ControlData::default()
        };
        self.place(&control, id, screen.cursor, screen.cell).ok()?
    }

    /// Remove the placements `control` names, freeing image data when the
    /// client asked for it.
    ///
    /// The uppercase form of a delete selector frees the image itself once no
    /// placement is left holding it, which is how a client reclaims the store
    /// without tracking what it transmitted.
    pub(crate) fn delete(&mut self, control: &ControlData, cursor: (usize, usize)) {
        let target = control.delete;
        let list = &mut self.placements[self.screen];

        // Which images the delete touched, so a freeing delete knows what to
        // consider dropping afterward.
        let mut touched: Vec<u32> = Vec::new();
        list.retain(|held| {
            let hit = match target.kind {
                DeleteKind::All => true,
                DeleteKind::Id => {
                    held.image == control.id
                        && (control.placement == 0 || held.placement == control.placement)
                },
                DeleteKind::Number => self
                    .images
                    .get(&held.image)
                    .is_some_and(|image| image.number != 0 && image.number == control.number),
                DeleteKind::Cursor => held.covers(cursor.0 as i32, cursor.1),
                DeleteKind::Cell => held.covers(control.src_y as i32, control.src_x as usize),
                DeleteKind::CellZ => {
                    held.covers(control.src_y as i32, control.src_x as usize) && held.z == control.z
                },
                DeleteKind::Column => held.spans_col(control.src_x as usize),
                DeleteKind::Row => held.spans_row(control.src_y as i32),
                DeleteKind::Z => held.z == control.z,
                DeleteKind::IdRange => {
                    (control.src_x..=control.src_y.max(control.src_x)).contains(&held.image)
                },
            };
            if hit {
                touched.push(held.image);
            }
            !hit
        });

        if !target.free_data {
            return;
        }
        for image in touched {
            let placed = self
                .placements
                .iter()
                .any(|list| list.iter().any(|held| held.image == image));
            if !placed && let Some(dropped) = self.images.remove(&image) {
                self.bytes -= dropped.rgba.len();
                self.order.retain(|&held| held != image);
            }
        }
    }

    /// Apply one graphics frame against the screen state in `screen`.
    ///
    /// The reply is `None` when the frame named no image to answer for, when
    /// the client's quiet level suppressed it, and for a chunk that is not the
    /// last of its transmission.
    pub(crate) fn apply(&mut self, frame: GraphicsFrame, screen: Screen) -> Applied {
        let control = frame.control;

        // A continuation carries only `m` and `q`, so it is recognized by an
        // open transmission rather than by anything in the frame itself.
        if self.pending.is_some() && is_continuation(&control) {
            return self.continue_transmission(control, frame.payload, screen);
        }

        // Anything else arriving mid-transmission means the client abandoned it,
        // and the partial payload describes no image.
        if self.pending.take().is_some() {
            return Applied::reply(respond(
                &control,
                error("EINVAL", "graphics command during a transmission"),
            ));
        }

        match control.action {
            Action::Transmit | Action::TransmitAndDisplay | Action::Query => {
                self.begin_transmission(control, frame.payload, screen)
            },
            Action::Put => self.put(control, screen),
            Action::Delete => {
                self.delete(&control, screen.cursor);
                Applied::reply(respond(&control, ResponseResult::Ok))
            },
        }
    }

    /// Place an image the client transmitted earlier.
    fn put(&mut self, control: ControlData, screen: Screen) -> Applied {
        match self.place(&control, control.id, screen.cursor, screen.cell) {
            Ok(cursor) => Applied {
                response: respond(&control, ResponseResult::Ok),
                cursor,
            },
            Err(result) => Applied::reply(respond(&control, result)),
        }
    }

    /// Take the first frame of a transmission, decoding it when it is also the
    /// last.
    fn begin_transmission(
        &mut self,
        control: ControlData,
        payload: Vec<u8>,
        screen: Screen,
    ) -> Applied {
        if control.medium != Medium::Direct {
            return Applied::reply(respond(
                &control,
                error("EBADF", "unsupported transmission medium"),
            ));
        }
        if payload.len() > MAX_TRANSMIT_BYTES {
            return Applied::reply(respond(&control, error("EFBIG", "transmission too large")));
        }

        match control.more {
            true => {
                self.pending = Some(Pending { control, payload });
                Applied::reply(None)
            },
            false => self.finish(control, payload, screen),
        }
    }

    /// Take a continuation chunk, decoding once one reports no more follow.
    fn continue_transmission(
        &mut self,
        control: ControlData,
        payload: Vec<u8>,
        screen: Screen,
    ) -> Applied {
        let pending = self.pending.as_mut().expect("checked by the caller");
        if pending.payload.len() + payload.len() > MAX_TRANSMIT_BYTES {
            let control = self.pending.take().expect("checked by the caller").control;
            return Applied::reply(respond(&control, error("EFBIG", "transmission too large")));
        }
        pending.payload.extend_from_slice(&payload);

        if control.more {
            return Applied::reply(None);
        }

        let pending = self.pending.take().expect("checked by the caller");
        // The opening frame's control data describes the image. The
        // continuation's only says the stream ended, and its quiet level is the
        // one the client repeated on every chunk.
        self.finish(pending.control, pending.payload, screen)
    }

    /// Decode a complete payload, store it, and place it when asked, or answer
    /// why not.
    fn finish(&mut self, control: ControlData, payload: Vec<u8>, screen: Screen) -> Applied {
        let decoded = match decode(&control, &payload) {
            Ok(decoded) => decoded,
            Err(response) => return Applied::reply(respond(&control, response)),
        };

        // A query asks whether this would work, so having decoded it is the
        // whole answer and the pixels are dropped.
        if control.action == Action::Query {
            return Applied::reply(respond(&control, ResponseResult::Ok));
        }

        let id = match control.id {
            0 => self.assign_id(),
            id => id,
        };
        if let Err(response) = self.store(id, control.number, decoded) {
            return Applied::reply(respond_as(&control, id, response));
        }

        if control.action != Action::TransmitAndDisplay {
            return Applied::reply(respond_as(&control, id, ResponseResult::Ok));
        }

        match self.place(&control, id, screen.cursor, screen.cell) {
            Ok(cursor) => Applied {
                response: respond_as(&control, id, ResponseResult::Ok),
                cursor,
            },
            Err(result) => Applied::reply(respond_as(&control, id, result)),
        }
    }

    /// Put `image` under `id`, evicting to stay inside the quota.
    fn store(&mut self, id: u32, number: u32, image: DecodedImage) -> Result<(), ResponseResult> {
        let incoming = image.rgba.len();
        if incoming > MAX_STORE_BYTES {
            return Err(error("ENOSPC", "image larger than the store"));
        }

        if let Some(existing) = self.images.remove(&id) {
            self.bytes -= existing.rgba.len();
            self.order.retain(|&held| held != id);
        }

        while self.bytes + incoming > MAX_STORE_BYTES || self.images.len() >= MAX_IMAGES {
            // Every image is evictable while nothing places them. The next
            // sibling adds placements, and with them a reason to skip an image
            // something on screen still reads.
            let Some(oldest) = self.order.first().copied() else {
                return Err(error("ENOSPC", "image store full"));
            };
            self.order.remove(0);
            if let Some(evicted) = self.images.remove(&oldest) {
                self.bytes -= evicted.rgba.len();
            }
        }

        self.generation += 1;
        self.images.insert(
            id,
            DecodedImage {
                generation: self.generation,
                number,
                ..image
            },
        );
        self.order.push(id);
        self.bytes += incoming;
        Ok(())
    }

    /// Take the next id for a client that transmitted without naming one.
    ///
    /// Counts down so an assigned id cannot collide with the low ones a client
    /// picks. Running the whole range dry would take four billion images, so it
    /// saturates rather than carrying a failure path nothing reaches.
    fn assign_id(&mut self) -> u32 {
        let id = self.next_assigned;
        self.next_assigned = self.next_assigned.saturating_sub(1).max(1);
        id
    }
}

impl Placement {
    /// The rows the placement occupies, as a half-open range.
    fn row_range(&self) -> std::ops::Range<i32> {
        self.row..self.row + self.rows as i32
    }

    /// Whether the placement covers the cell at `row`, `col`.
    fn covers(&self, row: i32, col: usize) -> bool {
        self.row_range().contains(&row) && (self.col..self.col + self.cols).contains(&col)
    }

    fn spans_col(&self, col: usize) -> bool {
        (self.col..self.col + self.cols).contains(&col)
    }

    fn spans_row(&self, row: i32) -> bool {
        self.row_range().contains(&row)
    }
}

/// The cell box a placement occupies.
///
/// A client may state both dimensions, one, or neither. Both stretches the
/// image into that box, and keeping the aspect ratio is then the client's
/// business rather than the terminal's. One derives the other from the image's
/// own aspect, so a client can size by width and get a proportional height.
/// Neither divides the image's pixels by the cell, rounding up so a partial
/// cell is still drawn into.
///
/// Every result is at least one cell, since a placement of no cells is one the
/// client cannot see and cannot delete by position.
fn placement_cells(
    control: &ControlData,
    image: &DecodedImage,
    cell: (u32, u32),
) -> (usize, usize) {
    let (cell_w, cell_h) = match cell {
        (0, _) | (_, 0) => FALLBACK_CELL,
        cell => cell,
    };

    // The crop is what gets drawn, so it is what the size is derived from.
    let source_w = match control.src_w {
        0 => image.width.saturating_sub(control.src_x).max(1),
        w => w,
    };
    let source_h = match control.src_h {
        0 => image.height.saturating_sub(control.src_y).max(1),
        h => h,
    };

    let by_pixels = |pixels: u32, per_cell: u32| pixels.div_ceil(per_cell).max(1) as usize;

    match (control.cols, control.rows) {
        (0, 0) => (by_pixels(source_w, cell_w), by_pixels(source_h, cell_h)),
        (cols, 0) => {
            let height =
                (source_h as u64 * cols as u64 * cell_w as u64) / (source_w as u64 * cell_h as u64);
            (cols as usize, (height as usize).max(1))
        },
        (0, rows) => {
            let width =
                (source_w as u64 * rows as u64 * cell_h as u64) / (source_h as u64 * cell_w as u64);
            ((width as usize).max(1), rows as usize)
        },
        (cols, rows) => (cols as usize, rows as usize),
    }
}

/// Decode an inline image, letting the decoder work out the format.
///
/// An iTerm2 escape names no format, so the bytes are all there is to go on.
/// An animated GIF yields its first frame, which the decoder gives for free and
/// which is a better answer than nothing.
fn decode_any_format(bytes: &[u8]) -> Result<DecodedImage, ResponseResult> {
    let decoded =
        image::load_from_memory(bytes).map_err(|_| error("EINVAL", "unrecognized image"))?;

    let (width, height) = (decoded.width(), decoded.height());
    check_size(width, height)?;

    Ok(DecodedImage {
        rgba: decoded.into_rgba8().into_raw().into(),
        width,
        height,
        generation: 0,
        number: 0,
    })
}

/// The cell box an inline image occupies.
///
/// Each side is whatever the client stated, and the two interact: with the
/// aspect ratio preserved, a side the client left automatic follows from the one
/// it gave, and a box it gave both sides of is fitted inside rather than filled.
/// Turning that off lets the image stretch to exactly the box.
///
/// Every result is at least one cell and no larger than the screen. An image
/// wider than the terminal is one the client cannot see the rest of, and one of
/// zero cells is one it cannot see at all.
fn inline_cells(
    file: &ItermFile,
    image: &DecodedImage,
    cell: (u32, u32),
    screen: (usize, usize),
) -> (usize, usize) {
    let (cell_w, cell_h) = match cell {
        (0, _) | (_, 0) => FALLBACK_CELL,
        cell => cell,
    };
    let (screen_rows, screen_cols) = screen;

    let natural_w = image.width.div_ceil(cell_w).max(1) as usize;
    let natural_h = image.height.div_ceil(cell_h).max(1) as usize;

    let resolve = |dimension: Dimension, per_cell: u32, span: usize, natural: usize| match dimension
    {
        Dimension::Auto => None,
        Dimension::Cells(cells) => Some((cells as usize).max(1)),
        Dimension::Pixels(pixels) => Some((pixels.div_ceil(per_cell).max(1)) as usize),
        Dimension::Percent(percent) => (span > 0).then_some((span * percent as usize / 100).max(1)),
        // A percent of nothing, and a natural size is the better answer than
        // none.
        #[allow(unreachable_patterns)]
        _ => Some(natural),
    };

    let stated_w = resolve(file.width, cell_w, screen_cols, natural_w);
    let stated_h = resolve(file.height, cell_h, screen_rows, natural_h);

    // Aspect is a ratio of pixels, and a cell is not square, so deriving one
    // side from the other goes through pixels rather than cell counts. Doing it
    // in cells would stretch every image by the cell's own aspect.
    let rows_for = |cols: usize| {
        let pixels = cols as u64 * u64::from(cell_w) * u64::from(image.height);
        ((pixels / (u64::from(image.width) * u64::from(cell_h))) as usize).max(1)
    };
    let cols_for = |rows: usize| {
        let pixels = rows as u64 * u64::from(cell_h) * u64::from(image.width);
        ((pixels / (u64::from(image.height) * u64::from(cell_w))) as usize).max(1)
    };

    let (cols, rows) = match (stated_w, stated_h, file.preserve_aspect_ratio) {
        (None, None, _) => (natural_w, natural_h),
        (Some(w), Some(h), false) => (w, h),
        // Fitted inside the box rather than filling it, so the picture keeps
        // its shape and sits within what the client allowed.
        (Some(w), Some(h), true) => match rows_for(w) <= h {
            true => (w, rows_for(w)),
            false => (cols_for(h), h),
        },
        (Some(w), None, true) => (w, rows_for(w)),
        (Some(w), None, false) => (w, natural_h),
        (None, Some(h), true) => (cols_for(h), h),
        (None, Some(h), false) => (natural_w, h),
    };

    (
        cols.clamp(1, screen_cols.max(1)),
        rows.clamp(1, screen_rows.max(1)),
    )
}

/// Whether `control` is a continuation frame rather than a new command.
///
/// A continuation carries only `m` and `q`, so every other field sits at its
/// default. Checking the whole shape rather than just `m` is what tells a
/// continuation from a fresh transmission that happens to be chunked.
fn is_continuation(control: &ControlData) -> bool {
    let bare = ControlData {
        more: control.more,
        quiet: control.quiet,
        ..ControlData::default()
    };
    *control == bare
}

/// Turn a complete base64 payload into RGBA pixels.
fn decode(control: &ControlData, payload: &[u8]) -> Result<DecodedImage, ResponseResult> {
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, payload)
        .map_err(|_| error("EINVAL", "payload is not valid base64"))?;

    let bytes = match control.compression {
        None => bytes,
        Some(Compression::Zlib) => inflate(&bytes)?,
    };

    match control.format {
        Format::Png => decode_png(&bytes),
        Format::Rgb => expand_rgb(control, &bytes),
        Format::Rgba => take_rgba(control, bytes),
    }
}

/// Inflate a zlib-compressed payload, bounded by the decoded cap.
///
/// The cap is applied while reading rather than after, since a small payload
/// can describe an unbounded expansion.
fn inflate(bytes: &[u8]) -> Result<Vec<u8>, ResponseResult> {
    use std::io::Read;

    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(bytes)
        .take(MAX_DECODED_BYTES as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|_| error("EINVAL", "payload is not valid zlib data"))?;

    match out.len() > MAX_DECODED_BYTES {
        true => Err(error("EFBIG", "decompressed image too large")),
        false => Ok(out),
    }
}

fn decode_png(bytes: &[u8]) -> Result<DecodedImage, ResponseResult> {
    let decoded = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .map_err(|_| error("EINVAL", "payload is not a valid PNG"))?;

    let (width, height) = (decoded.width(), decoded.height());
    check_size(width, height)?;

    Ok(DecodedImage {
        rgba: decoded.into_rgba8().into_raw().into(),
        width,
        height,
        generation: 0,
        number: 0,
    })
}

/// Widen three-byte pixels to the store's four, at full opacity.
fn expand_rgb(control: &ControlData, bytes: &[u8]) -> Result<DecodedImage, ResponseResult> {
    let (width, height) = declared_size(control)?;
    let pixels = width as usize * height as usize;
    if bytes.len() != pixels * 3 {
        return Err(error("EINVAL", "payload length does not match s and v"));
    }

    let mut rgba = Vec::with_capacity(pixels * RGBA);
    for pixel in bytes.as_chunks::<3>().0 {
        rgba.extend_from_slice(pixel);
        rgba.push(0xff);
    }

    Ok(DecodedImage {
        rgba: rgba.into(),
        width,
        height,
        generation: 0,
        number: 0,
    })
}

fn take_rgba(control: &ControlData, bytes: Vec<u8>) -> Result<DecodedImage, ResponseResult> {
    let (width, height) = declared_size(control)?;
    if bytes.len() != width as usize * height as usize * RGBA {
        return Err(error("EINVAL", "payload length does not match s and v"));
    }

    Ok(DecodedImage {
        rgba: bytes.into(),
        width,
        height,
        generation: 0,
        number: 0,
    })
}

/// The dimensions a raw-pixel transmission must state, since its bytes carry no
/// header to read them from.
fn declared_size(control: &ControlData) -> Result<(u32, u32), ResponseResult> {
    if control.width == 0 || control.height == 0 {
        return Err(error("EINVAL", "raw pixel data needs s and v"));
    }
    check_size(control.width, control.height)?;
    Ok((control.width, control.height))
}

fn check_size(width: u32, height: u32) -> Result<(), ResponseResult> {
    let bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(RGBA));

    match bytes {
        Some(bytes) if bytes <= MAX_DECODED_BYTES => Ok(()),
        _ => Err(error("EFBIG", "image too large")),
    }
}

fn error(code: &str, message: &str) -> ResponseResult {
    ResponseResult::Error {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

/// Build the reply for `control`, or `None` if none should go out.
fn respond(control: &ControlData, result: ResponseResult) -> Option<Response> {
    respond_as(control, control.id, result)
}

/// Build the reply naming `id`, which may be one the terminal assigned.
///
/// A frame that named neither an id nor a number gets no reply at all: the
/// client did not ask to be told, and an unsolicited reply would land in a
/// stream it is not reading. Beyond that the client's quiet level decides,
/// dropping the success reply at 1 and errors as well at 2.
fn respond_as(control: &ControlData, id: u32, result: ResponseResult) -> Option<Response> {
    if control.id == 0 && control.number == 0 {
        return None;
    }

    let quiet = match result {
        ResponseResult::Ok => control.quiet >= 1,
        ResponseResult::Error { .. } => control.quiet >= 2,
    };
    if quiet {
        return None;
    }

    Some(Response {
        id,
        number: control.number,
        placement: control.placement,
        result,
    })
}

#[cfg(test)]
mod tests {
    use super::{ImageStore, Screen, MAX_DECODED_BYTES, MAX_IMAGES, MAX_PLACEMENTS};
    use base64::Engine;
    use stoatty_protocol::kitty::{
        Action, Compression, ControlData, Format, GraphicsFrame, Medium, Response, ResponseResult,
    };

    /// A solid 2x2 image as a PNG, built rather than pasted so the pixels a test
    /// asserts and the bytes it feeds cannot drift apart.
    fn png_2x2(color: [u8; 4]) -> Vec<u8> {
        let buffer = image::RgbaImage::from_pixel(2, 2, image::Rgba(color));
        let mut out = std::io::Cursor::new(Vec::new());
        buffer
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode png");
        out.into_inner()
    }

    fn base64(bytes: &[u8]) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .encode(bytes)
            .into_bytes()
    }

    /// A screen with the cursor at the origin and a conventional cell, for a
    /// test whose subject is the transmission rather than the placement.
    fn screen() -> Screen {
        Screen {
            cursor: (0, 0),
            cell: (8, 16),
        }
    }

    /// Apply `frame` against that screen and keep only the reply, which is what
    /// the transmission tests are about.
    fn reply(store: &mut ImageStore, frame: GraphicsFrame) -> Option<Response> {
        store.apply(frame, screen()).response
    }

    fn frame(control: ControlData, payload: Vec<u8>) -> GraphicsFrame {
        GraphicsFrame { control, payload }
    }

    /// Control data for a transmission that names an id, so the store answers.
    fn transmit(id: u32) -> ControlData {
        ControlData {
            action: Action::Transmit,
            id,
            ..ControlData::default()
        }
    }

    fn ok(id: u32) -> Option<Response> {
        Some(Response {
            id,
            number: 0,
            placement: 0,
            result: ResponseResult::Ok,
        })
    }

    /// The image held under `id`, read straight off the map because the store
    /// has no consumer yet to justify an accessor.
    fn stored(store: &ImageStore, id: u32) -> Option<&super::DecodedImage> {
        store.images.get(&id)
    }

    fn code(response: &Option<Response>) -> Option<&str> {
        match response.as_ref().map(|r| &r.result) {
            Some(ResponseResult::Error { code, .. }) => Some(code),
            _ => None,
        }
    }

    #[test]
    fn a_png_transmission_stores_its_pixels() {
        let mut store = ImageStore::new();
        let control = ControlData {
            format: Format::Png,
            ..transmit(7)
        };

        assert_eq!(
            reply(&mut store, frame(control, base64(&png_2x2([1, 2, 3, 255])))),
            ok(7),
        );

        let image = stored(&store, 7).expect("the image is stored under its id");
        assert_eq!(
            (image.width, image.height, image.rgba.len()),
            (2, 2, 16),
            "the PNG header carries the size, so s and v are not needed",
        );
        assert_eq!(
            &image.rgba[..4],
            &[1, 2, 3, 255],
            "the pixels are the ones that went in"
        );
    }

    /// Three-byte pixels widen to the store's four, so everything downstream
    /// reads one format.
    #[test]
    fn rgb_pixels_expand_to_opaque_rgba() {
        let mut store = ImageStore::new();
        let control = ControlData {
            format: Format::Rgb,
            width: 2,
            height: 1,
            ..transmit(1)
        };

        assert_eq!(
            reply(&mut store, frame(control, base64(&[9, 8, 7, 6, 5, 4]))),
            ok(1)
        );
        assert_eq!(
            stored(&store, 1).expect("stored").rgba.as_ref(),
            [9, 8, 7, 255, 6, 5, 4, 255],
        );
    }

    #[test]
    fn a_zlib_payload_inflates_before_decoding() {
        use std::io::Write;

        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&[4, 5, 6, 255]).expect("deflate");
        let deflated = encoder.finish().expect("deflate");

        let mut store = ImageStore::new();
        let control = ControlData {
            format: Format::Rgba,
            compression: Some(Compression::Zlib),
            width: 1,
            height: 1,
            ..transmit(2)
        };

        assert_eq!(reply(&mut store, frame(control, base64(&deflated))), ok(2));
        assert_eq!(
            stored(&store, 2).expect("stored").rgba.as_ref(),
            [4, 5, 6, 255]
        );
    }

    /// A chunk of a base64 stream is not a chunk of the image, so the store
    /// joins every chunk before it decodes anything.
    #[test]
    fn a_chunked_transmission_decodes_only_once_joined() {
        let png = base64(&png_2x2([10, 20, 30, 255]));
        let third = png.len() / 3;
        let (head, rest) = png.split_at(third);
        let (middle, tail) = rest.split_at(third);

        let mut store = ImageStore::new();
        let control = ControlData {
            format: Format::Png,
            more: true,
            ..transmit(3)
        };

        assert_eq!(
            reply(&mut store, frame(control, head.to_vec())),
            None,
            "a chunk that is not the last says nothing",
        );
        assert_eq!(stored(&store, 3), None, "and stores nothing yet");

        let continuation = ControlData {
            more: true,
            ..ControlData::default()
        };
        assert_eq!(
            reply(&mut store, frame(continuation, middle.to_vec())),
            None,
            "nor does a middle chunk, which carries only m and q",
        );

        let last = ControlData {
            more: false,
            ..ControlData::default()
        };
        assert_eq!(
            reply(&mut store, frame(last, tail.to_vec())),
            ok(3),
            "the closing chunk answers under the opening frame's id",
        );
        assert_eq!(
            &stored(&store, 3).expect("stored").rgba[..4],
            &[10, 20, 30, 255],
        );
    }

    /// A client that abandons a transmission leaves a partial payload behind,
    /// and a partial base64 stream describes no image.
    #[test]
    fn a_command_during_a_transmission_aborts_it() {
        let mut store = ImageStore::new();
        let control = ControlData {
            format: Format::Png,
            more: true,
            ..transmit(4)
        };
        reply(&mut store, frame(control, base64(&png_2x2([1, 1, 1, 255]))));

        let interrupting = ControlData {
            format: Format::Rgba,
            width: 1,
            height: 1,
            ..transmit(5)
        };
        let response = reply(&mut store, frame(interrupting, base64(&[1, 2, 3, 4])));

        assert_eq!(code(&response), Some("EINVAL"));
        assert_eq!(
            (stored(&store, 4), stored(&store, 5)),
            (None, None),
            "neither the abandoned transmission nor the one that broke it stores",
        );
    }

    /// Refusing a medium is what makes a client fall back to streaming, which is
    /// the only medium this terminal reads.
    #[test]
    fn a_non_direct_medium_is_refused_by_code() {
        let mut store = ImageStore::new();

        for medium in [Medium::File, Medium::TempFile, Medium::SharedMemory] {
            let control = ControlData {
                medium,
                ..transmit(6)
            };
            assert_eq!(
                code(&reply(&mut store, frame(control, Vec::new()))),
                Some("EBADF"),
                "{medium:?} names data this terminal cannot read",
            );
        }
    }

    #[test]
    fn an_image_past_the_decoded_cap_is_refused() {
        let mut store = ImageStore::new();
        let side = (MAX_DECODED_BYTES as u32).isqrt();
        let control = ControlData {
            format: Format::Rgba,
            width: side,
            height: side,
            ..transmit(8)
        };

        assert_eq!(
            code(&reply(&mut store, frame(control, Vec::new()))),
            Some("EFBIG"),
            "the size is refused before any buffer is grown to hold it",
        );
    }

    #[test]
    fn a_payload_that_does_not_match_the_declared_size_is_refused() {
        let mut store = ImageStore::new();
        let control = ControlData {
            format: Format::Rgba,
            width: 4,
            height: 4,
            ..transmit(9)
        };

        assert_eq!(
            code(&reply(&mut store, frame(control, base64(&[0; 8])))),
            Some("EINVAL"),
        );
    }

    /// The quiet level is how a client says it is not reading replies, so the
    /// store must not write into a stream nobody drains.
    #[test]
    fn the_quiet_level_drops_the_replies_it_names() {
        let mut store = ImageStore::new();
        let good = |quiet| ControlData {
            format: Format::Png,
            quiet,
            ..transmit(11)
        };
        let bad = |quiet| ControlData {
            medium: Medium::File,
            quiet,
            ..transmit(12)
        };
        let png = base64(&png_2x2([2, 2, 2, 255]));

        assert!(reply(&mut store, frame(good(0), png.clone())).is_some());
        assert_eq!(
            reply(&mut store, frame(good(1), png)),
            None,
            "quiet 1 drops the success reply",
        );

        assert!(reply(&mut store, frame(bad(1), Vec::new())).is_some());
        assert_eq!(
            reply(&mut store, frame(bad(2), Vec::new())),
            None,
            "quiet 2 drops errors too",
        );
    }

    /// A frame naming neither an id nor a number asked nothing, so a reply would
    /// land in a stream the client is not reading.
    #[test]
    fn a_frame_naming_no_image_gets_no_reply() {
        let mut store = ImageStore::new();
        let control = ControlData {
            format: Format::Png,
            ..ControlData::default()
        };

        assert_eq!(
            reply(&mut store, frame(control, base64(&png_2x2([3; 4])))),
            None
        );
    }

    /// A client transmitting by number lets the terminal pick the id, so the
    /// reply has to carry both for the client to match them up.
    #[test]
    fn a_numbered_transmission_is_answered_with_the_assigned_id() {
        let mut store = ImageStore::new();
        let control = ControlData {
            format: Format::Png,
            number: 42,
            ..ControlData::default()
        };

        let response = reply(&mut store, frame(control, base64(&png_2x2([4; 4]))))
            .expect("a numbered frame is answered");

        assert_eq!(response.number, 42, "the reply echoes the client's number");
        assert!(
            response.id > 0 && stored(&store, response.id).is_some(),
            "and names an id the store actually holds",
        );
    }

    /// A query asks whether a transmission would work. Answering it must cost
    /// the store nothing, or a client probing repeatedly would fill it.
    #[test]
    fn a_query_validates_without_storing() {
        let mut store = ImageStore::new();
        let query = |payload| {
            (
                ControlData {
                    action: Action::Query,
                    format: Format::Png,
                    ..transmit(13)
                },
                payload,
            )
        };

        let (control, payload) = query(base64(&png_2x2([5; 4])));
        assert_eq!(reply(&mut store, frame(control, payload)), ok(13));
        assert_eq!(
            stored(&store, 13),
            None,
            "a validated image is still discarded"
        );

        let (control, payload) = query(b"not base64 at all!!".to_vec());
        assert_eq!(
            code(&reply(&mut store, frame(control, payload))),
            Some("EINVAL"),
            "and a bad one reports why rather than storing it",
        );
    }

    /// A put names an image the client transmitted earlier, so naming one it
    /// never sent has to say so rather than place nothing silently.
    #[test]
    fn a_put_of_an_unknown_id_reports_that_it_is_missing() {
        let mut store = ImageStore::new();
        let control = ControlData {
            action: Action::Put,
            ..transmit(31)
        };

        assert_eq!(
            code(&reply(&mut store, frame(control, Vec::new()))),
            Some("ENOENT")
        );
        assert!(store.placements().is_empty());
    }

    /// A transmit-and-display is one round trip for a client that wants the
    /// image on screen, so it must place as well as store.
    #[test]
    fn a_transmit_and_display_places_the_image_it_stored() {
        let mut store = ImageStore::new();
        let control = ControlData {
            action: Action::TransmitAndDisplay,
            format: Format::Png,
            ..transmit(32)
        };

        let applied = store.apply(
            frame(control, base64(&png_2x2([1; 4]))),
            Screen {
                cursor: (3, 5),
                cell: (8, 16),
            },
        );

        assert_eq!(
            store
                .placements()
                .iter()
                .map(|p| (p.image, p.row, p.col))
                .collect::<Vec<_>>(),
            [(32, 3, 5)],
            "the placement anchors to the cursor cell it was applied at",
        );
        assert!(applied.response.is_some());
    }

    /// The cursor lands past the image so the client's next line of text sits
    /// below it rather than over it.
    #[test]
    fn a_placement_moves_the_cursor_past_itself_unless_told_not_to() {
        let place = |cursor_policy| {
            let mut store = ImageStore::new();
            let control = ControlData {
                action: Action::TransmitAndDisplay,
                format: Format::Rgba,
                width: 16,
                height: 32,
                cursor_policy,
                ..transmit(33)
            };
            let payload = base64(&vec![0u8; 16 * 32 * 4]);
            store
                .apply(
                    frame(control, payload),
                    Screen {
                        cursor: (1, 2),
                        cell: (8, 16),
                    },
                )
                .cursor
        };

        assert_eq!(
            place(0),
            Some((2, 4)),
            "16x32 pixels over an 8x16 cell is 2x2 cells, so the cursor lands on \
             its last row and past its last column",
        );
        assert_eq!(place(1), None, "and stays put when the client says so");
    }

    /// A client may state the cell box, one side of it, or neither, and each
    /// case has to reach a size the image is actually drawn into.
    #[test]
    fn the_cell_box_comes_from_whichever_dimensions_the_client_stated() {
        let sized = |cols, rows| {
            let mut store = ImageStore::new();
            let control = ControlData {
                action: Action::TransmitAndDisplay,
                format: Format::Rgba,
                width: 32,
                height: 32,
                cols,
                rows,
                ..transmit(34)
            };
            let payload = base64(&vec![0u8; 32 * 32 * 4]);
            store.apply(
                frame(control, payload),
                Screen {
                    cursor: (0, 0),
                    cell: (8, 16),
                },
            );
            let placed = store.placements()[0];
            (placed.cols, placed.rows)
        };

        assert_eq!(
            sized(0, 0),
            (4, 2),
            "neither divides the pixels by the cell"
        );
        assert_eq!(sized(6, 3), (6, 3), "both stretch into the box as given");
        assert_eq!(
            sized(8, 0),
            (8, 4),
            "one derives the other from the image's aspect, in cells",
        );
        assert_eq!(sized(0, 4), (8, 4), "either way round");
    }

    /// Every delete selector has to reach the placements it names, since a
    /// client with no way to remove an image can only add.
    #[test]
    fn each_delete_selector_removes_what_it_names() {
        use stoatty_protocol::kitty::{DeleteKind, DeleteTarget};

        // Four placements of two images, spread so a positional selector picks
        // out exactly one of them.
        let populate = |store: &mut ImageStore| {
            for (id, row, col, z) in [
                (1u32, 0usize, 0usize, 0i32),
                (1, 4, 0, 1),
                (2, 0, 6, 0),
                (2, 4, 6, 2),
            ] {
                let control = ControlData {
                    action: Action::TransmitAndDisplay,
                    format: Format::Rgba,
                    width: 8,
                    height: 16,
                    z,
                    placement: row as u32 + 1,
                    ..transmit(id)
                };
                store.apply(
                    frame(control, base64(&vec![0u8; 8 * 16 * 4])),
                    Screen {
                        cursor: (row, col),
                        cell: (8, 16),
                    },
                );
            }
        };

        let remaining = |kind, control: ControlData| {
            let mut store = ImageStore::new();
            populate(&mut store);
            store.delete(
                &ControlData {
                    delete: DeleteTarget {
                        kind,
                        free_data: false,
                    },
                    ..control
                },
                (0, 0),
            );
            store.placements().len()
        };

        let plain = ControlData::default;
        assert_eq!(
            remaining(DeleteKind::All, plain()),
            0,
            "all clears the screen"
        );
        assert_eq!(
            remaining(DeleteKind::Id, ControlData { id: 1, ..plain() }),
            2,
            "an id takes both of that image's placements",
        );
        assert_eq!(
            remaining(
                DeleteKind::Id,
                ControlData {
                    id: 1,
                    placement: 1,
                    ..plain()
                }
            ),
            3,
            "and narrows to one when the client names the placement",
        );
        assert_eq!(
            remaining(DeleteKind::Cursor, plain()),
            3,
            "the cursor takes the one under it",
        );
        assert_eq!(
            remaining(
                DeleteKind::Cell,
                ControlData {
                    src_x: 6,
                    src_y: 0,
                    ..plain()
                }
            ),
            3,
            "a cell takes the placement covering it",
        );
        assert_eq!(
            remaining(
                DeleteKind::Column,
                ControlData {
                    src_x: 6,
                    ..plain()
                }
            ),
            2,
            "a column takes both placements spanning it",
        );
        assert_eq!(
            remaining(
                DeleteKind::Row,
                ControlData {
                    src_y: 4,
                    ..plain()
                }
            ),
            2,
            "and a row likewise",
        );
        assert_eq!(
            remaining(DeleteKind::Z, ControlData { z: 2, ..plain() }),
            3,
            "a z-index takes the placements sitting at it",
        );
        assert_eq!(
            remaining(
                DeleteKind::IdRange,
                ControlData {
                    src_x: 2,
                    src_y: 9,
                    ..plain()
                }
            ),
            2,
            "an id range takes every image inside it",
        );
    }

    /// The uppercase form is how a client reclaims the store without tracking
    /// what it transmitted, so it must free an image no placement holds.
    #[test]
    fn an_uppercase_delete_frees_the_image_data_too() {
        use stoatty_protocol::kitty::{DeleteKind, DeleteTarget};

        let delete_all = |free_data| {
            let mut store = ImageStore::new();
            let control = ControlData {
                action: Action::TransmitAndDisplay,
                format: Format::Png,
                ..transmit(41)
            };
            store.apply(frame(control, base64(&png_2x2([1; 4]))), screen());

            store.delete(
                &ControlData {
                    delete: DeleteTarget {
                        kind: DeleteKind::All,
                        free_data,
                    },
                    ..ControlData::default()
                },
                (0, 0),
            );
            (store.placements().len(), stored(&store, 41).is_some())
        };

        assert_eq!(
            delete_all(false),
            (0, true),
            "a lowercase delete takes the placement and keeps the pixels",
        );
        assert_eq!(
            delete_all(true),
            (0, false),
            "the uppercase twin takes the pixels with it",
        );
    }

    #[test]
    fn placements_stop_at_their_cap() {
        let mut store = ImageStore::new();
        let control = ControlData {
            action: Action::Transmit,
            format: Format::Png,
            ..transmit(51)
        };
        reply(&mut store, frame(control, base64(&png_2x2([1; 4]))));

        let put = |store: &mut ImageStore, placement| {
            let control = ControlData {
                action: Action::Put,
                placement,
                ..transmit(51)
            };
            store.apply(frame(control, Vec::new()), screen()).response
        };

        for placement in 1..=MAX_PLACEMENTS as u32 {
            put(&mut store, placement);
        }
        assert_eq!(store.placements().len(), MAX_PLACEMENTS);

        assert_eq!(
            code(&put(&mut store, MAX_PLACEMENTS as u32 + 1)),
            Some("ENOSPC"),
            "one past the cap is refused rather than dropping an older one",
        );
    }

    /// A transmit-and-display keeps its pixels even though nothing can place
    /// them yet, so the client need not send them twice.
    #[test]
    fn a_transmit_and_display_still_stores_its_data() {
        let mut store = ImageStore::new();
        let control = ControlData {
            action: Action::TransmitAndDisplay,
            format: Format::Png,
            ..transmit(15)
        };

        assert_eq!(
            reply(&mut store, frame(control, base64(&png_2x2([6; 4])))),
            ok(15)
        );
        assert!(stored(&store, 15).is_some());
    }

    #[test]
    fn re_transmitting_an_id_replaces_its_pixels_and_bumps_the_generation() {
        let mut store = ImageStore::new();
        let control = || ControlData {
            format: Format::Png,
            ..transmit(16)
        };

        reply(
            &mut store,
            frame(control(), base64(&png_2x2([7, 7, 7, 255]))),
        );
        let first = stored(&store, 16).expect("stored").generation;

        reply(
            &mut store,
            frame(control(), base64(&png_2x2([8, 8, 8, 255]))),
        );
        let second = stored(&store, 16).expect("stored");

        assert_eq!(&second.rgba[..4], &[8, 8, 8, 255], "the new pixels win");
        assert!(
            second.generation > first,
            "and the generation moves, so a consumer can tell them apart",
        );
    }

    /// A client looping images would grow the store without bound, so the
    /// oldest gives way rather than the newest being refused.
    #[test]
    fn passing_the_image_count_evicts_the_oldest() {
        let mut store = ImageStore::new();
        let png = base64(&png_2x2([9; 4]));
        let transmit_id = |store: &mut ImageStore, id| {
            let control = ControlData {
                format: Format::Png,
                ..transmit(id)
            };
            reply(store, frame(control, png.clone()))
        };

        for id in 1..=MAX_IMAGES as u32 {
            transmit_id(&mut store, id);
        }
        assert!(stored(&store, 1).is_some(), "the store is exactly full");

        assert_eq!(
            transmit_id(&mut store, MAX_IMAGES as u32 + 1),
            ok(MAX_IMAGES as u32 + 1),
            "one more is accepted rather than refused",
        );
        assert_eq!(stored(&store, 1), None, "and the oldest made room for it");
        assert!(stored(&store, 2).is_some(), "while the rest stay");
    }

    /// A client that transmits by number deletes by number too, and the store
    /// is the only place that mapping exists.
    #[test]
    fn a_delete_by_number_finds_the_image_that_number_transmitted() {
        use stoatty_protocol::kitty::{DeleteKind, DeleteTarget};

        let mut store = ImageStore::new();
        let control = ControlData {
            action: Action::TransmitAndDisplay,
            format: Format::Png,
            number: 77,
            ..ControlData::default()
        };
        store.apply(frame(control, base64(&png_2x2([1; 4]))), screen());
        assert_eq!(store.placements().len(), 1);

        store.delete(
            &ControlData {
                delete: DeleteTarget {
                    kind: DeleteKind::Number,
                    free_data: false,
                },
                number: 78,
                ..ControlData::default()
            },
            (0, 0),
        );
        assert_eq!(
            store.placements().len(),
            1,
            "another number names another image",
        );

        store.delete(
            &ControlData {
                delete: DeleteTarget {
                    kind: DeleteKind::Number,
                    free_data: false,
                },
                number: 77,
                ..ControlData::default()
            },
            (0, 0),
        );
        assert!(store.placements().is_empty(), "its own number finds it");
    }
}

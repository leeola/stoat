//! Putting a pane's image on screen, through the terminal's graphics protocol.
//!
//! An image pane holds a path and a size; the pixels live in the file. This is
//! what sends them and asks the terminal to draw them where the pane is.
//!
//! Three rules shape it, and each answers a cost. An image transmits once,
//! because its pixels do not change and re-sending them per frame would put a
//! file on the wire sixty times a second. A placement re-emits only when it
//! moved, because one that has not is already on screen. An image whose pane is
//! gone is deleted, because the terminal has no other way to learn that.
//!
//! Every frame carries the quiet level that suppresses all replies. stoat's
//! stdin is its keyboard, and a response arriving there would be typed into
//! whatever buffer has focus.

use crate::{app::Stoat, pane::View, render::layout::split_pane_status};
use ratatui::layout::Rect;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use stoat_scheduler::Task;
use stoatty_protocol::kitty::{self, Action, ControlData, Format};

/// Where one image sits on screen, as the terminal needs to be told.
///
/// Compared frame to frame to decide whether anything must be re-sent, so it
/// holds exactly what the placement depends on and nothing else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Placement {
    pub image: u32,
    /// The cell the image's top-left corner sits in, already centered inside
    /// the pane.
    pub row: u16,
    pub col: u16,
    pub cols: u16,
    pub rows: u16,
}

/// An image this session has sent, and the transmissions still in flight.
#[derive(Default)]
pub(crate) struct ImageRuntime {
    /// The id each file was transmitted under. A path present here has had its
    /// pixels sent and needs only placing.
    sent: HashMap<PathBuf, u32>,
    /// Placements as the terminal last heard them, by image id.
    placed: HashMap<u32, Placement>,
    /// Files being read and converted on the pool, by path, so one file opened
    /// in two panes is read once.
    pending: HashMap<PathBuf, PendingTransmit>,
    /// Ids handed out to files, counting up from one. Zero means unset on the
    /// wire, so it is never assigned.
    next_id: u32,
}

/// A file being turned into a transmission on the blocking pool.
struct PendingTransmit {
    /// The PNG bytes once ready, or `None` while the pool still has it. An
    /// error resolves to an empty vector, which the drain drops.
    result: Arc<Mutex<Option<Vec<u8>>>>,
    _task: Task<()>,
}

impl ImageRuntime {
    /// The id `path` was transmitted under, assigning one if it is new.
    fn id_for(&mut self, path: &Path) -> u32 {
        if let Some(id) = self.sent.get(path) {
            return *id;
        }
        self.next_id += 1;
        self.next_id
    }
}

/// Send this frame's image transmissions and placements.
///
/// A no-op unless the terminal is a stoatty new enough to draw images and the
/// tty has said how large a cell is. Without the cell size there is no way to
/// turn a pane's rectangle into the pixels an image should fill.
pub(crate) fn emit_images(stoat: &mut Stoat) {
    let capable = stoat.stoatty && stoat.stoatty_protocol >= 2;
    let Some(cell_px) = stoat.cell_pixels.filter(|_| capable) else {
        return;
    };
    let Some(apc_tx) = stoat.apc_tx.clone() else {
        return;
    };

    let wanted = wanted_images(stoat);
    start_transmits(stoat, &wanted);

    let mut batch = drain_transmits(stoat);
    let desired = placements(stoat, &wanted, cell_px);
    batch.extend(placement_batch(&desired, &stoat.images.placed));

    if !batch.is_empty() {
        let _ = apc_tx.send(batch);
    }

    stoat.images.placed = desired
        .into_iter()
        .map(|placement| (placement.image, placement))
        .collect();
}

/// Delete every image this session put on screen.
///
/// Run on the way out. The terminal holds placements and pixels until told
/// otherwise, so an editor that exited without this leaves its images over
/// whatever the shell prints next.
pub(crate) fn emit_drop_all_images(stoat: &Stoat) {
    let Some(apc_tx) = stoat.apc_tx.clone().filter(|_| stoat.stoatty) else {
        return;
    };
    if stoat.images.sent.is_empty() {
        return;
    }

    let mut batch = Vec::new();
    for id in stoat.images.sent.values() {
        delete_image_into(&mut batch, *id);
    }
    let _ = apc_tx.send(batch);
}

/// The image panes on screen, as their file and the rectangle they fill.
///
/// Read from the pane tree rather than collected while painting. A pane's
/// content rectangle follows from its area alone, so nothing about the paint is
/// needed to know where an image goes, and a collector threaded through the
/// render would be paid for by every pane that has no image.
fn wanted_images(stoat: &Stoat) -> Vec<(PathBuf, (u32, u32), Rect)> {
    stoat
        .active_workspace()
        .panes
        .split_panes()
        .filter_map(|(_, pane)| match &pane.view {
            View::Image { path, px } => Some((path.clone(), *px, split_pane_status(pane.area).0)),
            _ => None,
        })
        .collect()
}

/// Begin reading any wanted image this session has not sent or started.
fn start_transmits(stoat: &mut Stoat, wanted: &[(PathBuf, (u32, u32), Rect)]) {
    for (path, _, _) in wanted {
        if stoat.images.sent.contains_key(path) || stoat.images.pending.contains_key(path) {
            continue;
        }

        let result = Arc::new(Mutex::new(None));
        let task = {
            let result = result.clone();
            let fs_host = stoat.fs_host.clone();
            let redraw = stoat.redraw_notify.clone();
            let path = path.clone();
            stoat.executor.spawn_blocking(move || {
                let png = read_as_png(&*fs_host, &path).unwrap_or_default();
                *result.lock().expect("image transmit mutex") = Some(png);
                redraw.notify_one();
            })
        };
        stoat.images.pending.insert(
            path.clone(),
            PendingTransmit {
                result,
                _task: task,
            },
        );
    }
}

/// Collect every finished transmission into a batch, recording the ids.
///
/// A file that failed to read or decode resolves to no bytes and is dropped
/// rather than retried. The pane still shows its name and size, and retrying a
/// file that cannot be decoded would retry it every frame.
fn drain_transmits(stoat: &mut Stoat) -> Vec<u8> {
    let ready: Vec<(PathBuf, Vec<u8>)> = stoat
        .images
        .pending
        .iter()
        .filter_map(|(path, pending)| {
            let png = pending
                .result
                .lock()
                .expect("image transmit mutex")
                .take()?;
            Some((path.clone(), png))
        })
        .collect();

    let mut batch = Vec::new();
    for (path, png) in ready {
        stoat.images.pending.remove(&path);
        if png.is_empty() {
            continue;
        }

        let id = stoat.images.id_for(&path);
        stoat.images.sent.insert(path, id);
        transmit_into(&mut batch, id, &png);
    }
    batch
}

/// Where each wanted image with pixels already sent should sit.
fn placements(
    stoat: &Stoat,
    wanted: &[(PathBuf, (u32, u32), Rect)],
    cell_px: (u16, u16),
) -> Vec<Placement> {
    wanted
        .iter()
        .filter_map(|(path, px, rect)| {
            let image = *stoat.images.sent.get(path)?;
            let (cols, rows, col_off, row_off) = fit_cells(*px, (rect.width, rect.height), cell_px);
            (cols > 0 && rows > 0).then_some(Placement {
                image,
                row: rect.y + row_off,
                col: rect.x + col_off,
                cols,
                rows,
            })
        })
        .collect()
}

/// The bytes that turn `previous` into `desired`.
///
/// A placement that did not move sends nothing, an id no longer wanted is
/// deleted along with its pixels, and a moved one is deleted and re-placed
/// rather than moved, since the protocol offers no way to move a placement.
///
/// Each placement is bracketed by a cursor save and restore. Placing draws at
/// the cursor, so without that the editor's own cursor would end up wherever
/// the last image was.
fn placement_batch(desired: &[Placement], previous: &HashMap<u32, Placement>) -> Vec<u8> {
    let mut batch = Vec::new();

    for (id, held) in previous {
        if !desired.iter().any(|placement| placement.image == *id) {
            // Freed along with the placement: the pane is gone, and nothing
            // else in this session refers to the image.
            delete_image_into(&mut batch, held.image);
        }
    }

    for placement in desired {
        if previous.get(&placement.image) == Some(placement) {
            continue;
        }
        place_into(&mut batch, placement);
    }
    batch
}

/// Write a transmission of `png` under `id`.
fn transmit_into(out: &mut Vec<u8>, id: u32, png: &[u8]) {
    use base64::Engine;
    let payload = base64::engine::general_purpose::STANDARD
        .encode(png)
        .into_bytes();

    kitty::encode_chunked_into(
        out,
        &ControlData {
            action: Action::Transmit,
            format: Format::Png,
            id,
            quiet: 2,
            ..ControlData::default()
        },
        &payload,
    );
}

/// Write a placement, bracketed so the editor's cursor comes back.
fn place_into(out: &mut Vec<u8>, placement: &Placement) {
    // Save the cursor, since a placement draws where it is.
    out.extend_from_slice(b"\x1b7");
    out.extend_from_slice(format!("\x1b[{};{}H", placement.row + 1, placement.col + 1).as_bytes());

    // The old placement of this image goes first. The protocol has no way to
    // move one, so a move is a delete and a place.
    kitty::encode_into(
        out,
        &ControlData {
            action: Action::Delete,
            id: placement.image,
            placement: 1,
            quiet: 2,
            delete: kitty::DeleteTarget {
                kind: kitty::DeleteKind::Id,
                free_data: false,
            },
            ..ControlData::default()
        },
        b"",
    );
    kitty::encode_into(
        out,
        &ControlData {
            action: Action::Put,
            id: placement.image,
            placement: 1,
            cols: u32::from(placement.cols),
            rows: u32::from(placement.rows),
            cursor_policy: 1,
            quiet: 2,
            ..ControlData::default()
        },
        b"",
    );
    out.extend_from_slice(b"\x1b8");
}

/// Write a delete of `id` and the pixels behind it.
fn delete_image_into(out: &mut Vec<u8>, id: u32) {
    kitty::encode_into(
        out,
        &ControlData {
            action: Action::Delete,
            id,
            quiet: 2,
            delete: kitty::DeleteTarget {
                kind: kitty::DeleteKind::Id,
                free_data: true,
            },
            ..ControlData::default()
        },
        b"",
    );
}

/// Read `path` and return it as PNG bytes, or `None` if it is not an image.
///
/// A PNG passes through untouched. Anything else decodes and re-encodes,
/// because the protocol's other formats are raw pixel buffers and sending one
/// would put the decoded image on the wire rather than the compressed file.
fn read_as_png(fs: &dyn crate::host::FsHost, path: &Path) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    fs.read(path, &mut bytes).ok()?;

    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(bytes);
    }

    let decoded = image::load_from_memory(&bytes).ok()?;
    let mut out = std::io::Cursor::new(Vec::new());
    decoded.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// The cell box an image fills inside `rect`, and where to put it.
///
/// Fitted rather than filled, so the picture keeps its shape, and centered in
/// whichever direction it leaves room. The aspect is a ratio of pixels and a
/// cell is not square, so both go through pixels rather than cell counts.
///
/// Never scaled up past the picture's own pixels.
///
/// Returns zero cells for an image or a rectangle with no extent, which the
/// caller drops rather than placing something invisible.
pub(crate) fn fit_cells(
    px: (u32, u32),
    rect: (u16, u16),
    cell_px: (u16, u16),
) -> (u16, u16, u16, u16) {
    let (image_w, image_h) = px;
    let (rect_cols, rect_rows) = rect;
    let (cell_w, cell_h) = cell_px;
    if image_w == 0
        || image_h == 0
        || rect_cols == 0
        || rect_rows == 0
        || cell_w == 0
        || cell_h == 0
    {
        return (0, 0, 0, 0);
    }

    // The pane's extent in pixels, which is what the image is fitted into.
    let box_w = u64::from(rect_cols) * u64::from(cell_w);
    let box_h = u64::from(rect_rows) * u64::from(cell_h);

    // Scaled to whichever axis runs out first, then rounded up to whole cells,
    // since a placement is measured in them.
    let by_width = u64::from(image_h) * box_w / u64::from(image_w);
    let (draw_w, draw_h) = match by_width <= box_h {
        true => (box_w, by_width),
        false => (u64::from(image_w) * box_h / u64::from(image_h), box_h),
    };

    // Never larger than the picture itself. A pane is for looking at the file,
    // and an image drawn past its own pixels is a blurry version of one that
    // would have fit.
    let (draw_w, draw_h) = match draw_w > u64::from(image_w) {
        true => (u64::from(image_w), u64::from(image_h)),
        false => (draw_w, draw_h),
    };

    let cols = (draw_w.div_ceil(u64::from(cell_w)) as u16).clamp(1, rect_cols);
    let rows = (draw_h.div_ceil(u64::from(cell_h)) as u16).clamp(1, rect_rows);
    (cols, rows, (rect_cols - cols) / 2, (rect_rows - rows) / 2)
}

#[cfg(test)]
mod tests {
    use super::{fit_cells, placement_batch, Placement};
    use std::collections::HashMap;
    use stoatty_protocol::{
        command::{decode_stream, Command},
        kitty::Action,
    };

    /// An 8x16 cell, the ordinary shape, so a square image needs twice as many
    /// columns as rows to come out square on screen.
    const CELL: (u16, u16) = (8, 16);

    #[test]
    fn an_image_fits_inside_its_pane_and_centers_in_the_slack() {
        // A pane 10x10 cells is 80x160 pixels. A square image fits the width and
        // uses half the height, leaving rows to center in.
        assert_eq!(
            fit_cells((64, 64), (10, 10), CELL),
            (8, 4, 1, 3),
            "a square image is as wide as it is tall on screen, and centered",
        );

        // A tall image runs out of height first, so it keeps the rows and
        // centers across the columns it does not need.
        assert_eq!(
            fit_cells((64, 256), (10, 10), CELL),
            (5, 10, 2, 0),
            "a tall image runs out of height first and centers across the rest",
        );
    }

    #[test]
    fn an_image_matching_the_pane_fills_it_exactly() {
        // 80x160 pixels is precisely 10x10 cells.
        assert_eq!(fit_cells((80, 160), (10, 10), CELL), (10, 10, 0, 0));
    }

    /// A picture smaller than a cell still occupies one. Rounding it to nothing
    /// would place an image the user cannot see and cannot dismiss.
    #[test]
    fn an_image_smaller_than_a_cell_still_takes_one() {
        assert_eq!(fit_cells((2, 2), (10, 10), CELL), (1, 1, 4, 4));
    }

    /// Nothing with no extent can be placed, and every zero here would divide.
    #[test]
    fn no_extent_anywhere_places_nothing() {
        assert_eq!(fit_cells((0, 10), (10, 10), CELL), (0, 0, 0, 0));
        assert_eq!(fit_cells((10, 0), (10, 10), CELL), (0, 0, 0, 0));
        assert_eq!(fit_cells((10, 10), (0, 10), CELL), (0, 0, 0, 0));
        assert_eq!(fit_cells((10, 10), (10, 0), CELL), (0, 0, 0, 0));
        assert_eq!(fit_cells((10, 10), (10, 10), (0, 16)), (0, 0, 0, 0));
    }

    fn placement(image: u32, row: u16, col: u16) -> Placement {
        Placement {
            image,
            row,
            col,
            cols: 4,
            rows: 2,
        }
    }

    /// The graphics commands in a batch, in order, as (action, id).
    fn actions(batch: &[u8]) -> Vec<(Action, u32, bool)> {
        decode_stream(batch)
            .into_iter()
            .filter_map(|command| match command {
                Command::Kitty(frame) => Some((
                    frame.control.action,
                    frame.control.id,
                    frame.control.delete.free_data,
                )),
                _ => None,
            })
            .collect()
    }

    /// A placement that has not moved is already on screen, and re-sending it
    /// every frame would put an image command on the wire sixty times a second.
    #[test]
    fn an_unmoved_placement_sends_nothing() {
        let held = placement(1, 3, 5);
        let previous = HashMap::from([(1, held)]);

        assert!(placement_batch(&[held], &previous).is_empty());
    }

    /// The protocol offers no way to move a placement, so a move is a delete of
    /// the old one and a fresh place. The pixels stay, since the same image is
    /// about to be drawn again.
    #[test]
    fn a_moved_placement_is_deleted_and_placed_again() {
        let previous = HashMap::from([(1, placement(1, 3, 5))]);
        let batch = placement_batch(&[placement(1, 4, 5)], &previous);

        assert_eq!(
            actions(&batch),
            [(Action::Delete, 1, false), (Action::Put, 1, false)],
            "the old placement goes, the pixels stay, and the image is re-placed",
        );
    }

    /// A pane that closed leaves an image the terminal would keep drawing, and
    /// nothing else in the session refers to its pixels.
    #[test]
    fn an_image_no_longer_wanted_is_deleted_with_its_pixels() {
        let previous = HashMap::from([(1, placement(1, 0, 0)), (2, placement(2, 5, 0))]);
        let batch = placement_batch(&[placement(1, 0, 0)], &previous);

        assert_eq!(
            actions(&batch),
            [(Action::Delete, 2, true)],
            "only the vanished image, and its data goes with it",
        );
    }

    /// Placing draws at the cursor, so a batch that did not put the cursor back
    /// would leave the editor's own cursor wherever the last image was.
    #[test]
    fn a_placement_saves_and_restores_the_cursor_around_itself() {
        let batch = placement_batch(&[placement(7, 2, 3)], &HashMap::new());

        assert!(batch.starts_with(b"\x1b7"), "the cursor is saved first");
        assert!(batch.ends_with(b"\x1b8"), "and restored last");
        assert!(
            batch.windows(6).any(|window| window == b"\x1b[3;4H"),
            "and moved to the placement's cell, counting from one",
        );
    }

    /// stoat's stdin is its keyboard. A reply arriving there would be typed
    /// into whatever buffer has focus, so every frame has to suppress them.
    #[test]
    fn every_emitted_frame_suppresses_replies() {
        let previous = HashMap::from([(9, placement(9, 0, 0))]);
        let batch = placement_batch(&[placement(1, 1, 1)], &previous);
        let quiet: Vec<u8> = decode_stream(&batch)
            .into_iter()
            .filter_map(|command| match command {
                Command::Kitty(frame) => Some(frame.control.quiet),
                _ => None,
            })
            .collect();

        assert_eq!(quiet, [2, 2, 2], "a delete, a re-delete, and a place");
    }
}

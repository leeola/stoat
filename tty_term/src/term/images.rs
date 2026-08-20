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
use stoatty_protocol::kitty::{
    Action, Compression, ControlData, Format, GraphicsFrame, Medium, Response, ResponseResult,
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

    /// Apply one graphics frame, returning the reply to send back.
    ///
    /// `None` means say nothing, which covers a frame that named no image to
    /// answer for, a quiet level that suppressed the reply, and a chunk that is
    /// not the last of its transmission.
    pub(crate) fn apply(&mut self, frame: GraphicsFrame) -> Option<Response> {
        let control = frame.control;

        // A continuation carries only `m` and `q`, so it is recognized by an
        // open transmission rather than by anything in the frame itself.
        if self.pending.is_some() && is_continuation(&control) {
            return self.continue_transmission(control, frame.payload);
        }

        // Anything else arriving mid-transmission means the client abandoned it,
        // and the partial payload describes no image.
        if self.pending.take().is_some() {
            return respond(
                &control,
                error("EINVAL", "graphics command during a transmission"),
            );
        }

        match control.action {
            Action::Transmit | Action::TransmitAndDisplay | Action::Query => {
                self.begin_transmission(control, frame.payload)
            },
            // The placement half of the protocol is not built yet. Answering
            // rather than ignoring is what lets a client stop waiting.
            Action::Put | Action::Delete => {
                respond(&control, error("EINVAL", "unsupported action"))
            },
        }
    }

    /// Take the first frame of a transmission, decoding it when it is also the
    /// last.
    fn begin_transmission(&mut self, control: ControlData, payload: Vec<u8>) -> Option<Response> {
        if control.medium != Medium::Direct {
            return respond(&control, error("EBADF", "unsupported transmission medium"));
        }
        if payload.len() > MAX_TRANSMIT_BYTES {
            return respond(&control, error("EFBIG", "transmission too large"));
        }

        match control.more {
            true => {
                self.pending = Some(Pending { control, payload });
                None
            },
            false => self.finish(control, payload),
        }
    }

    /// Take a continuation chunk, decoding once one reports no more follow.
    fn continue_transmission(
        &mut self,
        control: ControlData,
        payload: Vec<u8>,
    ) -> Option<Response> {
        let pending = self.pending.as_mut().expect("checked by the caller");
        if pending.payload.len() + payload.len() > MAX_TRANSMIT_BYTES {
            let control = self.pending.take().expect("checked by the caller").control;
            return respond(&control, error("EFBIG", "transmission too large"));
        }
        pending.payload.extend_from_slice(&payload);

        if control.more {
            return None;
        }

        let pending = self.pending.take().expect("checked by the caller");
        // The opening frame's control data describes the image. The
        // continuation's only says the stream ended, and its quiet level is the
        // one the client repeated on every chunk.
        self.finish(pending.control, pending.payload)
    }

    /// Decode a complete payload and store it, or answer why not.
    fn finish(&mut self, control: ControlData, payload: Vec<u8>) -> Option<Response> {
        let decoded = match decode(&control, &payload) {
            Ok(decoded) => decoded,
            Err(response) => return respond(&control, response),
        };

        // A query asks whether this would work, so having decoded it is the
        // whole answer and the pixels are dropped.
        if control.action == Action::Query {
            return respond(&control, ResponseResult::Ok);
        }

        let id = match control.id {
            0 => self.assign_id(),
            id => id,
        };
        match self.store(id, decoded) {
            Ok(()) => respond_as(&control, id, ResponseResult::Ok),
            Err(response) => respond_as(&control, id, response),
        }
    }

    /// Put `image` under `id`, evicting to stay inside the quota.
    fn store(&mut self, id: u32, image: DecodedImage) -> Result<(), ResponseResult> {
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
    for pixel in bytes.chunks_exact(3) {
        rgba.extend_from_slice(pixel);
        rgba.push(0xff);
    }

    Ok(DecodedImage {
        rgba: rgba.into(),
        width,
        height,
        generation: 0,
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
    use super::{ImageStore, MAX_DECODED_BYTES, MAX_IMAGES};
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
            store.apply(frame(control, base64(&png_2x2([1, 2, 3, 255])))),
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
            store.apply(frame(control, base64(&[9, 8, 7, 6, 5, 4]))),
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

        assert_eq!(store.apply(frame(control, base64(&deflated))), ok(2));
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
            store.apply(frame(control, head.to_vec())),
            None,
            "a chunk that is not the last says nothing",
        );
        assert_eq!(stored(&store, 3), None, "and stores nothing yet");

        let continuation = ControlData {
            more: true,
            ..ControlData::default()
        };
        assert_eq!(
            store.apply(frame(continuation, middle.to_vec())),
            None,
            "nor does a middle chunk, which carries only m and q",
        );

        let last = ControlData {
            more: false,
            ..ControlData::default()
        };
        assert_eq!(
            store.apply(frame(last, tail.to_vec())),
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
        store.apply(frame(control, base64(&png_2x2([1, 1, 1, 255]))));

        let interrupting = ControlData {
            format: Format::Rgba,
            width: 1,
            height: 1,
            ..transmit(5)
        };
        let response = store.apply(frame(interrupting, base64(&[1, 2, 3, 4])));

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
                code(&store.apply(frame(control, Vec::new()))),
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
            code(&store.apply(frame(control, Vec::new()))),
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
            code(&store.apply(frame(control, base64(&[0; 8])))),
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

        assert!(store.apply(frame(good(0), png.clone())).is_some());
        assert_eq!(
            store.apply(frame(good(1), png)),
            None,
            "quiet 1 drops the success reply",
        );

        assert!(store.apply(frame(bad(1), Vec::new())).is_some());
        assert_eq!(
            store.apply(frame(bad(2), Vec::new())),
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

        assert_eq!(store.apply(frame(control, base64(&png_2x2([3; 4])))), None);
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

        let response = store
            .apply(frame(control, base64(&png_2x2([4; 4]))))
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
        assert_eq!(store.apply(frame(control, payload)), ok(13));
        assert_eq!(
            stored(&store, 13),
            None,
            "a validated image is still discarded"
        );

        let (control, payload) = query(b"not base64 at all!!".to_vec());
        assert_eq!(
            code(&store.apply(frame(control, payload))),
            Some("EINVAL"),
            "and a bad one reports why rather than storing it",
        );
    }

    /// The placement actions are a separate concern. Answering rather than
    /// ignoring is what stops a client waiting on a reply that never comes.
    #[test]
    fn the_placement_actions_report_that_they_are_unsupported() {
        let mut store = ImageStore::new();

        for action in [Action::Put, Action::Delete] {
            let control = ControlData {
                action,
                ..transmit(14)
            };
            assert_eq!(
                code(&store.apply(frame(control, Vec::new()))),
                Some("EINVAL")
            );
        }
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
            store.apply(frame(control, base64(&png_2x2([6; 4])))),
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

        store.apply(frame(control(), base64(&png_2x2([7, 7, 7, 255]))));
        let first = stored(&store, 16).expect("stored").generation;

        store.apply(frame(control(), base64(&png_2x2([8, 8, 8, 255]))));
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
            store.apply(frame(control, png.clone()))
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
}

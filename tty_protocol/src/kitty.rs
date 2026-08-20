//! The Kitty graphics protocol's wire format.
//!
//! A client sends an image as `ESC _ G <k=v,...> ; <base64> ESC \`, the control
//! data saying what to do with the payload and where to put it. The frame shares
//! its introducer and its leading `G` with the stoatty sub-protocol, but the two
//! cannot be confused: control data is `k=v` pairs, which `Gstoatty;...` is not,
//! and a Kitty frame carries no `Gstoatty` prefix to strip.
//!
//! The payload stays raw base64 here rather than decoding on arrival. A large
//! transmission arrives split into chunks that mean nothing individually, so the
//! terminal concatenates them and decodes once, when a chunk says no more
//! follow.

use crate::frame;

/// Largest base64 payload one frame carries, per the protocol.
///
/// A client splits anything longer across continuation frames. Emitters chunk to
/// this, and a terminal reassembles until a frame reports no more.
pub const MAX_CHUNK: usize = 4096;

/// What a graphics frame asks the terminal to do.
///
/// Covers the subset this terminal implements. The animation and compose actions
/// parse to nothing, which is how an unimplemented feature degrades here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Report whether a transmission would succeed, changing no state.
    Query,
    Transmit,
    TransmitAndDisplay,
    /// Display an image already transmitted.
    Put,
    Delete,
}

/// How the payload's bytes describe pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// Three bytes per pixel, no alpha.
    Rgb,
    Rgba,
    /// A whole PNG file, its own header carrying the dimensions.
    Png,
}

/// Where the terminal reads the image data from.
///
/// Only [`Medium::Direct`] streams the pixels in the frame itself. The other
/// three name a path or a handle the terminal opens, and this terminal refuses
/// them so a client falls back to streaming. They parse rather than being
/// dropped, because refusing one means answering it by id.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Medium {
    Direct,
    File,
    TempFile,
    SharedMemory,
}

/// How the payload is compressed under its base64.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Compression {
    Zlib,
}

/// Which placements a delete action removes.
///
/// The animation-frame target is absent, matching the actions this terminal
/// implements.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeleteKind {
    /// Every visible placement.
    All,
    /// Placements of one image, narrowed to one placement when `p` is set.
    Id,
    /// Placements of the image carrying an image number.
    Number,
    /// Placements under the cursor.
    Cursor,
    /// Placements covering the cell at `x`,`y`.
    Cell,
    /// Placements covering the cell at `x`,`y` at z-index `z`.
    CellZ,
    /// Placements covering column `x`.
    Column,
    /// Placements covering row `y`.
    Row,
    /// Placements at z-index `z`.
    Z,
    /// Every image whose id falls in the range `x..=y`, placements and all.
    IdRange,
}

/// What a delete action removes, and whether the image data goes with it.
///
/// The wire spells the pair as one letter whose case carries `free_data`, so an
/// uppercase target frees the stored image once its last placement is gone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeleteTarget {
    pub kind: DeleteKind,
    pub free_data: bool,
}

/// The `k=v` control data heading a graphics frame.
///
/// Every field carries the protocol's default when the frame omits its key, so a
/// consumer reads a value rather than an option. An unknown key is ignored, which
/// is what lets a client written against a later revision still talk to this one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ControlData {
    /// `a`
    pub action: Action,
    /// `f`
    pub format: Format,
    /// `t`
    pub medium: Medium,
    /// `s`, the transmitted image's pixel width. Ignored for [`Format::Png`],
    /// whose header carries its own.
    pub width: u32,
    /// `v`, the transmitted image's pixel height.
    pub height: u32,
    /// `S`, the byte size of the data a non-direct medium points at.
    pub size: u32,
    /// `O`, the byte offset into that data.
    pub offset: u32,
    /// `i`, the client's id for the image. Zero means the client named none, so
    /// the terminal assigns one.
    pub id: u32,
    /// `I`, an image number the terminal resolves to an id, for a client that
    /// would rather not track ids itself.
    pub number: u32,
    /// `p`, the client's id for this placement of the image. Zero means the
    /// client named none.
    pub placement: u32,
    /// `q`, how much of the response to suppress. 1 drops the success reply, 2
    /// drops errors too.
    pub quiet: u8,
    /// `m`, whether a continuation frame follows this one.
    pub more: bool,
    /// `o`
    pub compression: Option<Compression>,
    /// `x`, the left edge of the source rectangle to display, in pixels.
    pub src_x: u32,
    /// `y`, its top edge.
    pub src_y: u32,
    /// `w`, its width. Zero means the rest of the image.
    pub src_w: u32,
    /// `h`, its height. Zero means the rest of the image.
    pub src_h: u32,
    /// `c`, how many columns to scale the placement across. Zero means as many
    /// as the pixels need.
    pub cols: u32,
    /// `r`, how many rows. Zero means as many as the pixels need.
    pub rows: u32,
    /// `z`, where the placement sits relative to the text. Negative puts it
    /// behind, which is why this is signed.
    pub z: i32,
    /// `X`, the placement's pixel offset inside its starting cell.
    pub cell_x: u32,
    /// `Y`, its vertical offset inside that cell.
    pub cell_y: u32,
    /// `C`, whether the cursor stays put. Non-zero leaves it where it was.
    pub cursor_policy: u8,
    /// `d`
    pub delete: DeleteTarget,
}

impl Default for ControlData {
    fn default() -> Self {
        Self {
            action: Action::Transmit,
            format: Format::Rgba,
            medium: Medium::Direct,
            width: 0,
            height: 0,
            size: 0,
            offset: 0,
            id: 0,
            number: 0,
            placement: 0,
            quiet: 0,
            more: false,
            compression: None,
            src_x: 0,
            src_y: 0,
            src_w: 0,
            src_h: 0,
            cols: 0,
            rows: 0,
            z: 0,
            cell_x: 0,
            cell_y: 0,
            cursor_policy: 0,
            delete: DeleteTarget {
                kind: DeleteKind::All,
                free_data: false,
            },
        }
    }
}

/// One graphics frame: its control data and its payload, still base64.
///
/// The payload is left encoded because a chunked transmission's pieces only
/// decode correctly once joined. See the module docs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GraphicsFrame {
    pub control: ControlData,
    pub payload: Vec<u8>,
}

/// How a terminal answered a frame that asked for a response.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ResponseResult {
    Ok,
    /// An error code and its message, as `E<CODE>:<msg>` on the wire.
    Error {
        code: String,
        message: String,
    },
}

/// A terminal's reply to a graphics frame, naming what it answers for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Response {
    pub id: u32,
    pub number: u32,
    pub placement: u32,
    pub result: ResponseResult,
}

/// Parse a Kitty graphics frame, or `None` if `bytes` is not one.
///
/// Accepts the whole frame or the bare payload a VT parser yields after
/// stripping the introducer and terminator, matching the stoatty decoder.
///
/// `None` covers a frame that is not Kitty graphics at all, malformed control
/// data, and an action or format outside what this terminal implements. That
/// last case is the same degrade-to-nothing the stoatty decoder makes for a
/// sub-command it does not know.
pub fn parse(bytes: &[u8]) -> Option<GraphicsFrame> {
    let body = frame::strip_wrapper(bytes).strip_prefix(b"G")?;

    let (control, payload) = match body.iter().position(|&byte| byte == b';') {
        Some(at) => (&body[..at], body[at + 1..].to_vec()),
        None => (body, Vec::new()),
    };

    Some(GraphicsFrame {
        control: parse_control(control)?,
        payload,
    })
}

/// Write `ctrl` and `data` as one graphics frame.
///
/// `data` must already be base64 and no longer than [`MAX_CHUNK`]. Use
/// [`encode_chunked_into`] for anything larger.
pub fn encode_into(out: &mut Vec<u8>, ctrl: &ControlData, data: &[u8]) {
    out.extend_from_slice(b"\x1b_G");
    write_control(out, ctrl);
    out.push(b';');
    out.extend_from_slice(data);
    out.extend_from_slice(b"\x1b\\");
}

/// Write `ctrl` and `data` as however many frames the payload needs.
///
/// The first frame carries the full control data, and each continuation carries
/// only `m` and `q`, because the terminal already has the rest from the frame
/// that opened the transmission. Every frame but the last reports that more
/// follow.
///
/// An empty payload still emits one frame, since an action like a delete or a
/// put says everything it needs in its control data.
pub fn encode_chunked_into(out: &mut Vec<u8>, ctrl: &ControlData, data: &[u8]) {
    if data.len() <= MAX_CHUNK {
        let mut ctrl = *ctrl;
        ctrl.more = false;
        encode_into(out, &ctrl, data);
        return;
    }

    let mut chunks = data.chunks(MAX_CHUNK).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = chunks.peek().is_some();
        match first {
            true => {
                let mut head = *ctrl;
                head.more = more;
                encode_into(out, &head, chunk);
                first = false;
            },
            false => {
                out.extend_from_slice(b"\x1b_G");
                let mut wrote = false;
                write_pair(out, &mut wrote, "m", u32::from(more));
                write_pair(out, &mut wrote, "q", u32::from(ctrl.quiet));
                out.push(b';');
                out.extend_from_slice(chunk);
                out.extend_from_slice(b"\x1b\\");
            },
        }
    }
}

/// Write a terminal's reply to a frame.
///
/// The reply names the image it answers for so a client with several in flight
/// can match it. A zero id, number, or placement is left out, since zero is what
/// the wire means by absent.
pub fn encode_response_into(
    out: &mut Vec<u8>,
    id: u32,
    number: u32,
    placement: u32,
    result: &ResponseResult,
) {
    out.extend_from_slice(b"\x1b_G");

    let mut wrote = false;
    for (key, value) in [("i", id), ("I", number), ("p", placement)] {
        if value != 0 {
            write_pair(out, &mut wrote, key, value);
        }
    }

    out.push(b';');
    match result {
        ResponseResult::Ok => out.extend_from_slice(b"OK"),
        ResponseResult::Error { code, message } => {
            out.push(b'E');
            out.extend_from_slice(code.as_bytes());
            out.push(b':');
            out.extend_from_slice(message.as_bytes());
        },
    }
    out.extend_from_slice(b"\x1b\\");
}

/// Parse a terminal's reply, or `None` if `bytes` is not one.
///
/// The counterpart to [`encode_response_into`], for an emitter reading what the
/// terminal said and for tests asserting their own output.
pub fn parse_response(bytes: &[u8]) -> Option<Response> {
    let body = frame::strip_wrapper(bytes).strip_prefix(b"G")?;
    let at = body.iter().position(|&byte| byte == b';')?;
    let (control, message) = (&body[..at], &body[at + 1..]);

    let mut response = Response {
        id: 0,
        number: 0,
        placement: 0,
        result: ResponseResult::Ok,
    };
    for (key, value) in pairs(control) {
        let value: u32 = value.parse().ok()?;
        match key {
            "i" => response.id = value,
            "I" => response.number = value,
            "p" => response.placement = value,
            _ => {},
        }
    }

    let message = std::str::from_utf8(message).ok()?;
    response.result = match message.strip_prefix('E') {
        None if message == "OK" => ResponseResult::Ok,
        None => return None,
        Some(rest) => {
            let (code, message) = rest.split_once(':')?;
            ResponseResult::Error {
                code: code.to_owned(),
                message: message.to_owned(),
            }
        },
    };
    Some(response)
}

/// Read the `k=v` pairs of a control-data run, skipping malformed ones.
///
/// A pair with no `=` cannot be control data at all, so [`parse_control`] uses
/// its absence to tell a Kitty frame from a stoatty one.
fn pairs(control: &[u8]) -> impl Iterator<Item = (&str, &str)> {
    control
        .split(|&byte| byte == b',')
        .filter(|field| !field.is_empty())
        .filter_map(|field| std::str::from_utf8(field).ok())
        .filter_map(|field| field.split_once('='))
}

/// Build the control data from its wire pairs, or `None` if this is not a Kitty
/// frame or names something this terminal does not implement.
fn parse_control(control: &[u8]) -> Option<ControlData> {
    // Empty control data is a legal frame of pure defaults, but a non-empty run
    // holding no `k=v` pair at all is another protocol's payload wearing the
    // same introducer.
    if !control.is_empty() && pairs(control).next().is_none() {
        return None;
    }

    let mut out = ControlData::default();
    for (key, value) in pairs(control) {
        let number = || value.parse::<u32>().ok();
        let letter = || value.chars().next().filter(|_| value.chars().count() == 1);

        match key {
            "a" => {
                out.action = match letter()? {
                    'q' => Action::Query,
                    't' => Action::Transmit,
                    'T' => Action::TransmitAndDisplay,
                    'p' => Action::Put,
                    'd' => Action::Delete,
                    _ => return None,
                }
            },
            "f" => {
                out.format = match number()? {
                    24 => Format::Rgb,
                    32 => Format::Rgba,
                    100 => Format::Png,
                    _ => return None,
                }
            },
            "t" => {
                out.medium = match letter()? {
                    'd' => Medium::Direct,
                    'f' => Medium::File,
                    't' => Medium::TempFile,
                    's' => Medium::SharedMemory,
                    _ => return None,
                }
            },
            "o" => {
                out.compression = match letter()? {
                    'z' => Some(Compression::Zlib),
                    _ => return None,
                }
            },
            "d" => {
                let letter = letter()?;
                out.delete = DeleteTarget {
                    kind: match letter.to_ascii_lowercase() {
                        'a' => DeleteKind::All,
                        'i' => DeleteKind::Id,
                        'n' => DeleteKind::Number,
                        'c' => DeleteKind::Cursor,
                        'p' => DeleteKind::Cell,
                        'q' => DeleteKind::CellZ,
                        'x' => DeleteKind::Column,
                        'y' => DeleteKind::Row,
                        'z' => DeleteKind::Z,
                        'r' => DeleteKind::IdRange,
                        _ => return None,
                    },
                    free_data: letter.is_ascii_uppercase(),
                }
            },
            "s" => out.width = number()?,
            "v" => out.height = number()?,
            "S" => out.size = number()?,
            "O" => out.offset = number()?,
            "i" => out.id = number()?,
            "I" => out.number = number()?,
            "p" => out.placement = number()?,
            "q" => out.quiet = number()?.min(u32::from(u8::MAX)) as u8,
            "m" => out.more = number()? != 0,
            "x" => out.src_x = number()?,
            "y" => out.src_y = number()?,
            "w" => out.src_w = number()?,
            "h" => out.src_h = number()?,
            "c" => out.cols = number()?,
            "r" => out.rows = number()?,
            "z" => out.z = value.parse().ok()?,
            "X" => out.cell_x = number()?,
            "Y" => out.cell_y = number()?,
            "C" => out.cursor_policy = number()?.min(u32::from(u8::MAX)) as u8,
            _ => {},
        }
    }
    Some(out)
}

/// Write the control data, leaving out every key still at its default.
///
/// A frame carries what differs from the protocol's defaults, so the common
/// transmission stays short on the wire.
fn write_control(out: &mut Vec<u8>, ctrl: &ControlData) {
    let default = ControlData::default();
    let mut wrote = false;

    if ctrl.action != default.action {
        write_letter(
            out,
            &mut wrote,
            "a",
            match ctrl.action {
                Action::Query => 'q',
                Action::Transmit => 't',
                Action::TransmitAndDisplay => 'T',
                Action::Put => 'p',
                Action::Delete => 'd',
            },
        );
    }
    if ctrl.format != default.format {
        write_pair(
            out,
            &mut wrote,
            "f",
            match ctrl.format {
                Format::Rgb => 24,
                Format::Rgba => 32,
                Format::Png => 100,
            },
        );
    }
    if ctrl.medium != default.medium {
        write_letter(
            out,
            &mut wrote,
            "t",
            match ctrl.medium {
                Medium::Direct => 'd',
                Medium::File => 'f',
                Medium::TempFile => 't',
                Medium::SharedMemory => 's',
            },
        );
    }
    if let Some(Compression::Zlib) = ctrl.compression {
        write_letter(out, &mut wrote, "o", 'z');
    }
    if ctrl.delete != default.delete {
        let letter = match ctrl.delete.kind {
            DeleteKind::All => 'a',
            DeleteKind::Id => 'i',
            DeleteKind::Number => 'n',
            DeleteKind::Cursor => 'c',
            DeleteKind::Cell => 'p',
            DeleteKind::CellZ => 'q',
            DeleteKind::Column => 'x',
            DeleteKind::Row => 'y',
            DeleteKind::Z => 'z',
            DeleteKind::IdRange => 'r',
        };
        let letter = match ctrl.delete.free_data {
            true => letter.to_ascii_uppercase(),
            false => letter,
        };
        write_letter(out, &mut wrote, "d", letter);
    }

    for (key, value, default) in [
        ("s", ctrl.width, default.width),
        ("v", ctrl.height, default.height),
        ("S", ctrl.size, default.size),
        ("O", ctrl.offset, default.offset),
        ("i", ctrl.id, default.id),
        ("I", ctrl.number, default.number),
        ("p", ctrl.placement, default.placement),
        ("q", u32::from(ctrl.quiet), u32::from(default.quiet)),
        ("m", u32::from(ctrl.more), u32::from(default.more)),
        ("x", ctrl.src_x, default.src_x),
        ("y", ctrl.src_y, default.src_y),
        ("w", ctrl.src_w, default.src_w),
        ("h", ctrl.src_h, default.src_h),
        ("c", ctrl.cols, default.cols),
        ("r", ctrl.rows, default.rows),
        ("X", ctrl.cell_x, default.cell_x),
        ("Y", ctrl.cell_y, default.cell_y),
        (
            "C",
            u32::from(ctrl.cursor_policy),
            u32::from(default.cursor_policy),
        ),
    ] {
        if value != default {
            write_pair(out, &mut wrote, key, value);
        }
    }

    if ctrl.z != default.z {
        separate(out, &mut wrote);
        out.extend_from_slice(b"z=");
        out.extend_from_slice(ctrl.z.to_string().as_bytes());
    }
}

fn write_pair(out: &mut Vec<u8>, wrote: &mut bool, key: &str, value: u32) {
    separate(out, wrote);
    out.extend_from_slice(key.as_bytes());
    out.push(b'=');
    out.extend_from_slice(value.to_string().as_bytes());
}

fn write_letter(out: &mut Vec<u8>, wrote: &mut bool, key: &str, value: char) {
    separate(out, wrote);
    out.extend_from_slice(key.as_bytes());
    out.push(b'=');
    out.push(value as u8);
}

fn separate(out: &mut Vec<u8>, wrote: &mut bool) {
    if *wrote {
        out.push(b',');
    }
    *wrote = true;
}

#[cfg(test)]
mod tests {
    use super::{
        encode_chunked_into, encode_into, encode_response_into, parse, parse_response, Action,
        Compression, ControlData, DeleteKind, DeleteTarget, Format, Medium, ResponseResult,
        MAX_CHUNK,
    };

    fn frame(control: &str, payload: &str) -> Vec<u8> {
        format!("\x1b_G{control};{payload}\x1b\\").into_bytes()
    }

    #[test]
    fn an_omitted_key_reads_as_its_protocol_default() {
        let parsed = parse(&frame("", "")).expect("empty control data is a legal frame");

        assert_eq!(
            parsed.control,
            ControlData::default(),
            "a frame naming nothing carries the whole default set"
        );
        assert_eq!(
            (parsed.control.action, parsed.control.format),
            (Action::Transmit, Format::Rgba),
            "the two defaults a client relies on most"
        );
    }

    #[test]
    fn every_supported_key_reaches_its_field() {
        let control = "a=T,f=24,t=f,s=10,v=20,S=30,O=40,i=5,I=6,p=7,q=2,m=1,o=z,\
                       x=1,y=2,w=3,h=4,c=8,r=9,z=-3,X=11,Y=12,C=1,d=I";
        let parsed = parse(&frame(control, "Zm9v")).expect("control data parses");

        assert_eq!(
            parsed.control,
            ControlData {
                action: Action::TransmitAndDisplay,
                format: Format::Rgb,
                medium: Medium::File,
                width: 10,
                height: 20,
                size: 30,
                offset: 40,
                id: 5,
                number: 6,
                placement: 7,
                quiet: 2,
                more: true,
                compression: Some(Compression::Zlib),
                src_x: 1,
                src_y: 2,
                src_w: 3,
                src_h: 4,
                cols: 8,
                rows: 9,
                z: -3,
                cell_x: 11,
                cell_y: 12,
                cursor_policy: 1,
                delete: DeleteTarget {
                    kind: DeleteKind::Id,
                    free_data: true,
                },
            },
        );
        assert_eq!(
            parsed.payload, b"Zm9v",
            "the payload stays base64, since a chunk decodes correctly only joined",
        );
    }

    /// A client written against a later revision must still talk to this one, so
    /// a key this build never heard of is skipped rather than failing the frame.
    #[test]
    fn an_unknown_key_is_ignored() {
        let parsed = parse(&frame("i=9,zz=1,U=7", "")).expect("unknown keys do not fail the frame");

        assert_eq!(parsed.control.id, 9, "the keys around it still land");
    }

    /// Refusing a medium means answering it by id, so an unsupported one has to
    /// survive parsing. An unsupported action or format has nothing to answer.
    #[test]
    fn an_unsupported_medium_parses_but_an_unsupported_action_does_not() {
        assert_eq!(
            parse(&frame("t=s,i=4", "")).map(|f| (f.control.medium, f.control.id)),
            Some((Medium::SharedMemory, 4)),
        );
        assert_eq!(
            parse(&frame("a=c", "")),
            None,
            "compose is outside this build, so the frame degrades to nothing",
        );
        assert_eq!(parse(&frame("f=8", "")), None, "as does an unknown format");
    }

    /// The stoatty sub-protocol rides the same introducer and leading `G`, so
    /// its frames must not read as graphics control data.
    #[test]
    fn a_stoatty_frame_is_not_a_graphics_frame() {
        assert_eq!(parse(&frame("stoatty", "border")), None);
        assert_eq!(parse(b"\x1b_Gstoatty;border;AAA\x1b\\"), None);
        assert_eq!(parse(b"\x1b_Xa=t;AAA\x1b\\"), None, "nor is a foreign APC");
    }

    #[test]
    fn a_frame_round_trips_through_parse() {
        let control = ControlData {
            action: Action::Put,
            format: Format::Png,
            id: 12,
            placement: 3,
            z: -1,
            cols: 4,
            rows: 2,
            delete: DeleteTarget {
                kind: DeleteKind::Cell,
                free_data: false,
            },
            ..ControlData::default()
        };

        let mut out = Vec::new();
        encode_into(&mut out, &control, b"cGF5");

        assert_eq!(
            parse(&out).expect("own output parses"),
            super::GraphicsFrame {
                control,
                payload: b"cGF5".to_vec(),
            },
        );
    }

    /// A transmission longer than one frame carries splits across continuations,
    /// and only the joined payload decodes to the original image.
    #[test]
    fn a_chunked_payload_rejoins_across_its_frames() {
        let payload: Vec<u8> = std::iter::repeat_n(b'A', MAX_CHUNK * 2 + 7).collect();
        let control = ControlData {
            action: Action::TransmitAndDisplay,
            id: 3,
            width: 64,
            height: 64,
            ..ControlData::default()
        };

        let mut out = Vec::new();
        encode_chunked_into(&mut out, &control, &payload);

        let mut frames = Vec::new();
        let mut rest = out.as_slice();
        while let Some(span) = crate::frame::apc_span(rest) {
            frames.push(parse(&rest[span.clone()]).expect("each emitted frame parses"));
            rest = &rest[span.end..];
        }

        assert_eq!(frames.len(), 3, "two full chunks and the remainder");
        assert_eq!(
            frames.iter().map(|f| f.control.more).collect::<Vec<_>>(),
            [true, true, false],
            "every frame but the last says another follows",
        );
        assert_eq!(
            frames[0].control.id, 3,
            "the opening frame carries the full control data",
        );
        assert_eq!(
            (frames[1].control.id, frames[1].control.width),
            (0, 0),
            "a continuation carries only m and q, the terminal holding the rest",
        );
        assert_eq!(
            frames
                .iter()
                .flat_map(|f| f.payload.clone())
                .collect::<Vec<_>>(),
            payload,
            "the joined chunks are the payload that went in",
        );
    }

    #[test]
    fn a_payload_that_fits_emits_one_frame_reporting_no_more() {
        let mut out = Vec::new();
        encode_chunked_into(
            &mut out,
            &ControlData {
                more: true,
                ..ControlData::default()
            },
            b"short",
        );

        let parsed = parse(&out).expect("one frame");
        assert!(
            !parsed.control.more,
            "nothing follows, whatever the caller's control data claimed"
        );
    }

    #[test]
    fn a_response_round_trips() {
        for (id, number, placement, result) in [
            (7, 0, 2, ResponseResult::Ok),
            (
                0,
                4,
                0,
                ResponseResult::Error {
                    code: "ENOENT".to_owned(),
                    message: "no such file".to_owned(),
                },
            ),
        ] {
            let mut out = Vec::new();
            encode_response_into(&mut out, id, number, placement, &result);

            assert_eq!(
                parse_response(&out).expect("own response parses"),
                super::Response {
                    id,
                    number,
                    placement,
                    result,
                },
            );
        }
    }

    /// The delete table has to survive a round trip whole, since a selector the
    /// emitter cannot write is one the terminal will never be asked for.
    #[test]
    fn every_delete_selector_round_trips_in_both_cases() {
        use super::{encode_into, DeleteKind};

        let kinds = [
            DeleteKind::All,
            DeleteKind::Id,
            DeleteKind::Number,
            DeleteKind::Cursor,
            DeleteKind::Cell,
            DeleteKind::CellZ,
            DeleteKind::Column,
            DeleteKind::Row,
            DeleteKind::Z,
            DeleteKind::IdRange,
        ];

        for kind in kinds {
            for free_data in [false, true] {
                let control = ControlData {
                    action: Action::Delete,
                    delete: DeleteTarget { kind, free_data },
                    ..ControlData::default()
                };

                let mut out = Vec::new();
                encode_into(&mut out, &control, b"");
                assert_eq!(
                    parse(&out).expect("own output parses").control.delete,
                    DeleteTarget { kind, free_data },
                    "{kind:?} with free_data {free_data}",
                );
            }
        }
    }
}

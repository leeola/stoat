//! The popover command draws a floating box above the grid.
//!
//! Its content streams between the open and close markers rather than riding in
//! the head, so a line of text longer than one argument still arrives whole.

use crate::frame;

/// Draw a floating popover region above the grid.
///
/// A `width` by `height` cell box anchored at (`top`, `left`) in absolute grid
/// coordinates, filled with `fill` and outlined with `border` in the shape
/// `chrome` gives it. The region floats above the cells with its own z-order.
///
/// `content` is a line of text drawn inside the box in `content_fg`, drawn at
/// `scale` times the cell size, clipped to the box. It starts one cell in from
/// the box's top-left, or sits centred in the box when `center` is set.
///
/// The content is generic over its container so an emitter re-declaring a
/// popover every frame can hand a borrowed slice, while a decoded command owns
/// its copy. It defaults to the owned form, so a holder that is not encoding
/// writes the type with no parameter. A scope encoder takes its content from a
/// closure instead and reads only the head fields, so `PopoverCommand<()>`
/// names a head with no content of its own. The spans are generic the same
/// way.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PopoverCommand<S = String, P = Vec<PopoverSpan>> {
    pub top: u16,
    pub left: u16,
    pub width: u16,
    pub height: u16,
    pub fill: [u8; 3],
    pub border: [u8; 3],
    pub content_fg: [u8; 3],
    /// Integer multiple of the cell size the content text is drawn at, so a
    /// tooltip can render larger or smaller than the grid. A scale of 1 matches
    /// the grid metrics.
    pub scale: u8,
    /// Signed `[x, y]` pixel offset from the anchor cell, so a tooltip can sit
    /// exactly under a span rather than snapping to the cell grid. The box, its
    /// shadow, its content, and the content clip all shift by this offset.
    pub offset: [i16; 2],
    /// Shape the content text at bold weight rather than the default. Only the
    /// content is affected. The box chrome is unchanged.
    pub bold: bool,
    /// The box's corner radius, border width, and drop shadow.
    pub chrome: PopoverChrome,
    /// Centre the content block in the box by pixel, with no inset from the
    /// box's edge. The block is the content's longest line by its line count,
    /// at `scale`.
    pub center: bool,
    pub content: S,
    /// Runs of the content drawn in a color and weight of their own, over
    /// `content_fg` and `bold`.
    ///
    /// At most [`MAX_POPOVER_SPANS`] are sent, and the rest are dropped. A
    /// terminal that reads only the head ignores them, so the content keeps
    /// its one color there.
    pub spans: P,
}

/// A run of popover content drawn in its own color and weight.
///
/// `line` counts content lines from the first, and `start` counts characters
/// from the start of that line. The `len` characters from there draw in `fg`,
/// at bold weight when `bold` is set. A character no span covers draws in the
/// popover's `content_fg`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PopoverSpan {
    pub line: u16,
    pub start: u16,
    pub len: u16,
    pub fg: [u8; 3],
    pub bold: bool,
}

/// The shape of a popover's box: its corner radius, its border, and the drop
/// shadow it casts.
///
/// Each length is in logical px, which the terminal scales to the display. A
/// tooltip or a hover card takes [`Self::CARD`], and a popover that a design
/// board draws states the board's values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PopoverChrome {
    /// The radius of the box's corners. The terminal clamps it to half the
    /// box's shorter side.
    pub corner_radius: u8,
    /// The width of the border band inside the box's edge, at least one
    /// physical pixel.
    pub border_width: u8,
    /// The `[x, y]` displacement of the shadow from the box, positive right
    /// and down.
    pub shadow_offset: [i8; 2],
    /// The distance the shadow fades over past its rectangle, three sigma of
    /// its gaussian blur.
    pub shadow_margin: u8,
    /// The shadow's peak alpha, of 255, where the blur covers its rectangle
    /// whole.
    pub shadow_alpha: u8,
}

impl PopoverChrome {
    /// The chrome of a tooltip or a hover card.
    ///
    /// It rounds at 2 px with a 1 px border and casts the drop shadow of an
    /// overlay panel, 5 px right and 7 px down, which fades over 16 px from a
    /// peak alpha of 82.
    pub const CARD: PopoverChrome = PopoverChrome {
        corner_radius: 2,
        border_width: 1,
        shadow_offset: [5, 7],
        shadow_margin: 16,
        shadow_alpha: 82,
    };
}

/// The most spans one popover carries, which its one count byte bounds.
pub const MAX_POPOVER_SPANS: usize = u8::MAX as usize;

/// Bytes the head takes before its spans.
const HEAD_BYTES: usize = 30;

/// Bytes each [`PopoverSpan`] takes after the head.
const SPAN_BYTES: usize = 10;

/// Encode a [`PopoverCommand`] as a full `Gstoatty;popover` frame for an emitter.
///
/// The region, colors, scale, offset, bold flag, chrome, and centering flag
/// ride in a fixed 30-byte argument on the open marker. Spans, when the
/// popover has any, follow in the same argument as a count byte and 10 bytes a
/// span. The content streams between the markers.
pub fn encode_popover<S: AsRef<str>, P: AsRef<[PopoverSpan]>>(
    command: &PopoverCommand<S, P>,
) -> Vec<u8> {
    let mut out = Vec::new();
    encode_popover_into(&mut out, command);
    out
}

/// Append a `Gstoatty;popover` open marker, its streamed `content`, and a
/// `Gstoatty;popover_end` close marker to `out`.
///
/// The fixed head fields ride in the open marker's single argument, and
/// `command.content` streams as the raw bytes between the two markers, so it is
/// not bounded by the per-frame size cap. The content's container is generic, so
/// an emitter passes a slice of its own buffer rather than building an owned
/// [`String`] per frame.
///
/// The content must be plain text. Riding outside the APC wrapper is what frees
/// it from the size cap and is also what makes it indistinguishable from
/// terminal control bytes, so the terminal cuts the capture at the first `ESC`
/// and keeps only what came before. An emitter that wants styled content sets
/// the colors in the head fields rather than with escape sequences in the text.
pub fn encode_popover_into<S: AsRef<str>, P: AsRef<[PopoverSpan]>>(
    out: &mut Vec<u8>,
    command: &PopoverCommand<S, P>,
) {
    let content = command.content.as_ref();
    encode_popover_scope(out, command, |out| {
        out.extend_from_slice(content.as_bytes())
    });
}

/// Append a whole popover to `out`, being the open marker, whatever `content`
/// writes, then the close marker.
///
/// The streaming form of [`encode_popover_into`], for a caller that formats its
/// text into the output buffer rather than holding it as one string. Both emit
/// the same bytes and neither has a way to leave the close marker out, which
/// matters because an unclosed popover keeps capturing until the next marker
/// and swallows whatever the emitter writes next.
///
/// Only `command`'s head fields and spans are read, so its content container
/// is unconstrained. Write `PopoverCommand<()>` with `content: ()` to say the
/// closure supplies the text.
///
/// The plain-text rule applies here too. The terminal cuts the capture at the
/// first `ESC`, so `content` must write text and set its colors through the
/// head fields.
pub fn encode_popover_scope<S, P: AsRef<[PopoverSpan]>>(
    out: &mut Vec<u8>,
    command: &PopoverCommand<S, P>,
    content: impl FnOnce(&mut Vec<u8>),
) {
    frame::begin(out, "popover");
    frame::push_arg(out, |w| {
        w.write_all(&command.top.to_be_bytes())?;
        w.write_all(&command.left.to_be_bytes())?;
        w.write_all(&command.width.to_be_bytes())?;
        w.write_all(&command.height.to_be_bytes())?;
        w.write_all(&command.fill)?;
        w.write_all(&command.border)?;
        w.write_all(&command.content_fg)?;
        w.write_all(&[command.scale])?;
        w.write_all(&command.offset[0].to_be_bytes())?;
        w.write_all(&command.offset[1].to_be_bytes())?;
        w.write_all(&[command.bold as u8])?;
        let chrome = command.chrome;
        w.write_all(&[
            chrome.corner_radius,
            chrome.border_width,
            chrome.shadow_offset[0] as u8,
            chrome.shadow_offset[1] as u8,
            chrome.shadow_margin,
            chrome.shadow_alpha,
            command.center as u8,
        ])?;

        let spans = command.spans.as_ref();
        let spans = &spans[..spans.len().min(MAX_POPOVER_SPANS)];
        if spans.is_empty() {
            return Ok(());
        }
        w.write_all(&[spans.len() as u8])?;
        for span in spans {
            w.write_all(&span.line.to_be_bytes())?;
            w.write_all(&span.start.to_be_bytes())?;
            w.write_all(&span.len.to_be_bytes())?;
            w.write_all(&span.fg)?;
            w.write_all(&[span.bold as u8])?;
        }
        Ok(())
    });
    frame::end(out);
    content(out);
    encode_popover_end_into(out);
}

/// Encode a [`Command::PopoverEnd`] as a full `Gstoatty;popover_end` close-marker
/// frame.
///
/// The frame carries no arguments; receiving it commits the content streamed
/// since the matching [`Command::Popover`] into the popover's `content`.
pub fn encode_popover_end() -> Vec<u8> {
    let mut out = Vec::new();
    encode_popover_end_into(&mut out);
    out
}

/// Append an argument-less `Gstoatty;popover_end` close-marker frame to `out`.
pub fn encode_popover_end_into(out: &mut Vec<u8>) {
    frame::begin(out, "popover_end");
    frame::end(out);
}

/// Decode a `Gstoatty;popover` open marker's head. The `content` streams as the
/// bytes after this frame and is captured by the terminal between the open
/// marker and [`Command::PopoverEnd`], so it is empty here.
pub(super) fn decode_popover(args: &[Vec<u8>]) -> Option<PopoverCommand> {
    let head = args.first()?;
    let region: &[u8; HEAD_BYTES] = head.get(..HEAD_BYTES)?.try_into().ok()?;

    Some(PopoverCommand {
        top: u16::from_be_bytes([region[0], region[1]]),
        left: u16::from_be_bytes([region[2], region[3]]),
        width: u16::from_be_bytes([region[4], region[5]]),
        height: u16::from_be_bytes([region[6], region[7]]),
        fill: [region[8], region[9], region[10]],
        border: [region[11], region[12], region[13]],
        content_fg: [region[14], region[15], region[16]],
        scale: region[17],
        offset: [
            i16::from_be_bytes([region[18], region[19]]),
            i16::from_be_bytes([region[20], region[21]]),
        ],
        bold: region[22] != 0,
        chrome: PopoverChrome {
            corner_radius: region[23],
            border_width: region[24],
            shadow_offset: [region[25] as i8, region[26] as i8],
            shadow_margin: region[27],
            shadow_alpha: region[28],
        },
        center: region[29] != 0,
        content: String::new(),
        spans: decode_spans(&head[HEAD_BYTES..]),
    })
}

/// The spans after a popover head, a count byte and then that many spans.
///
/// A head with nothing after it carries no spans. A count past the bytes that
/// follow keeps only the whole spans those bytes hold.
fn decode_spans(tail: &[u8]) -> Vec<PopoverSpan> {
    let Some((&count, spans)) = tail.split_first() else {
        return Vec::new();
    };
    let (spans, _) = spans.as_chunks::<SPAN_BYTES>();
    spans
        .iter()
        .take(usize::from(count))
        .map(|span| PopoverSpan {
            line: u16::from_be_bytes([span[0], span[1]]),
            start: u16::from_be_bytes([span[2], span[3]]),
            len: u16::from_be_bytes([span[4], span[5]]),
            fg: [span[6], span[7], span[8]],
            bold: span[9] != 0,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{decode, Command};

    #[test]
    fn popover_end_round_trips() {
        // The popover head and its streamed content round-trip at the terminal
        // layer (the content streams between the open and popover_end markers, so
        // a single-frame decode cannot recover it); see the tty_term popover
        // tests. Here we cover the close marker.
        assert_eq!(decode(&encode_popover_end()), Some(Command::PopoverEnd));
    }

    #[test]
    fn rejects_wrong_length_popover_payload() {
        // The first arg here decodes to 3 bytes, not the 30 a popover region
        // needs, and the content arg is absent.
        assert!(decode(b"Gstoatty;popover;YWJj").is_none());
    }

    /// The bold and centering flags and the chrome come back field for field,
    /// a shadow cast up and to the left included.
    #[test]
    fn popover_head_round_trips_its_flags_and_chrome() {
        let badge = PopoverChrome {
            corner_radius: 10,
            border_width: 2,
            shadow_offset: [0, 10],
            shadow_margin: 36,
            shadow_alpha: 140,
        };
        let raised = PopoverChrome {
            shadow_offset: [-3, -4],
            ..badge
        };
        for (bold, chrome, center) in [
            (true, PopoverChrome::CARD, false),
            (false, badge, true),
            (true, raised, false),
        ] {
            let command = PopoverCommand {
                top: 1,
                left: 2,
                width: 4,
                height: 3,
                fill: [10, 20, 30],
                border: [40, 50, 60],
                content_fg: [70, 80, 90],
                scale: 2,
                offset: [4, -2],
                bold,
                chrome,
                center,
                content: String::new(),
                spans: Vec::new(),
            };
            // encode_popover emits the open marker, streamed content, then the
            // close marker. Content is empty here, so slicing off the close
            // marker leaves exactly the head frame to decode.
            let full = encode_popover(&command);
            let head = &full[..full.len() - encode_popover_end().len()];
            assert_eq!(decode(head), Some(Command::Popover(command)));
        }
    }

    /// Spans ride after the head in its argument and come back field for
    /// field, each with its own color and weight.
    #[test]
    fn popover_spans_round_trip() {
        let command = PopoverCommand {
            top: 1,
            left: 2,
            width: 30,
            height: 4,
            fill: [10, 20, 30],
            border: [40, 50, 60],
            content_fg: [70, 80, 90],
            scale: 1,
            offset: [0, 0],
            bold: false,
            chrome: PopoverChrome::CARD,
            center: false,
            content: String::new(),
            spans: vec![
                PopoverSpan {
                    line: 0,
                    start: 2,
                    len: 300,
                    fg: [1, 2, 3],
                    bold: true,
                },
                PopoverSpan {
                    line: 513,
                    start: 21,
                    len: 7,
                    fg: [4, 5, 6],
                    bold: false,
                },
            ],
        };
        let full = encode_popover(&command);
        let head = &full[..full.len() - encode_popover_end().len()];
        assert_eq!(decode(head), Some(Command::Popover(command)));
    }

    /// A popover past the cap sends the first [`MAX_POPOVER_SPANS`] spans, so
    /// its one count byte stays true to what follows it.
    #[test]
    fn spans_past_the_cap_are_dropped() {
        let span = |line| PopoverSpan {
            line,
            start: 0,
            len: 1,
            fg: [1, 2, 3],
            bold: false,
        };
        let command = PopoverCommand {
            top: 0,
            left: 0,
            width: 4,
            height: 3,
            fill: [0; 3],
            border: [0; 3],
            content_fg: [0; 3],
            scale: 1,
            offset: [0, 0],
            bold: false,
            chrome: PopoverChrome::CARD,
            center: false,
            content: String::new(),
            spans: (0..300).map(span).collect::<Vec<_>>(),
        };
        let full = encode_popover(&command);
        let head = &full[..full.len() - encode_popover_end().len()];
        let Some(Command::Popover(decoded)) = decode(head) else {
            panic!("the head decodes as a popover");
        };
        assert_eq!(decoded.spans, command.spans[..MAX_POPOVER_SPANS]);
    }

    /// The count byte bounds the spans read. A count larger than the spans that
    /// follow keeps the whole ones, so a short head never reads past its end.
    #[test]
    fn the_count_bounds_the_spans_read() {
        let span = [0, 0, 0, 1, 0, 2, 9, 9, 9, 1];
        let decoded = PopoverSpan {
            line: 0,
            start: 1,
            len: 2,
            fg: [9, 9, 9],
            bold: true,
        };
        let tail = |count: u8, rest: &[u8]| -> Vec<u8> {
            [count]
                .into_iter()
                .chain(span)
                .chain(rest.iter().copied())
                .collect()
        };

        assert_eq!(decode_spans(&tail(1, &span)), [decoded], "a count of one");
        assert_eq!(decode_spans(&tail(3, &[7, 7])), [decoded], "a short tail");
    }
}

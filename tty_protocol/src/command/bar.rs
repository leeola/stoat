//! The bar command fills a thin rectangle off the cell grid in a solid color,
//! or striped with a hatch.
//!
//! A non-cell component packs several of them into a fraction of a cell, so a
//! gutter draws its status and git marks without spending a column on each.

use crate::frame;

/// The bit of a bar's flags byte that makes it occlude as a panel's body does.
const OCCLUDES: u8 = 1;

/// The bit of a bar's flags byte that stripes it with the hatch color the
/// argument ends in.
const HATCHED: u8 = 2;

/// Bytes a `bar` argument takes, which hold the rect, the color, the flags
/// byte, the hatch color, the corner radius, and the corner mask.
const BAR_ARG: usize = 17;

/// Fill a thin rectangle off the cell grid in a solid color.
///
/// A non-cell component primitive: a gutter packs several variable-width status
/// or git bars and a hairline separator into a fraction of a cell. All four of
/// [`Self::x`], [`Self::y`], [`Self::width`], and [`Self::height`] are in
/// **sixteenths of a cell** (16 = one cell), x and width along the cell width, y
/// and height along the cell height, so a bar can be a fraction of a cell wide
/// and track live font zoom.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BarCommand {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub color: [u8; 3],
    /// The bar hides what lies under it as a panel's body does.
    ///
    /// The components declared before it draw nothing inside its rect, and a
    /// pane pool's composite leaves the rect to the live grid, so the bar holds
    /// its place while the pane glides or rests composited beneath it. This is
    /// for chrome that frames a pane from inside the pane's first row. `false`
    /// is a plain bar, which a later component and a pool composite draw over.
    /// [`BarsCommand`] carries no flag, so an occluding bar goes out on its own
    /// frame.
    pub occludes: bool,
    /// The color of a hatch over [`Self::color`], or `None` for a solid bar.
    ///
    /// The hatch is a 1 logical pixel line every 7 at 135 degrees, placed as a
    /// panel's hatch is, so the stripes run on unbroken across neighboring bars
    /// and panels. A hatched bar is a surface for other marks: it draws before
    /// every plain bar, so the plain bars and the text runs over its rect show
    /// whatever order they were declared in. [`BarsCommand`] carries no hatch.
    pub hatch: Option<[u8; 3]>,
    /// The radius the bar's corners round at, in logical pixels, or 0 for a
    /// square bar.
    ///
    /// The terminal holds the radius to the bar's shorter side, not to half of
    /// it. A bar narrower than its radius then still rounds its outer corners
    /// at the radius it shares with the bars beside it, as one share of a
    /// round-ended meter does. A caller rounds both corners of one side only
    /// at half that side or less. [`BarsCommand`] carries no radius, so its
    /// bars stay square.
    pub radius: u8,
    /// The corners that round, as the panel's mask names them: bit 0 the
    /// top-left and then clockwise, per
    /// [`CORNER_TOP_LEFT`](crate::command::panel::CORNER_TOP_LEFT). A mask of
    /// 0 rounds all four.
    pub corners: u8,
}

/// Encode a [`BarCommand`] as a full `Gstoatty;bar` frame for an emitter.
///
/// The position, size, color, a flags byte, the hatch color, the corner
/// radius, and the corner mask ride in a single fixed 17-byte argument. A bar
/// with no hatch writes the hatch color as zeros.
pub fn encode_bar(command: &BarCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_bar_into(&mut out, command);
    out
}

/// Append a `Gstoatty;bar` frame for `command` to `out` without allocating.
pub fn encode_bar_into(out: &mut Vec<u8>, command: &BarCommand) {
    frame::begin(out, "bar");
    frame::push_arg(out, |w| {
        w.write_all(&command.x.to_be_bytes())?;
        w.write_all(&command.y.to_be_bytes())?;
        w.write_all(&command.width.to_be_bytes())?;
        w.write_all(&command.height.to_be_bytes())?;
        w.write_all(&command.color)?;
        let occludes = if command.occludes { OCCLUDES } else { 0 };
        let hatched = if command.hatch.is_some() { HATCHED } else { 0 };
        w.write_all(&[occludes | hatched])?;
        w.write_all(&command.hatch.unwrap_or([0, 0, 0]))?;
        w.write_all(&[command.radius, command.corners])
    });
    frame::end(out);
}

pub(super) fn decode_bar(args: &[Vec<u8>]) -> Option<BarCommand> {
    let arg: &[u8; BAR_ARG] = args.first()?.get(..BAR_ARG)?.try_into().ok()?;

    Some(BarCommand {
        x: i16::from_be_bytes([arg[0], arg[1]]),
        y: i16::from_be_bytes([arg[2], arg[3]]),
        width: u16::from_be_bytes([arg[4], arg[5]]),
        height: u16::from_be_bytes([arg[6], arg[7]]),
        color: [arg[8], arg[9], arg[10]],
        occludes: arg[11] & OCCLUDES != 0,
        hatch: (arg[11] & HATCHED != 0).then_some([arg[12], arg[13], arg[14]]),
        radius: arg[15],
        corners: arg[16],
    })
}

/// Fill several thin rectangles of one color off the cell grid, in one frame.
///
/// A gutter draws a bar per marked row, and a frame per bar spends most of its
/// bytes on the frame itself. Each entry of [`Self::bars`] is one bar's x, y,
/// width, and height in sixteenths of a cell, as [`BarCommand`] places one, and
/// a terminal stores each entry as the [`BarCommand`] it stands for, in order.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BarsCommand {
    pub color: [u8; 3],
    pub bars: Vec<(i16, i16, u16, u16)>,
}

/// Bytes a `bars` argument spends ahead of its bars, the color and the count.
const BARS_HEAD: usize = 5;

/// Bytes one bar takes in a `bars` argument.
const BARS_ENTRY: usize = 8;

/// Encode a [`BarsCommand`] as `Gstoatty;bars` frames for an emitter.
pub fn encode_bars(command: &BarsCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_bars_into(&mut out, command);
    out
}

/// Append the `Gstoatty;bars` frames for `command` to `out`.
///
/// The terminal drops a payload past [`frame::MAX_APC_PAYLOAD`] whole, so a
/// batch past it is split across as many frames as it takes, each carrying the
/// color. A batch with no bars emits nothing.
pub fn encode_bars_into(out: &mut Vec<u8>, command: &BarsCommand) {
    let mut rest = &command.bars[..];
    while !rest.is_empty() {
        let take = frame::fitting(rest, frame::MAX_RAW_ARG - BARS_HEAD, |_| BARS_ENTRY);
        let (bars, tail) = rest.split_at(take);
        rest = tail;

        frame::begin(out, "bars");
        frame::push_arg(out, |w| {
            w.write_all(&command.color)?;
            w.write_all(&(bars.len() as u16).to_be_bytes())?;
            for (x, y, width, height) in bars {
                w.write_all(&x.to_be_bytes())?;
                w.write_all(&y.to_be_bytes())?;
                w.write_all(&width.to_be_bytes())?;
                w.write_all(&height.to_be_bytes())?;
            }
            Ok(())
        });
        frame::end(out);
    }
}

/// Decode a `Gstoatty;bars` frame, or `None` when its bars do not fill the
/// argument exactly.
pub(super) fn decode_bars(args: &[Vec<u8>]) -> Option<BarsCommand> {
    let arg = args.first()?;
    let head: &[u8; BARS_HEAD] = arg.get(..BARS_HEAD)?.try_into().ok()?;
    let entries = &arg[BARS_HEAD..];
    if entries.len() != usize::from(u16::from_be_bytes([head[3], head[4]])) * BARS_ENTRY {
        return None;
    }

    let bars = entries
        .as_chunks::<BARS_ENTRY>()
        .0
        .iter()
        .map(|bar| {
            (
                i16::from_be_bytes([bar[0], bar[1]]),
                i16::from_be_bytes([bar[2], bar[3]]),
                u16::from_be_bytes([bar[4], bar[5]]),
                u16::from_be_bytes([bar[6], bar[7]]),
            )
        })
        .collect();
    Some(BarsCommand {
        color: [head[0], head[1], head[2]],
        bars,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{
        decode, decode_stream,
        panel::{CORNER_TOP_LEFT, CORNER_TOP_RIGHT},
        Command,
    };

    /// A bar comes back whole, a plain one, an occluding one, a hatched one, and
    /// one rounded at two corners alike.
    #[test]
    fn bar_round_trips() {
        let plain = BarCommand {
            x: -4,
            y: 32,
            width: 3,
            height: 16,
            color: [220, 50, 47],
            occludes: false,
            hatch: None,
            radius: 0,
            corners: 0,
        };
        let occluding = BarCommand {
            occludes: true,
            ..plain
        };
        let hatched = BarCommand {
            hatch: Some([27, 29, 34]),
            ..plain
        };
        let rounded = BarCommand {
            radius: 2,
            corners: CORNER_TOP_LEFT | CORNER_TOP_RIGHT,
            ..plain
        };

        assert_eq!(
            [plain, occluding, hatched, rounded].map(|command| decode(&encode_bar(&command))),
            [
                Some(Command::Bar(plain)),
                Some(Command::Bar(occluding)),
                Some(Command::Bar(hatched)),
                Some(Command::Bar(rounded)),
            ],
        );
    }

    /// A bar's argument is a fixed 17 bytes, so the 16 bytes of a bar cut short
    /// of its corner mask decode to nothing.
    #[test]
    fn rejects_wrong_length_bar_payload() {
        let mut arg = frame::decode(&encode_bar(&BarCommand {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
            color: [5, 6, 7],
            occludes: false,
            hatch: None,
            radius: 0,
            corners: 0,
        }))
        .expect("one frame")
        .args
        .remove(0);
        arg.pop();

        assert_eq!(decode_bar(&[arg]), None);
        assert!(decode(b"Gstoatty;bar;YWJj").is_none());
    }

    /// A batch of bars comes back whole, and an empty batch sends nothing.
    #[test]
    fn a_bars_batch_round_trips() {
        let bars = BarsCommand {
            color: [7, 8, 9],
            bars: vec![(-4, 32, 3, 16), (0, 48, 2, 8)],
        };
        assert_eq!(decode(&encode_bars(&bars)), Some(Command::Bars(bars)));
        assert_eq!(
            encode_bars(&BarsCommand {
                color: [0; 3],
                bars: Vec::new()
            }),
            Vec::<u8>::new()
        );
    }

    /// A batch past the frame cap goes out as several frames under the cap,
    /// which together carry every bar in order. A payload that misses its count
    /// decodes to nothing.
    #[test]
    fn a_bars_batch_splits_past_the_cap_and_rejects_a_short_payload() {
        let bars = BarsCommand {
            color: [1, 2, 3],
            bars: (0..8000).map(|i| (i, 0, 1, 1)).collect(),
        };
        let encoded = encode_bars(&bars);

        let mut payloads = Vec::new();
        let mut rest = &encoded[..];
        while let Some(span) = frame::apc_span(rest) {
            payloads.push(frame::payload_len(span.len()));
            rest = &rest[span.end..];
        }
        assert!(
            payloads.len() > 1 && payloads.iter().all(|&len| len <= frame::MAX_APC_PAYLOAD),
            "several frames under the cap, got {payloads:?}",
        );
        let decoded: Vec<_> = decode_stream(&encoded)
            .into_iter()
            .flat_map(|command| match command {
                Command::Bars(frame) if frame.color == bars.color => frame.bars,
                other => panic!("a bars frame in the batch color, got {other:?}"),
            })
            .collect();
        assert_eq!(decoded, bars.bars);

        let one = encode_bars(&BarsCommand {
            color: [1, 2, 3],
            bars: vec![(1, 2, 3, 4)],
        });
        let mut arg = frame::decode(&one).expect("one frame").args.remove(0);
        arg.pop();
        assert_eq!(decode_bars(&[arg]), None);
    }
}

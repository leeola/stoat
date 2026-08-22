//! The text-run command draws a run of text off the cell grid at a fractional
//! scale.
//!
//! Its glyphs stream between the open and close markers, as a popover's content
//! does. The run can be smaller than the grid yet still line up with full-size
//! rows, which is how a gutter draws small line numbers.

use crate::frame;

/// Draw a run of text at a fractional scale, vertically centered on a cell row.
///
/// A non-cell component primitive: the run is drawn off the cell grid, so it can
/// be smaller than the grid (a gutter line number) yet still line up with
/// full-size rows. `col` and `row` are the anchor in **sixteenths of a cell**
/// (16 = one cell), so the run can sit at a fractional position; `scale` is the
/// glyph size in **256ths of the cell size** (256 = grid size), so it can be
/// fractional. The run advances one scaled cell width per character and is
/// vertically centered within the target row.
///
/// `bg`, when `Some`, is an opaque background box the renderer paints across the
/// run's full width (spaces included) before the glyphs alpha-blend over it, so
/// it need not match whatever lies beneath. `None` draws the glyphs with no
/// backing box, blending them directly over the surface behind the run.
///
/// The text is generic over its container so an emitter formatting a run every
/// frame can hand a borrowed slice, while a decoded command owns its copy. It
/// defaults to the owned form, so a holder that is not encoding writes the type
/// with no parameter. A scope encoder takes its text from a closure instead and
/// reads only the head fields, so `TextRunCommand<()>` names a head with no
/// text of its own.
// `Eq` is absent because the anchor carries a fractional row offset and
// `f32` is only `PartialEq`. Nothing keys a collection on a run.
#[derive(Clone, PartialEq, Debug)]
pub struct TextRunCommand<S = String> {
    pub col: i16,
    pub row: i16,
    pub scale: u16,
    pub color: [u8; 3],
    pub bg: Option<[u8; 3]>,
    /// Sketch whose reveal this run fades in with, so a label appears as the
    /// box it sits in draws itself. Zero fades the run in on its own.
    pub follow: u32,
    /// Pool this run rides, and the pool's top row, so the run glides with a
    /// scrolling pane the way a panel does. `None` leaves it screen-fixed.
    pub anchor: Option<(u32, f32)>,
    pub text: S,
}

/// Encode a [`TextRunCommand`] as a full `Gstoatty;text_run` frame for an
/// emitter.
///
/// The position, scale, color, and background ride in a fixed head on the open
/// marker, followed by the sketch this run follows and its optional anchor. A
/// 12-byte head is still decoded as the legacy form that always carries a
/// background, and a 13-byte one as the form that leads and is screen-fixed.
/// The run text streams between the markers.
pub fn encode_text_run<S: AsRef<str>>(command: &TextRunCommand<S>) -> Vec<u8> {
    let mut out = Vec::new();
    encode_text_run_into(&mut out, command);
    out
}

/// Append a `Gstoatty;text_run` frame to `out` without allocating.
///
/// Append a `Gstoatty;text_run` open marker, its streamed `text`, and a
/// `Gstoatty;text_run_end` close marker to `out`.
///
/// The fixed head fields ride in the open marker's single argument; `text`
/// streams as the raw bytes between the two markers, so it is not bounded by the
/// per-frame size cap. The text's container is generic, so an emitter passes a
/// slice of a reused buffer (a gutter formats line numbers into a stack buffer)
/// rather than building an owned [`String`] per frame.
///
/// The text must be plain. Riding outside the APC wrapper is what frees it from
/// the size cap and is also what makes it indistinguishable from terminal
/// control bytes, so the terminal cuts the capture at the first `ESC` and keeps
/// only what came before. An emitter that wants a styled run sets `color` and
/// `bg` rather than writing escape sequences into the text.
pub fn encode_text_run_into<S: AsRef<str>>(out: &mut Vec<u8>, command: &TextRunCommand<S>) {
    let text = command.text.as_ref();
    encode_text_run_scope(out, command, |out| out.extend_from_slice(text.as_bytes()));
}

/// Append a whole text run to `out`, being the open marker, whatever `text`
/// writes, then the close marker.
///
/// The streaming form of [`encode_text_run_into`], for a caller that formats
/// its text into the output buffer rather than holding it as one string. Both
/// emit the same bytes and neither has a way to leave the close marker out,
/// which matters because an unclosed run keeps capturing until the next marker
/// and swallows whatever the emitter writes next.
///
/// Only `command`'s head fields are read, so its text container is
/// unconstrained. Write `TextRunCommand<()>` with `text: ()` to say the closure
/// supplies the text.
///
/// The plain-text rule applies here too. The terminal cuts the capture at the
/// first `ESC`, so `text` must write text and set its colors through
/// `command.color` and `command.bg`.
pub fn encode_text_run_scope<S>(
    out: &mut Vec<u8>,
    command: &TextRunCommand<S>,
    text: impl FnOnce(&mut Vec<u8>),
) {
    frame::begin(out, "text_run");
    frame::push_arg(out, |w| {
        w.write_all(&command.col.to_be_bytes())?;
        w.write_all(&command.row.to_be_bytes())?;
        w.write_all(&command.scale.to_be_bytes())?;
        w.write_all(&command.color)?;
        w.write_all(&command.bg.unwrap_or([0, 0, 0]))?;
        w.write_all(&[command.bg.is_some() as u8])?;
        // Appended after the head every earlier build reads, so one of those
        // decodes the frame whole and treats the run as leading and
        // screen-fixed.
        w.write_all(&command.follow.to_be_bytes())?;
        w.write_all(&[command.anchor.is_some() as u8])?;
        let (host, top_rows) = command.anchor.unwrap_or((0, 0.0));
        w.write_all(&host.to_be_bytes())?;
        w.write_all(&top_rows.to_be_bytes())
    });
    frame::end(out);
    text(out);
    encode_text_run_end_into(out);
}

/// Encode a [`Command::TextRunEnd`] as a full `Gstoatty;text_run_end`
/// close-marker frame.
///
/// The frame carries no arguments; receiving it commits the text streamed since
/// the matching [`Command::TextRun`] into the run's `text`.
pub fn encode_text_run_end() -> Vec<u8> {
    let mut out = Vec::new();
    encode_text_run_end_into(&mut out);
    out
}

/// Append an argument-less `Gstoatty;text_run_end` close-marker frame to `out`.
pub fn encode_text_run_end_into(out: &mut Vec<u8>) {
    frame::begin(out, "text_run_end");
    frame::end(out);
}

/// Decode a `Gstoatty;text_run` open marker's head. The `text` streams as the
/// bytes after this frame and is captured by the terminal between the open
/// marker and [`Command::TextRunEnd`], so it is empty here.
pub(super) fn decode_text_run(args: &[Vec<u8>]) -> Option<TextRunCommand> {
    let arg = args.first()?;
    if arg.len() < 12 {
        return None;
    }

    // A 12-byte head predates the bg-presence byte and always carries a bg. A
    // 13-byte head gates the bg on its trailing presence byte.
    let bg = if arg.len() >= 13 {
        (arg[12] != 0).then_some([arg[9], arg[10], arg[11]])
    } else {
        Some([arg[9], arg[10], arg[11]])
    };

    Some(TextRunCommand {
        col: i16::from_be_bytes([arg[0], arg[1]]),
        row: i16::from_be_bytes([arg[2], arg[3]]),
        scale: u16::from_be_bytes([arg[4], arg[5]]),
        color: [arg[6], arg[7], arg[8]],
        bg,
        // A head that stops at the bg flag predates both, so the run leads
        // rather than follows and is screen-fixed.
        follow: arg
            .get(13..17)
            .map(|f| u32::from_be_bytes([f[0], f[1], f[2], f[3]]))
            .unwrap_or(0),
        anchor: arg.get(17..26).filter(|tail| tail[0] != 0).map(|tail| {
            (
                u32::from_be_bytes([tail[1], tail[2], tail[3], tail[4]]),
                f32::from_be_bytes([tail[5], tail[6], tail[7], tail[8]]),
            )
        }),
        text: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{decode, decode_stream, Command};

    /// A run that follows a sketch and rides a pool has to survive the wire,
    /// since both are what put a label on a mark that glides with its pane.
    #[test]
    fn a_following_anchored_run_round_trips() {
        let command = TextRunCommand {
            col: -3,
            row: 8,
            scale: 160,
            color: [1, 2, 3],
            bg: Some([4, 5, 6]),
            follow: 42,
            anchor: Some((7, 2.5)),
            text: "label".to_string(),
        };

        // The whole run is three frames, so it decodes as a stream rather
        // than as one, which also stitches the text back onto the head.
        assert_eq!(
            decode_stream(&encode_text_run(&command)),
            vec![Command::TextRun(command), Command::TextRunEnd],
        );
    }

    /// The two fields are appended, so a head that stops where an older build
    /// stopped still decodes. It reads as a run that leads rather than follows
    /// and is fixed to the screen.
    #[test]
    fn a_head_predating_follow_and_anchor_still_decodes() {
        // Thirteen bytes: the head through the bg-presence flag, and nothing
        // after it.
        let mut arg = Vec::new();
        arg.extend_from_slice(&(-3i16).to_be_bytes());
        arg.extend_from_slice(&8i16.to_be_bytes());
        arg.extend_from_slice(&160u16.to_be_bytes());
        arg.extend_from_slice(&[1, 2, 3]);
        arg.extend_from_slice(&[4, 5, 6]);
        arg.push(1);
        assert_eq!(arg.len(), 13);

        assert_eq!(
            decode_text_run(&[arg]),
            Some(TextRunCommand {
                col: -3,
                row: 8,
                scale: 160,
                color: [1, 2, 3],
                bg: Some([4, 5, 6]),
                follow: 0,
                anchor: None,
                text: String::new(),
            }),
        );
    }

    /// An anchor is written whether or not it is set, so the presence byte is
    /// the only thing separating a screen-fixed run from one riding pool zero
    /// at row zero.
    #[test]
    fn an_absent_anchor_stays_absent() {
        let command = TextRunCommand {
            col: 0,
            row: 0,
            scale: 256,
            color: [0, 0, 0],
            bg: None,
            follow: 0,
            anchor: None,
            text: String::new(),
        };

        let decoded = decode_stream(&encode_text_run(&command));
        let [Command::TextRun(run), Command::TextRunEnd] = decoded.as_slice() else {
            panic!("a run decodes as its open and close markers, got {decoded:?}");
        };
        assert_eq!(run.anchor, None);
    }

    #[test]
    fn text_run_end_round_trips() {
        // The text_run head and its streamed text round-trip at the terminal
        // layer (the text streams between the open and text_run_end markers, so a
        // single-frame decode cannot recover it); see the tty_term text_run
        // tests. Here we cover the close marker.
        assert_eq!(decode(&encode_text_run_end()), Some(Command::TextRunEnd));
    }

    #[test]
    fn rejects_wrong_length_text_run_payload() {
        // The first arg here decodes to 3 bytes, short of the 12 a text run head needs.
        assert!(decode(b"Gstoatty;text_run;YWJj").is_none());
    }

    #[test]
    fn text_run_decodes_legacy_arg_without_bg_presence() {
        // A 12-byte head predates the bg-presence byte and decodes to an opaque bg.
        let mut arg = Vec::new();
        arg.extend_from_slice(&(-8i16).to_be_bytes());
        arg.extend_from_slice(&48i16.to_be_bytes());
        arg.extend_from_slice(&192u16.to_be_bytes());
        arg.extend_from_slice(&[150, 160, 170]);
        arg.extend_from_slice(&[24, 26, 32]);
        assert_eq!(arg.len(), 12);

        assert_eq!(
            decode_text_run(&[arg]),
            Some(TextRunCommand {
                col: -8,
                row: 48,
                scale: 192,
                color: [150, 160, 170],
                bg: Some([24, 26, 32]),
                follow: 0,
                anchor: None,
                text: String::new(),
            })
        );
    }
}

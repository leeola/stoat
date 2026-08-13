//! The minimap commands declare a strip, fill its line-summary store, and move
//! its viewport thumb.
//!
//! A strip and its content are declared apart so a scroll moves the thumb without
//! resending the summaries. The store is spliced rather than replaced, and a
//! splice too large for one frame paginates across several.

use crate::frame;
use std::sync::Arc;

/// A single colored run on one minimap line, `len` columns wide starting at
/// `start_col`, drawn in palette entry `class`.
///
/// Columns and lengths are minimap columns (0 to `max_columns`), and `class`
/// indexes the strip's declared palette, so a run names color by class rather
/// than carrying an rgb triple per run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MinimapRun {
    pub start_col: u8,
    pub len: u8,
    pub class: u8,
}

/// The run summary of one buffer line, its runs left to right.
///
/// Empty for a blank line. The emitter caps the run count per line, and the wire
/// format bounds it to 255.
///
/// Shared rather than owned because a line is handed on several times without
/// ever being changed. It goes from the decode into the terminal's store, from
/// there into the grid's, and again wholesale whenever a resize reclones the
/// store. A file's worth of lines copied at each of those is what sharing them
/// avoids.
pub type LineSummary = Arc<[MinimapRun]>;

/// Declare a minimap strip and its rendering parameters.
///
/// The payload of [`Command::Minimap`]. The strip occupies a `width` by `height`
/// cell region at (`top`, `left`); [`Self::strip_id`] names this declaration and
/// [`Self::content_id`] the line-summary store it renders (see
/// [`MinimapLinesCommand`]). Each summarized line is drawn `lines_per_cell` to a
/// cell, up to `max_columns` wide. [`Self::bg`] and [`Self::thumb`] are rgba, the
/// thumb being the viewport overlay; [`Self::thumb_border`] is its rgb outline.
/// [`Self::palette`] holds up to 64 rgb entries a run's class indexes.
///
/// The palette is generic over its container so an emitter declaring a strip per
/// frame can hand a borrowed slice, while a decoded command owns its copy. It
/// defaults to the owned form, so a holder that is not encoding writes the type
/// with no parameter.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MinimapCommand<P = Vec<[u8; 3]>> {
    pub top: u16,
    pub left: u16,
    pub width: u16,
    pub height: u16,
    pub strip_id: u32,
    pub content_id: u32,
    pub lines_per_cell: u8,
    pub max_columns: u8,
    pub bg: [u8; 4],
    pub thumb: [u8; 4],
    pub thumb_border: [u8; 3],
    pub palette: P,
}

/// Splice line summaries into the content store [`Self::content_id`].
///
/// The payload of [`Command::MinimapLines`]. Starting at line [`Self::start`], it
/// replaces [`Self::removed`] existing lines with [`Self::lines`]. A pure
/// deletion carries an empty [`Self::lines`]. A pure insertion carries a zero
/// [`Self::removed`]. The wire count of inserted lines is `lines.len()`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MinimapLinesCommand {
    pub content_id: u32,
    pub start: u32,
    pub removed: u32,
    pub lines: Vec<LineSummary>,
}

/// Position a minimap strip's viewport thumb.
///
/// The payload of [`Command::MinimapView`]. [`Self::strip_id`] selects the strip;
/// [`Self::top_256`] is the fractional top buffer line in 1/256ths of a line, and
/// [`Self::visible_lines`] the viewport height in lines, together sizing and
/// placing the thumb.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MinimapViewCommand {
    pub strip_id: u32,
    pub top_256: u32,
    pub visible_lines: u16,
}

/// Retire the minimap content store [`Self::content_id`].
///
/// The payload of [`Command::MinimapDrop`]: a single content-store id, dropped
/// when its backing buffer closes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MinimapDropCommand {
    pub content_id: u32,
}

/// Encode a [`MinimapCommand`] as a full `Gstoatty;minimap` frame for an emitter.
///
/// The fixed head rides in a 29-byte first argument and the palette in a second
/// argument of consecutive rgb triples.
pub fn encode_minimap<P: AsRef<[[u8; 3]]>>(command: &MinimapCommand<P>) -> Vec<u8> {
    let mut out = Vec::new();
    encode_minimap_into(&mut out, command);
    out
}

/// Append a `Gstoatty;minimap` frame for `command` to `out`.
pub fn encode_minimap_into<P: AsRef<[[u8; 3]]>>(out: &mut Vec<u8>, command: &MinimapCommand<P>) {
    frame::begin(out, "minimap");
    frame::push_arg(out, |w| {
        w.write_all(&command.top.to_be_bytes())?;
        w.write_all(&command.left.to_be_bytes())?;
        w.write_all(&command.width.to_be_bytes())?;
        w.write_all(&command.height.to_be_bytes())?;
        w.write_all(&command.strip_id.to_be_bytes())?;
        w.write_all(&command.content_id.to_be_bytes())?;
        w.write_all(&[command.lines_per_cell, command.max_columns])?;
        w.write_all(&command.bg)?;
        w.write_all(&command.thumb)?;
        w.write_all(&command.thumb_border)
    });
    frame::push_arg(out, |w| {
        for entry in command.palette.as_ref() {
            w.write_all(entry)?;
        }
        Ok(())
    });
    frame::end(out);
}

/// Raw argument bytes one `minimap_lines` frame may carry.
///
/// Base64 turns 3 bytes into 4, so the pre-image of a full payload is three
/// quarters of [`frame::MAX_APC_PAYLOAD`]. The 64 subtracted covers the
/// `Gstoatty;minimap_lines;` prefix, which shares the payload budget with the
/// argument, plus margin.
const MINIMAP_LINES_RAW_BUDGET: usize = (frame::MAX_APC_PAYLOAD - 64) / 4 * 3;

/// Bytes the fixed fields take ahead of the line data in a splice argument.
/// Four `u32`s carry the content id, start, removed count, and line count.
const MINIMAP_LINES_HEADER: usize = 16;

/// Encode a [`MinimapLinesCommand`] as `Gstoatty;minimap_lines` frames.
///
/// A splice too large for one APC payload is spread over several frames, so the
/// result is one frame for an ordinary splice and a run of them for a big one.
/// See [`encode_minimap_lines_into`] for what the split guarantees.
pub fn encode_minimap_lines(command: &MinimapLinesCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_minimap_lines_into(&mut out, command);
    out
}

/// Append the `Gstoatty;minimap_lines` frames for `command` to `out`.
///
/// The terminal's scanner discards an APC payload past
/// [`frame::MAX_APC_PAYLOAD`] whole rather than truncating it, so a splice whose
/// lines would overrun that is packed into as many frames as it takes. Applying
/// them in order has the same effect as applying `command` in one go. The first
/// frame carries the removal and the leading lines, and each later one inserts
/// its share at the point the previous frame stopped.
///
/// A splice that fits emits exactly one frame, as does a pure deletion.
pub fn encode_minimap_lines_into(out: &mut Vec<u8>, command: &MinimapLinesCommand) {
    let budget = MINIMAP_LINES_RAW_BUDGET - MINIMAP_LINES_HEADER;
    let mut emitted = 0;
    loop {
        let mut used = 0;
        let mut take = 0;
        for line in &command.lines[emitted..] {
            let cost = 1 + 3 * line.len();
            // Always take one line, so a hypothetical line past the whole
            // budget still advances rather than looping forever. A line maxes
            // out at 1 + 3 * 255 bytes, far under it.
            if take > 0 && used + cost > budget {
                break;
            }
            used += cost;
            take += 1;
        }

        write_minimap_lines_frame(
            out,
            command.content_id,
            command.start + emitted as u32,
            if emitted == 0 { command.removed } else { 0 },
            &command.lines[emitted..emitted + take],
        );

        emitted += take;
        if emitted >= command.lines.len() {
            return;
        }
    }
}

/// Append one `Gstoatty;minimap_lines` frame splicing `lines` in at `start`.
fn write_minimap_lines_frame(
    out: &mut Vec<u8>,
    content_id: u32,
    start: u32,
    removed: u32,
    lines: &[LineSummary],
) {
    frame::begin(out, "minimap_lines");
    frame::push_arg(out, |w| {
        w.write_all(&content_id.to_be_bytes())?;
        w.write_all(&start.to_be_bytes())?;
        w.write_all(&removed.to_be_bytes())?;
        w.write_all(&(lines.len() as u32).to_be_bytes())?;
        for line in lines {
            w.write_all(&[line.len() as u8])?;
            for run in line.iter() {
                w.write_all(&[run.start_col, run.len, run.class])?;
            }
        }
        Ok(())
    });
    frame::end(out);
}

/// Encode a [`MinimapViewCommand`] as a full `Gstoatty;minimap_view` frame.
///
/// The strip id, fractional top, and viewport height ride in a fixed 10-byte
/// argument.
pub fn encode_minimap_view(command: &MinimapViewCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_minimap_view_into(&mut out, command);
    out
}

/// Append a `Gstoatty;minimap_view` frame for `command` to `out`.
pub fn encode_minimap_view_into(out: &mut Vec<u8>, command: &MinimapViewCommand) {
    frame::begin(out, "minimap_view");
    frame::push_arg(out, |w| {
        w.write_all(&command.strip_id.to_be_bytes())?;
        w.write_all(&command.top_256.to_be_bytes())?;
        w.write_all(&command.visible_lines.to_be_bytes())
    });
    frame::end(out);
}

/// Encode a [`MinimapDropCommand`] as a full `Gstoatty;minimap_drop` frame.
pub fn encode_minimap_drop(command: &MinimapDropCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_minimap_drop_into(&mut out, command);
    out
}

/// Append a `Gstoatty;minimap_drop` frame retiring content store `content_id`.
pub fn encode_minimap_drop_into(out: &mut Vec<u8>, command: &MinimapDropCommand) {
    frame::begin(out, "minimap_drop");
    frame::push_arg(out, |w| w.write_all(&command.content_id.to_be_bytes()));
    frame::end(out);
}

pub(super) fn decode_minimap(args: &[Vec<u8>]) -> Option<MinimapCommand> {
    let head: &[u8; 29] = args.first()?.get(..29)?.try_into().ok()?;
    let palette_bytes = args.get(1)?;
    if palette_bytes.len() % 3 != 0 || palette_bytes.len() / 3 > 64 {
        return None;
    }

    let palette = palette_bytes
        .chunks_exact(3)
        .map(|entry| [entry[0], entry[1], entry[2]])
        .collect();

    Some(MinimapCommand {
        top: u16::from_be_bytes([head[0], head[1]]),
        left: u16::from_be_bytes([head[2], head[3]]),
        width: u16::from_be_bytes([head[4], head[5]]),
        height: u16::from_be_bytes([head[6], head[7]]),
        strip_id: u32::from_be_bytes([head[8], head[9], head[10], head[11]]),
        content_id: u32::from_be_bytes([head[12], head[13], head[14], head[15]]),
        lines_per_cell: head[16],
        max_columns: head[17],
        bg: [head[18], head[19], head[20], head[21]],
        thumb: [head[22], head[23], head[24], head[25]],
        thumb_border: [head[26], head[27], head[28]],
        palette,
    })
}

pub(super) fn decode_minimap_lines(args: &[Vec<u8>]) -> Option<MinimapLinesCommand> {
    let arg = args.first()?;
    let header: &[u8; 16] = arg.get(..16)?.try_into().ok()?;
    let content_id = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let start = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    let removed = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    let inserted = u32::from_be_bytes([header[12], header[13], header[14], header[15]]);

    // Grown by push rather than pre-allocated, so an inflated `inserted` cannot
    // force a huge allocation before the run bytes prove short.
    let mut lines = Vec::new();
    let mut cursor = 16;
    for _ in 0..inserted {
        let run_count = *arg.get(cursor)? as usize;
        cursor += 1;
        let end = cursor.checked_add(run_count.checked_mul(3)?)?;
        let run_bytes = arg.get(cursor..end)?;
        let runs = run_bytes
            .chunks_exact(3)
            .map(|run| MinimapRun {
                start_col: run[0],
                len: run[1],
                class: run[2],
            })
            .collect();
        lines.push(runs);
        cursor = end;
    }

    if cursor != arg.len() {
        return None;
    }

    Some(MinimapLinesCommand {
        content_id,
        start,
        removed,
        lines,
    })
}

pub(super) fn decode_minimap_view(args: &[Vec<u8>]) -> Option<MinimapViewCommand> {
    let arg: &[u8; 10] = args.first()?.get(..10)?.try_into().ok()?;

    Some(MinimapViewCommand {
        strip_id: u32::from_be_bytes([arg[0], arg[1], arg[2], arg[3]]),
        top_256: u32::from_be_bytes([arg[4], arg[5], arg[6], arg[7]]),
        visible_lines: u16::from_be_bytes([arg[8], arg[9]]),
    })
}

pub(super) fn decode_minimap_drop(args: &[Vec<u8>]) -> Option<MinimapDropCommand> {
    let arg: &[u8; 4] = args.first()?.get(..4)?.try_into().ok()?;

    Some(MinimapDropCommand {
        content_id: u32::from_be_bytes(*arg),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        command::{decode, Command},
        frame::MAX_APC_PAYLOAD,
    };

    /// Assemble a frame for `sub` from raw (pre-base64) argument bytes, for
    /// crafting the malformed payloads the rejection tests probe.
    fn frame_bytes(sub: &str, args: Vec<Vec<u8>>) -> Vec<u8> {
        frame::encode(&frame::Frame {
            sub: sub.to_string(),
            args,
        })
    }

    #[test]
    fn minimap_round_trips() {
        let command = MinimapCommand {
            top: 0,
            left: 72,
            width: 8,
            height: 40,
            strip_id: 5,
            content_id: 9,
            lines_per_cell: 8,
            max_columns: 120,
            bg: [10, 20, 30, 0],
            thumb: [200, 200, 200, 48],
            thumb_border: [255, 255, 255],
            palette: vec![[0, 0, 0], [1, 2, 3]],
        };

        assert_eq!(
            decode(&encode_minimap(&command)),
            Some(Command::Minimap(command))
        );
    }

    #[test]
    fn minimap_accepts_empty_palette() {
        let command = MinimapCommand {
            top: 0,
            left: 0,
            width: 8,
            height: 8,
            strip_id: 1,
            content_id: 1,
            lines_per_cell: 8,
            max_columns: 120,
            bg: [0, 0, 0, 0],
            thumb: [0, 0, 0, 0],
            thumb_border: [0, 0, 0],
            palette: vec![],
        };

        assert_eq!(
            decode(&encode_minimap(&command)),
            Some(Command::Minimap(command))
        );
    }

    #[test]
    fn rejects_wrong_length_minimap_head() {
        // A 28-byte head is one short of the 29 a minimap declaration needs.
        let bytes = frame_bytes("minimap", vec![vec![0u8; 28], vec![0u8; 3]]);
        assert!(decode(&bytes).is_none());
    }

    #[test]
    fn rejects_oversized_minimap_palette() {
        // 65 rgb entries (195 bytes) exceeds the 64-entry palette cap.
        let bytes = frame_bytes("minimap", vec![vec![0u8; 29], vec![0u8; 195]]);
        assert!(decode(&bytes).is_none());
    }

    #[test]
    fn rejects_misaligned_minimap_palette() {
        // 4 palette bytes is not a whole number of rgb triples.
        let bytes = frame_bytes("minimap", vec![vec![0u8; 29], vec![0u8; 4]]);
        assert!(decode(&bytes).is_none());
    }

    #[test]
    fn minimap_lines_round_trips() {
        let command = MinimapLinesCommand {
            content_id: 9,
            start: 3,
            removed: 2,
            lines: summaries(vec![
                vec![
                    MinimapRun {
                        start_col: 0,
                        len: 4,
                        class: 2,
                    },
                    MinimapRun {
                        start_col: 6,
                        len: 3,
                        class: 5,
                    },
                ],
                vec![MinimapRun {
                    start_col: 2,
                    len: 8,
                    class: 1,
                }],
            ]),
        };

        assert_eq!(
            decode(&encode_minimap_lines(&command)),
            Some(Command::MinimapLines(command))
        );
    }

    /// Split `bytes` into the APC payloads it carries, the stretches between
    /// `ESC _` and the terminator that the terminal's scanner buffers and caps.
    fn apc_payloads(bytes: &[u8]) -> Vec<&[u8]> {
        bytes
            .split(|&b| b == 0x1b)
            .filter_map(|part| part.strip_prefix(b"_"))
            .collect()
    }

    /// Apply one splice to `store`, mirroring the terminal's own store update.
    fn apply_splice(store: &mut Vec<LineSummary>, c: &MinimapLinesCommand) {
        let start = (c.start as usize).min(store.len());
        let end = start.saturating_add(c.removed as usize).min(store.len());
        store.splice(start..end, c.lines.iter().cloned());
    }

    /// The shared summaries a store holds, for stating a fixture.
    fn summaries(lines: Vec<Vec<MinimapRun>>) -> Vec<LineSummary> {
        lines.into_iter().map(Into::into).collect()
    }

    /// A splice of `lines` rows at `runs` runs each, sized by the caller to sit
    /// either side of one APC payload's worth of content.
    fn dense_splice(lines: usize, runs: usize) -> MinimapLinesCommand {
        MinimapLinesCommand {
            content_id: 7,
            start: 2,
            removed: 5,
            lines: (0..lines)
                .map(|i| {
                    (0..runs)
                        .map(|r| MinimapRun {
                            start_col: (r * 2) as u8,
                            len: (i % 7 + 1) as u8,
                            class: (r % 4) as u8,
                        })
                        .collect()
                })
                .collect(),
        }
    }

    #[test]
    fn an_oversized_splice_paginates_within_the_payload_cap() {
        let command = dense_splice(4096, 12);
        let bytes = encode_minimap_lines(&command);
        let payloads = apc_payloads(&bytes);

        assert!(
            payloads.len() > 1,
            "a 4096-line by 12-run splice cannot fit one payload",
        );
        for payload in &payloads {
            assert!(
                payload.len() <= MAX_APC_PAYLOAD,
                "a payload of {} bytes would be discarded by the scanner",
                payload.len(),
            );
        }
    }

    #[test]
    fn paginated_frames_apply_to_the_same_store_as_one_splice() {
        let command = dense_splice(4096, 12);

        let mut expected: Vec<LineSummary> = summaries(vec![vec![]; 40]);
        apply_splice(&mut expected, &command);

        let mut got: Vec<LineSummary> = summaries(vec![vec![]; 40]);
        let bytes = encode_minimap_lines(&command);
        let mut frames = 0;
        for payload in apc_payloads(&bytes) {
            match decode(payload) {
                Some(Command::MinimapLines(part)) => {
                    assert_eq!(part.content_id, command.content_id);
                    apply_splice(&mut got, &part);
                    frames += 1;
                },
                other => panic!("frame {frames} decoded as {other:?}"),
            }
        }

        assert!(frames > 1, "the splice really did paginate");
        assert_eq!(
            got, expected,
            "applying every frame in order rebuilds the single splice's result",
        );
    }

    #[test]
    fn a_splice_that_fits_stays_one_frame() {
        let command = dense_splice(64, 3);
        assert_eq!(
            apc_payloads(&encode_minimap_lines(&command)).len(),
            1,
            "an ordinary splice is not split",
        );
        assert_eq!(
            decode(&encode_minimap_lines(&command)),
            Some(Command::MinimapLines(command)),
            "and still round-trips as one command",
        );
    }

    #[test]
    fn minimap_lines_pure_deletion_round_trips() {
        let command = MinimapLinesCommand {
            content_id: 4,
            start: 10,
            removed: 3,
            lines: vec![],
        };

        assert_eq!(
            decode(&encode_minimap_lines(&command)),
            Some(Command::MinimapLines(command))
        );
    }

    #[test]
    fn minimap_lines_blank_line_round_trips() {
        let command = MinimapLinesCommand {
            content_id: 4,
            start: 0,
            removed: 0,
            lines: summaries(vec![vec![]]),
        };

        assert_eq!(
            decode(&encode_minimap_lines(&command)),
            Some(Command::MinimapLines(command))
        );
    }

    #[test]
    fn rejects_truncated_minimap_lines_runs() {
        let mut arg = Vec::new();
        arg.extend_from_slice(&9u32.to_be_bytes()); // content_id
        arg.extend_from_slice(&0u32.to_be_bytes()); // start
        arg.extend_from_slice(&0u32.to_be_bytes()); // removed
        arg.extend_from_slice(&1u32.to_be_bytes()); // inserted = 1
        arg.push(2); // the line claims 2 runs, but no run bytes follow

        let bytes = frame_bytes("minimap_lines", vec![arg]);
        assert!(decode(&bytes).is_none());
    }

    #[test]
    fn rejects_trailing_bytes_minimap_lines() {
        let mut arg = Vec::new();
        arg.extend_from_slice(&9u32.to_be_bytes());
        arg.extend_from_slice(&0u32.to_be_bytes());
        arg.extend_from_slice(&0u32.to_be_bytes());
        arg.extend_from_slice(&0u32.to_be_bytes()); // inserted = 0
        arg.push(0); // a stray byte past the last declared line

        let bytes = frame_bytes("minimap_lines", vec![arg]);
        assert!(decode(&bytes).is_none());
    }

    #[test]
    fn minimap_view_round_trips() {
        let command = MinimapViewCommand {
            strip_id: 5,
            top_256: 1_280,
            visible_lines: 40,
        };

        assert_eq!(
            decode(&encode_minimap_view(&command)),
            Some(Command::MinimapView(command))
        );
    }

    #[test]
    fn rejects_wrong_length_minimap_view_payload() {
        // The single arg here decodes to 3 bytes, not the 10 a view needs.
        assert!(decode(b"Gstoatty;minimap_view;YWJj").is_none());
    }

    #[test]
    fn minimap_drop_round_trips() {
        let command = MinimapDropCommand { content_id: 9 };

        assert_eq!(
            decode(&encode_minimap_drop(&command)),
            Some(Command::MinimapDrop(command))
        );
    }

    #[test]
    fn rejects_wrong_length_minimap_drop_payload() {
        // The single arg here decodes to 3 bytes, not the 4 a content id needs.
        assert!(decode(b"Gstoatty;minimap_drop;YWJj").is_none());
    }
}

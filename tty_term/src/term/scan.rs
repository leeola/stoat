//! The sequences the vte parser hands back to nobody, recognized in one walk.
//!
//! `alacritty_terminal` drops APC strings, the private XTVERSION query, and OSC 9
//! / OSC 777 notifications, yet all three carry meaning to stoatty. [`EscScanner`]
//! watches the same byte stream the parser reads and emits an [`EscEvent`] as each
//! one completes.

use super::TermEvent;
use std::ops::Range;
use stoatty_protocol::frame::MAX_APC_PAYLOAD;

pub(super) const ESC: u8 = 0x1b;
/// Byte after `ESC` that opens an APC string (`ESC _`).
const APC_INTRODUCER: u8 = b'_';
/// Byte after `ESC` that closes a string control (`ESC \`, the ST).
const STRING_TERMINATOR: u8 = b'\\';
/// Bell, accepted as an alternate string terminator.
const BEL: u8 = 0x07;
/// A sequence the fused scanner recognized, emitted as it completes.
///
/// The payloads borrow the scanner's reused buffer, so each is valid only for the
/// call that yields it.
pub(super) enum EscEvent<'a> {
    /// An APC string's payload, the bytes between the introducer and the
    /// terminator, paired with the offset one past that terminator.
    ///
    /// The offset lets a caller split the input at frame boundaries to route the
    /// content between frames. A payload split across calls is retained until its
    /// terminator arrives, and its offset is then relative to the call it
    /// completes in.
    ///
    /// `interior` is where those payload bytes sat in the scanned slice, for a
    /// caller passing the stream on to a parser that has no use for them. It
    /// covers only the part that arrived in this call, so it is empty for a
    /// payload that arrived entirely in earlier ones, and it excludes the
    /// introducer and the terminator, which such a caller still has to pass on
    /// for the string to open and close.
    Apc {
        payload: &'a [u8],
        interior: Range<usize>,
        end: usize,
    },
    /// An XTVERSION query (`CSI > Ps q`) completed. The parameters are dropped,
    /// since the reply is fixed.
    XtVersion,
    /// An OSC 9 or OSC 777 notification, as its code and the bytes after the
    /// code's `;`.
    OscNotify { code: u32, payload: &'a [u8] },
    /// An OSC 1337 payload, the bytes between the code's `;` and the
    /// terminator, paired with where they sat and the offset one past that
    /// terminator.
    ///
    /// Reported like [`EscEvent::Apc`] so a caller can excise them from what it
    /// passes to a parser. That is not optional here: an image on an OSC is
    /// bytes the vte parser buffers without bound.
    ///
    /// `payload` is absent when the escape overran the cap. The interior is
    /// reported regardless, since the bytes must be excised whether or not
    /// anything can be drawn from them.
    OscImage {
        payload: Option<&'a [u8]>,
        interior: Range<usize>,
        end: usize,
    },
    /// A full reset (`ESC c`).
    ///
    /// The parser resets the screen itself, so this reports it only for the
    /// state the driver keeps alongside.
    Ris,
}

/// Recognizes the escape sequences the vte parser does not hand back, in one walk
/// of the stream.
///
/// Three kinds go unreported by `alacritty_terminal`, so the driver watches the
/// bytes for them itself: APC strings (`ESC _ ... ESC \`), which carry stoatty's own
/// protocol; XTVERSION queries (`CSI > Ps q`), which vte dispatches in the plain
/// `CSI Ps q` form but not this one; and OSC 9 / OSC 777 desktop notifications,
/// where vte handles OSC 0/1/2/4/52 and drops these. Each is framed across
/// [`Terminal::advance`] calls, so a sequence split between chunks completes on the
/// chunk that ends it.
///
/// All three open on the byte after `ESC`, and no two can be open at once, so one
/// state machine recognizes all of them for the cost of the one walk. A sequence
/// written inside another's payload is part of that payload rather than a sequence
/// of its own, which is how the vte parser reads the same bytes.
///
/// Recognizing a stoatty frame among the APC payloads is the decoder's job, not this
/// scanner's. Mapping a notification to an event is [`notification_from_osc`]'s.
#[derive(Default)]
pub(super) struct EscScanner {
    state: EscState,
    /// The payload of whichever sequence is open, shared because the APC and OSC
    /// states that fill it are mutually exclusive.
    payload: Vec<u8>,
    /// The OSC code being accumulated, then the one a buffered payload belongs to.
    code: u32,
    /// Set when an OSC payload outgrew its cap, so it is dropped at its terminator
    /// rather than reported truncated.
    overflow: bool,
}

#[derive(Clone, Copy, Default)]
enum EscState {
    #[default]
    Ground,
    Escape,
    /// Seen `ESC _`, buffering the APC payload.
    Apc,
    /// Seen `ESC` inside an APC payload, awaiting the `\` that terminates it.
    ApcEscape,
    /// Inside an APC whose payload overran the cap, skipped to its terminator so
    /// the scanner leaves the string where the vte parser does.
    ApcSkip,
    /// Seen `ESC` inside a skipped APC.
    ApcSkipEscape,
    /// Seen `ESC [`, waiting on the `>` that marks the private query.
    CsiEntry,
    /// Seen `ESC [ >`, consuming parameter bytes until the final byte.
    CsiGt,
    /// Seen `ESC ]`, accumulating the numeric code up to its `;`.
    OscPrefix,
    /// Inside an OSC 9 or 777 payload, which is buffered.
    OscBuffer,
    /// Seen `ESC` inside a buffered OSC payload.
    OscBufferEscape,
    /// Inside an OSC 1337 payload, buffered under its own far larger cap since
    /// it carries an image rather than a line of text.
    OscImage,
    /// Seen `ESC` inside an image payload.
    OscImageEscape,
    /// Inside any other OSC, skipped to its terminator so a large clipboard write
    /// is never copied.
    OscSkip,
    /// Seen `ESC` inside a skipped OSC.
    OscSkipEscape,
}

impl EscScanner {
    /// Whether no sequence is open, so a caller may skip a chunk containing no
    /// `ESC` without missing one.
    pub(super) fn is_idle(&self) -> bool {
        matches!(self.state, EscState::Ground)
    }

    /// Feed `bytes`, invoking `emit` for each sequence that completes within them.
    pub(super) fn scan(&mut self, bytes: &[u8], emit: &mut impl FnMut(EscEvent<'_>)) {
        let mut i = 0;

        // The stretch of this slice the open APC has taken as payload. A payload
        // carried in from an earlier call has taken nothing here yet, so it
        // starts empty at the front. Tracked rather than worked back from the
        // terminator, which cannot be sized: `ESC ESC \` ends a string as surely
        // as `ESC \` does, and counting back two would swallow the first `ESC`.
        let mut apc = 0..0;
        // The same, for an image OSC. Its payload is excised from what reaches
        // the vte parser, which would otherwise buffer a whole image without
        // bound.
        let mut osc = 0..0;

        while i < bytes.len() {
            let byte = bytes[i];
            match self.state {
                // Nothing is open, so jump to the next ESC and skip the plain bytes
                // between rather than stepping each one.
                EscState::Ground => match memchr::memchr(ESC, &bytes[i..]) {
                    Some(off) => {
                        self.state = EscState::Escape;
                        i += off;
                    },
                    None => break,
                },
                EscState::Escape => {
                    self.state = match byte {
                        APC_INTRODUCER => {
                            self.payload.clear();
                            apc = i + 1..i + 1;
                            EscState::Apc
                        },
                        b'[' => EscState::CsiEntry,
                        OSC_INTRODUCER => {
                            self.code = 0;
                            self.payload.clear();
                            self.overflow = false;
                            EscState::OscPrefix
                        },
                        ESC => EscState::Escape,
                        // RIS. vte applies it to the screen, but nothing tells
                        // the driver, and state the driver owns outside the
                        // screen has to go with it.
                        b'c' => {
                            emit(EscEvent::Ris);
                            EscState::Ground
                        },
                        _ => EscState::Ground,
                    };
                },

                // APC carries whole screens from stoat, so the payload between
                // terminators is taken in one run. Landing on the terminator
                // rather than past it leaves the arms below to read it.
                EscState::Apc => match byte {
                    ESC => self.state = EscState::ApcEscape,
                    BEL => self.finish_apc(apc.clone(), i + 1, emit),
                    _ => {
                        i += self.push_apc_run(payload_run(&bytes[i..]));
                        apc.end = i;
                        continue;
                    },
                },
                EscState::ApcEscape => match byte {
                    STRING_TERMINATOR => self.finish_apc(apc.clone(), i + 1, emit),
                    ESC => self.state = EscState::ApcEscape,
                    _ => {
                        self.payload.clear();
                        self.state = EscState::Ground;
                    },
                },
                EscState::ApcSkip => match byte {
                    ESC => self.state = EscState::ApcSkipEscape,
                    BEL => self.state = EscState::Ground,
                    _ => {
                        i += payload_run(&bytes[i..]).len();
                        continue;
                    },
                },
                EscState::ApcSkipEscape => match byte {
                    STRING_TERMINATOR => self.state = EscState::Ground,
                    ESC => self.state = EscState::ApcSkipEscape,
                    _ => self.state = EscState::ApcSkip,
                },

                EscState::CsiEntry => {
                    self.state = match byte {
                        b'>' => EscState::CsiGt,
                        _ => EscState::Ground,
                    }
                },
                EscState::CsiGt => {
                    self.state = match byte {
                        // Parameter and intermediate bytes keep the sequence open.
                        0x20..=0x3f => EscState::CsiGt,
                        b'q' => {
                            emit(EscEvent::XtVersion);
                            EscState::Ground
                        },
                        ESC => EscState::Escape,
                        _ => EscState::Ground,
                    }
                },

                EscState::OscPrefix => match byte {
                    b'0'..=b'9' => {
                        self.code = self
                            .code
                            .saturating_mul(10)
                            .saturating_add(u32::from(byte - b'0'));
                    },
                    b';' => {
                        // The image payload starts here, so the interior the
                        // driver excises starts one past the semicolon.
                        osc = i + 1..i + 1;
                        self.state = match self.code {
                            9 | 777 => EscState::OscBuffer,
                            1337 => EscState::OscImage,
                            _ => EscState::OscSkip,
                        };
                    },
                    ESC => self.state = EscState::OscSkipEscape,
                    BEL => self.state = EscState::Ground,
                    _ => self.state = EscState::OscSkip,
                },
                EscState::OscBuffer => match byte {
                    ESC => self.state = EscState::OscBufferEscape,
                    BEL => self.finish_osc(emit),
                    _ => {
                        i += self.push_osc_run(payload_run(&bytes[i..]));
                        continue;
                    },
                },
                EscState::OscBufferEscape => match byte {
                    STRING_TERMINATOR => self.finish_osc(emit),
                    ESC => self.state = EscState::OscBufferEscape,
                    _ => {
                        self.payload.clear();
                        self.state = EscState::Ground;
                    },
                },
                EscState::OscImage => match byte {
                    ESC => self.state = EscState::OscImageEscape,
                    BEL => self.finish_osc_image(osc.clone(), i + 1, emit),
                    _ => {
                        i += self.push_osc_image_run(payload_run(&bytes[i..]));
                        osc.end = i;
                        continue;
                    },
                },
                EscState::OscImageEscape => match byte {
                    STRING_TERMINATOR => self.finish_osc_image(osc.clone(), i + 1, emit),
                    ESC => self.state = EscState::OscImageEscape,
                    _ => {
                        self.payload.clear();
                        self.overflow = false;
                        self.state = EscState::Ground;
                    },
                },
                EscState::OscSkip => match byte {
                    ESC => self.state = EscState::OscSkipEscape,
                    BEL => self.state = EscState::Ground,
                    _ => {
                        i += payload_run(&bytes[i..]).len();
                        continue;
                    },
                },
                EscState::OscSkipEscape => match byte {
                    STRING_TERMINATOR => self.state = EscState::Ground,
                    ESC => self.state = EscState::OscSkipEscape,
                    _ => self.state = EscState::OscSkip,
                },
            }
            i += 1;
        }
    }

    /// Buffer a run of APC payload bytes, abandoning the frame if it overruns
    /// the cap, and report how many bytes of input the run consumed.
    ///
    /// The cap is [`MAX_APC_PAYLOAD`], shared with the encoders so a frame they emit
    /// can never be the thing this discards. Overrunning discards the whole frame,
    /// so the bytes buffered before the cap was reached are of no use and the run
    /// is dropped entire rather than filled to the brim first.
    ///
    /// Abandoning the frame skips to its terminator rather than returning to
    /// ground. The vte parser is still inside the string, and a scanner that
    /// left early would decode a frame buried in the oversized tail as a real
    /// command, applying it to a screen the parser never painted it on.
    fn push_apc_run(&mut self, run: &[u8]) -> usize {
        if self.payload.len() + run.len() <= MAX_APC_PAYLOAD {
            self.payload.extend_from_slice(run);
        } else {
            self.payload.clear();
            self.state = EscState::ApcSkip;
        }
        run.len()
    }

    /// Emit the buffered APC payload, ending one past its terminator at `end`,
    /// having taken `interior` of the current slice.
    fn finish_apc(
        &mut self,
        interior: Range<usize>,
        end: usize,
        emit: &mut impl FnMut(EscEvent<'_>),
    ) {
        emit(EscEvent::Apc {
            payload: &self.payload,
            interior,
            end,
        });
        self.payload.clear();
        self.state = EscState::Ground;
    }

    /// Buffer a run of OSC payload bytes, marking overflow past the cap so the
    /// sequence is dropped at its terminator rather than reported truncated, and
    /// report how many bytes of input the run consumed.
    ///
    /// Unlike an APC overrun this keeps scanning, so what fits is still buffered.
    /// The sequence is discarded at its terminator instead, which is where the
    /// overflow flag is read.
    fn push_osc_run(&mut self, run: &[u8]) -> usize {
        let room = MAX_OSC_NOTIFY_BYTES - self.payload.len();
        if run.len() > room {
            self.overflow = true;
        }
        self.payload.extend_from_slice(&run[..run.len().min(room)]);
        run.len()
    }

    /// Buffer a run of image payload, or note that the escape overran its cap.
    ///
    /// Overrunning keeps consuming rather than skipping to the terminator, and
    /// keeps the interior range growing with it. The bytes still have to be
    /// excised from what the vte parser sees, or the payload this refused would
    /// be buffered whole by the thing this cap exists to protect.
    fn push_osc_image_run(&mut self, run: &[u8]) -> usize {
        if self.overflow {
            return run.len();
        }
        if self.payload.len() + run.len() > MAX_OSC_IMAGE_BYTES {
            self.overflow = true;
            self.payload.clear();
        } else {
            self.payload.extend_from_slice(run);
        }
        run.len()
    }

    /// Emit the buffered image payload and reset, releasing its memory.
    ///
    /// The payload buffer is shared with the notification and frame paths, and
    /// an image leaves it orders of magnitude larger than either needs. Shrinking
    /// here rather than at the next use is what keeps one image off the
    /// session's memory for the rest of its life.
    fn finish_osc_image(
        &mut self,
        interior: Range<usize>,
        end: usize,
        emit: &mut impl FnMut(EscEvent<'_>),
    ) {
        emit(EscEvent::OscImage {
            payload: (!self.overflow).then_some(self.payload.as_slice()),
            interior,
            end,
        });

        self.payload.clear();
        self.payload.shrink_to(MAX_OSC_NOTIFY_BYTES);
        self.overflow = false;
        self.state = EscState::Ground;
    }

    /// Emit the buffered OSC payload unless it overran the cap, then reset.
    fn finish_osc(&mut self, emit: &mut impl FnMut(EscEvent<'_>)) {
        if !self.overflow {
            emit(EscEvent::OscNotify {
                code: self.code,
                payload: &self.payload,
            });
        }
        self.payload.clear();
        self.overflow = false;
        self.state = EscState::Ground;
    }
}

/// The leading bytes of `rest` that belong to a string payload, stopping at the
/// terminator that ends it or at the end of what has arrived.
///
/// Empty exactly when `rest` opens on a terminator, which is why the callers
/// take a run only from a byte they have already seen is not one. A zero-length
/// run would leave the scan position where it was.
fn payload_run(rest: &[u8]) -> &[u8] {
    match memchr::memchr2(ESC, BEL, rest) {
        Some(end) => &rest[..end],
        None => rest,
    }
}

/// The XTVERSION reply naming this terminal, answering `CSI > Ps q`.
///
/// A DCS string (`ESC P > | name ESC \`) carrying the terminal name and version,
/// the response xterm defined for XTVERSION. Programs such as fish query it at
/// startup and gate optional features on the answer; the vte parser does not
/// dispatch it, so the driver synthesizes this reply itself.
pub(super) const XTVERSION_REPLY: &str =
    concat!("\x1bP>|stoatty(", env!("CARGO_PKG_VERSION"), ")\x1b\\");

/// Byte after `ESC` that opens an OSC string (`ESC ]`).
const OSC_INTRODUCER: u8 = b']';

/// Cap on a buffered OSC 9 / OSC 777 notification payload, bounding memory
/// against a sequence that never terminates. A larger notification is discarded.
const MAX_OSC_NOTIFY_BYTES: usize = 4096;

/// Cap on a buffered OSC 1337 image payload.
///
/// Far above the notification cap because this carries a whole image rather
/// than a line of text, and far below unbounded because the bytes arrive from
/// whatever program holds the other end of a pty.
const MAX_OSC_IMAGE_BYTES: usize = 128 * 1024 * 1024;

/// Map a scanned OSC notification `(code, payload)` to a [`TermEvent::Notification`].
///
/// OSC 9 carries only a body. OSC 777's payload is `kind;title;body`, where only
/// the `notify` kind yields an event and a `;` inside the body is preserved. A
/// code or kind that is not a notification yields `None`.
pub(super) fn notification_from_osc(code: u32, payload: &[u8]) -> Option<TermEvent> {
    match code {
        9 => Some(TermEvent::Notification {
            title: None,
            body: String::from_utf8_lossy(payload).into_owned(),
        }),
        777 => {
            let text = String::from_utf8_lossy(payload);
            let mut parts = text.splitn(3, ';');
            if parts.next()? != "notify" {
                return None;
            }
            let title = parts.next()?.to_owned();
            let body = parts.next().unwrap_or_default().to_owned();
            Some(TermEvent::Notification {
                title: Some(title),
                body,
            })
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{EscEvent, EscScanner, MAX_APC_PAYLOAD, MAX_OSC_IMAGE_BYTES, MAX_OSC_NOTIFY_BYTES};

    #[test]
    fn osc_notify_scan_takes_both_terminators() {
        let mut bel = EscScanner::default();
        assert_eq!(
            scan_osc(&mut bel, b"\x1b]9;hi\x07"),
            vec![(9, b"hi".to_vec())]
        );

        let mut st = EscScanner::default();
        assert_eq!(
            scan_osc(&mut st, b"\x1b]9;hi\x1b\\"),
            vec![(9, b"hi".to_vec())]
        );
    }

    #[test]
    fn osc_notify_scan_retains_a_split_sequence() {
        let mut scanner = EscScanner::default();
        assert!(scan_osc(&mut scanner, b"\x1b]9;he").is_empty());
        assert_eq!(
            scan_osc(&mut scanner, b"llo\x07"),
            vec![(9, b"hello".to_vec())]
        );
    }

    #[test]
    fn osc_notify_scan_skips_other_codes_unbuffered() {
        let mut scanner = EscScanner::default();
        assert_eq!(
            scan_osc(&mut scanner, b"\x1b]52;c;QUJD\x07\x1b]9;ping\x07"),
            vec![(9, b"ping".to_vec())],
            "an OSC 52 write is skipped and the following OSC 9 is still found"
        );
    }

    #[test]
    fn osc_notify_scan_discards_over_the_cap() {
        let mut scanner = EscScanner::default();
        let mut seq = b"\x1b]9;".to_vec();
        seq.resize(seq.len() + MAX_OSC_NOTIFY_BYTES + 1, b'a');
        seq.push(0x07);
        assert!(
            scan_osc(&mut scanner, &seq).is_empty(),
            "a payload past the cap is dropped"
        );
        assert_eq!(
            scan_osc(&mut scanner, b"\x1b]9;ok\x07"),
            vec![(9, b"ok".to_vec())],
            "the scanner recovers for the next sequence"
        );
    }

    #[test]
    fn osc_notify_scan_skips_long_plain_runs() {
        let mut scanner = EscScanner::default();
        let mut input = vec![b'.'; 8192];
        input.extend_from_slice(b"\x1b]9;ping\x07");

        assert_eq!(scan_osc(&mut scanner, &input), vec![(9, b"ping".to_vec())]);
    }

    #[test]
    fn osc_notify_scan_output_is_split_invariant() {
        let input = b"log\x1b]9;alpha\x07gap\x1b]52;c;QQ==\x07\x1b]777;notify;t;b\x1b\\end";
        let whole = scan_osc(&mut EscScanner::default(), input);

        for split in 0..=input.len() {
            let mut scanner = EscScanner::default();
            let mut got = scan_osc(&mut scanner, &input[..split]);
            got.extend(scan_osc(&mut scanner, &input[split..]));
            assert_eq!(got, whole, "split at {split} changed the notifications");
        }
    }

    #[test]
    fn xtversion_scan_skips_long_plain_runs() {
        let mut scanner = EscScanner::default();
        let mut input = vec![b'.'; 8192];
        input.extend_from_slice(b"\x1b[>q");

        assert_eq!(scan_xtversion(&mut scanner, &input), 1);
    }

    #[test]
    fn xtversion_scan_output_is_split_invariant() {
        let input = b"a\x1b[>4;1mb\x1b[>qc\x1b[>0q";
        let whole = scan_xtversion(&mut EscScanner::default(), input);
        assert_eq!(whole, 2, "two queries; the modify-keys CSI is not one");

        for split in 0..=input.len() {
            let mut scanner = EscScanner::default();
            let got = scan_xtversion(&mut scanner, &input[..split])
                + scan_xtversion(&mut scanner, &input[split..]);
            assert_eq!(got, whole, "split at {split} changed the hit count");
        }
    }

    /// One walk finds all three sequence kinds, whatever order they arrive in.
    #[test]
    fn one_scan_finds_every_kind_interleaved() {
        let mut scanner = EscScanner::default();
        let input =
            b"a\x1b_Gstoatty;x\x1b\\b\x1b[>0q\x1b]9;ping\x07c\x1b_second\x07\x1b[>q\x1b]777;notify;t;b\x1b\\";

        let mut apc = Vec::new();
        let mut queries = 0;
        let mut notes = Vec::new();
        let mut resets = 0;
        scanner.scan(input, &mut |event| match event {
            EscEvent::Apc { payload, .. } => apc.push(payload.to_vec()),
            EscEvent::XtVersion => queries += 1,
            EscEvent::OscNotify { code, payload } => notes.push((code, payload.to_vec())),
            EscEvent::Ris => resets += 1,
            EscEvent::OscImage { .. } => panic!("this stream carries no image escape"),
        });
        assert_eq!(resets, 0, "this stream carries no full reset");

        assert_eq!(
            (apc, queries, notes),
            (
                vec![b"Gstoatty;x".to_vec(), b"second".to_vec()],
                2,
                vec![(9, b"ping".to_vec()), (777, b"notify;t;b".to_vec()),],
            ),
            "each sequence is recognized once, in one pass over the chunk",
        );
    }

    /// A sequence written inside another's payload belongs to that payload.
    ///
    /// One state machine reads the stream the way the vte parser does, so an APC
    /// payload holding what looks like a query is payload, not a query. Only
    /// malformed input can reach this, since a well-formed stream closes a sequence
    /// before opening the next.
    #[test]
    fn a_sequence_inside_a_payload_is_not_its_own() {
        let mut scanner = EscScanner::default();

        // The interior ESC abandons the APC frame, and the query bytes after it are
        // the abandoned payload's rather than a query of their own.
        assert_eq!(
            scan_xtversion(&mut scanner, b"\x1b_pay\x1b[>qload\x1b\\"),
            0,
            "a query inside an APC payload is part of the payload"
        );

        // An OSC 52 write is skipped to its terminator, so an APC frame drawn inside
        // it is skipped with it.
        let mut scanner = EscScanner::default();
        assert!(
            scan_collect(&mut scanner, b"\x1b]52;c;\x1b_Gstoatty;x\x1b\\\x07").is_empty(),
            "an APC frame inside an OSC payload is part of that payload"
        );
    }

    /// The APC payloads and their end offsets one scan of `bytes` completes.
    fn scan_collect(scanner: &mut EscScanner, bytes: &[u8]) -> Vec<(Vec<u8>, usize)> {
        let mut out = Vec::new();
        scanner.scan(bytes, &mut |event| {
            if let EscEvent::Apc { payload, end, .. } = event {
                out.push((payload.to_vec(), end));
            }
        });
        out
    }

    /// The interiors one scan of `bytes` reports, as the slices they name.
    fn scan_interiors<'a>(scanner: &mut EscScanner, bytes: &'a [u8]) -> Vec<&'a [u8]> {
        let mut out = Vec::new();
        scanner.scan(bytes, &mut |event| {
            if let EscEvent::Apc { interior, .. } = event {
                out.push(&bytes[interior]);
            }
        });
        out
    }

    /// How many XTVERSION queries one scan of `bytes` completes.
    fn scan_xtversion(scanner: &mut EscScanner, bytes: &[u8]) -> usize {
        let mut hits = 0;
        scanner.scan(bytes, &mut |event| {
            if matches!(event, EscEvent::XtVersion) {
                hits += 1;
            }
        });
        hits
    }

    /// The OSC notifications one scan of `bytes` completes, as `(code, payload)`.
    fn scan_osc(scanner: &mut EscScanner, bytes: &[u8]) -> Vec<(u32, Vec<u8>)> {
        let mut out = Vec::new();
        scanner.scan(bytes, &mut |event| {
            if let EscEvent::OscNotify { code, payload } = event {
                out.push((code, payload.to_vec()));
            }
        });
        out
    }

    #[test]
    fn scans_single_apc_frame() {
        let mut scanner = EscScanner::default();

        assert_eq!(
            scan_collect(&mut scanner, b"\x1b_Gstoatty;border\x1b\\"),
            vec![(b"Gstoatty;border".to_vec(), 19)]
        );
    }

    #[test]
    fn scans_frame_split_across_calls() {
        let mut scanner = EscScanner::default();

        assert!(scan_collect(&mut scanner, b"\x1b_Gstoat").is_empty());
        assert_eq!(
            scan_collect(&mut scanner, b"ty;x\x1b\\"),
            vec![(b"Gstoatty;x".to_vec(), 6)]
        );
    }

    #[test]
    fn scans_bel_terminated_frame() {
        let mut scanner = EscScanner::default();

        assert_eq!(
            scan_collect(&mut scanner, b"\x1b_foo\x07"),
            vec![(b"foo".to_vec(), 6)]
        );
    }

    #[test]
    fn scans_frame_between_text() {
        let mut scanner = EscScanner::default();

        assert_eq!(
            scan_collect(&mut scanner, b"a\x1b_foo\x1b\\b"),
            vec![(b"foo".to_vec(), 8)]
        );
    }

    #[test]
    fn scans_two_frames_in_one_chunk() {
        let mut scanner = EscScanner::default();

        assert_eq!(
            scan_collect(&mut scanner, b"\x1b_a\x1b\\\x1b_b\x1b\\"),
            vec![(b"a".to_vec(), 5), (b"b".to_vec(), 10)]
        );
    }

    #[test]
    fn apc_scan_discards_over_the_cap() {
        // Without this the payload buffer grows with whatever a child sends,
        // since an APC frame is only bounded by its terminator arriving.
        let mut scanner = EscScanner::default();
        let mut seq = b"\x1b_".to_vec();
        seq.resize(seq.len() + MAX_APC_PAYLOAD + 1, b'a');
        seq.extend_from_slice(b"\x1b\\");
        assert!(
            scan_collect(&mut scanner, &seq).is_empty(),
            "a payload past the cap is dropped",
        );
        assert_eq!(
            scan_collect(&mut scanner, b"\x1b_ok\x1b\\"),
            vec![(b"ok".to_vec(), 6)],
            "the scanner recovers for the next frame",
        );
    }

    /// The vte parser stays inside the APC string until its terminator, so a
    /// scanner that returned to ground on overflow would decode a frame buried
    /// in the tail and apply a command the parser never saw as one.
    #[test]
    fn a_frame_inside_an_over_cap_apc_tail_is_not_decoded() {
        let mut scanner = EscScanner::default();
        let mut seq = b"\x1b_".to_vec();
        seq.resize(seq.len() + MAX_APC_PAYLOAD + 1, b'a');
        seq.extend_from_slice(b"\x1b_Gstoatty;fill\x1b\\");
        seq.extend_from_slice(b"\x1b\\");

        assert!(
            scan_collect(&mut scanner, &seq).is_empty(),
            "nothing buried in the over-cap string is decoded",
        );
        assert_eq!(
            scan_collect(&mut scanner, b"\x1b_ok\x1b\\"),
            vec![(b"ok".to_vec(), 6)],
            "and the scanner still recovers for the next frame",
        );
    }

    #[test]
    fn scans_a_large_payload_split_across_calls() {
        // A pool fill arrives as several reads, so a run taken from one chunk
        // has to leave the payload open for the next rather than ending at the
        // chunk edge. The halves are uneven so a run that assumed it saw the
        // whole payload would land the frame short.
        let payload: Vec<u8> = (0..40_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        let mut input = vec![b'\x1b', b'_'];
        input.extend_from_slice(&payload);
        input.extend_from_slice(b"\x1b\\");

        let split = 9_000;
        let mut scanner = EscScanner::default();
        assert!(
            scan_collect(&mut scanner, &input[..split]).is_empty(),
            "no terminator has arrived yet",
        );
        assert_eq!(
            scan_collect(&mut scanner, &input[split..]),
            vec![(payload, input.len() - split)],
            "the whole payload, ending one past the terminator in the second chunk",
        );
    }

    #[test]
    fn csi_and_plain_text_yield_no_frames() {
        let mut scanner = EscScanner::default();

        assert!(scan_collect(&mut scanner, b"hello\x1b[31mworld").is_empty());
    }

    #[test]
    fn apc_scan_skips_long_plain_runs() {
        let mut scanner = EscScanner::default();
        let mut input = vec![b'.'; 8192];
        input.extend_from_slice(b"\x1b_frame\x07");
        let offset = input.len();

        assert_eq!(
            scan_collect(&mut scanner, &input),
            vec![(b"frame".to_vec(), offset)]
        );
    }

    /// A caller passing the stream on to another parser drops the interior and
    /// keeps the rest, so the interior must name payload bytes and nothing else.
    ///
    /// Keeping the introducer and the terminator is what lets that parser open
    /// and close the string, and `ESC ESC \` is here because it terminates one
    /// while being a byte longer than the terminator it ends with.
    #[test]
    fn an_apc_interior_names_the_payload_and_neither_end_of_it() {
        let cases: [(&[u8], &[&[u8]]); 5] = [
            (b"\x1b_alpha\x1b\\", &[b"alpha"]),
            (
                b"pre\x1b_alpha\x1b\\mid\x1b_beta\x07post",
                &[b"alpha", b"beta"],
            ),
            (b"\x1b_\x1b\\", &[b""]),
            (b"\x1b_alpha\x1b\x1b\\", &[b"alpha"]),
            (b"\x1b_\x07", &[b""]),
        ];

        for (input, expected) in cases {
            assert_eq!(
                scan_interiors(&mut EscScanner::default(), input),
                expected,
                "interiors of {input:?}",
            );
        }
    }

    /// However the stream is split, an interior names payload bytes and only
    /// payload bytes, which is what makes dropping it safe for a parser reading
    /// the same stream.
    ///
    /// Only the call that completes a payload reports one, and it reports what
    /// arrived in that call. Bytes buffered by an earlier call go unreported and
    /// are passed on as they always were, so a split payload is skipped from
    /// wherever it resumed rather than not at all.
    #[test]
    fn an_apc_interior_is_a_tail_of_its_payload_however_it_is_split() {
        let input = b"head\x1b_alpha beta\x1b\\tail";
        let payload: &[u8] = b"alpha beta";
        let introducer_end = 6;

        for split in 0..=input.len() {
            let mut scanner = EscScanner::default();
            let mut seen: Vec<u8> = Vec::new();
            for part in [&input[..split], &input[split..]] {
                for interior in scan_interiors(&mut scanner, part) {
                    seen.extend_from_slice(interior);
                }
            }

            assert!(
                payload.ends_with(&seen),
                "split at {split} reported {seen:?}, which is not payload",
            );
            if split <= introducer_end {
                assert_eq!(
                    seen, payload,
                    "split at {split} left the payload whole in one call",
                );
            }
        }
    }

    #[test]
    fn apc_scan_payloads_are_split_invariant() {
        let input = b"pre\x1b_alpha\x1b\\mid\x1b_beta\x07post";
        let whole: Vec<Vec<u8>> = scan_collect(&mut EscScanner::default(), input)
            .into_iter()
            .map(|(payload, _)| payload)
            .collect();

        for split in 0..=input.len() {
            let mut scanner = EscScanner::default();
            let mut got: Vec<Vec<u8>> = scan_collect(&mut scanner, &input[..split])
                .into_iter()
                .map(|(payload, _)| payload)
                .collect();
            got.extend(
                scan_collect(&mut scanner, &input[split..])
                    .into_iter()
                    .map(|(p, _)| p),
            );
            assert_eq!(got, whole, "split at {split} changed the payloads");
        }
    }

    /// A full reset is one byte after `ESC`, so it must not be confused with an
    /// escape sequence that merely starts the same way.
    #[test]
    fn a_full_reset_is_reported_and_its_neighbors_are_not() {
        let mut scanner = EscScanner::default();
        let mut resets = 0;
        let mut count = |input: &[u8]| {
            resets = 0;
            scanner.scan(input, &mut |event| {
                if matches!(event, EscEvent::Ris) {
                    resets += 1;
                }
            });
            resets
        };

        assert_eq!(count(b"\x1bc"), 1, "ESC c is a full reset");
        assert_eq!(count(b"a\x1bcb\x1bc"), 2, "amid other output too");
        assert_eq!(count(b"\x1b[2J"), 0, "an erase is not");
        assert_eq!(count(b"\x1b[c"), 0, "nor a device-attributes query");
        assert_eq!(
            count(b"\x1b_Gc;\x1b\\"),
            0,
            "nor the same byte inside an APC payload",
        );
    }

    /// The payload buffer is shared with the notification and frame paths, and
    /// an image leaves it orders of magnitude larger than either needs. Without
    /// the shrink one image costs the session that memory for its whole life.
    #[test]
    fn an_image_payload_releases_its_buffer_afterward() {
        let mut scanner = EscScanner::default();
        let mut image = Vec::from(b"\x1b]1337;File=inline=1:".as_slice());
        image.extend(std::iter::repeat_n(b'A', MAX_OSC_NOTIFY_BYTES * 4));
        image.extend_from_slice(b"\x1b\\");

        let mut seen = 0;
        scanner.scan(&image, &mut |event| {
            if let EscEvent::OscImage { payload, .. } = event {
                seen = payload.map_or(0, <[u8]>::len);
            }
        });

        assert_eq!(
            seen,
            MAX_OSC_NOTIFY_BYTES * 4 + "File=inline=1:".len(),
            "the whole payload is reported",
        );
        assert!(
            scanner.payload.capacity() <= MAX_OSC_NOTIFY_BYTES,
            "and the buffer is back to what a notification needs, not {}",
            scanner.payload.capacity(),
        );
    }

    /// A payload past the cap is dropped, but its bytes still have to be
    /// excised. The parser this cap protects would otherwise buffer the whole
    /// thing itself.
    #[test]
    fn an_oversize_image_reports_no_payload_and_still_names_its_bytes() {
        let mut scanner = EscScanner::default();
        // The interior starts one past the code's semicolon, so everything the
        // client sent after `1337;` is what gets excised.
        let prefix = b"\x1b]1337;";
        let mut image = Vec::from(prefix.as_slice());
        image.extend_from_slice(b"File=inline=1:");
        image.extend(std::iter::repeat_n(b'A', MAX_OSC_IMAGE_BYTES + 1));
        image.extend_from_slice(b"\x1b\\");

        let mut seen = None;
        scanner.scan(&image, &mut |event| {
            if let EscEvent::OscImage {
                payload,
                interior,
                end,
            } = event
            {
                seen = Some((payload.is_some(), interior, end));
            }
        });

        assert_eq!(
            seen,
            Some((false, prefix.len()..image.len() - 2, image.len())),
            "nothing to draw, but every byte of it named for excision",
        );
    }

    /// An escape split across reads completes on the read that ends it, which is
    /// the ordinary case for an image far larger than one pty read.
    #[test]
    fn an_image_split_across_reads_completes_on_the_last() {
        let mut scanner = EscScanner::default();
        let whole = b"\x1b]1337;File=inline=1:QUJD\x1b\\";

        let mut payloads: Vec<Vec<u8>> = Vec::new();
        for split in 1..whole.len() {
            payloads.clear();
            let mut collect = |event: EscEvent<'_>| {
                if let EscEvent::OscImage { payload, .. } = event {
                    payloads.push(payload.expect("within the cap").to_vec());
                }
            };
            scanner.scan(&whole[..split], &mut collect);
            scanner.scan(&whole[split..], &mut collect);

            assert_eq!(
                payloads,
                [b"File=inline=1:QUJD".to_vec()],
                "split at {split} loses the payload",
            );
        }
    }

    /// Every other 1337 escape belongs to a feature this terminal does not
    /// have, and buffering one would spend the image cap on a line of text.
    #[test]
    fn only_the_image_code_takes_the_buffered_path() {
        let mut scanner = EscScanner::default();
        let mut images = 0;
        scanner.scan(
            b"\x1b]1338;File=inline=1:QQ==\x1b\\\x1b]0;title\x07",
            &mut |event| {
                if matches!(event, EscEvent::OscImage { .. }) {
                    images += 1;
                }
            },
        );

        assert_eq!(images, 0);
    }
}

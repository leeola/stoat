//! Bounds the OSC payload a terminal pane's vte parser is allowed to buffer.
//!
//! A pane advances a vte parser over whatever the child writes. That parser
//! holds an open OSC's payload in a `Vec<u8>` with no bound, and it keeps the
//! capacity for the rest of the session. A program that writes a clipboard
//! escape as large as the buffer it copied therefore costs the pane that many
//! bytes of doubling memmoves on the run loop, and the memory for as long as
//! the pane lives.
//!
//! [`OscCap`] walks the stream ahead of the parser and reports the stretches to
//! forward, leaving out the payload of an OSC that outgrew what its code has
//! any reason to carry. The parser then reads the code and one empty argument,
//! which sets an empty title, and which OSC 52 ignores outright for want of its
//! three arguments. When an earlier read already gave the parser part of that
//! payload, the walk asks for a fresh parser instead, which reads none of it.

use smallvec::SmallVec;
use std::ops::Range;

/// Escape, which opens every sequence this recognizes.
const ESC: u8 = 0x1b;

/// Bell, which ends an OSC in the form most programs write.
const BEL: u8 = 0x07;

/// Byte after `ESC` that opens an OSC string.
const OSC_INTRODUCER: u8 = b']';

/// Byte after `ESC` that ends a string (`ESC \`).
const STRING_TERMINATOR: u8 = b'\\';

/// The OSC code that writes the clipboard.
const OSC_CLIPBOARD: u32 = 52;

/// Cap on the payload of an ordinary OSC, past which its bytes are cut from
/// what the parser sees.
///
/// A plain OSC carries a title, a working directory, a palette, or a hyperlink.
/// The largest of those is a single OSC 4 setting all 256 palette entries,
/// which runs to about 6 KB, so this leaves an order of magnitude of headroom.
/// The bound matters because the parser's OSC buffer keeps its capacity for the
/// pane's life, and the pty reader hands over 64 KiB a call.
pub(crate) const MAX_OSC_PLAIN_BYTES: usize = 64 * 1024;

/// Cap on the payload of an OSC 52.
///
/// A clipboard write carries a whole selection, so it is legitimately far
/// larger than any other code. This bounds a remote copy of an enormous buffer
/// without refusing an ordinary one.
pub(crate) const MAX_OSC_CLIPBOARD_BYTES: usize = 32 * 1024 * 1024;

/// Caps the OSC payloads one pane's parser buffers.
///
/// One of these sits beside the parser and reads the same bytes first. Hold it
/// for the life of the pane: an escape split across two reads is counted on its
/// total, which only a cap outliving the call can do.
pub(crate) struct OscCap {
    state: State,
    /// Code of the open OSC, accumulated digit by digit.
    code: u32,
    /// Payload bytes the open OSC has taken, across every call it spans.
    payload: usize,
    /// Whether the open OSC tripped its cap while the parser held part of it.
    ///
    /// The caller replaced its parser for that string, so the rest of the
    /// string, terminator included, stays out of the fresh one.
    dropped: bool,
    /// Cap on every code but [`OSC_CLIPBOARD`].
    plain_cap: usize,
    /// Cap on [`OSC_CLIPBOARD`], the one code with a reason to be large.
    clipboard_cap: usize,
}

/// Where the walk stands between calls, since an escape can span several.
#[derive(Clone, Copy)]
enum State {
    Ground,
    /// Seen `ESC`.
    Escape,
    /// Seen `ESC ]`, reading the numeric code up to its `;`.
    Prefix,
    /// Inside the payload, counting it against the code's cap.
    Payload,
    /// Seen `ESC` inside a payload, which either terminates the string or ends
    /// it where the parser ends it.
    PayloadEscape,
}

impl OscCap {
    /// A cap allowing `plain_cap` payload bytes for an ordinary code and
    /// `clipboard_cap` for OSC 52.
    pub(crate) fn new(plain_cap: usize, clipboard_cap: usize) -> OscCap {
        OscCap {
            state: State::Ground,
            code: 0,
            payload: 0,
            dropped: false,
            plain_cap,
            clipboard_cap,
        }
    }

    /// Fill `out` with the stretches of `bytes` to hand the parser, in order,
    /// and report whether the parser must be replaced before it reads them.
    ///
    /// Everything reaches the parser but the payload of an OSC past its cap. A
    /// stream carrying no oversized OSC yields one stretch covering the whole
    /// slice, for the cost of one walk for `ESC`.
    ///
    /// The cut opens one byte past the code's `;` rather than where the cap
    /// trips, so a capped OSC 52 leaves an empty argument rather than a
    /// truncated base64 that decodes to a corrupt clipboard.
    ///
    /// An escape spanning several calls is counted on its total. If it trips its
    /// cap after an earlier call forwarded part of its payload, the parser holds
    /// that part, and every way out of the parser's open string dispatches what
    /// it holds. This therefore returns `true` on the call that trips the cap,
    /// and the caller replaces its parser before it reads the stretches. No
    /// stretch comes before the cut on that call, since the carried payload
    /// starts it. The rest of the string, its terminator too, stays out of the
    /// fresh parser, which has no string for it to close.
    ///
    /// The reset has two edges. A fresh `Processor` also drops a DEC 2026
    /// synchronized update in flight, so a capped OSC 52 inside one costs one
    /// partial frame until the program's next redraw. A dropped string ended by
    /// a lone `ESC` as the last byte of a call loses that `ESC` as well, and the
    /// sequence it opens reaches the parser without it.
    pub(crate) fn spans(&mut self, bytes: &[u8], out: &mut SmallVec<[Range<usize>; 2]>) -> bool {
        out.clear();

        let mut i = 0;
        let mut forwarded = 0;
        // The stretch of this call the open payload has taken. A payload
        // carried in from an earlier call has taken nothing here yet, so it
        // starts empty at the front.
        let mut cut = 0..0;
        // Whether the parser holds part of the open payload, still under its
        // cap. Only that string asks for a reset, so this clears when it ends.
        let mut carried = matches!(self.state, State::Payload | State::PayloadEscape)
            && self.payload > 0
            && !self.over();
        let mut reset = false;

        while i < bytes.len() {
            let byte = bytes[i];
            match self.state {
                // Nothing is open, so jump to the next ESC rather than stepping
                // over the plain bytes between.
                State::Ground => match memchr::memchr(ESC, &bytes[i..]) {
                    Some(off) => {
                        self.state = State::Escape;
                        i += off;
                    },
                    None => break,
                },
                State::Escape => {
                    self.state = match byte {
                        OSC_INTRODUCER => {
                            self.code = 0;
                            self.payload = 0;
                            State::Prefix
                        },
                        ESC => State::Escape,
                        _ => State::Ground,
                    };
                },
                State::Prefix => match byte {
                    b'0'..=b'9' => {
                        self.code = self
                            .code
                            .saturating_mul(10)
                            .saturating_add(u32::from(byte - b'0'));
                    },
                    b';' => {
                        cut = i + 1..i + 1;
                        self.state = State::Payload;
                    },
                    BEL => self.state = State::Ground,
                    ESC => {
                        cut = i..i;
                        self.state = State::PayloadEscape;
                    },
                    // An OSC with no `;` has no argument to cut at, so the cut
                    // opens on this byte and takes the rest.
                    _ => {
                        cut = i..i;
                        self.state = State::Payload;
                    },
                },
                State::Payload => match byte {
                    ESC => self.state = State::PayloadEscape,
                    BEL => {
                        reset |= self.finish(&cut, i + 1, carried, &mut forwarded, out);
                        carried = false;
                    },
                    _ => {
                        let run = payload_run(&bytes[i..]);
                        self.payload = self.payload.saturating_add(run);
                        i += run;
                        cut.end = i;
                        continue;
                    },
                },
                State::PayloadEscape => match byte {
                    STRING_TERMINATOR => {
                        reset |= self.finish(&cut, i + 1, carried, &mut forwarded, out);
                        carried = false;
                    },
                    ESC => self.state = State::PayloadEscape,
                    // A lone `ESC` ends an OSC for the parser, which dispatches
                    // what it holds and reads this byte as an escape's start.
                    // Ending here too keeps a cut from swallowing what that
                    // parser goes on to print. The cut stops short of the `ESC`
                    // even for a dropped string, since a fresh parser reads it
                    // as that start too.
                    _ => {
                        reset |= self.finish(&cut, cut.end, carried, &mut forwarded, out);
                        carried = false;
                        self.state = State::Escape;
                        continue;
                    },
                },
            }
            i += 1;
        }

        // An open payload past its cap has no terminator to cut at, and the
        // bytes it took in this call must not reach the parser either.
        if matches!(self.state, State::Payload | State::PayloadEscape) && self.over() {
            reset |= self.announce(carried);
            // A dropped string's trailing `ESC` goes as well. A fresh parser
            // handed it pairs it with whatever byte the next call starts with.
            let end = match (self.dropped, self.state) {
                (true, State::PayloadEscape) => bytes.len(),
                _ => cut.end,
            };
            self.cut_out(&(cut.start..end), &mut forwarded, out);
        }

        if forwarded < bytes.len() {
            out.push(forwarded..bytes.len());
        }
        reset
    }

    /// Close the open OSC, cutting from the parser what its cap keeps out, and
    /// report whether this call dropped the string.
    ///
    /// A payload past its cap loses `cut`. Its terminator follows the payload
    /// and reaches the parser, which needs it to dispatch the code and close
    /// the string. For a dropped string the fresh parser has no string open, so
    /// the cut runs on to `through`, past what of the terminator it must not
    /// read.
    fn finish(
        &mut self,
        cut: &Range<usize>,
        through: usize,
        carried: bool,
        forwarded: &mut usize,
        out: &mut SmallVec<[Range<usize>; 2]>,
    ) -> bool {
        let mut reset = false;
        if self.over() {
            reset = self.announce(carried);
            let end = match self.dropped {
                true => through,
                false => cut.end,
            };
            self.cut_out(&(cut.start..end), forwarded, out);
        }
        self.dropped = false;
        self.state = State::Ground;
        reset
    }

    /// Drop the open OSC when the parser holds part of it, and report whether
    /// this call is the one that dropped it.
    ///
    /// `carried` says the parser holds part of the payload, which is true only
    /// of a string open and under its cap when the call started.
    fn announce(&mut self, carried: bool) -> bool {
        let announced = carried && !self.dropped;
        self.dropped |= announced;
        announced
    }

    /// Leave `cut` out of what the parser sees, closing the stretch before it.
    fn cut_out(
        &self,
        cut: &Range<usize>,
        forwarded: &mut usize,
        out: &mut SmallVec<[Range<usize>; 2]>,
    ) {
        if cut.is_empty() {
            return;
        }
        if *forwarded < cut.start {
            out.push(*forwarded..cut.start);
        }
        *forwarded = cut.end;
    }

    /// Whether the open OSC's payload has passed the cap its code allows.
    fn over(&self) -> bool {
        let cap = match self.code {
            OSC_CLIPBOARD => self.clipboard_cap,
            _ => self.plain_cap,
        };
        self.payload > cap
    }
}

/// How many leading bytes of `rest` belong to a payload, stopping at the
/// terminator that ends it or at the end of what has arrived.
///
/// Zero exactly when `rest` opens on a terminator, which is why the caller
/// takes a run only from a byte it has already seen is not one. A zero-length
/// run would leave the walk where it was.
fn payload_run(rest: &[u8]) -> usize {
    memchr::memchr2(ESC, BEL, rest).unwrap_or(rest.len())
}

#[cfg(test)]
mod tests {
    use super::OscCap;
    use smallvec::SmallVec;

    /// Whether the call asks for a fresh parser, and what the parser sees of
    /// `bytes`: every stretch the cap forwards, joined back together.
    fn walk(cap: &mut OscCap, bytes: &[u8]) -> (bool, Vec<u8>) {
        let mut spans = SmallVec::new();
        let reset = cap.spans(bytes, &mut spans);
        let seen = spans
            .into_iter()
            .flat_map(|span| bytes[span].to_vec())
            .collect();
        (reset, seen)
    }

    /// What the parser sees of `bytes`, for a call that stays with one parser.
    fn forwarded(cap: &mut OscCap, bytes: &[u8]) -> Vec<u8> {
        walk(cap, bytes).1
    }

    #[test]
    fn a_chunk_with_no_escape_forwards_whole() {
        let mut cap = OscCap::new(8, 16);
        assert_eq!(forwarded(&mut cap, b"plain output\n"), b"plain output\n");
    }

    #[test]
    fn an_osc_under_its_cap_forwards_whole() {
        let mut cap = OscCap::new(8, 16);
        assert_eq!(
            forwarded(&mut cap, b"\x1b]0;title\x07"),
            b"\x1b]0;title\x07"
        );
    }

    /// The parser is left the code and one empty argument, which is an empty
    /// title for OSC 0 and too few arguments for OSC 52 to act on.
    #[test]
    fn an_osc_past_its_cap_keeps_its_frame_and_loses_its_payload() {
        let mut cap = OscCap::new(8, 16);
        assert_eq!(
            forwarded(&mut cap, b"\x1b]0;a much longer title\x07after"),
            b"\x1b]0;\x07after",
        );
    }

    /// A clipboard write is the one code with a reason to be large, so a
    /// payload well past the plain cap still reaches the parser.
    #[test]
    fn an_osc_52_takes_the_clipboard_cap() {
        let mut cap = OscCap::new(8, 64);
        let seq = b"\x1b]52;c;QUFBQUFBQUFBQUFB\x1b\\";
        assert_eq!(forwarded(&mut cap, seq), seq);
    }

    /// The count carries across calls, or an escape delivered in pieces passes a
    /// cap its whole exceeds many times over. The parser holds what the first
    /// call forwarded, and every way out of its string dispatches that, so the
    /// call that trips the cap asks for a fresh parser and forwards nothing of
    /// the string, not even its terminator.
    #[test]
    fn a_payload_the_parser_already_holds_asks_for_a_reset() {
        let mut cap = OscCap::new(8, 16);
        assert_eq!(
            walk(&mut cap, b"\x1b]0;abcde"),
            (false, b"\x1b]0;abcde".to_vec()),
        );
        assert_eq!(walk(&mut cap, b"fghij\x07"), (true, Vec::new()));
    }

    /// A fresh parser has no string open, so a dropped string's terminator stays
    /// out of it even when its `ESC` and its `\` arrive in different calls. A
    /// fresh parser handed the `ESC` alone pairs it with the next call's first
    /// byte.
    #[test]
    fn a_reset_string_keeps_its_terminator_from_the_parser() {
        let mut cap = OscCap::new(8, 16);
        assert_eq!(
            walk(&mut cap, b"\x1b]0;abcde"),
            (false, b"\x1b]0;abcde".to_vec()),
        );
        assert_eq!(walk(&mut cap, b"fghij\x1b"), (true, Vec::new()));
        assert_eq!(walk(&mut cap, b"\\after"), (false, b"after".to_vec()));
    }

    /// Only the string the parser holds part of asks for a reset. One that
    /// opens and trips its cap within a call leaves the parser a string of its
    /// own to close, even when a carried string ended earlier in that call.
    #[test]
    fn a_string_that_opens_and_trips_in_one_call_asks_for_no_reset() {
        let mut cap = OscCap::new(8, 16);
        assert_eq!(
            walk(&mut cap, b"\x1b]0;abc"),
            (false, b"\x1b]0;abc".to_vec()),
        );
        assert_eq!(
            walk(&mut cap, b"de\x07\x1b]0;far too long\x07"),
            (false, b"de\x07\x1b]0;\x07".to_vec()),
        );
    }

    /// A lone `ESC` ends an OSC for the parser too, so a cut that ran past it
    /// would swallow what the parser goes on to print.
    #[test]
    fn a_lone_escape_ends_the_cut_where_the_parser_ends_the_string() {
        let mut cap = OscCap::new(4, 16);
        assert_eq!(
            forwarded(&mut cap, b"\x1b]0;oversized\x1b[31mred"),
            b"\x1b]0;\x1b[31mred",
        );
    }

    /// An OSC carrying no argument separator has nothing to cut at but its
    /// first payload byte.
    #[test]
    fn an_osc_with_no_semicolon_cuts_from_its_first_payload_byte() {
        let mut cap = OscCap::new(4, 16);
        assert_eq!(forwarded(&mut cap, b"\x1b]0abcdefghij\x07"), b"\x1b]0\x07");
    }

    /// An oversized escape that has not ended yet still leaves this call's
    /// bytes behind. The parser's buffer is what the cap protects, and it fills
    /// whether or not the escape ever terminates.
    #[test]
    fn an_unterminated_oversize_payload_is_cut_in_the_call_that_carries_it() {
        let mut cap = OscCap::new(4, 16);
        assert_eq!(forwarded(&mut cap, b"\x1b]0;abcdefghij"), b"\x1b]0;");
    }
}

//! The ink command colors the text runs and the bars after it from the
//! terminal's palette.
//!
//! A text run and a bar state their color as RGB. The output of a program
//! names the 16 colors and the indexed colors of the terminal instead, which a
//! cell resolves through the terminal's palette and the program's OSC 4
//! overrides, and a program that draws such output as runs does not know that
//! palette. An `ink` frame names an entry of it, and the text runs and the bars
//! after it draw in the color of that entry in place of the RGB they state.
//!
//! The ink colors the glyphs of a run and the fill of a bar, and a run keeps
//! the `bg` it states. It holds for every later `text_run`, `text_runs` entry,
//! `bar`, and `bars` entry, until the next `ink`, `lane_begin`, `lane_end`, or
//! `reset`. An ink of no entry ends it.
//!
//! Inside a `fill` or a `fill_decorations` scope, the ink holds for the text
//! runs and the bars of that page alone, and it ends with the scope. The live
//! lane keeps its own ink.
//!
//! The terminal resolves the entry when it stores the run or the bar. A later
//! change of the palette reaches the run with its next declaration.
//!
//! A terminal before protocol version 44 ignores the frame and draws each run
//! and bar in its own RGB, so send an ink only to a terminal at
//! [`crate::INK_PROTOCOL`] or later.

use crate::frame;

/// Encode a [`super::Command::Ink`] as a full `Gstoatty;ink` frame for an
/// emitter.
pub fn encode_ink(ink: Option<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    encode_ink_into(&mut out, ink);
    out
}

/// Append a `Gstoatty;ink` frame of `ink` to `out`.
///
/// The ink rides one 2-byte argument, a 1 and the palette index, or two zeros
/// for no entry.
pub fn encode_ink_into(out: &mut Vec<u8>, ink: Option<u8>) {
    let arg = match ink {
        Some(index) => [1, index],
        None => [0, 0],
    };
    frame::begin(out, "ink");
    frame::push_arg(out, |w| w.write_all(&arg));
    frame::end(out);
}

pub(super) fn decode_ink(args: &[Vec<u8>]) -> Option<Option<u8>> {
    let arg: &[u8; 2] = args.first()?.get(..2)?.try_into().ok()?;

    Some((arg[0] != 0).then_some(arg[1]))
}

#[cfg(test)]
mod tests {
    use super::encode_ink;
    use crate::command::{decode, Command};

    #[test]
    fn ink_round_trips() {
        assert_eq!(
            [encode_ink(Some(9)), encode_ink(None)],
            [
                b"\x1b_Gstoatty;ink;AQk=\x1b\\".to_vec(),
                b"\x1b_Gstoatty;ink;AAA=\x1b\\".to_vec(),
            ],
            "a 1 and the index, or two zeros",
        );
        assert_eq!(
            [Some(9), Some(0), Some(255), None].map(|ink| decode(&encode_ink(ink))),
            [
                Some(Command::Ink(Some(9))),
                Some(Command::Ink(Some(0))),
                Some(Command::Ink(Some(255))),
                Some(Command::Ink(None)),
            ],
        );
    }

    #[test]
    fn rejects_a_short_ink_payload() {
        assert_eq!(
            decode(b"Gstoatty;ink;AQ=="),
            None,
            "one byte, one short of an ink",
        );
        assert_eq!(decode(b"Gstoatty;ink"), None, "the payload is required");
    }
}

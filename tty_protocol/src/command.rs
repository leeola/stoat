//! The typed command surface: a parsed [`Frame`] dispatched by its namespaced
//! sub-command.
//!
//! [`decode`] is the terminal-facing entry point. It returns `None` for any
//! frame the terminal should ignore, whether malformed or carrying a
//! sub-command this build does not recognize, so an unsupported feature
//! degrades to nothing rather than erroring.

use crate::frame::{self, Frame};

/// A decoded stoatty command.
///
/// Feature sub-codes (borders, scaled text, popovers) add their variants here
/// as those items land. Until the first does, no sub-command is recognized, so
/// every well-formed frame is ignored.
#[non_exhaustive]
#[derive(Debug)]
pub enum Command {}

/// Decode a stoatty APC frame into a typed [`Command`], or `None` to ignore it.
///
/// `None` covers both a malformed frame and a well-formed one whose
/// sub-command is unknown to this build. Ignoring rather than erroring is what
/// lets the same byte stream degrade to nothing in another terminal.
pub fn decode(bytes: &[u8]) -> Option<Command> {
    let frame = frame::decode(bytes)?;
    dispatch(&frame)
}

/// Map a parsed [`Frame`] to its [`Command`] by sub-command name.
///
/// Feature items add an arm per sub-code; until the first lands, nothing is
/// recognized and every frame is ignored.
fn dispatch(_frame: &Frame) -> Option<Command> {
    None
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn ignores_unknown_subcommand() {
        assert!(decode(b"Gstoatty;nope").is_none());
    }

    #[test]
    fn ignores_malformed_frame() {
        assert!(decode(b"garbage").is_none());
    }
}

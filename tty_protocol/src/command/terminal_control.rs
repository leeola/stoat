//! The terminal-control commands carry the handshake and the terminal-wide state
//! changes.
//!
//! None of them names a region. They identify the two peers to each other, reset
//! the scene, reload the config, and step the font size.

use super::Command;
use crate::frame::{self, Frame};

/// The payload of [`Command::Hello`]: a program's self-identification.
///
/// Sent by a program (the stoat editor) to the terminal (stoatty) so the
/// terminal logs which process, over which session, on which host drives it --
/// the record that ties a remote editor to a local terminal log.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HelloCommand {
    pub pid: u32,
    pub log_id: String,
    pub hostname: String,
    pub version: String,
    /// The program's [`crate::PROTOCOL_VERSION`], or zero from one that predates
    /// the field. Distinct from `version`, which is the program's own package
    /// version and means nothing to the terminal beyond the log.
    pub protocol: u32,
}

/// The terminal's reply to a [`Command::Hello`], identifying the terminal.
///
/// Unlike a [`Command`], this travels terminal-to-program: it arrives as input
/// bytes on the program's stdin, so [`decode`] (the terminal-facing entry) never
/// yields it. Decode it explicitly with [`decode_ident_reply`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IdentReply {
    pub pid: u32,
    pub log_id: String,
    pub hostname: String,
    pub version: String,
    /// The terminal's [`crate::PROTOCOL_VERSION`], or zero from one that
    /// predates the field. This is what an emitter gates a feature on, unlike
    /// `version`, which only names the terminal's own release.
    pub protocol: u32,
}

/// Encode a [`Command::Reset`] as a full `Gstoatty;reset` frame for an emitter.
///
/// The frame carries no arguments; receiving it clears all accumulated stoatty
/// decoration state so the program can redraw its scene from scratch.
pub fn encode_reset() -> Vec<u8> {
    let mut out = Vec::new();
    encode_reset_into(&mut out);
    out
}

/// Append an argument-less `Gstoatty;reset` frame to `out`.
pub fn encode_reset_into(out: &mut Vec<u8>) {
    frame::begin(out, "reset");
    frame::end(out);
}

/// Encode a [`Command::ConfigReload`] as a full `Gstoatty;config_reload` frame
/// for an emitter.
pub fn encode_config_reload() -> Vec<u8> {
    let mut out = Vec::new();
    encode_config_reload_into(&mut out);
    out
}

/// Append an argument-less `Gstoatty;config_reload` frame to `out`.
pub fn encode_config_reload_into(out: &mut Vec<u8>) {
    frame::begin(out, "config_reload");
    frame::end(out);
}

/// Encode a [`Command::ZoomCapture`] as a full `Gstoatty;zoom_capture` frame for
/// an emitter.
pub fn encode_zoom_capture(on: bool) -> Vec<u8> {
    let mut out = Vec::new();
    encode_zoom_capture_into(&mut out, on);
    out
}

/// Append a `Gstoatty;zoom_capture` frame for `on` to `out`.
///
/// The claim rides as the word `on` or `off` rather than a byte, since it is a
/// once-per-session handshake and a readable frame is worth more there than a
/// byte saved.
pub fn encode_zoom_capture_into(out: &mut Vec<u8>, on: bool) {
    frame::begin(out, "zoom_capture");
    frame::push_arg(out, |w| w.write_all(if on { b"on" } else { b"off" }));
    frame::end(out);
}

/// Encode a [`Command::FontStep`] as a full `Gstoatty;font_step` frame for an
/// emitter.
pub fn encode_font_step(delta: i32) -> Vec<u8> {
    let mut out = Vec::new();
    encode_font_step_into(&mut out, delta);
    out
}

/// Append a `Gstoatty;font_step` frame for `delta` to `out`.
pub fn encode_font_step_into(out: &mut Vec<u8>, delta: i32) {
    frame::begin(out, "font_step");
    frame::push_arg(out, |w| write!(w, "{delta}"));
    frame::end(out);
}

/// Encode a [`HelloCommand`] as a full `Gstoatty;hello` frame for an emitter.
pub fn encode_hello(command: &HelloCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_hello_into(&mut out, command);
    out
}

/// Append a `Gstoatty;hello` frame for `command` to `out`.
///
/// The fields ride as separate arguments in field order, with the numbers as
/// decimal strings so the frame stays legible in a raw log. `protocol` comes
/// last because it was appended after the rest.
pub fn encode_hello_into(out: &mut Vec<u8>, command: &HelloCommand) {
    frame::begin(out, "hello");
    frame::push_arg(out, |w| write!(w, "{}", command.pid));
    frame::push_arg(out, |w| w.write_all(command.log_id.as_bytes()));
    frame::push_arg(out, |w| w.write_all(command.hostname.as_bytes()));
    frame::push_arg(out, |w| w.write_all(command.version.as_bytes()));
    frame::push_arg(out, |w| write!(w, "{}", command.protocol));
    frame::end(out);
}

pub(super) fn decode_zoom_capture(args: &[Vec<u8>]) -> Option<Command> {
    let [on, ..] = args else {
        return None;
    };
    match on.as_slice() {
        b"on" => Some(Command::ZoomCapture { on: true }),
        b"off" => Some(Command::ZoomCapture { on: false }),
        _ => None,
    }
}

pub(super) fn decode_font_step(args: &[Vec<u8>]) -> Option<Command> {
    let [delta, ..] = args else {
        return None;
    };
    Some(Command::FontStep {
        delta: std::str::from_utf8(delta).ok()?.parse().ok()?,
    })
}

pub(super) fn decode_hello(args: &[Vec<u8>]) -> Option<HelloCommand> {
    let [pid, log_id, hostname, version, ..] = args else {
        return None;
    };
    Some(HelloCommand {
        pid: std::str::from_utf8(pid).ok()?.parse().ok()?,
        log_id: String::from_utf8(log_id.clone()).ok()?,
        hostname: String::from_utf8(hostname.clone()).ok()?,
        version: String::from_utf8(version.clone()).ok()?,
        protocol: decode_protocol(args.get(4)),
    })
}

/// Encode an [`IdentReply`] as a full `Gstoatty;ident` frame for a terminal.
pub fn encode_ident_reply(reply: &IdentReply) -> Vec<u8> {
    let mut out = Vec::new();
    encode_ident_reply_into(&mut out, reply);
    out
}

/// Append a `Gstoatty;ident` frame for `reply` to `out`.
///
/// The terminal writes this to the program's stdin in answer to a
/// [`Command::Hello`], carrying the same fields.
pub fn encode_ident_reply_into(out: &mut Vec<u8>, reply: &IdentReply) {
    frame::begin(out, "ident");
    frame::push_arg(out, |w| write!(w, "{}", reply.pid));
    frame::push_arg(out, |w| w.write_all(reply.log_id.as_bytes()));
    frame::push_arg(out, |w| w.write_all(reply.hostname.as_bytes()));
    frame::push_arg(out, |w| w.write_all(reply.version.as_bytes()));
    frame::push_arg(out, |w| write!(w, "{}", reply.protocol));
    frame::end(out);
}

/// Decode a `Gstoatty;ident` [`Frame`] into an [`IdentReply`], or `None` when it
/// is not an ident frame or its fields do not parse.
///
/// Separate from [`decode`] because an ident reply travels terminal-to-program
/// and never appears in the terminal-facing command stream.
pub fn decode_ident_reply(frame: &Frame) -> Option<IdentReply> {
    if frame.sub != "ident" {
        return None;
    }
    let [pid, log_id, hostname, version, ..] = frame.args.as_slice() else {
        return None;
    };
    Some(IdentReply {
        pid: std::str::from_utf8(pid).ok()?.parse().ok()?,
        log_id: String::from_utf8(log_id.clone()).ok()?,
        hostname: String::from_utf8(hostname.clone()).ok()?,
        version: String::from_utf8(version.clone()).ok()?,
        protocol: decode_protocol(frame.args.get(4)),
    })
}

/// The protocol version an appended handshake argument carries, or zero when it
/// is absent or unreadable.
///
/// A terminal built before the field existed sends no fifth argument at all, and
/// reports as version zero rather than failing the handshake over it.
fn decode_protocol(arg: Option<&Vec<u8>>) -> u32 {
    arg.and_then(|arg| std::str::from_utf8(arg).ok()?.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::decode;

    #[test]
    fn hello_round_trips() {
        let command = HelloCommand {
            pid: 12345,
            log_id: "20260718-143022-12345".to_string(),
            hostname: "workstation".to_string(),
            version: "0.1.0 (abc)".to_string(),
            protocol: crate::PROTOCOL_VERSION,
        };
        assert_eq!(
            decode(&encode_hello(&command)),
            Some(Command::Hello(command))
        );
    }

    #[test]
    fn ident_reply_round_trips() {
        let reply = IdentReply {
            pid: 4321,
            log_id: "20260718-143000-4321".to_string(),
            hostname: "workstation".to_string(),
            version: "0.2.0".to_string(),
            protocol: crate::PROTOCOL_VERSION,
        };
        let frame = frame::decode(&encode_ident_reply(&reply)).expect("ident frame decodes");
        assert_eq!(decode_ident_reply(&frame), Some(reply));
    }

    /// Every emitter built before the version field sends four arguments, and
    /// a terminal that now expects five still has to read them.
    #[test]
    fn a_handshake_without_a_version_argument_reads_as_version_zero() {
        let hello = HelloCommand {
            pid: 7,
            log_id: "l".to_owned(),
            hostname: "h".to_owned(),
            version: "v".to_owned(),
            protocol: crate::PROTOCOL_VERSION,
        };

        // The frame a build predating the field emits, which is this one minus
        // its appended argument.
        let mut frame = frame::decode(&encode_hello(&hello)).expect("a valid frame");
        assert_eq!(frame.args.len(), 5, "the encoder appends the version");
        frame.args.pop();

        assert!(
            matches!(
                decode(&frame::encode(&frame)),
                Some(Command::Hello(command)) if command.protocol == 0
            ),
            "an older program's hello still decodes, reporting no version"
        );

        let mut reply = frame::decode(&encode_ident_reply(&IdentReply {
            pid: 7,
            log_id: "l".to_owned(),
            hostname: "h".to_owned(),
            version: "v".to_owned(),
            protocol: crate::PROTOCOL_VERSION,
        }))
        .expect("a valid frame");
        reply.args.pop();

        assert_eq!(
            decode_ident_reply(&reply).map(|reply| reply.protocol),
            Some(0),
            "an older terminal's ident still decodes, reporting no version"
        );
    }

    /// The ident reply travels terminal-to-program and decodes through its own
    /// entry point, so it needs the same tolerance proven separately.
    #[test]
    fn ident_reply_tolerates_a_later_versions_extra_argument() {
        let reply = IdentReply {
            pid: 1,
            log_id: "l".to_owned(),
            hostname: "h".to_owned(),
            version: "v".to_owned(),
            protocol: crate::PROTOCOL_VERSION,
        };
        let encoded = encode_ident_reply(&reply);
        let mut frame = frame::decode(&encoded).expect("the encoder emits a valid frame");
        frame.args.push(b"later".to_vec());

        assert_eq!(decode_ident_reply(&frame), Some(reply));
    }

    #[test]
    fn reset_round_trips() {
        assert_eq!(decode(&encode_reset()), Some(Command::Reset));
    }

    #[test]
    fn config_reload_round_trips() {
        assert_eq!(decode(&encode_config_reload()), Some(Command::ConfigReload));
    }

    #[test]
    fn zoom_capture_round_trips_both_states() {
        for on in [true, false] {
            assert_eq!(
                decode(&encode_zoom_capture(on)),
                Some(Command::ZoomCapture { on })
            );
        }
    }

    #[test]
    fn rejects_an_unreadable_zoom_capture_payload() {
        // "eWVz" is base64 for "yes", which is neither of the two words.
        assert!(decode(b"Gstoatty;zoom_capture;eWVz").is_none());
        assert!(
            decode(b"Gstoatty;zoom_capture").is_none(),
            "the claim has to say which way"
        );
    }

    #[test]
    fn font_step_round_trips_in_both_directions() {
        for delta in [1, -1, 4] {
            assert_eq!(
                decode(&encode_font_step(delta)),
                Some(Command::FontStep { delta })
            );
        }
    }

    #[test]
    fn rejects_a_non_numeric_font_step_payload() {
        // "Ymln" is base64 for "big".
        assert!(decode(b"Gstoatty;font_step;Ymln").is_none());
    }
}

//! Best-effort decoder for raw terminal input bytes.
//!
//! The startup ident handshake owns fd 0 before crossterm does, so anything
//! typed during it arrives as raw VT bytes with no parser attached. Crossterm's
//! own parser is not reachable from outside the crate, so this decodes the
//! classes a person produces at launch and skips the rest.
//!
//! Skipping is the point. The alternative this replaces threw the whole buffer
//! away, so recognizing most of it and logging the remainder is strictly better
//! than what it costs.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use std::ops::Range;

/// Decode `bytes` into the events a terminal would have produced for them.
///
/// Recognizes printable UTF-8, the C0 controls, `ESC`-prefixed keys, the CSI
/// and SS3 sequences a keyboard emits, and a bracketed-paste guard pair.
/// Anything else is skipped with a debug log, including a sequence cut short by
/// the end of `bytes`, which is why this is best-effort: the caller reads a
/// fixed window of stdin and a keypress straddling its end arrives in halves.
pub(crate) fn decode(bytes: &[u8]) -> Vec<Event> {
    let mut events = Vec::new();
    let mut at = 0;

    while at < bytes.len() {
        let Some((event, len)) = next_event(&bytes[at..]) else {
            tracing::debug!(
                at,
                remaining = bytes.len() - at,
                "skipping undecodable terminal input byte"
            );
            at += 1;
            continue;
        };
        if let Some(event) = event {
            events.push(event);
        }
        at += len;
    }

    events
}

/// The event at the head of `bytes` and how many bytes it consumed.
///
/// `Some((None, len))` is a run this recognized as a sequence without having a
/// key for it. Consuming it whole is what keeps its parameter bytes from being
/// re-read as typed characters, which would put garbage into the editor rather
/// than merely lose a keypress.
///
/// `None` means the head is not the start of anything, and the caller advances
/// one byte.
fn next_event(bytes: &[u8]) -> Option<(Option<Event>, usize)> {
    match bytes[0] {
        0x1b => Some(escape_sequence(bytes)),
        byte if byte < 0x20 || byte == 0x7f => Some((Some(control(byte)), 1)),
        _ => {
            let text = printable(bytes)?;
            let key = char_key(text.0);
            Some((Some(Event::Key(key)), text.1))
        },
    }
}

/// The event for an `ESC`-introduced sequence, and what it consumed.
///
/// Always consumes the sequence, even one it has no key for, since its bytes
/// are not text and must not be decoded as any.
fn escape_sequence(bytes: &[u8]) -> (Option<Event>, usize) {
    match bytes.get(1) {
        // A lone ESC is the Escape key. Anything following it in the same read
        // makes it an introducer instead, which is the same ambiguity a
        // terminal resolves by timing.
        None => (Some(Event::Key(bare(KeyCode::Esc))), 1),
        Some(b'[') => csi_sequence(bytes),
        Some(b'O') => {
            let code = match bytes.get(2) {
                Some(b'P') => Some(KeyCode::F(1)),
                Some(b'Q') => Some(KeyCode::F(2)),
                Some(b'R') => Some(KeyCode::F(3)),
                Some(b'S') => Some(KeyCode::F(4)),
                _ => None,
            };
            (code.map(|code| Event::Key(bare(code))), bytes.len().min(3))
        },
        // ESC then a printable is Alt+that key, which is how a terminal
        // encodes the Alt modifier without a CSI.
        Some(_) => match printable(&bytes[1..]) {
            Some((ch, len)) => {
                let mut key = char_key(ch);
                key.modifiers |= KeyModifiers::ALT;
                (Some(Event::Key(key)), 1 + len)
            },
            None => (None, bytes.len()),
        },
    }
}

/// The event for a `CSI`-introduced sequence, and what it consumed.
///
/// Parameters run up to the final byte, and a `1;m` suffix carries the
/// modifiers. The paste guards are handled here too, since the opening guard
/// consumes everything through its closing partner.
///
/// A sequence with no final byte was cut off by the end of the read window.
/// There is no more of it coming, so the remainder is consumed and nothing is
/// reported, which loses that one keypress and no others.
fn csi_sequence(bytes: &[u8]) -> (Option<Event>, usize) {
    let params_at = 2;
    let Some(end) = bytes[params_at..]
        .iter()
        .position(|byte| byte.is_ascii_alphabetic() || *byte == b'~')
    else {
        return (None, bytes.len());
    };
    let final_byte = bytes[params_at + end];
    let params = &bytes[params_at..params_at + end];
    let len = params_at + end + 1;

    if final_byte == b'~' && params == b"200" {
        return paste(&bytes[len..], len);
    }
    // A closing guard with no opener is the tail of a paste whose start fell
    // outside the read window. Its text is already lost, so the guard itself
    // carries nothing.
    if final_byte == b'~' && params == b"201" {
        return (None, len);
    }

    let Some((number, modifiers)) = csi_params(params) else {
        return (None, len);
    };
    let code = match (final_byte, number) {
        (b'A', _) => KeyCode::Up,
        (b'B', _) => KeyCode::Down,
        (b'C', _) => KeyCode::Right,
        (b'D', _) => KeyCode::Left,
        (b'H', _) => KeyCode::Home,
        (b'F', _) => KeyCode::End,
        (b'Z', _) => KeyCode::BackTab,
        (b'~', 1) | (b'~', 7) => KeyCode::Home,
        (b'~', 2) => KeyCode::Insert,
        (b'~', 3) => KeyCode::Delete,
        (b'~', 4) | (b'~', 8) => KeyCode::End,
        (b'~', 5) => KeyCode::PageUp,
        (b'~', 6) => KeyCode::PageDown,
        (b'~', n @ 11..=15) => KeyCode::F(n as u8 - 10),
        (b'~', n @ 17..=21) => KeyCode::F(n as u8 - 11),
        (b'~', n @ 23..=26) => KeyCode::F(n as u8 - 12),
        _ => return (None, len),
    };

    let mut key = bare(code);
    key.modifiers = modifiers;
    // A terminal sends Shift-Tab as CSI Z with no modifier parameter, so the
    // shift is in the final byte rather than the suffix.
    if final_byte == b'Z' {
        key.modifiers |= KeyModifiers::SHIFT;
    }
    (Some(Event::Key(key)), len)
}

/// The leading number of a CSI parameter list and the modifiers its `;m`
/// suffix names.
///
/// An empty list means the number defaults to 1, which is what a terminal omits
/// it for. A list with anything but digits and one semicolon is not something
/// this understands.
fn csi_params(params: &[u8]) -> Option<(u16, KeyModifiers)> {
    if params.is_empty() {
        return Some((1, KeyModifiers::NONE));
    }
    let text = std::str::from_utf8(params).ok()?;
    let mut parts = text.split(';');
    let number = parts.next()?.parse().ok()?;
    let modifiers = match parts.next() {
        Some(field) => modifiers_from_param(field.parse().ok()?),
        None => KeyModifiers::NONE,
    };
    match parts.next() {
        Some(_) => None,
        None => Some((number, modifiers)),
    }
}

/// The modifiers a CSI modifier parameter names.
///
/// The wire value is one more than the bitmask, so an unmodified key sends 1.
fn modifiers_from_param(param: u16) -> KeyModifiers {
    let bits = param.saturating_sub(1);
    let mut modifiers = KeyModifiers::NONE;
    if bits & 0b001 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if bits & 0b010 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if bits & 0b100 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }
    modifiers
}

/// The paste event for the text between an opening guard and its closing
/// partner, and the total length including both guards.
///
/// `after_open` starts just past the opening guard, and `open_len` is what it
/// consumed. An unterminated paste takes the rest of the buffer, since the
/// closing guard fell outside the read window and the text is still what was
/// pasted.
fn paste(after_open: &[u8], open_len: usize) -> (Option<Event>, usize) {
    let close = b"\x1b[201~";
    let (text, len) = match after_open
        .windows(close.len())
        .position(|window| window == close)
    {
        Some(at) => (&after_open[..at], open_len + at + close.len()),
        None => (after_open, open_len + after_open.len()),
    };
    (
        Some(Event::Paste(String::from_utf8_lossy(text).into_owned())),
        len,
    )
}

/// The key a C0 control byte stands for.
fn control(byte: u8) -> Event {
    let key = match byte {
        b'\r' | b'\n' => bare(KeyCode::Enter),
        b'\t' => bare(KeyCode::Tab),
        0x08 | 0x7f => bare(KeyCode::Backspace),
        0x1b => bare(KeyCode::Esc),
        // Ctrl strips the top bits off a letter, so 0x01 is Ctrl-A. The two
        // above that this shadows are matched first.
        0x01..=0x1a => KeyEvent::new(
            KeyCode::Char((byte - 1 + b'a') as char),
            KeyModifiers::CONTROL,
        ),
        _ => bare(KeyCode::Null),
    };
    Event::Key(key)
}

/// The character at the head of `bytes` and its encoded length, or `None` when
/// the UTF-8 is invalid or cut short by the end of the buffer.
fn printable(bytes: &[u8]) -> Option<(char, usize)> {
    let len = utf8_len(bytes[0])?;
    let text = std::str::from_utf8(bytes.get(..len)?).ok()?;
    text.chars().next().map(|ch| (ch, len))
}

/// How many bytes the UTF-8 sequence starting with `first` occupies, or `None`
/// when it is a continuation byte or otherwise not a valid start.
fn utf8_len(first: u8) -> Option<usize> {
    match first {
        0x00..=0x7f => Some(1),
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        _ => None,
    }
}

/// The key event for a typed character.
///
/// An uppercase letter carries SHIFT, which is what crossterm reports for one,
/// so a replayed capital compares equal to a live one.
fn char_key(ch: char) -> KeyEvent {
    let modifiers = match ch.is_uppercase() {
        true => KeyModifiers::SHIFT,
        false => KeyModifiers::NONE,
    };
    KeyEvent::new(KeyCode::Char(ch), modifiers)
}

fn bare(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// The first complete APC frame span in `bytes`, from `ESC _` through its
/// `ESC \` or `BEL` terminator inclusive, or `None` if no complete span is
/// present yet.
///
/// A range rather than the bytes, so a caller holding the buffer can splice the
/// frame out and keep what was typed around it. Leading bytes before the
/// introducer are skipped.
pub(crate) fn apc_span(bytes: &[u8]) -> Option<Range<usize>> {
    let start = bytes.windows(2).position(|pair| pair == b"\x1b_")?;
    let rest = &bytes[start..];
    let mut i = 2;
    while i < rest.len() {
        if rest[i] == 0x07 {
            return Some(start..start + i + 1);
        }
        if rest[i] == 0x1b && rest.get(i + 1) == Some(&b'\\') {
            return Some(start..start + i + 2);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn apc_span_covers_a_complete_st_frame() {
        let bytes = b"\x1b_Gstoatty;ident\x1b\\";
        assert_eq!(apc_span(bytes), Some(0..bytes.len()));
    }

    #[test]
    fn apc_span_skips_leading_garbage_and_accepts_bel() {
        let bytes = b"junk\x1b_Gstoatty;ident\x07";
        assert_eq!(apc_span(bytes), Some(4..bytes.len()));
    }

    #[test]
    fn apc_span_is_none_when_the_frame_is_incomplete() {
        assert_eq!(apc_span(b"\x1b_Gstoatty;ident"), None);
    }

    /// Every class a person types at launch, decoded the way crossterm would
    /// report it, so a replayed keystroke drives the same binding a live one
    /// does.
    #[test]
    fn decode_covers_the_classes_a_keyboard_produces() {
        let cases: Vec<(&[u8], Vec<Event>)> = vec![
            (
                b"hi",
                vec![
                    key(KeyCode::Char('h'), KeyModifiers::NONE),
                    key(KeyCode::Char('i'), KeyModifiers::NONE),
                ],
            ),
            // Uppercase carries SHIFT, which is what crossterm reports.
            (b"A", vec![key(KeyCode::Char('A'), KeyModifiers::SHIFT)]),
            (
                "é".as_bytes(),
                vec![key(KeyCode::Char('é'), KeyModifiers::NONE)],
            ),
            (b"\r", vec![key(KeyCode::Enter, KeyModifiers::NONE)]),
            (b"\t", vec![key(KeyCode::Tab, KeyModifiers::NONE)]),
            (b"\x7f", vec![key(KeyCode::Backspace, KeyModifiers::NONE)]),
            (
                b"\x03",
                vec![key(KeyCode::Char('c'), KeyModifiers::CONTROL)],
            ),
            (b"\x1b", vec![key(KeyCode::Esc, KeyModifiers::NONE)]),
            (b"\x1bx", vec![key(KeyCode::Char('x'), KeyModifiers::ALT)]),
            (b"\x1b[A", vec![key(KeyCode::Up, KeyModifiers::NONE)]),
            (b"\x1b[D", vec![key(KeyCode::Left, KeyModifiers::NONE)]),
            // The `1;m` suffix is the modifier, one more than its bitmask, so
            // 5 is Ctrl and 6 is Ctrl+Shift.
            (
                b"\x1b[1;5C",
                vec![key(KeyCode::Right, KeyModifiers::CONTROL)],
            ),
            (
                b"\x1b[1;6C",
                vec![key(
                    KeyCode::Right,
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                )],
            ),
            (b"\x1b[H", vec![key(KeyCode::Home, KeyModifiers::NONE)]),
            (b"\x1b[3~", vec![key(KeyCode::Delete, KeyModifiers::NONE)]),
            (b"\x1b[5~", vec![key(KeyCode::PageUp, KeyModifiers::NONE)]),
            (b"\x1b[Z", vec![key(KeyCode::BackTab, KeyModifiers::SHIFT)]),
            (b"\x1bOP", vec![key(KeyCode::F(1), KeyModifiers::NONE)]),
            (b"\x1b[15~", vec![key(KeyCode::F(5), KeyModifiers::NONE)]),
            (
                b"\x1b[200~pasted\x1b[201~",
                vec![Event::Paste("pasted".into())],
            ),
        ];

        for (bytes, want) in cases {
            assert_eq!(decode(bytes), want, "decoding {bytes:?}");
        }
    }

    /// A run of bytes decodes in order, so replayed input reaches the editor
    /// the way it was typed rather than rearranged.
    #[test]
    fn decode_keeps_a_mixed_run_in_order() {
        assert_eq!(
            decode(b"i\x1b[Bx\x1b"),
            vec![
                key(KeyCode::Char('i'), KeyModifiers::NONE),
                key(KeyCode::Down, KeyModifiers::NONE),
                key(KeyCode::Char('x'), KeyModifiers::NONE),
                key(KeyCode::Esc, KeyModifiers::NONE),
            ],
        );
    }

    /// What this cannot decode costs only itself.
    ///
    /// The buffer is a fixed window of stdin, so a sequence can be cut in half
    /// by its end, and an unfamiliar terminal can send something this has no
    /// case for. Either way the keystrokes around it still arrive, which is the
    /// whole gain over dropping the buffer.
    #[test]
    fn decode_skips_what_it_cannot_read_and_keeps_the_rest() {
        assert_eq!(
            decode(b"a\x1b[999Qb"),
            vec![
                key(KeyCode::Char('a'), KeyModifiers::NONE),
                key(KeyCode::Char('b'), KeyModifiers::NONE),
            ],
            "an unknown final byte drops that sequence alone",
        );
        assert_eq!(
            decode(b"a\x1b[1;5"),
            vec![key(KeyCode::Char('a'), KeyModifiers::NONE)],
            "and a sequence cut off by the window's end takes nothing with it",
        );
    }

    /// Typing around the reply frame survives the splice that removes it.
    ///
    /// This is the whole path the handshake takes: bytes arrive interleaved
    /// with the terminal's answer, the answer is cut out by range, and what is
    /// left is what the person typed.
    #[test]
    fn a_reply_frame_splices_out_leaving_what_was_typed() {
        let mut buf = b"ab\x1b_Gstoatty;ident\x1b\\cd".to_vec();
        let span = apc_span(&buf).expect("a complete frame");
        assert_eq!(&buf[span.clone()], b"\x1b_Gstoatty;ident\x1b\\");

        buf.drain(span);
        assert_eq!(
            decode(&buf),
            vec![
                key(KeyCode::Char('a'), KeyModifiers::NONE),
                key(KeyCode::Char('b'), KeyModifiers::NONE),
                key(KeyCode::Char('c'), KeyModifiers::NONE),
                key(KeyCode::Char('d'), KeyModifiers::NONE),
            ],
            "both sides of the frame are kept, in order",
        );
    }
}

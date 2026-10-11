//! The bytes a terminal sends its child for a key press.
//!
//! A terminal or agent pane passes its child every key the pane does not take
//! for itself, and the child reads those keys as it reads keys in any terminal.
//! So the encoding is xterm's, and a shell's line editor, a pager, and a
//! full-screen program each get the sequence their terminfo entry names.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The numbers F5 to F12 send as `ESC [ n ~`, in key order.
const F5_TO_F12: [u8; 8] = [15, 17, 18, 19, 20, 21, 23, 24];

/// The bytes xterm sends for `key`, or `None` for a key with no encoding,
/// which the pane then swallows.
///
/// `app_cursor` is whether the child turned on application cursor mode
/// (DECCKM). In that mode an unmodified arrow, Home, or End sends `ESC O` and
/// its final byte, which is the key string a program that turns the mode on
/// reads.
///
/// A held Shift, Alt, or Ctrl goes in a named key's xterm parameter
/// `1 + shift + 2 * alt + 4 * ctrl`, so Ctrl-Left is `ESC [ 1 ; 5 D`. Alt puts
/// an `ESC` ahead of a character, Enter, Backspace, and Tab. Ctrl with a
/// character sends its C0 byte. A Ctrl combination with no C0 byte sends
/// nothing, because a shell child reads no CSI-u. Super has no bit.
pub(crate) fn encode(key: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let param = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let cursor_intro = if app_cursor { b'O' } else { b'[' };

    let bytes = match key.code {
        KeyCode::Char(c) if ctrl => alt_prefixed(alt, &[control_byte(c)?]),
        KeyCode::Char(c) => alt_prefixed(alt, c.encode_utf8(&mut [0; 4]).as_bytes()),
        KeyCode::Enter => alt_prefixed(alt, b"\r"),
        KeyCode::Tab => alt_prefixed(alt, b"\t"),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => alt_prefixed(alt, if ctrl { &[0x08] } else { &[0x7f] }),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => final_byte_key(cursor_intro, b'A', param),
        KeyCode::Down => final_byte_key(cursor_intro, b'B', param),
        KeyCode::Right => final_byte_key(cursor_intro, b'C', param),
        KeyCode::Left => final_byte_key(cursor_intro, b'D', param),
        KeyCode::Home => final_byte_key(cursor_intro, b'H', param),
        KeyCode::End => final_byte_key(cursor_intro, b'F', param),
        KeyCode::Insert => tilde_key(2, param),
        KeyCode::Delete => tilde_key(3, param),
        KeyCode::PageUp => tilde_key(5, param),
        KeyCode::PageDown => tilde_key(6, param),
        KeyCode::F(n @ 1..=4) => final_byte_key(b'O', b"PQRS"[usize::from(n - 1)], param),
        KeyCode::F(n @ 5..=12) => tilde_key(F5_TO_F12[usize::from(n - 5)], param),
        _ => return None,
    };
    Some(bytes)
}

/// `bytes` with an `ESC` ahead of them when `alt` is held, which the child's
/// parser reads as Alt on the key that follows.
fn alt_prefixed(alt: bool, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    if alt {
        out.push(0x1b);
    }
    out.extend_from_slice(bytes);
    out
}

/// The bytes of a key that xterm names by a final byte.
///
/// Unmodified, the key sends `ESC`, `intro`, and `final_byte`. Modified, it
/// sends `ESC [ 1 ; param` and `final_byte`, because SS3 carries no parameter.
/// So F1 to F4, and a cursor key under DECCKM, leave their SS3 form once a
/// modifier is held.
fn final_byte_key(intro: u8, final_byte: u8, param: u8) -> Vec<u8> {
    if param == 1 {
        return vec![0x1b, intro, final_byte];
    }
    format!("\x1b[1;{param}{}", char::from(final_byte)).into_bytes()
}

/// The bytes of a key that xterm names by a number.
///
/// Unmodified, the key sends `ESC [ number ~`. Modified, it sends
/// `ESC [ number ; param ~`.
fn tilde_key(number: u8, param: u8) -> Vec<u8> {
    if param == 1 {
        return format!("\x1b[{number}~").into_bytes();
    }
    format!("\x1b[{number};{param}~").into_bytes()
}

/// The C0 control byte for Ctrl held with `c`, or `None` when `c` has none.
///
/// The letters and `@ [ \ ] ^ _` take the low five bits of their uppercase
/// form, so Ctrl-C is `0x03` and `Ctrl-[` is `ESC`. Ctrl-Space is NUL like
/// `Ctrl-@`, and `Ctrl-?` is DEL.
fn control_byte(c: char) -> Option<u8> {
    match c {
        ' ' => Some(0x00),
        '?' => Some(0x7f),
        'a'..='z' | 'A'..='Z' | '@' | '[' | '\\' | ']' | '^' | '_' => {
            Some((c.to_ascii_uppercase() as u8) & 0x1f)
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::encode;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn encodes_each_key_as_xterm_does() {
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        let alt = KeyModifiers::ALT;
        let shift = KeyModifiers::SHIFT;

        let cases: [(KeyCode, KeyModifiers, bool, Option<&[u8]>); 30] = [
            (KeyCode::Char('a'), none, false, Some(b"a")),
            (KeyCode::Char('Z'), shift, false, Some(b"Z")),
            (KeyCode::Char('c'), ctrl, false, Some(b"\x03")),
            (KeyCode::Char('a'), ctrl, false, Some(b"\x01")),
            (KeyCode::Char(' '), ctrl, false, Some(b"\x00")),
            (KeyCode::Char('['), ctrl, false, Some(b"\x1b")),
            (KeyCode::Char('?'), ctrl, false, Some(b"\x7f")),
            (KeyCode::Char('1'), ctrl, false, None),
            (KeyCode::Char('b'), alt, false, Some(b"\x1bb")),
            (KeyCode::Char('x'), ctrl | alt, false, Some(b"\x1b\x18")),
            (KeyCode::Enter, none, false, Some(b"\r")),
            (KeyCode::Enter, alt, false, Some(b"\x1b\r")),
            (KeyCode::Tab, none, false, Some(b"\t")),
            (KeyCode::BackTab, shift, false, Some(b"\x1b[Z")),
            (KeyCode::Backspace, none, false, Some(b"\x7f")),
            (KeyCode::Backspace, ctrl, false, Some(b"\x08")),
            (KeyCode::Esc, none, false, Some(b"\x1b")),
            (KeyCode::Up, none, false, Some(b"\x1b[A")),
            (KeyCode::Down, none, false, Some(b"\x1b[B")),
            (KeyCode::Right, none, false, Some(b"\x1b[C")),
            (KeyCode::Left, none, false, Some(b"\x1b[D")),
            (KeyCode::Home, none, false, Some(b"\x1b[H")),
            (KeyCode::Home, none, true, Some(b"\x1bOH")),
            (KeyCode::Left, ctrl, true, Some(b"\x1b[1;5D")),
            (KeyCode::PageDown, none, false, Some(b"\x1b[6~")),
            (KeyCode::Delete, ctrl, false, Some(b"\x1b[3;5~")),
            (KeyCode::F(1), none, false, Some(b"\x1bOP")),
            (KeyCode::F(4), shift, false, Some(b"\x1b[1;2S")),
            (KeyCode::F(5), none, false, Some(b"\x1b[15~")),
            (KeyCode::F(12), alt, false, Some(b"\x1b[24;3~")),
        ];
        for (code, mods, app_cursor, bytes) in cases {
            assert_eq!(
                encode(&KeyEvent::new(code, mods), app_cursor),
                bytes.map(<[u8]>::to_vec),
                "{code:?} with {mods:?}, application cursor mode {app_cursor}",
            );
        }
    }
}

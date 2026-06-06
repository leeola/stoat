//! Keyboard-to-terminal-input byte encoding.
//!
//! Translates a crossterm [`KeyEvent`] into the byte sequence a terminal
//! program expects on its stdin: control bytes for `Ctrl`-modified keys,
//! `ESC`-prefixed bytes for `Alt`, and CSI / SS3 escape sequences for the
//! named keys (cursor keys, `Home`/`End`, `PageUp`/`PageDown`,
//! `Insert`/`Delete`, function keys). Used by the full-screen terminal
//! view to forward keystrokes straight to the PTY.
//!
//! Cursor keys are emitted in their normal-mode CSI form (`\x1b[A`). The
//! application-cursor-key (DECCKM) SS3 form is not produced here -- that
//! needs the emulator's DECCKM state threaded in.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Encode `event` as the bytes a terminal program reads from stdin, or
/// `None` for a key that produces no input (e.g. a modifier-only event or
/// an unsupported code).
pub fn encode_key(event: KeyEvent) -> Option<Vec<u8>> {
    let m = event.modifiers;
    let alt = m.contains(KeyModifiers::ALT);
    match event.code {
        KeyCode::Char(c) => Some(encode_char(c, m)),
        KeyCode::Enter => Some(with_alt(alt, vec![b'\r'])),
        KeyCode::Tab => Some(with_alt(alt, vec![b'\t'])),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Backspace => Some(with_alt(alt, vec![0x7f])),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(csi_cursor(b'A', m)),
        KeyCode::Down => Some(csi_cursor(b'B', m)),
        KeyCode::Right => Some(csi_cursor(b'C', m)),
        KeyCode::Left => Some(csi_cursor(b'D', m)),
        KeyCode::Home => Some(csi_cursor(b'H', m)),
        KeyCode::End => Some(csi_cursor(b'F', m)),
        KeyCode::Insert => Some(csi_tilde(2, m)),
        KeyCode::Delete => Some(csi_tilde(3, m)),
        KeyCode::PageUp => Some(csi_tilde(5, m)),
        KeyCode::PageDown => Some(csi_tilde(6, m)),
        KeyCode::F(n) => function_key(n, m),
        _ => None,
    }
}

/// Encode a printable character with its `Ctrl`/`Alt` modifiers. `Ctrl`
/// folds ASCII letters and the `@[\]^_` group (plus space) to their C0
/// control byte; `Alt` prefixes the result with `ESC`.
fn encode_char(c: char, m: KeyModifiers) -> Vec<u8> {
    let mut bytes = if m.contains(KeyModifiers::CONTROL) {
        match ctrl_byte(c) {
            Some(b) => vec![b],
            None => char_bytes(c),
        }
    } else {
        char_bytes(c)
    };
    if m.contains(KeyModifiers::ALT) {
        bytes.insert(0, 0x1b);
    }
    bytes
}

fn char_bytes(c: char) -> Vec<u8> {
    c.to_string().into_bytes()
}

/// The C0 control byte for `Ctrl`+`c`, or `None` when `Ctrl` has no
/// defined byte for the character.
fn ctrl_byte(c: char) -> Option<u8> {
    let upper = c.to_ascii_uppercase() as u32;
    match upper {
        0x40..=0x5f => Some((upper & 0x1f) as u8),
        0x20 => Some(0),
        0x3f => Some(0x7f),
        _ => None,
    }
}

/// `1 + shift + 2*alt + 4*ctrl`: the xterm modifier parameter. `1` means
/// no modifiers, in which case the caller emits the unmodified sequence.
fn modifier_param(m: KeyModifiers) -> u8 {
    1 + u8::from(m.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(m.contains(KeyModifiers::ALT))
        + 4 * u8::from(m.contains(KeyModifiers::CONTROL))
}

/// A CSI cursor / `Home` / `End` key with final byte `final_byte`:
/// `\x1b[A` unmodified, `\x1b[1;<m>A` when modified.
fn csi_cursor(final_byte: u8, m: KeyModifiers) -> Vec<u8> {
    let param = modifier_param(m);
    if param == 1 {
        vec![0x1b, b'[', final_byte]
    } else {
        let mut bytes = format!("\x1b[1;{param}").into_bytes();
        bytes.push(final_byte);
        bytes
    }
}

/// A CSI `~`-terminated key numbered `num` (`PageUp` = 5, etc.):
/// `\x1b[5~` unmodified, `\x1b[5;<m>~` when modified.
fn csi_tilde(num: u8, m: KeyModifiers) -> Vec<u8> {
    let param = modifier_param(m);
    if param == 1 {
        format!("\x1b[{num}~").into_bytes()
    } else {
        format!("\x1b[{num};{param}~").into_bytes()
    }
}

/// Encode `F1`..`F12`. `F1`..`F4` use SS3 (`\x1bOP`) unmodified and the
/// CSI `1;<m>` form when modified; `F5`..`F12` use the `~` form.
fn function_key(n: u8, m: KeyModifiers) -> Option<Vec<u8>> {
    let param = modifier_param(m);
    if (1..=4).contains(&n) {
        let final_byte = b'P' + (n - 1);
        return Some(if param == 1 {
            vec![0x1b, b'O', final_byte]
        } else {
            let mut bytes = format!("\x1b[1;{param}").into_bytes();
            bytes.push(final_byte);
            bytes
        });
    }
    let num = match n {
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        12 => 24,
        _ => return None,
    };
    Some(csi_tilde(num, m))
}

fn with_alt(alt: bool, mut bytes: Vec<u8>) -> Vec<u8> {
    if alt {
        bytes.insert(0, 0x1b);
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn key_mod(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn plain_char_is_literal_utf8() {
        assert_eq!(encode_key(key(KeyCode::Char('a'))), Some(b"a".to_vec()));
        assert_eq!(
            encode_key(key(KeyCode::Char('é'))),
            Some("é".as_bytes().to_vec())
        );
    }

    #[test]
    fn ctrl_letters_map_to_control_bytes() {
        assert_eq!(
            encode_key(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(vec![0x03])
        );
        assert_eq!(
            encode_key(key_mod(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Some(vec![0x01])
        );
        assert_eq!(
            encode_key(key_mod(KeyCode::Char(' '), KeyModifiers::CONTROL)),
            Some(vec![0x00])
        );
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(
            encode_key(key_mod(KeyCode::Char('x'), KeyModifiers::ALT)),
            Some(vec![0x1b, b'x'])
        );
    }

    #[test]
    fn named_control_keys() {
        assert_eq!(encode_key(key(KeyCode::Enter)), Some(vec![b'\r']));
        assert_eq!(encode_key(key(KeyCode::Tab)), Some(vec![b'\t']));
        assert_eq!(encode_key(key(KeyCode::Backspace)), Some(vec![0x7f]));
        assert_eq!(encode_key(key(KeyCode::Esc)), Some(vec![0x1b]));
        assert_eq!(encode_key(key(KeyCode::BackTab)), Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn cursor_keys_use_csi() {
        assert_eq!(encode_key(key(KeyCode::Up)), Some(b"\x1b[A".to_vec()));
        assert_eq!(encode_key(key(KeyCode::Down)), Some(b"\x1b[B".to_vec()));
        assert_eq!(encode_key(key(KeyCode::Right)), Some(b"\x1b[C".to_vec()));
        assert_eq!(encode_key(key(KeyCode::Left)), Some(b"\x1b[D".to_vec()));
        assert_eq!(encode_key(key(KeyCode::Home)), Some(b"\x1b[H".to_vec()));
        assert_eq!(encode_key(key(KeyCode::End)), Some(b"\x1b[F".to_vec()));
    }

    #[test]
    fn modified_cursor_keys_carry_param() {
        assert_eq!(
            encode_key(key_mod(KeyCode::Up, KeyModifiers::CONTROL)),
            Some(b"\x1b[1;5A".to_vec())
        );
        assert_eq!(
            encode_key(key_mod(KeyCode::Right, KeyModifiers::SHIFT)),
            Some(b"\x1b[1;2C".to_vec())
        );
        assert_eq!(
            encode_key(key_mod(
                KeyCode::Left,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            Some(b"\x1b[1;6D".to_vec())
        );
    }

    #[test]
    fn tilde_keys() {
        assert_eq!(encode_key(key(KeyCode::PageUp)), Some(b"\x1b[5~".to_vec()));
        assert_eq!(
            encode_key(key(KeyCode::PageDown)),
            Some(b"\x1b[6~".to_vec())
        );
        assert_eq!(encode_key(key(KeyCode::Insert)), Some(b"\x1b[2~".to_vec()));
        assert_eq!(encode_key(key(KeyCode::Delete)), Some(b"\x1b[3~".to_vec()));
        assert_eq!(
            encode_key(key_mod(KeyCode::PageUp, KeyModifiers::CONTROL)),
            Some(b"\x1b[5;5~".to_vec())
        );
    }

    #[test]
    fn function_keys() {
        assert_eq!(encode_key(key(KeyCode::F(1))), Some(b"\x1bOP".to_vec()));
        assert_eq!(encode_key(key(KeyCode::F(4))), Some(b"\x1bOS".to_vec()));
        assert_eq!(encode_key(key(KeyCode::F(5))), Some(b"\x1b[15~".to_vec()));
        assert_eq!(encode_key(key(KeyCode::F(12))), Some(b"\x1b[24~".to_vec()));
        assert_eq!(
            encode_key(key_mod(KeyCode::F(1), KeyModifiers::SHIFT)),
            Some(b"\x1b[1;2P".to_vec())
        );
    }

    #[test]
    fn unsupported_key_is_none() {
        assert_eq!(encode_key(key(KeyCode::Null)), None);
        assert_eq!(encode_key(key(KeyCode::F(13))), None);
    }
}

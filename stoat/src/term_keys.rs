//! The bytes a terminal sends its child for a key press.
//!
//! A terminal or agent pane passes its child every key the pane does not take
//! for itself, and the child reads those keys as it reads keys in any terminal.
//! So the encoding is xterm's, and a shell's line editor, a pager, and a
//! full-screen program each get the sequence their terminfo entry names.
//!
//! A child that pushes kitty keyboard protocol flags gets that protocol's CSI
//! form for the keys its flags name, so it reads Ctrl-Shift-s apart from Ctrl-s
//! and Escape apart from an Alt chord.

use alacritty_terminal::term::TermMode;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The numbers F5 to F12 send as `ESC [ n ~`, in key order.
const F5_TO_F12: [u8; 8] = [15, 17, 18, 19, 20, 21, 23, 24];

/// The bit of Shift in [`kitty_modifier_bits`].
const SHIFT_BIT: u8 = 1;

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
/// nothing, unless the child pushed the kitty keyboard protocol. Super has no
/// bit.
///
/// `keyboard` holds the kitty keyboard protocol flags the child pushed, as
/// [`TermScreen::keyboard_modes`](crate::term_screen::TermScreen::keyboard_modes)
/// reports them. With none pushed, every key keeps the bytes above. So it does
/// with the alternate-keys or associated-text flag alone, because those only
/// reshape a CSI form that another flag asked for.
///
/// The disambiguate flag sends Escape, Shift-Tab, a Shift-held Enter or
/// Backspace, and a key with a modifier other than Shift alone as
/// `CSI code ; mods u`, so Ctrl-Shift-s reaches the child apart from Ctrl-s.
/// The report-all flag sends every key that way. The alternate-keys flag adds a
/// shifted letter after a colon, and the associated-text flag adds the typed
/// text. Super takes the modifier bit 8 in the CSI form. An unmodified cursor
/// key in application cursor mode keeps its SS3 form under every flag.
///
/// The key arrives with Shift folded into a letter's case, so an uppercase
/// ASCII letter counts as Shift held.
pub(crate) fn encode(key: &KeyEvent, app_cursor: bool, keyboard: TermMode) -> Option<Vec<u8>> {
    if takes_csi_u(key, app_cursor, keyboard) {
        return csi_u_sequence(key, keyboard);
    }

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

/// Whether `key` takes the kitty keyboard protocol's CSI form under the flags
/// in `keyboard`, in place of its xterm bytes.
///
/// The disambiguate and report-all flags ask for the form. The alternate-keys
/// and associated-text flags only reshape it, so alone they keep the xterm
/// bytes. An unmodified cursor key in application cursor mode keeps its SS3
/// form under every flag, since a program turns that mode on to read it.
fn takes_csi_u(key: &KeyEvent, app_cursor: bool, keyboard: TermMode) -> bool {
    let report_all = keyboard.contains(TermMode::REPORT_ALL_KEYS_AS_ESC);
    if !report_all && !keyboard.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
        return false;
    }
    let cursor_key = matches!(
        key.code,
        KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right | KeyCode::Home | KeyCode::End
    );
    if app_cursor && key.modifiers.is_empty() && cursor_key {
        return false;
    }
    if report_all {
        return true;
    }

    // Shift alone shapes the text a key types. On Enter and Backspace it
    // shapes no text, so only the CSI form reports it. Shift-Tab arrives as
    // BackTab.
    let modifiers = kitty_modifier_bits(key);
    let shift_counts = matches!(key.code, KeyCode::Enter | KeyCode::Backspace);
    let modified = modifiers != 0 && (modifiers != SHIFT_BIT || shift_counts);
    modified || matches!(key.code, KeyCode::Esc | KeyCode::BackTab)
}

/// The kitty keyboard protocol's CSI form of `key` under the flags in
/// `keyboard`, or `None` for a key that has no code here.
///
/// The form is `CSI code ; mods u`. A navigation or function key keeps its
/// xterm form, with the modifier parameter `1 + mods` when one is held. The
/// modifier field is left out when it is 1 and no text follows.
///
/// A crossterm key carries no base key for shifted punctuation, so only an
/// uppercase ASCII letter reports a shifted codepoint under the alternate-keys
/// flag. The protocol makes that codepoint optional.
fn csi_u_sequence(key: &KeyEvent, keyboard: TermMode) -> Option<Vec<u8>> {
    let modifiers = kitty_modifier_bits(key);
    let param = modifiers + 1;
    let code = match key.code {
        KeyCode::Esc => 27,
        KeyCode::Enter => 13,
        KeyCode::Tab | KeyCode::BackTab => 9,
        KeyCode::Backspace => 127,
        KeyCode::Char(c) => u32::from(c.to_ascii_lowercase()),
        KeyCode::Up => return Some(final_byte_key(b'[', b'A', param)),
        KeyCode::Down => return Some(final_byte_key(b'[', b'B', param)),
        KeyCode::Right => return Some(final_byte_key(b'[', b'C', param)),
        KeyCode::Left => return Some(final_byte_key(b'[', b'D', param)),
        KeyCode::Home => return Some(final_byte_key(b'[', b'H', param)),
        KeyCode::End => return Some(final_byte_key(b'[', b'F', param)),
        KeyCode::Insert => return Some(tilde_key(2, param)),
        KeyCode::Delete => return Some(tilde_key(3, param)),
        KeyCode::PageUp => return Some(tilde_key(5, param)),
        KeyCode::PageDown => return Some(tilde_key(6, param)),
        KeyCode::F(n @ 1..=4) => {
            return Some(final_byte_key(b'O', b"PQRS"[usize::from(n - 1)], param));
        },
        KeyCode::F(n @ 5..=12) => return Some(tilde_key(F5_TO_F12[usize::from(n - 5)], param)),
        _ => return None,
    };

    let mut sequence = format!("\x1b[{code}");
    if keyboard.contains(TermMode::REPORT_ALTERNATE_KEYS)
        && let KeyCode::Char(c) = key.code
        && c.is_ascii_uppercase()
    {
        sequence.push_str(&format!(":{}", u32::from(c)));
    }

    // Ctrl turns the typed character into a control byte, which is no text.
    let text = match key.code {
        KeyCode::Char(c)
            if keyboard.contains(TermMode::REPORT_ASSOCIATED_TEXT)
                && !key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            Some(c)
        },
        _ => None,
    };
    if modifiers != 0 || text.is_some() {
        sequence.push_str(&format!(";{param}"));
    }
    if let Some(c) = text {
        sequence.push_str(&format!(";{}", u32::from(c)));
    }
    sequence.push('u');
    Some(sequence.into_bytes())
}

/// The kitty keyboard protocol's modifier bits for `key`, with shift at `1`,
/// alt at `2`, control at `4`, and super at `8`.
///
/// The key arrives with Shift folded into a letter's case, and Shift-Tab as
/// BackTab, so both count as Shift held.
fn kitty_modifier_bits(key: &KeyEvent) -> u8 {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT)
        || key.code == KeyCode::BackTab
        || matches!(key.code, KeyCode::Char(c) if c.is_ascii_uppercase());
    u8::from(shift)
        | u8::from(key.modifiers.contains(KeyModifiers::ALT)) << 1
        | u8::from(key.modifiers.contains(KeyModifiers::CONTROL)) << 2
        | u8::from(key.modifiers.contains(KeyModifiers::SUPER)) << 3
}

#[cfg(test)]
mod tests {
    use super::encode;
    use alacritty_terminal::term::TermMode;
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
                under(TermMode::empty(), code, mods, app_cursor),
                bytes.map(<[u8]>::to_vec),
                "{code:?} with {mods:?}, application cursor mode {app_cursor}",
            );
        }
    }

    #[test]
    fn csi_u_reports_modified_keys_under_disambiguate() {
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        let shift = KeyModifiers::SHIFT;

        let cases: [(KeyCode, KeyModifiers, bool, &[u8]); 12] = [
            (KeyCode::Char('S'), ctrl, false, b"\x1b[115;6u"),
            (KeyCode::Char('s'), ctrl, false, b"\x1b[115;5u"),
            (KeyCode::Esc, none, false, b"\x1b[27u"),
            (KeyCode::Enter, ctrl, false, b"\x1b[13;5u"),
            (KeyCode::Enter, shift, false, b"\x1b[13;2u"),
            (KeyCode::Backspace, shift, false, b"\x1b[127;2u"),
            (KeyCode::BackTab, none, false, b"\x1b[9;2u"),
            (KeyCode::Char(' '), ctrl, false, b"\x1b[32;5u"),
            (KeyCode::Char('a'), none, false, b"a"),
            (KeyCode::Char('A'), none, false, b"A"),
            (KeyCode::Left, ctrl, false, b"\x1b[1;5D"),
            (KeyCode::Up, none, true, b"\x1bOA"),
        ];
        for (code, mods, app_cursor, bytes) in cases {
            assert_eq!(
                under(TermMode::DISAMBIGUATE_ESC_CODES, code, mods, app_cursor),
                Some(bytes.to_vec()),
                "{code:?} with {mods:?}, application cursor mode {app_cursor}",
            );
        }
    }

    #[test]
    fn csi_u_reports_the_shifted_letter_after_a_colon() {
        let flags = TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_ALTERNATE_KEYS;

        assert_eq!(
            (
                under(flags, KeyCode::Char('S'), KeyModifiers::CONTROL, false),
                under(flags, KeyCode::Char(':'), KeyModifiers::CONTROL, false),
            ),
            (
                Some(b"\x1b[115:83;6u".to_vec()),
                Some(b"\x1b[58;5u".to_vec())
            ),
            "a letter names its shifted form, and a symbol has no base key to name",
        );
    }

    #[test]
    fn csi_u_encodes_every_key_under_report_all() {
        let flags = TermMode::REPORT_ALL_KEYS_AS_ESC;
        let none = KeyModifiers::NONE;

        assert_eq!(
            (
                under(flags, KeyCode::Char('a'), none, false),
                under(flags, KeyCode::Enter, none, false),
                under(flags, KeyCode::Up, none, true),
            ),
            (
                Some(b"\x1b[97u".to_vec()),
                Some(b"\x1b[13u".to_vec()),
                Some(b"\x1bOA".to_vec())
            ),
            "text and Enter take the CSI form, and application cursor mode keeps SS3",
        );
    }

    #[test]
    fn csi_u_appends_associated_text() {
        let flags = TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_ASSOCIATED_TEXT;

        assert_eq!(
            (
                under(flags, KeyCode::Char('a'), KeyModifiers::ALT, false),
                under(flags, KeyCode::Char('a'), KeyModifiers::CONTROL, false),
            ),
            (
                Some(b"\x1b[97;3;97u".to_vec()),
                Some(b"\x1b[97;5u".to_vec())
            ),
            "Alt-a carries its text, and Ctrl-a, which types a control byte, carries none",
        );
    }

    #[test]
    fn alternate_keys_alone_keep_the_xterm_bytes() {
        assert_eq!(
            under(
                TermMode::REPORT_ALTERNATE_KEYS,
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
                false
            ),
            Some(vec![0x03]),
        );
    }

    /// The bytes `code` with `mods` sends to a child that pushed `keyboard`.
    fn under(
        keyboard: TermMode,
        code: KeyCode,
        mods: KeyModifiers,
        app_cursor: bool,
    ) -> Option<Vec<u8>> {
        encode(&KeyEvent::new(code, mods), app_cursor, keyboard)
    }
}

//! Byte encoding for keyboard, mouse, and paste input.
//!
//! What the window handler in [`crate::app`] hands to the PTY, split out
//! because none of it reads or writes editor state. Every function here is a
//! pure map from an event to the bytes a terminal expects, which is what makes
//! it testable without a window, a GPU, or a running child.

use alacritty_terminal::term::TermMode;
use stoatty_protocol::window_ipc::MouseButton as IpcMouseButton;
use winit::{
    event::{MouseButton, MouseScrollDelta},
    keyboard::{Key, KeyLocation, ModifiersState, NamedKey},
};

/// Smallest font size the live zoom allows, so cells never collapse to an
/// unreadable size.
const FONT_SIZE_FLOOR: u32 = 6;

/// Largest font size the live zoom allows.
///
/// A `font_step` carries a delta the writer chooses, so without a ceiling one
/// frame can drive the size arbitrarily high and hand the renderer a cell
/// larger than any surface. Far past the size anyone reads at, so a person
/// stepping the zoom never reaches it.
const FONT_SIZE_CEIL: u32 = 256;

/// The shape of the xterm sequence a named navigation, editing, or function
/// key sends, which decides where its modifier parameter goes.
///
/// Each variant holds the byte or number that tells apart the keys of its
/// shape. A modified key always takes a CSI form, because only CSI carries a
/// parameter.
enum XtermKey {
    /// The arrows, Home, and End send `ESC [ X`, or `ESC O X` in application
    /// cursor mode, or `ESC [ 1 ; m X` when modified.
    Csi(u8),

    /// F1 to F4 send `ESC O X`, or `ESC [ 1 ; m X` when modified.
    Ss3(u8),

    /// Insert, Delete, PageUp, PageDown, and F5 to F12 send `ESC [ n ~`, or
    /// `ESC [ n ; m ~` when modified.
    Tilde(u8),
}

impl XtermKey {
    /// The shape `key` sends, or `None` for a named key with no sequence of
    /// these shapes.
    fn of(key: NamedKey) -> Option<Self> {
        let shape = match key {
            NamedKey::ArrowUp => Self::Csi(b'A'),
            NamedKey::ArrowDown => Self::Csi(b'B'),
            NamedKey::ArrowRight => Self::Csi(b'C'),
            NamedKey::ArrowLeft => Self::Csi(b'D'),
            NamedKey::Home => Self::Csi(b'H'),
            NamedKey::End => Self::Csi(b'F'),
            NamedKey::F1 => Self::Ss3(b'P'),
            NamedKey::F2 => Self::Ss3(b'Q'),
            NamedKey::F3 => Self::Ss3(b'R'),
            NamedKey::F4 => Self::Ss3(b'S'),
            NamedKey::Insert => Self::Tilde(2),
            NamedKey::Delete => Self::Tilde(3),
            NamedKey::PageUp => Self::Tilde(5),
            NamedKey::PageDown => Self::Tilde(6),
            NamedKey::F5 => Self::Tilde(15),
            NamedKey::F6 => Self::Tilde(17),
            NamedKey::F7 => Self::Tilde(18),
            NamedKey::F8 => Self::Tilde(19),
            NamedKey::F9 => Self::Tilde(20),
            NamedKey::F10 => Self::Tilde(21),
            NamedKey::F11 => Self::Tilde(23),
            NamedKey::F12 => Self::Tilde(24),
            _ => return None,
        };
        Some(shape)
    }

    /// The bytes the key sends under the xterm modifier parameter `param`.
    ///
    /// A `param` of 1 names no modifier and keeps the plain form. With
    /// `app_cursor` set, the plain form of a cursor key is SS3, as application
    /// cursor mode (DECCKM) asks.
    fn bytes(self, param: u8, app_cursor: bool) -> Vec<u8> {
        match self {
            Self::Csi(final_byte) if param == 1 => {
                vec![0x1b, if app_cursor { b'O' } else { b'[' }, final_byte]
            },
            Self::Ss3(final_byte) if param == 1 => vec![0x1b, b'O', final_byte],
            Self::Csi(final_byte) | Self::Ss3(final_byte) => {
                format!("\x1b[1;{param}{}", char::from(final_byte)).into_bytes()
            },
            Self::Tilde(number) if param == 1 => format!("\x1b[{number}~").into_bytes(),
            Self::Tilde(number) => format!("\x1b[{number};{param}~").into_bytes(),
        }
    }
}

/// One key press as the encoder reads it.
///
/// Built from a winit key event at the call site and kept apart from it,
/// because winit builds no event by hand and the tests need presses of their
/// own.
pub(crate) struct KeyPress<'a> {
    /// The key the layout produced with the held modifiers applied.
    pub(crate) logical: &'a Key,
    /// The key with every modifier released, which names the base key of a
    /// shifted symbol, such as `;` under `:`.
    pub(crate) unmodified: &'a Key,
    /// The text the press types with every modifier applied, so Ctrl-a types
    /// `\x01`. `None` when the press types nothing.
    pub(crate) text: Option<&'a str>,
    /// Where the key sits, which tells a left modifier from a right one and a
    /// keypad key from its twin in the main block.
    pub(crate) location: KeyLocation,
}

/// The font-size step a key press maps to, or `None` when it is not the
/// platform zoom combo.
///
/// `platform_mod_held` is whether the platform zoom modifier (Cmd on macOS,
/// Ctrl elsewhere) is held; the caller resolves which physical modifier that
/// is. With it held, `=` steps up by one and `-` steps down by one.
pub(crate) fn font_step(platform_mod_held: bool, key: &Key) -> Option<i32> {
    if !platform_mod_held {
        return None;
    }

    match key {
        Key::Character(s) if s.as_str() == "=" => Some(1),
        Key::Character(s) if s.as_str() == "-" => Some(-1),
        _ => None,
    }
}

/// The CSI-u bytes a zoom press of `delta` reaches a claiming program as, when
/// the claim asked for delivery down the PTY.
///
/// The key is the one the user pressed: `=` (codepoint 61) to grow, `-` (45) to
/// shrink. The modifier field is 9, which is super alone in the `1 + bitmask`
/// encoding CSI-u uses.
///
/// Super whatever the user actually held. The physical zoom modifier is Cmd on
/// macOS and Ctrl elsewhere, and which of those it was says nothing about what
/// the press meant, so normalizing here is what lets the program on the other
/// end match one combo rather than one per platform it might be running under.
pub(crate) fn zoom_csi_u(delta: i32) -> &'static [u8] {
    match delta > 0 {
        true => b"\x1b[61;9u",
        false => b"\x1b[45;9u",
    }
}

/// The CSI-u bytes a digit chord on `ch` reaches a claiming program as, when
/// the claim asked for delivery down the PTY.
///
/// The modifier field is 9, which is super alone in the `1 + bitmask` encoding
/// CSI-u uses, matching [`zoom_csi_u`].
///
/// Super whatever the user actually held. The physical chord modifier is Cmd on
/// macOS and Ctrl elsewhere, so normalizing here lets the program on the other
/// end match one combo rather than one per platform.
pub(crate) fn chord_csi_u(ch: char) -> Vec<u8> {
    format!("\x1b[{};9u", u32::from(ch)).into_bytes()
}

/// The digit a platform-modifier chord names, or `None` when the press is not
/// one.
///
/// `platform_mod_held` is the same modifier [`font_step`] reads. A digit chord
/// has no terminal byte encoding, so a caller forwards it upstream instead of
/// writing it to the pty, and the program decides what the digit means.
pub(crate) fn chord_char(platform_mod_held: bool, key: &Key) -> Option<char> {
    if !platform_mod_held {
        return None;
    }

    let Key::Character(s) = key else {
        return None;
    };
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() || !c.is_ascii_digit() {
        return None;
    }
    Some(c)
}

/// The size `delta` steps `current` to, held inside the live-zoom range.
///
/// The delta arrives from the wire, so the add saturates rather than
/// overflowing, which would panic in debug and wrap a huge size into range in
/// release. A `current` already outside the range is pulled back into it, so a
/// size set from elsewhere cannot survive a step.
pub(crate) fn stepped_font_size(current: u32, delta: i32) -> u32 {
    current
        .saturating_add_signed(delta)
        .clamp(FONT_SIZE_FLOOR, FONT_SIZE_CEIL)
}

/// The size a step of `delta` moves `current` to, or `None` when the step
/// changes nothing.
///
/// Applying a step resets the metrics of every pass and reflows the terminal,
/// and a zoom key held at the range's clamp repeats its step on every
/// autorepeat. A step that lands on `current` therefore asks for no work.
pub(crate) fn font_step_target(current: u32, delta: i32) -> Option<u32> {
    let target = stepped_font_size(current, delta);
    (target != current).then_some(target)
}

/// Whether an unhandled key press is a macOS Cmd-combo that should be swallowed
/// rather than forwarded to the child.
///
/// True only on macOS while the super (Command) modifier is held. Terminal.app
/// and iTerm2 eat a Cmd-combo the terminal itself does not act on rather than
/// leak its bare character to the child, so a Cmd-C over an empty selection does
/// not reach the child editor as a `c`. Ctrl-based combos are never swallowed,
/// so a bare Ctrl-C still delivers SIGINT and the Linux ctrl+shift clipboard
/// chord is untouched.
pub(crate) fn swallow_super_combo(modifiers: ModifiersState) -> bool {
    cfg!(target_os = "macos") && modifiers.super_key()
}

/// Whether `key` is a bare modifier, which reaches the child only under the
/// report-all flag of the kitty keyboard protocol.
///
/// The caller writes such a press without the selection clear that typing
/// does. A modifier held on the way to the copy combo then keeps the selection
/// that the combo copies.
pub(crate) fn is_modifier_key(key: &Key) -> bool {
    matches!(
        key,
        Key::Named(
            NamedKey::Shift
                | NamedKey::Control
                | NamedKey::Alt
                | NamedKey::Super
                | NamedKey::Hyper
                | NamedKey::Meta
        )
    )
}

/// Encode a key press into the bytes a terminal sends to the shell, or `None`
/// for a key with no terminal encoding, such as a bare modifier, so the caller
/// writes nothing.
///
/// With Ctrl held, an ASCII letter or one of `@ [ \ ] ^ _` becomes its C0
/// control byte, so Ctrl-C is `0x03` and `Ctrl-[` is `ESC`. Ctrl-Space sends
/// NUL, the same byte as `Ctrl-@`. Printable keys pass through as their own
/// UTF-8 bytes.
///
/// With Alt held, Enter, Backspace, Tab, Space, and a character key send their
/// bytes behind an `ESC`, which the child terminal parser decodes as Alt on
/// the key. Escape takes no prefix, because `ESC ESC` decodes as a bare
/// Escape. On macOS a character key without Ctrl takes no prefix either.
/// There Option already turned the press into the character the layout types
/// (Option-d is U+2202 on a US layout), and a prefix breaks typing on every
/// layout that needs Option.
///
/// The named editing, navigation, and function keys send their xterm
/// sequences, which is what the child terminal parser decodes back into the
/// matching key. A held Shift, Alt, or Ctrl goes in the xterm modifier
/// parameter `1 + shift + 2 * alt + 4 * ctrl`. Ctrl-Left is then
/// `ESC [ 1 ; 5 D`, and Alt-Delete is `ESC [ 3 ; 3 ~`. A modified F1 to F4
/// takes that CSI form in place of its SS3 form, because SS3 carries no
/// parameter. Super has no bit.
///
/// `app_cursor` is whether the child turned on application cursor mode
/// (DECCKM). In that mode an unmodified arrow, Home, or End sends `ESC O X` in
/// place of `ESC [ X`. A program turns the mode on when it reads keys by their
/// terminfo strings, and the `xterm-256color` entry lists these SS3 forms. A
/// modified cursor key keeps its CSI form.
///
/// `keyboard` holds the kitty keyboard protocol flags the child pushed, as
/// [`Terminal::keyboard_modes`](stoatty_term::term::Terminal::keyboard_modes)
/// reports them. With none pushed, every key keeps the bytes above. So it does
/// with the alternate-keys or associated-text flag alone, because those only
/// reshape a CSI form that another flag asked for.
///
/// The disambiguate flag sends Escape, a keypad key, and a key with a modifier
/// other than Shift alone as `CSI code ; mods u`, so Ctrl-Shift-s reaches the
/// child apart from Ctrl-s. Typed text, a Shift-only character, and an
/// unmodified Enter, Tab, or Backspace keep their bytes. The report-all flag
/// sends every key in the CSI form, a bare modifier included. The
/// alternate-keys flag adds the shifted codepoint after a colon, and the
/// associated-text flag adds the typed text. Super takes the modifier bit 8 in
/// the CSI form. An unmodified cursor key in application cursor mode keeps its
/// SS3 form under every flag.
pub(crate) fn encode_key(
    press: &KeyPress<'_>,
    mods: ModifiersState,
    app_cursor: bool,
    keyboard: TermMode,
) -> Option<Vec<u8>> {
    encode_key_with(press, mods, cfg!(target_os = "macos"), app_cursor, keyboard)
}

/// [`encode_key`] with the macOS Option rule as a parameter, so a test run on
/// one platform covers the rules of both.
///
/// `option_composes` is whether Option turns a character key without Ctrl into
/// the composed character, as it does on macOS.
fn encode_key_with(
    press: &KeyPress<'_>,
    mods: ModifiersState,
    option_composes: bool,
    app_cursor: bool,
    keyboard: TermMode,
) -> Option<Vec<u8>> {
    let ctrl = mods.control_key();
    let shift = mods.shift_key();
    let alt = mods.alt_key();

    // An Alt that Option composed into the character is part of the character,
    // so neither form reports it as a modifier.
    let composed = option_composes && !ctrl && matches!(press.logical, Key::Character(_));
    let csi_mods = match composed {
        true => mods.difference(ModifiersState::ALT),
        false => mods,
    };
    if takes_csi_u(press, csi_mods, app_cursor, keyboard) {
        return csi_u_sequence(press, csi_mods, keyboard);
    }

    match press.logical {
        Key::Named(NamedKey::Enter) => Some(alt_prefixed(alt, b"\r")),
        Key::Named(NamedKey::Backspace) => Some(alt_prefixed(alt, &[0x7f])),
        Key::Named(NamedKey::Tab) if shift => Some(b"\x1b[Z".to_vec()),
        Key::Named(NamedKey::Tab) => Some(alt_prefixed(alt, b"\t")),
        Key::Named(NamedKey::Space) => Some(alt_prefixed(alt, if ctrl { &[0x00] } else { b" " })),
        Key::Named(NamedKey::Escape) => Some(vec![0x1b]),
        Key::Named(named) => {
            let param = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
            XtermKey::of(*named).map(|xterm_key| xterm_key.bytes(param, app_cursor))
        },
        // winit composes Option into the logical key only while Ctrl is up.
        // So a character key with Ctrl keeps its Alt on macOS too.
        Key::Character(s) if ctrl => ctrl_byte(s)
            .map(|byte| alt_prefixed(alt, &[byte]))
            .or_else(|| csi_u_ctrl(s, alt)),
        Key::Character(s) => Some(alt_prefixed(alt && !option_composes, s.as_str().as_bytes())),
        _ => None,
    }
}

/// `bytes` with an `ESC` ahead of them when `alt` is held, which a terminal
/// parser decodes as Alt on the key that follows.
fn alt_prefixed(alt: bool, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    if alt {
        out.push(0x1b);
    }
    out.extend_from_slice(bytes);
    out
}

/// The C0 control byte for Ctrl held with a single character, or `None` when
/// `s` is not one character that has such a byte.
///
/// The ASCII letters have one, and so do `@ [ \ ] ^ _`, which are next to the
/// uppercase letters in ASCII. The byte is the low five bits of the uppercase
/// character, so Ctrl-C is `0x03`, `Ctrl-@` is NUL, and `Ctrl-[` is `ESC`.
fn ctrl_byte(s: &str) -> Option<u8> {
    let mut chars = s.chars();
    let c = chars.next()?;
    let has_byte = c.is_ascii_alphabetic() || matches!(c, '@' | '[' | '\\' | ']' | '^' | '_');
    if chars.next().is_some() || !has_byte {
        return None;
    }

    Some((c.to_ascii_uppercase() as u8) & 0x1f)
}

/// The CSI-u bytes for Ctrl held with a single character that has no C0 byte,
/// or `None` when `s` is not one such character.
///
/// A ctrl combo that [`ctrl_byte`] gives no byte has no terminal encoding of
/// its own, so it reaches the child as nothing without this. `ESC [ cp ; 5 u`
/// names the character by codepoint instead, which is what lets a chord such
/// as `Ctrl-?` be bound at all.
///
/// The digits and `/` take this form too, though X11 gives most of them a
/// control byte. Those bytes already belong to other keys. In X11, Ctrl-2
/// sends NUL like Ctrl-Space, and `Ctrl-/` sends `0x1f`, which a parser
/// decodes as Ctrl-7.
///
/// The modifier field is 5, which is ctrl alone in the `1 + bitmask` encoding
/// CSI-u uses, or 7 when `alt` is held too. Alt goes in the field because an
/// `ESC` ahead of the sequence decodes as a bare Escape and leaves the rest as
/// typed text. Shift is dropped because the logical character already embodies
/// it: the press that produced `?` reaches the program as `?` with ctrl, not as
/// `/` with ctrl and shift.
///
/// This serves the legacy encoding. A child that pushed the kitty keyboard
/// protocol gets [`csi_u_sequence`] instead.
fn csi_u_ctrl(s: &str, alt: bool) -> Option<Vec<u8>> {
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() || ctrl_byte(s).is_some() {
        return None;
    }

    let field = if alt { 7 } else { 5 };
    Some(format!("\x1b[{};{field}u", c as u32).into_bytes())
}

/// Whether `press` takes the kitty keyboard protocol's CSI form under the
/// flags in `keyboard`, in place of its legacy bytes.
///
/// The disambiguate and report-all flags ask for the form. The alternate-keys
/// and associated-text flags only reshape it, so alone they keep the legacy
/// bytes. An unmodified cursor key in application cursor mode keeps its SS3
/// form under every flag, since a program turns that mode on to read it.
fn takes_csi_u(
    press: &KeyPress<'_>,
    mods: ModifiersState,
    app_cursor: bool,
    keyboard: TermMode,
) -> bool {
    let report_all = keyboard.contains(TermMode::REPORT_ALL_KEYS_AS_ESC);
    if !report_all && !keyboard.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
        return false;
    }
    let cursor_key = matches!(
        press.logical,
        Key::Named(named) if matches!(XtermKey::of(*named), Some(XtermKey::Csi(_)))
    );
    if app_cursor && mods.is_empty() && cursor_key {
        return false;
    }
    if report_all {
        return true;
    }

    // Shift alone shapes the text a key types. On Tab, Enter, and Backspace it
    // shapes no text, so only the CSI form reports it.
    let shift_counts = matches!(
        press.logical,
        Key::Named(NamedKey::Tab | NamedKey::Enter | NamedKey::Backspace)
    );
    let modified = !mods.is_empty() && (mods != ModifiersState::SHIFT || shift_counts);
    let escape = matches!(press.logical, Key::Named(NamedKey::Escape));
    if modified || escape || press.location == KeyLocation::Numpad {
        return true;
    }

    // A key that types no text reaches the child as a key.
    match press.logical {
        Key::Named(named) => named.to_text().is_none(),
        _ => press.text.is_none_or(str::is_empty),
    }
}

/// The kitty keyboard protocol's CSI form of `press` under the flags in
/// `keyboard`, or `None` for a key the protocol gives no code.
///
/// The form is `CSI code ; mods u`, or the legacy CSI shape `CSI 1 ; mods X`
/// or `CSI n ; mods ~` for a navigation or function key. The modifier field is
/// `1 + shift + 2 alt + 4 ctrl + 8 super`, left out when it is 1 and no text
/// follows. The alternate-keys flag adds a shifted codepoint to the code, and
/// the associated-text flag adds the typed text after the field.
fn csi_u_sequence(
    press: &KeyPress<'_>,
    mods: ModifiersState,
    keyboard: TermMode,
) -> Option<Vec<u8>> {
    let report_all = keyboard.contains(TermMode::REPORT_ALL_KEYS_AS_ESC);
    let mut modifiers = kitty_modifier_bits(mods);
    let text = press.text.filter(|text| {
        keyboard.contains(TermMode::REPORT_ASSOCIATED_TEXT)
            && !text.is_empty()
            && !is_control_text(text)
    });

    let field_follows = modifiers != 0 || text.is_some();
    let (code, terminator) = match keypad_code(press)
        .or_else(|| kitty_function_code(press))
        .or_else(|| xterm_function_code(press, field_follows))
    {
        Some(code) => code,
        None => control_or_modifier_code(press, report_all, &mut modifiers)
            .or_else(|| character_code(press, modifiers, keyboard, text))?,
    };

    let mut sequence = format!("\x1b[{code}");
    if modifiers != 0 || text.is_some() {
        sequence.push_str(&format!(";{}", modifiers + 1));
    }
    for (i, ch) in text.into_iter().flat_map(str::chars).enumerate() {
        let separator = if i == 0 { ';' } else { ':' };
        sequence.push_str(&format!("{separator}{}", u32::from(ch)));
    }
    sequence.push(terminator);
    Some(sequence.into_bytes())
}

/// The protocol's code for a key on the numeric keypad, which tells it apart
/// from its twin in the main block.
fn keypad_code(press: &KeyPress<'_>) -> Option<(String, char)> {
    if press.location != KeyLocation::Numpad {
        return None;
    }
    let code = match press.logical {
        Key::Character(s) => match s.as_str() {
            "." => 57409,
            "/" => 57410,
            "*" => 57411,
            "-" => 57412,
            "+" => 57413,
            "=" => 57415,
            digit => {
                let mut chars = digit.chars();
                let value = chars.next()?.to_digit(10)?;
                if chars.next().is_some() {
                    return None;
                }
                57399 + value
            },
        },
        Key::Named(named) => match named {
            NamedKey::Enter => 57414,
            NamedKey::ArrowLeft => 57417,
            NamedKey::ArrowRight => 57418,
            NamedKey::ArrowUp => 57419,
            NamedKey::ArrowDown => 57420,
            NamedKey::PageUp => 57421,
            NamedKey::PageDown => 57422,
            NamedKey::Home => 57423,
            NamedKey::End => 57424,
            NamedKey::Insert => 57425,
            NamedKey::Delete => 57426,
            _ => return None,
        },
        _ => return None,
    };
    Some((code.to_string(), 'u'))
}

/// The protocol's own code for a function key that it numbers apart from the
/// legacy sequence.
///
/// Those keys are F3, whose `CSI 1 ; mods R` reads as a cursor position
/// report, F13 and up, and the lock, print, pause, menu, and media keys.
fn kitty_function_code(press: &KeyPress<'_>) -> Option<(String, char)> {
    let Key::Named(named) = press.logical else {
        return None;
    };
    let code = match named {
        NamedKey::F3 => return Some(("13".to_owned(), '~')),
        NamedKey::F13 => 57376,
        NamedKey::F14 => 57377,
        NamedKey::F15 => 57378,
        NamedKey::F16 => 57379,
        NamedKey::F17 => 57380,
        NamedKey::F18 => 57381,
        NamedKey::F19 => 57382,
        NamedKey::F20 => 57383,
        NamedKey::F21 => 57384,
        NamedKey::F22 => 57385,
        NamedKey::F23 => 57386,
        NamedKey::F24 => 57387,
        NamedKey::F25 => 57388,
        NamedKey::F26 => 57389,
        NamedKey::F27 => 57390,
        NamedKey::F28 => 57391,
        NamedKey::F29 => 57392,
        NamedKey::F30 => 57393,
        NamedKey::F31 => 57394,
        NamedKey::F32 => 57395,
        NamedKey::F33 => 57396,
        NamedKey::F34 => 57397,
        NamedKey::F35 => 57398,
        NamedKey::ScrollLock => 57359,
        NamedKey::PrintScreen => 57361,
        NamedKey::Pause => 57362,
        NamedKey::ContextMenu => 57363,
        NamedKey::MediaPlay => 57428,
        NamedKey::MediaPause => 57429,
        NamedKey::MediaPlayPause => 57430,
        NamedKey::MediaStop => 57432,
        NamedKey::MediaFastForward => 57433,
        NamedKey::MediaRewind => 57434,
        NamedKey::MediaTrackNext => 57435,
        NamedKey::MediaTrackPrevious => 57436,
        NamedKey::MediaRecord => 57437,
        NamedKey::AudioVolumeDown => 57438,
        NamedKey::AudioVolumeUp => 57439,
        NamedKey::AudioVolumeMute => 57440,
        _ => return None,
    };
    Some((code.to_string(), 'u'))
}

/// The legacy CSI shape of a navigation key or a function key up to F12.
///
/// The arrows, Home, End, and F1 to F4 take `CSI X`, with a `1` ahead of the
/// modifier field when one follows. F1 to F4 drop their legacy SS3 form,
/// because the protocol reads every key as CSI. The rest take `CSI n ~`.
fn xterm_function_code(press: &KeyPress<'_>, field_follows: bool) -> Option<(String, char)> {
    let Key::Named(named) = press.logical else {
        return None;
    };
    match XtermKey::of(*named)? {
        XtermKey::Csi(final_byte) | XtermKey::Ss3(final_byte) => {
            let code = if field_follows { "1" } else { "" };
            Some((code.to_owned(), char::from(final_byte)))
        },
        XtermKey::Tilde(number) => Some((number.to_string(), '~')),
    }
}

/// The code of Tab, Enter, Escape, Space, or Backspace, or under the report-all
/// flag of a bare modifier or lock key.
///
/// A pressed modifier also sets its own bit in `modifiers`. The protocol counts
/// it as held from its own press, and the window system reports the new
/// modifier state only after the press.
fn control_or_modifier_code(
    press: &KeyPress<'_>,
    report_all: bool,
    modifiers: &mut u8,
) -> Option<(String, char)> {
    let Key::Named(named) = press.logical else {
        return None;
    };
    let code = match (named, press.location) {
        (NamedKey::Tab, _) => 9,
        (NamedKey::Enter, _) => 13,
        (NamedKey::Escape, _) => 27,
        (NamedKey::Space, _) => 32,
        (NamedKey::Backspace, _) => 127,
        _ if !report_all => return None,
        (NamedKey::Shift, KeyLocation::Left) => 57441,
        (NamedKey::Control, KeyLocation::Left) => 57442,
        (NamedKey::Alt, KeyLocation::Left) => 57443,
        (NamedKey::Super, KeyLocation::Left) => 57444,
        (NamedKey::Hyper, KeyLocation::Left) => 57445,
        (NamedKey::Meta, KeyLocation::Left) => 57446,
        (NamedKey::Shift, _) => 57447,
        (NamedKey::Control, _) => 57448,
        (NamedKey::Alt, _) => 57449,
        (NamedKey::Super, _) => 57450,
        (NamedKey::Hyper, _) => 57451,
        (NamedKey::Meta, _) => 57452,
        (NamedKey::CapsLock, _) => 57358,
        (NamedKey::NumLock, _) => 57360,
        _ => return None,
    };
    *modifiers |= match named {
        NamedKey::Shift => 1,
        NamedKey::Alt => 2,
        NamedKey::Control => 4,
        NamedKey::Super => 8,
        _ => 0,
    };
    Some((code.to_string(), 'u'))
}

/// The code of a character key, which is its unshifted codepoint.
///
/// When the alternate-keys flag asks for it and the two differ, the shifted
/// codepoint follows after a colon. The unshifted codepoint of a letter is its
/// lowercase form. A symbol has no case, so it takes the key with every
/// modifier released, and `:` reports `59:58`.
///
/// A key that types several characters names no codepoint. It reports code 0
/// under the report-all flag when its text follows, and nothing otherwise.
fn character_code(
    press: &KeyPress<'_>,
    modifiers: u8,
    keyboard: TermMode,
    text: Option<&str>,
) -> Option<(String, char)> {
    let Key::Character(character) = press.logical else {
        return None;
    };
    let mut chars = character.chars();
    let (Some(ch), None) = (chars.next(), chars.next()) else {
        let report_all = keyboard.contains(TermMode::REPORT_ALL_KEYS_AS_ESC);
        return (report_all && text.is_some()).then(|| ("0".to_owned(), 'u'));
    };

    let shift = modifiers & 1 != 0;
    let shifted = u32::from(ch);
    let mut code = match shift {
        true => ch.to_lowercase().next().map_or(shifted, u32::from),
        false => shifted,
    };
    if shift
        && code == shifted
        && let Key::Character(base) = press.unmodified
        && let Some(base) = base.chars().next()
    {
        code = u32::from(base);
    }

    let code = match keyboard.contains(TermMode::REPORT_ALTERNATE_KEYS) && code != shifted {
        true => format!("{code}:{shifted}"),
        false => code.to_string(),
    };
    Some((code, 'u'))
}

/// Pack the active keyboard modifiers into the bits of the kitty keyboard
/// protocol's modifier field, with shift at `1`, alt at `2`, control at `4`,
/// and super at `8`.
///
/// Distinct from [`modifier_bits`], which packs the same modifiers into this
/// project's own IPC layout. These bits go on the wire to a child process, so
/// their values belong to the protocol.
fn kitty_modifier_bits(mods: ModifiersState) -> u8 {
    u8::from(mods.shift_key())
        | u8::from(mods.alt_key()) << 1
        | u8::from(mods.control_key()) << 2
        | u8::from(mods.super_key()) << 3
}

/// Whether `text` is one control character, such as the `\x01` that Ctrl-a
/// types, which the associated-text flag leaves out.
fn is_control_text(text: &str) -> bool {
    let mut chars = text.chars();
    matches!((chars.next(), chars.next()), (Some(ch), None) if ch.is_control())
}

/// Encode clipboard `text` for the PTY on paste.
///
/// In bracketed-paste mode the payload is wrapped in the DECSET 2004 guard
/// markers, with any embedded end-guard stripped so pasted bytes never close
/// the bracket early and inject input. The strip repeats until no end guard
/// remains, because the bytes on the two sides of a removed guard sometimes
/// join into another. Otherwise newlines are normalized to carriage returns,
/// matching what the Enter key sends.
pub(crate) fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        // Each pass makes the text shorter, so the loop ends.
        let mut guarded = text.to_owned();
        while guarded.contains("\x1b[201~") {
            guarded = guarded.replace("\x1b[201~", "");
        }
        format!("\x1b[200~{guarded}\x1b[201~").into_bytes()
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// Map a winit pointer button to the protocol button, or `None` for one the
/// protocol does not name (the extra buttons past forward).
pub(crate) fn ipc_button(button: MouseButton) -> Option<IpcMouseButton> {
    match button {
        MouseButton::Left => Some(IpcMouseButton::Left),
        MouseButton::Middle => Some(IpcMouseButton::Middle),
        MouseButton::Right => Some(IpcMouseButton::Right),
        MouseButton::Back => Some(IpcMouseButton::Back),
        MouseButton::Forward => Some(IpcMouseButton::Forward),
        _ => None,
    }
}

/// Map the SGR button code of a held press (0 left, 1 middle, 2 right) to the
/// protocol button, or `None` for any other code.
pub(crate) fn ipc_button_for_code(code: u8) -> Option<IpcMouseButton> {
    match code {
        0 => Some(IpcMouseButton::Left),
        1 => Some(IpcMouseButton::Middle),
        2 => Some(IpcMouseButton::Right),
        _ => None,
    }
}

/// Pack the active keyboard modifiers into a bitmask, with shift at `0x1`,
/// control at `0x2`, alt at `0x4`, and super at `0x8`.
pub(crate) fn modifier_bits(mods: ModifiersState) -> u8 {
    u8::from(mods.shift_key())
        | u8::from(mods.control_key()) << 1
        | u8::from(mods.alt_key()) << 2
        | u8::from(mods.super_key()) << 3
}

/// Pack the active keyboard modifiers into the bits an SGR mouse report adds
/// to its button code, with shift at `4`, alt at `8`, and control at `16`.
///
/// Super has no bit, because the SGR encoding defines none. A child holding
/// only super therefore sees an unmodified report.
///
/// Distinct from [`modifier_bits`], which packs the same modifiers into this
/// project's own IPC layout. These bits go on the wire to a child process, so
/// their values belong to the terminal protocol rather than to us.
pub(crate) fn sgr_modifier_bits(mods: ModifiersState) -> u8 {
    u8::from(mods.shift_key()) << 2
        | u8::from(mods.alt_key()) << 3
        | u8::from(mods.control_key()) << 4
}

/// Resolve a wheel `delta` to whole lines of scrollback to move, positive
/// scrolling up into history.
///
/// Both delta kinds accrue in `pixels` against `cell_height` and yield whole
/// lines once a cell's worth has built up, carrying the sub-line remainder so a
/// stream of small deltas is not lost. A `LineDelta` scales its line count by
/// `cell_height` into the same accumulator, so a whole-notch mouse (`y = 1.0`)
/// still moves one line per event while a hi-res wheel's fractional line deltas
/// carry across events instead of rounding to zero.
pub(crate) fn wheel_lines(delta: MouseScrollDelta, pixels: &mut f64, cell_height: f64) -> i32 {
    match delta {
        MouseScrollDelta::LineDelta(_, y) => *pixels += f64::from(y) * cell_height,
        MouseScrollDelta::PixelDelta(position) => *pixels += position.y,
    }
    let lines = (*pixels / cell_height) as i32;
    *pixels -= f64::from(lines) * cell_height;
    lines
}

/// Resolve a wheel `delta` to fractional lines of travel, positive scrolling up
/// into history, for a child that takes travel rather than notches.
///
/// Nothing accumulates here: a program hearing fractions reads every event, so
/// the remainder [`wheel_lines`] carries between events has no reason to exist.
/// A `LineDelta` is already in lines, and a `PixelDelta` divides by the cell
/// height to reach them.
pub(crate) fn wheel_travel(delta: MouseScrollDelta, cell_height: f64) -> f64 {
    match delta {
        MouseScrollDelta::LineDelta(_, y) => f64::from(y),
        MouseScrollDelta::PixelDelta(position) => position.y / cell_height.max(1.0),
    }
}

/// Encode `lines` of wheel scroll as the application-cursor arrow keys an
/// alt-screen pager reads under alternate-scroll mode: one `ESC O A` (up) per
/// line when `lines` is positive, one `ESC O B` (down) per line when negative.
pub(crate) fn alternate_scroll_bytes(lines: i32) -> Vec<u8> {
    let arrow: &[u8] = if lines > 0 { b"\x1bOA" } else { b"\x1bOB" };
    arrow.repeat(lines.unsigned_abs() as usize)
}

/// Encode `lines` of wheel scroll as SGR mouse-wheel reports at cell
/// (`col`, `row`): one button-press report per line, button 64 (up) when
/// `lines` is positive, 65 (down) when negative, with 1-based coordinates.
///
/// `mods` are the held-modifier bits from [`sgr_modifier_bits`], summed into
/// the button code. A child decodes them back off the code, so a modified
/// notch arrives distinguishable from a plain one.
pub(crate) fn sgr_wheel_bytes(lines: i32, col: usize, row: usize, mods: u8) -> Vec<u8> {
    let direction = if lines > 0 { 64 } else { 65 };
    let button = direction + mods;
    let report = format!("\x1b[<{button};{};{}M", col + 1, row + 1);
    report.repeat(lines.unsigned_abs() as usize).into_bytes()
}

/// Encode a mouse button press or release at cell (`col`, `row`) as an SGR
/// (1006) report: `button` (0 left, 1 middle, 2 right) with 1-based
/// coordinates, ending in `M` on press and `m` on release. SGR reports the real
/// button on release, unlike legacy mouse encodings.
///
/// `mods` are the held-modifier bits from [`sgr_modifier_bits`], summed into
/// the button code, so a Ctrl-click arrives distinguishable from a plain one.
pub(crate) fn sgr_button_bytes(
    button: u8,
    pressed: bool,
    col: usize,
    row: usize,
    mods: u8,
) -> Vec<u8> {
    let terminator = if pressed { 'M' } else { 'm' };
    let code = button + mods;
    format!("\x1b[<{code};{};{}{terminator}", col + 1, row + 1).into_bytes()
}

/// Encode pointer motion at cell (`col`, `row`) as an SGR (1006) motion report.
///
/// The code is the held button (0 left, 1 middle, 2 right) plus the 32 motion
/// flag, or code 3 (no button) plus 32 for buttonless any-motion (1003)
/// tracking, with 1-based coordinates and a trailing `M`. `mods` are the
/// held-modifier bits from [`sgr_modifier_bits`], summed into the code too.
pub(crate) fn sgr_motion_bytes(button: Option<u8>, col: usize, row: usize, mods: u8) -> Vec<u8> {
    let code = button.unwrap_or(3) + 32 + mods;
    format!("\x1b[<{code};{};{}M", col + 1, row + 1).into_bytes()
}

/// The grid cell `(col, row)` under physical pixel (`x`, `y`), clamped to the
/// `rows` x `cols` grid, for a `cell_size` of `[width, height]` physical pixels.
pub(crate) fn cell_at(
    x: f64,
    y: f64,
    cell_size: [f32; 2],
    rows: usize,
    cols: usize,
) -> (usize, usize) {
    let col = ((x / f64::from(cell_size[0])) as usize).min(cols.saturating_sub(1));
    let row = ((y / f64::from(cell_size[1])) as usize).min(rows.saturating_sub(1));
    (col, row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::PhysicalPosition;

    /// The delta comes off the wire, so the extremes are reachable input rather
    /// than hypotheticals.
    #[test]
    fn font_step_saturates_and_clamps_to_the_zoom_range() {
        assert_eq!(stepped_font_size(14, 1), 15);
        assert_eq!(stepped_font_size(14, -1), 13);

        assert_eq!(
            stepped_font_size(14, i32::MAX),
            FONT_SIZE_CEIL,
            "the add saturates instead of overflowing"
        );
        assert_eq!(
            stepped_font_size(14, i32::MIN),
            FONT_SIZE_FLOOR,
            "a step far below the floor lands on it"
        );

        assert_eq!(
            stepped_font_size(FONT_SIZE_CEIL, 1),
            FONT_SIZE_CEIL,
            "the ceiling holds against another step up"
        );
        assert_eq!(
            stepped_font_size(FONT_SIZE_FLOOR, -1),
            FONT_SIZE_FLOOR,
            "the floor holds against another step down"
        );
        assert_eq!(
            stepped_font_size(u32::MAX, 0),
            FONT_SIZE_CEIL,
            "a size set from outside the zoom is pulled back into range"
        );
    }

    /// A step that changes nothing still resets every pass's metrics once
    /// applied, and a zoom key held at the clamp repeats it on every autorepeat.
    #[test]
    fn a_step_at_the_clamp_asks_for_nothing() {
        assert_eq!(
            [(FONT_SIZE_CEIL, 1), (FONT_SIZE_FLOOR, -1), (20, 0), (20, 1)]
                .map(|(size, delta)| font_step_target(size, delta)),
            [None, None, None, Some(21)],
        );
    }

    #[test]
    fn chord_char_maps_a_platform_digit_and_nothing_else() {
        assert_eq!(chord_char(true, &Key::Character("9".into())), Some('9'));
        assert_eq!(chord_char(true, &Key::Character("0".into())), Some('0'));
        assert_eq!(
            chord_char(false, &Key::Character("9".into())),
            None,
            "no platform modifier held"
        );
        assert_eq!(
            chord_char(true, &Key::Character("a".into())),
            None,
            "a letter is not a digit chord"
        );
        assert_eq!(
            chord_char(true, &Key::Named(NamedKey::Enter)),
            None,
            "a named key carries no character"
        );
    }

    /// The claiming program matches one combo whatever the user physically
    /// held, so the modifier field is the same 9 the zoom bytes carry.
    #[test]
    fn chord_csi_u_spells_the_digit_as_super_plus_that_key() {
        assert_eq!(chord_csi_u('9'), b"\x1b[57;9u");
        assert_eq!(chord_csi_u('0'), b"\x1b[48;9u");
    }

    #[test]
    fn font_step_maps_the_platform_zoom_combo() {
        assert_eq!(font_step(true, &Key::Character("=".into())), Some(1));
        assert_eq!(font_step(true, &Key::Character("-".into())), Some(-1));
        assert_eq!(
            font_step(false, &Key::Character("=".into())),
            None,
            "no platform modifier held"
        );
        assert_eq!(
            font_step(true, &Key::Character("a".into())),
            None,
            "unrelated key"
        );
        assert_eq!(
            font_step(true, &Key::Character("+".into())),
            None,
            "shifted plus no longer zooms"
        );
    }

    /// A program matches these bytes exactly, so what they are is the contract
    /// rather than an implementation detail of how they are built.
    #[test]
    fn a_zoom_press_reaches_a_program_as_super_plus_the_key() {
        assert_eq!(
            (zoom_csi_u(1), zoom_csi_u(-1)),
            (b"\x1b[61;9u".as_slice(), b"\x1b[45;9u".as_slice()),
            "`=` and `-` under modifier 9, which is super alone",
        );
    }

    #[test]
    fn wheel_travel_keeps_the_fraction_a_notch_count_loses() {
        // A whole notch is one line whatever the cell height.
        assert_eq!(
            wheel_travel(MouseScrollDelta::LineDelta(0.0, 3.0), 20.0),
            3.0,
        );
        // A tenth of a cell is a tenth of a line, where a notch count reads
        // zero and waits for the rest.
        assert!(
            (wheel_travel(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 2.0)),
                20.0,
            ) - 0.1)
                .abs()
                < 1e-9,
        );
        assert_eq!(
            wheel_travel(MouseScrollDelta::LineDelta(0.0, -0.5), 20.0),
            -0.5,
            "and travel down the document keeps its sign",
        );
    }

    #[test]
    fn wheel_lines_resolves_line_and_pixel_deltas() {
        // A whole-notch LineDelta scrolls its lines directly and, being whole,
        // leaves no sub-line remainder behind.
        let mut pixels = 0.0;
        assert_eq!(
            wheel_lines(MouseScrollDelta::LineDelta(0.0, 3.0), &mut pixels, 20.0),
            3
        );
        assert_eq!(pixels, 0.0, "a whole-line delta leaves no remainder");

        // A hi-res wheel's fractional line deltas accrue instead of rounding to
        // zero. Five 0.4-line deltas over a 20px cell yield two whole lines.
        let mut pixels = 0.0;
        let frac = |y| MouseScrollDelta::LineDelta(0.0, y);
        assert_eq!(
            [
                wheel_lines(frac(0.4), &mut pixels, 20.0),
                wheel_lines(frac(0.4), &mut pixels, 20.0),
                wheel_lines(frac(0.4), &mut pixels, 20.0),
                wheel_lines(frac(0.4), &mut pixels, 20.0),
                wheel_lines(frac(0.4), &mut pixels, 20.0),
            ],
            [0, 0, 1, 0, 1],
            "fractional line deltas carry across events"
        );

        // A PixelDelta steps whole lines once a cell's worth accrues, carrying
        // the remainder so a following small delta completes the next line.
        let mut pixels = 0.0;
        let px = |y| MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, y));
        assert_eq!(
            wheel_lines(px(50.0), &mut pixels, 20.0),
            2,
            "50px over a 20px cell is two lines"
        );
        assert_eq!(pixels, 10.0, "the sub-line remainder carries over");
        assert_eq!(
            wheel_lines(px(10.0), &mut pixels, 20.0),
            1,
            "the carried 10px completes a line"
        );
        assert_eq!(pixels, 0.0);

        assert_eq!(
            wheel_lines(px(5.0), &mut pixels, 20.0),
            0,
            "below a line scrolls nothing yet"
        );
    }

    #[test]
    fn alternate_scroll_bytes_emits_one_arrow_per_line() {
        assert_eq!(
            alternate_scroll_bytes(3),
            b"\x1bOA\x1bOA\x1bOA".to_vec(),
            "scrolling up sends one up arrow per line"
        );
        assert_eq!(
            alternate_scroll_bytes(-2),
            b"\x1bOB\x1bOB".to_vec(),
            "scrolling down sends one down arrow per line"
        );
        assert_eq!(
            alternate_scroll_bytes(0),
            b"".to_vec(),
            "no lines, no bytes"
        );
    }

    #[test]
    fn sgr_wheel_bytes_reports_one_press_per_line_at_the_cell() {
        // Two lines up: button 64, one press per line, 1-based cell (3,7)->(4,8).
        assert_eq!(
            sgr_wheel_bytes(2, 3, 7, 0),
            b"\x1b[<64;4;8M\x1b[<64;4;8M".to_vec(),
            "wheel up reports button 64 once per line"
        );
        assert_eq!(
            sgr_wheel_bytes(-1, 0, 0, 0),
            b"\x1b[<65;1;1M".to_vec(),
            "wheel down at the origin cell reports button 65"
        );
    }

    #[test]
    fn sgr_wheel_bytes_adds_the_modifier_bits_to_the_button() {
        let alt = sgr_modifier_bits(ModifiersState::ALT);
        assert_eq!(
            (
                sgr_wheel_bytes(1, 0, 0, alt),
                sgr_wheel_bytes(-1, 0, 0, alt)
            ),
            (b"\x1b[<72;1;1M".to_vec(), b"\x1b[<73;1;1M".to_vec()),
            "alt lifts the wheel buttons from 64/65 to 72/73"
        );
    }

    #[test]
    fn sgr_modifier_bits_packs_the_protocol_bits() {
        assert_eq!(
            (
                sgr_modifier_bits(ModifiersState::empty()),
                sgr_modifier_bits(ModifiersState::SHIFT),
                sgr_modifier_bits(ModifiersState::ALT),
                sgr_modifier_bits(ModifiersState::CONTROL),
                sgr_modifier_bits(ModifiersState::SUPER),
                sgr_modifier_bits(ModifiersState::ALT | ModifiersState::CONTROL),
            ),
            (0, 4, 8, 16, 0, 24),
            "shift 4, alt 8, control 16, and super has no bit to set"
        );
    }

    #[test]
    fn sgr_button_bytes_reports_press_and_release_at_the_cell() {
        // Left button (0) at 1-based cell (3,7)->(4,8): M on press, m on release.
        assert_eq!(
            sgr_button_bytes(0, true, 3, 7, 0),
            b"\x1b[<0;4;8M".to_vec(),
            "press reports the button with a trailing M"
        );
        assert_eq!(
            sgr_button_bytes(0, false, 3, 7, 0),
            b"\x1b[<0;4;8m".to_vec(),
            "release reports the same button with a trailing m"
        );
    }

    #[test]
    fn sgr_motion_bytes_encodes_button_and_motion_flag() {
        assert_eq!(
            sgr_motion_bytes(Some(0), 3, 7, 0),
            b"\x1b[<32;4;8M".to_vec(),
            "held left button (0) drags as code 0+32=32 at 1-based (4,8)"
        );
        assert_eq!(
            sgr_motion_bytes(None, 0, 0, 0),
            b"\x1b[<35;1;1M".to_vec(),
            "buttonless any-motion is the no-button code 3+32=35 at the origin"
        );
    }

    #[test]
    fn sgr_button_and_motion_bytes_add_the_modifier_bits() {
        let ctrl = sgr_modifier_bits(ModifiersState::CONTROL);
        let alt = sgr_modifier_bits(ModifiersState::ALT);
        assert_eq!(
            (
                sgr_button_bytes(0, true, 3, 7, ctrl),
                sgr_motion_bytes(Some(0), 3, 7, alt)
            ),
            (b"\x1b[<16;4;8M".to_vec(), b"\x1b[<40;4;8M".to_vec()),
            "ctrl lifts a left press from 0 to 16, and alt a left drag from 32 to 40"
        );
    }

    #[test]
    fn paste_bytes_wraps_in_bracketed_guards() {
        assert_eq!(paste_bytes("hi", true), b"\x1b[200~hi\x1b[201~".to_vec());
    }

    #[test]
    fn paste_bytes_strips_embedded_end_guard() {
        assert_eq!(
            paste_bytes("a\x1b[201~b", true),
            b"\x1b[200~ab\x1b[201~".to_vec(),
            "an embedded end-guard cannot break out of the bracket"
        );
    }

    #[test]
    fn paste_bytes_strips_a_nested_end_guard() {
        assert_eq!(
            paste_bytes("a\x1b[20\x1b[201~1~b", true),
            b"\x1b[200~ab\x1b[201~".to_vec(),
            "the guard that the first strip joins together goes too"
        );
    }

    #[test]
    fn paste_bytes_normalizes_newlines_when_unbracketed() {
        assert_eq!(paste_bytes("a\r\nb\nc", false), b"a\rb\rc".to_vec());
    }

    #[test]
    fn encode_key_maps_keys_to_terminal_bytes() {
        let named = |key| legacy(&Key::Named(key), ModifiersState::empty(), false);
        let printable = |s: &str| legacy(&Key::Character(s.into()), ModifiersState::empty(), false);

        assert_eq!(
            printable("a"),
            Some(b"a".to_vec()),
            "printable passes through"
        );
        assert_eq!(
            printable("A"),
            Some(b"A".to_vec()),
            "shifted char passes through"
        );

        assert_eq!(named(NamedKey::Enter), Some(vec![b'\r']));
        assert_eq!(named(NamedKey::Backspace), Some(vec![0x7f]));
        assert_eq!(named(NamedKey::Tab), Some(vec![b'\t']));
        assert_eq!(named(NamedKey::Space), Some(vec![b' ']));
        assert_eq!(named(NamedKey::Escape), Some(vec![0x1b]));

        assert_eq!(named(NamedKey::ArrowUp), Some(b"\x1b[A".to_vec()));
        assert_eq!(named(NamedKey::ArrowDown), Some(b"\x1b[B".to_vec()));
        assert_eq!(named(NamedKey::ArrowRight), Some(b"\x1b[C".to_vec()));
        assert_eq!(named(NamedKey::ArrowLeft), Some(b"\x1b[D".to_vec()));

        assert_eq!(named(NamedKey::Delete), Some(b"\x1b[3~".to_vec()));
        assert_eq!(named(NamedKey::Insert), Some(b"\x1b[2~".to_vec()));
        assert_eq!(named(NamedKey::Home), Some(b"\x1b[H".to_vec()));
        assert_eq!(named(NamedKey::End), Some(b"\x1b[F".to_vec()));
        assert_eq!(named(NamedKey::PageUp), Some(b"\x1b[5~".to_vec()));
        assert_eq!(named(NamedKey::PageDown), Some(b"\x1b[6~".to_vec()));

        assert_eq!(
            named(NamedKey::F1),
            Some(b"\x1bOP".to_vec()),
            "F1-F4 are SS3"
        );
        assert_eq!(
            named(NamedKey::F5),
            Some(b"\x1b[15~".to_vec()),
            "F5 up are CSI-tilde"
        );
        assert_eq!(named(NamedKey::F12), Some(b"\x1b[24~".to_vec()));

        assert_eq!(
            named(NamedKey::Shift),
            None,
            "a bare modifier writes nothing"
        );
    }

    #[test]
    fn encode_key_maps_ctrl_to_control_bytes() {
        let ctrl = |s: &str| legacy(&Key::Character(s.into()), ModifiersState::CONTROL, false);
        let space = |mods| legacy(&Key::Named(NamedKey::Space), mods, false);

        assert_eq!(ctrl("c"), Some(vec![0x03]), "Ctrl-C");
        assert_eq!(ctrl("a"), Some(vec![0x01]), "Ctrl-A");
        assert_eq!(ctrl("C"), Some(vec![0x03]), "folds case");
        assert_eq!(
            ["@", "[", "\\", "]", "^", "_"].map(ctrl),
            [0x00, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f].map(|byte| Some(vec![byte])),
            "punctuation takes the low five bits",
        );

        assert_eq!(
            space(ModifiersState::CONTROL),
            Some(vec![0x00]),
            "Ctrl-Space"
        );
        assert_eq!(
            space(ModifiersState::CONTROL | ModifiersState::ALT),
            Some(vec![0x1b, 0x00]),
            "Alt prefixes the NUL",
        );
    }

    /// A ctrl combo with no control byte has nothing else to write, so without
    /// CSI-u the child never hears the press. Naming the character by codepoint
    /// is what makes such a combo bindable.
    #[test]
    fn encode_key_maps_ctrl_punctuation_to_csi_u() {
        let ctrl = |s: &str| legacy(&Key::Character(s.into()), ModifiersState::CONTROL, false);

        assert_eq!(
            ctrl("?"),
            Some(b"\x1b[63;5u".to_vec()),
            "Ctrl-? names codepoint 63, with ctrl alone as the modifier",
        );
        assert_eq!(
            ctrl("1"),
            Some(b"\x1b[49;5u".to_vec()),
            "a digit takes the same encoding",
        );
        assert_eq!(
            ctrl("ab"),
            None,
            "a multi-character key names no single codepoint",
        );
    }

    #[test]
    fn encode_key_shift_tab_sends_csi_z() {
        assert_eq!(
            legacy(&Key::Named(NamedKey::Tab), ModifiersState::SHIFT, false),
            Some(b"\x1b[Z".to_vec()),
            "Shift-Tab sends CSI Z so stoat decodes BackTab"
        );
        assert_eq!(
            legacy(&Key::Named(NamedKey::Tab), ModifiersState::empty(), false),
            Some(vec![b'\t']),
            "plain Tab still sends a tab"
        );
    }

    #[test]
    fn encode_key_prefixes_alt_with_escape() {
        let alt = ModifiersState::ALT;
        let ctrl_alt = ModifiersState::CONTROL | alt;
        let character = |s: &str| Key::Character(s.into());

        let under_both_rules: [(Key, ModifiersState, &[u8]); 8] = [
            (Key::Named(NamedKey::Backspace), alt, b"\x1b\x7f"),
            (Key::Named(NamedKey::Enter), alt, b"\x1b\r"),
            (Key::Named(NamedKey::Tab), alt, b"\x1b\t"),
            (Key::Named(NamedKey::Space), alt, b"\x1b "),
            (
                Key::Named(NamedKey::Tab),
                ModifiersState::SHIFT | alt,
                b"\x1b[Z",
            ),
            (Key::Named(NamedKey::Escape), alt, b"\x1b"),
            (character("x"), ctrl_alt, b"\x1b\x18"),
            (character("?"), ctrl_alt, b"\x1b[63;7u"),
        ];
        for option_composes in [false, true] {
            for (key, mods, bytes) in &under_both_rules {
                assert_eq!(
                    encode_key_with(
                        &press(key),
                        *mods,
                        option_composes,
                        false,
                        TermMode::empty()
                    ),
                    Some(bytes.to_vec()),
                    "{key:?} with {mods:?}, option composes: {option_composes}"
                );
            }
        }

        let empty = TermMode::empty();
        assert_eq!(
            encode_key_with(&press(&character("d")), alt, false, false, empty),
            Some(b"\x1bd".to_vec()),
            "Alt-d where Option does not compose"
        );
        assert_eq!(
            encode_key_with(&press(&character("\u{2202}")), alt, true, false, empty),
            Some("\u{2202}".as_bytes().to_vec()),
            "the character Option composed takes no prefix"
        );
    }

    #[test]
    fn encode_key_gives_csi_keys_their_modifier_parameter() {
        let ctrl = ModifiersState::CONTROL;
        let shift = ModifiersState::SHIFT;
        let alt = ModifiersState::ALT;

        let cases: [(NamedKey, ModifiersState, &[u8]); 9] = [
            (NamedKey::ArrowLeft, ctrl, b"\x1b[1;5D"),
            (NamedKey::ArrowUp, shift, b"\x1b[1;2A"),
            (NamedKey::ArrowDown, alt, b"\x1b[1;3B"),
            (NamedKey::End, ctrl | shift, b"\x1b[1;6F"),
            (NamedKey::Delete, alt, b"\x1b[3;3~"),
            (NamedKey::Delete, ctrl, b"\x1b[3;5~"),
            (NamedKey::F5, ctrl, b"\x1b[15;5~"),
            (NamedKey::F1, alt, b"\x1b[1;3P"),
            (NamedKey::ArrowLeft, ModifiersState::empty(), b"\x1b[D"),
        ];
        for (key, mods, bytes) in cases {
            assert_eq!(
                legacy(&Key::Named(key), mods, false),
                Some(bytes.to_vec()),
                "{key:?} with {mods:?}"
            );
        }
    }

    #[test]
    fn encode_key_sends_ss3_cursor_keys_in_application_mode() {
        let plain = ModifiersState::empty();

        let cases: [(NamedKey, ModifiersState, &[u8]); 6] = [
            (NamedKey::ArrowUp, plain, b"\x1bOA"),
            (NamedKey::Home, plain, b"\x1bOH"),
            (NamedKey::End, plain, b"\x1bOF"),
            (NamedKey::ArrowLeft, ModifiersState::CONTROL, b"\x1b[1;5D"),
            (NamedKey::PageUp, plain, b"\x1b[5~"),
            (NamedKey::F1, plain, b"\x1bOP"),
        ];
        for (key, mods, bytes) in cases {
            assert_eq!(
                legacy(&Key::Named(key), mods, true),
                Some(bytes.to_vec()),
                "{key:?} with {mods:?}"
            );
        }

        assert_eq!(
            legacy(&Key::Named(NamedKey::ArrowUp), plain, false),
            Some(b"\x1b[A".to_vec()),
            "Up outside application cursor mode",
        );
    }

    #[test]
    fn csi_u_reports_ctrl_shift_letters_apart() {
        let (ctrl, shift, plain) = (
            ModifiersState::CONTROL,
            ModifiersState::SHIFT,
            ModifiersState::empty(),
        );
        let character = |s: &str| Key::Character(s.into());

        let cases: [(Key, ModifiersState, &[u8]); 11] = [
            (character("S"), ctrl | shift, b"\x1b[115;6u"),
            (Key::Named(NamedKey::Tab), shift, b"\x1b[9;2u"),
            (character("s"), ctrl, b"\x1b[115;5u"),
            (character("c"), ctrl, b"\x1b[99;5u"),
            (Key::Named(NamedKey::Escape), plain, b"\x1b[27u"),
            (Key::Named(NamedKey::Enter), ctrl, b"\x1b[13;5u"),
            (Key::Named(NamedKey::Space), ctrl, b"\x1b[32;5u"),
            (character("a"), plain, b"a"),
            (Key::Named(NamedKey::Enter), plain, b"\r"),
            (character("A"), shift, b"A"),
            (Key::Named(NamedKey::ArrowLeft), ctrl, b"\x1b[1;5D"),
        ];
        for (key, mods, bytes) in &cases {
            assert_eq!(
                under(TermMode::DISAMBIGUATE_ESC_CODES, key, *mods),
                Some(bytes.to_vec()),
                "{key:?} with {mods:?}"
            );
        }
    }

    #[test]
    fn csi_u_reports_the_shifted_key_after_a_colon() {
        let flags = TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_ALTERNATE_KEYS;
        let [colon, semicolon, upper, lower] =
            [":", ";", "S", "s"].map(|s| Key::Character(s.into()));
        let shifted = |key: &Key, base: &Key| {
            let shifted_press = KeyPress {
                unmodified: base,
                ..press(key)
            };
            encode_key(
                &shifted_press,
                ModifiersState::CONTROL | ModifiersState::SHIFT,
                false,
                flags,
            )
        };

        assert_eq!(
            (shifted(&colon, &semicolon), shifted(&upper, &lower)),
            (
                Some(b"\x1b[59:58;6u".to_vec()),
                Some(b"\x1b[115:83;6u".to_vec())
            ),
            "the base key leads and the shifted one follows the colon"
        );
    }

    #[test]
    fn csi_u_encodes_every_key_under_report_all() {
        let flags = TermMode::REPORT_ALL_KEYS_AS_ESC;
        let plain = ModifiersState::empty();
        let shift_key = Key::Named(NamedKey::Shift);
        let left_shift = KeyPress {
            location: KeyLocation::Left,
            ..press(&shift_key)
        };

        assert_eq!(
            (
                under(flags, &Key::Character("a".into()), plain),
                under(flags, &Key::Named(NamedKey::Enter), plain),
                encode_key(&left_shift, plain, false, flags),
            ),
            (
                Some(b"\x1b[97u".to_vec()),
                Some(b"\x1b[13u".to_vec()),
                Some(b"\x1b[57441;2u".to_vec()),
            ),
            "text and Enter take the CSI form, and a pressed Shift holds its own bit"
        );
    }

    #[test]
    fn csi_u_appends_associated_text() {
        let flags = TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_ASSOCIATED_TEXT;
        let a = Key::Character("a".into());
        let ctrl_a = KeyPress {
            text: Some("\x01"),
            ..press(&a)
        };

        assert_eq!(
            (
                under(flags, &a, ModifiersState::ALT),
                encode_key(&ctrl_a, ModifiersState::CONTROL, false, flags),
            ),
            (
                Some(b"\x1b[97;3;97u".to_vec()),
                Some(b"\x1b[97;5u".to_vec())
            ),
            "Alt-a carries its text, and the control character Ctrl-a types carries none"
        );
    }

    /// The alternate-keys flag alone reshapes no CSI form, application cursor
    /// mode keeps its SS3 form, and an Alt that Option composed stays part of
    /// the character.
    #[test]
    fn csi_u_keeps_the_legacy_bytes_no_flag_asks_to_change() {
        let disambiguate = TermMode::DISAMBIGUATE_ESC_CODES;
        let (ctrl, alt, plain) = (
            ModifiersState::CONTROL,
            ModifiersState::ALT,
            ModifiersState::empty(),
        );
        let up = Key::Named(NamedKey::ArrowUp);
        let in_app_cursor = |mods, flags| encode_key(&press(&up), mods, true, flags);
        let composed = Key::Character("\u{2202}".into());

        assert_eq!(
            (
                under(
                    TermMode::REPORT_ALTERNATE_KEYS,
                    &Key::Character("s".into()),
                    ctrl
                ),
                in_app_cursor(plain, disambiguate),
                in_app_cursor(plain, TermMode::REPORT_ALL_KEYS_AS_ESC),
                in_app_cursor(ctrl, disambiguate),
                encode_key_with(&press(&composed), alt, true, false, disambiguate),
            ),
            (
                Some(vec![0x13]),
                Some(b"\x1bOA".to_vec()),
                Some(b"\x1bOA".to_vec()),
                Some(b"\x1b[1;5A".to_vec()),
                Some("\u{2202}".as_bytes().to_vec()),
            ),
        );
    }

    #[test]
    fn csi_u_gives_keypad_and_function_keys_their_own_codes() {
        let flags = TermMode::DISAMBIGUATE_ESC_CODES;
        let plain = ModifiersState::empty();
        let enter = Key::Named(NamedKey::Enter);
        let keypad_enter = KeyPress {
            location: KeyLocation::Numpad,
            ..press(&enter)
        };
        let function = |key| under(flags, &Key::Named(key), plain);

        assert_eq!(
            (
                encode_key(&keypad_enter, plain, false, flags),
                function(NamedKey::F1),
                function(NamedKey::F3),
                function(NamedKey::F5),
            ),
            (
                Some(b"\x1b[57414u".to_vec()),
                Some(b"\x1b[P".to_vec()),
                Some(b"\x1b[13~".to_vec()),
                Some(b"\x1b[15~".to_vec()),
            ),
            "F3 takes 13~, since its CSI R form reads as a cursor position report"
        );
    }

    /// A press of `key` that types the key's own character, as a press with no
    /// Ctrl held does.
    fn press(key: &Key) -> KeyPress<'_> {
        let text = match key {
            Key::Character(s) => Some(s.as_str()),
            _ => None,
        };
        KeyPress {
            logical: key,
            unmodified: key,
            text,
            location: KeyLocation::Standard,
        }
    }

    /// The bytes `key` sends with `mods` to a child that pushed no keyboard
    /// flags.
    fn legacy(key: &Key, mods: ModifiersState, app_cursor: bool) -> Option<Vec<u8>> {
        encode_key(&press(key), mods, app_cursor, TermMode::empty())
    }

    /// The bytes `key` sends with `mods` under the keyboard flags `keyboard`,
    /// outside application cursor mode.
    fn under(keyboard: TermMode, key: &Key, mods: ModifiersState) -> Option<Vec<u8>> {
        encode_key(&press(key), mods, false, keyboard)
    }

    #[test]
    fn ipc_button_names_the_side_buttons() {
        use stoatty_protocol::window_ipc::MouseButton as IpcMouseButton;
        use winit::event::MouseButton;

        assert_eq!(ipc_button(MouseButton::Back), Some(IpcMouseButton::Back));
        assert_eq!(
            ipc_button(MouseButton::Forward),
            Some(IpcMouseButton::Forward)
        );
        assert_eq!(ipc_button(MouseButton::Left), Some(IpcMouseButton::Left));
        assert_eq!(ipc_button(MouseButton::Other(9)), None);
    }

    #[test]
    fn modifier_bits_packs_each_modifier() {
        assert_eq!(modifier_bits(ModifiersState::empty()), 0);
        assert_eq!(modifier_bits(ModifiersState::SHIFT), 0x1);
        assert_eq!(modifier_bits(ModifiersState::CONTROL), 0x2);
        assert_eq!(modifier_bits(ModifiersState::ALT), 0x4);
        assert_eq!(modifier_bits(ModifiersState::SUPER), 0x8);
        assert_eq!(
            modifier_bits(ModifiersState::SHIFT | ModifiersState::SUPER),
            0x9
        );
    }

    #[test]
    fn cell_at_maps_pixels_to_a_clamped_cell() {
        let cell = [10.0, 20.0];
        assert_eq!(
            cell_at(25.0, 50.0, cell, 5, 8),
            (2, 2),
            "x 25/10 is col 2, y 50/20 is row 2"
        );
        assert_eq!(
            cell_at(1000.0, 1000.0, cell, 5, 8),
            (7, 4),
            "a pointer past the grid clamps to the last cell"
        );
        assert_eq!(
            cell_at(-5.0, -5.0, cell, 5, 8),
            (0, 0),
            "a negative position saturates to the origin"
        );
    }
}

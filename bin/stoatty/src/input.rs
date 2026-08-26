//! Byte encoding for keyboard, mouse, and paste input.
//!
//! What the window handler in [`crate::app`] hands to the PTY, split out
//! because none of it reads or writes editor state. Every function here is a
//! pure map from an event to the bytes a terminal expects, which is what makes
//! it testable without a window, a GPU, or a running child.

use stoatty_protocol::window_ipc::MouseButton as IpcMouseButton;
use winit::{
    event::{MouseButton, MouseScrollDelta},
    keyboard::{Key, ModifiersState, NamedKey},
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

/// Encode a key press into the bytes a terminal sends to the shell, or `None`
/// for a key with no terminal encoding, such as a bare modifier, so the caller
/// writes nothing.
///
/// `ctrl` is whether Ctrl is held: with it, an ASCII letter becomes its C0
/// control byte (Ctrl-C is `0x03`). Printable keys pass through as their own
/// UTF-8 bytes.
///
/// The named editing, navigation, and function keys send their xterm
/// normal-mode sequences, which is what the child terminal parser decodes back
/// into the matching key. Only the plain forms are sent: a modified navigation
/// key writes its unmodified sequence, matching the cursor keys, which have no
/// application-mode form here either.
pub(crate) fn encode_key(key: &Key, ctrl: bool, shift: bool) -> Option<Vec<u8>> {
    match key {
        Key::Named(NamedKey::Enter) => Some(vec![b'\r']),
        Key::Named(NamedKey::Backspace) => Some(vec![0x7f]),
        Key::Named(NamedKey::Tab) if shift => Some(b"\x1b[Z".to_vec()),
        Key::Named(NamedKey::Tab) => Some(vec![b'\t']),
        Key::Named(NamedKey::Space) => Some(vec![b' ']),
        Key::Named(NamedKey::Escape) => Some(vec![0x1b]),
        Key::Named(NamedKey::ArrowUp) => Some(b"\x1b[A".to_vec()),
        Key::Named(NamedKey::ArrowDown) => Some(b"\x1b[B".to_vec()),
        Key::Named(NamedKey::ArrowRight) => Some(b"\x1b[C".to_vec()),
        Key::Named(NamedKey::ArrowLeft) => Some(b"\x1b[D".to_vec()),
        Key::Named(NamedKey::Delete) => Some(b"\x1b[3~".to_vec()),
        Key::Named(NamedKey::Insert) => Some(b"\x1b[2~".to_vec()),
        Key::Named(NamedKey::Home) => Some(b"\x1b[H".to_vec()),
        Key::Named(NamedKey::End) => Some(b"\x1b[F".to_vec()),
        Key::Named(NamedKey::PageUp) => Some(b"\x1b[5~".to_vec()),
        Key::Named(NamedKey::PageDown) => Some(b"\x1b[6~".to_vec()),
        Key::Named(NamedKey::F1) => Some(b"\x1bOP".to_vec()),
        Key::Named(NamedKey::F2) => Some(b"\x1bOQ".to_vec()),
        Key::Named(NamedKey::F3) => Some(b"\x1bOR".to_vec()),
        Key::Named(NamedKey::F4) => Some(b"\x1bOS".to_vec()),
        Key::Named(NamedKey::F5) => Some(b"\x1b[15~".to_vec()),
        Key::Named(NamedKey::F6) => Some(b"\x1b[17~".to_vec()),
        Key::Named(NamedKey::F7) => Some(b"\x1b[18~".to_vec()),
        Key::Named(NamedKey::F8) => Some(b"\x1b[19~".to_vec()),
        Key::Named(NamedKey::F9) => Some(b"\x1b[20~".to_vec()),
        Key::Named(NamedKey::F10) => Some(b"\x1b[21~".to_vec()),
        Key::Named(NamedKey::F11) => Some(b"\x1b[23~".to_vec()),
        Key::Named(NamedKey::F12) => Some(b"\x1b[24~".to_vec()),
        Key::Character(s) if ctrl => ctrl_byte(s).or_else(|| csi_u_ctrl(s)),
        Key::Character(s) => Some(s.as_str().as_bytes().to_vec()),
        _ => None,
    }
}

/// The C0 control byte for Ctrl held with a single ASCII letter (Ctrl-C is
/// `0x03`), or `None` when `s` is not one such letter.
fn ctrl_byte(s: &str) -> Option<Vec<u8>> {
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() || !c.is_ascii_alphabetic() {
        return None;
    }

    Some(vec![(c.to_ascii_uppercase() as u8) & 0x1f])
}

/// The CSI-u bytes for Ctrl held with a single character that has no C0 byte,
/// or `None` when `s` is not one such character.
///
/// Only the letters carry a control byte, so every other ctrl combo has no
/// terminal encoding of its own and reaches the child as nothing without this.
/// `ESC [ cp ; 5 u` names the character by codepoint instead, which is what
/// lets a chord such as `Ctrl-?` be bound at all.
///
/// The modifier field is 5, which is ctrl alone in the `1 + bitmask` encoding
/// CSI-u uses. Shift is dropped because the logical character already embodies
/// it: the press that produced `?` reaches the program as `?` with ctrl, not as
/// `/` with ctrl and shift.
fn csi_u_ctrl(s: &str) -> Option<Vec<u8>> {
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() || c.is_ascii_alphabetic() {
        return None;
    }

    Some(format!("\x1b[{};5u", c as u32).into_bytes())
}

/// Encode clipboard `text` for the PTY on paste.
///
/// In bracketed-paste mode the payload is wrapped in the DECSET 2004 guard
/// markers, with any embedded end-guard stripped so pasted bytes cannot close
/// the bracket early and inject input. Otherwise newlines are normalized to
/// carriage returns, matching what the Enter key sends.
pub(crate) fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let guarded = text.replace("\x1b[201~", "");
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
/// coordinates, terminated by `M` on press and `m` on release. SGR reports the
/// real button on release, unlike legacy mouse encodings.
pub(crate) fn sgr_button_bytes(button: u8, pressed: bool, col: usize, row: usize) -> Vec<u8> {
    let terminator = if pressed { 'M' } else { 'm' };
    format!("\x1b[<{button};{};{}{terminator}", col + 1, row + 1).into_bytes()
}

/// Encode pointer motion at cell (`col`, `row`) as an SGR (1006) motion report.
///
/// The code is the held button (0 left, 1 middle, 2 right) plus the 32 motion
/// flag, or code 3 (no button) plus 32 for buttonless any-motion (1003)
/// tracking, with 1-based coordinates and a trailing `M`.
pub(crate) fn sgr_motion_bytes(button: Option<u8>, col: usize, row: usize) -> Vec<u8> {
    let code = button.unwrap_or(3) + 32;
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
            sgr_button_bytes(0, true, 3, 7),
            b"\x1b[<0;4;8M".to_vec(),
            "press reports the button with a trailing M"
        );
        assert_eq!(
            sgr_button_bytes(0, false, 3, 7),
            b"\x1b[<0;4;8m".to_vec(),
            "release reports the same button with a trailing m"
        );
    }

    #[test]
    fn sgr_motion_bytes_encodes_button_and_motion_flag() {
        assert_eq!(
            sgr_motion_bytes(Some(0), 3, 7),
            b"\x1b[<32;4;8M".to_vec(),
            "held left button (0) drags as code 0+32=32 at 1-based (4,8)"
        );
        assert_eq!(
            sgr_motion_bytes(None, 0, 0),
            b"\x1b[<35;1;1M".to_vec(),
            "buttonless any-motion is the no-button code 3+32=35 at the origin"
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
    fn paste_bytes_normalizes_newlines_when_unbracketed() {
        assert_eq!(paste_bytes("a\r\nb\nc", false), b"a\rb\rc".to_vec());
    }

    #[test]
    fn encode_key_maps_keys_to_terminal_bytes() {
        let named = |key| encode_key(&Key::Named(key), false, false);
        let printable = |s: &str| encode_key(&Key::Character(s.into()), false, false);

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
    fn encode_key_maps_ctrl_letters_to_control_bytes() {
        let ctrl = |s: &str| encode_key(&Key::Character(s.into()), true, false);

        assert_eq!(ctrl("c"), Some(vec![0x03]), "Ctrl-C");
        assert_eq!(ctrl("a"), Some(vec![0x01]), "Ctrl-A");
        assert_eq!(ctrl("C"), Some(vec![0x03]), "folds case");
    }

    /// A ctrl combo with no control byte has nothing else to write, so without
    /// CSI-u the child never hears the press. Naming the character by codepoint
    /// is what makes such a combo bindable.
    #[test]
    fn encode_key_maps_ctrl_punctuation_to_csi_u() {
        let ctrl = |s: &str| encode_key(&Key::Character(s.into()), true, false);

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
            encode_key(&Key::Named(NamedKey::Tab), false, true),
            Some(b"\x1b[Z".to_vec()),
            "Shift-Tab sends CSI Z so stoat decodes BackTab"
        );
        assert_eq!(
            encode_key(&Key::Named(NamedKey::Tab), false, false),
            Some(vec![b'\t']),
            "plain Tab still sends a tab"
        );
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

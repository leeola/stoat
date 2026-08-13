//! The window commands open, close, and focus the aux OS windows.
//!
//! A window-bound pool composites into one of these rather than into the primary
//! grid, so a program renders to a second native window through the same wire.

use crate::frame;

/// Open an aux OS window `cols` by `rows` cells with an initial `title`.
///
/// The terminal creates a native window as a second render target for
/// window-bound pools, those whose [`PoolRegionCommand::window`] names it. The
/// primary grid is window `0` and is never opened this way.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WindowOpenCommand {
    pub window: u32,
    pub cols: u16,
    pub rows: u16,
    pub title: String,
}

/// Close the aux OS window named by [`WindowCloseCommand::window`].
///
/// The terminal destroys the native window and frees its render target, so any
/// pools still bound to it stop compositing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WindowCloseCommand {
    pub window: u32,
}

/// Raise and OS-focus the aux window named by [`WindowFocusCommand::window`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WindowFocusCommand {
    pub window: u32,
}

/// Encode a [`WindowOpenCommand`] as a full `Gstoatty;window_open` frame.
pub fn encode_window_open(command: &WindowOpenCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_window_open_into(&mut out, command);
    out
}

/// Append a `Gstoatty;window_open` frame for `command` to `out`.
pub fn encode_window_open_into(out: &mut Vec<u8>, command: &WindowOpenCommand) {
    frame::begin(out, "window_open");
    frame::push_arg(out, |w| {
        w.write_all(&command.window.to_be_bytes())?;
        w.write_all(&command.cols.to_be_bytes())?;
        w.write_all(&command.rows.to_be_bytes())
    });
    frame::push_arg(out, |w| w.write_all(command.title.as_bytes()));
    frame::end(out);
}

/// Encode a [`WindowCloseCommand`] as a full `Gstoatty;window_close` frame.
pub fn encode_window_close(command: &WindowCloseCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_window_close_into(&mut out, command);
    out
}

/// Append a `Gstoatty;window_close` frame for `command` to `out`.
pub fn encode_window_close_into(out: &mut Vec<u8>, command: &WindowCloseCommand) {
    frame::begin(out, "window_close");
    frame::push_arg(out, |w| w.write_all(&command.window.to_be_bytes()));
    frame::end(out);
}

/// Encode a [`WindowFocusCommand`] as a full `Gstoatty;window_focus` frame.
pub fn encode_window_focus(command: &WindowFocusCommand) -> Vec<u8> {
    let mut out = Vec::new();
    encode_window_focus_into(&mut out, command);
    out
}

/// Append a `Gstoatty;window_focus` frame for `command` to `out`.
pub fn encode_window_focus_into(out: &mut Vec<u8>, command: &WindowFocusCommand) {
    frame::begin(out, "window_focus");
    frame::push_arg(out, |w| w.write_all(&command.window.to_be_bytes()));
    frame::end(out);
}

pub(super) fn decode_window_open(args: &[Vec<u8>]) -> Option<WindowOpenCommand> {
    let [head, title, ..] = args else {
        return None;
    };
    let head: &[u8; 8] = head.get(..8)?.try_into().ok()?;

    Some(WindowOpenCommand {
        window: u32::from_be_bytes([head[0], head[1], head[2], head[3]]),
        cols: u16::from_be_bytes([head[4], head[5]]),
        rows: u16::from_be_bytes([head[6], head[7]]),
        title: String::from_utf8(title.clone()).ok()?,
    })
}

pub(super) fn decode_window_close(args: &[Vec<u8>]) -> Option<WindowCloseCommand> {
    let arg: &[u8; 4] = args.first()?.get(..4)?.try_into().ok()?;
    Some(WindowCloseCommand {
        window: u32::from_be_bytes(*arg),
    })
}

pub(super) fn decode_window_focus(args: &[Vec<u8>]) -> Option<WindowFocusCommand> {
    let arg: &[u8; 4] = args.first()?.get(..4)?.try_into().ok()?;
    Some(WindowFocusCommand {
        window: u32::from_be_bytes(*arg),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{decode, Command};

    #[test]
    fn window_open_round_trips() {
        let command = WindowOpenCommand {
            window: 2,
            cols: 80,
            rows: 24,
            title: "src/main.rs".to_string(),
        };

        assert_eq!(
            decode(&encode_window_open(&command)),
            Some(Command::WindowOpen(command))
        );
    }

    #[test]
    fn window_close_round_trips() {
        let command = WindowCloseCommand { window: 3 };

        assert_eq!(
            decode(&encode_window_close(&command)),
            Some(Command::WindowClose(command))
        );
    }

    #[test]
    fn window_focus_round_trips() {
        let command = WindowFocusCommand { window: 5 };

        assert_eq!(
            decode(&encode_window_focus(&command)),
            Some(Command::WindowFocus(command))
        );
    }
}

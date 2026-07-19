//! Upstream window-lifecycle events flowing from stoatty to the stoat program.
//!
//! A program's stdin belongs to crossterm mid-session and cannot carry
//! terminal-to-program frames, so window focus, resize, and close events ride a
//! separate local unix socket whose path stoatty exports as
//! `STOATTY_WINDOW_SOCKET`. Each event is one text line.
//!
//! A parser ignores any line whose leading keyword it does not recognize, so the
//! format can grow new event kinds without breaking an older peer. Window `0` is
//! the primary window.

/// A window-lifecycle event stoatty reports upstream for one OS window.
///
/// `window` is the aux-window id, matching a pool region's window binding, and
/// `0` is the primary window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WindowIpcEvent {
    /// The window gained OS focus.
    Focused { window: u32 },
    /// The window's cell grid was resized to `cols` by `rows`.
    Resized { window: u32, cols: u16, rows: u16 },
    /// The window was closed via its OS close control.
    Closed { window: u32 },
    /// A pointer event at cell `col`, `row` (window-relative) with keyboard
    /// modifiers `mods` as a bitmask. Aux windows report pointer input here
    /// rather than as PTY mouse escapes, whose coordinates are primary-grid.
    Mouse {
        window: u32,
        kind: MouseKind,
        col: u16,
        row: u16,
        mods: u8,
    },
}

/// A pointer gesture carried by [`WindowIpcEvent::Mouse`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseKind {
    Press(MouseButton),
    Release(MouseButton),
    Drag(MouseButton),
    WheelUp,
    WheelDown,
}

/// A pointer button named by a [`MouseKind`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

impl MouseKind {
    /// The verb and argument words this kind encodes to, e.g. `("press",
    /// "left")` or `("wheel", "up")`.
    fn words(self) -> (&'static str, &'static str) {
        match self {
            MouseKind::Press(button) => ("press", button.word()),
            MouseKind::Release(button) => ("release", button.word()),
            MouseKind::Drag(button) => ("drag", button.word()),
            MouseKind::WheelUp => ("wheel", "up"),
            MouseKind::WheelDown => ("wheel", "down"),
        }
    }

    fn from_words(verb: &str, arg: &str) -> Option<MouseKind> {
        Some(match verb {
            "press" => MouseKind::Press(MouseButton::from_word(arg)?),
            "release" => MouseKind::Release(MouseButton::from_word(arg)?),
            "drag" => MouseKind::Drag(MouseButton::from_word(arg)?),
            "wheel" => match arg {
                "up" => MouseKind::WheelUp,
                "down" => MouseKind::WheelDown,
                _ => return None,
            },
            _ => return None,
        })
    }
}

impl MouseButton {
    fn word(self) -> &'static str {
        match self {
            MouseButton::Left => "left",
            MouseButton::Middle => "middle",
            MouseButton::Right => "right",
        }
    }

    fn from_word(word: &str) -> Option<MouseButton> {
        match word {
            "left" => Some(MouseButton::Left),
            "middle" => Some(MouseButton::Middle),
            "right" => Some(MouseButton::Right),
            _ => None,
        }
    }
}

impl WindowIpcEvent {
    /// Render the event as a single socket line, without a trailing newline.
    pub fn encode_line(&self) -> String {
        match self {
            WindowIpcEvent::Focused { window } => format!("focused {window}"),
            WindowIpcEvent::Resized { window, cols, rows } => {
                format!("resized {window} {cols} {rows}")
            },
            WindowIpcEvent::Closed { window } => format!("closed {window}"),
            WindowIpcEvent::Mouse {
                window,
                kind,
                col,
                row,
                mods,
            } => {
                let (verb, arg) = kind.words();
                format!("mouse {window} {col} {row} {mods} {verb} {arg}")
            },
        }
    }
}

/// Parse one socket line into a [`WindowIpcEvent`].
///
/// Returns `None` for an unrecognized leading keyword (so an older peer skips a
/// newer event kind) and for a known keyword whose arguments are missing, extra,
/// or non-numeric.
pub fn parse_line(line: &str) -> Option<WindowIpcEvent> {
    let mut parts = line.split_whitespace();
    let event = match parts.next()? {
        "focused" => WindowIpcEvent::Focused {
            window: parts.next()?.parse().ok()?,
        },
        "resized" => WindowIpcEvent::Resized {
            window: parts.next()?.parse().ok()?,
            cols: parts.next()?.parse().ok()?,
            rows: parts.next()?.parse().ok()?,
        },
        "closed" => WindowIpcEvent::Closed {
            window: parts.next()?.parse().ok()?,
        },
        "mouse" => {
            let window = parts.next()?.parse().ok()?;
            let col = parts.next()?.parse().ok()?;
            let row = parts.next()?.parse().ok()?;
            let mods = parts.next()?.parse().ok()?;
            let kind = MouseKind::from_words(parts.next()?, parts.next()?)?;
            WindowIpcEvent::Mouse {
                window,
                kind,
                col,
                row,
                mods,
            }
        },
        _ => return None,
    };

    // A known keyword with trailing tokens is malformed, not a growable line.
    if parts.next().is_some() {
        return None;
    }
    Some(event)
}

#[cfg(test)]
mod tests {
    use super::{parse_line, MouseButton, MouseKind, WindowIpcEvent};

    #[test]
    fn encode_line_renders_each_event() {
        assert_eq!(
            WindowIpcEvent::Focused { window: 2 }.encode_line(),
            "focused 2"
        );
        assert_eq!(
            WindowIpcEvent::Resized {
                window: 2,
                cols: 80,
                rows: 24,
            }
            .encode_line(),
            "resized 2 80 24"
        );
        assert_eq!(
            WindowIpcEvent::Closed { window: 2 }.encode_line(),
            "closed 2"
        );
    }

    #[test]
    fn each_event_round_trips() {
        for event in [
            WindowIpcEvent::Focused { window: 0 },
            WindowIpcEvent::Resized {
                window: 3,
                cols: 120,
                rows: 40,
            },
            WindowIpcEvent::Closed { window: 7 },
        ] {
            assert_eq!(parse_line(&event.encode_line()), Some(event));
        }
    }

    #[test]
    fn unknown_keyword_yields_none() {
        assert_eq!(parse_line("hover 2 10 20"), None);
        assert_eq!(parse_line(""), None);
    }

    #[test]
    fn malformed_known_lines_yield_none() {
        assert_eq!(parse_line("focused"), None, "missing window");
        assert_eq!(parse_line("focused x"), None, "non-numeric window");
        assert_eq!(parse_line("resized 2 80"), None, "missing rows");
        assert_eq!(parse_line("closed 2 3"), None, "trailing token");
    }

    #[test]
    fn mouse_events_round_trip() {
        for event in [
            WindowIpcEvent::Mouse {
                window: 2,
                kind: MouseKind::Press(MouseButton::Left),
                col: 10,
                row: 20,
                mods: 0,
            },
            WindowIpcEvent::Mouse {
                window: 5,
                kind: MouseKind::Drag(MouseButton::Right),
                col: 3,
                row: 4,
                mods: 4,
            },
            WindowIpcEvent::Mouse {
                window: 1,
                kind: MouseKind::WheelDown,
                col: 0,
                row: 0,
                mods: 1,
            },
        ] {
            assert_eq!(parse_line(&event.encode_line()), Some(event));
        }
    }

    #[test]
    fn mouse_line_encodes_verb_and_button() {
        assert_eq!(
            WindowIpcEvent::Mouse {
                window: 2,
                kind: MouseKind::Release(MouseButton::Middle),
                col: 7,
                row: 8,
                mods: 0,
            }
            .encode_line(),
            "mouse 2 7 8 0 release middle"
        );
        assert_eq!(
            WindowIpcEvent::Mouse {
                window: 2,
                kind: MouseKind::WheelUp,
                col: 7,
                row: 8,
                mods: 0,
            }
            .encode_line(),
            "mouse 2 7 8 0 wheel up"
        );
    }

    #[test]
    fn malformed_mouse_lines_yield_none() {
        assert_eq!(parse_line("mouse 2 7 8 0 press"), None, "missing button");
        assert_eq!(
            parse_line("mouse 2 7 8 0 press up"),
            None,
            "wheel arg on press"
        );
        assert_eq!(parse_line("mouse 2 7 8 0 spin left"), None, "unknown verb");
        assert_eq!(
            parse_line("mouse 2 7 8 0 press left extra"),
            None,
            "trailing token"
        );
    }
}

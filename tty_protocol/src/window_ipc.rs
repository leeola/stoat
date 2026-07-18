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
    use super::{parse_line, WindowIpcEvent};

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
        assert_eq!(parse_line("mouse 2 10 20 press"), None);
        assert_eq!(parse_line(""), None);
    }

    #[test]
    fn malformed_known_lines_yield_none() {
        assert_eq!(parse_line("focused"), None, "missing window");
        assert_eq!(parse_line("focused x"), None, "non-numeric window");
        assert_eq!(parse_line("resized 2 80"), None, "missing rows");
        assert_eq!(parse_line("closed 2 3"), None, "trailing token");
    }
}

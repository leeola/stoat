//! A scrollback stoatty demo: print a long numbered document, then hold so the
//! history stays put for mouse-wheel scrollback.
//!
//! Pure VT -- numbered lines and `\r\n` newlines, with no `Gstoatty` frames -- so
//! it renders the same in any terminal. A full-width reversed banner every
//! viewport height marks a seam, so wheeling through the history shows the seams
//! glide past: before the terminal eases its own scrollback the view steps whole
//! cells, after it the seams move at fractional-pixel granularity and settle on a
//! cell boundary. Run as the PTY shell by the `scrollback` example.

use std::{
    fmt::Write as _,
    io::{self, Write},
    thread,
};

/// Viewport size in cells, matching the window the `scrollback` example opens.
/// The seam banners land every [`VIEWPORT_H`] lines, so they align with screen
/// boundaries while wheeling.
const COLS: usize = 80;
const VIEWPORT_H: usize = 24;

/// Lines printed into the scrollback, enough to fill many screens of history.
const LINES: usize = 1000;

fn main() {
    let mut out = String::from("\x1b[H");

    for line in 1..=LINES {
        if line % VIEWPORT_H == 0 {
            let banner = format!("{line:>4} | ===== seam at line {line} =====");
            let _ = write!(out, "\x1b[7m{banner:<COLS$}\x1b[0m");
        } else {
            let _ = write!(out, "{line:>4} | scrollback line {line}");
        }

        if line < LINES {
            out.push_str("\r\n");
        }
    }

    {
        let mut stdout = io::stdout();
        stdout.write_all(out.as_bytes()).expect("write to stdout");
        stdout.flush().expect("flush stdout");
    }

    // Hold so the shell does not exit and close the window; the window owns this
    // process's lifetime and kills it on close.
    loop {
        thread::park();
    }
}

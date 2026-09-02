//! Raw descriptor queries on the controlling terminal that crossterm does not
//! answer.
//!
//! The editor and the attach client both talk to the same tty and both need
//! answers crossterm's event model does not carry. Each query lives here once,
//! so the unsafe call behind it exists at one site rather than at every caller.

/// This terminal's text area and the grid it holds, or `None` when fd 1 is not
/// a terminal.
///
/// The pixel fields are what an image client divides by to size what it draws.
/// The cell counts are what the passthrough loop polls. Crossterm reports no
/// resize while a remote session owns fd 0, so the size has to be asked for
/// rather than waited on.
pub fn winsize() -> Option<libc::winsize> {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    // SAFETY: TIOCGWINSZ writes one winsize through the pointer, which borrows
    // a live local for the call, and reads nothing else.
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &raw mut ws) };

    (ok == 0).then_some(ws)
}

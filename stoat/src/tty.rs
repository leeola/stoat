//! Raw descriptor queries on the controlling terminal that crossterm does not
//! answer.
//!
//! The editor and the attach client both talk to the same tty and both need
//! answers crossterm's event model does not carry. Each query lives here once,
//! so the unsafe call behind it exists at one site rather than at every caller.

use std::{io, time::Duration};

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

/// Read what is on fd 0 into `buf`, waiting up to `timeout`.
///
/// Zero means the wait elapsed with nothing to read, which is also what a
/// closed stdin reports. A caller that has to tell the two apart polls for
/// `POLLHUP` itself.
pub fn read_stdin(buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
    let mut fds = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };

    // SAFETY: poll reads and writes the one pollfd through the pointer and
    // touches nothing else. The struct is initialized above.
    let ready = unsafe { libc::poll(&raw mut fds, 1, timeout.as_millis() as libc::c_int) };
    if ready < 0 {
        return Err(io::Error::last_os_error());
    }
    if ready == 0 {
        return Ok(0);
    }

    // SAFETY: read writes at most buf.len() bytes through the pointer, which
    // borrows a live slice for the call.
    let got = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
    if got < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(got as usize)
}

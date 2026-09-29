//! Raw descriptor queries on the controlling terminal that crossterm does not
//! answer.
//!
//! The editor and the attach client both talk to the same tty and both need
//! answers crossterm's event model does not carry. Each query lives here once,
//! so the unsafe call behind it exists at one site rather than at every caller.
//! The terminal's resize signal lives here too, for the same reason.

use std::{io, mem::MaybeUninit, ptr, time::Duration};

/// This terminal's text area and the grid it holds, or `None` when fd 1 is not
/// a terminal.
///
/// The pixel fields are what an image client divides by to size what it draws.
/// The cell counts are what the passthrough loop polls. Its raw read watches fd
/// 0 alone, so no resize wakes it, and the size has to be asked for rather than
/// waited on.
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

/// Block SIGWINCH on the calling thread, so a resize stays pending until
/// [`wait_resize_signal`] takes it.
///
/// The mask is per thread, and a new thread inherits the mask of the thread
/// that spawns it. So a process that takes the signal on one thread calls this
/// on its main thread before any other thread exists. Otherwise the kernel
/// hands the signal to a thread that has it unblocked, and the wait never sees
/// it.
///
/// Also replaces any SIGWINCH handler with one that does nothing.
pub fn block_resize_signal() -> io::Result<()> {
    // SIGWINCH is ignored by default. XNU discards an ignored signal when it is
    // sent, even while a thread blocks it. Linux keeps a blocked one pending. A
    // handler keeps it pending on both.
    let handler = resize_handler as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: the handler does nothing, so it is async-signal-safe.
    if unsafe { libc::signal(libc::SIGWINCH, handler) } == libc::SIG_ERR {
        return Err(io::Error::last_os_error());
    }

    let set = resize_signal_set()?;
    // SAFETY: pthread_sigmask reads the initialized set through the pointer.
    // The null pointer asks for no copy of the previous mask.
    let status = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, ptr::null_mut()) };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status));
    }

    Ok(())
}

/// Wait for a SIGWINCH, and take it.
///
/// The call takes a signal sent before it at once, and otherwise blocks with no
/// timeout. Every thread must block the signal, as [`block_resize_signal`]
/// arranges. A thread that does not block it takes it first, and this wait goes
/// on.
pub fn wait_resize_signal() -> io::Result<()> {
    let set = resize_signal_set()?;
    let mut signal = 0;

    // SAFETY: sigwait reads the initialized set, and writes one c_int through a
    // pointer that borrows a live local for the call.
    let status = unsafe { libc::sigwait(&raw const set, &raw mut signal) };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status));
    }

    Ok(())
}

/// The signal set that holds SIGWINCH alone.
fn resize_signal_set() -> io::Result<libc::sigset_t> {
    let mut set = MaybeUninit::<libc::sigset_t>::uninit();

    // SAFETY: sigemptyset initializes the whole set through the pointer, and
    // sigaddset adds one member to the set it initialized.
    let failed = unsafe {
        libc::sigemptyset(set.as_mut_ptr()) != 0
            || libc::sigaddset(set.as_mut_ptr(), libc::SIGWINCH) != 0
    };
    if failed {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: sigemptyset succeeded, so every byte of the set is initialized.
    Ok(unsafe { set.assume_init() })
}

/// The SIGWINCH handler that [`block_resize_signal`] installs.
///
/// It exists so the signal's disposition is not ignore. It never runs while
/// every thread blocks the signal, since [`wait_resize_signal`] takes it.
extern "C" fn resize_handler(_: libc::c_int) {}

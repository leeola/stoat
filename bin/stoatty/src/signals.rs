//! stoatty's termination signals, blocked on every thread and taken by one.
//!
//! SIGHUP, SIGINT, and SIGTERM end a process with no drop by default, which
//! strands the window-event socket file. The main thread blocks them before any
//! other thread exists, and one thread waits for the first. The event loop then
//! closes the window through the exit path that removes the socket file.

use std::{io, mem::MaybeUninit, ptr};

/// Block SIGHUP, SIGINT, and SIGTERM on the calling thread, so each stays
/// pending until [`wait_termination`] takes it.
///
/// The mask is per thread, and a new thread inherits the mask of the thread
/// that spawns it. So the call belongs on the main thread before any other
/// thread exists. Otherwise the kernel hands a signal to a thread that has it
/// unblocked, and its default action ends the process.
pub fn block_termination() -> io::Result<()> {
    let set = termination_set()?;
    // SAFETY: pthread_sigmask reads the initialized set through the pointer.
    // The null pointer asks for no copy of the previous mask.
    let status = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, ptr::null_mut()) };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status));
    }

    Ok(())
}

/// Wait for SIGHUP, SIGINT, or SIGTERM, take it, and return its number.
///
/// The call takes a signal sent before it at once, and otherwise blocks with no
/// timeout. Every thread must block the signals, as [`block_termination`]
/// arranges. A thread that does not block one takes it first, and its default
/// action ends the process.
pub fn wait_termination() -> io::Result<libc::c_int> {
    let set = termination_set()?;
    let mut signal = 0;

    // SAFETY: sigwait reads the initialized set, and writes one c_int through a
    // pointer that borrows a live local for the call.
    let status = unsafe { libc::sigwait(&raw const set, &raw mut signal) };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status));
    }

    Ok(signal)
}

/// The signal set that holds SIGHUP, SIGINT, and SIGTERM.
fn termination_set() -> io::Result<libc::sigset_t> {
    let mut set = MaybeUninit::<libc::sigset_t>::uninit();

    // SAFETY: sigemptyset initializes the whole set through the pointer, and
    // each sigaddset adds one member to the set it initialized.
    let failed = unsafe {
        libc::sigemptyset(set.as_mut_ptr()) != 0
            || libc::sigaddset(set.as_mut_ptr(), libc::SIGHUP) != 0
            || libc::sigaddset(set.as_mut_ptr(), libc::SIGINT) != 0
            || libc::sigaddset(set.as_mut_ptr(), libc::SIGTERM) != 0
    };
    if failed {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: sigemptyset succeeded, so every byte of the set is initialized.
    Ok(unsafe { set.assume_init() })
}

#[cfg(test)]
mod tests {
    use super::{block_termination, wait_termination};
    use std::thread;

    /// A thread-directed signal stays pending for that thread alone, and the
    /// mask ends with the thread, so the test touches no other test. The block
    /// comes first, because an unblocked termination signal ends the test
    /// binary.
    #[test]
    fn a_blocked_termination_signal_waits_for_the_wait() {
        let signals = [libc::SIGHUP, libc::SIGINT, libc::SIGTERM];

        let taken = thread::spawn(move || {
            block_termination().expect("block the signals");
            signals.map(|signal| {
                // SAFETY: pthread_kill directs a signal at this live thread,
                // which blocks it, so the signal waits for the wait below.
                let status = unsafe { libc::pthread_kill(libc::pthread_self(), signal) };
                assert_eq!(status, 0, "signal {signal} reaches the thread");
                wait_termination().expect("take the signal")
            })
        })
        .join()
        .expect("the signal thread");

        assert_eq!(taken, signals);
    }
}

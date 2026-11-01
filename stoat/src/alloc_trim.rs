//! Allocator trims that give the memory of closed previews and buffers back to
//! the kernel.
//!
//! glibc keeps the pages that an arena freed until a trim returns them. The
//! syntax trees of a picker preview and of a closed buffer come from the pool
//! threads' arenas, so without a trim the resident size climbs with each finder
//! session while the live heap stays flat.
//!
//! A release asks for a trim, and the run loop makes at most one per turn, so a
//! user action that releases many things pays for one trim.

use std::cell::Cell;

thread_local! {
    /// Whether this thread released memory since its last trim.
    ///
    /// The flag is per thread rather than per process, because the editor's own
    /// thread makes every request and runs the loop. Tests on separate threads
    /// then never see each other's requests.
    static PENDING: Cell<bool> = const { Cell::new(false) };
}

/// Ask the next run-loop turn to return freed memory to the kernel.
pub(crate) fn request() {
    PENDING.set(true);
}

/// Trim the allocator if a request came since the last call, and report
/// whether one came.
///
/// A trim walks every arena and costs milliseconds on a large heap. A call with
/// no request costs one flag read.
pub(crate) fn run_pending() -> bool {
    let pending = PENDING.replace(false);
    if pending {
        trim();
    }
    pending
}

/// Whether a request came since the last trim. The read leaves it in place.
#[cfg(test)]
pub(crate) fn pending() -> bool {
    PENDING.get()
}

fn trim() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim takes each arena's lock and returns only free pages,
    // so it is sound beside other threads and every live allocation stays valid.
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(test)]
mod tests {
    use super::{pending, request, run_pending};

    #[test]
    fn a_request_trims_once() {
        request();

        assert_eq!(
            [run_pending(), run_pending(), pending()],
            [true, false, false]
        );
    }
}

pub mod persist;

pub use persist::find_resume_anchor;
use serde::{Deserialize, Serialize};
use std::time::UNIX_EPOCH;
use stoat_scheduler::Executor;

/// Stable-across-restart workspace identifier.
///
/// Assigned once at construction time from the wall clock and serialized with
/// the workspace's persisted state, so a workspace keeps the same on-disk
/// filename across sessions. The nanosecond timestamp also gives a natural
/// creation-order sort that complements mtime-based "most recent" selection on
/// load.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceUid(pub u64);

impl WorkspaceUid {
    pub fn now(executor: &Executor) -> Self {
        let nanos = executor
            .system_now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        Self(nanos)
    }
}

impl std::fmt::Display for WorkspaceUid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

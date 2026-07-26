use crate::host::CommitInfo;
use std::path::PathBuf;

/// Where a walk puts HEAD back when it finishes.
///
/// Captured before the walk detaches, because a walk cannot tell afterwards
/// whether the user was on a branch or already detached, and reattaching
/// someone who was detached would move a branch they never asked to move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReturnRef {
    Branch(String),
    Detached(String),
}

/// A walk through a run of commits, showing each one's diff in turn.
///
/// The working tree follows the cursor. Stepping checks the current commit out
/// detached so files on disk, and the language servers reading them, match the
/// revision under review.
///
/// `commits` runs oldest-first from the chosen base up to the ref tip, so
/// stepping forward moves toward the tip the way reading history forward does.
pub(crate) struct ReviewWalk {
    pub(crate) workdir: PathBuf,
    pub(crate) commits: Vec<CommitInfo>,
    pub(crate) cursor: usize,
    pub(crate) return_ref: ReturnRef,
}

impl ReviewWalk {
    /// The commit under the cursor.
    ///
    /// A walk is never built empty, so this has something to return for the
    /// lifetime of the walk.
    pub(crate) fn current(&self) -> &CommitInfo {
        &self.commits[self.cursor]
    }

    /// Move the cursor by `delta`, clamped at both ends. Returns whether it
    /// moved, so a step off the end can be a no-op rather than wrapping around
    /// to the other end of the history.
    pub(crate) fn step(&mut self, delta: i32) -> bool {
        let max = (self.commits.len() - 1) as i32;
        let next = (self.cursor as i32 + delta).clamp(0, max) as usize;
        let moved = next != self.cursor;
        self.cursor = next;
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::{ReturnRef, ReviewWalk};
    use crate::host::CommitInfo;
    use std::path::PathBuf;

    fn walk(len: usize) -> ReviewWalk {
        ReviewWalk {
            workdir: PathBuf::from("/repo"),
            commits: (0..len)
                .map(|i| CommitInfo {
                    sha: format!("sha{i}"),
                    short_sha: format!("sha{i}"),
                    summary: format!("commit {i}"),
                    author_name: "test".into(),
                    author_email: "t@t".into(),
                    time: 0,
                    parents: vec![format!("sha{}", i + 1)],
                })
                .collect(),
            cursor: 0,
            return_ref: ReturnRef::Branch("main".into()),
        }
    }

    #[test]
    fn step_walks_forward_and_back() {
        let mut w = walk(3);
        assert_eq!(w.current().sha, "sha0");
        assert!(w.step(1));
        assert_eq!(w.current().sha, "sha1");
        assert!(w.step(-1));
        assert_eq!(w.current().sha, "sha0");
    }

    #[test]
    fn step_clamps_at_both_ends_without_wrapping() {
        let mut w = walk(2);
        assert!(!w.step(-1), "already at the oldest commit");
        assert_eq!(w.cursor, 0);

        assert!(w.step(1));
        assert!(!w.step(1), "already at the tip");
        assert_eq!(w.cursor, 1);
    }

    #[test]
    fn a_single_commit_walk_never_moves() {
        let mut w = walk(1);
        assert!(!w.step(1));
        assert!(!w.step(-1));
        assert_eq!(w.current().sha, "sha0");
    }
}

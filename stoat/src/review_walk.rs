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

    /// Move the cursor by `delta`, wrapping past either end to the other.
    /// Returns whether it moved, which a walk of one commit never does.
    pub(crate) fn step(&mut self, delta: i32) -> bool {
        let len = self.commits.len() as i32;
        let next = (self.cursor as i32 + delta).rem_euclid(len) as usize;
        let moved = next != self.cursor;
        self.cursor = next;
        moved
    }

    /// Put the cursor on commit `index`, clamped to the last commit. Returns
    /// whether it moved.
    ///
    /// A walkthrough jumps between the commits its stops name rather than
    /// stepping through them in order.
    pub(crate) fn seek(&mut self, index: usize) -> bool {
        let next = index.min(self.commits.len() - 1);
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
    fn step_wraps_past_both_ends() {
        let mut w = walk(2);
        let past_the_base = (w.step(-1), w.cursor);
        let past_the_tip = (w.step(1), w.cursor);

        assert_eq!(
            (past_the_base, past_the_tip),
            ((true, 1), (true, 0)),
            "a step past the base lands on the tip, and one past the tip on the base"
        );
    }

    #[test]
    fn seek_lands_on_the_named_commit_and_clamps() {
        let mut w = walk(3);
        assert!(w.seek(2));
        assert_eq!(w.current().sha, "sha2");
        assert!(!w.seek(2), "already there");
        assert!(w.seek(0));
        assert!(w.seek(9), "past the end clamps to the last commit");
        assert_eq!(w.current().sha, "sha2");
    }

    #[test]
    fn a_single_commit_walk_never_moves() {
        let mut w = walk(1);
        assert!(!w.step(1));
        assert!(!w.step(-1));
        assert_eq!(w.current().sha, "sha0");
    }
}

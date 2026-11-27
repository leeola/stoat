use crate::host::CommitInfo;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Where a walk puts HEAD back when it finishes.
///
/// Captured before the walk detaches, because a walk cannot tell afterwards
/// whether the user was on a branch or already detached, and reattaching
/// someone who was detached would move a branch they never asked to move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReturnRef {
    Branch(String),
    Detached(String),
}

/// What a walk diffs its commits against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkSpan {
    /// Each commit against its parent, so a step shows that commit alone.
    Commits,
    /// One commit, the tip of `from`, against `base`, the merge base of `from`
    /// and `against`.
    ///
    /// The diff holds all of `from`'s work since the fork and none of what
    /// landed on `against` after it. Both names stay as the reader typed them,
    /// which is how the badge and the diff status row name the review.
    Range {
        base: String,
        from: String,
        against: String,
    },
}

/// Where a walk started, which decides what each of its landings shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WalkOrigin {
    /// The commit list, a review, or a walkthrough. A landing shows the
    /// commit's first changed file, or the file the start names, in the
    /// per-file diff.
    Commits,
    /// A blamed line. A landing shows the line's file at the line, under the
    /// changes view.
    ///
    /// `path` is the repo-relative path and `row` the 0-based line that the
    /// blamed line has in the walked commit.
    Blame { path: PathBuf, row: u32 },
}

/// The stash that a blame checkout made of the working tree's changes, which
/// the walk's return pops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalkStash {
    /// The stash commit's sha.
    pub(crate) sha: String,
    /// How many files the stash took, untracked files among them.
    pub(crate) changes: usize,
}

/// A walk through a run of commits, showing each one's diff in turn.
///
/// The working tree follows the cursor. Stepping checks the current commit out
/// detached so files on disk, and the language servers reading them, match the
/// revision under review.
///
/// `commits` runs oldest-first from the chosen base up to the ref tip, so
/// stepping forward moves toward the tip the way reading history forward does.
/// A range review is a walk of one commit, whose [`WalkSpan::Range`] names the
/// base it reads against.
pub struct ReviewWalk {
    pub workdir: PathBuf,
    pub commits: Vec<CommitInfo>,
    pub cursor: usize,
    pub return_ref: ReturnRef,
    pub span: WalkSpan,
    pub origin: WalkOrigin,
    /// The stash the walk's first landing made, which the return pops, or
    /// `None` for a walk that stashed nothing.
    pub stash: Option<WalkStash>,
}

impl ReviewWalk {
    /// A blame checkout's walk at the one commit `short_sha`, back to
    /// `return_ref`, for a test that reads a walk without running a checkout.
    #[cfg(any(test, feature = "test-support"))]
    pub fn blame_checkout(
        short_sha: &str,
        return_ref: ReturnRef,
        stash: Option<WalkStash>,
    ) -> ReviewWalk {
        ReviewWalk {
            workdir: PathBuf::from("/repo"),
            commits: vec![CommitInfo {
                sha: short_sha.to_owned(),
                short_sha: short_sha.to_owned(),
                summary: String::new(),
                author_name: String::new(),
                author_email: String::new(),
                time: 0,
                parents: Vec::new(),
            }],
            cursor: 0,
            return_ref,
            span: WalkSpan::Commits,
            origin: WalkOrigin::Blame {
                path: PathBuf::from("src/lib.rs"),
                row: 0,
            },
            stash,
        }
    }

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

/// The on-disk shape of a blame checkout's walk, which a restart brings back.
///
/// A restart that loses the walk leaves HEAD detached at the commit, with no
/// way to return and a stash that nothing names. Only a blame checkout saves
/// its walk. A walk of the commit list keeps its lists and its cursor in state
/// the session does not hold, and a blame checkout's walk is one commit.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct BlameCheckoutSnap {
    pub workdir: PathBuf,
    pub commit: CommitInfo,
    pub return_ref: ReturnRef,
    pub origin: WalkOrigin,
    pub stash: Option<WalkStash>,
}

impl BlameCheckoutSnap {
    /// The snapshot of `walk`, or `None` for a walk that a blame checkout did
    /// not open.
    pub(crate) fn of(walk: &ReviewWalk) -> Option<BlameCheckoutSnap> {
        matches!(walk.origin, WalkOrigin::Blame { .. }).then(|| BlameCheckoutSnap {
            workdir: walk.workdir.clone(),
            commit: walk.current().clone(),
            return_ref: walk.return_ref.clone(),
            origin: walk.origin.clone(),
            stash: walk.stash.clone(),
        })
    }

    /// The walk this snapshot saved, at its one commit.
    pub(crate) fn into_walk(self) -> ReviewWalk {
        ReviewWalk {
            workdir: self.workdir,
            commits: vec![self.commit],
            cursor: 0,
            return_ref: self.return_ref,
            span: WalkSpan::Commits,
            origin: self.origin,
            stash: self.stash,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ReturnRef, ReviewWalk, WalkOrigin, WalkSpan};
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
            span: WalkSpan::Commits,
            origin: WalkOrigin::Commits,
            stash: None,
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

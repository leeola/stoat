use crate::host::{CommitInfo, ConflictedFile, RebaseTodo, RebaseTodoOp};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
};

/// Editable rebase plan owned by a workspace
/// while the user is in `"rebase"` mode. Seeded from the commit list
/// when the user presses `i` to enter the mode, mutated by todo-list
/// edits (op changes, reorders), and consumed by `ExecuteRebase`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RebaseState {
    pub workdir: PathBuf,
    pub todo: Vec<RebaseEntry>,
    pub selected: usize,
    /// Sha of the commit this plan stacks onto (typically the parent
    /// of the oldest entry).
    pub onto: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RebaseEntry {
    pub op: RebaseTodoOp,
    pub commit: CommitInfo,
}

impl RebaseState {
    pub fn new(workdir: PathBuf, onto: String, entries: Vec<RebaseEntry>) -> Self {
        Self {
            workdir,
            todo: entries,
            selected: 0,
            onto,
        }
    }

    pub fn move_up(&mut self) -> bool {
        if self.selected == 0 {
            return false;
        }
        self.selected -= 1;
        true
    }

    pub fn move_down(&mut self) -> bool {
        if self.todo.is_empty() || self.selected + 1 >= self.todo.len() {
            return false;
        }
        self.selected += 1;
        true
    }

    /// Reorder: swap the selected entry with the one above.
    pub fn swap_up(&mut self) -> bool {
        if self.selected == 0 || self.todo.is_empty() {
            return false;
        }
        self.todo.swap(self.selected, self.selected - 1);
        self.selected -= 1;
        true
    }

    /// Reorder: swap the selected entry with the one below.
    pub fn swap_down(&mut self) -> bool {
        if self.todo.is_empty() || self.selected + 1 >= self.todo.len() {
            return false;
        }
        self.todo.swap(self.selected, self.selected + 1);
        self.selected += 1;
        true
    }

    pub fn set_op(&mut self, op: RebaseTodoOp) -> bool {
        let Some(entry) = self.todo.get_mut(self.selected) else {
            return false;
        };
        if entry.op == op {
            return false;
        }
        entry.op = op;
        true
    }

    /// Exports the plan as the neutral [`RebaseTodo`] shape used by
    /// the `run_rebase` fast path and by the fake's bookkeeping.
    /// Unused by the interactive stepper but still the right API for
    /// external consumers.
    #[allow(dead_code)]
    pub fn to_git_todo(&self) -> Vec<RebaseTodo> {
        self.todo
            .iter()
            .map(|e| RebaseTodo {
                op: e.op,
                sha: e.commit.sha.clone(),
                message: e.commit.summary.clone(),
            })
            .collect()
    }
}

/// Actively executing rebase: owns state that survives across pauses
/// (reword input, edit-mode review, conflict resolution). Installed
/// when `ExecuteRebase` kicks off the plan and consumed when the plan
/// completes or aborts. Lives on the workspace as
/// `rebase_active`.
pub struct ActiveRebase {
    pub workdir: PathBuf,
    /// Original base the plan stacks onto; retained for diagnostics
    /// and potential recovery even though the stepper reads from
    /// `current_head` after the first entry lands.
    #[allow(dead_code)]
    pub onto: String,
    pub remaining: VecDeque<RebaseEntry>,
    /// The commit at the tip of the rebase-so-far.
    pub current_head: String,
    /// Latest Pick/Reword-produced commit. Squash/Fixup merge into it.
    pub last_pick_sha: Option<String>,
    /// Message of `last_pick_sha`, used when building squash messages.
    pub last_message: Option<String>,
    pub pause: Option<RebasePause>,
}

pub enum RebasePause {
    /// Waiting for the user to edit a commit message. UI-layer
    /// state (the GUI's modal entity) lives separately on the
    /// workspace, so the pause variant stays a pure data shape
    /// constructible from any caller. Cleaned up by
    /// `reword_confirm` / `reword_abort`.
    Reword {
        /// The sha that was just cherry-picked and committed; will be
        /// replaced with a new commit carrying the user's message when
        /// `RewordConfirm` fires.
        cherry_picked_commit: String,
        /// Original commit message, kept for the modal's reference line.
        original_message: String,
    },
    /// Waiting for the user to modify the picked commit (typically via
    /// review-mode hunk removal). The review's current source sha at
    /// `RebaseContinue` time becomes the new `current_head`.
    Edit {
        #[allow(dead_code)]
        cherry_picked_commit: String,
    },
    /// Waiting for per-file conflict resolutions.
    Conflict {
        source_sha: String,
        files: Vec<ConflictedFile>,
        selected: usize,
        resolutions: HashMap<PathBuf, ConflictResolution>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[allow(dead_code)]
pub enum ConflictResolution {
    TakeOurs,
    TakeTheirs,
    /// Skip this entry entirely (treat as Drop for rebase purposes).
    /// Reserved for a future "skip this file" variant in the resolution
    /// UI; currently the whole-entry skip path uses `ConflictSkipEntry`
    /// and bypasses this enum.
    SkipEntry,
}

impl ActiveRebase {
    pub fn new(state: RebaseState) -> Self {
        Self {
            workdir: state.workdir,
            onto: state.onto.clone(),
            remaining: state.todo.into(),
            current_head: state.onto,
            last_pick_sha: None,
            last_message: None,
            pause: None,
        }
    }
}

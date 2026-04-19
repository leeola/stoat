use crate::{
    buffer::BufferId,
    editor_state::EditorId,
    host::{CommitInfo, ConflictedFile, RebaseTodo, RebaseTodoOp},
};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
};

/// Editable rebase plan owned by a [`crate::workspace::Workspace`]
/// while the user is in `"rebase"` mode. Seeded from the commit list
/// when the user presses `i` to enter the mode, mutated by todo-list
/// edits (op changes, reorders), and consumed by `ExecuteRebase`.
pub(crate) struct RebaseState {
    pub workdir: PathBuf,
    pub todo: Vec<RebaseEntry>,
    pub selected: usize,
    /// Sha of the commit this plan stacks onto (typically the parent
    /// of the oldest entry).
    pub onto: String,
}

#[derive(Clone, Debug)]
pub(crate) struct RebaseEntry {
    pub op: RebaseTodoOp,
    pub commit: CommitInfo,
}

impl RebaseState {
    pub(crate) fn new(workdir: PathBuf, onto: String, entries: Vec<RebaseEntry>) -> Self {
        Self {
            workdir,
            todo: entries,
            selected: 0,
            onto,
        }
    }

    pub(crate) fn move_up(&mut self) -> bool {
        if self.selected == 0 {
            return false;
        }
        self.selected -= 1;
        true
    }

    pub(crate) fn move_down(&mut self) -> bool {
        if self.todo.is_empty() || self.selected + 1 >= self.todo.len() {
            return false;
        }
        self.selected += 1;
        true
    }

    /// Reorder: swap the selected entry with the one above.
    pub(crate) fn swap_up(&mut self) -> bool {
        if self.selected == 0 || self.todo.is_empty() {
            return false;
        }
        self.todo.swap(self.selected, self.selected - 1);
        self.selected -= 1;
        true
    }

    /// Reorder: swap the selected entry with the one below.
    pub(crate) fn swap_down(&mut self) -> bool {
        if self.todo.is_empty() || self.selected + 1 >= self.todo.len() {
            return false;
        }
        self.todo.swap(self.selected, self.selected + 1);
        self.selected += 1;
        true
    }

    pub(crate) fn set_op(&mut self, op: RebaseTodoOp) -> bool {
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
    pub(crate) fn to_git_todo(&self) -> Vec<RebaseTodo> {
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
/// completes or aborts. Lives on [`crate::workspace::Workspace`] as
/// `rebase_active`.
pub(crate) struct ActiveRebase {
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

pub(crate) enum RebasePause {
    /// Waiting for the user to edit a commit message. The user's
    /// in-progress message lives in a real [`crate::editor_state::EditorState`]
    /// backed by a scratch [`crate::buffer::TextBuffer`], so reword gets
    /// the full modal-editing experience (normal/insert submodes,
    /// motions, multi-line). The editor and buffer are owned by the
    /// active workspace's slotmaps and are cleaned up by
    /// `reword_confirm` / `reword_abort`.
    Reword {
        /// The sha that was just cherry-picked and committed; will be
        /// replaced with a new commit carrying the user's message when
        /// `RewordConfirm` fires.
        cherry_picked_commit: String,
        /// Original commit message, kept for the modal's reference line
        /// (the editable copy lives in the buffer below).
        original_message: String,
        editor_id: EditorId,
        buffer_id: BufferId,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum ConflictResolution {
    TakeOurs,
    TakeTheirs,
    /// Skip this entry entirely (treat as Drop for rebase purposes).
    /// Reserved for a future "skip this file" variant in the resolution
    /// UI; currently the whole-entry skip path uses `ConflictSkipEntry`
    /// and bypasses this enum.
    SkipEntry,
}

impl ActiveRebase {
    pub(crate) fn new(state: RebaseState) -> Self {
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

#[cfg(test)]
mod tests {
    use super::RebasePause;
    use crate::app::Stoat;

    fn seed_three(h: &mut crate::test_harness::TestHarness) {
        h.fake_git()
            .add_repo("/repo")
            .commit_with_message("c1", "c1: root", &[("a.rs", "line1\n")])
            .commit_with_parent_message("c2", "c1", "c2: middle", &[("a.rs", "line1\nline2\n")])
            .commit_with_parent_message(
                "c3",
                "c2",
                "c3: tip",
                &[("a.rs", "line1\nline2\nline3\n")],
            );
    }

    #[test]
    fn snapshot_rebase_open_todo() {
        let mut h = Stoat::test();
        h.resize(90, 12);
        seed_three(&mut h);
        h.open_commits("/repo");
        // Navigate to oldest commit (c1) so todo = [c2, c3] onto c1.
        h.type_keys("G");
        h.type_keys("i");
        assert_eq!(h.stoat.mode, "rebase");
        h.assert_snapshot("rebase_open_todo");
    }

    #[test]
    fn snapshot_rebase_set_ops() {
        let mut h = Stoat::test();
        h.resize(90, 12);
        seed_three(&mut h);
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        // Todo is [c2, c3]. Make c2 Squash and c3 Drop.
        h.type_keys("s");
        h.type_keys("j");
        h.type_keys("d");
        h.assert_snapshot("rebase_set_ops");
    }

    #[test]
    fn snapshot_rebase_reorder() {
        let mut h = Stoat::test();
        h.resize(90, 12);
        seed_three(&mut h);
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        // Move first entry (c2) down so order becomes c3, c2.
        h.type_keys("J");
        h.assert_snapshot("rebase_reorder");
    }

    #[test]
    fn enter_rebase_at_head_refuses() {
        let mut h = Stoat::test();
        h.resize(90, 12);
        seed_three(&mut h);
        h.open_commits("/repo");
        // Cursor defaults to selected=0 (HEAD). `i` should refuse.
        h.type_keys("i");
        assert_eq!(h.stoat.mode, "commits");
        assert!(h.stoat.active_workspace().rebase.is_none());
        let ws = h.stoat.active_workspace();
        let badge_id = ws
            .badges
            .find_by_source(crate::badge::BadgeSource::Review)
            .expect("info badge about empty todo");
        let badge = ws.badges.get(badge_id).unwrap();
        assert!(badge.label.contains("nothing"));
    }

    #[test]
    fn enter_rebase_at_middle_commit_uses_cursor_as_onto() {
        let mut h = Stoat::test();
        h.resize(90, 12);
        seed_three(&mut h);
        h.open_commits("/repo");
        // Move down once: selected = 1 (c2).
        h.type_keys("j");
        h.type_keys("i");
        assert_eq!(h.stoat.mode, "rebase");
        let state = h.stoat.active_workspace().rebase.as_ref().unwrap();
        assert_eq!(state.onto, "c2", "cursor's commit becomes onto");
        assert_eq!(state.todo.len(), 1, "only c3 above cursor");
        assert_eq!(state.todo[0].commit.sha, "c3");
    }

    #[test]
    fn execute_drop_rewrites_history_via_stepper() {
        use crate::host::GitHost;

        let mut h = Stoat::test();
        h.resize(90, 12);
        seed_three(&mut h);
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        // Todo is [c2, c3] (oldest first). Drop the first entry (c2).
        h.type_keys("d");
        h.type_keys("Enter");

        // The stepper completes synchronously for all-pick/drop plans.
        assert!(h.stoat.active_workspace().rebase.is_none());
        assert!(h.stoat.active_workspace().rebase_active.is_none());

        let repo = h.fake_git.discover(std::path::Path::new("/repo")).unwrap();
        let log = repo.log_commits(None, 10);
        // Expected: c1 root + one rebased descendant from c3; c2 dropped.
        assert_eq!(log.len(), 2, "c2 dropped, c3 rebased: {log:#?}");
        assert_eq!(log.last().unwrap().sha, "c1", "root unchanged");
    }

    #[test]
    fn conflict_on_execute_enters_conflict_mode() {
        let mut h = Stoat::test();
        h.resize(90, 12);
        seed_three(&mut h);
        h.fake_git().add_repo("/repo").simulate_conflict_at("c3");
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("Enter");
        // The stepper paused on conflict and entered conflict mode.
        assert_eq!(h.stoat.mode, "conflict");
        let ws = h.stoat.active_workspace();
        assert!(
            ws.rebase_active.is_some(),
            "rebase execution state retained for resolution"
        );
        let active = ws.rebase_active.as_ref().unwrap();
        assert!(
            matches!(active.pause, Some(RebasePause::Conflict { .. })),
            "paused on a conflict"
        );
    }

    #[test]
    fn reword_flow_rewrites_commit_message() {
        use crate::host::GitHost;

        let mut h = Stoat::test();
        h.resize(90, 12);
        seed_three(&mut h);
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        // Todo = [c2, c3]. Mark c2 as Reword (first entry, cursor at 0).
        h.type_keys("r");
        h.type_keys("Enter");
        assert_eq!(
            h.stoat.mode, "reword",
            "stepper paused into reword normal sub-mode"
        );

        // Enter insert sub-mode, delete the preloaded "c2: middle", type
        // a new message, exit to normal, then save.
        h.type_keys("i");
        assert_eq!(h.stoat.mode, "reword_insert");
        for _ in 0.."c2: middle".len() {
            h.type_keys("Backspace");
        }
        h.type_text("reworded!");
        h.type_keys("Escape");
        assert_eq!(h.stoat.mode, "reword");
        h.type_keys("ctrl-s");

        // Stepper resumes and completes.
        assert_ne!(h.stoat.mode, "reword");
        assert_ne!(h.stoat.mode, "reword_insert");
        let repo = h.fake_git.discover(std::path::Path::new("/repo")).unwrap();
        let log = repo.log_commits(None, 10);
        let msgs: Vec<_> = log.iter().map(|c| c.summary.clone()).collect();
        assert!(
            msgs.iter().any(|m| m == "reworded!"),
            "reworded commit in log: {msgs:?}"
        );
    }

    #[test]
    fn reword_submode_transitions() {
        let mut h = Stoat::test();
        h.resize(90, 12);
        seed_three(&mut h);
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("r");
        h.type_keys("Enter");
        assert_eq!(h.stoat.mode, "reword");
        h.type_keys("i");
        assert_eq!(h.stoat.mode, "reword_insert");
        h.type_keys("Escape");
        assert_eq!(h.stoat.mode, "reword");
    }

    #[test]
    fn reword_empty_message_auto_aborts() {
        use crate::host::GitHost;

        let mut h = Stoat::test();
        h.resize(90, 12);
        seed_three(&mut h);
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("r");
        h.type_keys("Enter");
        assert_eq!(h.stoat.mode, "reword");

        h.type_keys("i");
        for _ in 0.."c2: middle".len() {
            h.type_keys("Backspace");
        }
        h.type_keys("Escape");
        h.type_keys("ctrl-s");

        // Auto-abort path: rebase dropped, no reword-rewritten commit
        // landed, and the pre-existing c2 summary is still present.
        assert!(h.stoat.active_workspace().rebase_active.is_none());
        let repo = h.fake_git.discover(std::path::Path::new("/repo")).unwrap();
        let log = repo.log_commits(None, 10);
        let msgs: Vec<_> = log.iter().map(|c| c.summary.clone()).collect();
        assert!(
            !msgs.iter().any(|m| m.trim().is_empty()),
            "no empty-message commit landed: {msgs:?}"
        );
    }

    #[test]
    fn reword_escape_from_normal_aborts() {
        let mut h = Stoat::test();
        h.resize(90, 12);
        seed_three(&mut h);
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("r");
        h.type_keys("Enter");
        assert_eq!(h.stoat.mode, "reword");

        // Abort without entering insert sub-mode.
        h.type_keys("Escape");
        assert!(h.stoat.active_workspace().rebase_active.is_none());
        assert_ne!(h.stoat.mode, "reword");
        assert_ne!(h.stoat.mode, "reword_insert");
    }

    #[test]
    fn reword_multiline_message_preserved() {
        use crate::host::GitHost;

        let mut h = Stoat::test();
        h.resize(90, 12);
        seed_three(&mut h);
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("r");
        h.type_keys("Enter");
        assert_eq!(h.stoat.mode, "reword");

        h.type_keys("i");
        for _ in 0.."c2: middle".len() {
            h.type_keys("Backspace");
        }
        h.type_text("line one");
        h.type_keys("Enter");
        h.type_text("line two");
        h.type_keys("Escape");
        h.type_keys("ctrl-s");
        assert_ne!(h.stoat.mode, "reword");

        let repo = h.fake_git.discover(std::path::Path::new("/repo")).unwrap();
        let log = repo.log_commits(None, 10);
        let messages: Vec<String> = log
            .iter()
            .filter_map(|c| {
                h.fake_git
                    .commit_message(std::path::Path::new("/repo"), &c.sha)
            })
            .collect();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("line one") && m.contains("line two") && m.contains('\n')),
            "multi-line message preserved: {messages:?}"
        );
    }

    #[test]
    fn edit_flow_opens_review_and_continue_resumes() {
        use crate::host::GitHost;

        let mut h = Stoat::test();
        h.resize(90, 16);
        seed_three(&mut h);
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        // Mark c2 as Edit (first entry).
        h.type_keys("e");
        h.type_keys("Enter");
        // Stepper paused; opened review of the just-picked commit.
        assert_eq!(h.stoat.mode, "review");
        assert!(
            h.stoat.active_workspace().rebase_active.is_some(),
            "rebase execution state retained during edit"
        );
        let session = h
            .stoat
            .active_workspace()
            .review
            .as_ref()
            .expect("edit-mode review installed");
        assert_eq!(
            session.origin,
            crate::review_session::ReviewOrigin::FromRebaseEdit
        );

        // Resume via RebaseContinue.
        h.type_keys("C");
        assert!(
            h.stoat.active_workspace().rebase_active.is_none(),
            "rebase execution complete after continue"
        );
        let repo = h.fake_git.discover(std::path::Path::new("/repo")).unwrap();
        let log = repo.log_commits(None, 10);
        // Two rebased commits (from c2 and c3) plus root c1.
        assert_eq!(log.len(), 3, "full chain rebased: {log:#?}");
    }

    #[test]
    fn conflict_take_theirs_and_apply_completes_rebase() {
        use crate::host::GitHost;

        let mut h = Stoat::test();
        h.resize(90, 14);
        seed_three(&mut h);
        h.fake_git().add_repo("/repo").simulate_conflict_at("c3");
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("Enter");
        assert_eq!(h.stoat.mode, "conflict");

        // Take theirs on the selected file, then apply.
        h.type_keys("t");
        h.type_keys("Enter");

        // Stepper resumed past the conflict; rebase_active dropped.
        assert!(h.stoat.active_workspace().rebase_active.is_none());
        assert_ne!(h.stoat.mode, "conflict");

        let repo = h.fake_git.discover(std::path::Path::new("/repo")).unwrap();
        let log = repo.log_commits(None, 10);
        assert!(!log.is_empty(), "history remains readable after resolve");
    }

    #[test]
    fn conflict_skip_entry_drops_the_commit() {
        use crate::host::GitHost;

        let mut h = Stoat::test();
        h.resize(90, 14);
        seed_three(&mut h);
        h.fake_git().add_repo("/repo").simulate_conflict_at("c3");
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("Enter");
        assert_eq!(h.stoat.mode, "conflict");

        h.type_keys("s"); // skip the conflicted entry
        assert!(h.stoat.active_workspace().rebase_active.is_none());

        let repo = h.fake_git.discover(std::path::Path::new("/repo")).unwrap();
        let log = repo.log_commits(None, 10);
        // c3 was skipped; we should have c1 root + rebased c2.
        assert_eq!(log.len(), 2, "skipped entry absent from log: {log:#?}");
    }

    #[test]
    fn conflict_abort_drops_rebase_state() {
        let mut h = Stoat::test();
        h.resize(90, 14);
        seed_three(&mut h);
        h.fake_git().add_repo("/repo").simulate_conflict_at("c3");
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("Enter");
        assert_eq!(h.stoat.mode, "conflict");
        h.type_keys("a");
        assert!(h.stoat.active_workspace().rebase_active.is_none());
        assert_ne!(h.stoat.mode, "conflict");
    }

    #[test]
    fn snapshot_rebase_reword_mode_ui() {
        let mut h = Stoat::test();
        h.resize(90, 12);
        seed_three(&mut h);
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("r");
        h.type_keys("Enter");
        assert_eq!(h.stoat.mode, "reword");
        h.assert_snapshot("rebase_reword_mode");
    }

    #[test]
    fn snapshot_rebase_conflict_mode_ui() {
        let mut h = Stoat::test();
        h.resize(100, 18);
        seed_three(&mut h);
        h.fake_git().add_repo("/repo").simulate_conflict_at("c3");
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("Enter");
        assert_eq!(h.stoat.mode, "conflict");
        h.assert_snapshot("rebase_conflict_mode");
    }

    #[test]
    fn abort_discards_rebase_state() {
        let mut h = Stoat::test();
        h.resize(90, 12);
        seed_three(&mut h);
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        assert_eq!(h.stoat.mode, "rebase");
        h.type_keys("q");
        assert_eq!(h.stoat.mode, "commits");
        assert!(h.stoat.active_workspace().rebase.is_none());
        assert!(h
            .fake_git
            .applied_rebases(std::path::Path::new("/repo"))
            .is_empty());
    }
}

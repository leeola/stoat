use crate::host::{CommitInfo, RebaseTodo, RebaseTodoOp};
use std::path::PathBuf;

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

#[cfg(test)]
mod tests {
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
        h.type_keys("i");
        // Move first entry (c2) down so order becomes c3, c2.
        h.type_keys("J");
        h.assert_snapshot("rebase_reorder");
    }

    #[test]
    fn execute_drop_rewrites_history_via_fake() {
        let mut h = Stoat::test();
        h.resize(90, 12);
        seed_three(&mut h);
        h.open_commits("/repo");
        h.type_keys("i");
        // Todo is [c2, c3] (oldest first). Drop the first entry (c2).
        h.type_keys("d");
        h.type_keys("Enter");

        let rebases = h.fake_git.applied_rebases(std::path::Path::new("/repo"));
        assert_eq!(rebases.len(), 1, "one rebase recorded");
        let plan = &rebases[0];
        assert_eq!(plan.onto, "c1", "onto is the oldest loaded commit");
        assert_eq!(plan.todo.len(), 2);
        assert_eq!(plan.todo[0].op, crate::host::RebaseTodoOp::Drop);
        assert_eq!(plan.todo[0].sha, "c2");
        assert_eq!(plan.todo[1].op, crate::host::RebaseTodoOp::Pick);
        assert_eq!(plan.todo[1].sha, "c3");
    }

    #[test]
    fn conflict_on_execute_keeps_rebase_state_and_shows_badge() {
        let mut h = Stoat::test();
        h.resize(90, 12);
        seed_three(&mut h);
        h.fake_git().add_repo("/repo").simulate_conflict_at("c3");
        h.open_commits("/repo");
        h.type_keys("i");
        h.type_keys("Enter");
        let ws = h.stoat.active_workspace();
        let badge_id = ws
            .badges
            .find_by_source(crate::badge::BadgeSource::Review)
            .expect("error badge");
        let badge = ws.badges.get(badge_id).unwrap();
        assert_eq!(badge.state, crate::badge::BadgeState::Error);
        assert!(badge.label.contains("conflict"));
        assert!(
            h.stoat.active_workspace().rebase.is_some(),
            "rebase state retained on conflict"
        );
    }

    #[test]
    fn abort_discards_rebase_state() {
        let mut h = Stoat::test();
        h.resize(90, 12);
        seed_three(&mut h);
        h.open_commits("/repo");
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

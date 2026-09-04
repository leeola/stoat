use crate::host::{CommitInfo, ConflictedFile, RebaseTodo, RebaseTodoOp};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
};

/// Editable rebase plan owned by a [`crate::workspace::Workspace`]
/// while the user is in `"rebase"` mode. Seeded from the commit list
/// when the user presses `i` to enter the mode, mutated by todo-list
/// edits (op changes, reorders), and consumed by `ExecuteRebase`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RebaseState {
    pub workdir: PathBuf,
    pub todo: Vec<RebaseEntry>,
    pub selected: usize,
    /// Sha of the commit this plan stacks onto (typically the parent
    /// of the oldest entry).
    pub onto: String,
    /// First todo entry painted, which is what keeps a selection past the
    /// pane's height on screen.
    ///
    /// Defaulted for serde so a workspace saved before the list scrolled
    /// still loads, starting at the top as it did then.
    #[serde(default)]
    pub scroll_top: usize,
    /// Todo rows the pane held on the most recent render, which bounds the
    /// window the selection is kept inside. Zero until the first paint.
    #[serde(default)]
    pub viewport_rows: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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
            scroll_top: 0,
            viewport_rows: 0,
        }
    }

    /// Keep `selected` within `[scroll_top, scroll_top + height)`.
    pub(crate) fn ensure_selected_visible(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.selected < self.scroll_top {
            self.scroll_top = self.selected;
        } else if self.selected >= self.scroll_top + height {
            self.scroll_top = self.selected + 1 - height;
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
        input: crate::input_view::InputView,
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
pub(crate) enum ConflictResolution {
    TakeOurs,
    TakeTheirs,
    /// Skip this entry entirely (treat as Drop for rebase purposes).
    /// Reserved for a future "skip this file" variant in the resolution
    /// UI; currently the whole-entry skip path uses `RebaseConflictSkipEntry`
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
    use crate::{
        app::Stoat,
        badge::BadgeSource,
        host::GitHost,
        test_harness::{CommitSpec, TestHarness},
        workspace::diff::DiffBase,
    };

    const THREE_COMMITS: &[CommitSpec<'static>] = &[
        ("c1", "c1: root", &[("a.rs", "line1\n")]),
        ("c2", "c2: middle", &[("a.rs", "line1\nline2\n")]),
        ("c3", "c3: tip", &[("a.rs", "line1\nline2\nline3\n")]),
    ];

    #[test]
    fn snapshot_rebase_open_todo() {
        let mut h = Stoat::test();
        h.resize(90, 12);
        h.seed_linear_history("/repo", THREE_COMMITS);
        h.open_commits("/repo");
        // Navigate to oldest commit (c1) so todo = [c2, c3] onto c1.
        h.type_keys("G");
        h.type_keys("i");
        assert_eq!(h.stoat.current_view(), Some("rebase"));
        h.assert_snapshot("rebase_open_todo");
    }

    #[test]
    fn snapshot_rebase_set_ops() {
        let mut h = Stoat::test();
        h.resize(90, 12);
        h.seed_linear_history("/repo", THREE_COMMITS);
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
        h.seed_linear_history("/repo", THREE_COMMITS);
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        // Move first entry (c2) down so order becomes c3, c2.
        h.type_keys("J");
        h.assert_snapshot("rebase_reorder");
    }

    /// A todo taller than the pane walks the selection past the last painted
    /// row, where nothing marks it and `j` reads as dead. The list scrolls to
    /// follow instead.
    #[test]
    fn the_rebase_list_scrolls_to_follow_its_selection() {
        let names: Vec<(String, String, String)> = (0..30)
            .map(|i| (format!("c{i}"), format!("c{i}: commit"), format!("l{i}\n")))
            .collect();
        let files: Vec<[(&str, &str); 1]> = names
            .iter()
            .map(|(_, _, line)| [("a.rs", line.as_str())])
            .collect();
        let specs: Vec<CommitSpec<'_>> = names
            .iter()
            .zip(&files)
            .map(|((sha, msg, _), files)| (sha.as_str(), msg.as_str(), files.as_slice()))
            .collect();

        let mut h = Stoat::test();
        h.resize(90, 12);
        h.seed_linear_history("/repo", &specs);
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        assert_eq!(h.stoat.current_view(), Some("rebase"));

        // Ten rows past the top, on a pane that holds far fewer.
        for _ in 0..10 {
            h.type_keys("j");
        }
        h.snapshot();

        let state = h
            .stoat
            .active_workspace()
            .rebase
            .as_ref()
            .expect("the rebase screen is open");
        assert!(
            state.viewport_rows > 0 && state.viewport_rows < state.todo.len(),
            "the pane holds fewer rows than the todo, which is the case under \
             test: {} of {}",
            state.viewport_rows,
            state.todo.len(),
        );
        assert!(
            (state.scroll_top..state.scroll_top + state.viewport_rows).contains(&state.selected),
            "selected {} sits outside the painted window [{}, {})",
            state.selected,
            state.scroll_top,
            state.scroll_top + state.viewport_rows,
        );
    }

    #[test]
    fn enter_rebase_at_head_refuses() {
        let mut h = Stoat::test();
        h.resize(90, 12);
        h.seed_linear_history("/repo", THREE_COMMITS);
        h.open_commits("/repo");
        // Cursor defaults to selected=0 (HEAD). `i` should refuse.
        h.type_keys("i");
        assert_eq!(h.stoat.current_view(), Some("commits"));
        assert!(h.stoat.active_workspace().rebase.is_none());
        let ws = h.stoat.active_workspace();
        let badge_id = ws
            .badges
            .find_by_source(BadgeSource::Review)
            .expect("info badge about empty todo");
        let badge = ws.badges.get(badge_id).unwrap();
        assert!(badge.label.contains("nothing"));
    }

    #[test]
    fn enter_rebase_at_middle_commit_uses_cursor_as_onto() {
        let mut h = Stoat::test();
        h.resize(90, 12);
        h.seed_linear_history("/repo", THREE_COMMITS);
        h.open_commits("/repo");
        // Move down once: selected = 1 (c2).
        h.type_keys("j");
        h.type_keys("i");
        assert_eq!(h.stoat.current_view(), Some("rebase"));
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
        h.seed_linear_history("/repo", THREE_COMMITS);
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
        h.seed_linear_history("/repo", THREE_COMMITS);
        h.fake_git().add_repo("/repo").simulate_conflict_at("c3");
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("Enter");
        // The stepper paused on conflict and entered conflict mode.
        assert_eq!(h.stoat.current_view(), Some("rebase_conflict"));
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
        h.seed_linear_history("/repo", THREE_COMMITS);
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        // Todo = [c2, c3]. Mark c2 as Reword (first entry, cursor at 0).
        h.type_keys("r");
        h.type_keys("Enter");
        assert_eq!(
            h.stoat.current_view(),
            Some("reword"),
            "stepper paused into the reword screen"
        );

        // Enter insert sub-mode, delete the preloaded "c2: middle", type
        // a new message, exit to normal, then save.
        h.type_keys("i");
        assert_eq!(h.stoat.focused_mode(), "insert");
        for _ in 0.."c2: middle".len() {
            h.type_keys("Backspace");
        }
        h.type_text("reworded!");
        h.type_keys("Escape");
        assert_eq!(h.stoat.current_view(), Some("reword"));
        h.type_keys("ctrl-s");

        // Stepper resumes and completes.
        assert_ne!(h.stoat.current_view(), Some("reword"));
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
        h.seed_linear_history("/repo", THREE_COMMITS);
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("r");
        h.type_keys("Enter");
        assert_eq!(h.stoat.current_view(), Some("reword"));
        h.type_keys("i");
        assert_eq!(h.stoat.focused_mode(), "insert");
        h.type_keys("Escape");
        assert_eq!(h.stoat.current_view(), Some("reword"));
    }

    #[test]
    fn reword_empty_message_auto_aborts() {
        use crate::host::GitHost;

        let mut h = Stoat::test();
        h.resize(90, 12);
        h.seed_linear_history("/repo", THREE_COMMITS);
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("r");
        h.type_keys("Enter");
        assert_eq!(h.stoat.current_view(), Some("reword"));

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
        h.seed_linear_history("/repo", THREE_COMMITS);
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("r");
        h.type_keys("Enter");
        assert_eq!(h.stoat.current_view(), Some("reword"));

        // Abort without entering insert sub-mode.
        h.type_keys("Escape");
        assert!(h.stoat.active_workspace().rebase_active.is_none());
        assert_ne!(h.stoat.current_view(), Some("reword"));
    }

    #[test]
    fn reword_multiline_message_preserved() {
        use crate::host::GitHost;

        let mut h = Stoat::test();
        h.resize(90, 12);
        h.seed_linear_history("/repo", THREE_COMMITS);
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("r");
        h.type_keys("Enter");
        assert_eq!(h.stoat.current_view(), Some("reword"));

        h.type_keys("i");
        for _ in 0.."c2: middle".len() {
            h.type_keys("Backspace");
        }
        h.type_text("line one");
        h.type_keys("Enter");
        h.type_text("line two");
        h.type_keys("Escape");
        h.type_keys("ctrl-s");
        assert_ne!(h.stoat.current_view(), Some("reword"));

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

    /// A history whose middle commit changed nothing, so the Edit stop on it
    /// has no file to open.
    const EMPTY_MIDDLE: &[CommitSpec<'static>] = &[
        ("c1", "c1: root", &[("a.rs", "line1\n")]),
        ("c2", "c2: no change", &[("a.rs", "line1\n")]),
        ("c3", "c3: tip", &[("a.rs", "line1\nline2\n")]),
    ];

    /// Stop the stepper on an Edit entry, with the tree seeded so the diff the
    /// pause lands on has a buffer to read.
    fn pause_on_edit(h: &mut TestHarness) {
        pause_on_edit_over(h, THREE_COMMITS, b"line1\nline2\nline3\n");
    }

    fn pause_on_edit_over(h: &mut TestHarness, commits: &[CommitSpec<'_>], tree: &[u8]) {
        h.resize(90, 16);
        h.seed_linear_history("/repo", commits);
        h.fake_fs().insert_file("/repo/a.rs", tree);
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        // Mark c2 as Edit (first entry).
        h.type_keys("e");
        h.type_keys("Enter");
    }

    fn checkouts(h: &TestHarness) -> Vec<String> {
        h.fake_git.checkouts(std::path::Path::new("/repo"))
    }

    fn diff_base(h: &TestHarness) -> Option<Option<String>> {
        match h.stoat.active_workspace().diff_base() {
            Some(DiffBase::Rev { sha }) => Some(sha.clone()),
            _ => None,
        }
    }

    /// An Edit stop checks the picked commit out and shows what it changed.
    ///
    /// The checkout is what makes the diff mean anything: the stepper builds
    /// commits without moving HEAD or writing the tree, so without it the
    /// buffers on screen would be the pre-rebase tree measured against the
    /// picked commit's parent.
    #[test]
    fn an_edit_pause_checks_the_commit_out_and_diffs_it() {
        let mut h = Stoat::test();
        pause_on_edit(&mut h);

        assert_eq!(h.stoat.current_view(), Some("diff"));
        assert!(
            h.stoat.active_workspace().rebase_active.is_some(),
            "rebase execution state retained during edit"
        );

        let picked = checkouts(&h).last().cloned().expect("a checkout happened");
        let sha = picked
            .strip_prefix("detached:")
            .expect("detached checkout")
            .to_string();
        let repo = h
            .fake_git()
            .discover(std::path::Path::new("/repo"))
            .unwrap();
        assert_eq!(
            (repo.resolve_rev("HEAD"), diff_base(&h)),
            (Some(sha.clone()), Some(repo.parent_sha(&sha))),
            "HEAD is the picked commit and the diff reads against its parent"
        );

        let panes = &h.stoat.active_workspace().panes;
        assert!(
            panes.pane(panes.focus()).diff_mode,
            "the pause armed the diff view"
        );

        // The review screen used to be what said a rebase was stopped here. An
        // ordinary diff view says nothing, so the badge is the only thing left
        // telling the user where they are and how to leave.
        assert_eq!(
            pause_badge(&h).as_deref(),
            Some(format!("editing {}, C continues", &sha[..sha.len().min(7)]).as_str()),
        );
    }

    fn pause_badge(h: &TestHarness) -> Option<String> {
        let ws = h.stoat.active_workspace();
        ws.badges
            .find_by_source(BadgeSource::Review)
            .and_then(|id| ws.badges.get(id))
            .map(|badge| badge.label.clone())
    }

    /// A stop on a commit that changed nothing still holds the pause open.
    ///
    /// There is no file to open and so no editor carrying the diff view, which
    /// is what usually names the screen. Without the pause naming it instead the
    /// view would read as an ordinary file and the key that resumes the rebase
    /// would not be bound, stranding the user mid-plan.
    #[test]
    fn a_stop_on_an_empty_commit_keeps_continue_bound() {
        let mut h = Stoat::test();
        pause_on_edit_over(&mut h, EMPTY_MIDDLE, b"line1\n");

        assert_eq!(h.stoat.current_view(), Some("diff"), "the pause names it");

        h.type_keys("C");
        assert!(
            h.stoat.active_workspace().rebase_active.is_none(),
            "C resumed the rebase from the empty stop"
        );
    }

    /// An amend made while stopped is what the resume has to carry forward.
    ///
    /// The stepper recorded the commit it created, and an amend replaces that
    /// commit with another. HEAD is what tracks the replacement, so resuming
    /// from the recorded sha would rebase the rest of the plan onto the commit
    /// the user just edited away.
    #[test]
    fn continue_carries_an_amend_made_while_stopped() {
        let mut h = Stoat::test();
        pause_on_edit(&mut h);

        let repo = h.fake_git.discover(std::path::Path::new("/repo")).unwrap();
        let amended = {
            let head = repo.resolve_rev("HEAD").expect("HEAD");
            let tree = repo.tree_oid(&head).expect("the picked commit's tree");
            repo.amend_head(&tree, Some("c2: amended while stopped"))
                .expect("amend")
        };

        h.type_keys("C");
        let messages: Vec<String> = repo
            .log_commits(None, 10)
            .iter()
            .filter_map(|c| {
                h.fake_git
                    .commit_message(std::path::Path::new("/repo"), &c.sha)
            })
            .collect();
        assert!(
            messages.iter().any(|m| m.contains("amended while stopped")),
            "the rebase continued from {amended}, so the amend is in the chain: {messages:?}"
        );
    }

    /// Continuing resumes from HEAD rather than from the sha the stepper last
    /// recorded, and puts the diff view away with the pause.
    #[test]
    fn continue_resumes_from_head_and_clears_the_diff() {
        let mut h = Stoat::test();
        pause_on_edit(&mut h);

        h.type_keys("C");
        assert!(
            h.stoat.active_workspace().rebase_active.is_none(),
            "rebase execution complete after continue"
        );

        let panes = &h.stoat.active_workspace().panes;
        assert_eq!(
            (diff_base(&h), panes.pane(panes.focus()).diff_mode),
            (None, false),
            "the base and the latch went with the pause"
        );

        let repo = h
            .fake_git()
            .discover(std::path::Path::new("/repo"))
            .unwrap();
        let log = repo.log_commits(None, 10);
        // Two rebased commits (from c2 and c3) plus root c1.
        assert_eq!(log.len(), 3, "full chain rebased: {log:#?}");
    }

    #[test]
    fn conflict_take_theirs_and_apply_completes_rebase() {
        use crate::host::GitHost;

        let mut h = Stoat::test();
        h.resize(90, 14);
        h.seed_linear_history("/repo", THREE_COMMITS);
        h.fake_git().add_repo("/repo").simulate_conflict_at("c3");
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("Enter");
        assert_eq!(h.stoat.current_view(), Some("rebase_conflict"));

        // Take theirs on the selected file, then apply.
        h.type_keys("t");
        h.type_keys("Enter");

        // Stepper resumed past the conflict; rebase_active dropped.
        assert!(h.stoat.active_workspace().rebase_active.is_none());
        assert_ne!(h.stoat.current_view(), Some("rebase_conflict"));

        let repo = h.fake_git.discover(std::path::Path::new("/repo")).unwrap();
        let log = repo.log_commits(None, 10);
        assert!(!log.is_empty(), "history remains readable after resolve");
    }

    #[test]
    fn conflict_skip_entry_drops_the_commit() {
        use crate::host::GitHost;

        let mut h = Stoat::test();
        h.resize(90, 14);
        h.seed_linear_history("/repo", THREE_COMMITS);
        h.fake_git().add_repo("/repo").simulate_conflict_at("c3");
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("Enter");
        assert_eq!(h.stoat.current_view(), Some("rebase_conflict"));

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
        h.seed_linear_history("/repo", THREE_COMMITS);
        h.fake_git().add_repo("/repo").simulate_conflict_at("c3");
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("Enter");
        assert_eq!(h.stoat.current_view(), Some("rebase_conflict"));
        h.type_keys("a");
        assert!(h.stoat.active_workspace().rebase_active.is_none());
        assert_ne!(h.stoat.current_view(), Some("rebase_conflict"));
    }

    #[test]
    fn snapshot_rebase_reword_mode_ui() {
        let mut h = Stoat::test();
        h.resize(90, 12);
        h.seed_linear_history("/repo", THREE_COMMITS);
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("r");
        h.type_keys("Enter");
        assert_eq!(h.stoat.current_view(), Some("reword"));
        h.assert_snapshot("rebase_reword_mode");
    }

    #[test]
    fn snapshot_rebase_conflict_mode_ui() {
        let mut h = Stoat::test();
        h.resize(100, 18);
        h.seed_linear_history("/repo", THREE_COMMITS);
        h.fake_git().add_repo("/repo").simulate_conflict_at("c3");
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("Enter");
        assert_eq!(h.stoat.current_view(), Some("rebase_conflict"));
        h.assert_snapshot("rebase_conflict_mode");
    }

    #[test]
    fn abort_discards_rebase_state() {
        let mut h = Stoat::test();
        h.resize(90, 12);
        h.seed_linear_history("/repo", THREE_COMMITS);
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        assert_eq!(h.stoat.current_view(), Some("rebase"));
        h.type_keys("q");
        assert_eq!(h.stoat.current_view(), Some("commits"));
        assert!(h.stoat.active_workspace().rebase.is_none());
        assert!(h
            .fake_git
            .applied_rebases(std::path::Path::new("/repo"))
            .is_empty());
    }

    fn dirty_badge(h: &TestHarness) -> Option<String> {
        use crate::badge::BadgeSource;
        let ws = h.stoat.active_workspace();
        ws.badges
            .find_by_source(BadgeSource::Review)
            .and_then(|id| ws.badges.get(id))
            .map(|b| b.label.clone())
    }

    #[test]
    fn untracked_file_does_not_block_rebase() {
        use crate::host::GitHost;

        let mut h = Stoat::test();
        h.resize(90, 12);
        h.seed_linear_history("/repo", THREE_COMMITS);
        h.fake_git().add_repo("/repo").untracked("scratch.txt");
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("d");
        h.type_keys("Enter");
        assert_ne!(
            dirty_badge(&h).as_deref(),
            Some("working tree dirty: commit or stash first"),
            "untracked-only tree is not blocked"
        );
        let repo = h.fake_git.discover(std::path::Path::new("/repo")).unwrap();
        assert_eq!(
            repo.log_commits(None, 10).len(),
            2,
            "dropped commit rebased through the untracked tree"
        );
    }

    #[test]
    fn tracked_modification_blocks_rebase() {
        use crate::host::GitHost;

        let mut h = Stoat::test();
        h.resize(90, 12);
        h.seed_linear_history("/repo", THREE_COMMITS);
        h.fake_git()
            .add_repo("/repo")
            .modified("tracked.rs", "old\n", "new\n");
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");
        h.type_keys("d");
        h.type_keys("Enter");
        assert_eq!(
            dirty_badge(&h).as_deref(),
            Some("working tree dirty: commit or stash first")
        );
        let repo = h.fake_git.discover(std::path::Path::new("/repo")).unwrap();
        assert_eq!(
            repo.log_commits(None, 10).len(),
            3,
            "blocked rebase leaves history intact"
        );
    }
}

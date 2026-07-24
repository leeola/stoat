use crate::{
    commit_list::PendingPreview,
    fuzzy,
    host::CommitInfo,
    input_view::{InputView, SubmitTarget},
    review_session::ReviewSession,
    workspace::Workspace,
};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use stoat_scheduler::Executor;

/// What selecting a row in a [`CommitPicker`] does.
///
/// The rows, filtering, and preview are the same whatever the picker is being
/// used for, so a future commit-listing surface adds a variant here and a
/// select semantic rather than a second picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// `:git-review` is the only thing that opens a picker, so nothing constructs a
// role until that command lands.
#[allow(dead_code)]
pub(crate) enum CommitPickerRole {
    /// Choose the commit a review walk starts from.
    PickBase,
}

/// One rendered picker row, and the segment lengths the renderer colors by.
///
/// The same string is the fuzzy haystack, so a matched character offset is
/// always a column offset into this text. See [`CommitPicker::row`].
pub(crate) struct CommitRow {
    pub(crate) text: String,
    /// Characters of `text` holding the abbreviated sha.
    pub(crate) sha_chars: usize,
    /// Characters of `text` holding the branch names, excluding the space that
    /// separates them from the sha. Zero when no branch points at the commit.
    pub(crate) branch_chars: usize,
}

/// Modal listing a ref's first-parent history, fuzzy-filtered, with the
/// selected commit's diff previewed beside it.
///
/// Rows are newest-first as the walk produced them, and an empty query keeps
/// that order rather than sorting. Commit order is the information the list
/// carries.
///
/// Previews are lazy, one background [`ReviewSession`] build per sha, so
/// scrolling a long history does not diff every commit in it.
pub(crate) struct CommitPicker {
    pub(crate) role: CommitPickerRole,
    pub(crate) workdir: PathBuf,
    /// Full sha of the ref the picker was opened over. Excluded from
    /// [`Self::default_selection`], which looks for a branch other than the one
    /// the user is already looking at.
    #[allow(dead_code)]
    pub(crate) ref_sha: String,
    pub(crate) input: InputView,
    /// Walked history, newest first.
    pub(crate) commits: Vec<CommitInfo>,
    /// Full sha to the local branch names pointing at it.
    pub(crate) branch_tips: HashMap<String, Vec<String>>,
    /// Indices into `commits`, in display order.
    pub(crate) filtered: Vec<usize>,
    /// Matched character offsets per filtered row, parallel to `filtered`.
    pub(crate) match_indices: Vec<Vec<u32>>,
    pub(crate) selected: usize,
    /// Rendered list height, refreshed each frame so [`Self::page`] can size
    /// its step. `None` before the first render.
    pub(crate) viewport_rows: Option<usize>,
    pub(crate) preview_sessions: HashMap<String, Arc<ReviewSession>>,
    pub(crate) pending_preview: Option<PendingPreview>,
    /// Sha the in-flight or most recent preview was requested for, so a build
    /// that lands after the selection moved on is discarded.
    pub(crate) requested_preview: Option<String>,
}

impl CommitPicker {
    /// Build a picker over `commits`, starting on [`Self::default_selection`].
    ///
    /// Unused until `:git-review` lands, which is the command that opens one.
    #[allow(dead_code)]
    pub(crate) fn new(
        ws: &mut Workspace,
        executor: Executor,
        role: CommitPickerRole,
        workdir: PathBuf,
        ref_sha: String,
        commits: Vec<CommitInfo>,
        branch_tips: HashMap<String, Vec<String>>,
    ) -> Self {
        let input = InputView::create(ws, executor, SubmitTarget::CommitPicker, "", "insert", 1);
        let mut picker = Self {
            role,
            workdir,
            ref_sha,
            input,
            commits,
            branch_tips,
            filtered: Vec::new(),
            match_indices: Vec::new(),
            selected: 0,
            viewport_rows: None,
            preview_sessions: HashMap::new(),
            pending_preview: None,
            requested_preview: None,
        };
        picker.refilter("");
        picker.selected = picker.default_selection();
        picker
    }

    /// The display text for `idx`, which doubles as its fuzzy haystack.
    ///
    /// Composed as `<short sha> <branches> <summary>`, with the branch segment
    /// omitted when nothing points at the commit. Returning the segment lengths
    /// alongside the text lets the renderer color the three columns without
    /// rebuilding the string, which is what keeps highlight offsets meaningful.
    pub(crate) fn row(&self, idx: usize) -> CommitRow {
        let Some(commit) = self.commits.get(idx) else {
            return CommitRow {
                text: String::new(),
                sha_chars: 0,
                branch_chars: 0,
            };
        };

        let branches = self
            .branch_tips
            .get(&commit.sha)
            .map(|names| names.join(" "))
            .unwrap_or_default();

        let sha_chars = commit.short_sha.chars().count();
        let branch_chars = branches.chars().count();
        let text = if branches.is_empty() {
            format!("{} {}", commit.short_sha, commit.summary)
        } else {
            format!("{} {} {}", commit.short_sha, branches, commit.summary)
        };

        CommitRow {
            text,
            sha_chars,
            branch_chars,
        }
    }

    /// Re-rank the rows for `query`, matches first by score then by row text.
    /// An empty or whitespace-only query lists every commit newest-first with
    /// no highlights.
    pub(crate) fn refilter(&mut self, query: &str) {
        let items = (0..self.commits.len()).map(|idx| (idx, self.row(idx).text));
        let Some(mut matches) = fuzzy::match_and_rank(query, items) else {
            self.filtered = (0..self.commits.len()).collect();
            self.match_indices = vec![Vec::new(); self.commits.len()];
            self.clamp_selected();
            return;
        };

        matches.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.haystack.cmp(&b.haystack))
        });

        self.filtered = Vec::with_capacity(matches.len());
        self.match_indices = Vec::with_capacity(matches.len());
        for m in matches {
            self.filtered.push(m.item);
            self.match_indices.push(m.matched_indices);
        }
        self.clamp_selected();
    }

    /// Adjust the selection cursor by `delta`, saturating at list bounds.
    pub(crate) fn move_selection(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        let max = (self.filtered.len() - 1) as i32;
        self.selected = (self.selected as i32 + delta).clamp(0, max) as usize;
    }

    /// Page the selection by half the rendered list height in `dir` (negative
    /// up, positive down). Falls back to a single row before the first render
    /// sets [`Self::viewport_rows`].
    ///
    /// No key binds this yet. It exists so the picker matches the other modal
    /// lists, whose keymaps all page with Ctrl-F and Ctrl-B.
    #[allow(dead_code)]
    pub(crate) fn page(&mut self, dir: i32) {
        let step = self
            .viewport_rows
            .map(|v| v.div_ceil(2).max(1))
            .unwrap_or(1) as i32;
        self.move_selection(dir * step);
    }

    /// The commit under the selection cursor, or `None` for an empty list.
    pub(crate) fn selected_commit(&self) -> Option<&CommitInfo> {
        let idx = *self.filtered.get(self.selected)?;
        self.commits.get(idx)
    }

    /// The filtered row to start on.
    ///
    /// This is the newest commit carrying a local branch other than the ref the
    /// picker was opened over, falling back to the newest row.
    ///
    /// Opening `:git-review main` from `main` most often means reviewing what
    /// another branch added, so the nearest such branch tip is a better landing
    /// spot than the tip the user is already sitting on.
    #[allow(dead_code)]
    pub(crate) fn default_selection(&self) -> usize {
        self.filtered
            .iter()
            .position(|&idx| {
                let sha = &self.commits[idx].sha;
                *sha != self.ref_sha && self.branch_tips.contains_key(sha)
            })
            .unwrap_or(0)
    }

    pub(crate) fn dispose(&self, ws: &mut Workspace) {
        self.input.dispose(ws);
    }

    fn clamp_selected(&mut self) {
        if self.filtered.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len() - 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CommitPicker, CommitPickerRole};
    use crate::{
        buffer::BufferId,
        editor_state::EditorId,
        host::CommitInfo,
        input_view::{InputView, SubmitTarget},
    };
    use std::{collections::HashMap, path::PathBuf};

    fn commit(sha: &str, summary: &str) -> CommitInfo {
        CommitInfo {
            sha: sha.to_string(),
            short_sha: sha.chars().take(7).collect(),
            summary: summary.to_string(),
            author_name: "test".into(),
            author_email: "t@t".into(),
            time: 0,
            parent_count: 1,
        }
    }

    /// A picker over `commits` (newest first) with `branches` as
    /// `(name, sha)` pairs, opened over `ref_sha`.
    fn picker(commits: Vec<CommitInfo>, branches: &[(&str, &str)], ref_sha: &str) -> CommitPicker {
        let mut branch_tips: HashMap<String, Vec<String>> = HashMap::new();
        for (name, sha) in branches {
            branch_tips
                .entry((*sha).to_string())
                .or_default()
                .push((*name).to_string());
        }
        let mut p = CommitPicker {
            role: CommitPickerRole::PickBase,
            workdir: PathBuf::from("/repo"),
            ref_sha: ref_sha.to_string(),
            input: InputView {
                editor_id: EditorId::default(),
                buffer_id: BufferId::new(0),
                target: SubmitTarget::CommitPicker,
                max_height: 1,
            },
            commits,
            branch_tips,
            filtered: Vec::new(),
            match_indices: Vec::new(),
            selected: 0,
            viewport_rows: None,
            preview_sessions: HashMap::new(),
            pending_preview: None,
            requested_preview: None,
        };
        p.refilter("");
        p
    }

    fn history() -> Vec<CommitInfo> {
        vec![
            commit("ccc3333", "add the widget"),
            commit("bbb2222", "rename the gadget"),
            commit("aaa1111", "initial import"),
        ]
    }

    fn shown(p: &CommitPicker) -> Vec<String> {
        p.filtered
            .iter()
            .map(|&idx| p.commits[idx].sha.clone())
            .collect()
    }

    #[test]
    fn empty_query_keeps_newest_first_order() {
        let p = picker(history(), &[], "ccc3333");
        assert_eq!(shown(&p), ["ccc3333", "bbb2222", "aaa1111"]);
        assert_eq!(p.match_indices, vec![Vec::<u32>::new(); 3]);
    }

    #[test]
    fn refilter_matches_summary_text() {
        let mut p = picker(history(), &[], "ccc3333");
        p.refilter("widget");
        assert_eq!(shown(&p), ["ccc3333"]);
    }

    #[test]
    fn refilter_matches_an_abbreviated_sha() {
        let mut p = picker(history(), &[], "ccc3333");
        p.refilter("bbb2");
        assert_eq!(shown(&p), ["bbb2222"]);
    }

    #[test]
    fn refilter_matches_a_branch_name() {
        let mut p = picker(history(), &[("feature", "bbb2222")], "ccc3333");
        p.refilter("feature");
        assert_eq!(shown(&p), ["bbb2222"]);
    }

    #[test]
    fn row_reports_its_segment_lengths() {
        let p = picker(history(), &[("main", "ccc3333")], "ccc3333");
        let row = p.row(0);
        assert_eq!(row.text, "ccc3333 main add the widget");
        assert_eq!((row.sha_chars, row.branch_chars), (7, 4));

        let bare = p.row(1);
        assert_eq!(bare.text, "bbb2222 rename the gadget");
        assert_eq!((bare.sha_chars, bare.branch_chars), (7, 0));
    }

    #[test]
    fn default_selection_picks_the_newest_other_branch_tip() {
        let p = picker(
            history(),
            &[("main", "ccc3333"), ("feature", "bbb2222")],
            "ccc3333",
        );
        assert_eq!(
            p.default_selection(),
            1,
            "the ref's own tip is skipped for the next branch down"
        );
    }

    #[test]
    fn default_selection_falls_back_to_the_newest_row() {
        assert_eq!(picker(history(), &[], "ccc3333").default_selection(), 0);
        assert_eq!(
            picker(history(), &[("main", "ccc3333")], "ccc3333").default_selection(),
            0,
            "a repo whose only branch is the ref itself has no better row"
        );
    }

    #[test]
    fn move_selection_clamps_to_bounds() {
        let mut p = picker(history(), &[], "ccc3333");
        p.move_selection(-1);
        assert_eq!(p.selected, 0);
        p.move_selection(9);
        assert_eq!(p.selected, 2);
    }

    #[test]
    fn page_steps_by_half_the_viewport() {
        let mut p = picker(history(), &[], "ccc3333");
        p.viewport_rows = Some(4);
        p.page(1);
        assert_eq!(p.selected, 2);
        p.page(-1);
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn refilter_clamps_a_stale_selection() {
        let mut p = picker(history(), &[], "ccc3333");
        p.selected = 2;
        p.refilter("widget");
        assert_eq!(p.filtered.len(), 1);
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn selected_commit_follows_the_cursor() {
        let mut p = picker(history(), &[], "ccc3333");
        p.move_selection(1);
        assert_eq!(p.selected_commit().map(|c| c.sha.as_str()), Some("bbb2222"));
    }

    /// The `modal == commit_picker` keymap block reaches the handlers.
    ///
    /// Worth pinning because an unresolvable action name in config.stcfg is
    /// silently dropped, so a mistyped binding would leave the modal inert with
    /// nothing else to catch it.
    #[test]
    fn the_keymap_block_drives_the_picker() {
        let mut h = seeded_picker_harness();
        h.stoat.set_focused_mode("insert".to_string());

        h.type_keys("down");
        h.settle();
        assert_eq!(
            h.stoat
                .commit_picker
                .as_ref()
                .and_then(|p| p.selected_commit())
                .map(|c| c.sha.as_str()),
            Some("a1b2c3d4"),
            "Down steps past the default selection to the oldest commit"
        );

        h.type_keys("escape");
        h.settle();
        assert!(h.stoat.commit_picker.is_none(), "Escape closes the picker");
    }

    #[test]
    fn snapshot_commit_picker_open() {
        let mut h = seeded_picker_harness();
        h.assert_snapshot("commit_picker_open");
    }

    /// A harness with a three-commit `/repo`, `main` on its tip and `feature`
    /// one commit back, and an open picker built over that history.
    fn seeded_picker_harness() -> crate::test_harness::TestHarness {
        let mut h = crate::app::Stoat::test();
        h.resize(100, 24);
        h.seed_linear_history(
            "/repo",
            &[
                ("a1b2c3d4", "feat: add a.rs", &[("a.rs", "fn a() {}\n")]),
                (
                    "b2c3d4e5",
                    "chore: tweak a",
                    &[("a.rs", "fn a() {}\nfn a2() {}\n")],
                ),
                (
                    "c3d4e5f6",
                    "feat: add b.rs",
                    &[("a.rs", "fn a() {}\nfn a2() {}\n"), ("b.rs", "fn b() {}\n")],
                ),
            ],
        );
        h.fake_git()
            .add_repo("/repo")
            .branch("main", "c3d4e5f6")
            .branch("feature", "b2c3d4e5");

        let (commits, branch_tips) = {
            let repo = h
                .stoat
                .git_host
                .discover(std::path::Path::new("/repo"))
                .expect("seeded repo");
            let mut branch_tips: HashMap<String, Vec<String>> = HashMap::new();
            for (name, sha) in repo.local_branches() {
                branch_tips.entry(sha).or_default().push(name);
            }
            (repo.log_from("c3d4e5f6", 100), branch_tips)
        };

        let executor = h.stoat.executor.clone();
        let picker = CommitPicker::new(
            h.stoat.active_workspace_mut(),
            executor,
            CommitPickerRole::PickBase,
            PathBuf::from("/repo"),
            "c3d4e5f6".to_string(),
            commits,
            branch_tips,
        );
        h.stoat.commit_picker = Some(picker);
        h.settle();
        h
    }
}

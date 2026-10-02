//! Moving a hunk in or out of the commit the working tree sits on.
//!
//! A walk or a rebase edit stop checks a commit out and points `:diff` at its
//! parent, which makes the commit itself the staged side of every hunk on
//! screen. The keys that cross that line are the same `s` and `u` that drive
//! the git index elsewhere, so this is where they go instead.

use super::review::{HunkStage, StageOutcome};
use crate::{
    app::Stoat, diff_map::line_starts, host::GitRepo, rebase::RebasePause,
    review_apply::base_line_range, review_walk::ReturnRef, workspace::diff::DiffBase,
};
use std::{ops::Range, path::Path};

/// What the transport is allowed to rewrite, read once before any hunk is.
pub(super) struct AmendTarget {
    /// The review base the commit sits over, `None` for a root commit's empty
    /// tree.
    base_sha: Option<String>,
    /// The commit being rewritten, which is what the tree is checked out to.
    head_sha: String,
    /// The branch to carry onto the rewritten commit, `None` when no branch
    /// stands on it.
    ///
    /// A walk checks its commit out detached, so the amend writes HEAD and
    /// leaves the branch on the commit it replaced. `:done` returns by name, so
    /// a branch left behind takes the user back to the commit the amend
    /// replaced and the work is gone.
    branch: Option<String>,
}

/// Where `s` and `u` send a hunk, given what the workspace diffs against.
pub(super) enum AmendRoute {
    /// The workspace diffs against its own HEAD-plus-index, so the keys drive
    /// the index the way they do in any ordinary buffer.
    Index,
    /// The tree sits on a commit the user is free to rewrite.
    Commit(AmendTarget),
    /// No transport fits the installed base. The text is what the staging keys
    /// report.
    Refused(&'static str),
}

/// What every staging key says when [`AmendRoute::Refused`] closes the
/// transport over a commit that is not safe to rewrite, or over an agent
/// proposal, which has no commit. Both funnels that read the route share it,
/// so their wording stays in step.
pub(super) const REFUSED_BADGE: &str =
    "amend needs HEAD on the reviewed commit; use :rebase edit for older commits";

/// What every staging key says under a pair base. A hunk measured against
/// another file fits neither the index nor a commit.
pub(super) const PAIR_REFUSED: &str = "no staging while the diff compares two files";

/// Whether `s` and `u` rewrite the checked-out commit rather than the index.
///
/// The commit is rewritable only where nothing is built on it yet. That means
/// a rebase edit stop, where the stepper replays whatever follows the rewrite,
/// or a walk standing on the tip it started from, where nothing follows at
/// all. An amend anywhere else orphans the commits sitting on top, which is
/// work the user never asked to lose.
pub(super) fn amend_route(stoat: &Stoat, repo: &dyn GitRepo) -> AmendRoute {
    let ws = stoat.active_workspace();
    let base_sha = match ws.diff_base() {
        None | Some(DiffBase::Head) => return AmendRoute::Index,
        Some(DiffBase::Rev { sha }) => sha.clone(),
        // An agent's proposal sits under no commit, so there is nothing to
        // amend it into.
        Some(DiffBase::Memory { .. }) => return AmendRoute::Refused(REFUSED_BADGE),
        // The hunks measure one file against another, which no index or
        // commit holds.
        Some(DiffBase::Pair { .. }) => return AmendRoute::Refused(PAIR_REFUSED),
    };
    let Some(head_sha) = repo.resolve_rev("HEAD") else {
        return AmendRoute::Refused(REFUSED_BADGE);
    };

    let paused = ws
        .rebase_active
        .as_ref()
        .and_then(|active| active.pause.as_ref())
        .is_some_and(|pause| matches!(pause, RebasePause::Edit { .. }));
    // An amend pressed before `:review-done` starts after that press ends the
    // walk. The walk whose return is still out then answers for it.
    let walk_ref = ws
        .review_walk
        .as_ref()
        .or(ws.ending_walk.as_ref())
        .map(|walk| &walk.return_ref);
    let at_tip = match walk_ref {
        Some(ReturnRef::Detached(sha)) => sha == &head_sha,
        Some(ReturnRef::Branch(name)) => repo
            .local_branches()
            .iter()
            .any(|(branch, tip)| branch == name && *tip == head_sha),
        None => false,
    };
    let branch = match (at_tip, walk_ref) {
        (true, Some(ReturnRef::Branch(name))) => Some(name.clone()),
        _ => None,
    };

    match paused || at_tip {
        true => AmendRoute::Commit(AmendTarget {
            base_sha,
            head_sha,
            branch,
        }),
        false => AmendRoute::Refused(REFUSED_BADGE),
    }
}

/// The text `path` holds in the commit once the hunk, run, or line under the
/// cursor crosses the line the commit draws, with the status that names the
/// move.
///
/// Staging amends in. The hunk is a worktree-only edit sitting between the
/// commit and the buffer, and folding it in makes the commit say what the file
/// already says. Unstaging amends out. The hunk is the commit's own, sitting
/// between the base and the commit, and taking it out leaves it on disk as an
/// unstaged change.
///
/// [`HunkStage::Toggle`] has no staged-state signal to read, so it takes
/// whichever side holds a hunk at the cursor. It tries the amend-in direction
/// first, which is the preference the index path shows by trying the forward
/// patch before the reverse one.
///
/// Neither direction writes the working tree, which is what makes an
/// amended-out hunk read as unstaged rather than disappear.
///
/// A cursor on no change returns `Err` with the status that says so.
pub(super) fn amended_file(
    repo: &dyn GitRepo,
    target: &AmendTarget,
    path: &Path,
    buffer_text: &str,
    cursor_row: u32,
    mode: HunkStage,
    unit: AmendUnit,
) -> Result<(String, &'static str), &'static str> {
    // Read from the commit rather than from HEAD. They name the same content
    // while the tree stands here, but the amend rewrites one particular commit,
    // and reading it by sha is what keeps those two from drifting apart.
    let head = repo.content_at(&target.head_sha, path).unwrap_or_default();
    let parent = match &target.base_sha {
        Some(sha) => repo.content_at(sha, path).unwrap_or_default(),
        None => String::new(),
    };

    let amend_in = || amended_content(&head, buffer_text, &head, cursor_row, true, &unit);
    let amend_out = || amended_content(&parent, &head, &head, cursor_row, false, &unit);
    let (into, out_of, nothing) = unit.messages();
    let amended = match mode {
        HunkStage::Stage => amend_in().map(|text| (text, into)),
        HunkStage::Unstage => amend_out().map(|text| (text, out_of)),
        HunkStage::Toggle => amend_in()
            .map(|text| (text, into))
            .or_else(|| amend_out().map(|text| (text, out_of))),
    };
    amended.ok_or(nothing)
}

/// Write `amended` into the commit at `rel`, rewrite the commit, and carry the
/// walk's branch onto the result.
///
/// A walk that stands on a branch carries that branch onto the rewritten
/// commit. The walk reached the commit by detaching HEAD, so the amend writes
/// HEAD alone, and `:done` returns by name.
///
/// The commit is rewritten even when the branch move fails. The status then
/// names the stale branch, not an amend that never happened.
pub(super) fn write_amend(
    repo: &dyn GitRepo,
    target: &AmendTarget,
    rel: &Path,
    amended: String,
    message: &'static str,
) -> StageOutcome {
    // One path, so every other blob in the commit keeps the entry it had.
    let updates = [(rel.to_path_buf(), Some(amended))];
    let tree = match repo.tree_with_updates(&target.head_sha, &updates) {
        Ok(tree) => tree,
        Err(err) => {
            return StageOutcome::Unchanged(format!("could not build the amended tree: {err}"));
        },
    };
    let new_sha = match repo.amend_head(&tree, None) {
        Ok(new_sha) => new_sha,
        Err(err) => return StageOutcome::Unchanged(format!("could not amend: {err}")),
    };

    let status = match &target.branch {
        Some(branch) => match repo.set_branch_target(branch, &new_sha) {
            Ok(()) => message.to_string(),
            Err(err) => format!("amended, but {branch} stayed behind: {err}"),
        },
        None => message.to_string(),
    };
    StageOutcome::Amended {
        old_sha: target.head_sha.clone(),
        new_sha,
        status,
    }
}

/// The content `commit` holds once the change at `cursor_row` in the diff from
/// `from` to `to` moves across the line the commit draws, or `None` when no
/// hunk sits at that row.
///
/// The two directions read different diffs, because the hunk each moves lives
/// between a different pair of texts. Amending in diffs the commit against the
/// buffer and takes the buffer's text. Amending out diffs the base against the
/// commit and takes the base's text. Either way the result lands in `commit`,
/// which is why it is passed separately from the pair being diffed.
///
/// The hunk's lines pair the way
/// [`align_lines`](stoat_language::structural_diff::align_lines) pairs them. A
/// pair that moves takes the side the move goes to, and every other pair keeps
/// the line the commit already holds, so a line amend leaves the rest of its
/// hunk alone. A reindent pairs each moved line with its old self, so an
/// amended line that the reindent added goes in as an insertion rather than
/// over a neighbor.
fn amended_content(
    from: &str,
    to: &str,
    commit: &str,
    cursor_row: u32,
    stage: bool,
    unit: &AmendUnit,
) -> Option<String> {
    let (from_rows, to_rows) = hunk_rows_at(from, to, cursor_row)?;
    let from_lines = row_lines(from, from_rows.clone());
    let to_lines = row_lines(to, to_rows.clone());
    let pairs = stoat_language::structural_diff::align_lines(&from_lines, &to_lines);

    let cursor = cursor_row.saturating_sub(to_rows.start) as usize;
    let moves = |at: usize, to_line: Option<usize>| match unit {
        AmendUnit::Hunk => true,
        // A deletion covers no `to` row for the cursor to pick, so each press
        // moves its first remaining line, which walks it one press at a time.
        AmendUnit::Line if to_lines.is_empty() => at == 0,
        AmendUnit::Line => to_line == Some(cursor),
        AmendUnit::Rows(rows) => {
            to_line.is_some_and(|i| rows.contains(&(to_rows.start + i as u32)))
        },
    };

    let mut region = String::new();
    for (at, &(from_line, to_line)) in pairs.iter().enumerate() {
        let line = match moves(at, to_line) == stage {
            true => to_line.map(|i| to_lines[i]),
            false => from_line.map(|i| from_lines[i]),
        };
        if let Some(line) = line {
            region.push_str(line);
        }
    }

    let rows = match stage {
        true => from_rows,
        false => to_rows,
    };
    let mut amended = commit.to_string();
    amended.replace_range(line_span(commit, rows), &region);
    Some(amended)
}

/// How much of the hunk under the cursor one keypress moves.
#[derive(Clone)]
pub(super) enum AmendUnit {
    /// The whole hunk, which is what `s` and `u` move.
    Hunk,
    /// The cursor's line alone, which is what `S` and `U` move.
    Line,
    /// The live buffer rows of the marked run under the cursor, which is what
    /// `s` and `u` move inside a hunk the tree pass narrowed.
    Rows(Range<u32>),
}

impl AmendUnit {
    /// What the badge says about this unit, as
    /// `(amended in, amended out, nothing under the cursor)`.
    ///
    /// The unit is the whole point of the two key pairs, so it is the word the
    /// user needs back to know which pair they just pressed. A run answers as
    /// a hunk, since the reader pressed the hunk keys.
    fn messages(&self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Hunk | Self::Rows(_) => (
                "amended hunk into the commit",
                "amended hunk out of the commit",
                "no hunk under the cursor",
            ),
            Self::Line => (
                "amended line into the commit",
                "amended line out of the commit",
                "no line change under the cursor",
            ),
        }
    }
}

/// The line ranges the hunk covering `cursor_row` occupies on each side of the
/// diff from `from` to `to`, as `(from rows, to rows)`.
///
/// The row is a `to`-side row, since that is the text on screen. A hunk that
/// deletes covers no `to` row, so it is found by the anchor the gutter marks it
/// at rather than by containment. Its `to` rows are empty and anchored where
/// the deletion sat, which is where the gutter marks it.
///
/// The `from` rows come from the line ranges rather than from the hunk's raw
/// base bytes. A hunk records which base bytes it replaces but not which base
/// line they start on, so the two sides are only aligned once that anchor is
/// derived, which is what [`base_line_range`] does.
fn hunk_rows_at(from: &str, to: &str, cursor_row: u32) -> Option<(Range<u32>, Range<u32>)> {
    let result = stoat_language::structural_diff::diff(from, to);
    let hunks = crate::diff_map::changes_to_hunks(&result.changes, from, to);
    let k = hunks.iter().position(|hunk| {
        let rows = &hunk.buffer_line_range;
        match rows.is_empty() {
            true => rows.start == cursor_row,
            false => rows.contains(&cursor_row),
        }
    })?;

    Some((
        base_line_range(from, &hunks, k),
        hunks[k].buffer_line_range.clone(),
    ))
}

/// The lines `rows` covers in `text`, each with its terminator.
fn row_lines(text: &str, rows: Range<u32>) -> Vec<&str> {
    let starts = line_starts(text);
    let byte_at = |row: u32| starts.get(row as usize).copied().unwrap_or(text.len());
    rows.map(|row| &text[byte_at(row)..byte_at(row + 1)])
        .collect()
}

/// The bytes `rows` covers in `text`, from the start of the first row to the
/// start of the row after the last.
fn line_span(text: &str, rows: Range<u32>) -> Range<usize> {
    let starts = line_starts(text);
    let byte_at = |row: u32| starts.get(row as usize).copied().unwrap_or(text.len());
    byte_at(rows.start)..byte_at(rows.end)
}

/// Move the walk off `old_sha` and onto `new_sha`, so stepping on and returning
/// stay anchored to the commit the amend just replaced.
///
/// This finds the commit by `old_sha`, not at the cursor. A step pressed while
/// the amend waits in the git queue moves the cursor before the amend lands.
///
/// A detached return ref names the commit by sha, so it moves too when it named
/// the rewritten one. Left alone it reports a tip the amended commit no longer
/// sits on, which reads as a walk standing below the tip and refuses every hunk
/// after the first. A branch return ref needs nothing: the amend writes the ref
/// HEAD is on, so the branch already names the new commit.
///
/// An ended walk whose return still waits in the git queue moves too. That
/// return checks out what its ref names at its turn, which is then the amended
/// commit.
pub(super) fn anchor_walk_to(stoat: &mut Stoat, old_sha: &str, new_sha: &str) {
    let ws = stoat.active_workspace_mut();
    for walk in [ws.review_walk.as_mut(), ws.ending_walk.as_mut()]
        .into_iter()
        .flatten()
    {
        if let Some(commit) = walk.commits.iter_mut().find(|commit| commit.sha == old_sha) {
            commit.sha = new_sha.to_string();
            commit.short_sha = new_sha.chars().take(7).collect();
        }
        if matches!(&walk.return_ref, ReturnRef::Detached(sha) if sha == old_sha) {
            walk.return_ref = ReturnRef::Detached(new_sha.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DiffBase, ReturnRef};
    use crate::{
        action_handlers::review::BASE_MOVED,
        git_jobs,
        rebase::{ActiveRebase, RebasePause},
        test_harness::{CommitSpec, TestHarness},
    };
    use std::{
        collections::{HashMap, VecDeque},
        path::{Path, PathBuf},
        sync::Arc,
    };

    /// A repo whose tip commit rewrites one line, walked onto so the tree,
    /// HEAD, and the diff base are all where the transport needs them.
    ///
    /// The walk stands on the tip. Nothing is built on the commit there, so
    /// rewriting it orphans nothing, which is what makes an amend safe.
    fn walking_the_tip() -> TestHarness {
        walking_the_tip_of("a\nb\nc\n", "a\nb\nX\n")
    }

    /// The same walk over a commit that takes `a.rs` from `base` to `tip`, so a
    /// test needing more than one hunk writes the pair it wants.
    fn walking_the_tip_of(base: &str, tip: &str) -> TestHarness {
        walking_history(
            &[
                ("c1", "root", &[("a.rs", base)]),
                ("c2", "change c", &[("a.rs", tip)]),
            ],
            "c2",
            tip,
        )
    }

    /// A walk over a repo with one commit and no parent, whose review base is
    /// therefore the empty tree rather than a sha.
    fn walking_a_root_commit(text: &str) -> TestHarness {
        walking_history(&[("c1", "root", &[("a.rs", text)])], "c1", text)
    }

    /// Seed `commits`, put the branch and the working tree on `tip_sha`, then
    /// walk onto it.
    fn walking_history(commits: &[CommitSpec<'_>], tip_sha: &str, tip_text: &str) -> TestHarness {
        let mut h = TestHarness::with_size(80, 14);
        h.seed_linear_history("/repo", commits);
        h.fake_git()
            .add_repo("/repo")
            .branch("main", tip_sha)
            .set_head_branch("main")
            .head_file("a.rs", tip_text);
        h.stoat.active_workspace_mut().git_root = "/repo".into();
        h.fake_fs().insert_file("/repo/a.rs", tip_text.as_bytes());

        let commit = {
            let repo = h
                .stoat
                .git_host
                .discover(&PathBuf::from("/repo"))
                .expect("repo");
            repo.log_from(tip_sha, 1)
                .into_iter()
                .next()
                .expect("the tip commit")
        };
        super::super::review_walk::walk_one_commit(&mut h.stoat, PathBuf::from("/repo"), commit);
        h.settle();
        h
    }

    fn cursor_to(h: &mut TestHarness, row: u32) {
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, row);
    }

    /// The content the tip commit now holds for `a.rs`, after however many
    /// amends have replaced it.
    fn committed(h: &TestHarness) -> Option<String> {
        let repo = h.stoat.git_host.discover(&PathBuf::from("/repo"))?;
        let head = repo.resolve_rev("HEAD")?;
        repo.commit_tree(&head)?
            .get(&PathBuf::from("a.rs"))
            .cloned()
    }

    fn buffer_text(h: &mut TestHarness) -> String {
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        editor
            .display_map
            .snapshot()
            .buffer_snapshot()
            .rope()
            .to_string()
    }

    /// Unstaging takes the hunk out of the commit and leaves it on disk, which
    /// is what turns a change the commit owned into an unstaged one.
    #[test]
    fn unstage_amends_the_hunk_out_and_leaves_the_file() {
        let mut h = walking_the_tip();
        cursor_to(&mut h, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageHunk);
        h.settle();

        let text = buffer_text(&mut h);
        assert_eq!(
            (committed(&h), text),
            (Some("a\nb\nc\n".to_string()), "a\nb\nX\n".to_string()),
            "the commit gave the line back and the file kept it",
        );
    }

    /// The walk follows the commit it just rewrote. An amend replaces the
    /// commit, so a walk still holding the old sha would step on from, and
    /// return to, something that no longer exists.
    #[test]
    fn an_amend_re_anchors_the_walk() {
        let mut h = walking_the_tip();
        cursor_to(&mut h, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageHunk);
        h.settle();

        let repo = h
            .stoat
            .git_host
            .discover(&PathBuf::from("/repo"))
            .expect("repo");
        let walk = h
            .stoat
            .active_workspace()
            .review_walk
            .as_ref()
            .expect("walk");
        let head = repo.resolve_rev("HEAD").expect("HEAD");
        assert_eq!(
            (
                walk.current().sha.as_str(),
                walk.current().short_sha.as_str()
            ),
            (head.as_str(), &head[..7]),
            "the walk names the commit the amend left behind, badge included",
        );
    }

    /// A step pressed while an amend waits its turn moves the cursor before the
    /// amend lands. The landing still re-anchors the commit it rewrote, not the
    /// one the reader stepped to.
    #[test]
    fn an_amend_re_anchors_its_commit_after_the_cursor_steps_off() {
        let mut h = walking_the_tip();
        {
            let repo = h
                .stoat
                .git_host
                .discover(&PathBuf::from("/repo"))
                .expect("repo");
            let root = repo.log_from("c1", 1).into_iter().next().expect("c1");
            let walk = h
                .stoat
                .active_workspace_mut()
                .review_walk
                .as_mut()
                .expect("walk");
            walk.commits.insert(0, root);
            walk.cursor = 1;
        }
        cursor_to(&mut h, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageHunk);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewPrevCommit);
        h.settle();

        let main = h
            .stoat
            .git_host
            .discover(&PathBuf::from("/repo"))
            .and_then(|repo| repo.resolve_rev("main"))
            .expect("main");
        let walked: Vec<String> = h
            .stoat
            .active_workspace()
            .review_walk
            .as_ref()
            .expect("walk")
            .commits
            .iter()
            .map(|commit| commit.sha.clone())
            .collect();
        assert_eq!(
            (walked, main == "c2"),
            (vec!["c1".to_string(), main.clone()], false),
            "the amended sha replaced c2, and c1 kept its own",
        );
    }

    /// A root commit has no parent to read, so the base is the empty tree.
    /// Amending its content out empties the file rather than restoring text no
    /// earlier commit ever held.
    #[test]
    fn a_root_commit_amends_against_the_empty_tree() {
        let mut h = walking_a_root_commit("a\nb\nc\n");
        cursor_to(&mut h, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageHunk);
        h.settle();

        assert_eq!(
            committed(&h).as_deref(),
            Some(""),
            "the commit gave back everything it introduced",
        );
    }

    /// A walk standing below the tip refuses, because commits sit on top of the
    /// one under the cursor and an amend would orphan them.
    #[test]
    fn a_walk_below_the_tip_refuses_to_amend() {
        let mut h = walking_the_tip();
        // Point the walk's return ref at a commit HEAD is not on, which is the
        // shape of a walk that has more history ahead of it.
        h.stoat
            .active_workspace_mut()
            .review_walk
            .as_mut()
            .expect("walk")
            .return_ref = ReturnRef::Detached("c1".to_string());
        cursor_to(&mut h, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageHunk);
        h.settle();

        assert_eq!(
            committed(&h).as_deref(),
            Some("a\nb\nX\n"),
            "the commit was left exactly as it was",
        );
    }

    /// A rebase edit stop amends whatever the walk position says, because the
    /// stepper replays every commit that follows the rewrite.
    #[test]
    fn a_rebase_edit_stop_amends_below_the_tip() {
        let mut h = walking_the_tip();
        {
            let ws = h.stoat.active_workspace_mut();
            ws.review_walk.as_mut().expect("walk").return_ref =
                ReturnRef::Detached("c1".to_string());
            ws.rebase_active = Some(ActiveRebase {
                workdir: PathBuf::from("/repo"),
                onto: "c1".to_string(),
                remaining: VecDeque::new(),
                current_head: "c2".to_string(),
                last_pick_sha: Some("c2".to_string()),
                last_message: None,
                pause: Some(RebasePause::Edit {
                    cherry_picked_commit: "c2".to_string(),
                }),
            });
        }
        cursor_to(&mut h, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageHunk);
        h.settle();

        assert_eq!(
            committed(&h).as_deref(),
            Some("a\nb\nc\n"),
            "the stop allowed the amend the walk position alone would refuse",
        );
    }

    /// The branch the walk returns to follows the amend even when the walk
    /// detached HEAD to get there, which is what stepping onto a commit does.
    /// The amend writes HEAD alone from there, so a branch left behind sends
    /// `:done` back to the commit the amend replaced and the work is gone.
    #[test]
    fn the_branch_follows_an_amend_made_on_a_detached_head() {
        let mut h = walking_the_tip();
        {
            let repo = h
                .stoat
                .git_host
                .discover(&PathBuf::from("/repo"))
                .expect("repo");
            repo.checkout_detached("c2").expect("detach onto the tip");
            assert_eq!(repo.head_branch(), None, "the walk left the branch");
        }
        cursor_to(&mut h, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageHunk);
        h.settle();

        let repo = h
            .stoat
            .git_host
            .discover(&PathBuf::from("/repo"))
            .expect("repo");
        assert_eq!(
            repo.local_branches(),
            vec![("main".to_string(), repo.resolve_rev("HEAD").expect("HEAD"))],
            "main names the amended commit, so returning to it keeps the amend",
        );
    }

    /// A commit is amended one hunk at a time, so the transport has to survive
    /// its own first use. The amend replaces the commit the branch points at,
    /// and a branch left on the old sha reads as work sitting on top of HEAD.
    #[test]
    fn a_second_amend_follows_the_first() {
        let mut h = walking_the_tip_of("a\nb\nc\nd\ne\n", "a\nb\nX\nd\nY\n");

        for row in [2, 4] {
            cursor_to(&mut h, row);
            crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageHunk);
            h.settle();
        }

        assert_eq!(
            committed(&h).as_deref(),
            Some("a\nb\nc\nd\ne\n"),
            "both hunks came out, so the second amend ran as well as the first",
        );
    }

    /// A detached walk amends more than once too. Its return ref names the
    /// commit by sha rather than through a branch, so an amend that leaves that
    /// sha alone makes every later hunk read as a walk standing below the tip.
    #[test]
    fn a_detached_walk_amends_more_than_once() {
        let mut h = walking_the_tip_of("a\nb\nc\nd\ne\n", "a\nb\nX\nd\nY\n");
        h.stoat
            .active_workspace_mut()
            .review_walk
            .as_mut()
            .expect("walk")
            .return_ref = ReturnRef::Detached("c2".to_string());

        for row in [2, 4] {
            cursor_to(&mut h, row);
            crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageHunk);
            h.settle();
        }

        assert_eq!(
            committed(&h).as_deref(),
            Some("a\nb\nc\nd\ne\n"),
            "the return ref followed the amend, so the tip check kept passing",
        );
    }

    /// A `:review-done` pressed while an amend waits in the git queue returns
    /// to the commit the amend wrote, not to the one it replaced.
    ///
    /// The idle job holds the amend in the queue until after the press. Without
    /// it, the test scheduler runs the amend's work inline at the unstage press.
    #[test]
    fn a_return_behind_an_amend_lands_on_the_amended_commit() {
        let mut h = walking_the_tip();
        h.stoat
            .active_workspace_mut()
            .review_walk
            .as_mut()
            .expect("walk")
            .return_ref = ReturnRef::Detached("c2".to_string());
        cursor_to(&mut h, 2);
        git_jobs::enqueue(&mut h.stoat, git_jobs::idle_job(None));

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageHunk);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
        h.settle();

        let amends = h.fake_git().amend_history(Path::new("/repo"));
        assert_eq!(
            (
                amends.len(),
                h.fake_git().checkouts(Path::new("/repo")).last().cloned()
            ),
            (
                1,
                amends
                    .first()
                    .map(|amend| format!("detached:{}", amend.new_head))
            ),
            "the return checked out the commit the amend wrote",
        );
    }

    /// A row the commit changed and the buffer changed again holds a hunk on
    /// both sides at once. Toggle takes the amend-in side there, which is the
    /// tie the preference exists to settle.
    #[test]
    fn toggle_prefers_amending_in_when_both_sides_hold_a_hunk() {
        let mut h = walking_the_tip();
        h.seed_focused_buffer("a\nb\nQ\n");
        cursor_to(&mut h, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
        h.settle();

        assert_eq!(
            committed(&h).as_deref(),
            Some("a\nb\nQ\n"),
            "the buffer's edit went in rather than the commit's own hunk coming out",
        );
    }

    /// An agent's proposal sits under no commit, so there is nothing for the
    /// keys to amend into and the transport refuses rather than falling back to
    /// the index.
    #[test]
    fn a_memory_base_refuses_to_amend() {
        let mut h = walking_the_tip();
        {
            let files = HashMap::from([(
                PathBuf::from("/repo/a.rs"),
                Arc::new("a\nb\nc\n".to_string()),
            )]);
            h.stoat
                .active_workspace_mut()
                .set_diff_base(Some(DiffBase::Memory { files }));
        }
        cursor_to(&mut h, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageHunk);
        h.settle();

        assert_eq!(
            (committed(&h).as_deref(), h.stoat.pending_message.as_deref()),
            (
                Some("a\nb\nX\n"),
                Some("amend needs HEAD on the reviewed commit; use :rebase edit for older commits"),
            ),
            "the commit was left alone and the badge named the missing transport",
        );
    }

    /// A pair's hunks measure one file against another, which no index or
    /// commit holds, so the transport refuses with its own reason.
    #[test]
    fn a_pair_base_refuses_to_stage() {
        let mut h = walking_the_tip();
        h.stoat
            .active_workspace_mut()
            .set_diff_base(Some(DiffBase::Pair {
                path: "/repo/a.rs".into(),
                base_path: "/repo/other.rs".into(),
                text: Arc::new("a\nb\nc\n".to_string()),
                base_version: 0,
            }));
        cursor_to(&mut h, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageHunk);
        h.settle();

        assert_eq!(
            (committed(&h).as_deref(), h.stoat.pending_message.as_deref()),
            (
                Some("a\nb\nX\n"),
                Some("no staging while the diff compares two files")
            ),
        );
    }

    /// A press waits its turn in the git queue. A base that moves before that
    /// turn leaves the captured text and row describing another checkout, so
    /// the press refuses rather than amend the wrong commit.
    #[test]
    fn a_press_whose_review_base_moved_before_its_turn_refuses() {
        let mut h = walking_the_tip();
        cursor_to(&mut h, 2);
        git_jobs::enqueue(&mut h.stoat, git_jobs::idle_job(None));

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageHunk);
        h.stoat
            .active_workspace_mut()
            .set_diff_base(Some(DiffBase::Rev {
                sha: Some("c2".to_string()),
            }));
        h.settle();

        assert_eq!(
            (committed(&h).as_deref(), h.stoat.pending_message.as_deref()),
            (Some("a\nb\nX\n"), Some(BASE_MOVED)),
            "the commit was left alone and the badge asked for another press",
        );
    }

    /// Toggle has no staged-state signal to read, so it takes whichever side
    /// holds a hunk. A worktree-only edit is the amend-in side, and it wins the
    /// tie the same way the index path prefers staging.
    #[test]
    fn toggle_amends_an_edit_into_the_commit() {
        let mut h = walking_the_tip();
        h.seed_focused_buffer("Z\na\nb\nX\n");
        cursor_to(&mut h, 0);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
        h.settle();

        assert_eq!(
            committed(&h).as_deref(),
            Some("Z\na\nb\nX\n"),
            "toggle found the edit and folded it in",
        );
    }

    /// With no worktree edit at the cursor, toggle falls through to the
    /// commit's own hunk and takes it out.
    #[test]
    fn toggle_amends_the_commits_own_hunk_out() {
        let mut h = walking_the_tip();
        cursor_to(&mut h, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
        h.settle();

        assert_eq!(
            committed(&h).as_deref(),
            Some("a\nb\nc\n"),
            "toggle fell through to the amend-out side",
        );
    }

    /// Staging folds a worktree-only edit into the commit, so the commit says
    /// what the file already said.
    #[test]
    fn stage_amends_the_edit_into_the_commit() {
        let mut h = walking_the_tip();
        h.seed_focused_buffer("Z\na\nb\nX\n");
        cursor_to(&mut h, 0);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageHunk);
        h.settle();

        assert_eq!(
            committed(&h).as_deref(),
            Some("Z\na\nb\nX\n"),
            "the commit took the edit the buffer already carried",
        );
    }

    /// `S` moves the cursor's line alone. The rest of the hunk is a separate
    /// decision the user has not made yet, so it stays where it was.
    #[test]
    fn stage_line_amends_one_line_of_a_longer_hunk() {
        let mut h = walking_the_tip_of("a\nb\nc\nd\n", "a\nX\nY\nd\n");
        h.seed_focused_buffer("a\nP\nQ\nd\n");
        cursor_to(&mut h, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageLine);
        h.settle();

        assert_eq!(
            committed(&h).as_deref(),
            Some("a\nP\nY\nd\n"),
            "row 1 went in and row 2 kept the commit's own text",
        );
    }

    /// `U` is the same narrowing in the other direction, over the diff between
    /// the base and the commit rather than between the commit and the buffer.
    #[test]
    fn unstage_line_amends_one_line_of_a_longer_hunk() {
        let mut h = walking_the_tip_of("a\nb\nc\nd\n", "a\nX\nY\nd\n");
        cursor_to(&mut h, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageLine);
        h.settle();

        assert_eq!(
            committed(&h).as_deref(),
            Some("a\nb\nY\nd\n"),
            "row 1 came out and row 2 stayed in the commit",
        );
    }

    /// The amend stales the buffer's diff map. The map on screen was computed
    /// against the commit the amend replaced, so its gutter keeps marking a
    /// hunk the commit no longer carries until something recomputes it.
    #[test]
    fn an_amend_stales_the_diff_map() {
        let mut h = walking_the_tip();
        h.seed_current_diff_map("a\nb\nc\n");
        cursor_to(&mut h, 2);
        let buffer_id = h.stoat.focused_editor_ids().expect("editor").1;
        assert!(
            h.stoat.active_workspace().diff_map_current(buffer_id),
            "the map starts current, so staling it is the amend's doing",
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageHunk);
        h.settle();

        assert!(
            !h.stoat.active_workspace().diff_map_current(buffer_id),
            "the map was computed against a commit that no longer exists",
        );
    }

    /// The cursor's offset into the hunk is what picks its counterpart. On the
    /// second row of a two-row hunk the base's second line is what comes back,
    /// not the first one the hunk happens to start at.
    #[test]
    fn unstage_line_takes_the_counterpart_at_the_cursors_offset() {
        let mut h = walking_the_tip_of("a\nb\nc\nd\n", "a\nX\nY\nd\n");
        cursor_to(&mut h, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageLine);
        h.settle();

        assert_eq!(
            committed(&h).as_deref(),
            Some("a\nX\nc\nd\n"),
            "row 2 came back as c, the base line paired with it",
        );
    }

    /// Amending in reads the offset the same way, against the buffer rather
    /// than the base.
    #[test]
    fn stage_line_replaces_the_commit_line_at_the_cursors_offset() {
        let mut h = walking_the_tip_of("a\nb\nc\nd\n", "a\nX\nY\nd\n");
        h.seed_focused_buffer("a\nP\nQ\nd\n");
        cursor_to(&mut h, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageLine);
        h.settle();

        assert_eq!(
            committed(&h).as_deref(),
            Some("a\nX\nQ\nd\n"),
            "Q replaced Y, the commit line paired with it",
        );
    }

    /// A cursor past the end of the hunk's other side sits on a purely added
    /// line, which replaces nothing. The line is inserted where it lands, and
    /// its neighbour addition is left for a second press.
    #[test]
    fn stage_line_on_an_added_line_replaces_nothing() {
        let mut h = walking_the_tip_of("a\nb\nc\nd\n", "a\nX\nY\nd\n");
        h.seed_focused_buffer("a\nX\nY\nN1\nN2\nd\n");
        cursor_to(&mut h, 4);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageLine);
        h.settle();

        assert_eq!(
            committed(&h).as_deref(),
            Some("a\nX\nY\nN2\nd\n"),
            "the second added line went in on its own",
        );
    }

    /// A line that the buffer's reindent added goes into the commit as an
    /// insertion. The reindented lines pair with their old selves, so none of
    /// them is the commit line it replaces.
    #[test]
    fn stage_line_on_a_line_a_reindent_added_inserts_it() {
        let mut h = walking_the_tip_of(
            "fn f() {\n}\n",
            "fn f() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n}\n",
        );
        h.seed_focused_buffer(
            "fn f() {\n    if x {\n        let a = 1;\n        let b = 2;\n        let c = 3;\n    }\n}\n",
        );
        cursor_to(&mut h, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageLine);
        h.settle();

        assert_eq!(
            committed(&h).as_deref(),
            Some("fn f() {\n    if x {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n}\n"),
        );
    }

    /// Inside a hunk the tree pass narrowed, `s` amends only the marked run
    /// under the cursor. The buffer's `c = 4` sits in a second run, so it stays
    /// out of the commit with the reindent.
    #[test]
    fn stage_hunk_amends_only_the_run_under_the_cursor() {
        let mut h = walking_the_tip_of(
            "fn f() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n}\nfn g() {}\n",
            "fn f() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n}\nfn g() { 1 }\n",
        );
        h.stoat.set_diff_warm_auto(true);
        h.seed_focused_buffer(
            "fn f() {\n    if x {\n        let a = 1;\n        let b = 2;\n        let c = 4;\n    }\n}\nfn g() { 1 }\n",
        );
        h.settle_diff_jobs();
        cursor_to(&mut h, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageHunk);
        h.settle();

        assert_eq!(
            committed(&h).as_deref(),
            Some("fn f() {\n    if x {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n}\nfn g() { 1 }\n"),
        );
    }

    /// A deleted line ahead of the cursor's row shifts the pairs, so the cursor
    /// picks the pair that holds its own row and not the pair at its offset.
    #[test]
    fn stage_line_moves_the_pair_on_the_cursor_row() {
        let mut h = walking_the_tip_of("x1\na\n", "x1\nx2\na\n");
        h.seed_focused_buffer("y\n  a\n");
        cursor_to(&mut h, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageLine);
        h.settle();

        assert_eq!(committed(&h).as_deref(), Some("x1\nx2\n  a\n"));
    }

    /// A deletion covers no row for the cursor to pick, so each `S` moves its
    /// first remaining line, which walks the deletion one press at a time.
    #[test]
    fn stage_line_on_a_deletion_moves_its_first_line() {
        let mut h = walking_the_tip_of("a\nb\nc\nd\ne\n", "a\nb\nc\nd\n");
        h.seed_focused_buffer("a\nd\n");
        cursor_to(&mut h, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageLine);
        h.settle();

        assert_eq!(committed(&h).as_deref(), Some("a\nc\nd\n"));
    }

    /// Repeating walks the hunk a line at a time, which is what makes the
    /// narrowing useful rather than a one-shot.
    #[test]
    fn two_line_amends_empty_the_hunk() {
        let mut h = walking_the_tip_of("a\nb\nc\nd\n", "a\nX\nY\nd\n");

        for row in [1, 2] {
            cursor_to(&mut h, row);
            crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageLine);
            h.settle();
        }

        assert_eq!(
            committed(&h).as_deref(),
            Some("a\nb\nc\nd\n"),
            "both lines came out one press at a time",
        );
    }
}

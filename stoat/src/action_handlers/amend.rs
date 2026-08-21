//! Moving a hunk in or out of the commit the working tree sits on.
//!
//! A walk or a rebase edit stop checks a commit out and points `:diff` at its
//! parent, which makes the commit itself the staged side of every hunk on
//! screen. The keys that cross that line are the same `s` and `u` that drive
//! the git index elsewhere, so this is where they go instead.

use super::review::HunkStage;
use crate::{
    app::{Stoat, UpdateEffect},
    buffer::BufferId,
    diff_map::line_starts,
    host::GitRepo,
    rebase::RebasePause,
    review_apply::base_line_range,
    review_walk::ReturnRef,
    workspace::diff::DiffBase,
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
    /// A base is installed, but rewriting the commit under it would destroy
    /// work.
    Refused,
}

/// What every staging key says when [`AmendRoute::Refused`] closes the
/// transport. Both funnels that read the route share it, so their wording
/// stays in step.
pub(super) const REFUSED_BADGE: &str =
    "amend needs HEAD on the reviewed commit; use :rebase edit for older commits";

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
        None => return AmendRoute::Index,
        Some(DiffBase::Rev { sha }) => sha.clone(),
        // An agent's proposal sits under no commit, so there is nothing to
        // amend it into.
        Some(DiffBase::Memory { .. }) => return AmendRoute::Refused,
    };
    let Some(head_sha) = repo.resolve_rev("HEAD") else {
        return AmendRoute::Refused;
    };

    let paused = ws
        .rebase_active
        .as_ref()
        .and_then(|active| active.pause.as_ref())
        .is_some_and(|pause| matches!(pause, RebasePause::Edit { .. }));
    let walk_ref = ws.review_walk.as_ref().map(|walk| &walk.return_ref);
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
        false => AmendRoute::Refused,
    }
}

/// The hunk to move, named by where the cursor sits rather than by an index.
///
/// The buffer text travels with it because the hunk is resolved against the
/// live buffer, which is not what the file on disk holds once the user edits.
pub(super) struct HunkSite<'a> {
    pub(super) buffer_id: BufferId,
    pub(super) path: &'a Path,
    pub(super) cursor_row: u32,
    pub(super) buffer_text: &'a str,
}

/// Fold the hunk under the cursor into the commit, or take it back out.
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
/// A walk that stands on a branch carries that branch onto the rewritten
/// commit. The walk reached the commit by detaching HEAD, so the amend writes
/// HEAD alone, and `:done` returns by name.
pub(super) fn amend_hunk(
    stoat: &mut Stoat,
    repo: &dyn GitRepo,
    target: &AmendTarget,
    mode: HunkStage,
    unit: AmendUnit,
    site: HunkSite<'_>,
) -> UpdateEffect {
    let HunkSite {
        buffer_id,
        path,
        cursor_row,
        buffer_text,
    } = site;
    // Read from the commit rather than from HEAD. They name the same content
    // while the tree stands here, but the amend rewrites one particular commit,
    // and reading it by sha is what keeps those two from drifting apart.
    let head = repo.content_at(&target.head_sha, path).unwrap_or_default();
    let parent = match &target.base_sha {
        Some(sha) => repo.content_at(sha, path).unwrap_or_default(),
        None => String::new(),
    };

    let amend_in = || amended_content(&head, buffer_text, &head, cursor_row, true, unit);
    let amend_out = || amended_content(&parent, &head, &head, cursor_row, false, unit);
    let (into, out_of, nothing) = unit.messages();
    let amended_and_message = match mode {
        HunkStage::Stage => amend_in().map(|text| (text, into)),
        HunkStage::Unstage => amend_out().map(|text| (text, out_of)),
        HunkStage::Toggle => amend_in()
            .map(|text| (text, into))
            .or_else(|| amend_out().map(|text| (text, out_of))),
    };

    let Some((amended, message)) = amended_and_message else {
        stoat.set_status(nothing);
        return UpdateEffect::Redraw;
    };

    let Some(mut tree) = repo.commit_tree(&target.head_sha) else {
        stoat.set_status("could not read the commit's tree");
        return UpdateEffect::Redraw;
    };
    let git_root = stoat.active_workspace().git_root.clone();
    let rel = path.strip_prefix(&git_root).unwrap_or(path).to_path_buf();
    tree.insert(rel, amended);

    let new_sha = match repo.amend_head(&tree, None) {
        Ok(new_sha) => new_sha,
        Err(err) => {
            stoat.set_status(format!("could not amend: {err}"));
            return UpdateEffect::Redraw;
        },
    };

    anchor_walk_to(stoat, &target.head_sha, &new_sha);
    stoat.active_workspace_mut().invalidate_diff(buffer_id);

    // Set first so a failed branch move overwrites it. The commit is rewritten
    // either way, so the badge has to name a stale branch rather than an amend
    // that never happened.
    stoat.set_status(message);
    if let Some(branch) = &target.branch
        && let Err(err) = repo.set_branch_target(branch, &new_sha)
    {
        stoat.set_status(format!("amended, but {branch} stayed behind: {err}"));
    }
    UpdateEffect::Redraw
}

/// The content `commit` holds once the hunk at `cursor_row` in the diff from
/// `from` to `to` moves across the line the commit draws, or `None` when no
/// hunk sits at that row.
///
/// The two directions read different diffs, because the hunk each moves lives
/// between a different pair of texts. Amending in diffs the commit against the
/// buffer and takes the buffer's text. Amending out diffs the base against the
/// commit and takes the base's text. Either way the splice lands in `commit`,
/// which is why it is passed separately from the pair being diffed.
fn amended_content(
    from: &str,
    to: &str,
    commit: &str,
    cursor_row: u32,
    stage: bool,
    unit: AmendUnit,
) -> Option<String> {
    let (from_span, to_span) = match unit {
        AmendUnit::Hunk => hunk_spans_at(from, to, cursor_row)?,
        AmendUnit::Line => line_spans_at(from, to, cursor_row)?,
    };
    Some(match stage {
        true => splice(commit, from_span, to, to_span),
        false => splice(commit, to_span, from, from_span),
    })
}

/// How much of the hunk under the cursor one keypress moves.
#[derive(Clone, Copy)]
pub(super) enum AmendUnit {
    /// The whole hunk, which is what `s` and `u` move.
    Hunk,
    /// The cursor's line alone, which is what `S` and `U` move.
    Line,
}

impl AmendUnit {
    /// What the badge says about this unit, as
    /// `(amended in, amended out, nothing under the cursor)`.
    ///
    /// The unit is the whole point of the two key pairs, so it is the word the
    /// user needs back to know which pair they just pressed.
    fn messages(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Hunk => (
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

/// The byte spans a hunk covering `cursor_row` occupies on each side of the
/// diff from `from` to `to`, as `(from span, to span)`.
///
/// Both spans are whole lines, covering everything the hunk moves. A hunk that
/// deletes leaves an empty `to` span, anchored where the deletion sat, which is
/// where the gutter marks it.
///
/// See also:
/// - [`hunk_rows_at`] for how the hunk is found and its two sides aligned.
/// - [`line_spans_at`] for the same spans narrowed to one line.
fn hunk_spans_at(from: &str, to: &str, cursor_row: u32) -> Option<(Range<usize>, Range<usize>)> {
    let (from_rows, to_rows) = hunk_rows_at(from, to, cursor_row)?;
    Some((line_span(from, from_rows), line_span(to, to_rows)))
}

/// The line ranges the hunk covering `cursor_row` occupies on each side of the
/// diff from `from` to `to`, as `(from rows, to rows)`.
///
/// The row is a `to`-side row, since that is the text on screen. A hunk that
/// deletes covers no `to` row, so it is found by the anchor the gutter marks it
/// at rather than by containment.
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

/// The byte spans the cursor's single line occupies on each side of the diff
/// from `from` to `to`, as `(from span, to span)`.
///
/// The line-granularity counterpart to [`hunk_spans_at`], which is where the
/// hunk under the cursor is found. Only the narrowing lives here.
///
/// A hunk that replaces three lines with five has no arithmetic mapping from a
/// `to` row back to a `from` row, so the pairing is positional inside the hunk,
/// the way `hunk_rows` in [`crate::review_apply`] pairs the rows it emits. The
/// cursor's offset into the hunk is its offset into both sides, and a row past
/// the shorter side is one-sided.
///
/// A cursor on a purely added line therefore has no counterpart, and its `from`
/// span comes back empty, anchored where the line lands. A deletion hunk covers
/// no `to` row at all, so its offset is zero and each call moves the first
/// remaining `from` line, which walks the deletion one press at a time.
fn line_spans_at(from: &str, to: &str, cursor_row: u32) -> Option<(Range<usize>, Range<usize>)> {
    let (from_rows, to_rows) = hunk_rows_at(from, to, cursor_row)?;

    let offset = cursor_row.saturating_sub(to_rows.start);
    let from_row = from_rows.start + offset;
    let from_line = match from_row < from_rows.end {
        true => from_row..from_row + 1,
        false => from_rows.end..from_rows.end,
    };
    let to_line = match to_rows.is_empty() {
        true => to_rows.clone(),
        false => cursor_row..cursor_row + 1,
    };

    Some((line_span(from, from_line), line_span(to, to_line)))
}

/// The bytes `rows` covers in `text`, from the start of the first row to the
/// start of the row after the last.
fn line_span(text: &str, rows: Range<u32>) -> Range<usize> {
    let starts = line_starts(text);
    let byte_at = |row: u32| starts.get(row as usize).copied().unwrap_or(text.len());
    byte_at(rows.start)..byte_at(rows.end)
}

/// `target` with `span` replaced by `source[source_span]`.
fn splice(target: &str, span: Range<usize>, source: &str, source_span: Range<usize>) -> String {
    let mut out = target.to_string();
    out.replace_range(span, &source[source_span]);
    out
}

/// Move the walk off `old_sha` and onto `new_sha`, so stepping on and returning
/// stay anchored to the commit the amend just replaced.
///
/// A detached return ref names the commit by sha, so it moves too when it named
/// the rewritten one. Left alone it reports a tip the amended commit no longer
/// sits on, which reads as a walk standing below the tip and refuses every hunk
/// after the first. A branch return ref needs nothing: the amend writes the ref
/// HEAD is on, so the branch already names the new commit.
fn anchor_walk_to(stoat: &mut Stoat, old_sha: &str, new_sha: &str) {
    let Some(walk) = stoat.active_workspace_mut().review_walk.as_mut() else {
        return;
    };
    let cursor = walk.cursor;
    if let Some(commit) = walk.commits.get_mut(cursor) {
        commit.sha = new_sha.to_string();
        commit.short_sha = new_sha.chars().take(7).collect();
    }
    if matches!(&walk.return_ref, ReturnRef::Detached(sha) if sha == old_sha) {
        walk.return_ref = ReturnRef::Detached(new_sha.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::{DiffBase, ReturnRef};
    use crate::{
        rebase::{ActiveRebase, RebasePause},
        test_harness::{CommitSpec, TestHarness},
    };
    use std::{
        collections::{HashMap, VecDeque},
        path::PathBuf,
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

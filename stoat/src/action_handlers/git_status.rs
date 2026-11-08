//! The git status panel's actions, which open it, read its model, walk its
//! rows, act on the selected row, and close it.

use crate::{
    action_handlers::{
        conflict::{self, ConflictChoice},
        conflict_view, git_commit, git_merge, rebase, review,
    },
    app::{Stoat, UpdateEffect},
    auto_reload, buffer_lifecycle,
    buffer_registry::OpenOrigin,
    git_jobs::{self, GitJob, GitLanding, GitWork},
    git_status::{GitStatusModel, GitStatusPanel, Note, OpKind, RebaseProgress, SectionKind},
    host::{FsHost, GitApplyError, GitRepo},
    merge_view::Side,
    paths,
    rebase::RebasePause,
};
use std::{
    io,
    path::{Path, PathBuf},
};

/// The badge for `s` and `u` on a conflicted row, which only `o` and `t`
/// resolve.
const TAKES_A_SIDE: &str = "a conflicted file takes a side with o or t";

/// Open the git status panel and queue the read of its model.
///
/// Outside a git repository the panel does not open, and a review badge says
/// why.
pub(super) fn open_git_status(stoat: &mut Stoat) -> UpdateEffect {
    let git_root = stoat.active_workspace().git_root.clone();
    if stoat.git_host.discover(&git_root).is_none() {
        review::emit_review_error_badge(stoat, "not in a git repository", None);
        return UpdateEffect::Redraw;
    }

    stoat.set_focused_mode("normal".into());
    stoat.git_status = Some(GitStatusPanel::default());
    reload_git_status(stoat);
    UpdateEffect::Redraw
}

pub(super) fn close_git_status(stoat: &mut Stoat) -> UpdateEffect {
    stoat.git_status = None;
    UpdateEffect::Redraw
}

/// Move the panel's selection `delta` rows.
pub(super) fn git_status_step(stoat: &mut Stoat, delta: isize) -> UpdateEffect {
    let Some(panel) = stoat.git_status.as_mut() else {
        return UpdateEffect::None;
    };
    panel.step(delta);
    UpdateEffect::Redraw
}

/// Close the panel and open the selected file.
///
/// A conflicted file opens in the conflict view, a staged or unstaged file in
/// the per-file diff view, and an untracked file as it is. A file of a
/// rebase's conflict pause opens in the rebase conflict screen instead,
/// because the index holds none of the pause's conflicts.
pub(super) fn git_status_open(stoat: &mut Stoat) -> UpdateEffect {
    let Some(target) = selected(stoat) else {
        return UpdateEffect::None;
    };
    stoat.git_status = None;

    let (kind, path) = match target {
        Selected::Repo(kind, path) => (kind, path),
        Selected::Pause(index) => {
            conflict::conflict_select(stoat, index);
            return UpdateEffect::Redraw;
        },
    };

    if kind == SectionKind::Conflicted {
        open_conflicted(stoat, &path);
        return UpdateEffect::Redraw;
    }
    let target = stoat.active_workspace().panes.focus();
    if buffer_lifecycle::open_file_in_pane_now(stoat, target, &path, OpenOrigin::Visited).is_none()
    {
        return UpdateEffect::Redraw;
    }
    match kind {
        SectionKind::Staged | SectionKind::Unstaged => review::diff(stoat, None),
        SectionKind::Conflicted | SectionKind::Untracked => UpdateEffect::Redraw,
    }
}

/// Stage the selected unstaged or untracked file as the working tree holds
/// it, then read the panel again.
///
/// A file of a rebase's conflict pause badges instead, since only `o` and `t`
/// change it.
pub(super) fn git_status_stage(stoat: &mut Stoat) -> UpdateEffect {
    let Some(target) = selected(stoat) else {
        return UpdateEffect::None;
    };
    match target {
        Selected::Repo(SectionKind::Unstaged | SectionKind::Untracked, path) => {
            queue_index_write(stoat, path, IndexWrite::Stage);
        },
        Selected::Repo(SectionKind::Staged, _) => {
            review::emit_review_error_badge(stoat, "already staged", None);
        },
        Selected::Repo(SectionKind::Conflicted, _) | Selected::Pause(_) => {
            review::emit_review_error_badge(stoat, TAKES_A_SIDE, None);
        },
    }
    UpdateEffect::Redraw
}

/// Take the selected staged file out of the index, then read the panel again.
///
/// A file of a rebase's conflict pause badges instead, since only `o` and `t`
/// change it.
pub(super) fn git_status_unstage(stoat: &mut Stoat) -> UpdateEffect {
    let Some(target) = selected(stoat) else {
        return UpdateEffect::None;
    };
    match target {
        Selected::Repo(SectionKind::Staged, path) => {
            queue_index_write(stoat, path, IndexWrite::Unstage);
        },
        Selected::Repo(
            SectionKind::Conflicted | SectionKind::Unstaged | SectionKind::Untracked,
            _,
        ) => {
            review::emit_review_error_badge(stoat, "not staged", None);
        },
        Selected::Pause(_) => review::emit_review_error_badge(stoat, TAKES_A_SIDE, None),
    }
    UpdateEffect::Redraw
}

/// Resolve the selected conflicted file whole to `side`, stage it, note the
/// side for its row, and read the panel again.
///
/// A file of a rebase's conflict pause takes `side` as the pause's resolution
/// instead, which the rebase commits when it goes on.
pub(super) fn git_status_take(stoat: &mut Stoat, side: Side) -> UpdateEffect {
    let path = match selected(stoat) {
        Some(Selected::Repo(SectionKind::Conflicted, path)) => path,
        Some(Selected::Pause(index)) => return take_paused(stoat, index, side),
        _ => {
            review::emit_review_error_badge(stoat, "only a conflicted file takes a side", None);
            return UpdateEffect::Redraw;
        },
    };

    let fs = stoat.fs_host.clone();
    let job = GitJob::new("git status take side", None, move |stoat: &mut Stoat| {
        let (repo, _) = git_commit::commit_repo(stoat)?;
        Some(Box::new(move || {
            let taken = take_side(&*repo, &*fs, &path, side);
            Box::new(move |stoat: &mut Stoat| land_take(stoat, &path, side, taken)) as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
    reload_git_status(stoat);
    UpdateEffect::Redraw
}

/// Close the panel and commit what the banner names.
///
/// A merge commits through its message, a rebase goes on to its next step,
/// and with neither the index commits.
pub(super) fn git_status_commit(stoat: &mut Stoat) -> UpdateEffect {
    let operation = operation(stoat);
    stoat.git_status = None;
    match operation {
        Some(OpKind::Merge) => git_merge::git_merge_continue(stoat),
        Some(OpKind::Rebase) => rebase::git_rebase_continue(stoat),
        None => git_commit::git_commit(stoat),
    }
}

/// Abort the merge or the rebase the banner names, then read the panel again.
pub(super) fn git_status_abort(stoat: &mut Stoat) -> UpdateEffect {
    match operation(stoat) {
        Some(OpKind::Merge) => {
            git_merge::git_merge_abort(stoat);
        },
        Some(OpKind::Rebase) => {
            rebase::git_rebase_abort(stoat);
        },
        None => {
            review::emit_review_error_badge(stoat, "no merge or rebase to abort", None);
            return UpdateEffect::Redraw;
        },
    }
    reload_git_status(stoat);
    UpdateEffect::Redraw
}

/// Skip the rebase entry stopped on a conflict, as the banner's skip key reads,
/// then read the panel again.
///
/// A skip applies only to a conflict stop, as `git rebase --skip` skips the
/// patch that stopped. An edit or a reword stop comes after its commit
/// landed, so with no conflict stop a badge says so.
pub(super) fn git_status_skip(stoat: &mut Stoat) -> UpdateEffect {
    let stopped_on_conflict = matches!(
        stoat
            .active_workspace()
            .rebase_active
            .as_ref()
            .and_then(|active| active.pause.as_ref()),
        Some(RebasePause::Conflict { .. })
    );
    if !stopped_on_conflict {
        review::emit_review_error_badge(stoat, "no rebase stopped on a conflict", None);
        return UpdateEffect::Redraw;
    }

    let effect = conflict::conflict_skip_entry(stoat);
    reload_git_status(stoat);
    effect
}

/// What a key on the selected row acts on.
enum Selected {
    /// A file of the index or the working tree, in the section it sits in.
    Repo(SectionKind, PathBuf),
    /// The file at this index of the rebase's conflict pause.
    Pause(usize),
}

/// What a key on the selected row acts on, or `None` with the panel closed
/// or no row.
///
/// A pause row whose file the pause no longer holds also gives `None`, since
/// its pause ended after the panel's last read.
fn selected(stoat: &Stoat) -> Option<Selected> {
    let panel = stoat.git_status.as_ref()?;
    let (kind, row) = panel.selected_row()?;
    if !row.from_pause {
        return Some(Selected::Repo(kind, row.path.clone()));
    }

    // The pause names its files relative to the repository, and the read
    // joined each pause row's path to the model's root.
    let root = &panel.model.as_ref()?.root;
    let pause = stoat
        .active_workspace()
        .rebase_active
        .as_ref()?
        .pause
        .as_ref()?;
    let RebasePause::Conflict { files, .. } = pause else {
        return None;
    };
    files
        .iter()
        .position(|file| root.join(&file.path) == row.path)
        .map(Selected::Pause)
}

/// The operation the panel's last read found in progress.
fn operation(stoat: &Stoat) -> Option<OpKind> {
    let model = stoat.git_status.as_ref()?.model.as_ref()?;
    model.op.as_ref().map(|op| op.kind)
}

/// Show the conflicted file at `path` in the conflict view.
///
/// An open conflict session that lists the file switches to it, since a fresh
/// open toggles the session closed. Otherwise the file opens in the focused
/// pane and the view opens on it.
fn open_conflicted(stoat: &mut Stoat, path: &Path) {
    let listed = stoat
        .active_workspace()
        .conflict
        .as_ref()
        .map(|session| session.files.iter().position(|file| file == path));
    match listed {
        Some(Some(index)) => conflict_view::switch_to_file(stoat, index),
        Some(None) => {
            conflict_view::close_conflict(stoat);
            open_conflicted(stoat, path);
        },
        None => {
            let target = stoat.active_workspace().panes.focus();
            if buffer_lifecycle::open_file_in_pane_now(stoat, target, path, OpenOrigin::Visited)
                .is_some()
            {
                conflict_view::open_conflict(stoat);
            }
        },
    }
}

/// The index writes that `s` and `u` queue.
#[derive(Clone, Copy)]
enum IndexWrite {
    Stage,
    Unstage,
}

/// Queue `write` on `path` in the git queue, then a read of the panel behind
/// it, so the read sees what the write left.
fn queue_index_write(stoat: &mut Stoat, path: PathBuf, write: IndexWrite) {
    let job = GitJob::new("git status index write", None, move |stoat: &mut Stoat| {
        let (repo, _) = git_commit::commit_repo(stoat)?;
        Some(Box::new(move || {
            let written = match write {
                IndexWrite::Stage => repo.mark_resolved(&path),
                IndexWrite::Unstage => repo.unstage_path(&path),
            }
            .map_err(|GitApplyError::Backend { reason, .. }| reason);
            Box::new(move |stoat: &mut Stoat| land_index_write(stoat, &path, write, written))
                as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
    reload_git_status(stoat);
}

/// Recompute the diffs over the index `write` changed and name what it did,
/// or badge why it failed.
fn land_index_write(
    stoat: &mut Stoat,
    path: &Path,
    write: IndexWrite,
    written: Result<(), String>,
) {
    let (done, failed) = match write {
        IndexWrite::Stage => ("staged", "stage failed"),
        IndexWrite::Unstage => ("unstaged", "unstage failed"),
    };
    if let Err(reason) = written {
        review::emit_review_error_badge(stoat, failed, Some(reason));
        return;
    }
    stoat.active_workspace_mut().invalidate_all_diffs();
    let name = paths::display_relative(path, &stoat.active_workspace().git_root);
    stoat.set_status(format!("{done} {name}"));
}

/// Set `side` as the rebase pause's resolution of its file at `index`, then
/// read the panel again.
///
/// The pause resolves its selected file, so the file also becomes the rebase
/// conflict screen's selection.
fn take_paused(stoat: &mut Stoat, index: usize, side: Side) -> UpdateEffect {
    let choice = match side {
        Side::Ours => ConflictChoice::Ours,
        Side::Theirs => ConflictChoice::Theirs,
    };
    conflict::conflict_select(stoat, index);
    conflict::conflict_set(stoat, choice);
    reload_git_status(stoat);
    UpdateEffect::Redraw
}

/// Write `side`'s stage of the conflicted file at `path` over the file, then
/// stage it.
///
/// The stage text is the side's blob as the index holds it, so the file keeps
/// that side's line endings. A side that deleted the file deletes it here, so
/// the stage records the deletion.
fn take_side(repo: &dyn GitRepo, fs: &dyn FsHost, path: &Path, side: Side) -> Result<(), String> {
    let stages = repo
        .conflict_stages(path)
        .ok_or_else(|| "the file holds no conflict".to_string())?;
    let text = match side {
        Side::Ours => stages.ours,
        Side::Theirs => stages.theirs,
    };
    let written = match text {
        Some(text) => fs.write_atomic(path, text.as_bytes()),
        None => fs.remove_file(path).or_else(|err| match err.kind() {
            io::ErrorKind::NotFound => Ok(()),
            _ => Err(err),
        }),
    };
    written.map_err(|err| err.to_string())?;
    repo.mark_resolved(path)
        .map_err(|GitApplyError::Backend { reason, .. }| reason)
}

/// Note the side a take landed for its row and bring the editor to the file
/// it wrote, or badge why it failed.
fn land_take(stoat: &mut Stoat, path: &Path, side: Side, taken: Result<(), String>) {
    let (note, name) = match side {
        Side::Ours => (Note::TookOurs, "ours"),
        Side::Theirs => (Note::TookTheirs, "theirs"),
    };
    if let Err(reason) = taken {
        review::emit_review_error_badge(stoat, &format!("take {name} failed"), Some(reason));
        return;
    }

    stoat
        .active_workspace_mut()
        .resolved_conflicts
        .insert(path.to_path_buf(), note);
    auto_reload::reload_clean_buffers(stoat);
    stoat.active_workspace_mut().invalidate_all_diffs();
    let file = paths::display_relative(path, &stoat.active_workspace().git_root);
    stoat.set_status(format!("took {name} for {file}"));
}

/// Queue a read of the panel's model behind every git write queued before it.
///
/// The read waits its turn in the git queue, so it sees the repository that
/// every earlier write left. Its inputs are taken when its turn comes. A
/// panel closed by then reads nothing, and one closed while the read runs
/// takes no model.
fn reload_git_status(stoat: &mut Stoat) {
    let job = GitJob::new("git status read", None, |stoat: &mut Stoat| {
        stoat.git_status.as_ref()?;
        let ws = stoat.active_workspace();
        let repo = stoat.git_host.discover(&ws.git_root)?;
        let rebase = ws.rebase_active.as_ref().map(RebaseProgress::of);
        let resolved = ws.resolved_conflicts.clone();
        let fs = stoat.fs_host.clone();
        let languages = stoat.language_registry.clone();
        Some(Box::new(move || {
            let model = GitStatusModel::read(&*repo, &*fs, &languages, rebase.as_ref(), &resolved);
            Box::new(move |stoat: &mut Stoat| {
                if let Some(panel) = stoat.git_status.as_mut() {
                    panel.land(model);
                }
            }) as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
}

#[cfg(test)]
mod tests {
    use crate::{
        action_handlers::dispatch,
        app::Stoat,
        badge::BadgeSource,
        git_status::{Note, SectionKind},
        host::{FakeRepoBuilder, FsHost},
        keymap_state::{active_modal, ActiveModal},
        rebase::{ConflictResolution, RebasePause},
        test_harness::TestHarness,
    };
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
    };

    /// A harness rooted in `/repo`, with HEAD on `main` and the files
    /// `seed` registers.
    fn repo_harness(seed: impl FnOnce(&mut FakeRepoBuilder<'_>)) -> TestHarness {
        let mut h = Stoat::test();
        {
            let mut repo = h.fake_git().add_repo("/repo").with_fs(h.fake_fs());
            repo.set_head_branch("main");
            seed(&mut repo);
        }
        h.stoat.active_workspace_mut().git_root = "/repo".into();
        h
    }

    fn selected(h: &TestHarness) -> Option<usize> {
        h.stoat.git_status.as_ref().map(|panel| panel.selected)
    }

    /// Each section the panel's last read holds, with its rows' paths.
    fn sections(h: &TestHarness) -> Vec<(SectionKind, Vec<PathBuf>)> {
        h.stoat
            .git_status
            .iter()
            .filter_map(|panel| panel.model.as_ref())
            .flat_map(|model| &model.sections)
            .map(|section| {
                let paths = section.rows.iter().map(|row| row.path.clone()).collect();
                (section.kind, paths)
            })
            .collect()
    }

    /// Press `key` in the panel and let its write and the read behind it land.
    fn press(h: &mut TestHarness, key: &str) {
        h.type_keys(key);
        h.settle();
    }

    /// A harness mid-merge with `a.rs` conflicted, its file on disk holding
    /// the conflict markers.
    fn merging_harness() -> TestHarness {
        let h = repo_harness(|repo| {
            repo.merge_in_progress("f00dfeed", Some("feature"), "Merge")
                .conflicted_file("a.rs", Some("base\n"), Some("ours\n"), Some("theirs\n"));
        });
        h.fake_fs().insert_file(
            "/repo/a.rs",
            "<<<<<<< ours\nours\n=======\ntheirs\n>>>>>>> theirs\n",
        );
        h
    }

    /// A harness whose rebase stopped on a conflict over `a.rs` and `b.rs`,
    /// with the panel open on the pause.
    fn paused_harness() -> TestHarness {
        let mut h = Stoat::test();
        h.resize(90, 14);
        h.seed_linear_history(
            "/repo",
            &[
                ("c1", "c1: root", &[("a.rs", "a1\n"), ("b.rs", "b1\n")]),
                ("c2", "c2: middle", &[("a.rs", "a2\n"), ("b.rs", "b2\n")]),
                ("c3", "c3: tip", &[("a.rs", "a3\n"), ("b.rs", "b3\n")]),
            ],
        );
        h.fake_git().add_repo("/repo").simulate_conflict_at("c3");
        h.open_commits("/repo");
        for key in ["G", "i", "Enter"] {
            h.type_keys(key);
        }
        h.settle();
        dispatch(&mut h.stoat, &stoat_action::GitStatus);
        h.settle();
        h
    }

    /// The rebase pause's selected file and its resolutions, or `None` with
    /// no rebase stopped on a conflict.
    fn pause(h: &TestHarness) -> Option<(usize, HashMap<PathBuf, ConflictResolution>)> {
        let active = h.stoat.active_workspace().rebase_active.as_ref()?;
        let Some(RebasePause::Conflict {
            selected,
            resolutions,
            ..
        }) = &active.pause
        else {
            return None;
        };
        Some((*selected, resolutions.clone()))
    }

    /// The label of the review badge, which names why a key did nothing.
    fn review_badge(h: &TestHarness) -> Option<String> {
        let ws = h.stoat.active_workspace();
        ws.badges
            .find_by_source(BadgeSource::Review)
            .and_then(|id| ws.badges.get(id))
            .map(|badge| badge.label.clone())
    }

    /// `s` stages the selected unstaged file, and `u` takes it back out.
    #[test]
    fn s_stages_the_row_and_u_unstages_it() {
        let mut h = repo_harness(|repo| {
            repo.head_file("a.rs", "a").unstaged_file("a.rs", "edit");
        });
        run(&mut h, ":git status");
        let path = PathBuf::from("/repo/a.rs");

        press(&mut h, "s");
        let staged = sections(&h);
        press(&mut h, "u");
        assert_eq!(
            (staged, sections(&h)),
            (
                vec![(SectionKind::Staged, vec![path.clone()])],
                vec![(SectionKind::Unstaged, vec![path])],
            ),
        );
    }

    /// `o` writes our side over a conflicted file, stages it, and notes the
    /// side, so the conflicted row drops out.
    #[test]
    fn o_takes_our_side_of_a_conflicted_file() {
        let mut h = merging_harness();
        run(&mut h, ":git status");
        press(&mut h, "o");

        let path = PathBuf::from("/repo/a.rs");
        let mut written = Vec::new();
        h.fake_fs()
            .read(&path, &mut written)
            .expect("the file is on disk");
        assert_eq!(
            (
                String::from_utf8(written).expect("utf-8"),
                h.fake_git().resolved_paths(Path::new("/repo")),
                h.stoat.active_workspace().resolved_conflicts.clone(),
                sections(&h),
            ),
            (
                "ours\n".to_string(),
                vec![path.clone()],
                HashMap::from([(path, Note::TookOurs)]),
                Vec::new(),
            ),
        );
    }

    /// Enter on a conflicted row closes the panel and opens the conflict view
    /// on the file.
    #[test]
    fn enter_on_a_conflicted_row_opens_the_conflict_view() {
        let mut h = merging_harness();
        run(&mut h, ":git status");
        press(&mut h, "enter");

        assert_eq!(
            (active_modal(&h.stoat), h.stoat.current_view()),
            (None, Some("conflict")),
        );
    }

    /// Taking the side that deleted a conflicted file removes the file, so the
    /// stage records the deletion.
    #[test]
    fn taking_a_side_that_deleted_the_file_removes_it() {
        let mut h = repo_harness(|repo| {
            repo.merge_in_progress("f00dfeed", Some("feature"), "Merge")
                .conflicted_file("a.rs", Some("base\n"), None, Some("theirs\n"));
        });
        h.fake_fs().insert_file("/repo/a.rs", "theirs\n");
        run(&mut h, ":git status");
        press(&mut h, "o");

        assert_eq!(
            (
                h.fake_fs().exists(Path::new("/repo/a.rs")),
                h.fake_git().resolved_paths(Path::new("/repo")),
            ),
            (false, vec![PathBuf::from("/repo/a.rs")]),
        );
    }

    /// Enter on an unstaged row closes the panel and opens the file's
    /// per-file diff.
    #[test]
    fn enter_on_an_unstaged_row_opens_the_diff_view() {
        let mut h = repo_harness(|repo| {
            repo.head_file("a.rs", "a\n")
                .unstaged_file("a.rs", "edit\n");
        });
        run(&mut h, ":git status");
        press(&mut h, "enter");

        assert_eq!(
            (active_modal(&h.stoat), h.stoat.current_view()),
            (None, Some("diff")),
        );
    }

    /// Enter on a conflicted row switches an open conflict view to the file,
    /// where a fresh open would close the view instead.
    #[test]
    fn enter_on_a_conflicted_row_switches_an_open_conflict_view() {
        let mut h = repo_harness(|repo| {
            repo.merge_in_progress("f00dfeed", Some("feature"), "Merge")
                .conflicted_file("a.rs", Some("base\n"), Some("ours\n"), Some("theirs\n"))
                .conflicted_file("b.rs", Some("base\n"), Some("ours\n"), Some("theirs\n"));
        });
        dispatch(&mut h.stoat, &stoat_action::Conflict);
        h.settle();
        dispatch(&mut h.stoat, &stoat_action::GitStatus);
        h.settle();
        press(&mut h, "j");
        press(&mut h, "enter");

        let shown = h
            .stoat
            .active_workspace()
            .conflict
            .as_ref()
            .map(|session| session.files[session.current].clone());
        assert_eq!(
            (h.stoat.current_view(), shown),
            (Some("conflict"), Some(PathBuf::from("/repo/b.rs"))),
        );
    }

    /// `a` aborts the merge the banner names, and the panel's next read finds
    /// no merge.
    #[test]
    fn a_aborts_the_merge_in_progress() {
        let mut h = merging_harness();
        run(&mut h, ":git status");
        press(&mut h, "a");

        let merge = h
            .stoat
            .git_host
            .discover(Path::new("/repo"))
            .and_then(|repo| repo.merge_state());
        let op = h
            .stoat
            .git_status
            .as_ref()
            .and_then(|panel| panel.model.as_ref())
            .map(|model| model.op.is_some());
        assert_eq!((merge, op), (None, Some(false)));
    }

    /// `S` skips the rebase entry stopped on a conflict, so the rebase runs
    /// past it to its end.
    #[test]
    fn capital_s_skips_the_rebase_entry_stopped_on_a_conflict() {
        let mut h = paused_harness();
        let stopped = h.stoat.current_view();
        press(&mut h, "S");

        assert_eq!(
            (stopped, h.stoat.active_workspace().rebase_active.is_some()),
            (Some("rebase_conflict"), false),
        );
    }

    /// With no rebase stopped on a conflict, `S` badges and keeps the panel
    /// open.
    #[test]
    fn capital_s_with_no_conflict_stop_badges() {
        let mut h = repo_harness(|_| {});
        run(&mut h, ":git status");
        press(&mut h, "S");

        assert_eq!(
            (review_badge(&h).as_deref(), active_modal(&h.stoat)),
            (
                Some("no rebase stopped on a conflict"),
                Some(ActiveModal::GitStatus)
            ),
        );
    }

    /// Enter on a pause row closes the panel and selects the row's file in
    /// the rebase conflict screen, since the index holds none of the pause's
    /// conflicts.
    ///
    /// Our side of `a.rs` lists it under staged, so `b.rs` heads the list at
    /// row 0 while the pause holds it at index 1.
    #[test]
    fn enter_on_a_pause_row_selects_its_file_in_the_conflict_screen() {
        let mut h = paused_harness();
        press(&mut h, "o");
        let before = pause(&h).map(|(selected, _)| selected);
        press(&mut h, "enter");

        assert_eq!(
            (
                before,
                active_modal(&h.stoat),
                h.stoat.current_view(),
                pause(&h).map(|(selected, _)| selected),
            ),
            (Some(0), None, Some("rebase_conflict"), Some(1)),
        );
    }

    /// `t` on a pause row takes their side as the pause's resolution, and the
    /// panel's next read lists the file as staged. `o` on that staged row
    /// takes our side in its place.
    ///
    /// The pause starts on `a.rs`, so the side lands on `b.rs` only when the
    /// key selects it first.
    #[test]
    fn t_then_o_on_a_pause_row_set_its_resolution() {
        let mut h = paused_harness();
        press(&mut h, "j");
        press(&mut h, "t");
        let took_theirs = (pause(&h), sections(&h));
        press(&mut h, "o");

        let b = PathBuf::from("b.rs");
        assert_eq!(
            (took_theirs, pause(&h)),
            (
                (
                    Some((
                        1,
                        HashMap::from([(b.clone(), ConflictResolution::TakeTheirs)])
                    )),
                    vec![
                        (SectionKind::Conflicted, vec![PathBuf::from("/repo/a.rs")]),
                        (SectionKind::Staged, vec![PathBuf::from("/repo/b.rs")]),
                    ],
                ),
                Some((1, HashMap::from([(b, ConflictResolution::TakeOurs)]))),
            ),
        );
    }

    /// `u` and `s` on a resolved pause row badge and leave the file to the
    /// pause, since the index holds none of the pause's conflicts.
    #[test]
    fn u_and_s_on_a_resolved_pause_row_badge() {
        let mut h = paused_harness();
        press(&mut h, "o");
        press(&mut h, "j");
        let badges = ["u", "s"].map(|key| {
            press(&mut h, key);
            review_badge(&h)
        });

        let takes_a_side = Some("a conflicted file takes a side with o or t".to_string());
        let took_ours = HashMap::from([(PathBuf::from("a.rs"), ConflictResolution::TakeOurs)]);
        assert_eq!(
            (badges, pause(&h)),
            ([takes_a_side.clone(), takes_a_side], Some((0, took_ours))),
        );
    }

    /// `C` closes the panel and commits the merge in progress through the
    /// commit message buffer.
    #[test]
    fn c_commits_the_merge_through_the_message_buffer() {
        let mut h = merging_harness();
        run(&mut h, ":git status");
        press(&mut h, "C");

        let message = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(Path::new("/repo/.git/COMMIT_EDITMSG"));
        assert_eq!((active_modal(&h.stoat), message.is_some()), (None, true),);
    }

    /// Run `command` on the command line and let the panel's read land.
    fn run(h: &mut TestHarness, command: &str) {
        h.type_text(command);
        h.type_keys("enter");
        h.settle();
    }

    /// `:git status` and `:git st` open the panel on the repository's read,
    /// and Escape closes it.
    #[test]
    fn the_status_command_opens_the_panel_and_escape_closes_it() {
        let opened = |command: &str| {
            let mut h = repo_harness(|_| {});
            run(&mut h, command);
            let opened = (
                active_modal(&h.stoat),
                h.stoat
                    .git_status
                    .as_ref()
                    .and_then(|panel| panel.model.as_ref())
                    .and_then(|model| model.branch.clone()),
            );
            h.type_keys("Escape");
            (opened, active_modal(&h.stoat))
        };

        let open_then_closed = (
            (Some(ActiveModal::GitStatus), Some("main".to_string())),
            None,
        );
        assert_eq!(
            [opened(":git status"), opened(":git st")],
            [open_then_closed.clone(), open_then_closed],
        );
    }

    #[test]
    fn the_panel_does_not_open_outside_a_repository() {
        let mut h = Stoat::test();
        dispatch(&mut h.stoat, &stoat_action::GitStatus);
        h.settle();
        assert_eq!(active_modal(&h.stoat), None);
    }

    /// j and k walk the files of every section, and stop at the first and
    /// the last file.
    #[test]
    fn j_and_k_walk_the_files_and_stop_at_the_ends() {
        let mut h = repo_harness(|repo| {
            repo.head_file("a.rs", "a")
                .staged_file("a.rs", "b")
                .untracked("b.rs");
        });
        run(&mut h, ":git status");

        let walk: Vec<Option<usize>> = ["j", "j", "k", "k"]
            .into_iter()
            .map(|key| {
                h.type_keys(key);
                selected(&h)
            })
            .collect();
        assert_eq!(walk, [Some(1), Some(1), Some(0), Some(0)]);
    }

    /// A landed tally keeps the resolution notes while a merge is in
    /// progress, and drops them once no merge is.
    #[test]
    fn a_tally_with_no_merge_drops_the_resolution_notes() {
        let notes_after_tally = |merging: bool| {
            let mut h = repo_harness(|repo| {
                if merging {
                    repo.merge_in_progress("f00dfeed", Some("feature"), "Merge");
                }
            });
            h.stoat
                .active_workspace_mut()
                .resolved_conflicts
                .insert(PathBuf::from("/repo/a.rs"), Note::Resolved);
            h.stoat.set_diff_warm_auto(true);
            h.settle_diff_jobs();
            h.stoat.active_workspace().resolved_conflicts.len()
        };

        assert_eq!([notes_after_tally(true), notes_after_tally(false)], [1, 0]);
    }

    /// A list longer than the box scrolls to keep the selection in view, under
    /// the head of its section.
    #[test]
    fn snapshot_git_status_scrolled() {
        let mut h = repo_harness(|repo| {
            for i in 0..12 {
                repo.modified(format!("f{i:02}.rs"), "a", "b");
            }
        });
        h.resize(80, 14);
        run(&mut h, ":git status");
        for _ in 0..11 {
            h.type_keys("j");
        }
        h.assert_snapshot("git_status_scrolled");
    }

    /// The board's dirty state at 80 by 20, as the cells hold it.
    #[test]
    fn snapshot_git_status_dirty() {
        let mut h = repo_harness(|repo| {
            repo.commit("c1", &[])
                .commit_with_parent("c2", "c1", &[])
                .branch("main", "c1")
                .branch("ermine-tokens", "c2")
                .set_head("c2")
                .set_head_branch("ermine-tokens")
                .head_file("stoat/src/theme.rs", "a")
                .staged_file("stoat/src/theme.rs", "b")
                .lines("stoat/src/theme.rs", 6, 0)
                .modified("stoat/src/render/chrome.rs", "a", "b")
                .lines("stoat/src/render/chrome.rs", 5, 6)
                .modified("config.stcfg", "a", "b")
                .lines("config.stcfg", 14, 2)
                .untracked("themes/ermine-night.json");
        });
        h.resize(80, 20);
        run(&mut h, ":git status");
        h.assert_snapshot("git_status_dirty");
    }
}

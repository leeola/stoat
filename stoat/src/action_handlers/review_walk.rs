use crate::{
    app::{Stoat, UpdateEffect},
    auto_reload,
    badge::BadgeSource,
    buffer_registry::OpenOrigin,
    commit_list::PendingPreview,
    commit_picker::{self, CommitPicker, CommitPickerRole, LoadedCommits, RangeFrom},
    git_jobs::{self, GitJob, GitJobKey, GitLanding, GitWork},
    host::CommitInfo,
    review_walk::{ReturnRef, ReviewWalk, WalkOrigin, WalkSpan, WalkStash},
    workspace::diff::DiffBase,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat_scheduler::Task;

/// Commits the picker loads for a ref. Bounds the eager walk on a long
/// history without paging, which fuzzy filtering could not work across.
pub(super) const WALK_LIMIT: usize = 1000;

/// Open the commit picker over `reference`'s first-parent history.
///
/// The whole walk is loaded up front rather than paged. The picker filters
/// across every row, and a virtualized window would only ever search the part
/// already fetched. [`WALK_LIMIT`] bounds that, so a history longer than the
/// limit is silently truncated at its oldest end.
pub(crate) fn git_review(stoat: &mut Stoat, reference: &str) -> UpdateEffect {
    let git_root = stoat.active_workspace().git_root.clone();
    let Some(repo) = stoat.git_host.discover(&git_root) else {
        return review_error(stoat, "not in a git repository", None);
    };
    let Some(workdir) = repo.workdir() else {
        return review_error(stoat, "git repo has no working tree", None);
    };
    let Some(ref_sha) = repo.resolve_rev(reference) else {
        return review_error(
            stoat,
            "unknown revision",
            Some(format!("no revision named {reference}")),
        );
    };
    open_commit_picker(
        stoat,
        &repo,
        CommitPickerRole::PickBase,
        workdir,
        ref_sha,
        None,
    )
}

/// Review `from` against `against` as a walk of one commit, the tip of `from`
/// diffed against the merge base of the two.
///
/// With no `from`, the commit list opens over HEAD's history to pick both
/// ends, as [`CommitPickerRole::PickRange`] describes. With no `against`, the
/// branch `from` forked from stands in, as [`fork_branch`] picks it. The refs
/// resolve on the git queue, behind every write queued before the press. A
/// review walk or a rebase that holds HEAD at that turn refuses the review, as
/// it refuses a checkout.
pub(crate) fn review(stoat: &mut Stoat, from: Option<&str>, against: Option<&str>) -> UpdateEffect {
    let Some(from) = from else {
        return open_range_picker(stoat);
    };

    let (from, against) = (from.to_string(), against.map(str::to_string));
    let job = GitJob::new("review", None, move |stoat: &mut Stoat| {
        let repo = checkout_repo(stoat)?;
        let workdir = repo.workdir()?;
        Some(Box::new(move || {
            let resolved = resolve_review(&*repo, &from, against.as_deref());
            Box::new(move |stoat: &mut Stoat| match resolved {
                Ok((commit, span)) => {
                    queue_walk_start(
                        stoat,
                        workdir,
                        vec![commit],
                        WalkLandingKind::Walk,
                        span,
                        WalkEntry::default(),
                        WalkOrigin::Commits,
                    );
                },
                Err((label, detail)) => {
                    review_error(stoat, label, detail);
                },
            }) as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
    UpdateEffect::Redraw
}

/// Open the commit list over HEAD's history to pick both ends of a range
/// review, once the refusals a checkout runs let it.
fn open_range_picker(stoat: &mut Stoat) -> UpdateEffect {
    let Some(repo) = checkout_repo(stoat) else {
        return UpdateEffect::Redraw;
    };
    let Some(workdir) = repo.workdir() else {
        return UpdateEffect::Redraw;
    };
    let Some(head) = repo.resolve_rev("HEAD") else {
        return review_error(stoat, "no commits on this branch", None);
    };
    open_commit_picker(
        stoat,
        &repo,
        CommitPickerRole::PickRange,
        workdir,
        head,
        None,
    )
}

/// The tip of `from` and the span that reviews it against `against`, or the
/// badge label and detail that refuse the review, on whatever thread calls.
///
/// A `from` that `against` already holds has nothing to review, since the
/// merge base is then `from` itself.
fn resolve_review(
    repo: &dyn crate::host::GitRepo,
    from: &str,
    against: Option<&str>,
) -> Result<(CommitInfo, WalkSpan), (&'static str, Option<String>)> {
    let unknown = |rev: &str| ("unknown revision", Some(format!("no revision named {rev}")));
    let from_sha = repo.resolve_rev(from).ok_or_else(|| unknown(from))?;
    let (against, against_sha) = match against {
        Some(against) => {
            let sha = repo.resolve_rev(against).ok_or_else(|| unknown(against))?;
            (against.to_string(), sha)
        },
        None => fork_branch(repo, from, &from_sha).ok_or_else(|| {
            (
                "no branch to review against",
                Some(format!("{from} forked from no other local branch")),
            )
        })?,
    };

    let base = repo.merge_base(&from_sha, &against_sha).ok_or_else(|| {
        (
            "no common ancestor",
            Some(format!("{from} and {against} share no history")),
        )
    })?;
    if base == from_sha {
        return Err((
            "nothing to review",
            Some(format!("{from} is merged into {against}")),
        ));
    }

    let commit = repo
        .log_from(&from_sha, 1)
        .into_iter()
        .next()
        .ok_or_else(|| unknown(from))?;
    Ok((
        commit,
        WalkSpan::Range {
            base,
            from: from.to_string(),
            against,
        },
    ))
}

/// The local branch `from` forked from, as its name and tip sha.
///
/// Every local branch but `from` itself, and but any whose tip is `from_sha`,
/// is a candidate. The winner's merge base with `from_sha` comes first along
/// `from`'s first-parent history, which makes it the newest fork point. Ties
/// go to `main`, then `master`, then the first name in order. A merge base past
/// the first [`WALK_LIMIT`] commits ranks after every one within them.
///
/// `None` when no candidate shares history with `from`.
pub fn fork_branch(
    repo: &dyn crate::host::GitRepo,
    from: &str,
    from_sha: &str,
) -> Option<(String, String)> {
    let history: Vec<String> = repo
        .log_from(from_sha, WALK_LIMIT)
        .into_iter()
        .map(|commit| commit.sha)
        .collect();
    let rank = |name: &str| match name {
        "main" => 0,
        "master" => 1,
        _ => 2,
    };

    repo.local_branches()
        .into_iter()
        .filter(|(name, tip)| name != from && tip != from_sha)
        .filter_map(|(name, tip)| {
            let base = repo.merge_base(from_sha, &tip)?;
            let depth = history
                .iter()
                .position(|sha| *sha == base)
                .unwrap_or(usize::MAX);
            Some((depth, rank(&name), name, tip))
        })
        .min()
        .map(|(_, _, name, tip)| (name, tip))
}

/// List a ref's history read-only, defaulting to the current branch.
///
/// The same picker `:git review` opens, in a role where selecting a row
/// dismisses rather than starting a walk, so browsing history never touches the
/// working tree. A named `rev` titles the modal after itself, since a list
/// rooted somewhere other than HEAD has to say where.
pub fn git_ls(stoat: &mut Stoat, rev: Option<&str>) -> UpdateEffect {
    let git_root = stoat.active_workspace().git_root.clone();
    let Some(repo) = stoat.git_host.discover(&git_root) else {
        return review_error(stoat, "not in a git repository", None);
    };
    let Some(workdir) = repo.workdir() else {
        return review_error(stoat, "git repo has no working tree", None);
    };

    let Some(rev) = rev else {
        let Some(head) = repo.resolve_rev("HEAD") else {
            return review_error(stoat, "no commits on this branch", None);
        };
        return open_commit_picker(stoat, &repo, CommitPickerRole::Browse, workdir, head, None);
    };
    let Some(sha) = repo.resolve_rev(rev) else {
        return review_error(
            stoat,
            "unknown revision",
            Some(format!("no revision named {rev}")),
        );
    };

    open_commit_picker(
        stoat,
        &repo,
        CommitPickerRole::Browse,
        workdir,
        sha,
        Some(format!("git log {rev}")),
    )
}

/// List the stashes, newest first, in the commit picker.
///
/// Enter pops the highlighted stash and closes the picker. Escape closes the
/// picker and keeps every stash. Each row previews the stash's changes against
/// the commit that the stash was saved on.
pub(crate) fn git_stash_list(stoat: &mut Stoat) -> UpdateEffect {
    let git_root = stoat.active_workspace().git_root.clone();
    let Some(repo) = stoat.git_host.discover(&git_root) else {
        return review_error(stoat, "not in a git repository", None);
    };
    let Some(workdir) = repo.workdir() else {
        return review_error(stoat, "git repo has no working tree", None);
    };
    let Some(head) = repo.resolve_rev("HEAD") else {
        return review_error(stoat, "no commits on this branch", None);
    };

    open_commit_picker(stoat, &repo, CommitPickerRole::Stash, workdir, head, None)
}

/// Check `reference` out, attaching HEAD when it names a local branch and
/// detaching HEAD at the commit it resolves to otherwise.
///
/// The checkout runs behind every git write queued before the press, and the
/// open buffers, the diffs, and the commits lists follow the tree it leaves. A
/// review walk or a rebase owns HEAD until it ends, so either one refuses the
/// checkout at its turn.
pub(crate) fn git_checkout(stoat: &mut Stoat, reference: &str) -> UpdateEffect {
    let reference = reference.to_string();
    let job = GitJob::new("checkout", None, move |stoat: &mut Stoat| {
        let repo = checkout_repo(stoat)?;
        Some(Box::new(move || {
            let checked_out = check_out(&*repo, &reference);
            Box::new(move |stoat: &mut Stoat| land_checkout(stoat, checked_out)) as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
    UpdateEffect::Redraw
}

/// The repository a checkout acts on at its turn, or `None` after badging why
/// the checkout does not run.
///
/// The merge commands share it, since a review walk or a rebase holds HEAD
/// against a merge too.
pub(super) fn checkout_repo(stoat: &mut Stoat) -> Option<Arc<dyn crate::host::GitRepo>> {
    let ws = stoat.active_workspace();
    let refusal = if ws.review_walk.is_some() {
        Some(("a review walk is open", Some("run :done first".to_string())))
    } else if ws.rebase_active.is_some() {
        Some(("a rebase is in progress", None))
    } else {
        None
    };
    if let Some((label, detail)) = refusal {
        review_error(stoat, label, detail);
        return None;
    }

    let git_root = stoat.active_workspace().git_root.clone();
    let Some(repo) = stoat.git_host.discover(&git_root) else {
        review_error(stoat, "not in a git repository", None);
        return None;
    };
    if repo.workdir().is_none() {
        review_error(stoat, "git repo has no working tree", None);
        return None;
    }
    Some(repo)
}

/// Check `reference` out in `repo`, on whatever thread calls.
///
/// Answers the status line for the landing, or the badge label and detail
/// when HEAD did not move.
fn check_out(
    repo: &dyn crate::host::GitRepo,
    reference: &str,
) -> Result<String, (&'static str, Option<String>)> {
    let refused = |err: crate::host::GitApplyError| ("checkout failed", Some(err.to_string()));
    if repo
        .local_branches()
        .iter()
        .any(|(name, _)| name == reference)
    {
        repo.checkout_ref(reference).map_err(refused)?;
        return Ok(format!("checked out {reference}"));
    }

    let Some(sha) = repo.resolve_rev(reference) else {
        return Err((
            "unknown revision",
            Some(format!("no revision named {reference}")),
        ));
    };
    repo.checkout_detached(&sha).map_err(refused)?;
    Ok(format!("HEAD detached at {}", &sha[..sha.len().min(7)]))
}

/// Bring the editor to the tree a checkout left, or badge why HEAD did not
/// move.
///
/// Clean buffers re-read their files, every diff recomputes against the new
/// HEAD, and each commits list reloads from it.
fn land_checkout(stoat: &mut Stoat, checked_out: Result<String, (&'static str, Option<String>)>) {
    let status = match checked_out {
        Ok(status) => status,
        Err((label, detail)) => {
            review_error(stoat, label, detail);
            return;
        },
    };

    auto_reload::reload_clean_buffers(stoat);
    stoat.active_workspace_mut().invalidate_all_diffs();
    super::commits::reload_every_list(stoat);
    stoat.set_status(status);
}

/// Build and install a picker over `ref_sha`'s first-parent history.
///
/// The editor underneath is dropped to normal mode first, so a chord that
/// opened the picker releases rather than waiting for a key the picker has
/// taken focus away from. The picker's own input then supplies the insert mode
/// its keymap block is gated on.
///
/// A `scope_label` titles the modal after what it is rooted at. `None` leaves
/// the title to the picker's role, which is right whenever the root scope is
/// the obvious one for that role.
fn open_commit_picker(
    stoat: &mut Stoat,
    repo: &Arc<dyn crate::host::GitRepo>,
    role: CommitPickerRole,
    workdir: PathBuf,
    ref_sha: String,
    scope_label: Option<String>,
) -> UpdateEffect {
    stoat.set_focused_mode("normal".to_string());

    let task = spawn_history_walk(&stoat.executor, stoat.redraw_notify.clone(), {
        let repo = Arc::clone(repo);
        let ref_sha = ref_sha.clone();
        move || {
            // A browser shows the real DAG, so a merged branch's commits are
            // rows of their own. A range pick shows it too, since a commit on
            // a merged branch is a `from` as well. A branch HEAD does not reach lists in
            // neither, and a review of one names it. A base picker keeps the
            // first-parent walk, because the review chain a pick seeds is
            // defined over first-parent history.
            let commits = match role {
                CommitPickerRole::Browse | CommitPickerRole::PickRange => {
                    repo.log_graph(&ref_sha, WALK_LIMIT)
                },
                CommitPickerRole::PickBase => repo.log_from(&ref_sha, WALK_LIMIT),
                // No branch points at a stash commit, so the stash list
                // carries no branch tips.
                CommitPickerRole::Stash => {
                    return LoadedCommits::Root {
                        commits: repo.stash_list(),
                        branch_tips: HashMap::new(),
                    };
                },
            };
            let mut branch_tips: HashMap<String, Vec<String>> = HashMap::new();
            for (name, sha) in repo.local_branches() {
                branch_tips.entry(sha).or_default().push(name);
            }
            LoadedCommits::Root {
                commits,
                branch_tips,
            }
        }
    });

    // Ages are measured against the moment the picker opened, so they hold
    // still while the user scrolls rather than drifting under them.
    let now_epoch = commit_picker::now_epoch(&stoat.executor);
    let executor = stoat.executor.clone();
    let mut picker = CommitPicker::new(
        stoat.active_workspace_mut(),
        executor,
        role,
        workdir,
        repo.clone(),
        ref_sha,
        now_epoch,
    );
    picker.scope_label = scope_label;
    picker.pending_commits = Some(task);
    stoat.commit_picker = Some(picker);
    UpdateEffect::Redraw
}

/// Run `walk` on a worker, waking the run loop when it lands.
///
/// The wake is what makes the list appear on its own. The pump reading this
/// task polls with a noop waker, so a walk finishing after the last input event
/// would otherwise leave the picker empty until some unrelated event happened to
/// drive the pumps.
fn spawn_history_walk(
    executor: &stoat_scheduler::Executor,
    redraw: Arc<tokio::sync::Notify>,
    walk: impl FnOnce() -> LoadedCommits + Send + 'static,
) -> Task<LoadedCommits> {
    executor.spawn_blocking(move || {
        let loaded = walk();
        redraw.notify_one();
        loaded
    })
}

fn review_error(stoat: &mut Stoat, label: &str, detail: Option<String>) -> UpdateEffect {
    super::review::emit_review_error_badge(stoat, label, detail);
    UpdateEffect::Redraw
}

pub fn commit_picker_step(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.as_mut() else {
        return UpdateEffect::None;
    };
    picker.move_selection(delta);
    ensure_selected_preview(stoat);
    UpdateEffect::Redraw
}

/// Advance which column of the commit table the query filters.
///
/// Refilters against the query already typed, so the list narrows to the new
/// column immediately, and re-syncs the preview because the selection may land
/// on a different commit once the rows change.
pub(crate) fn commit_picker_column_cycle(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.as_ref() else {
        return UpdateEffect::None;
    };
    let query = picker.input.text(stoat.active_workspace());
    let Some(picker) = stoat.commit_picker.as_mut() else {
        return UpdateEffect::None;
    };
    picker.cycle_filter_column(&query);
    ensure_selected_preview(stoat);
    UpdateEffect::Redraw
}

/// Re-scope the list to the commits the selected merge brought in.
///
/// The scope is `merge^1..merge^2`, which leaves out the mainline the merge
/// landed on. That is the branch's own work, which is what a user drilling into
/// a merge is asking to see. A row with fewer than two parents has no branch to
/// drill into and badges instead. So does every stash, whose second parent is
/// the index it saved rather than a branch.
pub fn commit_picker_drill_in(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.as_ref() else {
        return UpdateEffect::None;
    };
    if picker.role == CommitPickerRole::Stash {
        return review_error(stoat, "a stash has no branch to drill into", None);
    }
    let Some(commit) = picker.selected_commit() else {
        return UpdateEffect::None;
    };
    let (sha, short_sha) = (commit.sha.clone(), commit.short_sha.clone());
    let workdir = picker.workdir.clone();
    let query = picker.input.text(stoat.active_workspace());

    let Some(repo) = stoat.git_host.discover(&workdir) else {
        return review_error(stoat, "not in a git repository", None);
    };
    // One commit lookup, so this stays here and refuses a non-merge on the
    // keystroke. The range walk behind it is the part worth deferring.
    let parents = repo.parent_shas(&sha);
    let [mainline, branch, ..] = parents.as_slice() else {
        return review_error(stoat, "not a merge commit", None);
    };

    let task = spawn_history_walk(&stoat.executor, stoat.redraw_notify.clone(), {
        let repo = Arc::clone(&repo);
        let (mainline, branch) = (mainline.clone(), branch.clone());
        let label = format!("merge {short_sha}");
        move || LoadedCommits::Scope {
            commits: repo.log_range(&branch, &mainline, WALK_LIMIT),
            label,
            ref_sha: branch,
            query_before: query,
        }
    });

    let Some(picker) = stoat.commit_picker.as_mut() else {
        return UpdateEffect::None;
    };
    picker.pending_commits = Some(task);
    UpdateEffect::Redraw
}

/// Return to the scope the last drill re-scoped away from.
///
/// The query typed in that scope comes back with it, since the user is being
/// put back where they were rather than handed a fresh list.
///
/// Back also cancels a drill whose walk has not landed, at any scope, the root
/// included, because the reader leaves the scope that the drill starts from.
pub fn commit_picker_back(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.as_mut() else {
        return UpdateEffect::None;
    };
    // A drill whose walk has not landed goes here. Otherwise it arrives later
    // and pushes its scope over the scope the reader returns to.
    let drilling = picker.pending_commits.take().is_some();
    let popped = picker.pop_scope();
    if picker
        .range_from
        .as_ref()
        .is_some_and(|range| picker.scope_stack.len() <= range.depth)
    {
        picker.range_from = None;
    }
    let Some(query) = popped else {
        return match drilling {
            true => UpdateEffect::Redraw,
            false => UpdateEffect::None,
        };
    };

    set_picker_query(stoat, &query);
    ensure_selected_preview(stoat);
    UpdateEffect::Redraw
}

/// Write `text` into the picker's query field, replacing whatever is there.
///
/// The workspace is reached by index rather than through `active_workspace_mut`
/// so the picker borrow and the workspace borrow stay disjoint.
pub fn set_picker_query(stoat: &mut Stoat, text: &str) {
    let active_idx = stoat.active_workspace;
    let ws = &mut stoat.workspaces[active_idx];
    if let Some(picker) = stoat.commit_picker.as_ref() {
        picker.input.replace_text(ws, text);
    }
}

/// Page the commit picker's selection by half its visible rows in `dir`.
///
/// The paging counterpart to [`commit_picker_step`], and it syncs the preview
/// the same way so the diff follows a paged selection as it does a stepped one.
pub(crate) fn commit_picker_page(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.as_mut() else {
        return UpdateEffect::None;
    };
    picker.page(dir);
    ensure_selected_preview(stoat);
    UpdateEffect::Redraw
}

/// Jump the commit picker's selection to the nearest branch tip in `dir`.
///
/// Syncs the preview like [`commit_picker_step`] and [`commit_picker_page`], so
/// the diff follows a jumped selection as it does a stepped one. The stash list
/// has no branch tips, so the jump does nothing there.
pub(crate) fn commit_picker_branch(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.as_mut() else {
        return UpdateEffect::None;
    };
    if picker.role == CommitPickerRole::Stash {
        return UpdateEffect::None;
    }
    picker.select_branch(dir);
    ensure_selected_preview(stoat);
    UpdateEffect::Redraw
}

/// Take the selected commit as the review base and start walking from it.
///
/// The walk spans the selected commit through the ref tip, including commits
/// the query filtered out of view. The user picked a starting point in
/// history, not a subset of rows.
pub fn commit_picker_select(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.as_ref() else {
        return UpdateEffect::None;
    };
    match picker.role {
        CommitPickerRole::PickBase => install_walk(stoat),
        CommitPickerRole::Browse => commit_picker_close(stoat),
        CommitPickerRole::PickRange => pick_range_step(stoat),
        CommitPickerRole::Stash => pop_selected_stash(stoat),
    }
}

/// Take the selected commit as one end of a range review.
///
/// The first pick is `from`, and the list re-scopes to `from`'s first-parent
/// history, where the second pick is `against`. The picker stays up behind the
/// second pick until the walk starts, so a refused review leaves both picks in
/// place. A second pick of `from` itself has nothing to review.
fn pick_range_step(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.as_ref() else {
        return UpdateEffect::None;
    };
    let Some(selected) = picker.selected_commit().cloned() else {
        return UpdateEffect::None;
    };

    let Some(from) = picker.range_from.as_ref().map(|range| &range.commit) else {
        let query = picker.input.text(stoat.active_workspace());
        let depth = picker.scope_stack.len();
        let task = spawn_history_walk(&stoat.executor, stoat.redraw_notify.clone(), {
            let repo = Arc::clone(&picker.repo);
            let sha = selected.sha.clone();
            let label = format!("review {} against", selected.short_sha);
            move || LoadedCommits::Scope {
                commits: repo.log_from(&sha, WALK_LIMIT),
                label,
                ref_sha: sha,
                query_before: query,
            }
        });
        let Some(picker) = stoat.commit_picker.as_mut() else {
            return UpdateEffect::None;
        };
        picker.range_from = Some(RangeFrom {
            commit: selected,
            depth,
        });
        picker.pending_commits = Some(task);
        return UpdateEffect::Redraw;
    };

    if from.sha == selected.sha {
        return review_error(
            stoat,
            "nothing to review",
            Some("pick an older commit".to_string()),
        );
    }
    let [from, against] = [from, &selected].map(|commit| revision_name(picker, commit));
    review(stoat, Some(&from), Some(&against))
}

/// The name a range review reads `commit` by, the first local branch at it,
/// or else its short sha, which resolves as a prefix.
fn revision_name(picker: &CommitPicker, commit: &CommitInfo) -> String {
    picker
        .branch_tips
        .get(&commit.sha)
        .and_then(|names| names.first())
        .unwrap_or(&commit.short_sha)
        .clone()
}

/// Pop the highlighted stash and close the stash list.
///
/// The list closes on the keystroke, and the pop waits its turn in the git
/// queue, as `:git stash pop` does.
fn pop_selected_stash(stoat: &mut Stoat) -> UpdateEffect {
    let Some(stash) = stoat
        .commit_picker
        .as_ref()
        .and_then(|picker| picker.selected_commit().cloned())
    else {
        return UpdateEffect::None;
    };

    commit_picker_close(stoat);
    super::git_stash::queue_stash_pop(stoat, Some(stash));
    UpdateEffect::Redraw
}

fn install_walk(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.as_ref() else {
        return UpdateEffect::None;
    };
    let Some(base_sha) = picker.selected_commit().map(|c| c.sha.clone()) else {
        return UpdateEffect::None;
    };
    let workdir = picker.workdir.clone();
    let Some(base_idx) = picker.commits.iter().position(|c| c.sha == base_sha) else {
        return UpdateEffect::None;
    };
    let mut commits = picker.commits[..=base_idx].to_vec();
    commits.reverse();

    queue_walk_start(
        stoat,
        workdir,
        commits,
        WalkLandingKind::Walk,
        WalkSpan::Commits,
        WalkEntry::default(),
        WalkOrigin::Commits,
    );
    UpdateEffect::Redraw
}

/// Queue the start of a walk over `commits`, which records where HEAD goes
/// back to and checks the first commit out.
///
/// `span` decides what the walk diffs each commit against, as [`WalkSpan`]
/// describes.
///
/// The start reads HEAD at the job's turn, after every git write queued before
/// the press landed. So a walk started while an ended walk's return waits in
/// the queue records the ref that return restores, not the ended walk's commit.
///
/// A refused start badges and leaves a base or range picker up, so what the
/// user picked is still selected when they come back to it. A walk started while
/// this one waited keeps its place.
///
/// A walkthrough's walk, of `kind` [`WalkLandingKind::Walkthrough`], starts
/// only while a tour that spans commits still plays. A refusal closes that
/// tour, since no landing comes to carry it on.
///
/// `entry` names what the first landing does beyond its checkout, as
/// [`WalkEntry`] describes. Every later step shows its own commit's first
/// file.
///
/// `origin` decides what each landing of the walk shows, as [`WalkOrigin`]
/// describes.
pub fn queue_walk_start(
    stoat: &mut Stoat,
    workdir: PathBuf,
    commits: Vec<CommitInfo>,
    kind: WalkLandingKind,
    span: WalkSpan,
    entry: WalkEntry,
    origin: WalkOrigin,
) {
    let job = GitJob::new(
        "walk start",
        Some(GitJobKey::WalkLanding),
        move |stoat: &mut Stoat| {
            let tour = kind == WalkLandingKind::Walkthrough;
            // A tour closed before this turn asked for nothing. Whatever tour plays
            // at this turn is not the one that queued the walk.
            if tour
                && !stoat
                    .active_workspace()
                    .walkthrough
                    .as_ref()
                    .is_some_and(|run| run.spans_commits())
            {
                return None;
            }

            let return_ref = if stoat.active_workspace().review_walk.is_some() {
                None
            } else {
                walk_return_ref(stoat, &workdir, entry.stash.is_some()).ok()
            };
            let Some(return_ref) = return_ref else {
                if tour {
                    super::walkthrough::abandon_commit_tour(stoat);
                }
                return None;
            };

            if stoat.commit_picker.as_ref().is_some_and(|picker| {
                matches!(
                    picker.role,
                    CommitPickerRole::PickBase | CommitPickerRole::PickRange
                )
            }) {
                commit_picker_close(stoat);
            }
            stoat.active_workspace_mut().review_walk = Some(ReviewWalk {
                workdir,
                commits,
                cursor: 0,
                return_ref,
                span,
                origin,
                stash: None,
            });

            let (workdir, sha, standing) = walk_position(stoat)?;
            walk_checkout_work(stoat, kind, workdir, sha, standing, entry)
        },
    );
    git_jobs::enqueue(stoat, job);
}

/// Where a walk over `workdir` must put HEAD back, or the effect that refuses
/// to start one.
///
/// Read at the start job's turn, after every git write queued before the press
/// landed, and before anything detaches. Afterwards there is no way to tell
/// whether the user was on a branch or already detached, and reattaching
/// someone who was detached moves a branch they never asked to move.
///
/// A tree with tracked changes refuses unless `allow_dirty` holds. A walk
/// checks commits out, so starting one over uncommitted work either loses it or
/// leaves the checkout half applied. A start that stashes the changes first
/// passes `allow_dirty`.
fn walk_return_ref(
    stoat: &mut Stoat,
    workdir: &Path,
    allow_dirty: bool,
) -> Result<ReturnRef, UpdateEffect> {
    let Some(repo) = stoat.git_host.discover(workdir) else {
        return Err(review_error(stoat, "not in a git repository", None));
    };
    if !allow_dirty && repo.has_tracked_changes() {
        return Err(dirty_tree_error(stoat));
    }
    match repo.head_branch() {
        Some(branch) => Ok(ReturnRef::Branch(branch)),
        None => match repo.resolve_rev("HEAD") {
            Some(sha) => Ok(ReturnRef::Detached(sha)),
            None => Err(review_error(stoat, "cannot resolve HEAD", None)),
        },
    }
}

/// Start a walk over `commit` alone and land on it, on `open` when given and on
/// the commit's first changed file otherwise.
///
/// The commits view opens a commit this way. A walk of one steps nowhere, but
/// it carries the same checkout and the same return ref, so `:done` puts the
/// tree back exactly as it does for a walk over many.
///
/// The walk starts at its queued job's turn, as [`queue_walk_start`]
/// describes, so the walk does not exist yet when this returns.
pub fn walk_one_commit(
    stoat: &mut Stoat,
    workdir: PathBuf,
    commit: CommitInfo,
    open: Option<PathBuf>,
) -> UpdateEffect {
    queue_walk_start(
        stoat,
        workdir,
        vec![commit],
        WalkLandingKind::Walk,
        WalkSpan::Commits,
        WalkEntry {
            open,
            ..WalkEntry::default()
        },
        WalkOrigin::Commits,
    );
    UpdateEffect::Redraw
}

/// Start a walk over `commit` alone for a blamed line, and land on the line.
///
/// `path` and `row` are the repo-relative path and the 0-based line that the
/// blamed line has in `commit`. The landing opens that file at that line and
/// opens the changes view over it, which lands on the same line. The walk is a
/// walk of one in every other way, so `:done` puts the tree back as it does for
/// any walk.
///
/// The walk starts at its queued job's turn, as [`queue_walk_start`]
/// describes, so the walk does not exist yet when this returns.
pub(super) fn walk_blame_commit(
    stoat: &mut Stoat,
    workdir: PathBuf,
    commit: CommitInfo,
    path: PathBuf,
    row: u32,
) -> UpdateEffect {
    start_blame_walk(stoat, workdir, commit, path, row, None)
}

/// Start a walk over `commit` alone for a blamed line, as
/// [`walk_blame_commit`] does, over a working tree whose changes the first
/// landing stashes.
///
/// The stash takes the untracked files too. Its message names the commit, so
/// `git stash list` says where the changes went. A landing that fails puts the
/// changes back and ends the walk.
pub(super) fn walk_blame_commit_stashing(
    stoat: &mut Stoat,
    workdir: PathBuf,
    commit: CommitInfo,
    path: PathBuf,
    row: u32,
) -> UpdateEffect {
    let stash = format!("stoat: blame checkout of {}", commit.short_sha);
    start_blame_walk(stoat, workdir, commit, path, row, Some(stash))
}

/// Queue the start of a blame walk, whose first landing stashes under `stash`
/// when one is given.
fn start_blame_walk(
    stoat: &mut Stoat,
    workdir: PathBuf,
    commit: CommitInfo,
    path: PathBuf,
    row: u32,
    stash: Option<String>,
) -> UpdateEffect {
    queue_walk_start(
        stoat,
        workdir,
        vec![commit],
        WalkLandingKind::Walk,
        WalkSpan::Commits,
        WalkEntry {
            open: Some(path.clone()),
            stash,
        },
        WalkOrigin::Blame { path, row },
    );
    UpdateEffect::Redraw
}

/// Check the walk's current commit out and point `:diff-files` at what it
/// changed.
///
/// The tree becomes the commit and the base becomes the commit's parent, so the
/// diff on screen is the commit itself. The buffers stay the real editable
/// files, which is what lets the user look around a commit and then step on
/// without a screen of its own to leave first.
///
/// The git work runs on the git queue, so this only queues it. A step taken
/// while a checkout is out moves the cursor, and the landing queues again for
/// where the cursor stands.
pub fn walk_navigate_as(stoat: &mut Stoat, kind: WalkLandingKind) -> UpdateEffect {
    let Some((workdir, sha, standing)) = walk_position(stoat) else {
        return UpdateEffect::None;
    };
    if stoat.git_jobs.holds(GitJobKey::WalkLanding) {
        return UpdateEffect::Redraw;
    }

    queue_walk_landing(stoat, kind, workdir, sha, standing);
    UpdateEffect::Redraw
}

/// Move the walk onto commit `index` and check it out, for a walkthrough stop
/// that reads against that commit.
///
/// Queues the checkout even when the cursor already stands there, since the
/// tour still needs a landing to arrive on. A landing over the commit already
/// checked out skips the checkout itself.
pub(super) fn walk_seek(stoat: &mut Stoat, index: usize) -> UpdateEffect {
    let Some(walk) = stoat.active_workspace_mut().review_walk.as_mut() else {
        return UpdateEffect::None;
    };
    walk.seek(index);
    walk_navigate_as(stoat, WalkLandingKind::Walkthrough)
}

pub(crate) fn review_next_commit(stoat: &mut Stoat) -> UpdateEffect {
    walk_step(stoat, 1)
}

pub(crate) fn review_prev_commit(stoat: &mut Stoat) -> UpdateEffect {
    walk_step(stoat, -1)
}

fn walk_step(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    let Some(walk) = stoat.active_workspace_mut().review_walk.as_mut() else {
        return UpdateEffect::None;
    };
    if !walk.step(delta) {
        return UpdateEffect::Redraw;
    }
    walk_navigate_as(stoat, WalkLandingKind::Walk)
}

/// End the walk and queue the checkout that puts the working tree back where it
/// started.
///
/// The walk ends at the press, so a step checkout still out applies nothing
/// when it lands. The return runs on the git queue behind that checkout. The
/// press drops a step that still waits its turn, so no step checkout follows
/// the return.
///
/// The ended walk waits in
/// [`Workspace::ending_walk`](crate::workspace::Workspace::ending_walk) until
/// the return's turn. An amend queued before the press lands first and moves
/// the return ref onto the commit it wrote. One return waits at a time, so a
/// press while one is out keeps its walk and reports why.
///
/// A failed return puts the walk back. The user then fixes whatever blocked it
/// and tries again, and does not stay detached with no record of where they
/// came from.
pub(crate) fn review_done(stoat: &mut Stoat) -> UpdateEffect {
    let Some(walk) = stoat.active_workspace_mut().review_walk.take() else {
        return UpdateEffect::None;
    };
    if stoat.active_workspace().ending_walk.is_some() {
        restore_walk(stoat, walk);
        return review_error(stoat, "the last walk is still returning", None);
    }
    // A tour that spans commits plays over this walk, so it ends with it.
    if stoat
        .active_workspace()
        .walkthrough
        .as_ref()
        .is_some_and(|run| run.spans_commits())
    {
        super::walkthrough::abandon_commit_tour(stoat);
    }
    stoat.git_jobs.drop_queued(GitJobKey::WalkLanding);
    stoat.active_workspace_mut().ending_walk = Some(walk);

    let job = GitJob::new("review done", None, move |stoat: &mut Stoat| {
        let walk = stoat.active_workspace_mut().ending_walk.take()?;
        let Some(repo) = stoat.git_host.discover(&walk.workdir) else {
            restore_walk(stoat, walk);
            review_error(stoat, "not in a git repository", None);
            return None;
        };
        Some(Box::new(move || {
            let returned = match &walk.return_ref {
                ReturnRef::Branch(name) => repo.checkout_ref(name),
                ReturnRef::Detached(sha) => repo.checkout_detached(sha),
            }
            .map_err(|err| err.to_string())
            .map(|()| {
                let stash = walk.stash.as_ref()?;
                repo.stash_pop(&stash.sha, true)
                    .err()
                    .map(|err| err.to_string())
            });
            Box::new(move |stoat: &mut Stoat| land_return(stoat, walk, returned)) as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
    UpdateEffect::Redraw
}

/// Leave the diff view once a walk's return lands, with every clean buffer
/// re-read from the restored tree, or put the walk back if the return failed.
///
/// `returned` is the return checkout's error, or the error of the stash pop
/// that followed it inside `Ok`.
///
/// A return that lands closes a focused changes view, stales the diffs, and
/// reloads the commits lists, as a checkout does. It takes the walk's badge
/// down too. The badge names the commit the walk stood on, and the tree has
/// left it. A commits list the walk opened from comes back in the focused
/// pane.
///
/// The status names where HEAD went back to, and how many changes a popped
/// stash put back. A pop that failed ends the walk all the same, since HEAD is
/// back. Its changes stay in the stash, and the Review badge says why.
fn land_return(stoat: &mut Stoat, walk: ReviewWalk, returned: Result<Option<String>, String>) {
    let pop_error = match returned {
        Ok(pop_error) => pop_error,
        Err(err) => {
            restore_walk(stoat, walk);
            review_error(stoat, "could not return", Some(err));
            return;
        },
    };

    auto_reload::reload_clean_buffers(stoat);

    // Only a return that landed clears the base. A failed one leaves the tree at
    // the commit, and the base must still name that commit's parent.
    stoat.active_workspace_mut().set_diff_base(None);
    if stoat.active_workspace().focused_diff_changes_id().is_some() {
        super::diff_changes::close_diff_changes(stoat);
    } else {
        super::review::exit_diff_view(stoat);
    }
    stoat.active_workspace_mut().invalidate_all_diffs();
    super::commits::reload_every_list(stoat);
    let focus = stoat.active_workspace().panes.focus();
    super::commits::restore_covered_commits(stoat, focus);
    stoat
        .active_workspace_mut()
        .badges
        .remove_by_source(BadgeSource::Review);

    let mut status = match &walk.return_ref {
        ReturnRef::Branch(name) => format!("returned to {name}"),
        ReturnRef::Detached(sha) => format!("HEAD detached at {}", &sha[..sha.len().min(7)]),
    };
    match (walk.stash, pop_error) {
        (_, Some(reason)) => {
            review_error(stoat, "stash kept", Some(reason));
        },
        (Some(stash), None) => match stash.changes {
            1 => status.push_str(", 1 change restored"),
            changes => status.push_str(&format!(", {changes} changes restored")),
        },
        (None, None) => {},
    }
    stoat.set_status(status);
}

/// Put back a walk whose return did not happen.
///
/// A walk started after the press keeps its place, since the reader went on to
/// that walk.
fn restore_walk(stoat: &mut Stoat, walk: ReviewWalk) {
    stoat.active_workspace_mut().review_walk.get_or_insert(walk);
}

/// Check the walk that a restore brought back against where HEAD stands.
///
/// A restore puts a blame checkout's walk back as the session saved it. A tool
/// that moved HEAD while stoat was closed leaves that walk on a commit HEAD
/// left, and a return from it checks out over work that the walk never saw.
/// The first call after the restore reads HEAD on the git queue, and later
/// calls do nothing.
///
/// When HEAD is not the walk's commit, the landing ends the walk and its base,
/// and the Review badge names the stash, since nothing else names it then. A
/// repository that is gone ends the walk too, since a return from it fails.
pub fn check_restored_walk(stoat: &mut Stoat) {
    if !std::mem::take(&mut stoat.active_workspace_mut().walk_unchecked) {
        return;
    }

    let job = GitJob::new("walk check", None, |stoat: &mut Stoat| {
        let (workdir, sha) = {
            let walk = stoat.active_workspace().review_walk.as_ref()?;
            (walk.workdir.clone(), walk.current().sha.clone())
        };
        let Some(repo) = stoat.git_host.discover(&workdir) else {
            land_walk_check(stoat, &sha, None);
            return None;
        };
        Some(Box::new(move || {
            let head = repo.resolve_rev("HEAD");
            Box::new(move |stoat: &mut Stoat| land_walk_check(stoat, &sha, head)) as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
}

/// End the restored walk at commit `sha` unless `head`, where HEAD stands, is
/// that commit.
///
/// A walk that is no longer at `sha` stays as it is, such as one that a return
/// took while the check ran.
pub fn land_walk_check(stoat: &mut Stoat, sha: &str, head: Option<String>) {
    let ws = stoat.active_workspace_mut();
    if head.as_deref() == Some(sha)
        || ws
            .review_walk
            .as_ref()
            .is_none_or(|walk| walk.current().sha != sha)
    {
        return;
    }

    let stash = ws.review_walk.take().and_then(|walk| walk.stash);
    ws.set_diff_base(None);
    let detail = stash.map(|stash| {
        format!(
            "stash {} holds your changes",
            &stash.sha[..stash.sha.len().min(7)]
        )
    });
    review_error(stoat, "checkout ended outside stoat", detail);
}

fn dirty_tree_error(stoat: &mut Stoat) -> UpdateEffect {
    review_error(
        stoat,
        "uncommitted changes",
        Some("commit or stash tracked changes before reviewing".to_string()),
    )
}

pub(crate) fn commit_picker_close(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.take() else {
        return UpdateEffect::None;
    };
    let active_idx = stoat.active_workspace;
    picker.dispose(&mut stoat.workspaces[active_idx]);
    UpdateEffect::Redraw
}

/// Re-run the picker's filter against its input text and keep the preview
/// pointed at the selection.
///
/// Called every drive tick rather than on a change hook, because the query
/// lives in an [`crate::input_view::InputView`] the global keymap types into
/// directly, so the picker only learns about a keystroke by re-reading it.
pub(crate) fn sync_commit_picker(stoat: &mut Stoat) {
    let Some(picker) = stoat.commit_picker.as_ref() else {
        return;
    };
    let query = picker.input.text(stoat.active_workspace());
    if let Some(picker) = stoat.commit_picker.as_mut() {
        picker.refilter(&query);
    }
    ensure_selected_preview(stoat);
}

/// Poll the in-flight preview build and start one for the selection when
/// needed. Returns whether anything landed or was spawned, so the caller knows
/// a redraw is warranted.
pub fn pump_commit_picker(stoat: &mut Stoat) -> bool {
    if stoat.commit_picker.is_none() {
        return false;
    }
    let walked = install_pending_commits(stoat);
    let landed = match stoat.commit_picker.as_mut() {
        Some(picker) => poll_pending_preview(picker, &stoat.redraw_notify),
        None => return walked,
    };

    let spawned_before = pending_sha(stoat).is_some();
    ensure_selected_preview(stoat);
    let spawned_after = pending_sha(stoat).is_some();

    walked || landed || (spawned_after && !spawned_before)
}

/// Poll the background history walk and install it, reporting whether it landed.
///
/// The query is read here rather than in the picker because the input's text
/// lives in the workspace. Whatever was typed while the walk ran filters the
/// list it brings, so a user who starts typing into an empty picker gets the
/// rows they asked for rather than all of them.
fn install_pending_commits(stoat: &mut Stoat) -> bool {
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll},
    };

    let Some(mut task) = stoat
        .commit_picker
        .as_mut()
        .and_then(|picker| picker.pending_commits.take())
    else {
        return false;
    };

    let waker = stoat_scheduler::notify_waker(&stoat.redraw_notify);
    let mut cx = Context::from_waker(&waker);
    let loaded = match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(loaded) => loaded,
        Poll::Pending => {
            if let Some(picker) = stoat.commit_picker.as_mut() {
                picker.pending_commits = Some(task);
            }
            return false;
        },
    };

    match loaded {
        LoadedCommits::Root {
            commits,
            branch_tips,
        } => {
            let query = match stoat.commit_picker.as_ref() {
                Some(picker) => picker.input.text(stoat.active_workspace()),
                None => return false,
            };
            if let Some(picker) = stoat.commit_picker.as_mut() {
                picker.set_commits(commits, branch_tips, &query);
            }
        },
        LoadedCommits::Scope {
            label,
            ref_sha,
            commits,
            query_before,
        } => {
            // Whether the merge brought anything in is only knowable once the
            // range has been walked, so the refusal lands here rather than on
            // the keystroke that asked for it.
            if commits.is_empty() {
                review_error(stoat, "merge brought in no commits", None);
                return true;
            }
            if let Some(picker) = stoat.commit_picker.as_mut() {
                picker.push_scope(label, ref_sha, commits, query_before);
                // The second step of a range pick lands where the first did,
                // on the nearest tip of a branch other than the one listed.
                if picker.role == CommitPickerRole::PickRange {
                    picker.selected = picker.default_selection();
                }
            }
            // The query clears only once the drilled scope shows, so a drill
            // canceled or empty leaves the query as typed.
            set_picker_query(stoat, "");
        },
    }

    ensure_selected_preview(stoat);
    true
}

fn pending_sha(stoat: &Stoat) -> Option<String> {
    stoat
        .commit_picker
        .as_ref()?
        .pending_preview
        .as_ref()
        .map(|p| p.sha.clone())
}

/// Spawn a background [`DiffDocument`] build for the selected commit unless
/// one is already cached, or any build is already in flight.
///
/// One build at a time, whatever it is for. A dropped [`stoat_scheduler::Task`]
/// does not cancel the closure the blocking pool is already running, so
/// spawning per row would put a build for every row passed through in front of
/// the one the selection came to rest on, all of them queued on the repo lock.
///
/// Waiting costs nothing, because [`pump_commit_picker`] comes back here as
/// soon as the build lands. A selection that moved while one was running is
/// picked up then, so the picker converges on wherever it ended.
fn ensure_selected_preview(stoat: &mut Stoat) {
    let Some(picker) = stoat.commit_picker.as_mut() else {
        return;
    };
    let Some(sha) = picker.selected_commit().map(|c| c.sha.clone()) else {
        return;
    };
    if picker.preview_sessions.mark_used(&sha) || picker.pending_preview.is_some() {
        return;
    }

    let workdir = picker.workdir.clone();
    let repo = picker.repo.clone();
    let task = spawn_preview_load(
        &stoat.executor,
        repo,
        workdir,
        sha.clone(),
        stoat.language_registry.clone(),
        stoat.redraw_notify.clone(),
        super::commits::PreviewHighlights::from_stoat(stoat),
    );

    if let Some(picker) = stoat.commit_picker.as_mut() {
        picker.requested_preview = Some(sha.clone());
        picker.pending_preview = Some(PendingPreview { sha, task });
    }
}

/// Poll the pending build, caching a finished session under its sha.
///
/// The sha is checked against the one that was asked for, which holds by
/// construction while only one build runs at a time, since the two are recorded
/// together at the spawn. It is the invariant that lets a session be cached
/// without asking where the selection has moved to in the meantime.
///
/// A build still running wakes `redraw_notify` when it lands.
fn poll_pending_preview(
    picker: &mut CommitPicker,
    redraw_notify: &Arc<tokio::sync::Notify>,
) -> bool {
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll},
    };

    let Some(mut pending) = picker.pending_preview.take() else {
        return false;
    };
    let waker = stoat_scheduler::notify_waker(redraw_notify);
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut pending.task).poll(&mut cx) {
        Poll::Ready(load) => {
            if picker.requested_preview.as_deref() == Some(pending.sha.as_str()) {
                // A commit that changed nothing yields no document, and that is
                // a final answer. Recording it is what stops the pump asking
                // again on the very next pass, which never converged.
                match load.document {
                    Some(document) => picker
                        .preview_sessions
                        .insert(pending.sha, Arc::new(document)),
                    None => picker.preview_sessions.insert_empty(pending.sha),
                }
            }
            true
        },
        Poll::Pending => {
            picker.pending_preview = Some(pending);
            false
        },
    }
}

/// Spawn the blocking diff build for `sha`, waking the run loop through
/// `redraw` when it lands.
///
/// The wake is what makes the preview appear on its own. The pump reading this
/// task polls with a noop waker, so without it a session that finishes after the
/// last input event stays behind "loading diff..." until some unrelated event
/// happens to drive the pumps. It fires on a failed tree read too, since the
/// pump has a pending handle to clear either way.
fn spawn_preview_load(
    executor: &stoat_scheduler::Executor,
    repo: Arc<dyn crate::host::GitRepo>,
    workdir: PathBuf,
    sha: String,
    language_registry: Arc<stoat_language::LanguageRegistry>,
    redraw: Arc<tokio::sync::Notify>,
    highlights: super::commits::PreviewHighlights,
) -> Task<crate::commit_list::PreviewLoad> {
    executor.spawn_blocking(move || {
        let parent = repo.parent_sha(&sha);
        let document = match super::review::changed_or_whole(&*repo, parent.as_deref(), &sha) {
            Some(changes) => {
                super::review::build_document_from_changes(&language_registry, &workdir, changes)
                    .map(|mut doc| {
                        highlights.attach(&mut doc);
                        doc
                    })
            },
            None => None,
        };
        redraw.notify_one();
        // The picker paints no file-change summary and no message, so it reads
        // neither.
        crate::commit_list::PreviewLoad {
            summary: None,
            message: None,
            document,
        }
    })
}

/// Which caller queued a landing, which decides what the job guards against,
/// who reports a failure, and whether the landing follows the walk cursor.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WalkLandingKind {
    /// A review-walk step. Refuses to check out over tracked changes, since the
    /// walk is a way of reading history rather than of discarding work.
    Walk,
    /// A rebase edit pause. The stepper wrote the commit a moment ago, so the
    /// checkout is unconditional and a failure is the rebase's to report.
    EditPause,
    /// A walkthrough stop's commit. Guarded like a walk step, but its landing
    /// hands the reader to the tour rather than to the commit's first changed
    /// file, since the stop names the code to look at.
    Walkthrough,
}

impl WalkLandingKind {
    /// Whether the landing belongs to a review walk and follows its cursor.
    ///
    /// Those refuse to check out over tracked changes, apply nothing once the
    /// walk has ended, and chase a cursor that moved on while they were out.
    fn follows_walk(self) -> bool {
        matches!(self, Self::Walk | Self::Walkthrough)
    }
}

/// What a walk's first landing does beyond its checkout.
///
/// Every later landing shows its own commit's first file and stashes nothing,
/// so only the start carries one.
#[derive(Default)]
pub struct WalkEntry {
    /// The file the first landing shows in place of the commit's first changed
    /// file, for a caller that knows which file the reader stands on.
    pub open: Option<PathBuf>,
    /// The message of the stash that the first landing makes of the working
    /// tree's changes, the untracked files with them, before it checks out.
    ///
    /// A start with one accepts a tree with tracked changes, which a walk
    /// otherwise refuses.
    pub stash: Option<String>,
}

/// What one step's git work produced.
pub struct WalkLanding {
    /// The commit's parent, which becomes the diff base.
    pub parent: Option<String>,
    /// The first path the commit changed, or `None` for one that changed
    /// nothing and so has no file to open onto.
    first_path: Option<PathBuf>,
    /// The sha of the stash the landing made and how many files it took, or
    /// `None` for a landing that stashed nothing.
    stash: Option<(String, usize)>,
}

/// Why a step's git work did not land.
pub enum WalkFailure {
    /// No repository at the workdir the caller named.
    NoRepository,
    /// Tracked changes stand in the way of the checkout.
    DirtyTree,
    /// The checkout itself failed, with what the backend said.
    Checkout(String),
}

/// Queue a checkout of `sha`, which lands on the loop when the git work is
/// done.
///
/// The checkout walks the tree and writes files, and the dirty guard before it
/// walks the whole status. On a large repository that is most of a second, so
/// the work runs on the pool, behind every git write queued before it.
///
/// If the tree already sits on `sha`, the job has nothing to check out and
/// skips the dirty guard. There, the guard only rejects a tree the walk itself
/// accepts.
///
/// `standing` is the badge text for the commit, which the caller knows and the
/// work does not. The job holds `workdir` and does not read it back at the
/// landing, because a rebase pause has no walk to read it from.
pub fn queue_walk_landing(
    stoat: &mut Stoat,
    kind: WalkLandingKind,
    workdir: PathBuf,
    sha: String,
    standing: String,
) {
    let job = GitJob::new(
        "walk landing",
        Some(GitJobKey::WalkLanding),
        move |stoat: &mut Stoat| {
            walk_checkout_work(stoat, kind, workdir, sha, standing, WalkEntry::default())
        },
    );
    git_jobs::enqueue(stoat, job);
}

/// The git work that checks `sha` out for a walk or an edit pause, from a job's
/// start.
///
/// The span is read here, at the job's start, so the work knows which base a
/// range review lands against. A rebase pause has no walk and reads none.
///
/// `entry` is what a walk's start asks of its first landing, and
/// [`WalkEntry::default`] for every later one.
///
/// Returns `None`, with the failure reported, when `workdir` holds no
/// repository.
fn walk_checkout_work(
    stoat: &mut Stoat,
    kind: WalkLandingKind,
    workdir: PathBuf,
    sha: String,
    standing: String,
    entry: WalkEntry,
) -> Option<GitWork> {
    let range_base = stoat
        .active_workspace()
        .review_walk
        .as_ref()
        .filter(|_| kind.follows_walk())
        .and_then(|walk| match &walk.span {
            WalkSpan::Range { base, .. } => Some(base.clone()),
            WalkSpan::Commits => None,
        });
    let Some(repo) = stoat.git_host.discover(&workdir) else {
        report_failure(stoat, kind, WalkFailure::NoRepository);
        return None;
    };
    let stashing = entry.stash.is_some();
    Some(Box::new(move || {
        let landed = walk_landing(&*repo, kind, &workdir, &sha, range_base.as_deref(), entry);
        Box::new(move |stoat: &mut Stoat| {
            land_walk_checkout(stoat, kind, &workdir, &standing, &sha, landed, stashing);
        }) as GitLanding
    }) as GitWork)
}

/// Check `sha` out and read what the landing needs, on whatever thread calls.
///
/// Under a `range_base` the first path is the first tracked file the whole
/// range changed against that base, rather than the first file the commit
/// touched, since the review reads as one diff against the base. The entry's
/// `open` path is the first path under either, since its caller knows the file
/// the reader stands on.
///
/// An entry with a stash message stashes the working tree's changes first, the
/// untracked files with them, whether or not HEAD moves, so the confirm that
/// asked for the stash holds. A tree with nothing to stash goes on with no
/// stash. A checkout that then fails pops the stash back before it answers.
pub fn walk_landing(
    repo: &dyn crate::host::GitRepo,
    kind: WalkLandingKind,
    workdir: &Path,
    sha: &str,
    range_base: Option<&str>,
    entry: WalkEntry,
) -> Result<WalkLanding, WalkFailure> {
    let stash = match &entry.stash {
        Some(message) => stash_tree(repo, message)?,
        None => None,
    };
    if repo.resolve_rev("HEAD").as_deref() != Some(sha) {
        if kind.follows_walk() && entry.stash.is_none() && repo.has_tracked_changes() {
            return Err(WalkFailure::DirtyTree);
        }
        if let Err(err) = repo.checkout_detached(sha) {
            let detail = match stash.as_ref().map(|(stash, _)| repo.stash_pop(stash, true)) {
                Some(Err(pop)) => format!("{err}; the changes stay in the stash: {pop}"),
                Some(Ok(())) | None => err.to_string(),
            };
            return Err(WalkFailure::Checkout(detail));
        }
    }

    let first_path = match (entry.open, range_base) {
        (Some(path), _) => Some(path),
        (None, Some(base)) => repo
            .changed_files_from(base)
            .into_iter()
            .find(|file| !file.untracked)
            .and_then(|file| file.path.strip_prefix(workdir).ok().map(Path::to_path_buf)),
        (None, None) => repo.commit_first_path(sha),
    };
    Ok(WalkLanding {
        parent: repo.parent_sha(sha),
        first_path,
        stash,
    })
}

/// Stash the working tree's changes under `message`, the untracked files with
/// them, and answer the stash's sha and how many files it took.
///
/// A tree with nothing to stash answers `None`, since a checkout over it loses
/// nothing.
fn stash_tree(
    repo: &dyn crate::host::GitRepo,
    message: &str,
) -> Result<Option<(String, usize)>, WalkFailure> {
    let changes = repo.changed_files().len();
    match repo.stash_save(Some(message), true) {
        Ok(sha) => Ok(Some((sha, changes))),
        Err(crate::host::GitApplyError::Backend { reason, .. }) if reason == "nothing to stash" => {
            Ok(None)
        },
        Err(err) => Err(WalkFailure::Checkout(err.to_string())),
    }
}

/// Apply a landed checkout of `sha`, or report why it did not land.
///
/// A walk checkout that lands after the walk ended applies nothing and reports
/// nothing, since the reader left the walk that asked for it.
///
/// A failed landing of a start that stashed ends its walk. HEAD never moved,
/// and the stash went back into the tree, so no walk stands to return from.
///
/// A stash the landing made goes on record first, even for a walk that ended
/// while its checkout was out, since that walk's return pops it.
fn land_walk_checkout(
    stoat: &mut Stoat,
    kind: WalkLandingKind,
    workdir: &Path,
    standing: &str,
    sha: &str,
    landed: Result<WalkLanding, WalkFailure>,
    stashing: bool,
) {
    if let Ok(WalkLanding {
        stash: Some((stash, changes)),
        ..
    }) = &landed
    {
        record_stash(stoat, stash, *changes);
    }
    if kind.follows_walk() && stoat.active_workspace().review_walk.is_none() {
        return;
    }

    match landed {
        Ok(landing) => {
            auto_reload::reload_clean_buffers(stoat);
            land_walk(stoat, kind, workdir, standing, sha, landing);
        },
        Err(failure) => {
            if stashing {
                stoat.active_workspace_mut().review_walk = None;
            }
            report_failure(stoat, kind, failure);
        },
    }

    // The reader steps on while a checkout is out, so the cursor is sometimes
    // past the commit this landing describes. A checkout queued for where the
    // cursor stands now converges on it. An edit pause has no cursor to follow.
    if kind.follows_walk()
        && let Some((workdir, cursor_sha, standing)) = walk_position(stoat)
        && cursor_sha != sha
    {
        queue_walk_landing(stoat, kind, workdir, cursor_sha, standing);
    }
}

/// Record the stash `sha`, which took `changes` files, on the walk whose first
/// landing made it.
///
/// The walk still stands, or its return waits in the git queue. At most one of
/// the two holds a walk while a first landing lands, since a walk starts only
/// once the return before it has run.
fn record_stash(stoat: &mut Stoat, sha: &str, changes: usize) {
    let ws = stoat.active_workspace_mut();
    for walk in [ws.review_walk.as_mut(), ws.ending_walk.as_mut()]
        .into_iter()
        .flatten()
    {
        walk.stash = Some(WalkStash {
            sha: sha.to_string(),
            changes,
        });
    }
}

/// Show a landed commit, with its parent as the diff base and its first changed
/// path on screen.
///
/// The landing sets the diff base, names the walk's place in the Review badge,
/// and opens the path in the per-file diff, all from values the job already
/// read. A range review takes its merge base as the diff base instead, named
/// by both refs.
///
/// A walkthrough's landing hands the screen to the tour instead, which opens
/// the file its stop names on `sha`. A blame walk's landing raises no badge,
/// and it opens the blamed file at the blamed line under the changes view.
fn land_walk(
    stoat: &mut Stoat,
    kind: WalkLandingKind,
    workdir: &Path,
    standing: &str,
    sha: &str,
    landing: WalkLanding,
) {
    let walk = stoat
        .active_workspace()
        .review_walk
        .as_ref()
        .filter(|_| kind.follows_walk());
    let base = match walk.map(|walk| &walk.span) {
        Some(WalkSpan::Range {
            base,
            from,
            against,
        }) => DiffBase::review(base.clone(), from, against),
        Some(WalkSpan::Commits) | None => DiffBase::parent_of(sha, landing.parent),
    };
    let origin = walk.map(|walk| walk.origin.clone());
    stoat.active_workspace_mut().set_diff_base(Some(base));

    if let Some(WalkOrigin::Blame { path, row }) = origin {
        super::file::open_file(stoat, &workdir.join(path), OpenOrigin::Visited);
        if let Some(editor) = super::focused_editor_mut(stoat) {
            super::movement::set_cursor_row(editor, row);
        }
        super::diff_changes::open_diff_changes(stoat);
        return;
    }
    super::review::emit_review_info_badge(stoat, standing);

    if kind == WalkLandingKind::Walkthrough {
        super::walkthrough::arrive_at_commit(stoat, sha);
        return;
    }

    // A commit that changed nothing has no file to open onto, so whatever the
    // last step showed stays up and only the base moves.
    if let Some(rel_path) = landing.first_path {
        super::file::open_file(stoat, &workdir.join(rel_path), OpenOrigin::Visited);
        // A step reads the commit's file from its first change, so the view
        // opens on the change nearest the top rather than nearest the cursor
        // the last step left.
        if let Some(editor) = super::focused_editor_mut(stoat) {
            super::movement::set_cursor_row(editor, 0);
        }
        super::review::reopen_diff_view(stoat);
    }
}

/// Badge a failed landing through whoever armed it.
///
/// A walk step reports as the review walk, a rebase pause as the rebase, since
/// each is what the reader was doing when the step was taken.
fn report_failure(stoat: &mut Stoat, kind: WalkLandingKind, failure: WalkFailure) {
    let (label, detail) = match failure {
        WalkFailure::NoRepository => ("not in a git repository", None),
        WalkFailure::DirtyTree => {
            dirty_tree_error(stoat);
            return;
        },
        WalkFailure::Checkout(detail) => ("checkout failed", Some(detail)),
    };

    match kind {
        WalkLandingKind::Walk | WalkLandingKind::Walkthrough => {
            review_error(stoat, label, detail);
        },
        WalkLandingKind::EditPause => {
            super::rebase::emit_rebase_error(stoat, label, detail);
        },
    }
}
/// The workdir, sha, and badge text of the commit under the walk cursor.
///
/// A range review names its two refs in the badge, since its one commit is
/// the tip of `from` and a position among one says nothing.
fn walk_position(stoat: &Stoat) -> Option<(PathBuf, String, String)> {
    stoat.active_workspace().review_walk.as_ref().map(|walk| {
        let standing = match &walk.span {
            WalkSpan::Range { from, against, .. } => {
                format!("reviewing {from} against {against}")
            },
            WalkSpan::Commits => format!(
                "reviewing {} ({}/{})",
                walk.current().short_sha,
                walk.cursor + 1,
                walk.commits.len()
            ),
        };
        (walk.workdir.clone(), walk.current().sha.clone(), standing)
    })
}

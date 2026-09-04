use crate::{
    app::{Stoat, UpdateEffect},
    commit_list::PendingPreview,
    commit_picker::{CommitPicker, CommitPickerRole, LoadedCommits},
    host::CommitInfo,
    review_walk::{ReturnRef, ReviewWalk},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Commits the picker loads for a ref. Bounds the eager walk on a long
/// history without paging, which fuzzy filtering could not work across.
const WALK_LIMIT: usize = 1000;

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

/// List a ref's history read-only, defaulting to the current branch.
///
/// The same picker `:git-review` opens, in a role where selecting a row
/// dismisses rather than starting a walk, so browsing history never touches the
/// working tree. A named `rev` titles the modal after itself, since a list
/// rooted somewhere other than HEAD has to say where.
pub(crate) fn git_ls(stoat: &mut Stoat, rev: Option<&str>) -> UpdateEffect {
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
            // rows of their own. A base picker keeps the first-parent walk,
            // because the review chain a pick seeds is defined over
            // first-parent history.
            let commits = match role {
                CommitPickerRole::Browse => repo.log_graph(&ref_sha, WALK_LIMIT),
                CommitPickerRole::PickBase => repo.log_from(&ref_sha, WALK_LIMIT),
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

    let executor = stoat.executor.clone();
    let mut picker = CommitPicker::new(
        stoat.active_workspace_mut(),
        executor,
        role,
        workdir,
        repo.clone(),
        ref_sha,
        // Ages are measured against the moment the picker opened, so they hold
        // still while the user scrolls rather than drifting under them.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs() as i64)
            .unwrap_or(0),
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
) -> stoat_scheduler::Task<LoadedCommits> {
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

pub(crate) fn commit_picker_step(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
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
/// drill into and badges instead.
pub(crate) fn commit_picker_drill_in(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.as_ref() else {
        return UpdateEffect::None;
    };
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

    set_picker_query(stoat, "");
    UpdateEffect::Redraw
}

/// Return to the scope the last drill re-scoped away from.
///
/// The query typed in that scope comes back with it, since the user is being
/// put back where they were rather than handed a fresh list.
pub(crate) fn commit_picker_back(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.as_mut() else {
        return UpdateEffect::None;
    };
    let Some(query) = picker.pop_scope() else {
        return UpdateEffect::None;
    };
    // A drill still walking would otherwise arrive and push its scope on top of
    // the one just returned to.
    picker.pending_commits = None;

    set_picker_query(stoat, &query);
    ensure_selected_preview(stoat);
    UpdateEffect::Redraw
}

/// Write `text` into the picker's query field, replacing whatever is there.
///
/// The workspace is reached by index rather than through `active_workspace_mut`
/// so the picker borrow and the workspace borrow stay disjoint.
fn set_picker_query(stoat: &mut Stoat, text: &str) {
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
/// the diff follows a jumped selection as it does a stepped one.
pub(crate) fn commit_picker_branch(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.as_mut() else {
        return UpdateEffect::None;
    };
    picker.select_branch(dir);
    ensure_selected_preview(stoat);
    UpdateEffect::Redraw
}

/// Take the selected commit as the review base and start walking from it.
///
/// The walk spans the selected commit through the ref tip, including commits
/// the query filtered out of view. The user picked a starting point in
/// history, not a subset of rows.
pub(crate) fn commit_picker_select(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.as_ref() else {
        return UpdateEffect::None;
    };
    match picker.role {
        CommitPickerRole::PickBase => install_walk(stoat),
        CommitPickerRole::Browse => commit_picker_close(stoat),
    }
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

    let return_ref = match walk_return_ref(stoat, &workdir) {
        Ok(return_ref) => return_ref,
        Err(refused) => return refused,
    };

    // Closed only now. A refusal above leaves the picker up, so the base the
    // user picked is still selected when they come back to it.
    commit_picker_close(stoat);
    stoat.active_workspace_mut().review_walk = Some(ReviewWalk {
        workdir,
        commits,
        cursor: 0,
        return_ref,
    });
    walk_navigate(stoat)
}

/// Where a walk over `workdir` must put HEAD back, or the effect that refuses
/// to start one.
///
/// Captured before anything detaches, because afterwards there is no way to
/// tell whether the user was on a branch or already detached, and reattaching
/// someone who was detached would move a branch they never asked to move.
///
/// A tree with tracked changes refuses. A walk checks commits out, so starting
/// one over uncommitted work would either lose it or leave the checkout half
/// applied.
fn walk_return_ref(stoat: &mut Stoat, workdir: &Path) -> Result<ReturnRef, UpdateEffect> {
    let Some(repo) = stoat.git_host.discover(workdir) else {
        return Err(review_error(stoat, "not in a git repository", None));
    };
    if repo.has_tracked_changes() {
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

/// Start a walk over `commit` alone and land on it.
///
/// The commits view opens a commit this way. A walk of one steps nowhere, but
/// it carries the same checkout and the same return ref, so `:done` puts the
/// tree back exactly as it does for a walk over many.
pub(super) fn walk_one_commit(
    stoat: &mut Stoat,
    workdir: PathBuf,
    commit: CommitInfo,
) -> UpdateEffect {
    let return_ref = match walk_return_ref(stoat, &workdir) {
        Ok(return_ref) => return_ref,
        Err(refused) => return refused,
    };

    stoat.active_workspace_mut().review_walk = Some(ReviewWalk {
        workdir,
        commits: vec![commit],
        cursor: 0,
        return_ref,
    });
    walk_navigate(stoat)
}

/// Check the walk's current commit out and point `:diff` at what it changed.
///
/// The tree becomes the commit and the base becomes the commit's parent, so the
/// diff on screen is the commit itself. The buffers stay the real editable
/// files, which is what lets the user look around a commit and then step on
/// without a screen of its own to leave first.
///
/// A commit that changed nothing still moves the base, because the base names
/// what the tree is compared against and the tree did move. Only the landing is
/// skipped: there is no file the commit would open onto, so whatever the last
/// step showed stays up and the badge reports where the walk now stands.
fn walk_navigate(stoat: &mut Stoat) -> UpdateEffect {
    let Some((workdir, sha, standing)) =
        stoat.active_workspace().review_walk.as_ref().map(|walk| {
            (
                walk.workdir.clone(),
                walk.current().sha.clone(),
                format!(
                    "reviewing {} ({}/{})",
                    walk.current().short_sha,
                    walk.cursor + 1,
                    walk.commits.len()
                ),
            )
        })
    else {
        return UpdateEffect::None;
    };
    let Some(repo) = stoat.git_host.discover(&workdir) else {
        return review_error(stoat, "not in a git repository", None);
    };

    // Re-entering a commit the tree already sits on has nothing to check out,
    // so the dirty guard would only reject a tree the walk itself is fine with.
    if repo.resolve_rev("HEAD").as_deref() != Some(sha.as_str()) {
        if repo.has_tracked_changes() {
            return dirty_tree_error(stoat);
        }
        if let Err(err) = repo.checkout_detached(&sha) {
            return review_error(stoat, "checkout failed", Some(err.to_string()));
        }
    }

    super::review::emit_review_info_badge(stoat, &standing);
    super::review::land_diff_on_commit(stoat, &*repo, &workdir, &sha);
    UpdateEffect::Redraw
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
    walk_navigate(stoat)
}

/// End the walk, putting the working tree back where it started.
///
/// A failed checkout back keeps the walk, so the user can fix whatever blocked
/// it and retry rather than being stranded detached with no record of where
/// they came from.
pub(crate) fn review_done(stoat: &mut Stoat) -> UpdateEffect {
    let Some(walk) = stoat.active_workspace_mut().review_walk.take() else {
        return UpdateEffect::None;
    };
    let Some(repo) = stoat.git_host.discover(&walk.workdir) else {
        stoat.active_workspace_mut().review_walk = Some(walk);
        return review_error(stoat, "not in a git repository", None);
    };

    let restored = match &walk.return_ref {
        ReturnRef::Branch(name) => repo.checkout_ref(name),
        ReturnRef::Detached(sha) => repo.checkout_detached(sha),
    };
    if let Err(err) = restored {
        stoat.active_workspace_mut().review_walk = Some(walk);
        return review_error(stoat, "could not return", Some(err.to_string()));
    }

    // Cleared only once the tree is back. A failed return leaves the tree at the
    // commit, and a base dropped beforehand would name a base the tree is not
    // at, which is exactly the state the retry has to read correctly.
    stoat.active_workspace_mut().set_diff_base(None);
    super::review::exit_diff_view(stoat);

    UpdateEffect::Redraw
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
pub(crate) fn pump_commit_picker(stoat: &mut Stoat) -> bool {
    if stoat.commit_picker.is_none() {
        return false;
    }
    let walked = install_pending_commits(stoat);
    let landed = match stoat.commit_picker.as_mut() {
        Some(picker) => poll_pending_preview(picker),
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

    let waker = futures::task::noop_waker();
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
            }
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
fn poll_pending_preview(picker: &mut CommitPicker) -> bool {
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll},
    };

    let Some(mut pending) = picker.pending_preview.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
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
) -> stoat_scheduler::Task<crate::commit_list::PreviewLoad> {
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
        // The picker paints no file-change summary, so it reads none.
        crate::commit_list::PreviewLoad {
            summary: None,
            document,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{walk_navigate, ReturnRef, ReviewWalk};
    use crate::{
        app::Stoat, badge::BadgeSource, commit_list::Preview, commit_picker::CommitPickerRole,
        test_harness::TestHarness, workspace::diff::DiffBase,
    };
    use std::path::{Path, PathBuf};

    /// A harness whose `/repo` carries three commits, `main` on the tip and
    /// `feature` one commit back, with the workspace rooted there.
    fn harness() -> TestHarness {
        let mut h = Stoat::test();
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
            .branch("feature", "b2c3d4e5")
            .set_head_branch("main");
        h.stoat.active_workspace_mut().git_root = "/repo".into();
        // The fake host moves HEAD on a checkout but does not write the tree to
        // disk, so the working tree is seeded once at the tip and stays there.
        // A walk still diffs correctly, because every base it installs is read
        // from the commit rather than from these files.
        h.fake_fs()
            .insert_file("/repo/a.rs", b"fn a() {}\nfn a2() {}\n");
        h.fake_fs().insert_file("/repo/b.rs", b"fn b() {}\n");
        h
    }

    fn diff_base(h: &TestHarness) -> Option<Option<String>> {
        match h.stoat.active_workspace().diff_base() {
            Some(DiffBase::Rev { sha }) => Some(sha.clone()),
            _ => None,
        }
    }

    fn latched(h: &TestHarness) -> bool {
        let panes = &h.stoat.active_workspace().panes;
        panes.pane(panes.focus()).diff_mode
    }

    fn open_path(h: &TestHarness) -> Option<PathBuf> {
        h.stoat
            .active_workspace()
            .buffers
            .path_for(h.stoat.focused_editor_ids()?.1)
            .map(Path::to_path_buf)
    }

    fn review_badge(h: &TestHarness) -> Option<String> {
        let ws = h.stoat.active_workspace();
        ws.badges
            .find_by_source(BadgeSource::Review)
            .and_then(|id| ws.badges.get(id))
            .map(|badge| badge.label.clone())
    }

    /// Opening does not walk the history on the keystroke that asked for it.
    ///
    /// The walk reads and decodes up to a thousand commits under the repo lock,
    /// which is why the picker comes up empty and fills in behind itself.
    #[test]
    fn opening_the_picker_leaves_the_walk_to_a_worker() {
        let mut h = harness();
        super::git_ls(&mut h.stoat, None);

        let picker = h.stoat.commit_picker.as_ref().expect("picker is open");
        assert!(
            picker.pending_commits.is_some(),
            "the walk is out at a worker rather than already done",
        );
        assert!(picker.commits.is_empty(), "nothing has been walked yet");

        h.settle();
        let picker = h.stoat.commit_picker.as_ref().expect("picker is open");
        assert!(picker.pending_commits.is_none(), "the walk was installed");
        assert_eq!(picker.commits.len(), 3, "the seeded history arrived");
    }

    /// The pump reading the walk polls with a noop waker, so without a wake at
    /// completion the picker would sit empty after the keystroke that opened it
    /// until some unrelated event drove the pumps.
    ///
    /// The query matches no row, so the walk lands on an empty selection and no
    /// preview build follows it. That leaves the walk as the only thing that
    /// could have woken the loop, which a preview build would otherwise mask.
    #[test]
    fn a_history_walk_wakes_the_run_loop_when_it_lands() {
        use futures::FutureExt;

        let mut h = harness();

        // Drain any permit standing before the open, against an Arc clone so
        // the observer never borrows `h` across settle. Notify holds at most
        // one permit, so a single drain clears it.
        let redraw = h.stoat.redraw_notify.clone();
        let _ = redraw.notified().now_or_never();

        super::git_ls(&mut h.stoat, None);
        super::set_picker_query(&mut h.stoat, "zzzznomatch");
        h.settle();

        assert!(
            h.stoat
                .commit_picker
                .as_ref()
                .expect("picker")
                .selected_commit()
                .is_none(),
            "nothing matched, so no preview build was spawned to wake the loop",
        );

        let notified = redraw.notified();
        tokio::pin!(notified);
        assert!(
            notified.enable(),
            "the walk should wake the loop so the list appears on its own",
        );
    }

    /// Drilling into a merge defers its range walk the same way, and the list
    /// it brings replaces the one drilled from.
    #[test]
    fn drilling_into_a_merge_leaves_the_walk_to_a_worker() {
        let mut h = merged_harness();
        h.type_text(":git-ls");
        h.type_keys("enter");
        h.settle();

        let before = h
            .stoat
            .commit_picker
            .as_ref()
            .expect("picker")
            .commits
            .len();
        super::commit_picker_drill_in(&mut h.stoat);

        let picker = h.stoat.commit_picker.as_ref().expect("picker");
        assert!(
            picker.pending_commits.is_some(),
            "the range walk is out at a worker",
        );
        assert_eq!(
            picker.commits.len(),
            before,
            "the list drilled from is still showing until the walk lands",
        );

        h.settle();
        let picker = h.stoat.commit_picker.as_ref().expect("picker");
        assert!(picker.pending_commits.is_none(), "the walk was installed");
        assert_eq!(
            picker.commits.len(),
            2,
            "the merge's two branch commits replaced the list",
        );
    }

    /// A query typed while the walk is still running filters the list it
    /// brings, rather than being dropped because it arrived first.
    #[test]
    fn a_query_typed_before_the_walk_lands_filters_what_arrives() {
        let mut h = harness();
        super::git_ls(&mut h.stoat, None);
        super::set_picker_query(&mut h.stoat, "tweak");
        h.settle();

        let picker = h.stoat.commit_picker.as_ref().expect("picker");
        assert_eq!(
            picker.filtered.len(),
            1,
            "only the commit the query names is listed",
        );
        assert_eq!(
            picker.selected_commit().map(|c| c.sha.clone()),
            Some("b2c3d4e5".to_string()),
            "and the selection sits on it",
        );
    }

    /// A commit that changed nothing yields no diff to build, and the picker
    /// has to remember that rather than ask again.
    ///
    /// The preview pump spawns a build when the cache has no answer for the
    /// selected sha. An answer of "this commit has no diff" is still an answer:
    /// dropping it leaves the pump asking for the same build on every pass,
    /// which never converges.
    #[test]
    fn a_commit_with_no_diff_stops_the_picker_asking() {
        let mut h = Stoat::test();
        h.seed_linear_history(
            "/repo",
            &[
                ("aaaa1111", "feat: add a.rs", &[("a.rs", "fn a() {}\n")]),
                (
                    "bbbb2222",
                    "chore: touch nothing",
                    &[("a.rs", "fn a() {}\n")],
                ),
            ],
        );
        h.fake_git()
            .add_repo("/repo")
            .branch("main", "bbbb2222")
            .set_head_branch("main");
        h.stoat.active_workspace_mut().git_root = "/repo".into();

        h.type_text(":git-review main");
        h.type_keys("enter");
        h.settle();

        {
            let picker = h.stoat.commit_picker.as_ref().expect("picker open");
            assert_eq!(
                picker.selected_commit().map(|c| c.sha.as_str()),
                Some("bbbb2222"),
                "the empty commit is the one selected",
            );
            assert!(
                picker.pending_preview.is_none(),
                "no build is left in flight for a commit that has no diff",
            );
            assert!(
                matches!(picker.preview_sessions.get("bbbb2222"), Preview::Empty),
                "the empty answer is cached, not left looking unbuilt",
            );
        }

        h.snapshot();
        assert!(
            h.rendered_text().contains("no changes"),
            "the preview says the commit is empty rather than that it is loading",
        );
    }

    #[test]
    fn git_review_opens_the_picker_over_the_refs_history() {
        let mut h = harness();
        h.type_text(":git-review main");
        h.type_keys("enter");
        h.settle();

        let picker = h.stoat.commit_picker.as_ref().expect("picker open");
        assert_eq!(
            picker
                .commits
                .iter()
                .map(|c| c.sha.as_str())
                .collect::<Vec<_>>(),
            ["c3d4e5f6", "b2c3d4e5", "a1b2c3d4"],
            "the walk runs newest-first from the ref tip"
        );
        assert_eq!(
            picker.selected_commit().map(|c| c.sha.as_str()),
            Some("b2c3d4e5"),
            "selection starts on the nearest branch tip that is not the ref"
        );
        assert_eq!(review_badge(&h), None);
    }

    #[test]
    fn git_review_resolves_a_sha_as_well_as_a_branch() {
        let mut h = harness();
        h.type_text(":git-review b2c3d4e5");
        h.type_keys("enter");
        h.settle();

        let picker = h.stoat.commit_picker.as_ref().expect("picker open");
        assert_eq!(
            picker
                .commits
                .iter()
                .map(|c| c.sha.as_str())
                .collect::<Vec<_>>(),
            ["b2c3d4e5", "a1b2c3d4"],
            "the walk starts at the named commit, not the branch tip"
        );
    }

    #[test]
    fn an_unknown_revision_badges_without_opening_anything() {
        let mut h = harness();
        h.type_text(":git-review junk");
        h.type_keys("enter");
        h.settle();

        assert!(h.stoat.commit_picker.is_none());
        assert_eq!(review_badge(&h).as_deref(), Some("unknown revision"));
    }

    #[test]
    fn outside_a_repository_it_badges_too() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = "/elsewhere".into();
        h.type_text(":git-review main");
        h.type_keys("enter");
        h.settle();

        assert!(h.stoat.commit_picker.is_none());
        assert_eq!(review_badge(&h).as_deref(), Some("not in a git repository"));
    }

    #[test]
    fn the_picker_takes_typing_as_its_query() {
        let mut h = harness();
        h.type_text(":git-review main");
        h.type_keys("enter");
        h.settle();
        assert_eq!(h.stoat.focused_mode(), "insert", "the query field is live");

        h.type_text("widget");
        h.settle();
        assert_eq!(
            h.stoat.commit_picker.as_ref().map(|p| p.filtered.len()),
            Some(0),
            "typing filters the list rather than editing the buffer behind it"
        );
    }

    fn picker_shas(h: &TestHarness) -> Vec<String> {
        h.stoat
            .commit_picker
            .as_ref()
            .map(|p| p.commits.iter().map(|c| c.sha.clone()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn git_ls_browses_head_history() {
        let mut h = harness();
        h.type_text(":git-ls");
        h.type_keys("enter");
        h.settle();

        let picker = h.stoat.commit_picker.as_ref().expect("picker open");
        assert_eq!(picker.role, CommitPickerRole::Browse);
        assert_eq!(
            picker.selected_commit().map(|c| c.sha.as_str()),
            Some("c3d4e5f6"),
            "a browser starts at the newest commit, not a branch-tip heuristic"
        );
        assert_eq!(picker_shas(&h), ["c3d4e5f6", "b2c3d4e5", "a1b2c3d4"]);
    }

    /// The pump reading the preview task polls with a noop waker, so without a
    /// wake at completion the diff below the table stays on "loading diff..."
    /// until some unrelated event drives the pumps -- which is what made
    /// stepping the selection read as a dead key.
    #[test]
    fn a_preview_build_wakes_the_run_loop_when_it_lands() {
        use futures::FutureExt;

        let mut h = harness();
        h.type_text(":git-ls");
        h.type_keys("enter");
        h.settle();

        // Opening the picker wakes the loop too. Drain that permit against an
        // Arc clone, so the observer never borrows `h` across settle, leaving
        // the next build's wake as the only one to observe. Notify holds at
        // most one permit, so a single drain clears it.
        let redraw = h.stoat.redraw_notify.clone();
        let _ = redraw.notified().now_or_never();

        super::commit_picker_step(&mut h.stoat, 1);
        h.settle();

        let notified = redraw.notified();
        tokio::pin!(notified);
        assert!(
            notified.enable(),
            "the preview build for the newly selected commit should wake the \
             loop so the diff follows the selection on its own",
        );
    }

    /// Rows scrolled past do not each get a build of their own.
    ///
    /// A dropped task keeps running on the blocking pool, so one build per
    /// keystroke would put every row passed through ahead of the row the
    /// selection stops on, each waiting on the repo lock. Holding the count at
    /// one costs nothing, since the pump comes back for the current selection
    /// the moment the running build lands.
    #[test]
    fn stepping_the_selection_twice_leaves_one_build_running() {
        let mut h = harness();
        h.type_text(":git-ls");
        h.type_keys("enter");
        h.settle();

        // Two steps with nothing polled between them, which is what holding the
        // key down does.
        super::commit_picker_step(&mut h.stoat, 1);
        let first = h
            .stoat
            .commit_picker
            .as_ref()
            .and_then(|p| p.pending_preview.as_ref())
            .map(|p| p.sha.clone());
        assert!(
            first.is_some(),
            "the first step has to start a build, or there is nothing to block",
        );
        super::commit_picker_step(&mut h.stoat, 1);

        let picker = h.stoat.commit_picker.as_ref().expect("picker");
        assert_eq!(
            picker.pending_preview.as_ref().map(|p| p.sha.clone()),
            first,
            "the second step waits on the build the first one started",
        );

        // Whatever it started on, it ends up showing the row it stopped at.
        h.settle();
        let picker = h.stoat.commit_picker.as_ref().expect("picker");
        let selected = picker.selected_commit().expect("selection").sha.clone();
        assert!(
            matches!(picker.preview_sessions.get(&selected), Preview::Built(_)),
            "the preview for the row the selection came to rest on is cached",
        );
    }

    /// A harness whose `/repo` has a two-commit `feature` branch merged back
    /// into `main`, with the merge on the tip so `:git-ls` opens on it.
    fn merged_harness() -> TestHarness {
        let mut h = Stoat::test();
        {
            let mut repo = h.fake_git().add_repo("/repo");
            repo.commit_with_message("a1b2c3d4", "feat: add a.rs", &[("a.rs", "fn a() {}\n")])
                .commit_with_parent_message(
                    "f1111111",
                    "a1b2c3d4",
                    "feat: start the widget",
                    &[("a.rs", "fn a() {}\n"), ("w.rs", "fn w() {}\n")],
                )
                .commit_with_parent_message(
                    "f2222222",
                    "f1111111",
                    "feat: finish the widget",
                    &[("a.rs", "fn a() {}\n"), ("w.rs", "fn w() {}\nfn w2() {}\n")],
                )
                .commit_with_parent_message(
                    "b2c3d4e5",
                    "a1b2c3d4",
                    "chore: tweak a",
                    &[("a.rs", "fn a() {}\nfn a2() {}\n")],
                )
                .merge_commit(
                    "m9999999",
                    "f2222222",
                    &[
                        ("a.rs", "fn a() {}\nfn a2() {}\n"),
                        ("w.rs", "fn w() {}\nfn w2() {}\n"),
                    ],
                )
                .branch("main", "m9999999")
                .set_head_branch("main");
        }
        h.stoat.active_workspace_mut().git_root = "/repo".into();
        h
    }

    /// Every row `:git-ls` lists over [`merged_harness`], newest seeded first.
    /// The browser walks the whole DAG, so the merged branch is in here.
    const MERGED_DAG: [&str; 5] = ["m9999999", "b2c3d4e5", "f2222222", "f1111111", "a1b2c3d4"];

    fn picker_query(h: &TestHarness) -> String {
        h.stoat
            .commit_picker
            .as_ref()
            .map(|p| p.input.text(h.stoat.active_workspace()))
            .unwrap_or_default()
    }

    /// Shas of the rows the query actually leaves showing, as opposed to
    /// [`picker_shas`], which reports the whole scope regardless of filtering.
    fn picker_filtered_shas(h: &TestHarness) -> Vec<String> {
        h.stoat
            .commit_picker
            .as_ref()
            .map(|p| {
                p.filtered
                    .iter()
                    .map(|&idx| p.commits[idx].sha.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn browsing_lists_merged_branches_while_a_base_pick_does_not() {
        let mut h = merged_harness();
        h.type_text(":git-ls");
        h.type_keys("enter");
        h.settle();
        assert_eq!(
            picker_shas(&h),
            ["m9999999", "b2c3d4e5", "f2222222", "f1111111", "a1b2c3d4"],
            "a browser walks the real DAG, so the merged branch is listed"
        );

        h.type_keys("escape");
        h.settle();
        h.type_text(":git-review main");
        h.type_keys("enter");
        h.settle();
        assert_eq!(
            picker_shas(&h),
            ["m9999999", "b2c3d4e5", "a1b2c3d4"],
            "a base pick keeps first-parent history, which the review walks"
        );
    }

    #[test]
    fn drilling_a_merge_lists_only_the_branch_it_brought_in() {
        let mut h = merged_harness();
        h.type_text(":git-ls");
        h.type_keys("enter");
        h.settle();
        assert_eq!(picker_shas(&h), MERGED_DAG);

        h.type_keys("alt-right");
        h.settle();

        assert_eq!(
            picker_shas(&h),
            ["f2222222", "f1111111"],
            "the merged branch's own commits, without the mainline"
        );
        let picker = h.stoat.commit_picker.as_ref().expect("picker open");
        assert_eq!(picker.scope_label.as_deref(), Some("merge m999999"));
        assert_eq!(picker.selected, 0);
    }

    #[test]
    fn leaving_a_drilled_scope_restores_the_list_and_the_query() {
        let mut h = merged_harness();
        h.type_text(":git-ls");
        h.type_keys("enter");
        h.settle();

        h.type_text("9999");
        h.settle();
        assert_eq!(
            picker_filtered_shas(&h),
            ["m9999999"],
            "narrowed to the merge"
        );

        h.type_keys("alt-right");
        h.settle();
        assert_eq!(picker_shas(&h), ["f2222222", "f1111111"]);
        assert_eq!(picker_query(&h), "", "a drilled scope opens unfiltered");
        assert_eq!(picker_filtered_shas(&h), ["f2222222", "f1111111"]);

        h.type_keys("down");
        h.settle();
        h.type_keys("alt-left");
        h.settle();

        assert_eq!(picker_shas(&h), MERGED_DAG);
        assert_eq!(picker_query(&h), "9999", "the query typed in that scope");
        assert_eq!(
            picker_filtered_shas(&h),
            ["m9999999"],
            "and the list it was narrowing"
        );
        assert_eq!(
            h.stoat
                .commit_picker
                .as_ref()
                .and_then(|p| p.scope_label.clone()),
            None,
            "back at the root, titled by the role again"
        );
    }

    #[test]
    fn drilling_a_row_that_is_not_a_merge_badges_and_leaves_the_list_alone() {
        let mut h = merged_harness();
        h.type_text(":git-ls");
        h.type_keys("enter");
        h.settle();

        // Filtering by sha rather than by summary word, because the fuzzy
        // matcher runs over the whole joined row and a word like "tweak" is a
        // subsequence of several of them.
        h.type_text("b2c3d4");
        h.settle();
        assert_eq!(picker_filtered_shas(&h), ["b2c3d4e5"], "one-parent commit");

        h.type_keys("alt-right");
        h.settle();

        assert_eq!(review_badge(&h).as_deref(), Some("not a merge commit"));
        assert_eq!(picker_shas(&h), MERGED_DAG);
        assert_eq!(picker_query(&h), "b2c3d4", "the query survives the refusal");
    }

    #[test]
    fn leaving_the_outermost_scope_does_nothing() {
        let mut h = merged_harness();
        h.type_text(":git-ls");
        h.type_keys("enter");
        h.settle();

        h.type_keys("alt-left");
        h.settle();

        assert_eq!(picker_shas(&h), MERGED_DAG);
        assert!(h.stoat.commit_picker.is_some(), "the picker stays open");
    }

    #[test]
    fn picking_inside_a_drilled_scope_walks_the_drilled_commits() {
        let mut h = merged_harness();
        h.type_text(":git-review main");
        h.type_keys("enter");
        h.settle();

        h.type_keys("alt-right");
        h.settle();
        assert_eq!(picker_shas(&h), ["f2222222", "f1111111"]);

        h.type_keys("down enter");
        h.settle();

        let walk = h
            .stoat
            .active_workspace()
            .review_walk
            .as_ref()
            .expect("walk installed");
        assert_eq!(
            walk.commits
                .iter()
                .map(|c| c.sha.as_str())
                .collect::<Vec<_>>(),
            ["f1111111", "f2222222"],
            "the walk covers the drilled branch, oldest first"
        );
    }

    #[test]
    fn space_capital_g_l_opens_the_browser() {
        let mut h = harness();
        h.type_keys("space G l");
        h.settle();

        assert_eq!(
            h.stoat.commit_picker.as_ref().map(|p| p.role),
            Some(CommitPickerRole::Browse),
        );
        assert_eq!(
            h.stoat.focused_mode(),
            "insert",
            "the chord releases into the picker's query field"
        );

        h.type_keys("escape");
        h.settle();
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "closing the picker leaves normal mode, not the space_git chord"
        );
    }

    #[test]
    fn selecting_while_browsing_changes_nothing() {
        let mut h = harness();
        h.type_text(":git-ls");
        h.type_keys("enter");
        h.settle();

        h.type_keys("down");
        h.type_keys("enter");
        h.settle();

        assert!(h.stoat.commit_picker.is_none(), "the picker dismisses");
        assert!(
            h.stoat.active_workspace().review_walk.is_none(),
            "browsing installs no walk"
        );
        assert!(
            checkouts(&h).is_empty(),
            "browsing never touches the working tree"
        );
    }

    #[test]
    fn git_ls_outside_a_repository_badges() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = "/elsewhere".into();
        h.type_text(":git-ls");
        h.type_keys("enter");
        h.settle();

        assert!(h.stoat.commit_picker.is_none());
        assert_eq!(review_badge(&h).as_deref(), Some("not in a git repository"));
    }

    #[test]
    fn git_ls_with_a_rev_roots_the_list_there_and_says_so() {
        let mut h = harness();
        h.type_text(":git-ls feature");
        h.type_keys("enter");
        h.settle();

        let picker = h.stoat.commit_picker.as_ref().expect("picker open");
        assert_eq!(picker.role, CommitPickerRole::Browse);
        assert_eq!(
            picker_shas(&h),
            ["b2c3d4e5", "a1b2c3d4"],
            "the branch's history, not HEAD's"
        );
        assert_eq!(picker.scope_label.as_deref(), Some("git log feature"));
    }

    #[test]
    fn git_ls_with_an_unknown_rev_badges_without_opening_anything() {
        let mut h = harness();
        h.type_text(":git-ls junk");
        h.type_keys("enter");
        h.settle();

        assert!(h.stoat.commit_picker.is_none());
        assert_eq!(review_badge(&h).as_deref(), Some("unknown revision"));
    }

    fn checkouts(h: &TestHarness) -> Vec<String> {
        h.fake_git().checkouts(Path::new("/repo"))
    }

    fn walk_shas(h: &TestHarness) -> Vec<String> {
        h.stoat
            .active_workspace()
            .review_walk
            .as_ref()
            .map(|w| w.commits.iter().map(|c| c.sha.clone()).collect())
            .unwrap_or_default()
    }

    fn walk_cursor(h: &TestHarness) -> Option<usize> {
        h.stoat
            .active_workspace()
            .review_walk
            .as_ref()
            .map(|w| w.cursor)
    }

    /// Open the picker over `main` and select the oldest commit as the base.
    fn start_walk(h: &mut TestHarness) {
        h.type_text(":git-review main");
        h.type_keys("enter");
        h.settle();
        h.type_keys("down");
        h.settle();
        assert_eq!(
            h.stoat
                .commit_picker
                .as_ref()
                .and_then(|p| p.selected_commit())
                .map(|c| c.sha.as_str()),
            Some("a1b2c3d4"),
        );
        h.type_keys("enter");
        h.settle();
    }

    #[test]
    fn selecting_a_base_installs_the_walk_oldest_first() {
        let mut h = harness();
        start_walk(&mut h);

        assert!(h.stoat.commit_picker.is_none(), "the picker closes");
        assert_eq!(walk_shas(&h), ["a1b2c3d4", "b2c3d4e5", "c3d4e5f6"]);
        assert_eq!(walk_cursor(&h), Some(0));
        assert_eq!(checkouts(&h), ["detached:a1b2c3d4"]);
    }

    /// The base is the commit's parent, so what the reader sees is the commit.
    /// A root commit has no parent, and the empty tree is the base that makes
    /// every one of its lines read as added rather than as nothing at all.
    #[test]
    fn a_walk_lands_on_the_root_commits_file_against_the_empty_tree() {
        let mut h = harness();
        start_walk(&mut h);

        assert_eq!(
            (diff_base(&h), latched(&h), open_path(&h)),
            (Some(None), true, Some(PathBuf::from("/repo/a.rs"))),
        );
    }

    /// Stepping re-points the base at the new commit's parent and opens what
    /// that commit changed, which is not the file the previous step left open.
    #[test]
    fn a_step_opens_what_the_new_commit_changed() {
        let mut h = harness();
        start_walk(&mut h);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
        h.settle();
        assert_eq!(
            (diff_base(&h), open_path(&h)),
            (
                Some(Some("a1b2c3d4".to_string())),
                Some(PathBuf::from("/repo/a.rs"))
            ),
            "the middle commit touched a.rs",
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
        h.settle();
        assert_eq!(
            (diff_base(&h), latched(&h), open_path(&h)),
            (
                Some(Some("b2c3d4e5".to_string())),
                true,
                Some(PathBuf::from("/repo/b.rs"))
            ),
            "the tip commit added b.rs, so the walk crosses to it",
        );
    }

    /// The badge is the only thing naming where a walk stands, since the view
    /// it lands on is the ordinary diff view rather than a screen of its own.
    #[test]
    fn the_badge_names_the_commit_and_the_position() {
        let mut h = harness();
        start_walk(&mut h);
        assert_eq!(review_badge(&h).as_deref(), Some("reviewing a1b2c3d (1/3)"));

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
        h.settle();
        assert_eq!(review_badge(&h).as_deref(), Some("reviewing b2c3d4e (2/3)"));
    }
    /// Drive a walk without the commit picker that normally installs one.
    ///
    /// The picker spawns a preview build per selected commit, and a commit
    /// whose diff is empty is exactly what it cannot cache, so opening the
    /// picker over a history holding one never settles. That is a defect in the
    /// picker rather than in the walk, and the walk is what this covers.
    fn seed_walk(h: &mut TestHarness, shas: &[&str]) {
        let repo = h.stoat.git_host.discover(Path::new("/repo")).expect("repo");
        let commits = shas
            .iter()
            .map(|sha| {
                repo.log_from(sha, 1)
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| panic!("no commit {sha}"))
            })
            .collect();
        h.stoat.active_workspace_mut().review_walk = Some(ReviewWalk {
            workdir: PathBuf::from("/repo"),
            commits,
            cursor: 0,
            return_ref: ReturnRef::Branch("main".to_string()),
        });
        walk_navigate(&mut h.stoat);
        h.settle();
    }

    /// A commit that changed nothing has no file to land on. The base still
    /// follows the tree onto it, so the view left up from the last step reads
    /// against the right commit rather than against a stale one.
    #[test]
    fn an_empty_commit_moves_the_base_and_leaves_the_view_alone() {
        let mut h = Stoat::test();
        h.seed_linear_history(
            "/repo",
            &[
                ("a1b2c3d4", "feat: add a.rs", &[("a.rs", "fn a() {}\n")]),
                ("b2c3d4e5", "chore: nothing", &[("a.rs", "fn a() {}\n")]),
            ],
        );
        h.stoat.active_workspace_mut().git_root = "/repo".into();
        h.fake_fs().insert_file("/repo/a.rs", b"fn a() {}\n");
        seed_walk(&mut h, &["a1b2c3d4", "b2c3d4e5"]);

        let landed = open_path(&h);
        assert_eq!(landed, Some(PathBuf::from("/repo/a.rs")), "the root landed");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
        h.settle();
        assert_eq!(
            (diff_base(&h), open_path(&h), review_badge(&h)),
            (
                Some(Some("a1b2c3d4".to_string())),
                landed,
                Some("reviewing b2c3d4e (2/2)".to_string())
            ),
            "the base moved to the empty commit's parent and the view stayed",
        );
    }

    /// A walk that opens on an empty commit has nothing to show, so it shows
    /// nothing. Arming the view regardless would widen the pane over whichever
    /// buffer happened to be focused and call it the commit's diff.
    #[test]
    fn a_walk_opening_on_an_empty_commit_arms_nothing() {
        let mut h = Stoat::test();
        h.seed_linear_history(
            "/repo",
            &[
                ("a1b2c3d4", "feat: add a.rs", &[("a.rs", "fn a() {}\n")]),
                ("b2c3d4e5", "chore: nothing", &[("a.rs", "fn a() {}\n")]),
            ],
        );
        h.stoat.active_workspace_mut().git_root = "/repo".into();
        h.fake_fs().insert_file("/repo/a.rs", b"fn a() {}\n");
        seed_walk(&mut h, &["b2c3d4e5"]);

        assert_eq!(
            (diff_base(&h), latched(&h), review_badge(&h)),
            (
                Some(Some("a1b2c3d4".to_string())),
                false,
                Some("reviewing b2c3d4e (1/1)".to_string())
            ),
            "the base tracks the commit but no view opened over it",
        );
    }

    /// Ending the walk puts the diff back where the tree goes: off the commit
    /// and onto the working tree's own base.
    #[test]
    fn review_done_clears_the_base_and_the_latch() {
        let mut h = harness();
        start_walk(&mut h);
        assert!(latched(&h), "the walk armed the view");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
        h.settle();
        assert_eq!(
            (diff_base(&h), latched(&h)),
            (None, false),
            "the base and the latch went with the walk",
        );
    }

    /// A return that fails leaves the tree at the commit, so the base has to
    /// keep naming that commit's parent. Dropping it would leave the statusline
    /// claiming a base the files on disk are not measured against.
    #[test]
    fn a_failed_return_keeps_the_base() {
        let mut h = harness();
        h.fake_git().add_repo("/repo").set_head_branch("gone");
        start_walk(&mut h);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
        h.settle();
        assert_eq!(
            (diff_base(&h), latched(&h)),
            (Some(None), true),
            "the view still reads against the commit the tree is at",
        );
    }

    #[test]
    fn the_walk_spans_history_the_query_hid() {
        let mut h = harness();
        h.type_text(":git-review main");
        h.type_keys("enter");
        h.settle();
        h.type_text("a1b2");
        h.settle();
        assert_eq!(
            h.stoat.commit_picker.as_ref().map(|p| p.filtered.len()),
            Some(1),
            "the sha query hides the two newer commits"
        );

        h.type_keys("enter");
        h.settle();
        assert_eq!(
            walk_shas(&h),
            ["a1b2c3d4", "b2c3d4e5", "c3d4e5f6"],
            "the filtered-out commits are still walked through"
        );
    }

    #[test]
    fn stepping_checks_each_commit_out_and_clamps_at_the_tip() {
        let mut h = harness();
        start_walk(&mut h);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
        h.settle();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
        h.settle();
        assert_eq!(walk_cursor(&h), Some(2));
        assert_eq!(
            checkouts(&h),
            [
                "detached:a1b2c3d4",
                "detached:b2c3d4e5",
                "detached:c3d4e5f6"
            ]
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
        h.settle();
        assert_eq!(walk_cursor(&h), Some(2), "a step past the tip is a no-op");
        assert_eq!(checkouts(&h).len(), 3, "and checks nothing else out");
    }

    #[test]
    fn stepping_back_walks_toward_the_base() {
        let mut h = harness();
        start_walk(&mut h);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
        h.settle();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewPrevCommit);
        h.settle();
        assert_eq!(walk_cursor(&h), Some(0));

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewPrevCommit);
        h.settle();
        assert_eq!(walk_cursor(&h), Some(0), "a step past the base is a no-op");
    }

    #[test]
    fn a_dirty_tree_refuses_to_start_the_walk() {
        let mut h = harness();
        h.fake_git()
            .add_repo("/repo")
            .modified("a.rs", "fn a() {}\n", "fn a() { edited }\n");

        start_walk(&mut h);

        assert!(h.stoat.active_workspace().review_walk.is_none());
        assert_eq!(review_badge(&h).as_deref(), Some("uncommitted changes"));
        assert!(checkouts(&h).is_empty(), "nothing was checked out");
        assert!(
            h.stoat.commit_picker.is_some(),
            "the picker stays open so the base is not lost"
        );
    }

    #[test]
    fn review_done_returns_to_the_starting_branch() {
        let mut h = harness();
        start_walk(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewNextCommit);
        h.settle();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
        h.settle();

        assert!(h.stoat.active_workspace().review_walk.is_none());
        assert_eq!(
            checkouts(&h).last().map(String::as_str),
            Some("ref:main"),
            "the branch HEAD was attached to is restored"
        );
    }

    #[test]
    fn review_done_from_a_detached_head_returns_to_the_sha() {
        let mut h = Stoat::test();
        h.seed_linear_history(
            "/repo",
            &[
                ("a1b2c3d4", "one", &[("a.rs", "1\n")]),
                ("b2c3d4e5", "two", &[("a.rs", "2\n")]),
            ],
        );
        h.fake_git().add_repo("/repo").branch("main", "b2c3d4e5");
        h.stoat.active_workspace_mut().git_root = "/repo".into();

        h.type_text(":git-review main");
        h.type_keys("enter");
        h.settle();
        h.type_keys("down");
        h.type_keys("enter");
        h.settle();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
        h.settle();
        assert_eq!(
            checkouts(&h).last().map(String::as_str),
            Some("detached:b2c3d4e5"),
            "an already-detached HEAD is restored by sha, not attached to a branch"
        );
    }

    /// The fake fails a return checkout on an unknown branch rather than on a
    /// dirty tree, which it does not model. Either way this is the path where
    /// the return fails and the walk has to survive it.
    #[test]
    fn a_failed_return_keeps_the_walk() {
        let mut h = harness();
        h.fake_git().add_repo("/repo").set_head_branch("gone");
        start_walk(&mut h);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReviewDone);
        h.settle();

        assert!(
            h.stoat.active_workspace().review_walk.is_some(),
            "the walk survives so :review-done can be retried"
        );
        assert_eq!(review_badge(&h).as_deref(), Some("could not return"));
    }
}

use crate::{
    app::{Stoat, UpdateEffect},
    commit_list::PendingPreview,
    commit_picker::CommitPickerRole,
    review_session::ReviewSession,
};
use std::{path::PathBuf, sync::Arc};

pub(crate) fn commit_picker_step(stoat: &mut Stoat, delta: i32) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.as_mut() else {
        return UpdateEffect::None;
    };
    picker.move_selection(delta);
    ensure_selected_preview(stoat);
    UpdateEffect::Redraw
}

/// Take the selected commit as the review base.
pub(crate) fn commit_picker_select(stoat: &mut Stoat) -> UpdateEffect {
    let Some(picker) = stoat.commit_picker.as_ref() else {
        return UpdateEffect::None;
    };
    match picker.role {
        // FIXME: installing the walk lands with the ReviewWalk item. Until
        // then, selecting only dismisses the picker.
        CommitPickerRole::PickBase => commit_picker_close(stoat),
    }
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
    let landed = match stoat.commit_picker.as_mut() {
        Some(picker) => poll_pending_preview(picker),
        None => return false,
    };

    let spawned_before = pending_sha(stoat).is_some();
    ensure_selected_preview(stoat);
    let spawned_after = pending_sha(stoat).is_some();

    landed || (spawned_after && !spawned_before)
}

fn pending_sha(stoat: &Stoat) -> Option<String> {
    stoat
        .commit_picker
        .as_ref()?
        .pending_preview
        .as_ref()
        .map(|p| p.sha.clone())
}

/// Spawn a background [`ReviewSession`] build for the selected commit unless
/// one is already cached or in flight for that sha.
fn ensure_selected_preview(stoat: &mut Stoat) {
    let Some(picker) = stoat.commit_picker.as_ref() else {
        return;
    };
    let Some(sha) = picker.selected_commit().map(|c| c.sha.clone()) else {
        return;
    };
    if picker.preview_sessions.contains_key(&sha)
        || picker
            .pending_preview
            .as_ref()
            .is_some_and(|p| p.sha == sha)
    {
        return;
    }

    let workdir = picker.workdir.clone();
    let Some(repo) = stoat.git_host.discover(&workdir) else {
        return;
    };
    let task = spawn_preview_load(
        &stoat.executor,
        repo,
        workdir,
        sha.clone(),
        stoat.language_registry.clone(),
    );

    if let Some(picker) = stoat.commit_picker.as_mut() {
        picker.requested_preview = Some(sha.clone());
        picker.pending_preview = Some(PendingPreview { sha, task });
    }
}

/// Poll the pending build, caching a finished session under its sha. A session
/// for a sha the selection has since moved off is dropped rather than cached,
/// since the picker only ever renders the selected commit's preview.
fn poll_pending_preview(picker: &mut crate::commit_picker::CommitPicker) -> bool {
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
        Poll::Ready(Some(session)) => {
            if picker.requested_preview.as_deref() == Some(pending.sha.as_str()) {
                picker
                    .preview_sessions
                    .insert(pending.sha, Arc::new(session));
            }
            true
        },
        Poll::Ready(None) => true,
        Poll::Pending => {
            picker.pending_preview = Some(pending);
            false
        },
    }
}

fn spawn_preview_load(
    executor: &stoat_scheduler::Executor,
    repo: Arc<dyn crate::host::GitRepo>,
    workdir: PathBuf,
    sha: String,
    language_registry: Arc<stoat_language::LanguageRegistry>,
) -> stoat_scheduler::Task<Option<ReviewSession>> {
    executor.spawn_blocking(move || {
        let new_tree = repo.commit_tree(&sha)?;
        let base_tree = match repo.parent_sha(&sha) {
            Some(parent) => repo.commit_tree(&parent).unwrap_or_default(),
            None => std::collections::BTreeMap::new(),
        };
        let source = crate::review_session::ReviewSource::Commit {
            workdir: workdir.clone(),
            sha: sha.clone(),
        };
        super::review::build_session_from_trees(
            &language_registry,
            source,
            &workdir,
            &base_tree,
            &new_tree,
        )
    })
}

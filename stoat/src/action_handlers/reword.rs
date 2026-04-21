use crate::{
    action_handlers::rebase::{drive_rebase, emit_rebase_error},
    app::{Stoat, UpdateEffect},
    buffer::BufferId,
    editor_state::{EditorId, EditorState},
};
use stoat_text::Bias;

/// Release the editor + buffer slots owned by a paused reword. Called
/// on both confirm and abort so both paths converge on the same teardown.
fn drop_reword_editor(stoat: &mut Stoat, editor_id: EditorId, _buffer_id: BufferId) {
    let ws = stoat.active_workspace_mut();
    ws.editors.remove(editor_id);
    // Scratch buffers in `BufferRegistry` have no explicit release API
    // today; they live on in the registry's map until workspace teardown.
    // That's fine for reword: each pause allocates a fresh scratch, and
    // scratches are cheap.
}

pub(super) fn reword_abort(stoat: &mut Stoat) -> UpdateEffect {
    use crate::rebase::RebasePause;
    let pause_editor = {
        let ws = stoat.active_workspace();
        ws.rebase_active
            .as_ref()
            .and_then(|a| a.pause.as_ref())
            .and_then(|p| match p {
                RebasePause::Reword {
                    editor_id,
                    buffer_id,
                    ..
                } => Some((*editor_id, *buffer_id)),
                _ => None,
            })
    };
    if let Some((editor_id, buffer_id)) = pause_editor {
        drop_reword_editor(stoat, editor_id, buffer_id);
    }
    stoat.active_workspace_mut().rebase_active = None;
    emit_rebase_error(
        stoat,
        "rebase aborted during reword",
        Some("HEAD left at partial rebase state".into()),
    );
    stoat.mode = if stoat.active_workspace().commits.is_some() {
        "commits".into()
    } else {
        "normal".into()
    };
    UpdateEffect::Redraw
}

pub(super) fn reword_confirm(stoat: &mut Stoat) -> UpdateEffect {
    use crate::{host::GitApplyError, rebase::RebasePause};

    let (workdir, picked_sha, new_message, fallback_parent, editor_id, buffer_id) = {
        let Some(active) = stoat.active_workspace().rebase_active.as_ref() else {
            return UpdateEffect::None;
        };
        let Some(RebasePause::Reword {
            cherry_picked_commit,
            editor_id,
            buffer_id,
            ..
        }) = active.pause.as_ref()
        else {
            return UpdateEffect::None;
        };
        let buffer_text = stoat
            .active_workspace()
            .buffers
            .get(*buffer_id)
            .map(|b| b.read().expect("poisoned").rope().to_string())
            .unwrap_or_default();
        (
            active.workdir.clone(),
            cherry_picked_commit.clone(),
            buffer_text,
            Some(active.current_head.clone()),
            *editor_id,
            *buffer_id,
        )
    };

    // Empty (whitespace-only) message auto-aborts, matching git's
    // behaviour when the commit message file is emptied by the editor.
    if new_message.trim().is_empty() {
        drop_reword_editor(stoat, editor_id, buffer_id);
        stoat.active_workspace_mut().rebase_active = None;
        emit_rebase_error(
            stoat,
            "rebase aborted: empty commit message",
            Some("HEAD left at partial rebase state".into()),
        );
        stoat.mode = if stoat.active_workspace().commits.is_some() {
            "commits".into()
        } else {
            "normal".into()
        };
        return UpdateEffect::Redraw;
    }

    let Some(repo) = stoat.git_host.discover(&workdir) else {
        emit_rebase_error(stoat, "git repo not found", None);
        return UpdateEffect::Redraw;
    };
    let Some(tree) = repo.commit_tree(&picked_sha) else {
        emit_rebase_error(stoat, "reword: commit tree unreadable", None);
        return UpdateEffect::Redraw;
    };
    let real_parent = repo.parent_sha(&picked_sha).or(fallback_parent);
    let trimmed_message = new_message.trim().to_string();
    match repo.create_commit(
        real_parent.as_deref(),
        &tree,
        &trimmed_message,
        "stoat",
        "stoat@example.invalid",
    ) {
        Ok(new_sha) => {
            drop_reword_editor(stoat, editor_id, buffer_id);
            let Some(active) = stoat.active_workspace_mut().rebase_active.as_mut() else {
                return UpdateEffect::None;
            };
            active.current_head = new_sha.clone();
            active.last_pick_sha = Some(new_sha.clone());
            active.last_message = Some(trimmed_message);
            active.pause = None;
            drive_rebase(stoat)
        },
        Err(GitApplyError::Backend(msg)) => {
            emit_rebase_error(stoat, "reword failed", Some(msg));
            UpdateEffect::Redraw
        },
    }
}

/// Create a scratch buffer seeded with `original_message`, wrap it in an
/// [`EditorState`], place the cursor at end, and install a
/// [`crate::rebase::RebasePause::Reword`] pointing at the new editor + buffer
/// slots. Caller is responsible for transitioning `stoat.mode` to `"reword"`
/// after this returns.
pub(super) fn install_reword_pause(
    stoat: &mut Stoat,
    cherry_picked_commit: String,
    original_message: String,
) {
    use crate::rebase::RebasePause;
    use stoat_text::SelectionGoal;

    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();

    let (buffer_id, shared_buffer) = ws.buffers.new_scratch();
    {
        let mut guard = shared_buffer.write().expect("poisoned");
        guard.edit(0..0, &original_message);
    }

    let mut editor_state = EditorState::new(buffer_id, shared_buffer, executor);
    let end_offset = original_message.len();
    {
        let snapshot = editor_state.display_map.snapshot();
        let buf_snapshot = snapshot.buffer_snapshot();
        let anchor = buf_snapshot.anchor_at(end_offset, Bias::Right);
        editor_state.selections.transform(buf_snapshot, |s| {
            let mut new = s.clone();
            new.collapse_to(anchor, SelectionGoal::None);
            new
        });
    }
    let editor_id = ws.editors.insert(editor_state);

    let Some(active) = ws.rebase_active.as_mut() else {
        // Safeguard: caller should only invoke this with an active rebase.
        ws.editors.remove(editor_id);
        return;
    };
    active.pause = Some(RebasePause::Reword {
        cherry_picked_commit,
        original_message,
        editor_id,
        buffer_id,
    });
}

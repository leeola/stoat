//! The buffer open and close lifecycle, either side of a file's time in a pane.
//!
//! Opening a file is asynchronous above a size threshold, so the pipeline here
//! is a small state machine rather than one call: a large read is handed to the
//! blocking pool, parked on [`Stoat::pending_file_opens`], and finished on the
//! main thread once it lands. Closing is the mirror half, unwinding the same
//! workspace state the open established.
//!
//! None of this dispatches an action of its own. The action entry points that
//! drive it live in [`crate::action_handlers::file`].

use crate::{
    action_handlers::{
        dispose_view, file::display_name, focused_editor_mut, gc_editor_if_unreferenced, jump,
        read_open_content, restore_covered_terminal, EditorDisposal, OpenContent,
    },
    app::{self, Stoat, UpdateEffect},
    badge::{Anchor, Badge, BadgeSource, BadgeState},
    buffer::{BufferId, SharedBuffer},
    buffer_registry::OpenOrigin,
    editor_state::{EditorId, EditorState},
    pane::{FocusTarget, PaneId, View},
    workspace::{BridgeWaiter, Workspace, WorkspaceId},
};
use lsp_types::{DidCloseTextDocumentParams, TextDocumentIdentifier};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};
use stoat_scheduler::{Executor, Task};
use stoat_text::{LineEnding, Rope};

/// Largest file opened synchronously on the main thread.
///
/// Files over this size read on the blocking pool and install once the read
/// finishes (see [`install_pending_opens`]), so a huge file or slow mount does
/// not stall input before first paint.
const OPEN_SYNC_MAX_BYTES: u64 = 1 << 20;

/// A large file reading on the blocking pool, awaiting install.
///
/// The task fills `result` with the read outcome and wakes the run loop;
/// [`install_pending_opens`] then finishes the open on the main thread. Held in
/// [`Stoat::pending_file_opens`] so the task is not dropped, which would cancel
/// the read, before it lands.
pub(crate) struct PendingFileOpen {
    path: PathBuf,
    /// The workspace that asked for this file, which is where it installs.
    ///
    /// [`PaneId`] is a per-workspace key, so `target` names a different pane in
    /// every workspace and says nothing on its own about which one meant it.
    workspace: WorkspaceId,
    target: PaneId,
    disk_mtime: Option<SystemTime>,
    origin: OpenOrigin,
    _task: Task<()>,
    result: Arc<Mutex<Option<std::io::Result<OpenContent>>>>,
    /// The commands parked on this open, which wait on the buffer the read
    /// installs as.
    ///
    /// A read that installs no buffer drops them, which releases each command
    /// as closed.
    waiters: Vec<BridgeWaiter>,
}

/// Open `path` in pane `target`, recording `origin` as how the buffer was
/// reached, which decides whether the buffer picker lists it.
pub(crate) fn open_file_in_pane(
    stoat: &mut Stoat,
    target: PaneId,
    path: &Path,
    origin: OpenOrigin,
) -> Option<BufferId> {
    let absolute = absolute_path(stoat, path);

    let meta = stoat.fs_host.metadata(&absolute).ok().flatten();
    let disk_mtime = meta.map(|m| m.modified);
    if meta.map_or(0, |m| m.len) > OPEN_SYNC_MAX_BYTES {
        spawn_pending_open(stoat, target, absolute, disk_mtime, origin);
        return None;
    }

    let content = match read_open_content(&*stoat.fs_host, &absolute) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => OpenContent::Text("\n".to_string()),
        Err(e) => {
            tracing::error!("failed to read {}: {}", absolute.display(), e);
            stoat.set_status(format!("cannot open {}: {e}", display_name(&absolute)));
            return None;
        },
    };
    let workspace = stoat.active_workspace;
    install_content(
        stoat, workspace, target, &absolute, content, disk_mtime, origin,
    )
}

/// `path` as an absolute path, where a relative one starts at the active
/// workspace's root.
fn absolute_path(stoat: &Stoat, path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    stoat.active_workspace().git_root.join(path)
}

/// Show what a read found, as the buffer or the image it turned out to be.
///
/// Shared by the synchronous open and the background one, which differ only in
/// where the read happened and how far the content was carried before it
/// arrived here.
fn install_content(
    stoat: &mut Stoat,
    workspace: WorkspaceId,
    target: PaneId,
    absolute: &Path,
    content: OpenContent,
    disk_mtime: Option<SystemTime>,
    origin: OpenOrigin,
) -> Option<BufferId> {
    match content {
        OpenContent::Text(text) => finish_open(
            stoat,
            workspace,
            target,
            absolute,
            OpenBody::Text(&text),
            disk_mtime,
            origin,
        ),
        OpenContent::Rope { rope, ending } => finish_open(
            stoat,
            workspace,
            target,
            absolute,
            OpenBody::Rope { rope, ending },
            disk_mtime,
            origin,
        ),
        OpenContent::Image { px } => {
            show_image(stoat, workspace, target, absolute, px);
            None
        },
    }
}

/// The text a finished open registers, in whichever form reached it.
///
/// A synchronous open carries the bytes it read and normalizes them here. A
/// background one did that work on the pool and carries the rope, since
/// building it is the expensive half of opening a large file.
enum OpenBody<'a> {
    Text(&'a str),
    Rope { rope: Rope, ending: LineEnding },
}

/// Point `target` at an image file, releasing whatever it showed as
/// [`replace_pane_view`] describes.
///
/// No buffer and no editor: there is nothing to edit, nothing to save, and no
/// language to serve. The pane holds the path and the size, which is everything
/// drawing it later needs.
fn show_image(
    stoat: &mut Stoat,
    workspace: WorkspaceId,
    target: PaneId,
    absolute: &Path,
    px: (u32, u32),
) {
    let executor = stoat.executor.clone();
    let Some(ws) = stoat.workspaces.get_mut(workspace) else {
        return;
    };
    if !ws.panes.contains(target) {
        return;
    }
    let image = View::Image {
        path: absolute.to_path_buf(),
        px,
    };
    replace_pane_view(ws, &executor, target, image);
    stoat.set_status(format!("{} is an image", display_name(absolute)));
}

/// Read `absolute` on the blocking pool and queue it for install into the
/// active workspace.
///
/// A no-op if the same workspace already has an open pending for the path, so
/// repeated opens of one large file spawn a single read. Another workspace
/// asking for the same file is a separate request, since the read installs
/// somewhere else.
fn spawn_pending_open(
    stoat: &mut Stoat,
    target: PaneId,
    absolute: PathBuf,
    disk_mtime: Option<SystemTime>,
    origin: OpenOrigin,
) {
    let workspace = stoat.active_workspace;
    if let Some(pending) = stoat
        .pending_file_opens
        .iter_mut()
        .find(|p| p.path == absolute && p.workspace == workspace)
    {
        // The read already under way serves this open too, and a named open
        // promotes it the way it promotes a registered buffer.
        if origin == OpenOrigin::Named {
            pending.origin = OpenOrigin::Named;
        }
        return;
    }

    let result: Arc<Mutex<Option<std::io::Result<OpenContent>>>> = Arc::new(Mutex::new(None));
    let task = {
        let result = result.clone();
        let fs_host = stoat.fs_host.clone();
        let redraw = stoat.redraw_notify.clone();
        let path = absolute.clone();
        stoat.executor.spawn_blocking(move || {
            // Normalized and roped here rather than at install. Both walk the
            // whole file, and this path exists because that file is large
            // enough for the walk to be felt on the thread that paints.
            let content = read_open_content(&*fs_host, &path).map(|content| match content {
                OpenContent::Text(text) => OpenContent::Rope {
                    ending: LineEnding::detect(&text),
                    rope: Rope::from(LineEnding::normalize(&text).as_ref()),
                },
                other => other,
            });
            *result.lock().expect("pending open mutex") = Some(content);
            redraw.notify_one();
        })
    };
    stoat.pending_file_opens.push(PendingFileOpen {
        path: absolute,
        workspace,
        target,
        disk_mtime,
        origin,
        _task: task,
        result,
        waiters: Vec::new(),
    });

    let ws = stoat.active_workspace_mut();
    ws.badges.remove_by_source(BadgeSource::FileOpen);
    ws.badges.insert(Badge {
        source: BadgeSource::FileOpen,
        anchor: Anchor::BottomRight,
        state: BadgeState::Active,
        label: "opening file".to_string(),
        detail: None,
    });
}

/// Whether a large file still on the blocking pool installs into `pane` of the
/// active workspace.
///
/// Until the read lands, the pane shows the view the file replaces. So a
/// terminal there must not take the keys meant for the buffer.
pub(crate) fn pane_awaits_open(stoat: &Stoat, pane: PaneId) -> bool {
    stoat
        .pending_file_opens
        .iter()
        .any(|p| p.workspace == stoat.active_workspace && p.target == pane)
}

/// Park `waiter` on the pending open of `path`, so it waits on the buffer the
/// read installs as.
///
/// Answers whether the active workspace has such an open. Without one, the
/// waiter drops. `path` resolves as [`open_file_in_pane`] resolves it.
pub(crate) fn hold_pending_open(stoat: &mut Stoat, path: &Path, waiter: BridgeWaiter) -> bool {
    let absolute = absolute_path(stoat, path);
    let workspace = stoat.active_workspace;
    let Some(pending) = stoat
        .pending_file_opens
        .iter_mut()
        .find(|p| p.path == absolute && p.workspace == workspace)
    else {
        return false;
    };
    pending.waiters.push(waiter);
    true
}

/// Drop every waiter that connection `client` parked on a pending open of
/// `workspace`, since the command that waited through it no longer runs.
pub(crate) fn drop_pending_waiters(stoat: &mut Stoat, workspace: WorkspaceId, client: u64) {
    for pending in stoat
        .pending_file_opens
        .iter_mut()
        .filter(|p| p.workspace == workspace)
    {
        pending.waiters.retain(|waiter| waiter.client != client);
    }
}

/// Install every pending open whose read has finished.
///
/// Called from [`Stoat::drive_background`]. Drops an open whose target pane
/// vanished while it read, and clears the [`BadgeSource::FileOpen`] badge once
/// none remain. The commands parked on an open then wait on the buffer it
/// installs as.
pub(crate) fn install_pending_opens(stoat: &mut Stoat) {
    let mut ready = Vec::new();
    let mut i = 0;
    while i < stoat.pending_file_opens.len() {
        let done = stoat.pending_file_opens[i]
            .result
            .lock()
            .expect("pending open mutex")
            .is_some();
        if done {
            ready.push(stoat.pending_file_opens.remove(i));
        } else {
            i += 1;
        }
    }

    let cleared: Vec<WorkspaceId> = ready.iter().map(|p| p.workspace).collect();

    for pending in ready {
        let content = match pending.result.lock().expect("pending open mutex").take() {
            Some(Ok(c)) => c,
            Some(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                OpenContent::Text("\n".to_string())
            },
            Some(Err(e)) => {
                tracing::error!("failed to read {}: {}", pending.path.display(), e);
                stoat.set_status(format!("cannot open {}: {e}", display_name(&pending.path)));
                continue;
            },
            None => continue,
        };
        // A workspace closed mid-read takes its panes with it, so there is
        // nothing left to install into and nowhere to redirect to.
        let Some(ws) = stoat.workspaces.get(pending.workspace) else {
            continue;
        };
        if !ws.panes.contains(pending.target) {
            continue;
        }
        let installed = install_content(
            stoat,
            pending.workspace,
            pending.target,
            &pending.path,
            content,
            pending.disk_mtime,
            pending.origin,
        );
        if let Some(buffer) = installed
            && let Some(ws) = stoat.workspaces.get_mut(pending.workspace)
        {
            for waiter in pending.waiters {
                ws.hold_buffer(buffer, waiter);
            }
        }
    }

    // The badge belongs to whichever workspace raised it, which is not
    // necessarily the one in front of the user when the queue drains.
    for id in cleared {
        if !stoat.pending_file_opens.iter().any(|p| p.workspace == id)
            && let Some(ws) = stoat.workspaces.get_mut(id)
        {
            ws.badges.remove_by_source(BadgeSource::FileOpen);
        }
    }
}

/// Open `content` as the buffer for `absolute` in `workspace` and show it in
/// `target`.
///
/// The shared tail of the sync and background open paths. It registers the
/// buffer (deduping on path), records `origin` on it, applies mtime and
/// language, notifies LSP, records the pane switch, and installs the editor.
///
/// `workspace` is named rather than taken as the active one because the
/// background path installs a read that may have finished after the user
/// switched away, and `target` is a key only that workspace can resolve.
fn finish_open(
    stoat: &mut Stoat,
    workspace: WorkspaceId,
    target: PaneId,
    absolute: &Path,
    content: OpenBody<'_>,
    disk_mtime: Option<SystemTime>,
    origin: OpenOrigin,
) -> Option<BufferId> {
    let lang = stoat.language_registry.for_path(absolute);
    let executor = stoat.executor.clone();

    let (buffer_id, buffer) = {
        let ws = &mut stoat.workspaces[workspace];
        // Opening a path already registered hands back the existing buffer and
        // discards `content`. Everything read alongside it describes a file the
        // buffer never took, so none of it may replace what is already recorded.
        // The mtime matters most. Adopting it would move the change-guard's
        // baseline onto a write the buffer never saw, leaving the guard
        // comparing that write against itself.
        let existed = ws.buffers.id_for_path(absolute).is_some();
        let (ending, (buffer_id, buffer)) = match content {
            OpenBody::Text(text) => (
                LineEnding::detect(text),
                ws.buffers.open(absolute, &LineEnding::normalize(text)),
            ),
            OpenBody::Rope { rope, ending } => (ending, ws.buffers.open_rope(absolute, rope)),
        };
        ws.buffers.note_origin(buffer_id, origin);
        if !existed {
            ws.buffers.set_line_ending(buffer_id, ending);
            if let Some(mtime) = disk_mtime {
                ws.buffers.set_disk_mtime(buffer_id, mtime);
            }
        }
        if let Some(lang) = lang
            && ws.buffers.language_for(buffer_id).is_none()
        {
            ws.buffers.set_language(buffer_id, lang);
        }
        (buffer_id, buffer)
    };

    // The buffer's own rope, not the text just read from disk. An already-open
    // buffer keeps what it holds, so its rope is what the server needs to see.
    let rope = buffer.read().expect("buffer lock").rope().clone();
    crate::lsp::session::notify_buffer_opened(stoat, workspace, buffer_id, absolute, rope);

    jump::record_pane_switch(stoat, workspace, target, buffer_id);
    show_buffer_in_pane(stoat, workspace, target, buffer_id, buffer, executor)
}

/// Show `buffer_id` in `target` by swapping the pane's editor to a fresh
/// [`EditorState`] over the buffer, garbage-collecting the outgoing one.
///
/// Returns early with the pane untouched when it already shows this buffer,
/// so re-showing an open buffer skips the editor swap. The buffer must
/// already be registered in the workspace. Callers that read from disk go
/// through [`open_file_in_pane`].
///
/// The displaced buffer becomes the pane's last accessed one, which is what
/// [`goto_last_accessed`] switches back to.
///
/// A pinned mode on the displaced editor carries onto the new one, and the pin
/// travels with it, so a chord the user still holds survives the swap. Every
/// other mode is dropped, since the new editor starts on a different buffer.
///
/// [`EditorState::pinned`] is what marks a mode as pinned. A user config that
/// still ships `*_pin` mode blocks marks it by name instead, which
/// [`app::is_pinned_mode`] answers for.
pub(crate) fn show_buffer_in_pane(
    stoat: &mut Stoat,
    workspace: WorkspaceId,
    target: PaneId,
    buffer_id: BufferId,
    buffer: SharedBuffer,
    executor: Executor,
) -> Option<BufferId> {
    let ws = &mut stoat.workspaces[workspace];
    ws.buffers.mark_shown(buffer_id);
    if let View::Editor(eid) = ws.panes.pane(target).view
        && ws
            .editors
            .get(eid)
            .is_some_and(|e| e.buffer_id == buffer_id)
    {
        return Some(buffer_id);
    }

    let old = match ws.panes.pane(target).view {
        View::Editor(eid) => Some(eid),
        _ => None,
    };
    let carried_mode = old
        .and_then(|eid| ws.editors.get(eid))
        .filter(|editor| editor.pinned || app::is_pinned_mode(&editor.mode))
        .map(|editor| (editor.mode.clone(), editor.pinned));

    stash_commits_list(ws, target, &executor);
    let mut editor = ws.seeded_editor(buffer_id, buffer, executor);
    if let Some((mode, pinned)) = carried_mode {
        editor.mode = mode;
        editor.pinned = pinned;
    }
    let new_editor_id = ws.editors.insert(editor);

    // Read before the gc below, which is free to drop the outgoing editor.
    let outgoing = old
        .and_then(|eid| ws.editors.get(eid))
        .map(|editor| editor.buffer_id)
        .filter(|id| *id != buffer_id);

    ws.panes.pane_mut(target).view = View::Editor(new_editor_id);
    if let Some(outgoing) = outgoing {
        ws.panes.pane_mut(target).record_shown(outgoing);
    }

    if let Some(old_id) = old {
        gc_editor_if_unreferenced(ws, old_id);
    }

    // A latched pane opens each buffer it navigates to as a diff, but only when
    // that file has a change against its base, a hunk or a staged mark. A clean
    // or untracked file shows plain with the latch still armed, so hopping back
    // to a modified file re-enters the diff. Neither widen nor cursor moves here.
    // The widen belongs to the latched session, and the jump target is the
    // position.
    let latched = stoat.workspaces[workspace].panes.pane(target).diff_mode;
    if latched
        && crate::action_handlers::review::ensure_diff_map(stoat, new_editor_id, buffer_id)
        && let Some(editor) = stoat.workspaces[workspace].editors.get_mut(new_editor_id)
    {
        editor.set_diff_view(true);
    }

    Some(buffer_id)
}

/// Point pane `target` at `view`, releasing the view it showed so no editor,
/// shell, or commits list outlives every pane that showed it.
///
/// A commits list moves behind the new view, as [`show_buffer_in_pane`] keeps
/// one. Any other view goes as a tab close disposes it: an editor unless
/// another pane shows it, and a shell unless another view shows it.
///
/// A shell the pane already records behind itself stays too. An open-in-term
/// request records the shell there, then opens its first path into the shell's
/// pane.
pub(crate) fn replace_pane_view(
    ws: &mut Workspace,
    executor: &Executor,
    target: PaneId,
    view: View,
) {
    stash_commits_list(ws, target, executor);
    let (outgoing, recorded) = {
        let pane = ws.panes.pane_mut(target);
        let recorded = match pane.prev_view {
            Some(View::Agent(id) | View::Terminal(id)) => Some(id),
            _ => None,
        };
        (std::mem::replace(&mut pane.view, view), recorded)
    };

    match outgoing {
        // The stash above put the list behind the new view, or disposed it when
        // the slot was taken.
        View::Commits(_) => {},
        View::Agent(id) | View::Terminal(id) if recorded == Some(id) || ws.term_shown(id) => {},
        outgoing => dispose_view(ws, executor, outgoing, EditorDisposal::GcIfUnreferenced),
    }
}

/// Keep the commits list `target` shows behind the view about to replace it,
/// so a walk opened from the list returns to it.
fn stash_commits_list(ws: &mut Workspace, target: PaneId, executor: &Executor) {
    let View::Commits(list) = ws.panes.pane(target).view else {
        return;
    };
    let pane = ws.panes.pane_mut(target);
    if pane.prev_view.is_none() {
        pane.prev_view = Some(View::Commits(list));
        return;
    }

    // The pane records one view behind its front. When that slot already holds
    // a view, such as a shell the pane covers, the list has no place to wait. A
    // list nothing reaches holds its pages and tasks for the life of the
    // workspace, which is worse than a walk that ends on its own file.
    dispose_view(ws, executor, View::Commits(list), EditorDisposal::Remove);
}

/// Show the buffer the focused pane displayed before its current one.
///
/// Repeating alternates between the pair, because the switch records the buffer
/// it leaves on the way out. The jump lands on the pane's jumplist first, so a
/// backward jump reverses it like any cross-file open.
///
/// The switch passes over a closed buffer to the one the pane showed before it.
/// Sets a status message and moves nothing when the pane has shown no other
/// buffer that is still open.
pub(crate) fn goto_last_accessed(stoat: &mut Stoat) -> UpdateEffect {
    let workspace = stoat.active_workspace;
    let resolved = {
        let ws = &mut stoat.workspaces[workspace];
        let target = match ws.focus {
            FocusTarget::SplitPane => ws.panes.focus(),
            FocusTarget::Dock(_) => return UpdateEffect::None,
        };
        let current = match ws.panes.pane(target).view {
            View::Editor(eid) => ws.editors.get(eid).map(|editor| editor.buffer_id),
            _ => None,
        };
        prior_live_buffer(ws, target, current).map(|(id, buffer)| (target, id, buffer))
    };

    let Some((target, buffer_id, buffer)) = resolved else {
        stoat.set_status("no previously shown buffer");
        return UpdateEffect::Redraw;
    };

    let executor = stoat.executor.clone();
    jump::record_pane_switch(stoat, workspace, target, buffer_id);
    show_buffer_in_pane(stoat, workspace, target, buffer_id, buffer, executor);
    UpdateEffect::Redraw
}

/// Drop the focused buffer from the workspace's
/// [`crate::buffer_registry::BufferRegistry`] and notify the LSP server via
/// [`crate::host::LspHost::did_close`].
///
/// A pane that showed the buffer over a live shell returns to that shell.
/// Each other pane that showed the buffer returns to the buffer it showed most
/// recently that is still open, and a latched diff pane re-enters the diff
/// there. A pane that has shown nothing else still open gets a fresh scratch
/// buffer, and so does an editor no pane of the active tab shows.
///
/// Refuses to close when the buffer is dirty, so unsaved edits are not
/// silently lost.
///
/// Session state keyed by the buffer goes with it, buffer-local marks
/// included. Global marks stay. They hold a path and an offset rather
/// than an anchor into the closed buffer.
pub(crate) fn close_buffer(stoat: &mut Stoat) -> UpdateEffect {
    let Some(editor) = focused_editor_mut(stoat) else {
        return UpdateEffect::None;
    };
    let buffer_id = editor.buffer_id;
    let buffer = match stoat.active_workspace().buffers.get(buffer_id) {
        Some(b) => b,
        None => return UpdateEffect::None,
    };
    if buffer.read().expect("buffer poisoned").dirty {
        tracing::warn!(target: "stoat::file", ?buffer_id, "refusing close of dirty buffer");
        return UpdateEffect::None;
    }
    close_buffer_by_id(stoat, buffer_id)
}

/// Drop `buffer_id` from the active workspace, whatever its dirty state.
///
/// The caller answers for unsaved edits. The panes, the session state, and the
/// servers are treated as [`close_buffer`] documents, and a command parked on
/// the buffer learns whether it went with unsaved edits.
pub(crate) fn close_buffer_by_id(stoat: &mut Stoat, buffer_id: BufferId) -> UpdateEffect {
    let outcome = stoat.active_workspace().bridge_outcome(buffer_id);
    let executor = stoat.executor.clone();
    let workspace = stoat.active_workspace;
    let showing: Vec<PaneId> = {
        let ws = &stoat.workspaces[workspace];
        ws.panes
            .split_pane_ids()
            .into_iter()
            .filter(|&pane| match ws.panes.pane(pane).view {
                View::Editor(eid) => ws
                    .editors
                    .get(eid)
                    .is_some_and(|editor| editor.buffer_id == buffer_id),
                _ => false,
            })
            .collect()
    };
    // No jumplist entry records these switches. Such an entry names the closed
    // buffer, which the purge below takes out of every jumplist anyway.
    for pane in showing {
        if restore_covered_terminal(stoat, pane) {
            continue;
        }
        let prior = prior_live_buffer(&mut stoat.workspaces[workspace], pane, Some(buffer_id));
        if let Some((id, buffer)) = prior {
            show_buffer_in_pane(stoat, workspace, pane, id, buffer, executor.clone());
        }
    }

    // What still holds the buffer is a pane with nothing live to return to,
    // an editor in a parked tab, or an editor no pane shows.
    let editor_ids: Vec<EditorId> = stoat
        .active_workspace()
        .editors
        .iter()
        .filter_map(|(id, e)| (e.buffer_id == buffer_id).then_some(id))
        .collect();
    for editor_id in &editor_ids {
        let ws = stoat.active_workspace_mut();
        let (new_buffer_id, new_buffer) = ws.buffers.new_scratch();
        if let Some(slot) = ws.editors.get_mut(*editor_id) {
            let redraw = ws.redraw_notify.clone();
            *slot = EditorState::new(new_buffer_id, new_buffer, executor.clone(), redraw);
        }
    }

    let path = stoat.active_workspace_mut().buffers.remove(buffer_id);
    stoat
        .active_workspace_mut()
        .release_buffer(buffer_id, path.as_deref());

    // Purge the closed buffer from every pane's history and jumplist, so no
    // later switch or walk resolves a stale entry into it.
    let ws = stoat.active_workspace_mut();
    for tree in ws.pane_trees_mut() {
        for pane_id in tree.split_pane_ids() {
            tree.pane_mut(pane_id).forget_buffer(buffer_id);
        }
    }

    stoat
        .active_workspace_mut()
        .release_bridge_waiters(buffer_id, outcome);
    stoat.marks.retain(|(id, _), _| *id != buffer_id);

    // The servers mirror one workspace. A buffer of any other was never opened
    // on them, and its id names another workspace's document in their state.
    if stoat.active_workspace != stoat.lsp_workspace {
        return UpdateEffect::Redraw;
    }
    stoat.lsp_opened.remove(&buffer_id);
    stoat.lsp_buffer_versions.remove(&buffer_id);
    stoat.lsp_pending_changes.remove(&buffer_id);
    stoat.lsp_doc_versions.remove(&buffer_id);
    stoat
        .lsp_last_delivered_text
        .lock()
        .expect("lsp text mutex")
        .remove(&buffer_id);
    stoat
        .lsp_last_delivered_buffer_version
        .lock()
        .expect("lsp version mutex")
        .remove(&buffer_id);

    // Dropping the task cancels the request, so no answer arrives for a
    // buffer the workspace no longer holds.
    stoat.pull_diagnostic_result_ids.remove(&buffer_id);
    stoat.pending_pull_diagnostics.remove(&buffer_id);
    stoat.last_pull_diagnostic_key.remove(&buffer_id);

    if let Some(path) = path
        && let Some(uri) = crate::action_handlers::lsp::path_to_uri(&path)
    {
        let params = DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier { uri },
        };
        for lsp in crate::lsp::hosts::hosts_for_buffer(stoat, buffer_id) {
            let params = params.clone();
            stoat
                .executor
                .spawn(async move {
                    if let Err(err) = lsp.did_close(params).await {
                        tracing::warn!(target: "stoat::lsp", ?err, "did_close notification failed");
                    }
                })
                .detach();
        }
    }
    UpdateEffect::Redraw
}

/// Close a buffer whose bridge waiters a quit just released, when nothing else
/// needs it.
///
/// The buffer stays when a command still waits on it, an editor still holds
/// it, or it holds unsaved edits, so the close discards no edit.
pub(crate) fn close_released_buffer(stoat: &mut Stoat, buffer_id: BufferId) {
    let needed = {
        let ws = stoat.active_workspace();
        ws.editor_bridge_waiters.contains_key(&buffer_id)
            || ws
                .editors
                .values()
                .any(|editor| editor.buffer_id == buffer_id)
            || ws
                .buffers
                .get(buffer_id)
                .is_none_or(|buffer| buffer.read().expect("buffer poisoned").dirty)
    };
    if !needed {
        close_buffer_by_id(stoat, buffer_id);
    }
}

/// The buffer `pane` showed most recently that is still open, other than
/// `current`, the one it shows.
///
/// A closed buffer never comes back, and the pane's own buffer is no switch at
/// all. Both leave the history's tail on the way, so a later walk does not
/// resolve them again.
fn prior_live_buffer(
    ws: &mut Workspace,
    pane: PaneId,
    current: Option<BufferId>,
) -> Option<(BufferId, SharedBuffer)> {
    let history = &mut ws.panes.pane_mut(pane).buffer_history;
    while let Some(&id) = history.last()
        && (Some(id) == current || ws.buffers.get(id).is_none())
    {
        history.pop();
    }
    history
        .last()
        .and_then(|&id| ws.buffers.get(id).map(|buffer| (id, buffer)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        action_handlers::{commits, dispatch},
        test_harness::{editor, TestHarness},
    };
    use stoat_action::{
        CloseBuffer, Diff, FocusLeft, GotoLastAccessed, OpenBuffer, OpenFile, SetMark, SplitRight,
    };

    fn focused_buffer_id(stoat: &mut Stoat) -> BufferId {
        focused_editor_mut(stoat).expect("editor").buffer_id
    }

    fn open_path(h: &mut TestHarness, content: &[u8]) -> (PathBuf, BufferId) {
        let root = PathBuf::from("/close-test");
        let path = root.join("file.txt");
        h.fake_fs().insert_file(&path, content);
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        let buffer_id = focused_editor_mut(&mut h.stoat).expect("editor").buffer_id;
        (path, buffer_id)
    }

    #[test]
    fn large_file_opens_on_the_background_pool() {
        use crate::badge::BadgeSource;

        let mut h = TestHarness::with_size(80, 24);
        let root = Path::new("/big");
        let path = root.join("huge.txt");
        let big = vec![b'x'; OPEN_SYNC_MAX_BYTES as usize + 16];
        h.fake_fs().insert_file(&path, &big);
        h.stoat.active_workspace_mut().git_root = root.to_path_buf();

        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });

        assert!(
            h.stoat
                .active_workspace()
                .buffers
                .id_for_path(&path)
                .is_none(),
            "a large open defers past the synchronous dispatch"
        );
        assert!(
            h.stoat
                .active_workspace()
                .badges
                .find_by_source(BadgeSource::FileOpen)
                .is_some(),
            "the pending badge shows while the read runs"
        );

        h.settle();
        install_pending_opens(&mut h.stoat);

        let buffer_id = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(&path)
            .expect("the buffer installs once the read finishes");
        assert_eq!(
            h.stoat
                .active_workspace()
                .buffers
                .get(buffer_id)
                .expect("buffer")
                .read()
                .expect("poisoned")
                .rope()
                .len(),
            big.len(),
            "the full file content lands in the buffer"
        );
        assert!(
            h.stoat
                .active_workspace()
                .badges
                .find_by_source(BadgeSource::FileOpen)
                .is_none(),
            "the badge clears once no open is pending"
        );
    }

    #[test]
    fn a_deferred_open_installs_into_the_workspace_that_asked() {
        let mut h = TestHarness::with_size(80, 24);
        let origin = h.stoat.active_workspace;
        let root = Path::new("/big-switch");
        let path = root.join("huge.txt");
        let big = vec![b'x'; OPEN_SYNC_MAX_BYTES as usize + 16];
        h.fake_fs().insert_file(&path, &big);
        h.stoat.active_workspace_mut().git_root = root.to_path_buf();

        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });

        // The read is still in flight when the user moves on.
        let elsewhere = h.create_workspace();
        h.set_active_workspace(elsewhere);

        h.settle();
        install_pending_opens(&mut h.stoat);

        assert!(
            h.stoat.workspaces[origin]
                .buffers
                .id_for_path(&path)
                .is_some(),
            "the buffer belongs to the workspace that asked for it"
        );
        assert!(
            h.stoat.workspaces[elsewhere]
                .buffers
                .id_for_path(&path)
                .is_none(),
            "and not to whichever one happened to be active"
        );
    }

    #[test]
    fn a_deferred_open_clears_the_badge_it_raised() {
        use crate::badge::BadgeSource;

        let mut h = TestHarness::with_size(80, 24);
        let origin = h.stoat.active_workspace;
        let root = Path::new("/big-badge");
        let path = root.join("huge.txt");
        let big = vec![b'x'; OPEN_SYNC_MAX_BYTES as usize + 16];
        h.fake_fs().insert_file(&path, &big);
        h.stoat.active_workspace_mut().git_root = root.to_path_buf();

        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        let elsewhere = h.create_workspace();
        h.set_active_workspace(elsewhere);

        h.settle();
        install_pending_opens(&mut h.stoat);

        assert!(
            h.stoat.workspaces[origin]
                .badges
                .find_by_source(BadgeSource::FileOpen)
                .is_none(),
            "the badge clears where it was raised, not where the user ended up"
        );
    }

    #[test]
    fn a_deferred_open_whose_workspace_closed_is_dropped() {
        // The pane it was told to fill went with the workspace, and no other
        // workspace's pane of that key has anything to do with this file.
        let mut h = TestHarness::with_size(80, 24);
        let doomed = h.create_workspace();
        h.set_active_workspace(doomed);
        let root = Path::new("/big-closed");
        let path = root.join("huge.txt");
        let big = vec![b'x'; OPEN_SYNC_MAX_BYTES as usize + 16];
        h.fake_fs().insert_file(&path, &big);
        h.stoat.active_workspace_mut().git_root = root.to_path_buf();

        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });

        let survivor = h.create_workspace();
        h.set_active_workspace(survivor);
        h.stoat.workspaces.remove(doomed);

        h.settle();
        install_pending_opens(&mut h.stoat);

        assert!(
            h.stoat.workspaces[survivor]
                .buffers
                .id_for_path(&path)
                .is_none(),
            "the read does not fall through to whoever is left"
        );
        assert!(h.stoat.pending_file_opens.is_empty(), "and is not requeued");
    }

    #[test]
    fn two_workspaces_opening_one_large_file_both_get_it() {
        let mut h = TestHarness::with_size(80, 24);
        let first = h.stoat.active_workspace;
        let root = Path::new("/big-shared");
        let path = root.join("huge.txt");
        let big = vec![b'x'; OPEN_SYNC_MAX_BYTES as usize + 16];
        h.fake_fs().insert_file(&path, &big);
        h.stoat.active_workspace_mut().git_root = root.to_path_buf();

        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });

        let second = h.create_workspace();
        h.set_active_workspace(second);
        h.stoat.active_workspace_mut().git_root = root.to_path_buf();
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });

        h.settle();
        install_pending_opens(&mut h.stoat);

        assert!(
            h.stoat.workspaces[first]
                .buffers
                .id_for_path(&path)
                .is_some(),
            "the first workspace's request lands"
        );
        assert!(
            h.stoat.workspaces[second]
                .buffers
                .id_for_path(&path)
                .is_some(),
            "and the second's is not dropped as a duplicate of it"
        );
    }

    /// A lone latin-1 byte, which no UTF-8 decoder will take.
    const NOT_UTF8: &[u8] = b"caf\xe9 au lait\n";

    #[test]
    fn opening_a_non_utf8_file_says_why() {
        let mut h = TestHarness::with_size(80, 24);
        let root = Path::new("/latin1");
        let path = root.join("cafe.txt");
        h.fake_fs().insert_file(&path, NOT_UTF8);
        h.stoat.active_workspace_mut().git_root = root.to_path_buf();

        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();

        assert!(
            h.stoat
                .active_workspace()
                .buffers
                .id_for_path(&path)
                .is_none(),
            "nothing opens"
        );
        let message = h.stoat.pending_message.as_deref().unwrap_or("");
        assert!(
            message.contains("cafe.txt") && message.contains("utf-8"),
            "the failure must name the file and what was wrong, got {message:?}"
        );
    }

    #[test]
    fn a_deferred_open_of_a_non_utf8_file_says_why() {
        let mut h = TestHarness::with_size(80, 24);
        let root = Path::new("/latin1-big");
        let path = root.join("cafe.txt");
        let mut big = vec![b'x'; OPEN_SYNC_MAX_BYTES as usize + 16];
        big.extend_from_slice(NOT_UTF8);
        h.fake_fs().insert_file(&path, &big);
        h.stoat.active_workspace_mut().git_root = root.to_path_buf();

        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        install_pending_opens(&mut h.stoat);

        assert!(
            h.stoat
                .active_workspace()
                .buffers
                .id_for_path(&path)
                .is_none(),
            "nothing installs"
        );
        let message = h.stoat.pending_message.as_deref().unwrap_or("");
        assert!(
            message.contains("cafe.txt") && message.contains("utf-8"),
            "the deferred failure must reach the user too, got {message:?}"
        );
    }

    #[test]
    fn small_file_opens_synchronously() {
        let mut h = TestHarness::with_size(80, 24);
        let root = Path::new("/small");
        let path = root.join("tiny.txt");
        h.fake_fs().insert_file(&path, b"hello\n");
        h.stoat.active_workspace_mut().git_root = root.to_path_buf();

        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });

        assert!(
            h.stoat
                .active_workspace()
                .buffers
                .id_for_path(&path)
                .is_some(),
            "a small file opens on the dispatch with no background read"
        );
    }

    #[test]
    fn open_buffer_activates_live_modified_buffer() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/open-buffer-test");
        h.fake_fs().insert_file(root.join("a.txt"), b"disk-a\n");
        h.fake_fs().insert_file(root.join("b.txt"), b"disk-b\n");
        h.stoat.active_workspace_mut().git_root = root.clone();

        dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("a.txt"),
            },
        );
        h.settle();
        let a_id = focused_buffer_id(&mut h.stoat);
        {
            let buffer = h
                .stoat
                .active_workspace()
                .buffers
                .get(a_id)
                .expect("buffer");
            buffer.write().expect("poisoned").edit(0..0, "live-edit ");
        }

        dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("b.txt"),
            },
        );
        h.settle();
        assert_ne!(
            focused_buffer_id(&mut h.stoat),
            a_id,
            "focus moved to b.txt"
        );

        dispatch(
            &mut h.stoat,
            &OpenBuffer {
                path: root.join("a.txt"),
            },
        );
        h.settle();
        assert_eq!(
            focused_buffer_id(&mut h.stoat),
            a_id,
            "OpenBuffer activates the existing buffer rather than creating a new one",
        );
        let text = {
            let buffer = h
                .stoat
                .active_workspace()
                .buffers
                .get(a_id)
                .expect("buffer");
            let guard = buffer.read().expect("poisoned");
            guard.rope().to_string()
        };
        assert_eq!(
            text, "live-edit disk-a\n",
            "the live in-memory edit must survive, proving no disk reload",
        );
    }

    /// Open `name` under `/last-accessed` in the focused pane, returning its id.
    fn open_named(h: &mut TestHarness, name: &str) -> BufferId {
        let path = Path::new("/last-accessed").join(name);
        h.fake_fs().insert_file(&path, name.as_bytes());
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/last-accessed");
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        focused_buffer_id(&mut h.stoat)
    }

    /// Open `a.txt` then `b.txt` in the focused pane, returning both ids in
    /// open order. The pane ends on `b`, with `a` as the one to switch back to.
    fn open_two(h: &mut TestHarness) -> (BufferId, BufferId) {
        (open_named(h, "a.txt"), open_named(h, "b.txt"))
    }

    /// Close `name`, a file already open under `/last-accessed`, from a second
    /// pane, then focus back on the first.
    ///
    /// Closing acts on the focused buffer, so a pane never closes a buffer out
    /// of its own history. The second pane closes it out from under the first.
    fn close_from_a_split(h: &mut TestHarness, name: &str) {
        dispatch(&mut h.stoat, &SplitRight);
        dispatch(
            &mut h.stoat,
            &OpenFile {
                path: Path::new("/last-accessed").join(name),
            },
        );
        h.settle();
        dispatch(&mut h.stoat, &CloseBuffer);
        h.settle();
        dispatch(&mut h.stoat, &FocusLeft);
    }

    #[test]
    fn goto_last_accessed_alternates_between_the_pair() {
        let mut h = Stoat::test();
        let (a, b) = open_two(&mut h);

        dispatch(&mut h.stoat, &GotoLastAccessed);
        h.settle();
        let first = focused_buffer_id(&mut h.stoat);

        dispatch(&mut h.stoat, &GotoLastAccessed);
        h.settle();
        let second = focused_buffer_id(&mut h.stoat);

        assert_eq!(
            (first, second),
            (a, b),
            "the pane walks back to the previous buffer, then returns"
        );
    }

    /// The pane's only earlier buffer is the scratch it started on, which the
    /// open dropped.
    #[test]
    fn goto_last_accessed_reports_when_the_pane_has_shown_nothing_else() {
        let mut h = Stoat::test();
        let (_path, buffer_id) = open_path(&mut h, b"only\n");

        dispatch(&mut h.stoat, &GotoLastAccessed);
        h.settle();

        assert_eq!(
            (
                focused_buffer_id(&mut h.stoat),
                h.stoat.pending_message.as_deref()
            ),
            (buffer_id, Some("no previously shown buffer")),
            "a pane with no history stays put and says so"
        );
    }

    #[test]
    fn a_split_inherits_the_buffer_history_of_its_origin() {
        let mut h = Stoat::test();
        let (a, _) = open_two(&mut h);

        dispatch(&mut h.stoat, &SplitRight);
        dispatch(&mut h.stoat, &GotoLastAccessed);
        h.settle();

        assert_eq!(
            focused_buffer_id(&mut h.stoat),
            a,
            "the new pane switches back to a, as its origin does",
        );
    }

    #[test]
    fn goto_last_accessed_reports_when_the_previous_buffer_was_closed() {
        let mut h = Stoat::test();
        let (a, b) = open_two(&mut h);
        close_from_a_split(&mut h, "a.txt");

        dispatch(&mut h.stoat, &GotoLastAccessed);
        h.settle();

        let focused_pane = h.stoat.active_workspace().panes.focus();
        assert_eq!(
            (
                h.stoat.active_workspace().buffers.get(a).is_some(),
                focused_buffer_id(&mut h.stoat),
                h.stoat.pending_message.as_deref(),
                h.stoat
                    .active_workspace()
                    .panes
                    .pane(focused_pane)
                    .buffer_history
                    .contains(&a),
            ),
            (false, b, Some("no previously shown buffer"), false),
            "a closed previous buffer reports, stays put, and leaves no entry"
        );
    }

    #[test]
    fn goto_last_accessed_walks_past_a_closed_buffer() {
        let mut h = Stoat::test();
        let (a, _) = open_two(&mut h);
        open_named(&mut h, "c.txt");
        close_from_a_split(&mut h, "b.txt");

        dispatch(&mut h.stoat, &GotoLastAccessed);
        h.settle();

        assert_eq!(
            focused_buffer_id(&mut h.stoat),
            a,
            "the switch passes over the closed b"
        );
    }

    #[test]
    fn goto_last_accessed_passes_over_the_buffer_it_shows() {
        let mut h = Stoat::test();
        let (a, _) = open_two(&mut h);
        open_named(&mut h, "c.txt");
        dispatch(&mut h.stoat, &GotoLastAccessed);
        h.settle();
        close_from_a_split(&mut h, "c.txt");

        dispatch(&mut h.stoat, &GotoLastAccessed);
        h.settle();

        assert_eq!(
            focused_buffer_id(&mut h.stoat),
            a,
            "the pane shows b, which holds the tail once c is gone"
        );
    }

    #[test]
    fn close_buffer_drops_buffer_from_registry() {
        let mut h = Stoat::test();
        let (_path, buffer_id) = open_path(&mut h, b"hello\n");
        assert!(h.stoat.active_workspace().buffers.get(buffer_id).is_some());
        assert_eq!(dispatch(&mut h.stoat, &CloseBuffer), UpdateEffect::Redraw);
        assert!(h.stoat.active_workspace().buffers.get(buffer_id).is_none());
    }

    /// The workspace keys a parse job, a diff job and its recorded version, a
    /// settle timer, an index job, and a debounce to each buffer, and caches
    /// each diffed file's HEAD and index blobs by path. Nothing else drops any
    /// of it, so a session browsing hundreds of files carried hundreds of
    /// doubled file texts until it exited.
    #[test]
    fn close_buffer_releases_its_workspace_state() {
        // A file that differs from HEAD, so the diff pipeline actually runs and
        // leaves the entries this is about.
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario("/repo", &[("a.txt", "a\nb\n", "a\nc\n")]);
        h.stoat.set_diff_warm_auto(true);
        let path = PathBuf::from("/repo/a.txt");
        h.open_file(&path);
        h.settle_diff_jobs();

        let buffer_id = focused_editor_mut(&mut h.stoat).expect("editor").buffer_id;
        assert!(
            h.stoat
                .active_workspace()
                .holds_buffer_state(buffer_id, Some(&path)),
            "the open buffer accumulated state, or this proves nothing",
        );

        assert_eq!(dispatch(&mut h.stoat, &CloseBuffer), UpdateEffect::Redraw);
        assert!(
            !h.stoat
                .active_workspace()
                .holds_buffer_state(buffer_id, Some(&path)),
            "the close took every per-buffer entry and the path's cached base",
        );

        // The reopen no longer has a cached base to reuse, so it has to diff
        // again from the repo rather than come back blank.
        h.open_file(&path);
        h.settle_diff_jobs();
        let reopened = h
            .stoat
            .active_workspace()
            .buffers
            .id_for_path(&path)
            .expect("the file reopened");
        let ws = h.stoat.active_workspace();
        assert!(
            ws.buffers
                .get(reopened)
                .expect("the reopened buffer")
                .read()
                .expect("buffer poisoned")
                .diff_map
                .is_some(),
            "the reopened file re-derived its diff from the repo",
        );
    }

    #[test]
    fn close_buffer_replaces_editor_with_scratch() {
        let mut h = Stoat::test();
        let (_path, original_id) = open_path(&mut h, b"hello\n");
        dispatch(&mut h.stoat, &CloseBuffer);
        let new_id = focused_editor_mut(&mut h.stoat).expect("editor").buffer_id;
        assert_ne!(new_id, original_id);
        let new_buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(new_id)
            .expect("scratch buffer exists");
        assert_eq!(new_buffer.read().expect("poisoned").rope().to_string(), "");
    }

    #[test]
    fn close_returns_the_pane_to_the_buffer_it_showed_before() {
        let mut h = Stoat::test();
        let (a, b) = open_two(&mut h);
        dispatch(&mut h.stoat, &CloseBuffer);
        h.settle();

        assert_eq!(
            (
                focused_buffer_id(&mut h.stoat),
                h.stoat.active_workspace().buffers.get(b).is_some()
            ),
            (a, false),
            "the pane shows a again and b is gone",
        );
    }

    #[test]
    fn repeated_closes_walk_back_through_the_shown_buffers() {
        let mut h = Stoat::test();
        let (a, b) = open_two(&mut h);
        open_named(&mut h, "c.txt");
        let close = |h: &mut TestHarness| {
            dispatch(&mut h.stoat, &CloseBuffer);
            h.settle();
            focused_buffer_id(&mut h.stoat)
        };

        let landed = [close(&mut h), close(&mut h)];
        assert_eq!(landed, [b, a], "each close returns to the buffer before");

        let last = close(&mut h);
        let ws = h.stoat.active_workspace();
        let text = ws
            .buffers
            .get(last)
            .map(|buffer| buffer.read().expect("poisoned").rope().to_string());
        assert_eq!(
            (ws.buffers.path_for(last), text),
            (None, Some(String::new())),
            "and the last close leaves a pathless empty scratch",
        );
    }

    /// A latched diff pane re-enters the diff on the file a close returns it to.
    #[test]
    fn a_close_in_the_diff_view_reopens_the_prior_file_as_a_diff() {
        let mut h = TestHarness::with_size(80, 24);
        h.stage_review_scenario(
            "/repo",
            &[("a.txt", "a\nb\n", "a\nc\n"), ("b.txt", "d\ne\n", "d\nf\n")],
        );
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.txt"));
        h.open_file(Path::new("/repo/b.txt"));
        h.settle_diff_jobs();
        dispatch(&mut h.stoat, &Diff { rev: None });
        h.settle();

        dispatch(&mut h.stoat, &CloseBuffer);
        h.settle();

        let buffer_id = focused_buffer_id(&mut h.stoat);
        let diff_view = focused_editor_mut(&mut h.stoat).expect("editor").diff_view;
        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                ws.buffers.path_for(buffer_id),
                diff_view,
                ws.panes.pane(ws.panes.focus()).diff_mode
            ),
            (Some(Path::new("/repo/a.txt")), true, true),
            "the pane returns to a.txt and shows it as a diff",
        );
    }

    /// A pane starts on a scratch, and a close leaves one behind. Opening a file
    /// moves the pane off it, and nothing else can reach it afterward.
    #[test]
    fn a_pane_moving_off_an_untouched_scratch_drops_it() {
        let mut h = Stoat::test();
        let scratch = focused_editor_mut(&mut h.stoat).expect("editor").buffer_id;
        let before = h.stoat.active_workspace().buffers.len();

        open_path(&mut h, b"hello\n");

        let ws = h.stoat.active_workspace();
        assert!(
            ws.buffers.get(scratch).is_none(),
            "the scratch the pane left goes with it",
        );
        assert_eq!(
            ws.buffers.len(),
            before,
            "so the file takes the scratch's place rather than adding to it",
        );
    }

    /// Typed text is what separates a scratch worth keeping from one to drop,
    /// and the buffer's own dirty flag is how that is read.
    #[test]
    fn a_scratch_holding_typed_text_survives_the_switch() {
        let mut h = Stoat::test();
        let scratch = focused_editor_mut(&mut h.stoat).expect("editor").buffer_id;
        {
            let buffer = h
                .stoat
                .active_workspace()
                .buffers
                .get(scratch)
                .expect("the pane opens on a scratch");
            let mut guard = buffer.write().expect("poisoned");
            guard.edit(0..0, "kept");
        }

        open_path(&mut h, b"hello\n");

        assert!(
            h.stoat
                .active_workspace()
                .buffers
                .get(scratch)
                .is_some_and(|buffer| buffer.read().expect("poisoned").dirty),
            "a scratch with unsaved text stays",
        );
    }

    /// A split shows the same buffer through a second editor, so the pane that
    /// moves off is not the last reader.
    #[test]
    fn a_scratch_a_second_split_shows_survives_the_switch() {
        let mut h = Stoat::test();
        let scratch = focused_editor_mut(&mut h.stoat).expect("editor").buffer_id;
        dispatch(&mut h.stoat, &SplitRight);
        h.settle();

        open_path(&mut h, b"hello\n");

        assert!(
            h.stoat.active_workspace().buffers.get(scratch).is_some(),
            "the split still shows it, so it is not the pane's to drop",
        );
    }

    #[test]
    fn close_buffer_clears_the_state_keyed_by_the_buffer() {
        let mut h = Stoat::test();
        let (_path, buffer_id) = open_path(&mut h, b"hello\n");
        assert!(h.stoat.lsp_opened.contains(&buffer_id));

        dispatch(&mut h.stoat, &SetMark);
        h.type_keys("a");
        h.stoat
            .pull_diagnostic_result_ids
            .insert(buffer_id, String::from("rev-1"));
        h.stoat.last_pull_diagnostic_key.insert(buffer_id, 1);
        let pending = h.stoat.spawn_woken(async { None });
        h.stoat.pending_pull_diagnostics.insert(buffer_id, pending);
        assert_eq!(h.stoat.marks.len(), 1, "the mark the keypress stored");

        dispatch(&mut h.stoat, &CloseBuffer);

        assert!(!h.stoat.lsp_opened.contains(&buffer_id));
        let retained = (
            h.stoat.pull_diagnostic_result_ids.len(),
            h.stoat.last_pull_diagnostic_key.len(),
            h.stoat.pending_pull_diagnostics.len(),
            h.stoat.marks.len(),
        );
        assert_eq!(
            retained,
            (0, 0, 0, 0),
            "result ids, pull keys, pending pulls, marks",
        );
    }

    #[test]
    fn close_buffer_refuses_when_dirty() {
        let mut h = Stoat::test();
        let (_path, buffer_id) = open_path(&mut h, b"hello\n");
        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer");
        {
            let mut guard = buffer.write().expect("poisoned");
            guard.edit(0..0, "x");
        }
        assert_eq!(dispatch(&mut h.stoat, &CloseBuffer), UpdateEffect::None);
        assert!(
            h.stoat.active_workspace().buffers.get(buffer_id).is_some(),
            "dirty buffer should not be closed",
        );
    }

    /// A raw path holding a space parses as no URI at all. A notification built
    /// that way reaches no server, and the document leaks open there for a
    /// buffer that no longer exists.
    #[test]
    fn a_close_releases_the_document_by_the_uri_the_open_registered() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/close uri");
        let path = root.join("my file.txt");
        h.fake_fs().insert_file(&path, b"hello\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        dispatch(&mut h.stoat, &CloseBuffer);
        h.settle();

        let closed: Vec<String> = h
            .fake_lsp()
            .observed_closes()
            .iter()
            .map(|params| params.text_document.uri.as_str().to_string())
            .collect();
        let opened: Vec<String> = h
            .fake_lsp()
            .observed_opens()
            .iter()
            .map(|params| params.text_document.uri.as_str().to_string())
            .collect();
        assert_eq!(opened, ["file:///close%20uri/my%20file.txt"]);
        assert_eq!(closed, opened);
    }

    #[test]
    fn close_buffer_on_scratch_buffer_succeeds() {
        let mut h = Stoat::test();
        let editor = focused_editor_mut(&mut h.stoat).expect("editor");
        let scratch_id = editor.buffer_id;
        assert!(!editor::focused_dirty(&h.stoat));
        assert_eq!(dispatch(&mut h.stoat, &CloseBuffer), UpdateEffect::Redraw);
        assert!(h.stoat.active_workspace().buffers.get(scratch_id).is_none());
    }

    /// Opening an image used to report "cannot open", which reads as a broken
    /// file rather than a file this is not an editor for.
    #[test]
    fn opening_an_image_shows_it_rather_than_refusing_it() {
        let mut h = TestHarness::with_size(40, 10);
        open_a_png(&mut h);

        let ws = h.stoat.active_workspace();
        let view = &ws.panes.pane(ws.panes.focus()).view;
        assert!(
            matches!(view, View::Image { px: (4, 2), .. }),
            "the pane shows the image and its size, got {view:?}",
        );
        assert!(
            !h.stoat
                .pending_message
                .as_deref()
                .unwrap_or_default()
                .contains("cannot open"),
            "and nothing reports it as unopenable",
        );
    }

    /// A walk opened from a commits list opens its first changed file over the
    /// list, so an image there has to keep the list for the walk to return to.
    #[test]
    fn an_image_opened_over_a_commits_list_keeps_the_list_behind_it() {
        let mut h = TestHarness::with_size(90, 16);
        h.seed_linear_history("/repo", &[("a1b2c3d4", "one", &[("a.rs", "1\n")])]);
        h.open_commits("/repo");
        let list = h.stoat.active_workspace().focused_commits_id();

        open_a_png(&mut h);

        let ws = h.stoat.active_workspace();
        let focus = ws.panes.focus();
        assert_eq!(
            (
                list.is_some(),
                matches!(ws.panes.pane(focus).view, View::Image { .. }),
                commits::covered_commits(ws, focus)
            ),
            (true, true, list),
            "the image takes the pane, and the live list waits behind it"
        );
    }

    /// An open-in-term request records the shell behind its pane before it
    /// opens its first path there, so an image in that pane leaves the shell
    /// running for the pane to return to.
    #[test]
    fn an_image_opened_over_a_shell_the_pane_records_keeps_the_shell() {
        let mut h = TestHarness::with_size(40, 10);
        dispatch(&mut h.stoat, &stoat_action::Terminal);
        let shell = {
            let ws = h.stoat.active_workspace_mut();
            let focus = ws.panes.focus();
            let View::Terminal(shell) = ws.panes.pane(focus).view else {
                panic!("the terminal action shows a shell");
            };
            ws.panes.pane_mut(focus).prev_view = Some(View::Terminal(shell));
            shell
        };

        open_a_png(&mut h);

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                matches!(ws.panes.pane(ws.panes.focus()).view, View::Image { .. }),
                ws.terms.contains_key(shell)
            ),
            (true, true),
            "the image opens in the shell's pane, and the recorded shell lives on"
        );
    }

    /// Write a 4x2 PNG to `/repo/pic.png` and open it in the focused pane.
    fn open_a_png(h: &mut TestHarness) {
        let png = {
            let buffer = image::RgbaImage::from_pixel(4, 2, image::Rgba([1, 2, 3, 255]));
            let mut out = std::io::Cursor::new(Vec::new());
            buffer
                .write_to(&mut out, image::ImageFormat::Png)
                .expect("encode png");
            out.into_inner()
        };
        h.fake_fs().insert_file("/repo/pic.png", png);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");

        dispatch(
            &mut h.stoat,
            &OpenFile {
                path: PathBuf::from("/repo/pic.png"),
            },
        );
        h.settle();
    }

    /// A background open normalizes and ropes off the run loop, so what
    /// installs is a rope and the ending it was read with.
    ///
    /// Neither survives the pool by accident. The rope is what the read
    /// produced, and the ending is a fact about the file the rope no longer
    /// holds, since normalizing is what took the terminators out of it.
    #[test]
    fn a_large_crlf_file_installs_its_rope_and_its_ending() {
        let mut h = TestHarness::with_size(80, 24);
        let root = Path::new("/big");
        let path = root.join("crlf.txt");

        let line = b"a line of text\r\n";
        let mut big = Vec::new();
        while big.len() <= OPEN_SYNC_MAX_BYTES as usize {
            big.extend_from_slice(line);
        }
        let lines = big.len() / line.len();
        h.fake_fs().insert_file(&path, &big);
        h.stoat.active_workspace_mut().git_root = root.to_path_buf();

        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();

        // Read before the install takes it, which is where the pool's work
        // shows. A read that only fetches bytes leaves text here and the run
        // loop the same walk over the file to do.
        {
            let pending = h.stoat.pending_file_opens.first().expect("one read is out");
            let held = pending.result.lock().expect("pending open mutex");
            let Some(Ok(OpenContent::Rope { rope, ending })) = held.as_ref() else {
                panic!("the pool hands back a rope, not the bytes it read")
            };
            assert_eq!(*ending, LineEnding::Crlf, "with the ending it detected");
            assert_eq!(
                rope.len(),
                lines * (line.len() - 1),
                "and normalized before building",
            );
        }

        install_pending_opens(&mut h.stoat);

        let ws = h.stoat.active_workspace();
        let buffer_id = ws
            .buffers
            .id_for_path(&path)
            .expect("the buffer installs once the read finishes");
        assert_eq!(
            ws.buffers.line_ending(buffer_id),
            LineEnding::Crlf,
            "the ending the pool detected is what the buffer writes back with",
        );

        let rope = ws
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .read()
            .expect("poisoned")
            .rope()
            .clone();
        assert_eq!(
            rope.len(),
            lines * (line.len() - 1),
            "and the rope holds the file with one terminator byte per line gone",
        );
    }
}

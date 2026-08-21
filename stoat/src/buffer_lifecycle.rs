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
        file::display_name, focused_editor_mut, gc_editor_if_unreferenced, jump, read_open_content,
        OpenContent,
    },
    app::{self, Stoat, UpdateEffect},
    badge::{Anchor, Badge, BadgeSource, BadgeState},
    buffer::{BufferId, SharedBuffer},
    editor_state::{EditorId, EditorState},
    pane::{FocusTarget, PaneId, View},
    workspace::WorkspaceId,
};
use lsp_types::{DidCloseTextDocumentParams, TextDocumentIdentifier};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};
use stoat_scheduler::{Executor, Task};
use stoat_text::LineEnding;

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
    _task: Task<()>,
    result: Arc<Mutex<Option<std::io::Result<OpenContent>>>>,
}

pub(crate) fn open_file_in_pane(
    stoat: &mut Stoat,
    target: PaneId,
    path: &Path,
) -> Option<BufferId> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        stoat.active_workspace().git_root.join(path)
    };

    let meta = stoat.fs_host.metadata(&absolute).ok().flatten();
    let disk_mtime = meta.map(|m| m.modified);
    if meta.map_or(0, |m| m.len) > OPEN_SYNC_MAX_BYTES {
        spawn_pending_open(stoat, target, absolute, disk_mtime);
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
    match content {
        OpenContent::Text(text) => {
            finish_open(stoat, workspace, target, &absolute, &text, disk_mtime)
        },
        OpenContent::Image { px } => {
            show_image(stoat, workspace, target, &absolute, px);
            None
        },
    }
}

/// Point `target` at an image file, replacing whatever it showed.
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
    let Some(ws) = stoat.workspaces.get_mut(workspace) else {
        return;
    };
    if !ws.panes.contains(target) {
        return;
    }
    ws.panes.pane_mut(target).view = View::Image {
        path: absolute.to_path_buf(),
        px,
    };
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
) {
    let workspace = stoat.active_workspace;
    if stoat
        .pending_file_opens
        .iter()
        .any(|p| p.path == absolute && p.workspace == workspace)
    {
        return;
    }

    let result: Arc<Mutex<Option<std::io::Result<OpenContent>>>> = Arc::new(Mutex::new(None));
    let task = {
        let result = result.clone();
        let fs_host = stoat.fs_host.clone();
        let redraw = stoat.redraw_notify.clone();
        let path = absolute.clone();
        stoat.executor.spawn_blocking(move || {
            let content = read_open_content(&*fs_host, &path);
            *result.lock().expect("pending open mutex") = Some(content);
            redraw.notify_one();
        })
    };
    stoat.pending_file_opens.push(PendingFileOpen {
        path: absolute,
        workspace,
        target,
        disk_mtime,
        _task: task,
        result,
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

/// Install every pending open whose read has finished.
///
/// Called from [`Stoat::drive_background`]. Drops an open whose target pane
/// vanished while it read, and clears the [`BadgeSource::FileOpen`] badge once
/// none remain.
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
        match content {
            OpenContent::Text(text) => {
                finish_open(
                    stoat,
                    pending.workspace,
                    pending.target,
                    &pending.path,
                    &text,
                    pending.disk_mtime,
                );
            },
            OpenContent::Image { px } => {
                show_image(stoat, pending.workspace, pending.target, &pending.path, px);
            },
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
/// buffer (deduping on path), applies mtime and language, notifies LSP, records
/// the pane switch, and installs the editor.
///
/// `workspace` is named rather than taken as the active one because the
/// background path installs a read that may have finished after the user
/// switched away, and `target` is a key only that workspace can resolve.
fn finish_open(
    stoat: &mut Stoat,
    workspace: WorkspaceId,
    target: PaneId,
    absolute: &Path,
    content: &str,
    disk_mtime: Option<SystemTime>,
) -> Option<BufferId> {
    let lang = stoat.language_registry.for_path(absolute);
    let executor = stoat.executor.clone();

    let ending = LineEnding::detect(content);
    let content = LineEnding::normalize(content);

    let (buffer_id, buffer) = {
        let ws = &mut stoat.workspaces[workspace];
        // Opening a path already registered hands back the existing buffer and
        // discards `content`. Everything read alongside it describes a file the
        // buffer never took, so none of it may replace what is already recorded.
        // The mtime matters most. Adopting it would move the change-guard's
        // baseline onto a write the buffer never saw, leaving the guard
        // comparing that write against itself.
        let existed = ws.buffers.id_for_path(absolute).is_some();
        let (buffer_id, buffer) = ws.buffers.open(absolute, &content);
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
/// A pinned mode on the displaced editor carries onto the new one, so a chord
/// the user still holds survives the swap. Every other mode is dropped, since
/// the new editor starts on a different buffer. See [`app::is_pinned_mode`].
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
        .map(|editor| editor.mode.clone())
        .filter(|mode| app::is_pinned_mode(mode));

    let mut editor = ws.seeded_editor(buffer_id, buffer, executor);
    if let Some(mode) = carried_mode {
        editor.mode = mode;
    }
    let new_editor_id = ws.editors.insert(editor);

    // Read before the gc below, which is free to drop the outgoing editor.
    let outgoing = old
        .and_then(|eid| ws.editors.get(eid))
        .map(|editor| editor.buffer_id)
        .filter(|id| *id != buffer_id);

    ws.panes.pane_mut(target).view = View::Editor(new_editor_id);
    if let Some(outgoing) = outgoing {
        ws.panes.pane_mut(target).last_buffer = Some(outgoing);
    }

    if let Some(old_id) = old {
        gc_editor_if_unreferenced(ws, old_id);
    }

    // A latched pane opens each buffer it navigates to as a diff, but only when
    // that file actually has hunks against HEAD. A clean or untracked file shows
    // plain with the latch still armed, so hopping back to a modified file
    // re-enters the diff. Neither widen nor cursor moves here. The widen belongs
    // to the latched session, and the jump target is the position.
    let latched = stoat.workspaces[workspace].panes.pane(target).diff_mode;
    if latched
        && crate::action_handlers::review::ensure_diff_map(stoat, new_editor_id, buffer_id)
        && let Some(editor) = stoat.workspaces[workspace].editors.get_mut(new_editor_id)
    {
        editor.set_diff_view(true);
    }

    Some(buffer_id)
}

/// Show the buffer the focused pane displayed before its current one.
///
/// Repeating alternates between the pair, because the switch records the buffer
/// it leaves on the way out. The jump lands on the pane's jumplist first, so a
/// backward jump reverses it like any cross-file open.
///
/// Sets a status message and moves nothing when the pane has shown no other
/// buffer, or when that buffer has since been closed.
pub(crate) fn goto_last_accessed(stoat: &mut Stoat) -> UpdateEffect {
    let workspace = stoat.active_workspace;
    let resolved = {
        let ws = &mut stoat.workspaces[workspace];
        let target = match ws.focus {
            FocusTarget::SplitPane => ws.panes.focus(),
            FocusTarget::Dock(_) => return UpdateEffect::None,
        };
        let previous = ws
            .panes
            .pane(target)
            .last_buffer
            .and_then(|id| ws.buffers.get(id).map(|buffer| (id, buffer)));

        // A closed buffer never comes back, so drop the dangling id rather than
        // resolving it again on every later press.
        if previous.is_none() {
            ws.panes.pane_mut(target).last_buffer = None;
        }
        previous.map(|(id, buffer)| (target, id, buffer))
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
/// [`crate::buffer_registry::BufferRegistry`] and notify the LSP
/// server via [`crate::host::LspHost::did_close`]. Editor states
/// that referenced the buffer are rebound to fresh scratch buffers
/// so panes stay coherent. Refuses to close when the buffer is
/// dirty so unsaved edits aren't silently lost.
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

    let executor = stoat.executor.clone();
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

    // Purge the closed buffer from every pane's jumplist so a later walk can
    // never resolve a stale entry into it.
    let ws = stoat.active_workspace_mut();
    for tree in ws.pane_trees_mut() {
        for pane_id in tree.split_pane_ids() {
            tree.pane_mut(pane_id).jumplist.remove_buffer(buffer_id);
        }
    }

    if let Some(done) = stoat
        .active_workspace_mut()
        .editor_bridge_waiters
        .remove(&buffer_id)
    {
        let _ = done.send(());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        action_handlers::dispatch,
        test_harness::{editor, TestHarness},
    };
    use stoat_action::{
        CloseBuffer, FocusLeft, GotoLastAccessed, OpenBuffer, OpenFile, SplitRight,
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

    /// Open `a.txt` then `b.txt` in the focused pane, returning both ids in
    /// open order. The pane ends on `b`, with `a` as the one to switch back to.
    fn open_two(h: &mut TestHarness) -> (BufferId, BufferId) {
        let root = PathBuf::from("/last-accessed");
        h.fake_fs().insert_file(root.join("a.txt"), b"a\n");
        h.fake_fs().insert_file(root.join("b.txt"), b"b\n");
        h.stoat.active_workspace_mut().git_root = root.clone();

        dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("a.txt"),
            },
        );
        h.settle();
        let a = focused_buffer_id(&mut h.stoat);

        dispatch(
            &mut h.stoat,
            &OpenFile {
                path: root.join("b.txt"),
            },
        );
        h.settle();
        (a, focused_buffer_id(&mut h.stoat))
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

    #[test]
    fn goto_last_accessed_reports_when_the_pane_has_shown_nothing_else() {
        let mut h = Stoat::test();
        let (_path, buffer_id) = open_path(&mut h, b"only\n");

        // A split inherits the focused view but starts its own history, so the
        // new pane has shown exactly one buffer.
        dispatch(&mut h.stoat, &SplitRight);
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
    fn goto_last_accessed_reports_when_the_previous_buffer_was_closed() {
        let mut h = Stoat::test();
        let (a, b) = open_two(&mut h);

        // Closing acts on the focused buffer, so a pane never closes its own
        // previous one. A second pane closes `a` out from under the first.
        dispatch(&mut h.stoat, &SplitRight);
        dispatch(
            &mut h.stoat,
            &OpenFile {
                path: PathBuf::from("/last-accessed/a.txt"),
            },
        );
        h.settle();
        dispatch(&mut h.stoat, &CloseBuffer);
        h.settle();
        dispatch(&mut h.stoat, &FocusLeft);

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
                    .last_buffer,
            ),
            (false, b, Some("no previously shown buffer"), None),
            "a closed previous buffer reports, stays put, and drops the dangling id"
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
    fn close_buffer_clears_lsp_opened() {
        let mut h = Stoat::test();
        let (_path, buffer_id) = open_path(&mut h, b"hello\n");
        assert!(h.stoat.lsp_opened.contains(&buffer_id));
        dispatch(&mut h.stoat, &CloseBuffer);
        assert!(!h.stoat.lsp_opened.contains(&buffer_id));
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
}

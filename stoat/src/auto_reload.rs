//! Following a file as it changes on disk, and the commands that drive it.
//!
//! A buffer can be flagged to re-read its file whenever the file advances past
//! the mtime the last read recorded. A poll timer wakes the run loop at
//! [`crate::app::AUTO_RELOAD_POLL`], [`pump_auto_reload`] answers the tick, and
//! the poll disarms itself once no buffer is flagged, so a session that follows
//! nothing pays nothing.
//!
//! The re-read is written to preserve as much of the reader's place as it can.
//! Content that only extends the old text is appended rather than replacing it,
//! which keeps every anchor below the seam, and a cursor resting on what was the
//! last line follows the new end while any other cursor stays put. That is what
//! makes a followed log read like a log rather than like a file that keeps
//! jumping.
//!
//! None of this dispatches an action of its own. The entry points that drive it
//! are the dispatch arms in [`crate::action_handlers`] and the run loop's
//! auto-reload tick.

use crate::{
    action_handlers::{
        file::{display_name, open_file},
        focused_editor_mut, read_string_via_host,
        view::ensure_cursor_in_view,
    },
    app::{Stoat, UpdateEffect},
    buffer::BufferId,
    buffer_registry::AutoReloadMode,
    editor_state::{EditorId, EditorState},
    lsp::sync,
};
use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};
use stoat_scheduler::Task;
use stoat_text::{Bias, LineEnding, Rope, SelectionGoal};

/// Environment variable stoatty exports to its children, carrying the log id
/// its own log file is named after.
const STOATTY_LOG_ID: &str = "STOATTY_LOG_ID";

/// A followed file read on the blocking pool, awaiting the edit it describes.
///
/// The stat that started it runs on the run loop, since it decides whether
/// anything happened. Everything after it walks the whole file, which a
/// followed build log makes the run loop do twice a second.
///
/// Held in [`Stoat::pending_auto_reloads`] so the task is not dropped, which
/// cancels the read, before [`pump_auto_reload_install`] takes it.
pub(crate) struct PendingAutoReload {
    /// Which buffer the read answers for, so a poll landing while it is still
    /// out does not start a second read of the same file.
    id: BufferId,
    _task: Task<()>,
    result: Arc<Mutex<Option<AutoReloadResult>>>,
}

/// What a pool read found, as the run loop needs it.
struct AutoReloadResult {
    id: BufferId,
    mode: AutoReloadMode,
    /// The file's mtime as the stat read it, recorded whatever the outcome, so
    /// a file the reload rejects is not re-read twice a second until it
    /// changes again.
    mtime: SystemTime,
    /// The buffer version the comparison was made against.
    ///
    /// An edit landing while the read ran leaves the spans below describing
    /// text that has since moved, so the install drops them and the next tick
    /// reads again.
    version: u64,
    outcome: Result<ReloadDiff, String>,
}

/// The edit a read implies, or none when the file matches the buffer.
struct ReloadDiff {
    ending: LineEnding,
    /// Where the buffer's text is replaced and by what, or `None` when the two
    /// already agree and only the mtime moves.
    ///
    /// The replacement is the slice that changed rather than the whole file.
    /// Carrying the file across costs a copy of it, which is the cost running
    /// the comparison here rather than on the run loop removes.
    splice: Option<(Range<usize>, String)>,
    /// The buffer's last row when the comparison was made, which is the row a
    /// tail follower must rest on to follow the new end.
    old_last_row: u32,
    /// Where a follow-mode cursor lands, at the first byte that changed.
    follow_offset: usize,
}

/// Arm the auto-reload poll if it is not already running.
///
/// Spawns a timer loop sending a tick every [`crate::app::AUTO_RELOAD_POLL`],
/// which the run loop receives and answers by driving [`pump_auto_reload`]. The
/// tick deliberately does not wake a repaint on its own, so following an idle
/// file costs no frames. The task cancels when dropped, and
/// [`pump_auto_reload`] drops it once no buffer is flagged. Called when a buffer
/// opts into file-following, such as the session log buffer and the
/// `:auto-reload` command.
pub(crate) fn ensure_auto_reload_poll(stoat: &mut Stoat) {
    if stoat.auto_reload_poll.is_some() {
        return;
    }
    let executor = stoat.executor.clone();
    let tick = stoat.auto_reload_tx.clone();
    let task = stoat.executor.spawn(async move {
        loop {
            executor.timer(crate::app::AUTO_RELOAD_POLL).await;
            // The single-slot channel makes this wait out a busy loop rather
            // than queue ticks behind it, so a stall costs one late pass and
            // not a burst of them.
            if tick.send(()).await.is_err() {
                break;
            }
        }
    });
    stoat.auto_reload_poll = Some(task);
}

/// Stat every auto-reload-flagged buffer's file and read the ones that moved,
/// disarming the poll when none remain.
///
/// The stat is all this does on the run loop. A file whose mtime advanced is
/// read and compared on the blocking pool, and
/// [`pump_auto_reload_install`] applies whatever that found. A dirty buffer is
/// skipped so in-memory edits are never clobbered.
///
/// Returns whether a read was started, so the run loop knows work is out. What
/// the reader sees moving is the install's answer, not this one.
pub(crate) fn pump_auto_reload(stoat: &mut Stoat) -> bool {
    if stoat.auto_reload_poll.is_none() {
        return false;
    }
    let paths = stoat.active_workspace().buffers.auto_reload_paths();
    if paths.is_empty() {
        stoat.auto_reload_poll = None;
        return false;
    }

    let mut spawned = false;
    for (id, path, mode) in paths {
        let Some(buffer) = stoat.active_workspace().buffers.get(id) else {
            continue;
        };
        let (dirty, version, rope) = {
            let guard = buffer.read().expect("buffer poisoned");
            (
                guard.dirty,
                guard.snapshot.version,
                guard.snapshot.visible_text.clone(),
            )
        };
        if dirty {
            continue;
        }
        let Some(mtime) = stoat
            .fs_host
            .metadata(&path)
            .ok()
            .flatten()
            .map(|m| m.modified)
        else {
            continue;
        };
        if stoat.active_workspace().buffers.disk_mtime(id) == Some(mtime) {
            continue;
        }
        // One read per file at a time. A poll landing while the last one is
        // still out reads the same bytes a second time, and the install keeps
        // only the one the buffer has not moved past.
        if stoat.pending_auto_reloads.iter().any(|p| p.id == id) {
            continue;
        }

        let result: Arc<Mutex<Option<AutoReloadResult>>> = Arc::new(Mutex::new(None));
        let task = {
            let result = result.clone();
            let fs_host = stoat.fs_host.clone();
            let redraw = stoat.redraw_notify.clone();
            let path = path.clone();
            stoat.executor.spawn_blocking(move || {
                let outcome = read_and_compare(&*fs_host, &path, &rope);
                *result.lock().expect("pending reload mutex") = Some(AutoReloadResult {
                    id,
                    mode,
                    mtime,
                    version,
                    outcome,
                });
                redraw.notify_one();
            })
        };
        stoat.pending_auto_reloads.push(PendingAutoReload {
            id,
            _task: task,
            result,
        });
        spawned = true;
    }

    spawned
}

/// Read `path` and work out the edit that brings `old` up to it.
///
/// Runs on the blocking pool. The read, the ending, the normalize, and both
/// walks all cover the whole file, which is what this exists to keep off the
/// thread that paints.
fn read_and_compare(
    fs: &dyn crate::host::FsHost,
    path: &Path,
    old: &Rope,
) -> Result<ReloadDiff, String> {
    let new = read_string_via_host(fs, path)
        .map_err(|e| format!("cannot reload {}: {e}", display_name(path)))?;

    // Before the prefix diff, which would otherwise compare the file's
    // carriage returns against a buffer that has none and call every line
    // changed. The file may also have changed style since it was opened, so
    // its current one is the honest answer.
    let ending = LineEnding::detect(&new);
    let normalized = LineEnding::normalize(&new);

    let old_len = old.len();
    let old_last_row = old.max_point().row;
    let common = common_prefix_len(old.chunks(), &normalized);

    // The buffer is a prefix of the file only when every one of its bytes
    // matched, which is the appended-log fast path.
    let appended = common == old_len;
    if appended && normalized.len() == old_len {
        return Ok(ReloadDiff {
            ending,
            splice: None,
            old_last_row,
            follow_offset: old_len,
        });
    }

    let (splice, follow_offset) = match appended {
        true => (
            (old_len..old_len, normalized[old_len..].to_string()),
            old_len,
        ),
        false => {
            let (old_span, new_span) = changed_span(old, &normalized);
            let mut offset = common;
            while !normalized.is_char_boundary(offset) {
                offset -= 1;
            }
            ((old_span, normalized[new_span].to_string()), offset)
        },
    };

    Ok(ReloadDiff {
        ending,
        splice: Some(splice),
        old_last_row,
        follow_offset,
    })
}

/// Apply every finished auto-reload read, and report whether the reader sees
/// anything move.
///
/// A read whose buffer was edited or reloaded while it ran describes text that
/// has since moved, so it is dropped and the next poll reads again. The mtime
/// is recorded whatever the outcome, including a failed read, so a file the
/// reload rejects is not re-read at the poll cadence until it changes.
///
/// When the new content extends the old it is appended in place, preserving
/// anchors for the log-tail case. Otherwise the changed span is replaced. A
/// cursor resting on what was the last line follows to the new end, while any
/// other cursor stays put.
pub(crate) fn pump_auto_reload_install(stoat: &mut Stoat) -> bool {
    let mut ready = Vec::new();
    stoat.pending_auto_reloads.retain(|pending| {
        match pending.result.lock().expect("pending reload mutex").take() {
            Some(result) => {
                ready.push(result);
                false
            },
            None => true,
        }
    });
    if ready.is_empty() {
        return false;
    }

    let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
    // Separate from `changed` because a failed read repaints the status without
    // editing the buffer, and the LSP notify below must not hear about a buffer
    // that never changed.
    let mut status_set = false;
    let mut changed = false;

    for result in ready {
        let AutoReloadResult {
            id,
            mode,
            mtime,
            version,
            outcome,
        } = result;

        let Some(buffer) = stoat.active_workspace().buffers.get(id) else {
            continue;
        };
        let (dirty, current) = {
            let guard = buffer.read().expect("buffer poisoned");
            (guard.dirty, guard.snapshot.version)
        };
        if dirty || current != version {
            continue;
        }

        let diff = match outcome {
            Ok(diff) => diff,
            Err(message) => {
                stoat
                    .active_workspace_mut()
                    .buffers
                    .set_disk_mtime(id, mtime);
                stoat.set_status(message);
                status_set = true;
                continue;
            },
        };

        stoat
            .active_workspace_mut()
            .buffers
            .set_line_ending(id, diff.ending);

        let Some((old_span, text)) = diff.splice else {
            stoat
                .active_workspace_mut()
                .buffers
                .set_disk_mtime(id, mtime);
            continue;
        };

        let tail_followers: Vec<EditorId> = if mode == AutoReloadMode::Tail {
            stoat
                .active_workspace_mut()
                .editors
                .iter_mut()
                .filter_map(|(eid, editor)| {
                    (editor.buffer_id == id && editor_cursor_row(editor) == diff.old_last_row)
                        .then_some(eid)
                })
                .collect()
        } else {
            Vec::new()
        };

        {
            let mut guard = buffer.write().expect("buffer poisoned");
            guard.edit(old_span, &text);
            guard.mark_clean();
        }
        stoat
            .active_workspace_mut()
            .buffers
            .set_disk_mtime(id, mtime);
        changed = true;

        let ws = stoat.active_workspace_mut();
        for eid in tail_followers {
            if let Some(editor) = ws.editors.get_mut(eid) {
                collapse_to_buffer_end(editor, scrolloff);
            }
        }
        if mode == AutoReloadMode::Follow {
            let follow_editors: Vec<EditorId> = ws
                .editors
                .iter()
                .filter_map(|(eid, editor)| (editor.buffer_id == id).then_some(eid))
                .collect();
            for eid in follow_editors {
                if let Some(editor) = ws.editors.get_mut(eid) {
                    collapse_to_offset(editor, diff.follow_offset, scrolloff);
                }
            }
        }
    }

    if changed {
        sync::notify_buffer_changes_pending(stoat);
    }
    changed || status_set
}

/// Re-read the focused buffer's backing file from disk, backing `:reload` and
/// `:reload!`.
///
/// Refuses when the buffer has unsaved edits unless `force` discards them. A
/// scratch buffer with no path reports and changes nothing, as does a file that
/// no longer exists on disk (the buffer keeps its content). A successful reload
/// replaces the buffer content in one undoable edit, marks it clean, records the
/// new disk mtime, and notifies the language server. Returns [`UpdateEffect::None`]
/// only when no editor is focused. Every other path redraws.
pub(crate) fn reload_focused(stoat: &mut Stoat, force: bool) -> UpdateEffect {
    let Some(id) = focused_editor_mut(stoat).map(|e| e.buffer_id) else {
        return UpdateEffect::None;
    };

    let Some(path) = stoat
        .active_workspace()
        .buffers
        .path_for(id)
        .map(Path::to_path_buf)
    else {
        stoat.set_status("buffer has no file to reload");
        return UpdateEffect::Redraw;
    };

    let dirty = stoat
        .active_workspace()
        .buffers
        .get(id)
        .map(|b| b.read().expect("buffer poisoned").dirty)
        .unwrap_or(false);
    if dirty && !force {
        stoat.set_status("unsaved changes; :reload! discards them");
        return UpdateEffect::Redraw;
    }

    match reload_from_disk(stoat, id, &path) {
        ReloadOutcome::Missing => stoat.set_status("file no longer exists on disk"),
        ReloadOutcome::Unchanged => stoat.set_status("already up to date"),
        ReloadOutcome::Reloaded => {
            stoat.set_status(format!("reloaded {}", display_name(&path)));
            sync::notify_buffer_changes_pending(stoat);
        },
    }
    UpdateEffect::Redraw
}

/// Re-read every open file-backed buffer from disk, backing `:reload-all` and
/// `:reload-all!`.
///
/// Each buffer reloads via [`reload_from_disk`]. The unforced form skips a
/// buffer with unsaved edits, while the forced form discards them. A file
/// missing on disk is reported and its buffer left intact, never blocking the
/// other reloads. The status summarizes the outcome by cause: how many buffers
/// reloaded, how many unsaved buffers were skipped, and how many files were
/// missing.
pub(crate) fn reload_all(stoat: &mut Stoat, force: bool) -> UpdateEffect {
    let paths = stoat.active_workspace().buffers.open_paths();
    if paths.is_empty() {
        stoat.set_status("no files to reload");
        return UpdateEffect::Redraw;
    }

    let mut reloaded = 0usize;
    let mut skipped = 0usize;
    let mut missing = 0usize;

    for path in paths {
        let Some(id) = stoat.active_workspace().buffers.id_for_path(&path) else {
            continue;
        };
        let dirty = stoat
            .active_workspace()
            .buffers
            .get(id)
            .map(|b| b.read().expect("buffer poisoned").dirty)
            .unwrap_or(false);
        if dirty && !force {
            skipped += 1;
            continue;
        }
        match reload_from_disk(stoat, id, &path) {
            ReloadOutcome::Reloaded => reloaded += 1,
            ReloadOutcome::Missing => missing += 1,
            ReloadOutcome::Unchanged => {},
        }
    }

    if reloaded > 0 {
        sync::notify_buffer_changes_pending(stoat);
    }

    let mut parts: Vec<String> = Vec::new();
    if reloaded > 0 {
        parts.push(format!("reloaded {reloaded}"));
    }
    if skipped > 0 {
        parts.push(format!("{skipped} unsaved skipped (:reload-all! discards)"));
    }
    if missing > 0 {
        parts.push(format!("{missing} missing on disk"));
    }
    let status = if parts.is_empty() {
        "all files up to date".to_string()
    } else {
        parts.join("; ")
    };
    stoat.set_status(status);
    UpdateEffect::Redraw
}

/// The disposition of a [`reload_from_disk`] attempt.
enum ReloadOutcome {
    /// The buffer content was replaced with the file's newer bytes.
    Reloaded,
    /// The file already matched the buffer. Only the clean flag and mtime
    /// baseline were refreshed.
    Unchanged,
    /// The file has no metadata or could not be read. The buffer is untouched.
    Missing,
}

/// Replace buffer `id`'s content with `path`'s current bytes, mirroring
/// [`pump_auto_reload`]'s replace arm without the cursor-follow logic.
///
/// A file that cannot be stat-ed or read yields [`ReloadOutcome::Missing`] and
/// leaves the buffer untouched. A file already byte-identical to the buffer is
/// left un-edited to avoid version churn, but the buffer is still marked clean
/// and its mtime baseline refreshed. Otherwise the whole buffer is replaced in
/// one undoable edit and marked clean.
fn reload_from_disk(stoat: &mut Stoat, id: BufferId, path: &Path) -> ReloadOutcome {
    let Some(mtime) = stoat
        .fs_host
        .metadata(path)
        .ok()
        .flatten()
        .map(|m| m.modified)
    else {
        return ReloadOutcome::Missing;
    };
    let Ok(new) = read_string_via_host(&*stoat.fs_host, path) else {
        return ReloadOutcome::Missing;
    };
    stoat
        .active_workspace_mut()
        .buffers
        .set_line_ending(id, LineEnding::detect(&new));

    // Borrowed rather than owned, as the pump's own reload is.
    let normalized = LineEnding::normalize(&new);

    let Some(buffer) = stoat.active_workspace().buffers.get(id) else {
        return ReloadOutcome::Missing;
    };

    let (old_len, common) = {
        let guard = buffer.read().expect("buffer poisoned");
        let text = &guard.snapshot.visible_text;
        (text.len(), common_prefix_len(text.chunks(), &normalized))
    };
    let unchanged = common == old_len && normalized.len() == old_len;

    if unchanged {
        if buffer.read().expect("buffer poisoned").dirty {
            buffer.write().expect("buffer poisoned").mark_clean();
        }
    } else {
        let mut guard = buffer.write().expect("buffer poisoned");
        let (old_span, new_span) = changed_span(&guard.snapshot.visible_text, &normalized);
        guard.edit(old_span, &normalized[new_span]);
        guard.mark_clean();
    }
    stoat
        .active_workspace_mut()
        .buffers
        .set_disk_mtime(id, mtime);

    if unchanged {
        ReloadOutcome::Unchanged
    } else {
        ReloadOutcome::Reloaded
    }
}

/// Open a log file and follow it as new lines are written.
///
/// `target` is the `:logs` argument: [`None`] or `"stoat"` opens this
/// session's own log, `"stoatty"` opens the enclosing terminal's. Resolves the
/// file under `<log dir>` and delegates to [`open_log_buffer`]. Reports via
/// [`Stoat::pending_message`] and opens nothing when the log directory cannot
/// be resolved, when `target` names neither program, or when the named session
/// wrote no log file.
pub(crate) fn open_logs(stoat: &mut Stoat, target: Option<&str>) -> UpdateEffect {
    let Ok(dir) = stoat_log::log_dir() else {
        stoat.set_status("could not resolve the log directory");
        return UpdateEffect::Redraw;
    };

    let stem = target_log_stem(
        target,
        stoat_log::ident::get().map(|i| i.file_stem.as_str()),
        stoat.env_host.var(STOATTY_LOG_ID).as_deref(),
    );
    let stem = match stem {
        Ok(stem) => stem,
        Err(status) => {
            stoat.set_status(status);
            return UpdateEffect::Redraw;
        },
    };

    let Some(path) = session_log_path(&dir, stem.as_deref()) else {
        stoat.set_status("no log file for this session; started with --log-stderr?");
        return UpdateEffect::Redraw;
    };
    open_log_buffer(stoat, &path)
}

/// The log file stem `target` names, or the status to report instead.
///
/// `Ok(None)` means the named session wrote no log file, which is what
/// `--log-stderr` produces; [`session_log_path`] turns that into no path.
///
/// Pure so every branch is testable. Its two inputs are otherwise unreachable
/// from a test: `ident_stem` comes from a first-write-wins process global, and
/// `stoatty_id` from the environment.
fn target_log_stem(
    target: Option<&str>,
    ident_stem: Option<&str>,
    stoatty_id: Option<&str>,
) -> Result<Option<String>, &'static str> {
    match target {
        None | Some("stoat") => Ok(ident_stem.map(str::to_owned)),
        // stoatty names its log after the same id it exports, so the stem is
        // the variable with the prefix put back on.
        Some("stoatty") => match stoatty_id {
            Some(sid) => Ok(Some(format!("stoatty-{sid}"))),
            None => Err("logs: not running inside stoatty"),
        },
        Some(_) => Err("logs: expected stoat or stoatty"),
    }
}

/// The log file `stem` names under `dir`, or `None` when no stem was given.
///
/// Split out from [`open_logs`] because the stem comes from a process-global
/// identity that is first-write-wins (`stoat_log::ident::install`). A test
/// cannot install one without fixing it for every later test in the binary, so
/// the path logic takes the stem as a parameter instead of reading it.
///
/// A `None` stem means the binary never named a log file, which is what
/// `--log-stderr` produces.
fn session_log_path(dir: &Path, stem: Option<&str>) -> Option<PathBuf> {
    Some(dir.join(format!("{}.log", stem?)))
}

/// Open `path` as an auto-reloading buffer tailing its end, or report when the
/// file is absent.
///
/// The path is taken as a parameter so tests inject a fixture. When no file
/// exists there (e.g. the session logs to stderr), sets [`Stoat::pending_message`]
/// and opens nothing. Otherwise opens the file, flags it auto-reload, arms the
/// poll, and drops the focused cursor on the last line.
pub(crate) fn open_log_buffer(stoat: &mut Stoat, path: &Path) -> UpdateEffect {
    if !matches!(stoat.fs_host.metadata(path), Ok(Some(_))) {
        stoat.set_status("no log file for this session; started with --log-stderr?");
        return UpdateEffect::Redraw;
    }

    let Some(id) = open_file(stoat, path) else {
        return UpdateEffect::Redraw;
    };
    stoat
        .active_workspace_mut()
        .buffers
        .set_auto_reload(id, AutoReloadMode::Tail);
    ensure_auto_reload_poll(stoat);

    let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
    if let Some(editor) = focused_editor_mut(stoat) {
        collapse_to_buffer_end(editor, scrolloff);
    }
    UpdateEffect::Redraw
}

/// Set the focused buffer's auto-reload mode, backing `:auto-reload`.
///
/// The `state` argument is matched case-insensitively. "on" tails the file,
/// "off" disables reload, and "follow" jumps the cursor to each reload's first
/// changed region. Any other value reports the expected form and changes
/// nothing. Requesting "follow" while already following toggles it back off, so
/// one binding both starts and stops it. Enabling a scratch buffer with no
/// backing file reports that and changes nothing.
///
/// Enabling arms the poll. Disabling relies on the pump auto-disarming once no
/// buffer is set.
pub(crate) fn set_buffer_auto_reload(stoat: &mut Stoat, state: &str) -> UpdateEffect {
    let requested = match state.trim().to_ascii_lowercase().as_str() {
        "on" => AutoReloadMode::Tail,
        "off" => AutoReloadMode::Off,
        "follow" => AutoReloadMode::Follow,
        _ => {
            stoat.set_status("auto-reload: expected on, off, or follow");
            return UpdateEffect::Redraw;
        },
    };

    let Some(id) = focused_editor_mut(stoat).map(|e| e.buffer_id) else {
        return UpdateEffect::None;
    };

    let already_following =
        stoat.active_workspace().buffers.auto_reload_mode(id) == AutoReloadMode::Follow;
    let mode = if requested == AutoReloadMode::Follow && already_following {
        AutoReloadMode::Off
    } else {
        requested
    };

    if mode != AutoReloadMode::Off && stoat.active_workspace().buffers.path_for(id).is_none() {
        stoat.set_status("buffer has no file to reload");
        return UpdateEffect::Redraw;
    }

    stoat
        .active_workspace_mut()
        .buffers
        .set_auto_reload(id, mode);
    if mode != AutoReloadMode::Off {
        ensure_auto_reload_poll(stoat);
    }
    stoat.set_status(match mode {
        AutoReloadMode::Off => "auto-reload off",
        AutoReloadMode::Tail => "auto-reload on",
        AutoReloadMode::Follow => "auto-reload follow",
    });
    UpdateEffect::Redraw
}

/// Set whether saving a config file re-applies it, backing
/// `:auto-reload-config`.
///
/// The `state` argument is matched case-insensitively and accepts "on" or
/// "off". Any other value reports the expected form and changes nothing.
///
/// This overrides the `config.auto_reload` setting for the running session
/// only. Reloading stoat's config restores whatever value that file sets.
pub(crate) fn set_auto_reload_config(stoat: &mut Stoat, state: &str) -> UpdateEffect {
    let enabled = match state.trim().to_ascii_lowercase().as_str() {
        "on" => true,
        "off" => false,
        _ => {
            stoat.set_status("auto-reload-config: expected on or off");
            return UpdateEffect::Redraw;
        },
    };

    stoat.settings.config_auto_reload = Some(enabled);
    stoat.set_status(if enabled {
        "auto-reload-config on"
    } else {
        "auto-reload-config off"
    });
    UpdateEffect::Redraw
}

/// Collapse `editor`'s selection onto the end of its buffer and scroll it into
/// view, so a tailing buffer shows its newest content.
fn collapse_to_buffer_end(editor: &mut EditorState, scrolloff: u32) {
    let snapshot = editor.display_map.snapshot();
    let buf_snap = snapshot.buffer_snapshot();
    let end = buf_snap.rope().len();
    let anchor = buf_snap.anchor_at(end, Bias::Left);
    editor.selections.transform(buf_snap, |s| {
        let mut sel = s.clone();
        sel.collapse_to(anchor, SelectionGoal::None);
        sel
    });
    ensure_cursor_in_view(editor, scrolloff);
}

/// Collapse `editor`'s selection onto `offset` and scroll it into view, so a
/// following buffer jumps to the first region a reload changed.
fn collapse_to_offset(editor: &mut EditorState, offset: usize, scrolloff: u32) {
    let snapshot = editor.display_map.snapshot();
    let buf_snap = snapshot.buffer_snapshot();
    let anchor = buf_snap.anchor_at(offset, Bias::Left);
    editor.selections.transform(buf_snap, |s| {
        let mut sel = s.clone();
        sel.collapse_to(anchor, SelectionGoal::None);
        sel
    });
    ensure_cursor_in_view(editor, scrolloff);
}

/// The number of leading bytes the rope `chunks` share with `new`.
///
/// Walks the chunks byte by byte, stopping at the first mismatch or once `new`
/// is exhausted, so a followed log is diffed against the new file without ever
/// materializing the whole buffer. The result is the append splice point when
/// it equals the buffer length, and the first divergence otherwise.
fn common_prefix_len<'a>(chunks: impl Iterator<Item = &'a str>, new: &str) -> usize {
    let new = new.as_bytes();
    let mut matched = 0;
    for chunk in chunks {
        if matched == new.len() {
            break;
        }
        let chunk = chunk.as_bytes();
        let tail = &new[matched..];
        let n = chunk.len().min(tail.len());
        let common = chunk[..n]
            .iter()
            .zip(tail)
            .take_while(|(a, b)| a == b)
            .count();
        matched += common;
        if common < chunk.len() {
            break;
        }
    }
    matched
}

/// The number of trailing bytes the rope `chunks` share with `new`.
///
/// `chunks` runs back to front, each chunk's own text forward, which is what
/// [`Rope::reversed_chunks_in_range`] yields. The mirror of
/// [`common_prefix_len`], and walked the same way so a followed log is never
/// materialized whole.
fn common_suffix_len<'a>(chunks: impl Iterator<Item = &'a str>, new: &str) -> usize {
    let new = new.as_bytes();
    let mut matched = 0;
    for chunk in chunks {
        if matched == new.len() {
            break;
        }
        let chunk = chunk.as_bytes();
        let head = &new[..new.len() - matched];
        let n = chunk.len().min(head.len());
        let common = chunk[chunk.len() - n..]
            .iter()
            .rev()
            .zip(head.iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        matched += common;
        if common < chunk.len() {
            break;
        }
    }
    matched
}

/// The span of `old` that `new` changes, paired with the span of `new` that
/// replaces it.
///
/// Rewriting a buffer wholesale deletes every fragment the anchors point into,
/// and a deleted fragment resolves to the same offset for all of them, so
/// cursors, marks, jumplist entries, and folds all collapse together. Trimming
/// the shared prefix and suffix leaves the edit covering only what moved, and
/// anchors outside it ride through untouched. Anchors inside it still collapse
/// to its boundary.
///
/// Both splits land on char boundaries in both texts. A boundary in one is not
/// a boundary in the other, since the byte at the split can differ.
fn changed_span(old: &Rope, new: &str) -> (Range<usize>, Range<usize>) {
    let mut prefix = common_prefix_len(old.chunks(), new);
    while prefix > 0 && !(old.is_char_boundary(prefix) && new.is_char_boundary(prefix)) {
        prefix -= 1;
    }

    // The prefix already claimed its bytes, so the suffix can only take what is
    // left of the shorter text. Without the clamp the two spans would overlap
    // and describe more text than either holds.
    let mut suffix = common_suffix_len(old.reversed_chunks_in_range(0..old.len()), new)
        .min(old.len().min(new.len()) - prefix);
    while suffix > 0
        && !(old.is_char_boundary(old.len() - suffix) && new.is_char_boundary(new.len() - suffix))
    {
        suffix -= 1;
    }

    (prefix..old.len() - suffix, prefix..new.len() - suffix)
}

/// The buffer-line row of `editor`'s primary cursor, resolved through its
/// current display snapshot.
fn editor_cursor_row(editor: &mut EditorState) -> u32 {
    let snapshot = editor.display_map.snapshot();
    let buf_snap = snapshot.buffer_snapshot();
    let head = buf_snap.resolve_anchor(&editor.selections.newest_anchor().head());
    buf_snap.rope().offset_to_point(head).row
}

#[cfg(test)]
mod tests {
    use super::{
        collapse_to_offset, editor_cursor_row, ensure_auto_reload_poll, open_log_buffer, open_logs,
        pump_auto_reload, pump_auto_reload_install, reload_all, reload_focused, session_log_path,
        set_auto_reload_config, set_buffer_auto_reload, target_log_stem,
    };
    use crate::{
        action_handlers::{dispatch, focused_editor_mut},
        app::UpdateEffect,
        buffer::BufferId,
        buffer_registry::AutoReloadMode,
        host::FsHost,
        test_harness::{editor, TestHarness},
        Stoat,
    };
    use std::path::{Path, PathBuf};
    use stoat_action::{MoveDown, OpenFile};

    /// Latin-1 bytes, which are not valid UTF-8.
    const NOT_UTF8: &[u8] = b"caf\xe9 au lait\n";

    /// mtime baseline the save guard checks against.
    fn open_edited(h: &mut TestHarness, root: &Path, name: &str, seed: &[u8]) -> PathBuf {
        let path = root.join(name);
        h.fake_fs().insert_file(&path, seed);
        h.stoat.active_workspace_mut().git_root = root.to_path_buf();
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        let buffer_id = focused_editor_mut(&mut h.stoat).expect("editor").buffer_id;
        let buffer = h
            .stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer");
        buffer.write().expect("poisoned").edit(0..0, "edited ");
        path
    }

    #[test]
    fn auto_reload_reports_undecodable_content_once_per_change() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-latin1");
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", b"line1\n");

        h.fake_fs().insert_file(&path, NOT_UTF8);
        pump_poll(&mut h);

        assert_eq!(
            buffer_text(&h, id),
            "line1\n",
            "the buffer keeps what it had"
        );
        let message = h.stoat.pending_message.as_deref().unwrap_or("");
        assert!(
            message.contains("log.txt") && message.contains("utf-8"),
            "following must say why it stopped, got {message:?}"
        );

        // Polling twice a second, a version that cannot be decoded must not be
        // re-read and re-reported until the file changes again.
        h.stoat.pending_message = None;
        pump_poll(&mut h);
        assert_eq!(
            h.stoat.pending_message, None,
            "the same undecodable version must not report again"
        );
    }

    /// Open `name` (seeded with `seed`) under `root`, flag it auto-reload, and
    /// arm the poll. Returns the absolute path and buffer id.
    fn open_auto_reload(
        h: &mut TestHarness,
        root: &Path,
        name: &str,
        seed: &[u8],
    ) -> (PathBuf, BufferId) {
        let path = root.join(name);
        h.fake_fs().insert_file(&path, seed);
        h.stoat.active_workspace_mut().git_root = root.to_path_buf();
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        let id = focused_editor_mut(&mut h.stoat).expect("editor").buffer_id;
        h.stoat
            .active_workspace_mut()
            .buffers
            .set_auto_reload(id, AutoReloadMode::Tail);
        ensure_auto_reload_poll(&mut h.stoat);
        (path, id)
    }

    fn buffer_text(h: &TestHarness, id: BufferId) -> String {
        h.stoat
            .active_workspace()
            .buffers
            .get(id)
            .expect("buffer")
            .read()
            .expect("poisoned")
            .snapshot
            .visible_text
            .to_string()
    }

    fn focused_cursor_row(h: &mut TestHarness) -> u32 {
        editor_cursor_row(focused_editor_mut(&mut h.stoat).expect("editor"))
    }

    /// Drive one poll pass, standing in for the tick the timer sends in
    /// production, and report whether the reader sees anything move.
    ///
    /// The tick only stats and spawns. What the run loop paints for is the
    /// install, which reaches the reads once the pool has run them, so the
    /// settle between the two is the same wait production makes.
    fn pump_poll(h: &mut TestHarness) -> bool {
        pump_auto_reload(&mut h.stoat);
        // Not a settle, which drives the install itself and leaves nothing for
        // the call below to report. The reads finish on the pool and the
        // install is what the run loop paints for.
        h.run_until_parked();
        pump_auto_reload_install(&mut h.stoat)
    }

    /// The run loop paints a poll tick only when the pump reports one, so a
    /// buffer following an idle file has to cost no frames.
    #[test]
    fn only_the_poll_that_reloads_asks_for_a_repaint() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-quiet");
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", b"line1\n");

        assert!(!pump_poll(&mut h), "an unchanged file asks for no repaint");

        h.fake_fs().insert_file(&path, b"line1\nline2\n");

        assert!(pump_poll(&mut h), "the reload asks for a repaint");
        assert_eq!(buffer_text(&h, id), "line1\nline2\n");
        assert!(
            !pump_poll(&mut h),
            "the poll after the reload lands asks for no repaint",
        );
    }

    #[test]
    fn pump_auto_reload_appends_and_keeps_the_buffer_clean() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-append");
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", b"line1\n");

        h.fake_fs().insert_file(&path, b"line1\nline2\n");
        pump_poll(&mut h);

        assert_eq!(buffer_text(&h, id), "line1\nline2\n");
        assert!(
            !editor::focused_dirty(&h.stoat),
            "a reloaded buffer stays clean"
        );
    }

    #[test]
    fn pump_auto_reload_normalizes_before_the_prefix_diff() {
        // The re-read is compared against the buffer to find the common prefix.
        // Carriage returns still in the disk text diverge on the first line, so
        // an ordinary append would be read as a whole-file rewrite, and the
        // buffer would take the carriage returns as content.
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-crlf");
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", b"line1\r\n");

        h.fake_fs().insert_file(&path, b"line1\r\nline2\r\n");
        pump_poll(&mut h);

        assert_eq!(buffer_text(&h, id), "line1\nline2\n");
    }

    #[test]
    fn pump_auto_reload_appends_across_a_chunk_boundary() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-chunks");
        // Larger than one rope chunk, so the streaming compare walks several
        // chunks before reaching the append point.
        let seed = "abcdefghij\n".repeat(200);
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", seed.as_bytes());

        let appended = format!("{seed}tail line\n");
        h.fake_fs().insert_file(&path, appended.as_bytes());
        pump_poll(&mut h);

        assert_eq!(buffer_text(&h, id), appended);
        assert!(
            !editor::focused_dirty(&h.stoat),
            "a chunk-boundary append stays clean"
        );
    }

    #[test]
    fn pump_auto_reload_replaces_on_a_mid_chunk_change() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-chunk-replace");
        let seed = "abcdefghij\n".repeat(200);
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", seed.as_bytes());

        // Flip a byte deep inside the buffer, so the compare finds a mismatch
        // mid-chunk and full-replaces rather than appending.
        let mut changed = seed.into_bytes();
        changed[1500] = b'Z';
        h.fake_fs().insert_file(&path, &changed);
        pump_poll(&mut h);

        assert_eq!(
            buffer_text(&h, id).into_bytes(),
            changed,
            "a mid-chunk change replaces the buffer"
        );
    }

    #[test]
    fn pump_auto_reload_skips_dirty_buffers() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-dirty");
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", b"line1\n");
        h.stoat
            .active_workspace()
            .buffers
            .get(id)
            .expect("buffer")
            .write()
            .expect("poisoned")
            .edit(0..0, "x");

        h.fake_fs().insert_file(&path, b"line1\nline2\n");
        pump_poll(&mut h);

        assert_eq!(
            buffer_text(&h, id),
            "xline1\n",
            "a dirty buffer keeps its in-memory edits"
        );
    }

    #[test]
    fn pump_auto_reload_tail_follows_a_last_line_cursor() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-tail");
        // The seed has no trailing newline, so the last line "ccc" carries
        // content and a col-0 cursor on it sits before the append point. Natural
        // anchoring leaves such a cursor put. Only the tail-follow carries it to
        // the new end.
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", b"aaa\nbbb\nccc");
        dispatch(&mut h.stoat, &MoveDown);
        dispatch(&mut h.stoat, &MoveDown);
        assert_eq!(
            focused_cursor_row(&mut h),
            2,
            "cursor starts on the last line"
        );

        h.fake_fs().insert_file(&path, b"aaa\nbbb\nccc\nddd\n");
        pump_poll(&mut h);

        assert_eq!(buffer_text(&h, id), "aaa\nbbb\nccc\nddd\n");
        assert_eq!(
            focused_cursor_row(&mut h),
            4,
            "a last-line cursor follows the append to the new end"
        );
    }

    /// Park the cursor, a buffer-local mark, and a jumplist entry on row 3, so
    /// a reload can be checked against three anchors that ride the same edit.
    fn anchor_row_3(h: &mut TestHarness, id: BufferId) {
        let offset = {
            let editor = focused_editor_mut(&mut h.stoat).expect("editor");
            let snapshot = editor.display_map.snapshot();
            let offset = snapshot
                .buffer_snapshot()
                .rope()
                .point_to_offset(stoat_text::Point::new(3, 0));
            collapse_to_offset(editor, offset, 0);
            offset
        };

        let anchor = {
            let editor = focused_editor_mut(&mut h.stoat).expect("editor");
            let snapshot = editor.display_map.snapshot();
            snapshot
                .buffer_snapshot()
                .anchor_at(offset, stoat_text::Bias::Right)
        };
        h.stoat.marks.insert((id, 'a'), anchor);
        crate::action_handlers::jump::push_jump(&mut h.stoat);
    }

    /// The rows the cursor, the mark, and the jumplist entry parked by
    /// [`anchor_row_3`] now resolve to.
    fn anchored_rows(h: &mut TestHarness, id: BufferId) -> (u32, u32, u32) {
        let cursor = focused_cursor_row(h);

        let mark = *h.stoat.marks.get(&(id, 'a')).expect("mark stored");
        let jump = {
            let ws = h.stoat.active_workspace();
            let pane = ws.panes.pane(ws.panes.focus());
            let entry = pane.jumplist.entries().last().expect("jump recorded");
            entry.selections.first().expect("selection").start
        };

        let editor = focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf = snapshot.buffer_snapshot();
        let row_of = |anchor| buf.rope().offset_to_point(buf.resolve_anchor(&anchor)).row;
        (cursor, row_of(mark), row_of(jump))
    }

    /// Replacing the whole buffer deletes every fragment the anchors point
    /// into, and they all resolve to the same place afterwards. Editing only
    /// the changed span leaves everything below it where it was.
    #[test]
    fn pump_auto_reload_keeps_anchors_below_a_mid_file_change() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-anchors");
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", b"l0\nl1\nl2\nl3\nl4\n");
        anchor_row_3(&mut h, id);
        assert_eq!(
            anchored_rows(&mut h, id),
            (3, 3, 3),
            "all three start on row 3"
        );

        h.fake_fs().insert_file(&path, b"l0\nCHANGED\nl2\nl3\nl4\n");
        pump_poll(&mut h);

        assert_eq!(buffer_text(&h, id), "l0\nCHANGED\nl2\nl3\nl4\n");
        assert_eq!(anchored_rows(&mut h, id), (3, 3, 3));
    }

    #[test]
    fn reload_keeps_anchors_below_a_mid_file_change() {
        let mut h = Stoat::test();
        let root = Path::new("/reload-anchors");
        let path = root.join("a.txt");
        let id = open_plain(&mut h, root, "a.txt", b"l0\nl1\nl2\nl3\nl4\n");
        anchor_row_3(&mut h, id);

        h.fake_fs().insert_file(&path, b"l0\nCHANGED\nl2\nl3\nl4\n");
        reload_focused(&mut h.stoat, false);

        assert_eq!(buffer_text(&h, id), "l0\nCHANGED\nl2\nl3\nl4\n");
        assert_eq!(anchored_rows(&mut h, id), (3, 3, 3));
    }

    #[test]
    fn pump_auto_reload_leaves_a_mid_file_cursor_put() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-mid");
        let (path, _) = open_auto_reload(&mut h, &root, "log.txt", b"a\nb\nc\nd\n");
        assert_eq!(focused_cursor_row(&mut h), 0, "cursor starts mid-file");

        h.fake_fs().insert_file(&path, b"a\nb\nc\nd\ne\n");
        pump_poll(&mut h);

        assert_eq!(
            focused_cursor_row(&mut h),
            0,
            "a mid-file cursor stays put through an append"
        );
    }

    #[test]
    fn pump_auto_reload_follow_jumps_to_the_first_changed_line() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-follow-mid");
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", b"a\nb\nc\nd\n");
        h.stoat
            .active_workspace_mut()
            .buffers
            .set_auto_reload(id, AutoReloadMode::Follow);
        assert_eq!(focused_cursor_row(&mut h), 0, "cursor starts at the top");

        h.fake_fs().insert_file(&path, b"a\nb\nCHANGED\nd\n");
        pump_poll(&mut h);

        assert_eq!(buffer_text(&h, id), "a\nb\nCHANGED\nd\n");
        assert_eq!(
            focused_cursor_row(&mut h),
            2,
            "follow jumps the cursor to the first changed line"
        );
        assert!(
            !editor::focused_dirty(&h.stoat),
            "a followed reload stays clean"
        );
    }

    #[test]
    fn pump_auto_reload_follow_jumps_to_the_appended_content() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-follow-append");
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", b"a\nb\n");
        h.stoat
            .active_workspace_mut()
            .buffers
            .set_auto_reload(id, AutoReloadMode::Follow);
        assert_eq!(focused_cursor_row(&mut h), 0, "cursor starts at the top");

        h.fake_fs().insert_file(&path, b"a\nb\nc\nd\n");
        pump_poll(&mut h);

        assert_eq!(buffer_text(&h, id), "a\nb\nc\nd\n");
        assert_eq!(
            focused_cursor_row(&mut h),
            2,
            "follow jumps the cursor to the start of the appended content"
        );
    }

    #[test]
    fn pump_auto_reload_ignores_unflagged_buffers() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-unflagged");
        let path = root.join("log.txt");
        h.fake_fs().insert_file(&path, b"line1\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        let id = focused_editor_mut(&mut h.stoat).expect("editor").buffer_id;
        ensure_auto_reload_poll(&mut h.stoat);

        h.fake_fs().insert_file(&path, b"line1\nline2\n");
        pump_poll(&mut h);

        assert_eq!(
            buffer_text(&h, id),
            "line1\n",
            "an unflagged buffer is never reloaded"
        );
    }

    #[test]
    fn pump_auto_reload_disarms_when_no_buffer_is_flagged() {
        let mut h = Stoat::test();
        ensure_auto_reload_poll(&mut h.stoat);
        assert!(h.stoat.auto_reload_poll.is_some(), "poll armed");

        pump_poll(&mut h);

        assert!(
            h.stoat.auto_reload_poll.is_none(),
            "the pump drops the poll task when no buffer is flagged"
        );
    }

    #[test]
    fn open_log_buffer_flags_auto_reload_and_tails() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/logs-open");
        let path = root.join("headless-stoat-1.log");
        h.fake_fs().insert_file(&path, b"line1\nline2\nline3\n");
        h.stoat.active_workspace_mut().git_root = root;

        assert_eq!(open_log_buffer(&mut h.stoat, &path), UpdateEffect::Redraw);

        let id = focused_editor_mut(&mut h.stoat).expect("editor").buffer_id;
        let flagged = h
            .stoat
            .active_workspace()
            .buffers
            .auto_reload_paths()
            .iter()
            .any(|(fid, _, _)| *fid == id);
        assert!(flagged, "the log buffer is flagged auto-reload");
        assert!(h.stoat.auto_reload_poll.is_some(), "the poll is armed");
        assert_eq!(
            focused_cursor_row(&mut h),
            3,
            "the cursor tails the last line"
        );
    }

    #[test]
    fn open_log_buffer_reports_a_missing_log_file() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/logs-missing");
        let path = root.join("headless-stoat-1.log");
        h.stoat.active_workspace_mut().git_root = root;

        assert_eq!(open_log_buffer(&mut h.stoat, &path), UpdateEffect::Redraw);

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("no log file for this session; started with --log-stderr?")
        );
        assert!(
            h.stoat.auto_reload_poll.is_none(),
            "no poll is armed for a missing log"
        );
        assert!(
            h.stoat
                .active_workspace()
                .buffers
                .id_for_path(&path)
                .is_none(),
            "no buffer is opened for a missing log"
        );
    }

    /// Open `name` (seeded with `seed`) under `root` and return its clean,
    /// unflagged buffer id.
    fn open_plain(h: &mut TestHarness, root: &Path, name: &str, seed: &[u8]) -> BufferId {
        let path = root.join(name);
        h.fake_fs().insert_file(&path, seed);
        h.stoat.active_workspace_mut().git_root = root.to_path_buf();
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        focused_editor_mut(&mut h.stoat).expect("editor").buffer_id
    }

    fn is_flagged(h: &TestHarness, id: BufferId) -> bool {
        h.stoat
            .active_workspace()
            .buffers
            .auto_reload_paths()
            .iter()
            .any(|(fid, _, _)| *fid == id)
    }

    #[test]
    fn set_buffer_auto_reload_on_flags_and_arms() {
        let mut h = Stoat::test();
        let id = open_plain(&mut h, &PathBuf::from("/ar-on"), "a.txt", b"x\n");

        assert_eq!(
            set_buffer_auto_reload(&mut h.stoat, "on"),
            UpdateEffect::Redraw
        );

        assert!(is_flagged(&h, id), "the focused buffer is flagged");
        assert!(h.stoat.auto_reload_poll.is_some(), "the poll is armed");
        assert_eq!(h.stoat.pending_message.as_deref(), Some("auto-reload on"));
    }

    #[test]
    fn set_buffer_auto_reload_off_clears_and_pump_disarms() {
        let mut h = Stoat::test();
        let id = open_plain(&mut h, &PathBuf::from("/ar-off"), "a.txt", b"x\n");
        set_buffer_auto_reload(&mut h.stoat, "on");

        assert_eq!(
            set_buffer_auto_reload(&mut h.stoat, "OFF"),
            UpdateEffect::Redraw
        );
        assert!(!is_flagged(&h, id), "the flag is cleared");
        assert_eq!(h.stoat.pending_message.as_deref(), Some("auto-reload off"));

        pump_poll(&mut h);
        assert!(
            h.stoat.auto_reload_poll.is_none(),
            "the pump drops the poll after the last flag clears"
        );
    }

    #[test]
    fn set_buffer_auto_reload_rejects_a_bogus_argument() {
        let mut h = Stoat::test();
        let id = open_plain(&mut h, &PathBuf::from("/ar-bogus"), "a.txt", b"x\n");

        assert_eq!(
            set_buffer_auto_reload(&mut h.stoat, "maybe"),
            UpdateEffect::Redraw
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("auto-reload: expected on, off, or follow")
        );
        assert!(!is_flagged(&h, id), "no flag changed");
        assert!(h.stoat.auto_reload_poll.is_none(), "no poll armed");
    }

    #[test]
    fn set_buffer_auto_reload_on_rejects_a_scratch_buffer() {
        let mut h = Stoat::test();

        assert_eq!(
            set_buffer_auto_reload(&mut h.stoat, "on"),
            UpdateEffect::Redraw
        );

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("buffer has no file to reload")
        );
        assert!(
            h.stoat.auto_reload_poll.is_none(),
            "no poll armed for a scratch buffer"
        );
    }

    #[test]
    fn reload_replaces_a_clean_buffer_and_advances_the_mtime() {
        let mut h = Stoat::test();
        let path = Path::new("/reload-clean").join("a.txt");
        let id = open_plain(&mut h, Path::new("/reload-clean"), "a.txt", b"old\n");
        let before = h.stoat.active_workspace().buffers.disk_mtime(id);

        h.fake_fs().insert_file(&path, b"new\n");
        let effect = reload_focused(&mut h.stoat, false);

        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(buffer_text(&h, id), "new\n");
        assert!(
            !editor::focused_dirty(&h.stoat),
            "a reloaded buffer is clean"
        );
        assert!(
            h.stoat.active_workspace().buffers.disk_mtime(id) > before,
            "the reload records the advanced disk mtime",
        );
    }

    #[test]
    fn reload_on_unchanged_disk_reports_up_to_date() {
        let mut h = Stoat::test();
        let id = open_plain(&mut h, Path::new("/reload-same"), "a.txt", b"same\n");

        let effect = reload_focused(&mut h.stoat, false);

        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("already up to date")
        );
        assert_eq!(buffer_text(&h, id), "same\n");
    }

    #[test]
    fn reload_unforced_refuses_a_dirty_buffer() {
        let mut h = Stoat::test();
        open_edited(&mut h, Path::new("/reload-dirty"), "a.txt", b"disk\n");
        let id = focused_editor_mut(&mut h.stoat).expect("editor").buffer_id;

        let effect = reload_focused(&mut h.stoat, false);

        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("unsaved changes; :reload! discards them"),
        );
        assert_eq!(
            buffer_text(&h, id),
            "edited disk\n",
            "the unsaved edits survive"
        );
        assert!(editor::focused_dirty(&h.stoat), "the buffer stays dirty");
    }

    #[test]
    fn reload_forced_discards_edits_and_reloads() {
        let mut h = Stoat::test();
        let path = open_edited(&mut h, Path::new("/reload-force"), "a.txt", b"disk\n");
        let id = focused_editor_mut(&mut h.stoat).expect("editor").buffer_id;
        h.fake_fs().insert_file(&path, b"changed\n");

        let effect = reload_focused(&mut h.stoat, true);

        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(buffer_text(&h, id), "changed\n");
        assert!(
            !editor::focused_dirty(&h.stoat),
            "the forced reload clears the dirty flag"
        );
    }

    #[test]
    fn reload_reports_a_file_missing_on_disk() {
        let mut h = Stoat::test();
        let path = Path::new("/reload-gone").join("a.txt");
        let id = open_plain(&mut h, Path::new("/reload-gone"), "a.txt", b"content\n");

        h.fake_fs().remove_file(&path).expect("remove");
        let effect = reload_focused(&mut h.stoat, false);

        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("file no longer exists on disk"),
        );
        assert_eq!(
            buffer_text(&h, id),
            "content\n",
            "the buffer keeps its content"
        );
    }

    #[test]
    fn reload_rejects_a_scratch_buffer() {
        let mut h = Stoat::test();

        let effect = reload_focused(&mut h.stoat, false);

        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("buffer has no file to reload"),
        );
    }

    fn buffer_dirty(h: &TestHarness, id: BufferId) -> bool {
        h.stoat
            .active_workspace()
            .buffers
            .get(id)
            .expect("buffer")
            .read()
            .expect("poisoned")
            .dirty
    }

    #[test]
    fn reload_all_unforced_reloads_clean_skips_dirty_reports_missing() {
        let mut h = Stoat::test();
        let root = Path::new("/reload-all");
        let clean = root.join("clean.txt");
        let gone = root.join("gone.txt");
        let clean_id = open_plain(&mut h, root, "clean.txt", b"aaa\n");
        open_edited(&mut h, root, "dirty.txt", b"bbb\n");
        let dirty_id = focused_editor_mut(&mut h.stoat).expect("editor").buffer_id;
        let gone_id = open_plain(&mut h, root, "gone.txt", b"ccc\n");

        h.fake_fs().insert_file(&clean, b"AAA\n");
        h.fake_fs().remove_file(&gone).expect("remove");

        let effect = reload_all(&mut h.stoat, false);

        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(
            buffer_text(&h, clean_id),
            "AAA\n",
            "the clean buffer reloads"
        );
        assert_eq!(
            buffer_text(&h, dirty_id),
            "edited bbb\n",
            "the dirty buffer keeps its edits",
        );
        assert_eq!(
            buffer_text(&h, gone_id),
            "ccc\n",
            "the missing buffer keeps its content",
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("reloaded 1; 1 unsaved skipped (:reload-all! discards); 1 missing on disk"),
        );
    }

    #[test]
    fn reload_all_forced_replaces_dirty_and_still_reports_missing() {
        let mut h = Stoat::test();
        let root = Path::new("/reload-all-force");
        let dirty = root.join("dirty.txt");
        let gone = root.join("gone.txt");
        open_edited(&mut h, root, "dirty.txt", b"bbb\n");
        let dirty_id = focused_editor_mut(&mut h.stoat).expect("editor").buffer_id;
        let gone_id = open_plain(&mut h, root, "gone.txt", b"ccc\n");

        h.fake_fs().insert_file(&dirty, b"BBB\n");
        h.fake_fs().remove_file(&gone).expect("remove");

        let effect = reload_all(&mut h.stoat, true);

        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(
            buffer_text(&h, dirty_id),
            "BBB\n",
            "the forced reload replaces the dirty buffer",
        );
        assert!(
            !buffer_dirty(&h, dirty_id),
            "the forced reload clears the dirty flag",
        );
        assert_eq!(
            buffer_text(&h, gone_id),
            "ccc\n",
            "the missing buffer keeps its content",
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("reloaded 1; 1 missing on disk"),
        );
    }

    #[test]
    fn reload_all_reports_up_to_date_when_nothing_changed() {
        let mut h = Stoat::test();
        let root = Path::new("/reload-all-same");
        open_plain(&mut h, root, "a.txt", b"aaa\n");
        open_plain(&mut h, root, "b.txt", b"bbb\n");

        let effect = reload_all(&mut h.stoat, false);

        assert_eq!(effect, UpdateEffect::Redraw);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("all files up to date"),
        );
    }

    #[test]
    fn space_r_reloads_all_and_returns_to_normal() {
        let mut h = Stoat::test();
        let root = Path::new("/space-r");
        let path = root.join("a.txt");
        let id = open_plain(&mut h, root, "a.txt", b"before\n");

        h.fake_fs().insert_file(&path, b"after\n");
        h.type_keys("space r");

        assert_eq!(
            buffer_text(&h, id),
            "after\n",
            "space r reloads the buffer from disk",
        );
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "space r returns to normal mode",
        );
    }

    #[test]
    fn set_buffer_auto_reload_follow_twice_toggles_off() {
        let mut h = Stoat::test();
        let id = open_plain(&mut h, &PathBuf::from("/ar-follow"), "a.txt", b"x\n");

        assert_eq!(
            set_buffer_auto_reload(&mut h.stoat, "follow"),
            UpdateEffect::Redraw
        );
        assert!(is_flagged(&h, id), "follow flags the buffer");
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("auto-reload follow")
        );

        assert_eq!(
            set_buffer_auto_reload(&mut h.stoat, "follow"),
            UpdateEffect::Redraw
        );
        assert!(!is_flagged(&h, id), "a second follow unflags the buffer");
        assert_eq!(h.stoat.pending_message.as_deref(), Some("auto-reload off"));
    }

    #[test]
    fn set_buffer_auto_reload_follow_rejects_a_scratch_buffer() {
        let mut h = Stoat::test();

        assert_eq!(
            set_buffer_auto_reload(&mut h.stoat, "follow"),
            UpdateEffect::Redraw
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("buffer has no file to reload")
        );
        assert!(
            h.stoat.auto_reload_poll.is_none(),
            "no poll armed for a scratch buffer"
        );
    }

    #[test]
    fn set_auto_reload_config_flips_the_setting_both_ways() {
        let mut h = Stoat::test();

        assert_eq!(
            set_auto_reload_config(&mut h.stoat, "off"),
            UpdateEffect::Redraw
        );
        assert_eq!(h.stoat.settings.config_auto_reload, Some(false));
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("auto-reload-config off")
        );

        assert_eq!(
            set_auto_reload_config(&mut h.stoat, "ON"),
            UpdateEffect::Redraw
        );
        assert_eq!(h.stoat.settings.config_auto_reload, Some(true));
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("auto-reload-config on")
        );
    }

    #[test]
    fn set_auto_reload_config_rejects_a_bogus_argument() {
        let mut h = Stoat::test();
        let before = h.stoat.settings.config_auto_reload;

        assert_eq!(
            set_auto_reload_config(&mut h.stoat, "sometimes"),
            UpdateEffect::Redraw
        );

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("auto-reload-config: expected on or off")
        );
        assert_eq!(
            h.stoat.settings.config_auto_reload, before,
            "a bad argument leaves the setting untouched"
        );
    }

    #[test]
    fn reloading_a_crlf_file_keeps_the_buffer_in_lf() {
        // `:e` re-reads from disk straight into the buffer, so it needs the
        // same normalization the open path does.
        let mut h = Stoat::test();
        let root = PathBuf::from("/crlf-reload");
        let path = root.join("a.rs");
        h.fake_fs().insert_file(&path, b"one\r\ntwo\r\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        h.fake_fs().insert_file(&path, b"one\r\ntwo\r\nthree\r\n");

        reload_focused(&mut h.stoat, true);
        h.settle();

        let id = focused_editor_mut(&mut h.stoat).expect("editor").buffer_id;
        assert_eq!(buffer_text(&h, id), "one\ntwo\nthree\n");
    }

    #[test]
    fn target_log_stem_resolves_each_target() {
        let ident = Some("headless-stoat-20260718-143022-1");
        let sid = Some("20260718-143022-7");

        assert_eq!(
            target_log_stem(None, ident, sid),
            Ok(Some("headless-stoat-20260718-143022-1".to_string())),
            "an omitted target is this stoat's own log",
        );
        assert_eq!(
            target_log_stem(Some("stoat"), ident, sid),
            Ok(Some("headless-stoat-20260718-143022-1".to_string())),
            "the stoat target is the same as omitting it",
        );
        assert_eq!(
            target_log_stem(Some("stoatty"), ident, sid),
            Ok(Some("stoatty-20260718-143022-7".to_string())),
            "the stoatty target puts the prefix back on the exported id",
        );

        assert_eq!(
            target_log_stem(None, None, sid),
            Ok(None),
            "a stoat that named no log file resolves no stem",
        );
        assert_eq!(
            target_log_stem(Some("stoatty"), ident, None),
            Err("logs: not running inside stoatty"),
            "no exported id means no enclosing stoatty",
        );
        assert_eq!(
            target_log_stem(Some("x"), ident, sid),
            Err("logs: expected stoat or stoatty"),
            "an unknown target names neither program",
        );
    }

    #[test]
    fn open_logs_reports_a_stoatty_target_outside_stoatty() {
        let mut h = Stoat::test();

        assert_eq!(
            open_logs(&mut h.stoat, Some("stoatty")),
            UpdateEffect::Redraw
        );

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("logs: not running inside stoatty"),
        );
        assert!(
            h.stoat.auto_reload_poll.is_none(),
            "no poll is armed when nothing opened"
        );
    }

    #[test]
    fn open_logs_reports_an_unknown_target() {
        let mut h = Stoat::test();

        assert_eq!(open_logs(&mut h.stoat, Some("x")), UpdateEffect::Redraw);

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("logs: expected stoat or stoatty"),
        );
        assert!(
            h.stoat.auto_reload_poll.is_none(),
            "no poll is armed when nothing opened"
        );
    }

    #[test]
    fn session_log_path_names_the_ident_stem() {
        let dir = Path::new("/logs");
        assert_eq!(
            session_log_path(dir, Some("headless-stoat-20260718-143022-1")),
            Some(PathBuf::from("/logs/headless-stoat-20260718-143022-1.log")),
            "the stem names the file under the log dir",
        );
        assert_eq!(
            session_log_path(dir, None),
            None,
            "a session that named no log file resolves no path",
        );
    }

    /// A read whose buffer moved while it ran is dropped, not applied.
    ///
    /// The comparison happens on the pool against the text the buffer held
    /// when the stat fired. An edit landing before the result does means the
    /// spans it carries name text that has since moved, and applying them
    /// would splice the file into the wrong place.
    #[test]
    fn a_reload_whose_buffer_moved_under_it_is_dropped() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/auto-reload-raced");
        let (path, id) = open_auto_reload(&mut h, &root, "log.txt", b"line1\n");

        h.fake_fs().insert_file(&path, b"line1\nline2\n");
        pump_auto_reload(&mut h.stoat);
        h.run_until_parked();

        // The user types while the read is out, which is what moves the
        // version the comparison was made against.
        {
            let buffer = h
                .stoat
                .active_workspace()
                .buffers
                .get(id)
                .expect("the buffer is open");
            let mut guard = buffer.write().expect("buffer poisoned");
            guard.edit(0..0, "typed\n");
        }

        assert!(
            !pump_auto_reload_install(&mut h.stoat),
            "the result answers a buffer that has since moved",
        );
        assert_eq!(
            buffer_text(&h, id),
            "typed\nline1\n",
            "so the edit stands and the file's line never landed",
        );

        // The next poll reads again, against what the buffer holds now.
        h.stoat
            .active_workspace_mut()
            .buffers
            .get(id)
            .expect("the buffer is open")
            .write()
            .expect("buffer poisoned")
            .mark_clean();
        assert!(pump_poll(&mut h), "the poll after it reloads for real");
        assert_eq!(
            buffer_text(&h, id),
            "line1\nline2\n",
            "and the file replaces what the buffer held",
        );
    }
}

use super::{amend::AmendRoute, movement::ChangeDir};
use crate::{
    app::{Stoat, UpdateEffect},
    buffer::BufferId,
    diff_cache::{DiffCache, DiffCacheKey},
    display_map::syntax_theme::SyntaxStyles,
    editor_state::EditorId,
    host::GitRepo,
    review::{line_count, ReviewFileInput, ReviewHunk},
    review_apply::{
        base_line_range, hunk_rows, hunk_to_patch, line_restricted_rows, rows_to_unified_diff,
        HUNK_CONTEXT,
    },
    review_session::DiffDocument,
    workspace::diff::{compute_base_highlights, BaseHighlightCache, DiffBase},
};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use stoat_language::{Language, LanguageRegistry};
use stoat_text::{Point, SelectionGoal};

/// Cache key matching what [`populate_diff_cache`] writes, so a scan reads back
/// the hunks a prior install stored for an unchanged file.
pub(crate) fn diff_cache_key(
    base: &str,
    buffer: &str,
    language: Option<&Arc<Language>>,
) -> DiffCacheKey {
    DiffCacheKey {
        left_hash: blake3::hash(base.as_bytes()).into(),
        right_hash: blake3::hash(buffer.as_bytes()).into(),
        language: language.map(|l| l.name.to_string()),
    }
}

pub(super) fn emit_review_info_badge(stoat: &mut Stoat, label: &str) {
    use crate::badge::{Anchor, Badge, BadgeSource, BadgeState};
    let ws = stoat.active_workspace_mut();
    ws.badges.remove_by_source(BadgeSource::Review);
    ws.badges.insert(Badge {
        source: BadgeSource::Review,
        anchor: Anchor::BottomRight,
        state: BadgeState::Active,
        label: label.to_string(),
        detail: None,
    });
}

pub(super) fn emit_review_error_badge(stoat: &mut Stoat, label: &str, detail: Option<String>) {
    use crate::badge::{Anchor, Badge, BadgeSource, BadgeState};
    let ws = stoat.active_workspace_mut();
    ws.badges.remove_by_source(BadgeSource::Review);
    ws.badges.insert(Badge {
        source: BadgeSource::Review,
        anchor: Anchor::BottomRight,
        state: BadgeState::Error,
        label: label.to_string(),
        detail,
    });
}

pub(super) fn commits_open_review(stoat: &mut Stoat) -> UpdateEffect {
    let Some((workdir, commit)) = stoat.active_workspace().commits.as_ref().and_then(|state| {
        let commit = state.commits.get(state.selected)?;
        Some((state.workdir.clone(), commit.clone()))
    }) else {
        return UpdateEffect::None;
    };
    super::review_walk::walk_one_commit(stoat, workdir, commit)
}

/// Land an agent's proposed edits as unsaved buffer edits over their own base.
///
/// Each proposal replaces its file's buffer content, and the base texts become
/// a [`DiffBase::Memory`] override, so `:diff` shows the proposal against what
/// the agent read. The first edited file opens with the latch armed.
///
/// A proposal exists nowhere in git, which is what separates this from the
/// commit openers. No revision holds what the agent read, so the base travels
/// with the edits instead.
///
/// Landing the proposals as ordinary unsaved edits is what gives the reader the
/// accept and reject verbs for free. Saving writes a file, `:reload` throws it
/// away, and undo backs one file out at a time, because each proposal is a
/// single edit.
///
/// Does nothing when no proposal opens.
pub(super) fn open_review_agent_edits(stoat: &mut Stoat, edits: &[stoat_action::AgentEdit]) {
    let git_root = stoat.active_workspace().git_root.clone();
    let mut base_texts: std::collections::HashMap<std::path::PathBuf, Arc<String>> =
        std::collections::HashMap::new();
    let mut first: Option<std::path::PathBuf> = None;

    for edit in edits {
        // Join rather than test for absoluteness, since an absolute proposal
        // path comes back from the join unchanged.
        let path = git_root.join(&edit.path);
        let Some(buffer_id) = super::file::open_file(stoat, &path) else {
            continue;
        };
        let Some(buffer) = stoat.active_workspace().buffers.get(buffer_id) else {
            continue;
        };
        {
            let mut guard = buffer.write().expect("buffer poisoned");
            let len = guard.snapshot.visible_text.len();
            guard.edit(0..len, &edit.proposed_text);
        }

        base_texts.insert(path.clone(), edit.base_text.clone());
        first.get_or_insert(path);
    }

    let Some(first) = first else {
        return;
    };
    stoat
        .active_workspace_mut()
        .set_diff_base(Some(DiffBase::Memory { files: base_texts }));
    super::file::open_file(stoat, &first);
    enter_diff_view(stoat);
    stoat.set_status("agent edits: save accepts, :reload rejects");
}

/// Leave the diff view, reporting whether there was one to leave.
///
/// Clears the focused editor's flag, the focused pane's latch, and the widen
/// the view took, which is every piece of state entering it set. Returns false
/// and touches nothing when no editor is focused or neither half is set, so a
/// caller can offer the exit unconditionally.
///
/// Either half being set counts as on, so this leaves a latched pane showing a
/// clean file as readily as a diff itself.
/// Open the diff against `rev`, or toggle the working-tree diff when `None`.
///
/// A revision points the whole workspace at that commit, so every buffer diffs
/// against it and the change list spans everything committed since. Opening the
/// view from a buffer with no hunks of its own then crosses into the first file
/// in that list, which is what [`toggle_diff_view`] already does and what makes
/// a revision land somewhere worth reading: with a base further back, the file
/// the reader wants is rarely the one on screen.
///
/// A revision the repository cannot resolve changes nothing at all: it badges
/// and leaves the view, the latch, and the base where they were. A command that
/// half-applies leaves the reader worse off than one that refuses.
pub(super) fn diff(stoat: &mut Stoat, rev: Option<&str>) -> UpdateEffect {
    let Some(rev) = rev else {
        // The bare command also drops any base a revision left installed, so
        // closing the diff always returns to the working tree's own HEAD.
        toggle_diff_view(stoat);
        return UpdateEffect::Redraw;
    };

    let git_root = stoat.active_workspace().git_root.clone();
    let Some(repo) = stoat.git_host.discover(&git_root) else {
        emit_review_error_badge(stoat, "not in a git repository", None);
        return UpdateEffect::Redraw;
    };
    let Some(sha) = repo.resolve_rev(rev) else {
        emit_review_error_badge(
            stoat,
            "unknown revision",
            Some(format!("no revision named {rev}")),
        );
        return UpdateEffect::Redraw;
    };

    stoat
        .active_workspace_mut()
        .set_diff_base(Some(DiffBase::Rev { sha: Some(sha) }));
    // Turned on rather than toggled. Naming a revision asks to look at it, so a
    // second `:diff <rev>` re-targets the view rather than closing it; only the
    // bare command closes.
    enter_diff_view(stoat);
    UpdateEffect::Redraw
}

/// Point the diff view at what `sha` changed, with the tree already on it.
///
/// The base becomes the commit's first parent, so the diff on screen is the
/// commit itself. A root commit has no parent, and the empty tree is the base
/// that makes every one of its lines read as added rather than as nothing.
///
/// The caller is responsible for the tree: this reads the commit to decide what
/// to open, but the buffers it opens are whatever is on disk, so a tree that is
/// not on `sha` yields a diff of something else entirely.
///
/// Returns whether a file was opened. A commit that changed nothing has none to
/// open, so the view is left exactly where it was and only the base moves.
pub(super) fn land_diff_on_commit(
    stoat: &mut Stoat,
    repo: &dyn GitRepo,
    workdir: &Path,
    sha: &str,
) -> bool {
    stoat
        .active_workspace_mut()
        .set_diff_base(Some(DiffBase::Rev {
            sha: repo.parent_sha(sha),
        }));

    let Some(rel_path) = repo.commit_first_path(sha) else {
        return false;
    };
    super::file::open_file(stoat, &workdir.join(rel_path));
    enter_diff_view(stoat);
    true
}

/// Turn the diff view on, whatever it was showing before.
///
/// [`toggle_diff_view`] closes an open view, so a caller that means "show this"
/// rather than "flip this" has to shut the view before opening it. Every such
/// caller arrives with a base it just installed, and the exit half leaves a
/// base alone by design, so the round trip re-reads the new base rather than
/// dropping it.
pub(super) fn enter_diff_view(stoat: &mut Stoat) {
    exit_diff_view(stoat);
    toggle_diff_view(stoat);
}

pub(super) fn exit_diff_view(stoat: &mut Stoat) -> bool {
    let latched = {
        let panes = &stoat.active_workspace().panes;
        panes.pane(panes.focus()).diff_mode
    };
    let Some(editor) = super::focused_editor_mut(stoat) else {
        return false;
    };
    if !(latched || editor.diff_view) {
        return false;
    }
    editor.set_diff_view(false);

    let panes = &mut stoat.active_workspace_mut().panes;
    let focus = panes.focus();
    panes.pane_mut(focus).diff_mode = false;
    if panes.widened() == Some(focus) {
        panes.unwiden();
    }
    true
}

/// Toggle the live per-file diff view on the focused editor, driven by
/// [`stoat_action::Diff`].
///
/// Flips `diff_view` (and the display map's deleted-block splicing) on the
/// focused editor. Unlike the session review there is no scan, no session, and
/// no scratch buffer -- the editor stays the real, editable file buffer, so
/// re-pressing toggles the two columns off again.
///
/// Opening the view lands the cursor on the focused buffer's first change chunk
/// (the hunk `n` reaches from the top of the file) and pushes the pre-jump
/// position to the jumplist, so the usual jump-back returns. When the focused
/// buffer has no changes of its own, opening the view instead crosses into the
/// first changed file and lands on its first hunk. Toggling the view off leaves
/// the cursor untouched.
///
/// See also:
/// - [`exit_diff_view`], the exit half, which other screens reuse.
pub(super) fn toggle_diff_view(stoat: &mut Stoat) {
    let origin = super::jump::live_entry(stoat);
    let Some(buffer_id) = super::focused_editor_mut(stoat).map(|editor| editor.buffer_id) else {
        return;
    };

    if exit_diff_view(stoat) {
        stoat.active_workspace_mut().set_diff_base(None);
        return;
    }

    {
        let Some(editor) = super::focused_editor_mut(stoat) else {
            return;
        };
        editor.set_diff_view(true);

        // Give the diff its own full width. An unwidenable layout stays put and
        // rides the unified fallback.
        let panes = &mut stoat.active_workspace_mut().panes;
        let focus = panes.focus();
        panes.pane_mut(focus).diff_mode = true;
        panes.widen(focus);
    }

    if let Some((editor_id, _)) = stoat.focused_editor_ids() {
        ensure_diff_map(stoat, editor_id, buffer_id);
    }

    let jumped = super::focused_editor_mut(stoat).is_some_and(|editor| {
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        // The first stop rather than the first hunk, so opening over a refined
        // block lands on the row that changed instead of the block's top.
        let target_row = display_snapshot.diff_map().and_then(|diff_map| {
            diff_map
                .live_hunks(buffer_snapshot)
                .change_stops()
                .first()
                .map(|stop| stop.start)
        });
        let Some(target_row) = target_row else {
            return false;
        };
        let target_offset = buffer_snapshot
            .rope()
            .point_to_offset(Point::new(target_row, 0));
        editor.selections.transform(buffer_snapshot, |sel| {
            crate::selection::land_block_cursor(
                sel.id,
                target_offset,
                SelectionGoal::None,
                buffer_snapshot.rope(),
                buffer_snapshot,
            )
        });
        true
    });

    // The jump only moves the selection. Pull the view onto it here so a
    // non-key dispatch (the `stoat review` startup, a mouse palette accept)
    // lands scrolled, not relying on the Key-event epilogue.
    if jumped {
        let scrolloff = stoat.settings.scrolloff.unwrap_or(3);
        if let Some(editor) = super::focused_editor_mut(stoat) {
            super::view::ensure_cursor_in_view(editor, scrolloff);
        }
    }

    if jumped && let Some(entry) = origin {
        super::jump::push_entry(stoat, entry);
    }

    // A buffer with no changes of its own has no hunk to land on, so cross into
    // the first changed file. This makes `:diff` from a scratch or unchanged
    // buffer open a real diff instead of silently toggling empty columns.
    if !jumped {
        let _ = super::movement::goto_change(stoat, ChangeDir::Next);
    }
}

/// Make sure `editor_id`'s display map holds a current diff map for
/// `buffer_id`, and report whether it found any hunk to show.
///
/// The editor is named rather than taken as the focused one, because the pane a
/// navigation targets is not always the focused pane, and an answer from the
/// wrong editor's map opens the wrong view.
///
/// The background diff job usually has the map ready, since it drives the
/// gutter marks, so the synchronous install below runs only when the fast path
/// is empty. A map left over from before an external git mutation counts as
/// empty. Such a map describes a base that has since moved, so trusting it
/// opens the view on hunks measured against a HEAD that no longer exists.
///
/// Only the hunks are computed here. The left column's colors cost a parse of
/// the whole base file, which the background pass takes on and lands a settle
/// later.
///
/// `false` is the answer for a clean or untracked file, which is what tells a
/// latched pane to show it plain.
pub(crate) fn ensure_diff_map(stoat: &mut Stoat, editor_id: EditorId, buffer_id: BufferId) -> bool {
    let map_current = stoat.active_workspace().diff_map_current(buffer_id);
    let has_map = stoat
        .active_workspace_mut()
        .editors
        .get_mut(editor_id)
        .is_some_and(|editor| editor.display_map.snapshot().diff_map().is_some());
    if !has_map || !map_current {
        let git_host = stoat.git_host.clone();
        let language_registry = stoat.language_registry.clone();
        let syntax_styles = stoat.syntax_styles.clone();
        let base_cache = stoat.base_highlights_cache.clone();
        stoat.active_workspace_mut().install_diff_map_now(
            &git_host,
            &language_registry,
            &syntax_styles,
            &base_cache,
            buffer_id,
        );
    }

    stoat
        .active_workspace_mut()
        .editors
        .get_mut(editor_id)
        .is_some_and(|editor| {
            editor
                .display_map
                .snapshot()
                .diff_map()
                .is_some_and(|diff_map| !diff_map.hunks_in_range(0..u32::MAX).is_empty())
        })
}

/// How [`stage_hunk`] acts on the hunk under the cursor.
#[derive(Clone, Copy)]
pub(super) enum HunkStage {
    Stage,
    Unstage,
    Toggle,
}

/// Stage, unstage, or toggle the git-index state of the diff hunk under the
/// cursor in the focused editor.
///
/// The hunk is resolved by diffing the file's HEAD content against the live
/// buffer and taking the one whose buffer rows hold the cursor, which is the
/// rule the gutter marks by, so the staged unit is the one drawn there. The
/// action works in any editor view on a git-tracked file. A missing repo, an
/// untracked file, or a cursor on a row no hunk covers sets a status message
/// and changes nothing.
///
/// [`HunkStage::Toggle`] has no staged-state signal to read yet, so it stages
/// by applying the forward patch and, only when that fails because the hunk is
/// already staged, unstages by applying the reverse patch.
pub(super) fn stage_hunk(stoat: &mut Stoat, mode: HunkStage) -> UpdateEffect {
    let Some((_editor_id, buffer_id)) = stoat.focused_editor_ids() else {
        return UpdateEffect::None;
    };

    let (cursor_row, buffer_text) = {
        let Some(editor) = super::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor().clone();
        let head = buffer_snapshot.resolve_anchor(&sel.head());
        let cursor_row = buffer_snapshot.rope().offset_to_point(head).row;
        (cursor_row, buffer_snapshot.rope().to_string())
    };

    let Some(path) = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)
    else {
        return UpdateEffect::None;
    };
    let git_root = stoat.active_workspace().git_root.clone();

    let Some(repo) = stoat.git_host.discover(&git_root) else {
        stoat.set_status("not in a git repository");
        return UpdateEffect::Redraw;
    };

    // Decided before the hunk is resolved, so a refusal says why the transport
    // is unavailable rather than reporting whatever sits under the cursor.
    match super::amend::amend_route(stoat, &*repo) {
        AmendRoute::Index => {},
        AmendRoute::Commit(target) => {
            let site = super::amend::HunkSite {
                buffer_id,
                path: &path,
                cursor_row,
                buffer_text: &buffer_text,
            };
            return super::amend::amend_hunk(
                stoat,
                &*repo,
                &target,
                mode,
                super::amend::AmendUnit::Hunk,
                site,
            );
        },
        AmendRoute::Refused => {
            stoat.set_status(super::amend::REFUSED_BADGE);
            return UpdateEffect::Redraw;
        },
    }

    let Some(base_text) = repo.head_content(&path) else {
        stoat.set_status("no hunk under the cursor");
        return UpdateEffect::Redraw;
    };

    let rel = path.strip_prefix(&git_root).unwrap_or(&path).to_path_buf();
    let hunks = {
        let result = stoat_language::structural_diff::diff(&base_text, &buffer_text);
        crate::diff_map::changes_to_hunks(&result.changes, &base_text, &buffer_text)
    };

    // Resolved by the gutter's own rule, so the staged unit is the one drawn
    // under the cursor. A zero-width range is a deletion or a move, which the
    // gutter marks at its anchor row.
    let Some(k) = hunks.iter().position(|hunk| {
        let rows = &hunk.buffer_line_range;
        match rows.is_empty() {
            true => rows.start == cursor_row,
            false => rows.contains(&cursor_row),
        }
    }) else {
        stoat.set_status("no hunk under the cursor");
        return UpdateEffect::Redraw;
    };

    let (Some(forward), Some(reverse)) = (
        hunk_to_patch(&rel, &base_text, &buffer_text, &hunks, k, false),
        hunk_to_patch(&rel, &base_text, &buffer_text, &hunks, k, true),
    ) else {
        stoat.set_status("no hunk under the cursor");
        return UpdateEffect::Redraw;
    };

    let result = match mode {
        HunkStage::Stage => repo.apply_to_index(&forward).map(|()| "staged hunk"),
        HunkStage::Unstage => repo.apply_to_index(&reverse).map(|()| "unstaged hunk"),
        HunkStage::Toggle => match repo.apply_to_index(&forward) {
            Ok(()) => Ok("staged hunk"),
            Err(_) => repo.apply_to_index(&reverse).map(|()| "unstaged hunk"),
        },
    };

    match result {
        Ok(message) => {
            stoat.active_workspace_mut().invalidate_diff(buffer_id);
            stoat.set_status(message);
        },
        Err(err) => stoat.set_status(format!("could not update staging: {err}")),
    }

    UpdateEffect::Redraw
}

/// Stage, unstage, or toggle the git-index state of only the cursor line's
/// change, in the focused editor on any git-tracked file.
///
/// Unlike [`stage_hunk`], the emitted patch is restricted to the cursor's line
/// via [`line_restricted_rows`], and staging diffs the git index (not HEAD)
/// against the live buffer so sequential single-line stages inside one hunk
/// compose. A missing repo, an untracked file, or a cursor on no change sets a
/// status message and changes nothing.
///
/// The index is not the only target. Under a review base the staged side is the
/// checked-out commit, so the keys amend the cursor's line into or out of it
/// through [`amend::amend_hunk`], leaving the rest of the hunk where it was.
/// [`stage_hunk`] moves the whole hunk the same way.
pub(super) fn stage_line(stoat: &mut Stoat, mode: HunkStage) -> UpdateEffect {
    let Some((_editor_id, buffer_id)) = stoat.focused_editor_ids() else {
        return UpdateEffect::None;
    };

    let (cursor_row, buffer_text) = {
        let Some(editor) = super::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor().clone();
        let head = buffer_snapshot.resolve_anchor(&sel.head());
        let cursor_row = buffer_snapshot.rope().offset_to_point(head).row;
        (cursor_row, buffer_snapshot.rope().to_string())
    };

    let Some(path) = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)
    else {
        return UpdateEffect::None;
    };
    let git_root = stoat.active_workspace().git_root.clone();

    let Some(repo) = stoat.git_host.discover(&git_root) else {
        stoat.set_status("not in a git repository");
        return UpdateEffect::Redraw;
    };

    // Read before the line is resolved, so a refusal names the missing
    // transport rather than whatever sits under the cursor.
    match super::amend::amend_route(stoat, &*repo) {
        AmendRoute::Index => {},
        AmendRoute::Commit(target) => {
            let site = super::amend::HunkSite {
                buffer_id,
                path: &path,
                cursor_row,
                buffer_text: &buffer_text,
            };
            return super::amend::amend_hunk(
                stoat,
                &*repo,
                &target,
                mode,
                super::amend::AmendUnit::Line,
                site,
            );
        },
        AmendRoute::Refused => {
            stoat.set_status(super::amend::REFUSED_BADGE);
            return UpdateEffect::Redraw;
        },
    }

    let Some(head_text) = repo.head_content(&path) else {
        stoat.set_status("no line change under the cursor");
        return UpdateEffect::Redraw;
    };
    let index_text = repo
        .index_content(&path)
        .unwrap_or_else(|| head_text.clone());

    let rel = path
        .strip_prefix(&git_root)
        .unwrap_or(&path)
        .to_string_lossy()
        .into_owned();

    let stage = || stage_line_patch(&rel, &index_text, &buffer_text, cursor_row);
    let unstage = || unstage_line_patch(&rel, &index_text, &head_text, &buffer_text, cursor_row);
    let patch_and_message = match mode {
        HunkStage::Stage => stage().map(|patch| (patch, "staged line")),
        HunkStage::Unstage => unstage().map(|patch| (patch, "unstaged line")),
        HunkStage::Toggle => stage()
            .map(|patch| (patch, "staged line"))
            .or_else(|| unstage().map(|patch| (patch, "unstaged line"))),
    };

    let Some((patch, message)) = patch_and_message else {
        stoat.set_status("no line change under the cursor");
        return UpdateEffect::Redraw;
    };

    match repo.apply_to_index(&patch) {
        Ok(()) => {
            stoat.active_workspace_mut().invalidate_diff(buffer_id);
            stoat.set_status(message);
        },
        Err(err) => stoat.set_status(format!("could not update staging: {err}")),
    }

    UpdateEffect::Redraw
}

/// Line hunks between `base` and `buffer`, the extents every staging path
/// resolves against.
fn line_hunks(base: &str, buffer: &str) -> Vec<crate::diff_map::DiffHunk> {
    let result = stoat_language::structural_diff::diff(base, buffer);
    crate::diff_map::changes_to_hunks(&result.changes, base, buffer)
}

/// Build the patch that stages only the cursor line by diffing the git index
/// against the live buffer and keeping the cursor row's change.
///
/// `None` when the cursor sits on no index-vs-buffer change.
fn stage_line_patch(
    rel: &str,
    index_text: &str,
    buffer_text: &str,
    cursor_row: u32,
) -> Option<String> {
    let hunks = line_hunks(index_text, buffer_text);
    let k = hunks
        .iter()
        .position(|hunk| hunk.buffer_line_range.contains(&cursor_row))?;
    let hunk_rows = hunk_rows(index_text, buffer_text, &hunks, k, HUNK_CONTEXT)?;
    let rows = line_restricted_rows(&hunk_rows, cursor_row + 1, true)?;
    Some(rows_to_unified_diff(
        Path::new(rel),
        index_text,
        buffer_text,
        &rows,
    ))
}

/// Build the patch that unstages only the cursor line by reverting its index
/// row to HEAD.
///
/// The cursor row is a buffer coordinate, so it is first mapped back to the
/// index by removing the line delta of every index-vs-buffer hunk above it.
/// Diffing the index against HEAD then expresses the staged change as an
/// index-side row, and the forward patch reverts that one row to HEAD. `None`
/// when the mapped row carries no staged change.
fn unstage_line_patch(
    rel: &str,
    index_text: &str,
    head_text: &str,
    buffer_text: &str,
    cursor_row: u32,
) -> Option<String> {
    let index_row = map_buffer_row_to_index(index_text, buffer_text, cursor_row);
    // The emitted patch runs index to HEAD, so the index is this diff's base
    // side and the staged row is a base row. DiffHunk records base bytes rather
    // than base lines, so the range comes from base_line_range.
    let hunks = line_hunks(index_text, head_text);
    let k =
        (0..hunks.len()).find(|&k| base_line_range(index_text, &hunks, k).contains(&index_row))?;
    let hunk_rows = hunk_rows(index_text, head_text, &hunks, k, HUNK_CONTEXT)?;
    let rows = line_restricted_rows(&hunk_rows, index_row + 1, false)?;
    Some(rows_to_unified_diff(
        Path::new(rel),
        index_text,
        head_text,
        &rows,
    ))
}

/// Map a 0-based buffer row to its 0-based row in the git index.
///
/// Each index-vs-buffer hunk fully above the cursor shifts buffer rows past the
/// index by its added-minus-removed line count, so subtracting that shift
/// recovers the index row. The `index_text` slicing keys off each hunk's base
/// byte range to count its index lines.
fn map_buffer_row_to_index(index_text: &str, buffer_text: &str, cursor_row: u32) -> u32 {
    let mut shift: i64 = 0;
    for hunk in line_hunks(index_text, buffer_text) {
        if hunk.buffer_line_range.end <= cursor_row {
            let buffer_len = (hunk.buffer_line_range.end - hunk.buffer_line_range.start) as i64;
            let index_len =
                line_count(index_text.get(hunk.base_byte_range.clone()).unwrap_or("")) as i64;
            shift += buffer_len - index_len;
        }
    }
    (cursor_row as i64 - shift).max(0) as u32
}

/// The files `new` changed against `base`, falling back to what `new` holds
/// outright when `base` cannot be read.
///
/// A base the repository cannot produce is treated as no base at all, so the
/// commit still shows as whole-file additions rather than as nothing. The scans
/// have always been this forgiving, having read the base tree with a default on
/// failure.
pub(super) fn changed_or_whole(
    repo: &dyn GitRepo,
    base: Option<&str>,
    new: &str,
) -> Option<Vec<(std::path::PathBuf, String, String)>> {
    repo.changed_contents(base, new)
        .or_else(|| repo.changed_contents(None, new))
}

/// Common builder used by the commit scans, over the changed files a repo
/// reported as `(repo-relative path, base, buffer)`.
///
/// A pair whose two sides are equal is skipped, so a caller that reports a
/// path without a content change contributes nothing.
///
/// Takes the language registry directly rather than `&Stoat` so it runs
/// inside the off-loop scan closures, including the commit-preview build in
/// the sibling `commits` module.
pub(super) fn build_document_from_changes(
    langs: &LanguageRegistry,
    workdir: &Path,
    changes: Vec<(std::path::PathBuf, String, String)>,
) -> Option<DiffDocument> {
    if changes.is_empty() {
        return None;
    }
    let mut doc = DiffDocument::default();
    let mut inputs: Vec<ReviewFileInput> = Vec::new();
    for (rel, base, buffer) in changes {
        if base == buffer {
            continue;
        }
        let abs = workdir.join(&rel);
        let lang = langs.for_path(&abs);
        inputs.push(ReviewFileInput {
            path: abs,
            rel_path: rel.display().to_string(),
            language: lang,
            base_text: Arc::new(base),
            buffer_text: Arc::new(buffer),
        });
    }
    doc.add_files(inputs);
    if doc.order.is_empty() {
        return None;
    }
    Some(doc)
}

/// Bake tree-sitter spans for both sides of every file the session holds.
///
/// A preview paints the same token colors the editor does, so the spans have
/// to exist before paint. They are resolved here, on the blocking pool that
/// builds the session, rather than at paint time on the UI thread.
///
/// The memoized pipeline means an unchanged text parses once and resolves once
/// per theme, so a session over files the last preview already read costs hash
/// lookups. A file with no language is skipped and keeps `None`, which paints
/// untokenized, matching a syntax-off editor.
///
/// The styles bake against the theme in force here, so whoever switches themes
/// drops the documents holding them.
pub(super) fn attach_preview_highlights(
    doc: &mut DiffDocument,
    styles: &SyntaxStyles,
    cache: &BaseHighlightCache,
) {
    for file in &mut doc.files {
        let Some(language) = file.language.clone() else {
            continue;
        };
        file.base_highlights = Some(compute_base_highlights(
            &file.base_text,
            &language,
            styles,
            cache,
            None,
        ));
        file.buffer_highlights = Some(compute_base_highlights(
            &file.buffer_text,
            &language,
            styles,
            cache,
            None,
        ));
    }
}

/// Write each of `session`'s files' hunks into `cache` move-aware, so a later
/// review open serves them without re-diffing.
///
/// Locks the cache once per file rather than for the whole session, and checks
/// `cancel` between files, so the background warm ([`crate::diff_warm`]) can be
/// superseded mid-write without blocking a real scan or leaving the lock held.
pub(crate) fn populate_diff_cache_from(
    cache: &Mutex<DiffCache>,
    doc: &DiffDocument,
    cancel: &AtomicBool,
) {
    for file in &doc.files {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let hunks: Vec<ReviewHunk> = file
            .chunks
            .iter()
            .filter_map(|id| doc.chunks.get(id).map(|c| c.hunk.clone()))
            .collect();
        let key = diff_cache_key(&file.base_text, &file.buffer_text, file.language.as_ref());
        cache
            .lock()
            .expect("diff_cache poisoned")
            .insert(key, Arc::new(hunks), true);
    }
}

#[cfg(test)]
mod tests {
    use crate::{badge::BadgeSource, test_harness::TestHarness, workspace::diff::DiffBase};
    use std::path::{Path, PathBuf};

    /// The text of the buffer open at `path`, which lets a test read a proposal
    /// that landed in a file the pane no longer shows.
    fn buffer_text_at(h: &TestHarness, path: &str) -> Option<String> {
        let ws = h.stoat.active_workspace();
        let id = ws.buffers.id_for_path(Path::new(path))?;
        let shared = ws.buffers.get(id)?;
        let text = shared.read().expect("buffer poisoned").rope().to_string();
        Some(text)
    }

    /// The base texts a [`DiffBase::Memory`] override carries, by path.
    fn memory_base(h: &TestHarness) -> Vec<(String, String)> {
        let Some(DiffBase::Memory { files }) = h.stoat.active_workspace().diff_base() else {
            return Vec::new();
        };
        let mut out: Vec<(String, String)> = files
            .iter()
            .map(|(path, text)| (path.display().to_string(), text.to_string()))
            .collect();
        out.sort();
        out
    }

    /// A proposal lands as an unsaved edit on the real file's buffer rather
    /// than in a screen of its own, so the editor's own verbs accept and reject
    /// it.
    #[test]
    fn agent_edits_land_as_unsaved_buffer_edits() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_fs().insert_file("/work/a.rs", b"old\n");
        h.fake_fs().insert_file("/work/b.rs", b"stale\n");

        h.open_agent_edit_review(&[("a.rs", "old\n", "new\n"), ("b.rs", "stale\n", "fresh\n")]);

        assert_eq!(
            (
                buffer_text_at(&h, "/work/a.rs"),
                buffer_text_at(&h, "/work/b.rs")
            ),
            (Some("new\n".to_string()), Some("fresh\n".to_string())),
            "both proposals are sitting in their files, unsaved",
        );
    }

    /// The base travels with the proposals, since no revision holds what the
    /// agent read. It is keyed by absolute path, which is what the diff reads
    /// back.
    #[test]
    fn agent_edits_install_their_own_base() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_fs().insert_file("/work/a.rs", b"old\n");

        h.open_agent_edit_review(&[("a.rs", "old\n", "new\n")]);

        assert_eq!(
            memory_base(&h),
            vec![("/work/a.rs".to_string(), "old\n".to_string())],
            "the agent's own base is what the proposal diffs against",
        );
    }

    /// The landing matches the commit openers. The first changed file opens
    /// with the latch armed, and a badge names the two verbs.
    #[test]
    fn agent_edits_land_on_the_first_file_with_the_latch_armed() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_fs().insert_file("/work/a.rs", b"old\n");
        h.fake_fs().insert_file("/work/b.rs", b"stale\n");

        h.open_agent_edit_review(&[("a.rs", "old\n", "new\n"), ("b.rs", "stale\n", "fresh\n")]);

        let panes = &h.stoat.active_workspace().panes;
        assert_eq!(
            (
                crate::test_harness::editor::focused_buffer_path(&h.stoat),
                panes.pane(panes.focus()).diff_mode,
                h.stoat.pending_message.as_deref(),
            ),
            (
                PathBuf::from("/work/a.rs"),
                true,
                Some("agent edits: save accepts, :reload rejects"),
            ),
            "the first proposal is on screen, latched, with the verbs named",
        );
    }

    /// Each proposal is one edit, so one undo backs one file out. That is the
    /// fine-grained reject, and it costs no verb of its own.
    #[test]
    fn one_undo_rejects_one_file() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_fs().insert_file("/work/a.rs", b"old\n");

        h.open_agent_edit_review(&[("a.rs", "old\n", "new\n")]);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Undo);

        assert_eq!(
            buffer_text_at(&h, "/work/a.rs"),
            Some("old\n".to_string()),
            "the file is back to what the agent read",
        );
    }

    /// A proposal for a file that does not exist yet opens empty and reads as a
    /// whole-file addition, which is what its empty base says it is.
    #[test]
    fn a_proposed_new_file_opens_against_an_empty_base() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();

        h.open_agent_edit_review(&[("new.rs", "", "added\n")]);

        assert_eq!(
            (buffer_text_at(&h, "/work/new.rs"), memory_base(&h)),
            (
                Some("added\n".to_string()),
                vec![("/work/new.rs".to_string(), String::new())]
            ),
            "the proposal is in a buffer with nothing behind it",
        );
    }

    /// The 1-based new-side line and 0-based buffer row of the review
    /// editor's text cursor.
    fn review_cursor_row(h: &mut TestHarness) -> u32 {
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let bs = snapshot.buffer_snapshot();
        let head = editor.selections.newest_anchor().head();
        bs.rope().offset_to_point(bs.resolve_anchor(&head)).row
    }

    /// A repo where `b.rs` matches HEAD and differs from commit `base0`, so it
    /// is changed against the revision and against nothing else.
    fn diff_rev_harness() -> (TestHarness, PathBuf) {
        let mut h = TestHarness::with_size(80, 20);
        let workdir = PathBuf::from("/repo");
        h.stoat.active_workspace_mut().git_root = workdir.clone();
        {
            let mut builder = h.fake_git().add_repo(&workdir).with_fs(h.fake_fs());
            builder.head_file("b.rs", "fn b() {}\nfn b2() {}\n");
            builder.commit("base0", &[("b.rs", "fn b() {}\n")]);
        }
        h.fake_fs()
            .insert_file(workdir.join("b.rs"), b"fn b() {}\nfn b2() {}\n");
        h.stoat.set_diff_warm_auto(true);
        (h, workdir)
    }

    /// What the three pieces of `:diff <rev>` state currently read: the base
    /// every buffer diffs against, whether the pane is latched into the diff
    /// layout, and which file is open.
    fn diff_state(h: &TestHarness) -> (Option<String>, bool, Option<PathBuf>) {
        let base = match h.stoat.active_workspace().diff_base() {
            Some(DiffBase::Rev { sha }) => sha.clone(),
            _ => None,
        };
        let panes = &h.stoat.active_workspace().panes;
        let latched = panes.pane(panes.focus()).diff_mode;
        let path = h
            .stoat
            .active_workspace()
            .buffers
            .path_for(h.stoat.focused_editor_ids().expect("editor").1)
            .map(Path::to_path_buf);
        (base, latched, path)
    }

    /// A revision points the whole workspace at that commit and opens what
    /// changed since it. Against HEAD `b.rs` is untouched, so nothing but the
    /// revision could have brought the reader here.
    #[test]
    fn diff_with_a_rev_installs_the_base_and_opens_the_changed_file() {
        let (mut h, workdir) = diff_rev_harness();

        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::Diff {
                rev: Some("base0".to_string()),
            },
        );
        h.settle();

        assert_eq!(
            diff_state(&h),
            (Some("base0".to_string()), true, Some(workdir.join("b.rs")),),
        );
    }

    /// The bare command closes the diff, and a base a revision installed goes
    /// with it. Leaving it behind would have every later diff silently measured
    /// against a commit the reader thought they had closed.
    #[test]
    fn a_bare_diff_clears_the_base_a_rev_installed() {
        let (mut h, _) = diff_rev_harness();
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::Diff {
                rev: Some("base0".to_string()),
            },
        );
        h.settle();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Diff { rev: None });
        h.settle();

        let (base, latched, _) = diff_state(&h);
        assert_eq!((base, latched), (None, false));
    }

    /// Naming a revision asks to look at it, so running it again re-targets the
    /// view rather than closing it. Only the bare command closes, which is what
    /// keeps `:diff <rev>` from being a toggle whose meaning depends on what
    /// the reader last did.
    #[test]
    fn a_repeated_diff_rev_keeps_the_view_open() {
        let (mut h, workdir) = diff_rev_harness();
        let open = |h: &mut TestHarness| {
            crate::action_handlers::dispatch(
                &mut h.stoat,
                &stoat_action::Diff {
                    rev: Some("base0".to_string()),
                },
            );
            h.settle();
        };

        open(&mut h);
        open(&mut h);

        assert_eq!(
            diff_state(&h),
            (Some("base0".to_string()), true, Some(workdir.join("b.rs")),),
            "the second run lands on the same view rather than closing it",
        );
    }

    /// The base holds the resolved commit, not the text the reader typed. A
    /// revspec is a name for a commit at one moment: `main` moves, a prefix is
    /// not a sha, and either would leave the base naming something that stops
    /// meaning the commit it was chosen for.
    #[test]
    fn diff_stores_the_resolved_sha_rather_than_the_revspec() {
        let (mut h, _) = diff_rev_harness();

        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::Diff {
                rev: Some("base".to_string()),
            },
        );
        h.settle();

        let (base, _, _) = diff_state(&h);
        assert_eq!(
            base.as_deref(),
            Some("base0"),
            "the prefix resolved to the commit it names",
        );
    }

    /// A revision the repo cannot resolve changes nothing. Half-applying it
    /// would leave the reader in a diff against a base they never named.
    #[test]
    fn an_unknown_rev_badges_and_changes_nothing() {
        let (mut h, _) = diff_rev_harness();
        let before = diff_state(&h);

        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::Diff {
                rev: Some("nosuch".to_string()),
            },
        );
        h.settle();

        assert_eq!(diff_state(&h), before, "no state moved");
        let badge = h
            .stoat
            .active_workspace()
            .badges
            .find_by_source(BadgeSource::Review)
            .and_then(|id| h.stoat.active_workspace().badges.get(id))
            .map(|badge| badge.label.clone());
        assert_eq!(
            badge.as_deref(),
            Some("unknown revision"),
            "the refusal is reported rather than silent",
        );
    }

    #[test]
    fn diff_recomputes_a_map_staled_by_an_external_git_mutation() {
        let mut h = TestHarness::with_size(80, 14);
        // a.txt matches HEAD, so its first diff map holds no hunks at all --
        // exactly the state a buffer is left in when it was clean before an
        // external rebase moved HEAD out from under it.
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_git()
            .add_repo("/repo")
            .with_fs(h.fake_fs())
            .head_file("a.txt", "a\nb\nc\n");
        h.fake_fs()
            .insert_file(PathBuf::from("/repo/a.txt"), b"a\nb\nc\n");
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.txt"));
        h.settle_diff_jobs();

        h.fake_git()
            .add_repo("/repo")
            .head_file("a.txt", "a\nOLD\nc\n");
        h.stoat.active_workspace_mut().invalidate_all_diffs();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Diff { rev: None });

        assert_eq!(
            review_cursor_row(&mut h),
            1,
            "the stale empty map is recomputed and the cursor lands on the new first hunk",
        );
    }

    fn open_git_file_at_cursor(h: &mut TestHarness, row: u32) -> PathBuf {
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\n", "a\nb\nX\nd\n")]);
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, row);
        workdir
    }

    /// Open a file with two modified lines four unchanged lines apart, cursor
    /// on `row`. The gap is under the old chunk extraction's 6-row merge
    /// window, so this is exactly the shape that used to stage both.
    fn open_two_hunk_file_at(h: &mut TestHarness, row: u32) -> PathBuf {
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(
            &workdir,
            &[(
                "a.rs",
                "a\nb\nc\nd\ne\nf\ng\nh\n",
                "a\nZ\nc\nd\ne\nf\nY\nh\n",
            )],
        );
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, row);
        workdir
    }

    /// The staged unit is the hunk the gutter draws. Two hunks close enough to
    /// share a context window still stage one at a time.
    #[test]
    fn staging_one_hunk_leaves_a_nearby_hunk_alone() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_two_hunk_file_at(&mut h, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageHunk);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
        let patch = &patches[0];
        assert!(
            patch.contains("-b\n"),
            "removes the first base line: {patch}"
        );
        assert!(
            patch.contains("+Z\n"),
            "adds the first buffer line: {patch}"
        );
        assert!(
            !patch.contains("-g\n") && !patch.contains("+Y\n"),
            "and the second hunk's change is absent: {patch}"
        );
        // Context stops before the neighbor, so the patch never restates the
        // second hunk's row as unchanged base text and silently reverts it.
        assert!(
            patch.contains("@@ -1,5 +1,5 @@"),
            "and the context stops short of the second hunk: {patch}"
        );
    }

    /// Hunks closer together than the context width. Context has to stop at the
    /// neighbor rather than restating its changed row as unchanged base text,
    /// which would silently revert the second change on apply.
    #[test]
    fn context_stops_at_a_hunk_closer_than_the_context_width() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\ne\n", "a\nZ\nc\nd\nY\n")]);
        h.open_file(&workdir.join("a.rs"));
        {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            crate::action_handlers::movement::set_cursor_row(editor, 1);
        }

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageHunk);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
        let patch = &patches[0];
        assert!(
            !patch.contains("\n e\n"),
            "the neighbor's base line is not carried as context: {patch}"
        );
        assert!(
            patch.contains("@@ -1,4 +1,4 @@"),
            "the patch stops one row short of the neighbor: {patch}"
        );
    }
    /// The other hunk of the same pair stages on its own too, so the split is
    /// the gutter's and not an artifact of which one comes first.
    #[test]
    fn staging_the_second_hunk_leaves_the_first_alone() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_two_hunk_file_at(&mut h, 6);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageHunk);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
        let patch = &patches[0];
        assert!(
            patch.contains("-g\n"),
            "removes the second base line: {patch}"
        );
        assert!(
            patch.contains("+Y\n"),
            "adds the second buffer line: {patch}"
        );
        assert!(
            !patch.contains("-b\n") && !patch.contains("+Z\n"),
            "and the first hunk's change is absent: {patch}"
        );
    }

    /// A cursor on a plain context row stages nothing. The gutter is empty
    /// there, so reaching for the nearest hunk would stage a unit the user
    /// cannot see under the cursor.
    #[test]
    fn a_cursor_off_every_hunk_stages_nothing() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_two_hunk_file_at(&mut h, 3);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageHunk);

        assert_eq!(
            h.fake_git().applied_patches(&workdir),
            Vec::<String>::new(),
            "nothing was staged"
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("no hunk under the cursor"),
            "and the message says why"
        );
    }

    /// A pure addition has no base lines of its own, so its base anchor comes
    /// from the lines before it. The header has to name that line, or the
    /// index apply places the hunk somewhere else.
    #[test]
    fn a_mid_file_addition_anchors_its_header_at_the_derived_base_line() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\n", "a\nb\nNEW\nc\nd\n")]);
        h.open_file(&workdir.join("a.rs"));
        {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            crate::action_handlers::movement::set_cursor_row(editor, 2);
        }

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageHunk);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
        let patch = &patches[0];
        assert!(patch.contains("+NEW\n"), "adds the new line: {patch}");
        assert!(
            patch.contains("@@ -1,4 +1,5 @@"),
            "the header counts from the file start with no base line consumed: {patch}"
        );
    }

    /// A deletion's rows are zero-width in the buffer, so the gutter marks it
    /// at its anchor row and the patch body carries only the removed lines.
    #[test]
    fn a_deletion_stages_from_its_anchor_row() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nGONE\nc\nd\n", "a\nb\nc\nd\n")]);
        h.open_file(&workdir.join("a.rs"));
        {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            crate::action_handlers::movement::set_cursor_row(editor, 2);
        }

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageHunk);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
        let patch = &patches[0];
        assert!(patch.contains("-GONE\n"), "removes the base line: {patch}");
        assert!(
            !patch
                .lines()
                .any(|line| line.starts_with('+') && !line.starts_with("+++")),
            "and adds nothing: {patch}"
        );
    }
    #[test]
    fn stage_hunk_applies_the_forward_patch_for_the_cursor_hunk() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_git_file_at_cursor(&mut h, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageHunk);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
        let patch = &patches[0];
        assert!(
            patch.contains("--- a/a.rs\n+++ b/a.rs\n"),
            "targets a.rs: {patch}"
        );
        assert!(patch.contains("-c\n"), "removes the base line: {patch}");
        assert!(patch.contains("+X\n"), "adds the buffer line: {patch}");
    }

    #[test]
    fn unstage_hunk_applies_the_reverse_patch() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_git_file_at_cursor(&mut h, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageHunk);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
        let patch = &patches[0];
        assert!(
            patch.contains("-X\n"),
            "reverse removes the buffer line: {patch}"
        );
        assert!(
            patch.contains("+c\n"),
            "reverse restores the base line: {patch}"
        );
    }

    #[test]
    fn toggle_stage_hunk_stages_when_the_forward_patch_applies() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_git_file_at_cursor(&mut h, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(
            patches.len(),
            1,
            "toggle stages via the forward patch: {patches:?}"
        );
        assert!(patches[0].contains("-c\n") && patches[0].contains("+X\n"));
    }

    #[test]
    fn stage_hunk_off_a_hunk_is_a_message_only_noop() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_git_file_at_cursor(&mut h, 0);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageHunk);

        assert!(
            h.fake_git().applied_patches(&workdir).is_empty(),
            "cursor off a hunk applies nothing"
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("no hunk under the cursor")
        );
    }

    #[test]
    fn stage_line_stages_only_the_cursor_line() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\n", "a\nB\nC\nd\n")]);
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageLine);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "one patch: {patches:?}");
        let patch = &patches[0];
        assert!(
            patch.contains("-b\n") && patch.contains("+B\n"),
            "stages the cursor line: {patch}"
        );
        assert!(
            !patch.contains("+C\n"),
            "leaves the other line unstaged: {patch}"
        );
        assert!(
            patch.contains(" c\n"),
            "the other line stays as context: {patch}"
        );
    }

    /// Staging moves the index, which is one of the two sides the gutter reads,
    /// so the map computed before it no longer describes what is on screen.
    #[test]
    fn stage_hunk_stales_the_diff_map() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\n", "a\nB\nC\nd\n")]);
        h.open_file(&workdir.join("a.rs"));
        h.seed_current_diff_map("a\nb\nc\nd\n");
        let buffer_id = h.stoat.focused_editor_ids().expect("editor").1;
        {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            crate::action_handlers::movement::set_cursor_row(editor, 1);
        }
        assert!(
            h.stoat.active_workspace().diff_map_current(buffer_id),
            "the map starts current, so staling it is the stage's doing",
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageHunk);

        assert!(
            !h.stoat.active_workspace().diff_map_current(buffer_id),
            "the index moved, so the map is owed a recompute",
        );
    }

    /// The line funnel writes the index too, and it is a second call site, so
    /// it stales the map on its own.
    #[test]
    fn stage_line_stales_the_diff_map() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\n", "a\nB\nC\nd\n")]);
        h.open_file(&workdir.join("a.rs"));
        h.seed_current_diff_map("a\nb\nc\nd\n");
        let buffer_id = h.stoat.focused_editor_ids().expect("editor").1;
        {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            crate::action_handlers::movement::set_cursor_row(editor, 1);
        }
        assert!(
            h.stoat.active_workspace().diff_map_current(buffer_id),
            "the map starts current, so staling it is the stage's doing",
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageLine);

        assert!(
            !h.stoat.active_workspace().diff_map_current(buffer_id),
            "one staged line moves the index the same way a hunk does",
        );
    }

    /// A review base makes the checked-out commit the staged side, so the line
    /// keys reach it through the amend transport. The index is not on screen
    /// there, and an index write changes a thing the user never sees.
    #[test]
    fn stage_line_under_a_commit_base_amends_rather_than_writing_the_index() {
        let mut h = reviewing_a_commit("a\nP\nQ\nd\n");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageLine);

        assert_eq!(
            (
                h.fake_git().applied_patches(&PathBuf::from("/work")),
                h.stoat.pending_message.as_deref()
            ),
            (vec![], Some("amended line into the commit")),
            "the line went into the commit and the index was left alone",
        );
    }

    /// Unstaging takes the same route. Both keys read the base before they read
    /// the cursor, so neither reaches the index while a commit is the staged
    /// side.
    #[test]
    fn unstage_line_under_a_commit_base_amends_rather_than_writing_the_index() {
        let mut h = reviewing_a_commit("a\nX\nY\nd\n");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageLine);

        assert_eq!(
            (
                h.fake_git().applied_patches(&PathBuf::from("/work")),
                h.stoat.pending_message.as_deref()
            ),
            (vec![], Some("amended line out of the commit")),
            "the line came out of the commit and the index was left alone",
        );
    }

    /// A rebase stopped on an edit over commit `c1`, with `c0` as the review
    /// base, and the cursor on row 1 of `a.rs`. `working` is what the buffer
    /// holds, which is what separates a worktree edit from the commit's own
    /// change.
    fn reviewing_a_commit(working: &str) -> TestHarness {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stoat.active_workspace_mut().git_root = workdir.clone();
        h.fake_git()
            .add_repo(&workdir)
            .commit("c0", &[("a.rs", "a\nb\nc\nd\n")])
            .commit_with_parent("c1", "c0", &[("a.rs", "a\nX\nY\nd\n")])
            .head_file("a.rs", working);
        h.fake_fs().insert_file("/work/a.rs", working.as_bytes());
        {
            let ws = h.stoat.active_workspace_mut();
            ws.set_diff_base(Some(DiffBase::Rev {
                sha: Some("c0".to_string()),
            }));
            ws.rebase_active = Some(paused_rebase(&workdir));
        }
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, 1);
        h
    }

    /// A base with nothing safe to rewrite under it gets the transport's own
    /// refusal rather than the line-granularity one, since the commit is out of
    /// reach for `s` and `u` as well.
    #[test]
    fn stage_line_reports_the_transport_refusal_below_the_tip() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\n", "a\nB\nC\nd\n")]);
        h.fake_git().add_repo(&workdir).commit("c1", &[]);
        h.stoat
            .active_workspace_mut()
            .set_diff_base(Some(DiffBase::Rev {
                sha: Some("c1".to_string()),
            }));
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageLine);

        assert_eq!(
            (
                h.fake_git().applied_patches(&workdir),
                h.stoat.pending_message.as_deref()
            ),
            (vec![], Some(crate::action_handlers::amend::REFUSED_BADGE)),
            "no walk and no edit stop, so the transport is closed to both key pairs",
        );
    }

    /// A rebase stopped on an edit, which is one of the two positions that make
    /// rewriting the checked-out commit safe.
    fn paused_rebase(workdir: &Path) -> crate::rebase::ActiveRebase {
        crate::rebase::ActiveRebase {
            workdir: workdir.to_path_buf(),
            onto: "c1".to_string(),
            remaining: std::collections::VecDeque::new(),
            current_head: "c1".to_string(),
            last_pick_sha: Some("c1".to_string()),
            last_message: None,
            pause: Some(crate::rebase::RebasePause::Edit {
                cherry_picked_commit: "c1".to_string(),
            }),
        }
    }

    /// The line stage resolves its hunk from bare extents now, so a second hunk
    /// close enough that the old chunk extraction would have merged the two is
    /// absent from the patch rather than riding along as context.
    #[test]
    fn stage_line_ignores_a_hunk_the_old_chunking_would_have_merged() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(
            &workdir,
            &[(
                "a.rs",
                "a\nb\nc\nd\ne\nf\ng\nh\n",
                "a\nZ\nc\nd\ne\nf\nY\nh\n",
            )],
        );
        h.open_file(&workdir.join("a.rs"));
        {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            crate::action_handlers::movement::set_cursor_row(editor, 1);
        }

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageLine);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "one patch: {patches:?}");
        let patch = &patches[0];
        assert!(
            patch.contains("-b\n") && patch.contains("+Z\n"),
            "stages the cursor line: {patch}"
        );
        // Context that ran into the far hunk would carry its buffer text as an
        // unchanged line, telling git the index already holds the change.
        assert!(
            !patch.contains("\n Y\n"),
            "the far hunk's text is not carried as context: {patch}"
        );
        assert!(
            patch.contains("@@ -1,5 +1,5 @@"),
            "and the patch stops well short of it: {patch}"
        );
    }
    #[test]
    fn unstage_line_reverts_the_staged_line_to_head() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_index_scenario(&workdir, &[("a.rs", "a\nb\nc\n", "a\nB\nc\n", "a\nB\nc\n")]);
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageLine);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "one patch: {patches:?}");
        let patch = &patches[0];
        assert!(patch.contains("-B\n"), "reverts the staged line: {patch}");
        assert!(patch.contains("+b\n"), "restores the HEAD line: {patch}");
    }

    #[test]
    fn toggle_stage_line_stages_an_unstaged_line() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\n", "a\nB\nc\n")]);
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(
            patches.len(),
            1,
            "toggle stages the unstaged line: {patches:?}"
        );
        assert!(patches[0].contains("-b\n") && patches[0].contains("+B\n"));
    }

    #[test]
    fn toggle_stage_line_unstages_a_staged_line() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_index_scenario(&workdir, &[("a.rs", "a\nb\nc\n", "a\nB\nc\n", "a\nB\nc\n")]);
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(
            patches.len(),
            1,
            "toggle unstages the staged line: {patches:?}"
        );
        assert!(patches[0].contains("-B\n") && patches[0].contains("+b\n"));
    }

    #[test]
    fn stage_line_composes_against_the_updated_index() {
        // HEAD a/b/c/d; the index already stages line 1 (b -> B), and the
        // buffer additionally changes line 2 (c -> C). Staging line 2 diffs the
        // index (not HEAD), so the patch touches only line 2.
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_index_scenario(
            &workdir,
            &[("a.rs", "a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nC\nd\n")],
        );
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageLine);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "one patch: {patches:?}");
        let patch = &patches[0];
        assert!(
            patch.contains("-c\n") && patch.contains("+C\n"),
            "stages line 2 against the index: {patch}"
        );
        assert!(
            !patch.contains("+B\n") && !patch.contains("-b\n"),
            "line 1 is already staged in the index and stays context, not re-staged: {patch}"
        );
    }

    #[test]
    fn unstage_line_maps_the_cursor_past_an_unstaged_insertion() {
        // HEAD a/b; the index stages line 1 (b -> B); the buffer inserts NEW
        // above unstaged, so the staged line sits at buffer row 2 but index row
        // 1. Unstaging must map back and revert only the index line.
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_index_scenario(&workdir, &[("a.rs", "a\nb\n", "a\nB\n", "NEW\na\nB\n")]);
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnstageLine);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "one patch: {patches:?}");
        let patch = &patches[0];
        assert!(patch.contains("-B\n"), "reverts the staged line: {patch}");
        assert!(patch.contains("+b\n"), "restores the HEAD line: {patch}");
        assert!(
            !patch.contains("NEW"),
            "the unstaged insertion is untouched: {patch}"
        );
    }

    #[test]
    fn stage_line_off_a_change_is_a_message_only_noop() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_git_file_at_cursor(&mut h, 0);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::StageLine);

        assert!(
            h.fake_git().applied_patches(&workdir).is_empty(),
            "cursor off a change applies nothing"
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("no line change under the cursor")
        );
    }

    #[test]
    fn space_capital_g_s_stages_the_hunk_from_a_plain_editor() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_git_file_at_cursor(&mut h, 2);

        h.type_keys("space G s");

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "space G s stages the hunk: {patches:?}");
        assert!(
            patches[0].contains("-c\n") && patches[0].contains("+X\n"),
            "stages the cursor hunk: {}",
            patches[0]
        );
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "the one-shot git mode returns to normal after acting"
        );
    }
}

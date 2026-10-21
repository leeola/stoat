use super::{
    amend::{self, AmendRoute, AmendUnit},
    movement::ChangeDir,
};
use crate::{
    app::{Stoat, UpdateEffect},
    buffer::BufferId,
    buffer_registry::OpenOrigin,
    diff_cache::{DiffCache, DiffCacheKey},
    diff_map::{hunk_base_lines, line_starts},
    display_map::{syntax_theme::SyntaxStyles, DisplaySnapshot},
    editor_state::EditorId,
    git_jobs::{self, GitJob, GitLanding, GitWork},
    host::GitRepo,
    jumplist::JumpEntry,
    multi_buffer::MultiBufferSnapshot,
    pane::{FocusTarget, PaneId, View},
    review::{ReviewFileInput, ReviewHunk},
    review_apply::{
        base_line_ranges, hunk_rows, hunk_to_patch, line_restricted_rows, rows_to_unified_diff,
        HUNK_CONTEXT,
    },
    review_session::DiffDocument,
    workspace::{
        diff::{
            self, compute_base_highlights, BaseHighlightCache, DiffBase, DiffLanding, PairBase,
            RevOrigin, WorktreeBase,
        },
        Workspace,
    },
};
use std::{
    ops::Range,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use stoat_language::{Language, LanguageRegistry};
use stoat_text::SelectionGoal;
use tracing::field;

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
    let Some((workdir, commit)) = stoat
        .active_workspace()
        .focused_commits()
        .and_then(|state| {
            let commit = state.commits.get(state.selected)?;
            Some((state.workdir.clone(), commit.clone()))
        })
    else {
        return UpdateEffect::None;
    };
    super::review_walk::walk_one_commit(stoat, workdir, commit)
}

/// Land an agent's proposed edits as unsaved buffer edits over their own base.
///
/// Each proposal replaces its file's buffer content, and the base texts become
/// a [`DiffBase::Memory`] override, so `:diff-files` shows the proposal against
/// what the agent read. The first edited file opens with the latch armed.
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
        let Some(buffer_id) = super::file::open_file(stoat, &path, OpenOrigin::Named) else {
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
    super::file::open_file(stoat, &first, OpenOrigin::Named);
    reopen_diff_view(stoat);
    stoat.set_status("agent edits: save accepts, :reload rejects");
}

/// Open the diff against `rev`, or toggle the working-tree diff when `None`.
///
/// A revision points the whole workspace at that commit, so every buffer diffs
/// against it and the change list spans everything committed since. Opening the
/// view from a buffer with no hunks of its own then crosses into the first file
/// in that list, which is what [`toggle_diff_view`] already does and what makes
/// a revision land somewhere worth reading: with a base further back, the file
/// the reader wants is rarely the one on screen.
///
/// The focused file's pair gives way to the revision, and a pair over any other
/// file holds.
///
/// A revision the repository cannot resolve changes nothing at all: it badges
/// and leaves the view, the latch, and the base where they were. A command that
/// half-applies leaves the reader worse off than one that refuses.
///
/// The words `index` and `HEAD`, in any letter case, name the working tree's
/// own two bases, as [`diff_against`] picks them. HEAD as a word keeps the
/// index marking what is staged, which a HEAD resolved to a sha does not. A
/// branch that carries either name stays reachable as `refs/heads/<name>`.
pub(super) fn diff(stoat: &mut Stoat, rev: Option<&str>) -> UpdateEffect {
    let Some(rev) = rev else {
        // The bare command also drops any base a revision left installed, so
        // closing the diff always returns to the working tree's own base.
        toggle_diff_view(stoat);
        return UpdateEffect::Redraw;
    };
    if rev.eq_ignore_ascii_case("index") {
        return diff_against(stoat, WorktreeBase::Index);
    }
    if rev.eq_ignore_ascii_case("head") {
        return diff_against(stoat, WorktreeBase::Head);
    }

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

    drop_focused_pair(stoat);
    tracing::info!(target: "stoat::review", rev, sha = %sha, "diff base set");
    stoat
        .active_workspace_mut()
        .set_diff_base(Some(DiffBase::named(rev, sha)));
    // Turned on rather than toggled. Naming a revision asks to look at it, so a
    // second `:diff-files <rev>` re-targets the view rather than closing it;
    // only the bare command closes.
    reopen_diff_view(stoat);
    UpdateEffect::Redraw
}

/// Pick `base` as the working tree's own base and show the diff view over it.
///
/// The pick holds after the view closes, so the gutter of a plain pane keeps
/// marking against it. A revision, a review, a proposal base, or the focused
/// file's pair gives way to the pick, but the checkout a review made stays
/// where it is.
///
/// The base already installed with the view open costs nothing, not even a
/// re-diff. With the view closed, the view opens over that base.
pub(super) fn diff_against(stoat: &mut Stoat, base: WorktreeBase) -> UpdateEffect {
    let paired = drop_focused_pair(stoat);
    let ws = stoat.active_workspace();
    let same = ws.worktree_base() == base
        && matches!(
            (ws.diff_base(), base),
            (None, WorktreeBase::Index) | (Some(DiffBase::Head), WorktreeBase::Head)
        );
    let view_on = super::focused_editor_mut(stoat).is_some_and(|editor| editor.diff_view);
    if same && view_on && !paired {
        return UpdateEffect::Redraw;
    }

    if !same {
        stoat.active_workspace_mut().set_worktree_base(base);
    }
    // Turned on rather than toggled, as a named revision is.
    reopen_diff_view(stoat);
    UpdateEffect::Redraw
}

/// Flip the working tree's own base between the index and HEAD, and name both
/// sides of the diff in the status line.
///
/// A revision, a review, a proposal, or the focused file's pair gives way to
/// the pick on the first press, with no flip, so one press always lands back
/// on the working tree. The next press flips.
///
/// The diff view stays open or closed, and the cursor stays where it is.
pub(super) fn toggle_diff_base(stoat: &mut Stoat) -> UpdateEffect {
    let paired = drop_focused_pair(stoat);
    let ws = stoat.active_workspace();
    let displaced = paired
        || matches!(
            ws.diff_base(),
            Some(DiffBase::Rev { .. } | DiffBase::Memory { .. } | DiffBase::Pair { .. })
        );
    let next = match (displaced, ws.worktree_base()) {
        (true, pick) => pick,
        (false, WorktreeBase::Index) => WorktreeBase::Head,
        (false, WorktreeBase::Head) => WorktreeBase::Index,
    };
    stoat.active_workspace_mut().set_worktree_base(next);

    let (from, to) = diff::diff_sides(stoat.active_workspace().diff_base());
    stoat.set_status(format!("diff: {from} → {to}"));
    UpdateEffect::Redraw
}

/// Open the command palette holding `diff-files` and the current base, for the
/// reader to retype.
///
/// Enter runs the ordinary `:diff-files <rev>`, so a revision, `index`, or
/// `HEAD` points the diff at it, and Escape changes nothing. A revision the
/// reader named seeds as typed, and a review seeds its parent's short sha. A
/// base with no revision name to retype, the empty parent of a root commit, an
/// agent proposal, or the focused file's pair, seeds a bare `diff-files `.
pub(super) fn edit_diff_base(stoat: &mut Stoat) -> UpdateEffect {
    let seed = match stoat.active_workspace().diff_base() {
        _ if focused_file_pair(stoat).is_some() => "diff-files ".to_string(),
        Some(DiffBase::Rev {
            origin: RevOrigin::Named(name),
            ..
        }) => format!("diff-files {name}"),
        Some(DiffBase::Rev { sha: Some(sha), .. }) => {
            format!("diff-files {}", diff::short_sha(sha))
        },
        Some(DiffBase::Rev { sha: None, .. } | DiffBase::Memory { .. } | DiffBase::Pair { .. }) => {
            "diff-files ".to_string()
        },
        base @ (None | Some(DiffBase::Head)) => format!("diff-files {}", diff::diff_sides(base).0),
    };
    super::palette::open_palette_seeded(stoat, &seed)
}

/// Open the diff view over the workspace's file pair, or close it when it
/// already shows that pair, driven by [`stoat_action::DiffPair`].
///
/// The first pane's file is the base column. The other pane shows the diff and
/// takes the focus, so each file stays on the side it was on. The base is the
/// first file's buffer text, unsaved edits included, and the diff drive keeps
/// it in step with that buffer.
///
/// The pair holds for the measured file alone. Every other file keeps the
/// workspace's base, and a pair in another tab stays as it is. A second pair
/// for the same file replaces the first, and closing returns the file to the
/// workspace's base.
///
/// Two equal files open the view with equal columns, and a status says so once
/// the pair's hunks are in. With no pair it badges and changes nothing.
///
/// See also:
/// - [`Workspace::pair_panes`] for what counts as a pair.
pub(super) fn diff_pair(stoat: &mut Stoat) -> UpdateEffect {
    let Some([(_, base_buffer), (target_pane, target_buffer)]) =
        stoat.active_workspace().pair_panes()
    else {
        emit_review_error_badge(stoat, "diff pair needs two files in two panes", None);
        return UpdateEffect::Redraw;
    };

    let (base_path, path, showing) = {
        let ws = stoat.active_workspace();
        let (Some(base_path), Some(path)) = (
            ws.buffers.path_for(base_buffer),
            ws.buffers.path_for(target_buffer),
        ) else {
            return UpdateEffect::None;
        };
        let installed = ws
            .pair_for(path)
            .is_some_and(|pair| pair.base_path == base_path);
        let view_on = match ws.panes.pane(target_pane).view {
            View::Editor(id) => ws.editors.get(id).is_some_and(|editor| editor.diff_view),
            _ => false,
        };
        (
            base_path.to_path_buf(),
            path.to_path_buf(),
            installed && view_on,
        )
    };

    {
        let ws = stoat.active_workspace_mut();
        ws.panes.set_focus(target_pane);
        ws.focus = FocusTarget::SplitPane;
    }

    if showing {
        exit_diff_view(stoat);
        stoat.active_workspace_mut().remove_pair(&path);
        return UpdateEffect::Redraw;
    }

    let Some((base_version, text)) =
        stoat
            .active_workspace()
            .buffers
            .get(base_buffer)
            .map(|shared| {
                let guard = shared.read().expect("buffer poisoned");
                (
                    guard.snapshot.version,
                    Arc::new(guard.snapshot.visible_text.to_string()),
                )
            })
    else {
        return UpdateEffect::None;
    };
    stoat.active_workspace_mut().set_pair(
        path,
        PairBase {
            base_path,
            text,
            base_version,
        },
    );
    reopen_diff_view(stoat);

    if let Some((editor, buffer_id)) = stoat.focused_editor_ids() {
        ensure_diff_map(stoat, buffer_id, Some(DiffLanding::PairVerdict { editor }));
    }
    UpdateEffect::Redraw
}

/// Turn the diff view on, whatever it was showing before.
///
/// [`toggle_diff_view`] closes an open view, so a caller that means "show this"
/// rather than "flip this" has to shut the view before opening it. Every such
/// caller arrives with a base it just installed, and the exit half leaves a
/// base alone by design, so the round trip re-reads the new base rather than
/// dropping it.
pub(super) fn reopen_diff_view(stoat: &mut Stoat) {
    exit_diff_view(stoat);
    toggle_diff_view(stoat);
}

/// Arm the focused pane's diff latch, and show the diff where the reader stands
/// once the file's hunks are in.
///
/// Unlike [`toggle_diff_view`], this never crosses into another file or moves
/// the cursor. A walkthrough stop over a file its commit did not touch must stay
/// on that file, so a buffer with no hunks shows plain under the armed latch,
/// the way a latched pane shows any clean file it navigates to.
///
/// A diff in any other split pane of the tab turns off, so the tab holds one
/// diff.
pub(super) fn latch_diff_view(stoat: &mut Stoat) {
    {
        let ws = stoat.active_workspace_mut();
        let focus = ws.panes.focus();
        drop_other_diff_views(ws, focus);
        ws.panes.pane_mut(focus).diff_mode = true;
        ws.panes.widen(focus);
    }

    let Some((editor, buffer_id)) = stoat.focused_editor_ids() else {
        return;
    };
    ensure_diff_map(stoat, buffer_id, Some(DiffLanding::LatchedView { editor }));
}

/// Turn the diff view on for the focused editor where it stands.
///
/// Sets the editor's flag, latches the focused pane and widens it, and wants
/// the hunks the view reads. The view paints at once, and hunks that are not
/// current land a frame or more later. The cursor stays where it is, and the
/// jump to the first change is what [`toggle_diff_view`] adds on top of this.
/// A caller that places the cursor itself calls this rather than the toggle.
///
/// The widen makes the view a full-window screen. A layout action pressed on it
/// closes the view rather than leaving it narrow beside a split. A diff in any
/// other split pane of the tab turns off, so the tab holds one diff.
///
/// See also:
/// - [`exit_diff_view`], the exit half, which undoes every piece of this.
/// - [`close_full_window_screen`](super::pane::close_full_window_screen), which closes the view on
///   a layout action.
pub(crate) fn enter_diff_view(stoat: &mut Stoat) {
    let focus = stoat.active_workspace().panes.focus();
    drop_other_diff_views(stoat.active_workspace_mut(), focus);

    let Some(editor) = super::focused_editor_mut(stoat) else {
        return;
    };
    editor.set_diff_view(true);

    // Give the diff its own full width. An unwidenable layout stays put and
    // rides the unified fallback.
    let panes = &mut stoat.active_workspace_mut().panes;
    panes.pane_mut(focus).diff_mode = true;
    panes.widen(focus);

    // The reader's own open decides the view, so a latched switch that waits
    // on the same hunks does not turn it off when they land.
    if let Some((editor, buffer_id)) = stoat.focused_editor_ids() {
        stoat
            .active_workspace_mut()
            .drop_latched_view(buffer_id, editor);
        ensure_diff_map(stoat, buffer_id, None);
        tracing::info!(
            target: "stoat::review",
            path = stoat
                .active_workspace()
                .buffers
                .path_for(buffer_id)
                .map(|path| field::display(path.display())),
            "entered diff view"
        );
    }
}

/// Leave the diff view, reporting whether there was one to leave.
///
/// Clears the focused editor's flag, the focused pane's latch, and the widen
/// the view took, which is every piece of state entering it set. Returns false
/// and touches nothing when no editor is focused or neither half is set, so a
/// caller can offer the exit unconditionally.
///
/// While the focused pane covers a commits list beneath the diff, the widen
/// belongs to the list and stays until the list closes.
///
/// Either half being set counts as on, so this leaves a latched pane showing a
/// clean file as readily as a diff itself.
pub(super) fn exit_diff_view(stoat: &mut Stoat) -> bool {
    let latched = {
        let panes = &stoat.active_workspace().panes;
        panes.pane(panes.focus()).diff_mode
    };
    let commits_open = {
        let ws = stoat.active_workspace();
        super::commits::covered_commits(ws, ws.panes.focus()).is_some()
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
    if !commits_open && panes.widened() == Some(focus) {
        panes.unwiden();
    }
    tracing::info!(target: "stoat::review", "left diff view");
    true
}

/// Turn the diff off in every split pane of the tab except `keep`.
///
/// A tab holds at most one diff, because the diff view is a full-window screen.
/// A layout action on a diff closes it, but other paths still put the focus
/// beside a diff. Examples are a click into a layout that refused the widen,
/// and an agent or `stoat <paths>` open.
///
/// The pane's latch and its editor's flag both clear, because
/// [`exit_diff_view`] counts either one as on. A latch left armed also opens
/// the next changed file that the pane shows as a diff.
fn drop_other_diff_views(ws: &mut Workspace, keep: PaneId) {
    for id in ws.panes.split_pane_ids() {
        if id == keep {
            continue;
        }
        let pane = ws.panes.pane_mut(id);
        pane.diff_mode = false;
        if let View::Editor(editor_id) = pane.view
            && let Some(editor) = ws.editors.get_mut(editor_id)
        {
            editor.set_diff_view(false);
        }
    }
}

/// Toggle the live per-file diff view on the focused editor, driven by
/// [`stoat_action::DiffFiles`].
///
/// Flips `diff_view` (and the display map's deleted-block splicing) on the
/// focused editor. Unlike the session review there is no scan, no session, and
/// no scratch buffer -- the editor stays the real, editable file buffer, so
/// re-pressing toggles the two columns off again.
///
/// On a focused changes view it opens the per-file diff at the cursor's hunk,
/// the mirror of [`stoat_action::DiffChanges`] on the diff.
///
/// Opening the view lands the cursor once the file's hunks are in, which takes
/// a frame or more when they are not current, so the loop paints the view
/// first. [`land_toggle_jump`] places the cursor. Toggling the view off leaves
/// the cursor untouched, also when the hunks of the open land after the close.
///
/// See also:
/// - [`enter_diff_view`], the entry half, which turns the view on where the reader stands.
/// - [`exit_diff_view`], the exit half, which other screens reuse.
pub(super) fn toggle_diff_view(stoat: &mut Stoat) {
    if stoat.active_workspace().focused_diff_changes_id().is_some() {
        super::diff_changes::diff_changes_open(stoat);
        return;
    }

    let origin = super::focused_editor_mut(stoat).map(super::jump::live_entry);
    if super::focused_editor_mut(stoat).is_none() {
        return;
    }

    if exit_diff_view(stoat) {
        drop_focused_pair(stoat);
        stoat.active_workspace_mut().drop_diff_base();
        return;
    }

    enter_diff_view(stoat);
    if let Some((editor, buffer_id)) = stoat.focused_editor_ids() {
        ensure_diff_map(
            stoat,
            buffer_id,
            Some(DiffLanding::ToggleJump { editor, origin }),
        );
    }
}

/// Land a diff toggle's cursor on the focused buffer's hunks.
///
/// The cursor goes to the change nearest its row, with a tie going to the
/// change below, and `origin` goes to the jumplist, so the usual jump-back
/// returns. A cursor that already sits on a change stays where it is, with no
/// jump and no scroll. When the focused buffer has no changes of its own, the
/// view instead crosses into the first changed file and lands on its first
/// hunk.
pub(super) fn land_toggle_jump(stoat: &mut Stoat, origin: Option<JumpEntry>) {
    // `Some(true)` when the cursor moved onto a change, `Some(false)` when it
    // already sat on one, and `None` when the buffer has no change at all.
    let landing = super::focused_editor_mut(stoat).and_then(|editor| {
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let cursor_row = {
            let sel = editor.selections.newest_anchor();
            let tail = buffer_snapshot.resolve_anchor(&sel.tail());
            let head = buffer_snapshot.resolve_anchor(&sel.head());
            let cursor = stoat_text::cursor_offset(buffer_snapshot.rope(), tail, head);
            buffer_snapshot.rope().offset_to_point(cursor).row
        };
        // The stops rather than the hunks, so opening over a refined block
        // lands on the stop nearest the cursor instead of the block's top.
        let hunk_stops = display_snapshot
            .diff_map()?
            .live_hunks(buffer_snapshot)
            .hunk_stops();
        let (rows, holds) =
            super::movement::nearest_hunk_rows(buffer_snapshot.rope(), &hunk_stops, cursor_row)?;
        if holds {
            return Some(false);
        }
        let target_offset = super::movement::stop_landing_cell(buffer_snapshot.rope(), rows).start;
        editor.selections.transform(buffer_snapshot, |sel| {
            crate::selection::land_block_cursor(
                sel.id,
                target_offset,
                SelectionGoal::None,
                buffer_snapshot.rope(),
                buffer_snapshot,
            )
        });
        Some(true)
    });
    let jumped = landing == Some(true);

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
    // the first changed file. This makes `:diff-files` from a scratch or
    // unchanged buffer open a real diff instead of silently toggling empty
    // columns.
    if landing.is_none() {
        let _ = super::movement::goto_change(stoat, ChangeDir::Next);
    }
}

/// Make sure `buffer_id`'s diff map holds current hunks, and run `landing`
/// once it does.
///
/// A map current at the buffer's version, with its colors or still without
/// them, answers at once, and the landing runs now. Any other map has its hunks
/// wanted on the blocking pool, where resolving the base and running the line
/// pass take 5-50 ms, and the landing waits for them, so the loop paints the
/// frame meanwhile. A buffer with no file has nothing to wait for, so its
/// landing runs at once too.
///
/// A map left over from before an external git mutation counts as stale. Such
/// a map describes a base that has since moved, so trusting it opens the view
/// on hunks measured against a HEAD that no longer exists.
///
/// Only the hunks are computed for the wait. The left column's colors cost a
/// parse of the whole base file, which the background pass takes on and lands a
/// frame later. A landing that reads a walk's stops waits for the whole map
/// instead, since the tree pass splits a hunk into the runs a walk stops on.
pub(crate) fn ensure_diff_map(
    stoat: &mut Stoat,
    buffer_id: BufferId,
    landing: Option<DiffLanding>,
) {
    let ws = stoat.active_workspace();
    let whole = landing.as_ref().is_some_and(DiffLanding::wants_whole_map);
    if ws.diff_map_current(buffer_id) || (!whole && ws.hunks_installed(buffer_id)) {
        if let Some(landing) = landing {
            land_diff_want(stoat, buffer_id, landing);
        }
        return;
    }

    let executor = stoat.executor.clone();
    let git_host = stoat.git_host.clone();
    let language_registry = stoat.language_registry.clone();
    let syntax_styles = stoat.syntax_styles.clone();
    let base_cache = stoat.base_highlights_cache.clone();
    let redraw_notify = stoat.redraw_notify.clone();
    let unwanted = stoat.active_workspace_mut().want_diff_map(
        &executor,
        &git_host,
        &language_registry,
        &syntax_styles,
        &base_cache,
        &redraw_notify,
        buffer_id,
        landing,
    );
    if let Some(landing) = unwanted {
        land_diff_want(stoat, buffer_id, landing);
    }
}

/// Install every wanted diff map of the active workspace whose hunks landed,
/// run the landings that waited for them, and report whether any landed.
pub(crate) fn pump_diff_wants(stoat: &mut Stoat) -> bool {
    let language_registry = stoat.language_registry.clone();
    let landed = stoat
        .active_workspace_mut()
        .poll_diff_wants(&language_registry);
    let any = !landed.is_empty();
    for (buffer_id, landings) in landed {
        for landing in landings {
            land_diff_want(stoat, buffer_id, landing);
        }
    }
    any
}

/// Run `landing` now that `buffer_id`'s hunks are in.
///
/// A landing acts only where the reader still is, since the reader moves on
/// while the hunks are on the pool. A latched view needs its editor to show
/// `buffer_id` in a latched split pane. A toggle jump or a pair verdict needs
/// its editor focused with the diff view still on, so a view closed before the
/// hunks land keeps the cursor and the open file. A change press or a file hop
/// needs the focused buffer to be `buffer_id`. Otherwise the landing is
/// dropped.
fn land_diff_want(stoat: &mut Stoat, buffer_id: BufferId, landing: DiffLanding) {
    let focused = stoat.focused_editor_ids();
    match landing {
        DiffLanding::LatchedView { editor } => {
            let ws = stoat.active_workspace();
            let shows = ws
                .editors
                .get(editor)
                .is_some_and(|state| state.buffer_id == buffer_id);
            let latched = ws.panes.split_panes().any(|(_, pane)| {
                pane.diff_mode && matches!(pane.view, View::Editor(id) if id == editor)
            });
            if !shows || !latched {
                return;
            }
            let has_changes = editor_shows_changes(stoat, editor);
            if let Some(state) = stoat.active_workspace_mut().editors.get_mut(editor) {
                state.set_diff_view(has_changes);
            }
        },
        DiffLanding::ToggleJump { editor, origin } => {
            if focused_diff_view(stoat, editor) {
                land_toggle_jump(stoat, origin);
            }
        },
        DiffLanding::PairVerdict { editor } => {
            if focused_diff_view(stoat, editor) && !editor_shows_changes(stoat, editor) {
                stoat.set_status("the two files do not differ");
            }
        },
        DiffLanding::ChangeWalk { dir, count } => {
            if focused.is_some_and(|(_, id)| id == buffer_id) {
                super::movement::goto_change_impl(stoat, dir, count);
            }
        },
        DiffLanding::EdgeChange { last } => {
            if focused.is_some_and(|(_, id)| id == buffer_id) {
                super::movement::goto_edge_change(stoat, last);
            }
        },
        DiffLanding::FileHop {
            dir,
            origin,
            wrapped,
            fallback,
        } => {
            if focused.is_some_and(|(_, id)| id == buffer_id) {
                super::movement::land_file_hop(stoat, dir, origin, wrapped, fallback);
            }
        },
    }
}

/// Whether `editor` holds the focus and still shows the diff view.
fn focused_diff_view(stoat: &Stoat, editor: EditorId) -> bool {
    stoat
        .focused_editor_ids()
        .is_some_and(|(id, _)| id == editor)
        && stoat
            .active_workspace()
            .editors
            .get(editor)
            .is_some_and(|state| state.diff_view)
}

/// Whether the editor's diff map holds a hunk or a staged mark.
fn editor_shows_changes(stoat: &mut Stoat, editor_id: EditorId) -> bool {
    stoat
        .active_workspace_mut()
        .editors
        .get_mut(editor_id)
        .is_some_and(|editor| {
            editor
                .display_map
                .snapshot()
                .diff_map()
                .is_some_and(|diff_map| {
                    !diff_map.hunks_in_range(0..u32::MAX).is_empty()
                        || !diff_map.staged_marks.is_empty()
                })
        })
}

/// What a staging press reports when the review base moved between the press
/// and its turn in the git queue.
///
/// The press captured its text and cursor row against the checkout that base
/// named, so neither describes the tree the job writes against.
pub(super) const BASE_MOVED: &str = "review base moved; press again";

/// What the git work of a staging press did, for its landing to report.
pub(super) enum StageOutcome {
    /// The job wrote nothing, for the reason the status gives.
    Unchanged(String),
    /// The index took the patch.
    Staged(&'static str),
    /// The commit `old_sha` was rewritten as `new_sha`.
    ///
    /// The status names the move, or the branch left behind, since the commit
    /// is rewritten either way.
    Amended {
        old_sha: String,
        new_sha: String,
        status: String,
    },
}

/// Toggle the git-index state of the diff hunk under the cursor in the focused
/// editor.
///
/// The press stages the hunk when any of it is unstaged, and otherwise unstages
/// it. Staging diffs the git index against the live buffer and takes the change
/// the gutter draws under the cursor, so the staged unit is the one drawn
/// there. That is the hunk whose buffer rows hold the cursor. Inside a hunk the
/// tree pass narrowed, it is the marked run under the cursor, staged against
/// the index the way a line is. Unstaging diffs the index against HEAD at the
/// cursor's index rows. Each reads the index, so a change lands whatever the
/// index holds above it.
///
/// The action works in any editor view on a file in the repo's workdir. A press
/// on an untracked file's hunk adds the file to the index. A missing repo, an
/// ignored file, or a cursor on a row no hunk covers sets a status message and
/// changes nothing.
///
/// The press captures the cursor row, the text, and the review base. The diff
/// and the write run as a queued git job, and the status and the gutter follow
/// when the job lands.
pub(super) fn stage_hunk(stoat: &mut Stoat) -> UpdateEffect {
    queue_stage(stoat, AmendUnit::Hunk)
}

/// Toggle the git-index state of only the cursor line's change, in the focused
/// editor on any file in the repo's workdir.
///
/// The press stages the line when any of its change is unstaged, and otherwise
/// unstages it. The emitted patch is restricted to the cursor's line via
/// [`line_restricted_rows`], and staging diffs the git index (not HEAD) against
/// the live buffer, so sequential single-line stages inside one hunk compose. A
/// press on an untracked file's line adds the file to the index with that line
/// alone. A missing repo, an ignored file, or a cursor on no change sets a
/// status message and changes nothing.
///
/// The index is not the only target. Under a review base the staged side is the
/// checked-out commit, so the keys amend the cursor's line into or out of it
/// through [`amend::amended_file`], leaving the rest of the hunk where it was.
/// [`stage_hunk`] moves the whole hunk the same way.
///
/// The press captures the cursor row, the text, and the review base. The status
/// and the gutter follow when the queued git job lands.
pub(super) fn stage_line(stoat: &mut Stoat) -> UpdateEffect {
    queue_stage(stoat, AmendUnit::Line)
}

/// Capture a staging press and queue its git work.
///
/// The row is the block cursor's cell, so a cursor resting on a line's last
/// cell stages that line, not the next.
///
/// The text is a rope handle, so the press copies nothing that grows with the
/// file. The review base is read now and again at the job's turn. A walk step
/// or a `:review-done` checkout that lands ahead of the job moves the tree the
/// captured text and row describe.
fn queue_stage(stoat: &mut Stoat, unit: AmendUnit) -> UpdateEffect {
    let Some((_editor_id, buffer_id)) = stoat.focused_editor_ids() else {
        return UpdateEffect::None;
    };

    let (cursor_row, text, unit, drawn) = {
        let Some(editor) = super::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
        };
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor().clone();
        let tail = buffer_snapshot.resolve_anchor(&sel.tail());
        let head = buffer_snapshot.resolve_anchor(&sel.head());
        let cursor = stoat_text::cursor_offset(buffer_snapshot.rope(), tail, head);
        let cursor_row = buffer_snapshot.rope().offset_to_point(cursor).row;

        let unit = match unit {
            AmendUnit::Hunk => change_stop_at(&snapshot, buffer_snapshot, cursor_row)
                .map_or(AmendUnit::Hunk, AmendUnit::Rows),
            unit => unit,
        };
        // The press log reports what the gutter draws at the cursor row, which
        // is the live hunk there and the row's staged mark.
        let drawn = snapshot.diff_map().map(|map| {
            let hunk = map
                .live_hunks(buffer_snapshot)
                .in_range(cursor_row..cursor_row + 1)
                .next()
                .map(|(hunk, rows)| (hunk.status, rows, hunk.refined()));
            (hunk, map.staged_for_line(cursor_row))
        });
        (cursor_row, buffer_snapshot.rope().clone(), unit, drawn)
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
    let pressed_rev = review_rev(stoat.active_workspace());
    tracing::info!(
        target: "stoat::review",
        path = %path.display(),
        cursor_row,
        unit = ?unit,
        drawn = ?drawn,
        sides = ?diff::diff_sides(stoat.active_workspace().diff_base()),
        "staging press"
    );

    let job = GitJob::new("stage", None, move |stoat: &mut Stoat| {
        // The index route diffs against HEAD and the amend route writes into
        // the commit, so after a base move both act on a checkout the captured
        // text and row do not describe.
        if review_rev(stoat.active_workspace()) != pressed_rev {
            tracing::warn!(
                target: "stoat::review",
                path = %path.display(),
                status = %BASE_MOVED,
                "staging press refused"
            );
            stoat.set_status(BASE_MOVED);
            return None;
        }
        let Some(repo) = stoat.git_host.discover(&git_root) else {
            let why = "not in a git repository";
            tracing::warn!(
                target: "stoat::review",
                path = %path.display(),
                status = %why,
                "staging press refused"
            );
            stoat.set_status(why);
            return None;
        };

        // Decided before the hunk is resolved, so a refusal says why the
        // transport is unavailable rather than reporting whatever sits under
        // the cursor.
        let target = match amend::amend_route(stoat, &*repo, &path) {
            AmendRoute::Index => None,
            AmendRoute::Commit(target) => Some(target),
            AmendRoute::Refused(why) => {
                tracing::warn!(
                    target: "stoat::review",
                    path = %path.display(),
                    status = %why,
                    "staging press refused"
                );
                stoat.set_status(why);
                return None;
            },
        };

        Some(Box::new(move || {
            let buffer_text = text.to_string();
            let rel = path.strip_prefix(&git_root).unwrap_or(&path);
            let outcome = match (target, unit) {
                (None, AmendUnit::Hunk) => index_hunk(&*repo, &path, rel, &buffer_text, cursor_row),
                (None, AmendUnit::Line) => index_rows(
                    &*repo,
                    &path,
                    rel,
                    &buffer_text,
                    cursor_row..cursor_row + 1,
                    LINE_MESSAGES,
                ),
                (None, AmendUnit::Rows(rows)) => {
                    index_rows(&*repo, &path, rel, &buffer_text, rows, RUN_MESSAGES)
                },
                (Some(target), unit) => match amend::amended_file(
                    &*repo,
                    &target,
                    &path,
                    &buffer_text,
                    cursor_row,
                    unit,
                ) {
                    Ok((amended, message)) => {
                        amend::write_amend(&*repo, &target, rel, amended, message)
                    },
                    Err(nothing) => StageOutcome::Unchanged(nothing.to_string()),
                },
            };
            Box::new(move |stoat: &mut Stoat| land_stage(stoat, buffer_id, &path, outcome))
                as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
    UpdateEffect::Redraw
}

/// The change stop under `cursor_row`, as live buffer rows, when the hunk
/// there is one the tree pass narrowed.
///
/// The stops tile the hunk, so every row of a narrowed hunk answers its stop.
/// `None` when no hunk covers the row or when its hunk was never narrowed.
/// Each of those moves the whole hunk. A deletion or a move covers no buffer
/// row, so it never answers.
fn change_stop_at(
    snapshot: &DisplaySnapshot,
    buffer_snapshot: &MultiBufferSnapshot,
    cursor_row: u32,
) -> Option<Range<u32>> {
    let live = snapshot.diff_map()?.live_hunks(buffer_snapshot);
    let (hunk, _) = live
        .in_range(cursor_row..cursor_row + 1)
        .find(|(_, rows)| rows.contains(&cursor_row))?;
    if !hunk.refined() {
        return None;
    }
    // Staged marks hold no anchor. Until the next diff lands, an edit above
    // one leaves it on a hunk row, where it sorts ahead of the wider stop.
    live.hunk_stops()
        .into_iter()
        .find(|stop| stop.contains(&cursor_row))
}

/// The commit `ws` reviews against, or `None` when no commit is the base.
///
/// A root commit's base is `Some(None)`, the empty tree. A `Memory` base names
/// no commit, so it reads as no base, and so does the HEAD base, which reviews
/// nothing. A pair base names no commit either.
fn review_rev(ws: &Workspace) -> Option<Option<String>> {
    match ws.diff_base() {
        Some(DiffBase::Rev { sha, .. }) => Some(sha.clone()),
        None
        | Some(DiffBase::Head)
        | Some(DiffBase::Memory { .. })
        | Some(DiffBase::Pair { .. }) => None,
    }
}

/// The focused editor's file, when it measures against another file.
fn focused_file_pair(stoat: &Stoat) -> Option<std::path::PathBuf> {
    let (_, buffer_id) = stoat.focused_editor_ids()?;
    let ws = stoat.active_workspace();
    let path = ws.buffers.path_for(buffer_id)?;
    ws.pair_for(path).map(|_| path.to_path_buf())
}

/// Return the focused file to the workspace's base, and answer whether it had
/// a pair.
///
/// Every command that points the focused file at a base of its own drops the
/// pair first, since a pair wins over the workspace's base for its file.
fn drop_focused_pair(stoat: &mut Stoat) -> bool {
    let Some(path) = focused_file_pair(stoat) else {
        return false;
    };
    stoat.active_workspace_mut().remove_pair(&path)
}

/// Report a landed staging press, and stale the gutter's map when git moved.
///
/// An amend also moves the walk and the base's commit name onto the commit it
/// rewrote.
fn land_stage(stoat: &mut Stoat, buffer_id: BufferId, path: &Path, outcome: StageOutcome) {
    let (status, landed) = match outcome {
        StageOutcome::Unchanged(status) => (status, false),
        StageOutcome::Staged(message) => {
            stoat
                .active_workspace_mut()
                .invalidate_diff(buffer_id, path);
            (message.to_string(), true)
        },
        StageOutcome::Amended {
            old_sha,
            new_sha,
            status,
        } => {
            amend::anchor_walk_to(stoat, &old_sha, &new_sha);
            stoat
                .active_workspace_mut()
                .follow_amended_commit(&old_sha, &new_sha);
            stoat
                .active_workspace_mut()
                .invalidate_diff(buffer_id, path);
            (status, true)
        },
    };

    if landed {
        tracing::info!(
            target: "stoat::review",
            path = %path.display(),
            %status,
            "staging press landed"
        );
    } else {
        tracing::warn!(
            target: "stoat::review",
            path = %path.display(),
            %status,
            "staging press changed nothing"
        );
    }
    stoat.set_status(status);
}

/// Apply the hunk under `cursor_row` to the index, on whatever thread calls.
///
/// The hunk stages when a forward patch exists, and unstages otherwise.
/// Staging diffs the index against `buffer_text`, and unstaging diffs the index
/// against HEAD, so the patch lands whatever the index holds above the hunk.
/// See [`stage_hunk`].
fn index_hunk(
    repo: &dyn GitRepo,
    path: &Path,
    rel: &Path,
    buffer_text: &str,
    cursor_row: u32,
) -> StageOutcome {
    let nothing = || StageOutcome::Unchanged("no hunk under the cursor".to_string());
    let Some(texts) = read_staging_texts(repo, path, buffer_text) else {
        return nothing();
    };

    let stage = || stage_hunk_patch(rel, &texts, buffer_text, cursor_row);
    let unstage = || {
        unstage_hunk_patch(
            rel,
            &texts.index,
            texts.head.as_deref(),
            buffer_text,
            cursor_row,
        )
    };
    let patch_and_message = stage()
        .map(|patch| (patch, "staged hunk"))
        .or_else(|| unstage().map(|patch| (patch, "unstaged hunk")));

    let Some((patch, message)) = patch_and_message else {
        return nothing();
    };
    match repo.apply_to_index(&patch) {
        Ok(()) => StageOutcome::Staged(message),
        Err(err) => StageOutcome::Unchanged(format!("could not update staging: {err}")),
    }
}

/// [`staging_texts`] for a staging press, with a debug line on the
/// `stoat::review` target that says what the press read.
///
/// A file that no blob holds and that is no untracked work logs as such,
/// because its press reports the same status as a row that no hunk covers.
fn read_staging_texts(repo: &dyn GitRepo, path: &Path, buffer_text: &str) -> Option<StagingTexts> {
    let Some(texts) = staging_texts(repo, path) else {
        tracing::debug!(
            target: "stoat::review",
            path = %path.display(),
            "no blob holds the file, and the repo ignores it or it lies outside the workdir"
        );
        return None;
    };
    tracing::debug!(
        target: "stoat::review",
        path = %path.display(),
        head = texts.head.is_some(),
        in_index = texts.in_index,
        index_lines = texts.index.lines().count(),
        buffer_lines = buffer_text.lines().count(),
        "staging texts read"
    );
    Some(texts)
}

/// HEAD's and the index's text for a staging press, as [`staging_texts`]
/// reads them.
struct StagingTexts {
    /// [`None`] for a file HEAD does not hold.
    head: Option<String>,
    index: String,
    /// False for an untracked file, whose empty index text stands for no entry,
    /// so its stage creates one.
    in_index: bool,
}

/// HEAD's and the index's text for `path`, for a press that stages or
/// unstages.
///
/// A moved file has no HEAD blob at its own path, so its HEAD text comes from
/// the path it moved from. A file HEAD does not hold reads [`None`] there, and
/// an unstage that empties its index text removes it from the index.
///
/// A file with no index entry reads its index text as HEAD's. A patch for a
/// file that `git rm --cached` took out of the index then fails to apply,
/// rather than putting part of the file back.
///
/// An untracked file reads as empty index text with no entry behind it, so its
/// stage adds the file to the index. [`None`] when no blob holds the file and
/// the repo ignores it or it lies outside the workdir.
fn staging_texts(repo: &dyn GitRepo, path: &Path) -> Option<StagingTexts> {
    let head = repo
        .head_content(path)
        .or_else(|| repo.head_content(&repo.rename_source(path)?));
    if let Some(index) = repo.index_content(path).or_else(|| head.clone()) {
        return Some(StagingTexts {
            head,
            index,
            in_index: true,
        });
    }
    diff::untracked_in_tree(repo, path).then(|| StagingTexts {
        head: None,
        index: String::new(),
        in_index: false,
    })
}

/// Build the patch that stages the hunk under the cursor by diffing the git
/// index against the live buffer.
///
/// An untracked file's patch creates its index entry. `None` when the cursor
/// sits on no index-vs-buffer hunk.
fn stage_hunk_patch(
    rel: &Path,
    texts: &StagingTexts,
    buffer_text: &str,
    cursor_row: u32,
) -> Option<String> {
    let index_text = texts.index.as_str();
    let (buffer_text, removes_file) = stage_post_image(buffer_text);
    let hunks = line_hunks(index_text, buffer_text);
    log_hunks_near(
        "stage hunk",
        cursor_row,
        &hunks,
        &base_line_ranges(index_text, &hunks),
        false,
    );

    // Resolved by the gutter's own rule, so the staged unit is the one drawn
    // under the cursor. A zero-width range is a deletion or a move, which the
    // gutter marks at its anchor row.
    let k = hunks.iter().position(|hunk| {
        let rows = &hunk.buffer_line_range;
        match rows.is_empty() {
            true => rows.start == cursor_row,
            false => rows.contains(&cursor_row),
        }
    })?;
    hunk_to_patch(
        rel,
        index_text,
        buffer_text,
        &hunks,
        k,
        removes_file,
        !texts.in_index,
    )
}

/// The text a stage diffs the index against, and whether it stands for a file
/// removed from the working tree.
///
/// A removed file opens as a lone newline, which a diff reads as one blank
/// line. It stages as empty text instead, so the patch removes the file rather
/// than writing a blob that holds one blank line.
fn stage_post_image(buffer_text: &str) -> (&str, bool) {
    match diff::buffer_removed(buffer_text) {
        true => ("", true),
        false => (buffer_text, false),
    }
}

/// Build the patch that unstages the hunk under the cursor by reverting its
/// index rows to HEAD.
///
/// The cursor row is a buffer coordinate, so it is first mapped to the index.
/// Diffing the index against HEAD then expresses each staged change as index
/// rows, and the patch reverts the one at the mapped row. `None` when that row
/// carries no staged change.
///
/// A `head_text` of [`None`] marks a file HEAD does not hold, which an unstage
/// of all its index lines removes from the index.
fn unstage_hunk_patch(
    rel: &Path,
    index_text: &str,
    head_text: Option<&str>,
    buffer_text: &str,
    cursor_row: u32,
) -> Option<String> {
    let removes_file = head_text.is_none();
    let head_text = head_text.unwrap_or("");
    let index_row = map_buffer_row_to_index(index_text, buffer_text, cursor_row);
    let hunks = line_hunks(index_text, head_text);
    let ranges = base_line_ranges(index_text, &hunks);
    log_hunks_near("unstage hunk", index_row, &hunks, &ranges, true);

    // A staged deletion reads as HEAD lines added over an empty index range,
    // so an empty range matches the row it starts at.
    let k = ranges.iter().position(|rows| match rows.is_empty() {
        true => rows.start == index_row,
        false => rows.contains(&index_row),
    })?;
    hunk_to_patch(rel, index_text, head_text, &hunks, k, removes_file, false)
}

/// What a line press reports, as `(staged, unstaged, nothing to move)`.
const LINE_MESSAGES: (&str, &str, &str) = (
    "staged line",
    "unstaged line",
    "no line change under the cursor",
);

/// What a press on a marked run reports. The reader pressed the hunk keys, so
/// the words are the hunk's.
const RUN_MESSAGES: (&str, &str, &str) =
    ("staged hunk", "unstaged hunk", "no hunk under the cursor");

/// Apply the change on buffer `rows` to the index, on whatever thread calls,
/// and report it with the `(staged, unstaged, nothing)` statuses.
///
/// The rows stage when a forward patch exists, and unstage otherwise. They
/// are staged against the index rather than HEAD, so sequential
/// stages inside one hunk compose. Rows that meet more than one hunk move with
/// one patch per hunk, applied in order until one fails. See [`stage_line`]
/// and [`stage_hunk`].
fn index_rows(
    repo: &dyn GitRepo,
    path: &Path,
    rel: &Path,
    buffer_text: &str,
    rows: Range<u32>,
    (staged, unstaged, nothing): (&'static str, &'static str, &'static str),
) -> StageOutcome {
    let unchanged = || StageOutcome::Unchanged(nothing.to_string());
    let Some(texts) = read_staging_texts(repo, path, buffer_text) else {
        return unchanged();
    };
    let rel = rel.to_string_lossy();

    let stage = || {
        Some(stage_rows_patch(&rel, &texts, buffer_text, rows.clone()))
            .filter(|patches| !patches.is_empty())
    };
    let unstage = || {
        Some(unstage_rows_patch(
            &rel,
            &texts.index,
            texts.head.as_deref(),
            buffer_text,
            rows.clone(),
        ))
        .filter(|patches| !patches.is_empty())
    };
    let patches_and_message = stage()
        .map(|patches| (patches, staged))
        .or_else(|| unstage().map(|patches| (patches, unstaged)));

    let Some((patches, message)) = patches_and_message else {
        return unchanged();
    };
    for patch in &patches {
        if let Err(err) = repo.apply_to_index(patch) {
            return StageOutcome::Unchanged(format!("could not update staging: {err}"));
        }
    }
    StageOutcome::Staged(message)
}

/// Line hunks between `base` and `buffer`, the extents every staging path
/// resolves against.
fn line_hunks(base: &str, buffer: &str) -> Vec<crate::diff_map::DiffHunk> {
    let result = stoat_language::structural_diff::diff(base, buffer);
    crate::diff_map::changes_to_hunks(&result.changes, base, buffer)
}

/// The index range of the two ranges in `compared` beside `row`, which are the
/// first range that reaches the row or lies past it, and the range before it.
///
/// An empty range is a deletion, and it reaches the row it starts at, as the
/// lookups match it.
fn hunks_around(row: u32, compared: &[Range<u32>]) -> Range<usize> {
    let k = compared
        .iter()
        .position(|rows| match rows.is_empty() {
            true => rows.start >= row,
            false => rows.end > row,
        })
        .unwrap_or(compared.len());
    k.saturating_sub(1)..(k + 1).min(compared.len())
}

/// Log the hunks a staging lookup compared around `row`, so a press that
/// stages nothing shows how near the closest hunks sat and on which side.
///
/// Each hunk logs as its status, its buffer rows, and its base rows from
/// `base_rows`. `row` lies in the base rows when `by_base` is set, and in the
/// buffer rows otherwise. The two neighbors log at debug, and the whole list
/// at trace.
fn log_hunks_near(
    what: &'static str,
    row: u32,
    hunks: &[crate::diff_map::DiffHunk],
    base_rows: &[Range<u32>],
    by_base: bool,
) {
    let triples: Vec<_> = hunks
        .iter()
        .zip(base_rows)
        .map(|(hunk, base)| (hunk.status, hunk.buffer_line_range.clone(), base.clone()))
        .collect();
    let compared: Vec<Range<u32>> = match by_base {
        true => base_rows.to_vec(),
        false => hunks
            .iter()
            .map(|hunk| hunk.buffer_line_range.clone())
            .collect(),
    };
    tracing::debug!(
        target: "stoat::review",
        what,
        row,
        count = hunks.len(),
        near = ?&triples[hunks_around(row, &compared)],
        "staging lookup"
    );
    tracing::trace!(target: "stoat::review", what, all = ?triples, "staging lookup");
}

/// Build the patches that stage only buffer `rows` by diffing the git index
/// against the live buffer and keeping the changes on those rows.
///
/// One patch per index-vs-buffer hunk the rows meet, last hunk first. Each
/// patch is built against the same index text, and a patch applied below
/// another never moves the lines the other names. Empty when the rows meet no
/// change.
///
/// An untracked file's empty index text meets the buffer in one hunk, so its
/// rows take one patch, and that patch creates the index entry.
fn stage_rows_patch(
    rel: &str,
    texts: &StagingTexts,
    buffer_text: &str,
    rows: Range<u32>,
) -> Vec<String> {
    let index_text = texts.index.as_str();
    let (buffer_text, removes_file) = stage_post_image(buffer_text);
    let hunks = line_hunks(index_text, buffer_text);
    log_hunks_near(
        "stage rows",
        rows.start,
        &hunks,
        &base_line_ranges(index_text, &hunks),
        false,
    );
    (0..hunks.len())
        .filter(|&k| ranges_meet(&hunks[k].buffer_line_range, &rows))
        .rev()
        .filter_map(|k| {
            let hunk_rows = hunk_rows(index_text, buffer_text, &hunks, k, HUNK_CONTEXT)?;
            let kept = line_restricted_rows(&hunk_rows, rows.start + 1..rows.end + 1, true)?;
            Some(rows_to_unified_diff(
                Path::new(rel),
                index_text,
                buffer_text,
                &kept,
                removes_file,
                !texts.in_index,
            ))
        })
        .collect()
}

/// Build the patches that unstage only buffer `rows` by reverting their index
/// rows to HEAD.
///
/// The rows are buffer coordinates, so they are first mapped back to the index
/// by removing the line delta of every index-vs-buffer hunk above them.
/// Diffing the index against HEAD then expresses each staged change as
/// index-side rows, and each forward patch reverts the mapped rows of one hunk
/// to HEAD, last hunk first. Empty when the mapped rows carry no staged change.
///
/// A `head_text` of [`None`] marks a file HEAD does not hold, as
/// [`unstage_hunk_patch`] reads it.
fn unstage_rows_patch(
    rel: &str,
    index_text: &str,
    head_text: Option<&str>,
    buffer_text: &str,
    rows: Range<u32>,
) -> Vec<String> {
    let removes_file = head_text.is_none();
    let head_text = head_text.unwrap_or("");
    let index_rows = {
        let starts = line_starts(index_text);
        let buffer_hunks = line_hunks(index_text, buffer_text);
        let index_row = |row| index_row_through(&buffer_hunks, &starts, index_text, row);
        index_row(rows.start)..index_row(rows.end.saturating_sub(1)) + 1
    };

    // The emitted patch runs index to HEAD, so the index is this diff's base
    // side and the staged rows are base rows. DiffHunk records base bytes
    // rather than base lines, so the ranges come from base_line_ranges.
    let hunks = line_hunks(index_text, head_text);
    let ranges = base_line_ranges(index_text, &hunks);
    log_hunks_near("unstage rows", index_rows.start, &hunks, &ranges, true);
    (0..hunks.len())
        .filter(|&k| ranges_meet(&ranges[k], &index_rows))
        .rev()
        .filter_map(|k| {
            let hunk_rows = hunk_rows(index_text, head_text, &hunks, k, HUNK_CONTEXT)?;
            let kept =
                line_restricted_rows(&hunk_rows, index_rows.start + 1..index_rows.end + 1, false)?;
            Some(rows_to_unified_diff(
                Path::new(rel),
                index_text,
                head_text,
                &kept,
                removes_file,
                false,
            ))
        })
        .collect()
}

/// Whether row ranges `a` and `b` share a row.
fn ranges_meet(a: &Range<u32>, b: &Range<u32>) -> bool {
    a.start < b.end && b.start < a.end
}

/// Map a 0-based buffer row to its 0-based row in the git index.
///
/// Each index-vs-buffer hunk fully above the cursor shifts buffer rows past the
/// index by its added-minus-removed line count, so subtracting that shift
/// recovers the index row. The removed count comes from [`hunk_base_lines`],
/// the count the staging patches place their hunks by, so the two agree on
/// where each hunk sits.
///
/// An amend passes the commit's text as `index_text`, since the commit is the
/// staged side there. See [`amend::amended_file`].
pub(super) fn map_buffer_row_to_index(index_text: &str, buffer_text: &str, cursor_row: u32) -> u32 {
    let starts = line_starts(index_text);
    let buffer_hunks = line_hunks(index_text, buffer_text);
    index_row_through(&buffer_hunks, &starts, index_text, cursor_row)
}

/// [`map_buffer_row_to_index`] through index-vs-buffer hunks and index line
/// starts the caller already holds, so a caller that maps several rows diffs
/// the two texts once.
fn index_row_through(
    buffer_hunks: &[crate::diff_map::DiffHunk],
    starts: &[usize],
    index_text: &str,
    cursor_row: u32,
) -> u32 {
    let mut shift: i64 = 0;
    for hunk in buffer_hunks {
        if hunk.buffer_line_range.end <= cursor_row {
            let buffer_len = (hunk.buffer_line_range.end - hunk.buffer_line_range.start) as i64;
            let index_len = hunk_base_lines(hunk, starts, index_text).len() as i64;
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
///
/// The base side reads the tree the changeset diff filed in the document's
/// memo, so a base parses once per preview. The buffer side parses here. The
/// diff files left sides only, so the memo answers nothing for a buffer, and a
/// buffer tree stored there evicts a base that the files behind it read.
pub(super) fn attach_preview_highlights(
    doc: &mut DiffDocument,
    styles: &SyntaxStyles,
    cache: &BaseHighlightCache,
) {
    let memo = doc.tree_cache.clone();
    for file in &mut doc.files {
        let Some(language) = file.language.clone() else {
            continue;
        };
        file.base_highlights = Some(compute_base_highlights(
            &file.base_text,
            &language,
            styles,
            cache,
            Some(&memo),
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
    use super::{
        attach_preview_highlights, build_document_from_changes, editor_shows_changes, hunks_around,
        stage_rows_patch, unstage_rows_patch, StagingTexts,
    };
    use crate::{
        badge::BadgeSource,
        display_map::syntax_theme::SyntaxStyles,
        review_session::DiffDocument,
        test_harness::TestHarness,
        theme::Theme,
        workspace::diff::{self, BaseHighlightCache, BaseHighlightMemo, DiffBase},
    };
    use std::{
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };
    use stoat_action::Action;
    use stoat_language::{structural_diff::TreeCache, LanguageRegistry};
    use stoat_text::Point;

    /// The text of the buffer open at `path`, which lets a test read a proposal
    /// that landed in a file the pane no longer shows.
    fn buffer_text_at(h: &TestHarness, path: &str) -> Option<String> {
        let ws = h.stoat.active_workspace();
        let id = ws.buffers.id_for_path(Path::new(path))?;
        let shared = ws.buffers.get(id)?;
        let text = shared.read().expect("buffer poisoned").rope().to_string();
        Some(text)
    }

    /// The text the diff map of the buffer open at `path` measures against.
    fn diff_base_at(h: &TestHarness, path: &str) -> Option<String> {
        let ws = h.stoat.active_workspace();
        let id = ws.buffers.id_for_path(Path::new(path))?;
        let shared = ws.buffers.get(id)?;
        let guard = shared.read().expect("buffer poisoned");
        Some(guard.diff_map.as_ref()?.base_text()?.to_string())
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

    /// A repo where `a.rs` and `b.rs` differ from HEAD on their second row and
    /// `clean.rs` matches it, with no file open.
    fn repo_with_a_clean_file() -> TestHarness {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/repo",
            &[("a.rs", "a\nb\n", "a\nc\n"), ("b.rs", "d\ne\n", "d\nf\n")],
        );
        // stage_review_scenario refuses a file that matches HEAD.
        h.fake_fs().insert_file("/repo/clean.rs", b"same\n");
        {
            let mut builder = h.fake_git().add_repo("/repo").with_fs(h.fake_fs());
            builder.head_file("clean.rs", "same\n");
        }
        h
    }

    /// [`repo_with_a_clean_file`] open on `a.rs` with the latch armed and its
    /// hunks in.
    fn latched_pair() -> TestHarness {
        let mut h = repo_with_a_clean_file();
        h.open_file(Path::new("/repo/a.rs"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
        h.settle();
        h
    }

    /// Where the diff of the buffer at `path` stands, as `(diff view on the
    /// focused editor, hunks on the pool, hunks-only map in, whole map
    /// current, hunk count)`.
    fn want_state(h: &mut TestHarness, path: &str) -> (bool, bool, bool, bool, usize) {
        let diff_view = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .diff_view;
        let ws = h.stoat.active_workspace();
        let id = ws
            .buffers
            .id_for_path(Path::new(path))
            .expect("open buffer");
        let hunks = ws
            .buffers
            .get(id)
            .expect("buffer")
            .read()
            .expect("buffer poisoned")
            .diff_map
            .as_ref()
            .map_or(0, |map| map.hunks_in_range(0..u32::MAX).len());
        (
            diff_view,
            ws.diff_map_pending(id),
            ws.hunks_installed(id),
            ws.diff_map_current(id),
            hunks,
        )
    }

    /// The test scheduler runs blocking work inline, so the hop count, not the
    /// blob reads, is what shows the switch left its diff to the pool.
    #[test]
    fn a_latched_switch_keeps_the_panes_view_until_its_hunks_land() {
        let mut h = latched_pair();
        let hops = h.blocking_calls();

        h.stoat.open_file(Path::new("/repo/b.rs"));
        assert_eq!(
            (want_state(&mut h, "/repo/b.rs"), h.blocking_calls() - hops),
            ((true, true, false, false, 0), 1),
            "the switch keeps the diff layout and leaves the hunks to the pool",
        );

        h.settle();
        assert_eq!(
            want_state(&mut h, "/repo/b.rs"),
            (true, false, true, false, 1),
            "the hunk lands, and its colors stay owed",
        );
    }

    #[test]
    fn a_latched_switch_to_a_clean_file_goes_plain_when_its_hunks_land() {
        let mut h = latched_pair();

        h.stoat.open_file(Path::new("/repo/clean.rs"));
        assert_eq!(
            want_state(&mut h, "/repo/clean.rs"),
            (true, true, false, false, 0),
            "the switch keeps the diff layout while the hunks are on the pool",
        );

        h.settle();
        assert_eq!(
            want_state(&mut h, "/repo/clean.rs"),
            (false, false, true, false, 0),
            "a file with no change shows plain once its hunks land",
        );
    }

    #[test]
    fn leaving_the_diff_before_a_switch_lands_keeps_the_view_off() {
        let mut h = latched_pair();

        h.stoat.open_file(Path::new("/repo/b.rs"));
        super::exit_diff_view(&mut h.stoat);
        h.settle();
        assert_eq!(
            want_state(&mut h, "/repo/b.rs"),
            (false, false, true, false, 1),
            "the landing finds the latch gone and leaves the view off",
        );
    }

    #[test]
    fn a_diff_open_after_a_latched_switch_keeps_the_view_on_a_clean_file() {
        let mut h = latched_pair();

        h.stoat.open_file(Path::new("/repo/clean.rs"));
        super::enter_diff_view(&mut h.stoat);
        h.settle();
        assert_eq!(
            want_state(&mut h, "/repo/clean.rs"),
            (true, false, true, false, 0),
            "the open outranks the switch that waited on the same hunks",
        );
    }

    /// The number of entries on the focused pane's jumplist.
    fn jump_count(h: &TestHarness) -> usize {
        let panes = &h.stoat.active_workspace().panes;
        panes.pane(panes.focus()).jumplist.entries().len()
    }

    #[test]
    fn a_diff_toggle_jumps_to_the_nearest_change_when_its_hunks_land() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario("/repo", &[("a.rs", "a\nb\n", "a\nc\n")]);
        h.open_file(Path::new("/repo/a.rs"));
        let jumps_before = jump_count(&h);

        h.stoat.toggle_diff_view();
        assert_eq!(
            (
                review_cursor_row(&mut h),
                want_state(&mut h, "/repo/a.rs").1,
                jump_count(&h),
            ),
            (0, true, jumps_before),
            "the cursor waits for the hunks",
        );

        h.settle();
        assert_eq!(
            (review_cursor_row(&mut h), jump_count(&h)),
            (1, jumps_before + 1),
            "the cursor lands on the change, and the jump goes on the jumplist",
        );
    }

    /// A second toggle closes the view while the hunks of the first are still
    /// on the pool, and their landing finds the view closed.
    #[test]
    fn a_diff_closed_before_its_hunks_land_leaves_the_cursor() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario("/repo", &[("a.rs", "a\nb\n", "a\nc\n")]);
        h.open_file(Path::new("/repo/a.rs"));
        let jumps_before = jump_count(&h);

        h.stoat.toggle_diff_view();
        h.stoat.toggle_diff_view();
        let pending = want_state(&mut h, "/repo/a.rs").1;
        h.settle();

        assert_eq!(
            (
                pending,
                review_cursor_row(&mut h),
                jump_count(&h),
                want_state(&mut h, "/repo/a.rs").0,
            ),
            (true, 0, jumps_before, false),
            "the hunks land after the close, and the cursor and the jumplist stay",
        );
    }

    /// A layout key closes the view of a clean file while its hunks are on the
    /// pool, so their landing does not cross into the first changed file.
    #[test]
    fn a_layout_key_before_the_hunks_land_leaves_the_clean_file_open() {
        let mut h = repo_with_a_clean_file();
        h.open_file(Path::new("/repo/clean.rs"));

        h.stoat.toggle_diff_view();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::TogglePaneWiden);
        let pending = want_state(&mut h, "/repo/clean.rs").1;
        h.settle();

        assert_eq!(
            (
                pending,
                pair_view(&mut h),
                h.stoat.pending_message.as_deref()
            ),
            (
                true,
                (false, false, Some(PathBuf::from("/repo/clean.rs"))),
                Some("diff closed"),
            ),
        );
    }

    /// The hunk opens on a blank row, so the toggle lands on the row below it
    /// that holds text, where a walk lands on the same stop.
    #[test]
    fn a_diff_toggle_lands_past_a_stop_s_blank_row() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario("/repo", &[("a.rs", "a\nb\n", "a\n\nc\n")]);
        h.open_file(Path::new("/repo/a.rs"));

        h.stoat.toggle_diff_view();
        h.settle();

        assert_eq!(
            crate::test_harness::editor::focused_cursor_point(&mut h.stoat),
            Point::new(2, 0)
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

    /// What the three pieces of `:diff-files <rev>` state currently read: the
    /// base every buffer diffs against, whether the pane is latched into the
    /// diff layout, and which file is open.
    fn diff_state(h: &TestHarness) -> (Option<String>, bool, Option<PathBuf>) {
        let base = match h.stoat.active_workspace().diff_base() {
            Some(DiffBase::Rev { sha, .. }) => sha.clone(),
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
            &stoat_action::DiffFiles {
                rev: Some("base0".to_string()),
            },
        );
        h.settle();

        assert_eq!(
            diff_state(&h),
            (Some("base0".to_string()), true, Some(workdir.join("b.rs")),),
        );
    }

    /// The sides bar names a revision as the reader typed it, beside the short
    /// sha it resolved to. A typed sha needs no second spelling.
    #[test]
    fn diff_with_a_rev_names_the_base_as_typed() {
        let (mut h, workdir) = diff_rev_harness();
        h.fake_git().add_repo(&workdir).branch("main", "base0");

        let sides = ["main", "base0"].map(|rev| {
            crate::action_handlers::dispatch(
                &mut h.stoat,
                &stoat_action::DiffFiles {
                    rev: Some(rev.to_string()),
                },
            );
            h.settle();
            diff::diff_sides(h.stoat.active_workspace().diff_base())
        });

        assert_eq!(
            sides,
            [("main base0", "working tree"), ("base0", "working tree")]
                .map(|(left, right)| (left.to_string(), right.to_string())),
            "a branch keeps its name, and a typed sha shows once",
        );
    }

    /// A repo whose `f.txt` has a staged change and an unstaged one, open in
    /// the focused pane.
    fn half_staged_harness() -> TestHarness {
        let mut h = TestHarness::with_size(80, 20);
        h.stage_index_scenario(
            "/repo",
            &[("f.txt", "a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nc\nD\n")],
        );
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/f.txt"));
        h
    }

    /// Whether the base is HEAD, and whether the focused editor shows the diff.
    fn head_base_and_view(h: &mut TestHarness) -> (bool, bool) {
        let head = matches!(h.stoat.active_workspace().diff_base(), Some(DiffBase::Head));
        let view = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .diff_view;
        (head, view)
    }

    fn run(h: &mut TestHarness, action: &dyn Action) {
        crate::action_handlers::dispatch(&mut h.stoat, action);
        h.settle();
    }

    #[test]
    fn diff_against_head_installs_the_head_base_and_opens_the_view() {
        let mut h = half_staged_harness();
        run(&mut h, &stoat_action::DiffAgainstHead);
        assert_eq!(head_base_and_view(&mut h), (true, true));
    }

    /// The installed base with the view closed opens the view, which lands on
    /// the first change. With the view already open it changes nothing, so a
    /// second press leaves the cursor where the reader moved it.
    #[test]
    fn diff_against_the_installed_base_keeps_the_open_view() {
        let mut h = half_staged_harness();
        run(&mut h, &stoat_action::DiffAgainstIndex);
        assert_eq!(
            (head_base_and_view(&mut h).1, review_cursor_row(&mut h)),
            (true, 3),
            "the view opens on the unstaged hunk, the first change against the index",
        );

        run(&mut h, &stoat_action::MoveUp);
        run(&mut h, &stoat_action::MoveUp);
        run(&mut h, &stoat_action::DiffAgainstIndex);
        assert_eq!(
            review_cursor_row(&mut h),
            1,
            "and a second press moves nothing"
        );
    }

    #[test]
    fn diff_against_index_returns_from_head_to_the_index() {
        let mut h = half_staged_harness();
        run(&mut h, &stoat_action::DiffAgainstHead);
        run(&mut h, &stoat_action::DiffAgainstIndex);

        assert_eq!(
            (
                h.stoat.active_workspace().diff_base().is_none(),
                head_base_and_view(&mut h).1
            ),
            (true, true),
            "the index base with the view still open",
        );
    }

    /// Closing the diff keeps the HEAD base the reader picked, so the gutter
    /// of the plain pane goes on marking against HEAD.
    #[test]
    fn a_bare_diff_close_keeps_the_head_base() {
        let mut h = half_staged_harness();
        run(&mut h, &stoat_action::DiffAgainstHead);
        run(&mut h, &stoat_action::DiffFiles { rev: None });

        assert_eq!(
            head_base_and_view(&mut h),
            (true, false),
            "the view closes onto the HEAD base",
        );
    }

    /// A revision displaces the HEAD pick only while it holds. Closing the
    /// diff returns to HEAD, not to the index.
    #[test]
    fn a_displaced_head_base_returns_when_the_revision_closes() {
        let mut h = half_staged_harness();
        run(&mut h, &stoat_action::DiffAgainstHead);
        h.stoat
            .active_workspace_mut()
            .set_diff_base(Some(DiffBase::named("base0", "base0".into())));
        run(&mut h, &stoat_action::DiffFiles { rev: None });

        assert_eq!(head_base_and_view(&mut h), (true, false));
    }

    #[test]
    fn diff_against_head_under_a_review_drops_the_review_base() {
        let mut h = half_staged_harness();
        h.stoat
            .active_workspace_mut()
            .set_diff_base(Some(DiffBase::named("base0", "base0".into())));
        run(&mut h, &stoat_action::DiffAgainstHead);

        assert_eq!(
            head_base_and_view(&mut h),
            (true, true),
            "HEAD replaces the review base"
        );
    }

    #[test]
    fn the_base_toggle_flips_between_head_and_the_index() {
        let mut h = half_staged_harness();
        run(&mut h, &stoat_action::DiffBaseToggle);
        assert_eq!(
            (
                head_base_and_view(&mut h),
                h.stoat.pending_message.as_deref()
            ),
            ((false, false), Some("diff: index → working tree")),
            "the first press picks the index and opens nothing",
        );

        run(&mut h, &stoat_action::DiffBaseToggle);
        assert_eq!(
            (
                head_base_and_view(&mut h),
                h.stoat.pending_message.as_deref()
            ),
            ((true, false), Some("diff: HEAD → working tree")),
            "the second press returns to HEAD",
        );
    }

    #[test]
    fn the_base_toggle_keeps_an_open_view_open() {
        let mut h = half_staged_harness();
        run(&mut h, &stoat_action::DiffFiles { rev: None });
        run(&mut h, &stoat_action::DiffBaseToggle);
        assert_eq!(head_base_and_view(&mut h), (false, true));
    }

    /// The first press under a revision lands on the pick with no flip, so one
    /// press always returns to the working tree.
    #[test]
    fn the_base_toggle_under_a_revision_returns_to_the_pick() {
        let mut h = half_staged_harness();
        h.stoat
            .active_workspace_mut()
            .set_diff_base(Some(DiffBase::named("base0", "base0".into())));
        run(&mut h, &stoat_action::DiffBaseToggle);

        assert_eq!(
            (
                matches!(h.stoat.active_workspace().diff_base(), Some(DiffBase::Head)),
                h.stoat.pending_message.as_deref(),
            ),
            (true, Some("diff: HEAD → working tree")),
        );
    }

    #[test]
    fn diff_head_as_a_word_installs_the_head_base() {
        let mut h = half_staged_harness();
        run(
            &mut h,
            &stoat_action::DiffFiles {
                rev: Some("HEAD".into()),
            },
        );
        assert_eq!(head_base_and_view(&mut h), (true, true));
    }

    #[test]
    fn diff_index_as_a_word_returns_to_the_index() {
        let mut h = half_staged_harness();
        run(&mut h, &stoat_action::DiffAgainstHead);
        run(
            &mut h,
            &stoat_action::DiffFiles {
                rev: Some("index".into()),
            },
        );

        assert_eq!(
            (
                h.stoat.active_workspace().diff_base().is_none(),
                head_base_and_view(&mut h).1
            ),
            (true, true),
        );
    }

    #[test]
    fn the_base_edit_seeds_the_palette_with_the_current_base() {
        let mut h = half_staged_harness();
        let mut seeds = vec![base_edit_seed(&mut h)];
        run(&mut h, &stoat_action::DiffAgainstIndex);
        seeds.push(base_edit_seed(&mut h));
        for base in [
            DiffBase::parent_of("c1", Some("base0".into())),
            DiffBase::parent_of("c1", None),
            DiffBase::named("main", "base0".into()),
        ] {
            h.stoat.active_workspace_mut().set_diff_base(Some(base));
            seeds.push(base_edit_seed(&mut h));
        }

        assert_eq!(
            seeds,
            [
                "diff-files HEAD",
                "diff-files index",
                "diff-files base0",
                "diff-files ",
                "diff-files main"
            ]
            .map(|seed| Some(seed.to_string())),
        );
    }

    /// The text `DiffBaseEdit` opens the palette with. The palette closes again,
    /// so the next press opens a fresh one.
    fn base_edit_seed(h: &mut TestHarness) -> Option<String> {
        run(h, &stoat_action::DiffBaseEdit);
        let seed = h
            .stoat
            .command_palette
            .as_ref()
            .map(|palette| palette.input.text(h.stoat.active_workspace()));
        run(h, &stoat_action::CancelPromptInput);
        seed
    }

    #[test]
    fn diff_pair_opens_the_second_file_against_the_first() {
        let mut h = pair_harness();
        run(&mut h, &stoat_action::DiffPair);
        assert_eq!(
            (
                pair_base(&h, "/pair/b.txt"),
                pair_view(&mut h),
                review_cursor_row(&mut h),
                h.stoat.pending_message.as_deref(),
            ),
            (
                Some(("/pair/a.txt".into(), "one\ntwo\nthree\n".into())),
                (true, true, Some(PathBuf::from("/pair/b.txt"))),
                1,
                None,
            ),
            "the right file diffs against the left, focused and widened, with the cursor on \
             the first difference and no word that the files match",
        );
    }

    #[test]
    fn a_second_diff_pair_closes_the_view() {
        let mut h = pair_harness();
        run(&mut h, &stoat_action::DiffPair);
        run(&mut h, &stoat_action::DiffPair);
        assert_eq!(
            (pair_base(&h, "/pair/b.txt"), pair_view(&mut h)),
            (None, (false, false, Some(PathBuf::from("/pair/b.txt")))),
        );
    }

    /// A split pressed on the diff closes the diff and splits nothing, so the
    /// view never ends up narrow beside a new pane.
    #[test]
    fn a_split_on_the_diff_closes_it_and_splits_nothing() {
        let mut h = half_staged_harness();
        run(&mut h, &stoat_action::DiffFiles { rev: None });

        run(&mut h, &stoat_action::SplitRight);

        assert_eq!(layout(&h), (1, Some("file"), false, Some("diff closed")));
    }

    /// A focus move on a diff widened over a split closes the diff where it
    /// stands, and the split it covered comes back.
    #[test]
    fn a_focus_move_on_the_diff_closes_it_in_place() {
        let mut h = half_staged_harness();
        run(&mut h, &stoat_action::SplitRight);
        run(&mut h, &stoat_action::DiffFiles { rev: None });
        let focus = h.stoat.active_workspace().panes.focus();

        run(&mut h, &stoat_action::FocusLeft);

        assert_eq!(
            (
                h.stoat.active_workspace().panes.focus() == focus,
                layout(&h)
            ),
            (true, (2, Some("file"), false, Some("diff closed"))),
        );
    }

    #[test]
    fn a_widen_toggle_on_the_diff_closes_it() {
        let mut h = half_staged_harness();
        run(&mut h, &stoat_action::DiffFiles { rev: None });

        run(&mut h, &stoat_action::TogglePaneWiden);

        assert_eq!(layout(&h), (1, Some("file"), false, Some("diff closed")));
    }

    /// A focus move on a pair diff closes the view and drops the pair, as the
    /// bare `:diff-files` does.
    #[test]
    fn a_focus_move_on_a_pair_diff_drops_the_pair() {
        let mut h = pair_harness();
        run(&mut h, &stoat_action::DiffPair);

        run(&mut h, &stoat_action::FocusLeft);

        assert_eq!(
            (
                pair_base(&h, "/pair/b.txt"),
                pair_view(&mut h),
                h.stoat.pending_message.as_deref(),
            ),
            (
                None,
                (false, false, Some(PathBuf::from("/pair/b.txt"))),
                Some("diff closed"),
            ),
        );
    }

    /// Opening a diff turns off a diff that another pane of the tab still
    /// shows, whether the open toggles or latches, so the tab holds one diff.
    #[test]
    fn opening_a_diff_turns_off_the_diff_beside_it() {
        let toggle: fn(&mut TestHarness) = |h| run(h, &stoat_action::DiffFiles { rev: None });
        let latch: fn(&mut TestHarness) = |h| {
            super::latch_diff_view(&mut h.stoat);
            h.settle();
        };
        for (name, open) in [("toggle", toggle), ("latch", latch)] {
            let mut h = half_staged_harness();
            let left = h.stoat.active_workspace().panes.focus();
            let (left_editor, _) = h.stoat.focused_editor_ids().expect("editor");
            run(&mut h, &stoat_action::SplitRight);
            let (right_editor, _) = h.stoat.focused_editor_ids().expect("editor");

            {
                let ws = h.stoat.active_workspace_mut();
                ws.panes.pane_mut(left).diff_mode = true;
                ws.editors[left_editor].set_diff_view(true);
            }

            open(&mut h);

            let ws = h.stoat.active_workspace();
            assert_eq!(
                (
                    ws.panes.pane(left).diff_mode,
                    ws.editors[left_editor].diff_view,
                    ws.editors[right_editor].diff_view,
                ),
                (false, false, true),
                "after the {name}, only the right pane shows a diff",
            );
        }
    }

    /// The pane count, the screen in front, whether a pane is widened, and the
    /// status message, which together show what a layout action did.
    fn layout(h: &TestHarness) -> (usize, Option<&'static str>, bool, Option<&str>) {
        let panes = &h.stoat.active_workspace().panes;
        (
            panes.split_pane_ids().len(),
            h.stoat.current_view(),
            panes.widened().is_some(),
            h.stoat.pending_message.as_deref(),
        )
    }

    #[test]
    fn diff_pair_over_equal_files_says_so_and_stays_put() {
        let mut h = TestHarness::with_size(120, 20);
        h.stage_review_scenario("/repo", &[("c.txt", "1\n", "2\n")]);
        h.open_side_by_side(("/repo/a.txt", "same\n"), ("/repo/b.txt", "same\n"));
        run(&mut h, &stoat_action::DiffPair);

        let view = pair_view(&mut h);
        assert_eq!(
            (
                h.stoat.pending_changed_file_jump.is_none(),
                h.stoat.pending_message.as_deref(),
                view,
            ),
            (
                true,
                Some("the two files do not differ"),
                (true, true, Some(PathBuf::from("/repo/b.txt"))),
            ),
            "no hop into the repo's changed file",
        );
    }

    /// A reader who leaves the measured file before its hunks land gets no
    /// verdict about the file they left.
    ///
    /// The focus moves as a click into the other pane moves it, because a focus
    /// key on the diff closes the diff instead.
    #[test]
    fn the_verdict_drops_once_the_focus_moves_on() {
        let mut h = TestHarness::with_size(120, 20);
        h.open_side_by_side(("/pair/a.txt", "same\n"), ("/pair/b.txt", "same\n"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffPair);
        {
            let panes = &mut h.stoat.active_workspace_mut().panes;
            let measured = panes.focus();
            let left = panes
                .split_pane_ids()
                .into_iter()
                .find(|&id| id != measured)
                .expect("the left pane");
            panes.set_focus(left);
        }
        h.settle();

        assert_eq!(h.stoat.pending_message.as_deref(), None);
    }

    /// A focus key closes a pair diff while its hunks are on the pool, so no
    /// verdict and no change walk reports on the closed pair.
    #[test]
    fn a_pair_diff_closed_before_its_hunks_land_gets_no_verdict() {
        let mut h = TestHarness::with_size(120, 20);
        h.open_side_by_side(("/pair/a.txt", "same\n"), ("/pair/b.txt", "same\n"));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffPair);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusLeft);
        let pending = want_state(&mut h, "/pair/b.txt").1;
        h.settle();

        assert_eq!(
            (pending, h.stoat.pending_message.as_deref()),
            (true, Some("diff closed")),
        );
    }

    /// The git diff on the measured file before the press shows changes, and
    /// the pair's own hunks show none, so the verdict waits for the pair's.
    #[test]
    fn equal_files_say_so_over_a_file_with_a_git_diff() {
        let mut h = TestHarness::with_size(120, 20);
        h.stage_review_scenario("/repo", &[("b.txt", "zero\n", "same\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_side_by_side(("/repo/a.txt", "same\n"), ("/repo/b.txt", "same\n"));
        h.settle_diff_jobs();
        run(&mut h, &stoat_action::DiffPair);

        let view = pair_view(&mut h);
        assert_eq!(
            (h.stoat.pending_message.as_deref(), view),
            (
                Some("the two files do not differ"),
                (true, true, Some(PathBuf::from("/repo/b.txt"))),
            ),
        );
    }

    #[test]
    fn the_change_walk_ends_inside_a_pair() {
        let mut h = repo_pair_harness();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextChange);

        let path = pair_view(&mut h).2;
        assert_eq!(
            (
                h.stoat.pending_changed_file_jump.is_none(),
                h.stoat.pending_message.as_deref(),
                path,
            ),
            (
                true,
                Some("no more changes"),
                Some(PathBuf::from("/repo/b.txt"))
            ),
        );
    }

    /// A git diff in another tab is no part of the pair, so closing it leaves
    /// the pair and its view as they were.
    #[test]
    fn a_git_diff_closed_in_another_tab_leaves_the_pair() {
        let mut h = repo_pair_harness();
        run(&mut h, &stoat_action::NewTab);
        h.open_file(Path::new("/repo/c.txt"));
        run(&mut h, &stoat_action::DiffFiles { rev: None });
        run(&mut h, &stoat_action::DiffFiles { rev: None });
        run(&mut h, &stoat_action::PrevTab);

        assert_eq!(
            (pair_base(&h, "/repo/b.txt"), pair_view(&mut h)),
            (
                Some(("/repo/a.txt".into(), "one\ntwo\n".into())),
                (true, true, Some(PathBuf::from("/repo/b.txt"))),
            ),
        );
    }

    #[test]
    fn two_pairs_in_two_tabs_coexist() {
        let h = two_tab_pairs();
        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                pair_base(&h, "/repo/b.txt"),
                pair_base(&h, "/repo/d.txt"),
                [ws.tab_title(0), ws.tab_title(1)],
            ),
            (
                Some(("/repo/a.txt".into(), "one\ntwo\n".into())),
                Some(("/repo/c2.txt".into(), "x\ny\n".into())),
                ["a.txt → b.txt".to_string(), "c2.txt → d.txt".to_string()],
            ),
            "each tab measures and names its own pair",
        );
    }

    #[test]
    fn closing_a_pair_view_drops_only_its_own_pair() {
        let mut h = two_tab_pairs();
        h.stoat.set_diff_warm_auto(true);
        run(&mut h, &stoat_action::DiffPair);
        h.settle_diff_jobs();

        let (editor, _) = h.stoat.focused_editor_ids().expect("editor");
        assert_eq!(
            (
                pair_base(&h, "/repo/b.txt"),
                pair_base(&h, "/repo/d.txt"),
                editor_shows_changes(&mut h.stoat, editor),
                diff_base_at(&h, "/repo/d.txt"),
            ),
            (
                Some(("/repo/a.txt".into(), "one\ntwo\n".into())),
                None,
                true,
                Some(String::new()),
            ),
            "d.txt diffs against the repo again, which does not track it, so every line reads \
             as added",
        );
    }

    /// Every diff command over a paired file returns the file to the base the
    /// command names, or to the workspace's base for the bare close. The base
    /// edit seeds a bare `diff-files `, since a pair has no revision to
    /// retype.
    #[test]
    fn diff_commands_over_a_paired_file_drop_its_pair() {
        let paired = || {
            let (mut h, _) = diff_rev_harness();
            h.open_side_by_side(("/repo/a.txt", "one\n"), ("/repo/b.txt", "one\ntwo\n"));
            run(&mut h, &stoat_action::DiffPair);
            h
        };
        let after = |action: &dyn Action| {
            let mut h = paired();
            run(&mut h, action);
            pair_base(&h, "/repo/b.txt")
        };
        let named = |rev: &str| stoat_action::DiffFiles {
            rev: Some(rev.to_string()),
        };

        assert_eq!(
            (
                pair_base(&paired(), "/repo/b.txt"),
                base_edit_seed(&mut paired()),
                [
                    after(&stoat_action::DiffFiles { rev: None }),
                    after(&named("HEAD")),
                    after(&named("index")),
                    after(&named("base0")),
                    after(&stoat_action::DiffBaseToggle),
                ],
            ),
            (
                Some(("/repo/a.txt".into(), "one\n".into())),
                Some("diff-files ".to_string()),
                [None, None, None, None, None],
            ),
        );
    }

    /// `/repo/b.txt` diffed against `/repo/a.txt`, in a repo whose own change
    /// is `c.txt`.
    fn repo_pair_harness() -> TestHarness {
        let mut h = TestHarness::with_size(120, 20);
        h.stage_review_scenario("/repo", &[("c.txt", "1\n", "2\n")]);
        h.open_side_by_side(("/repo/a.txt", "one\ntwo\n"), ("/repo/b.txt", "one\nTWO\n"));
        run(&mut h, &stoat_action::DiffPair);
        h
    }

    /// [`repo_pair_harness`] with a second tab, active, that diffs
    /// `/repo/d.txt` against `/repo/c2.txt`.
    fn two_tab_pairs() -> TestHarness {
        let mut h = repo_pair_harness();
        run(&mut h, &stoat_action::NewTab);
        h.open_side_by_side(("/repo/c2.txt", "x\ny\n"), ("/repo/d.txt", "x\nY\n"));
        run(&mut h, &stoat_action::DiffPair);
        h
    }

    #[test]
    fn diff_pair_without_a_pair_badges_and_changes_nothing() {
        let mut h = TestHarness::with_size(80, 20);
        run(&mut h, &stoat_action::DiffPair);

        let badge = h
            .stoat
            .active_workspace()
            .badges
            .find_by_source(BadgeSource::Review)
            .and_then(|id| h.stoat.active_workspace().badges.get(id))
            .map(|badge| badge.label.clone());
        assert_eq!(
            (
                badge.as_deref(),
                matches!(h.stoat.active_workspace().diff_base(), Some(DiffBase::Head))
            ),
            (Some("diff pair needs two files in two panes"), true),
        );
    }

    /// Two files side by side with the focus on the first, the base.
    fn pair_harness() -> TestHarness {
        let mut h = TestHarness::with_size(120, 20);
        h.open_side_by_side(
            ("/pair/a.txt", "one\ntwo\nthree\n"),
            ("/pair/b.txt", "one\nTWO\nthree\n"),
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusLeft);
        h
    }

    /// The `(base_path, text)` of the pair `path` measures against.
    fn pair_base(h: &TestHarness, path: &str) -> Option<(String, String)> {
        let pair = h.stoat.active_workspace().pair_for(Path::new(path))?;
        Some((pair.base_path.display().to_string(), pair.text.to_string()))
    }

    /// The focused editor's diff flag, whether the focused pane is widened, and
    /// the focused buffer's path.
    fn pair_view(h: &mut TestHarness) -> (bool, bool, Option<PathBuf>) {
        let diff_view = crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .is_some_and(|editor| editor.diff_view);
        let ws = h.stoat.active_workspace();
        let widened = ws.panes.widened() == Some(ws.panes.focus());
        let path = h
            .stoat
            .focused_editor_ids()
            .and_then(|(_, buffer_id)| ws.buffers.path_for(buffer_id).map(Path::to_path_buf));
        (diff_view, widened, path)
    }

    /// The bare command closes the diff, and a base a revision installed goes
    /// with it. Leaving it behind would have every later diff silently measured
    /// against a commit the reader thought they had closed.
    #[test]
    fn a_bare_diff_clears_the_base_a_rev_installed() {
        let (mut h, _) = diff_rev_harness();
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::DiffFiles {
                rev: Some("base0".to_string()),
            },
        );
        h.settle();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
        h.settle();

        let (base, latched, _) = diff_state(&h);
        assert_eq!((base, latched), (None, false));
    }

    /// Naming a revision asks to look at it, so running it again re-targets the
    /// view rather than closing it. Only the bare command closes, which is what
    /// keeps `:diff-files <rev>` from being a toggle whose meaning depends on
    /// what the reader last did.
    #[test]
    fn a_repeated_diff_rev_keeps_the_view_open() {
        let (mut h, workdir) = diff_rev_harness();
        let open = |h: &mut TestHarness| {
            crate::action_handlers::dispatch(
                &mut h.stoat,
                &stoat_action::DiffFiles {
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
            &stoat_action::DiffFiles {
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
            &stoat_action::DiffFiles {
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

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
        h.settle();

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

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

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

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

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

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

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

    /// A block that the buffer wraps in a new `if`, which reindents every line.
    const REINDENT_BASE: &str = "fn f() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n}\n";

    /// [`REINDENT_BASE`] wrapped in `if x { .. }`, with `c` set to 4. The line
    /// pass sees rows 1 to 5 changed, and the tree pass marks two runs: the
    /// `if x` on row 1, and the `4` with the closing brace on rows 4 and 5.
    const REINDENT_BUFFER: &str =
        "fn f() {\n    if x {\n        let a = 1;\n        let b = 2;\n        let c = 4;\n    }\n}\n";

    /// The `if x {` of [`REINDENT_BUFFER`] staged on its own, as an insertion
    /// above the index's unchanged `let a = 1;`.
    const REINDENT_IF_STAGE: &str = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,5 +1,6 @@\n fn f() {\n+    if x {\n     let a = 1;\n     let b = 2;\n     let c = 3;\n }\n";

    /// The stop that the `if x {` of [`REINDENT_BUFFER`] opens, rows 1 to 3,
    /// staged on its own. The stop reaches the second run, so the reindented
    /// `let` rows move with the `if x {`.
    const REINDENT_IF_STOP_STAGE: &str = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,5 +1,6 @@\n fn f() {\n+    if x {\n-    let a = 1;\n+        let a = 1;\n-    let b = 2;\n+        let b = 2;\n     let c = 3;\n }\n";

    /// The second run of [`REINDENT_BUFFER`], rows 4 and 5, staged on its own.
    /// Each row carries its new indent, since a staged row is the whole buffer
    /// line.
    const REINDENT_TAIL_STAGE: &str = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,5 +1,6 @@\n fn f() {\n     let a = 1;\n     let b = 2;\n-    let c = 3;\n+        let c = 4;\n+    }\n }\n";

    /// Open `file` holding `buffer` over a HEAD of `base`, with its diff jobs
    /// settled so the tree pass has marked its runs, cursor on `row`.
    fn open_settled_at(
        h: &mut TestHarness,
        file: &str,
        base: &str,
        buffer: &str,
        row: u32,
    ) -> PathBuf {
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[(file, base, buffer)]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(&workdir.join(file));
        h.settle_diff_jobs();
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, row);
        workdir
    }

    /// Open [`REINDENT_BUFFER`] over [`REINDENT_BASE`], cursor on `row`.
    fn open_reindent_at(h: &mut TestHarness, row: u32) -> PathBuf {
        open_settled_at(h, "a.rs", REINDENT_BASE, REINDENT_BUFFER, row)
    }

    /// A reindent pairs each moved line with its old self, so the `if x {` it
    /// added stages as an insertion rather than over `let a = 1;`.
    #[test]
    fn stage_line_on_a_line_a_reindent_added_inserts_it() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_reindent_at(&mut h, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);

        assert_eq!(h.fake_git().applied_patches(&workdir), [REINDENT_IF_STAGE]);
    }

    /// The gutter marks only the runs that hold an edit of their own, but a
    /// stop reaches from its run to the next. So `s` on the first run moves
    /// the reindented lines below it too, and leaves the second run unstaged.
    #[test]
    fn staging_a_narrowed_hunk_moves_the_stop_under_the_cursor() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_reindent_at(&mut h, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
        h.settle();

        assert_eq!(
            (
                h.fake_git().applied_patches(&workdir),
                h.stoat.pending_message.as_deref(),
            ),
            (
                vec![REINDENT_IF_STOP_STAGE.to_string()],
                Some("staged hunk")
            ),
            "the stop moves alone, and the status names the hunk keys pressed",
        );
    }

    /// A failed index write reports why, and claims no stage.
    #[test]
    fn a_run_whose_index_write_fails_reports_why() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_reindent_at(&mut h, 1);
        h.fake_git()
            .add_repo(&workdir)
            .fail_apply_with("index locked");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
        h.settle();

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("could not update staging: git backend failure: index locked"),
        );
    }

    /// The second run spans two rows, and a cursor on its last row moves the
    /// whole run.
    #[test]
    fn staging_the_other_run_leaves_the_first_alone() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_reindent_at(&mut h, 5);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

        assert_eq!(
            h.fake_git().applied_patches(&workdir),
            [REINDENT_TAIL_STAGE]
        );
    }

    /// Rows that meet two hunks move as one patch per hunk, last hunk first, so
    /// a patch applied in order never shifts the lines a later one names.
    #[test]
    fn rows_over_two_hunks_patch_the_last_hunk_first() {
        const HEAD: &str = "a\nb\nc\nd\ne\nf\ng\nh\n";
        const EDITED: &str = "a\nZ\nc\nd\ne\nf\nY\nh\n";
        let patch =
            |hunk: &str| format!("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n{hunk}");
        let tracked = StagingTexts {
            head: Some(HEAD.to_string()),
            index: HEAD.to_string(),
            in_index: true,
        };

        assert_eq!(
            (
                stage_rows_patch("a.rs", &tracked, EDITED, 1..7),
                unstage_rows_patch("a.rs", EDITED, Some(HEAD), EDITED, 1..7),
            ),
            (
                vec![
                    patch("@@ -4,5 +4,5 @@\n d\n e\n f\n-g\n+Y\n h\n"),
                    patch("@@ -1,5 +1,5 @@\n a\n-b\n+Z\n c\n d\n e\n"),
                ],
                vec![
                    patch("@@ -4,5 +4,5 @@\n d\n e\n f\n-Y\n+g\n h\n"),
                    patch("@@ -1,5 +1,5 @@\n a\n-Z\n+b\n c\n d\n e\n"),
                ],
            ),
        );
    }

    /// The stop that the `if x {` of a staged [`REINDENT_BUFFER`] opens, taken
    /// back out of the index. The reindented `let` rows go back to their old
    /// indent with it, which leaves the second run staged.
    const REINDENT_IF_STOP_UNSTAGE: &str = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,7 +1,6 @@\n fn f() {\n-    if x {\n-        let a = 1;\n+    let a = 1;\n-        let b = 2;\n+    let b = 2;\n         let c = 4;\n     }\n }\n";

    /// Under the HEAD base a staged reindent shows as a narrowed hunk, so
    /// `ToggleStageHunk` on its first run unstages that run's stop alone.
    #[test]
    fn unstaging_a_narrowed_hunk_moves_the_stop_under_the_cursor() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_index_scenario(
            &workdir,
            &[("a.rs", REINDENT_BASE, REINDENT_BUFFER, REINDENT_BUFFER)],
        );
        h.stoat.set_diff_warm_auto(true);
        h.open_file(&workdir.join("a.rs"));
        run(&mut h, &stoat_action::DiffAgainstHead);
        h.settle_diff_jobs();
        {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            crate::action_handlers::movement::set_cursor_row(editor, 1);
        }

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
        h.settle();

        assert_eq!(
            (
                h.fake_git().applied_patches(&workdir),
                h.stoat.pending_message.as_deref(),
            ),
            (
                vec![REINDENT_IF_STOP_UNSTAGE.to_string()],
                Some("unstaged hunk")
            ),
        );
    }

    /// Under the index base, the index's rename of `f` to `g` is a staged mark
    /// on the row above the hunk. Deleting the first line moves the anchored
    /// hunk up onto the mark's row before the next diff lands. A press there
    /// still stages the hunk's first stop, not the one row the mark covers.
    #[test]
    fn a_stale_staged_mark_leaves_a_press_the_stop_under_it() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        let renamed = |text: &str| format!("// top\n{}", text.replace("fn f", "fn g"));
        h.stage_index_scenario(
            &workdir,
            &[(
                "a.rs",
                &format!("// top\n{REINDENT_BASE}"),
                &renamed(REINDENT_BASE),
                &renamed(REINDENT_BUFFER),
            )],
        );
        h.stoat.set_diff_warm_auto(true);
        h.open_file(&workdir.join("a.rs"));
        run(&mut h, &stoat_action::DiffAgainstIndex);
        h.settle_diff_jobs();
        h.edit_focused(0.."// top\n".len(), "");
        {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            crate::action_handlers::movement::set_cursor_row(editor, 1);
        }

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
        h.settle();

        assert_eq!(
            h.fake_git().applied_patches(&workdir),
            ["diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -2,5 +2,6 @@\n fn g() {\n+    if x {\n-    let a = 1;\n+        let a = 1;\n-    let b = 2;\n+        let b = 2;\n     let c = 3;\n }\n"],
        );
    }

    /// A text file gets no tree pass, so its hunk is never narrowed and `s`
    /// moves all of it, the removed `c` included. A stage narrowed to the
    /// cursor's row keeps `c` in the index.
    #[test]
    fn an_unnarrowed_hunk_still_stages_whole() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_settled_at(&mut h, "a.txt", "a\nb\nc\nd\n", "a\nX\nd\n", 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

        assert_eq!(
            h.fake_git().applied_patches(&workdir),
            ["diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,4 +1,3 @@\n a\n-b\n+X\n-c\n d\n"],
        );
    }

    /// HEAD of a file whose first hunk turns `b` into two lines.
    const SHIFT_HEAD: &str = "a\nb\nc\nd\ne\nf\ng\nh\n";

    /// The buffer over [`SHIFT_HEAD`]. Each row below the first hunk sits one
    /// row lower than in HEAD, and row 7 holds the second hunk's `Y`.
    const SHIFT_BUFFER: &str = "a\nZ1\nZ2\nc\nd\ne\nf\nY\nh\n";

    /// The second hunk staged into an index that lacks the first. Both starts
    /// name index row 4, where the context opens.
    const SECOND_HUNK_STAGE: &str =
        "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -4,5 +4,5 @@\n d\n e\n f\n-g\n+Y\n h\n";

    /// The second hunk unstaged from an index that holds both. The first hunk
    /// moved it down a row, so both starts name index row 5.
    const SECOND_HUNK_UNSTAGE: &str =
        "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -5,5 +5,5 @@\n d\n e\n f\n-Y\n+g\n h\n";

    /// Press `action` on the second hunk of [`SHIFT_BUFFER`] over an index that
    /// holds `index`, and return the one patch the press applied.
    fn press_on_the_second_hunk(index: &str, action: &dyn Action) -> String {
        press_with_cursor(index, action, |h| {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            crate::action_handlers::movement::set_cursor_row(editor, 7);
        })
    }

    /// Press `action` in [`SHIFT_BUFFER`] over an index that holds `index`, with
    /// the cursor `place` puts, and return the one patch the press applied.
    fn press_with_cursor(
        index: &str,
        action: &dyn Action,
        place: impl FnOnce(&mut TestHarness),
    ) -> String {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_index_scenario(&workdir, &[("a.rs", SHIFT_HEAD, index, SHIFT_BUFFER)]);
        h.open_file(&workdir.join("a.rs"));
        place(&mut h);

        crate::action_handlers::dispatch(&mut h.stoat, action);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
        patches[0].clone()
    }

    /// libgit2 looks for a hunk exactly at its `+` start. With the first hunk
    /// out of the index, the context opens at index row 4 but buffer row 5, and
    /// the header names the index row on both sides.
    #[test]
    fn a_line_below_an_unstaged_hunk_stages_at_its_index_row() {
        assert_eq!(
            press_on_the_second_hunk(SHIFT_HEAD, &stoat_action::ToggleStageLine),
            SECOND_HUNK_STAGE
        );
    }

    /// The hunk key reads the index as the line key does, so it stages the same
    /// patch.
    #[test]
    fn a_hunk_below_an_unstaged_hunk_stages_at_its_index_row() {
        assert_eq!(
            press_on_the_second_hunk(SHIFT_HEAD, &stoat_action::ToggleStageHunk),
            SECOND_HUNK_STAGE
        );
    }

    /// With the first hunk staged, the context opens at index row 5 as in the
    /// buffer, one row lower than in HEAD. A patch diffed from HEAD names row 4
    /// and misses it.
    #[test]
    fn a_hunk_below_a_staged_hunk_stages_at_its_index_row() {
        assert_eq!(
            press_on_the_second_hunk(
                "a\nZ1\nZ2\nc\nd\ne\nf\ng\nh\n",
                &stoat_action::ToggleStageHunk
            ),
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -5,5 +5,5 @@\n d\n e\n f\n-g\n+Y\n h\n",
        );
    }

    /// Escape after typing a line, `g l`, and `A` leave the block cursor on the
    /// line's newline cell. Its head is the start of the next line, and the
    /// press stages the line the cursor is drawn on.
    #[test]
    fn a_cursor_on_a_lines_last_cell_stages_that_line() {
        let patch = press_with_cursor(SHIFT_HEAD, &stoat_action::ToggleStageHunk, |h| {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            let snapshot = editor.display_map.snapshot();
            let bs = snapshot.buffer_snapshot();
            let offset = bs.rope().point_to_offset(Point::new(7, 1));
            editor.selections.set_block_cursor(offset, bs);
        });
        assert_eq!(patch, SECOND_HUNK_STAGE);
    }

    /// A line selected with `x` heads at the start of the next line, and the
    /// press stages the selected line.
    #[test]
    fn a_line_selection_stages_the_selected_line() {
        let patch = press_with_cursor(SHIFT_HEAD, &stoat_action::ToggleStageLine, |h| {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            crate::action_handlers::movement::set_cursor_row(editor, 7);
            h.type_keys("x");
        });
        assert_eq!(patch, SECOND_HUNK_STAGE);
    }

    #[test]
    fn a_hunk_below_a_staged_hunk_unstages_at_its_index_row() {
        assert_eq!(
            press_on_the_second_hunk(SHIFT_BUFFER, &stoat_action::ToggleStageHunk),
            SECOND_HUNK_UNSTAGE
        );
    }

    #[test]
    fn hunks_around_picks_the_neighbors_of_the_row() {
        let compared = [0..2, 5..7, 9..9];
        assert_eq!(
            [6, 7, 0, 9, 20].map(|row| hunks_around(row, &compared)),
            [0..2, 1..3, 0..1, 1..3, 2..3],
            "a row inside, just past, before, at an empty range, and past the last"
        );
        assert_eq!(hunks_around(0, &[]), 0..0, "no ranges, no neighbors");
    }

    #[test]
    fn toggle_stage_hunk_unstages_a_staged_hunk() {
        assert_eq!(
            press_on_the_second_hunk(SHIFT_BUFFER, &stoat_action::ToggleStageHunk),
            SECOND_HUNK_UNSTAGE
        );
    }

    /// A cursor on a plain context row stages nothing. The gutter is empty
    /// there, so reaching for the nearest hunk would stage a unit the user
    /// cannot see under the cursor.
    #[test]
    fn a_cursor_off_every_hunk_stages_nothing() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_two_hunk_file_at(&mut h, 3);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
        h.settle();

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

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

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

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

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

    /// The change walk lands a deletion on the row the staging keys read it
    /// from, so a stage press right after the walk takes the deletion.
    #[test]
    fn walking_to_a_deletion_then_staging_removes_it() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nGONE\nc\nd\n", "a\nb\nc\nd\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(&workdir.join("a.rs"));
        h.settle_diff_jobs();
        {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            crate::action_handlers::movement::set_cursor_row(editor, 0);
        }

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextChange);
        assert_eq!(
            crate::test_harness::editor::focused_cursor_point(&mut h.stoat).row,
            2,
            "the walk lands on the row after the removed line"
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
        assert!(
            patches[0].contains("-GONE\n"),
            "removes the base line: {}",
            patches[0]
        );
    }

    #[test]
    fn stage_hunk_applies_the_forward_patch_for_the_cursor_hunk() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_git_file_at_cursor(&mut h, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

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

    /// A press on a hunk the index lacks stages it, and a press once the index
    /// holds it unstages it, so two presses send the forward patch and then its
    /// reverse.
    #[test]
    fn a_second_toggle_on_a_staged_hunk_sends_the_reverse_patch() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_git_file_at_cursor(&mut h, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
        h.settle();
        h.fake_git()
            .add_repo(&workdir)
            .index_file("a.rs", "a\nb\nX\nd\n");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

        let patch = |from: &str, to: &str| {
            format!(
                "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n\
                 @@ -1,4 +1,4 @@\n a\n b\n-{from}\n+{to}\n d\n"
            )
        };
        assert_eq!(
            h.fake_git().applied_patches(&workdir),
            [patch("c", "X"), patch("X", "c")],
        );
    }

    /// A line with a staged edit and an unstaged edit on top of it holds both
    /// a forward and a reverse patch. Each toggle prefers the forward one, so a
    /// partly staged hunk or line stages its rest.
    #[test]
    fn a_partly_staged_change_stages_its_rest() {
        const FORWARD: &str =
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,3 @@\n a\n-B\n+BB\n c\n";
        let pressed = |action: &dyn Action| {
            let mut h = TestHarness::with_size(80, 14);
            h.stage_index_scenario("/repo", &[("a.rs", "a\nb\nc\n", "a\nB\nc\n", "a\nBB\nc\n")]);
            h.open_file(Path::new("/repo/a.rs"));
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            crate::action_handlers::movement::set_cursor_row(editor, 1);
            crate::action_handlers::dispatch(&mut h.stoat, action);
            h.fake_git().applied_patches(Path::new("/repo"))
        };

        assert_eq!(
            [
                pressed(&stoat_action::ToggleStageHunk),
                pressed(&stoat_action::ToggleStageLine),
            ],
            [[FORWARD], [FORWARD]],
        );
    }

    /// The tree pass marks the changed call alone, so the blank row added below
    /// it lies in no run. The stop the press stages covers it, so staging each
    /// stop leaves nothing unstaged.
    #[test]
    fn stage_hunk_takes_the_blank_row_its_stop_covers() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_settled_at(
            &mut h,
            "a.rs",
            "fn a() {\n    x();\n}\n",
            "fn a() {\n    y();\n\n}\n",
            1,
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

        assert_eq!(
            h.fake_git().applied_patches(&workdir),
            ["diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,4 @@\n fn a() {\n-    x();\n+    y();\n+\n }\n"],
        );
    }

    /// A press hands the diff and the index write to the git queue. The test
    /// scheduler runs that work inline, so the patch is in at the press, and
    /// the status waits for the landing.
    #[test]
    fn a_stage_press_hands_the_index_write_to_a_worker() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_git_file_at_cursor(&mut h, 2);
        let before = h.blocking_calls();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
        assert_eq!(
            (
                h.blocking_calls() - before,
                h.fake_git().applied_patches(&workdir).len(),
                h.stoat.pending_message.as_deref()
            ),
            (1, 1, None),
            "the write ran as one blocking job, and nothing landed yet",
        );

        h.settle();
        assert_eq!(h.stoat.pending_message.as_deref(), Some("staged hunk"));
    }

    /// A patch that creates `rel` in the index with `hunk`.
    fn creating_patch(rel: &str, hunk: &str) -> String {
        format!(
            "diff --git a/{rel} b/{rel}\nnew file mode 100644\n--- /dev/null\n+++ b/{rel}\n{hunk}"
        )
    }

    /// Presses `action` on row 0 of untracked `name`, which holds `x` and `y`
    /// beside a repo's one tracked change, and answers the patches the press
    /// applied and the status it set.
    fn press_on_untracked(name: &str, action: &dyn Action) -> (Vec<String>, Option<String>) {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/repo");
        h.stage_review_scenario(&workdir, &[("t.rs", "a\n", "b\n")]);
        h.fake_fs().insert_file(workdir.join(name), b"x\ny\n");
        h.fake_git().add_repo(&workdir).untracked(name);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(&workdir.join(name));
        h.settle_diff_jobs();

        run(&mut h, action);
        (
            h.fake_git().applied_patches(&workdir),
            h.stoat.pending_message.clone(),
        )
    }

    /// No blob holds an untracked file, so its hunk stages as a patch that
    /// creates the file in the index. The tree pass narrows a Rust file's
    /// added hunk to a run, which stages through the rows path, while a file
    /// with no language stages its whole hunk.
    #[test]
    fn staging_an_untracked_file_s_hunk_adds_it_to_the_index() {
        let whole = "@@ -0,0 +1,2 @@\n+x\n+y\n";
        assert_eq!(
            [
                press_on_untracked("fresh.rs", &stoat_action::ToggleStageHunk),
                press_on_untracked("fresh.txt", &stoat_action::ToggleStageHunk),
            ],
            [
                (
                    vec![creating_patch("fresh.rs", whole)],
                    Some("staged hunk".to_string())
                ),
                (
                    vec![creating_patch("fresh.txt", whole)],
                    Some("staged hunk".to_string())
                ),
            ],
        );
    }

    /// A line press creates the index entry with that line alone, which leaves
    /// the rest of the file an unstaged hunk.
    #[test]
    fn staging_an_untracked_file_s_line_adds_that_line_alone() {
        assert_eq!(
            press_on_untracked("fresh.rs", &stoat_action::ToggleStageLine),
            (
                vec![creating_patch("fresh.rs", "@@ -0,0 +1,1 @@\n+x\n")],
                Some("staged line".to_string())
            ),
        );
    }

    #[test]
    fn unstage_hunk_reverts_the_staged_hunk_to_head() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_index_scenario(
            &workdir,
            &[("a.rs", "a\nb\nc\nd\n", "a\nb\nX\nd\n", "a\nb\nX\nd\n")],
        );
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "exactly one patch applied: {patches:?}");
        let patch = &patches[0];
        assert!(patch.contains("-X\n"), "removes the staged line: {patch}");
        assert!(patch.contains("+c\n"), "restores the HEAD line: {patch}");
    }

    /// Open `rel` under `/work`, press `action` with the cursor on `row`, and
    /// return every patch the press applied.
    fn press_in(h: &mut TestHarness, rel: &str, row: u32, action: &dyn Action) -> Vec<String> {
        let workdir = PathBuf::from("/work");
        h.open_file(&workdir.join(rel));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, row);

        crate::action_handlers::dispatch(&mut h.stoat, action);
        h.fake_git().applied_patches(&workdir)
    }

    /// `git add` leaves a new file in the index with no HEAD blob. Its hunks
    /// stage against the index text alone.
    #[test]
    fn a_file_only_the_index_holds_stages_its_hunk() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario_with_staged("/work", &[], &[("new.rs", "a\nb\n")]);
        h.fake_git().add_repo("/work").index_file("new.rs", "a\n");

        assert_eq!(
            press_in(&mut h, "new.rs", 1, &stoat_action::ToggleStageHunk),
            ["diff --git a/new.rs b/new.rs\n--- a/new.rs\n+++ b/new.rs\n@@ -1,1 +1,2 @@\n a\n+b\n"],
        );
    }

    /// HEAD holds the file empty, so an unstage that empties the index text
    /// writes an empty blob rather than removing the entry.
    #[test]
    fn unstaging_all_of_a_file_committed_empty_keeps_its_entry() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_index_scenario("/work", &[("a.rs", "", "x\n", "x\n")]);

        assert_eq!(
            press_in(&mut h, "a.rs", 0, &stoat_action::ToggleStageHunk),
            ["diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,1 +1,0 @@\n-x\n"],
        );
    }

    /// A removed file opens as a lone newline. The press stages the removal
    /// rather than a blob that holds one blank line.
    #[test]
    fn a_file_removed_from_disk_stages_its_deletion() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        h.fake_git()
            .add_repo("/work")
            .head_file("gone.rs", "a\nb\n");

        assert_eq!(
            press_in(&mut h, "gone.rs", 0, &stoat_action::ToggleStageHunk),
            ["diff --git a/gone.rs b/gone.rs\ndeleted file mode 100644\n--- a/gone.rs\n+++ /dev/null\n@@ -1,2 +1,0 @@\n-a\n-b\n"],
        );
    }

    /// An unstage takes a file HEAD does not hold out of the index only when no
    /// line of it stays staged, from the hunk key and the line key alike.
    #[test]
    fn unstaging_an_added_file_removes_it_only_when_no_line_stays() {
        let unstage = |index: &str, row: u32, action: &dyn Action| {
            let mut h = TestHarness::with_size(80, 14);
            h.stage_review_scenario_with_staged("/work", &[], &[("new.rs", index)]);
            h.fake_git().add_repo("/work").index_file("new.rs", index);
            press_in(&mut h, "new.rs", row, action)
        };
        let removal = "diff --git a/new.rs b/new.rs\ndeleted file mode 100644\n--- a/new.rs\n+++ /dev/null\n@@ -1,1 +1,0 @@\n-a\n";

        assert_eq!(
            (
                unstage("a\n", 0, &stoat_action::ToggleStageHunk),
                unstage("a\n", 0, &stoat_action::ToggleStageLine),
                unstage("a\nb\n", 1, &stoat_action::ToggleStageLine),
            ),
            (
                vec![removal.to_string()],
                vec![removal.to_string()],
                vec!["diff --git a/new.rs b/new.rs\n--- a/new.rs\n+++ b/new.rs\n@@ -1,2 +1,1 @@\n a\n-b\n".to_string()],
            ),
        );
    }

    /// A moved file has no HEAD blob at its own path. An unstage reverts its
    /// index lines to the text HEAD holds at the path it moved from.
    #[test]
    fn unstaging_in_a_moved_file_reverts_to_the_text_it_moved_from() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_rename_scenario("/work", "old.rs", "new.rs", "a\nb\n", "a\nB\n");
        h.fake_git()
            .add_repo("/work")
            .index_file("new.rs", "a\nB\n");

        assert_eq!(
            press_in(&mut h, "new.rs", 1, &stoat_action::ToggleStageHunk),
            ["diff --git a/new.rs b/new.rs\n--- a/new.rs\n+++ b/new.rs\n@@ -1,2 +1,2 @@\n a\n-B\n+b\n"],
        );
    }

    #[test]
    fn toggle_stage_hunk_stages_an_unstaged_hunk() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_git_file_at_cursor(&mut h, 2);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(
            patches.len(),
            1,
            "toggle stages the unstaged hunk: {patches:?}"
        );
        assert!(patches[0].contains("-c\n") && patches[0].contains("+X\n"));
    }

    #[test]
    fn stage_hunk_off_a_hunk_is_a_message_only_noop() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = open_git_file_at_cursor(&mut h, 0);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
        h.settle();

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

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);

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

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageHunk);
        h.settle();

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

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);
        h.settle();

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

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);
        h.settle();

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

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);
        h.settle();

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
            ws.set_diff_base(Some(DiffBase::parent_of("c1", Some("c0".to_string()))));
            ws.rebase_active = Some(paused_rebase(&workdir));
        }
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, 1);
        h
    }

    /// A base with nothing safe to rewrite under it gets the transport's own
    /// refusal rather than the line-granularity one, since the commit is out of
    /// reach for `s` as well.
    #[test]
    fn stage_line_reports_the_transport_refusal_below_the_tip() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_review_scenario(&workdir, &[("a.rs", "a\nb\nc\nd\n", "a\nB\nC\nd\n")]);
        h.fake_git().add_repo(&workdir).commit("c1", &[]);
        h.stoat
            .active_workspace_mut()
            .set_diff_base(Some(DiffBase::parent_of("c2", Some("c1".to_string()))));
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, 1);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);
        h.settle();

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
            branch: None,
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

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);

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

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);

        let patches = h.fake_git().applied_patches(&workdir);
        assert_eq!(patches.len(), 1, "one patch: {patches:?}");
        let patch = &patches[0];
        assert!(patch.contains("-B\n"), "reverts the staged line: {patch}");
        assert!(patch.contains("+b\n"), "restores the HEAD line: {patch}");
    }

    /// The blank line the buffer adds above the cursor is not in the index, so
    /// the cursor's index row sits one row above its buffer row.
    #[test]
    fn unstage_line_below_an_added_blank_line_reverts_its_own_row() {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_index_scenario(
            &workdir,
            &[("a.rs", "a\nb\nc\nd\n", "a\nb\nc\nD\n", "a\n\nb\nc\nD\n")],
        );
        h.open_file(&workdir.join("a.rs"));
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, 4);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);

        assert_eq!(
            h.fake_git().applied_patches(&workdir),
            ["diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,4 +1,4 @@\n a\n b\n c\n-D\n+d\n"],
        );
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

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);

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

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);

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

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleStageLine);
        h.settle();

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
        // The stage keys bind once the diff holds a hunk, as it does a moment
        // after a file opens.
        let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
        h.settle_diff_map(buffer_id);

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

    /// The patches `keys` applies with the cursor on the second hunk of
    /// [`SHIFT_BUFFER`], over an index that holds `index`.
    fn chord_on_the_second_hunk(index: &str, keys: &str) -> Vec<String> {
        let mut h = TestHarness::with_size(80, 14);
        let workdir = PathBuf::from("/work");
        h.stage_index_scenario(&workdir, &[("a.rs", SHIFT_HEAD, index, SHIFT_BUFFER)]);
        h.open_file(&workdir.join("a.rs"));
        let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
        h.settle_diff_map(buffer_id);
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        crate::action_handlers::movement::set_cursor_row(editor, 7);
        h.type_keys(keys);
        h.fake_git().applied_patches(&workdir)
    }

    #[test]
    fn space_capital_g_s_unstages_a_staged_hunk_from_a_plain_editor() {
        assert_eq!(
            chord_on_the_second_hunk(SHIFT_BUFFER, "space G s"),
            [SECOND_HUNK_UNSTAGE]
        );
    }

    #[test]
    fn space_capital_g_capital_s_toggles_the_line_from_a_plain_editor() {
        assert_eq!(
            (
                chord_on_the_second_hunk(SHIFT_HEAD, "space G S"),
                chord_on_the_second_hunk(SHIFT_BUFFER, "space G S")
            ),
            (
                vec![SECOND_HUNK_STAGE.to_string()],
                vec![SECOND_HUNK_UNSTAGE.to_string()]
            ),
            "the line key stages an unstaged line and unstages a staged one",
        );
    }

    /// A preview parses each file's base twice, once for the changeset diff
    /// and once for the left column's colors, which is 4 ms twice on a large
    /// file. One memo serves both.
    ///
    /// Two things have to hold. The base call must reach the memo, which an
    /// empty one filling by that call alone shows. The buffer call must not,
    /// since the diff files left sides only and a buffer tree there evicts a
    /// base the files behind it read.
    #[test]
    fn a_preview_parses_each_base_once() {
        let langs = LanguageRegistry::standard();
        let styles = SyntaxStyles::from_theme(&Theme::empty());
        let cache: BaseHighlightCache = Arc::new(Mutex::new(BaseHighlightMemo::default()));
        let memo_len =
            |doc: &DiffDocument| doc.tree_cache.lock().expect("tree memo poisoned").len();

        let changes = vec![
            (
                PathBuf::from("a.rs"),
                "fn a() {}\n".into(),
                "fn a() { 1 }\n".into(),
            ),
            (
                PathBuf::from("b.rs"),
                "fn b() {}\n".into(),
                "fn b() { 2 }\n".into(),
            ),
        ];
        let mut doc = build_document_from_changes(&langs, Path::new("/work"), changes)
            .expect("two changed files build a document");
        assert_eq!(memo_len(&doc), 2, "the diff files one base per file");

        // A fresh memo, so what the highlight pass files is what this counts.
        doc.tree_cache = TreeCache::default();
        attach_preview_highlights(&mut doc, &styles, &cache);
        assert_eq!(
            memo_len(&doc),
            2,
            "the base side goes through the memo and the buffer side does not",
        );
    }
}

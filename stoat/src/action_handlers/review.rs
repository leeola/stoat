use super::{
    amend::{self, AmendRoute, AmendUnit},
    movement::{ChangeDir, ChangeUnit},
};
use crate::{
    app::{Stoat, UpdateEffect},
    buffer::BufferId,
    buffer_registry::OpenOrigin,
    commit_list::Preview,
    diff_cache::{DiffCache, DiffCacheKey},
    diff_map::{hunk_base_lines, line_starts},
    display_map::{syntax_theme::SyntaxStyles, DisplaySnapshot, FoldEnd, FoldScope, ScopeMark},
    editor_state::EditorId,
    git_jobs::{self, GitJob, GitLanding, GitWork},
    host::GitRepo,
    jumplist::JumpEntry,
    multi_buffer::MultiBufferSnapshot,
    pane::{FocusTarget, PaneId, View},
    render::commits,
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
use stoat_text::{Rope, SelectionGoal};
use tracing::field;

/// Cache key matching what [`populate_diff_cache`] writes, so a scan reads back
/// the hunks a prior install stored for an unchanged file.
pub fn diff_cache_key(base: &str, buffer: &str, language: Option<&Arc<Language>>) -> DiffCacheKey {
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

/// Walk the commits view's selected commit, landing on the file its preview
/// shows.
///
/// A preview still loading names no file, so the walk lands on the commit's
/// first changed file, as it does for a commit with no preview at all.
pub(super) fn commits_open_review(stoat: &mut Stoat) -> UpdateEffect {
    let Some((workdir, commit, open)) =
        stoat
            .active_workspace()
            .focused_commits()
            .and_then(|state| {
                let commit = state.commits.get(state.selected)?;
                let open = match state.preview_sessions.get(&commit.sha) {
                    Preview::Built(document) => {
                        commits::previewed_file(document, state.preview_scroll)
                            .map(std::path::PathBuf::from)
                    },
                    Preview::Empty | Preview::Unbuilt => None,
                };
                Some((state.workdir.clone(), commit.clone(), open))
            })
    else {
        return UpdateEffect::None;
    };
    super::review_walk::walk_one_commit(stoat, workdir, commit, open)
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
/// On a changes view a revision, `index`, or `HEAD` changes the base and the
/// changes view stays. The bare command there opens the per-file diff.
///
/// An open conflict view closes first, so the diff shows the plain file that
/// the conflict view covered.
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
        leave_conflict_view(stoat);
        // The bare command also drops any base a revision left installed, so
        // closing the diff always returns to the working tree's own base.
        toggle_diff_view(stoat);
        return UpdateEffect::Redraw;
    };
    if rev.eq_ignore_ascii_case("index") {
        leave_conflict_view(stoat);
        return diff_against(stoat, WorktreeBase::Index);
    }
    if rev.eq_ignore_ascii_case("head") {
        leave_conflict_view(stoat);
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

    leave_conflict_view(stoat);
    drop_focused_pair(stoat);
    tracing::info!(target: "stoat::review", rev, sha = %sha, "diff base set");
    stoat
        .active_workspace_mut()
        .set_diff_base(Some(DiffBase::named(rev, sha)));
    // Turned on rather than toggled. Naming a revision asks to look at it, so a
    // second `:diff-files <rev>` re-targets the view rather than closing it;
    // only the bare command closes.
    show_installed_base(stoat);
    UpdateEffect::Redraw
}

/// Close an open conflict view, so [`diff`] shows the plain file and not the
/// view's scratch merged buffer.
///
/// [`diff`] calls it only once the command applies, because a close discards
/// the unwritten resolution and a refused revision changes nothing.
fn leave_conflict_view(stoat: &mut Stoat) {
    if stoat.active_workspace().conflict.is_some() {
        super::conflict_view::close_conflict(stoat);
    }
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
///
/// On a changes view the base changes and the changes view stays.
pub(super) fn diff_against(stoat: &mut Stoat, base: WorktreeBase) -> UpdateEffect {
    let paired = drop_focused_pair(stoat);
    let ws = stoat.active_workspace();
    let same = ws.worktree_base() == base
        && matches!(
            (ws.diff_base(), base),
            (None, WorktreeBase::Index) | (Some(DiffBase::Head), WorktreeBase::Head)
        );
    let changes = ws.focused_diff_changes_id().is_some();
    let view_on =
        changes || super::focused_editor_mut(stoat).is_some_and(|editor| editor.diff_view);
    if same && view_on && !paired {
        return UpdateEffect::Redraw;
    }

    if !same {
        stoat.active_workspace_mut().set_worktree_base(base);
    }
    // Turned on rather than toggled, as a named revision is.
    show_installed_base(stoat);
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

/// Show the diff over a base the caller installed, in the layout the reader
/// has.
///
/// A changes view stays, since its stack follows the base. Any other view
/// opens the per-file diff, as [`reopen_diff_view`] opens it.
fn show_installed_base(stoat: &mut Stoat) {
    if stoat.active_workspace().focused_diff_changes_id().is_some() {
        return;
    }
    reopen_diff_view(stoat);
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
pub fn latch_diff_view(stoat: &mut Stoat) {
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
pub fn enter_diff_view(stoat: &mut Stoat) {
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
pub fn exit_diff_view(stoat: &mut Stoat) -> bool {
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
/// On a focused changes view with a folded excerpt in focus it opens the
/// per-file diff at the cursor's hunk, the mirror of
/// [`stoat_action::DiffChanges`] on the diff. With no folded excerpt in focus
/// it closes the changes view and turns the diff on for the file the view
/// covered, so the key always leaves the changes view.
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
    if stoat.active_workspace().focused_diff_changes_id().is_some()
        && !super::diff_changes::close_for_covered_diff(stoat)
    {
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
/// status row says so, and the view crosses into the next changed file in path
/// order and lands on its first hunk.
pub(super) fn land_toggle_jump(stoat: &mut Stoat, origin: Option<JumpEntry>) {
    let diff_walk = super::movement::walks_diff_view(stoat);
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
        let (rows, holds) = super::movement::nearest_hunk_rows(
            buffer_snapshot.rope(),
            &hunk_stops,
            cursor_row,
            diff_walk,
        )?;
        if holds {
            return Some(false);
        }
        let target_offset =
            super::movement::stop_landing_cell(buffer_snapshot.rope(), rows, diff_walk).start;
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
    tracing::debug!(
        target: "stoat::change_walk",
        landing = match landing {
            Some(true) => "moved",
            Some(false) => "held",
            None => "none, crossing",
        },
        cursor_row = ?super::focused_editor_mut(stoat)
            .map(|editor| crate::diff_changes::cursor_point(editor).row),
        "diff toggle landing"
    );

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
    // the next changed file. This makes `:diff-files` from a scratch or
    // unchanged buffer open a real diff instead of silently toggling empty
    // columns.
    if landing.is_none() {
        let name = stoat
            .focused_editor_ids()
            .and_then(|(_, buffer_id)| stoat.active_workspace().buffers.path_for(buffer_id))
            .and_then(Path::file_name)
            .map_or_else(
                || "the buffer".to_owned(),
                |name| name.to_string_lossy().into_owned(),
            );
        stoat.set_status(format!("{name} has no changes"));
        let _ = super::movement::goto_change(stoat, ChangeDir::Next, ChangeUnit::Tree);
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
pub fn ensure_diff_map(stoat: &mut Stoat, buffer_id: BufferId, landing: Option<DiffLanding>) {
    let ws = stoat.active_workspace();
    let whole = landing.as_ref().is_some_and(DiffLanding::wants_whole_map);
    if ws.diff_map_current(buffer_id) || (!whole && ws.hunks_installed(buffer_id)) {
        if let Some(landing) = landing {
            tracing::debug!(
                target: "stoat::change_walk",
                ?buffer_id,
                landing = landing.name(),
                "diff landing lands now"
            );
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
    let name = landing.as_ref().map(DiffLanding::name);
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
    } else if let Some(name) = name {
        tracing::debug!(
            target: "stoat::change_walk",
            ?buffer_id,
            landing = name,
            "diff landing waits on the pool"
        );
    }
}

/// Install every wanted diff map of the active workspace whose hunks landed,
/// run the landings that waited for them, and report whether any landed.
pub fn pump_diff_wants(stoat: &mut Stoat) -> bool {
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
/// `buffer_id` in a latched split pane. A toggle jump, a pair verdict, or a
/// held row needs its editor focused with the diff view still on, so a view
/// closed before the hunks land keeps the cursor and the open file. A change
/// press or a file hop needs the focused buffer to be `buffer_id`. Otherwise
/// the landing is dropped.
fn land_diff_want(stoat: &mut Stoat, buffer_id: BufferId, landing: DiffLanding) {
    let name = landing.name();
    tracing::debug!(
        target: "stoat::change_walk",
        ?buffer_id,
        path = ?stoat.active_workspace().buffers.path_for(buffer_id),
        landing = name,
        "diff landing runs"
    );

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
                log_dropped_landing(stoat, buffer_id, name);
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
            } else {
                log_dropped_landing(stoat, buffer_id, name);
            }
        },
        DiffLanding::PairVerdict { editor } => {
            if !focused_diff_view(stoat, editor) {
                log_dropped_landing(stoat, buffer_id, name);
                return;
            }
            if !editor_shows_changes(stoat, editor) {
                stoat.set_status("the two files do not differ");
            }
        },
        DiffLanding::ChangeWalk { dir, count, unit } => {
            if focused.is_some_and(|(_, id)| id == buffer_id) {
                super::movement::goto_change_impl(stoat, dir, count, unit);
            } else {
                log_dropped_landing(stoat, buffer_id, name);
            }
        },
        DiffLanding::EdgeChange { last } => {
            if focused.is_some_and(|(_, id)| id == buffer_id) {
                super::movement::goto_edge_change(stoat, last);
            } else {
                log_dropped_landing(stoat, buffer_id, name);
            }
        },
        DiffLanding::FileHop {
            dir,
            unit,
            origin,
            wrapped,
            fallback,
        } => {
            if focused.is_some_and(|(_, id)| id == buffer_id) {
                super::movement::land_file_hop(stoat, dir, unit, origin, wrapped, fallback);
            } else {
                log_dropped_landing(stoat, buffer_id, name);
            }
        },
        DiffLanding::HeldRow { editor, screen_row } => {
            if !focused_diff_view(stoat, editor) {
                log_dropped_landing(stoat, buffer_id, name);
                return;
            }
            if let Some(state) = stoat.active_workspace_mut().editors.get_mut(editor) {
                state.held_screen_row = Some(screen_row);
            }
        },
    }
}

/// Log that landing `name` on `buffer_id` found the reader gone and did
/// nothing.
fn log_dropped_landing(stoat: &Stoat, buffer_id: BufferId, name: &str) {
    tracing::debug!(
        target: "stoat::change_walk",
        ?buffer_id,
        path = ?stoat.active_workspace().buffers.path_for(buffer_id),
        landing = name,
        "diff landing dropped, the reader moved"
    );
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
pub fn editor_shows_changes(stoat: &mut Stoat, editor_id: EditorId) -> bool {
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
/// changes nothing. In the diff view the row above a removed block counts as
/// the deletion's row, since the view draws the block below it.
///
/// The press captures the cursor row, the text, and the review base. The diff
/// and the write run as a queued git job, and the status and the gutter follow
/// when the job lands. Under a range review the press marks the hunk approved
/// instead, with no git write, through [`toggle_approval`].
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
/// and the gutter follow when the queued git job lands. Under a range review the
/// press marks the line approved instead, with no git write.
pub(super) fn stage_line(stoat: &mut Stoat) -> UpdateEffect {
    queue_stage(stoat, AmendUnit::Line)
}

/// Capture a staging press and queue its git work.
///
/// The row is the block cursor's cell, so a cursor resting on a line's last
/// cell stages that line, not the next. In the diff view the row above a
/// zero-width hunk stands for that hunk, as [`pressed_row`] gives it.
///
/// The text is a rope handle, so the press copies nothing that grows with the
/// file. The review base is read now and again at the job's turn. A walk step
/// or a `:review-done` checkout that lands ahead of the job moves the tree the
/// captured text and row describe.
fn queue_stage(stoat: &mut Stoat, unit: AmendUnit) -> UpdateEffect {
    let Some((_editor_id, buffer_id)) = stoat.focused_editor_ids() else {
        return UpdateEffect::None;
    };

    let diff_walk = super::movement::walks_diff_view(stoat);
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
        let cursor_row = pressed_row(&snapshot, buffer_snapshot, cursor_row, diff_walk);

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

    // A range review has nothing to write into, so the press marks the rows
    // itself rather than queue git work. A file that measures against another
    // file goes to the job, which refuses it.
    let reviewing = {
        let ws = stoat.active_workspace();
        ws.pair_for(&path).is_none()
            && matches!(
                ws.diff_base(),
                Some(DiffBase::Rev {
                    origin: RevOrigin::Review { .. },
                    ..
                })
            )
    };
    if reviewing {
        let hunk_rows = drawn.and_then(|(hunk, _)| hunk).map(|(_, rows, _)| rows);
        return toggle_approval(stoat, buffer_id, &path, cursor_row, unit, hunk_rows);
    }

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
            // The press marks rows under a review and queues nothing, so a
            // review base at this turn arrived after a press on another base.
            AmendRoute::Approve => {
                tracing::warn!(
                    target: "stoat::review",
                    path = %path.display(),
                    status = %BASE_MOVED,
                    "staging press refused"
                );
                stoat.set_status(BASE_MOVED);
                return None;
            },
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

/// The buffer row a staging press on `cursor_row` acts on.
///
/// The diff view splices a zero-width hunk's removed block below the live row
/// above its seam and ticks the block there, so with `diff_walk` set a press on
/// that row reads the seam. A row inside a hunk answers itself, and so does
/// every row of a plain pane, whose gutter marks the seam row.
pub fn pressed_row(
    snapshot: &DisplaySnapshot,
    buffer_snapshot: &MultiBufferSnapshot,
    cursor_row: u32,
    diff_walk: bool,
) -> u32 {
    let Some(map) = snapshot.diff_map().filter(|_| diff_walk) else {
        return cursor_row;
    };
    let live = map.live_hunks(buffer_snapshot);
    let on_hunk = live
        .in_range(cursor_row..cursor_row + 1)
        .any(|(_, rows)| rows.contains(&cursor_row));
    let seam_below = live
        .in_range(cursor_row + 1..cursor_row + 2)
        .any(|(_, rows)| rows.is_empty() && rows.start == cursor_row + 1);
    if !on_hunk && seam_below {
        cursor_row + 1
    } else {
        cursor_row
    }
}

/// Mark the change under the cursor approved under a range review, or take the
/// mark back when every row of it is already approved.
///
/// The press writes nothing to git. The workspace stores the rows per file and
/// the buffer re-diffs, so the gutter, the diff view, and the changes view read
/// the mark through the staged state they already show. `hunk_rows` is the
/// live hunk under the cursor, and a zero-width hunk at row `a` stands for row
/// `a`. With no hunk there, the press sets the status the staging keys set and
/// marks nothing.
fn toggle_approval(
    stoat: &mut Stoat,
    buffer_id: BufferId,
    path: &Path,
    cursor_row: u32,
    unit: AmendUnit,
    hunk_rows: Option<Range<u32>>,
) -> UpdateEffect {
    let (approved_status, unapproved_status, nothing) = match unit {
        AmendUnit::Line => ("approved line", "unapproved line", LINE_MESSAGES.2),
        AmendUnit::Hunk | AmendUnit::Rows(_) => {
            ("approved hunk", "unapproved hunk", RUN_MESSAGES.2)
        },
    };
    let Some(hunk_rows) = hunk_rows else {
        stoat.set_status(nothing);
        return UpdateEffect::Redraw;
    };
    let rows = match unit {
        AmendUnit::Rows(rows) => rows,
        AmendUnit::Hunk if hunk_rows.is_empty() => hunk_rows.start..hunk_rows.start + 1,
        AmendUnit::Hunk => hunk_rows,
        AmendUnit::Line => cursor_row..cursor_row + 1,
    };

    let ws = stoat.active_workspace_mut();
    let mut approved = ws
        .approved_rows(path)
        .map(|approved| (*approved).clone())
        .unwrap_or_default();
    let status = match approved.covers(&rows) {
        true => {
            approved.unapprove(rows.clone());
            unapproved_status
        },
        false => {
            approved.approve(rows.clone());
            approved_status
        },
    };
    ws.set_approved_rows(buffer_id, path, approved);

    tracing::info!(
        target: "stoat::review",
        path = %path.display(),
        ?rows,
        %status,
        "approval press landed"
    );
    stoat.set_status(status);
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
pub(crate) fn changed_or_whole(
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
/// Each chunk carries the Scope of its first changed line, which costs one
/// parse of each buffer whose language has an outline query.
///
/// Takes the language registry directly rather than `&Stoat` so it runs
/// inside the off-loop scan closures, including the commit-preview builds in
/// the sibling `commits` module and in [`crate::commit_preview`].
pub(crate) fn build_document_from_changes(
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
    attach_chunk_scopes(&mut doc);
    Some(doc)
}

/// Set the Scope of each chunk of `doc`, the chain of items that hold the
/// chunk's first changed line.
///
/// A change reads the buffer's tree, and a pure removal reads the base's. The
/// buffer parses with no memo, since the document's memo holds bases and a
/// buffer tree filed there evicts a base that a later diff reads. The base tree
/// is the one the diff filed in that memo.
fn attach_chunk_scopes(doc: &mut DiffDocument) {
    let memo = doc.tree_cache.clone();
    let DiffDocument { files, chunks, .. } = doc;
    for file in files.iter() {
        let Some(language) = file.language.as_ref() else {
            continue;
        };
        let Some(query) = language.outline_query() else {
            continue;
        };
        let buffer =
            stoat_language::structural_diff::parse_memoized(language, &file.buffer_text, None)
                .map(|tree| (tree, Rope::from(file.buffer_text.as_str())));
        let mut base = None;

        for id in &file.chunks {
            let Some(chunk) = chunks.get_mut(id) else {
                continue;
            };
            let (row, parsed) = match chunk.buffer_line_range.is_empty() {
                false => (chunk.buffer_line_range.start, buffer.as_ref()),
                true => {
                    let base = base.get_or_insert_with(|| {
                        stoat_language::structural_diff::parse_memoized(
                            language,
                            &file.base_text,
                            Some(&memo),
                        )
                        .map(|tree| (tree, Rope::from(file.base_text.as_str())))
                    });
                    (chunk.base_line_range.start, base.as_ref())
                },
            };
            let Some((tree, rope)) = parsed else {
                continue;
            };

            let byte = crate::diff_changes::first_text_byte(rope, row);
            let segments = stoat_language::scope_chain(query, tree.root_node(), rope, byte);
            if !segments.is_empty() {
                chunk.scope = Some(Arc::new(FoldScope {
                    segments,
                    split: None,
                    end: FoldEnd::None,
                    mark: ScopeMark::None,
                }));
            }
        }
    }
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
        attach_preview_highlights, build_document_from_changes, hunks_around, stage_rows_patch,
        unstage_rows_patch, StagingTexts,
    };
    use crate::{
        display_map::syntax_theme::SyntaxStyles,
        review_session::DiffDocument,
        theme::Theme,
        workspace::diff::{BaseHighlightCache, BaseHighlightMemo},
    };
    use std::{
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };
    use stoat_language::{structural_diff::TreeCache, LanguageRegistry};

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

    /// Each chunk carries the Scope of its first changed line. A change inside
    /// a function names the function, a removal names it from the parent's
    /// text, and a change at the top level names nothing. A changed line that
    /// opens a function inside an impl names both, since the chain reads the
    /// line's first non-blank byte.
    #[test]
    fn each_chunk_carries_the_scope_of_its_first_change() {
        let changes = vec![
            (
                PathBuf::from("a.rs"),
                "fn alpha() {\n    1;\n}\n".into(),
                "fn alpha() {\n    2;\n}\n".into(),
            ),
            (
                PathBuf::from("b.rs"),
                "// head\nfn beta() {\n    1;\n    2;\n}\n".into(),
                "// head\nfn beta() {\n    1;\n}\n".into(),
            ),
            (PathBuf::from("c.rs"), "// one\n".into(), "// two\n".into()),
            (
                PathBuf::from("d.rs"),
                "impl A {\n    fn gamma() {}\n}\n".into(),
                "impl A {\n    fn gamma() { 1 }\n}\n".into(),
            ),
        ];
        let doc =
            build_document_from_changes(&LanguageRegistry::standard(), Path::new("/work"), changes)
                .expect("four changed files build a document");

        let scopes: Vec<Option<Vec<String>>> = doc
            .files
            .iter()
            .map(|file| {
                doc.chunks[&file.chunks[0]].scope.as_ref().map(|scope| {
                    scope
                        .segments
                        .iter()
                        .map(|segment| format!("{:?} {}", segment.keyword, segment.name))
                        .collect()
                })
            })
            .collect();
        let chain = |segments: &[&str]| Some(segments.iter().map(|s| s.to_string()).collect());
        assert_eq!(
            scopes,
            [
                chain(&["Some(\"fn\") alpha"]),
                chain(&["Some(\"fn\") beta"]),
                None,
                chain(&["Some(\"impl\") A", "Some(\"fn\") gamma"]),
            ],
        );
    }
}

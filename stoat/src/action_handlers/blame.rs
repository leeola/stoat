use crate::{
    action_handlers::{review, review_walk},
    app::{Stoat, UpdateEffect},
    blame::{self, BlameCard, CommitDetails, LineBlame},
    buffer::BufferId,
    checkout_confirm::{self, CheckoutTarget},
    commit_preview::{self, PreviewOrigin},
    editor_state::EditorId,
    git_jobs::{self, GitJob, GitLanding, GitWork},
    host::{self, BlameCommit, ClipboardKind},
    keymap_state::{self, StoatKeymapState},
    pane::{FocusTarget, View},
    render::editor,
};
use std::{path::Path, sync::Arc, time::Duration};
use stoat_scheduler::Task;

/// How long the cursor rests on a committed line of a blamed pane before the
/// Blame Card opens by itself.
///
/// The BlameFlow board opens the card "after a moment" and names no setting,
/// so the wait is a constant.
const BLAME_CARD_REST: Duration = Duration::from_millis(500);

/// The status a blame key sets on a line whose blame has not landed.
const BLAME_LOADING: &str = "blame is loading";

/// The status a blame key sets on a line that no commit holds.
const LINE_UNCOMMITTED: &str = "line is not committed";

/// The rest the card's timer waits on, as the focused editor, its cursor's
/// buffer row, and its buffer's version.
///
/// A move inside one line keeps the key, and a move to another line or an
/// edit changes it.
pub(crate) type RestKey = (EditorId, u32, u64);

/// Show or hide the Blame Column of the focused split pane.
///
/// A dock, or a pane that shows no file, keeps its flag, and the status row
/// says why. Outside a repository the column does not turn on, and the Review
/// badge says why. The column turns off with no repository too, so a pane
/// always has a way out of it.
pub(super) fn toggle_blame(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let focus = ws.panes.focus();
    let shows_file = matches!(ws.focus, FocusTarget::SplitPane)
        && matches!(ws.panes.pane(focus).view, View::Editor(_));
    if !shows_file {
        stoat.set_status("blame needs a file pane");
        return UpdateEffect::Redraw;
    }

    let turning_on = !ws.panes.pane(focus).blame;
    if turning_on && ws.repo_root.is_none() {
        review::emit_review_error_badge(stoat, "not in a git repository", None);
        return UpdateEffect::Redraw;
    }
    ws.panes.pane_mut(focus).blame = turning_on;
    stoat.blame_card = None;
    UpdateEffect::Redraw
}

/// Open the Blame Card on the cursor's line.
///
/// A pane that does not blame opens nothing. With `announce`, a line whose
/// blame has not landed and an uncommitted line say so in the status row
/// instead. A card reads its details from the workspace's cache, and loads
/// them on the blocking pool otherwise. The card takes the place of an open
/// hover.
pub(super) fn open_card(stoat: &mut Stoat, announce: bool) -> UpdateEffect {
    if blamed_cursor(stoat).is_none() {
        return UpdateEffect::None;
    }
    let Some((editor, row, line)) = cursor_line_blame(stoat) else {
        if announce {
            stoat.set_status(BLAME_LOADING);
        }
        return UpdateEffect::Redraw;
    };
    let LineBlame::Commit { commit, .. } = line else {
        if announce {
            stoat.set_status(LINE_UNCOMMITTED);
        }
        return UpdateEffect::Redraw;
    };
    let commit = commit.clone();

    stoat.pending_hover = None;
    stoat.pending_hover_request.clear();
    let details = stoat
        .active_workspace()
        .blame
        .details(&commit.info.sha)
        .cloned();
    let load = details
        .is_none()
        .then(|| load_details(stoat, commit.info.sha.clone()));
    stoat.blame_card = Some(BlameCard::new(editor, row, commit, details, load));

    // The card's own keys need `blame_card`, so the hints read the state with
    // the card open.
    let hints = card_hints(stoat);
    if let Some(card) = stoat.blame_card.as_mut() {
        card.hints = hints;
    }
    UpdateEffect::Redraw
}

/// Close the Blame Card.
pub(super) fn close_card(stoat: &mut Stoat) -> UpdateEffect {
    stoat.blame_card = None;
    UpdateEffect::Redraw
}

/// Copy the full sha of the commit the blame keys act on to the system
/// clipboard, and name its short sha in the status row.
///
/// The registers keep what they hold. A pane that does not blame copies
/// nothing. A line whose blame has not landed and an uncommitted line copy
/// nothing, and the status says why.
pub(super) fn copy_sha(stoat: &mut Stoat) -> UpdateEffect {
    if blamed_cursor(stoat).is_none() {
        return UpdateEffect::None;
    }
    let commit = match acting_commit(stoat) {
        Ok(commit) => commit,
        Err(status) => {
            stoat.set_status(status);
            return UpdateEffect::Redraw;
        },
    };

    host::clipboard_copy(
        stoat.clipboard_host().as_ref(),
        stoat.env_host().as_ref(),
        ClipboardKind::System,
        &commit.info.sha,
    );
    stoat.set_status(format!("copied {}", commit.info.short_sha));
    UpdateEffect::Redraw
}

/// Open the Commit Preview on the commit of the cursor's line, or the
/// working-tree diff on an uncommitted line.
///
/// The preview opens at the line's place in the commit, and its label names
/// the pane's file and the cursor's line. A pane that does not blame opens
/// nothing. A line whose blame has not landed opens nothing, and the status
/// row says so.
pub(super) fn preview(stoat: &mut Stoat) -> UpdateEffect {
    let Some((_, buffer, row)) = blamed_cursor(stoat) else {
        return UpdateEffect::None;
    };
    let (commit, origin) = match cursor_line_blame(stoat) {
        None => {
            stoat.set_status(BLAME_LOADING);
            return UpdateEffect::Redraw;
        },
        Some((_, _, LineBlame::Uncommitted)) => return review::diff(stoat, None),
        Some((
            _,
            _,
            LineBlame::Commit {
                commit,
                commit_line,
                commit_path,
                ..
            },
        )) => {
            let name = stoat
                .active_workspace()
                .buffers
                .path_for(buffer)
                .and_then(Path::file_name)
                .unwrap_or_default()
                .to_string_lossy();
            let origin = PreviewOrigin {
                path: commit_path.to_path_buf(),
                line: commit_line,
                label: format!("{name}:{}", row + 1),
            };
            (commit.clone(), origin)
        },
    };

    commit_preview::open(stoat, commit, origin);
    UpdateEffect::Redraw
}

/// Check the commit of the cursor's line out, and show the line in the changes
/// view against the commit's parent.
///
/// While the Commit Preview is open, the commit and the line are the
/// preview's. The checkout is a review walk of one commit, so `:done` puts HEAD
/// back. A pane that does not blame checks nothing out. A line whose blame has
/// not landed and an uncommitted line check nothing out, and the status row
/// says why. An open walk, a running rebase, and a buffer with unsaved edits
/// refuse the checkout, and the Review badge says why.
///
/// The git queue reads the working tree's changes. A clean tree checks out at
/// once. A tree with changes, untracked files among them, opens the checkout
/// confirm, which offers to stash them first.
pub(super) fn checkout(stoat: &mut Stoat) -> UpdateEffect {
    let (commit, path, row) = match &stoat.commit_preview {
        Some(preview) => (
            preview.commit.info.clone(),
            preview.origin.path.clone(),
            preview.origin.line,
        ),
        None => {
            if blamed_cursor(stoat).is_none() {
                return UpdateEffect::None;
            }
            match cursor_line_blame(stoat) {
                None => {
                    stoat.set_status(BLAME_LOADING);
                    return UpdateEffect::Redraw;
                },
                Some((_, _, LineBlame::Uncommitted)) => {
                    stoat.set_status(LINE_UNCOMMITTED);
                    return UpdateEffect::Redraw;
                },
                Some((
                    _,
                    _,
                    LineBlame::Commit {
                        commit,
                        commit_line,
                        commit_path,
                        ..
                    },
                )) => (commit.info.clone(), commit_path.to_path_buf(), commit_line),
            }
        },
    };

    let Some(repo) = review_walk::checkout_repo(stoat) else {
        return UpdateEffect::Redraw;
    };
    let Some(workdir) = repo.workdir() else {
        return UpdateEffect::Redraw;
    };
    // A checkout writes the files under the buffers, and the reload after it
    // passes over a buffer with unsaved edits. That buffer keeps the text of
    // the commit the reader left, and a write puts it over the checked-out file.
    if !stoat.active_workspace().buffers.dirty_buffers().is_empty() {
        review::emit_review_error_badge(
            stoat,
            "unsaved buffers",
            Some("write or discard them before a checkout".to_string()),
        );
        return UpdateEffect::Redraw;
    }

    stoat.blame_card = None;
    let target = CheckoutTarget {
        workdir,
        commit,
        path,
        row,
    };
    let job = GitJob::new("checkout check", None, move |_: &mut Stoat| {
        Some(Box::new(move || {
            let files = checkout_confirm::file_rows(&repo.changed_files(), &target.workdir);
            let branch = repo.head_branch();
            let head = repo
                .resolve_rev("HEAD")
                .map(|sha| sha.chars().take(7).collect())
                .unwrap_or_default();
            Box::new(move |stoat: &mut Stoat| {
                land_checkout_check(stoat, target, branch, head, files);
            }) as GitLanding
        }) as GitWork)
    });
    git_jobs::enqueue(stoat, job);
    UpdateEffect::Redraw
}

/// Start the checkout of `target` once the git queue read the working tree, or
/// ask first when the tree holds changes.
///
/// A clean tree closes the Commit Preview and checks out at once.
fn land_checkout_check(
    stoat: &mut Stoat,
    target: CheckoutTarget,
    branch: Option<String>,
    head: String,
    files: Vec<(String, String)>,
) {
    if !files.is_empty() {
        checkout_confirm::open(stoat, target, branch, head, files);
        return;
    }

    stoat.commit_preview = None;
    let CheckoutTarget {
        workdir,
        commit,
        path,
        row,
    } = target;
    review_walk::walk_blame_commit(stoat, workdir, commit, path, row);
}

/// Stash the working tree's changes and start the checkout the confirm names.
///
/// The Commit Preview behind the confirm closes with it.
pub(super) fn accept_checkout(stoat: &mut Stoat) -> UpdateEffect {
    let Some(confirm) = stoat.checkout_confirm.take() else {
        return UpdateEffect::None;
    };
    let CheckoutTarget {
        workdir,
        commit,
        path,
        row,
    } = confirm.target;
    review_walk::walk_blame_commit_stashing(stoat, workdir, commit, path, row)
}

/// Keep the Blame Card in step with the cursor after an event.
///
/// The card closes once its line is gone from under the cursor. The rest
/// timer starts again whenever the cursor's line or its text changes, so the
/// card opens by itself after the cursor rests on a line for
/// [`BLAME_CARD_REST`].
pub(crate) fn sync_card(stoat: &mut Stoat) {
    close_stale_card(stoat);
    arm_rest(stoat);
}

/// Open the Blame Card once the cursor has rested on its line for
/// [`BLAME_CARD_REST`], and answer whether a card opened.
///
/// A timer whose rest the cursor has since left opens nothing, and so does a
/// rest while a card shows. A rest on an uncommitted line, or before the
/// blame lands, opens nothing and sets no status.
pub(crate) fn pump_rest(stoat: &mut Stoat) -> bool {
    let Some(key) = stoat.blame_rest.poll(&stoat.redraw_notify) else {
        return false;
    };
    if stoat.last_blame_rest != Some(key) || stoat.blame_card.is_some() {
        return false;
    }
    open_card(stoat, false);
    stoat.blame_card.is_some()
}

/// Close the Blame Card once its line is gone from under the cursor.
///
/// The card stays while no modal is open, the focused pane still blames, the
/// cursor stays on the card's row, and the card's commit still holds that row.
/// Any other state closes it.
fn close_stale_card(stoat: &mut Stoat) {
    let Some(card) = stoat.blame_card.as_ref() else {
        return;
    };
    let holds = keymap_state::active_modal(stoat).is_none()
        && cursor_line_blame(stoat).is_some_and(|(editor, row, line)| {
            editor == card.editor
                && row == card.row
                && matches!(line, LineBlame::Commit { commit, .. } if commit.info.sha == card.commit.info.sha)
        });
    if !holds {
        stoat.blame_card = None;
    }
}

/// Start the rest timer when the rest key of the cursor's line changed.
///
/// A state with no key drops the timer. A key that did not change keeps the
/// running timer, or none once it fired, so a card closed with `Escape` stays
/// closed until the cursor leaves its line or the buffer changes.
fn arm_rest(stoat: &mut Stoat) {
    let Some(key) = rest_key(stoat) else {
        stoat.blame_rest.clear();
        stoat.last_blame_rest = None;
        return;
    };
    if stoat.last_blame_rest == Some(key) {
        return;
    }

    stoat.last_blame_rest = Some(key);
    let executor = stoat.executor.clone();
    let task = stoat.spawn_woken(async move {
        executor.timer(BLAME_CARD_REST).await;
        key
    });
    stoat.blame_rest.arm(task);
}

/// The rest key of the cursor's line, or `None` unless the focused pane
/// blames in normal mode with no modal open.
fn rest_key(stoat: &Stoat) -> Option<RestKey> {
    if stoat.focused_mode() != "normal" || keymap_state::active_modal(stoat).is_some() {
        return None;
    }
    let (editor, _, row) = blamed_cursor(stoat)?;
    let version = stoat
        .active_workspace()
        .editors
        .get(editor)?
        .display_map
        .buffer_snapshot()
        .version();
    Some((editor, row, version))
}

/// Fill the Blame Card's details once their load lands, and keep them in the
/// workspace's cache.
///
/// Answers whether details landed.
pub(crate) fn pump_card(stoat: &mut Stoat) -> bool {
    let Some(card) = stoat.blame_card.as_mut() else {
        return false;
    };
    let Some(details) = card.poll_details(&stoat.redraw_notify) else {
        return false;
    };

    let details = Arc::new(details);
    card.details = Some(details.clone());
    let sha = card.commit.info.sha.clone();
    stoat
        .active_workspace_mut()
        .blame
        .file_details(sha, details);
    true
}

/// The focused editor, its buffer, and its cursor's buffer row, when the
/// focused split pane blames.
fn blamed_cursor(stoat: &Stoat) -> Option<(EditorId, BufferId, u32)> {
    let ws = stoat.active_workspace();
    if !matches!(ws.focus, FocusTarget::SplitPane) {
        return None;
    }
    let pane = ws.panes.pane(ws.panes.focus());
    let View::Editor(editor_id) = pane.view else {
        return None;
    };
    let state = ws.editors.get(editor_id)?;
    if !pane.blames(state) {
        return None;
    }
    let (line, _) = editor::editor_cursor_position(state)?;
    Some((editor_id, state.buffer_id, line - 1))
}

/// The blame of the cursor's line in the focused split pane, with the line's
/// editor and buffer row.
///
/// `None` when the pane does not blame, and before a blame of its buffer
/// lands.
fn cursor_line_blame(stoat: &Stoat) -> Option<(EditorId, u32, LineBlame<'_>)> {
    let (editor, buffer, row) = blamed_cursor(stoat)?;
    let line = stoat.active_workspace().blame.line(buffer, row)?;
    Some((editor, row, line))
}

/// The commit the blame keys act on, or the status that says why there is
/// none.
///
/// The commit is the one that last changed the cursor's line, so every blame
/// key reads it from here. While the Commit Preview is open, the commit is the
/// one the preview shows. The status reads `blame is loading` before a blame of
/// the buffer lands, and `line is not committed` on an uncommitted line.
fn acting_commit(stoat: &Stoat) -> Result<BlameCommit, &'static str> {
    if let Some(preview) = &stoat.commit_preview {
        return Ok(preview.commit.clone());
    }
    match cursor_line_blame(stoat) {
        None => Err(BLAME_LOADING),
        Some((_, _, LineBlame::Uncommitted)) => Err(LINE_UNCOMMITTED),
        Some((_, _, LineBlame::Commit { commit, .. })) => Ok(commit.clone()),
    }
}

/// Read the Blame Card details of commit `sha` on the blocking pool.
///
/// The task wakes the run loop when it lands, so the card fills in on the
/// next pass.
fn load_details(stoat: &Stoat, sha: String) -> Task<Option<CommitDetails>> {
    let git_host = stoat.git_host.clone();
    let root = stoat.active_workspace().git_root.clone();
    let redraw = stoat.redraw_notify.clone();
    stoat.executor.spawn_blocking(move || {
        let details = git_host
            .discover(&root)
            .map(|repo| blame::commit_details(&*repo, &sha));
        redraw.notify_one();
        details
    })
}

/// The key and the label of each labeled blame binding that a press reaches
/// now, in keymap order.
fn card_hints(stoat: &Stoat) -> Vec<(String, String)> {
    let state = StoatKeymapState::from_stoat(stoat);
    stoat
        .keymap
        .flag_bindings(&state, "blame")
        .into_iter()
        .filter_map(|(key, _, label)| Some((key, label?.to_owned())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::BLAME_CARD_REST;
    use crate::{
        action_handlers::{self, dispatch},
        app::Stoat,
        badge::BadgeSource,
        diff_changes,
        host::{ClipboardKind, GitRepo},
        keymap_state,
        lsp::hover::{HoverOutcome, HoverResponse},
        pane::View,
        register::Register,
        render::{commit_patch::PatchRow, paint, OVERLAY_LANE},
        review_walk::{ReturnRef, WalkStash},
        test_fixture,
        test_harness::{CommitSpec, TestHarness},
        theme::scope,
        workspace::{diff::DiffBase, Workspace},
    };
    use crossterm::event::MouseEventKind;
    use ratatui::layout::Rect;
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };
    use stoat_action::{
        BlameCard, BlameCheckout, BlamePreview, CheckoutConfirmAccept, CommitPreviewParent,
        DiffChanges, OpenCommandPalette, ReviewDone, SetTheme, ToggleBlame,
    };
    use stoatty_protocol::{
        command::{decode_stream, BorderStyle, Command},
        PROTOCOL_VERSION,
    };

    /// `space h a` sets the flag of the focused pane, and a second press
    /// clears it.
    #[test]
    fn space_h_a_turns_blame_on_and_off_for_the_focused_pane() {
        let mut h = Stoat::test();
        h.stage_review_scenario("/repo", &[("a.rs", "a\n", "b\n")]);
        h.open_file(Path::new("/repo/a.rs"));

        let before = blamed(&h);
        h.type_keys("space h a");
        let on = blamed(&h);
        h.type_keys("space h a");
        assert_eq!(
            [before, on, blamed(&h)],
            [false, true, false],
            "off, on after the first press, off after the second",
        );
    }

    /// Outside a repository the column stays off, and the Review badge says
    /// why. The key binds only inside one, so the action runs as the command
    /// line runs it.
    #[test]
    fn blame_stays_off_outside_a_repository() {
        let mut h = Stoat::test();
        dispatch(&mut h.stoat, &ToggleBlame);
        assert_eq!(
            (blamed(&h), review_badge(&h)),
            (false, Some("not in a git repository".to_string())),
        );
    }

    /// A pane that shows no file keeps the column off, and the status row says
    /// why.
    #[test]
    fn blame_needs_a_pane_that_shows_a_file() {
        let mut h = Stoat::test();
        h.stage_review_scenario("/repo", &[("a.rs", "a\n", "b\n")]);
        {
            let ws = h.stoat.active_workspace_mut();
            let focus = ws.panes.focus();
            ws.panes.pane_mut(focus).view = View::Label("label".into());
        }

        dispatch(&mut h.stoat, &ToggleBlame);
        assert_eq!(
            (blamed(&h), h.stoat.pending_message.clone()),
            (false, Some("blame needs a file pane".to_string())),
        );
    }

    /// `K` opens the card on the cursor's line with the line's commit, its
    /// row, and the labeled blame keys, and the commit's body and change size
    /// land on it.
    #[test]
    fn k_opens_the_card_on_the_cursor_line_and_its_details_land() {
        let mut h = blamed_repo("one\ntwo\n");
        h.type_keys("j K");
        h.settle();

        let card = h.stoat.blame_card.as_ref().expect("the card opened");
        let details = card.details.as_ref().expect("the details landed");
        assert_eq!(
            (
                (card.commit.info.sha.as_str(), card.row),
                (
                    details.body.as_deref(),
                    details.files,
                    details.added,
                    details.removed
                ),
                card.hints.clone(),
            ),
            (
                ("c2", 1),
                (Some("the body of two"), 1, 1, 0),
                vec![
                    ("Ret".to_string(), "preview patch".to_string()),
                    ("c".to_string(), "check out".to_string()),
                    ("y".to_string(), "copy sha".to_string()),
                    ("Esc".to_string(), "close".to_string()),
                ],
            ),
        );
    }

    /// The card closes when the cursor leaves its line.
    #[test]
    fn a_move_off_the_line_closes_the_card() {
        let mut h = blamed_repo("one\ntwo\n");
        h.type_keys("K");
        let opened = h.stoat.blame_card.is_some();
        h.type_keys("j");
        assert_eq!((opened, h.stoat.blame_card.is_some()), (true, false));
    }

    /// A move to another line of the card's commit closes the card too, since
    /// the card names one line.
    #[test]
    fn a_move_within_the_commit_closes_the_card() {
        let mut h = blamed_history(&[("c1", "one", &[("a.rs", "one\ntwo\n")])], "one\ntwo\n");
        h.type_keys("K");
        let opened = h.stoat.blame_card.is_some();
        h.type_keys("j");
        assert_eq!((opened, h.stoat.blame_card.is_some()), (true, false));
    }

    /// `Escape` closes the card and leaves the editor in normal mode.
    #[test]
    fn escape_closes_the_card_in_normal_mode() {
        let mut h = blamed_repo("one\ntwo\n");
        h.type_keys("K");
        let opened = h.stoat.blame_card.is_some();
        h.type_keys("escape");
        assert_eq!(
            (opened, h.stoat.blame_card.is_some(), h.stoat.focused_mode()),
            (true, false, "normal"),
        );
    }

    /// `K` on an uncommitted line opens no card, and the status row says why.
    #[test]
    fn k_on_an_uncommitted_line_says_so() {
        let mut h = blamed_repo("one\nTWO\n");
        h.type_keys("j K");
        assert_eq!(
            (
                h.stoat.blame_card.is_some(),
                h.stoat.pending_message.clone()
            ),
            (false, Some("line is not committed".to_string())),
        );
    }

    /// A hover that lands closes the card, so the two never show together.
    #[test]
    fn a_landed_hover_closes_the_card() {
        let mut h = blamed_repo("one\ntwo\n");
        h.type_keys("K");
        let opened = h.stoat.blame_card.is_some();

        let (editor_id, _) = h.stoat.focused_editor_ids().expect("a focused editor");
        let response = HoverResponse {
            text: "fn one()".to_owned(),
            plain: true,
            anchor_offset: 0,
            editor_id,
        };
        let task = h
            .stoat
            .executor
            .spawn_blocking(move || HoverOutcome::Content(response));
        h.stoat.pending_hover_request.arm(task);
        h.settle();
        assert_eq!(
            (
                opened,
                h.stoat.blame_card.is_some(),
                h.stoat.pending_hover.is_some()
            ),
            (true, false, true),
        );
    }

    /// A modal that opens over the pane closes the card.
    #[test]
    fn a_modal_that_opens_closes_the_card() {
        let mut h = blamed_repo("one\ntwo\n");
        h.type_keys("K");
        let opened = h.stoat.blame_card.is_some();
        h.type_keys(":");
        assert_eq!((opened, h.stoat.blame_card.is_some()), (true, false));
    }

    /// A card drops the hover still in flight, so the hover does not land over
    /// the card.
    #[test]
    fn a_card_drops_the_hover_in_flight() {
        let mut h = blamed_repo("one\ntwo\n");
        let (editor_id, _) = h.stoat.focused_editor_ids().expect("a focused editor");
        let response = HoverResponse {
            text: "fn one()".to_owned(),
            plain: true,
            anchor_offset: 0,
            editor_id,
        };
        let task = h
            .stoat
            .executor
            .spawn_blocking(move || HoverOutcome::Content(response));
        h.stoat.pending_hover_request.arm(task);
        dispatch(&mut h.stoat, &BlameCard);
        h.settle();
        assert_eq!(
            (
                h.stoat.blame_card.is_some(),
                h.stoat.pending_hover.is_some()
            ),
            (true, false),
        );
    }

    /// A second card on a commit reads the details the first one loaded, with
    /// no load of its own.
    #[test]
    fn a_second_card_reads_the_details_the_first_loaded() {
        let mut h = blamed_repo("one\ntwo\n");
        h.type_keys("K");
        h.settle();
        h.type_keys("escape");
        dispatch(&mut h.stoat, &BlameCard);
        let details = h
            .stoat
            .blame_card
            .as_ref()
            .and_then(|card| card.details.as_ref())
            .map(|details| (details.files, details.added, details.removed));
        assert_eq!(details, Some((1, 1, 0)));
    }

    /// A rest of 500 ms on a committed line opens the card for that line, and
    /// a rest a millisecond short of that opens nothing.
    #[test]
    fn a_rest_on_a_committed_line_opens_the_card() {
        let mut h = blamed_repo("one\ntwo\n");
        h.type_keys("j");
        h.advance_clock(Duration::from_millis(499));
        let before = card_line(&h);
        h.advance_clock(Duration::from_millis(1));
        assert_eq!((before, card_line(&h)), (None, Some(("c2".to_string(), 1))));
    }

    /// An edit starts the rest again, so a card closed with `Escape` opens
    /// again a rest after an edit of its line that an undo takes back.
    #[test]
    fn an_edit_starts_the_rest_again() {
        let mut h = blamed_repo("one\ntwo\n");
        h.advance_clock(BLAME_CARD_REST);
        h.type_keys("escape");
        h.type_keys("> u");
        h.advance_clock(BLAME_CARD_REST);
        assert_eq!(card_line(&h), Some(("c1".to_string(), 0)));
    }

    /// A rest with a modal open opens no card, though the mode stays normal.
    #[test]
    fn a_rest_under_a_modal_opens_nothing() {
        let mut h = blamed_repo("one\ntwo\n");
        h.type_keys("space h t");
        h.advance_clock(BLAME_CARD_REST);
        assert_eq!((h.stoat.git_status.is_some(), card_line(&h)), (true, None));
    }

    /// A move to another line starts the rest again, so the card opens a whole
    /// rest after the last move.
    #[test]
    fn a_move_starts_the_rest_again() {
        let mut h = blamed_repo("one\ntwo\n");
        h.advance_clock(Duration::from_millis(300));
        h.type_keys("j");
        h.advance_clock(Duration::from_millis(300));
        let early = card_line(&h);
        h.advance_clock(Duration::from_millis(200));
        assert_eq!((early, card_line(&h)), (None, Some(("c2".to_string(), 1))));
    }

    /// A card closed with `Escape` stays closed while the cursor rests on its
    /// line, and a rest after the cursor leaves the line and comes back opens
    /// it again.
    #[test]
    fn a_card_closed_with_escape_stays_closed_until_the_cursor_moves() {
        let mut h = blamed_repo("one\ntwo\n");
        h.advance_clock(BLAME_CARD_REST);
        let opened = card_line(&h);
        h.type_keys("escape");
        h.advance_clock(BLAME_CARD_REST);
        let closed = card_line(&h);
        h.type_keys("j k");
        h.advance_clock(BLAME_CARD_REST);

        let line_one = Some(("c1".to_string(), 0));
        assert_eq!(
            (opened, closed, card_line(&h)),
            (line_one.clone(), None, line_one),
        );
    }

    /// A rest on an uncommitted line opens no card and sets no status.
    #[test]
    fn a_rest_on_an_uncommitted_line_opens_nothing() {
        let mut h = blamed_repo("one\nTWO\n");
        h.type_keys("j");
        h.advance_clock(BLAME_CARD_REST);
        assert_eq!(
            (card_line(&h), h.stoat.pending_message.clone()),
            (None, None)
        );
    }

    /// A rest in insert mode opens no card.
    #[test]
    fn a_rest_in_insert_mode_opens_nothing() {
        let mut h = blamed_repo("one\ntwo\n");
        h.type_keys("i");
        h.advance_clock(BLAME_CARD_REST);
        assert_eq!((h.stoat.focused_mode(), card_line(&h)), ("insert", None));
    }

    /// `y` copies the full 40-character sha of the cursor line's commit to
    /// the system clipboard, names its short sha in the status row, and
    /// leaves the unnamed register as it was.
    #[test]
    fn y_copies_the_commit_sha_and_keeps_the_registers() {
        let sha = "c2".repeat(20);
        let mut h = blamed_history(
            &[
                ("c1", "one", &[("a.rs", "one\n")]),
                (&sha, "two", &[("a.rs", "one\ntwo\n")]),
            ],
            "one\ntwo\n",
        );
        let register = unnamed(&h);
        h.type_keys("j y");
        assert_eq!(
            (
                h.fake_clipboard().writes_for(ClipboardKind::System),
                h.stoat.pending_message.clone(),
                unnamed(&h),
            ),
            (vec![sha], Some("copied c2c2c2c".to_string()), register),
        );
    }

    /// `y` on an uncommitted line copies nothing, and the status says why.
    #[test]
    fn y_on_an_uncommitted_line_copies_nothing() {
        let mut h = blamed_repo("one\nTWO\n");
        h.type_keys("j y");
        assert_eq!(
            (h.fake_clipboard().writes(), h.stoat.pending_message.clone()),
            (
                Vec::<String>::new(),
                Some("line is not committed".to_string())
            ),
        );
    }

    /// In a pane that does not blame, `y` yanks the selection into the unnamed
    /// register and copies nothing to the clipboard.
    #[test]
    fn y_yanks_in_a_pane_that_does_not_blame() {
        let mut h = blamed_repo("one\ntwo\n");
        h.type_keys("space h a");
        h.type_keys("y");
        assert_eq!(
            (unnamed(&h), h.fake_clipboard().writes()),
            (Some(vec!["o".to_string()]), Vec::<String>::new()),
        );
    }

    /// In a pane that does not blame, `K` extends the selection up, as the
    /// plain normal keys bind it.
    #[test]
    fn k_extends_up_in_a_pane_that_does_not_blame() {
        let mut h = blamed_repo("one\ntwo\n");
        h.type_keys("space h a");
        h.type_keys("j K");
        assert_eq!(
            (h.cursor_display_positions(), h.stoat.blame_card.is_some()),
            (vec![(0, 0)], false),
        );
    }

    /// `Enter` on a committed line opens the Commit Preview on the line's
    /// commit as the `commit_preview` modal, and its build lands the patch of
    /// the commit's file and the commit's change size.
    #[test]
    fn enter_opens_the_preview_on_the_line_commit_and_its_patch_lands() {
        let mut h = blamed_repo("one\ntwo\n");
        h.type_keys("j enter");
        let modal = keymap_state::modal_predicate(&h.stoat);
        h.settle();

        let preview = h.stoat.commit_preview.as_ref().expect("the preview opened");
        let files = preview.document.as_ref().map(|doc| {
            doc.as_ref().map(|doc| {
                doc.files
                    .iter()
                    .map(|file| file.rel_path.clone())
                    .collect::<Vec<_>>()
            })
        });
        assert_eq!(
            (
                preview.commit.info.sha.as_str(),
                modal,
                preview.summary,
                files
            ),
            (
                "c2",
                Some("commit_preview"),
                Some((1, 1, 0)),
                Some(Some(vec!["a.rs".to_string()])),
            ),
        );
    }

    /// `j` scrolls the preview's patch one row, and `Ctrl-d` half of the 13
    /// rows the patch shows in at this window size.
    #[test]
    fn j_scrolls_the_preview_a_row_and_ctrl_d_half_the_patch_rows() {
        let mut h = long_preview();
        h.type_keys("j");
        let row = preview_scroll(&h);
        h.type_keys("ctrl-d");
        assert_eq!((row, preview_scroll(&h)), (Some(1), Some(7)));
    }

    /// The scroll stops where the patch's last row meets the bottom of the 13
    /// rows the patch shows in. The patch holds the file's header, its one
    /// line of context, and its 40 added lines, so the last page starts at row
    /// 29.
    #[test]
    fn the_preview_scroll_stops_at_the_patch_last_page() {
        let mut h = long_preview();
        h.type_keys("ctrl-d ctrl-d ctrl-d ctrl-d ctrl-d ctrl-d ctrl-d ctrl-d");
        assert_eq!(preview_scroll(&h), Some(29));
    }

    /// The preview of a commit that edits two files heads the rows of each.
    #[test]
    fn the_preview_heads_each_file_of_the_commit() {
        let mut h = blamed_history(
            &[
                ("c1", "one", &[("a.rs", "one\n"), ("b.rs", "b\n")]),
                ("c2", "two", &[("a.rs", "one\ntwo\n"), ("b.rs", "b\nc\n")]),
            ],
            "one\ntwo\n",
        );
        h.type_keys("j enter");
        h.settle();

        let preview = h.stoat.commit_preview.as_ref().expect("the preview opened");
        let doc = preview
            .document
            .clone()
            .flatten()
            .expect("the patch landed");
        let headers: Vec<&str> = preview
            .rows
            .iter()
            .filter_map(|row| match row {
                PatchRow::Header { file } => Some(doc.files[*file].rel_path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(headers, ["a.rs", "b.rs"]);
    }

    /// `] f` puts the next file's header at the top of the body, and `[ f` the
    /// nearest header above the top row. Past the last or the first file the
    /// scroll stays and the status says so. Each chord hands back normal mode.
    #[test]
    fn bracket_f_hops_the_preview_between_its_files() {
        let mut h = three_file_preview();
        let hop = |keys: &str| {
            h.type_keys(keys);
            (
                top_header(&h),
                h.stoat.pending_message.take(),
                h.stoat.focused_mode().to_string(),
            )
        };
        let at = |file: &str, status: Option<&str>| {
            (
                Some(file.to_string()),
                status.map(str::to_string),
                "normal".to_string(),
            )
        };
        assert_eq!(
            ["[ f", "] f", "] f", "] f", "[ f"].map(hop),
            [
                at("a.rs", Some("no file above")),
                at("b.rs", None),
                at("c.rs", None),
                at("c.rs", Some("no file below")),
                at("b.rs", None),
            ],
        );
    }

    /// The editor's bracket keys stay out of the preview, so `] Space` and
    /// `[ Space` add no line to the buffer under it. `Escape` leaves each menu
    /// and keeps the preview open.
    #[test]
    fn the_editor_bracket_keys_leave_the_buffer_under_the_preview() {
        let mut h = blamed_repo("one\ntwo\n");
        h.type_keys("j enter");
        h.type_keys("] space escape [ space escape");
        assert_eq!(
            (
                h.focused_buffer_text(),
                h.stoat.commit_preview.is_some(),
                h.stoat.focused_mode(),
            ),
            ("one\ntwo\n".to_string(), true, "normal"),
        );
    }

    /// `P` reads a merge's patch against its second parent, and a second `P`
    /// against its first again. Each patch lists the files that differ from its
    /// parent and carries their syntax colors, and its meta line names that
    /// parent and counts the patch. Only the first parent's patch rings the
    /// origin. A theme switch builds the patch again against the parent it
    /// reads.
    #[test]
    fn p_steps_a_merge_preview_through_its_parents() {
        let mut h = merge_preview();
        let opened = parent_view(&mut h);
        h.type_keys("P");
        h.settle();
        let stepped = parent_view(&mut h);
        dispatch(
            &mut h.stoat,
            &SetTheme {
                name: "default_dark".into(),
            },
        );
        h.settle();
        let themed = parent_view(&mut h);
        h.type_keys("P");
        h.settle();

        let view = |parent: usize, files: &[&str], ringed: bool, against: &str| {
            let files: Vec<String> = files.iter().map(|file| file.to_string()).collect();
            let meta = format!("fake \u{b7} 2023/11/14 22:13 \u{b7} against parent {against}");
            (parent, files, ringed, true, meta)
        };
        let first = view(0, &["a.rs"], true, "m3 \u{b7} 1 file +1 \u{2212}0");
        let second = view(
            1,
            &["a.rs", "c.rs"],
            false,
            "s2 \u{b7} 2 files +2 \u{2212}0",
        );
        assert_eq!(
            [opened, stepped, themed, parent_view(&mut h)],
            [first.clone(), second.clone(), second, first],
        );
    }

    /// A step to another parent drops the patch, its rows, its summary, and its
    /// origin row until the next build lands, and starts that patch at its top.
    #[test]
    fn a_parent_step_drops_the_patch_until_its_build_lands() {
        let mut h = merge_preview();
        if let Some(preview) = h.stoat.commit_preview.as_mut() {
            preview.scroll = 2;
        }
        dispatch(&mut h.stoat, &CommitPreviewParent);

        let preview = h
            .stoat
            .commit_preview
            .as_ref()
            .expect("the preview is open");
        assert_eq!(
            (
                preview.parent,
                preview.document.is_none(),
                preview.rows.len(),
                preview.summary,
                preview.origin_row,
                preview.scroll,
            ),
            (1, true, 0, None, None, 0),
        );
    }

    /// `P` on a commit with one parent or none keeps the patch it shows, and
    /// the status says why.
    #[test]
    fn p_on_a_commit_with_one_parent_or_none_keeps_its_patch() {
        let press = |history: &[CommitSpec<'_>], keys: &str| {
            let mut h = blamed_history(history, "one\ntwo\n");
            h.type_keys(keys);
            h.settle();
            h.type_keys("P");
            (h.stoat.pending_message.take(), preview_built(&h))
        };
        assert_eq!(
            [
                press(&[("c1", "one", &[("a.rs", "one\ntwo\n")])], "enter"),
                press(
                    &[
                        ("c1", "one", &[("a.rs", "one\n")]),
                        ("c2", "two", &[("a.rs", "one\ntwo\n")]),
                    ],
                    "j enter",
                ),
            ],
            [
                (Some("commit has no parent".to_string()), true),
                (Some("commit has one parent".to_string()), true),
            ],
        );
    }

    /// A preview opened from a line the commit added in the second of its two
    /// files opens with that file's header at the top of the body. Under a live
    /// scene a light ring in the focused border's color marks the line's row,
    /// before the row's number runs so the ring hides none of them. A run on the
    /// row's tint ends a cell short of the row's right edge, so its 8 characters
    /// start at column 97, and it names the pane's file and the cursor's line,
    /// which an uncommitted line above it moves off the line's place in the
    /// commit.
    #[test]
    fn the_preview_opens_at_the_blamed_line_ringed_and_labeled() {
        let lines =
            |stem: &str, count| -> String { (0..count).map(|i| format!("{stem}{i}\n")).collect() };
        let (a, b) = (
            format!("a\n{}", lines("a", 40)),
            format!("b\n{}", lines("b", 20)),
        );
        let mut h = TestHarness::with_size(120, 24);
        h.seed_linear_history(
            "/repo",
            &[
                ("c1", "one", &[("a.rs", "a\n"), ("b.rs", "b\n")]),
                ("c2", "two", &[("a.rs", &a), ("b.rs", &b)]),
            ],
        );
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_fs().insert_file("/repo/b.rs", format!("new\n{b}"));
        h.open_file(Path::new("/repo/b.rs"));
        h.type_keys("space h a");
        h.settle();
        h.stoat.stoatty_protocol = PROTOCOL_VERSION;
        h.type_keys("j j enter");
        h.settle();
        h.stoat.render();

        let theme = &h.stoat.theme;
        let accent = paint::style_rgb(theme.get(scope::UI_BORDER_FOCUSED).fg);
        let tint = paint::style_rgb(theme.get(scope::VCS_COMMIT_ROW_ADDED).bg);
        let commands = decode_stream(h.stoat.apc_scene.lane_bytes(OVERLAY_LANE));
        let rings: Vec<(usize, Rect)> = commands
            .iter()
            .enumerate()
            .filter_map(|(at, command)| match command {
                Command::Panel(panel)
                    if panel.style == BorderStyle::Light && Some(panel.border) == accent =>
                {
                    Some((
                        at,
                        Rect::new(panel.left, panel.top, panel.width, panel.height),
                    ))
                },
                _ => None,
            })
            .collect();
        let numbers = commands
            .iter()
            .position(|command| matches!(command, Command::TextRuns(_)));
        let labels: Vec<(&str, i16, Option<[u8; 3]>)> = commands
            .iter()
            .filter_map(|command| match command {
                Command::TextRun(run) if run.text.starts_with('\u{25c2}') => {
                    Some((run.text.as_str(), run.col, run.bg))
                },
                _ => None,
            })
            .collect();
        assert_eq!(
            (
                preview_scroll(&h),
                rings.iter().map(|&(_, rect)| rect).collect::<Vec<_>>(),
                rings
                    .iter()
                    .all(|&(at, _)| numbers.is_some_and(|numbers| at < numbers)),
                labels,
            ),
            (
                Some(42),
                vec![Rect::new(14, 9, 92, 1)],
                true,
                vec![("\u{25c2} b.rs:3", 97 * 16, tint)],
            ),
        );
    }

    /// A wheel notch over the patch scrolls it three rows.
    #[test]
    fn a_wheel_notch_over_the_patch_scrolls_it_three_rows() {
        let mut h = long_preview();
        h.stoat.update(test_fixture::mouse_event(
            MouseEventKind::ScrollDown,
            50,
            10,
        ));
        assert_eq!(preview_scroll(&h), Some(3));
    }

    /// The preview opens in normal mode, where its keys bind, from any mode.
    #[test]
    fn the_preview_opens_in_normal_mode() {
        let mut h = blamed_repo("one\ntwo\n");
        h.type_keys("j i");
        dispatch(&mut h.stoat, &BlamePreview);
        assert_eq!(
            (h.stoat.commit_preview.is_some(), h.stoat.focused_mode()),
            (true, "normal"),
        );
    }

    /// The palette key over the preview closes the preview as the palette
    /// opens, so two modals never show at once.
    #[test]
    fn the_palette_key_closes_the_preview() {
        let mut h = blamed_repo("one\ntwo\n");
        h.type_keys("j enter");
        h.type_keys("ctrl-;");
        assert_eq!(
            (
                h.stoat.commit_preview.is_some(),
                h.stoat.command_palette.is_some()
            ),
            (false, true),
        );
    }

    /// `Escape` closes the preview, and the pane under it still blames.
    #[test]
    fn escape_closes_the_preview_and_the_pane_still_blames() {
        let mut h = blamed_repo("one\ntwo\n");
        h.type_keys("j enter");
        let opened = h.stoat.commit_preview.is_some();
        h.type_keys("escape");
        assert_eq!(
            (opened, h.stoat.commit_preview.is_some(), blamed(&h)),
            (true, false, true),
        );
    }

    /// `y` in the preview copies the commit the preview shows, after the
    /// cursor under it moves to a line of another commit.
    #[test]
    fn y_in_the_preview_copies_the_previewed_commit() {
        let mut h = blamed_repo("one\ntwo\n");
        h.type_keys("j enter");
        h.stoat.collapse_focused_cursor_to(0);
        h.type_keys("y");
        assert_eq!(
            h.fake_clipboard().writes_for(ClipboardKind::System),
            vec!["c2".to_string()],
        );
    }

    /// `Enter` on an edited line opens the working-tree diff and no preview.
    #[test]
    fn enter_on_an_edited_line_opens_the_working_tree_diff() {
        let mut h = blamed_repo("one\nTWO\n");
        h.type_keys("j enter");
        let diff_view =
            action_handlers::focused_editor_mut(&mut h.stoat).map(|editor| editor.diff_view);
        assert_eq!(
            (diff_view, h.stoat.commit_preview.is_some()),
            (Some(true), false),
        );
    }

    /// A theme switch builds the open preview's patch again, since the build
    /// bakes the theme's syntax colors into the patch.
    #[test]
    fn a_theme_switch_builds_the_open_preview_again() {
        let mut h = blamed_repo("one\ntwo\n");
        h.type_keys("j enter");
        h.settle();
        let built = preview_built(&h);
        dispatch(
            &mut h.stoat,
            &SetTheme {
                name: "default_dark".into(),
            },
        );
        let dropped = preview_built(&h);
        h.settle();
        assert_eq!((built, dropped, preview_built(&h)), (true, false, true));
    }

    /// A checkout from the last line `b2c3d4e5` added closes the Blame Card,
    /// checks that commit out, detached, and reads it against its parent. The
    /// changes view opens on the line's file at the line, past the first line
    /// of its hunk, and the checkout raises no Review badge.
    #[test]
    fn a_checkout_opens_the_changes_view_at_the_blamed_line() {
        let mut h = checkout_history(CHECKOUT_A);
        h.type_keys("j j K");
        dispatch(&mut h.stoat, &BlameCheckout);
        let card = h.stoat.blame_card.is_some();
        h.settle();
        assert_eq!(
            (
                card,
                checkouts(&h),
                base_sha(&h),
                changes_cursor(&mut h),
                review_badge(&h),
            ),
            (
                false,
                vec!["detached:b2c3d4e5".to_string()],
                Some("a1b2c3d4".to_string()),
                Some(("a.rs".to_string(), 2)),
                None,
            ),
        );
    }

    /// The landing shows the blamed line after the reader moved off the line,
    /// and off its file, while the checkout ran.
    #[test]
    fn a_checkout_lands_on_the_blamed_line_after_the_reader_moved() {
        let mut h = checkout_history(CHECKOUT_A);
        h.fake_fs().insert_file("/repo/b.rs", "fn b() {}\n");
        h.type_keys("j j");
        dispatch(&mut h.stoat, &BlameCheckout);
        h.stoat.collapse_focused_cursor_to(0);
        h.stoat.open_file(Path::new("/repo/b.rs"));
        h.settle();
        assert_eq!(changes_cursor(&mut h), Some(("a.rs".to_string(), 2)));
    }

    /// A checkout before the blame lands, or from an edited line, checks
    /// nothing out, and the status says why.
    #[test]
    fn a_checkout_needs_a_commit_for_the_line() {
        let loading = {
            let mut h = repo_pane(CHECKOUT_HISTORY, CHECKOUT_A);
            h.stoat.active_workspace_mut().repo_root = Some(PathBuf::from("/repo"));
            dispatch(&mut h.stoat, &ToggleBlame);
            dispatch(&mut h.stoat, &BlameCheckout);
            h.settle();
            (checkouts(&h), h.stoat.pending_message.clone())
        };
        let edited = {
            let mut h = checkout_history("fn a() {}\nfn a2() {}\nEDITED\n");
            h.type_keys("j j c");
            h.settle();
            (checkouts(&h), h.stoat.pending_message.clone())
        };
        assert_eq!(
            [loading, edited],
            [
                (Vec::new(), Some("blame is loading".to_string())),
                (Vec::new(), Some("line is not committed".to_string())),
            ],
        );
    }

    /// An unsaved buffer and the walk an earlier `c` opened each refuse a
    /// checkout, and the Review badge says why.
    #[test]
    fn c_refuses_over_unsaved_buffers_and_an_open_walk() {
        let refused = |seed: &dyn Fn(&mut TestHarness)| {
            let mut h = checkout_history(CHECKOUT_A);
            seed(&mut h);
            h.type_keys("j j c");
            h.settle();
            (checkouts(&h), review_badge(&h))
        };
        assert_eq!(
            [
                refused(&|h| h.type_keys("i x escape")),
                refused(&|h| {
                    h.type_keys("j j c");
                    h.settle();
                    dispatch(&mut h.stoat, &DiffChanges);
                    h.type_keys("k k");
                }),
            ],
            [
                (Vec::new(), Some("unsaved buffers".to_string())),
                (
                    vec!["detached:b2c3d4e5".to_string()],
                    Some("a review walk is open".to_string()),
                ),
            ],
        );
    }

    /// In a pane that does not blame, `c` changes the selection, as the plain
    /// normal keys bind it, and the checkout action does nothing.
    #[test]
    fn c_changes_in_a_pane_that_does_not_blame() {
        let mut h = blamed_repo("one\ntwo\n");
        h.type_keys("space h a");
        h.type_keys("c");
        dispatch(&mut h.stoat, &BlameCheckout);
        assert_eq!(
            (
                h.focused_buffer_text(),
                h.stoat.focused_mode(),
                h.stoat.pending_message.clone(),
            ),
            ("ne\ntwo\n".to_string(), "insert", None),
        );
    }

    /// Over a tree with changes, a checkout opens the confirm, which lists the
    /// changes with the untracked file among them, and checks nothing out.
    #[test]
    fn a_checkout_over_changes_opens_the_confirm() {
        let mut h = dirty_checkout();
        h.type_keys("j j c");
        h.settle();
        let pair = |left: &str, right: &str| (left.to_string(), right.to_string());
        assert_eq!(
            (
                keymap_state::modal_predicate(&h.stoat),
                h.stoat.checkout_confirm.as_ref().map(|confirm| {
                    (
                        confirm.branch.clone(),
                        confirm.head.clone(),
                        confirm.files.clone(),
                        confirm.hints.clone(),
                    )
                }),
                checkouts(&h),
            ),
            (
                Some("checkout_confirm"),
                Some((
                    Some("main".to_string()),
                    "c3d4e5f".to_string(),
                    vec![pair(" M", "b.rs"), pair("??", "notes.md")],
                    vec![pair("Ret", "stash and check out"), pair("Esc", "cancel")],
                )),
                Vec::<String>::new(),
            ),
        );
    }

    /// The confirm opens in normal mode, where its keys bind, from any mode.
    #[test]
    fn the_confirm_opens_in_normal_mode() {
        let mut h = dirty_checkout();
        h.type_keys("j j");
        dispatch(&mut h.stoat, &BlameCheckout);
        h.stoat.set_focused_mode("insert".to_string());
        h.settle();
        assert_eq!(
            (h.stoat.checkout_confirm.is_some(), h.stoat.focused_mode()),
            (true, "normal"),
        );
    }

    /// The palette key over the confirm closes the confirm and the preview
    /// behind it as the palette opens, so the palette is the one modal open.
    #[test]
    fn the_palette_key_closes_the_confirm_and_the_preview_behind_it() {
        let mut h = dirty_checkout();
        h.type_keys("j j enter");
        h.settle();
        h.type_keys("c");
        h.settle();
        h.type_keys("ctrl-;");
        assert_eq!(
            (
                h.stoat.checkout_confirm.is_some(),
                h.stoat.commit_preview.is_some(),
                h.stoat.command_palette.is_some(),
            ),
            (false, false, true),
        );
    }

    /// `Enter` in the confirm stashes the changes, the untracked file with
    /// them, under a message that names the commit. It then checks the commit
    /// out and opens the changes view at the blamed line.
    #[test]
    fn enter_in_the_confirm_stashes_the_changes_and_checks_out() {
        let mut h = dirty_checkout();
        h.type_keys("j j c");
        h.settle();
        h.type_keys("enter");
        h.settle();
        assert_eq!(
            (
                stashes(&h),
                changed_files(&h),
                checkouts(&h),
                h.stoat.checkout_confirm.is_some(),
                changes_cursor(&mut h),
            ),
            (
                vec!["On main: stoat: blame checkout of b2c3d4e".to_string()],
                0,
                vec!["detached:b2c3d4e5".to_string()],
                false,
                Some(("a.rs".to_string(), 2)),
            ),
        );
    }

    /// `Escape` in the confirm closes it with no stash and no walk.
    #[test]
    fn escape_in_the_confirm_checks_nothing_out() {
        let mut h = dirty_checkout();
        h.type_keys("j j c");
        h.settle();
        h.type_keys("escape");
        h.settle();
        assert_eq!(
            (
                h.stoat.checkout_confirm.is_some(),
                stashes(&h),
                checkouts(&h),
                h.stoat.active_workspace().review_walk.is_some(),
            ),
            (false, Vec::<String>::new(), Vec::<String>::new(), false),
        );
    }

    /// A stash that fails checks nothing out, leaves no walk, and badges the
    /// failure.
    #[test]
    fn a_failed_stash_ends_the_checkout() {
        let mut h = dirty_checkout();
        h.type_keys("j j c");
        h.settle();
        h.fake_git().add_repo("/repo").fail_apply_with("disk full");
        h.type_keys("enter");
        h.settle();
        assert_eq!(
            (
                checkouts(&h),
                h.stoat.active_workspace().review_walk.is_some(),
                review_badge(&h),
            ),
            (
                Vec::<String>::new(),
                false,
                Some("checkout failed".to_string())
            ),
        );
    }

    /// From an open Commit Preview, `c` takes the preview behind the confirm,
    /// and the frame paints both boxes. `Escape` brings the preview back.
    #[test]
    fn the_confirm_holds_the_preview_behind_it_and_gives_it_back() {
        let mut h = dirty_checkout();
        h.stoat.stoatty = false;
        h.type_keys("j j enter");
        h.settle();
        h.type_keys("c");
        h.settle();
        let held = (
            keymap_state::modal_predicate(&h.stoat),
            h.stoat
                .checkout_confirm
                .as_ref()
                .is_some_and(|confirm| confirm.behind.is_some()),
        );
        h.snapshot();
        let text = h.rendered_text();
        let painted =
            ["preview \u{b7} read-only", "Check out b2c3d4e?"].map(|part| text.contains(part));
        h.type_keys("escape");
        assert_eq!(
            (held, painted, keymap_state::modal_predicate(&h.stoat)),
            (
                (Some("checkout_confirm"), true),
                [true, true],
                Some("commit_preview"),
            ),
        );
    }

    /// A modal opened while the check waits in the git queue keeps the confirm
    /// out, and nothing is checked out.
    #[test]
    fn a_modal_opened_before_the_check_lands_keeps_the_confirm_out() {
        let mut h = dirty_checkout();
        h.type_keys("j j");
        dispatch(&mut h.stoat, &BlameCheckout);
        dispatch(&mut h.stoat, &OpenCommandPalette);
        h.settle();
        assert_eq!(
            (
                h.stoat.checkout_confirm.is_some(),
                h.stoat.command_palette.is_some(),
                checkouts(&h),
            ),
            (false, true, Vec::<String>::new()),
        );
    }

    /// From the Commit Preview over a clean tree, `c` closes the preview and
    /// checks the previewed commit out at the preview's line.
    #[test]
    fn c_in_the_preview_checks_its_commit_out_over_a_clean_tree() {
        let mut h = checkout_history(CHECKOUT_A);
        h.type_keys("j j enter");
        h.settle();
        h.type_keys("c");
        h.settle();
        assert_eq!(
            (
                h.stoat.commit_preview.is_some(),
                checkouts(&h),
                changes_cursor(&mut h),
            ),
            (
                false,
                vec!["detached:b2c3d4e5".to_string()],
                Some(("a.rs".to_string(), 2)),
            ),
        );
    }

    /// A stashing checkout records its stash on the walk, with how many files
    /// it took, so the return pops it.
    #[test]
    fn a_stashing_checkout_records_its_stash_on_the_walk() {
        let h = stashed_checkout();
        let sha = repo(&h)
            .stash_list()
            .first()
            .map(|stash| stash.sha.clone())
            .expect("the checkout's stash");
        assert_eq!(
            h.stoat
                .active_workspace()
                .review_walk
                .as_ref()
                .and_then(|walk| walk.stash.clone()),
            Some(WalkStash { sha, changes: 2 }),
        );
    }

    /// `space h c` after a stashing checkout returns to `main` and pops the
    /// stash, so both changes come back. The base and the walk clear, an
    /// editor takes the focus, and the status names the return.
    #[test]
    fn space_h_c_returns_from_a_stashing_checkout_and_pops_the_stash() {
        let mut h = stashed_checkout();
        h.type_keys("space h c");
        h.settle();
        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                checkouts(&h).last().cloned(),
                stashes(&h),
                changed_files(&h),
                base_sha(&h),
                ws.review_walk.is_some(),
                matches!(ws.panes.pane(ws.panes.focus()).view, View::Editor(_)),
                h.stoat.pending_message.clone(),
            ),
            (
                Some("ref:main".to_string()),
                Vec::<String>::new(),
                2,
                None,
                false,
                true,
                Some("returned to main, 2 changes restored".to_string()),
            ),
        );
    }

    /// A return pressed while the stashing checkout is still out pops the stash
    /// too. The checkout's landing records the stash on the ended walk, whose
    /// return runs after it.
    #[test]
    fn a_return_pressed_while_the_checkout_is_out_pops_the_stash() {
        let mut h = dirty_checkout();
        h.type_keys("j j c");
        h.settle();
        dispatch(&mut h.stoat, &CheckoutConfirmAccept);
        dispatch(&mut h.stoat, &ReviewDone);
        h.settle();
        assert_eq!(
            (
                stashes(&h),
                changed_files(&h),
                h.stoat.active_workspace().review_walk.is_some(),
                h.stoat.pending_message.clone(),
            ),
            (
                Vec::<String>::new(),
                2,
                false,
                Some("returned to main, 2 changes restored".to_string()),
            ),
        );
    }

    /// A pop that fails still ends the walk, since HEAD is back, and the Review
    /// badge says the stash kept the changes.
    #[test]
    fn a_failed_pop_ends_the_walk_and_keeps_the_stash() {
        let mut h = stashed_checkout();
        h.fake_git().add_repo("/repo").fail_apply_with("conflict");
        h.type_keys("space h c");
        h.settle();
        assert_eq!(
            (
                checkouts(&h).last().cloned(),
                h.stoat.active_workspace().review_walk.is_some(),
                stashes(&h).len(),
                review_badge(&h),
            ),
            (
                Some("ref:main".to_string()),
                false,
                1,
                Some("stash kept".to_string()),
            ),
        );
    }

    /// The return's status names a detached HEAD by its commit, and it reads
    /// one restored change in the singular.
    #[test]
    fn the_return_status_names_a_detached_head_and_one_change() {
        let returned = |mut h: TestHarness, stashing: bool| {
            h.type_keys("j j c");
            h.settle();
            if stashing {
                h.type_keys("enter");
                h.settle();
            }
            h.type_keys("space h c");
            h.settle();
            h.stoat.pending_message.clone()
        };
        let detached = returned(blamed_history(CHECKOUT_HISTORY, CHECKOUT_A), false);
        let one = {
            let h = checkout_history(CHECKOUT_A);
            h.fake_git().add_repo("/repo").untracked("notes.md");
            returned(h, true)
        };
        assert_eq!(
            [detached, one],
            [
                Some("HEAD detached at c3d4e5f".to_string()),
                Some("returned to main, 1 change restored".to_string()),
            ],
        );
    }

    /// With no walk open, `space h c` turns the change walk's wrap off and on.
    #[test]
    fn space_h_c_with_no_walk_turns_the_change_walk_wrap() {
        let mut h = checkout_history(CHECKOUT_A);
        let before = h.stoat.change_walk_wrap;
        h.type_keys("space h c");
        assert_eq!((before, h.stoat.change_walk_wrap), (true, false));
    }

    /// A restart restores a stashing checkout's walk with its stash, its banner,
    /// and the commit's parent as the base. HEAD still stands on the commit, so
    /// the check keeps the walk and runs no more, and `space h c` then returns
    /// and pops the stash.
    #[test]
    fn a_restarted_checkout_keeps_its_walk_and_returns_with_its_stash() {
        let mut h = restarted(stashed_checkout());
        let restored = (
            h.stoat.active_workspace().review_walk.as_ref().map(|walk| {
                (
                    walk.return_ref.clone(),
                    walk.stash.as_ref().map(|s| s.changes),
                )
            }),
            base_sha(&h),
            h.stoat.banner_shown(),
            h.stoat.active_workspace().walk_unchecked,
        );
        h.type_keys("space h c");
        h.settle();
        assert_eq!(
            (
                restored,
                stashes(&h),
                changed_files(&h),
                h.stoat.pending_message.clone(),
            ),
            (
                (
                    Some((ReturnRef::Branch("main".to_string()), Some(2))),
                    Some("a1b2c3d4".to_string()),
                    true,
                    false,
                ),
                Vec::<String>::new(),
                2,
                Some("returned to main, 2 changes restored".to_string()),
            ),
        );
    }

    /// A restart after another tool moved HEAD ends the restored walk and its
    /// base, and the Review badge names the stash that holds the changes.
    #[test]
    fn a_restart_after_head_moved_ends_the_walk_and_names_the_stash() {
        let h = stashed_checkout();
        let stash = repo(&h)
            .stash_list()
            .first()
            .map(|stash| stash.sha.clone())
            .expect("the checkout's stash");
        h.fake_git().add_repo("/repo").set_head("a1b2c3d4");
        let h = restarted(h);

        let ws = h.stoat.active_workspace();
        let badge = ws
            .badges
            .find_by_source(BadgeSource::Review)
            .and_then(|id| ws.badges.get(id))
            .map(|badge| (badge.label.clone(), badge.detail.clone()));
        assert_eq!(
            (ws.review_walk.is_some(), base_sha(&h), badge),
            (
                false,
                None,
                Some((
                    "checkout ended outside stoat".to_string(),
                    Some(format!(
                        "stash {} holds your changes",
                        &stash[..stash.len().min(7)]
                    )),
                )),
            ),
        );
    }

    /// A 120-cell window over `a.rs` of a repository whose first commit writes
    /// `one` and whose second adds `two` under a message with a body, with
    /// `text` in the working tree and the Blame Column on.
    fn blamed_repo(text: &str) -> TestHarness {
        blamed_history(
            &[
                ("c1", "one", &[("a.rs", "one\n")]),
                ("c2", "two\n\nthe body\nof two\n", &[("a.rs", "one\ntwo\n")]),
            ],
            text,
        )
    }

    /// `a.rs` as `b2c3d4e5` leaves it, with the two lines that commit added
    /// after the first.
    const CHECKOUT_A: &str = "fn a() {}\nfn a2() {}\nfn a3() {}\n";

    /// A history of three commits, in which `a1b2c3d4` writes the first line of
    /// `a.rs`, `b2c3d4e5` adds two more, and `c3d4e5f6` adds `b.rs`.
    const CHECKOUT_HISTORY: &[CommitSpec<'static>] = &[
        ("a1b2c3d4", "feat: add a.rs", &[("a.rs", "fn a() {}\n")]),
        ("b2c3d4e5", "chore: tweak a", &[("a.rs", CHECKOUT_A)]),
        (
            "c3d4e5f6",
            "feat: add b.rs",
            &[("a.rs", CHECKOUT_A), ("b.rs", "fn b() {}\n")],
        ),
    ];

    /// A 120-cell window over `a.rs` of [`CHECKOUT_HISTORY`] in `/repo` on
    /// `main` at `c3d4e5f6`, with `text` in the working tree and the Blame
    /// Column on.
    fn checkout_history(text: &str) -> TestHarness {
        let h = blamed_history(CHECKOUT_HISTORY, text);
        h.fake_git()
            .add_repo("/repo")
            .branch("main", "c3d4e5f6")
            .set_head_branch("main");
        h
    }

    /// Each checkout the fake host recorded in `/repo`, oldest first.
    fn checkouts(h: &TestHarness) -> Vec<String> {
        h.fake_git().checkouts(Path::new("/repo"))
    }

    /// The commit the workspace's diff base reads against.
    fn base_sha(h: &TestHarness) -> Option<String> {
        match h.stoat.active_workspace().diff_base() {
            Some(DiffBase::Rev { sha, .. }) => sha.clone(),
            _ => None,
        }
    }

    /// The file and the cursor's buffer row of the focused excerpt of the
    /// focused changes view.
    fn changes_cursor(h: &mut TestHarness) -> Option<(String, u32)> {
        let id = h.stoat.active_workspace().focused_diff_changes_id()?;
        let Workspace {
            diff_changes_views,
            editors,
            ..
        } = h.stoat.active_workspace_mut();
        let excerpt = diff_changes_views.get(id)?.focused_excerpt()?;
        let editor = editors.get_mut(excerpt.editor_id)?;
        Some((
            excerpt.rel_path.clone(),
            diff_changes::cursor_point(editor).row,
        ))
    }

    /// [`checkout_history`] over a working tree that edits `b.rs` and holds
    /// the untracked `notes.md`.
    fn dirty_checkout() -> TestHarness {
        let h = checkout_history(CHECKOUT_A);
        h.fake_git()
            .add_repo("/repo")
            .unstaged_file("b.rs", "fn b() { 1 }\n")
            .untracked("notes.md");
        h
    }

    /// [`dirty_checkout`] after `Enter` in the confirm of a checkout from the
    /// last line `b2c3d4e5` added, with the checkout landed.
    fn stashed_checkout() -> TestHarness {
        let mut h = dirty_checkout();
        h.type_keys("j j c");
        h.settle();
        h.type_keys("enter");
        h.settle();
        h
    }

    /// `h` after a restart with `--continue`.
    ///
    /// The session saves, a fresh workspace takes the active one's place, and
    /// the save restores into it on the path that `--continue` takes.
    fn restarted(mut h: TestHarness) -> TestHarness {
        let path = PathBuf::from("/state/session.ron");
        let id = h.stoat.active_workspace;
        h.stoat.workspaces[id]
            .save_state(&path, &*h.stoat.fs_host)
            .expect("the session saves");

        let mut fresh = Workspace::new(
            h.stoat.workspaces[id].git_root.clone(),
            &h.stoat.executor,
            h.stoat.redraw_notify.clone(),
        );
        fresh.id = id;
        h.stoat.workspaces[id] = fresh;
        let size = h.stoat.layout_size();
        h.stoat.workspaces[id].layout(size);

        // The replay finishes on the pool during a settle, and the drive after
        // it installs the result and queues the check of HEAD.
        h.stoat.spawn_workspace_restore(id, path);
        h.settle();
        h.stoat.drive_background();
        h.settle();
        h
    }

    /// The stash messages of `/repo`, newest first.
    fn stashes(h: &TestHarness) -> Vec<String> {
        repo(h)
            .stash_list()
            .into_iter()
            .map(|stash| stash.summary)
            .collect()
    }

    /// How many files of `/repo` differ from HEAD, untracked files among them.
    fn changed_files(h: &TestHarness) -> usize {
        repo(h).changed_files().len()
    }

    /// The fake host's `/repo`.
    fn repo(h: &TestHarness) -> Arc<dyn GitRepo> {
        h.stoat
            .git_host
            .discover(Path::new("/repo"))
            .expect("the repository")
    }

    /// A 120-cell window over `a.rs` of a repository that holds `history`, with
    /// `text` in the working tree and the Blame Column on.
    fn blamed_history(history: &[CommitSpec<'_>], text: &str) -> TestHarness {
        let mut h = repo_pane(history, text);
        h.type_keys("space h a");
        h.settle();
        h
    }

    /// A 120-cell window over `a.rs` of a repository that holds `history`, with
    /// `text` in the working tree and the Blame Column off.
    fn repo_pane(history: &[CommitSpec<'_>], text: &str) -> TestHarness {
        let mut h = TestHarness::with_size(120, 24);
        h.seed_linear_history("/repo", history);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_fs().insert_file("/repo/a.rs", text);
        h.open_file(Path::new("/repo/a.rs"));
        h
    }

    /// The unnamed register's contents.
    fn unnamed(h: &TestHarness) -> Option<Vec<String>> {
        h.stoat
            .registers
            .read(Register::Unnamed)
            .map(<[String]>::to_vec)
    }

    /// A 120-cell window with the Commit Preview open on a commit that adds 40
    /// lines, its patch built, so the patch has more rows than the box shows.
    fn long_preview() -> TestHarness {
        let text = {
            let lines: String = (0..40).map(|i| format!("line {i}\n")).collect();
            format!("one\n{lines}")
        };
        let mut h = blamed_history(
            &[
                ("c1", "one", &[("a.rs", "one\n")]),
                ("c2", "two", &[("a.rs", &text)]),
            ],
            &text,
        );
        h.type_keys("j enter");
        h.settle();
        h
    }

    /// A 120-cell window with the Commit Preview open on a commit that edits
    /// `a.rs`, `b.rs`, and `c.rs`, its patch built. The third file adds 20
    /// lines, so its header reaches the top of the 13 rows the patch shows in.
    fn three_file_preview() -> TestHarness {
        let c = {
            let lines: String = (0..20).map(|i| format!("c{i}\n")).collect();
            format!("c\n{lines}")
        };
        let mut h = blamed_history(
            &[
                (
                    "c1",
                    "one",
                    &[("a.rs", "one\n"), ("b.rs", "b\n"), ("c.rs", "c\n")],
                ),
                (
                    "c2",
                    "two",
                    &[("a.rs", "one\ntwo\n"), ("b.rs", "b\nc\n"), ("c.rs", &c)],
                ),
            ],
            "one\ntwo\n",
        );
        h.type_keys("j enter");
        h.settle();
        h
    }

    /// A 120-cell window with the Commit Preview open on the merge `m4`, its
    /// patch built and its box drawn in cells.
    ///
    /// `c1` writes `a.rs`. The side branch `s2` adds `side` to it, and the
    /// mainline `m3` adds `main` and the file `c.rs`. `m4` merges `s2` into
    /// `m3`, so its first parent lacks the `side` line that the preview opens
    /// from.
    fn merge_preview() -> TestHarness {
        let mut h = TestHarness::with_size(120, 24);
        h.fake_git()
            .add_repo("/repo")
            .commit_with_message("c1", "one", &[("a.rs", "one\n")])
            .commit_with_parent_message("s2", "c1", "side", &[("a.rs", "one\nside\n")])
            .commit_with_parent_message(
                "m3",
                "c1",
                "main",
                &[("a.rs", "one\nmain\n"), ("c.rs", "c\n")],
            )
            .merge_commit(
                "m4",
                "s2",
                &[("a.rs", "one\nmain\nside\n"), ("c.rs", "c\n")],
            );
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_fs().insert_file("/repo/a.rs", "one\nmain\nside\n");
        h.open_file(Path::new("/repo/a.rs"));
        h.type_keys("space h a");
        h.settle();
        h.stoat.stoatty = false;
        h.type_keys("j j enter");
        h.settle();
        h
    }

    /// The rows the open preview's patch scrolls down by.
    fn preview_scroll(h: &TestHarness) -> Option<usize> {
        h.stoat
            .commit_preview
            .as_ref()
            .map(|preview| preview.scroll)
    }

    /// The path of the file whose header is the open preview's top row, or
    /// `None` when the top row is no header.
    fn top_header(h: &TestHarness) -> Option<String> {
        let preview = h.stoat.commit_preview.as_ref()?;
        let doc = preview.document.as_ref()?.as_ref()?;
        match preview.rows.get(preview.scroll)? {
            PatchRow::Header { file } => Some(doc.files[*file].rel_path.clone()),
            _ => None,
        }
    }

    /// The open preview's parent index, the files of its patch, whether the
    /// patch rings the origin, whether each file carries its syntax colors, and
    /// the meta line that a cell box draws.
    fn parent_view(h: &mut TestHarness) -> (usize, Vec<String>, bool, bool, String) {
        h.snapshot();
        let meta = h
            .rendered_text()
            .lines()
            .nth(5)
            .map(|row| row.chars().skip(15).take(91).collect::<String>())
            .unwrap_or_default();
        let preview = h
            .stoat
            .commit_preview
            .as_ref()
            .expect("the preview is open");
        let doc = preview
            .document
            .clone()
            .flatten()
            .expect("the patch landed");
        (
            preview.parent,
            doc.files.iter().map(|file| file.rel_path.clone()).collect(),
            preview.origin_row.is_some(),
            doc.files
                .iter()
                .all(|file| file.buffer_highlights.is_some()),
            meta.trim_end().to_string(),
        )
    }

    /// Whether the open preview's build landed.
    fn preview_built(h: &TestHarness) -> bool {
        h.stoat
            .commit_preview
            .as_ref()
            .is_some_and(|preview| preview.document.is_some())
    }

    /// The commit and the row of the open card.
    fn card_line(h: &TestHarness) -> Option<(String, u32)> {
        h.stoat
            .blame_card
            .as_ref()
            .map(|card| (card.commit.info.sha.clone(), card.row))
    }

    /// Whether the focused split pane holds the blame flag.
    fn blamed(h: &TestHarness) -> bool {
        let ws = h.stoat.active_workspace();
        ws.panes.pane(ws.panes.focus()).blame
    }

    fn review_badge(h: &TestHarness) -> Option<String> {
        let ws = h.stoat.active_workspace();
        ws.badges
            .find_by_source(BadgeSource::Review)
            .and_then(|id| ws.badges.get(id))
            .map(|badge| badge.label.clone())
    }
}

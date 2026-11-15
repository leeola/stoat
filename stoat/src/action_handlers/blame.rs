use crate::{
    action_handlers::review,
    app::{Stoat, UpdateEffect},
    blame::{self, BlameCard, CommitDetails, LineBlame},
    buffer::BufferId,
    commit_preview::{self, PreviewOrigin},
    editor_state::EditorId,
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
            stoat.set_status("line is not committed");
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
        Some((_, _, LineBlame::Uncommitted)) => Err("line is not committed"),
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
        host::ClipboardKind,
        keymap_state,
        lsp::hover::{HoverOutcome, HoverResponse},
        pane::View,
        register::Register,
        render::{commit_patch::PatchRow, paint, OVERLAY_LANE},
        test_fixture,
        test_harness::{CommitSpec, TestHarness},
        theme::scope,
    };
    use crossterm::event::MouseEventKind;
    use ratatui::layout::Rect;
    use std::{
        path::{Path, PathBuf},
        time::Duration,
    };
    use stoat_action::{BlameCard, BlamePreview, SetTheme, ToggleBlame};
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

    /// A 120-cell window over `a.rs` of a repository that holds `history`, with
    /// `text` in the working tree and the Blame Column on.
    fn blamed_history(history: &[CommitSpec<'_>], text: &str) -> TestHarness {
        let mut h = TestHarness::with_size(120, 24);
        h.seed_linear_history("/repo", history);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_fs().insert_file("/repo/a.rs", text);
        h.open_file(Path::new("/repo/a.rs"));
        h.type_keys("space h a");
        h.settle();
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

    /// The rows the open preview's patch scrolls down by.
    fn preview_scroll(h: &TestHarness) -> Option<usize> {
        h.stoat
            .commit_preview
            .as_ref()
            .map(|preview| preview.scroll)
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

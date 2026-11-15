use crate::{
    action_handlers::review,
    app::{Stoat, UpdateEffect},
    blame::{self, BlameCard, CommitDetails, LineBlame},
    buffer::BufferId,
    editor_state::EditorId,
    keymap_state::{self, StoatKeymapState},
    pane::{FocusTarget, View},
    render::editor,
};
use std::sync::Arc;
use stoat_scheduler::Task;

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
            stoat.set_status("blame is loading");
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

/// Close the Blame Card once its line is gone from under the cursor.
///
/// The card stays while no modal is open, the focused pane still blames, the
/// cursor stays on the card's row, and the card's commit still holds that row.
/// Any other state closes it.
pub(crate) fn sync_card(stoat: &mut Stoat) {
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
    use crate::{
        action_handlers::dispatch,
        app::Stoat,
        badge::BadgeSource,
        lsp::hover::{HoverOutcome, HoverResponse},
        pane::View,
        test_harness::{CommitSpec, TestHarness},
    };
    use std::path::{Path, PathBuf};
    use stoat_action::{BlameCard, ToggleBlame};

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
                vec![("Esc".to_string(), "close".to_string())],
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

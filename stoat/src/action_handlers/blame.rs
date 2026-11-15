use crate::{
    action_handlers::review,
    app::{Stoat, UpdateEffect},
    pane::{FocusTarget, View},
};

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
    UpdateEffect::Redraw
}

#[cfg(test)]
mod tests {
    use crate::{
        action_handlers::dispatch, app::Stoat, badge::BadgeSource, pane::View,
        test_harness::TestHarness,
    };
    use std::path::Path;
    use stoat_action::ToggleBlame;

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

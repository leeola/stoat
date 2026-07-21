use crate::{
    action_handlers::pane::{dispose_view, EditorDisposal},
    app::{Stoat, UpdateEffect},
};

/// Switch to the 1-based tab `index`, reporting a miss in the status line.
pub(super) fn goto_tab(stoat: &mut Stoat, index: usize) -> UpdateEffect {
    let Some(target) = index.checked_sub(1) else {
        stoat.set_status("no tab 0");
        return UpdateEffect::Redraw;
    };
    if target >= stoat.active_workspace().tabs.len() {
        stoat.set_status(format!("no tab {index}"));
        return UpdateEffect::Redraw;
    }
    stoat.active_workspace_mut().switch_tab(target);
    relayout(stoat);
    UpdateEffect::Redraw
}

/// Append a tab on a fresh scratch buffer and switch to it.
pub(super) fn new_tab(stoat: &mut Stoat) -> UpdateEffect {
    let executor = stoat.executor.clone();
    stoat.active_workspace_mut().new_tab(&executor);
    relayout(stoat);
    UpdateEffect::Redraw
}

/// Switch back to the previously active tab, reporting when there is none.
pub(super) fn toggle_tab(stoat: &mut Stoat) -> UpdateEffect {
    if !stoat.active_workspace_mut().toggle_tab() {
        stoat.set_status("no previous tab");
        return UpdateEffect::Redraw;
    }
    relayout(stoat);
    UpdateEffect::Redraw
}

/// Close the active tab and release every view its layout held.
///
/// Editors go through the referenced check rather than being dropped outright,
/// since tabs share a workspace's editors and another tab may still show one.
pub(super) fn close_tab(stoat: &mut Stoat) -> UpdateEffect {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let active = ws.active_tab;
    let Some(closed) = ws.close_tab(active) else {
        stoat.set_status("cannot close the last tab");
        return UpdateEffect::Redraw;
    };

    let views: Vec<_> = closed
        .split_panes()
        .map(|(_, pane)| pane.view.clone())
        .collect();
    for view in views {
        dispose_view(ws, &executor, view, EditorDisposal::GcIfUnreferenced);
    }

    relayout(stoat);
    UpdateEffect::Redraw
}

/// Re-layout the newly active tab's tree to the terminal size, so the first
/// render after a switch shows correctly-sized panes rather than the
/// zero-sized rects a parked tree was stored with.
fn relayout(stoat: &mut Stoat) {
    let size = stoat.size();
    stoat.active_workspace_mut().layout(size);
}

#[cfg(test)]
mod tests {
    use crate::{app::Stoat, pane::View, test_harness::TestHarness};

    fn dispatch(h: &mut TestHarness, action: &dyn stoat_action::Action) {
        crate::action_handlers::dispatch(&mut h.stoat, action);
    }

    #[test]
    fn goto_tab_switches_by_one_based_number() {
        let mut h = Stoat::test();
        dispatch(&mut h, &stoat_action::NewTab);
        assert_eq!(h.stoat.active_workspace().active_tab, 1);

        dispatch(&mut h, &stoat_action::GotoTab { index: 1 });
        assert_eq!(h.stoat.active_workspace().active_tab, 0, "1 is the first");

        dispatch(&mut h, &stoat_action::GotoTab { index: 2 });
        assert_eq!(h.stoat.active_workspace().active_tab, 1);
    }

    #[test]
    fn goto_tab_past_the_end_reports_and_stays_put() {
        let mut h = Stoat::test();
        dispatch(&mut h, &stoat_action::NewTab);
        let before = h.stoat.active_workspace().active_tab;

        dispatch(&mut h, &stoat_action::GotoTab { index: 9 });

        assert_eq!(h.stoat.active_workspace().active_tab, before);
        assert_eq!(h.stoat.pending_message.as_deref(), Some("no tab 9"));
    }

    #[test]
    fn toggle_tab_reports_when_there_is_nowhere_to_go_back_to() {
        let mut h = Stoat::test();
        dispatch(&mut h, &stoat_action::ToggleTab);
        assert_eq!(h.stoat.pending_message.as_deref(), Some("no previous tab"));
    }

    #[test]
    fn close_tab_refuses_on_the_last_one() {
        let mut h = Stoat::test();
        dispatch(&mut h, &stoat_action::CloseTab);

        assert_eq!(h.stoat.active_workspace().tabs.len(), 1);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("cannot close the last tab")
        );
    }

    /// Asserting the tab count alone would pass against a close that leaked
    /// every view the tab held, so this follows both halves of the disposal
    /// contract: the terminal's session is dropped, and an editor another tab
    /// still shows is spared.
    #[test]
    fn close_tab_kills_its_terminal_but_spares_a_shared_editor() {
        let mut h = Stoat::test();
        let fake = std::sync::Arc::new(crate::host::FakeTerminalSession::new());
        h.stoat.terminal_host = std::sync::Arc::new(crate::host::FakeTerminalHost::new(fake));
        h.allow_host_swap();

        // Tab 0 keeps showing the scratch editor the workspace opened with.
        let ws = h.stoat.active_workspace();
        let View::Editor(shared_editor) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("the first tab shows an editor");
        };

        // Tab 1 splits a terminal against tab 0's editor, so both views go
        // through the close. That is what makes each half of the assertion
        // below meaningful rather than vacuous.
        dispatch(&mut h, &stoat_action::NewTab);
        dispatch(&mut h, &stoat_action::Terminal);
        let ws = h.stoat.active_workspace_mut();
        let View::Terminal(term_id) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("the second tab shows a terminal");
        };
        let sibling = ws.panes.split(crate::pane::Axis::Vertical);
        ws.panes.pane_mut(sibling).view = View::Editor(shared_editor);
        assert!(ws.terms.contains_key(term_id));

        dispatch(&mut h, &stoat_action::CloseTab);

        let ws = h.stoat.active_workspace();
        assert_eq!(ws.tabs.len(), 1);
        assert!(
            !ws.terms.contains_key(term_id),
            "the closed tab's terminal session is released"
        );
        assert!(
            ws.editors.contains_key(shared_editor),
            "an editor another tab still shows survives"
        );
    }
}

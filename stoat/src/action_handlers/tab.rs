use crate::{
    action_handlers::pane::{dispose_view, EditorDisposal},
    app::{Stoat, UpdateEffect},
    pane::View,
};
use stoat_config::TabBarMode;

/// Switch to the 1-based tab `index`, reporting a miss in the status line.
pub(crate) fn goto_tab(stoat: &mut Stoat, index: usize) -> UpdateEffect {
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

/// Switch to the next tab in display order, wrapping past the last.
pub(super) fn next_tab(stoat: &mut Stoat) -> UpdateEffect {
    cycle_tab(stoat, 1)
}

/// Switch to the previous tab in display order, wrapping past the first.
pub(super) fn prev_tab(stoat: &mut Stoat) -> UpdateEffect {
    cycle_tab(stoat, -1)
}

/// Step the active tab by `dir` (`+1` next, `-1` previous) with wraparound,
/// reporting when the workspace has no other tab to move to.
fn cycle_tab(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let len = ws.tabs.len();
    if len < 2 {
        stoat.set_status("no other tab");
        return UpdateEffect::Redraw;
    }

    let step = if dir >= 0 { 1 } else { len - 1 };
    let target = (ws.active_tab + step) % len;
    ws.switch_tab(target);
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
/// A shell that a pane of the tab covered ends with the tab, unless another
/// view shows it, and so does a commits list a pane covered.
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
    let covered: Vec<_> = closed
        .split_panes()
        .filter_map(|(_, pane)| pane.prev_view.clone())
        .collect();
    for view in views {
        dispose_view(ws, &executor, view, EditorDisposal::GcIfUnreferenced);
    }
    for view in covered {
        match view {
            View::Terminal(term_id) if !ws.term_shown(term_id) => dispose_view(
                ws,
                &executor,
                View::Terminal(term_id),
                EditorDisposal::GcIfUnreferenced,
            ),
            View::Commits(list) => dispose_view(
                ws,
                &executor,
                View::Commits(list),
                EditorDisposal::GcIfUnreferenced,
            ),
            _ => {},
        }
    }

    relayout(stoat);
    UpdateEffect::Redraw
}

/// Show the tab bar when it is hidden and hide it when it is showing.
///
/// The flip is expressed as an explicit mode rather than a boolean, so a later
/// config reload changing `ui.tab_bar` does not silently invert what the user
/// asked for this session.
pub(super) fn toggle_tab_bar(stoat: &mut Stoat) -> UpdateEffect {
    let (mode, status) = if stoat.tab_bar_visible() {
        (TabBarMode::Never, "tab bar hidden")
    } else {
        (TabBarMode::Always, "tab bar shown")
    };
    stoat.tab_bar_override = Some(mode);
    stoat.set_status(status);
    UpdateEffect::Redraw
}

/// Set, clear, or prompt for the active tab's title.
///
/// A `Some` name sets the override, or clears it when empty or all whitespace,
/// so the tab falls back to its derived title. `None` opens the command palette
/// seeded for `tab-rename`, which is how the bare keybinding reaches inline
/// name entry.
pub(super) fn rename_tab(stoat: &mut Stoat, name: Option<&str>) -> UpdateEffect {
    let Some(name) = name else {
        return super::palette::open_palette_seeded(stoat, "tab-rename ");
    };

    let trimmed = name.trim();
    let status = {
        let ws = stoat.active_workspace_mut();
        let active = ws.active_tab;
        if trimmed.is_empty() {
            ws.tabs[active].name = None;
            "tab name cleared".to_string()
        } else {
            ws.tabs[active].name = Some(trimmed.to_string());
            format!("tab renamed to {trimmed}")
        }
    };

    stoat.set_status(status);
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
    use crate::{app::Stoat, pane::View, term_session::TermId, test_harness::TestHarness};
    use stoat_config::TabBarMode;

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
    fn next_and_prev_cycle_with_wraparound() {
        let mut h = Stoat::test();
        dispatch(&mut h, &stoat_action::NewTab);
        dispatch(&mut h, &stoat_action::NewTab);
        assert_eq!(h.stoat.active_workspace().active_tab, 2, "on the last tab");

        dispatch(&mut h, &stoat_action::NextTab);
        assert_eq!(
            h.stoat.active_workspace().active_tab,
            0,
            "next from the last wraps to the first"
        );

        dispatch(&mut h, &stoat_action::PrevTab);
        assert_eq!(
            h.stoat.active_workspace().active_tab,
            2,
            "prev from the first wraps to the last"
        );
    }

    #[test]
    fn next_records_the_origin_as_the_toggle_target() {
        let mut h = Stoat::test();
        dispatch(&mut h, &stoat_action::NewTab);
        dispatch(&mut h, &stoat_action::GotoTab { index: 1 });
        assert_eq!(h.stoat.active_workspace().active_tab, 0);

        dispatch(&mut h, &stoat_action::NextTab);
        dispatch(&mut h, &stoat_action::ToggleTab);

        assert_eq!(
            h.stoat.active_workspace().active_tab,
            0,
            "toggle returns to the tab next moved away from"
        );
    }

    #[test]
    fn cycling_a_single_tab_reports_and_stays_put() {
        let mut h = Stoat::test();
        dispatch(&mut h, &stoat_action::NextTab);

        assert_eq!(h.stoat.active_workspace().active_tab, 0);
        assert_eq!(h.stoat.active_workspace().tabs.len(), 1);
        assert_eq!(h.stoat.pending_message.as_deref(), Some("no other tab"));
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

    /// Auto is the shipped default, so a single-tab session must look exactly
    /// as it did before tabs existed. Both states are measured on the same
    /// workspace, since a height assertion against one state alone proves
    /// nothing about what the row costs.
    #[test]
    fn auto_reveals_the_bar_only_once_a_second_tab_exists() {
        let mut h = Stoat::test();
        h.stoat.settings.ui_tab_bar = Some(TabBarMode::Auto);
        let full = h.stoat.size().height;

        assert!(!h.stoat.tab_bar_visible(), "one tab needs no bar");
        assert_eq!(h.stoat.layout_size().height, full);

        dispatch(&mut h, &stoat_action::NewTab);

        assert!(h.stoat.tab_bar_visible(), "a second tab reveals it");
        assert_eq!(
            h.stoat.layout_size().height,
            full - 1,
            "the bar costs the panes exactly one row"
        );
        assert_eq!(h.stoat.layout_size().y, 1, "and panes start below it");
    }

    #[test]
    fn always_and_never_ignore_the_tab_count() {
        let mut h = Stoat::test();

        h.stoat.settings.ui_tab_bar = Some(TabBarMode::Always);
        assert!(h.stoat.tab_bar_visible(), "always shows with one tab");

        h.stoat.settings.ui_tab_bar = Some(TabBarMode::Never);
        dispatch(&mut h, &stoat_action::NewTab);
        assert!(!h.stoat.tab_bar_visible(), "never stays hidden with two");
    }

    #[test]
    fn toggling_the_bar_flips_it_and_reports() {
        let mut h = Stoat::test();
        h.stoat.settings.ui_tab_bar = Some(TabBarMode::Always);

        dispatch(&mut h, &stoat_action::ToggleTabBar);
        assert!(!h.stoat.tab_bar_visible());
        assert_eq!(h.stoat.pending_message.as_deref(), Some("tab bar hidden"));

        dispatch(&mut h, &stoat_action::ToggleTabBar);
        assert!(h.stoat.tab_bar_visible());
        assert_eq!(h.stoat.pending_message.as_deref(), Some("tab bar shown"));
    }

    /// The row has to carry both numbered labels and distinguish the active tab
    /// visually, or it reports the count without answering which one you are on.
    #[test]
    fn the_bar_row_numbers_both_tabs_and_marks_the_active_one() {
        let mut h = Stoat::test();
        dispatch(&mut h, &stoat_action::NewTab);
        // The bar paints as APC components, so it only reaches cells once the
        // scene is composited, exactly as a real terminal shows it.
        let buf = h.render_composited();

        let row: String = (0..buf.area.width).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(row.contains("1:"), "the first tab is numbered, got {row:?}");
        assert!(
            row.contains("2:"),
            "the second tab is numbered, got {row:?}"
        );

        let style_at = |needle: &str| {
            let col = row.find(needle).expect("label painted") as u16;
            *buf[(col, 0)].style().bg.as_ref().expect("a background")
        };
        assert_ne!(
            style_at("2:"),
            style_at("1:"),
            "the active tab reads differently from the inactive one"
        );
    }

    /// A tab showing a file names it, so the bar tells tabs apart by what they
    /// hold rather than by number alone. The parked tab is titled from its own
    /// stored tree, which is the half a title read off the active tree would
    /// silently get wrong.
    #[test]
    fn a_tab_is_titled_by_the_file_it_shows() {
        let mut h = Stoat::test();
        let path = h.write_file("notes.md", "hello");
        h.open_file(&path);

        dispatch(&mut h, &stoat_action::NewTab);

        let ws = h.stoat.active_workspace();
        assert_eq!(ws.tab_title(0), "notes.md", "the parked tab keeps its file");
        assert_eq!(ws.tab_title(1), "scratch", "a fresh tab has no file yet");
        assert_eq!(ws.tab_title(9), "", "an out-of-range index is empty");
    }

    /// A pane in the diff view names the diff by its two sides, as the sides
    /// bar over it does. The view lives on the editor, so a parked tab keeps
    /// the name, and closing the view gives the tab back to its file.
    #[test]
    fn a_tab_in_the_diff_view_is_named_by_the_diffs_sides() {
        let mut h = Stoat::test();
        h.stage_review_scenario("/repo", &[("a.rs", "fn a() {}\n", "fn a() { 1 }\n")]);
        dispatch(&mut h, &stoat_action::NewTab);
        h.type_text(":diff");
        h.type_keys("enter");
        h.settle();

        dispatch(&mut h, &stoat_action::NewTab);
        let ws = h.stoat.active_workspace();
        assert_eq!(
            [ws.tab_title(1), ws.tab_title(2)],
            ["HEAD → working tree", "scratch"],
            "a parked diff tab keeps the diff's name and the fresh tab reads scratch",
        );

        dispatch(&mut h, &stoat_action::PrevTab);
        h.type_text(":diff");
        h.type_keys("enter");
        h.settle();
        assert_eq!(
            h.stoat.active_workspace().tab_title(1),
            "a.rs",
            "closing the view names the file the view crossed into",
        );
    }

    /// The commits screen covers the active tab's focused pane, so that tab
    /// reads as the list. A parked tab never shows the list and keeps its file.
    #[test]
    fn a_tab_under_the_commits_screen_reads_commits() {
        let mut h = Stoat::test();
        h.seed_linear_history(
            "/repo",
            &[("aaaa1111", "feat: add a.rs", &[("a.rs", "fn a() {}\n")])],
        );
        let path = h.write_file("notes.md", "hello");
        h.open_file(&path);
        dispatch(&mut h, &stoat_action::NewTab);
        h.open_commits("/repo");

        let ws = h.stoat.active_workspace();
        assert_eq!(
            [ws.tab_title(0), ws.tab_title(1)],
            ["notes.md", "commits"],
            "the overlay names the active tab and leaves the parked one its file",
        );

        dispatch(&mut h, &stoat_action::CloseCommits);
        assert_eq!(
            h.stoat.active_workspace().tab_title(1),
            "scratch",
            "the empty scratch names the tab once it is the visible item",
        );
    }

    /// The rebase todo opens from the commits list and leaves the list in its
    /// pane beneath it. The tab names what the todo covers, as for the other
    /// rebase screens, and the todo covers the list.
    #[test]
    fn a_tab_under_the_rebase_screen_keeps_its_panes_name() {
        let mut h = Stoat::test();
        h.seed_linear_history(
            "/repo",
            &[
                ("aaaa1111", "feat: add a.rs", &[("a.rs", "fn a() {}\n")]),
                ("bbbb2222", "feat: grow a.rs", &[("a.rs", "fn a() { 1 }\n")]),
            ],
        );
        h.open_commits("/repo");
        h.type_keys("G");
        h.type_keys("i");

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                ws.rebase.is_some(),
                ws.focused_commits().is_some(),
                ws.tab_title(0)
            ),
            (true, true, "commits".to_string()),
            "the todo stands over the list, and the tab keeps its pane's name",
        );
    }

    /// A tab on a shell or an agent reads as what the child says it runs. A
    /// parked tab follows its session too, and the cut keeps a long title from
    /// filling the bar.
    #[test]
    fn a_terminal_tab_takes_the_childs_title_and_falls_back_to_its_kind() {
        let mut h = Stoat::test();
        dispatch(&mut h, &stoat_action::NewTab);
        dispatch(&mut h, &stoat_action::Terminal);
        let term_id = focused_terminal(&h);
        let titled = |h: &mut TestHarness, bytes: &[u8]| {
            h.stoat.active_workspace_mut().terms[term_id]
                .term
                .feed(bytes);
            h.stoat.active_workspace().tab_title(1)
        };

        let named = titled(&mut h, b"\x1b]0;build\x07");
        dispatch(&mut h, &stoat_action::PrevTab);
        let long = titled(&mut h, format!("\x1b]0;{}\x07", "x".repeat(30)).as_bytes());
        let cleared = titled(&mut h, b"\x1b]2;\x07");

        assert_eq!(
            [named, long, cleared],
            ["build".to_string(), "x".repeat(24), "term".to_string()],
        );
    }

    #[test]
    fn an_agent_tab_takes_the_childs_title() {
        let mut h = Stoat::test();
        dispatch(&mut h, &stoat_action::Terminal);
        let term_id = focused_terminal(&h);
        let ws = h.stoat.active_workspace_mut();
        let focus = ws.panes.focus();
        ws.panes.pane_mut(focus).view = View::Agent(term_id);

        let untitled = ws.tab_title(ws.active_tab);
        ws.terms[term_id].term.feed(b"\x1b]0;claude\x07");

        assert_eq!(
            [untitled, ws.tab_title(ws.active_tab)],
            ["agent".to_string(), "claude".to_string()],
        );
    }

    #[test]
    fn a_renamed_tab_keeps_its_name_over_a_terminal_title() {
        let mut h = Stoat::test();
        dispatch(&mut h, &stoat_action::Terminal);
        dispatch(
            &mut h,
            &stoat_action::RenameTab {
                name: Some("ops".to_string()),
            },
        );
        let term_id = focused_terminal(&h);

        h.stoat.active_workspace_mut().terms[term_id]
            .term
            .feed(b"\x1b]0;build\x07");

        let ws = h.stoat.active_workspace();
        assert_eq!(ws.tab_title(ws.active_tab), "ops");
    }

    /// The terminal session the focused pane shows.
    fn focused_terminal(h: &TestHarness) -> TermId {
        let ws = h.stoat.active_workspace();
        let View::Terminal(term_id) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("the focused pane shows a terminal");
        };
        term_id
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

    #[test]
    fn rename_with_a_name_titles_the_active_tab() {
        let mut h = Stoat::test();

        dispatch(
            &mut h,
            &stoat_action::RenameTab {
                name: Some("hello".to_string()),
            },
        );

        let ws = h.stoat.active_workspace();
        assert_eq!(ws.tab_title(ws.active_tab), "hello");
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("tab renamed to hello")
        );
    }

    #[test]
    fn rename_with_a_whitespace_name_clears_the_override() {
        let mut h = Stoat::test();
        dispatch(
            &mut h,
            &stoat_action::RenameTab {
                name: Some("hello".to_string()),
            },
        );

        dispatch(
            &mut h,
            &stoat_action::RenameTab {
                name: Some("   ".to_string()),
            },
        );

        let ws = h.stoat.active_workspace();
        assert_eq!(
            ws.tabs[ws.active_tab].name, None,
            "a whitespace name clears the override"
        );
        assert_eq!(h.stoat.pending_message.as_deref(), Some("tab name cleared"));
    }

    /// Bare `RenameTab` has to land the user in inline entry, so it opens the
    /// palette already parsing `tab-rename` as a command with a pending argument
    /// rather than as filter text.
    #[test]
    fn bare_rename_opens_the_palette_seeded_in_arg_mode() {
        let mut h = Stoat::test();

        dispatch(&mut h, &stoat_action::RenameTab { name: None });

        let active_idx = h.stoat.active_workspace;
        let ws = &h.stoat.workspaces[active_idx];
        let palette = h.stoat.command_palette.as_ref().expect("the palette opens");
        assert_eq!(palette.input.text(ws), "tab-rename ");
        assert_eq!(
            palette.command.map(|entry| entry.def.name()),
            Some("RenameTab"),
            "the seed parses into arg mode, not filter text"
        );
    }
}

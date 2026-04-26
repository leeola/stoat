mod claude;
mod commits;
mod conflict;
mod file;
mod file_finder;
mod help;
mod movement;
mod palette;
mod pane;
mod prompt;
mod rebase;
mod review;
mod reword;
mod run;
mod workspace;

use crate::{
    app::{Stoat, UpdateEffect},
    command_palette::CommandPalette,
    editor_state::EditorState,
    help::Help,
    host::FsHost,
    pane::{Axis, Direction, DockSide, FocusTarget, View},
    workspace_picker::WorkspacePicker,
};
pub(crate) use claude::handle_follow_tool_use;
pub(crate) use commits::pump_commits;
pub(crate) use file_finder::close_file_finder;
#[cfg(test)]
pub(crate) use review::install_review_session;
use std::path::Path;
use stoat_action::{
    Action, ActionKind, Dump, OpenFile, OpenReviewAgentEdits, OpenReviewCommit,
    OpenReviewCommitRange, RenameWorkspace, Run,
};

pub fn dispatch(stoat: &mut Stoat, action: &dyn Action) -> UpdateEffect {
    let effect = match action.kind() {
        ActionKind::Quit => {
            if pane::close_focused_pane(stoat) {
                UpdateEffect::Redraw
            } else {
                UpdateEffect::Quit
            }
        },
        // FIXME: prompt on unsaved buffers once dirty tracking exists
        ActionKind::QuitAll => UpdateEffect::Quit,
        ActionKind::SplitRight => pane::split_pane(stoat, Axis::Vertical),
        ActionKind::SplitDown => pane::split_pane(stoat, Axis::Horizontal),
        ActionKind::SplitNewRight => pane::split_pane_new(stoat, Axis::Vertical),
        ActionKind::SplitNewDown => pane::split_pane_new(stoat, Axis::Horizontal),
        ActionKind::FocusLeft => {
            pane::focus_direction(stoat, Direction::Left);
            UpdateEffect::Redraw
        },
        ActionKind::FocusRight => {
            pane::focus_direction(stoat, Direction::Right);
            UpdateEffect::Redraw
        },
        ActionKind::FocusUp => {
            stoat
                .active_workspace_mut()
                .panes
                .focus_direction(Direction::Up);
            UpdateEffect::Redraw
        },
        ActionKind::FocusDown => {
            stoat
                .active_workspace_mut()
                .panes
                .focus_direction(Direction::Down);
            UpdateEffect::Redraw
        },
        ActionKind::FocusNext => {
            stoat.active_workspace_mut().panes.focus_next();
            UpdateEffect::Redraw
        },
        ActionKind::FocusPrev => {
            stoat.active_workspace_mut().panes.focus_prev();
            UpdateEffect::Redraw
        },
        ActionKind::ClosePane => {
            pane::close_focused_pane(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::CloseOtherPanes => {
            pane::close_other_panes(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::OpenFile => {
            let open = action
                .as_any()
                .downcast_ref::<OpenFile>()
                .expect("OpenFile action downcast");
            file::open_file(stoat, &open.path);
            UpdateEffect::Redraw
        },
        ActionKind::OpenFileFinder => {
            file_finder::open_file_finder(stoat, crate::file_finder::OpenIntent::Replace)
        },
        ActionKind::OpenFileFinderHSplit => {
            file_finder::open_file_finder(stoat, crate::file_finder::OpenIntent::HSplit)
        },
        ActionKind::OpenFileFinderVSplit => {
            file_finder::open_file_finder(stoat, crate::file_finder::OpenIntent::VSplit)
        },
        ActionKind::FileFinderSelectPrev => file_finder::file_finder_move_selection(stoat, -1),
        ActionKind::FileFinderSelectNext => file_finder::file_finder_move_selection(stoat, 1),
        ActionKind::FileFinderScopeToggle => file_finder::file_finder_scope_toggle(stoat),
        ActionKind::OpenCommandPalette => {
            let previous_mode = stoat.mode.clone();
            let executor = stoat.executor.clone();
            let availability = crate::command_palette::Availability::from_stoat(stoat);
            let ws = stoat.active_workspace_mut();
            stoat.command_palette = Some(CommandPalette::new(
                ws,
                executor,
                previous_mode,
                availability,
            ));
            stoat.mode = "prompt".into();
            UpdateEffect::Redraw
        },
        ActionKind::OpenHelp => {
            let active = stoat.active_bindings_for_current_mode();
            let mode = stoat.mode.clone();
            let executor = stoat.executor.clone();
            let previous_mode = stoat.mode.clone();
            let ws = stoat.active_workspace_mut();
            stoat.help = Some(Help::new(&mode, active, ws, executor, previous_mode));
            stoat.mode = "prompt".into();
            UpdateEffect::Redraw
        },
        ActionKind::OpenReview => {
            review::open_review(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::AddSelectionBelow => movement::add_selection_below(stoat),
        ActionKind::AddSelectionAbove => movement::add_selection_above(stoat),
        ActionKind::SplitSelectionOnNewline => movement::split_selection_on_newline(stoat),
        ActionKind::Increment => movement::increment(stoat),
        ActionKind::Decrement => movement::decrement(stoat),
        ActionKind::MoveLeft => movement::move_horizontal(stoat, -1, false),
        ActionKind::MoveRight => movement::move_horizontal(stoat, 1, false),
        ActionKind::MoveUp => movement::move_vertical(stoat, -1, false),
        ActionKind::MoveDown => movement::move_vertical(stoat, 1, false),
        ActionKind::PageUp => movement::page_motion(stoat, movement::PageDir::Up, false),
        ActionKind::PageDown => movement::page_motion(stoat, movement::PageDir::Down, false),
        ActionKind::HalfPageUp => movement::page_motion(stoat, movement::PageDir::Up, true),
        ActionKind::HalfPageDown => movement::page_motion(stoat, movement::PageDir::Down, true),
        ActionKind::MoveNextWordStart => {
            movement::move_word(stoat, movement::WordTarget::NextStart, false)
        },
        ActionKind::MoveNextWordEnd => {
            movement::move_word(stoat, movement::WordTarget::NextEnd, false)
        },
        ActionKind::MovePrevWordStart => {
            movement::move_word(stoat, movement::WordTarget::PrevStart, false)
        },
        ActionKind::MovePrevWordEnd => {
            movement::move_word(stoat, movement::WordTarget::PrevEnd, false)
        },
        ActionKind::MoveNextLongWordStart => {
            movement::move_word(stoat, movement::WordTarget::NextLongStart, false)
        },
        ActionKind::MoveNextLongWordEnd => {
            movement::move_word(stoat, movement::WordTarget::NextLongEnd, false)
        },
        ActionKind::MovePrevLongWordStart => {
            movement::move_word(stoat, movement::WordTarget::PrevLongStart, false)
        },
        ActionKind::MovePrevLongWordEnd => {
            movement::move_word(stoat, movement::WordTarget::PrevLongEnd, false)
        },
        ActionKind::ExtendLeft => movement::move_horizontal(stoat, -1, true),
        ActionKind::ExtendRight => movement::move_horizontal(stoat, 1, true),
        ActionKind::ExtendUp => movement::move_vertical(stoat, -1, true),
        ActionKind::ExtendDown => movement::move_vertical(stoat, 1, true),
        ActionKind::ExtendNextWordStart => {
            movement::move_word(stoat, movement::WordTarget::NextStart, true)
        },
        ActionKind::ExtendNextWordEnd => {
            movement::move_word(stoat, movement::WordTarget::NextEnd, true)
        },
        ActionKind::ExtendPrevWordStart => {
            movement::move_word(stoat, movement::WordTarget::PrevStart, true)
        },
        ActionKind::ExtendPrevWordEnd => {
            movement::move_word(stoat, movement::WordTarget::PrevEnd, true)
        },
        ActionKind::GotoLineStart => movement::goto_line_start(stoat, false),
        ActionKind::GotoLineEnd => movement::goto_line_end(stoat, false),
        ActionKind::GotoFirstNonwhitespace => movement::goto_first_nonwhitespace(stoat, false),
        ActionKind::GotoFileStart => movement::goto_file_start(stoat, false),
        ActionKind::GotoLastLine => movement::goto_last_line(stoat, false),
        ActionKind::GotoWindowTop => movement::goto_window(stoat, movement::WindowAlign::Top),
        ActionKind::GotoWindowCenter => movement::goto_window(stoat, movement::WindowAlign::Center),
        ActionKind::GotoWindowBottom => movement::goto_window(stoat, movement::WindowAlign::Bottom),
        ActionKind::AlignViewTop => movement::align_view(stoat, movement::ViewAlign::Top),
        ActionKind::AlignViewCenter => movement::align_view(stoat, movement::ViewAlign::Center),
        ActionKind::AlignViewBottom => movement::align_view(stoat, movement::ViewAlign::Bottom),
        ActionKind::ScrollUp => movement::scroll_view(stoat, movement::ScrollDir::Up),
        ActionKind::ScrollDown => movement::scroll_view(stoat, movement::ScrollDir::Down),
        ActionKind::SwitchCase => movement::switch_case(stoat),
        ActionKind::SwitchToUppercase => movement::switch_to_uppercase(stoat),
        ActionKind::SwitchToLowercase => movement::switch_to_lowercase(stoat),
        ActionKind::ExtendToLineStart => movement::goto_line_start(stoat, true),
        ActionKind::ExtendToLineEnd => movement::goto_line_end(stoat, true),
        ActionKind::ExtendToFileStart => movement::goto_file_start(stoat, true),
        ActionKind::ExtendToLastLine => movement::goto_last_line(stoat, true),
        ActionKind::CollapseSelection => movement::collapse_selection(stoat),
        ActionKind::FlipSelections => movement::flip_selections(stoat),
        ActionKind::SelectAll => movement::select_all(stoat),
        ActionKind::SelectLineBelow => movement::select_line_below(stoat),
        ActionKind::KeepPrimarySelection => movement::keep_primary_selection(stoat),
        ActionKind::RotateSelectionsForward => movement::rotate_selections_forward(stoat),
        ActionKind::RotateSelectionsBackward => movement::rotate_selections_backward(stoat),
        ActionKind::TrimSelections => movement::trim_selections(stoat),
        ActionKind::OpenRun => run::open_run(stoat),
        ActionKind::RunSubmit => run::run_submit(stoat),
        ActionKind::RunInterrupt => run::run_interrupt(stoat),
        ActionKind::RunHistoryPrev => run::run_history_prev(stoat),
        ActionKind::RunHistoryNext => run::run_history_next(stoat),
        ActionKind::HelpSelectPrev => help::help_select_prev(stoat),
        ActionKind::HelpSelectNext => help::help_select_next(stoat),
        ActionKind::HelpScopeToggle => help::help_scope_toggle(stoat),
        ActionKind::HelpScrollDetailUp => help::help_scroll_detail_up(stoat),
        ActionKind::HelpScrollDetailDown => help::help_scroll_detail_down(stoat),
        ActionKind::HelpJumpFirst => help::help_jump_first(stoat),
        ActionKind::HelpJumpLast => help::help_jump_last(stoat),
        ActionKind::CloseHelp => help::help_cancel(stoat),
        ActionKind::Run => {
            let cmd = action
                .as_any()
                .downcast_ref::<Run>()
                .expect("Run action downcast");
            run::run_command(stoat, &cmd.command)
        },
        ActionKind::OpenClaude => claude::open_claude(stoat),
        ActionKind::ClaudeSubmit => claude::claude_submit(stoat),
        ActionKind::ClaudeToPane => claude::claude_to_pane(stoat),
        ActionKind::ClaudeToDockLeft => claude::claude_to_dock(stoat, DockSide::Left),
        ActionKind::ClaudeToDockRight => claude::claude_to_dock(stoat, DockSide::Right),
        ActionKind::ClaudeToggleFollow => claude::toggle_claude_follow(stoat),
        ActionKind::ToggleDockRight => pane::toggle_dock(stoat, DockSide::Right),
        ActionKind::ToggleDockLeft => pane::toggle_dock(stoat, DockSide::Left),
        ActionKind::JumpToMoveSource => {
            movement::move_nav(stoat, movement::MoveNavigation::FirstSource)
        },
        ActionKind::JumpToMoveTarget => movement::move_nav(stoat, movement::MoveNavigation::Target),
        ActionKind::JumpToNextMoveSource => {
            movement::move_nav(stoat, movement::MoveNavigation::NextSource)
        },
        ActionKind::JumpToPrevMoveSource => {
            movement::move_nav(stoat, movement::MoveNavigation::PrevSource)
        },
        ActionKind::QueryMoveRelationships => {
            // Scriptable surface: observes the move metadata under the
            // cursor but does not navigate. A future automation hook
            // will expose this via the action SDK; for now it resolves
            // and logs the relationship count so the action is
            // observable from tests.
            if let Some(summary) = movement::current_move_summary(stoat) {
                tracing::info!(
                    sources = summary.source_count,
                    same_side_target = ?summary.target_line,
                    "move relationships under cursor"
                );
                UpdateEffect::Redraw
            } else {
                UpdateEffect::None
            }
        },
        ActionKind::ReviewNextChunk => review::review_step(stoat, review::ReviewStep::Next),
        ActionKind::ReviewPrevChunk => review::review_step(stoat, review::ReviewStep::Prev),
        ActionKind::ReviewStageChunk => review::review_mark(stoat, review::ReviewMark::Stage),
        ActionKind::ReviewUnstageChunk => review::review_mark(stoat, review::ReviewMark::Unstage),
        ActionKind::ReviewToggleStage => review::review_mark(stoat, review::ReviewMark::Toggle),
        ActionKind::ReviewSkipChunk => review::review_mark(stoat, review::ReviewMark::Skip),
        ActionKind::ReviewRefresh => review::review_refresh(stoat),
        ActionKind::ReviewApplyStaged => review::review_apply_staged(stoat),
        ActionKind::CloseReview => review::close_review(stoat),
        ActionKind::OpenReviewCommit => {
            let a = action
                .as_any()
                .downcast_ref::<OpenReviewCommit>()
                .expect("OpenReviewCommit action downcast");
            review::open_review_commit(stoat, &a.workdir, &a.sha);
            UpdateEffect::Redraw
        },
        ActionKind::OpenReviewCommitRange => {
            let a = action
                .as_any()
                .downcast_ref::<OpenReviewCommitRange>()
                .expect("OpenReviewCommitRange action downcast");
            review::open_review_commit_range(stoat, &a.workdir, &a.from, &a.to);
            UpdateEffect::Redraw
        },
        ActionKind::OpenReviewAgentEdits => {
            let a = action
                .as_any()
                .downcast_ref::<OpenReviewAgentEdits>()
                .expect("OpenReviewAgentEdits action downcast");
            review::open_review_agent_edits(stoat, &a.edits);
            UpdateEffect::Redraw
        },
        ActionKind::OpenCommits => commits::open_commits(stoat),
        ActionKind::CloseCommits => commits::close_commits(stoat),
        ActionKind::CommitsNext => commits::commits_step(stoat, commits::CommitStep::Down(1)),
        ActionKind::CommitsPrev => commits::commits_step(stoat, commits::CommitStep::Up(1)),
        ActionKind::CommitsPageDown => commits::commits_step(stoat, commits::CommitStep::PageDown),
        ActionKind::CommitsPageUp => commits::commits_step(stoat, commits::CommitStep::PageUp),
        ActionKind::CommitsFirst => commits::commits_step(stoat, commits::CommitStep::First),
        ActionKind::CommitsLast => commits::commits_step(stoat, commits::CommitStep::Last),
        ActionKind::CommitsRefresh => commits::commits_refresh(stoat),
        ActionKind::CommitsOpenReview => review::commits_open_review(stoat),
        ActionKind::ReviewRemoveSelected => review::review_remove_selected(stoat),
        ActionKind::EnterRebase => rebase::enter_rebase(stoat),
        ActionKind::AbortRebase => rebase::abort_rebase(stoat),
        ActionKind::ExecuteRebase => rebase::execute_rebase(stoat),
        ActionKind::RebaseNext => rebase::rebase_move(stoat, rebase::RebaseMove::Next),
        ActionKind::RebasePrev => rebase::rebase_move(stoat, rebase::RebaseMove::Prev),
        ActionKind::RebaseMoveUp => rebase::rebase_move(stoat, rebase::RebaseMove::SwapUp),
        ActionKind::RebaseMoveDown => rebase::rebase_move(stoat, rebase::RebaseMove::SwapDown),
        ActionKind::SetRebaseOpPick => {
            rebase::rebase_set_op(stoat, crate::host::RebaseTodoOp::Pick)
        },
        ActionKind::SetRebaseOpSquash => {
            rebase::rebase_set_op(stoat, crate::host::RebaseTodoOp::Squash)
        },
        ActionKind::SetRebaseOpFixup => {
            rebase::rebase_set_op(stoat, crate::host::RebaseTodoOp::Fixup)
        },
        ActionKind::SetRebaseOpDrop => {
            rebase::rebase_set_op(stoat, crate::host::RebaseTodoOp::Drop)
        },
        ActionKind::SetRebaseOpReword => {
            rebase::rebase_set_op(stoat, crate::host::RebaseTodoOp::Reword)
        },
        ActionKind::SetRebaseOpEdit => {
            rebase::rebase_set_op(stoat, crate::host::RebaseTodoOp::Edit)
        },
        ActionKind::RewordConfirm => reword::reword_confirm(stoat),
        ActionKind::RewordAbort => reword::reword_abort(stoat),
        ActionKind::RebaseContinue => rebase::rebase_continue(stoat),
        ActionKind::ConflictTakeOurs => {
            conflict::conflict_set(stoat, conflict::ConflictChoice::Ours)
        },
        ActionKind::ConflictTakeTheirs => {
            conflict::conflict_set(stoat, conflict::ConflictChoice::Theirs)
        },
        ActionKind::ConflictSkipEntry => conflict::conflict_skip_entry(stoat),
        ActionKind::ConflictNextFile => conflict::conflict_step(stoat, true),
        ActionKind::ConflictPrevFile => conflict::conflict_step(stoat, false),
        ActionKind::ConflictApply => conflict::conflict_apply(stoat),
        ActionKind::ConflictAbort => conflict::conflict_abort(stoat),
        ActionKind::Dump => {
            let dump = action
                .as_any()
                .downcast_ref::<Dump>()
                .expect("Dump action downcast");
            workspace::handle_dump(stoat, &dump.name);
            UpdateEffect::Redraw
        },
        ActionKind::NewWorkspace => workspace::new_workspace(stoat),
        ActionKind::CopyWorkspace => workspace::copy_workspace(stoat),
        ActionKind::SwitchWorkspace => {
            stoat.workspace_picker = Some(WorkspacePicker::new(
                &stoat.workspaces,
                stoat.active_workspace,
            ));
            UpdateEffect::Redraw
        },
        ActionKind::CloseWorkspace => workspace::close_workspace(stoat),
        ActionKind::RenameWorkspace => {
            let action = action
                .as_any()
                .downcast_ref::<RenameWorkspace>()
                .expect("RenameWorkspace action downcast");
            workspace::rename_workspace(stoat, &action.name);
            UpdateEffect::Redraw
        },
        ActionKind::SubmitPromptInput => prompt::submit_prompt_input(stoat),
        ActionKind::CancelPromptInput => prompt::cancel_prompt_input(stoat),
        ActionKind::PromptInsertNewline => prompt::prompt_insert_newline(stoat),
        ActionKind::PaletteSelectPrev => prompt::palette_select_prev(stoat),
        ActionKind::PaletteSelectNext => prompt::palette_select_next(stoat),
        ActionKind::PaletteScopeToggle => palette::palette_scope_toggle(stoat),
    };
    stoat.sync_claude_badges();
    effect
}

/// Return a mutable reference to the effective focused editor, respecting
/// the reword-pause override. Shared by every movement handler plus the
/// move-navigation summary lookup.
pub(crate) fn focused_editor_mut(stoat: &mut Stoat) -> Option<&mut EditorState> {
    use crate::rebase::RebasePause;
    let ws = stoat.active_workspace_mut();

    // While a reword pause is active, the "focused" editor for motion
    // and insertion purposes is the reword scratch editor, regardless
    // of which pane had focus when rebase started.
    if let Some(editor_id) = ws
        .rebase_active
        .as_ref()
        .and_then(|a| a.pause.as_ref())
        .and_then(|p| match p {
            RebasePause::Reword { input, .. } => Some(input.editor_id),
            _ => None,
        })
    {
        return ws.editors.get_mut(editor_id);
    }

    let view = match ws.focus {
        FocusTarget::SplitPane(_) => {
            let focused = ws.panes.focus();
            ws.panes.pane(focused).view.clone()
        },
        FocusTarget::Dock(dock_id) => match ws.docks.get(dock_id) {
            Some(dock) => dock.view.clone(),
            None => return None,
        },
    };
    match view {
        View::Editor(id) => ws.editors.get_mut(id),
        View::Claude(session_id) => {
            let editor_id = ws.chats.get(&session_id)?.input.editor_id;
            ws.editors.get_mut(editor_id)
        },
        _ => None,
    }
}

/// Close the help modal, disposing its scratch editor and restoring the
/// mode that was active before the modal opened. No-op when help is not
/// open. Shared between `CancelPromptInput`, Ctrl-C cleanup, and the help
/// `HelpOutcome::Close`/`HelpOutcome::Dispatch` paths.
pub(crate) fn close_help(stoat: &mut Stoat) {
    let Some(help) = stoat.help.take() else {
        return;
    };
    let active_idx = stoat.active_workspace;
    help.dispose(&mut stoat.workspaces[active_idx]);
    stoat.mode = help.previous_mode.clone();
}

/// Read `path` through the supplied [`FsHost`] as a UTF-8 string.
pub(crate) fn read_string_via_host(fs: &dyn FsHost, path: &Path) -> std::io::Result<String> {
    let mut buf = Vec::new();
    fs.read(path, &mut buf)?;
    String::from_utf8(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use stoat_action::{
        AddSelectionBelow, CollapseSelection, ExtendDown, ExtendLeft, ExtendNextWordEnd,
        ExtendNextWordStart, ExtendPrevWordEnd, ExtendPrevWordStart, ExtendRight,
        ExtendToFileStart, ExtendToLastLine, ExtendToLineEnd, ExtendToLineStart, ExtendUp,
        FlipSelections, HalfPageDown, MoveDown, MoveLeft, MoveNextWordEnd, MoveNextWordStart,
        MovePrevWordEnd, MovePrevWordStart, MoveRight, MoveUp, PageDown, Quit, QuitAll,
        RenameWorkspace, SelectAll, SplitNewRight, SplitRight,
    };
    use stoat_scheduler::TestScheduler;
    use stoat_text::{Bias, SelectionGoal};

    fn stoat() -> Stoat {
        let scheduler = Arc::new(TestScheduler::new());
        let mut stoat = Stoat::new(
            scheduler.executor(),
            stoat_config::Settings::default(),
            std::path::PathBuf::new(),
        );
        stoat.update(crossterm::event::Event::Resize(80, 24));
        stoat
    }

    use crate::test_harness::editor;

    #[test]
    fn dispatch_quit() {
        assert_eq!(dispatch(&mut stoat(), &Quit), UpdateEffect::Quit);
    }

    #[test]
    fn dispatch_quit_with_splits_closes_pane() {
        let mut stoat = stoat();
        dispatch(&mut stoat, &SplitRight);
        assert_eq!(stoat.active_workspace().panes.pane_count(), 2);
        assert_eq!(dispatch(&mut stoat, &Quit), UpdateEffect::Redraw);
        assert_eq!(stoat.active_workspace().panes.pane_count(), 1);
    }

    #[test]
    fn dispatch_quit_all_exits_with_splits() {
        let mut stoat = stoat();
        dispatch(&mut stoat, &SplitRight);
        dispatch(&mut stoat, &SplitRight);
        assert_eq!(stoat.active_workspace().panes.pane_count(), 3);
        assert_eq!(dispatch(&mut stoat, &QuitAll), UpdateEffect::Quit);
    }

    #[test]
    fn split_new_right_uses_fresh_scratch_buffer() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "original");

        let original_pane = stoat.active_workspace().panes.focus();
        let original_buffer_id = match stoat.active_workspace().panes.pane(original_pane).view {
            View::Editor(eid) => stoat.active_workspace().editors[eid].buffer_id,
            _ => panic!("focused pane is not an editor"),
        };

        dispatch(&mut stoat, &SplitNewRight);
        assert_eq!(stoat.active_workspace().panes.pane_count(), 2);

        let ws = stoat.active_workspace();
        let new_pane = ws.panes.focus();
        assert_ne!(new_pane, original_pane);

        let new_editor_id = match ws.panes.pane(new_pane).view {
            View::Editor(eid) => eid,
            _ => panic!("new pane is not an editor"),
        };
        let new_buffer_id = ws.editors[new_editor_id].buffer_id;
        assert_ne!(new_buffer_id, original_buffer_id);

        let new_buffer = ws.buffers.get(new_buffer_id).expect("buffer exists");
        let guard = new_buffer.read().expect("buffer poisoned");
        assert_eq!(guard.snapshot.visible_text.to_string(), "");

        let original_buffer = ws.buffers.get(original_buffer_id).expect("buffer exists");
        let original_guard = original_buffer.read().expect("buffer poisoned");
        assert_eq!(original_guard.snapshot.visible_text.to_string(), "original");
    }

    #[test]
    fn move_left_at_start_is_noop() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "hello");
        dispatch(&mut stoat, &MoveLeft);
        assert_eq!(editor::head_offsets(&mut stoat), vec![0]);
    }

    #[test]
    fn move_right_advances_one_grapheme() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abc");
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(editor::head_offsets(&mut stoat), vec![1]);
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(editor::head_offsets(&mut stoat), vec![2]);
    }

    #[test]
    fn move_right_at_end_is_noop() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abc");
        dispatch(&mut stoat, &MoveRight);
        dispatch(&mut stoat, &MoveRight);
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(editor::head_offsets(&mut stoat), vec![3]);
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(editor::head_offsets(&mut stoat), vec![3]);
    }

    #[test]
    fn move_right_across_newline() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "ab\ncd");
        dispatch(&mut stoat, &MoveRight);
        dispatch(&mut stoat, &MoveRight);
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(editor::head_offsets(&mut stoat), vec![3]);
    }

    #[test]
    fn move_right_multibyte() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "héllo");
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(editor::head_offsets(&mut stoat), vec![1]);
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(editor::head_offsets(&mut stoat), vec![3]);
    }

    #[test]
    fn move_down_advances_one_row() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abc\ndef\n");
        dispatch(&mut stoat, &MoveDown);
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(1, 0)]);
    }

    #[test]
    fn move_up_at_first_row_is_noop() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abc\ndef");
        dispatch(&mut stoat, &MoveUp);
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(0, 0)]);
    }

    #[test]
    fn move_down_at_last_row_is_noop() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abc");
        dispatch(&mut stoat, &MoveDown);
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(0, 0)]);
    }

    #[test]
    fn move_down_preserves_goal_column() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "long line\nxx\nlong line\n");
        for _ in 0..7 {
            dispatch(&mut stoat, &MoveRight);
        }
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(0, 7)]);
        dispatch(&mut stoat, &MoveDown);
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(1, 2)]);
        dispatch(&mut stoat, &MoveDown);
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(2, 7)]);
    }

    #[test]
    fn move_next_word_start_creates_selection() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar");
        dispatch(&mut stoat, &MoveNextWordStart);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, false)]);
        assert_eq!(editor::head_offsets(&mut stoat), vec![3]);
    }

    #[test]
    fn move_next_word_start_repeated_snaps_tail() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar baz");
        dispatch(&mut stoat, &MoveNextWordStart);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, false)]);
        dispatch(&mut stoat, &MoveNextWordStart);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(3, 7, false)]);
    }

    #[test]
    fn move_next_word_end_creates_selection() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar");
        dispatch(&mut stoat, &MoveNextWordEnd);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 2, false)]);
    }

    #[test]
    fn move_next_word_end_at_eof_is_noop() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo");
        for _ in 0..3 {
            dispatch(&mut stoat, &MoveRight);
        }
        assert_eq!(editor::head_offsets(&mut stoat), vec![3]);
        dispatch(&mut stoat, &MoveNextWordEnd);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(3, 3, false)]);
    }

    #[test]
    fn move_prev_word_start_creates_reversed_selection() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar");
        for _ in 0..6 {
            dispatch(&mut stoat, &MoveRight);
        }
        assert_eq!(editor::head_offsets(&mut stoat), vec![6]);
        dispatch(&mut stoat, &MovePrevWordStart);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(4, 7, true)]);
        assert_eq!(editor::head_offsets(&mut stoat), vec![4]);
    }

    #[test]
    fn move_prev_word_start_at_start_is_noop() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar");
        dispatch(&mut stoat, &MovePrevWordStart);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 0, false)]);
    }

    #[test]
    fn move_prev_word_end_lands_on_last_char_of_prev_word() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar");
        for _ in 0..6 {
            dispatch(&mut stoat, &MoveRight);
        }
        assert_eq!(editor::head_offsets(&mut stoat), vec![6]);
        dispatch(&mut stoat, &MovePrevWordEnd);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 7, true)]);
        assert_eq!(editor::head_offsets(&mut stoat), vec![2]);
    }

    #[test]
    fn move_prev_word_end_at_start_is_noop() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar");
        dispatch(&mut stoat, &MovePrevWordEnd);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 0, false)]);
    }

    #[test]
    fn move_right_with_multiple_cursors_advances_each() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abc\ndef\nghi\n");
        dispatch(&mut stoat, &AddSelectionBelow);
        assert_eq!(editor::head_offsets(&mut stoat), vec![0, 4]);
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(editor::head_offsets(&mut stoat), vec![1, 5]);
    }

    #[test]
    fn move_next_word_start_multi_cursor_independent() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar\nbaz qux\n");
        dispatch(&mut stoat, &AddSelectionBelow);
        assert_eq!(editor::head_offsets(&mut stoat), vec![0, 8]);
        dispatch(&mut stoat, &MoveNextWordStart);
        assert_eq!(
            editor::selection_spans(&mut stoat),
            vec![(0, 3, false), (8, 11, false)]
        );
    }

    #[test]
    fn add_selection_below_with_no_editor_focus_is_noop() {
        let mut stoat = stoat();
        {
            let ws = stoat.active_workspace_mut();
            let focused = ws.panes.focus();
            ws.panes.pane_mut(focused).view = View::Label("nothing".into());
        }
        assert_eq!(dispatch(&mut stoat, &AddSelectionBelow), UpdateEffect::None);
    }

    #[test]
    fn add_selection_below_adds_cursor_on_next_display_row() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abc\ndef\nghi\n");

        assert_eq!(
            dispatch(&mut stoat, &AddSelectionBelow),
            UpdateEffect::Redraw
        );

        let positions = editor::cursor_display_positions(&mut stoat);
        assert_eq!(positions, vec![(0, 0), (1, 0)]);
    }

    #[test]
    fn add_selection_below_at_last_row_is_noop() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abc");

        assert_eq!(dispatch(&mut stoat, &AddSelectionBelow), UpdateEffect::None);
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(0, 0)]);
    }

    #[test]
    fn add_selection_below_preserves_goal_column_on_short_line() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "long line\nxx\nlong line\n");

        {
            let ws = stoat.active_workspace_mut();
            let focused = ws.panes.focus();
            let editor_id = match ws.panes.pane(focused).view {
                View::Editor(id) => id,
                _ => unreachable!(),
            };
            let editor = ws.editors.get_mut(editor_id).expect("editor");
            let snapshot = editor.display_map.snapshot();
            let buffer = snapshot.buffer_snapshot();
            let offset = buffer.rope().point_to_offset(stoat_text::Point::new(0, 7));
            let anchor = buffer.anchor_at(offset, Bias::Right);
            editor
                .selections
                .insert_cursor(anchor, SelectionGoal::Column(7), buffer);
        }

        assert_eq!(
            dispatch(&mut stoat, &AddSelectionBelow),
            UpdateEffect::Redraw
        );
        let after_one = editor::cursor_display_positions(&mut stoat);
        assert_eq!(after_one, vec![(0, 0), (0, 7), (1, 2)]);

        assert_eq!(
            dispatch(&mut stoat, &AddSelectionBelow),
            UpdateEffect::Redraw
        );
        let after_two = editor::cursor_display_positions(&mut stoat);
        assert_eq!(after_two, vec![(0, 0), (0, 7), (1, 2), (2, 7)]);
    }

    #[test]
    fn extend_right_grows_selection_from_cursor() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abc");
        dispatch(&mut stoat, &ExtendRight);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 1, false)]);
    }

    #[test]
    fn extend_right_further_keeps_tail() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abcdef");
        dispatch(&mut stoat, &ExtendRight);
        dispatch(&mut stoat, &ExtendRight);
        dispatch(&mut stoat, &ExtendRight);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, false)]);
    }

    #[test]
    fn extend_right_at_end_is_noop() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "ab");
        dispatch(&mut stoat, &MoveRight);
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 2, false)]);
        dispatch(&mut stoat, &ExtendRight);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 2, false)]);
    }

    #[test]
    fn extend_left_across_tail_flips_reversed() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abcdef");
        dispatch(&mut stoat, &MoveRight);
        dispatch(&mut stoat, &MoveRight);
        dispatch(&mut stoat, &ExtendRight);
        dispatch(&mut stoat, &ExtendRight);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 4, false)]);
        dispatch(&mut stoat, &ExtendLeft);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 3, false)]);
        dispatch(&mut stoat, &ExtendLeft);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 2, false)]);
        dispatch(&mut stoat, &ExtendLeft);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(1, 2, true)]);
    }

    #[test]
    fn extend_down_preserves_goal_column() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "long line\nxx\nlong line\n");
        for _ in 0..7 {
            dispatch(&mut stoat, &MoveRight);
        }
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(0, 7)]);
        dispatch(&mut stoat, &ExtendDown);
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(1, 2)]);
        dispatch(&mut stoat, &ExtendDown);
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(2, 7)]);
    }

    #[test]
    fn extend_down_at_last_row_is_noop() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abc");
        dispatch(&mut stoat, &ExtendDown);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 0, false)]);
    }

    #[test]
    fn extend_up_from_second_line_grows_backward() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abc\ndef\n");
        dispatch(&mut stoat, &MoveDown);
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(5, 5, false)]);
        dispatch(&mut stoat, &ExtendUp);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(1, 5, true)]);
    }

    #[test]
    fn extend_next_word_start_grows_selection_from_cursor() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar");
        dispatch(&mut stoat, &ExtendNextWordStart);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, false)]);
    }

    #[test]
    fn extend_next_word_start_repeated_keeps_tail() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar baz");
        dispatch(&mut stoat, &ExtendNextWordStart);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, false)]);
        dispatch(&mut stoat, &ExtendNextWordStart);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 7, false)]);
    }

    #[test]
    fn extend_next_word_end_grows_selection_from_cursor() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar");
        dispatch(&mut stoat, &ExtendNextWordEnd);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 2, false)]);
    }

    #[test]
    fn extend_prev_word_start_keeps_tail_at_cursor() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar");
        for _ in 0..6 {
            dispatch(&mut stoat, &MoveRight);
        }
        assert_eq!(editor::selection_spans(&mut stoat), vec![(6, 6, false)]);
        dispatch(&mut stoat, &ExtendPrevWordStart);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(4, 6, true)]);
    }

    #[test]
    fn extend_prev_word_end_keeps_tail_at_cursor() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar");
        for _ in 0..6 {
            dispatch(&mut stoat, &MoveRight);
        }
        assert_eq!(editor::selection_spans(&mut stoat), vec![(6, 6, false)]);
        dispatch(&mut stoat, &ExtendPrevWordEnd);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 6, true)]);
    }

    #[test]
    fn extend_right_with_multiple_cursors_grows_each() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abc\ndef\nghi\n");
        dispatch(&mut stoat, &AddSelectionBelow);
        assert_eq!(editor::head_offsets(&mut stoat), vec![0, 4]);
        dispatch(&mut stoat, &ExtendRight);
        assert_eq!(
            editor::selection_spans(&mut stoat),
            vec![(0, 1, false), (4, 5, false)]
        );
    }

    #[test]
    fn extend_to_line_end_grows_forward() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar");
        dispatch(&mut stoat, &ExtendToLineEnd);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 7, false)]);
    }

    #[test]
    fn extend_to_line_start_from_mid_reverses() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar");
        dispatch(&mut stoat, &MoveRight);
        dispatch(&mut stoat, &MoveRight);
        dispatch(&mut stoat, &MoveRight);
        dispatch(&mut stoat, &ExtendToLineStart);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, true)]);
    }

    #[test]
    fn extend_to_last_line_grows_forward() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abc\ndef\nghi\n");
        dispatch(&mut stoat, &ExtendToLastLine);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 8, false)]);
    }

    #[test]
    fn extend_to_file_start_reverses_from_end() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abcdef");
        dispatch(&mut stoat, &MoveRight);
        dispatch(&mut stoat, &MoveRight);
        dispatch(&mut stoat, &MoveRight);
        dispatch(&mut stoat, &ExtendToFileStart);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, true)]);
    }

    #[test]
    fn collapse_selection_shrinks_to_head() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abcdef");
        dispatch(&mut stoat, &ExtendRight);
        dispatch(&mut stoat, &ExtendRight);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 2, false)]);
        dispatch(&mut stoat, &CollapseSelection);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 2, false)]);
    }

    #[test]
    fn collapse_selection_preserves_reversed_head() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar");
        for _ in 0..6 {
            dispatch(&mut stoat, &MoveRight);
        }
        dispatch(&mut stoat, &MovePrevWordStart);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(4, 7, true)]);
        dispatch(&mut stoat, &CollapseSelection);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(4, 4, false)]);
    }

    #[test]
    fn collapse_selection_multi_cursor_collapses_each() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abc\ndef\nghi\n");
        dispatch(&mut stoat, &AddSelectionBelow);
        dispatch(&mut stoat, &ExtendRight);
        assert_eq!(
            editor::selection_spans(&mut stoat),
            vec![(0, 1, false), (4, 5, false)]
        );
        dispatch(&mut stoat, &CollapseSelection);
        assert_eq!(
            editor::selection_spans(&mut stoat),
            vec![(1, 1, false), (5, 5, false)]
        );
    }

    #[test]
    fn flip_selections_toggles_reversed() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abcdef");
        dispatch(&mut stoat, &ExtendRight);
        dispatch(&mut stoat, &ExtendRight);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 2, false)]);
        dispatch(&mut stoat, &FlipSelections);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 2, true)]);
        dispatch(&mut stoat, &FlipSelections);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 2, false)]);
    }

    #[test]
    fn flip_selections_empty_is_noop() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abc");
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(1, 1, false)]);
        dispatch(&mut stoat, &FlipSelections);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(1, 1, false)]);
    }

    #[test]
    fn select_all_replaces_all_selections() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abc\ndef\n");
        dispatch(&mut stoat, &AddSelectionBelow);
        assert_eq!(editor::head_offsets(&mut stoat), vec![0, 4]);
        dispatch(&mut stoat, &SelectAll);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 8, false)]);
    }

    #[test]
    fn select_all_on_empty_buffer() {
        let mut stoat = stoat();
        dispatch(&mut stoat, &SelectAll);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 0, false)]);
    }

    #[test]
    fn claude_submit_queues_when_session_not_ready() {
        let mut stoat = stoat();

        dispatch(&mut stoat, &stoat_action::OpenClaude);

        let session_id = stoat
            .active_workspace()
            .claude_chat
            .expect("claude_chat should be set");
        assert!(
            stoat.claude_sessions().get(session_id).is_none(),
            "host slot should be None after reserve_slot"
        );

        {
            let ws = stoat.active_workspace();
            let chat = ws.chats.get(&session_id).expect("chat state exists");
            let buffer = ws.buffers.get(chat.input.buffer_id).expect("buffer");
            buffer.write().expect("poisoned").edit(0..0, "hello claude");
        }

        dispatch(&mut stoat, &stoat_action::ClaudeSubmit);

        let ws = stoat.active_workspace();
        let chat = ws.chats.get(&session_id).expect("chat state");
        assert_eq!(chat.messages.len(), 1, "user message should be in chat");
        assert_eq!(
            chat.pending_sends,
            vec!["hello claude"],
            "message should be queued, not dropped"
        );
    }

    #[test]
    fn type_action_direct() {
        let mut h = Stoat::test();
        h.type_action("SetMode(space)");
        let last = h.frames().last().expect("no frames");
        assert_eq!(last.mode, "space");
    }

    #[test]
    fn open_file_shows_in_focused_pane() {
        let mut h = Stoat::test();
        let path = h.write_file("test.txt", "hello world");

        h.open_file(&path);
        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 1);
        h.assert_snapshot("open_file_shows_in_focused_pane");
    }

    #[test]
    fn open_file_replaces_focused_pane_does_not_split() {
        let mut h = Stoat::test();
        let a = h.write_file("a.txt", "file A");
        let b = h.write_file("b.txt", "file B");

        h.open_file(&a);
        h.open_file(&b);
        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 1);
        h.assert_snapshot("open_file_replaces_focused_pane");
    }

    #[test]
    fn split_then_open_creates_multi_pane_layout() {
        let mut h = Stoat::test();
        let a = h.write_file("a.txt", "AAA");
        let b = h.write_file("b.txt", "BBB");
        let c = h.write_file("c.txt", "CCC");

        h.open_file(&a);
        h.type_action("SplitRight()");
        h.open_file(&b);
        h.type_action("SplitRight()");
        h.open_file(&c);
        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 3);
        h.assert_snapshot("split_then_open_three");
    }

    #[test]
    fn open_missing_file_creates_empty_buffer() {
        let path = std::path::PathBuf::from("/test/does_not_exist.txt");

        let mut h = Stoat::test();
        h.open_file(&path);
        let frame = h.snapshot();
        assert_eq!(frame.pane_count, 1);
    }

    #[test]
    fn open_file_via_fs_host_reads_from_fake_fs() {
        let mut h = Stoat::test();
        h.fake_fs
            .insert_file("/work/hello.txt", b"greetings from fake fs");
        h.stoat.open_file(Path::new("/work/hello.txt"));
        let ws = h.stoat.active_workspace();
        let focused = ws.panes.focus();
        let editor_id = match ws.panes.pane(focused).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let editor = ws.editors.get(editor_id).unwrap();
        let buffer = ws.buffers.get(editor.buffer_id).unwrap();
        let guard = buffer.read().unwrap();
        assert_eq!(
            guard.snapshot.visible_text.to_string(),
            "greetings from fake fs"
        );
    }

    #[test]
    fn type_action_quit_from_space() {
        let mut h = Stoat::test();
        h.type_keys("space");
        h.type_action("Quit");
    }

    #[test]
    #[should_panic(expected = "unreachable")]
    fn type_action_unreachable_panics() {
        let mut h = Stoat::test();
        h.type_action("NonExistentAction");
    }

    #[test]
    fn new_workspace_adds_fresh_workspace_and_switches() {
        let mut h = Stoat::test();
        let original = h.stoat.active_workspace;
        assert_eq!(h.stoat.workspaces.len(), 1);

        h.type_action("NewWorkspace()");

        assert_eq!(h.stoat.workspaces.len(), 2);
        assert_ne!(h.stoat.active_workspace, original);
        let new_ws = h.stoat.active_workspace();
        assert_eq!(new_ws.panes.pane_count(), 1);
        assert_eq!(new_ws.editors.len(), 1);
        assert!(new_ws.claude_chat.is_none());
        assert!(new_ws.rebase.is_none());
    }

    #[test]
    fn copy_workspace_duplicates_pane_layout() {
        let mut h = Stoat::test();
        h.type_action("SplitRight()");
        let before_pane_count = h.stoat.active_workspace().panes.pane_count();
        assert_eq!(before_pane_count, 2);

        h.type_action("CopyWorkspace()");

        assert_eq!(h.stoat.workspaces.len(), 2);
        let new_ws = h.stoat.active_workspace();
        assert_eq!(new_ws.panes.pane_count(), before_pane_count);
    }

    #[test]
    fn copy_workspace_clones_buffer_contents() {
        let mut h = Stoat::test();
        h.fake_fs.insert_file("/work/note.txt", b"original text");
        h.stoat.open_file(Path::new("/work/note.txt"));

        h.type_action("CopyWorkspace()");

        let ws = h.stoat.active_workspace();
        let focused = ws.panes.focus();
        let editor_id = match &ws.panes.pane(focused).view {
            View::Editor(id) => *id,
            other => panic!("expected editor in focused pane, got {other:?}"),
        };
        let editor = ws.editors.get(editor_id).expect("editor missing");
        let buffer = ws.buffers.get(editor.buffer_id).expect("buffer missing");
        let guard = buffer.read().expect("buffer poisoned");
        assert_eq!(guard.snapshot.visible_text.to_string(), "original text");
    }

    #[test]
    fn copy_workspace_gets_distinct_uid() {
        let mut h = Stoat::test();
        let source_uid = h.stoat.active_workspace().uid;

        h.type_action("CopyWorkspace()");

        let copy_uid = h.stoat.active_workspace().uid;
        assert_ne!(
            source_uid, copy_uid,
            "copy must have its own uid so both workspaces can persist",
        );
    }

    #[test]
    fn switch_workspace_opens_picker() {
        let mut h = Stoat::test();
        h.type_action("NewWorkspace()");
        assert!(h.stoat.workspace_picker.is_none());

        h.type_action("SwitchWorkspace()");

        assert!(h.stoat.workspace_picker.is_some());
        let picker = h.stoat.workspace_picker.as_ref().unwrap();
        assert_eq!(picker.entries().len(), 2);
    }

    #[test]
    fn picker_enter_switches_to_selected_workspace() {
        let mut h = Stoat::test();
        h.type_action("NewWorkspace()");
        let second = h.stoat.active_workspace;
        h.type_action("NewWorkspace()");
        let third = h.stoat.active_workspace;
        assert_eq!(h.stoat.workspaces.len(), 3);

        h.type_action("SwitchWorkspace()");
        h.type_keys("down enter");

        // Picker sorts current first, then by basename. With all three sharing
        // the empty basename, uid is the tiebreaker (smallest first after the
        // current one), so "Down Enter" lands on whichever sibling sorts first.
        assert!(h.stoat.workspace_picker.is_none());
        assert_ne!(h.stoat.active_workspace, third);
        assert!(h.stoat.active_workspace == second || h.stoat.active_workspace != third);
    }

    #[test]
    fn picker_escape_closes_without_switching() {
        let mut h = Stoat::test();
        h.type_action("NewWorkspace()");
        let before = h.stoat.active_workspace;

        h.type_action("SwitchWorkspace()");
        h.type_keys("escape");

        assert!(h.stoat.workspace_picker.is_none());
        assert_eq!(h.stoat.active_workspace, before);
    }

    #[test]
    fn close_workspace_refuses_when_only_one_remains() {
        let mut h = Stoat::test();
        let only = h.stoat.active_workspace;

        h.type_action("CloseWorkspace()");

        assert_eq!(h.stoat.workspaces.len(), 1);
        assert_eq!(h.stoat.active_workspace, only);
    }

    #[test]
    fn snapshot_workspace_picker_listing() {
        let mut h = Stoat::test();
        h.type_action("NewWorkspace()");
        // NewWorkspace builds a Workspace via the production path, which
        // generates a random uid-derived name. Clear it before opening
        // the picker so the rendered snapshot is stable across runs.
        for (_, ws) in h.stoat.workspaces.iter_mut() {
            ws.name = String::new();
        }
        h.type_action("SwitchWorkspace()");
        h.assert_snapshot("workspace_picker_listing");
    }

    #[test]
    fn fresh_workspace_is_fresh() {
        let h = Stoat::test();
        assert!(h.stoat.active_workspace().is_fresh());
    }

    #[test]
    fn typing_in_scratch_breaks_freshness() {
        let mut h = Stoat::test();
        h.edit_focused(0..0, "x");
        assert!(!h.stoat.active_workspace().is_fresh());
    }

    #[test]
    fn opening_file_breaks_freshness() {
        let mut h = Stoat::test();
        h.fake_fs.insert_file("/work/note.txt", b"hello");
        h.stoat.open_file(Path::new("/work/note.txt"));
        assert!(!h.stoat.active_workspace().is_fresh());
    }

    #[test]
    fn splitting_pane_breaks_freshness() {
        let mut h = Stoat::test();
        h.type_action("SplitRight()");
        assert!(!h.stoat.active_workspace().is_fresh());
    }

    #[test]
    fn close_workspace_switches_to_sibling() {
        let mut h = Stoat::test();
        let first = h.stoat.active_workspace;
        h.type_action("NewWorkspace()");
        let second = h.stoat.active_workspace;
        assert_ne!(first, second);

        h.type_action("CloseWorkspace()");

        assert_eq!(h.stoat.workspaces.len(), 1);
        assert_eq!(h.stoat.active_workspace, first);
        assert!(h.stoat.workspaces.get(second).is_none());
    }

    #[test]
    fn rename_workspace_sets_active_workspace_name() {
        let mut stoat = stoat();
        dispatch(
            &mut stoat,
            &RenameWorkspace {
                name: "alpha".to_string(),
            },
        );
        assert_eq!(stoat.active_workspace().name, "alpha");
    }

    #[test]
    fn rename_workspace_with_empty_name_clears_back_to_fallback() {
        let mut stoat = stoat();
        dispatch(
            &mut stoat,
            &RenameWorkspace {
                name: "alpha".to_string(),
            },
        );
        assert_eq!(stoat.active_workspace().name, "alpha");
        dispatch(
            &mut stoat,
            &RenameWorkspace {
                name: String::new(),
            },
        );
        assert_eq!(stoat.active_workspace().name, "");
    }

    fn set_focused_viewport_rows(stoat: &mut Stoat, rows: Option<u32>) {
        let ws = stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        let editor_id = match ws.panes.pane(focused).view {
            View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        ws.editors[editor_id].viewport_rows = rows;
    }

    #[test]
    fn page_down_with_unrendered_editor_uses_default_viewport() {
        let mut stoat = stoat();
        let text: String = (0..30).map(|i| format!("line{i:02}\n")).collect();
        editor::seed_focused_buffer(&mut stoat, &text);
        set_focused_viewport_rows(&mut stoat, None);
        dispatch(&mut stoat, &PageDown);
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(20, 0)]);
    }

    #[test]
    fn half_page_down_rounds_up_for_one_row_viewport() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "a\nb\nc\n");
        set_focused_viewport_rows(&mut stoat, Some(1));
        dispatch(&mut stoat, &HalfPageDown);
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(1, 0)]);
    }

    #[test]
    fn page_down_collapses_multi_cursors_to_one() {
        let mut stoat = stoat();
        let text: String = (0..30).map(|i| format!("line{i:02}\n")).collect();
        editor::seed_focused_buffer(&mut stoat, &text);
        set_focused_viewport_rows(&mut stoat, Some(10));
        dispatch(&mut stoat, &AddSelectionBelow);
        assert_eq!(editor::head_offsets(&mut stoat).len(), 2);
        dispatch(&mut stoat, &PageDown);
        // AddSelectionBelow makes row 1 the primary cursor; PageDown from
        // row 1 with viewport=10 lands on row 11. Both cursors collapse to
        // the same target via the transform dedupe.
        assert_eq!(editor::head_offsets(&mut stoat).len(), 1);
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(11, 0)]);
    }
}

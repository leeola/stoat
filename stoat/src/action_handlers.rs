mod agent;
mod commits;
pub(crate) mod completion;
mod conflict;
pub(crate) mod file;
mod file_finder;
pub(crate) mod filter_selections;
mod help;
pub(crate) mod jump;
pub(crate) mod lsp;
pub(crate) mod macro_recording;
pub(crate) mod marks;
pub(crate) mod movement;
mod palette;
mod pane;
mod picker;
mod prompt;
mod rebase;
pub(crate) mod review;
mod reword;
mod run;
pub(crate) mod search;
pub(crate) mod shell;
pub(crate) mod split_selection;
pub(crate) mod surround;
mod terminal;
pub(crate) mod textobject;
pub(crate) mod textobject_nav;
mod workspace;
pub(crate) mod yank;

use crate::{
    app::{Stoat, UpdateEffect},
    command_palette::CommandPalette,
    editor_state::{EditorId, EditorState},
    help::Help,
    host::FsHost,
    jumplist::JumpList,
    pane::{Axis, Direction, DockSide, FocusTarget, View},
    workspace_picker::WorkspacePicker,
};
pub(crate) use commits::pump_commits;
pub(crate) use file_finder::{close_file_finder, sync_file_finder_preview};
pub(crate) use lsp::pump_lsp_jumps;
pub(crate) use palette::sync_palette_picker;
pub(crate) use pane::{close_pane_by_id, restore_pane_after_term_exit};
#[cfg(test)]
pub(crate) use review::install_review_session;
pub(crate) use review::{pump_review_scan, PendingReviewScan};
use std::path::Path;
use stoat_action::{
    Action, ActionKind, Dump, OpenBuffer, OpenFile, OpenReviewAgentEdits, OpenReviewCommit,
    OpenReviewCommitRange, RenameWorkspace, ReviewExternalEdit, Run, SetCwd,
};
use stoat_text::{Anchor, BufferId, Selection};
pub(crate) use terminal::respawn_terminal_panes;

pub fn dispatch(stoat: &mut Stoat, action: &dyn Action) -> UpdateEffect {
    let group_buffer = if manages_own_undo_group(action.kind()) {
        None
    } else {
        begin_action_group(stoat)
    };
    let effect = match action.kind() {
        ActionKind::Quit => {
            if pane::close_focused_pane(stoat) {
                UpdateEffect::Redraw
            } else {
                UpdateEffect::Quit
            }
        },
        ActionKind::QuitAll => quit_all(stoat),
        ActionKind::QuitAllConfirm => quit_all_confirm(stoat),
        ActionKind::QuitAllCancel => quit_all_cancel(stoat),
        ActionKind::ShowVersion => show_version(stoat),
        ActionKind::OpenLogs => file::open_logs(stoat),
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
        ActionKind::OpenBuffer => {
            let open = action
                .as_any()
                .downcast_ref::<OpenBuffer>()
                .expect("OpenBuffer action downcast");
            file::open_file(stoat, &open.path);
            UpdateEffect::Redraw
        },
        ActionKind::OpenFileFinder => {
            file_finder::open_file_finder(stoat, crate::file_finder::OpenIntent::Replace, None)
        },
        ActionKind::OpenFileFinderHSplit => {
            file_finder::open_file_finder(stoat, crate::file_finder::OpenIntent::HSplit, None)
        },
        ActionKind::OpenFileFinderVSplit => {
            file_finder::open_file_finder(stoat, crate::file_finder::OpenIntent::VSplit, None)
        },
        ActionKind::OpenChangedFilePicker => file_finder::open_file_finder(
            stoat,
            crate::file_finder::OpenIntent::Replace,
            Some(crate::file_finder::FinderScope::Modified),
        ),
        ActionKind::OpenBufferPicker => file_finder::open_file_finder(
            stoat,
            crate::file_finder::OpenIntent::Replace,
            Some(crate::file_finder::FinderScope::Buffers),
        ),
        ActionKind::FileFinderSelectPrev => file_finder::file_finder_move_selection(stoat, -1),
        ActionKind::FileFinderSelectNext => file_finder::file_finder_move_selection(stoat, 1),
        ActionKind::FileFinderPageUp => file_finder::file_finder_page(stoat, -1),
        ActionKind::FileFinderPageDown => file_finder::file_finder_page(stoat, 1),
        ActionKind::FileFinderScopeToggle => file_finder::file_finder_scope_toggle(stoat),
        ActionKind::OpenCommandPalette => {
            let executor = stoat.executor.clone();
            let availability = crate::command_palette::Availability::from_stoat(stoat);
            let ws = stoat.active_workspace_mut();
            stoat.command_palette = Some(CommandPalette::new(ws, executor, availability));
            UpdateEffect::Redraw
        },
        ActionKind::OpenHelp => {
            let active = stoat.active_bindings_for_current_mode();
            let mode = stoat.focused_mode().to_string();
            let executor = stoat.executor.clone();
            let ws = stoat.active_workspace_mut();
            stoat.help = Some(Help::new(&mode, active, ws, executor));
            UpdateEffect::Redraw
        },
        ActionKind::Diff => {
            review::open_review(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::ToggleDiff => review::toggle_diff(stoat),
        ActionKind::AddSelectionBelow => movement::add_selection_below(stoat),
        ActionKind::AddSelectionAbove => movement::add_selection_above(stoat),
        ActionKind::SplitSelectionOnNewline => movement::split_selection_on_newline(stoat),
        ActionKind::SplitSelection => {
            split_selection::open(stoat, split_selection::RegexSelectKind::Split)
        },
        ActionKind::SelectRegex => {
            split_selection::open(stoat, split_selection::RegexSelectKind::Select)
        },
        ActionKind::KeepSelections => filter_selections::open_keep(stoat),
        ActionKind::RemoveSelections => filter_selections::open_remove(stoat),
        ActionKind::RecordMacro => macro_recording::toggle_record(stoat),
        ActionKind::ReplayMacro => macro_recording::arm_replay(stoat),
        ActionKind::ShellPipe => shell::open_pipe(stoat),
        ActionKind::ShellPipeTo => shell::open_pipe_to(stoat),
        ActionKind::ShellInsertOutput => shell::open_insert_output(stoat),
        ActionKind::ShellAppendOutput => shell::open_append_output(stoat),
        ActionKind::ShellKeepPipe => shell::open_keep_pipe(stoat),
        ActionKind::SaveBuffer => file::save_buffer(stoat),
        ActionKind::ForceSaveBuffer => file::force_save_buffer(stoat),
        ActionKind::WriteQuit => file::write_quit(stoat),
        ActionKind::CloseBuffer => file::close_buffer(stoat),
        ActionKind::AcceptCompletion => crate::completion::accept::execute(stoat),
        ActionKind::SmartTab => completion::smart_tab(stoat),
        ActionKind::InsertTab => completion::insert_tab(stoat),
        ActionKind::TriggerCompletion => completion::trigger_completion(stoat),
        ActionKind::AlignSelections => movement::align_selections(stoat),
        ActionKind::Increment => movement::increment(stoat),
        ActionKind::Decrement => movement::decrement(stoat),
        ActionKind::DeleteSelection => movement::delete_selection(stoat),
        ActionKind::DeleteSelectionNoYank => movement::delete_selection_no_yank(stoat),
        ActionKind::ChangeSelection => movement::change_selection(stoat),
        ActionKind::Undo => movement::undo(stoat),
        ActionKind::Redo => movement::redo(stoat),
        ActionKind::CommitUndoCheckpoint => movement::commit_undo_checkpoint(stoat),
        ActionKind::IndentSelection => movement::indent_selection(stoat),
        ActionKind::UnindentSelection => movement::unindent_selection(stoat),
        ActionKind::ToggleComments => movement::toggle_comments(stoat),
        ActionKind::ToggleSyntaxHighlight => {
            stoat.syntax_highlight = !stoat.syntax_highlight;
            UpdateEffect::Redraw
        },
        ActionKind::ToggleInlayHints => {
            stoat.inlay_hints_enabled = !stoat.inlay_hints_enabled;
            if !stoat.inlay_hints_enabled {
                lsp::clear_inlay_hints(stoat);
            }
            stoat.last_inlay_hint_key = None;
            stoat.pending_inlay_hint_request = None;
            UpdateEffect::Redraw
        },
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
        ActionKind::EnterInsertMode => movement::enter_insert_mode(stoat),
        ActionKind::AppendMode => movement::append_mode(stoat),
        ActionKind::InsertAtLineEnd => movement::insert_at_line_end(stoat),
        ActionKind::InsertAtLineStart => movement::insert_at_line_start(stoat),
        ActionKind::OpenBelow => movement::open_line(stoat, movement::OpenDir::Below),
        ActionKind::OpenAbove => movement::open_line(stoat, movement::OpenDir::Above),
        ActionKind::ReplaceChar => movement::set_pending_replace(stoat),
        ActionKind::GotoFileStart => movement::goto_file_start(stoat, false),
        ActionKind::GotoLastLine => movement::goto_last_line(stoat, false),
        ActionKind::GotoLineNumber => movement::goto_line_number(stoat),
        ActionKind::GotoColumn => movement::goto_column(stoat, false),
        ActionKind::ExtendGotoColumn => movement::goto_column(stoat, true),
        ActionKind::GotoNextChange => movement::goto_change(stoat, movement::ChangeDir::Next),
        ActionKind::GotoPrevChange => movement::goto_change(stoat, movement::ChangeDir::Prev),
        ActionKind::GotoNextParagraph => movement::goto_paragraph(stoat, movement::ParaDir::Next),
        ActionKind::GotoPrevParagraph => movement::goto_paragraph(stoat, movement::ParaDir::Prev),
        ActionKind::MatchBrackets => movement::match_brackets(stoat),
        ActionKind::ExpandSelection => movement::expand_selection(stoat),
        ActionKind::ShrinkSelection => movement::shrink_selection(stoat),
        ActionKind::SelectNextSibling => {
            movement::select_sibling(stoat, movement::SiblingDir::Next, false)
        },
        ActionKind::SelectPrevSibling => {
            movement::select_sibling(stoat, movement::SiblingDir::Prev, false)
        },
        ActionKind::SelectAllSiblings => movement::select_all_siblings(stoat),
        ActionKind::SelectAllChildren => movement::select_all_children(stoat),
        ActionKind::ExtendSelectNextSibling => {
            movement::select_sibling(stoat, movement::SiblingDir::Next, true)
        },
        ActionKind::ExtendSelectPrevSibling => {
            movement::select_sibling(stoat, movement::SiblingDir::Prev, true)
        },
        ActionKind::MoveParentNodeStart => {
            movement::move_to_parent_bound(stoat, movement::NodeBound::Start, false)
        },
        ActionKind::MoveParentNodeEnd => {
            movement::move_to_parent_bound(stoat, movement::NodeBound::End, false)
        },
        ActionKind::ExtendMoveParentNodeStart => {
            movement::move_to_parent_bound(stoat, movement::NodeBound::Start, true)
        },
        ActionKind::ExtendMoveParentNodeEnd => {
            movement::move_to_parent_bound(stoat, movement::NodeBound::End, true)
        },
        ActionKind::SaveSelection => jump::save_selection(stoat),
        ActionKind::JumpBackward => jump::jump_backward(stoat),
        ActionKind::JumpForward => jump::jump_forward(stoat),
        ActionKind::OpenJumplistPicker => open_jumplist_picker(stoat),
        ActionKind::OpenDiagnosticsPicker => open_diagnostics_picker(stoat),
        ActionKind::OpenWorkspaceDiagnosticsPicker => open_workspace_diagnostics_picker(stoat),
        ActionKind::OpenGlobalSearch => open_global_search(stoat),
        ActionKind::JumplistPickerNext => picker::jumplist_picker_next(stoat),
        ActionKind::JumplistPickerPrev => picker::jumplist_picker_prev(stoat),
        ActionKind::JumplistPickerSelect => picker::jumplist_picker_select(stoat),
        ActionKind::JumplistPickerClose => picker::jumplist_picker_close(stoat),
        ActionKind::DiagnosticsPickerNext => picker::diagnostics_picker_next(stoat),
        ActionKind::DiagnosticsPickerPrev => picker::diagnostics_picker_prev(stoat),
        ActionKind::DiagnosticsPickerSelect => picker::diagnostics_picker_select(stoat),
        ActionKind::DiagnosticsPickerClose => picker::diagnostics_picker_close(stoat),
        ActionKind::LocationPickerNext => picker::location_picker_next(stoat),
        ActionKind::LocationPickerPrev => picker::location_picker_prev(stoat),
        ActionKind::LocationPickerSelect => picker::location_picker_select(stoat),
        ActionKind::LocationPickerClose => picker::location_picker_close(stoat),
        ActionKind::GlobalSearchPickerNext => picker::global_search_picker_next(stoat),
        ActionKind::GlobalSearchPickerPrev => picker::global_search_picker_prev(stoat),
        ActionKind::GlobalSearchPickerSelect => picker::global_search_picker_select(stoat),
        ActionKind::GlobalSearchPickerClose => picker::global_search_picker_close(stoat),
        ActionKind::FindNextChar => {
            movement::set_pending_find(stoat, movement::FindKind::NextChar, false)
        },
        ActionKind::FindPrevChar => {
            movement::set_pending_find(stoat, movement::FindKind::PrevChar, false)
        },
        ActionKind::TillNextChar => {
            movement::set_pending_find(stoat, movement::FindKind::TillNextChar, false)
        },
        ActionKind::TillPrevChar => {
            movement::set_pending_find(stoat, movement::FindKind::TillPrevChar, false)
        },
        ActionKind::ExtendFindNextChar => {
            movement::set_pending_find(stoat, movement::FindKind::NextChar, true)
        },
        ActionKind::ExtendFindPrevChar => {
            movement::set_pending_find(stoat, movement::FindKind::PrevChar, true)
        },
        ActionKind::ExtendTillNextChar => {
            movement::set_pending_find(stoat, movement::FindKind::TillNextChar, true)
        },
        ActionKind::ExtendTillPrevChar => {
            movement::set_pending_find(stoat, movement::FindKind::TillPrevChar, true)
        },
        ActionKind::SetMark => marks::set_mark(stoat),
        ActionKind::GotoMark => marks::goto_mark(stoat),
        ActionKind::GotoMarkExact => marks::goto_mark_exact(stoat),
        ActionKind::SurroundAdd => surround::surround_add(stoat),
        ActionKind::SurroundReplace => surround::surround_replace(stoat),
        ActionKind::SurroundDelete => surround::surround_delete(stoat),
        ActionKind::SelectTextobjectAround => textobject::select_textobject_around(stoat),
        ActionKind::SelectTextobjectInner => textobject::select_textobject_inner(stoat),
        ActionKind::GotoNextFunction => textobject_nav::goto_textobject(
            stoat,
            textobject_nav::NavKind::Function,
            textobject_nav::NavDirection::Next,
        ),
        ActionKind::GotoPrevFunction => textobject_nav::goto_textobject(
            stoat,
            textobject_nav::NavKind::Function,
            textobject_nav::NavDirection::Prev,
        ),
        ActionKind::GotoNextClass => textobject_nav::goto_textobject(
            stoat,
            textobject_nav::NavKind::Class,
            textobject_nav::NavDirection::Next,
        ),
        ActionKind::GotoPrevClass => textobject_nav::goto_textobject(
            stoat,
            textobject_nav::NavKind::Class,
            textobject_nav::NavDirection::Prev,
        ),
        ActionKind::OpenSearchInput => search::open_search_input(stoat),
        ActionKind::OpenReverseSearchInput => search::open_reverse_search_input(stoat),
        ActionKind::SearchNext => search::search_next(stoat),
        ActionKind::SearchPrev => search::search_prev(stoat),
        ActionKind::Yank => yank::yank(stoat),
        ActionKind::PasteAfter => yank::paste_after(stoat),
        ActionKind::PasteBefore => yank::paste_before(stoat),
        ActionKind::ReplaceWithYanked => yank::replace_with_yanked(stoat),
        ActionKind::YankToClipboard => yank::yank_to_clipboard(stoat),
        ActionKind::YankMainToClipboard => yank::yank_main_to_clipboard(stoat),
        ActionKind::PasteClipboardAfter => yank::paste_clipboard_after(stoat),
        ActionKind::PasteClipboardBefore => yank::paste_clipboard_before(stoat),
        ActionKind::SelectRegister => yank::select_register(stoat),
        ActionKind::InsertRegister => yank::insert_register(stoat),
        ActionKind::RepeatLastMotion => movement::repeat_last_motion(stoat),
        ActionKind::GotoWindowTop => {
            movement::goto_window(stoat, movement::WindowAlign::Top, false)
        },
        ActionKind::GotoWindowCenter => {
            movement::goto_window(stoat, movement::WindowAlign::Center, false)
        },
        ActionKind::GotoWindowBottom => {
            movement::goto_window(stoat, movement::WindowAlign::Bottom, false)
        },
        ActionKind::GotoWord => movement::goto_word(stoat),
        ActionKind::ExtendGotoFirstNonwhitespace => movement::goto_first_nonwhitespace(stoat, true),
        ActionKind::ExtendGotoFileStart => movement::goto_file_start(stoat, true),
        ActionKind::ExtendGotoLastLine => movement::goto_last_line(stoat, true),
        ActionKind::ExtendGotoWindowTop => {
            movement::goto_window(stoat, movement::WindowAlign::Top, true)
        },
        ActionKind::ExtendGotoWindowCenter => {
            movement::goto_window(stoat, movement::WindowAlign::Center, true)
        },
        ActionKind::ExtendGotoWindowBottom => {
            movement::goto_window(stoat, movement::WindowAlign::Bottom, true)
        },
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
        ActionKind::EnsureSelectionsForward => movement::ensure_selections_forward(stoat),
        ActionKind::SelectAll => movement::select_all(stoat),
        ActionKind::SelectLineBelow => movement::select_line_below(stoat),
        ActionKind::ExtendToLineBounds => movement::extend_to_line_bounds(stoat),
        ActionKind::ShrinkToLineBounds => movement::shrink_to_line_bounds(stoat),
        ActionKind::KeepPrimarySelection => movement::keep_primary_selection(stoat),
        ActionKind::RemovePrimarySelection => movement::remove_primary_selection(stoat),
        ActionKind::RotateSelectionsForward => movement::rotate_selections_forward(stoat),
        ActionKind::RotateSelectionsBackward => movement::rotate_selections_backward(stoat),
        ActionKind::RotateSelectionContentsForward => {
            movement::rotate_selection_contents_forward(stoat)
        },
        ActionKind::RotateSelectionContentsBackward => {
            movement::rotate_selection_contents_backward(stoat)
        },
        ActionKind::JoinSelections => movement::join_selections(stoat),
        ActionKind::JoinSelectionsSpace => movement::join_selections_space(stoat),
        ActionKind::TrimSelections => movement::trim_selections(stoat),
        ActionKind::OpenRun => run::open_run(stoat),
        ActionKind::SpawnClaude => agent::spawn_claude_pane(stoat),
        ActionKind::Terminal => terminal::open_terminal_pane(stoat),
        ActionKind::RunSubmit => run::run_submit(stoat),
        ActionKind::RunInterrupt => run::run_interrupt(stoat),
        ActionKind::RunModalDismiss => run::run_modal_dismiss(stoat),
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
        ActionKind::ToggleDockRight => pane::toggle_dock(stoat, DockSide::Right),
        ActionKind::ToggleDockLeft => pane::toggle_dock(stoat, DockSide::Left),
        ActionKind::JumpToMoveSource => {
            if focused_editor_mut(stoat).is_some_and(|e| e.review_view.is_some()) {
                review::jump_to_move(stoat, review::MoveJumpDir::Source)
            } else {
                movement::move_nav(stoat, movement::MoveNavigation::FirstSource)
            }
        },
        ActionKind::JumpToMoveTarget => {
            if focused_editor_mut(stoat).is_some_and(|e| e.review_view.is_some()) {
                review::jump_to_move(stoat, review::MoveJumpDir::Target)
            } else {
                movement::move_nav(stoat, movement::MoveNavigation::Target)
            }
        },
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
                    same_side_target = ?summary.target_ref,
                    "move relationships under cursor"
                );
                UpdateEffect::Redraw
            } else {
                UpdateEffect::None
            }
        },
        ActionKind::GotoNextDiagnostic => {
            lsp::goto_diagnostic(stoat, lsp::DiagnosticDirection::Next)
        },
        ActionKind::GotoPrevDiagnostic => {
            lsp::goto_diagnostic(stoat, lsp::DiagnosticDirection::Prev)
        },
        ActionKind::GotoDefinition => lsp::goto_definition(stoat),
        ActionKind::GotoDeclaration => lsp::goto_declaration(stoat),
        ActionKind::GotoTypeDefinition => lsp::goto_type_definition(stoat),
        ActionKind::GotoImplementation => lsp::goto_implementation(stoat),
        ActionKind::GotoCaller => crate::code_index::nav::goto_caller(stoat),
        ActionKind::GotoCallee => crate::code_index::nav::goto_callee(stoat),
        ActionKind::GotoReferences => lsp::goto_references(stoat),
        ActionKind::GotoImplementors => crate::code_index::nav::goto_implementors(stoat),
        ActionKind::GotoDiffCallerUp => crate::code_index::nav::goto_diff_caller_up(stoat),
        ActionKind::GotoDiffCalleeDown => crate::code_index::nav::goto_diff_callee_down(stoat),
        ActionKind::MarkTrailStart => crate::code_index::nav::mark_trail_start(stoat),
        ActionKind::MarkTrailEnd => crate::code_index::nav::mark_trail_end(stoat),
        ActionKind::TrailNext => crate::code_index::nav::trail_next(stoat),
        ActionKind::TrailPrev => crate::code_index::nav::trail_prev(stoat),
        ActionKind::Hover => lsp::hover(stoat),
        ActionKind::CodeAction => lsp::code_action(stoat),
        ActionKind::RenameSymbol => lsp::rename_symbol(stoat),
        ActionKind::OpenSymbolPicker => lsp::open_symbol_picker(stoat),
        ActionKind::OpenWorkspaceSymbolPicker => lsp::open_workspace_symbol_picker(stoat),
        ActionKind::FormatSelections => lsp::format_selections(stoat),
        ActionKind::Format => lsp::format_document(stoat),
        ActionKind::ReviewNextChunk => review::review_step(stoat, review::ReviewStep::Next),
        ActionKind::ReviewPrevChunk => review::review_step(stoat, review::ReviewStep::Prev),
        ActionKind::ReviewStageChunk => review::review_mark(stoat, review::ReviewMark::Stage),
        ActionKind::ReviewUnstageChunk => review::review_mark(stoat, review::ReviewMark::Unstage),
        ActionKind::ReviewToggleStage => review::review_mark(stoat, review::ReviewMark::Toggle),
        ActionKind::ReviewSkipChunk => review::review_mark(stoat, review::ReviewMark::Skip),
        ActionKind::ReviewRefresh => review::review_refresh(stoat, None),
        ActionKind::ReviewExternalEdit => {
            let a = action
                .as_any()
                .downcast_ref::<ReviewExternalEdit>()
                .expect("ReviewExternalEdit action downcast");
            review::review_external_edit(stoat, &a.path)
        },
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
        ActionKind::WorkspacePickerNext => workspace::workspace_picker_next(stoat),
        ActionKind::WorkspacePickerPrev => workspace::workspace_picker_prev(stoat),
        ActionKind::WorkspacePickerSelect => workspace::workspace_picker_select(stoat),
        ActionKind::WorkspacePickerClose => workspace::workspace_picker_close(stoat),
        ActionKind::CloseWorkspace => workspace::close_workspace(stoat),
        ActionKind::RenameWorkspace => {
            let action = action
                .as_any()
                .downcast_ref::<RenameWorkspace>()
                .expect("RenameWorkspace action downcast");
            workspace::rename_workspace(stoat, &action.name);
            UpdateEffect::Redraw
        },
        ActionKind::SetCwd => {
            let action = action
                .as_any()
                .downcast_ref::<SetCwd>()
                .expect("SetCwd action downcast");
            workspace::set_cwd(stoat, &action.path);
            UpdateEffect::Redraw
        },
        ActionKind::ShowCwd => {
            workspace::show_cwd(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::ReloadEnv => {
            crate::project_env::reload_active_workspace(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::SubmitPromptInput => prompt::submit_prompt_input(stoat),
        ActionKind::CancelPromptInput => prompt::cancel_prompt_input(stoat),
        ActionKind::PromptInsertNewline => prompt::prompt_insert_newline(stoat),
        ActionKind::PaletteSelectPrev => prompt::palette_select_prev(stoat),
        ActionKind::PaletteSelectNext => prompt::palette_select_next(stoat),
        ActionKind::PalettePageUp => palette::palette_page(stoat, -1),
        ActionKind::PalettePageDown => palette::palette_page(stoat, 1),
        ActionKind::PaletteScopeToggle => palette::palette_scope_toggle(stoat),
        ActionKind::OpenLastPicker => open_last_picker(stoat),
    };
    end_action_group(stoat, group_buffer);
    if matches!(effect, UpdateEffect::Redraw) && is_picker_open_kind(action.kind()) {
        stoat.last_picker_action = Some(action.def().name());
    }
    effect
}

/// Actions that drive the undo history themselves, so [`dispatch`] must not wrap
/// them in an action group. `CommitUndoCheckpoint` seals and reopens the insert
/// group directly. Undo and redo pop and replay existing groups.
fn manages_own_undo_group(kind: ActionKind) -> bool {
    matches!(
        kind,
        ActionKind::Undo | ActionKind::Redo | ActionKind::CommitUndoCheckpoint
    )
}

/// Open an undo group on the focused editor's buffer so every edit the
/// dispatched action makes collapses into one undo step, capturing the
/// pre-action selections to restore on undo. Returns the grouped buffer, or
/// `None` when the focused pane is not an editor. Lazy group materialization
/// means a non-editing action leaves the undo history untouched.
fn begin_action_group(stoat: &mut Stoat) -> Option<BufferId> {
    let (editor_id, buffer_id) = stoat.focused_editor_ids()?;
    let before = editor_selection_snapshot(stoat, editor_id);
    let buffer = stoat.active_workspace().buffers.get(buffer_id)?;
    buffer.write().expect("poisoned").begin_group(before);
    Some(buffer_id)
}

/// Seal the group opened by [`begin_action_group`], capturing the post-action
/// selections to restore on redo. The group self-discards when the action made
/// no edits.
fn end_action_group(stoat: &mut Stoat, buffer_id: Option<BufferId>) {
    let Some(buffer_id) = buffer_id else {
        return;
    };
    let after = focused_selection_snapshot(stoat, buffer_id);
    if let Some(buffer) = stoat.active_workspace().buffers.get(buffer_id) {
        buffer.write().expect("poisoned").seal_group(after);
    }
}

/// Selections of `editor_id` as an owned snapshot, empty when it is gone.
fn editor_selection_snapshot(stoat: &Stoat, editor_id: EditorId) -> Vec<Selection<Anchor>> {
    stoat
        .active_workspace()
        .editors
        .get(editor_id)
        .map(|editor| editor.selections.all_anchors().to_vec())
        .unwrap_or_default()
}

/// Selections of the focused editor when it still shows `buffer_id`, else empty.
/// An action that changed focus away from the grouped buffer seals with no
/// post-selections, so redo revalidates rather than restoring a foreign set.
fn focused_selection_snapshot(stoat: &Stoat, buffer_id: BufferId) -> Vec<Selection<Anchor>> {
    match stoat.focused_editor_ids() {
        Some((editor_id, focused_buffer)) if focused_buffer == buffer_id => {
            editor_selection_snapshot(stoat, editor_id)
        },
        _ => Vec::new(),
    }
}

/// Action kinds whose handlers open a top-level picker modal.
/// Used by the `dispatch` post-hook that records the most
/// recently opened picker for [`OpenLastPicker`] recall.
fn is_picker_open_kind(kind: ActionKind) -> bool {
    matches!(
        kind,
        ActionKind::OpenFileFinder
            | ActionKind::OpenFileFinderHSplit
            | ActionKind::OpenFileFinderVSplit
            | ActionKind::OpenBufferPicker
            | ActionKind::OpenChangedFilePicker
            | ActionKind::OpenCommandPalette
            | ActionKind::OpenJumplistPicker
            | ActionKind::OpenGlobalSearch
            | ActionKind::OpenDiagnosticsPicker
            | ActionKind::OpenWorkspaceDiagnosticsPicker
    )
}

/// Drive [`ActionKind::OpenLastPicker`]. Re-fires the action
/// recorded on `Stoat::last_picker_action` so the user can
/// resume the most recently opened picker without remembering
/// the original chord. The picker rebuilds fresh from current
/// state -- prior query and selection are not restored. No-op
/// when nothing is recorded or the registry lookup fails.
fn open_last_picker(stoat: &mut Stoat) -> UpdateEffect {
    let Some(name) = stoat.last_picker_action else {
        return UpdateEffect::None;
    };
    let Some(entry) = stoat_action::registry::lookup(name) else {
        return UpdateEffect::None;
    };
    let Ok(action) = (entry.create)(&[]) else {
        return UpdateEffect::None;
    };
    dispatch(stoat, &*action)
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
        _ => None,
    }
}

/// Return a mutable reference to the focused pane's jumplist, or `None` when
/// focus is on a dock rather than a split pane.
///
/// The jumplist lives on the pane rather than the editor so it survives the
/// `EditorState` swap a cross-file open performs. Resolves the same pane
/// [`focused_editor_mut`] reads, so a recorded jump matches the editor it was
/// taken from.
pub(crate) fn focused_pane_jumplist(stoat: &mut Stoat) -> Option<&mut JumpList> {
    let ws = stoat.active_workspace_mut();
    match ws.focus {
        FocusTarget::SplitPane(_) => {
            let focused = ws.panes.focus();
            Some(&mut ws.panes.pane_mut(focused).jumplist)
        },
        FocusTarget::Dock(_) => None,
    }
}

/// Remove `editor_id` from the workspace's editor store unless it is still
/// live. An editor stays alive while a split pane shows it, or while it is
/// the review editor parked off-screen by a toggled-off diff session (see
/// [`crate::review_session::ReviewSession::toggled_off`]).
///
/// Editor-swapping sites (opening a file, rebuilding the review, closing a
/// review) call this on the editor they displaced so a pane never leaves a
/// dangling editor behind, while the parked review editor survives a toggle.
pub(crate) fn gc_editor_if_unreferenced(ws: &mut crate::workspace::Workspace, editor_id: EditorId) {
    let referenced = ws
        .panes
        .split_panes()
        .any(|(_, p)| matches!(p.view, View::Editor(eid) if eid == editor_id));
    if referenced {
        return;
    }
    let parked = ws
        .review
        .as_ref()
        .is_some_and(|r| r.toggled_off && r.view_editor == Some(editor_id));
    if parked {
        return;
    }
    ws.editors.remove(editor_id);
}

/// Drive [`ActionKind::OpenGlobalSearch`]. Opens the input modal so the
/// user can type a regex pattern; submission triggers the workspace
/// scan via [`global_search_submit`].
fn open_global_search(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.global_search_input.is_some() {
        return UpdateEffect::None;
    }
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let input = crate::input_view::InputView::create(
        ws,
        executor,
        crate::input_view::SubmitTarget::GlobalSearch,
        "",
        "insert",
        1,
    );
    stoat.global_search_input = Some(crate::global_search::GlobalSearchInputState { input });
    UpdateEffect::Redraw
}

/// Submit the global-search query. Reads the typed pattern, runs the
/// scan via [`crate::global_search::perform_search`], and stores a
/// [`crate::global_search::GlobalSearchPicker`] on `Stoat`. Empty or
/// invalid patterns close the input without opening the picker.
/// Returns `true` when the input modal was open.
pub(crate) fn global_search_submit(stoat: &mut Stoat) -> bool {
    let Some(state) = stoat.global_search_input.take() else {
        return false;
    };
    let query = state.input.text(stoat.active_workspace());
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);
    if query.is_empty() {
        return true;
    }
    let git_root = stoat.active_workspace().git_root.clone();
    let matches = match crate::global_search::perform_search(&*stoat.fs_host, &git_root, &query) {
        Ok(m) => m,
        Err(_) => {
            return true;
        },
    };
    if matches.is_empty() {
        return true;
    }
    stoat.global_search = Some(crate::global_search::GlobalSearchPicker::new(
        matches, query,
    ));
    true
}

/// Cancel the global-search input modal without running the scan.
/// Returns `true` when the input was open.
pub(crate) fn global_search_cancel(stoat: &mut Stoat) -> bool {
    let Some(state) = stoat.global_search_input.take() else {
        return false;
    };
    let ws = stoat.active_workspace_mut();
    state.input.dispose(ws);
    true
}

/// Drive [`ActionKind::OpenJumplistPicker`]. Builds a snapshot of the focused
/// pane's jumplist and stores it in [`Stoat::jumplist_picker`]. No-op when the
/// jumplist is empty or focus is on a dock.
fn open_jumplist_picker(stoat: &mut Stoat) -> UpdateEffect {
    let Some(jumplist) = focused_pane_jumplist(stoat) else {
        return UpdateEffect::None;
    };
    if jumplist.entries().is_empty() {
        return UpdateEffect::None;
    }
    let jumplist = jumplist.clone();
    let picker =
        crate::jumplist_picker::JumplistPicker::new(&jumplist, &stoat.active_workspace().buffers);
    stoat.jumplist_picker = Some(picker);
    UpdateEffect::Redraw
}

/// Drive [`ActionKind::OpenWorkspaceDiagnosticsPicker`].
/// Snapshots every `(path, diagnostic)` pair currently in
/// `Stoat::diagnostics` and stores the workspace-scope picker
/// on `Stoat::diagnostics_picker`. No-op when the workspace
/// has no diagnostics loaded.
fn open_workspace_diagnostics_picker(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.diagnostics.iter().next().is_none() {
        return UpdateEffect::None;
    }
    let picker = crate::diagnostics_picker::DiagnosticsPicker::workspace(&stoat.diagnostics);
    if picker.entries().is_empty() {
        return UpdateEffect::None;
    }
    stoat.diagnostics_picker = Some(picker);
    UpdateEffect::Redraw
}

/// Drive [`ActionKind::OpenDiagnosticsPicker`]. Snapshots the
/// focused buffer's diagnostic list from `Stoat::diagnostics`
/// and stores the picker on `Stoat::diagnostics_picker`. No-op
/// when the focused pane is not an editor, the buffer has no
/// path on disk, or the path has no diagnostics.
fn open_diagnostics_picker(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let editor_id = match ws.focus {
        FocusTarget::SplitPane(pane_id) => match ws.panes.pane(pane_id).view {
            View::Editor(id) => id,
            _ => return UpdateEffect::None,
        },
        FocusTarget::Dock(_) => return UpdateEffect::None,
    };
    let buffer_id = match ws.editors.get(editor_id) {
        Some(e) => e.buffer_id,
        None => return UpdateEffect::None,
    };
    let path = match ws.buffers.path_for(buffer_id) {
        Some(p) => p.to_path_buf(),
        None => return UpdateEffect::None,
    };
    let diagnostics = stoat.diagnostics.get(&path).to_vec();
    if diagnostics.is_empty() {
        return UpdateEffect::None;
    }
    let ws = stoat.active_workspace_mut();
    let buffer = match ws.buffers.get(buffer_id) {
        Some(b) => b,
        None => return UpdateEffect::None,
    };
    let picker = {
        let guard = buffer.read().expect("buffer poisoned");
        crate::diagnostics_picker::DiagnosticsPicker::new(&diagnostics, &guard)
    };
    stoat.diagnostics_picker = Some(picker);
    UpdateEffect::Redraw
}

/// Drive [`ActionKind::QuitAll`]. Quits immediately when no buffer is
/// dirty; otherwise opens the [`crate::quit_all_confirm::QuitAllConfirm`]
/// modal and waits for the user to confirm or cancel.
fn quit_all(stoat: &mut Stoat) -> UpdateEffect {
    let mut dirty: Vec<crate::buffer_registry::DirtyBuffer> = Vec::new();
    let mut git_root: Option<std::path::PathBuf> = None;
    for ws in stoat.workspaces.values() {
        let ws_dirty = ws.buffers.dirty_buffers();
        if !ws_dirty.is_empty() && git_root.is_none() {
            git_root = Some(ws.git_root.clone());
        }
        dirty.extend(ws_dirty);
    }
    if dirty.is_empty() {
        return UpdateEffect::Quit;
    }
    let root = git_root.unwrap_or_else(|| stoat.active_workspace().git_root.clone());
    stoat.quit_all_confirm = Some(crate::quit_all_confirm::QuitAllConfirm::new(&dirty, &root));
    UpdateEffect::Redraw
}

/// Confirm the quit-all prompt, exiting and discarding its unsaved buffers.
fn quit_all_confirm(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.quit_all_confirm.is_none() {
        return UpdateEffect::None;
    }
    UpdateEffect::Quit
}

/// Dismiss the quit-all prompt without exiting.
fn quit_all_cancel(stoat: &mut Stoat) -> UpdateEffect {
    stoat.quit_all_confirm = None;
    UpdateEffect::Redraw
}

/// Drive [`ActionKind::ShowVersion`]. Raise an app-level badge reporting
/// stoat's version, plus stoatty's when running inside it.
///
/// stoatty's version is read from the `STOATTY_VERSION` env var it sets on the
/// child. An older stoatty that sets only `STOATTY=1` yields "unknown". The
/// badge is retired by the next key press (see [`Stoat::handle_key`]).
fn show_version(stoat: &mut Stoat) -> UpdateEffect {
    use crate::badge::{Anchor, Badge, BadgeSource, BadgeState};

    let mut label = format!("stoat {}", stoat.version_info);
    if stoat.stoatty {
        let stoatty = stoat
            .env_host()
            .var("STOATTY_VERSION")
            .unwrap_or_else(|| "unknown".to_string());
        label.push_str(&format!(" | stoatty {stoatty}"));
    }

    stoat.badges.remove_by_source(BadgeSource::Version);
    stoat.badges.insert(Badge {
        source: BadgeSource::Version,
        anchor: Anchor::BottomRight,
        state: BadgeState::Complete,
        label,
        detail: None,
    });
    UpdateEffect::Redraw
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
    use crate::test_harness::{editor, keys};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use std::{sync::Arc, time::Duration};
    use stoat_action::{
        AddSelectionBelow, CollapseSelection, ExtendDown, ExtendLeft, ExtendNextWordEnd,
        ExtendNextWordStart, ExtendPrevWordEnd, ExtendPrevWordStart, ExtendRight,
        ExtendToFileStart, ExtendToLastLine, ExtendToLineEnd, ExtendToLineStart, ExtendUp,
        FlipSelections, HalfPageDown, MoveDown, MoveLeft, MoveNextWordEnd, MoveNextWordStart,
        MovePrevWordEnd, MovePrevWordStart, MoveRight, MoveUp, PageDown, PageUp, Quit, QuitAll,
        RenameWorkspace, SaveSelection, SelectAll, SplitNewRight, SplitRight,
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
        stoat.update(Event::Resize(80, 24));
        stoat
    }

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
    fn dispatch_quit_all_with_dirty_buffer_opens_modal() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "unsaved");
        assert_eq!(dispatch(&mut stoat, &QuitAll), UpdateEffect::Redraw);
        assert!(stoat.quit_all_confirm.is_some());
    }

    #[test]
    fn quit_all_confirm_y_quits() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("unsaved");
        dispatch(&mut h.stoat, &QuitAll);
        assert!(h.stoat.quit_all_confirm.is_some());
        assert_eq!(
            h.stoat.update(Event::Key(keys::key(KeyCode::Char('y')))),
            UpdateEffect::Quit
        );
    }

    #[test]
    fn quit_all_confirm_n_cancels() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("unsaved");
        dispatch(&mut h.stoat, &QuitAll);
        assert_eq!(
            h.stoat.update(Event::Key(keys::key(KeyCode::Char('n')))),
            UpdateEffect::Redraw
        );
        assert!(h.stoat.quit_all_confirm.is_none());
    }

    #[test]
    fn quit_all_confirm_esc_cancels() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("unsaved");
        dispatch(&mut h.stoat, &QuitAll);
        assert_eq!(
            h.stoat.update(Event::Key(keys::key(KeyCode::Esc))),
            UpdateEffect::Redraw
        );
        assert!(h.stoat.quit_all_confirm.is_none());
    }

    #[test]
    fn quit_all_confirm_ctrl_c_cancels() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("unsaved");
        dispatch(&mut h.stoat, &QuitAll);
        let event = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(h.stoat.update(Event::Key(event)), UpdateEffect::Redraw);
        assert!(h.stoat.quit_all_confirm.is_none());
    }

    #[test]
    fn open_jumplist_picker_with_empty_jumplist_is_noop() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "alpha\nbeta\n");
        assert_eq!(
            dispatch(&mut stoat, &stoat_action::OpenJumplistPicker),
            UpdateEffect::None
        );
        assert!(stoat.jumplist_picker.is_none());
    }

    #[test]
    fn open_jumplist_picker_opens_modal_when_jumplist_has_entries() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "alpha\nbeta\ngamma\n");
        dispatch(&mut stoat, &SaveSelection);
        dispatch(&mut stoat, &MoveDown);
        dispatch(&mut stoat, &SaveSelection);
        assert_eq!(
            dispatch(&mut stoat, &stoat_action::OpenJumplistPicker),
            UpdateEffect::Redraw
        );
        let picker = stoat.jumplist_picker.as_ref().expect("modal open");
        assert_eq!(picker.entries().len(), 2);
    }

    #[test]
    fn jumplist_picker_enter_jumps_focused_cursor() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("alpha\nbeta\ngamma\n");
        dispatch(&mut h.stoat, &SaveSelection);
        dispatch(&mut h.stoat, &MoveDown);
        dispatch(&mut h.stoat, &MoveDown);
        dispatch(&mut h.stoat, &SaveSelection);
        dispatch(&mut h.stoat, &stoat_action::OpenJumplistPicker);
        h.stoat.update(Event::Key(keys::key(KeyCode::Up)));
        assert_eq!(
            h.stoat.update(Event::Key(keys::key(KeyCode::Enter))),
            UpdateEffect::Redraw
        );
        assert!(h.stoat.jumplist_picker.is_none());
        assert_eq!(editor::head_offsets(&mut h.stoat), vec![0]);
    }

    #[test]
    fn jumplist_picker_esc_closes_without_jumping() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("alpha\nbeta\n");
        dispatch(&mut h.stoat, &MoveDown);
        let before = editor::head_offsets(&mut h.stoat);
        dispatch(&mut h.stoat, &SaveSelection);
        dispatch(&mut h.stoat, &stoat_action::OpenJumplistPicker);
        assert_eq!(
            h.stoat.update(Event::Key(keys::key(KeyCode::Esc))),
            UpdateEffect::Redraw
        );
        assert!(h.stoat.jumplist_picker.is_none());
        assert_eq!(editor::head_offsets(&mut h.stoat), before);
    }

    #[test]
    fn jumplist_picker_ctrl_c_closes_without_jumping() {
        let mut h = Stoat::test();
        h.seed_focused_buffer("alpha\nbeta\n");
        dispatch(&mut h.stoat, &SaveSelection);
        dispatch(&mut h.stoat, &stoat_action::OpenJumplistPicker);
        let event = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(h.stoat.update(Event::Key(event)), UpdateEffect::Redraw);
        assert!(h.stoat.jumplist_picker.is_none());
    }

    #[test]
    fn open_global_search_opens_input_modal() {
        let mut stoat = stoat();
        assert_eq!(
            dispatch(&mut stoat, &stoat_action::OpenGlobalSearch),
            UpdateEffect::Redraw
        );
        assert!(stoat.global_search_input.is_some());
        assert_eq!(stoat.focused_mode(), "insert");
    }

    #[test]
    fn global_search_submit_with_no_matches_closes_input() {
        let mut h = Stoat::test();
        let root = std::path::PathBuf::from("/repo");
        h.fake_fs().insert_file(root.join("a.rs"), b"hello\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &stoat_action::OpenGlobalSearch);
        h.type_text("zzz_no_match");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        assert!(h.stoat.global_search_input.is_none());
        assert!(h.stoat.global_search.is_none());
    }

    #[test]
    fn global_search_submit_with_matches_opens_picker() {
        let mut h = Stoat::test();
        let root = std::path::PathBuf::from("/repo");
        h.fake_fs().insert_file(root.join("a.rs"), b"alpha\nbeta\n");
        h.fake_fs().insert_file(root.join("b.rs"), b"alpha again\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &stoat_action::OpenGlobalSearch);
        h.type_text("alpha");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        let picker = h.stoat.global_search.as_ref().expect("picker open");
        assert_eq!(picker.matches().len(), 2);
    }

    #[test]
    fn global_search_picker_esc_closes_without_jumping() {
        let mut h = Stoat::test();
        let root = std::path::PathBuf::from("/repo");
        h.fake_fs().insert_file(root.join("a.rs"), b"alpha\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &stoat_action::OpenGlobalSearch);
        h.type_text("alpha");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        assert!(h.stoat.global_search.is_some());
        assert_eq!(
            h.stoat.update(Event::Key(keys::key(KeyCode::Esc))),
            UpdateEffect::Redraw
        );
        assert!(h.stoat.global_search.is_none());
    }

    #[test]
    fn global_search_picker_enter_opens_and_jumps() {
        let mut h = Stoat::test();
        let root = std::path::PathBuf::from("/repo");
        h.fake_fs().insert_file(root.join("a.rs"), b"alpha\nbeta\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &stoat_action::OpenGlobalSearch);
        h.type_text("beta");
        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        assert!(h.stoat.global_search.is_some());

        h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
        h.settle();
        assert!(h.stoat.global_search.is_none());
        assert_eq!(editor::head_offsets(&mut h.stoat), vec![6]);
    }

    #[test]
    fn global_search_input_esc_cancels() {
        let mut h = Stoat::test();
        dispatch(&mut h.stoat, &stoat_action::OpenGlobalSearch);
        assert!(h.stoat.global_search_input.is_some());
        h.stoat.update(Event::Key(keys::key(KeyCode::Esc)));
        assert!(h.stoat.global_search_input.is_none());
        assert!(h.stoat.global_search.is_none());
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
        assert_eq!(guard.snapshot.visible_text.to_string(), "\n");

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
        assert_eq!(editor::head_offsets(&mut stoat), vec![2]);
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(editor::head_offsets(&mut stoat), vec![2]);
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
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 4, false)]);
        assert_eq!(editor::head_offsets(&mut stoat), vec![3]);
    }

    #[test]
    fn extend_right_crosses_the_newline() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "ab\ncd");
        stoat.set_focused_mode("select".into());
        // From 'a', three extend-rights walk over the line's last char, across
        // the newline, and onto 'c' on the next line rather than clamping.
        dispatch(&mut stoat, &ExtendRight);
        dispatch(&mut stoat, &ExtendRight);
        dispatch(&mut stoat, &ExtendRight);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 4, false)]);
    }

    #[test]
    fn move_next_word_start_repeated_snaps_tail() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar baz");
        dispatch(&mut stoat, &MoveNextWordStart);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 4, false)]);
        dispatch(&mut stoat, &MoveNextWordStart);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(4, 8, false)]);
    }

    #[test]
    fn move_next_word_start_from_whitespace_advances_anchor() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, " foo bar");
        dispatch(&mut stoat, &MoveNextWordStart);
        // The anchor advances past the leading space onto the word start, so the
        // selection excludes the space and `dw` here would not eat it.
        assert_eq!(editor::selection_spans(&mut stoat), vec![(1, 5, false)]);
    }

    #[test]
    fn move_next_word_start_from_blank_line_runs_through_word() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "\nfoo");
        dispatch(&mut stoat, &MoveNextWordStart);
        // Starting on the blank line, the anchor skips the newline and the head
        // runs through the following word to its end.
        assert_eq!(editor::selection_spans(&mut stoat), vec![(1, 4, false)]);
    }

    #[test]
    fn move_next_word_end_creates_selection() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar");
        dispatch(&mut stoat, &MoveNextWordEnd);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, false)]);
    }

    #[test]
    fn move_next_word_end_at_eof_is_noop() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo");
        for _ in 0..3 {
            dispatch(&mut stoat, &MoveRight);
        }
        assert_eq!(editor::head_offsets(&mut stoat), vec![2]);
        dispatch(&mut stoat, &MoveNextWordEnd);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 3, false)]);
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
    fn move_prev_word_start_from_word_boundary_retreats_anchor() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar");
        for _ in 0..4 {
            dispatch(&mut stoat, &MoveRight);
        }
        assert_eq!(editor::head_offsets(&mut stoat), vec![4]);
        dispatch(&mut stoat, &MovePrevWordStart);
        // On the word start 'b', the tail retreats past it, so the selection
        // ends at the word start rather than one cell past it.
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 4, true)]);
    }

    #[test]
    fn move_prev_word_start_over_forward_selection_keeps_trailing_char() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar");
        // `e` makes a forward selection whose block cursor sits on the last
        // word char, one cell back from the head. `b` must scan from there
        // rather than the head, or it swallows the char after the cursor.
        dispatch(&mut stoat, &MoveNextWordEnd);
        dispatch(&mut stoat, &MovePrevWordStart);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, true)]);
        assert_eq!(editor::head_offsets(&mut stoat), vec![0]);
    }

    #[test]
    fn move_prev_word_end_over_forward_selection_keeps_trailing_char() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar baz");
        for _ in 0..4 {
            dispatch(&mut stoat, &MoveRight);
        }
        dispatch(&mut stoat, &MoveNextWordEnd);
        dispatch(&mut stoat, &MovePrevWordEnd);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 7, true)]);
        assert_eq!(editor::head_offsets(&mut stoat), vec![2]);
    }

    #[test]
    fn move_prev_word_start_at_start_is_noop() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar");
        dispatch(&mut stoat, &MovePrevWordStart);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 1, false)]);
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
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 1, false)]);
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
            vec![(0, 4, false), (8, 12, false)]
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
    fn add_selection_below_copies_each_selection_skipping_short_lines() {
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
        // The column-0 cursor copies onto row 1. The column-7 cursor cannot fit
        // on the short row 1, so it skips to row 2 rather than clamping.
        let positions = editor::cursor_display_positions(&mut stoat);
        assert_eq!(positions, vec![(0, 0), (0, 7), (1, 0), (2, 7)]);
    }

    #[test]
    fn extend_right_grows_selection_from_cursor() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abc");
        dispatch(&mut stoat, &ExtendRight);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 2, false)]);
    }

    #[test]
    fn extend_right_further_keeps_tail() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abcdef");
        dispatch(&mut stoat, &ExtendRight);
        dispatch(&mut stoat, &ExtendRight);
        dispatch(&mut stoat, &ExtendRight);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 4, false)]);
    }

    #[test]
    fn extend_right_at_end_is_noop() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "ab");
        dispatch(&mut stoat, &MoveRight);
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(1, 2, false)]);
        dispatch(&mut stoat, &ExtendRight);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(1, 2, false)]);
    }

    #[test]
    fn extend_left_across_tail_flips_reversed() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abcdef");
        dispatch(&mut stoat, &MoveRight);
        dispatch(&mut stoat, &MoveRight);
        dispatch(&mut stoat, &ExtendRight);
        dispatch(&mut stoat, &ExtendRight);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 5, false)]);
        dispatch(&mut stoat, &ExtendLeft);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 4, false)]);
        dispatch(&mut stoat, &ExtendLeft);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 3, false)]);
        // Crossing the 1-wide tail flips the range and steps the tail forward
        // so the anchor's cell stays covered (Helix shrink-then-flip).
        dispatch(&mut stoat, &ExtendLeft);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(1, 3, true)]);
        dispatch(&mut stoat, &ExtendLeft);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, true)]);
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
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(1, 1)]);
        dispatch(&mut stoat, &ExtendDown);
        assert_eq!(editor::cursor_display_positions(&mut stoat), vec![(2, 7)]);
    }

    #[test]
    fn extend_down_at_last_row_is_noop() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abc");
        dispatch(&mut stoat, &ExtendDown);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 1, false)]);
    }

    #[test]
    fn extend_up_from_second_line_grows_backward() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abc\ndef\n");
        dispatch(&mut stoat, &MoveDown);
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(5, 6, false)]);
        dispatch(&mut stoat, &ExtendUp);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(1, 5, true)]);
    }

    #[test]
    fn extend_next_word_start_grows_selection_from_cursor() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar");
        dispatch(&mut stoat, &ExtendNextWordStart);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 4, false)]);
    }

    #[test]
    fn extend_next_word_start_repeated_keeps_tail() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar baz");
        dispatch(&mut stoat, &ExtendNextWordStart);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 4, false)]);
        dispatch(&mut stoat, &ExtendNextWordStart);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 8, false)]);
    }

    #[test]
    fn extend_next_word_end_grows_selection_from_cursor() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar");
        dispatch(&mut stoat, &ExtendNextWordEnd);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, false)]);
    }

    #[test]
    fn extend_prev_word_start_keeps_tail_at_cursor() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "foo bar");
        for _ in 0..6 {
            dispatch(&mut stoat, &MoveRight);
        }
        assert_eq!(editor::selection_spans(&mut stoat), vec![(6, 7, false)]);
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
        assert_eq!(editor::selection_spans(&mut stoat), vec![(6, 7, false)]);
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
            vec![(0, 2, false), (4, 6, false)]
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
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 9, false)]);
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
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, false)]);
        dispatch(&mut stoat, &CollapseSelection);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(2, 3, false)]);
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
        assert_eq!(editor::selection_spans(&mut stoat), vec![(4, 5, false)]);
    }

    #[test]
    fn collapse_selection_multi_cursor_collapses_each() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abc\ndef\nghi\n");
        dispatch(&mut stoat, &AddSelectionBelow);
        dispatch(&mut stoat, &ExtendRight);
        assert_eq!(
            editor::selection_spans(&mut stoat),
            vec![(0, 2, false), (4, 6, false)]
        );
        dispatch(&mut stoat, &CollapseSelection);
        assert_eq!(
            editor::selection_spans(&mut stoat),
            vec![(1, 2, false), (5, 6, false)]
        );
    }

    #[test]
    fn flip_selections_toggles_reversed() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abcdef");
        dispatch(&mut stoat, &ExtendRight);
        dispatch(&mut stoat, &ExtendRight);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, false)]);
        dispatch(&mut stoat, &FlipSelections);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, true)]);
        dispatch(&mut stoat, &FlipSelections);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 3, false)]);
    }

    #[test]
    fn flip_selections_on_bare_cursor_toggles_reversed() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "abc");
        dispatch(&mut stoat, &MoveRight);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(1, 2, false)]);
        dispatch(&mut stoat, &FlipSelections);
        assert_eq!(editor::selection_spans(&mut stoat), vec![(1, 2, true)]);
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
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 1, false)]);
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
    fn toggle_syntax_highlight_drops_coloring_then_restores() {
        let mut h = Stoat::test();
        let path = h.write_file("a.rs", "fn main() {\n    let x = 1;\n}\n");
        h.open_file(&path);

        let colored = h.stoat.render();

        dispatch(&mut h.stoat, &stoat_action::ToggleSyntaxHighlight);
        let plain = h.stoat.render();
        assert_ne!(
            colored, plain,
            "toggling syntax highlighting off recolors the render",
        );
        // The code is unchanged. The snapshot shows it in the default
        // foreground with search/diagnostic highlights still available.
        h.assert_snapshot("toggle_syntax_off");

        dispatch(&mut h.stoat, &stoat_action::ToggleSyntaxHighlight);
        let restored = h.stoat.render();
        assert_eq!(
            colored, restored,
            "toggling syntax highlighting back on restores the coloring",
        );
    }

    fn version_badge_label(stoat: &Stoat) -> Option<String> {
        let id = stoat
            .badges
            .find_by_source(crate::badge::BadgeSource::Version)?;
        stoat.badges.get(id).map(|b| b.label.clone())
    }

    #[test]
    fn show_version_badge_appends_stoatty_when_inside_it() {
        let mut h = Stoat::test();
        h.stoat.set_version_info("0.1.0 (aaa 2026-07-03)");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        h.stoat.set_stoatty_apc(true, tx);
        h.fake_env()
            .set("STOATTY_VERSION", "0.2.0 (bbb 2026-07-03)");

        dispatch(&mut h.stoat, &stoat_action::ShowVersion);

        assert_eq!(
            version_badge_label(&h.stoat).as_deref(),
            Some("stoat 0.1.0 (aaa 2026-07-03) | stoatty 0.2.0 (bbb 2026-07-03)"),
        );
    }

    #[test]
    fn show_version_badge_is_stoat_only_outside_stoatty() {
        let mut h = Stoat::test();
        h.stoat.set_version_info("0.1.0 (aaa 2026-07-03)");

        dispatch(&mut h.stoat, &stoat_action::ShowVersion);

        assert_eq!(
            version_badge_label(&h.stoat).as_deref(),
            Some("stoat 0.1.0 (aaa 2026-07-03)"),
        );
    }

    #[test]
    fn show_version_badge_is_dismissed_on_the_next_key() {
        let mut h = Stoat::test();
        dispatch(&mut h.stoat, &stoat_action::ShowVersion);
        assert!(
            version_badge_label(&h.stoat).is_some(),
            "the badge is shown after ShowVersion",
        );

        h.type_keys("j");
        assert!(
            version_badge_label(&h.stoat).is_none(),
            "the next key press retires the version badge",
        );
    }

    #[test]
    fn dispatch_show_cwd_reports_git_root() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = "/some/root".into();

        dispatch(&mut h.stoat, &stoat_action::ShowCwd);

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("Current working directory is /some/root"),
        );
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

        h.advance_clock(Duration::from_nanos(1));
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
    fn half_page_down_extends_the_selection_in_select_mode() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "a\nb\nc\nd\ne\nf\n");
        set_focused_viewport_rows(&mut stoat, Some(4));
        stoat.set_focused_mode("select".into());
        dispatch(&mut stoat, &HalfPageDown);
        // Half of viewport 4 is two rows, so the anchor holds at the top and
        // the head extends down to row 2 rather than collapsing there.
        assert_eq!(editor::selection_spans(&mut stoat), vec![(0, 4, false)]);
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

    #[test]
    fn count_prefix_page_down_moves_n_pages() {
        let mut stoat = stoat();
        let text: String = (0..100).map(|i| format!("line{i:02}\n")).collect();
        editor::seed_focused_buffer(&mut stoat, &text);
        set_focused_viewport_rows(&mut stoat, Some(10));
        stoat.pending_count = Some(3);
        dispatch(&mut stoat, &PageDown);
        assert_eq!(
            editor::cursor_display_positions(&mut stoat),
            vec![(30, 0)],
            "3 Ctrl-f with viewport=10 should land at row 30"
        );
    }

    #[test]
    fn page_down_arms_a_scroll_glide() {
        let mut stoat = stoat();
        let text: String = (0..100).map(|i| format!("line{i:02}\n")).collect();
        editor::seed_focused_buffer(&mut stoat, &text);
        set_focused_viewport_rows(&mut stoat, Some(10));

        dispatch(&mut stoat, &PageDown);

        let editor = focused_editor_mut(&mut stoat).expect("focused editor");
        assert_eq!(
            editor.scroll_row, 10,
            "PageDown jumps scroll_row a full viewport"
        );
        assert_eq!(
            editor.scroll_offset, 0.0,
            "scroll_offset lags at the pre-jump row so the pool eases up to it"
        );
        assert!(editor.scroll_glide, "a page glide is armed");
    }

    #[test]
    fn count_prefix_half_page_down_moves_n_half_pages() {
        let mut stoat = stoat();
        let text: String = (0..100).map(|i| format!("line{i:02}\n")).collect();
        editor::seed_focused_buffer(&mut stoat, &text);
        set_focused_viewport_rows(&mut stoat, Some(10));
        stoat.pending_count = Some(3);
        dispatch(&mut stoat, &HalfPageDown);
        assert_eq!(
            editor::cursor_display_positions(&mut stoat),
            vec![(15, 0)],
            "3 Ctrl-d with viewport=10 (half-page=5) should land at row 15"
        );
    }

    #[test]
    fn count_prefix_page_up_moves_n_pages() {
        let mut stoat = stoat();
        let text: String = (0..100).map(|i| format!("line{i:02}\n")).collect();
        editor::seed_focused_buffer(&mut stoat, &text);
        set_focused_viewport_rows(&mut stoat, Some(10));
        dispatch(&mut stoat, &PageDown);
        dispatch(&mut stoat, &PageDown);
        dispatch(&mut stoat, &PageDown);
        dispatch(&mut stoat, &PageDown);
        assert_eq!(
            editor::cursor_display_positions(&mut stoat),
            vec![(40, 0)],
            "test setup: cursor at row 40 after four page-downs"
        );
        stoat.pending_count = Some(3);
        dispatch(&mut stoat, &PageUp);
        assert_eq!(
            editor::cursor_display_positions(&mut stoat),
            vec![(10, 0)],
            "3 Ctrl-b from row 40 with viewport=10 should land at row 10"
        );
    }

    #[test]
    fn count_prefix_page_down_clamps_at_buffer_end() {
        let mut stoat = stoat();
        let text: String = (0..30).map(|i| format!("line{i:02}\n")).collect();
        editor::seed_focused_buffer(&mut stoat, &text);
        set_focused_viewport_rows(&mut stoat, Some(10));
        stoat.pending_count = Some(99);
        dispatch(&mut stoat, &PageDown);
        assert_eq!(
            editor::cursor_display_positions(&mut stoat),
            vec![(29, 6)],
            "huge count clamps the 1-wide cursor onto the last real cell, not the empty final row"
        );
    }

    #[test]
    fn last_picker_action_records_command_palette() {
        let mut stoat = stoat();
        dispatch(&mut stoat, &stoat_action::OpenCommandPalette);
        assert_eq!(stoat.last_picker_action, Some("OpenCommandPalette"));
    }

    #[test]
    fn last_picker_recall_no_op_with_no_history() {
        let mut stoat = stoat();
        assert_eq!(
            dispatch(&mut stoat, &stoat_action::OpenLastPicker),
            UpdateEffect::None
        );
        assert!(stoat.command_palette.is_none());
        assert!(stoat.jumplist_picker.is_none());
        assert!(stoat.diagnostics_picker.is_none());
    }

    #[test]
    fn last_picker_recall_reopens_jumplist_after_close() {
        let mut stoat = stoat();
        editor::seed_focused_buffer(&mut stoat, "alpha\nbeta\ngamma\n");
        dispatch(&mut stoat, &SaveSelection);
        dispatch(&mut stoat, &MoveDown);
        dispatch(&mut stoat, &SaveSelection);
        dispatch(&mut stoat, &stoat_action::OpenJumplistPicker);
        assert_eq!(
            stoat.last_picker_action,
            Some("OpenJumplistPicker"),
            "open should record the recall target"
        );
        stoat.jumplist_picker = None;
        assert_eq!(
            dispatch(&mut stoat, &stoat_action::OpenLastPicker),
            UpdateEffect::Redraw
        );
        assert!(stoat.jumplist_picker.is_some(), "recall should reopen");
    }
}

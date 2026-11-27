mod agent;
pub mod amend;
pub(crate) mod blame;
pub mod code_search;
mod comment_reflow;
pub mod commits;
pub(crate) mod completion;
pub(crate) mod conflict;
pub mod conflict_view;
pub mod diff_changes;
pub mod diff_nav;
pub(crate) mod file;
mod file_finder;
pub(crate) mod filter_selections;
pub(crate) mod git_commit;
mod git_merge;
mod git_stash;
mod git_status;
pub(crate) mod help;
pub(crate) mod jobs;
pub mod jump;
pub mod lsp;
pub(crate) mod macro_recording;
pub(crate) mod marks;
pub(crate) mod move_nav;
pub mod movement;
mod palette;
pub mod pane;
pub(crate) mod picker;
pub(crate) mod prompt;
pub(crate) mod rebase;
pub mod review;
pub mod review_walk;
mod reword;
pub(crate) mod run;
pub(crate) mod search;
pub(crate) mod shell;
pub(crate) mod split_selection;
pub(crate) mod surround;
pub(crate) mod tab;
mod terminal;
pub mod textobject;
pub(crate) mod textobject_nav;
pub mod view;
pub mod walkthrough;
pub mod workspace;
pub mod yank;

use crate::{
    agent_ipc::BridgeOutcome,
    apc_emit,
    app::{MessageKind, Stoat, UpdateEffect},
    auto_reload,
    buffer_registry::OpenOrigin,
    checkout_confirm,
    command_palette::CommandPalette,
    commit_preview,
    diff_changes::ExpandDir,
    display_map::syntax_theme::SyntaxStyles,
    editor_state::{EditorId, EditorState},
    host::FsHost,
    jumplist::JumpList,
    keymap_state::{self, ActiveModal, StoatKeymapState},
    merge_view::Side,
    pane::{Axis, Direction, DockSide, FocusTarget, View},
    workspace::diff::WorktreeBase,
};
pub(crate) use commits::{pump_commits, respawn_commits_panes};
pub(crate) use file_finder::close_file_finder;
pub use file_finder::{file_finder_move_selection, sync_file_finder_preview};
pub(crate) use palette::{
    close_palette, open_run_block, run_block_width, sync_palette_picker, sync_pick_preview,
};
pub use pane::close_pane_by_id;
pub(crate) use pane::{
    dispose_view, restore_covered_terminal, restore_pane_after_term_exit, retire_parked_pane,
    scratch_editor_view, EditorDisposal,
};
use std::{io::Read, path::Path, sync::Arc};
use stoat_action::{
    Action, ActionKind, AutoReload, AutoReloadConfig, DiffFiles, Dump, FocusPane, GitCheckout,
    GitLs, GitMerge, GitRebase, GitReview, GitStashPush, GotoTab, Mosh, Notify, OpenBuffer,
    OpenConfig, OpenFile, OpenLogs, OpenReviewAgentEdits, RenameTab, RenameWorkspace, Review, Run,
    SetCwd, SetTheme, Ssh, WalkthroughOpen,
};
use stoat_text::{Anchor, BufferId, LineEnding, Rope, Selection};
pub use terminal::respawn_terminal_panes;

/// A motion `RepeatLastMotion` (Alt-.) plays back.
///
/// Each variant carries what its handler reads from the keypress, so a replay
/// repeats the motion the user made. That is the count for the motions that
/// take one, since the count does not outlive the keypress.
///
/// A find and a parent-node step also keep the extend flag they ran with, as
/// their Helix counterparts do. The other motions that extend in select mode
/// read the mode when they replay, so a replay in select mode extends where the
/// first press moved.
///
/// Only motions worth stepping through repeatedly are recorded. A motion that
/// records nothing leaves the previous one in place, which is what makes Alt-.
/// after an unrecorded key repeat the last motion rather than do nothing.
#[derive(Debug, Clone, Copy)]
pub(crate) enum LastMotion {
    Find {
        kind: movement::FindKind,
        ch: char,
        count: u32,
        extend: bool,
    },
    /// The same find aimed at a line ending rather than a character. Nothing
    /// names the target in advance, since one line ends in LF where the next
    /// ends in CRLF, so there is no char to carry.
    FindLineEnding {
        kind: movement::FindKind,
        count: u32,
        extend: bool,
    },
    Paragraph {
        dir: movement::ParaDir,
        count: u32,
    },
    Change {
        dir: movement::ChangeDir,
        count: u32,
        unit: movement::ChangeUnit,
    },
    Diagnostic {
        dir: lsp::DiagnosticDirection,
    },
    TsSibling {
        dir: movement::SiblingDir,
    },
    ParentBound {
        bound: movement::NodeBound,
        extend: bool,
    },
    TsObject {
        kind: textobject_nav::NavKind,
        dir: textobject_nav::NavDirection,
        count: u32,
    },
    ExpandSelection,
    ShrinkSelection,
    SelectAllSiblings,
    SelectAllChildren,
    /// The `mi`/`ma` chord, whose mode and type char come from two keypresses.
    /// All three ride along so a replay repeats the chord rather than stopping
    /// to read the char again.
    TextObject {
        mode: textobject::TextobjectMode,
        ch: char,
        count: usize,
    },
}

/// The count typed in front of the key that dispatches now, or one.
///
/// A chord reads its count here rather than where it runs, because dispatching
/// the action that arms the chord already cleared the pending count. The chars
/// completing the chord arrive over later keypresses with nothing left to read.
pub(crate) fn arming_count(stoat: &mut Stoat) -> usize {
    stoat.take_pending_count().unwrap_or(1).max(1) as usize
}

/// [`arming_count`] as a signed step, for the handlers that walk by a delta.
///
/// A typed count saturates far past `i32`, and a cast wraps it into a step the
/// other way, so a count out of range steps as far as `i32` reaches.
fn arming_step(stoat: &mut Stoat) -> i32 {
    i32::try_from(arming_count(stoat)).unwrap_or(i32::MAX)
}

/// Run the last recorded motion again, this key's count deciding how many
/// times.
///
/// The count repeats the whole motion rather than widening its reach, so a
/// motion made with its own count carries that count into each repeat.
pub(crate) fn repeat_last_motion(stoat: &mut Stoat) -> UpdateEffect {
    let Some(motion) = stoat.last_motion else {
        return UpdateEffect::None;
    };
    let repeat = stoat.take_pending_count().unwrap_or(1).max(1);

    let mut effect = UpdateEffect::None;
    for _ in 0..repeat {
        effect = match motion {
            LastMotion::Find {
                kind,
                ch,
                count,
                extend,
            } => movement::execute_find(stoat, kind, ch, extend, count),
            LastMotion::FindLineEnding {
                kind,
                count,
                extend,
            } => movement::execute_find_line_ending(stoat, kind, extend, count),
            LastMotion::Paragraph { dir, count } => {
                movement::goto_paragraph_impl(stoat, dir, stoat.in_select_mode(), count)
            },
            LastMotion::Change { dir, count, unit } => {
                movement::goto_change_impl(stoat, dir, count, unit)
            },
            LastMotion::Diagnostic { dir } => lsp::goto_diagnostic(stoat, dir),
            LastMotion::TsSibling { dir } => movement::select_sibling(stoat, dir),
            LastMotion::ParentBound { bound, extend } => {
                movement::move_to_parent_bound(stoat, bound, extend)
            },
            LastMotion::TsObject { kind, dir, count } => {
                textobject_nav::goto_textobject_impl(stoat, kind, dir, count)
            },
            LastMotion::ExpandSelection => movement::expand_selection(stoat),
            LastMotion::ShrinkSelection => movement::shrink_selection(stoat),
            LastMotion::SelectAllSiblings => movement::select_all_siblings(stoat),
            LastMotion::SelectAllChildren => movement::select_all_children(stoat),
            LastMotion::TextObject { mode, ch, count } => {
                textobject::execute_select_textobject(stoat, mode, ch, count)
            },
        };
    }
    effect
}

pub fn dispatch(stoat: &mut Stoat, action: &dyn Action) -> UpdateEffect {
    // The command that runs is the command that spends the register selection,
    // whether or not it had any use for one. Otherwise a selection the reader
    // made and then moved on from waits to surprise a later yank.
    stoat.take_selected_register();
    conflict_view::disarm_clobber_unless_pick(stoat, action.kind());
    conflict_view::set_chip_menu(stoat, None);
    let modal_before = keymap_state::active_modal(stoat);

    let group_buffer = if manages_own_undo_group(action.kind()) {
        None
    } else {
        begin_action_group(stoat)
    };
    if pane::is_layout_action(action.kind()) && pane::close_full_window_screen(stoat) {
        end_action_group(stoat, group_buffer);
        return UpdateEffect::Redraw;
    }
    let effect = match action.kind() {
        ActionKind::Quit => {
            let focused = stoat.active_workspace().panes.focus();
            pane::quit_or_exit(stoat, focused, false)
        },
        ActionKind::ForceQuit => {
            let focused = stoat.active_workspace().panes.focus();
            pane::quit_or_exit(stoat, focused, true)
        },
        ActionKind::QuitAll => quit_all(stoat),
        ActionKind::ForceQuitAll => UpdateEffect::Quit,
        ActionKind::QuitAllConfirm => quit_all_confirm(stoat),
        ActionKind::QuitAllCancel => quit_all_cancel(stoat),
        ActionKind::ShowVersion => show_version(stoat),
        ActionKind::OpenLogs => {
            let open = action
                .as_any()
                .downcast_ref::<OpenLogs>()
                .expect("OpenLogs action downcast");
            auto_reload::open_logs(stoat, open.target.as_deref())
        },
        ActionKind::Notify => {
            let notify = action
                .as_any()
                .downcast_ref::<Notify>()
                .expect("Notify action downcast");
            stoat.notify(MessageKind::Info, notify.text.clone());
            UpdateEffect::Redraw
        },
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
        ActionKind::MovePaneLeft => {
            pane::move_pane_direction(stoat, Direction::Left);
            UpdateEffect::Redraw
        },
        ActionKind::MovePaneDown => {
            pane::move_pane_direction(stoat, Direction::Down);
            UpdateEffect::Redraw
        },
        ActionKind::MovePaneUp => {
            pane::move_pane_direction(stoat, Direction::Up);
            UpdateEffect::Redraw
        },
        ActionKind::MovePaneRight => {
            pane::move_pane_direction(stoat, Direction::Right);
            UpdateEffect::Redraw
        },
        ActionKind::MovePaneNext => {
            pane::move_pane_rotate(stoat, true);
            UpdateEffect::Redraw
        },
        ActionKind::MovePanePrev => {
            pane::move_pane_rotate(stoat, false);
            UpdateEffect::Redraw
        },
        ActionKind::FocusPane => {
            let focus = action
                .as_any()
                .downcast_ref::<FocusPane>()
                .expect("FocusPane action downcast");
            pane::focus_pane_by_index(stoat, focus.index);
            UpdateEffect::Redraw
        },
        ActionKind::GotoTab => {
            let goto = action
                .as_any()
                .downcast_ref::<GotoTab>()
                .expect("GotoTab action downcast");
            tab::goto_tab(stoat, goto.index)
        },
        ActionKind::NewTab => tab::new_tab(stoat),
        ActionKind::NextTab => tab::next_tab(stoat),
        ActionKind::PrevTab => tab::prev_tab(stoat),
        ActionKind::MoveTabLeft => tab::move_tab_left(stoat),
        ActionKind::MoveTabRight => tab::move_tab_right(stoat),
        ActionKind::CloseTab => {
            let active = stoat.active_workspace().active_tab;
            tab::close_tab_at(stoat, active)
        },
        ActionKind::ToggleTab => tab::toggle_tab(stoat),
        ActionKind::ToggleTabBar => tab::toggle_tab_bar(stoat),
        ActionKind::TogglePaneFade => tab::toggle_pane_fade(stoat),
        ActionKind::ToggleRelativeNumbers => tab::toggle_relative_numbers(stoat),
        ActionKind::RenameTab => {
            let name = action
                .as_any()
                .downcast_ref::<RenameTab>()
                .expect("RenameTab action downcast");
            tab::rename_tab(stoat, name.name.as_deref())
        },
        ActionKind::ClosePane => {
            pane::close_focused_pane(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::CloseOtherPanes => {
            pane::close_other_panes(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::TogglePaneWiden => {
            pane::toggle_pane_widen(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::DetachPane => {
            pane::detach_focused_pane(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::ReattachPane => {
            pane::reattach_focused_pane(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::OpenFile => {
            let open = action
                .as_any()
                .downcast_ref::<OpenFile>()
                .expect("OpenFile action downcast");
            file::open_file(stoat, &open.path, OpenOrigin::Named);
            UpdateEffect::Redraw
        },
        ActionKind::OpenConfig => {
            let open = action
                .as_any()
                .downcast_ref::<OpenConfig>()
                .expect("OpenConfig action downcast");
            file::open_config(stoat, open.target.as_deref());
            UpdateEffect::Redraw
        },
        ActionKind::ToggleMinimap => {
            stoat.toggle_minimap();
            UpdateEffect::Redraw
        },
        ActionKind::ToggleWrap => {
            stoat.toggle_wrap();
            UpdateEffect::Redraw
        },
        ActionKind::ToggleKeyHints => {
            // A screen that keeps its box up shows it with the toggle off, so
            // there the toggle hides that box rather than forcing one on.
            let ws = stoat.active_workspace();
            let screen = keymap_state::view_predicate(ws);
            match crate::render::keeps_hints_up(screen, ws.walkthrough.is_some()) {
                true => stoat.screen_hints_hidden = !stoat.screen_hints_hidden,
                false => stoat.key_hints_visible = !stoat.key_hints_visible,
            }
            UpdateEffect::Redraw
        },
        ActionKind::PinMode => match stoat.set_focused_pinned(true) {
            true => UpdateEffect::Redraw,
            false => UpdateEffect::None,
        },
        ActionKind::DismissKeyHints => {
            if stoat.key_hints_visible {
                stoat.key_hints_visible = false;
                UpdateEffect::Redraw
            } else {
                UpdateEffect::None
            }
        },
        ActionKind::OpenBuffer => {
            let open = action
                .as_any()
                .downcast_ref::<OpenBuffer>()
                .expect("OpenBuffer action downcast");
            file::open_file(stoat, &open.path, OpenOrigin::Named);
            UpdateEffect::Redraw
        },
        ActionKind::NewBuffer => {
            crate::buffer_lifecycle::show_new_scratch(stoat);
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
        ActionKind::OpenWorkspaceFileFinder => file_finder::open_file_finder(
            stoat,
            crate::file_finder::OpenIntent::Replace,
            Some(crate::file_finder::FinderScope::AllWorkspaces),
        ),
        ActionKind::FileFinderScopeToggle => file_finder::file_finder_scope_toggle(stoat),
        ActionKind::OpenCommandPalette if stoat.command_palette.is_some() => UpdateEffect::None,
        ActionKind::OpenCommandPalette => {
            // `Ctrl-:` and `Ctrl-;` bind at the keymap's root, past every
            // `!modal` guard, so the modal they fire over closes and the
            // palette owns the keys alone.
            keymap_state::close_topmost_modal(stoat);
            let executor = stoat.executor.clone();
            let availability = crate::command_palette::Availability::from_stoat(stoat);
            let keys = stoat
                .keymap
                .keys_by_action(&StoatKeymapState::from_stoat(stoat));
            let commands = stoat.commands.clone();
            let ws = stoat.active_workspace_mut();
            stoat.command_palette = Some(CommandPalette::new(
                ws,
                executor,
                availability,
                keys,
                commands,
            ));
            UpdateEffect::Redraw
        },
        ActionKind::OpenHelp => {
            help::open_help(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::DiffFiles => {
            let action = action
                .as_any()
                .downcast_ref::<DiffFiles>()
                .expect("DiffFiles action downcast");
            review::diff(stoat, action.rev.as_deref())
        },
        ActionKind::Conflict => {
            conflict_view::open_conflict(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::CloseConflict => {
            conflict_view::close_conflict(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::ConflictPickOurs => {
            conflict_view::conflict_pick_ours(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::ConflictPickTheirs => {
            conflict_view::conflict_pick_theirs(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::ConflictPickBoth => {
            conflict_view::conflict_pick_both(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::ConflictPickOursLine => {
            conflict_view::conflict_pick_ours_line(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::ConflictPickTheirsLine => {
            conflict_view::conflict_pick_theirs_line(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::ConflictResetChunk => {
            conflict_view::conflict_reset_chunk(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::ConflictNextChunk => {
            let count = arming_count(stoat);
            conflict_view::conflict_step_chunk(stoat, true, count);
            UpdateEffect::Redraw
        },
        ActionKind::ConflictPrevChunk => {
            let count = arming_count(stoat);
            conflict_view::conflict_step_chunk(stoat, false, count);
            UpdateEffect::Redraw
        },
        ActionKind::ConflictNextFile => {
            let count = arming_count(stoat);
            conflict_view::conflict_step_file(stoat, true, count);
            UpdateEffect::Redraw
        },
        ActionKind::ConflictPrevFile => {
            let count = arming_count(stoat);
            conflict_view::conflict_step_file(stoat, false, count);
            UpdateEffect::Redraw
        },
        ActionKind::ConflictApply => {
            conflict_view::conflict_apply(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::ToggleStageHunk => review::stage_hunk(stoat),
        ActionKind::ToggleStageLine => review::stage_line(stoat),
        ActionKind::AddSelectionBelow => movement::add_selection_below(stoat),
        ActionKind::AddSelectionAbove => movement::add_selection_above(stoat),
        ActionKind::SplitSelectionOnNewline => {
            with_focused_editor(stoat, movement::split_selection_on_newline)
        },
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
        ActionKind::ReloadBuffer => auto_reload::reload_focused(stoat, false),
        ActionKind::ForceReloadBuffer => auto_reload::reload_focused(stoat, true),
        ActionKind::ReloadAll => auto_reload::reload_all(stoat, false),
        ActionKind::ForceReloadAll => auto_reload::reload_all(stoat, true),
        ActionKind::FollowChanges => auto_reload::toggle_follow_changes(stoat),
        ActionKind::LiveReload => auto_reload::toggle_live_reload(stoat),
        ActionKind::FollowReload => auto_reload::toggle_follow_reload(stoat),
        ActionKind::WriteQuit => file::write_quit(stoat, false),
        ActionKind::ForceWriteQuit => file::write_quit(stoat, true),
        ActionKind::CloseBuffer => crate::buffer_lifecycle::close_buffer(stoat, false),
        ActionKind::ForceCloseBuffer => crate::buffer_lifecycle::close_buffer(stoat, true),
        ActionKind::CloseAllBuffers => crate::buffer_lifecycle::close_all_buffers(stoat, false),
        ActionKind::ForceCloseAllBuffers => crate::buffer_lifecycle::close_all_buffers(stoat, true),
        ActionKind::CloseOtherBuffers => crate::buffer_lifecycle::close_other_buffers(stoat, false),
        ActionKind::ForceCloseOtherBuffers => {
            crate::buffer_lifecycle::close_other_buffers(stoat, true)
        },
        ActionKind::GotoLastAccessed => crate::buffer_lifecycle::goto_last_accessed(stoat),
        ActionKind::AutoReload => {
            let auto = action
                .as_any()
                .downcast_ref::<AutoReload>()
                .expect("AutoReload action downcast");
            auto_reload::set_buffer_auto_reload(stoat, &auto.state)
        },
        ActionKind::AutoReloadConfig => {
            let auto = action
                .as_any()
                .downcast_ref::<AutoReloadConfig>()
                .expect("AutoReloadConfig action downcast");
            auto_reload::set_auto_reload_config(stoat, &auto.state)
        },
        ActionKind::FontSizeInc => file::font_size_step(stoat, 1),
        ActionKind::FontSizeDec => file::font_size_step(stoat, -1),
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
        ActionKind::ChangeSelectionNoYank => movement::change_selection_no_yank(stoat),
        ActionKind::Undo => movement::undo(stoat),
        ActionKind::Redo => movement::redo(stoat),
        ActionKind::Earlier => movement::earlier(stoat),
        ActionKind::Later => movement::later(stoat),
        ActionKind::CommitUndoCheckpoint => movement::commit_undo_checkpoint(stoat),
        ActionKind::IndentSelection => movement::indent_selection(stoat),
        ActionKind::UnindentSelection => movement::unindent_selection(stoat),
        ActionKind::ToggleComments => movement::toggle_comments(stoat),
        ActionKind::ToggleLineComments => movement::toggle_line_comments(stoat),
        ActionKind::ToggleBlockComments => movement::toggle_block_comments(stoat),
        ActionKind::ToggleSyntaxHighlight => {
            stoat.syntax_highlight = !stoat.syntax_highlight;
            UpdateEffect::Redraw
        },
        ActionKind::ToggleLspStatus => {
            stoat.lsp_status_pinned = !stoat.lsp_status_pinned;
            UpdateEffect::Redraw
        },
        ActionKind::ToggleInlayHints => {
            stoat.inlay_hints_enabled = !stoat.inlay_hints_enabled;
            stoat.last_inlay_hint_key = None;
            stoat.pending_inlay_hint_request.clear();
            if stoat.inlay_hints_enabled {
                lsp::enable_inlay_hints_now(stoat);
            } else {
                lsp::clear_inlay_hints(stoat);
                stoat.set_status("inlay hints off");
            }
            UpdateEffect::Redraw
        },
        ActionKind::MoveLeft => movement::move_horizontal(stoat, -1, false),
        ActionKind::MoveRight => movement::move_horizontal(stoat, 1, false),
        ActionKind::MoveUp => movement::move_vertical(stoat, -1, false),
        ActionKind::MoveDown => movement::move_vertical(stoat, 1, false),
        ActionKind::MoveLineUp => movement::move_vertical_by_line(stoat, -1, false),
        ActionKind::MoveLineDown => movement::move_vertical_by_line(stoat, 1, false),
        ActionKind::PageUp => view::page_motion(stoat, view::PageDir::Up, false),
        ActionKind::PageDown => view::page_motion(stoat, view::PageDir::Down, false),
        ActionKind::HalfPageUp => view::page_motion(stoat, view::PageDir::Up, true),
        ActionKind::HalfPageDown => view::page_motion(stoat, view::PageDir::Down, true),
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
        ActionKind::ExtendLineUp => movement::move_vertical_by_line(stoat, -1, true),
        ActionKind::ExtendLineDown => movement::move_vertical_by_line(stoat, 1, true),
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
        ActionKind::ExtendNextLongWordStart => {
            movement::move_word(stoat, movement::WordTarget::NextLongStart, true)
        },
        ActionKind::ExtendNextLongWordEnd => {
            movement::move_word(stoat, movement::WordTarget::NextLongEnd, true)
        },
        ActionKind::ExtendPrevLongWordStart => {
            movement::move_word(stoat, movement::WordTarget::PrevLongStart, true)
        },
        ActionKind::ExtendPrevLongWordEnd => {
            movement::move_word(stoat, movement::WordTarget::PrevLongEnd, true)
        },
        ActionKind::GotoLineStart => movement::goto_line_start(stoat, false),
        ActionKind::GotoLineEnd => movement::goto_line_end(stoat, false),
        ActionKind::GotoFirstNonwhitespace => with_focused_editor(stoat, |editor| {
            movement::goto_first_nonwhitespace(editor, false)
        }),
        ActionKind::EnterInsertMode => with_focused_editor(stoat, movement::enter_insert_mode),
        ActionKind::AppendMode => movement::append_mode(stoat),
        ActionKind::InsertAtLineEnd => movement::insert_at_line_end(stoat),
        ActionKind::InsertAtLineStart => movement::insert_at_line_start(stoat),
        ActionKind::OpenBelow => movement::open_line(
            stoat,
            movement::OpenDir::Below,
            movement::CommentContinuation::Enabled,
        ),
        ActionKind::OpenAbove => movement::open_line(
            stoat,
            movement::OpenDir::Above,
            movement::CommentContinuation::Enabled,
        ),
        ActionKind::ReplaceChar => movement::set_pending_replace(stoat),
        ActionKind::GotoFileStart => movement::goto_file_start(stoat, false),
        ActionKind::GotoLastLine => movement::goto_last_line(stoat, false),
        ActionKind::GotoLineNumber => movement::goto_line_number(stoat, false),
        ActionKind::GotoColumn => movement::goto_column(stoat, false),
        ActionKind::ExtendGotoColumn => movement::goto_column(stoat, true),
        ActionKind::GotoNextChange => {
            movement::goto_change(stoat, movement::ChangeDir::Next, movement::ChangeUnit::Tree)
        },
        ActionKind::GotoPrevChange => {
            movement::goto_change(stoat, movement::ChangeDir::Prev, movement::ChangeUnit::Tree)
        },
        ActionKind::GotoNextGitChange => {
            movement::goto_change(stoat, movement::ChangeDir::Next, movement::ChangeUnit::Git)
        },
        ActionKind::GotoPrevGitChange => {
            movement::goto_change(stoat, movement::ChangeDir::Prev, movement::ChangeUnit::Git)
        },
        ActionKind::ChangeWalkWrap => movement::toggle_change_wrap(stoat),
        ActionKind::ToggleBlame => blame::toggle_blame(stoat),
        ActionKind::BlameCard => blame::open_card(stoat, true),
        ActionKind::BlameCardClose => blame::close_card(stoat),
        ActionKind::BlameCopySha => blame::copy_sha(stoat),
        ActionKind::BlamePreview => blame::preview(stoat),
        ActionKind::BlameCheckout => blame::checkout(stoat),
        ActionKind::BlameParent => blame::blame_parent(stoat),
        ActionKind::CheckoutConfirmAccept => blame::accept_checkout(stoat),
        ActionKind::CheckoutConfirmCancel => checkout_confirm::cancel(stoat),
        ActionKind::CommitPreviewClose => commit_preview::close(stoat),
        ActionKind::CommitPreviewLineDown => commit_preview::scroll_by(stoat, 1),
        ActionKind::CommitPreviewLineUp => commit_preview::scroll_by(stoat, -1),
        ActionKind::CommitPreviewNextFile => commit_preview::hop_file(stoat, true),
        ActionKind::CommitPreviewPrevFile => commit_preview::hop_file(stoat, false),
        ActionKind::CommitPreviewParent => commit_preview::next_parent(stoat),
        ActionKind::DiffUnderline => stoat.handle_diff_underline_toggle(),
        ActionKind::DiffBold => stoat.handle_diff_bold_toggle(),
        ActionKind::DiffSyntax => stoat.handle_diff_syntax_toggle(),
        ActionKind::DiffTintDown => stoat.handle_diff_tint_step(-1),
        ActionKind::DiffTintUp => stoat.handle_diff_tint_step(1),
        ActionKind::DiffBack => diff_nav::back(stoat),
        ActionKind::DiffForward => diff_nav::forward(stoat),
        ActionKind::DiffAgainstIndex => review::diff_against(stoat, WorktreeBase::Index),
        ActionKind::DiffAgainstHead => review::diff_against(stoat, WorktreeBase::Head),
        ActionKind::GitDiff => review::diff_against(stoat, WorktreeBase::Index),
        ActionKind::GitDiffCached => review::diff_against(stoat, WorktreeBase::Head),
        ActionKind::DiffBaseToggle => review::toggle_diff_base(stoat),
        ActionKind::DiffBaseEdit => review::edit_diff_base(stoat),
        ActionKind::DiffPair => review::diff_pair(stoat),
        ActionKind::GotoNextParagraph => {
            movement::goto_paragraph(stoat, movement::ParaDir::Next, false)
        },
        ActionKind::GotoPrevParagraph => {
            movement::goto_paragraph(stoat, movement::ParaDir::Prev, false)
        },
        ActionKind::ExtendGotoNextParagraph => {
            movement::goto_paragraph(stoat, movement::ParaDir::Next, true)
        },
        ActionKind::ExtendGotoPrevParagraph => {
            movement::goto_paragraph(stoat, movement::ParaDir::Prev, true)
        },
        ActionKind::MatchBrackets => movement::match_brackets(stoat, false),
        ActionKind::ExtendMatchBrackets => movement::match_brackets(stoat, true),
        ActionKind::ExpandSelection => movement::expand_selection(stoat),
        ActionKind::ShrinkSelection => movement::shrink_selection(stoat),
        ActionKind::SelectNextSibling => {
            movement::select_sibling(stoat, movement::SiblingDir::Next)
        },
        ActionKind::SelectPrevSibling => {
            movement::select_sibling(stoat, movement::SiblingDir::Prev)
        },
        ActionKind::SelectAllSiblings => movement::select_all_siblings(stoat),
        ActionKind::SelectAllChildren => movement::select_all_children(stoat),
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
        ActionKind::OpenJumplistPicker => picker::open_jumplist_picker(stoat),
        ActionKind::OpenWalkthroughPicker => picker::open_walkthrough_picker(stoat),
        ActionKind::OpenPaneFinder => picker::open_pane_finder(stoat),
        ActionKind::OpenDiagnosticsPicker => picker::open_diagnostics_picker(stoat),
        ActionKind::OpenWorkspaceDiagnosticsPicker => {
            picker::open_workspace_diagnostics_picker(stoat)
        },
        ActionKind::PickerNext => {
            let step = arming_step(stoat);
            picker::picker_step(stoat, step)
        },
        ActionKind::PickerPrev => {
            let step = arming_step(stoat);
            picker::picker_step(stoat, -step)
        },
        ActionKind::PickerPageDown => picker::picker_page(stoat, 1),
        ActionKind::PickerPageUp => picker::picker_page(stoat, -1),
        ActionKind::PickerComplete => picker::picker_complete(stoat),
        ActionKind::PickerFirst => picker::picker_end(stoat, false),
        ActionKind::PickerLast => match stoat.take_pending_count() {
            Some(n) => picker::picker_nth(stoat, n.max(1) as usize - 1),
            None => picker::picker_end(stoat, true),
        },
        ActionKind::PickerDetailDown => picker::picker_detail(stoat, 1),
        ActionKind::PickerDetailUp => picker::picker_detail(stoat, -1),
        ActionKind::PickerDelete => picker::picker_delete(stoat),
        ActionKind::PickerRename => picker::picker_rename(stoat),
        ActionKind::GitReview => {
            let action = action
                .as_any()
                .downcast_ref::<GitReview>()
                .expect("GitReview action downcast");
            review_walk::git_review(stoat, &action.reference)
        },
        ActionKind::Review => {
            let action = action
                .as_any()
                .downcast_ref::<Review>()
                .expect("Review action downcast");
            review_walk::review(stoat, action.from.as_deref(), action.against.as_deref())
        },
        ActionKind::GitLs => {
            let action = action
                .as_any()
                .downcast_ref::<GitLs>()
                .expect("GitLs action downcast");
            review_walk::git_ls(stoat, action.rev.as_deref())
        },
        ActionKind::GitCheckout => {
            let action = action
                .as_any()
                .downcast_ref::<GitCheckout>()
                .expect("GitCheckout action downcast");
            review_walk::git_checkout(stoat, &action.reference)
        },
        ActionKind::GitMerge => {
            let action = action
                .as_any()
                .downcast_ref::<GitMerge>()
                .expect("GitMerge action downcast");
            git_merge::git_merge(stoat, &action.branch)
        },
        ActionKind::GitMergeAbort => git_merge::git_merge_abort(stoat),
        ActionKind::GitMergeContinue => git_merge::git_merge_continue(stoat),
        ActionKind::GitBranch => palette::open_palette_seeded(stoat, "git checkout "),
        ActionKind::GitCommit => git_commit::git_commit(stoat),
        ActionKind::GitAmend => git_commit::git_amend(stoat),
        ActionKind::GitAdd => git_commit::git_add(stoat),
        ActionKind::GitStash => git_stash::git_stash(stoat, None),
        ActionKind::GitStashPush => {
            let action = action
                .as_any()
                .downcast_ref::<GitStashPush>()
                .expect("GitStashPush action downcast");
            git_stash::git_stash(stoat, action.message.as_deref())
        },
        ActionKind::GitStashPop => git_stash::git_stash_pop(stoat),
        ActionKind::GitStashList => review_walk::git_stash_list(stoat),
        ActionKind::ReviewNextCommit => review_walk::review_next_commit(stoat),
        ActionKind::ReviewPrevCommit => review_walk::review_prev_commit(stoat),
        ActionKind::ReviewDone => review_walk::review_done(stoat),
        ActionKind::CommitPickerNextBranch => review_walk::commit_picker_branch(stoat, 1),
        ActionKind::CommitPickerPrevBranch => review_walk::commit_picker_branch(stoat, -1),
        ActionKind::CommitPickerColumnCycle => review_walk::commit_picker_column_cycle(stoat),
        ActionKind::CommitPickerDrillIn => review_walk::commit_picker_drill_in(stoat),
        ActionKind::CommitPickerBack => review_walk::commit_picker_back(stoat),
        ActionKind::OpenCodeSearch => code_search::open_code_search(stoat),
        ActionKind::CodeSearchSelect => {
            code_search::code_search_select(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::CodeSearchClose => {
            code_search::close_code_search(stoat);
            UpdateEffect::Redraw
        },
        ActionKind::CodeSearchModeToggle => code_search::code_search_mode_toggle(stoat),
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
        ActionKind::GotoNextParameter => textobject_nav::goto_textobject(
            stoat,
            textobject_nav::NavKind::Parameter,
            textobject_nav::NavDirection::Next,
        ),
        ActionKind::GotoPrevParameter => textobject_nav::goto_textobject(
            stoat,
            textobject_nav::NavKind::Parameter,
            textobject_nav::NavDirection::Prev,
        ),
        ActionKind::GotoNextComment => textobject_nav::goto_textobject(
            stoat,
            textobject_nav::NavKind::Comment,
            textobject_nav::NavDirection::Next,
        ),
        ActionKind::GotoPrevComment => textobject_nav::goto_textobject(
            stoat,
            textobject_nav::NavKind::Comment,
            textobject_nav::NavDirection::Prev,
        ),
        ActionKind::GotoNextTest => textobject_nav::goto_textobject(
            stoat,
            textobject_nav::NavKind::Test,
            textobject_nav::NavDirection::Next,
        ),
        ActionKind::GotoPrevTest => textobject_nav::goto_textobject(
            stoat,
            textobject_nav::NavKind::Test,
            textobject_nav::NavDirection::Prev,
        ),
        ActionKind::GotoNextEntry => textobject_nav::goto_textobject(
            stoat,
            textobject_nav::NavKind::Entry,
            textobject_nav::NavDirection::Next,
        ),
        ActionKind::GotoPrevEntry => textobject_nav::goto_textobject(
            stoat,
            textobject_nav::NavKind::Entry,
            textobject_nav::NavDirection::Prev,
        ),
        ActionKind::GotoNextXmlElement => textobject_nav::goto_textobject(
            stoat,
            textobject_nav::NavKind::XmlElement,
            textobject_nav::NavDirection::Next,
        ),
        ActionKind::GotoPrevXmlElement => textobject_nav::goto_textobject(
            stoat,
            textobject_nav::NavKind::XmlElement,
            textobject_nav::NavDirection::Prev,
        ),
        ActionKind::OpenSearchInput => search::open_search_input(stoat),
        ActionKind::OpenReverseSearchInput => search::open_reverse_search_input(stoat),
        ActionKind::SearchNext => search::search_next(stoat),
        ActionKind::SearchPrev => search::search_prev(stoat),
        ActionKind::ExtendSearchNext => search::extend_search_next(stoat),
        ActionKind::ExtendSearchPrev => search::extend_search_prev(stoat),
        ActionKind::SearchSelection => search::search_selection(stoat, false),
        ActionKind::SearchSelectionDetectWordBoundaries => search::search_selection(stoat, true),
        ActionKind::Yank => yank::yank(stoat),
        ActionKind::PasteAfter => yank::paste_after(stoat),
        ActionKind::PasteBefore => yank::paste_before(stoat),
        ActionKind::ReplaceWithYanked => yank::replace_with_yanked(stoat),
        ActionKind::ReplaceWithClipboard => yank::replace_with_clipboard(stoat),
        ActionKind::YankToClipboard => yank::yank_to_clipboard(stoat),
        ActionKind::YankMainToClipboard => yank::yank_main_to_clipboard(stoat),
        ActionKind::PasteClipboardAfter => yank::paste_clipboard_after(stoat),
        ActionKind::PasteClipboardBefore => yank::paste_clipboard_before(stoat),
        ActionKind::SelectRegister => yank::select_register(stoat),
        ActionKind::InsertRegister => yank::insert_register(stoat),
        ActionKind::RepeatLastMotion => repeat_last_motion(stoat),
        ActionKind::GotoWindowTop => view::goto_window(stoat, view::WindowAlign::Top, false),
        ActionKind::GotoWindowCenter => view::goto_window(stoat, view::WindowAlign::Center, false),
        ActionKind::GotoWindowBottom => view::goto_window(stoat, view::WindowAlign::Bottom, false),
        ActionKind::GotoWord => movement::goto_word(stoat),
        ActionKind::ExtendToWord => movement::extend_to_word(stoat),
        ActionKind::GotoLastModification => movement::goto_last_modification(stoat),
        ActionKind::AddNewlineBelow => movement::add_newline_below(stoat),
        ActionKind::AddNewlineAbove => movement::add_newline_above(stoat),
        ActionKind::ExtendGotoFirstNonwhitespace => with_focused_editor(stoat, |editor| {
            movement::goto_first_nonwhitespace(editor, true)
        }),
        ActionKind::ExtendGotoFileStart => movement::goto_file_start(stoat, true),
        ActionKind::ExtendGotoLastLine => movement::goto_last_line(stoat, true),
        ActionKind::ExtendGotoWindowTop => view::goto_window(stoat, view::WindowAlign::Top, true),
        ActionKind::ExtendGotoWindowCenter => {
            view::goto_window(stoat, view::WindowAlign::Center, true)
        },
        ActionKind::ExtendGotoWindowBottom => {
            view::goto_window(stoat, view::WindowAlign::Bottom, true)
        },
        ActionKind::AlignViewTop => align_focused_view(stoat, view::ViewAlign::Top),
        ActionKind::AlignViewCenter => align_focused_view(stoat, view::ViewAlign::Center),
        ActionKind::AlignViewBottom => align_focused_view(stoat, view::ViewAlign::Bottom),
        ActionKind::ScrollUp => view::scroll_view(stoat, view::ScrollDir::Up),
        ActionKind::ScrollDown => view::scroll_view(stoat, view::ScrollDir::Down),
        ActionKind::SwitchCase => movement::switch_case(stoat),
        ActionKind::SwitchToUppercase => movement::switch_to_uppercase(stoat),
        ActionKind::SwitchToLowercase => movement::switch_to_lowercase(stoat),
        ActionKind::ExtendToLineStart => movement::goto_line_start(stoat, true),
        ActionKind::ExtendToLineEnd => movement::goto_line_end(stoat, true),
        ActionKind::ExtendToFileStart => movement::goto_file_start(stoat, true),
        ActionKind::ExtendGotoLineNumber => movement::goto_line_number(stoat, true),
        ActionKind::CollapseSelection => with_focused_editor(stoat, movement::collapse_selection),
        ActionKind::FlipSelections => with_focused_editor(stoat, movement::flip_selections),
        ActionKind::EnsureSelectionsForward => {
            with_focused_editor(stoat, movement::ensure_selections_forward)
        },
        ActionKind::SelectAll => with_focused_editor(stoat, movement::select_all),
        ActionKind::ExtendLineBelow => movement::extend_line_below(stoat),
        ActionKind::ExtendToLineBounds => {
            with_focused_editor(stoat, movement::extend_to_line_bounds)
        },
        ActionKind::ShrinkToLineBounds => {
            with_focused_editor(stoat, movement::shrink_to_line_bounds)
        },
        ActionKind::KeepPrimarySelection => {
            with_focused_editor(stoat, movement::keep_primary_selection)
        },
        ActionKind::RemovePrimarySelection => movement::remove_primary_selection(stoat),
        ActionKind::MergeSelections => with_focused_editor(stoat, movement::merge_selections),
        ActionKind::MergeConsecutiveSelections => {
            with_focused_editor(stoat, movement::merge_consecutive_selections)
        },
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
        ActionKind::TrimSelections => with_focused_editor(stoat, movement::trim_selections),
        ActionKind::OpenRun => run::open_run(stoat),
        ActionKind::SpawnClaude => agent::spawn_claude_pane(stoat),
        ActionKind::Terminal => terminal::open_terminal_pane(stoat),
        ActionKind::RunSubmit => run::run_submit(stoat),
        ActionKind::RunSubmitJob => jobs::run_submit_job(stoat),
        ActionKind::RunInterrupt => run::run_interrupt(stoat),
        ActionKind::RunJobToggle => jobs::run_job_toggle(stoat),
        ActionKind::JobGlance => jobs::job_glance_open(stoat),
        ActionKind::JobGlanceNext => jobs::job_glance_step(stoat, 1),
        ActionKind::JobGlancePrev => jobs::job_glance_step(stoat, -1),
        ActionKind::JobGlanceClose => jobs::job_glance_close(stoat),
        ActionKind::JobOpen => jobs::job_open(stoat),
        ActionKind::JobRerun => jobs::job_rerun(stoat),
        ActionKind::JobInterrupt => jobs::job_interrupt(stoat),
        ActionKind::JobDismiss => jobs::job_dismiss(stoat),
        ActionKind::Jobs => jobs::open_job_list(stoat),
        ActionKind::JobListScopeToggle => jobs::job_list_scope_toggle(stoat),
        ActionKind::JobRerunLast => jobs::job_rerun_last(stoat),
        ActionKind::RunToggleView => run::run_toggle_view(stoat),
        ActionKind::RunAgain => run::run_again(stoat),
        ActionKind::RunBlockToPane => run::run_block_to_pane(stoat),
        ActionKind::RunLocationList => run::run_location_list(stoat),
        ActionKind::RunHistoryPrev => run::run_history_prev(stoat),
        ActionKind::RunHistoryNext => run::run_history_next(stoat),
        ActionKind::HelpScopeToggle => help::help_scope_toggle(stoat),
        ActionKind::WorkspacePickerScopeToggle => workspace::workspace_picker_scope_toggle(stoat),
        ActionKind::WorkspacePickerCreateSub => workspace::workspace_picker_create_sub(stoat),
        ActionKind::WorkspacePickerCloseSession => workspace::workspace_picker_close_session(stoat),
        ActionKind::CloseHelp => help::help_cancel(stoat),
        ActionKind::Run => {
            let cmd = action
                .as_any()
                .downcast_ref::<Run>()
                .expect("Run action downcast");
            run::run_line(stoat, &cmd.command)
        },
        ActionKind::ToggleDockRight => pane::toggle_dock(stoat, DockSide::Right),
        ActionKind::ToggleDockLeft => pane::toggle_dock(stoat, DockSide::Left),
        ActionKind::JumpToMoveSource => {
            move_nav::navigate(stoat, move_nav::MoveNavigation::FirstSource)
        },
        ActionKind::JumpToMoveTarget => move_nav::navigate(stoat, move_nav::MoveNavigation::Target),
        ActionKind::JumpToNextMoveSource => {
            move_nav::navigate(stoat, move_nav::MoveNavigation::NextSource)
        },
        ActionKind::JumpToPrevMoveSource => {
            move_nav::navigate(stoat, move_nav::MoveNavigation::PrevSource)
        },
        ActionKind::QueryMoveRelationships => {
            // Scriptable surface: observes the move metadata under the
            // cursor but does not navigate. A future automation hook
            // will expose this via the action SDK; for now it resolves
            // and logs the relationship count so the action is
            // observable from tests.
            if let Some(summary) =
                focused_editor_mut(stoat).and_then(move_nav::current_move_summary)
            {
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
        ActionKind::GotoFirstDiagnostic => {
            lsp::goto_diagnostic(stoat, lsp::DiagnosticDirection::First)
        },
        ActionKind::GotoLastDiagnostic => {
            lsp::goto_diagnostic(stoat, lsp::DiagnosticDirection::Last)
        },
        ActionKind::GotoFirstChange => movement::goto_first_change(stoat),
        ActionKind::GotoLastChange => movement::goto_last_change(stoat),
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
        ActionKind::TrailClear => crate::code_index::nav::trail_clear(stoat),
        ActionKind::WalkthroughOpen => {
            let action = action
                .as_any()
                .downcast_ref::<WalkthroughOpen>()
                .expect("WalkthroughOpen action downcast");
            walkthrough::open(stoat, &action.slug)
        },
        ActionKind::WalkthroughNext => {
            let step = arming_step(stoat);
            walkthrough::next(stoat, step)
        },
        ActionKind::WalkthroughPrev => {
            let step = arming_step(stoat);
            walkthrough::prev(stoat, step)
        },
        ActionKind::WalkthroughNextAnnotation => {
            let step = arming_step(stoat);
            walkthrough::next_annotation(stoat, step)
        },
        ActionKind::WalkthroughPrevAnnotation => {
            let step = arming_step(stoat);
            walkthrough::prev_annotation(stoat, step)
        },
        ActionKind::WalkthroughForward => {
            let step = arming_step(stoat);
            walkthrough::forward(stoat, step)
        },
        ActionKind::WalkthroughBackward => {
            let step = arming_step(stoat);
            walkthrough::backward(stoat, step)
        },
        ActionKind::WalkthroughShowNarration => walkthrough::show_narration_again(stoat),
        ActionKind::WalkthroughShowDescription => walkthrough::show_description(stoat),
        ActionKind::WalkthroughDone => walkthrough::done(stoat),
        ActionKind::Hover => crate::lsp::hover::hover(stoat),
        ActionKind::CodeAction => lsp::code_action(stoat),
        ActionKind::RenameSymbol => lsp::rename_symbol(stoat),
        ActionKind::OpenSymbolPicker => lsp::open_symbol_picker(stoat),
        ActionKind::OpenWorkspaceSymbolPicker => lsp::open_workspace_symbol_picker(stoat),
        ActionKind::FormatSelections => lsp::format_selections(stoat),
        ActionKind::Format => lsp::format_document(stoat),
        ActionKind::ReflowComment => comment_reflow::reflow_comment(stoat),
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
        ActionKind::CommitsNext => {
            let count = arming_count(stoat);
            commits::commits_step(stoat, commits::CommitStep::Down(count))
        },
        ActionKind::CommitsPrev => {
            let count = arming_count(stoat);
            commits::commits_step(stoat, commits::CommitStep::Up(count))
        },
        ActionKind::CommitsPageDown => commits::commits_step(stoat, commits::CommitStep::PageDown),
        ActionKind::CommitsPageUp => commits::commits_step(stoat, commits::CommitStep::PageUp),
        ActionKind::CommitsDetailDown => commits::commits_detail_half_page(stoat, 1),
        ActionKind::CommitsDetailUp => commits::commits_detail_half_page(stoat, -1),
        ActionKind::CommitsFirst => commits::commits_step(stoat, commits::CommitStep::First),
        ActionKind::CommitsLast => match stoat.take_pending_count() {
            Some(n) => {
                commits::commits_step(stoat, commits::CommitStep::Nth(n.max(1) as usize - 1))
            },
            None => commits::commits_step(stoat, commits::CommitStep::Last),
        },
        ActionKind::CommitsRefresh => commits::commits_refresh(stoat),
        ActionKind::CommitsOpenReview => review::commits_open_review(stoat),
        ActionKind::DiffChanges => diff_changes::toggle_diff_changes(stoat),
        ActionKind::GitStatus => git_status::open_git_status(stoat),
        ActionKind::CloseGitStatus => git_status::close_git_status(stoat),
        ActionKind::GitStatusNext => git_status::git_status_step(stoat, 1),
        ActionKind::GitStatusPrev => git_status::git_status_step(stoat, -1),
        ActionKind::GitStatusOpen => git_status::git_status_open(stoat),
        ActionKind::GitStatusStage => git_status::git_status_stage(stoat),
        ActionKind::GitStatusUnstage => git_status::git_status_unstage(stoat),
        ActionKind::GitStatusTakeOurs => git_status::git_status_take(stoat, Side::Ours),
        ActionKind::GitStatusTakeTheirs => git_status::git_status_take(stoat, Side::Theirs),
        ActionKind::GitStatusCommit => git_status::git_status_commit(stoat),
        ActionKind::GitStatusAbort => git_status::git_status_abort(stoat),
        ActionKind::GitStatusSkip => git_status::git_status_skip(stoat),
        ActionKind::CloseDiffChanges => diff_changes::close_diff_changes(stoat),
        ActionKind::DiffChangesNext => {
            let step = arming_step(stoat);
            diff_changes::diff_changes_step(stoat, step)
        },
        ActionKind::DiffChangesPrev => {
            let step = arming_step(stoat);
            diff_changes::diff_changes_step(stoat, -step)
        },
        ActionKind::DiffChangesPageDown => {
            diff_changes::diff_changes_page(stoat, view::PageDir::Down)
        },
        ActionKind::DiffChangesPageUp => diff_changes::diff_changes_page(stoat, view::PageDir::Up),
        ActionKind::DiffChangesHalfPageDown => {
            diff_changes::diff_changes_half_page(stoat, view::PageDir::Down)
        },
        ActionKind::DiffChangesHalfPageUp => {
            diff_changes::diff_changes_half_page(stoat, view::PageDir::Up)
        },
        ActionKind::DiffChangesFirst => diff_changes::diff_changes_edge(stoat, false),
        ActionKind::DiffChangesLast => diff_changes::diff_changes_edge(stoat, true),
        ActionKind::DiffChangesOpen => diff_changes::diff_changes_open(stoat),
        ActionKind::DiffChangesExpandUp => diff_changes::diff_changes_expand(stoat, ExpandDir::Up),
        ActionKind::DiffChangesExpandDown => {
            diff_changes::diff_changes_expand(stoat, ExpandDir::Down)
        },
        ActionKind::DiffChangesExpandAll => {
            diff_changes::diff_changes_expand(stoat, ExpandDir::All)
        },
        ActionKind::DiffChangesCollapse => diff_changes::diff_changes_collapse(stoat),
        ActionKind::EnterRebase => rebase::enter_rebase(stoat),
        ActionKind::GitRebase => {
            let action = action
                .as_any()
                .downcast_ref::<GitRebase>()
                .expect("GitRebase action downcast");
            rebase::git_rebase(stoat, action.rev.as_deref())
        },
        ActionKind::AbortRebase => rebase::abort_rebase(stoat),
        ActionKind::GitRebaseAbort => rebase::git_rebase_abort(stoat),
        ActionKind::ExecuteRebase => rebase::execute_rebase(stoat),
        ActionKind::RebaseNext => {
            let count = arming_count(stoat);
            rebase::rebase_move(stoat, rebase::RebaseMove::Next, count)
        },
        ActionKind::RebasePrev => {
            let count = arming_count(stoat);
            rebase::rebase_move(stoat, rebase::RebaseMove::Prev, count)
        },
        ActionKind::RebaseMoveUp => {
            let count = arming_count(stoat);
            rebase::rebase_move(stoat, rebase::RebaseMove::SwapUp, count)
        },
        ActionKind::RebaseMoveDown => {
            let count = arming_count(stoat);
            rebase::rebase_move(stoat, rebase::RebaseMove::SwapDown, count)
        },
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
        ActionKind::GitRebaseContinue => rebase::git_rebase_continue(stoat),
        ActionKind::RebaseConflictTakeOurs => {
            conflict::conflict_set(stoat, conflict::ConflictChoice::Ours)
        },
        ActionKind::RebaseConflictTakeTheirs => {
            conflict::conflict_set(stoat, conflict::ConflictChoice::Theirs)
        },
        ActionKind::RebaseConflictSkipEntry => conflict::conflict_skip_entry(stoat),
        ActionKind::RebaseConflictNextFile => {
            let count = arming_count(stoat);
            conflict::conflict_step(stoat, true, count)
        },
        ActionKind::RebaseConflictPrevFile => {
            let count = arming_count(stoat);
            conflict::conflict_step(stoat, false, count)
        },
        ActionKind::RebaseConflictApply => conflict::conflict_apply(stoat),
        ActionKind::RebaseConflictAbort => conflict::conflict_abort(stoat),
        ActionKind::Dump => {
            let dump = action
                .as_any()
                .downcast_ref::<Dump>()
                .expect("Dump action downcast");
            workspace::handle_dump(stoat, &dump.name);
            UpdateEffect::Redraw
        },
        ActionKind::SetTheme => {
            let action = action
                .as_any()
                .downcast_ref::<SetTheme>()
                .expect("SetTheme action downcast");
            set_theme(stoat, &action.name)
        },
        ActionKind::NewWorkspace => workspace::new_workspace(stoat),
        ActionKind::CopyWorkspace => workspace::copy_workspace(stoat),
        ActionKind::SwitchWorkspace => workspace::open_workspace_picker(stoat),
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
        ActionKind::Ssh => {
            let action = action
                .as_any()
                .downcast_ref::<Ssh>()
                .expect("Ssh action downcast");
            crate::ssh::connect(
                stoat,
                crate::ssh::Transport::Ssh,
                action.host.as_deref(),
                &action.args,
            )
        },
        ActionKind::Mosh => {
            let action = action
                .as_any()
                .downcast_ref::<Mosh>()
                .expect("Mosh action downcast");
            crate::ssh::connect(
                stoat,
                crate::ssh::Transport::Mosh,
                action.host.as_deref(),
                &action.args,
            )
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
        ActionKind::PromptHistoryPrev => prompt::prompt_history_prev(stoat),
        ActionKind::PromptHistoryNext => prompt::prompt_history_next(stoat),
        ActionKind::PaletteScopeToggle => palette::palette_scope_toggle(stoat),
        ActionKind::PaletteCmdToJobs => {
            palette::palette_cmd_to_jobs(stoat).unwrap_or(UpdateEffect::None)
        },
        ActionKind::PaletteSubmitJob => {
            palette::palette_submit_job(stoat).unwrap_or(UpdateEffect::None)
        },
        ActionKind::PaletteUp => palette::palette_up(stoat),
        ActionKind::PaletteRefresh => palette::palette_refresh(stoat),
        ActionKind::OpenLastPicker => open_last_picker(stoat),
    };
    end_action_group(stoat, group_buffer);
    if matches!(effect, UpdateEffect::Redraw) && is_picker_open_kind(action.kind()) {
        stoat.last_picker_action = Some(action.def().name());
    }
    log_picker_open(stoat, action, modal_before);
    effect
}

/// Log one `debug` line on `stoat::picker` when `action` opened a picker.
///
/// A picker counts as opened only when a modal is up that was not before the
/// action ran, so a second open of a picker already up logs nothing. A re-fire
/// through `OpenLastPicker` or a palette command logs once, from the opening
/// action's own dispatch, because neither of those is an opener kind.
fn log_picker_open(stoat: &Stoat, action: &dyn Action, modal_before: Option<ActiveModal>) {
    if !opens_picker_kind(action.kind()) {
        return;
    }
    let modal = keymap_state::active_modal(stoat);
    if modal.is_some() && modal != modal_before {
        tracing::debug!(target: "stoat::picker", action = action.def().name(), "picker opened");
    }
}

/// Action kinds that open a picker, for the `picker opened` line.
///
/// Wider than [`is_picker_open_kind`], which also decides what `OpenLastPicker`
/// re-fires. The help, workspace, symbol, and commit pickers open here too.
fn opens_picker_kind(kind: ActionKind) -> bool {
    is_picker_open_kind(kind)
        || matches!(
            kind,
            ActionKind::OpenHelp
                | ActionKind::SwitchWorkspace
                | ActionKind::OpenSymbolPicker
                | ActionKind::OpenWorkspaceSymbolPicker
                | ActionKind::GitReview
                | ActionKind::GitLs
                | ActionKind::GitStashList
        )
}

/// Actions that drive the undo history themselves, so [`dispatch`] must not wrap
/// them in an action group. `CommitUndoCheckpoint` seals and reopens the insert
/// group directly. Undo and redo pop and replay existing groups.
fn manages_own_undo_group(kind: ActionKind) -> bool {
    matches!(
        kind,
        ActionKind::Undo
            | ActionKind::Redo
            | ActionKind::Earlier
            | ActionKind::Later
            | ActionKind::CommitUndoCheckpoint
    )
}

/// Run `edit` as one undo step on the focused buffer, the way [`dispatch`]
/// runs an action.
///
/// This is for an edit that a key or a press completes outside [`dispatch`],
/// such as the replace and surround chords and a press that picks a conflict
/// side. One undo takes back every edit that `edit` makes and restores the
/// selections from before it.
pub(crate) fn with_action_group(
    stoat: &mut Stoat,
    edit: impl FnOnce(&mut Stoat) -> UpdateEffect,
) -> UpdateEffect {
    let group = begin_action_group(stoat);
    let effect = edit(stoat);
    end_action_group(stoat, group);
    effect
}

/// Open an undo group on the focused editor's buffer so every edit the
/// dispatched action makes collapses into one undo step, capturing the
/// pre-action selections to restore on undo. Returns the grouped buffer, or
/// `None` when the focused pane is not an editor. Lazy group materialization
/// means a non-editing action leaves the undo history untouched.
///
/// An already-open group (an in-flight insert session) takes precedence: the
/// action opens no group of its own, returns `None` so [`end_action_group`]
/// seals nothing, and its edits join the session's single undo step.
fn begin_action_group(stoat: &mut Stoat) -> Option<BufferId> {
    let (editor_id, buffer_id) = stoat.focused_editor_ids()?;
    let buffer = stoat.active_workspace().buffers.get(buffer_id)?;
    let opened = buffer
        .write()
        .expect("poisoned")
        .try_begin_group(|| editor_selection_snapshot(stoat, editor_id));
    opened.then_some(buffer_id)
}

/// Seal the group opened by [`begin_action_group`], capturing the post-action
/// selections to restore on redo. The group self-discards when the action made
/// no edits.
///
/// A `None` buffer id means [`begin_action_group`] joined an already-open
/// insert session instead of opening its own group, so there is nothing to seal
/// and the session stays open.
///
/// The post-selections are gathered only for a group that took an edit, since
/// sealing an untouched one discards them. Most actions edit nothing, and
/// gathering costs a copy of the whole selection set.
fn end_action_group(stoat: &mut Stoat, buffer_id: Option<BufferId>) {
    // An insert-entry chord runs its editing half before the mode switch, and
    // the two are one change to the user. Leaving the group open lets the
    // insert session adopt it, so the edit and everything typed into it undo
    // together.
    if stoat.group_held_for_insert {
        return;
    }
    let Some(buffer_id) = buffer_id else {
        return;
    };
    let Some(buffer) = stoat.active_workspace().buffers.get(buffer_id) else {
        return;
    };

    // Gathered under the buffer lock rather than before it. The selections live
    // on the editor, so this reaches for no buffer and cannot reenter the lock
    // already held.
    let mut buffer = buffer.write().expect("poisoned");
    let after = match buffer.group_started() {
        true => focused_selection_snapshot(stoat, buffer_id),
        false => Arc::from([]),
    };

    buffer.seal_group(after);
}

/// Selections of `editor_id` as a shared handle, empty when it is gone.
fn editor_selection_snapshot(stoat: &Stoat, editor_id: EditorId) -> Arc<[Selection<Anchor>]> {
    #[cfg(any(test, feature = "test-support"))]
    stoat
        .selection_snapshots
        .set(stoat.selection_snapshots.get() + 1);

    stoat
        .active_workspace()
        .editors
        .get(editor_id)
        .map(|editor| editor.selections.shared_anchors())
        .unwrap_or_else(|| Arc::from([]))
}

/// Selections of the focused editor when it still shows `buffer_id`, else empty.
/// An action that changed focus away from the grouped buffer seals with no
/// post-selections, so redo revalidates rather than restoring a foreign set.
pub(crate) fn focused_selection_snapshot(
    stoat: &Stoat,
    buffer_id: BufferId,
) -> Arc<[Selection<Anchor>]> {
    match stoat.focused_editor_ids() {
        Some((editor_id, focused_buffer)) if focused_buffer == buffer_id => {
            editor_selection_snapshot(stoat, editor_id)
        },
        _ => Arc::from([]),
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
            | ActionKind::OpenWorkspaceFileFinder
            | ActionKind::OpenCommandPalette
            | ActionKind::OpenJumplistPicker
            | ActionKind::OpenWalkthroughPicker
            | ActionKind::OpenPaneFinder
            | ActionKind::OpenCodeSearch
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

/// Re-resolve `name` against the loaded theme blocks and apply it live.
///
/// On success the theme and its derived style tables are swapped and every
/// minimap content store is dropped so each strip re-ships against the new
/// palette. On failure the current theme is kept and a transient message names
/// the problem.
///
/// Per-language highlight maps are not rebuilt, because they index the static
/// theme-key table and only the resolved colors change. For the same reason
/// tokens already produced are recolored by pointing them at the new style
/// table rather than by reparsing or re-requesting them, which would leave them
/// showing the previous theme until an edit or a server response arrived.
fn set_theme(stoat: &mut Stoat, name: &str) -> UpdateEffect {
    match stoat.theme_pool.resolve(name) {
        Ok(theme) => {
            stoat.theme = Arc::new(theme);
            stoat.syntax_styles = SyntaxStyles::from_theme(&stoat.theme);

            let interner = stoat.syntax_styles.interner.clone();
            for (_, ws) in stoat.workspaces.iter_mut() {
                ws.buffers.swap_token_interners(&interner);
                for editor in ws.editors.values_mut() {
                    editor.display_map.swap_style_interner(&interner);
                }
            }

            // A preview session bakes its per-line styles at build, so the
            // cached ones and any build already in flight carry the old theme.
            for (_, ws) in stoat.workspaces.iter_mut() {
                for commits in ws.commit_lists.values_mut() {
                    commits.preview_sessions.clear();
                    commits.pending_preview = None;
                    commits.requested_preview = None;
                }
            }
            if let Some(picker) = stoat.commit_picker.as_mut() {
                picker.preview_sessions.clear();
                picker.pending_preview = None;
                picker.requested_preview = None;
            }
            commit_preview::reload(stoat);

            // The class table is rebuilt and the stores are kept. A class is a
            // theme-key index, so what a store holds means the same under the
            // new palette, and the colors ride the strip declaration on the
            // next frame. A theme that changes which scopes have a foreground
            // moves the table's class version, and each strip recolors.
            stoat.minimap_class_table = crate::minimap::ClassTable::from_theme(&stoat.theme);
            stoat.theme_epoch += 1;
            stoat.paint_generation += 1;
            apc_emit::emit_theme_default_colors(stoat);
            UpdateEffect::Redraw
        },
        Err(e) => {
            stoat.set_status(e.to_string());
            UpdateEffect::Redraw
        },
    }
}

/// Return a mutable reference to the effective focused editor, respecting
/// the reword-pause override. Shared by every movement handler plus the
/// move-navigation summary lookup.
pub fn focused_editor_mut(stoat: &mut Stoat) -> Option<&mut EditorState> {
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
        FocusTarget::SplitPane => {
            let focused = ws.panes.focus();
            ws.panes.pane(focused).view.clone()
        },
        FocusTarget::Dock(dock_id) => {
            let dock = ws.docks.get(dock_id)?;
            dock.view.clone()
        },
    };
    match view {
        View::Editor(id) => ws.editors.get_mut(id),
        View::DiffChanges(id) => {
            let editor_id = ws.diff_changes_views.get(id)?.focused_excerpt()?.editor_id;
            ws.editors.get_mut(editor_id)
        },
        _ => None,
    }
}

/// The editor that the focused split pane's editing keys act on, and the buffer
/// it shows.
///
/// A plain editor pane answers its editor. A changes view answers its focused
/// excerpt's editor, folded or not, so an edit there changes the excerpt's
/// file. A run pane's input editor and a dock take no editing key from the
/// handlers that read this, so they answer `None`, as every other view does.
///
/// See also:
/// - [`focused_editor_mut`] for the editor that motions move, which also answers for a paused
///   reword and a focused dock.
pub(crate) fn pane_editor_ids(ws: &crate::workspace::Workspace) -> Option<(EditorId, BufferId)> {
    match ws.panes.pane(ws.panes.focus()).view {
        View::Editor(id) => Some((id, ws.editors.get(id)?.buffer_id)),
        View::DiffChanges(id) => {
            let excerpt = ws.diff_changes_views.get(id)?.focused_excerpt()?;
            Some((excerpt.editor_id, excerpt.buffer_id))
        },
        _ => None,
    }
}

/// Align the focused view on its cursor's row.
///
/// A changes view scrolls its stack, because the stack's paint sets each
/// excerpt editor's scroll from the stack's own. Any other view scrolls the
/// focused editor.
fn align_focused_view(stoat: &mut Stoat, align: view::ViewAlign) -> UpdateEffect {
    if stoat.active_workspace().focused_diff_changes_id().is_some() {
        return diff_changes::align_stack(stoat, align);
    }
    with_focused_editor(stoat, |editor| view::align_view(editor, align))
}

/// Run `f` on the focused editor, or answer [`UpdateEffect::None`] when no
/// editor has focus.
///
/// A handler that reads only the focused editor takes it as its argument and
/// is wired through here, so a unit test drives it on an [`EditorState`]
/// without a [`Stoat`].
pub(crate) fn with_focused_editor(
    stoat: &mut Stoat,
    f: impl FnOnce(&mut EditorState) -> UpdateEffect,
) -> UpdateEffect {
    match focused_editor_mut(stoat) {
        Some(editor) => f(editor),
        None => UpdateEffect::None,
    }
}

/// The editor a cursor key moves in, which is the editor typing reaches.
///
/// While a modal with an input is open, that input's editor answers.
/// Otherwise this is [`focused_editor_mut`]. Every caller that is not a
/// key-driven character or line motion keeps [`focused_editor_mut`], because
/// it acts on the pane behind the modal.
pub(crate) fn input_target_editor_mut(stoat: &mut Stoat) -> Option<&mut EditorState> {
    match stoat.active_modal_input() {
        Some((editor_id, _)) => stoat.active_workspace_mut().editors.get_mut(editor_id),
        None => focused_editor_mut(stoat),
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
        FocusTarget::SplitPane => {
            let focused = ws.panes.focus();
            Some(&mut ws.panes.pane_mut(focused).jumplist)
        },
        FocusTarget::Dock(_) => None,
    }
}

/// Remove `editor_id` from the workspace's editor store unless a split pane
/// still shows it.
///
/// Every editor-swapping site calls this on the editor it displaced, so a pane
/// never leaves a dangling editor behind.
pub(crate) fn gc_editor_if_unreferenced(ws: &mut crate::workspace::Workspace, editor_id: EditorId) {
    if ws.editor_referenced(editor_id) {
        return;
    }
    if let Some(editor) = ws.editors.remove(editor_id) {
        drop_unreferenced_scratch(ws, editor.buffer_id);
    }
}

/// Drop `id` from the workspace when it is a scratch buffer nothing reaches.
///
/// A pane left showing a scratch keeps it in the registry for the workspace's
/// life: a close, a split, and a terminal exit each make one, no picker lists
/// them, and a session snapshot writes every one with its history. Sweeping
/// when a pane moves off is what bounds that.
///
/// Kept when the buffer has a path, is a preview surface, is dirty, or another
/// editor still holds it. Dirty rather than empty is the test, so a scratch
/// whose edits were undone still goes.
pub(crate) fn drop_unreferenced_scratch(ws: &mut crate::workspace::Workspace, id: BufferId) {
    if ws.buffers.path_for(id).is_some() || ws.buffers.is_preview(id) {
        return;
    }
    let dirty = ws
        .buffers
        .get(id)
        .is_none_or(|buffer| buffer.read().expect("buffer poisoned").dirty);
    if dirty || ws.editors.values().any(|editor| editor.buffer_id == id) {
        return;
    }

    ws.buffers.remove(id);
    ws.release_buffer(id, None);
    tracing::debug!(target: "stoat::file", buffer_id = ?id, "dropped unreferenced scratch buffer");

    // A jumplist entry outliving its buffer would resolve into nothing on the
    // next walk, which is what the close path purges for the same reason.
    for tree in ws.pane_trees_mut() {
        for pane_id in tree.split_pane_ids() {
            tree.pane_mut(pane_id).jumplist.remove_buffer(id);
        }
    }
    ws.release_bridge_waiters(id, BridgeOutcome::Closed);
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
    let stoatty = stoat
        .env_host()
        .var("STOATTY_VERSION")
        .unwrap_or_else(|| "unknown".to_string());
    label.push_str(&format!(" | stoatty {stoatty}"));

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

/// Read `path` through the supplied [`FsHost`] as a UTF-8 string.
pub(crate) fn read_string_via_host(fs: &dyn FsHost, path: &Path) -> std::io::Result<String> {
    let mut buf = Vec::new();
    fs.read(path, &mut buf)?;
    String::from_utf8(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Read the text a reader of `path`'s current content sees, decompressed when
/// the file is a gzip stream.
///
/// See also:
/// - [`read_string_via_host`] for the bytes as they sit on disk.
pub fn read_text_via_host(fs: &dyn FsHost, path: &Path) -> std::io::Result<String> {
    let mut buf = Vec::new();
    fs.read(path, &mut buf)?;
    decode_text(buf).map(|(text, _)| text)
}

/// What opening a file found in it.
///
/// The open path has to tell an image from text before it decides what kind of
/// pane to make, and it has only the bytes to go on. Reading them once and
/// answering both questions is what keeps a large file off the disk twice.
pub enum OpenContent {
    Text {
        text: String,
        /// The bytes on disk are a gzip stream and `text` is what it held. The
        /// text is not the file's content, so it has no file to write back to.
        compressed: bool,
    },
    /// Text already normalized and built into a rope, with the terminator the
    /// file arrived with.
    ///
    /// Only the background open produces this. Detecting the ending, replacing
    /// its terminators, and building the rope are the expensive half of opening
    /// a large file, and the read that reached it was already off the run loop.
    Rope {
        rope: Rope,
        ending: LineEnding,
        /// As on [`OpenContent::Text`].
        compressed: bool,
    },
    /// An image, as its pixel dimensions. The bytes are dropped: nothing yet
    /// draws them, and holding a decoded image per open pane would cost far
    /// more than the label that is shown instead.
    Image { px: (u32, u32) },
}

/// Read `path` through the supplied [`FsHost`], as an image or as text.
///
/// An image is recognized by its leading bytes rather than by its name. A name
/// is what someone typed, and a file whose suffix disagrees with its contents is
/// exactly where guessing from the name goes wrong in both directions.
///
/// Anything that is not a recognized image is read as UTF-8 and fails the same
/// way it always has, so a genuinely undecodable file still reports what is
/// wrong with it. A gzip stream, also told by its leading bytes, is
/// decompressed in memory first, and the file on disk stays as it is.
pub fn read_open_content(fs: &dyn FsHost, path: &Path) -> std::io::Result<OpenContent> {
    let mut buf = Vec::new();
    fs.read(path, &mut buf)?;

    if is_image(&buf)
        && let Some(px) = image_dimensions(&buf)
    {
        return Ok(OpenContent::Image { px });
    }

    let (text, compressed) = decode_text(buf)?;
    Ok(OpenContent::Text { text, compressed })
}

/// Whether `bytes` opens with the signature of an image format this can size.
///
/// A signature check rather than a decode, so the common case of opening a text
/// file pays a handful of byte comparisons instead of a decoder's guess.
fn is_image(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || bytes.starts_with(b"BM")
        || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"))
}

/// The pixel size of an image, read from its header alone.
///
/// `None` for a file that opens like an image and is not one, which is a
/// truncated or corrupt file rather than something to show a size for.
fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// The two leading bytes of every gzip member.
pub(crate) const GZIP_MAGIC: &[u8] = &[0x1f, 0x8b];

/// Decode a file's bytes as UTF-8 text, decompressing a gzip stream first, and
/// say whether it was one.
///
/// A truncated or corrupt stream fails as [`std::io::ErrorKind::InvalidData`],
/// the same kind an undecodable text file fails with.
fn decode_text(buf: Vec<u8>) -> std::io::Result<(String, bool)> {
    let compressed = buf.starts_with(GZIP_MAGIC);
    let bytes = match compressed {
        // Concatenated members are one valid stream, so every member decodes.
        // The reader is a byte slice, so any failure is the decoder's.
        true => {
            let mut out = Vec::new();
            flate2::read::MultiGzDecoder::new(&buf[..])
                .read_to_end(&mut out)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            out
        },
        false => buf,
    };
    let text = String::from_utf8(bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok((text, compressed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_mode_status_label_is_sel() {
        let theme = crate::theme::Theme::empty();
        let badges = std::collections::BTreeMap::new();
        let (label, _) = crate::render::pane::mode_segment("select", &theme, &badges);
        assert_eq!(label, "SEL");
    }

    #[test]
    fn config_badge_overrides_hardcoded_label() {
        let theme = crate::theme::Theme::empty();
        let mut badges = std::collections::BTreeMap::new();
        badges.insert("select".to_string(), "VIS".to_string());
        let (label, _) = crate::render::pane::mode_segment("select", &theme, &badges);
        assert_eq!(label, "VIS");
    }

    #[test]
    fn config_badge_supplies_label_for_user_defined_mode() {
        let theme = crate::theme::Theme::empty();
        let mut badges = std::collections::BTreeMap::new();
        badges.insert("custom".to_string(), "CUS".to_string());
        let (label, _) = crate::render::pane::mode_segment("custom", &theme, &badges);
        assert_eq!(label, "CUS");
        // No badge entry -> hardcoded fallback for unknown mode is "---".
        let empty = std::collections::BTreeMap::new();
        let (label, _) = crate::render::pane::mode_segment("custom", &theme, &empty);
        assert_eq!(label, "---");
    }

    fn theme_from_src(src: &str) -> crate::theme::Theme {
        let (config, errors) = stoat_config::parse(src);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        let config = config.expect("expected successful parse");
        crate::theme::Theme::from_config(&config, "t").expect("theme load failed")
    }

    #[test]
    fn theme_per_mode_color_overrides_default() {
        let theme = theme_from_src(r#"theme t { ui.statusline.normal.fg = red; }"#);
        let badges = std::collections::BTreeMap::new();
        let (_, color) = crate::render::pane::mode_segment("normal", &theme, &badges);
        assert_eq!(color, ratatui::style::Color::Red);
    }

    #[test]
    fn theme_per_mode_color_for_user_defined_mode() {
        let theme = theme_from_src(r#"theme t { ui.statusline.custom.fg = magenta; }"#);
        let badges = std::collections::BTreeMap::new();
        let (_, color) = crate::render::pane::mode_segment("custom", &theme, &badges);
        assert_eq!(color, ratatui::style::Color::Magenta);
    }

    #[test]
    fn legacy_submode_scope_still_colors_all_submodes() {
        let theme = theme_from_src(r#"theme t { ui.statusline.submode.fg = cyan; }"#);
        let badges = std::collections::BTreeMap::new();
        for mode in [
            "goto",
            "z",
            "match",
            "space",
            "space_workspace",
            "space_pane_nav",
        ] {
            let (_, color) = crate::render::pane::mode_segment(mode, &theme, &badges);
            assert_eq!(
                color,
                ratatui::style::Color::Cyan,
                "submode `{mode}` should inherit the legacy submode color",
            );
        }
    }

    #[test]
    fn theme_per_mode_color_wins_over_legacy_submode_scope() {
        let theme = theme_from_src(
            r#"theme t {
                ui.statusline.submode.fg = cyan;
                ui.statusline.goto.fg = red;
            }"#,
        );
        let badges = std::collections::BTreeMap::new();
        let (_, goto_color) = crate::render::pane::mode_segment("goto", &theme, &badges);
        let (_, space_color) = crate::render::pane::mode_segment("space", &theme, &badges);
        assert_eq!(goto_color, ratatui::style::Color::Red);
        assert_eq!(space_color, ratatui::style::Color::Cyan);
    }

    #[test]
    fn submode_status_labels() {
        let theme = crate::theme::Theme::empty();
        let cases = [
            ("goto", "GTO"),
            ("z", "VWA"),
            ("bracket_next", "BNX"),
            ("bracket_prev", "BPV"),
            ("match", "MAT"),
            ("select_goto", "SLG"),
            ("space", "SPC"),
            ("space_workspace", "SWS"),
            ("space_pane_nav", "SPN"),
            ("space_pane_nav_new", "SNN"),
        ];
        let badges = std::collections::BTreeMap::new();
        for (mode, expected) in cases {
            let (label, _) = crate::render::pane::mode_segment(mode, &theme, &badges);
            assert_eq!(label, expected, "label for mode {mode:?}");
        }
    }

    /// A file's name is what someone typed. Sniffing the bytes is what makes an
    /// image with the wrong suffix, or a text file with an image's suffix, open
    /// as what it actually is.
    #[test]
    fn an_image_is_recognized_by_its_leading_bytes() {
        for (name, bytes, image) in [
            ("png", b"\x89PNG\r\n\x1a\nrest".as_slice(), true),
            ("jpeg", b"\xff\xd8\xff\xe0rest".as_slice(), true),
            ("gif87", b"GIF87arest".as_slice(), true),
            ("gif89", b"GIF89arest".as_slice(), true),
            ("bmp", b"BMrest".as_slice(), true),
            ("webp", b"RIFF\0\0\0\0WEBPrest".as_slice(), true),
            (
                "riff that is not webp",
                b"RIFF\0\0\0\0WAVErest".as_slice(),
                false,
            ),
            ("text", b"hello".as_slice(), false),
            ("empty", b"".as_slice(), false),
            ("one byte of a png", b"\x89".as_slice(), false),
            ("riff cut short", b"RIFF\0\0\0".as_slice(), false),
        ] {
            assert_eq!(is_image(bytes), image, "{name}");
        }
    }

    /// A 2x3 PNG, built rather than pasted so the bytes and the size a test
    /// asserts cannot drift apart.
    fn png_2x3() -> Vec<u8> {
        let buffer = image::RgbaImage::from_pixel(2, 3, image::Rgba([1, 2, 3, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        buffer
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode png");
        out.into_inner()
    }

    #[test]
    fn reading_an_image_reports_its_size_and_reading_text_reports_its_text() {
        let fs = crate::host::FakeFs::new();
        fs.insert_file("/w/pic.png", png_2x3());
        fs.insert_file("/w/notes.txt", b"hello");
        // An image whose name says otherwise, which is the case a suffix check
        // gets wrong.
        fs.insert_file("/w/pic.txt", png_2x3());

        let read = |path: &str| match read_open_content(&fs, Path::new(path)) {
            Ok(OpenContent::Image { px }) => Ok(Some(px)),
            Ok(OpenContent::Text { text, compressed }) => {
                assert_eq!(
                    (text.as_str(), compressed),
                    ("hello", false),
                    "{path} read as text"
                );
                Ok(None)
            },
            Ok(OpenContent::Rope { .. }) => {
                unreachable!("the read hands back bytes; only the open path builds a rope")
            },
            Err(e) => Err(e.kind()),
        };

        assert_eq!(read("/w/pic.png"), Ok(Some((2, 3))));
        assert_eq!(read("/w/pic.txt"), Ok(Some((2, 3))), "the bytes decide");
        assert_eq!(read("/w/notes.txt"), Ok(None));
    }

    /// A file that is neither an image nor text still has to say what is wrong
    /// with it, which is the behavior this replaced for images only.
    #[test]
    fn a_file_that_is_neither_still_reports_bad_data() {
        let fs = crate::host::FakeFs::new();
        fs.insert_file("/w/bin", [0xff, 0xfe, 0x00, 0x80]);

        assert_eq!(
            read_open_content(&fs, Path::new("/w/bin"))
                .err()
                .map(|e| e.kind()),
            Some(std::io::ErrorKind::InvalidData),
        );
    }
}

use crate::{
    keymap::compiled::{action_first_string_arg, action_name},
    stoat::{KeyContext, Stoat, StoatEvent},
};
use gpui::{AppContext, Entity};
use stoat_config::ActionExpr;

/// Dispatch an editor-level action directly on the [`Stoat`] entity.
///
/// Handles movement, editing, selection, mode transitions, diff review,
/// and file write actions. Returns `true` if the action was recognized
/// and dispatched.
pub fn dispatch_editor_action<C: AppContext>(
    stoat: &Entity<Stoat>,
    action: &ActionExpr,
    cx: &mut C,
) -> bool {
    let name = action_name(action);

    macro_rules! ed {
        ($stoat:ident, $cx:ident, |$s:ident, $c:ident| $body:expr) => {{
            let _ = $stoat.update($cx, |$s, $c| $body);
        }};
    }

    match name {
        "MoveLeft" => ed!(stoat, cx, |s, cx| s.move_left(cx)),
        "MoveRight" => ed!(stoat, cx, |s, cx| s.move_right(cx)),
        "MoveUp" => ed!(stoat, cx, |s, cx| s.move_up(cx)),
        "MoveDown" => ed!(stoat, cx, |s, cx| s.move_down(cx)),
        "MoveWordLeft" => ed!(stoat, cx, |s, cx| s.move_word_left(cx)),
        "MoveWordRight" => ed!(stoat, cx, |s, cx| s.move_word_right(cx)),
        "MoveToLineStart" => ed!(stoat, cx, |s, cx| s.move_to_line_start(cx)),
        "MoveToLineEnd" => ed!(stoat, cx, |s, cx| s.move_to_line_end(cx)),
        "MoveToFileStart" => ed!(stoat, cx, |s, cx| s.move_to_file_start(cx)),
        "MoveToFileEnd" => ed!(stoat, cx, |s, cx| s.move_to_file_end(cx)),
        "PageUp" => ed!(stoat, cx, |s, cx| s.page_up(cx)),
        "PageDown" => ed!(stoat, cx, |s, cx| s.page_down(cx)),
        "MoveNextWordEnd" => ed!(stoat, cx, |s, cx| s.move_next_word_end(cx)),
        "MoveNextLongWordEnd" => ed!(stoat, cx, |s, cx| s.move_next_long_word_end(cx)),
        "FindCharForward" => ed!(stoat, cx, |s, cx| s.find_char_forward(cx)),
        "FindCharBackward" => ed!(stoat, cx, |s, cx| s.find_char_backward(cx)),
        "TillCharForward" => ed!(stoat, cx, |s, cx| s.till_char_forward(cx)),
        "TillCharBackward" => ed!(stoat, cx, |s, cx| s.till_char_backward(cx)),
        "MoveToFirstNonWhitespace" => {
            ed!(stoat, cx, |s, cx| s.move_to_first_non_whitespace(cx))
        },
        "HalfPageUp" => ed!(stoat, cx, |s, cx| s.half_page_up(cx)),
        "HalfPageDown" => ed!(stoat, cx, |s, cx| s.half_page_down(cx)),

        "DeleteLeft" => ed!(stoat, cx, |s, cx| s.delete_left(cx)),
        "DeleteRight" => ed!(stoat, cx, |s, cx| s.delete_right(cx)),
        "DeleteWordLeft" => ed!(stoat, cx, |s, cx| s.delete_word_left(cx)),
        "DeleteWordRight" => ed!(stoat, cx, |s, cx| s.delete_word_right(cx)),
        "NewLine" => ed!(stoat, cx, |s, cx| s.new_line(cx)),
        "DeleteLine" => ed!(stoat, cx, |s, cx| s.delete_line(cx)),
        "DeleteToEndOfLine" => ed!(stoat, cx, |s, cx| s.delete_to_end_of_line(cx)),
        "Undo" => ed!(stoat, cx, |s, cx| s.undo(cx)),
        "Redo" => ed!(stoat, cx, |s, cx| s.redo(cx)),
        "UndoSelection" => ed!(stoat, cx, |s, cx| s.undo_selection(cx)),
        "RedoSelection" => ed!(stoat, cx, |s, cx| s.redo_selection(cx)),
        "UndoState" => ed!(stoat, cx, |s, cx| s.undo_state(cx)),
        "RedoState" => ed!(stoat, cx, |s, cx| s.redo_state(cx)),

        "OpenLineBelow" => ed!(stoat, cx, |s, cx| s.open_line_below(cx)),
        "OpenLineAbove" => ed!(stoat, cx, |s, cx| s.open_line_above(cx)),
        "Append" => ed!(stoat, cx, |s, cx| s.append(cx)),
        "AppendAtLineEnd" => ed!(stoat, cx, |s, cx| s.append_at_line_end(cx)),
        "InsertAtLineStart" => ed!(stoat, cx, |s, cx| s.insert_at_line_start(cx)),
        "DeleteSelection" => ed!(stoat, cx, |s, cx| s.delete_selection(cx)),
        "ChangeSelection" => ed!(stoat, cx, |s, cx| s.change_selection(cx)),
        "SelectAll" => ed!(stoat, cx, |s, cx| s.select_all(cx)),
        "Yank" => ed!(stoat, cx, |s, cx| s.yank(cx)),
        "PasteAfter" => ed!(stoat, cx, |s, cx| s.paste_after(cx)),
        "PasteBefore" => ed!(stoat, cx, |s, cx| s.paste_before(cx)),
        "JoinLines" => ed!(stoat, cx, |s, cx| s.join_lines(cx)),
        "Indent" => ed!(stoat, cx, |s, cx| s.indent(cx)),
        "Outdent" => ed!(stoat, cx, |s, cx| s.outdent(cx)),
        "Lowercase" => ed!(stoat, cx, |s, cx| s.lowercase(cx)),
        "Uppercase" => ed!(stoat, cx, |s, cx| s.uppercase(cx)),
        "SwapCase" => ed!(stoat, cx, |s, cx| s.swap_case(cx)),
        "ReplaceChar" => ed!(stoat, cx, |s, cx| s.replace_char(cx)),

        "MoveNextWordStart" => ed!(stoat, cx, |s, cx| s.move_next_word_start(cx)),
        "MovePrevWordStart" => ed!(stoat, cx, |s, cx| s.move_prev_word_start(cx)),
        "MoveNextLongWordStart" => ed!(stoat, cx, |s, cx| s.move_next_long_word_start(cx)),
        "MovePrevLongWordStart" => ed!(stoat, cx, |s, cx| s.move_prev_long_word_start(cx)),
        "SelectLeft" => ed!(stoat, cx, |s, cx| s.select_left(cx)),
        "SelectRight" => ed!(stoat, cx, |s, cx| s.select_right(cx)),
        "SelectUp" => ed!(stoat, cx, |s, cx| s.select_up(cx)),
        "SelectDown" => ed!(stoat, cx, |s, cx| s.select_down(cx)),
        "SelectToLineStart" => ed!(stoat, cx, |s, cx| s.select_to_line_start(cx)),
        "SelectToLineEnd" => ed!(stoat, cx, |s, cx| s.select_to_line_end(cx)),
        "SplitSelectionIntoLines" => ed!(stoat, cx, |s, cx| s.split_selection_into_lines(cx)),
        "SelectNext" => ed!(stoat, cx, |s, cx| s.select_next(cx)),
        "SelectPrevious" => ed!(stoat, cx, |s, cx| s.select_previous(cx)),
        "SelectAllMatches" => ed!(stoat, cx, |s, cx| s.select_all_matches(cx)),
        "AddSelectionAbove" => ed!(stoat, cx, |s, cx| s.add_selection_above(cx)),
        "AddSelectionBelow" => ed!(stoat, cx, |s, cx| s.add_selection_below(cx)),
        "CollapseSelection" => ed!(stoat, cx, |s, cx| s.collapse_selection(cx)),
        "KeepPrimarySelection" => ed!(stoat, cx, |s, cx| s.keep_primary_selection(cx)),
        "FlipSelection" => ed!(stoat, cx, |s, cx| s.flip_selection(cx)),
        "ExtendNextWordEnd" => ed!(stoat, cx, |s, cx| s.extend_next_word_end(cx)),
        "ExtendNextLongWordEnd" => ed!(stoat, cx, |s, cx| s.extend_next_long_word_end(cx)),
        "SelectLine" => ed!(stoat, cx, |s, cx| s.select_line(cx)),
        "SelectRegex" => ed!(stoat, cx, |s, cx| s.select_regex(cx)),

        "SetMode" => {
            if let Some(mode_name) = action_first_string_arg(action) {
                let _ = stoat.update(cx, |s, cx| s.set_mode_by_name(&mode_name, cx));
            }
        },
        "EnterInsertMode" => ed!(stoat, cx, |s, cx| s.enter_insert_mode(cx)),
        "EnterNormalMode" => ed!(stoat, cx, |s, cx| s.enter_normal_mode(cx)),
        "EnterVisualMode" => ed!(stoat, cx, |s, cx| s.enter_visual_mode(cx)),
        "EnterSpaceMode" => ed!(stoat, cx, |s, cx| s.enter_space_mode(cx)),
        "EnterPaneMode" => ed!(stoat, cx, |s, cx| s.enter_pane_mode(cx)),
        "EnterGitFilterMode" => ed!(stoat, cx, |s, cx| s.enter_git_filter_mode(cx)),

        "SetKeyContext" => {
            if let Some(ctx_name) = action_first_string_arg(action) {
                if let Ok(key_context) = KeyContext::from_str(&ctx_name) {
                    let _ = stoat.update(cx, |s, cx| s.handle_set_key_context(key_context, cx));
                }
            }
        },

        "ToggleDiffHunk" => ed!(stoat, cx, |s, cx| s.toggle_diff_hunk(cx)),
        "GotoNextHunk" => ed!(stoat, cx, |s, cx| s.goto_next_hunk(cx)),
        "GotoPrevHunk" => ed!(stoat, cx, |s, cx| s.goto_prev_hunk(cx)),

        "DiffReviewNextHunk" => ed!(stoat, cx, |s, cx| s.diff_review_next_hunk(cx)),
        "DiffReviewPrevHunk" => ed!(stoat, cx, |s, cx| s.diff_review_prev_hunk(cx)),
        "DiffReviewApproveHunk" => ed!(stoat, cx, |s, cx| s.diff_review_approve_hunk(cx)),
        "DiffReviewToggleApproval" => ed!(stoat, cx, |s, cx| s.diff_review_toggle_approval(cx)),
        "DiffReviewNextUnreviewedHunk" => {
            ed!(stoat, cx, |s, cx| s.diff_review_next_unreviewed_hunk(cx))
        },
        "DiffReviewResetProgress" => ed!(stoat, cx, |s, cx| s.diff_review_reset_progress(cx)),
        "DiffReviewDismiss" => ed!(stoat, cx, |s, cx| s.diff_review_dismiss(cx)),
        "DiffReviewCycleComparisonMode" => {
            ed!(stoat, cx, |s, cx| s.diff_review_cycle_comparison_mode(cx))
        },
        "DiffReviewPreviousCommit" => {
            ed!(stoat, cx, |s, cx| s.diff_review_previous_commit(cx))
        },
        "DiffReviewRevertHunk" => {
            let _ = stoat.update(cx, |s, cx| {
                if let Err(e) = s.diff_review_revert_hunk(cx) {
                    tracing::error!("DiffReviewRevertHunk failed: {e}");
                }
            });
        },
        "DiffReviewToggleFollow" => ed!(stoat, cx, |s, cx| s.diff_review_toggle_follow(cx)),

        "DiffReviewEnterLineSelect" => {
            ed!(stoat, cx, |s, cx| s.diff_review_enter_line_select(cx))
        },
        "DiffReviewLineSelectToggle" => {
            ed!(stoat, cx, |s, cx| s.diff_review_line_select_toggle(cx))
        },
        "DiffReviewLineSelectAll" => {
            ed!(stoat, cx, |s, cx| s.diff_review_line_select_all(cx))
        },
        "DiffReviewLineSelectNone" => {
            ed!(stoat, cx, |s, cx| s.diff_review_line_select_none(cx))
        },
        "DiffReviewLineSelectStage" => {
            ed!(stoat, cx, |s, cx| s.diff_review_line_select_stage(cx))
        },
        "DiffReviewLineSelectUnstage" => {
            ed!(stoat, cx, |s, cx| s.diff_review_line_select_unstage(cx))
        },
        "DiffReviewLineSelectCancel" => {
            ed!(stoat, cx, |s, cx| s.diff_review_line_select_cancel(cx))
        },
        "DiffReviewLineSelectDown" => {
            ed!(stoat, cx, |s, cx| s.diff_review_line_select_down(cx))
        },
        "DiffReviewLineSelectUp" => {
            ed!(stoat, cx, |s, cx| s.diff_review_line_select_up(cx))
        },

        "GitStageHunk" => {
            let _ = stoat.update(cx, |s, cx| {
                if let Err(e) = s.git_stage_hunk(cx) {
                    tracing::error!("GitStageHunk failed: {e}");
                }
            });
        },
        "GitUnstageHunk" => {
            let _ = stoat.update(cx, |s, cx| {
                if let Err(e) = s.git_unstage_hunk(cx) {
                    tracing::error!("GitUnstageHunk failed: {e}");
                }
            });
        },
        "GitToggleStageHunk" => {
            let _ = stoat.update(cx, |s, cx| {
                if let Err(e) = s.git_toggle_stage_hunk(cx) {
                    tracing::error!("GitToggleStageHunk failed: {e}");
                }
            });
        },
        "GitToggleStageLine" => {
            let _ = stoat.update(cx, |s, cx| {
                if let Err(e) = s.git_toggle_stage_line(cx) {
                    tracing::error!("GitToggleStageLine failed: {e}");
                }
            });
        },

        "ConflictAcceptOurs" => ed!(stoat, cx, |s, cx| s.conflict_accept_ours(cx)),
        "ConflictAcceptTheirs" => ed!(stoat, cx, |s, cx| s.conflict_accept_theirs(cx)),
        "ConflictAcceptBoth" => ed!(stoat, cx, |s, cx| s.conflict_accept_both(cx)),
        "ConflictNextConflict" => ed!(stoat, cx, |s, cx| s.conflict_next_conflict(cx)),
        "ConflictPrevConflict" => ed!(stoat, cx, |s, cx| s.conflict_prev_conflict(cx)),
        "ConflictReviewDismiss" => ed!(stoat, cx, |s, cx| s.conflict_review_dismiss(cx)),
        "ConflictToggleView" => ed!(stoat, cx, |s, cx| s.conflict_toggle_view(cx)),

        "WriteFile" | "Save" => {
            let _ = stoat.update(cx, |s, cx| {
                if let Err(e) = s.write_file(cx) {
                    tracing::error!("WriteFile failed: {}", e);
                }
            });
        },
        "WriteAll" => {
            let _ = stoat.update(cx, |s, cx| {
                if let Err(e) = s.write_all(cx) {
                    tracing::error!("WriteAll failed: {}", e);
                }
            });
        },

        _ => return false,
    }

    true
}

/// Dispatch a pane-level action by emitting [`StoatEvent::Action`].
///
/// Handles actions that require the [`PaneGroupView`] layer: pane management,
/// finders, command palette, git status, help/about modals, and application
/// quit. Returns `true` if the action was recognized.
pub fn dispatch_pane_action<C: AppContext>(
    stoat: &Entity<Stoat>,
    action: &ActionExpr,
    cx: &mut C,
) -> bool {
    let name = action_name(action);

    let is_pane_action = matches!(
        name,
        // Pane management
        "SplitUp"
            | "SplitDown"
            | "SplitLeft"
            | "SplitRight"
            | "Quit"
            | "ClosePane"
            | "FocusPaneUp"
            | "FocusPaneDown"
            | "FocusPaneLeft"
            | "FocusPaneRight"
            | "CloseBuffer"
            | "CloseOtherPanes"
            // Finders
            | "OpenFileFinder"
            | "FileFinderNext"
            | "FileFinderPrev"
            | "FileFinderSelect"
            | "FileFinderDismiss"
            | "OpenBufferFinder"
            | "BufferFinderNext"
            | "BufferFinderPrev"
            | "BufferFinderSelect"
            | "BufferFinderDismiss"
            // Command palette
            | "OpenCommandPalette"
            | "CommandPaletteNext"
            | "CommandPalettePrev"
            | "CommandPaletteExecute"
            | "CommandPaletteDismiss"
            | "ToggleCommandPaletteHidden"
            | "OpenCommandPaletteV2"
            | "DismissCommandPaletteV2"
            | "AcceptCommandPaletteV2"
            | "SelectNextCommandV2"
            | "SelectPrevCommandV2"
            // Git status
            | "OpenGitStatus"
            | "GitStatusNext"
            | "GitStatusPrev"
            | "GitStatusSelect"
            | "GitStatusDismiss"
            | "GitStatusCycleFilter"
            | "GitStatusSetFilterAll"
            | "GitStatusSetFilterStaged"
            | "GitStatusSetFilterUnstaged"
            | "GitStatusSetFilterUnstagedWithUntracked"
            | "GitStatusSetFilterUntracked"
            // Help/About
            | "OpenHelpOverlay"
            | "OpenHelpModal"
            | "HelpModalDismiss"
            | "OpenAboutModal"
            | "AboutModalDismiss"
            // View
            | "ToggleMinimap"
            | "ShowMinimapOnScroll"
            // Diff review (open from pane level)
            | "OpenDiffReview"
            // Conflict review (open from pane level)
            | "OpenConflictReview"
            // Command line
            | "ShowCommandLine"
            | "CommandLineDismiss"
            | "ChangeDirectory"
            | "PrintWorkingDirectory"
            // Application
            | "QuitAll"
    );

    if !is_pane_action {
        return false;
    }

    let action_name = name.to_string();
    let args: Vec<String> = action_first_string_arg(action).into_iter().collect();

    let _ = stoat.update(cx, |_, cx| {
        cx.emit(StoatEvent::Action {
            name: action_name,
            args,
        });
    });

    true
}

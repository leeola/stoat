//! Command dispatch by TypeId
//!
//! Provides type-safe dispatch of actions from TypeId, enabling the command palette
//! to execute arbitrary commands selected by the user.

use crate::{actions::*, editor_view::EditorView};
use gpui::{Context, Window};
use std::any::TypeId;

/// Dispatch a command by its TypeId.
///
/// This function maps from a TypeId (obtained from the command palette) to the
/// concrete action type and dispatches it via the window. This enables type-safe
/// execution of dynamically selected commands.
///
/// # Arguments
///
/// * `type_id` - The TypeId of the action to dispatch
/// * `window` - The GPUI window context
/// * `cx` - The EditorView context
///
/// # Implementation Note
///
/// This function requires an explicit match for every action type. This is necessary
/// because Rust's type system doesn't allow constructing trait objects from TypeId alone.
/// While verbose, this approach is type-safe and compile-time checked.
pub fn dispatch_command_by_type_id(
    type_id: TypeId,
    window: &mut Window,
    cx: &mut Context<'_, EditorView>,
) {
    // Movement actions
    if type_id == TypeId::of::<MoveLeft>() {
        window.dispatch_action(Box::new(MoveLeft), cx);
    } else if type_id == TypeId::of::<MoveRight>() {
        window.dispatch_action(Box::new(MoveRight), cx);
    } else if type_id == TypeId::of::<MoveUp>() {
        window.dispatch_action(Box::new(MoveUp), cx);
    } else if type_id == TypeId::of::<MoveDown>() {
        window.dispatch_action(Box::new(MoveDown), cx);
    } else if type_id == TypeId::of::<MoveWordLeft>() {
        window.dispatch_action(Box::new(MoveWordLeft), cx);
    } else if type_id == TypeId::of::<MoveWordRight>() {
        window.dispatch_action(Box::new(MoveWordRight), cx);
    } else if type_id == TypeId::of::<MoveToLineStart>() {
        window.dispatch_action(Box::new(MoveToLineStart), cx);
    } else if type_id == TypeId::of::<MoveToLineEnd>() {
        window.dispatch_action(Box::new(MoveToLineEnd), cx);
    } else if type_id == TypeId::of::<MoveToFileStart>() {
        window.dispatch_action(Box::new(MoveToFileStart), cx);
    } else if type_id == TypeId::of::<MoveToFileEnd>() {
        window.dispatch_action(Box::new(MoveToFileEnd), cx);
    } else if type_id == TypeId::of::<PageUp>() {
        window.dispatch_action(Box::new(PageUp), cx);
    } else if type_id == TypeId::of::<PageDown>() {
        window.dispatch_action(Box::new(PageDown), cx);
    }
    // Edit actions
    else if type_id == TypeId::of::<DeleteLeft>() {
        window.dispatch_action(Box::new(DeleteLeft), cx);
    } else if type_id == TypeId::of::<DeleteRight>() {
        window.dispatch_action(Box::new(DeleteRight), cx);
    } else if type_id == TypeId::of::<DeleteWordLeft>() {
        window.dispatch_action(Box::new(DeleteWordLeft), cx);
    } else if type_id == TypeId::of::<DeleteWordRight>() {
        window.dispatch_action(Box::new(DeleteWordRight), cx);
    } else if type_id == TypeId::of::<NewLine>() {
        window.dispatch_action(Box::new(NewLine), cx);
    } else if type_id == TypeId::of::<DeleteLine>() {
        window.dispatch_action(Box::new(DeleteLine), cx);
    } else if type_id == TypeId::of::<DeleteToEndOfLine>() {
        window.dispatch_action(Box::new(DeleteToEndOfLine), cx);
    }
    // Selection actions
    else if type_id == TypeId::of::<SelectNextSymbol>() {
        window.dispatch_action(Box::new(SelectNextSymbol), cx);
    } else if type_id == TypeId::of::<SelectPrevSymbol>() {
        window.dispatch_action(Box::new(SelectPrevSymbol), cx);
    } else if type_id == TypeId::of::<SelectNextToken>() {
        window.dispatch_action(Box::new(SelectNextToken), cx);
    } else if type_id == TypeId::of::<SelectPrevToken>() {
        window.dispatch_action(Box::new(SelectPrevToken), cx);
    } else if type_id == TypeId::of::<SelectLeft>() {
        window.dispatch_action(Box::new(SelectLeft), cx);
    } else if type_id == TypeId::of::<SelectRight>() {
        window.dispatch_action(Box::new(SelectRight), cx);
    } else if type_id == TypeId::of::<SelectUp>() {
        window.dispatch_action(Box::new(SelectUp), cx);
    } else if type_id == TypeId::of::<SelectDown>() {
        window.dispatch_action(Box::new(SelectDown), cx);
    } else if type_id == TypeId::of::<SelectToLineStart>() {
        window.dispatch_action(Box::new(SelectToLineStart), cx);
    } else if type_id == TypeId::of::<SelectToLineEnd>() {
        window.dispatch_action(Box::new(SelectToLineEnd), cx);
    }
    // Mode actions
    else if type_id == TypeId::of::<EnterInsertMode>() {
        window.dispatch_action(Box::new(EnterInsertMode), cx);
    } else if type_id == TypeId::of::<EnterNormalMode>() {
        window.dispatch_action(Box::new(EnterNormalMode), cx);
    } else if type_id == TypeId::of::<EnterVisualMode>() {
        window.dispatch_action(Box::new(EnterVisualMode), cx);
    } else if type_id == TypeId::of::<EnterSpaceMode>() {
        window.dispatch_action(Box::new(EnterSpaceMode), cx);
    } else if type_id == TypeId::of::<EnterPaneMode>() {
        window.dispatch_action(Box::new(EnterPaneMode), cx);
    } else if type_id == TypeId::of::<EnterGitFilterMode>() {
        window.dispatch_action(Box::new(EnterGitFilterMode), cx);
    }
    // File finder actions
    else if type_id == TypeId::of::<OpenFileFinder>() {
        window.dispatch_action(Box::new(OpenFileFinder), cx);
    } else if type_id == TypeId::of::<FileFinderNext>() {
        window.dispatch_action(Box::new(FileFinderNext), cx);
    } else if type_id == TypeId::of::<FileFinderPrev>() {
        window.dispatch_action(Box::new(FileFinderPrev), cx);
    } else if type_id == TypeId::of::<FileFinderDismiss>() {
        window.dispatch_action(Box::new(FileFinderDismiss), cx);
    } else if type_id == TypeId::of::<FileFinderSelect>() {
        window.dispatch_action(Box::new(FileFinderSelect), cx);
    }
    // Buffer finder actions
    else if type_id == TypeId::of::<OpenBufferFinder>() {
        window.dispatch_action(Box::new(OpenBufferFinder), cx);
    } else if type_id == TypeId::of::<BufferFinderNext>() {
        window.dispatch_action(Box::new(BufferFinderNext), cx);
    } else if type_id == TypeId::of::<BufferFinderPrev>() {
        window.dispatch_action(Box::new(BufferFinderPrev), cx);
    } else if type_id == TypeId::of::<BufferFinderDismiss>() {
        window.dispatch_action(Box::new(BufferFinderDismiss), cx);
    } else if type_id == TypeId::of::<BufferFinderSelect>() {
        window.dispatch_action(Box::new(BufferFinderSelect), cx);
    }
    // Command palette actions
    else if type_id == TypeId::of::<OpenCommandPalette>() {
        window.dispatch_action(Box::new(OpenCommandPalette), cx);
    } else if type_id == TypeId::of::<CommandPaletteNext>() {
        window.dispatch_action(Box::new(CommandPaletteNext), cx);
    } else if type_id == TypeId::of::<CommandPalettePrev>() {
        window.dispatch_action(Box::new(CommandPalettePrev), cx);
    } else if type_id == TypeId::of::<CommandPaletteDismiss>() {
        window.dispatch_action(Box::new(CommandPaletteDismiss), cx);
    } else if type_id == TypeId::of::<ToggleCommandPaletteHidden>() {
        window.dispatch_action(Box::new(ToggleCommandPaletteHidden), cx);
    }
    // Git status actions
    else if type_id == TypeId::of::<OpenGitStatus>() {
        window.dispatch_action(Box::new(OpenGitStatus), cx);
    } else if type_id == TypeId::of::<GitStatusNext>() {
        window.dispatch_action(Box::new(GitStatusNext), cx);
    } else if type_id == TypeId::of::<GitStatusPrev>() {
        window.dispatch_action(Box::new(GitStatusPrev), cx);
    } else if type_id == TypeId::of::<GitStatusDismiss>() {
        window.dispatch_action(Box::new(GitStatusDismiss), cx);
    } else if type_id == TypeId::of::<GitStatusSelect>() {
        window.dispatch_action(Box::new(GitStatusSelect), cx);
    } else if type_id == TypeId::of::<GitStatusCycleFilter>() {
        window.dispatch_action(Box::new(GitStatusCycleFilter), cx);
    } else if type_id == TypeId::of::<GitStatusSetFilterAll>() {
        window.dispatch_action(Box::new(GitStatusSetFilterAll), cx);
    } else if type_id == TypeId::of::<GitStatusSetFilterStaged>() {
        window.dispatch_action(Box::new(GitStatusSetFilterStaged), cx);
    } else if type_id == TypeId::of::<GitStatusSetFilterUnstaged>() {
        window.dispatch_action(Box::new(GitStatusSetFilterUnstaged), cx);
    } else if type_id == TypeId::of::<GitStatusSetFilterUnstagedWithUntracked>() {
        window.dispatch_action(Box::new(GitStatusSetFilterUnstagedWithUntracked), cx);
    } else if type_id == TypeId::of::<GitStatusSetFilterUntracked>() {
        window.dispatch_action(Box::new(GitStatusSetFilterUntracked), cx);
    }
    // Git diff hunk actions
    else if type_id == TypeId::of::<ToggleDiffHunk>() {
        window.dispatch_action(Box::new(ToggleDiffHunk), cx);
    } else if type_id == TypeId::of::<GotoNextHunk>() {
        window.dispatch_action(Box::new(GotoNextHunk), cx);
    } else if type_id == TypeId::of::<GotoPrevHunk>() {
        window.dispatch_action(Box::new(GotoPrevHunk), cx);
    }
    // Diff review actions
    else if type_id == TypeId::of::<OpenDiffReview>() {
        window.dispatch_action(Box::new(OpenDiffReview), cx);
    } else if type_id == TypeId::of::<DiffReviewNextHunk>() {
        window.dispatch_action(Box::new(DiffReviewNextHunk), cx);
    } else if type_id == TypeId::of::<DiffReviewPrevHunk>() {
        window.dispatch_action(Box::new(DiffReviewPrevHunk), cx);
    } else if type_id == TypeId::of::<DiffReviewApproveHunk>() {
        window.dispatch_action(Box::new(DiffReviewApproveHunk), cx);
    } else if type_id == TypeId::of::<DiffReviewToggleApproval>() {
        window.dispatch_action(Box::new(DiffReviewToggleApproval), cx);
    } else if type_id == TypeId::of::<DiffReviewNextUnreviewedHunk>() {
        window.dispatch_action(Box::new(DiffReviewNextUnreviewedHunk), cx);
    } else if type_id == TypeId::of::<DiffReviewResetProgress>() {
        window.dispatch_action(Box::new(DiffReviewResetProgress), cx);
    } else if type_id == TypeId::of::<DiffReviewDismiss>() {
        window.dispatch_action(Box::new(DiffReviewDismiss), cx);
    }
    // Help actions
    else if type_id == TypeId::of::<OpenHelpOverlay>() {
        window.dispatch_action(Box::new(OpenHelpOverlay), cx);
    } else if type_id == TypeId::of::<OpenHelpModal>() {
        window.dispatch_action(Box::new(OpenHelpModal), cx);
    } else if type_id == TypeId::of::<HelpModalDismiss>() {
        window.dispatch_action(Box::new(HelpModalDismiss), cx);
    }
    // About actions
    else if type_id == TypeId::of::<OpenAboutModal>() {
        window.dispatch_action(Box::new(OpenAboutModal), cx);
    } else if type_id == TypeId::of::<AboutModalDismiss>() {
        window.dispatch_action(Box::new(AboutModalDismiss), cx);
    }
    // View actions
    else if type_id == TypeId::of::<ToggleMinimap>() {
        window.dispatch_action(Box::new(ToggleMinimap), cx);
    } else if type_id == TypeId::of::<ShowMinimapOnScroll>() {
        window.dispatch_action(Box::new(ShowMinimapOnScroll), cx);
    }
    // File actions
    else if type_id == TypeId::of::<WriteFile>() {
        window.dispatch_action(Box::new(WriteFile), cx);
    } else if type_id == TypeId::of::<WriteAll>() {
        window.dispatch_action(Box::new(WriteAll), cx);
    }
    // Pane actions
    else if type_id == TypeId::of::<Quit>() {
        window.dispatch_action(Box::new(Quit), cx);
    }
    // Application actions
    else if type_id == TypeId::of::<QuitAll>() {
        window.dispatch_action(Box::new(QuitAll), cx);
    }
    // Add more actions as they become available
}

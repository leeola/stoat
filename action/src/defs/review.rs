use crate::{
    action::{define_action, define_action_def},
    Action, ActionDef, ActionKind, ActionPriority, ParamDef, ParamKind, ValueSource,
};
use std::{any::Any, path::PathBuf};

const DIFF_PARAMS: &[ParamDef] = &[ParamDef {
    name: "rev",
    kind: ParamKind::String,
    value_source: ValueSource::None,
    required: false,
    description: "Branch, tag, sha, or revspec to diff against. Without it the working tree \
                  diffs against its own base. `index` and `HEAD` name the working tree's two \
                  bases.",
}];

define_action_def!(
    DiffDef,
    "Diff",
    ActionKind::Diff,
    "open a diff of working-tree changes",
    "Open the first changed file with a structural diff against the working \
     tree's own base, or against the given revision. That base is the index, \
     with what the index holds over HEAD marked staged in the left column, \
     unless DiffAgainstHead picked HEAD. A revision points the whole \
     workspace at that commit, so every file diffs against it and the change \
     list spans everything committed since. `index` and `HEAD` name the \
     working tree's two bases rather than revisions. Running it again closes \
     the diff, and a revision gives way to the working tree's base. While the \
     diff is open, the plain wheel scrolls until the change under the cursor \
     passes the editor.diff_wheel_jump line. The next notch then walks to \
     the next change, and a notch up walks back. DiffWheelWalk turns that \
     walk off and on.",
    ActionPriority::Common,
    params = DIFF_PARAMS
);

#[derive(Debug)]
pub struct Diff {
    /// Revision to diff against, or `None` for the working tree's own base.
    pub rev: Option<String>,
}

impl Diff {
    pub const DEF: &DiffDef = &DiffDef;
}

impl Action for Diff {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

define_action!(
    DiffWheelWalkDef,
    DiffWheelWalk,
    "DiffWheelWalk",
    ActionKind::DiffWheelWalk,
    "toggle the wheel walk in the diff view",
    "Toggle the wheel walk in the diff view. While on, the plain wheel \
     scrolls until the change under the cursor passes the \
     editor.diff_wheel_jump line, and the next notch walks to the next \
     change. While off, the wheel scrolls the pane as in any other editor, \
     and the change keys and Alt-wheel still walk. A second run turns the \
     walk back on. The walk lasts for the session and starts on.",
    ActionPriority::Normal,
    command_name = "diff-wheel-walk"
);

define_action!(
    ChangeWalkWrapDef,
    ChangeWalkWrap,
    "ChangeWalkWrap",
    ActionKind::ChangeWalkWrap,
    "toggle the change walk's wrap past the ends",
    "Toggle whether the change walk wraps round. While on, a step past the \
     last changed file lands on the first and a step before the first lands \
     on the last, with the status \"wrapped\". While off, the walk stops at \
     either end with the status \"no more changes\". A second run turns the \
     wrap back on. The wrap lasts for the session and starts on.",
    ActionPriority::Normal,
    command_name = "change-walk-wrap"
);

define_action!(
    DiffUnderlineDef,
    DiffUnderline,
    "DiffUnderline",
    ActionKind::DiffUnderline,
    "toggle underline on diff change spans",
    "Toggle the underline on every change span in the diff view and on the \
     commits screen. While off, a span underlines only on a theme that does \
     not blend and on a changed part of a string or a comment. The dial \
     lasts for the session and starts off. The shipped chord is Ctrl-6, or \
     Cmd-6 on macOS.",
    ActionPriority::Normal,
    command_name = "diff-underline"
);

define_action!(
    DiffBoldDef,
    DiffBold,
    "DiffBold",
    ActionKind::DiffBold,
    "toggle bold on diff change spans",
    "Toggle the bold on every change span in the diff view and on the \
     commits screen. While off, only a replacement inside a string, a \
     comment, or a file with no grammar bolds. The dial lasts for the \
     session and starts off. The shipped chord is Ctrl-7, or Cmd-7 on macOS.",
    ActionPriority::Normal,
    command_name = "diff-bold"
);

define_action!(
    DiffSyntaxDef,
    DiffSyntax,
    "DiffSyntax",
    ActionKind::DiffSyntax,
    "toggle syntax colors in the diff view",
    "Toggle the syntax colors in both columns of the diff view and on the \
     commits screen. While off, the receding and the bold are the only marks \
     of a change. The dial lasts for the session and starts on. The shipped \
     chord is Ctrl-8, or Cmd-8 on macOS.",
    ActionPriority::Normal,
    command_name = "diff-syntax"
);

define_action!(
    DiffTintDownDef,
    DiffTintDown,
    "DiffTintDown",
    ActionKind::DiffTintDown,
    "lower the diff tint",
    "Lower the diff tint by one level in the diff view and on the commits \
     screen. The tint shifts a changed row toward its status color and \
     drains the color out of the rows around it. Level 0 is off, and a step \
     below it holds there. The level lasts for the session and starts off. \
     The shipped chord is Ctrl-9, or Cmd-9 on macOS.",
    ActionPriority::Normal,
    command_name = "diff-tint-down"
);

define_action!(
    DiffTintUpDef,
    DiffTintUp,
    "DiffTintUp",
    ActionKind::DiffTintUp,
    "raise the diff tint",
    "Raise the diff tint by one level in the diff view and on the commits \
     screen. The tint shifts a changed row toward its status color and \
     drains the color out of the rows around it. A step past the top level \
     holds there. The level lasts for the session and starts off. The \
     shipped chord is Ctrl-0, or Cmd-0 on macOS.",
    ActionPriority::Normal,
    command_name = "diff-tint-up"
);

define_action!(
    DiffAgainstIndexDef,
    DiffAgainstIndex,
    "DiffAgainstIndex",
    ActionKind::DiffAgainstIndex,
    "diff the working tree against the index",
    "Open the diff view with the index as the working tree's base, which it \
     stays until DiffAgainstHead picks HEAD. The left column shows the staged \
     text, its gutter marks what the index holds over HEAD, and the right \
     column shows only unstaged edits. Under a revision or a commit review \
     this returns the diff to the working tree, and :review-done still \
     returns the checkout.",
    ActionPriority::Normal,
    command_name = "diff-against-index"
);

define_action!(
    DiffAgainstHeadDef,
    DiffAgainstHead,
    "DiffAgainstHead",
    ActionKind::DiffAgainstHead,
    "diff the working tree against HEAD",
    "Open the diff view with HEAD as the base, so the right column shows \
     every change since the commit, a staged hunk marked in the staged color \
     and an unstaged one in the unstaged color. The base holds after the \
     diff closes, so the gutter keeps marking against HEAD until \
     DiffAgainstIndex picks the index.",
    ActionPriority::Normal,
    command_name = "diff-against-head"
);

define_action!(
    DiffBaseToggleDef,
    DiffBaseToggle,
    "DiffBaseToggle",
    ActionKind::DiffBaseToggle,
    "switch the diff base between the index and HEAD",
    "Flip the base the working tree diffs against between the index and \
     HEAD, in the diff view or in a plain pane, where the gutter follows it. \
     The diff view stays as it is, open or closed. Under a revision, a commit \
     review, or an agent proposal the first run returns to the working \
     tree's base, and the next run flips it. The status line names both \
     sides.",
    ActionPriority::Normal,
    command_name = "diff-base-toggle"
);

define_action!(
    DiffBaseEditDef,
    DiffBaseEdit,
    "DiffBaseEdit",
    ActionKind::DiffBaseEdit,
    "edit what the diff compares against",
    "Open the command line holding the diff command and the current base, so \
     a retyped revision, `index`, or `HEAD` and Enter points the diff at it. \
     Escape leaves the base as it is.",
    ActionPriority::Normal,
    command_name = "diff-base-edit"
);

define_action!(
    DiffPairDef,
    DiffPair,
    "DiffPair",
    ActionKind::DiffPair,
    "diff the two open files",
    "Open the diff view over the two files that two panes show. The file in \
     the first pane, the left one or the top one, is the base in the left \
     column. The file in the other pane is the editable right column and \
     takes the focus, so each file stays on its side. Git does not have to \
     track either file. The left column follows the first file's buffer, so \
     an edit or a reload of that file shows after a short pause. The change \
     keys walk the differences and stop at the last one, and the staging keys \
     do nothing here. A second run closes the view. The command needs exactly \
     two panes that show two different files.",
    ActionPriority::Normal,
    command_name = "diff-pair"
);

define_action!(
    DiffBackDef,
    DiffBack,
    "DiffBack",
    ActionKind::DiffBack,
    "walk to the next change, or back through the jumplist after a jump",
    "In the diff view, walk to the next change, the way GotoNextChange does. \
     Once a jump such as a definition lookup has carried the cursor off the \
     change the last walk landed on, retrace the jumplist instead, one \
     position per run, until the cursor is on that change again. The next run \
     then walks to the next change. Bound to the mouse back button in the diff \
     view.",
    ActionPriority::Normal,
    command_name = "diff-back"
);

define_action!(
    DiffForwardDef,
    DiffForward,
    "DiffForward",
    ActionKind::DiffForward,
    "walk to the previous change, or forward through the jumplist after a jump",
    "In the diff view, re-advance the jumplist while a jump has left the \
     change the last walk landed on and forward history remains. Otherwise \
     walk to the previous change, the way GotoPrevChange does. Bound to the \
     mouse forward button in the diff view.",
    ActionPriority::Normal,
    command_name = "diff-forward"
);

define_action!(
    StageHunkDef,
    StageHunk,
    "StageHunk",
    ActionKind::StageHunk,
    "stage the hunk under the cursor",
    "Apply the diff hunk under the cursor to the git index, staging just \
     that change. Inside a hunk the structural diff narrowed, it stages only \
     the marked run under the cursor. Works in any editor view on a \
     git-tracked file, and is a no-op with a status message when the cursor \
     is not on a hunk.",
    ActionPriority::Common
);

define_action!(
    UnstageHunkDef,
    UnstageHunk,
    "UnstageHunk",
    ActionKind::UnstageHunk,
    "unstage the hunk under the cursor",
    "Reverse-apply the diff hunk under the cursor against the git index, \
     unstaging just that change. Inside a hunk the structural diff narrowed, \
     it unstages only the marked run under the cursor. Works in any editor \
     view on a git-tracked file, and is a no-op with a status message when \
     the cursor is not on a hunk.",
    ActionPriority::Common
);

define_action!(
    ToggleStageHunkDef,
    ToggleStageHunk,
    "ToggleStageHunk",
    ActionKind::ToggleStageHunk,
    "toggle staging of the hunk under the cursor",
    "Stage the diff hunk under the cursor when it is unstaged, or unstage \
     it when it is already staged. Inside a hunk the structural diff \
     narrowed, it toggles only the marked run under the cursor. Works in \
     any editor view on a git-tracked file, and is a no-op with a status \
     message when the cursor is not on a hunk.",
    ActionPriority::Common
);

define_action!(
    StageLineDef,
    StageLine,
    "StageLine",
    ActionKind::StageLine,
    "stage the line under the cursor",
    "Apply only the cursor line's change to the git index, staging the \
     minus/plus pair of a modified line. Works in any editor view on a \
     git-tracked file, and is a no-op with a status message when the \
     cursor is on no change.",
    ActionPriority::Common
);

define_action!(
    UnstageLineDef,
    UnstageLine,
    "UnstageLine",
    ActionKind::UnstageLine,
    "unstage the line under the cursor",
    "Revert only the cursor line's staged change in the git index back to \
     HEAD, unstaging the minus/plus pair of a modified line. Works in any \
     editor view on a git-tracked file, and is a no-op with a status \
     message when the cursor is on no staged change.",
    ActionPriority::Common
);

define_action!(
    ToggleStageLineDef,
    ToggleStageLine,
    "ToggleStageLine",
    ActionKind::ToggleStageLine,
    "toggle staging of the line under the cursor",
    "Stage the cursor line's change when it is unstaged, or unstage it when \
     it is already staged. Works in any editor view on a git-tracked file, \
     and is a no-op with a status message when the cursor is on no change.",
    ActionPriority::Common
);

define_action!(
    JumpToMoveSourceDef,
    JumpToMoveSource,
    "JumpToMoveSource",
    ActionKind::JumpToMoveSource,
    "jump to the source of a moved hunk",
    "If the cursor is on a Moved hunk, navigate to its first recorded source \
     location. For ambiguous moves, JumpToNextMoveSource / JumpToPrevMoveSource \
     cycle among the alternates.",
    ActionPriority::Rare
);

define_action!(
    JumpToMoveTargetDef,
    JumpToMoveTarget,
    "JumpToMoveTarget",
    ActionKind::JumpToMoveTarget,
    "jump to the target of a moved hunk",
    "From the negative (source) side of a Moved hunk, navigate forward to the \
     corresponding target location on the positive side.",
    ActionPriority::Rare
);

define_action!(
    JumpToNextMoveSourceDef,
    JumpToNextMoveSource,
    "JumpToNextMoveSource",
    ActionKind::JumpToNextMoveSource,
    "cycle to the next source of an ambiguous moved hunk",
    "When a Moved hunk has multiple candidate sources (consolidation from N to \
     1), advance the selection cursor to the next source and jump there.",
    ActionPriority::Rare
);

define_action!(
    JumpToPrevMoveSourceDef,
    JumpToPrevMoveSource,
    "JumpToPrevMoveSource",
    ActionKind::JumpToPrevMoveSource,
    "cycle to the previous source of an ambiguous moved hunk",
    "When a Moved hunk has multiple candidate sources, step the selection cursor \
     to the previous source and jump there.",
    ActionPriority::Rare
);

define_action!(
    QueryMoveRelationshipsDef,
    QueryMoveRelationships,
    "QueryMoveRelationships",
    ActionKind::QueryMoveRelationships,
    "describe the move provenance at the cursor",
    "Report the cardinality and source locations of the Moved hunk under the \
     cursor. Scriptable surface for future automation hooks; a no-op today \
     when the cursor is not on a Moved hunk.",
    ActionPriority::Rare
);

define_action!(
    ReviewNextCommitDef,
    ReviewNextCommit,
    "ReviewNextCommit",
    ActionKind::ReviewNextCommit,
    "review the next commit",
    "Step a review walk one commit toward the ref tip, checking that commit \
     out and showing its diff. Wraps to the base past the tip. Refuses while the \
     working tree has uncommitted changes to tracked files.",
    ActionPriority::Rare,
    command_name = "review-next-commit"
);

define_action!(
    ReviewPrevCommitDef,
    ReviewPrevCommit,
    "ReviewPrevCommit",
    ActionKind::ReviewPrevCommit,
    "review the previous commit",
    "Step a review walk one commit back toward its base, checking that commit \
     out and showing its diff. Wraps to the tip past the base. Refuses while the \
     working tree has uncommitted changes to tracked files.",
    ActionPriority::Rare,
    command_name = "review-prev-commit"
);

define_action!(
    ReviewDoneDef,
    ReviewDone,
    "ReviewDone",
    ActionKind::ReviewDone,
    "end the review walk",
    "End a review walk, checking out the branch or commit the working tree \
     was on when the walk started and closing any open diff. Keeps the walk \
     so it can be retried when the checkout back fails.",
    ActionPriority::Rare,
    command_name = "review-done"
);

const GIT_REVIEW_PARAMS: &[ParamDef] = &[ParamDef {
    name: "reference",
    kind: ParamKind::String,
    value_source: ValueSource::None,
    required: true,
    description: "Branch, tag, sha, or revspec whose history to review from.",
}];

define_action_def!(
    GitReviewDef,
    "GitReview",
    ActionKind::GitReview,
    "review from a commit on a ref",
    "Resolve the given branch, tag, sha, or revspec and open a picker over \
     its first-parent history so a commit can be chosen as the review \
     base. Reports an error without opening anything when the revision \
     does not resolve or the workspace is not inside a repository.",
    ActionPriority::Normal,
    command_name = "git-review",
    params = GIT_REVIEW_PARAMS
);

#[derive(Debug)]
pub struct GitReview {
    pub reference: String,
}

impl GitReview {
    pub const DEF: &GitReviewDef = &GitReviewDef;
}

impl Action for GitReview {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// Palette-invisible because the edits payload cannot be constructed from
// a string. Dispatched programmatically by agent-bridge code.
define_action_def!(
    OpenReviewAgentEditsDef,
    "OpenReviewAgentEdits",
    ActionKind::OpenReviewAgentEdits,
    "review agent-proposed edits",
    "Open a review session over a list of agent-proposed edits. \
     Dispatched programmatically; not visible in the palette because \
     the edits payload cannot be represented as a parameter string.",
    ActionPriority::Normal,
    palette_visible = false
);

#[derive(Debug, Clone)]
pub struct AgentEdit {
    pub path: PathBuf,
    pub base_text: std::sync::Arc<String>,
    pub proposed_text: std::sync::Arc<String>,
}

#[derive(Debug)]
pub struct OpenReviewAgentEdits {
    pub edits: Vec<AgentEdit>,
}

impl OpenReviewAgentEdits {
    pub const DEF: &OpenReviewAgentEditsDef = &OpenReviewAgentEditsDef;
}

impl Action for OpenReviewAgentEdits {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Action;

    #[test]
    fn kind_and_name() {
        let diff = Diff { rev: None };
        assert_eq!(diff.kind(), ActionKind::Diff);
        assert_eq!(diff.def().name(), "Diff");
        assert!(diff.def().palette_visible());
    }

    /// The revision is optional, so `:diff` on its own still means HEAD. A
    /// required parameter would make the bare command an error.
    #[test]
    fn the_revision_is_one_optional_string() {
        let params = Diff { rev: None }.def().params();
        assert_eq!(
            params
                .iter()
                .map(|param| (param.name, param.kind, param.required))
                .collect::<Vec<_>>(),
            [("rev", ParamKind::String, false)],
        );
    }

    #[test]
    fn downcast() {
        let action: Box<dyn Action> = Box::new(Diff { rev: None });
        assert!(action.as_any().downcast_ref::<Diff>().is_some());
    }
}

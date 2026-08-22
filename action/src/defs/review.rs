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
    description: "Branch, tag, sha, or revspec to diff against. Defaults to HEAD.",
}];

define_action_def!(
    DiffDef,
    "Diff",
    ActionKind::Diff,
    "open a diff of working-tree changes",
    "Open the first changed file with a structural diff against HEAD, or \
     against the given revision. A revision points the whole workspace at \
     that commit, so every file diffs against it and the change list spans \
     everything committed since. Running it again closes the diff and \
     returns to HEAD.",
    ActionPriority::Common,
    params = DIFF_PARAMS
);

#[derive(Debug)]
pub struct Diff {
    /// Revision to diff against, or `None` for the working tree's own
    /// HEAD-plus-index.
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
    StageHunkDef,
    StageHunk,
    "StageHunk",
    ActionKind::StageHunk,
    "stage the hunk under the cursor",
    "Apply the diff hunk under the cursor to the git index, staging just \
     that change. Works in any editor view on a git-tracked file, and is \
     a no-op with a status message when the cursor is not on a hunk.",
    ActionPriority::Common
);

define_action!(
    UnstageHunkDef,
    UnstageHunk,
    "UnstageHunk",
    ActionKind::UnstageHunk,
    "unstage the hunk under the cursor",
    "Reverse-apply the diff hunk under the cursor against the git index, \
     unstaging just that change. Works in any editor view on a git-tracked \
     file, and is a no-op with a status message when the cursor is not on \
     a hunk.",
    ActionPriority::Common
);

define_action!(
    ToggleStageHunkDef,
    ToggleStageHunk,
    "ToggleStageHunk",
    ActionKind::ToggleStageHunk,
    "toggle staging of the hunk under the cursor",
    "Stage the diff hunk under the cursor when it is unstaged, or unstage \
     it when it is already staged. Works in any editor view on a \
     git-tracked file, and is a no-op with a status message when the \
     cursor is not on a hunk.",
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
     out and showing its diff. Does nothing at the tip. Refuses while the \
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
     out and showing its diff. Does nothing at the base. Refuses while the \
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

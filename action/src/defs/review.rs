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
    ToggleDiffDef,
    ToggleDiff,
    "ToggleDiff",
    ActionKind::ToggleDiff,
    "toggle between the diff and the plain file",
    "Swap the focused pane between the side-by-side review and a plain \
     editor on the same file, keeping the review session alive so the \
     toggle is instant and staging decisions survive. From the diff, \
     lands the cursor on the file line under the review cursor; from the \
     file, restores the diff at the chunk the cursor sits in.",
    ActionPriority::Common
);

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
    ReviewNextChunkDef,
    ReviewNextChunk,
    "ReviewNextChunk",
    ActionKind::ReviewNextChunk,
    "advance to the next review chunk",
    "Move the review cursor forward to the next chunk in visit order, \
     scrolling the pane to keep the chunk's header in view. Clamps at the \
     last chunk and emits an end-of-review badge when already there.",
    ActionPriority::Rare
);

define_action!(
    ReviewPrevChunkDef,
    ReviewPrevChunk,
    "ReviewPrevChunk",
    ActionKind::ReviewPrevChunk,
    "step back to the previous review chunk",
    "Move the review cursor backward to the previous chunk in visit order, \
     scrolling the pane to keep the chunk's header in view. Clamps at the \
     first chunk.",
    ActionPriority::Rare
);

define_action!(
    ReviewStageChunkDef,
    ReviewStageChunk,
    "ReviewStageChunk",
    ActionKind::ReviewStageChunk,
    "mark the current chunk as staged",
    "Mark the current review chunk as Staged. Progress footer updates and \
     the chunk's gutter flips to the staged glyph.",
    ActionPriority::Rare
);

define_action!(
    ReviewUnstageChunkDef,
    ReviewUnstageChunk,
    "ReviewUnstageChunk",
    ActionKind::ReviewUnstageChunk,
    "mark the current chunk as unstaged",
    "Mark the current review chunk as Unstaged.",
    ActionPriority::Rare
);

define_action!(
    ReviewToggleStageDef,
    ReviewToggleStage,
    "ReviewToggleStage",
    ActionKind::ReviewToggleStage,
    "toggle staged/unstaged for the current chunk",
    "Flip the current chunk between Staged and Unstaged. Chunks in Pending \
     or Skipped flip to Staged on first press.",
    ActionPriority::Rare
);

define_action!(
    ReviewSkipChunkDef,
    ReviewSkipChunk,
    "ReviewSkipChunk",
    ActionKind::ReviewSkipChunk,
    "skip the current chunk",
    "Mark the current chunk as Skipped: read but not acted on. Used when \
     stepping through commits to pass over changes that don't need a \
     stage/unstage decision.",
    ActionPriority::Rare
);

define_action!(
    ReviewRefreshDef,
    ReviewRefresh,
    "ReviewRefresh",
    ActionKind::ReviewRefresh,
    "rescan the review source",
    "Rebuild the review session from its source, preserving staged/unstaged \
     decisions on chunks whose base content still matches.",
    ActionPriority::Rare
);

define_action!(
    ReviewApplyStagedDef,
    ReviewApplyStaged,
    "ReviewApplyStaged",
    ActionKind::ReviewApplyStaged,
    "apply staged chunks",
    "Apply all staged chunks to the underlying source (git index for the \
     working tree, commit rewrite for past commits). Unimplemented for v1.",
    ActionPriority::Rare
);

define_action!(
    CloseReviewDef,
    CloseReview,
    "CloseReview",
    ActionKind::CloseReview,
    "close the active review",
    "Drop the active review session and return the focused pane to a \
     regular editor. Unreviewed chunks are lost; use the palette's \
     `ReviewApplyStaged` first to act on decisions."
);

define_action!(
    ReviewRemoveSelectedDef,
    ReviewRemoveSelected,
    "ReviewRemoveSelected",
    ActionKind::ReviewRemoveSelected,
    "remove staged hunks from the reviewed commit",
    "Only valid when the review's source is a commit: rewrite that \
     commit with every Staged chunk spliced back to its parent-side \
     content. When the reviewed commit is HEAD, amends HEAD directly; \
     otherwise rewrites and cherry-picks descendants. Refuses with an \
     error badge if the working tree is dirty.",
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

const OPEN_REVIEW_COMMIT_PARAMS: &[ParamDef] = &[
    ParamDef {
        name: "workdir",
        kind: ParamKind::String,
        value_source: ValueSource::None,
        required: true,
        description: "Absolute path of a directory inside the target repository.",
    },
    ParamDef {
        name: "sha",
        kind: ParamKind::String,
        value_source: ValueSource::None,
        required: true,
        description: "Commit sha to review against its first parent.",
    },
];

// Hidden from the palette because its workdir and sha parameters are not
// something a user can type. The commits view dispatches it directly.
define_action_def!(
    OpenReviewCommitDef,
    "OpenReviewCommit",
    ActionKind::OpenReviewCommit,
    "review a single commit",
    "Open a review session diffing the given commit's tree against its \
     first parent. Root commits diff against the empty tree.",
    ActionPriority::Normal,
    palette_visible = false,
    params = OPEN_REVIEW_COMMIT_PARAMS
);

#[derive(Debug)]
pub struct OpenReviewCommit {
    pub workdir: PathBuf,
    pub sha: String,
}

impl OpenReviewCommit {
    pub const DEF: &OpenReviewCommitDef = &OpenReviewCommitDef;
}

impl Action for OpenReviewCommit {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

const OPEN_REVIEW_COMMIT_RANGE_PARAMS: &[ParamDef] = &[
    ParamDef {
        name: "workdir",
        kind: ParamKind::String,
        value_source: ValueSource::None,
        required: true,
        description: "Absolute path of a directory inside the target repository.",
    },
    ParamDef {
        name: "from",
        kind: ParamKind::String,
        value_source: ValueSource::None,
        required: true,
        description: "Base commit sha (exclusive in git-diff semantics).",
    },
    ParamDef {
        name: "to",
        kind: ParamKind::String,
        value_source: ValueSource::None,
        required: true,
        description: "Tip commit sha.",
    },
];

// Hidden from the palette because its workdir and revision parameters are
// not something a user can type. The commits view dispatches it directly.
define_action_def!(
    OpenReviewCommitRangeDef,
    "OpenReviewCommitRange",
    ActionKind::OpenReviewCommitRange,
    "review a commit range",
    "Open a review session diffing `to`'s tree against `from`'s tree. \
     Mirrors `git diff from..to`.",
    ActionPriority::Normal,
    palette_visible = false,
    params = OPEN_REVIEW_COMMIT_RANGE_PARAMS
);

#[derive(Debug)]
pub struct OpenReviewCommitRange {
    pub workdir: PathBuf,
    pub from: String,
    pub to: String,
}

impl OpenReviewCommitRange {
    pub const DEF: &OpenReviewCommitRangeDef = &OpenReviewCommitRangeDef;
}

impl Action for OpenReviewCommitRange {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// Palette-invisible because the path is supplied by the filesystem
// watcher dispatch, not user input. Triggers a session rescan and
// jumps the cursor to the first chunk in the affected file.
define_action_def!(
    ReviewExternalEditDef,
    "ReviewExternalEdit",
    ActionKind::ReviewExternalEdit,
    "react to an external edit on a reviewed file",
    "Refresh the active review session because the named file \
     changed on disk, then jump the cursor to the first chunk in \
     that file. Dispatched by the filesystem-watch drain when the \
     path is one of the session's reviewed files.",
    ActionPriority::Normal,
    palette_visible = false
);

#[derive(Debug)]
pub struct ReviewExternalEdit {
    pub path: PathBuf,
}

impl ReviewExternalEdit {
    pub const DEF: &ReviewExternalEditDef = &ReviewExternalEditDef;
}

impl Action for ReviewExternalEdit {
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

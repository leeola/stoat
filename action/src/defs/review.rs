use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    OpenReviewDef,
    OpenReview,
    "OpenReview",
    ActionKind::OpenReview,
    "review changed files",
    "Open the first modified or staged file with a structural diff against HEAD.",
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
    ReviewApproveHunkDef,
    ReviewApproveHunk,
    "ReviewApproveHunk",
    ActionKind::ReviewApproveHunk,
    "approve the current chunk and advance",
    "Mark the current chunk as approved and advance the review cursor to \
     the next chunk. Approval is independent of staged/unstaged so a \
     reviewer can step through changes without committing to a staging \
     decision.",
    ActionPriority::Rare
);

define_action!(
    ReviewToggleApprovalDef,
    ReviewToggleApproval,
    "ReviewToggleApproval",
    ActionKind::ReviewToggleApproval,
    "toggle approval of the current chunk",
    "Flip the current chunk's approval flag without moving the review \
     cursor. Independent of staged/unstaged status.",
    ActionPriority::Rare
);

define_action!(
    ReviewNextUnreviewedHunkDef,
    ReviewNextUnreviewedHunk,
    "ReviewNextUnreviewedHunk",
    ActionKind::ReviewNextUnreviewedHunk,
    "advance to the next unapproved chunk",
    "Move the review cursor to the next chunk whose approval flag is \
     false, wrapping from the end of the session back to the start. \
     No-op when every chunk has been approved.",
    ActionPriority::Rare
);

define_action!(
    ReviewResetProgressDef,
    ReviewResetProgress,
    "ReviewResetProgress",
    ActionKind::ReviewResetProgress,
    "reset review progress",
    "Clear every chunk's approval flag, revert every chunk's status to \
     Pending, and move the review cursor back to the first chunk. Used \
     to start the review over.",
    ActionPriority::Rare
);

define_action!(
    GitToggleStageHunkDef,
    GitToggleStageHunk,
    "GitToggleStageHunk",
    ActionKind::GitToggleStageHunk,
    "stage or unstage the current hunk",
    "Toggle the git-index staged state of the chunk under the review \
     cursor. Stages it and marks the chunk Staged when it is not yet \
     staged; otherwise reverses the patch back out of the index and \
     marks the chunk Pending. Acts on the index directly, independent \
     of the batch apply flow.",
    ActionPriority::Rare
);

define_action!(
    GitUnstageHunkDef,
    GitUnstageHunk,
    "GitUnstageHunk",
    ActionKind::GitUnstageHunk,
    "unstage the current hunk",
    "Reverse the chunk under the review cursor back out of the git \
     index regardless of its current state, marking the chunk Pending. \
     The explicit-unstage counterpart to GitToggleStageHunk.",
    ActionPriority::Rare
);

define_action!(
    GitToggleStageLineDef,
    GitToggleStageLine,
    "GitToggleStageLine",
    ActionKind::GitToggleStageLine,
    "stage or unstage the current line",
    "Stage or unstage a single line within the chunk under the review \
     cursor, applying a one-line patch to the git index. Marks the chunk \
     PartiallyStaged when it gains a staged line; reverses the line and \
     marks the chunk Pending when toggled off. Acts on the index \
     directly, independent of the batch apply flow.",
    ActionPriority::Rare
);

define_action!(
    ReviewRevertHunkDef,
    ReviewRevertHunk,
    "ReviewRevertHunk",
    ActionKind::ReviewRevertHunk,
    "revert the current hunk on disk",
    "Apply the reversed patch of the chunk under the review cursor to the \
     working tree, undoing that change on disk. Acts on files directly, \
     not the git index.",
    ActionPriority::Rare
);

define_action!(
    ReviewCycleComparisonModeDef,
    ReviewCycleComparisonMode,
    "ReviewCycleComparisonMode",
    ActionKind::ReviewCycleComparisonMode,
    "cycle the diff comparison source",
    "Swap the review between diff-comparison sources in order: the full \
     working tree, unstaged-only changes, staged-only changes, and the \
     last commit, then back to the working tree. Re-extracts hunks from \
     the new source and preserves each chunk's review decision where its \
     content still matches.",
    ActionPriority::Rare
);

define_action!(
    ReviewToggleFollowDef,
    ReviewToggleFollow,
    "ReviewToggleFollow",
    ActionKind::ReviewToggleFollow,
    "toggle review follow mode",
    "Toggle follow mode: while on, an external edit to a reviewed file \
     moves the review cursor to that file's first chunk. Defaults off.",
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
     `ReviewApplyStaged` first to act on decisions.",
    ActionPriority::Normal
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

use crate::{Action, ActionDef, ParamDef, ParamKind};
use serde::Deserialize;
use std::{any::Any, path::PathBuf};

const OPEN_REVIEW_COMMIT_PARAMS: &[ParamDef] = &[
    ParamDef {
        name: "workdir",
        kind: ParamKind::String,
        required: true,
        description: "Absolute path of a directory inside the target repository.",
    },
    ParamDef {
        name: "sha",
        kind: ParamKind::String,
        required: true,
        description: "Commit sha to review against its first parent.",
    },
];

#[derive(Debug)]
pub struct OpenReviewCommitDef;

impl ActionDef for OpenReviewCommitDef {
    fn name(&self) -> &'static str {
        "OpenReviewCommit"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::OpenReviewCommit
    }

    fn params(&self) -> &'static [ParamDef] {
        OPEN_REVIEW_COMMIT_PARAMS
    }

    fn short_desc(&self) -> &'static str {
        "review a single commit"
    }

    fn long_desc(&self) -> &'static str {
        "Open a review session diffing the given commit's tree against its \
         first parent. Root commits diff against the empty tree."
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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

const OPEN_REVIEW_WATCH_PARAMS: &[ParamDef] = &[ParamDef {
    name: "workdir",
    kind: ParamKind::String,
    required: true,
    description: "Absolute path of the directory whose live edits should populate the review.",
}];

#[derive(Debug)]
pub struct OpenReviewWatchDef;

impl ActionDef for OpenReviewWatchDef {
    fn name(&self) -> &'static str {
        "OpenReviewWatch"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::OpenReviewWatch
    }

    fn params(&self) -> &'static [ParamDef] {
        OPEN_REVIEW_WATCH_PARAMS
    }

    fn short_desc(&self) -> &'static str {
        "review live edits to a workspace"
    }

    fn long_desc(&self) -> &'static str {
        "Open an empty review session that grows as files inside \
         `workdir` change on disk. Each external write becomes a \
         review chunk diffed against git HEAD; the cursor jumps to \
         the most recent change."
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OpenReviewWatch {
    pub workdir: PathBuf,
}

impl OpenReviewWatch {
    pub const DEF: &OpenReviewWatchDef = &OpenReviewWatchDef;
}

impl Action for OpenReviewWatch {
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
        required: true,
        description: "Absolute path of a directory inside the target repository.",
    },
    ParamDef {
        name: "from",
        kind: ParamKind::String,
        required: true,
        description: "Base commit sha (exclusive in git-diff semantics).",
    },
    ParamDef {
        name: "to",
        kind: ParamKind::String,
        required: true,
        description: "Tip commit sha.",
    },
];

#[derive(Debug)]
pub struct OpenReviewCommitRangeDef;

impl ActionDef for OpenReviewCommitRangeDef {
    fn name(&self) -> &'static str {
        "OpenReviewCommitRange"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::OpenReviewCommitRange
    }

    fn params(&self) -> &'static [ParamDef] {
        OPEN_REVIEW_COMMIT_RANGE_PARAMS
    }

    fn short_desc(&self) -> &'static str {
        "review a commit range"
    }

    fn long_desc(&self) -> &'static str {
        "Open a review session diffing `to`'s tree against `from`'s tree. \
         Mirrors `git diff from..to`."
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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

const OPEN_REVIEW_BRANCH_PARAMS: &[ParamDef] = &[
    ParamDef {
        name: "workdir",
        kind: ParamKind::String,
        required: true,
        description: "Absolute path of a directory inside the target repository.",
    },
    ParamDef {
        name: "base",
        kind: ParamKind::String,
        required: false,
        description: "Base ref to diff the branch against. \
                      Defaults to the repository's default branch when omitted.",
    },
];

#[derive(Debug)]
pub struct OpenReviewBranchDef;

impl ActionDef for OpenReviewBranchDef {
    fn name(&self) -> &'static str {
        "OpenReviewBranch"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::OpenReviewBranch
    }

    fn params(&self) -> &'static [ParamDef] {
        OPEN_REVIEW_BRANCH_PARAMS
    }

    fn short_desc(&self) -> &'static str {
        "review a branch commit by commit"
    }

    fn long_desc(&self) -> &'static str {
        "Open a review session that walks each commit in the branch \
         (merge-base(base, HEAD)..HEAD) as its own diff, oldest first."
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OpenReviewBranch {
    pub workdir: PathBuf,
    pub base: Option<String>,
}

impl OpenReviewBranch {
    pub const DEF: &OpenReviewBranchDef = &OpenReviewBranchDef;
}

impl Action for OpenReviewBranch {
    fn def(&self) -> &'static dyn ActionDef {
        Self::DEF
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Palette-invisible because the path is supplied by the filesystem
/// watcher dispatch, not user input. Triggers a session rescan and
/// jumps the cursor to the first chunk in the affected file.
#[derive(Debug)]
pub struct ReviewExternalEditDef;

impl ActionDef for ReviewExternalEditDef {
    fn name(&self) -> &'static str {
        "ReviewExternalEdit"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::ReviewExternalEdit
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn short_desc(&self) -> &'static str {
        "react to an external edit on a reviewed file"
    }

    fn long_desc(&self) -> &'static str {
        "Refresh the active review session because the named file \
         changed on disk, then jump the cursor to the first chunk in \
         that file. Dispatched by the filesystem-watch drain when the \
         path is one of the session's reviewed files."
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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

/// Palette-invisible because the edits payload cannot be constructed from
/// a string. Dispatched programmatically by agent-bridge code.
#[derive(Debug)]
pub struct OpenReviewAgentEditsDef;

impl ActionDef for OpenReviewAgentEditsDef {
    fn name(&self) -> &'static str {
        "OpenReviewAgentEdits"
    }

    fn kind(&self) -> ActionKind {
        ActionKind::OpenReviewAgentEdits
    }

    fn params(&self) -> &'static [ParamDef] {
        &[]
    }

    fn palette_visible(&self) -> bool {
        false
    }

    fn short_desc(&self) -> &'static str {
        "review agent-proposed edits"
    }

    fn long_desc(&self) -> &'static str {
        "Open a review session over a list of agent-proposed edits. \
         Dispatched programmatically; not visible in the palette because \
         the edits payload cannot be represented as a parameter string."
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AgentEdit {
    pub path: PathBuf,
    pub base_text: std::sync::Arc<String>,
    pub proposed_text: std::sync::Arc<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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

define_action!(
    ReviewEnterLineSelectDef,
    ReviewEnterLineSelect,
    "ReviewEnterLineSelect",
    ActionKind::ReviewEnterLineSelect,
    "select lines in the current hunk",
    "Enter line-select mode on the chunk under the review cursor, snapshotting its rows with every row selected so individual lines can be staged or unstaged as a group.",
    ActionPriority::Common
);

define_action!(
    ReviewLineSelectCancelDef,
    ReviewLineSelectCancel,
    "ReviewLineSelectCancel",
    ActionKind::ReviewLineSelectCancel,
    "cancel line selection",
    "Discard the current line selection and return to review mode without staging anything.",
    ActionPriority::Common
);

define_action!(
    ReviewLineSelectToggleDef,
    ReviewLineSelectToggle,
    "ReviewLineSelectToggle",
    ActionKind::ReviewLineSelectToggle,
    "toggle the selected line",
    "Toggle whether the line under the review cursor participates in the next stage or unstage while in line-select mode.",
    ActionPriority::Common
);

define_action!(
    ReviewLineSelectAllDef,
    ReviewLineSelectAll,
    "ReviewLineSelectAll",
    ActionKind::ReviewLineSelectAll,
    "select every line",
    "Select every line in the current hunk while in line-select mode.",
    ActionPriority::Common
);

define_action!(
    ReviewLineSelectStageDef,
    ReviewLineSelectStage,
    "ReviewLineSelectStage",
    ActionKind::ReviewLineSelectStage,
    "stage the selected lines",
    "Stage the selected lines of the current hunk to the git index, then leave line-select mode.",
    ActionPriority::Common
);

define_action!(
    ReviewLineSelectUnstageDef,
    ReviewLineSelectUnstage,
    "ReviewLineSelectUnstage",
    ActionKind::ReviewLineSelectUnstage,
    "unstage the selected lines",
    "Unstage the selected lines of the current hunk from the git index, then leave line-select mode.",
    ActionPriority::Common
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Action;

    #[test]
    fn kind_and_name() {
        assert_eq!(OpenReview.kind(), ActionKind::OpenReview);
        assert_eq!(OpenReview.def().name(), "OpenReview");
        assert!(OpenReview.def().params().is_empty());
        assert!(OpenReview.def().palette_visible());
    }

    #[test]
    fn downcast() {
        let action: Box<dyn Action> = Box::new(OpenReview);
        assert!(action.as_any().downcast_ref::<OpenReview>().is_some());
    }
}

use crate::{action::define_action, ActionKind, ActionPriority};

define_action!(
    DiffDef,
    Diff,
    "Diff",
    ActionKind::Diff,
    "open a diff of working-tree changes",
    "Open the first modified or staged file with a structural diff against HEAD.",
    ActionPriority::Common
);

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

use crate::{Action, ActionDef, ParamDef, ParamKind, ValueSource};
use std::{any::Any, path::PathBuf};

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
        assert_eq!(Diff.kind(), ActionKind::Diff);
        assert_eq!(Diff.def().name(), "Diff");
        assert!(Diff.def().params().is_empty());
        assert!(Diff.def().palette_visible());
    }

    #[test]
    fn downcast() {
        let action: Box<dyn Action> = Box::new(Diff);
        assert!(action.as_any().downcast_ref::<Diff>().is_some());
    }
}

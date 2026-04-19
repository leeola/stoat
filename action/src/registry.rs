use crate::{
    defs::{
        app::Quit,
        claude::{
            ClaudeSubmit, ClaudeToDockLeft, ClaudeToDockRight, ClaudeToPane, OpenClaude,
            ToggleDockLeft, ToggleDockRight,
        },
        commits::{
            CloseCommits, CommitsFirst, CommitsLast, CommitsNext, CommitsPageDown, CommitsPageUp,
            CommitsPrev, CommitsRefresh, OpenCommits,
        },
        editor::{
            AddSelectionBelow, MoveDown, MoveLeft, MoveNextWordEnd, MoveNextWordStart,
            MovePrevWordStart, MoveRight, MoveUp,
        },
        file::OpenFile,
        palette::OpenCommandPalette,
        pane::{
            ClosePane, FocusDown, FocusLeft, FocusNext, FocusPrev, FocusRight, FocusUp, SplitDown,
            SplitRight,
        },
        review::{
            CloseReview, JumpToMoveSource, JumpToMoveTarget, JumpToNextMoveSource,
            JumpToPrevMoveSource, OpenReview, OpenReviewCommit, OpenReviewCommitRange,
            QueryMoveRelationships, ReviewApplyStaged, ReviewNextChunk, ReviewPrevChunk,
            ReviewRefresh, ReviewSkipChunk, ReviewStageChunk, ReviewToggleStage,
            ReviewUnstageChunk,
        },
        run::{OpenRun, Run, RunInterrupt, RunSubmit},
    },
    Action, ActionDef, ParamError, ParamKind, ParamValue,
};
use std::{collections::HashMap, path::PathBuf, sync::OnceLock};

pub type CreateFn = fn(&[ParamValue]) -> Result<Box<dyn Action>, ParamError>;

pub struct RegistryEntry {
    pub def: &'static dyn ActionDef,
    pub create: CreateFn,
}

static REGISTRY: OnceLock<HashMap<&'static str, RegistryEntry>> = OnceLock::new();

fn init() -> HashMap<&'static str, RegistryEntry> {
    let mut map = HashMap::with_capacity(16);
    let mut add = |def: &'static dyn ActionDef, create: CreateFn| {
        map.insert(def.name(), RegistryEntry { def, create });
    };

    add(Quit::DEF, |_| Ok(Box::new(Quit)));
    add(SplitRight::DEF, |_| Ok(Box::new(SplitRight)));
    add(SplitDown::DEF, |_| Ok(Box::new(SplitDown)));
    add(FocusLeft::DEF, |_| Ok(Box::new(FocusLeft)));
    add(FocusRight::DEF, |_| Ok(Box::new(FocusRight)));
    add(FocusUp::DEF, |_| Ok(Box::new(FocusUp)));
    add(FocusDown::DEF, |_| Ok(Box::new(FocusDown)));
    add(FocusNext::DEF, |_| Ok(Box::new(FocusNext)));
    add(FocusPrev::DEF, |_| Ok(Box::new(FocusPrev)));
    add(ClosePane::DEF, |_| Ok(Box::new(ClosePane)));
    add(OpenCommandPalette::DEF, |_| {
        Ok(Box::new(OpenCommandPalette))
    });
    add(OpenReview::DEF, |_| Ok(Box::new(OpenReview)));
    add(JumpToMoveSource::DEF, |_| Ok(Box::new(JumpToMoveSource)));
    add(JumpToMoveTarget::DEF, |_| Ok(Box::new(JumpToMoveTarget)));
    add(JumpToNextMoveSource::DEF, |_| {
        Ok(Box::new(JumpToNextMoveSource))
    });
    add(JumpToPrevMoveSource::DEF, |_| {
        Ok(Box::new(JumpToPrevMoveSource))
    });
    add(QueryMoveRelationships::DEF, |_| {
        Ok(Box::new(QueryMoveRelationships))
    });
    add(ReviewNextChunk::DEF, |_| Ok(Box::new(ReviewNextChunk)));
    add(ReviewPrevChunk::DEF, |_| Ok(Box::new(ReviewPrevChunk)));
    add(ReviewStageChunk::DEF, |_| Ok(Box::new(ReviewStageChunk)));
    add(ReviewUnstageChunk::DEF, |_| {
        Ok(Box::new(ReviewUnstageChunk))
    });
    add(ReviewToggleStage::DEF, |_| Ok(Box::new(ReviewToggleStage)));
    add(ReviewSkipChunk::DEF, |_| Ok(Box::new(ReviewSkipChunk)));
    add(ReviewRefresh::DEF, |_| Ok(Box::new(ReviewRefresh)));
    add(ReviewApplyStaged::DEF, |_| Ok(Box::new(ReviewApplyStaged)));
    add(CloseReview::DEF, |_| Ok(Box::new(CloseReview)));
    add(OpenReviewCommit::DEF, |params| {
        let workdir = params
            .first()
            .ok_or(ParamError::Missing("workdir"))?
            .as_string()
            .ok_or(ParamError::WrongKind {
                name: "workdir",
                expected: ParamKind::String,
            })?;
        let sha = params
            .get(1)
            .ok_or(ParamError::Missing("sha"))?
            .as_string()
            .ok_or(ParamError::WrongKind {
                name: "sha",
                expected: ParamKind::String,
            })?;
        Ok(Box::new(OpenReviewCommit {
            workdir: PathBuf::from(workdir),
            sha: sha.to_owned(),
        }))
    });
    add(OpenReviewCommitRange::DEF, |params| {
        let workdir = params
            .first()
            .ok_or(ParamError::Missing("workdir"))?
            .as_string()
            .ok_or(ParamError::WrongKind {
                name: "workdir",
                expected: ParamKind::String,
            })?;
        let from = params
            .get(1)
            .ok_or(ParamError::Missing("from"))?
            .as_string()
            .ok_or(ParamError::WrongKind {
                name: "from",
                expected: ParamKind::String,
            })?;
        let to = params
            .get(2)
            .ok_or(ParamError::Missing("to"))?
            .as_string()
            .ok_or(ParamError::WrongKind {
                name: "to",
                expected: ParamKind::String,
            })?;
        Ok(Box::new(OpenReviewCommitRange {
            workdir: PathBuf::from(workdir),
            from: from.to_owned(),
            to: to.to_owned(),
        }))
    });
    add(AddSelectionBelow::DEF, |_| Ok(Box::new(AddSelectionBelow)));
    add(MoveLeft::DEF, |_| Ok(Box::new(MoveLeft)));
    add(MoveRight::DEF, |_| Ok(Box::new(MoveRight)));
    add(MoveUp::DEF, |_| Ok(Box::new(MoveUp)));
    add(MoveDown::DEF, |_| Ok(Box::new(MoveDown)));
    add(MoveNextWordStart::DEF, |_| Ok(Box::new(MoveNextWordStart)));
    add(MoveNextWordEnd::DEF, |_| Ok(Box::new(MoveNextWordEnd)));
    add(MovePrevWordStart::DEF, |_| Ok(Box::new(MovePrevWordStart)));
    add(OpenFile::DEF, |params| {
        let raw = params
            .first()
            .ok_or(ParamError::Missing("path"))?
            .as_string()
            .ok_or(ParamError::WrongKind {
                name: "path",
                expected: ParamKind::String,
            })?;
        Ok(Box::new(OpenFile {
            path: PathBuf::from(raw),
        }))
    });
    add(OpenRun::DEF, |_| Ok(Box::new(OpenRun)));
    add(RunSubmit::DEF, |_| Ok(Box::new(RunSubmit)));
    add(RunInterrupt::DEF, |_| Ok(Box::new(RunInterrupt)));
    add(OpenClaude::DEF, |_| Ok(Box::new(OpenClaude)));
    add(ClaudeSubmit::DEF, |_| Ok(Box::new(ClaudeSubmit)));
    add(ClaudeToPane::DEF, |_| Ok(Box::new(ClaudeToPane)));
    add(ClaudeToDockLeft::DEF, |_| Ok(Box::new(ClaudeToDockLeft)));
    add(ClaudeToDockRight::DEF, |_| Ok(Box::new(ClaudeToDockRight)));
    add(ToggleDockRight::DEF, |_| Ok(Box::new(ToggleDockRight)));
    add(ToggleDockLeft::DEF, |_| Ok(Box::new(ToggleDockLeft)));
    add(OpenCommits::DEF, |_| Ok(Box::new(OpenCommits)));
    add(CloseCommits::DEF, |_| Ok(Box::new(CloseCommits)));
    add(CommitsNext::DEF, |_| Ok(Box::new(CommitsNext)));
    add(CommitsPrev::DEF, |_| Ok(Box::new(CommitsPrev)));
    add(CommitsPageDown::DEF, |_| Ok(Box::new(CommitsPageDown)));
    add(CommitsPageUp::DEF, |_| Ok(Box::new(CommitsPageUp)));
    add(CommitsFirst::DEF, |_| Ok(Box::new(CommitsFirst)));
    add(CommitsLast::DEF, |_| Ok(Box::new(CommitsLast)));
    add(CommitsRefresh::DEF, |_| Ok(Box::new(CommitsRefresh)));
    add(Run::DEF, |params| {
        let raw = params
            .first()
            .ok_or(ParamError::Missing("command"))?
            .as_string()
            .ok_or(ParamError::WrongKind {
                name: "command",
                expected: ParamKind::String,
            })?;
        Ok(Box::new(Run {
            command: raw.to_owned(),
        }))
    });

    map
}

pub fn lookup(name: &str) -> Option<&'static RegistryEntry> {
    REGISTRY.get_or_init(init).get(name)
}

pub fn all() -> impl Iterator<Item = &'static RegistryEntry> {
    REGISTRY.get_or_init(init).values()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZERO_ARG_NAMES: &[&str] = &[
        "Quit",
        "SplitRight",
        "SplitDown",
        "FocusLeft",
        "FocusRight",
        "FocusUp",
        "FocusDown",
        "FocusNext",
        "FocusPrev",
        "ClosePane",
        "OpenCommandPalette",
        "OpenReview",
        "JumpToMoveSource",
        "JumpToMoveTarget",
        "JumpToNextMoveSource",
        "JumpToPrevMoveSource",
        "QueryMoveRelationships",
        "AddSelectionBelow",
        "MoveLeft",
        "MoveRight",
        "MoveUp",
        "MoveDown",
        "MoveNextWordStart",
        "MoveNextWordEnd",
        "MovePrevWordStart",
        "ReviewNextChunk",
        "ReviewPrevChunk",
        "ReviewStageChunk",
        "ReviewUnstageChunk",
        "ReviewToggleStage",
        "ReviewSkipChunk",
        "ReviewRefresh",
        "ReviewApplyStaged",
        "CloseReview",
        "OpenCommits",
        "CloseCommits",
        "CommitsNext",
        "CommitsPrev",
        "CommitsPageDown",
        "CommitsPageUp",
        "CommitsFirst",
        "CommitsLast",
        "CommitsRefresh",
        "OpenRun",
        "RunSubmit",
        "RunInterrupt",
        "OpenClaude",
        "ClaudeSubmit",
        "ClaudeToPane",
        "ClaudeToDockLeft",
        "ClaudeToDockRight",
        "ToggleDockRight",
        "ToggleDockLeft",
    ];

    #[test]
    fn lookup_all_actions() {
        for name in ZERO_ARG_NAMES {
            assert!(lookup(name).is_some(), "missing: {name}");
        }
        assert!(lookup("OpenFile").is_some());
    }

    #[test]
    fn lookup_unknown_returns_none() {
        assert!(lookup("Foo").is_none());
        assert!(lookup("SetMode").is_none());
    }

    #[test]
    fn factory_creates_correct_kind() {
        for name in ZERO_ARG_NAMES {
            let entry = lookup(name).expect(name);
            let action = (entry.create)(&[]).expect(name);
            assert_eq!(action.kind(), entry.def.kind(), "kind mismatch for {name}");
        }
    }

    #[test]
    fn open_file_factory_consumes_path() {
        let entry = lookup("OpenFile").expect("OpenFile");
        let params = vec![ParamValue::String("/tmp/x.rs".into())];
        let action = (entry.create)(&params).expect("create");
        let open = action
            .as_any()
            .downcast_ref::<OpenFile>()
            .expect("downcast");
        assert_eq!(open.path, PathBuf::from("/tmp/x.rs"));
    }

    #[test]
    fn open_file_factory_missing_param_errors() {
        let entry = lookup("OpenFile").expect("OpenFile");
        assert_eq!((entry.create)(&[]).err(), Some(ParamError::Missing("path")));
    }

    #[test]
    fn open_file_factory_wrong_kind_errors() {
        let entry = lookup("OpenFile").expect("OpenFile");
        let err = (entry.create)(&[ParamValue::Number(1.0)]).err();
        assert_eq!(
            err,
            Some(ParamError::WrongKind {
                name: "path",
                expected: ParamKind::String,
            })
        );
    }

    #[test]
    fn all_returns_complete_list() {
        assert_eq!(all().count(), 57);
    }

    #[test]
    fn all_have_descriptions() {
        for entry in all() {
            assert!(!entry.def.short_desc().is_empty(), "{}", entry.def.name());
            assert!(!entry.def.long_desc().is_empty(), "{}", entry.def.name());
        }
    }
}

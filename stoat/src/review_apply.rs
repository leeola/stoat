mod patch;

use crate::review_session::{ChunkStatus, ReviewSession, ReviewSource};
pub(crate) use patch::chunk_to_unified_diff;
use std::path::PathBuf;

/// Action taken on a chunk when a session's staged set is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum AppliedChunk {
    Staged {
        path: PathBuf,
        base_line_start: u32,
        base_line_end: u32,
    },
    Unstaged {
        path: PathBuf,
        base_line_start: u32,
        base_line_end: u32,
    },
    Skipped {
        path: PathBuf,
    },
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub(crate) struct ApplyReport {
    pub applied: Vec<AppliedChunk>,
    pub skipped: usize,
    pub unstaged: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum ApplyError {
    /// Underlying tooling (git, filesystem) reported a failure. The wrapped
    /// message is human-readable and free of secrets.
    Backend(String),
}

/// Dispatch target for turning a session's `Staged` chunks into real
/// changes on the underlying source. Kept as a trait so new sources
/// (commit rewrite, agent edit acceptance) can slot in without touching
/// the action handler.
#[allow(dead_code)]
pub(crate) trait ReviewApplier {
    fn apply(&mut self, session: &ReviewSession) -> Result<ApplyReport, ApplyError>;
}

/// Stub for the git-index applier. When implemented, this will produce a
/// unified-diff patch per staged chunk and feed it to `git apply --cached`
/// via libgit2.
#[allow(dead_code)]
pub(crate) struct WorkingTreeApplier;

impl ReviewApplier for WorkingTreeApplier {
    fn apply(&mut self, _session: &ReviewSession) -> Result<ApplyReport, ApplyError> {
        todo!("git index apply lands in a follow-up phase")
    }
}

#[allow(dead_code)]
pub(crate) fn applier_for(source: &ReviewSource) -> Box<dyn ReviewApplier> {
    match source {
        ReviewSource::WorkingTree { .. } => Box::new(WorkingTreeApplier),
        _ => todo!("apply for this source kind is not yet implemented"),
    }
}

#[allow(dead_code)]
pub(crate) fn summarize_pending(session: &ReviewSession) -> ApplyReport {
    let mut report = ApplyReport::default();
    for id in &session.order {
        let Some(chunk) = session.chunks.get(id) else {
            continue;
        };
        let file = session.files.get(chunk.file_index);
        let path = file.map(|f| f.path.clone()).unwrap_or_default();
        match chunk.status {
            ChunkStatus::Staged => {
                report.applied.push(AppliedChunk::Staged {
                    path,
                    base_line_start: chunk.base_line_range.start,
                    base_line_end: chunk.base_line_range.end,
                });
            },
            ChunkStatus::Unstaged => {
                report.unstaged += 1;
                report.applied.push(AppliedChunk::Unstaged {
                    path,
                    base_line_start: chunk.base_line_range.start,
                    base_line_end: chunk.base_line_range.end,
                });
            },
            ChunkStatus::Skipped => {
                report.skipped += 1;
                report.applied.push(AppliedChunk::Skipped { path });
            },
            ChunkStatus::Pending => {},
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review_session::InMemoryFile;
    use std::sync::Arc;

    fn build_session() -> ReviewSession {
        let mut s = ReviewSession::new(ReviewSource::InMemory {
            files: Arc::new(Vec::<InMemoryFile>::new()),
        });
        s.add_file(
            PathBuf::from("a.txt"),
            "a.txt".into(),
            None,
            Arc::new("a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n".to_string()),
            Arc::new("A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\n".to_string()),
        );
        s
    }

    #[test]
    #[should_panic(expected = "git index apply lands")]
    fn working_tree_applier_panics_as_todo() {
        let session = ReviewSession::new(ReviewSource::WorkingTree {
            workdir: PathBuf::from("/tmp/no-repo"),
        });
        let _ = WorkingTreeApplier.apply(&session);
    }

    #[test]
    #[should_panic(expected = "apply for this source kind")]
    fn in_memory_applier_panics_via_applier_for() {
        let _ = applier_for(&ReviewSource::InMemory {
            files: Arc::new(Vec::<InMemoryFile>::new()),
        });
    }

    #[test]
    fn summarize_pending_counts_and_records() {
        let mut s = build_session();
        let ids: Vec<_> = s.order.clone();
        assert!(ids.len() >= 2);
        s.set_status(ids[0], ChunkStatus::Staged);
        s.set_status(ids[1], ChunkStatus::Unstaged);

        let report = summarize_pending(&s);
        assert_eq!(report.skipped, 0);
        assert_eq!(report.unstaged, 1);
        assert_eq!(report.applied.len(), 2);
        assert!(matches!(report.applied[0], AppliedChunk::Staged { .. }));
        assert!(matches!(report.applied[1], AppliedChunk::Unstaged { .. }));
    }
}

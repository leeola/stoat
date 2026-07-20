use crate::{
    host::ConflictedFile,
    rebase::{ActiveRebase, ConflictResolution, RebaseEntry, RebasePause, RebaseState},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
};

/// Serializable subset of a [`crate::workspace::Workspace`]. Covers the
/// state required to reproduce mid-rebase bugs in v1 (rebase plan,
/// active rebase, UI mode). Expands as more workspace state becomes
/// serializable.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct WorkspaceSnapshot {
    pub rebase: Option<RebaseState>,
    pub rebase_active: Option<ActiveRebaseSnap>,
    /// UI mode string at capture time (`"rebase"`, `"rebase_conflict"`,
    /// `"commits"`, etc.). Restored verbatim on load so the TUI comes
    /// up in the same view.
    pub mode: String,
}

/// Snapshot counterpart of [`ActiveRebase`]. Identical shape except
/// [`Self::pause`] uses the serializable [`RebasePauseSnap`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ActiveRebaseSnap {
    pub workdir: PathBuf,
    pub onto: String,
    pub remaining: VecDeque<RebaseEntry>,
    pub current_head: String,
    pub last_pick_sha: Option<String>,
    pub last_message: Option<String>,
    pub pause: Option<RebasePauseSnap>,
}

/// Serializable variants of [`RebasePause`]. Omits `Reword` because
/// that variant carries `EditorId` / `BufferId` references into
/// workspace-scoped slotmaps whose contents (the in-progress message
/// buffer) are not yet captured in the snapshot pipeline. Callers that
/// encounter a live `Reword` pause should record it in
/// `DumpMeta.dropped_fields` and set the snapshot `pause` to `None`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum RebasePauseSnap {
    Edit {
        cherry_picked_commit: String,
    },
    Conflict {
        source_sha: String,
        files: Vec<ConflictedFile>,
        selected: usize,
        resolutions: HashMap<PathBuf, ConflictResolution>,
    },
}

/// Conversion outcome for [`ActiveRebaseSnap::from_active`]: the
/// snapshot plus any dropped-field markers the caller should propagate
/// up into [`crate::dump::DumpMeta::dropped_fields`].
pub(crate) struct ActiveRebaseCapture {
    pub snap: ActiveRebaseSnap,
    pub dropped: Vec<String>,
}

impl ActiveRebaseSnap {
    /// Build a snapshot from a live [`ActiveRebase`]. When the pause is
    /// `Reword` the snapshot keeps the rest of the execution state but
    /// drops the pause (returned in [`ActiveRebaseCapture::dropped`])
    /// because the reword editor/buffer pair is not currently
    /// serializable.
    pub fn from_active(active: &ActiveRebase) -> ActiveRebaseCapture {
        let mut dropped = Vec::new();
        let pause = match active.pause.as_ref() {
            None => None,
            Some(RebasePause::Edit {
                cherry_picked_commit,
            }) => Some(RebasePauseSnap::Edit {
                cherry_picked_commit: cherry_picked_commit.clone(),
            }),
            Some(RebasePause::Conflict {
                source_sha,
                files,
                selected,
                resolutions,
            }) => Some(RebasePauseSnap::Conflict {
                source_sha: source_sha.clone(),
                files: files.clone(),
                selected: *selected,
                resolutions: resolutions.clone(),
            }),
            Some(RebasePause::Reword { .. }) => {
                dropped.push("rebase_active.pause.reword".to_string());
                None
            },
        };
        ActiveRebaseCapture {
            snap: Self {
                workdir: active.workdir.clone(),
                onto: active.onto.clone(),
                remaining: active.remaining.clone(),
                current_head: active.current_head.clone(),
                last_pick_sha: active.last_pick_sha.clone(),
                last_message: active.last_message.clone(),
                pause,
            },
            dropped,
        }
    }

    pub fn into_active(self) -> ActiveRebase {
        ActiveRebase {
            workdir: self.workdir,
            onto: self.onto,
            remaining: self.remaining,
            current_head: self.current_head,
            last_pick_sha: self.last_pick_sha,
            last_message: self.last_message,
            pause: self.pause.map(|p| p.into_pause()),
        }
    }
}

impl RebasePauseSnap {
    fn into_pause(self) -> RebasePause {
        match self {
            Self::Edit {
                cherry_picked_commit,
            } => RebasePause::Edit {
                cherry_picked_commit,
            },
            Self::Conflict {
                source_sha,
                files,
                selected,
                resolutions,
            } => RebasePause::Conflict {
                source_sha,
                files,
                selected,
                resolutions,
            },
        }
    }
}

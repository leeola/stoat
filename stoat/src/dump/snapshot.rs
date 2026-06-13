use crate::{
    host::ConflictedFile,
    rebase::{ConflictResolution, RebaseEntry, RebaseState},
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
    /// UI mode string at capture time (`"rebase"`, `"conflict"`,
    /// `"commits"`, etc.). Restored verbatim on load so the TUI comes
    /// up in the same view.
    pub mode: String,
}

/// Snapshot counterpart of `ActiveRebase`. Identical shape except
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

/// Serializable variants of `RebasePause`. Omits `Reword` because
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

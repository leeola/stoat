use super::{persist::WorkspaceStateV1, WorkspaceUid};
use crate::host::FsHost;
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// Lightweight per-workspace metadata written beside each `<uid>.ron` state file
/// as a `<uid>.meta` sidecar.
///
/// Sidecars let a workspace finder and cross-workspace search list every
/// persisted workspace without parsing the heavy state RON, which carries full
/// buffer op logs. The `.meta` extension keeps them invisible to the
/// `extension == "ron"` state scans, so a resume never mistakes one for a state
/// file.
///
/// One wart is the single-instance assumption. Workspace status (active,
/// background, inactive) is derived in-process against the one running instance
/// at listing time. There is no cross-process liveness tracking, so a second
/// concurrent instance would see the other's open workspaces as inactive. Stoat
/// assumes one instance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkspaceMeta {
    pub uid: WorkspaceUid,
    pub name: String,
    pub git_root: PathBuf,
    pub buffer_count: usize,
}

/// A persisted workspace discovered by [`list_all`], pairing its metadata with
/// the state file it describes and that file's modification time.
///
/// The listing is consumed by the workspace picker and cross-workspace search.
#[derive(Clone, Debug)]
pub(crate) struct RegistryEntry {
    pub meta: WorkspaceMeta,
    pub state_path: PathBuf,
    pub mtime: SystemTime,
}

/// The sidecar path for a `<uid>.ron` state file, its path with a `.meta`
/// extension.
pub(crate) fn meta_path_for(state_path: &Path) -> PathBuf {
    state_path.with_extension("meta")
}

/// Write `meta` as the sidecar for `state_path`, atomically via a tmp+rename.
pub(crate) fn write_meta(
    meta: &WorkspaceMeta,
    state_path: &Path,
    fs: &dyn FsHost,
) -> io::Result<()> {
    let path = meta_path_for(state_path);
    let body = ron::ser::to_string_pretty(meta, ron::ser::PrettyConfig::default())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let tmp = path.with_extension("meta.tmp");
    fs.write(&tmp, body.as_bytes())?;
    fs.rename(&tmp, &path)?;
    Ok(())
}

/// List every persisted workspace across all git roots, newest state file first.
///
/// Reads each `<uid>.meta` sidecar under the workspace state directory. A legacy
/// `<uid>.ron` with no sidecar is backfilled by parsing its state (metadata
/// only, no op-log replay) and writing the sidecar.
pub(crate) fn list_all(fs: &dyn FsHost) -> io::Result<Vec<RegistryEntry>> {
    list_all_in(&stoat_log::workspace_state_dir()?, fs)
}

/// [`list_all`] against an explicit workspace-state directory so tests can run
/// it over a tempdir.
fn list_all_in(workspaces_dir: &Path, fs: &dyn FsHost) -> io::Result<Vec<RegistryEntry>> {
    if !fs.exists(workspaces_dir) {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    for root_entry in fs.list_dir(workspaces_dir)? {
        if !root_entry.is_dir {
            continue;
        }
        let root_dir = workspaces_dir.join(root_entry.name.as_str());
        for entry in fs.list_dir(&root_dir)? {
            let state_path = root_dir.join(entry.name.as_str());
            if state_path.extension().and_then(|s| s.to_str()) != Some("ron") {
                continue;
            }
            let Some(meta) = read_or_backfill(&state_path, fs) else {
                continue;
            };
            let mtime = fs
                .metadata(&state_path)
                .ok()
                .flatten()
                .map(|m| m.modified)
                .unwrap_or(UNIX_EPOCH);
            entries.push(RegistryEntry {
                meta,
                state_path,
                mtime,
            });
        }
    }

    entries.sort_by_key(|e| std::cmp::Reverse(e.mtime));
    Ok(entries)
}

/// Read the sidecar for `state_path`, or backfill it from the state file when
/// the sidecar is absent or unreadable. `None` when neither parses.
fn read_or_backfill(state_path: &Path, fs: &dyn FsHost) -> Option<WorkspaceMeta> {
    let meta_path = meta_path_for(state_path);
    if fs.exists(&meta_path)
        && let Some(meta) = read_meta(&meta_path, fs)
    {
        return Some(meta);
    }
    let meta = meta_from_state(state_path, fs)?;
    let _ = write_meta(&meta, state_path, fs);
    Some(meta)
}

fn read_meta(path: &Path, fs: &dyn FsHost) -> Option<WorkspaceMeta> {
    let mut buf = Vec::new();
    fs.read(path, &mut buf).ok()?;
    let body = String::from_utf8(buf).ok()?;
    ron::from_str(&body).ok()
}

fn meta_from_state(state_path: &Path, fs: &dyn FsHost) -> Option<WorkspaceMeta> {
    let mut buf = Vec::new();
    fs.read(state_path, &mut buf).ok()?;
    let body = String::from_utf8(buf).ok()?;
    let state: WorkspaceStateV1 = ron::from_str(&body).ok()?;
    Some(WorkspaceMeta {
        uid: state.uid,
        name: state.name,
        git_root: state.git_root,
        buffer_count: state.buffers.entries.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{host::FakeFs, workspace::Workspace};
    use std::sync::Arc;
    use stoat_scheduler::TestScheduler;

    #[test]
    fn save_writes_a_meta_sidecar_invisible_to_ron_scans() {
        let fake = FakeFs::new();
        let exec = Arc::new(TestScheduler::new()).executor();
        let git_root = PathBuf::from("/proj");
        let ws = Workspace::new(git_root.clone(), &exec);

        let state_path = PathBuf::from("/state/hash/7.ron");
        ws.save_state(&state_path, &fake).unwrap();

        let meta_path = meta_path_for(&state_path);
        assert_eq!(
            meta_path.extension().and_then(|s| s.to_str()),
            Some("meta"),
            "the sidecar hides from `extension == ron` scans"
        );
        let meta = read_meta(&meta_path, &fake).expect("the sidecar parses");
        assert_eq!(meta.git_root, git_root);
        assert_eq!(meta.name, ws.name);
        assert_eq!(meta.buffer_count, ws.buffers.len());
    }

    #[test]
    fn list_all_merges_roots_and_backfills_a_legacy_state_file() {
        let fake = FakeFs::new();
        let exec = Arc::new(TestScheduler::new()).executor();
        let dir = PathBuf::from("/state");

        let a = dir.join("hashA").join("1.ron");
        Workspace::new(PathBuf::from("/proj-a"), &exec)
            .save_state(&a, &fake)
            .unwrap();

        let b = dir.join("hashB").join("2.ron");
        Workspace::new(PathBuf::from("/proj-b"), &exec)
            .save_state(&b, &fake)
            .unwrap();
        fake.remove_file(&meta_path_for(&b)).unwrap();
        assert!(
            !fake.exists(&meta_path_for(&b)),
            "the legacy file starts without a sidecar"
        );

        let entries = list_all_in(&dir, &fake).unwrap();

        let mut roots: Vec<_> = entries.iter().map(|e| e.meta.git_root.clone()).collect();
        roots.sort();
        assert_eq!(
            roots,
            vec![PathBuf::from("/proj-a"), PathBuf::from("/proj-b")],
            "both roots merge into one listing"
        );
        assert!(
            fake.exists(&meta_path_for(&b)),
            "the legacy file's sidecar is backfilled"
        );
    }
}

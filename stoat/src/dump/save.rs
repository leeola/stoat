use super::{
    bundle,
    meta::DumpMeta,
    snapshot::{ActiveRebaseSnap, WorkspaceSnapshot},
    walker, CreateDirSnafu, DumpError, DumpId, RonSnafu, WriteDumpSnafu,
};
use crate::{host::FsHost, workspace::Workspace};
use snafu::ResultExt;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

/// A dump whose workspace snapshot is taken and whose tree is not yet read.
///
/// The snapshot reads editor state, so it is taken on the run loop when the
/// command runs. The write reads the whole tree, so the caller sends it off the
/// run loop, and the bundle still holds the workspace as it stood at the
/// command.
pub(crate) struct PendingDump {
    id: DumpId,
    meta: DumpMeta,
    dumps: PathBuf,
}

impl PendingDump {
    /// Write the bundle to `<dumps>/<id>.dump`, creating the directory, and
    /// return the dump's id.
    ///
    /// The bundle holds the working tree (respecting `.gitignore`), the
    /// `.git/` directory, the `.stoat/` directory (if present), and the
    /// metadata, which the load path extracts to `.stoat/dump.ron`. Every file
    /// under the git root is read, which takes seconds on a large repository.
    pub(crate) fn write(self, fs: &dyn FsHost) -> Result<DumpId, DumpError> {
        fs.create_dir_all(&self.dumps)
            .with_context(|_| CreateDirSnafu {
                path: self.dumps.clone(),
            })?;
        let meta_ron = self.meta.to_ron().map_err(|e| {
            RonSnafu {
                reason: e.to_string(),
            }
            .build()
        })?;

        let archive_path = self.dumps.join(self.id.filename());
        write_bundle(&meta_ron, &self.meta.git_root, &archive_path, fs)?;
        Ok(self.id)
    }
}

/// Snapshot `workspace` in UI `mode` for a dump named `name` at `at`, bound
/// for the directory `dumps`.
///
/// Reads no file. The run loop pays for clones of the rebase state and the
/// mode, and [`PendingDump::write`] does the rest.
pub(crate) fn capture(
    workspace: &Workspace,
    mode: &str,
    name: &str,
    at: OffsetDateTime,
    dumps: &Path,
) -> Result<PendingDump, DumpError> {
    let id = DumpId::new(name, at)?;
    let meta = capture_meta(workspace, mode, &id, at);
    Ok(PendingDump {
        id,
        meta,
        dumps: dumps.to_path_buf(),
    })
}

/// The metadata of the dump `id`, taken at `at`, of `workspace` in UI `mode`.
///
/// Holds the serializable subset of the workspace (the rebase plan and the
/// active rebase) and names the fields a dump drops.
fn capture_meta(workspace: &Workspace, mode: &str, id: &DumpId, at: OffsetDateTime) -> DumpMeta {
    let (snapshot, snapshot_dropped) = build_snapshot(workspace, mode);
    let mut dropped_fields = dropped_fields_for(workspace);
    dropped_fields.extend(snapshot_dropped);

    DumpMeta {
        created_at: at,
        name: id.name().unwrap_or("").to_string(),
        stoat_version: env!("CARGO_PKG_VERSION").to_string(),
        git_root: workspace.git_root.clone(),
        dropped_fields,
        workspace: snapshot,
    }
}

/// Write a bundle of `meta_ron` and the tree under `git_root` to
/// `archive_path`.
fn write_bundle(
    meta_ron: &str,
    git_root: &Path,
    archive_path: &Path,
    fs: &dyn FsHost,
) -> Result<(), DumpError> {
    let paths = walker::gather_workspace_files(fs, git_root)?;
    let bundle_bytes = bundle::serialize(meta_ron, git_root, &paths, fs)?;
    fs.write(archive_path, &bundle_bytes)
        .with_context(|_| WriteDumpSnafu {
            path: archive_path.to_path_buf(),
        })
}

fn dropped_fields_for(workspace: &Workspace) -> Vec<String> {
    let mut dropped = Vec::new();
    if !workspace.runs.is_empty() {
        dropped.push("runs".to_string());
    }
    if !workspace.docks.is_empty() {
        dropped.push("docks".to_string());
    }
    dropped.push("buffers".to_string());
    dropped.push("editors".to_string());
    dropped.push("panes".to_string());
    if workspace.commits.is_some() {
        dropped.push("commits".to_string());
    }
    dropped
}

fn build_snapshot(workspace: &Workspace, mode: &str) -> (WorkspaceSnapshot, Vec<String>) {
    let mut dropped = Vec::new();
    let rebase_active = workspace.rebase_active.as_ref().map(|active| {
        let capture = ActiveRebaseSnap::from_active(active);
        dropped.extend(capture.dropped);
        capture.snap
    });
    let snapshot = WorkspaceSnapshot {
        rebase: workspace.rebase.clone(),
        rebase_active,
        mode: mode.to_string(),
    };
    (snapshot, dropped)
}

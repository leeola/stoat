use super::{
    bundle, dumps_dir,
    meta::DumpMeta,
    snapshot::{ActiveRebaseSnap, WorkspaceSnapshot},
    walker, CreateDirSnafu, DumpError, DumpId, RonSnafu, WriteDumpSnafu,
};
use crate::{app::Stoat, host::FsHost, workspace::Workspace};
use snafu::ResultExt;
use std::path::Path;
use time::OffsetDateTime;

/// Write a dump bundle to `<XDG_DATA_HOME>/stoat/dumps/<id>.dump`.
///
/// Captures the working tree (respecting `.gitignore`), the `.git/`
/// directory, the `.stoat/` directory (if present), and a
/// `.stoat/dump.ron` file with metadata plus the serializable subset of
/// the active workspace (rebase plan + active rebase).
pub fn save_at(
    stoat: &Stoat,
    name: &str,
    at: OffsetDateTime,
    fs: &dyn FsHost,
) -> Result<DumpId, DumpError> {
    let id = DumpId::new(name, at)?;
    let dumps = dumps_dir()?;
    fs.create_dir_all(&dumps).with_context(|_| CreateDirSnafu {
        path: dumps.clone(),
    })?;
    let archive_path = dumps.join(id.filename());
    write_archive(
        stoat.active_workspace(),
        &stoat.mode,
        &id,
        at,
        &archive_path,
        fs,
    )?;
    Ok(id)
}

/// Low-level writer: produce a dump bundle at the exact path
/// `archive_path` from `workspace` plus the current UI `mode`. Splits
/// the IO-bound work out of [`save_at`] so callers that already know
/// where the bundle should go (tests, internal replay tooling) can
/// bypass [`dumps_dir`].
pub(crate) fn write_archive(
    workspace: &Workspace,
    mode: &str,
    id: &DumpId,
    at: OffsetDateTime,
    archive_path: &Path,
    fs: &dyn FsHost,
) -> Result<(), DumpError> {
    let (snapshot, snapshot_dropped) = build_snapshot(workspace, mode);
    let mut dropped_fields = dropped_fields_for(workspace);
    dropped_fields.extend(snapshot_dropped);

    let meta = DumpMeta {
        created_at: at,
        name: id.name().unwrap_or("").to_string(),
        stoat_version: env!("CARGO_PKG_VERSION").to_string(),
        git_root: workspace.git_root.clone(),
        dropped_fields,
        workspace: snapshot,
    };
    write_meta_and_tree(&meta, archive_path, fs)
}

/// Write a dump bundle for a workspace with no TUI [`Stoat`] state to
/// snapshot, such as the GUI. Records the working tree under `git_root`
/// plus a minimal [`DumpMeta`]: the UI `mode` at capture time and no
/// rebase plan. `dropped_fields` notes the runtime workspace content a
/// directory-only dump does not preserve.
pub(crate) fn write_workspace_dir_archive(
    git_root: &Path,
    mode: &str,
    id: &DumpId,
    at: OffsetDateTime,
    archive_path: &Path,
    fs: &dyn FsHost,
) -> Result<(), DumpError> {
    let meta = DumpMeta {
        created_at: at,
        name: id.name().unwrap_or("").to_string(),
        stoat_version: env!("CARGO_PKG_VERSION").to_string(),
        git_root: git_root.to_path_buf(),
        dropped_fields: vec![
            "buffers".to_string(),
            "editors".to_string(),
            "panes".to_string(),
            "docks".to_string(),
        ],
        workspace: WorkspaceSnapshot {
            rebase: None,
            rebase_active: None,
            mode: mode.to_string(),
        },
    };
    write_meta_and_tree(&meta, archive_path, fs)
}

/// Serialize `meta` to RON, gather the working tree under
/// `meta.git_root` (force-including `.git`/`.stoat`), and write the
/// framed bundle to `archive_path`. Shared tail of [`write_archive`]
/// and [`write_workspace_dir_archive`].
fn write_meta_and_tree(
    meta: &DumpMeta,
    archive_path: &Path,
    fs: &dyn FsHost,
) -> Result<(), DumpError> {
    let meta_ron = meta.to_ron().map_err(|e| {
        RonSnafu {
            reason: e.to_string(),
        }
        .build()
    })?;

    let entries = walker::gather_workspace_files(fs, &meta.git_root)?;

    let bundle_bytes = bundle::serialize(&meta_ron, &entries)?;
    fs.write(archive_path, &bundle_bytes)
        .with_context(|_| WriteDumpSnafu {
            path: archive_path.to_path_buf(),
        })?;
    Ok(())
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
    if workspace.review.is_some() {
        dropped.push("review".to_string());
    }
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

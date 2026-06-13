use super::{
    bundle, meta::DumpMeta, snapshot::WorkspaceSnapshot, walker, DumpError, DumpId, RonSnafu,
    WriteDumpSnafu,
};
use crate::host::FsHost;
use snafu::ResultExt;
use std::path::Path;
use time::OffsetDateTime;

/// Write a dump bundle for a workspace with no in-memory editor state to
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
/// framed bundle to `archive_path`. Shared tail of
/// [`write_workspace_dir_archive`].
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

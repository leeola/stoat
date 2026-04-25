use super::{
    dumps_dir,
    meta::{DumpMeta, META_PATH_IN_ARCHIVE},
    snapshot::{ActiveRebaseSnap, WorkspaceSnapshot},
    DumpError, DumpId,
};
use crate::{app::Stoat, host::FsHost, workspace::Workspace};
use ignore::WalkBuilder;
use std::{
    collections::HashSet,
    fs::File,
    path::{Path, PathBuf},
};
use tar::{Builder, Header};
use time::OffsetDateTime;
use walkdir::WalkDir;
use zstd::Encoder;

const ZSTD_LEVEL: i32 = 3;

const FORCE_INCLUDE_DIRS: &[&str] = &[".git", ".stoat"];

/// Write a dump archive to `<XDG_DATA_HOME>/stoat/dumps/<id>.tar.zst`.
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
    fs.create_dir_all(&dumps)?;
    let archive_path = dumps.join(id.filename());
    write_archive(
        stoat.active_workspace(),
        &stoat.mode,
        &id,
        at,
        &archive_path,
    )?;
    Ok(id)
}

/// Low-level writer: produce a dump archive at the exact path `archive_path`
/// from `workspace` plus the current UI `mode`. Splits the IO-bound work
/// out of [`save_at`] so callers that already know where the archive
/// should go (tests, internal replay tooling) can bypass [`dumps_dir`].
pub(crate) fn write_archive(
    workspace: &Workspace,
    mode: &str,
    id: &DumpId,
    at: OffsetDateTime,
    archive_path: &Path,
) -> Result<(), DumpError> {
    let sanitized_name = id.name().unwrap_or("").to_string();
    let git_root = workspace.git_root.clone();

    let (snapshot, snapshot_dropped) = build_snapshot(workspace, mode);
    let mut dropped_fields = dropped_fields_for(workspace);
    dropped_fields.extend(snapshot_dropped);

    let meta = DumpMeta {
        created_at: at,
        name: sanitized_name,
        stoat_version: env!("CARGO_PKG_VERSION").to_string(),
        git_root: git_root.clone(),
        dropped_fields,
        workspace: snapshot,
    };
    let meta_ron = meta.to_ron().map_err(|e| DumpError::Ron(e.to_string()))?;

    let output = File::create(archive_path)?;
    let mut encoder = Encoder::new(output, ZSTD_LEVEL).map_err(DumpError::Io)?;
    {
        let mut tar = Builder::new(&mut encoder);
        tar.follow_symlinks(false);

        let mut added: HashSet<PathBuf> = HashSet::new();

        let walker = WalkBuilder::new(&git_root)
            .hidden(false)
            .git_ignore(true)
            .git_exclude(true)
            .git_global(true)
            .require_git(false)
            .build();
        for entry in walker.flatten() {
            let path = entry.path();
            if path == git_root {
                continue;
            }
            let rel = match path.strip_prefix(&git_root) {
                Ok(r) => r.to_path_buf(),
                Err(_) => continue,
            };
            if FORCE_INCLUDE_DIRS.iter().any(|top| rel.starts_with(top)) {
                continue;
            }
            let ft = match entry.file_type() {
                Some(ft) => ft,
                None => continue,
            };
            if ft.is_file() && !added.contains(&rel) {
                tar.append_path_with_name(path, &rel)?;
                added.insert(rel);
            }
        }

        for top in FORCE_INCLUDE_DIRS {
            let src = git_root.join(top);
            if !src.exists() {
                continue;
            }
            for entry in WalkDir::new(&src).follow_links(false).into_iter().flatten() {
                if !entry.file_type().is_file() {
                    continue;
                }
                let rel = match entry.path().strip_prefix(&git_root) {
                    Ok(r) => r.to_path_buf(),
                    Err(_) => continue,
                };
                if added.contains(&rel) {
                    continue;
                }
                tar.append_path_with_name(entry.path(), &rel)?;
                added.insert(rel);
            }
        }

        let meta_bytes = meta_ron.as_bytes();
        let mut header = Header::new_gnu();
        header
            .set_path(META_PATH_IN_ARCHIVE)
            .map_err(DumpError::Io)?;
        header.set_size(meta_bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(unix_secs(at));
        header.set_cksum();
        tar.append(&header, meta_bytes)?;

        tar.finish()?;
    }
    encoder.finish().map_err(DumpError::Io)?;
    Ok(())
}

fn unix_secs(at: OffsetDateTime) -> u64 {
    let secs = at.unix_timestamp();
    if secs < 0 {
        0
    } else {
        secs as u64
    }
}

fn dropped_fields_for(workspace: &Workspace) -> Vec<String> {
    let mut dropped = Vec::new();
    if !workspace.runs.is_empty() {
        dropped.push("runs".to_string());
    }
    if !workspace.chats.is_empty() {
        dropped.push("chats".to_string());
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

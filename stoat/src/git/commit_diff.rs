use crate::git::{provider::GitProvider, repository::CommitFileChange, status::DiffPreviewData};
use std::{path::PathBuf, sync::Arc};

pub async fn load_commit_files(
    git: Arc<dyn GitProvider>,
    repo_path: PathBuf,
    oid_hex: String,
) -> Option<Vec<CommitFileChange>> {
    smol::unblock(move || {
        let repo = git.open(&repo_path).ok()?;
        repo.commit_files_by_oid(&oid_hex).ok()
    })
    .await
}

pub async fn load_commit_file_diff(
    git: Arc<dyn GitProvider>,
    repo_path: PathBuf,
    oid_hex: String,
    file_path: PathBuf,
) -> Option<DiffPreviewData> {
    smol::unblock(move || {
        let repo = git.open(&repo_path).ok()?;
        let text = repo.commit_file_diff(&oid_hex, &file_path).ok()?;
        if text.is_empty() {
            None
        } else {
            Some(DiffPreviewData::new(text))
        }
    })
    .await
}

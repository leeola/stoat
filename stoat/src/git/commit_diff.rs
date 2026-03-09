use crate::git::{provider::GitProvider, repository::CommitFileChange, status::DiffPreviewData};
use std::{path::PathBuf, sync::Arc};

pub async fn load_commit_files(
    git: Arc<dyn GitProvider>,
    repo_path: PathBuf,
    oid_hex: String,
) -> Option<Vec<CommitFileChange>> {
    let repo = git.open(&repo_path).await.ok()?;
    repo.commit_files_by_oid(&oid_hex).await.ok()
}

pub async fn load_commit_file_diff(
    git: Arc<dyn GitProvider>,
    repo_path: PathBuf,
    oid_hex: String,
    file_path: PathBuf,
) -> Option<DiffPreviewData> {
    let repo = git.open(&repo_path).await.ok()?;
    let text = repo.commit_file_diff(&oid_hex, &file_path).await.ok()?;
    if text.is_empty() {
        None
    } else {
        Some(DiffPreviewData::new(text))
    }
}

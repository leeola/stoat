use git2::{Patch, Repository, Status};
use std::{
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiffStatus {
    #[default]
    Unchanged,
    Added,
    Modified,
}

#[derive(Clone, Debug)]
pub struct DeletedHunk {
    pub after_buffer_line: u32,
    pub base_byte_range: Range<usize>,
    pub line_count: u32,
}

#[derive(Clone, Debug, Default)]
pub struct BufferDiff {
    line_status: HashMap<u32, DiffStatus>,
    base_text: Option<Arc<String>>,
    deleted_hunks: Vec<DeletedHunk>,
}

impl BufferDiff {
    pub fn status_for_line(&self, line: u32) -> DiffStatus {
        self.line_status.get(&line).copied().unwrap_or_default()
    }

    pub fn has_deletion_after(&self, line: u32) -> bool {
        self.deleted_hunks
            .iter()
            .any(|h| h.after_buffer_line == line)
    }

    pub fn deleted_hunks(&self) -> &[DeletedHunk] {
        &self.deleted_hunks
    }

    pub fn base_text(&self) -> Option<&Arc<String>> {
        self.base_text.as_ref()
    }

    pub fn deleted_content(&self, hunk: &DeletedHunk) -> &str {
        self.base_text
            .as_ref()
            .map(|t| &t[hunk.base_byte_range.clone()])
            .unwrap_or("")
    }

    pub fn total_deleted_lines(&self) -> u32 {
        self.deleted_hunks.iter().map(|h| h.line_count).sum()
    }

    #[cfg(test)]
    pub fn set_base_text(&mut self, text: Arc<String>) {
        self.base_text = Some(text);
    }

    #[cfg(test)]
    pub fn add_deleted_hunk(&mut self, hunk: DeletedHunk) {
        self.deleted_hunks.push(hunk);
    }
}

pub fn query_diff(file_path: &Path) -> Option<BufferDiff> {
    let repo = Repository::discover(file_path).ok()?;
    let head_content = Arc::new(load_head_content(&repo, file_path)?);
    let working_content = std::fs::read_to_string(file_path).ok()?;
    compute_diff(head_content, &working_content)
}

pub fn modified_files(cwd: &Path) -> Option<Vec<PathBuf>> {
    let repo = Repository::discover(cwd).ok()?;
    let workdir = repo.workdir()?.to_path_buf();
    let statuses = repo.statuses(None).ok()?;

    let dominated_status =
        Status::INDEX_NEW | Status::INDEX_MODIFIED | Status::WT_NEW | Status::WT_MODIFIED;

    let files: Vec<PathBuf> = statuses
        .iter()
        .filter(|entry| entry.status().intersects(dominated_status))
        .filter_map(|entry| entry.path().map(|p| workdir.join(p)))
        .collect();

    if files.is_empty() {
        None
    } else {
        Some(files)
    }
}

fn load_head_content(repo: &Repository, file_path: &Path) -> Option<String> {
    let workdir = repo.workdir()?.canonicalize().ok()?;
    let canonical_file = file_path.canonicalize().ok()?;
    let relative_path = canonical_file.strip_prefix(&workdir).ok()?;

    let head = repo.head().ok()?;
    let tree = head.peel_to_tree().ok()?;
    let entry = tree.get_path(relative_path).ok()?;
    let blob = repo.find_blob(entry.id()).ok()?;
    let content = std::str::from_utf8(blob.content()).ok()?;
    Some(content.to_string())
}

fn compute_diff(old: Arc<String>, new: &str) -> Option<BufferDiff> {
    let patch = Patch::from_buffers(old.as_bytes(), None, new.as_bytes(), None, None).ok()?;

    let mut diff = BufferDiff {
        base_text: Some(old.clone()),
        ..Default::default()
    };

    for hunk_idx in 0..patch.num_hunks() {
        let (hunk, num_lines) = patch.hunk(hunk_idx).ok()?;
        let new_start = hunk.new_start();

        let mut new_line = new_start;
        let mut pending_deletion_start: Option<usize> = None;
        let mut pending_deletion_after: u32 = 0;
        let mut pending_deletion_line_count: u32 = 0;

        for line_idx in 0..num_lines {
            let line = patch.line_in_hunk(hunk_idx, line_idx).ok()?;
            match line.origin() {
                '+' => {
                    flush_pending_deletion(
                        &mut diff,
                        &mut pending_deletion_start,
                        pending_deletion_after,
                        pending_deletion_line_count,
                        &old,
                        line.old_lineno(),
                    );
                    pending_deletion_line_count = 0;
                    diff.line_status.insert(new_line - 1, DiffStatus::Added);
                    new_line += 1;
                },
                '-' => {
                    let content_start = line.content_offset() as usize;
                    if pending_deletion_start.is_none() {
                        pending_deletion_start = Some(content_start);
                        pending_deletion_after = if new_line > 1 { new_line - 2 } else { 0 };
                    }
                    pending_deletion_line_count += 1;
                },
                ' ' => {
                    flush_pending_deletion(
                        &mut diff,
                        &mut pending_deletion_start,
                        pending_deletion_after,
                        pending_deletion_line_count,
                        &old,
                        line.old_lineno(),
                    );
                    pending_deletion_line_count = 0;
                    new_line += 1;
                },
                _ => {},
            }
        }

        flush_pending_deletion(
            &mut diff,
            &mut pending_deletion_start,
            pending_deletion_after,
            pending_deletion_line_count,
            &old,
            None,
        );
    }

    if diff.line_status.is_empty() && diff.deleted_hunks.is_empty() {
        return None;
    }

    Some(diff)
}

fn flush_pending_deletion(
    diff: &mut BufferDiff,
    pending_start: &mut Option<usize>,
    after_line: u32,
    line_count: u32,
    old: &str,
    next_old_lineno: Option<u32>,
) {
    if let Some(start) = pending_start.take() {
        if line_count > 0 {
            let end = if let Some(lineno) = next_old_lineno {
                line_byte_offset(old, lineno - 1)
            } else {
                old.len()
            };
            let mut byte_end = end;
            while byte_end > start && old.as_bytes().get(byte_end - 1) == Some(&b'\n') {
                byte_end -= 1;
            }
            diff.deleted_hunks.push(DeletedHunk {
                after_buffer_line: after_line,
                base_byte_range: start..byte_end,
                line_count,
            });
        }
    }
}

fn line_byte_offset(text: &str, line: u32) -> usize {
    text.lines().take(line as usize).map(|l| l.len() + 1).sum()
}

#[cfg(test)]
mod tests {
    use super::{query_diff, DiffStatus};
    use git2::{Repository, Signature};
    use std::{fs, process::Command};
    use tempfile::TempDir;

    fn create_test_repo() -> (TempDir, Repository) {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();

        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        (dir, repo)
    }

    fn commit_file(repo: &Repository, path: &std::path::Path, content: &str) {
        fs::write(path, content).unwrap();
        let mut index = repo.index().unwrap();
        let workdir = repo.workdir().unwrap().canonicalize().unwrap();
        let canonical_path = path.canonicalize().unwrap();
        index
            .add_path(canonical_path.strip_prefix(&workdir).unwrap())
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = Signature::now("Test", "test@test.com").unwrap();

        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<_> = parent.iter().collect();

        repo.commit(Some("HEAD"), &sig, &sig, "commit", &tree, &parents)
            .unwrap();
    }

    #[test]
    fn detects_added_lines() {
        let (dir, repo) = create_test_repo();
        let file_path = dir.path().join("test.txt");

        commit_file(&repo, &file_path, "line1\nline2\n");
        fs::write(&file_path, "line1\nline2\nnew line\n").unwrap();

        let diff = query_diff(&file_path).unwrap();
        assert_eq!(diff.status_for_line(0), DiffStatus::Unchanged);
        assert_eq!(diff.status_for_line(1), DiffStatus::Unchanged);
        assert_eq!(diff.status_for_line(2), DiffStatus::Added);
    }

    #[test]
    fn detects_deleted_lines() {
        let (dir, repo) = create_test_repo();
        let file_path = dir.path().join("test.txt");

        commit_file(&repo, &file_path, "line1\nline2\nline3\n");
        fs::write(&file_path, "line1\nline3\n").unwrap();

        let diff = query_diff(&file_path).unwrap();
        assert!(diff.has_deletion_after(0));
    }

    #[test]
    fn no_diff_for_unchanged() {
        let (dir, repo) = create_test_repo();
        let file_path = dir.path().join("test.txt");

        commit_file(&repo, &file_path, "line1\nline2\n");

        let diff = query_diff(&file_path);
        assert!(diff.is_none());
    }

    #[test]
    fn no_diff_for_non_repo() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "content").unwrap();

        let diff = query_diff(&file_path);
        assert!(diff.is_none());
    }
}

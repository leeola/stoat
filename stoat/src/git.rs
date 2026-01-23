use git2::{Patch, Repository};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiffStatus {
    #[default]
    Unchanged,
    Added,
    Modified,
}

#[derive(Clone, Debug, Default)]
pub struct BufferDiff {
    line_status: HashMap<u32, DiffStatus>,
    deletion_markers: HashSet<u32>,
}

impl BufferDiff {
    pub fn status_for_line(&self, line: u32) -> DiffStatus {
        self.line_status.get(&line).copied().unwrap_or_default()
    }

    pub fn has_deletion_after(&self, line: u32) -> bool {
        self.deletion_markers.contains(&line)
    }
}

pub fn query_diff(file_path: &Path) -> Option<BufferDiff> {
    let repo = Repository::discover(file_path).ok()?;
    let head_content = load_head_content(&repo, file_path)?;
    let working_content = std::fs::read_to_string(file_path).ok()?;
    compute_diff(&head_content, &working_content)
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

fn compute_diff(old: &str, new: &str) -> Option<BufferDiff> {
    let patch = Patch::from_buffers(old.as_bytes(), None, new.as_bytes(), None, None).ok()?;

    let mut diff = BufferDiff::default();

    for hunk_idx in 0..patch.num_hunks() {
        let (hunk, num_lines) = patch.hunk(hunk_idx).ok()?;
        let new_start = hunk.new_start();

        let mut new_line = new_start;
        for line_idx in 0..num_lines {
            let line = patch.line_in_hunk(hunk_idx, line_idx).ok()?;
            match line.origin() {
                '+' => {
                    diff.line_status.insert(new_line - 1, DiffStatus::Added);
                    new_line += 1;
                },
                '-' => {
                    if new_line > 1 {
                        diff.deletion_markers.insert(new_line - 2);
                    } else {
                        diff.deletion_markers.insert(0);
                    }
                },
                ' ' => {
                    new_line += 1;
                },
                _ => {},
            }
        }
    }

    if diff.line_status.is_empty() && diff.deletion_markers.is_empty() {
        return None;
    }

    Some(diff)
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

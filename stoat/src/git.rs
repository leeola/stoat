use git2::{Repository, Status, StatusOptions};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiffStatus {
    #[default]
    Unchanged,
    Added,
    Modified,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChangedFile {
    pub(crate) path: PathBuf,
    pub(crate) staged: bool,
}

const STAGED: Status = Status::INDEX_NEW
    .union(Status::INDEX_MODIFIED)
    .union(Status::INDEX_RENAMED);

const UNSTAGED: Status = Status::WT_NEW
    .union(Status::WT_MODIFIED)
    .union(Status::WT_RENAMED);

pub(crate) fn discover_repo(start: &Path) -> Option<Repository> {
    Repository::discover(start).ok()
}

/// List changed files (staged and/or unstaged) relative to HEAD.
///
/// Staged entries sort before unstaged. If a file has both staged and
/// unstaged changes, only the staged entry is kept (the buffer shows
/// the full working-tree state regardless).
pub(crate) fn changed_files(repo: &Repository) -> Vec<ChangedFile> {
    let workdir = match repo.workdir() {
        Some(w) => w.to_path_buf(),
        None => return Vec::new(),
    };

    let statuses = {
        let mut opts = StatusOptions::new();
        opts.include_untracked(false);
        match repo.statuses(Some(&mut opts)) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        }
    };

    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut staged_paths = std::collections::HashSet::new();

    for entry in statuses.iter() {
        let rel = match entry.path() {
            Some(p) => p,
            None => continue,
        };
        let abs = workdir.join(rel);
        let status = entry.status();

        if status.intersects(STAGED) {
            staged_paths.insert(abs.clone());
            staged.push(ChangedFile {
                path: abs,
                staged: true,
            });
        } else if status.intersects(UNSTAGED) {
            unstaged.push(ChangedFile {
                path: abs,
                staged: false,
            });
        }
    }

    staged.sort_by(|a, b| a.path.cmp(&b.path));
    unstaged.sort_by(|a, b| a.path.cmp(&b.path));
    staged.extend(unstaged);
    staged
}

/// Read the UTF-8 content of `path` from HEAD's tree.
///
/// Returns `None` for orphan branches (no HEAD commit), paths not
/// present in HEAD, or binary (non-UTF-8) blobs.
pub(crate) fn head_content(repo: &Repository, path: &Path) -> Option<String> {
    let workdir = repo.workdir()?;
    let rel = path.strip_prefix(workdir).ok()?;
    let tree = repo.head().ok()?.peel_to_tree().ok()?;
    let entry = tree.get_path(rel).ok()?;
    let blob = entry.to_object(repo).ok()?.peel_to_blob().ok()?;
    std::str::from_utf8(blob.content()).ok().map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn init_repo(dir: &Path) -> Repository {
        Repository::init(dir).expect("init repo")
    }

    fn commit_file(repo: &Repository, dir: &Path, name: &str, content: &str) {
        fs::write(dir.join(name), content).expect("write file");
        let mut index = repo.index().expect("index");
        index.add_path(Path::new(name)).expect("add path");
        index.write().expect("write index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        let sig = git2::Signature::now("test", "t@t").expect("sig");
        let parents: Vec<_> = repo
            .head()
            .ok()
            .and_then(|h| h.peel_to_commit().ok())
            .into_iter()
            .collect();
        let parent_refs: Vec<_> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, "c", &tree, &parent_refs)
            .expect("commit");
    }

    #[test]
    fn discover_finds_repo() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let _repo = init_repo(dir.path());
        assert!(discover_repo(dir.path()).is_some());
    }

    #[test]
    fn discover_returns_none_outside_repo() {
        let dir = tempfile::tempdir().expect("tmpdir");
        assert!(discover_repo(dir.path()).is_none());
    }

    #[test]
    fn changed_files_empty_when_clean() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let repo = init_repo(dir.path());
        commit_file(&repo, dir.path(), "a.rs", "fn main() {}");
        assert!(changed_files(&repo).is_empty());
    }

    #[test]
    fn changed_files_detects_modified() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let repo = init_repo(dir.path());
        commit_file(&repo, dir.path(), "a.rs", "v1");
        fs::write(dir.path().join("a.rs"), "v2").expect("write");
        let files = changed_files(&repo);
        assert_eq!(files.len(), 1);
        assert!(!files[0].staged);
        assert!(files[0].path.ends_with("a.rs"));
    }

    #[test]
    fn changed_files_staged_first() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let repo = init_repo(dir.path());
        commit_file(&repo, dir.path(), "a.rs", "v1");
        commit_file(&repo, dir.path(), "b.rs", "v1");

        // Stage a change to b.rs
        fs::write(dir.path().join("b.rs"), "v2").expect("write");
        let mut index = repo.index().expect("index");
        index.add_path(Path::new("b.rs")).expect("add");
        index.write().expect("write");

        // Unstaged change to a.rs
        fs::write(dir.path().join("a.rs"), "v2").expect("write");

        let files = changed_files(&repo);
        assert_eq!(files.len(), 2);
        assert!(files[0].staged, "staged should sort first");
        assert!(files[0].path.ends_with("b.rs"));
        assert!(!files[1].staged);
        assert!(files[1].path.ends_with("a.rs"));
    }

    #[test]
    fn changed_files_deduplicates_staged_and_unstaged() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let repo = init_repo(dir.path());
        commit_file(&repo, dir.path(), "a.rs", "v1");

        // Stage a change, then make another unstaged change
        fs::write(dir.path().join("a.rs"), "v2").expect("write");
        let mut index = repo.index().expect("index");
        index.add_path(Path::new("a.rs")).expect("add");
        index.write().expect("write");
        fs::write(dir.path().join("a.rs"), "v3").expect("write");

        let files = changed_files(&repo);
        assert_eq!(files.len(), 1, "should deduplicate to staged only");
        assert!(files[0].staged);
    }

    #[test]
    fn changed_files_empty_on_no_commits() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let repo = init_repo(dir.path());
        fs::write(dir.path().join("new.rs"), "x").expect("write");
        assert!(changed_files(&repo).is_empty());
    }

    #[test]
    fn head_content_reads_blob() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let repo = init_repo(dir.path());
        commit_file(&repo, dir.path(), "a.rs", "fn main() {}");
        let workdir = repo.workdir().expect("workdir");
        let content = head_content(&repo, &workdir.join("a.rs"));
        assert_eq!(content.as_deref(), Some("fn main() {}"));
    }

    #[test]
    fn head_content_none_for_new_file() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let repo = init_repo(dir.path());
        commit_file(&repo, dir.path(), "a.rs", "v1");
        let workdir = repo.workdir().expect("workdir");
        assert!(head_content(&repo, &workdir.join("b.rs")).is_none());
    }

    #[test]
    fn head_content_none_on_orphan_branch() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let repo = init_repo(dir.path());
        let workdir = repo.workdir().expect("workdir");
        assert!(head_content(&repo, &workdir.join("a.rs")).is_none());
    }
}

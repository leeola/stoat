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
pub(crate) struct TestRepo {
    pub repo: Repository,
    _dir: tempfile::TempDir,
}

#[cfg(test)]
impl TestRepo {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("create tempdir");
        let repo = Repository::init(dir.path()).expect("init repo");
        Self { repo, _dir: dir }
    }

    pub fn path(&self) -> &Path {
        self.repo.workdir().expect("repo has workdir")
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.path().join(name)
    }

    pub fn write(&self, name: &str, content: &str) -> &Self {
        let path = self.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&path, content).expect("write file");
        self
    }

    pub fn stage(&self, name: &str) -> &Self {
        let mut index = self.repo.index().expect("index");
        index.add_path(Path::new(name)).expect("add path");
        index.write().expect("write index");
        self
    }

    pub fn write_and_stage(&self, name: &str, content: &str) -> &Self {
        self.write(name, content).stage(name)
    }

    pub fn commit(&self, message: &str) -> &Self {
        let mut index = self.repo.index().expect("index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = self.repo.find_tree(tree_id).expect("find tree");
        let sig = git2::Signature::now("test", "t@t").expect("sig");
        let parents: Vec<_> = self
            .repo
            .head()
            .ok()
            .and_then(|h| h.peel_to_commit().ok())
            .into_iter()
            .collect();
        let parent_refs: Vec<_> = parents.iter().collect();
        self.repo
            .commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)
            .expect("commit");
        self
    }

    pub fn commit_file(&self, name: &str, content: &str) -> &Self {
        self.write_and_stage(name, content).commit("c")
    }

    pub fn create_branch(&self, name: &str) -> &Self {
        let head = self
            .repo
            .head()
            .expect("HEAD")
            .peel_to_commit()
            .expect("commit");
        self.repo.branch(name, &head, false).expect("create branch");
        self.repo
            .set_head(&format!("refs/heads/{name}"))
            .expect("set HEAD");
        self.repo
            .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .expect("checkout");
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_finds_repo() {
        let r = TestRepo::new();
        assert!(discover_repo(r.path()).is_some());
    }

    #[test]
    fn discover_returns_none_outside_repo() {
        let dir = tempfile::tempdir().expect("tmpdir");
        assert!(discover_repo(dir.path()).is_none());
    }

    #[test]
    fn changed_files_empty_when_clean() {
        let r = TestRepo::new();
        r.commit_file("a.rs", "fn main() {}");
        assert!(changed_files(&r.repo).is_empty());
    }

    #[test]
    fn changed_files_detects_modified() {
        let r = TestRepo::new();
        r.commit_file("a.rs", "v1");
        r.write("a.rs", "v2");
        let files = changed_files(&r.repo);
        assert_eq!(files.len(), 1);
        assert!(!files[0].staged);
        assert!(files[0].path.ends_with("a.rs"));
    }

    #[test]
    fn changed_files_staged_first() {
        let r = TestRepo::new();
        r.commit_file("a.rs", "v1").commit_file("b.rs", "v1");
        r.write_and_stage("b.rs", "v2");
        r.write("a.rs", "v2");

        let files = changed_files(&r.repo);
        assert_eq!(files.len(), 2);
        assert!(files[0].staged, "staged should sort first");
        assert!(files[0].path.ends_with("b.rs"));
        assert!(!files[1].staged);
        assert!(files[1].path.ends_with("a.rs"));
    }

    #[test]
    fn changed_files_deduplicates_staged_and_unstaged() {
        let r = TestRepo::new();
        r.commit_file("a.rs", "v1");
        r.write_and_stage("a.rs", "v2");
        r.write("a.rs", "v3");

        let files = changed_files(&r.repo);
        assert_eq!(files.len(), 1, "should deduplicate to staged only");
        assert!(files[0].staged);
    }

    #[test]
    fn changed_files_empty_on_no_commits() {
        let r = TestRepo::new();
        r.write("new.rs", "x");
        assert!(changed_files(&r.repo).is_empty());
    }

    #[test]
    fn head_content_reads_blob() {
        let r = TestRepo::new();
        r.commit_file("a.rs", "fn main() {}");
        assert_eq!(
            head_content(&r.repo, &r.join("a.rs")).as_deref(),
            Some("fn main() {}")
        );
    }

    #[test]
    fn head_content_none_for_new_file() {
        let r = TestRepo::new();
        r.commit_file("a.rs", "v1");
        assert!(head_content(&r.repo, &r.join("b.rs")).is_none());
    }

    #[test]
    fn head_content_none_on_orphan_branch() {
        let r = TestRepo::new();
        assert!(head_content(&r.repo, &r.join("a.rs")).is_none());
    }
}

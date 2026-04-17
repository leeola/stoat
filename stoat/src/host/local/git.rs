use crate::host::git::{ChangedFile, GitApplyError, GitHost, GitRepo};
use git2::{ApplyLocation, Diff, Repository, Status, StatusOptions};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Production [`GitHost`] wrapping libgit2.
pub struct LocalGit;

impl LocalGit {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LocalGit {
    fn default() -> Self {
        Self::new()
    }
}

impl GitHost for LocalGit {
    fn discover(&self, path: &Path) -> Option<Arc<dyn GitRepo>> {
        let repo = Repository::discover(path).ok()?;
        Some(Arc::new(LocalGitRepo {
            repo: Mutex::new(repo),
        }))
    }
}

/// libgit2-backed [`GitRepo`]. Wraps [`Repository`] in a [`Mutex`] so
/// the trait object can be `Send + Sync` even though [`Repository`]
/// itself is `!Sync`.
struct LocalGitRepo {
    repo: Mutex<Repository>,
}

const STAGED: Status = Status::INDEX_NEW
    .union(Status::INDEX_MODIFIED)
    .union(Status::INDEX_RENAMED);

const UNSTAGED: Status = Status::WT_NEW
    .union(Status::WT_MODIFIED)
    .union(Status::WT_RENAMED);

impl GitRepo for LocalGitRepo {
    fn workdir(&self) -> Option<PathBuf> {
        let repo = self.repo.lock().expect("git repo lock");
        repo.workdir().map(|p| p.to_path_buf())
    }

    fn changed_files(&self) -> Vec<ChangedFile> {
        let repo = self.repo.lock().expect("git repo lock");
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

        let mut staged: Vec<ChangedFile> = Vec::new();
        let mut unstaged: Vec<ChangedFile> = Vec::new();
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
            } else if status.intersects(UNSTAGED) && !staged_paths.contains(&abs) {
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

    fn head_content(&self, path: &Path) -> Option<String> {
        let repo = self.repo.lock().expect("git repo lock");
        let workdir = repo.workdir()?;
        let rel = path.strip_prefix(workdir).ok()?;
        let tree = repo.head().ok()?.peel_to_tree().ok()?;
        let entry = tree.get_path(rel).ok()?;
        let blob = entry.to_object(&repo).ok()?.peel_to_blob().ok()?;
        std::str::from_utf8(blob.content()).ok().map(String::from)
    }

    fn apply_to_index(&self, patch: &str) -> Result<(), GitApplyError> {
        let repo = self.repo.lock().expect("git repo lock");
        let diff = Diff::from_buffer(patch.as_bytes())
            .map_err(|e| GitApplyError::Backend(e.message().to_string()))?;
        repo.apply(&diff, ApplyLocation::Index, None)
            .map_err(|e| GitApplyError::Backend(e.message().to_string()))
    }

    fn commit_tree(&self, sha: &str) -> Option<BTreeMap<PathBuf, String>> {
        let repo = self.repo.lock().expect("git repo lock");
        let oid = git2::Oid::from_str(sha).ok()?;
        let commit = repo.find_commit(oid).ok()?;
        let tree = commit.tree().ok()?;

        let mut out: BTreeMap<PathBuf, String> = BTreeMap::new();
        let mut utf8_violation = false;
        tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
            if entry.kind() != Some(git2::ObjectType::Blob) {
                return git2::TreeWalkResult::Ok;
            }
            let name = match entry.name() {
                Some(n) => n,
                None => return git2::TreeWalkResult::Ok,
            };
            let rel = if dir.is_empty() {
                PathBuf::from(name)
            } else {
                PathBuf::from(dir).join(name)
            };
            let blob = match entry.to_object(&repo).and_then(|o| o.peel_to_blob()) {
                Ok(b) => b,
                Err(_) => return git2::TreeWalkResult::Ok,
            };
            match std::str::from_utf8(blob.content()) {
                Ok(s) => {
                    out.insert(rel, s.to_string());
                    git2::TreeWalkResult::Ok
                },
                Err(_) => {
                    utf8_violation = true;
                    git2::TreeWalkResult::Abort
                },
            }
        })
        .ok()?;
        if utf8_violation {
            return None;
        }
        Some(out)
    }

    fn parent_sha(&self, sha: &str) -> Option<String> {
        let repo = self.repo.lock().expect("git repo lock");
        let oid = git2::Oid::from_str(sha).ok()?;
        let commit = repo.find_commit(oid).ok()?;
        let parent = commit.parents().next()?;
        Some(parent.id().to_string())
    }
}

#[cfg(test)]
pub(crate) mod test_repo {
    use git2::Repository;
    use std::path::{Path, PathBuf};

    /// On-disk git repo for integration-testing [`super::LocalGit`].
    /// Production code must not use this type.
    pub(crate) struct TestRepo {
        pub repo: Repository,
        _dir: tempfile::TempDir,
    }

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

        /// Current HEAD commit sha, or panics if HEAD is orphan or unreadable.
        pub fn head_sha(&self) -> String {
            self.repo
                .head()
                .expect("HEAD")
                .peel_to_commit()
                .expect("commit")
                .id()
                .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{test_repo::TestRepo, *};

    #[test]
    fn discover_finds_repo() {
        let tr = TestRepo::new();
        let host = LocalGit::new();
        assert!(host.discover(tr.path()).is_some());
    }

    #[test]
    fn discover_returns_none_outside_repo() {
        let dir = tempfile::tempdir().unwrap();
        let host = LocalGit::new();
        assert!(host.discover(dir.path()).is_none());
    }

    #[test]
    fn workdir_returns_repo_workdir() {
        let tr = TestRepo::new();
        let repo = LocalGit::new().discover(tr.path()).unwrap();
        assert_eq!(repo.workdir().as_deref(), Some(tr.path()));
    }

    #[test]
    fn changed_files_empty_when_clean() {
        let tr = TestRepo::new();
        tr.commit_file("a.rs", "fn main() {}");
        let repo = LocalGit::new().discover(tr.path()).unwrap();
        assert!(repo.changed_files().is_empty());
    }

    #[test]
    fn changed_files_detects_modified() {
        let tr = TestRepo::new();
        tr.commit_file("a.rs", "v1");
        tr.write("a.rs", "v2");
        let repo = LocalGit::new().discover(tr.path()).unwrap();
        let files = repo.changed_files();
        assert_eq!(files.len(), 1);
        assert!(!files[0].staged);
        assert!(files[0].path.ends_with("a.rs"));
    }

    #[test]
    fn changed_files_staged_first() {
        let tr = TestRepo::new();
        tr.commit_file("a.rs", "v1").commit_file("b.rs", "v1");
        tr.write_and_stage("b.rs", "v2");
        tr.write("a.rs", "v2");
        let repo = LocalGit::new().discover(tr.path()).unwrap();
        let files = repo.changed_files();
        assert_eq!(files.len(), 2);
        assert!(files[0].staged);
        assert!(files[0].path.ends_with("b.rs"));
        assert!(!files[1].staged);
        assert!(files[1].path.ends_with("a.rs"));
    }

    #[test]
    fn changed_files_deduplicates_staged_and_unstaged() {
        let tr = TestRepo::new();
        tr.commit_file("a.rs", "v1");
        tr.write_and_stage("a.rs", "v2");
        tr.write("a.rs", "v3");
        let repo = LocalGit::new().discover(tr.path()).unwrap();
        let files = repo.changed_files();
        assert_eq!(files.len(), 1);
        assert!(files[0].staged);
    }

    #[test]
    fn changed_files_empty_on_no_commits() {
        let tr = TestRepo::new();
        tr.write("new.rs", "x");
        let repo = LocalGit::new().discover(tr.path()).unwrap();
        assert!(repo.changed_files().is_empty());
    }

    #[test]
    fn head_content_reads_blob() {
        let tr = TestRepo::new();
        tr.commit_file("a.rs", "fn main() {}");
        let repo = LocalGit::new().discover(tr.path()).unwrap();
        assert_eq!(
            repo.head_content(&tr.join("a.rs")).as_deref(),
            Some("fn main() {}")
        );
    }

    #[test]
    fn head_content_none_for_new_file() {
        let tr = TestRepo::new();
        tr.commit_file("a.rs", "v1");
        let repo = LocalGit::new().discover(tr.path()).unwrap();
        assert!(repo.head_content(&tr.join("b.rs")).is_none());
    }

    #[test]
    fn head_content_none_on_orphan_branch() {
        let tr = TestRepo::new();
        let repo = LocalGit::new().discover(tr.path()).unwrap();
        assert!(repo.head_content(&tr.join("a.rs")).is_none());
    }

    fn staged_blob(workdir: &Path, rel: &str) -> Option<String> {
        let repo = Repository::open(workdir).ok()?;
        let mut index = repo.index().ok()?;
        index.read(true).ok()?;
        let entry = index.get_path(Path::new(rel), 0)?;
        let blob = repo.find_blob(entry.id).ok()?;
        std::str::from_utf8(blob.content()).ok().map(String::from)
    }

    #[test]
    fn apply_to_index_stages_modification() {
        let tr = TestRepo::new();
        tr.commit_file("a.rs", "old\n");
        tr.write("a.rs", "new\n");
        let patch = "diff --git a/a.rs b/a.rs\n\
                     --- a/a.rs\n\
                     +++ b/a.rs\n\
                     @@ -1,1 +1,1 @@\n\
                     -old\n\
                     +new\n";
        let repo = LocalGit::new().discover(tr.path()).unwrap();
        repo.apply_to_index(patch).expect("apply ok");
        assert_eq!(staged_blob(tr.path(), "a.rs").as_deref(), Some("new\n"));
    }

    #[test]
    fn apply_to_index_stages_pure_addition() {
        let tr = TestRepo::new();
        tr.commit_file("a.rs", "seed\n");
        tr.write("b.rs", "hello\n");
        let patch = "diff --git a/b.rs b/b.rs\n\
                     new file mode 100644\n\
                     --- /dev/null\n\
                     +++ b/b.rs\n\
                     @@ -0,0 +1,1 @@\n\
                     +hello\n";
        let repo = LocalGit::new().discover(tr.path()).unwrap();
        repo.apply_to_index(patch).expect("apply ok");
        assert_eq!(staged_blob(tr.path(), "b.rs").as_deref(), Some("hello\n"));
    }

    #[test]
    fn apply_to_index_stages_pure_deletion() {
        let tr = TestRepo::new();
        tr.commit_file("a.rs", "gone\n");
        std::fs::remove_file(tr.join("a.rs")).unwrap();
        let patch = "diff --git a/a.rs b/a.rs\n\
                     deleted file mode 100644\n\
                     --- a/a.rs\n\
                     +++ /dev/null\n\
                     @@ -1,1 +0,0 @@\n\
                     -gone\n";
        let repo = LocalGit::new().discover(tr.path()).unwrap();
        repo.apply_to_index(patch).expect("apply ok");
        assert!(
            staged_blob(tr.path(), "a.rs").is_none(),
            "index should no longer have a.rs"
        );
    }

    #[test]
    fn apply_to_index_rejects_malformed_patch() {
        let tr = TestRepo::new();
        tr.commit_file("a.rs", "x\n");
        let repo = LocalGit::new().discover(tr.path()).unwrap();
        let err = repo
            .apply_to_index("not a patch")
            .expect_err("garbage must fail");
        let GitApplyError::Backend(msg) = err;
        assert!(!msg.is_empty(), "error message must be non-empty");
    }

    #[test]
    fn apply_to_index_rejects_conflicting_patch() {
        let tr = TestRepo::new();
        tr.commit_file("a.rs", "actual\n");
        let patch = "diff --git a/a.rs b/a.rs\n\
                     --- a/a.rs\n\
                     +++ b/a.rs\n\
                     @@ -1,1 +1,1 @@\n\
                     -unexpected\n\
                     +new\n";
        let repo = LocalGit::new().discover(tr.path()).unwrap();
        let err = repo
            .apply_to_index(patch)
            .expect_err("conflicting context must fail");
        let GitApplyError::Backend(msg) = err;
        assert!(!msg.is_empty(), "error must have message");
    }

    #[test]
    fn emitted_patch_from_chunk_applies_cleanly() {
        use crate::{
            review_apply::chunk_to_unified_diff,
            review_session::{ReviewSession, ReviewSource},
        };
        use std::sync::Arc;

        let tr = TestRepo::new();
        tr.commit_file("a.rs", "line1\nOLD\nline3\n");
        tr.write("a.rs", "line1\nNEW\nline3\n");

        let workdir = tr.path().to_path_buf();
        let mut session = ReviewSession::new(ReviewSource::WorkingTree {
            workdir: workdir.clone(),
        });
        session.add_file(
            workdir.join("a.rs"),
            "a.rs".into(),
            None,
            Arc::new("line1\nOLD\nline3\n".into()),
            Arc::new("line1\nNEW\nline3\n".into()),
        );
        let id = session.order[0];
        let chunk = &session.chunks[&id];
        let file = &session.files[chunk.file_index];
        let patch = chunk_to_unified_diff(file, chunk, &workdir);

        let host = LocalGit::new();
        let repo = host.discover(&workdir).unwrap();
        repo.apply_to_index(&patch)
            .expect("emitted patch must apply to real libgit2");
        assert_eq!(
            staged_blob(&workdir, "a.rs").as_deref(),
            Some("line1\nNEW\nline3\n"),
            "index must reflect the applied change"
        );
    }

    #[test]
    fn commit_tree_reads_all_blobs() {
        let tr = TestRepo::new();
        tr.write_and_stage("a.rs", "hello a")
            .write_and_stage("sub/b.rs", "hello b")
            .commit("c1");
        let sha = tr.head_sha();
        let repo = LocalGit::new().discover(tr.path()).unwrap();
        let tree = repo.commit_tree(&sha).expect("tree");
        assert_eq!(
            tree.get(Path::new("a.rs")).map(String::as_str),
            Some("hello a")
        );
        assert_eq!(
            tree.get(Path::new("sub/b.rs")).map(String::as_str),
            Some("hello b")
        );
        assert_eq!(tree.len(), 2);
    }

    #[test]
    fn commit_tree_unknown_sha_returns_none() {
        let tr = TestRepo::new();
        tr.commit_file("a.rs", "x");
        let repo = LocalGit::new().discover(tr.path()).unwrap();
        assert!(repo.commit_tree("bad-sha").is_none());
        assert!(repo.commit_tree(&"0".repeat(40)).is_none());
    }

    #[test]
    fn parent_sha_follows_first_parent() {
        let tr = TestRepo::new();
        tr.commit_file("a.rs", "v1");
        let c1 = tr.head_sha();
        tr.commit_file("a.rs", "v2");
        let c2 = tr.head_sha();

        let repo = LocalGit::new().discover(tr.path()).unwrap();
        assert_eq!(repo.parent_sha(&c2), Some(c1));
    }

    #[test]
    fn parent_sha_root_commit_returns_none() {
        let tr = TestRepo::new();
        tr.commit_file("a.rs", "root");
        let root = tr.head_sha();
        let repo = LocalGit::new().discover(tr.path()).unwrap();
        assert!(repo.parent_sha(&root).is_none());
    }

    #[test]
    fn apply_to_index_rejects_double_apply() {
        let tr = TestRepo::new();
        tr.commit_file("a.rs", "v1\n");
        tr.write("a.rs", "v2\n");
        let patch = "diff --git a/a.rs b/a.rs\n\
                     --- a/a.rs\n\
                     +++ b/a.rs\n\
                     @@ -1,1 +1,1 @@\n\
                     -v1\n\
                     +v2\n";
        let repo = LocalGit::new().discover(tr.path()).unwrap();
        repo.apply_to_index(patch).expect("first apply ok");
        let err = repo
            .apply_to_index(patch)
            .expect_err("context no longer matches on second apply");
        let GitApplyError::Backend(msg) = err;
        assert!(!msg.is_empty());
    }
}

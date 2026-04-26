mod rebase;
mod tree;

use crate::host::git::{
    ChangedFile, CherryPickOutcome, CommitFileChange, CommitFileChangeKind, CommitInfo,
    GitApplyError, GitHost, GitRepo, RebaseError, RebaseTodo, RewriteResult,
};
use git2::{ApplyLocation, Diff, DiffOptions, Repository, Sort, Status, StatusOptions};
use std::{
    collections::{BTreeMap, HashMap},
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

    fn log_commits(&self, after: Option<&str>, limit: usize) -> Vec<CommitInfo> {
        if limit == 0 {
            return Vec::new();
        }
        let repo = self.repo.lock().expect("git repo lock");
        let start_oid = match after {
            Some(sha) => {
                let Ok(oid) = git2::Oid::from_str(sha) else {
                    return Vec::new();
                };
                let Ok(commit) = repo.find_commit(oid) else {
                    return Vec::new();
                };
                match commit.parents().next() {
                    Some(p) => p.id(),
                    None => return Vec::new(),
                }
            },
            None => match repo.head().and_then(|h| h.peel_to_commit()) {
                Ok(c) => c.id(),
                Err(_) => return Vec::new(),
            },
        };

        let mut walk = match repo.revwalk() {
            Ok(w) => w,
            Err(_) => return Vec::new(),
        };
        if walk.set_sorting(Sort::TOPOLOGICAL).is_err() {
            return Vec::new();
        }
        if walk.simplify_first_parent().is_err() {
            return Vec::new();
        }
        if walk.push(start_oid).is_err() {
            return Vec::new();
        }

        // Cap the reserved capacity so callers passing `usize::MAX` as
        // "unbounded" don't trigger an allocation overflow; the Vec
        // grows on demand if the walk actually yields more rows.
        let mut out: Vec<CommitInfo> = Vec::with_capacity(limit.min(4096));
        for oid_res in walk.take(limit) {
            let Ok(oid) = oid_res else { continue };
            let Ok(commit) = repo.find_commit(oid) else {
                continue;
            };
            let sha = oid.to_string();
            let short_sha = sha.chars().take(7).collect();
            let summary = commit.summary().unwrap_or_default().trim().to_string();
            let author = commit.author();
            let author_name = author.name().unwrap_or_default().to_string();
            let author_email = author.email().unwrap_or_default().to_string();
            let time = commit.time().seconds();
            let parent_count = commit.parent_count() as u32;
            out.push(CommitInfo {
                sha,
                short_sha,
                summary,
                author_name,
                author_email,
                time,
                parent_count,
            });
        }
        out
    }

    fn amend_head(
        &self,
        tree: &BTreeMap<PathBuf, String>,
        message: Option<&str>,
    ) -> Result<String, GitApplyError> {
        let repo = self.repo.lock().expect("git repo lock");
        let head = repo
            .head()
            .and_then(|h| h.peel_to_commit())
            .map_err(err_msg)?;
        let tree_oid = tree::build_tree_from_map(&repo, tree).map_err(err_msg)?;
        let new_tree = repo.find_tree(tree_oid).map_err(err_msg)?;
        let new_id = head
            .amend(Some("HEAD"), None, None, None, message, Some(&new_tree))
            .map_err(err_msg)?;
        Ok(new_id.to_string())
    }

    fn rewrite_commit(
        &self,
        sha: &str,
        tree: &BTreeMap<PathBuf, String>,
        message: Option<&str>,
        descendants: &[String],
    ) -> Result<RewriteResult, GitApplyError> {
        let repo = self.repo.lock().expect("git repo lock");
        let target_oid = git2::Oid::from_str(sha).map_err(err_msg)?;
        let target = repo.find_commit(target_oid).map_err(err_msg)?;

        let new_tree_oid = tree::build_tree_from_map(&repo, tree).map_err(err_msg)?;
        let new_tree = repo.find_tree(new_tree_oid).map_err(err_msg)?;

        let parents: Vec<_> = target.parents().collect();
        let parent_refs: Vec<_> = parents.iter().collect();
        let msg = message.unwrap_or_else(|| target.message().unwrap_or(""));
        let author = target.author();
        let committer = target.committer();

        let rewritten = repo
            .commit(None, &author, &committer, msg, &new_tree, &parent_refs)
            .map_err(err_msg)?;

        let mut mapping: HashMap<String, String> = HashMap::new();
        mapping.insert(sha.to_string(), rewritten.to_string());
        let mut current = rewritten;

        for desc_sha in descendants {
            let desc_oid = git2::Oid::from_str(desc_sha).map_err(err_msg)?;
            let desc_commit = repo.find_commit(desc_oid).map_err(err_msg)?;
            let onto_commit = repo.find_commit(current).map_err(err_msg)?;

            let mut index = repo
                .cherrypick_commit(&desc_commit, &onto_commit, 0, None)
                .map_err(err_msg)?;
            if index.has_conflicts() {
                return Err(GitApplyError::Backend(format!(
                    "cherry-pick conflict at {desc_sha}"
                )));
            }
            let picked_tree_oid = index.write_tree_to(&repo).map_err(err_msg)?;
            let picked_tree = repo.find_tree(picked_tree_oid).map_err(err_msg)?;
            let new_id = repo
                .commit(
                    None,
                    &desc_commit.author(),
                    &desc_commit.committer(),
                    desc_commit.message().unwrap_or(""),
                    &picked_tree,
                    &[&onto_commit],
                )
                .map_err(err_msg)?;
            mapping.insert(desc_sha.clone(), new_id.to_string());
            current = new_id;
        }

        repo.reference("HEAD", current, true, "rewrite_commit")
            .map_err(err_msg)?;

        Ok(RewriteResult {
            new_head: current.to_string(),
            mapping,
        })
    }

    fn run_rebase(&self, onto: &str, todo: &[RebaseTodo]) -> Result<String, RebaseError> {
        let repo = self.repo.lock().expect("git repo lock");
        rebase::run_rebase(&repo, onto, todo)
    }

    fn cherry_pick_tree(
        &self,
        source_sha: &str,
        onto_sha: &str,
    ) -> Result<CherryPickOutcome, GitApplyError> {
        let repo = self.repo.lock().expect("git repo lock");
        rebase::cherry_pick_tree(&repo, source_sha, onto_sha)
    }

    fn create_commit(
        &self,
        parent_sha: Option<&str>,
        tree: &BTreeMap<PathBuf, String>,
        message: &str,
        author_name: &str,
        author_email: &str,
    ) -> Result<String, GitApplyError> {
        let repo = self.repo.lock().expect("git repo lock");
        let tree_oid = tree::build_tree_from_map(&repo, tree).map_err(err_msg)?;
        let tree = repo.find_tree(tree_oid).map_err(err_msg)?;
        let sig = git2::Signature::now(author_name, author_email).map_err(err_msg)?;
        let parent_commit = match parent_sha {
            Some(sha) => {
                let oid = git2::Oid::from_str(sha).map_err(err_msg)?;
                Some(repo.find_commit(oid).map_err(err_msg)?)
            },
            None => None,
        };
        let parents: Vec<&git2::Commit<'_>> = parent_commit.as_ref().into_iter().collect();
        let new_id = repo
            .commit(None, &sig, &sig, message, &tree, &parents)
            .map_err(err_msg)?;
        Ok(new_id.to_string())
    }

    fn update_head(&self, sha: &str) -> Result<(), GitApplyError> {
        let repo = self.repo.lock().expect("git repo lock");
        let oid = git2::Oid::from_str(sha).map_err(err_msg)?;
        repo.reference("HEAD", oid, true, "stoat rebase")
            .map_err(err_msg)?;
        Ok(())
    }

    fn commit_file_changes(&self, sha: &str) -> Vec<CommitFileChange> {
        let repo = self.repo.lock().expect("git repo lock");
        let Ok(oid) = git2::Oid::from_str(sha) else {
            return Vec::new();
        };
        let Ok(commit) = repo.find_commit(oid) else {
            return Vec::new();
        };
        let Ok(new_tree) = commit.tree() else {
            return Vec::new();
        };
        let parent_tree = commit.parents().next().and_then(|p| p.tree().ok());

        let mut opts = DiffOptions::new();
        opts.include_typechange(true);
        let diff =
            match repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&new_tree), Some(&mut opts)) {
                Ok(d) => d,
                Err(_) => return Vec::new(),
            };

        let stats = match diff.stats() {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let deltas = diff.deltas();
        let mut out: Vec<CommitFileChange> = Vec::with_capacity(deltas.len());
        for (i, delta) in deltas.enumerate() {
            let rel_path = match delta.new_file().path().or_else(|| delta.old_file().path()) {
                Some(p) => p.to_path_buf(),
                None => continue,
            };
            let kind = match delta.status() {
                git2::Delta::Added => CommitFileChangeKind::Added,
                git2::Delta::Deleted => CommitFileChangeKind::Deleted,
                git2::Delta::Modified => CommitFileChangeKind::Modified,
                git2::Delta::Renamed => CommitFileChangeKind::Renamed,
                git2::Delta::Typechange => CommitFileChangeKind::TypeChange,
                _ => CommitFileChangeKind::Modified,
            };
            let patch = git2::Patch::from_diff(&diff, i).ok().flatten();
            let (additions, deletions) = match patch {
                Some(p) => match p.line_stats() {
                    Ok((_ctx, add, del)) => (add as u32, del as u32),
                    Err(_) => (0, 0),
                },
                None => (0, 0),
            };
            let _ = &stats;
            out.push(CommitFileChange {
                rel_path,
                kind,
                additions,
                deletions,
            });
        }
        out
    }
}

fn err_msg(e: git2::Error) -> GitApplyError {
    GitApplyError::Backend(e.message().to_string())
}

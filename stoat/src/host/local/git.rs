mod rebase;
mod tree;

use crate::host::git::{
    BackendSnafu, ChangedFile, CherryPickOutcome, CommitFileChange, CommitFileChangeKind,
    CommitInfo, ConflictedFile, GitApplyError, GitHost, GitRepo, HunkTallies, RebaseError,
    RebaseTodo, RewriteResult,
};
use git2::{
    build::CheckoutBuilder, ApplyLocation, BranchType, Commit, Delta, Diff, DiffFindOptions,
    DiffOptions, Repository, RepositoryState, Sort, Status, StatusEntry, StatusOptions,
};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use stoat_text::LineEnding;

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
    .union(Status::INDEX_DELETED)
    .union(Status::INDEX_RENAMED);

const UNSTAGED: Status = Status::WT_NEW
    .union(Status::WT_MODIFIED)
    .union(Status::WT_DELETED)
    .union(Status::WT_RENAMED);

/// Longest line quoted into an apply-failure reason. The reason reaches the
/// one-line status bar, so a long source line is truncated rather than
/// pushing the rest of the message out of view.
const QUOTED_LINE_MAX: usize = 80;

impl GitRepo for LocalGitRepo {
    fn workdir(&self) -> Option<PathBuf> {
        let repo = self.repo.lock().expect("git repo lock");
        repo.workdir().map(|p| p.to_path_buf())
    }

    fn is_path_ignored(&self, path: &Path) -> bool {
        let repo = self.repo.lock().expect("git repo lock");
        let rel = repo
            .workdir()
            .and_then(|wd| path.strip_prefix(wd).ok())
            .unwrap_or(path);
        // A libgit2 error (path outside the repo, unreadable ignore file) falls
        // back to not-ignored, so an uncertain path still refreshes the review.
        repo.is_path_ignored(rel).unwrap_or(false)
    }

    fn rebase_in_progress(&self) -> bool {
        let repo = self.repo.lock().expect("git repo lock");
        matches!(
            repo.state(),
            RepositoryState::Rebase
                | RepositoryState::RebaseInteractive
                | RepositoryState::RebaseMerge
        )
    }

    fn changed_files(&self) -> Vec<ChangedFile> {
        let repo = self.repo.lock().expect("git repo lock");
        let workdir = match repo.workdir() {
            Some(w) => w.to_path_buf(),
            None => return Vec::new(),
        };

        let statuses = match repo.statuses(Some(&mut rename_aware_status())) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let mut staged: Vec<ChangedFile> = Vec::new();
        let mut unstaged: Vec<ChangedFile> = Vec::new();
        let mut staged_paths = std::collections::HashSet::new();

        for entry in statuses.iter() {
            let status = entry.status();
            let Some((abs, renamed_from)) = entry_paths(&entry, &workdir) else {
                continue;
            };

            if status.intersects(STAGED) {
                staged_paths.insert(abs.clone());
                staged.push(ChangedFile {
                    path: abs,
                    staged: true,
                    untracked: false,
                    renamed_from,
                });
            } else if status.intersects(UNSTAGED) && !staged_paths.contains(&abs) {
                unstaged.push(ChangedFile {
                    path: abs,
                    staged: false,
                    untracked: status.intersects(Status::WT_NEW),
                    renamed_from,
                });
            }
        }

        staged.sort_by(|a, b| a.path.cmp(&b.path));
        unstaged.sort_by(|a, b| a.path.cmp(&b.path));
        staged.extend(unstaged);
        staged
    }

    fn changed_files_from(&self, base_sha: &str) -> Vec<ChangedFile> {
        let repo = self.repo.lock().expect("git repo lock");
        let Some(workdir) = repo.workdir().map(Path::to_path_buf) else {
            return Vec::new();
        };
        let Ok(tree) = repo
            .revparse_single(base_sha)
            .and_then(|object| object.peel_to_commit())
            .and_then(|commit| commit.tree())
        else {
            return Vec::new();
        };

        // Through the index rather than the working tree alone, so a staged
        // change and the unstaged edit on top of it fold into the one entry
        // this list is about: whether the file differs from the base.
        let mut opts = DiffOptions::new();
        opts.include_untracked(true).recurse_untracked_dirs(true);
        let Ok(mut diff) = repo.diff_tree_to_workdir_with_index(Some(&tree), Some(&mut opts))
        else {
            return Vec::new();
        };
        let _ = diff.find_similar(Some(&mut rename_detection(true)));

        let mut files: Vec<ChangedFile> = diff
            .deltas()
            .filter_map(|delta| {
                let rel = delta
                    .new_file()
                    .path()
                    .or_else(|| delta.old_file().path())?;
                Some(ChangedFile {
                    path: workdir.join(rel),
                    // Staged-ness describes where an uncommitted change sits,
                    // which says nothing about a file's distance from a commit
                    // further back. Every entry here is simply changed.
                    staged: false,
                    untracked: delta.status() == Delta::Untracked,
                    renamed_from: match delta.status() {
                        Delta::Renamed => delta.old_file().path().map(|old| workdir.join(old)),
                        _ => None,
                    },
                })
            })
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        files.dedup_by(|a, b| a.path == b.path);
        files
    }

    fn hunk_tallies(&self) -> HunkTallies {
        let repo = self.repo.lock().expect("git repo lock");
        // An orphan branch has no tree to diff against, and `None` is how git2
        // spells the empty one, so everything reads as added.
        let head_tree = repo.head().ok().and_then(|head| head.peel_to_tree().ok());

        // Zero context and no interhunk merging, so the count is of edits
        // rather than of the windows a reader would see around them.
        let hunk_only = || {
            let mut opts = DiffOptions::new();
            opts.context_lines(0).interhunk_lines(0);
            opts
        };
        // No `show_untracked_content`. That flag reads and xdiffs every
        // untracked file's whole content just to learn it holds one hunk.
        // [`count_hunks`] counts such a delta from its size instead.
        let with_untracked = || {
            let mut opts = hunk_only();
            opts.include_untracked(true).recurse_untracked_dirs(true);
            opts
        };

        // Rename detection runs before every count, so a moved file pairs into
        // one Renamed delta and a pure move contributes no hunks at all.
        let staged = repo
            .diff_tree_to_index(head_tree.as_ref(), None, Some(&mut hunk_only()))
            .map_or(0, |mut diff| {
                let _ = diff.find_similar(Some(&mut rename_detection(false)));
                count_hunks(&diff, &mut |_, _| {})
            });
        let unstaged = repo
            .diff_index_to_workdir(None, Some(&mut with_untracked()))
            .map_or(0, |mut diff| {
                let _ = diff.find_similar(Some(&mut rename_detection(true)));
                count_hunks(&diff, &mut |_, _| {})
            });

        let mut per_file: BTreeMap<PathBuf, usize> = BTreeMap::new();
        if let Ok(mut diff) =
            repo.diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut with_untracked()))
        {
            let _ = diff.find_similar(Some(&mut rename_detection(true)));
            count_hunks(&diff, &mut |path, hunks| {
                *per_file.entry(path).or_default() += hunks;
            });
        }

        HunkTallies {
            staged,
            unstaged,
            per_file: per_file.into_iter().collect(),
        }
    }

    fn has_tracked_changes(&self) -> bool {
        let repo = self.repo.lock().expect("git repo lock");
        let mut opts = StatusOptions::new();
        opts.include_untracked(false);
        let statuses = match repo.statuses(Some(&mut opts)) {
            Ok(s) => s,
            Err(_) => return false,
        };
        statuses
            .iter()
            .any(|entry| entry.status().intersects(STAGED.union(UNSTAGED)))
    }

    fn head_contents(&self, paths: &[&Path]) -> Vec<Option<String>> {
        let repo = self.repo.lock().expect("git repo lock");
        let Some(workdir) = repo.workdir() else {
            return vec![None; paths.len()];
        };
        let Some(tree) = repo.head().ok().and_then(|h| h.peel_to_tree().ok()) else {
            return vec![None; paths.len()];
        };
        paths
            .iter()
            .map(|path| {
                let rel = path.strip_prefix(workdir).ok()?;
                let entry = tree.get_path(rel).ok()?;
                let blob = entry.to_object(&repo).ok()?.peel_to_blob().ok()?;
                std::str::from_utf8(blob.content())
                    .ok()
                    .map(|text| LineEnding::normalize(text).into_owned())
            })
            .collect()
    }

    fn content_at(&self, sha: &str, path: &Path) -> Option<String> {
        let repo = self.repo.lock().expect("git repo lock");
        let workdir = repo.workdir()?;
        let rel = path.strip_prefix(workdir).ok()?;
        let oid = git2::Oid::from_str(sha).ok()?;
        let tree = repo.find_commit(oid).ok()?.tree().ok()?;
        let entry = tree.get_path(rel).ok()?;
        let blob = entry.to_object(&repo).ok()?.peel_to_blob().ok()?;
        std::str::from_utf8(blob.content())
            .ok()
            .map(|text| LineEnding::normalize(text).into_owned())
    }

    fn index_content(&self, path: &Path) -> Option<String> {
        let repo = self.repo.lock().expect("git repo lock");
        let workdir = repo.workdir()?;
        let rel = path.strip_prefix(workdir).ok()?;
        let text = index_blob_text(&repo, rel)?;
        Some(LineEnding::normalize(&text).into_owned())
    }

    fn rename_source(&self, path: &Path) -> Option<PathBuf> {
        let repo = self.repo.lock().expect("git repo lock");
        let workdir = repo.workdir()?.to_path_buf();
        let statuses = repo.statuses(Some(&mut rename_aware_status())).ok()?;

        statuses.iter().find_map(|entry| {
            let (abs, renamed_from) = entry_paths(&entry, &workdir)?;
            (abs == path).then_some(renamed_from).flatten()
        })
    }

    fn conflicted_paths(&self) -> Vec<PathBuf> {
        let repo = self.repo.lock().expect("git repo lock");
        read_index_conflicts(&repo)
            .map(|files| files.into_iter().map(|f| f.path).collect())
            .unwrap_or_default()
    }

    fn conflict_stages(&self, path: &Path) -> Option<ConflictedFile> {
        let repo = self.repo.lock().expect("git repo lock");
        let target = abs_in_workdir(&repo, path);
        read_index_conflicts(&repo)
            .ok()?
            .into_iter()
            .find(|f| f.path == target)
    }

    fn mark_resolved(&self, path: &Path) -> Result<(), GitApplyError> {
        let repo = self.repo.lock().expect("git repo lock");
        let workdir = repo.workdir().map(Path::to_path_buf);
        let rel = workdir
            .as_deref()
            .and_then(|wd| path.strip_prefix(wd).ok())
            .unwrap_or(path);
        let mut index = repo.index().map_err(err_msg)?;
        index.add_path(rel).map_err(err_msg)?;
        index.write().map_err(err_msg)?;
        Ok(())
    }

    fn apply_to_index(&self, patch: &str) -> Result<(), GitApplyError> {
        let repo = self.repo.lock().expect("git repo lock");
        let diff = Diff::from_buffer(patch.as_bytes()).map_err(err_msg)?;
        match repo.apply(&diff, ApplyLocation::Index, None) {
            Ok(()) => Ok(()),
            Err(err) => Err(apply_error(&repo, patch, &err)),
        }
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
                Ok(n) => n,
                Err(_) => return git2::TreeWalkResult::Ok,
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

    fn tree_oid(&self, sha: &str) -> Option<String> {
        let repo = self.repo.lock().expect("git repo lock");
        let oid = git2::Oid::from_str(sha).ok()?;
        Some(repo.find_commit(oid).ok()?.tree_id().to_string())
    }

    fn tree_with_updates(
        &self,
        base_sha: &str,
        updates: &[(PathBuf, Option<String>)],
    ) -> Result<String, GitApplyError> {
        let repo = self.repo.lock().expect("git repo lock");
        let base = git2::Oid::from_str(base_sha)
            .and_then(|oid| repo.find_commit(oid))
            .and_then(|commit| commit.tree())
            .map_err(err_msg)?;

        // The blobs are written before the builder runs because upsert takes an
        // oid, and a blob written for an update the builder then rejects is
        // unreferenced rather than wrong: git gc collects it.
        let mut builder = git2::build::TreeUpdateBuilder::new();
        let mut written = Vec::with_capacity(updates.len());
        for (path, content) in updates {
            match content {
                Some(content) => {
                    written.push((path, repo.blob(content.as_bytes()).map_err(err_msg)?))
                },
                None => {
                    builder.remove(path);
                },
            }
        }
        for (path, blob) in &written {
            builder.upsert(path, *blob, git2::FileMode::Blob);
        }

        builder
            .create_updated(&repo, &base)
            .map(|oid| oid.to_string())
            .map_err(err_msg)
    }

    /// Reads only the blobs the tree diff names, so the cost tracks the
    /// commit's size rather than the repository's.
    ///
    /// A changed file that is not UTF-8 is left out rather than failing the
    /// whole call, so a commit touching a binary alongside source still yields
    /// the source. That is the divergence from [`Self::commit_tree`], which
    /// refuses any tree holding such a blob at all.
    fn changed_contents(
        &self,
        base: Option<&str>,
        new: &str,
    ) -> Option<Vec<(PathBuf, String, String)>> {
        let repo = self.repo.lock().expect("git repo lock");

        let new_tree = {
            let oid = git2::Oid::from_str(new).ok()?;
            repo.find_commit(oid).ok()?.tree().ok()?
        };
        let base_tree = match base {
            Some(base) => {
                let oid = git2::Oid::from_str(base).ok()?;
                Some(repo.find_commit(oid).ok()?.tree().ok()?)
            },
            None => None,
        };

        let mut opts = DiffOptions::new();
        opts.include_typechange(true);
        let diff = repo
            .diff_tree_to_tree(base_tree.as_ref(), Some(&new_tree), Some(&mut opts))
            .ok()?;

        // A side with no blob reads as empty, which is how an addition and a
        // deletion each become one entry rather than a shape of their own.
        let blob_text = |id: git2::Oid| -> Option<String> {
            if id.is_zero() {
                return Some(String::new());
            }
            let blob = repo.find_blob(id).ok()?;
            std::str::from_utf8(blob.content()).ok().map(str::to_string)
        };

        let mut out = Vec::new();
        for delta in diff.deltas() {
            let Some(rel) = delta.new_file().path().or_else(|| delta.old_file().path()) else {
                continue;
            };
            let (Some(before), Some(after)) = (
                blob_text(delta.old_file().id()),
                blob_text(delta.new_file().id()),
            ) else {
                continue;
            };
            if before != after {
                out.push((rel.to_path_buf(), before, after));
            }
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

    fn parent_shas(&self, sha: &str) -> Vec<String> {
        let repo = self.repo.lock().expect("git repo lock");
        let Ok(oid) = git2::Oid::from_str(sha) else {
            return Vec::new();
        };
        let Ok(commit) = repo.find_commit(oid) else {
            return Vec::new();
        };
        commit.parents().map(|p| p.id().to_string()).collect()
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

        walk_history(&repo, start_oid, WalkMode::FirstParent, None, limit)
    }

    fn log_from(&self, start_sha: &str, limit: usize) -> Vec<CommitInfo> {
        if limit == 0 {
            return Vec::new();
        }
        let repo = self.repo.lock().expect("git repo lock");
        let Ok(oid) = git2::Oid::from_str(start_sha) else {
            return Vec::new();
        };
        if repo.find_commit(oid).is_err() {
            return Vec::new();
        }
        walk_history(&repo, oid, WalkMode::FirstParent, None, limit)
    }

    fn log_range(&self, tip_sha: &str, exclude_sha: &str, limit: usize) -> Vec<CommitInfo> {
        if limit == 0 {
            return Vec::new();
        }
        let repo = self.repo.lock().expect("git repo lock");
        let resolve = |sha: &str| {
            let oid = git2::Oid::from_str(sha).ok()?;
            repo.find_commit(oid).ok()?;
            Some(oid)
        };
        let (Some(tip), Some(exclude)) = (resolve(tip_sha), resolve(exclude_sha)) else {
            return Vec::new();
        };

        walk_history(&repo, tip, WalkMode::FirstParent, Some(exclude), limit)
    }

    fn log_graph(&self, start_sha: &str, limit: usize) -> Vec<CommitInfo> {
        if limit == 0 {
            return Vec::new();
        }
        let repo = self.repo.lock().expect("git repo lock");
        let Ok(oid) = git2::Oid::from_str(start_sha) else {
            return Vec::new();
        };
        if repo.find_commit(oid).is_err() {
            return Vec::new();
        }

        walk_history(&repo, oid, WalkMode::FullGraph, None, limit)
    }

    fn resolve_rev(&self, rev: &str) -> Option<String> {
        let repo = self.repo.lock().expect("git repo lock");
        let object = repo.revparse_single(rev).ok()?;
        let commit = object.peel_to_commit().ok()?;
        Some(commit.id().to_string())
    }

    fn local_branches(&self) -> Vec<(String, String)> {
        let repo = self.repo.lock().expect("git repo lock");
        let Ok(branches) = repo.branches(Some(BranchType::Local)) else {
            return Vec::new();
        };
        branches
            .filter_map(|entry| {
                let (branch, _) = entry.ok()?;
                let name = branch.name().ok().flatten()?.to_string();
                let tip = branch.get().peel_to_commit().ok()?.id().to_string();
                Some((name, tip))
            })
            .collect()
    }

    fn head_branch(&self) -> Option<String> {
        let repo = self.repo.lock().expect("git repo lock");
        let head = repo.head().ok()?;
        if !head.is_branch() {
            return None;
        }
        head.shorthand().ok().map(|name| name.to_string())
    }

    fn amend_head(&self, tree_oid: &str, message: Option<&str>) -> Result<String, GitApplyError> {
        let repo = self.repo.lock().expect("git repo lock");
        let head = repo
            .head()
            .and_then(|h| h.peel_to_commit())
            .map_err(err_msg)?;
        let new_tree = git2::Oid::from_str(tree_oid)
            .and_then(|oid| repo.find_tree(oid))
            .map_err(err_msg)?;
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
                return BackendSnafu {
                    reason: format!("cherry-pick conflict at {desc_sha}"),
                }
                .fail();
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
        tree_oid: &str,
        message: &str,
        author_name: &str,
        author_email: &str,
    ) -> Result<String, GitApplyError> {
        let repo = self.repo.lock().expect("git repo lock");
        let tree = git2::Oid::from_str(tree_oid)
            .and_then(|oid| repo.find_tree(oid))
            .map_err(err_msg)?;
        let sig = git2::Signature::now(author_name, author_email).map_err(err_msg)?;
        let parent_commit = match parent_sha {
            Some(sha) => {
                let oid = git2::Oid::from_str(sha).map_err(err_msg)?;
                Some(repo.find_commit(oid).map_err(err_msg)?)
            },
            None => None,
        };
        let parents: Vec<&Commit<'_>> = parent_commit.as_ref().into_iter().collect();
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

    fn checkout_detached(&self, sha: &str) -> Result<(), GitApplyError> {
        let repo = self.repo.lock().expect("git repo lock");
        let oid = git2::Oid::from_str(sha).map_err(err_msg)?;
        let commit = repo.find_commit(oid).map_err(err_msg)?;

        checkout_commit(&repo, &commit)?;
        repo.set_head_detached(oid).map_err(err_msg)
    }

    fn checkout_ref(&self, name: &str) -> Result<(), GitApplyError> {
        let repo = self.repo.lock().expect("git repo lock");
        let full_name = format!("refs/heads/{name}");
        let commit = repo
            .find_reference(&full_name)
            .and_then(|branch| branch.peel_to_commit())
            .map_err(err_msg)?;

        checkout_commit(&repo, &commit)?;
        repo.set_head(&full_name).map_err(err_msg)
    }

    fn set_branch_target(&self, name: &str, sha: &str) -> Result<(), GitApplyError> {
        let repo = self.repo.lock().expect("git repo lock");
        let oid = git2::Oid::from_str(sha).map_err(err_msg)?;
        repo.find_commit(oid).map_err(err_msg)?;

        repo.reference(&format!("refs/heads/{name}"), oid, true, "stoat: amend")
            .map(|_| ())
            .map_err(err_msg)
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
        let mut diff =
            match repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&new_tree), Some(&mut opts)) {
                Ok(d) => d,
                Err(_) => return Vec::new(),
            };
        // Before the indexed patch reads below, which read the delta list this
        // rewrites in place. Without it a commit that moves a file lists a
        // deletion beside an addition.
        let _ = diff.find_similar(Some(&mut rename_detection(false)));

        let deltas = diff.deltas();
        let mut out: Vec<CommitFileChange> = Vec::with_capacity(deltas.len());
        for (i, delta) in deltas.enumerate() {
            let rel_path = match delta.new_file().path().or_else(|| delta.old_file().path()) {
                Some(p) => p.to_path_buf(),
                None => continue,
            };
            let kind = match delta.status() {
                Delta::Added => CommitFileChangeKind::Added,
                Delta::Deleted => CommitFileChangeKind::Deleted,
                Delta::Modified => CommitFileChangeKind::Modified,
                Delta::Renamed => CommitFileChangeKind::Renamed,
                Delta::Typechange => CommitFileChangeKind::TypeChange,
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
            out.push(CommitFileChange {
                rel_path,
                kind,
                additions,
                deletions,
            });
        }
        out
    }

    fn commit_first_path(&self, sha: &str) -> Option<PathBuf> {
        let repo = self.repo.lock().expect("git repo lock");
        let commit = git2::Oid::from_str(sha)
            .and_then(|oid| repo.find_commit(oid))
            .ok()?;
        let new_tree = commit.tree().ok()?;
        let parent_tree = commit.parents().next().and_then(|p| p.tree().ok());

        let mut opts = DiffOptions::new();
        opts.include_typechange(true);
        let mut diff = repo
            .diff_tree_to_tree(parent_tree.as_ref(), Some(&new_tree), Some(&mut opts))
            .ok()?;
        // Same pairing the full listing does, so both name a moved file at the
        // path it moved to rather than at the one it left.
        let _ = diff.find_similar(Some(&mut rename_detection(false)));

        // No patch is built, so no delta is diffed. The delta list alone is
        // what makes this cheap enough to run on the run loop.
        diff.deltas().find_map(|delta| {
            delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(Path::to_path_buf)
        })
    }
}

/// Which edges a history walk follows.
enum WalkMode {
    /// First parents only, so a merge reads as a single row and the branch it
    /// merged stays hidden.
    ///
    /// The walk queue holds one commit at a time, because each pop enqueues
    /// only the first parent. The yield order is the first-parent chain
    /// whatever the sort mode, so this arm runs unsorted and streams lazily.
    FirstParent,
    /// Every parent, so a merge's side branches appear as commits of their own.
    ///
    /// This arm sorts topologically, which a graph layout needs to place a
    /// child above every parent it points at.
    FullGraph,
}

/// Walk history from `start`, newest first, yielding at most `limit` commits.
///
/// Shared by every log method on [`GitRepo`], which differ only in how they
/// pick the starting commit, which edges they follow, and whether they bound
/// the walk. A commit the walk cannot read is skipped rather than truncating
/// the rest of the history.
///
/// A `hide` sha bounds the walk. That commit and everything reachable from it
/// are skipped, which is how a range walk stops at a branch's fork point.
///
/// Cost follows `mode`. A [`WalkMode::FirstParent`] page costs O(`limit`) and
/// touches no commit past the ones it returns, so a paged listing stays fast
/// on a history of any depth. A [`WalkMode::FullGraph`] walk sorts
/// topologically, and libgit2 satisfies that sort only after it traverses
/// every commit reachable from `start`, so even a small `limit` pays for the
/// whole history.
fn walk_history(
    repo: &Repository,
    start: git2::Oid,
    mode: WalkMode,
    hide: Option<git2::Oid>,
    limit: usize,
) -> Vec<CommitInfo> {
    let mut walk = match repo.revwalk() {
        Ok(w) => w,
        Err(_) => return Vec::new(),
    };
    if matches!(mode, WalkMode::FullGraph) && walk.set_sorting(Sort::TOPOLOGICAL).is_err() {
        return Vec::new();
    }
    if matches!(mode, WalkMode::FirstParent) && walk.simplify_first_parent().is_err() {
        return Vec::new();
    }
    if walk.push(start).is_err() {
        return Vec::new();
    }
    if let Some(hide) = hide
        && walk.hide(hide).is_err()
    {
        return Vec::new();
    }

    // Cap the reserved capacity so callers passing `usize::MAX` as
    // "unbounded" don't trigger an allocation overflow. The Vec grows on
    // demand if the walk actually yields more rows.
    let mut out: Vec<CommitInfo> = Vec::with_capacity(limit.min(4096));
    for oid_res in walk.take(limit) {
        let Ok(oid) = oid_res else { continue };
        let Ok(commit) = repo.find_commit(oid) else {
            continue;
        };
        let sha = oid.to_string();
        let short_sha = sha.chars().take(7).collect();
        let summary = commit
            .summary()
            .ok()
            .flatten()
            .unwrap_or_default()
            .trim()
            .to_string();
        let author = commit.author();
        let author_name = author.name().unwrap_or_default().to_string();
        let author_email = author.email().unwrap_or_default().to_string();
        let time = commit.time().seconds();
        let parents = commit.parent_ids().map(|id| id.to_string()).collect();
        out.push(CommitInfo {
            sha,
            short_sha,
            summary,
            author_name,
            author_email,
            time,
            parents,
        });
    }
    out
}

/// Update the working tree and index to `commit`'s tree without moving HEAD.
///
/// Safe mode means libgit2 refuses the whole checkout when a tracked file
/// carries local modifications it would have to overwrite. HEAD is left for the
/// caller to move afterwards, so a refusal leaves the repository exactly as it
/// was rather than stranding HEAD at a commit the files do not match.
fn checkout_commit(repo: &Repository, commit: &Commit<'_>) -> Result<(), GitApplyError> {
    let mut opts = CheckoutBuilder::new();
    opts.safe();
    repo.checkout_tree(commit.as_object(), Some(&mut opts))
        .map_err(err_msg)
}

fn err_msg(e: git2::Error) -> GitApplyError {
    BackendSnafu {
        reason: e.message().to_string(),
    }
    .build()
}

/// Builds the error for a failed index apply, widening libgit2's message with
/// the file the patch targets and the first preimage line that diverges from
/// the index.
///
/// libgit2 reports only "hunk at line N did not apply", which names neither
/// the file nor what it found there, so a report of the failure carries
/// nothing to diagnose from. The full patch is warn-logged instead of folded
/// into the reason, because callers put the reason in the one-line status bar
/// while the log can hold a complete repro.
fn apply_error(repo: &Repository, patch: &str, err: &git2::Error) -> GitApplyError {
    let reason = err.message();
    tracing::warn!(
        target: "stoat::git",
        reason,
        patch,
        "applying patch to index failed",
    );

    let Some(rel) = patch_target_path(patch) else {
        return BackendSnafu {
            reason: reason.to_string(),
        }
        .build();
    };

    let detail = apply_mismatch_detail(patch, index_blob_text(repo, rel).as_deref());
    BackendSnafu {
        reason: format!("{reason} ({}: {detail})", rel.display()),
    }
    .build()
}

/// The file a unified-diff patch targets, read from its `+++ b/<path>` header
/// and falling back to `--- a/<path>` when the new side is `/dev/null`.
///
/// Only handles the headers stoat itself emits in
/// [`rows_to_unified_diff`](crate::review_apply::patch), which is the sole
/// source of the patches reaching the index.
fn patch_target_path(patch: &str) -> Option<&Path> {
    header_path(patch, "+++ ").or_else(|| header_path(patch, "--- "))
}

/// The path on the first `marker`-prefixed header line, with its `a/` or `b/`
/// prefix removed. [`None`] when no such header exists or when that side is
/// `/dev/null`, which marks the file as absent on that side rather than named.
///
/// Body lines can also start with `-` or `+`, so the first match wins. The
/// headers always precede the hunks.
fn header_path<'a>(patch: &'a str, marker: &str) -> Option<&'a Path> {
    let rest = patch.lines().find_map(|line| line.strip_prefix(marker))?;
    if rest == "/dev/null" {
        return None;
    }
    let rel = rest
        .strip_prefix("a/")
        .or_else(|| rest.strip_prefix("b/"))
        .unwrap_or(rest);
    Some(Path::new(rel))
}

/// Describes where `patch`'s preimage stops matching `index_text`, as the
/// parenthetical of an apply-failure reason.
///
/// Walks every `@@ -start,count` hunk and compares the context and `-` lines
/// against the index at the line numbers the hunk claims, reporting the first
/// divergence.
///
/// When nothing diverges the wording names no line at all. A patch whose text
/// matches can still be rejected over line endings, trailing whitespace, or a
/// mode change, and pointing at an innocent line would send the reader the
/// wrong way.
fn apply_mismatch_detail(patch: &str, index_text: Option<&str>) -> String {
    let Some(index_text) = index_text else {
        return "file not in index".to_string();
    };
    let index_lines: Vec<&str> = index_text.lines().collect();

    let mut idx = 0usize;
    let mut in_hunk = false;
    for line in patch.lines() {
        if let Some(start) = hunk_preimage_start(line) {
            idx = start.saturating_sub(1);
            in_hunk = true;
            continue;
        }
        if line.starts_with("diff --git") {
            in_hunk = false;
            continue;
        }
        if !in_hunk {
            continue;
        }

        let expected = match line.as_bytes().first() {
            Some(b' ' | b'-') => &line[1..],
            _ => continue,
        };
        let Some(actual) = index_lines.get(idx) else {
            return format!(
                "index ends at line {} but patch expects line {} to be {}",
                index_lines.len(),
                idx + 1,
                quoted_line(expected)
            );
        };
        if *actual != expected {
            return format!(
                "index line {} is {} but patch expects {}",
                idx + 1,
                quoted_line(actual),
                quoted_line(expected)
            );
        }
        idx += 1;
    }

    "patch preimage matches the index".to_string()
}

/// The 1-based preimage line a `@@ -start,count +start,count @@` header opens
/// at, or [`None`] for any other line.
fn hunk_preimage_start(line: &str) -> Option<usize> {
    let rest = line.strip_prefix("@@ -")?;
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Quotes `text` for an error reason, truncating past [`QUOTED_LINE_MAX`]
/// characters so one long source line cannot crowd out the rest.
fn quoted_line(text: &str) -> String {
    match text.char_indices().nth(QUOTED_LINE_MAX) {
        Some((cut, _)) => format!("\"{}...\"", &text[..cut]),
        None => format!("\"{text}\""),
    }
}

/// The staged (stage 0) blob for `rel` as text, or [`None`] when the path is
/// absent from the index or its content is not valid UTF-8.
///
/// Takes the [`Repository`] directly rather than going through
/// [`GitRepo::index_content`], because the apply path already holds the repo
/// mutex and it is not reentrant.
///
/// Line terminators stay as the blob carries them, unlike
/// [`GitRepo::index_content`], which normalizes what it gets from here. The
/// apply path quotes this text back in a rejected-patch reason, so it must
/// describe the blob git applies against rather than the buffer's view of it.
fn index_blob_text(repo: &Repository, rel: &Path) -> Option<String> {
    let entry = repo.index().ok()?.get_path(rel, 0)?;
    let blob = repo.find_blob(entry.id).ok()?;
    std::str::from_utf8(blob.content()).ok().map(String::from)
}

/// Returns `path` unchanged when it is already absolute, otherwise joins
/// it onto the repo workdir so it lines up with the absolute paths that
/// [`read_index_conflicts`] produces.
fn abs_in_workdir(repo: &Repository, path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    repo.workdir()
        .map(|wd| wd.join(path))
        .unwrap_or_else(|| path.to_path_buf())
}

/// Every unmerged entry in the repository's on-disk index as a
/// [`ConflictedFile`] with an absolute path. Each stage blob is `None` when
/// that side is absent or not valid UTF-8, mirroring the cherry-pick path.
fn read_index_conflicts(repo: &Repository) -> Result<Vec<ConflictedFile>, GitApplyError> {
    let workdir = repo.workdir().map(Path::to_path_buf);
    let index = repo.index().map_err(err_msg)?;
    let mut out = Vec::new();
    for conflict in index.conflicts().map_err(err_msg)? {
        let conflict = conflict.map_err(err_msg)?;
        let rel_bytes = conflict
            .ancestor
            .as_ref()
            .map(|e| e.path.clone())
            .or_else(|| conflict.our.as_ref().map(|e| e.path.clone()))
            .or_else(|| conflict.their.as_ref().map(|e| e.path.clone()))
            .unwrap_or_default();
        let rel = PathBuf::from(std::str::from_utf8(&rel_bytes).unwrap_or(""));
        let path = match &workdir {
            Some(wd) => wd.join(&rel),
            None => rel,
        };
        let ancestor = conflict
            .ancestor
            .as_ref()
            .and_then(|e| tree::read_blob(repo, e.id));
        let ours = conflict
            .our
            .as_ref()
            .and_then(|e| tree::read_blob(repo, e.id));
        let theirs = conflict
            .their
            .as_ref()
            .and_then(|e| tree::read_blob(repo, e.id));
        out.push(ConflictedFile {
            path,
            ancestor,
            ours,
            theirs,
        });
    }
    Ok(out)
}

/// Status options that report a moved file as one renamed entry on either side
/// of the index, rather than as a deletion paired with an addition.
fn rename_aware_status() -> StatusOptions {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .renames_head_to_index(true)
        .renames_index_to_workdir(true);
    opts
}

/// Find options that pair a diff's deletions and additions back into renames.
///
/// `untracked` extends the pairing to files git has never seen, which a diff
/// only carries when it was built with untracked content shown.
fn rename_detection(untracked: bool) -> DiffFindOptions {
    let mut opts = DiffFindOptions::new();
    opts.renames(true).for_untracked(untracked);
    opts
}

/// The absolute path `entry` names, and the absolute path it moved from when
/// the entry is a rename.
///
/// [`StatusEntry::path`] reads the *old* side of a staged rename, so the
/// current path has to come off the entry's delta instead. Returns `None` for
/// an entry naming no path at all, such as one whose bytes are not UTF-8.
fn entry_paths(entry: &StatusEntry<'_>, workdir: &Path) -> Option<(PathBuf, Option<PathBuf>)> {
    let status = entry.status();
    let renamed = if status.intersects(Status::INDEX_RENAMED) {
        entry.head_to_index()
    } else if status.intersects(Status::WT_RENAMED) {
        entry.index_to_workdir()
    } else {
        None
    };

    match renamed {
        Some(delta) => Some((
            workdir.join(delta.new_file().path()?),
            delta.old_file().path().map(|old| workdir.join(old)),
        )),
        None => Some((workdir.join(entry.path().ok()?), None)),
    }
}

/// Hunks in `diff`, reporting each delta's path and its own count to `per_file`
/// along the way.
///
/// The hunk callback is the only one asked for, so libgit2 never walks the
/// lines a count does not read. A delta names its new path, except a deletion,
/// which has only an old one.
fn count_hunks(diff: &Diff<'_>, per_file: &mut dyn FnMut(PathBuf, usize)) -> usize {
    let mut total = 0;
    let mut current: Option<(PathBuf, usize)> = None;
    let _ = diff.foreach(
        &mut |_, _| true,
        None,
        Some(&mut |delta, _| {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(Path::to_path_buf);
            total += 1;
            match (&mut current, path) {
                (Some((held, count)), Some(path)) if *held == path => *count += 1,
                (held, Some(path)) => {
                    if let Some((done, count)) = held.take() {
                        per_file(done, count);
                    }
                    *held = Some((path, 1));
                },
                (_, None) => {},
            }
            true
        }),
        None,
    );
    if let Some((done, count)) = current {
        per_file(done, count);
    }

    // An untracked file carries no hunk through the walk above, because the
    // diff was built without `show_untracked_content` and libgit2 reads no
    // content it was not asked for. Its whole content is the work, so it owes
    // one hunk when it holds bytes and none when it is empty. The sizes come
    // off the deltas, which is metadata the diff already holds.
    for delta in diff.deltas() {
        if delta.status() != Delta::Untracked || delta.new_file().size() == 0 {
            continue;
        }
        let Some(path) = delta.new_file().path().map(Path::to_path_buf) else {
            continue;
        };
        total += 1;
        per_file(path, 1);
    }
    total
}

#[cfg(test)]
mod tests {
    use super::{apply_mismatch_detail, patch_target_path, LocalGit, QUOTED_LINE_MAX};
    use crate::host::git::{CherryPickOutcome, GitHost, GitRepo};
    use git2::{Oid, Repository, RepositoryInitOptions, Signature};
    use std::{
        collections::BTreeMap,
        path::{Path, PathBuf},
        sync::Arc,
    };
    use tempfile::TempDir;

    const NAMED_SIDES: &str = "--- a/a.rs\n+++ b/a.rs\n";

    fn patch(header: &str, body: &str) -> String {
        format!("diff --git a/a.rs b/a.rs\n{header}@@ -1,2 +1,2 @@\n{body}")
    }

    #[test]
    fn target_path_reads_the_new_side() {
        assert_eq!(
            patch_target_path(&patch(NAMED_SIDES, "")),
            Some(Path::new("a.rs"))
        );
    }

    #[test]
    fn target_path_falls_back_to_old_side_for_deletion() {
        assert_eq!(
            patch_target_path(&patch("--- a/gone.rs\n+++ /dev/null\n", "")),
            Some(Path::new("gone.rs"))
        );
    }

    #[test]
    fn target_path_reads_new_side_for_addition() {
        assert_eq!(
            patch_target_path(&patch("--- /dev/null\n+++ b/new.rs\n", "")),
            Some(Path::new("new.rs"))
        );
    }

    #[test]
    fn target_path_none_without_headers() {
        assert_eq!(patch_target_path("not a patch"), None);
    }

    #[test]
    fn mismatch_detail_names_first_diverging_line() {
        let patch = patch(NAMED_SIDES, " one\n-two\n+TWO\n");
        assert_eq!(
            apply_mismatch_detail(&patch, Some("one\nother\n")),
            "index line 2 is \"other\" but patch expects \"two\""
        );
    }

    #[test]
    fn mismatch_detail_counts_from_the_hunk_header() {
        let patch =
            format!("diff --git a/a.rs b/a.rs\n{NAMED_SIDES}@@ -4,1 +4,1 @@\n-four\n+FOUR\n");
        assert_eq!(
            apply_mismatch_detail(&patch, Some("a\nb\nc\nd\n")),
            "index line 4 is \"d\" but patch expects \"four\""
        );
    }

    #[test]
    fn mismatch_detail_reports_short_index() {
        let patch = patch(NAMED_SIDES, " one\n-two\n+TWO\n");
        assert_eq!(
            apply_mismatch_detail(&patch, Some("one\n")),
            "index ends at line 1 but patch expects line 2 to be \"two\""
        );
    }

    #[test]
    fn mismatch_detail_reports_missing_file() {
        assert_eq!(
            apply_mismatch_detail(&patch(NAMED_SIDES, "-x\n"), None),
            "file not in index"
        );
    }

    #[test]
    fn mismatch_detail_claims_no_line_when_preimage_matches() {
        let patch = patch(NAMED_SIDES, " one\n-two\n+TWO\n");
        assert_eq!(
            apply_mismatch_detail(&patch, Some("one\ntwo\n")),
            "patch preimage matches the index"
        );
    }

    #[test]
    fn mismatch_detail_ignores_added_lines() {
        let patch = patch(NAMED_SIDES, "+added\n one\n");
        assert_eq!(
            apply_mismatch_detail(&patch, Some("one\n")),
            "patch preimage matches the index"
        );
    }

    #[test]
    fn mismatch_detail_truncates_long_lines() {
        let long = "x".repeat(QUOTED_LINE_MAX + 10);
        let patch = patch(NAMED_SIDES, &format!("-{long}\n"));
        assert_eq!(
            apply_mismatch_detail(&patch, Some("short\n")),
            format!(
                "index line 1 is \"short\" but patch expects \"{}...\"",
                "x".repeat(QUOTED_LINE_MAX)
            )
        );
    }

    /// A repo with three commits on `main`, returned oldest-sha first.
    ///
    /// The initial branch is pinned rather than left to libgit2, whose default
    /// comes from the ambient git config and would otherwise make the branch
    /// assertions depend on whoever runs the tests.
    fn seeded_repo() -> (TempDir, Repository, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let repo = {
            let mut opts = RepositoryInitOptions::new();
            opts.initial_head("main");
            Repository::init_opts(dir.path(), &opts).unwrap()
        };

        let sig = Signature::now("test", "t@t").unwrap();
        let mut shas: Vec<String> = Vec::new();
        for content in ["one", "two", "three"] {
            std::fs::write(dir.path().join("a.txt"), content).unwrap();
            let tree = {
                let mut index = repo.index().unwrap();
                index.add_path(Path::new("a.txt")).unwrap();
                index.write().unwrap();
                repo.find_tree(index.write_tree().unwrap()).unwrap()
            };

            let parent = shas
                .last()
                .map(|sha| repo.find_commit(Oid::from_str(sha).unwrap()).unwrap());
            let parents: Vec<_> = parent.iter().collect();
            let oid = repo
                .commit(Some("HEAD"), &sig, &sig, content, &tree, &parents)
                .unwrap();
            shas.push(oid.to_string());
        }

        (dir, repo, shas)
    }

    fn discover(dir: &TempDir) -> Arc<dyn GitRepo> {
        LocalGit::new().discover(dir.path()).unwrap()
    }

    /// Commit `contents` on top of HEAD, adding every named path to the index.
    fn commit_files(repo: &Repository, dir: &TempDir, files: &[(&str, &[u8])]) -> String {
        let sig = Signature::now("test", "t@t").unwrap();
        for (name, bytes) in files {
            std::fs::write(dir.path().join(name), bytes).unwrap();
        }
        let tree = {
            let mut index = repo.index().unwrap();
            for (name, _) in files {
                index.add_path(Path::new(name)).unwrap();
            }
            index.write().unwrap();
            repo.find_tree(index.write_tree().unwrap()).unwrap()
        };
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "add", &tree, &[&parent])
            .unwrap()
            .to_string()
    }

    /// Every entry of the tree `oid` names, as `(path, blob oid)`.
    fn entries(repo: &Repository, oid: &str) -> BTreeMap<PathBuf, String> {
        let tree = repo.find_tree(Oid::from_str(oid).unwrap()).unwrap();
        let mut out = BTreeMap::new();
        tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
            if entry.kind() == Some(git2::ObjectType::Blob) {
                let name = entry.name().expect("the fixture writes utf-8 names");
                let rel = if dir.is_empty() {
                    PathBuf::from(name)
                } else {
                    PathBuf::from(dir).join(name)
                };
                out.insert(rel, entry.id().to_string());
            }
            git2::TreeWalkResult::Ok
        })
        .unwrap();
        out
    }

    /// `commit_tree` refuses a tree holding anything that is not UTF-8, which is
    /// why amend and reword cannot run in a repository carrying a font. Updating
    /// a tree by oid never reads the blob it does not name, so the binary rides
    /// through untouched.
    #[test]
    fn tree_with_updates_keeps_a_binary_blob_it_does_not_name() {
        let (dir, repo, _) = seeded_repo();
        let binary = [0u8, 159, 146, 150];
        let base = commit_files(&repo, &dir, &[("font.ttf", &binary), ("b.txt", b"keep")]);
        let git = discover(&dir);

        assert_eq!(
            git.commit_tree(&base),
            None,
            "reading the tree as text refuses it, which is the fault this avoids",
        );

        let oid = git
            .tree_with_updates(
                &base,
                &[(PathBuf::from("b.txt"), Some("edited".to_string()))],
            )
            .expect("the update writes a tree");

        let blob = {
            let tree = repo.find_tree(Oid::from_str(&oid).unwrap()).unwrap();
            let entry = tree.get_path(Path::new("font.ttf")).unwrap();
            repo.find_blob(entry.id()).unwrap().content().to_vec()
        };
        assert_eq!(
            blob, binary,
            "the blob nothing named came through as it was"
        );
    }

    /// The cost of the old shape was re-hashing every blob in the tree. What
    /// makes the new one cheap is that an entry the updates do not name keeps
    /// the oid it already had, so this pins the oids rather than the contents.
    #[test]
    fn tree_with_updates_leaves_every_unnamed_entry_at_its_own_oid() {
        let (dir, repo, _) = seeded_repo();
        let base = commit_files(&repo, &dir, &[("b.txt", b"keep"), ("c.txt", b"also keep")]);
        let git = discover(&dir);
        let before = entries(&repo, &git.tree_oid(&base).expect("the commit has a tree"));

        let after = entries(
            &repo,
            &git.tree_with_updates(
                &base,
                &[(PathBuf::from("b.txt"), Some("edited".to_string()))],
            )
            .expect("the update writes a tree"),
        );

        assert_eq!(
            after
                .iter()
                .filter(|(path, oid)| before.get(*path) != Some(*oid))
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>(),
            vec![PathBuf::from("b.txt")],
            "only the named path got a new blob",
        );
        assert_eq!(
            after.keys().collect::<Vec<_>>(),
            before.keys().collect::<Vec<_>>(),
            "and the tree still holds the same paths",
        );
    }

    /// A conflict resolved by deleting a file, and a rebased pick over a commit
    /// that removed one, both need a path taken out rather than blanked.
    #[test]
    fn tree_with_updates_removes_the_path_a_none_names() {
        let (dir, repo, _) = seeded_repo();
        let base = commit_files(&repo, &dir, &[("b.txt", b"keep"), ("gone.txt", b"bye")]);
        let git = discover(&dir);

        let oid = git
            .tree_with_updates(&base, &[(PathBuf::from("gone.txt"), None)])
            .expect("the update writes a tree");

        assert_eq!(
            entries(&repo, &oid).keys().cloned().collect::<Vec<_>>(),
            vec![PathBuf::from("a.txt"), PathBuf::from("b.txt")],
            "the removed path is gone and the rest is untouched",
        );
    }

    /// The oid is what a caller carries in place of the whole tree, so it has to
    /// name the tree the commit actually has.
    #[test]
    fn tree_oid_names_the_commits_own_tree() {
        let (dir, repo, shas) = seeded_repo();
        let git = discover(&dir);

        let commit = repo.find_commit(Oid::from_str(&shas[2]).unwrap()).unwrap();
        assert_eq!(
            git.tree_oid(&shas[2]),
            Some(commit.tree_id().to_string()),
            "the oid is the commit's tree",
        );
        assert_eq!(git.tree_oid("nope"), None, "an unknown sha names no tree");
    }

    /// A pick used to read its merged tree out as text, which skipped every
    /// blob that was not UTF-8 and so took the file out of the commit the
    /// rebase then wrote. Carrying the tree by oid never reads the blob at all.
    #[test]
    fn a_pick_keeps_a_binary_blob_in_its_tree() {
        let (dir, repo, shas) = seeded_repo();
        let binary = [0u8, 159, 146, 150];
        let with_font = commit_files(&repo, &dir, &[("font.ttf", &binary)]);
        let git = discover(&dir);

        let outcome = git
            .cherry_pick_tree(&with_font, &shas[1])
            .expect("adding a file applies cleanly onto an older commit");
        let CherryPickOutcome::Clean { tree, .. } = outcome else {
            panic!("the pick adds a path neither side touched, so it is clean");
        };

        let picked = {
            let tree = repo.find_tree(Oid::from_str(&tree).unwrap()).unwrap();
            let entry = tree
                .get_path(Path::new("font.ttf"))
                .expect("the picked tree still holds the file the commit added");
            repo.find_blob(entry.id()).unwrap().content().to_vec()
        };
        assert_eq!(picked, binary, "and holds it byte for byte");
    }

    /// The amend walk used to read the whole HEAD tree as text before it could
    /// rewrite one path, so a repository holding a font refused every amend
    /// with nothing rewritten. Building the tree from the one path it changes
    /// never reads the rest.
    #[test]
    fn an_amend_over_a_tree_holding_a_binary_blob_keeps_the_blob() {
        let (dir, repo, _) = seeded_repo();
        let binary = [0u8, 159, 146, 150];
        let head = commit_files(&repo, &dir, &[("font.ttf", &binary), ("b.txt", b"before")]);
        let git = discover(&dir);

        assert_eq!(
            git.commit_tree(&head),
            None,
            "reading the tree as text refuses it, which is what blocked the amend",
        );

        let tree = git
            .tree_with_updates(
                &head,
                &[(PathBuf::from("b.txt"), Some("after".to_string()))],
            )
            .expect("the amended tree builds from the one path it changes");
        let amended = git.amend_head(&tree, None).expect("the amend commits");

        let entries = {
            let tree = repo.find_commit(Oid::from_str(&amended).unwrap()).unwrap();
            let tree = tree.tree().unwrap();
            let blob = |name: &str| {
                let entry = tree.get_path(Path::new(name)).expect("the path survived");
                repo.find_blob(entry.id()).unwrap().content().to_vec()
            };
            (blob("font.ttf"), blob("b.txt"))
        };
        assert_eq!(
            entries,
            (binary.to_vec(), b"after".to_vec()),
            "the binary rode through untouched and the named path was rewritten",
        );
    }

    #[test]
    fn resolve_rev_reads_branches_shas_and_expressions() {
        let (dir, _repo, shas) = seeded_repo();
        let git = discover(&dir);

        assert_eq!(git.resolve_rev("main").as_deref(), Some(shas[2].as_str()));
        assert_eq!(
            git.resolve_rev(&shas[0][..7]).as_deref(),
            Some(shas[0].as_str())
        );
        assert_eq!(git.resolve_rev("HEAD~1").as_deref(), Some(shas[1].as_str()));
        assert_eq!(git.resolve_rev("nope"), None);
    }

    #[test]
    fn log_from_includes_the_start_and_stops_at_the_root() {
        let (dir, _repo, shas) = seeded_repo();
        let git = discover(&dir);

        let walked: Vec<String> = git
            .log_from(&shas[1], 10)
            .into_iter()
            .map(|c| c.sha)
            .collect();
        assert_eq!(walked, [shas[1].clone(), shas[0].clone()]);
        assert!(git.log_from(&"0".repeat(40), 10).is_empty());
    }

    #[test]
    fn parent_shas_reports_both_sides_of_a_merge() {
        let (dir, repo, shas) = seeded_repo();
        let sig = Signature::now("test", "t@t").unwrap();
        let commit_at = |sha: &str| repo.find_commit(Oid::from_str(sha).unwrap()).unwrap();

        // Committed with no ref to update, so `main` stays at shas[2] and the
        // merge is reachable only by the sha the test holds.
        let side = {
            let root = commit_at(&shas[0]);
            let tree = root.tree().unwrap();
            repo.commit(None, &sig, &sig, "side", &tree, &[&root])
                .unwrap()
                .to_string()
        };
        let merge = {
            let (mainline, branch) = (commit_at(&shas[2]), commit_at(&side));
            let tree = mainline.tree().unwrap();
            repo.commit(None, &sig, &sig, "merge", &tree, &[&mainline, &branch])
                .unwrap()
                .to_string()
        };

        let git = discover(&dir);
        assert_eq!(
            git.parent_shas(&merge),
            [shas[2].clone(), side],
            "mainline first"
        );
        assert_eq!(git.parent_shas(&shas[1]), [shas[0].clone()]);
        assert!(
            git.parent_shas(&shas[0]).is_empty(),
            "a root has no parents"
        );
        assert!(git.parent_shas(&"0".repeat(40)).is_empty());
    }

    #[test]
    fn log_range_lists_only_what_a_merge_brought_in() {
        let (dir, repo, shas) = seeded_repo();
        let sig = Signature::now("test", "t@t").unwrap();
        let commit_at = |sha: &str| repo.find_commit(Oid::from_str(sha).unwrap()).unwrap();

        // A two-commit branch forked from the root, so the walk from its tip
        // would run into mainline history if the exclude did not bound it.
        let mut branch: Vec<String> = Vec::new();
        for message in ["f1", "f2"] {
            let parent = match branch.last() {
                Some(sha) => commit_at(sha),
                None => commit_at(&shas[0]),
            };
            let tree = parent.tree().unwrap();
            let sha = repo
                .commit(None, &sig, &sig, message, &tree, &[&parent])
                .unwrap()
                .to_string();
            branch.push(sha);
        }
        let merge = {
            let (mainline, tip) = (commit_at(&shas[2]), commit_at(&branch[1]));
            let tree = mainline.tree().unwrap();
            repo.commit(None, &sig, &sig, "merge", &tree, &[&mainline, &tip])
                .unwrap()
                .to_string()
        };

        let git = discover(&dir);
        let parents = git.parent_shas(&merge);
        let walked: Vec<String> = git
            .log_range(&parents[1], &parents[0], 10)
            .into_iter()
            .map(|c| c.sha)
            .collect();
        assert_eq!(
            walked,
            [branch[1].clone(), branch[0].clone()],
            "the fork point bounds the walk"
        );

        let unknown = "0".repeat(40);
        assert!(git.log_range(&unknown, &shas[2], 10).is_empty());
        assert!(git.log_range(&branch[1], &unknown, 10).is_empty());
        assert!(git.log_range(&branch[1], &shas[2], 0).is_empty());
    }

    #[test]
    fn log_graph_reaches_the_commits_a_first_parent_walk_hides() {
        let (dir, repo, shas) = seeded_repo();
        let sig = Signature::now("test", "t@t").unwrap();
        let commit_at = |sha: &str| repo.find_commit(Oid::from_str(sha).unwrap()).unwrap();

        let side = {
            let root = commit_at(&shas[0]);
            let tree = root.tree().unwrap();
            repo.commit(None, &sig, &sig, "side", &tree, &[&root])
                .unwrap()
                .to_string()
        };
        let merge = {
            let (mainline, branch) = (commit_at(&shas[2]), commit_at(&side));
            let tree = mainline.tree().unwrap();
            repo.commit(None, &sig, &sig, "merge", &tree, &[&mainline, &branch])
                .unwrap()
                .to_string()
        };

        let git = discover(&dir);
        let first_parent: Vec<String> = git
            .log_from(&merge, 10)
            .into_iter()
            .map(|c| c.sha)
            .collect();
        assert!(
            !first_parent.contains(&side),
            "the side branch stays hidden from a first-parent walk"
        );

        let mut graph: Vec<String> = git
            .log_graph(&merge, 10)
            .into_iter()
            .map(|c| c.sha)
            .collect();
        graph.sort();
        let mut expected = vec![
            merge,
            side,
            shas[2].clone(),
            shas[1].clone(),
            shas[0].clone(),
        ];
        expected.sort();
        assert_eq!(graph, expected, "every commit reachable, each exactly once");

        assert!(git.log_graph(&"0".repeat(40), 10).is_empty());
    }

    #[test]
    fn head_branch_follows_attachment() {
        let (dir, repo, shas) = seeded_repo();
        let git = discover(&dir);
        assert_eq!(git.head_branch().as_deref(), Some("main"));

        repo.set_head_detached(Oid::from_str(&shas[0]).unwrap())
            .unwrap();
        assert_eq!(git.head_branch(), None, "a detached HEAD names no branch");
    }

    #[test]
    fn local_branches_pairs_every_branch_with_its_tip() {
        let (dir, repo, shas) = seeded_repo();
        let root = repo.find_commit(Oid::from_str(&shas[0]).unwrap()).unwrap();
        repo.branch("feature", &root, false).unwrap();

        let mut branches = discover(&dir).local_branches();
        branches.sort();
        assert_eq!(
            branches,
            [
                ("feature".to_string(), shas[0].clone()),
                ("main".to_string(), shas[2].clone()),
            ]
        );
    }

    #[test]
    fn checkout_detached_rewrites_the_working_tree() {
        let (dir, _repo, shas) = seeded_repo();
        let git = discover(&dir);
        let tracked = dir.path().join("a.txt");

        git.checkout_detached(&shas[0]).unwrap();
        assert_eq!(std::fs::read_to_string(&tracked).unwrap(), "one");
        assert_eq!(git.resolve_rev("HEAD").as_deref(), Some(shas[0].as_str()));
        assert_eq!(git.head_branch(), None);
    }

    #[test]
    fn checkout_ref_returns_to_the_branch_tip() {
        let (dir, _repo, shas) = seeded_repo();
        let git = discover(&dir);
        let tracked = dir.path().join("a.txt");

        git.checkout_detached(&shas[0]).unwrap();
        git.checkout_ref("main").unwrap();
        assert_eq!(std::fs::read_to_string(&tracked).unwrap(), "three");
        assert_eq!(git.head_branch().as_deref(), Some("main"));
    }

    #[test]
    fn checkout_refuses_over_a_dirty_tracked_file() {
        let (dir, _repo, shas) = seeded_repo();
        let git = discover(&dir);
        let tracked = dir.path().join("a.txt");
        std::fs::write(&tracked, "local edit").unwrap();

        assert!(git.checkout_detached(&shas[0]).is_err());
        assert_eq!(
            std::fs::read_to_string(&tracked).unwrap(),
            "local edit",
            "a refused checkout must not overwrite the edit"
        );
        assert_eq!(
            git.resolve_rev("HEAD").as_deref(),
            Some(shas[2].as_str()),
            "a refused checkout must not move HEAD"
        );
    }

    #[test]
    fn checkout_ref_rejects_an_unknown_branch() {
        let (dir, _repo, shas) = seeded_repo();
        let git = discover(&dir);

        assert!(git.checkout_ref("nope").is_err());
        assert_eq!(git.head_branch().as_deref(), Some("main"));
        assert_eq!(git.resolve_rev("HEAD").as_deref(), Some(shas[2].as_str()));
    }
}

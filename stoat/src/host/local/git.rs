mod rebase;
mod tree;

use crate::host::git::{
    BackendSnafu, ChangedFile, CherryPickOutcome, CommitFileChange, CommitFileChangeKind,
    CommitInfo, ConflictedFile, GitApplyError, GitHost, GitRepo, RebaseError, RebaseTodo,
    RewriteResult,
};
use git2::{
    ApplyLocation, Diff, DiffOptions, Repository, RepositoryState, Sort, Status, StatusOptions,
};
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

        let statuses = {
            let mut opts = StatusOptions::new();
            opts.include_untracked(true).recurse_untracked_dirs(true);
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
                Ok(p) => p,
                Err(_) => continue,
            };
            let abs = workdir.join(rel);
            let status = entry.status();

            if status.intersects(STAGED) {
                staged_paths.insert(abs.clone());
                staged.push(ChangedFile {
                    path: abs,
                    staged: true,
                    untracked: false,
                });
            } else if status.intersects(UNSTAGED) && !staged_paths.contains(&abs) {
                unstaged.push(ChangedFile {
                    path: abs,
                    staged: false,
                    untracked: status.intersects(Status::WT_NEW),
                });
            }
        }

        staged.sort_by(|a, b| a.path.cmp(&b.path));
        unstaged.sort_by(|a, b| a.path.cmp(&b.path));
        staged.extend(unstaged);
        staged
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
                std::str::from_utf8(blob.content()).ok().map(String::from)
            })
            .collect()
    }

    fn index_content(&self, path: &Path) -> Option<String> {
        let repo = self.repo.lock().expect("git repo lock");
        let workdir = repo.workdir()?;
        let rel = path.strip_prefix(workdir).ok()?;
        index_blob_text(&repo, rel)
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

#[cfg(test)]
mod tests {
    use super::{apply_mismatch_detail, patch_target_path, QUOTED_LINE_MAX};
    use std::path::Path;

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
}

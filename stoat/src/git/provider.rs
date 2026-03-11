use crate::git::{
    blame::BlameData,
    diff_review::DiffComparisonMode,
    repository::{CommitFileChange, CommitLogEntry, GitError, Repository},
    status::{GitBranchInfo, GitStatusEntry, GitStatusError},
};
use async_trait::async_trait;
use std::{
    any::Any,
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug)]
pub enum ApplyLocation {
    Index,
    WorkDir,
}

#[async_trait]
pub trait GitProvider: Send + Sync {
    fn as_any(&self) -> &dyn Any;
    async fn discover(&self, path: &Path) -> Result<Box<dyn GitRepo>, GitError>;
    async fn open(&self, path: &Path) -> Result<Box<dyn GitRepo>, GitError>;
}

#[async_trait]
pub trait GitRepo: Send + Sync {
    fn workdir(&self) -> &Path;
    async fn head_content(&self, path: &Path) -> Result<String, GitError>;
    async fn index_content(&self, path: &Path) -> Result<String, GitError>;
    async fn parent_content(&self, path: &Path) -> Result<String, GitError>;
    async fn gather_status(&self) -> Result<Vec<GitStatusEntry>, GitStatusError>;
    async fn gather_branch_info(&self) -> Option<GitBranchInfo>;
    async fn blame_file(&self, path: &Path) -> Result<BlameData, GitError>;
    async fn count_hunks_by_file(
        &self,
        mode: DiffComparisonMode,
    ) -> Result<HashMap<PathBuf, usize>, GitError>;
    async fn commit_changed_files(&self) -> Result<Vec<PathBuf>, GitError>;
    async fn commit_files_by_oid(&self, oid: &str) -> Result<Vec<CommitFileChange>, GitError>;
    async fn commit_file_diff(&self, oid: &str, path: &Path) -> Result<String, GitError>;
    async fn apply_diff(
        &self,
        patch: &str,
        reverse: bool,
        location: ApplyLocation,
    ) -> Result<(), GitError>;
    async fn stage_file(&self, path: &Path) -> Result<(), GitError>;
    async fn unstage_file(&self, path: &Path) -> Result<(), GitError>;
    async fn stage_all(&self) -> Result<(), GitError>;
    async fn unstage_all(&self) -> Result<(), GitError>;
    async fn log_commits(
        &self,
        base: &str,
        head: &str,
        max: usize,
    ) -> Result<Vec<CommitLogEntry>, GitError>;
    async fn log_all(&self, head: &str, max: usize) -> Result<Vec<CommitLogEntry>, GitError>;
    async fn log_all_branches(
        &self,
        offset: usize,
        max: usize,
    ) -> Result<Vec<CommitLogEntry>, GitError>;
    async fn merge_base(&self, ref1: &str, ref2: &str) -> Result<String, GitError>;
    async fn upstream_ref(&self) -> Result<Option<String>, GitError>;
    async fn rebase_interactive(&self, base_ref: &str, todo_content: &str) -> Result<(), GitError>;
    async fn rebase_continue(&self) -> Result<(), GitError>;
    async fn rebase_abort(&self) -> Result<(), GitError>;
    async fn rebase_skip(&self) -> Result<(), GitError>;
    async fn has_unmerged_paths(&self) -> bool;
    async fn conflict_files(&self) -> Vec<PathBuf>;
}

// -- Real implementations --

pub struct RealGitProvider;

#[async_trait]
impl GitProvider for RealGitProvider {
    fn as_any(&self) -> &dyn Any {
        self
    }

    async fn discover(&self, path: &Path) -> Result<Box<dyn GitRepo>, GitError> {
        let path = path.to_path_buf();
        smol::unblock(move || {
            let repo = Repository::discover(&path)?;
            let workdir_path = repo.workdir().to_path_buf();
            Ok(Box::new(RealGitRepo { workdir_path }) as Box<dyn GitRepo>)
        })
        .await
    }

    async fn open(&self, path: &Path) -> Result<Box<dyn GitRepo>, GitError> {
        let path = path.to_path_buf();
        smol::unblock(move || {
            let repo = Repository::open(&path)?;
            let workdir_path = repo.workdir().to_path_buf();
            Ok(Box::new(RealGitRepo { workdir_path }) as Box<dyn GitRepo>)
        })
        .await
    }
}

pub struct RealGitRepo {
    workdir_path: PathBuf,
}

#[async_trait]
impl GitRepo for RealGitRepo {
    fn workdir(&self) -> &Path {
        &self.workdir_path
    }

    async fn head_content(&self, path: &Path) -> Result<String, GitError> {
        let workdir = self.workdir_path.clone();
        let path = path.to_path_buf();
        smol::unblock(move || {
            let repo = Repository::open(&workdir)?;
            repo.head_content(&path)
        })
        .await
    }

    async fn index_content(&self, path: &Path) -> Result<String, GitError> {
        let workdir = self.workdir_path.clone();
        let path = path.to_path_buf();
        smol::unblock(move || {
            let repo = Repository::open(&workdir)?;
            repo.index_content(&path)
        })
        .await
    }

    async fn parent_content(&self, path: &Path) -> Result<String, GitError> {
        let workdir = self.workdir_path.clone();
        let path = path.to_path_buf();
        smol::unblock(move || {
            let repo = Repository::open(&workdir)?;
            repo.parent_content(&path)
        })
        .await
    }

    async fn gather_status(&self) -> Result<Vec<GitStatusEntry>, GitStatusError> {
        let workdir = self.workdir_path.clone();
        smol::unblock(move || {
            let repo =
                Repository::open(&workdir).map_err(|e| GitStatusError::GitError(e.to_string()))?;
            crate::git::status::gather_git_status(repo.inner())
        })
        .await
    }

    async fn gather_branch_info(&self) -> Option<GitBranchInfo> {
        let workdir = self.workdir_path.clone();
        smol::unblock(move || {
            let repo = Repository::open(&workdir).ok()?;
            crate::git::status::gather_git_branch_info(repo.inner())
        })
        .await
    }

    async fn blame_file(&self, path: &Path) -> Result<BlameData, GitError> {
        let workdir = self.workdir_path.clone();
        let path = path.to_path_buf();
        smol::unblock(move || {
            let repo = Repository::open(&workdir)?;
            crate::git::blame::blame_file(&repo, &path)
        })
        .await
    }

    async fn count_hunks_by_file(
        &self,
        mode: DiffComparisonMode,
    ) -> Result<HashMap<PathBuf, usize>, GitError> {
        let workdir = self.workdir_path.clone();
        smol::unblock(move || {
            let repo = Repository::open(&workdir)?;
            repo.count_hunks_by_file(mode)
        })
        .await
    }

    async fn commit_changed_files(&self) -> Result<Vec<PathBuf>, GitError> {
        let workdir = self.workdir_path.clone();
        smol::unblock(move || {
            let repo = Repository::open(&workdir)?;
            repo.commit_changed_files()
        })
        .await
    }

    async fn commit_files_by_oid(&self, oid: &str) -> Result<Vec<CommitFileChange>, GitError> {
        let workdir = self.workdir_path.clone();
        let oid = oid.to_string();
        smol::unblock(move || {
            let repo = Repository::open(&workdir)?;
            repo.commit_files_by_oid(&oid)
        })
        .await
    }

    async fn commit_file_diff(&self, oid: &str, path: &Path) -> Result<String, GitError> {
        let workdir = self.workdir_path.clone();
        let oid = oid.to_string();
        let path = path.to_path_buf();
        smol::unblock(move || {
            let repo = Repository::open(&workdir)?;
            repo.commit_file_diff(&oid, &path)
        })
        .await
    }

    async fn apply_diff(
        &self,
        patch: &str,
        reverse: bool,
        location: ApplyLocation,
    ) -> Result<(), GitError> {
        let workdir = self.workdir_path.clone();
        let patch = patch.to_string();
        smol::unblock(move || {
            let patch_str = if reverse {
                reverse_patch_text(&patch)
            } else {
                patch
            };
            let repo = Repository::open(&workdir)?;
            let diff = git2::Diff::from_buffer(patch_str.as_bytes())
                .map_err(|e| GitError::GitOperationFailed(format!("Failed to parse patch: {e}")))?;
            let git2_location = match location {
                ApplyLocation::Index => git2::ApplyLocation::Index,
                ApplyLocation::WorkDir => git2::ApplyLocation::WorkDir,
            };
            repo.inner()
                .apply(&diff, git2_location, None)
                .map_err(|e| GitError::GitOperationFailed(format!("Failed to apply diff: {e}")))?;
            Ok(())
        })
        .await
    }

    async fn stage_file(&self, path: &Path) -> Result<(), GitError> {
        let workdir = self.workdir_path.clone();
        let path = path.to_path_buf();
        smol::unblock(move || run_git(&workdir, &["add", "--", &path.to_string_lossy()])).await
    }

    async fn unstage_file(&self, path: &Path) -> Result<(), GitError> {
        let workdir = self.workdir_path.clone();
        let path = path.to_path_buf();
        smol::unblock(move || run_git(&workdir, &["reset", "HEAD", "--", &path.to_string_lossy()]))
            .await
    }

    async fn stage_all(&self) -> Result<(), GitError> {
        let workdir = self.workdir_path.clone();
        smol::unblock(move || run_git(&workdir, &["add", "-A"])).await
    }

    async fn unstage_all(&self) -> Result<(), GitError> {
        let workdir = self.workdir_path.clone();
        smol::unblock(move || run_git(&workdir, &["reset", "HEAD"])).await
    }

    async fn log_commits(
        &self,
        base: &str,
        head: &str,
        max: usize,
    ) -> Result<Vec<CommitLogEntry>, GitError> {
        let workdir = self.workdir_path.clone();
        let base = base.to_string();
        let head = head.to_string();
        smol::unblock(move || {
            let repo = Repository::open(&workdir)?;
            repo.log_commits(&base, &head, max)
        })
        .await
    }

    async fn log_all(&self, head: &str, max: usize) -> Result<Vec<CommitLogEntry>, GitError> {
        let workdir = self.workdir_path.clone();
        let head = head.to_string();
        smol::unblock(move || {
            let repo = Repository::open(&workdir)?;
            repo.log_all(&head, max)
        })
        .await
    }

    async fn log_all_branches(
        &self,
        offset: usize,
        max: usize,
    ) -> Result<Vec<CommitLogEntry>, GitError> {
        let workdir = self.workdir_path.clone();
        smol::unblock(move || {
            let repo = Repository::open(&workdir)?;
            repo.log_all_branches(offset, max)
        })
        .await
    }

    async fn merge_base(&self, ref1: &str, ref2: &str) -> Result<String, GitError> {
        let workdir = self.workdir_path.clone();
        let ref1 = ref1.to_string();
        let ref2 = ref2.to_string();
        smol::unblock(move || {
            let repo = Repository::open(&workdir)?;
            repo.merge_base(&ref1, &ref2)
        })
        .await
    }

    async fn upstream_ref(&self) -> Result<Option<String>, GitError> {
        let workdir = self.workdir_path.clone();
        smol::unblock(move || {
            let repo = Repository::open(&workdir)?;
            repo.upstream_ref()
        })
        .await
    }

    async fn rebase_interactive(&self, base_ref: &str, todo_content: &str) -> Result<(), GitError> {
        let workdir = self.workdir_path.clone();
        let base_ref = base_ref.to_string();
        let todo_content = todo_content.to_string();
        smol::unblock(move || {
            let mut tmp = tempfile::NamedTempFile::new().map_err(|e| {
                GitError::GitOperationFailed(format!("Failed to create temp file: {e}"))
            })?;
            tmp.write_all(todo_content.as_bytes())
                .map_err(|e| GitError::GitOperationFailed(format!("Failed to write todo: {e}")))?;
            tmp.flush()
                .map_err(|e| GitError::GitOperationFailed(format!("Failed to flush todo: {e}")))?;

            let todo_path_str = tmp.path().to_string_lossy().replace('\'', "'\\''");
            let seq_editor = format!("cp '{}' ", todo_path_str);

            let output = std::process::Command::new("git")
                .args(["rebase", "-i", &base_ref])
                .env("GIT_SEQUENCE_EDITOR", &seq_editor)
                .current_dir(&workdir)
                .output()
                .map_err(|e| {
                    GitError::GitOperationFailed(format!("Failed to run git rebase: {e}"))
                })?;

            if !output.status.success() {
                let rebase_dir = workdir.join(".git/rebase-merge");
                if !rebase_dir.exists() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(GitError::GitOperationFailed(format!(
                        "git rebase -i failed: {stderr}"
                    )));
                }
            }
            Ok(())
        })
        .await
    }

    async fn rebase_continue(&self) -> Result<(), GitError> {
        let workdir = self.workdir_path.clone();
        smol::unblock(move || {
            let output = std::process::Command::new("git")
                .args(["rebase", "--continue"])
                .env("GIT_EDITOR", "true")
                .current_dir(&workdir)
                .output()
                .map_err(|e| {
                    GitError::GitOperationFailed(format!("Failed to run git rebase: {e}"))
                })?;

            if !output.status.success() {
                let rebase_dir = workdir.join(".git/rebase-merge");
                if !rebase_dir.exists() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(GitError::GitOperationFailed(format!(
                        "git rebase --continue failed: {stderr}"
                    )));
                }
            }
            Ok(())
        })
        .await
    }

    async fn rebase_abort(&self) -> Result<(), GitError> {
        let workdir = self.workdir_path.clone();
        smol::unblock(move || run_git(&workdir, &["rebase", "--abort"])).await
    }

    async fn rebase_skip(&self) -> Result<(), GitError> {
        let workdir = self.workdir_path.clone();
        smol::unblock(move || {
            let output = std::process::Command::new("git")
                .args(["rebase", "--skip"])
                .current_dir(&workdir)
                .output()
                .map_err(|e| {
                    GitError::GitOperationFailed(format!("Failed to run git rebase: {e}"))
                })?;

            if !output.status.success() {
                let rebase_dir = workdir.join(".git/rebase-merge");
                if !rebase_dir.exists() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(GitError::GitOperationFailed(format!(
                        "git rebase --skip failed: {stderr}"
                    )));
                }
            }
            Ok(())
        })
        .await
    }

    async fn has_unmerged_paths(&self) -> bool {
        let workdir = self.workdir_path.clone();
        smol::unblock(move || {
            let Ok(repo) = Repository::open(&workdir) else {
                return false;
            };
            repo.inner()
                .index()
                .map(|idx| idx.has_conflicts())
                .unwrap_or(false)
        })
        .await
    }

    async fn conflict_files(&self) -> Vec<PathBuf> {
        let workdir = self.workdir_path.clone();
        smol::unblock(move || {
            let Ok(repo) = Repository::open(&workdir) else {
                return Vec::new();
            };
            let Ok(index) = repo.inner().index() else {
                return Vec::new();
            };
            let Ok(conflicts) = index.conflicts() else {
                return Vec::new();
            };
            let mut paths = Vec::new();
            for entry in conflicts.flatten() {
                let path = entry
                    .our
                    .as_ref()
                    .or(entry.their.as_ref())
                    .or(entry.ancestor.as_ref());
                if let Some(ie) = path {
                    let p = String::from_utf8_lossy(&ie.path);
                    paths.push(PathBuf::from(p.as_ref()));
                }
            }
            paths
        })
        .await
    }
}

fn run_git_output(workdir: &Path, args: &[&str]) -> Result<std::process::Output, GitError> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(workdir)
        .output()
        .map_err(|e| GitError::GitOperationFailed(format!("Failed to run git: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GitError::GitOperationFailed(format!(
            "git {} failed: {stderr}",
            args.join(" ")
        )));
    }
    Ok(output)
}

fn run_git(workdir: &Path, args: &[&str]) -> Result<(), GitError> {
    run_git_output(workdir, args)?;
    Ok(())
}

fn reverse_patch_text(patch: &str) -> String {
    let mut result = String::with_capacity(patch.len());
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix('+') {
            result.push('-');
            result.push_str(rest);
        } else if let Some(rest) = line.strip_prefix('-') {
            result.push('+');
            result.push_str(rest);
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }
    result
}

// -- Fake implementations --

#[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
use crate::fs::FakeFs;
#[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
use parking_lot::Mutex;
#[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
use std::sync::Arc;

#[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
pub struct FakeGitProvider {
    state: Arc<Mutex<FakeGitState>>,
    fs: Arc<FakeFs>,
}

#[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
pub struct FakeCommit {
    pub oid: String,
    pub changed_files: Vec<CommitFileChange>,
    pub diffs: HashMap<PathBuf, String>,
}

#[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
struct FakeGitState {
    workdir: PathBuf,
    head_files: HashMap<PathBuf, String>,
    index_files: HashMap<PathBuf, String>,
    parent_files: HashMap<PathBuf, String>,
    status_entries: Vec<GitStatusEntry>,
    branch_info: Option<GitBranchInfo>,
    hunk_counts: HashMap<PathBuf, usize>,
    staged_files: std::collections::HashSet<PathBuf>,
    applied_diffs: Vec<(String, ApplyLocation, bool)>,
    blame_data: HashMap<PathBuf, BlameData>,
    commit_history: Vec<FakeCommit>,
    exists: bool,
    has_conflicts: bool,
    conflict_file_list: Vec<PathBuf>,
}

#[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
impl FakeGitProvider {
    pub fn new(fs: Arc<FakeFs>) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeGitState {
                workdir: PathBuf::new(),
                head_files: HashMap::new(),
                index_files: HashMap::new(),
                parent_files: HashMap::new(),
                status_entries: Vec::new(),
                branch_info: None,
                hunk_counts: HashMap::new(),
                staged_files: std::collections::HashSet::new(),
                applied_diffs: Vec::new(),
                blame_data: HashMap::new(),
                commit_history: Vec::new(),
                exists: false,
                has_conflicts: false,
                conflict_file_list: Vec::new(),
            })),
            fs,
        }
    }

    pub fn with_repo(workdir: PathBuf, fs: Arc<FakeFs>) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeGitState {
                workdir,
                head_files: HashMap::new(),
                index_files: HashMap::new(),
                parent_files: HashMap::new(),
                status_entries: Vec::new(),
                branch_info: None,
                hunk_counts: HashMap::new(),
                staged_files: std::collections::HashSet::new(),
                applied_diffs: Vec::new(),
                blame_data: HashMap::new(),
                commit_history: Vec::new(),
                exists: true,
                has_conflicts: false,
                conflict_file_list: Vec::new(),
            })),
            fs,
        }
    }

    pub fn set_exists(&self, exists: bool) {
        self.state.lock().exists = exists;
    }

    pub fn set_workdir(&self, workdir: PathBuf) {
        self.state.lock().workdir = workdir;
    }

    pub fn set_head_content(&self, path: impl Into<PathBuf>, content: impl Into<String>) {
        self.state
            .lock()
            .head_files
            .insert(path.into(), content.into());
    }

    pub fn set_index_content(&self, path: impl Into<PathBuf>, content: impl Into<String>) {
        self.state
            .lock()
            .index_files
            .insert(path.into(), content.into());
    }

    pub fn set_status(&self, entries: Vec<GitStatusEntry>) {
        self.state.lock().status_entries = entries;
    }

    pub fn set_branch_info(&self, info: Option<GitBranchInfo>) {
        self.state.lock().branch_info = info;
    }

    pub fn set_parent_content(&self, path: impl Into<PathBuf>, content: impl Into<String>) {
        self.state
            .lock()
            .parent_files
            .insert(path.into(), content.into());
    }

    pub fn set_hunk_counts(&self, counts: HashMap<PathBuf, usize>) {
        self.state.lock().hunk_counts = counts;
    }

    /// Set head + index to same content (simulates committed state).
    pub fn commit_file(&self, path: impl Into<PathBuf>, content: impl Into<String>) {
        let path = path.into();
        let content = content.into();
        let mut state = self.state.lock();
        state.head_files.insert(path.clone(), content.clone());
        state.index_files.insert(path, content);
    }

    /// Read-back accessor for index content (test assertions).
    pub fn index_content(&self, path: &Path) -> Option<String> {
        self.state.lock().index_files.get(path).cloned()
    }

    /// Read-back accessor for head content (test assertions).
    pub fn head_content(&self, path: &Path) -> Option<String> {
        self.state.lock().head_files.get(path).cloned()
    }

    pub fn set_blame_data(&self, path: impl Into<PathBuf>, data: BlameData) {
        self.state.lock().blame_data.insert(path.into(), data);
    }

    pub fn add_commit(
        &self,
        oid: &str,
        files: Vec<CommitFileChange>,
        diffs: HashMap<PathBuf, String>,
    ) {
        self.state.lock().commit_history.push(FakeCommit {
            oid: oid.to_string(),
            changed_files: files,
            diffs,
        });
    }

    pub fn staged_files(&self) -> std::collections::HashSet<PathBuf> {
        self.state.lock().staged_files.clone()
    }

    pub fn applied_diffs(&self) -> Vec<(String, ApplyLocation, bool)> {
        self.state.lock().applied_diffs.clone()
    }

    pub fn set_has_conflicts(&self, v: bool) {
        self.state.lock().has_conflicts = v;
    }

    pub fn set_conflict_files(&self, files: Vec<PathBuf>) {
        self.state.lock().conflict_file_list = files;
    }
}

#[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
#[async_trait]
impl GitProvider for FakeGitProvider {
    fn as_any(&self) -> &dyn Any {
        self
    }

    async fn discover(&self, _path: &Path) -> Result<Box<dyn GitRepo>, GitError> {
        let state = self.state.lock();
        if !state.exists {
            return Err(GitError::RepositoryNotFound(PathBuf::new()));
        }
        Ok(Box::new(FakeGitRepo {
            workdir_path: state.workdir.clone(),
            state: self.state.clone(),
            fs: self.fs.clone(),
        }))
    }

    async fn open(&self, _path: &Path) -> Result<Box<dyn GitRepo>, GitError> {
        self.discover(_path).await
    }
}

#[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
struct FakeGitRepo {
    workdir_path: PathBuf,
    state: Arc<Mutex<FakeGitState>>,
    fs: Arc<FakeFs>,
}

#[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
#[async_trait]
impl GitRepo for FakeGitRepo {
    fn workdir(&self) -> &Path {
        &self.workdir_path
    }

    async fn head_content(&self, path: &Path) -> Result<String, GitError> {
        let state = self.state.lock();
        state
            .head_files
            .get(path)
            .cloned()
            .ok_or(GitError::FileNotFound(path.to_path_buf()))
    }

    async fn index_content(&self, path: &Path) -> Result<String, GitError> {
        let state = self.state.lock();
        state
            .index_files
            .get(path)
            .cloned()
            .ok_or(GitError::FileNotFound(path.to_path_buf()))
    }

    async fn parent_content(&self, path: &Path) -> Result<String, GitError> {
        let state = self.state.lock();
        state
            .parent_files
            .get(path)
            .cloned()
            .ok_or(GitError::FileNotFound(path.to_path_buf()))
    }

    async fn gather_status(&self) -> Result<Vec<GitStatusEntry>, GitStatusError> {
        Ok(self.state.lock().status_entries.clone())
    }

    async fn gather_branch_info(&self) -> Option<GitBranchInfo> {
        self.state.lock().branch_info.clone()
    }

    async fn blame_file(&self, path: &Path) -> Result<BlameData, GitError> {
        let state = self.state.lock();
        state
            .blame_data
            .get(path)
            .cloned()
            .ok_or(GitError::FileNotFound(path.to_path_buf()))
    }

    async fn count_hunks_by_file(
        &self,
        mode: DiffComparisonMode,
    ) -> Result<HashMap<PathBuf, usize>, GitError> {
        let state = self.state.lock();
        if !state.hunk_counts.is_empty() {
            return Ok(state.hunk_counts.clone());
        }
        compute_hunk_counts(&state, &self.fs, &self.workdir_path, mode)
    }

    async fn commit_changed_files(&self) -> Result<Vec<PathBuf>, GitError> {
        let state = self.state.lock();
        Ok(state
            .commit_history
            .last()
            .map(|c| c.changed_files.iter().map(|f| f.path.clone()).collect())
            .unwrap_or_default())
    }

    async fn commit_files_by_oid(&self, oid: &str) -> Result<Vec<CommitFileChange>, GitError> {
        let state = self.state.lock();
        Ok(state
            .commit_history
            .iter()
            .find(|c| c.oid == oid)
            .map(|c| c.changed_files.clone())
            .unwrap_or_default())
    }

    async fn commit_file_diff(&self, oid: &str, path: &Path) -> Result<String, GitError> {
        let state = self.state.lock();
        state
            .commit_history
            .iter()
            .find(|c| c.oid == oid)
            .and_then(|c| c.diffs.get(path).cloned())
            .ok_or(GitError::FileNotFound(path.to_path_buf()))
    }

    async fn apply_diff(
        &self,
        patch: &str,
        reverse: bool,
        location: ApplyLocation,
    ) -> Result<(), GitError> {
        let mut state = self.state.lock();
        state
            .applied_diffs
            .push((patch.to_string(), location, reverse));

        let effective = if reverse {
            reverse_patch_text(patch)
        } else {
            patch.to_string()
        };

        if let Some(file_path) = parse_filename_from_patch(&effective) {
            let abs_path = state.workdir.join(&file_path);
            let base = match location {
                ApplyLocation::Index => state
                    .index_files
                    .get(&abs_path)
                    .cloned()
                    .unwrap_or_default(),
                ApplyLocation::WorkDir => {
                    self.fs.read_to_string_fake(&abs_path).unwrap_or_default()
                },
            };
            let result = apply_unified_patch(&base, &effective)?;
            match location {
                ApplyLocation::Index => {
                    state.index_files.insert(abs_path, result);
                },
                ApplyLocation::WorkDir => {
                    drop(state);
                    self.fs.insert_file(&abs_path, result);
                },
            }
        }
        Ok(())
    }

    async fn stage_file(&self, path: &Path) -> Result<(), GitError> {
        self.state.lock().staged_files.insert(path.to_path_buf());
        Ok(())
    }

    async fn unstage_file(&self, path: &Path) -> Result<(), GitError> {
        self.state.lock().staged_files.remove(path);
        Ok(())
    }

    async fn stage_all(&self) -> Result<(), GitError> {
        let mut state = self.state.lock();
        let all_paths: Vec<PathBuf> = state
            .status_entries
            .iter()
            .map(|e| e.path.clone())
            .collect();
        state.staged_files.extend(all_paths);
        Ok(())
    }

    async fn unstage_all(&self) -> Result<(), GitError> {
        self.state.lock().staged_files.clear();
        Ok(())
    }

    async fn log_commits(
        &self,
        _base: &str,
        _head: &str,
        max: usize,
    ) -> Result<Vec<CommitLogEntry>, GitError> {
        let state = self.state.lock();
        Ok(state
            .commit_history
            .iter()
            .take(max)
            .map(|c| CommitLogEntry {
                oid: c.oid.clone(),
                short_hash: c.oid[..7.min(c.oid.len())].to_string(),
                author: "Test Author".to_string(),
                timestamp: 0,
                message: c
                    .changed_files
                    .first()
                    .map(|f| f.path.to_string_lossy().to_string())
                    .unwrap_or_default(),
                parent_oids: vec![],
            })
            .collect())
    }

    async fn log_all(&self, _head: &str, max: usize) -> Result<Vec<CommitLogEntry>, GitError> {
        self.log_commits("", "", max).await
    }

    async fn log_all_branches(
        &self,
        offset: usize,
        max: usize,
    ) -> Result<Vec<CommitLogEntry>, GitError> {
        let all = self.log_commits("", "", usize::MAX).await?;
        Ok(all.into_iter().skip(offset).take(max).collect())
    }

    async fn merge_base(&self, _ref1: &str, _ref2: &str) -> Result<String, GitError> {
        let state = self.state.lock();
        state
            .commit_history
            .first()
            .map(|c| c.oid.clone())
            .ok_or_else(|| GitError::GitOperationFailed("no merge base found".into()))
    }

    async fn upstream_ref(&self) -> Result<Option<String>, GitError> {
        let state = self.state.lock();
        Ok(state
            .branch_info
            .as_ref()
            .map(|_| "refs/remotes/origin/main".to_string()))
    }

    async fn rebase_interactive(
        &self,
        _base_ref: &str,
        _todo_content: &str,
    ) -> Result<(), GitError> {
        Ok(())
    }

    async fn rebase_continue(&self) -> Result<(), GitError> {
        Ok(())
    }

    async fn rebase_abort(&self) -> Result<(), GitError> {
        Ok(())
    }

    async fn rebase_skip(&self) -> Result<(), GitError> {
        Ok(())
    }

    async fn has_unmerged_paths(&self) -> bool {
        self.state.lock().has_conflicts
    }

    async fn conflict_files(&self) -> Vec<PathBuf> {
        self.state.lock().conflict_file_list.clone()
    }
}

#[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
fn parse_filename_from_patch(patch: &str) -> Option<PathBuf> {
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            return Some(PathBuf::from(rest));
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            if rest != "/dev/null" {
                return Some(PathBuf::from(rest));
            }
        }
    }
    None
}

#[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
fn apply_unified_patch(base: &str, patch: &str) -> Result<String, GitError> {
    let base_lines: Vec<&str> = if base.is_empty() {
        Vec::new()
    } else {
        base.lines().collect()
    };
    let mut result_lines: Vec<String> = Vec::new();
    let mut base_idx: usize = 0;

    let patch_lines: Vec<&str> = patch.lines().collect();
    let mut pi = 0;

    while pi < patch_lines.len() {
        let line = patch_lines[pi];
        if line.starts_with("@@") {
            break;
        }
        pi += 1;
    }

    while pi < patch_lines.len() {
        let line = patch_lines[pi];

        if let Some(hunk_header) = line.strip_prefix("@@") {
            let (old_start, old_count) = parse_hunk_header(hunk_header)?;
            // For pure additions (old_count=0), old_start is the anchor line;
            // insert after it. For modifications, old_start is the first affected line.
            let target = if old_count == 0 {
                old_start as usize
            } else {
                (old_start as usize).saturating_sub(1)
            };
            while base_idx < target && base_idx < base_lines.len() {
                result_lines.push(base_lines[base_idx].to_string());
                base_idx += 1;
            }
            pi += 1;
            continue;
        }

        if let Some(_content) = line.strip_prefix('-') {
            base_idx += 1;
        } else if let Some(content) = line.strip_prefix('+') {
            result_lines.push(content.to_string());
        } else if let Some(content) = line.strip_prefix(' ') {
            result_lines.push(content.to_string());
            base_idx += 1;
        } else if line.starts_with('\\') {
            // "\ No newline at end of file" -- skip
        } else {
            result_lines.push(line.to_string());
            base_idx += 1;
        }
        pi += 1;
    }

    while base_idx < base_lines.len() {
        result_lines.push(base_lines[base_idx].to_string());
        base_idx += 1;
    }

    let mut result = result_lines.join("\n");
    if base.ends_with('\n') || (!base.is_empty() && !result.is_empty()) {
        result.push('\n');
    }
    Ok(result)
}

#[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
fn parse_hunk_header(header: &str) -> Result<(u32, u32), GitError> {
    let err = || GitError::GitOperationFailed(format!("Failed to parse hunk header: @@{header}"));
    let trimmed = header.trim();
    let minus_part = trimmed
        .strip_prefix('-')
        .and_then(|s| s.split_whitespace().next())
        .ok_or_else(err)?;
    let parts: Vec<&str> = minus_part.split(',').collect();
    let start: u32 = parts[0].parse().map_err(|_| err())?;
    let count: u32 = if parts.len() > 1 {
        parts[1].parse().map_err(|_| err())?
    } else {
        1
    };
    Ok((start, count))
}

#[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
fn compute_hunk_counts(
    state: &FakeGitState,
    fs: &FakeFs,
    workdir: &Path,
    mode: DiffComparisonMode,
) -> Result<HashMap<PathBuf, usize>, GitError> {
    use crate::git::diff_review::DiffComparisonMode;

    let mut counts = HashMap::new();

    let mut all_paths = std::collections::HashSet::new();
    for p in state.head_files.keys() {
        all_paths.insert(p.clone());
    }
    for p in state.index_files.keys() {
        all_paths.insert(p.clone());
    }
    for p in fs.files() {
        if p.starts_with(workdir) {
            all_paths.insert(p);
        }
    }

    for abs_path in &all_paths {
        let rel_path = abs_path.strip_prefix(workdir).unwrap_or(abs_path);

        let (old_content, new_content) = match mode {
            DiffComparisonMode::WorkingVsHead => {
                let old = state.head_files.get(abs_path).cloned().unwrap_or_default();
                let new = fs.read_to_string_fake(abs_path).unwrap_or_default();
                (old, new)
            },
            DiffComparisonMode::WorkingVsIndex => {
                let old = state.index_files.get(abs_path).cloned().unwrap_or_default();
                let new = fs.read_to_string_fake(abs_path).unwrap_or_default();
                (old, new)
            },
            DiffComparisonMode::IndexVsHead => {
                let old = state.head_files.get(abs_path).cloned().unwrap_or_default();
                let new = state.index_files.get(abs_path).cloned().unwrap_or_default();
                (old, new)
            },
            DiffComparisonMode::HeadVsParent => {
                let old = state
                    .parent_files
                    .get(abs_path)
                    .cloned()
                    .unwrap_or_default();
                let new = state.head_files.get(abs_path).cloned().unwrap_or_default();
                (old, new)
            },
        };

        if old_content == new_content {
            continue;
        }

        let mut diff_opts = git2::DiffOptions::new();
        diff_opts.context_lines(0);
        let patch = git2::Patch::from_buffers(
            old_content.as_bytes(),
            None,
            new_content.as_bytes(),
            None,
            Some(&mut diff_opts),
        )
        .map_err(|e| GitError::GitOperationFailed(format!("Patch::from_buffers failed: {e}")))?;

        let num_hunks = patch.num_hunks();
        if num_hunks > 0 {
            counts.insert(rel_path.to_path_buf(), num_hunks);
        }
    }

    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{fs::FakeFs, git::status::GitBranchInfo};
    use std::sync::Arc;

    #[test]
    fn fake_log_commits_returns_history() {
        smol::block_on(async {
            let fs = Arc::new(FakeFs::new());
            let provider = FakeGitProvider::new(fs);
            let workdir = PathBuf::from("/fake/repo");
            provider.set_exists(true);
            provider.set_workdir(workdir.clone());

            provider.add_commit(
                "abc1234567890",
                vec![CommitFileChange {
                    path: PathBuf::from("src/main.rs"),
                    status: "M".to_string(),
                }],
                HashMap::new(),
            );
            provider.add_commit(
                "def5678901234",
                vec![CommitFileChange {
                    path: PathBuf::from("src/lib.rs"),
                    status: "A".to_string(),
                }],
                HashMap::new(),
            );

            let repo = provider.open(&workdir).await.unwrap();
            let commits = repo.log_commits("base", "HEAD", 10).await.unwrap();
            assert_eq!(commits.len(), 2);
            assert_eq!(commits[0].oid, "abc1234567890");
            assert_eq!(commits[0].short_hash, "abc1234");
            assert_eq!(commits[1].oid, "def5678901234");
        });
    }

    #[test]
    fn fake_merge_base_returns_first() {
        smol::block_on(async {
            let fs = Arc::new(FakeFs::new());
            let provider = FakeGitProvider::new(fs);
            let workdir = PathBuf::from("/fake/repo");
            provider.set_exists(true);
            provider.set_workdir(workdir.clone());
            provider.add_commit("first_commit", vec![], HashMap::new());
            provider.add_commit("second_commit", vec![], HashMap::new());

            let repo = provider.open(&workdir).await.unwrap();
            assert_eq!(repo.merge_base("a", "b").await.unwrap(), "first_commit");
        });
    }

    #[test]
    fn fake_upstream_ref_with_branch_info() {
        smol::block_on(async {
            let fs = Arc::new(FakeFs::new());
            let provider = FakeGitProvider::new(fs);
            let workdir = PathBuf::from("/fake/repo");
            provider.set_exists(true);
            provider.set_workdir(workdir.clone());
            provider.set_branch_info(Some(GitBranchInfo {
                branch_name: "main".to_string(),
                ahead: 0,
                behind: 0,
            }));

            let repo = provider.open(&workdir).await.unwrap();
            assert_eq!(
                repo.upstream_ref().await.unwrap(),
                Some("refs/remotes/origin/main".to_string())
            );
        });
    }

    #[test]
    fn fake_upstream_ref_without_branch_info() {
        smol::block_on(async {
            let fs = Arc::new(FakeFs::new());
            let provider = FakeGitProvider::new(fs);
            let workdir = PathBuf::from("/fake/repo");
            provider.set_exists(true);
            provider.set_workdir(workdir.clone());

            let repo = provider.open(&workdir).await.unwrap();
            assert_eq!(repo.upstream_ref().await.unwrap(), None);
        });
    }

    #[test]
    fn fake_rebase_methods_are_noops() {
        smol::block_on(async {
            let fs = Arc::new(FakeFs::new());
            let provider = FakeGitProvider::new(fs);
            let workdir = PathBuf::from("/fake/repo");
            provider.set_exists(true);
            provider.set_workdir(workdir.clone());

            let repo = provider.open(&workdir).await.unwrap();
            assert!(repo
                .rebase_interactive("base", "pick abc123 msg\n")
                .await
                .is_ok());
            assert!(repo.rebase_continue().await.is_ok());
            assert!(repo.rebase_abort().await.is_ok());
            assert!(repo.rebase_skip().await.is_ok());
            assert!(!repo.has_unmerged_paths().await);
        });
    }

    #[test]
    fn fake_has_unmerged_paths_configurable() {
        smol::block_on(async {
            let fs = Arc::new(FakeFs::new());
            let provider = FakeGitProvider::new(fs);
            let workdir = PathBuf::from("/fake/repo");
            provider.set_exists(true);
            provider.set_workdir(workdir.clone());

            let repo = provider.open(&workdir).await.unwrap();
            assert!(!repo.has_unmerged_paths().await);

            provider.set_has_conflicts(true);
            let repo = provider.open(&workdir).await.unwrap();
            assert!(repo.has_unmerged_paths().await);
        });
    }

    #[test]
    fn fake_conflict_files() {
        smol::block_on(async {
            let fs = Arc::new(FakeFs::new());
            let provider = FakeGitProvider::new(fs);
            let workdir = PathBuf::from("/fake/repo");
            provider.set_exists(true);
            provider.set_workdir(workdir.clone());

            provider.set_conflict_files(vec![
                PathBuf::from("src/main.rs"),
                PathBuf::from("src/lib.rs"),
            ]);

            let repo = provider.open(&workdir).await.unwrap();
            assert_eq!(
                repo.conflict_files().await,
                vec![PathBuf::from("src/main.rs"), PathBuf::from("src/lib.rs")]
            );
        });
    }
}

use crate::git::{
    blame::BlameData,
    diff_review::DiffComparisonMode,
    repository::{CommitFileChange, CommitLogEntry, GitError, Repository},
    status::{GitBranchInfo, GitStatusEntry, GitStatusError},
};
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

pub trait GitProvider: Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn discover(&self, path: &Path) -> Result<Box<dyn GitRepo>, GitError>;
    fn open(&self, path: &Path) -> Result<Box<dyn GitRepo>, GitError>;
}

pub trait GitRepo: Send {
    fn workdir(&self) -> &Path;
    fn head_content(&self, path: &Path) -> Result<String, GitError>;
    fn index_content(&self, path: &Path) -> Result<String, GitError>;
    fn parent_content(&self, path: &Path) -> Result<String, GitError>;
    fn gather_status(&self) -> Result<Vec<GitStatusEntry>, GitStatusError>;
    fn gather_branch_info(&self) -> Option<GitBranchInfo>;
    fn blame_file(&self, path: &Path) -> Result<BlameData, GitError>;
    fn count_hunks_by_file(
        &self,
        mode: DiffComparisonMode,
    ) -> Result<HashMap<PathBuf, usize>, GitError>;
    fn commit_changed_files(&self) -> Result<Vec<PathBuf>, GitError>;
    fn commit_files_by_oid(&self, oid: &str) -> Result<Vec<CommitFileChange>, GitError>;
    fn commit_file_diff(&self, oid: &str, path: &Path) -> Result<String, GitError>;
    fn apply_diff(
        &self,
        patch: &str,
        reverse: bool,
        location: ApplyLocation,
    ) -> Result<(), GitError>;
    fn stage_file(&self, path: &Path) -> Result<(), GitError>;
    fn unstage_file(&self, path: &Path) -> Result<(), GitError>;
    fn stage_all(&self) -> Result<(), GitError>;
    fn unstage_all(&self) -> Result<(), GitError>;
    fn log_commits(
        &self,
        base: &str,
        head: &str,
        max: usize,
    ) -> Result<Vec<CommitLogEntry>, GitError>;
    fn merge_base(&self, ref1: &str, ref2: &str) -> Result<String, GitError>;
    fn upstream_ref(&self) -> Result<Option<String>, GitError>;
    fn rebase_interactive(&self, base_ref: &str, todo_content: &str) -> Result<(), GitError>;
    fn rebase_continue(&self) -> Result<(), GitError>;
    fn rebase_abort(&self) -> Result<(), GitError>;
    fn rebase_skip(&self) -> Result<(), GitError>;
    fn has_unmerged_paths(&self) -> bool;
}

// -- Real implementations --

pub struct RealGitProvider;

impl GitProvider for RealGitProvider {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn discover(&self, path: &Path) -> Result<Box<dyn GitRepo>, GitError> {
        let repo = Repository::discover(path)?;
        Ok(Box::new(RealGitRepo { repo }))
    }

    fn open(&self, path: &Path) -> Result<Box<dyn GitRepo>, GitError> {
        let repo = Repository::open(path)?;
        Ok(Box::new(RealGitRepo { repo }))
    }
}

pub struct RealGitRepo {
    repo: Repository,
}

impl GitRepo for RealGitRepo {
    fn workdir(&self) -> &Path {
        self.repo.workdir()
    }

    fn head_content(&self, path: &Path) -> Result<String, GitError> {
        self.repo.head_content(path)
    }

    fn index_content(&self, path: &Path) -> Result<String, GitError> {
        self.repo.index_content(path)
    }

    fn parent_content(&self, path: &Path) -> Result<String, GitError> {
        self.repo.parent_content(path)
    }

    fn gather_status(&self) -> Result<Vec<GitStatusEntry>, GitStatusError> {
        crate::git::status::gather_git_status(self.repo.inner())
    }

    fn gather_branch_info(&self) -> Option<GitBranchInfo> {
        crate::git::status::gather_git_branch_info(self.repo.inner())
    }

    fn blame_file(&self, path: &Path) -> Result<BlameData, GitError> {
        crate::git::blame::blame_file(&self.repo, path)
    }

    fn count_hunks_by_file(
        &self,
        mode: DiffComparisonMode,
    ) -> Result<HashMap<PathBuf, usize>, GitError> {
        self.repo.count_hunks_by_file(mode)
    }

    fn commit_changed_files(&self) -> Result<Vec<PathBuf>, GitError> {
        self.repo.commit_changed_files()
    }

    fn commit_files_by_oid(&self, oid: &str) -> Result<Vec<CommitFileChange>, GitError> {
        self.repo.commit_files_by_oid(oid)
    }

    fn commit_file_diff(&self, oid: &str, path: &Path) -> Result<String, GitError> {
        self.repo.commit_file_diff(oid, path)
    }

    fn apply_diff(
        &self,
        patch: &str,
        reverse: bool,
        location: ApplyLocation,
    ) -> Result<(), GitError> {
        let patch_str = if reverse {
            reverse_patch_text(patch)
        } else {
            patch.to_string()
        };
        let diff = git2::Diff::from_buffer(patch_str.as_bytes())
            .map_err(|e| GitError::GitOperationFailed(format!("Failed to parse patch: {e}")))?;
        let git2_location = match location {
            ApplyLocation::Index => git2::ApplyLocation::Index,
            ApplyLocation::WorkDir => git2::ApplyLocation::WorkDir,
        };
        self.repo
            .inner()
            .apply(&diff, git2_location, None)
            .map_err(|e| GitError::GitOperationFailed(format!("Failed to apply diff: {e}")))?;
        Ok(())
    }

    fn stage_file(&self, path: &Path) -> Result<(), GitError> {
        run_git(self.repo.workdir(), &["add", "--", &path.to_string_lossy()])
    }

    fn unstage_file(&self, path: &Path) -> Result<(), GitError> {
        run_git(
            self.repo.workdir(),
            &["reset", "HEAD", "--", &path.to_string_lossy()],
        )
    }

    fn stage_all(&self) -> Result<(), GitError> {
        run_git(self.repo.workdir(), &["add", "-A"])
    }

    fn unstage_all(&self) -> Result<(), GitError> {
        run_git(self.repo.workdir(), &["reset", "HEAD"])
    }

    fn log_commits(
        &self,
        base: &str,
        head: &str,
        max: usize,
    ) -> Result<Vec<CommitLogEntry>, GitError> {
        self.repo.log_commits(base, head, max)
    }

    fn merge_base(&self, ref1: &str, ref2: &str) -> Result<String, GitError> {
        self.repo.merge_base(ref1, ref2)
    }

    fn upstream_ref(&self) -> Result<Option<String>, GitError> {
        self.repo.upstream_ref()
    }

    fn rebase_interactive(&self, base_ref: &str, todo_content: &str) -> Result<(), GitError> {
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
            .args(["rebase", "-i", base_ref])
            .env("GIT_SEQUENCE_EDITOR", &seq_editor)
            .current_dir(self.repo.workdir())
            .output()
            .map_err(|e| GitError::GitOperationFailed(format!("Failed to run git rebase: {e}")))?;

        // Exit code 1 with rebase-merge dir means paused (conflicts/edit) -- not a failure
        if !output.status.success() {
            let rebase_dir = self.repo.workdir().join(".git/rebase-merge");
            if !rebase_dir.exists() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(GitError::GitOperationFailed(format!(
                    "git rebase -i failed: {stderr}"
                )));
            }
        }
        Ok(())
    }

    fn rebase_continue(&self) -> Result<(), GitError> {
        let output = std::process::Command::new("git")
            .args(["rebase", "--continue"])
            .env("GIT_EDITOR", "true")
            .current_dir(self.repo.workdir())
            .output()
            .map_err(|e| GitError::GitOperationFailed(format!("Failed to run git rebase: {e}")))?;

        if !output.status.success() {
            let rebase_dir = self.repo.workdir().join(".git/rebase-merge");
            if !rebase_dir.exists() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(GitError::GitOperationFailed(format!(
                    "git rebase --continue failed: {stderr}"
                )));
            }
        }
        Ok(())
    }

    fn rebase_abort(&self) -> Result<(), GitError> {
        run_git(self.repo.workdir(), &["rebase", "--abort"])
    }

    fn rebase_skip(&self) -> Result<(), GitError> {
        let output = std::process::Command::new("git")
            .args(["rebase", "--skip"])
            .current_dir(self.repo.workdir())
            .output()
            .map_err(|e| GitError::GitOperationFailed(format!("Failed to run git rebase: {e}")))?;

        if !output.status.success() {
            let rebase_dir = self.repo.workdir().join(".git/rebase-merge");
            if !rebase_dir.exists() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(GitError::GitOperationFailed(format!(
                    "git rebase --skip failed: {stderr}"
                )));
            }
        }
        Ok(())
    }

    fn has_unmerged_paths(&self) -> bool {
        self.repo
            .inner()
            .index()
            .map(|idx| idx.has_conflicts())
            .unwrap_or(false)
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
}

#[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
impl GitProvider for FakeGitProvider {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn discover(&self, _path: &Path) -> Result<Box<dyn GitRepo>, GitError> {
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

    fn open(&self, _path: &Path) -> Result<Box<dyn GitRepo>, GitError> {
        self.discover(_path)
    }
}

#[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
struct FakeGitRepo {
    workdir_path: PathBuf,
    state: Arc<Mutex<FakeGitState>>,
    fs: Arc<FakeFs>,
}

#[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
impl GitRepo for FakeGitRepo {
    fn workdir(&self) -> &Path {
        &self.workdir_path
    }

    fn head_content(&self, path: &Path) -> Result<String, GitError> {
        let state = self.state.lock();
        state
            .head_files
            .get(path)
            .cloned()
            .ok_or(GitError::FileNotFound(path.to_path_buf()))
    }

    fn index_content(&self, path: &Path) -> Result<String, GitError> {
        let state = self.state.lock();
        state
            .index_files
            .get(path)
            .cloned()
            .ok_or(GitError::FileNotFound(path.to_path_buf()))
    }

    fn parent_content(&self, path: &Path) -> Result<String, GitError> {
        let state = self.state.lock();
        state
            .parent_files
            .get(path)
            .cloned()
            .ok_or(GitError::FileNotFound(path.to_path_buf()))
    }

    fn gather_status(&self) -> Result<Vec<GitStatusEntry>, GitStatusError> {
        Ok(self.state.lock().status_entries.clone())
    }

    fn gather_branch_info(&self) -> Option<GitBranchInfo> {
        self.state.lock().branch_info.clone()
    }

    fn blame_file(&self, path: &Path) -> Result<BlameData, GitError> {
        let state = self.state.lock();
        state
            .blame_data
            .get(path)
            .cloned()
            .ok_or(GitError::FileNotFound(path.to_path_buf()))
    }

    fn count_hunks_by_file(
        &self,
        mode: DiffComparisonMode,
    ) -> Result<HashMap<PathBuf, usize>, GitError> {
        let state = self.state.lock();
        if !state.hunk_counts.is_empty() {
            return Ok(state.hunk_counts.clone());
        }
        compute_hunk_counts(&state, &self.fs, &self.workdir_path, mode)
    }

    fn commit_changed_files(&self) -> Result<Vec<PathBuf>, GitError> {
        let state = self.state.lock();
        Ok(state
            .commit_history
            .last()
            .map(|c| c.changed_files.iter().map(|f| f.path.clone()).collect())
            .unwrap_or_default())
    }

    fn commit_files_by_oid(&self, oid: &str) -> Result<Vec<CommitFileChange>, GitError> {
        let state = self.state.lock();
        Ok(state
            .commit_history
            .iter()
            .find(|c| c.oid == oid)
            .map(|c| c.changed_files.clone())
            .unwrap_or_default())
    }

    fn commit_file_diff(&self, oid: &str, path: &Path) -> Result<String, GitError> {
        let state = self.state.lock();
        state
            .commit_history
            .iter()
            .find(|c| c.oid == oid)
            .and_then(|c| c.diffs.get(path).cloned())
            .ok_or(GitError::FileNotFound(path.to_path_buf()))
    }

    fn apply_diff(
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

    fn stage_file(&self, path: &Path) -> Result<(), GitError> {
        self.state.lock().staged_files.insert(path.to_path_buf());
        Ok(())
    }

    fn unstage_file(&self, path: &Path) -> Result<(), GitError> {
        self.state.lock().staged_files.remove(path);
        Ok(())
    }

    fn stage_all(&self) -> Result<(), GitError> {
        let mut state = self.state.lock();
        let all_paths: Vec<PathBuf> = state
            .status_entries
            .iter()
            .map(|e| e.path.clone())
            .collect();
        state.staged_files.extend(all_paths);
        Ok(())
    }

    fn unstage_all(&self) -> Result<(), GitError> {
        self.state.lock().staged_files.clear();
        Ok(())
    }

    fn log_commits(
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
            })
            .collect())
    }

    fn merge_base(&self, _ref1: &str, _ref2: &str) -> Result<String, GitError> {
        let state = self.state.lock();
        Ok(state
            .commit_history
            .first()
            .map(|c| c.oid.clone())
            .unwrap_or_default())
    }

    fn upstream_ref(&self) -> Result<Option<String>, GitError> {
        let state = self.state.lock();
        Ok(state
            .branch_info
            .as_ref()
            .map(|_| "refs/remotes/origin/main".to_string()))
    }

    fn rebase_interactive(&self, _base_ref: &str, _todo_content: &str) -> Result<(), GitError> {
        Ok(())
    }

    fn rebase_continue(&self) -> Result<(), GitError> {
        Ok(())
    }

    fn rebase_abort(&self) -> Result<(), GitError> {
        Ok(())
    }

    fn rebase_skip(&self) -> Result<(), GitError> {
        Ok(())
    }

    fn has_unmerged_paths(&self) -> bool {
        false
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

        let repo = provider.open(&workdir).unwrap();
        let commits = repo.log_commits("base", "HEAD", 10).unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].oid, "abc1234567890");
        assert_eq!(commits[0].short_hash, "abc1234");
        assert_eq!(commits[1].oid, "def5678901234");
    }

    #[test]
    fn fake_merge_base_returns_first() {
        let fs = Arc::new(FakeFs::new());
        let provider = FakeGitProvider::new(fs);
        let workdir = PathBuf::from("/fake/repo");
        provider.set_exists(true);
        provider.set_workdir(workdir.clone());
        provider.add_commit("first_commit", vec![], HashMap::new());
        provider.add_commit("second_commit", vec![], HashMap::new());

        let repo = provider.open(&workdir).unwrap();
        assert_eq!(repo.merge_base("a", "b").unwrap(), "first_commit");
    }

    #[test]
    fn fake_upstream_ref_with_branch_info() {
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

        let repo = provider.open(&workdir).unwrap();
        assert_eq!(
            repo.upstream_ref().unwrap(),
            Some("refs/remotes/origin/main".to_string())
        );
    }

    #[test]
    fn fake_upstream_ref_without_branch_info() {
        let fs = Arc::new(FakeFs::new());
        let provider = FakeGitProvider::new(fs);
        let workdir = PathBuf::from("/fake/repo");
        provider.set_exists(true);
        provider.set_workdir(workdir.clone());

        let repo = provider.open(&workdir).unwrap();
        assert_eq!(repo.upstream_ref().unwrap(), None);
    }

    #[test]
    fn fake_rebase_methods_are_noops() {
        let fs = Arc::new(FakeFs::new());
        let provider = FakeGitProvider::new(fs);
        let workdir = PathBuf::from("/fake/repo");
        provider.set_exists(true);
        provider.set_workdir(workdir.clone());

        let repo = provider.open(&workdir).unwrap();
        assert!(repo.rebase_interactive("base", "pick abc123 msg\n").is_ok());
        assert!(repo.rebase_continue().is_ok());
        assert!(repo.rebase_abort().is_ok());
        assert!(repo.rebase_skip().is_ok());
        assert!(!repo.has_unmerged_paths());
    }
}

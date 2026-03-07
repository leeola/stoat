use crate::git::{
    blame::BlameData,
    diff_review::DiffComparisonMode,
    repository::{CommitFileChange, GitError, Repository},
    status::{GitBranchInfo, GitStatusEntry, GitStatusError},
};
use std::{
    any::Any,
    collections::HashMap,
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
}

fn run_git(workdir: &Path, args: &[&str]) -> Result<(), GitError> {
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

#[cfg(any(test, feature = "test-support"))]
use parking_lot::Mutex;
#[cfg(any(test, feature = "test-support"))]
use std::sync::Arc;

#[cfg(any(test, feature = "test-support"))]
pub struct FakeGitProvider {
    state: Arc<Mutex<FakeGitState>>,
}

#[cfg(any(test, feature = "test-support"))]
struct FakeGitState {
    workdir: PathBuf,
    head_files: HashMap<PathBuf, String>,
    index_files: HashMap<PathBuf, String>,
    status_entries: Vec<GitStatusEntry>,
    branch_info: Option<GitBranchInfo>,
    exists: bool,
}

#[cfg(any(test, feature = "test-support"))]
impl FakeGitProvider {
    pub fn empty() -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeGitState {
                workdir: PathBuf::new(),
                head_files: HashMap::new(),
                index_files: HashMap::new(),
                status_entries: Vec::new(),
                branch_info: None,
                exists: false,
            })),
        }
    }

    pub fn with_repo(workdir: PathBuf) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeGitState {
                workdir,
                head_files: HashMap::new(),
                index_files: HashMap::new(),
                status_entries: Vec::new(),
                branch_info: None,
                exists: true,
            })),
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
}

#[cfg(any(test, feature = "test-support"))]
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
        }))
    }

    fn open(&self, _path: &Path) -> Result<Box<dyn GitRepo>, GitError> {
        self.discover(_path)
    }
}

#[cfg(any(test, feature = "test-support"))]
struct FakeGitRepo {
    workdir_path: PathBuf,
    state: Arc<Mutex<FakeGitState>>,
}

#[cfg(any(test, feature = "test-support"))]
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

    fn parent_content(&self, _path: &Path) -> Result<String, GitError> {
        Ok(String::new())
    }

    fn gather_status(&self) -> Result<Vec<GitStatusEntry>, GitStatusError> {
        Ok(self.state.lock().status_entries.clone())
    }

    fn gather_branch_info(&self) -> Option<GitBranchInfo> {
        self.state.lock().branch_info.clone()
    }

    fn blame_file(&self, _path: &Path) -> Result<BlameData, GitError> {
        Ok(BlameData {
            entries: Vec::new(),
            line_to_entry: Vec::new(),
        })
    }

    fn count_hunks_by_file(
        &self,
        _mode: DiffComparisonMode,
    ) -> Result<HashMap<PathBuf, usize>, GitError> {
        Ok(HashMap::new())
    }

    fn commit_changed_files(&self) -> Result<Vec<PathBuf>, GitError> {
        Ok(Vec::new())
    }

    fn commit_files_by_oid(&self, _oid: &str) -> Result<Vec<CommitFileChange>, GitError> {
        Ok(Vec::new())
    }

    fn commit_file_diff(&self, _oid: &str, _path: &Path) -> Result<String, GitError> {
        Ok(String::new())
    }

    fn apply_diff(
        &self,
        _patch: &str,
        _reverse: bool,
        _location: ApplyLocation,
    ) -> Result<(), GitError> {
        Ok(())
    }

    fn stage_file(&self, _path: &Path) -> Result<(), GitError> {
        Ok(())
    }

    fn unstage_file(&self, _path: &Path) -> Result<(), GitError> {
        Ok(())
    }

    fn stage_all(&self) -> Result<(), GitError> {
        Ok(())
    }

    fn unstage_all(&self) -> Result<(), GitError> {
        Ok(())
    }
}

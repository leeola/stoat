use crate::host::{
    fake::FakeFs,
    git::{
        ChangedFile, CommitFileChange, CommitFileChangeKind, CommitInfo, GitApplyError, GitHost,
        GitRepo, RebaseError, RebaseTodo, RebaseTodoOp, RewriteResult,
    },
};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// In-memory [`GitHost`] for tests.
///
/// Populate with repos via [`FakeGit::add_repo`]; each call returns a
/// [`FakeRepoBuilder`] that mirrors the ergonomics of [`crate::host::FakeClaudeCode`]'s
/// `push_*` helpers. When a [`FakeFs`] reference is supplied to the
/// builder, the builder also writes working-tree content into it so the
/// application code (which reads via `FsHost`) sees consistent state.
pub struct FakeGit {
    state: Mutex<FakeGitState>,
}

struct FakeGitState {
    repos: HashMap<PathBuf, Arc<FakeGitRepo>>,
    /// Descendant-path to workdir. Populated when a repo is registered so
    /// `discover` can walk up from any child path the way
    /// `git2::Repository::discover` would. Most-specific prefix wins.
    discover_index: Vec<(PathBuf, PathBuf)>,
}

impl Default for FakeGit {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeGit {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(FakeGitState {
                repos: HashMap::new(),
                discover_index: Vec::new(),
            }),
        }
    }

    /// Register a repository rooted at `workdir`. Returns a builder for
    /// populating HEAD contents and changed-file entries. Calling this
    /// again with the same workdir returns a builder that appends to the
    /// existing state.
    pub fn add_repo(&self, workdir: impl Into<PathBuf>) -> FakeRepoBuilder<'_> {
        let workdir = workdir.into();
        {
            let mut state = self.state.lock().unwrap();
            state.repos.entry(workdir.clone()).or_insert_with(|| {
                Arc::new(FakeGitRepo {
                    workdir: workdir.clone(),
                    state: Mutex::new(FakeRepoState::default()),
                })
            });
            if !state
                .discover_index
                .iter()
                .any(|(start, _)| start == &workdir)
            {
                state
                    .discover_index
                    .push((workdir.clone(), workdir.clone()));
            }
        }
        FakeRepoBuilder {
            host: self,
            workdir,
            fs: None,
        }
    }

    /// Snapshot the rebase plans executed against `workdir`, in call
    /// order. Each entry captures the onto sha, the exact todo list,
    /// and the new HEAD sha minted by the fake.
    pub fn applied_rebases(&self, workdir: &Path) -> Vec<RecordedRebase> {
        let state = self.state.lock().unwrap();
        state
            .repos
            .get(workdir)
            .map(|repo| repo.state.lock().unwrap().applied_rebases.clone())
            .unwrap_or_default()
    }

    /// Snapshot the amend-head calls against `workdir`, in call order.
    pub fn amend_history(&self, workdir: &Path) -> Vec<RecordedAmend> {
        let state = self.state.lock().unwrap();
        state
            .repos
            .get(workdir)
            .map(|repo| repo.state.lock().unwrap().amend_history.clone())
            .unwrap_or_default()
    }

    /// Snapshot the patches applied to a repo via `apply_to_index`. Useful
    /// for asserting the review-apply flow wrote the expected unified-diff
    /// patch. Empty when no patches have been applied or the repo is unknown.
    pub fn applied_patches(&self, workdir: &Path) -> Vec<String> {
        let state = self.state.lock().unwrap();
        state
            .repos
            .get(workdir)
            .map(|repo| repo.state.lock().unwrap().applied_patches.clone())
            .unwrap_or_default()
    }

    /// Snapshot applied patches grouped by the target path parsed out of
    /// their `+++ b/<rel>` header. Returns absolute paths by joining the
    /// relative target against `workdir`. Patches whose target cannot be
    /// parsed (or that target `/dev/null`) map to `workdir` itself.
    pub fn applied_patches_by_path(&self, workdir: &Path) -> Vec<(PathBuf, String)> {
        self.applied_patches(workdir)
            .into_iter()
            .map(|patch| {
                let rel = parse_patch_target(&patch);
                let abs = match rel {
                    Some(r) => workdir.join(r),
                    None => workdir.to_path_buf(),
                };
                (abs, patch)
            })
            .collect()
    }
}

fn parse_patch_target(patch: &str) -> Option<PathBuf> {
    let mut buffer_target: Option<PathBuf> = None;
    let mut base_target: Option<PathBuf> = None;
    let mut buffer_is_dev_null = false;
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            buffer_target = Some(PathBuf::from(rest));
        } else if line == "+++ /dev/null" {
            buffer_is_dev_null = true;
        } else if let Some(rest) = line.strip_prefix("--- a/") {
            base_target = Some(PathBuf::from(rest));
        }
    }
    if buffer_is_dev_null {
        base_target
    } else {
        buffer_target.or(base_target)
    }
}

impl GitHost for FakeGit {
    fn discover(&self, path: &Path) -> Option<Arc<dyn GitRepo>> {
        let state = self.state.lock().unwrap();
        let workdir = state
            .discover_index
            .iter()
            .filter(|(start, _)| path.starts_with(start))
            .max_by_key(|(start, _)| start.components().count())
            .map(|(_, wd)| wd.clone())?;
        state
            .repos
            .get(&workdir)
            .map(|arc| arc.clone() as Arc<dyn GitRepo>)
    }
}

/// Builder returned by [`FakeGit::add_repo`]. Method chaining style
/// mirrors [`crate::host::FakeClaudeCode`]'s `push_*` API: each call
/// returns `&mut Self` so a test can line up fixtures in a single
/// statement.
pub struct FakeRepoBuilder<'a> {
    host: &'a FakeGit,
    workdir: PathBuf,
    fs: Option<&'a FakeFs>,
}

impl<'a> FakeRepoBuilder<'a> {
    /// Attach a [`FakeFs`] to this builder. Subsequent calls that accept a
    /// working-tree content string will also write the file into this
    /// filesystem so `FsHost::read` sees the same content the application
    /// code expects.
    pub fn with_fs(mut self, fs: &'a FakeFs) -> Self {
        self.fs = Some(fs);
        self
    }

    /// Register `rel_path` as present in HEAD with the given content.
    /// Does not write to any attached [`FakeFs`]; use [`Self::unstaged_file`],
    /// [`Self::staged_file`], or [`Self::modified`] for working-tree state.
    pub fn head_file(&mut self, rel_path: impl AsRef<Path>, content: &str) -> &mut Self {
        self.mutate_repo(|state| {
            state
                .head_contents
                .insert(rel_path.as_ref().to_path_buf(), content.to_string());
        });
        self
    }

    /// Record `rel_path` as modified in the working tree. Writes `working`
    /// to the attached [`FakeFs`] at the absolute path if one was attached.
    pub fn unstaged_file(&mut self, rel_path: impl AsRef<Path>, working: &str) -> &mut Self {
        let rel = rel_path.as_ref().to_path_buf();
        let abs = self.workdir.join(&rel);
        self.mutate_repo(|state| {
            state.changed.retain(|f| f.path != abs);
            state.changed.push(ChangedFile {
                path: abs.clone(),
                staged: false,
            });
        });
        if let Some(fs) = self.fs {
            fs.insert_file(&abs, working.as_bytes());
        }
        self
    }

    /// Record `rel_path` as staged in the index. Behaves like
    /// [`Self::unstaged_file`] but marks the entry staged.
    pub fn staged_file(&mut self, rel_path: impl AsRef<Path>, working: &str) -> &mut Self {
        let rel = rel_path.as_ref().to_path_buf();
        let abs = self.workdir.join(&rel);
        self.mutate_repo(|state| {
            state.changed.retain(|f| f.path != abs);
            state.changed.push(ChangedFile {
                path: abs.clone(),
                staged: true,
            });
        });
        if let Some(fs) = self.fs {
            fs.insert_file(&abs, working.as_bytes());
        }
        self
    }

    /// Convenience: the common "modified file" case. Registers HEAD content
    /// plus an unstaged working-tree version in one call. The two contents
    /// must differ; equal contents indicate the caller meant
    /// [`Self::head_file`] and this panics to catch the mistake.
    pub fn modified(&mut self, rel_path: impl AsRef<Path>, head: &str, working: &str) -> &mut Self {
        assert_ne!(
            head, working,
            "FakeRepoBuilder::modified expects head != working; use head_file() for unchanged files"
        );
        let rel = rel_path.as_ref().to_path_buf();
        self.head_file(&rel, head);
        self.unstaged_file(&rel, working);
        self
    }

    /// Convenience: a newly added file with no HEAD blob.
    pub fn added(&mut self, rel_path: impl AsRef<Path>, working: &str) -> &mut Self {
        self.unstaged_file(rel_path, working);
        self
    }

    /// Record `rel_path` as deleted in the working tree: present in HEAD,
    /// absent from the filesystem. Mirrors `git status` reporting a deleted
    /// path. Does not write to any attached [`FakeFs`]; callers that
    /// previously seeded the file there should remove it themselves.
    pub fn deleted(&mut self, rel_path: impl AsRef<Path>, head: &str) -> &mut Self {
        let rel = rel_path.as_ref().to_path_buf();
        let abs = self.workdir.join(&rel);
        self.head_file(&rel, head);
        self.mutate_repo(|state| {
            state.changed.retain(|f| f.path != abs);
            state.changed.push(ChangedFile {
                path: abs,
                staged: false,
            });
        });
        self
    }

    /// Seed a root (no-parent) commit identified by `sha` with the given
    /// tree. `files` is a slice of `(rel_path, content)` pairs; entries
    /// are stored as the commit's full tree snapshot.
    pub fn commit(&mut self, sha: &str, files: &[(&str, &str)]) -> &mut Self {
        self.commit_full(sha, None, None, files)
    }

    /// Seed a commit with a given first-parent. The parent sha does not
    /// need to exist at the time of this call; the lookup happens only
    /// when [`GitRepo::parent_sha`] or [`GitRepo::commit_tree`] is
    /// invoked.
    pub fn commit_with_parent(
        &mut self,
        sha: &str,
        parent: &str,
        files: &[(&str, &str)],
    ) -> &mut Self {
        self.commit_full(sha, Some(parent.to_string()), None, files)
    }

    /// Seed a commit with an explicit message. Writers for commit-list
    /// tests prefer this over [`Self::commit`] because the summary line
    /// shows up in the rendered UI.
    pub fn commit_with_message(
        &mut self,
        sha: &str,
        message: &str,
        files: &[(&str, &str)],
    ) -> &mut Self {
        self.commit_full(sha, None, Some(message.to_string()), files)
    }

    /// Seed a commit with both a parent and an explicit message.
    pub fn commit_with_parent_message(
        &mut self,
        sha: &str,
        parent: &str,
        message: &str,
        files: &[(&str, &str)],
    ) -> &mut Self {
        self.commit_full(
            sha,
            Some(parent.to_string()),
            Some(message.to_string()),
            files,
        )
    }

    fn commit_full(
        &mut self,
        sha: &str,
        parent: Option<String>,
        message: Option<String>,
        files: &[(&str, &str)],
    ) -> &mut Self {
        let tree: BTreeMap<PathBuf, String> = files
            .iter()
            .map(|(p, c)| (PathBuf::from(p), (*c).to_string()))
            .collect();
        let sha = sha.to_string();
        self.mutate_repo(|state| {
            let seq = state.commits.len() as i64;
            let commit = FakeCommit {
                parent,
                tree,
                message: message.unwrap_or_else(|| format!("commit {sha}")),
                author_name: "fake".into(),
                author_email: "fake@example.invalid".into(),
                time: 1_700_000_000 + seq,
            };
            state.commits.insert(sha.clone(), commit);
            state.head = Some(sha);
        });
        self
    }

    /// Point HEAD at `sha` explicitly. Useful when a test seeds several
    /// independent branches; the last [`Self::commit_full`] invocation
    /// wins by default, which is the common case.
    pub fn set_head(&mut self, sha: &str) -> &mut Self {
        let sha = sha.to_string();
        self.mutate_repo(|state| state.head = Some(sha));
        self
    }

    /// Program the repo to return `Err(GitApplyError::Backend(message))`
    /// for every subsequent call to [`GitRepo::apply_to_index`] until
    /// [`Self::clear_apply_failure`] is called. The failing calls still
    /// record into `applied_patches` so tests can assert what was
    /// attempted.
    pub fn fail_apply_with(&mut self, message: &str) -> &mut Self {
        let msg = message.to_string();
        self.mutate_repo(|state| state.apply_error = Some(msg));
        self
    }

    /// Force the next [`GitRepo::rewrite_commit`] or
    /// [`GitRepo::run_rebase`] to surface a conflict at the given sha.
    /// The operation aborts atomically without mutating state.
    pub fn simulate_conflict_at(&mut self, sha: &str) -> &mut Self {
        let s = sha.to_string();
        self.mutate_repo(|state| state.conflict_at = Some(s));
        self
    }

    /// Clear any previously installed conflict simulation.
    pub fn clear_conflict_simulation(&mut self) -> &mut Self {
        self.mutate_repo(|state| state.conflict_at = None);
        self
    }

    /// Remove any injected apply failure so subsequent
    /// [`GitRepo::apply_to_index`] calls succeed again.
    pub fn clear_apply_failure(&mut self) -> &mut Self {
        self.mutate_repo(|state| state.apply_error = None);
        self
    }

    fn mutate_repo<F: FnOnce(&mut FakeRepoState)>(&self, f: F) {
        let state = self.host.state.lock().unwrap();
        let repo = state
            .repos
            .get(&self.workdir)
            .expect("FakeRepoBuilder: repo must be registered");
        let mut inner = repo.state.lock().unwrap();
        f(&mut inner);
    }
}

pub struct FakeGitRepo {
    workdir: PathBuf,
    state: Mutex<FakeRepoState>,
}

#[derive(Default)]
struct FakeRepoState {
    head_contents: HashMap<PathBuf, String>,
    changed: Vec<ChangedFile>,
    applied_patches: Vec<String>,
    /// When `Some`, the next [`GitRepo::apply_to_index`] call returns
    /// `Err(GitApplyError::Backend(_))` with this message. The failing
    /// patch is still pushed to `applied_patches`.
    apply_error: Option<String>,
    /// Commit objects keyed by opaque sha. Populated via
    /// [`FakeRepoBuilder::commit`] and friends.
    commits: HashMap<String, FakeCommit>,
    /// The tip of the simulated branch. Defaults to the last inserted
    /// commit, overridable via [`FakeRepoBuilder::set_head`].
    head: Option<String>,
    /// When set, rewrite/rebase calls that touch this sha return a
    /// conflict error without mutating state. Used by error-path tests.
    conflict_at: Option<String>,
    /// Counter used to mint synthetic shas for amend/rewrite/rebase.
    synth_counter: u64,
    /// Record of every rebase plan executed against this repo, oldest
    /// first. Tests assert on this to check plan correctness.
    applied_rebases: Vec<RecordedRebase>,
    /// Record of every amend_head invocation, in call order.
    amend_history: Vec<RecordedAmend>,
}

#[derive(Clone, Debug)]
pub struct RecordedRebase {
    pub onto: String,
    pub todo: Vec<RebaseTodo>,
    pub new_head: String,
}

#[derive(Clone, Debug)]
pub struct RecordedAmend {
    pub old_head: String,
    pub new_head: String,
}

#[derive(Clone)]
struct FakeCommit {
    parent: Option<String>,
    tree: BTreeMap<PathBuf, String>,
    message: String,
    author_name: String,
    author_email: String,
    time: i64,
}

impl GitRepo for FakeGitRepo {
    fn workdir(&self) -> Option<PathBuf> {
        Some(self.workdir.clone())
    }

    fn changed_files(&self) -> Vec<ChangedFile> {
        let state = self.state.lock().unwrap();
        let mut staged: Vec<ChangedFile> =
            state.changed.iter().filter(|f| f.staged).cloned().collect();
        let mut unstaged: Vec<ChangedFile> = state
            .changed
            .iter()
            .filter(|f| !f.staged)
            .cloned()
            .collect();
        staged.sort_by(|a, b| a.path.cmp(&b.path));
        unstaged.sort_by(|a, b| a.path.cmp(&b.path));
        staged.extend(unstaged);
        staged
    }

    fn head_content(&self, path: &Path) -> Option<String> {
        let rel = path.strip_prefix(&self.workdir).ok()?;
        let state = self.state.lock().unwrap();
        state.head_contents.get(rel).cloned()
    }

    fn apply_to_index(&self, patch: &str) -> Result<(), GitApplyError> {
        let mut state = self.state.lock().unwrap();
        state.applied_patches.push(patch.to_string());
        match &state.apply_error {
            Some(msg) => Err(GitApplyError::Backend(msg.clone())),
            None => Ok(()),
        }
    }

    fn commit_tree(&self, sha: &str) -> Option<BTreeMap<PathBuf, String>> {
        let state = self.state.lock().unwrap();
        state.commits.get(sha).map(|c| c.tree.clone())
    }

    fn parent_sha(&self, sha: &str) -> Option<String> {
        let state = self.state.lock().unwrap();
        state.commits.get(sha).and_then(|c| c.parent.clone())
    }

    fn log_commits(&self, after: Option<&str>, limit: usize) -> Vec<CommitInfo> {
        if limit == 0 {
            return Vec::new();
        }
        let state = self.state.lock().unwrap();
        let start = match after {
            Some(sha) => state.commits.get(sha).and_then(|c| c.parent.clone()),
            None => state.head.clone(),
        };
        let Some(mut cursor) = start else {
            return Vec::new();
        };

        let mut out: Vec<CommitInfo> = Vec::with_capacity(limit.min(4096));
        while out.len() < limit {
            let Some(commit) = state.commits.get(&cursor) else {
                break;
            };
            let parent_count = commit.parent.as_ref().map(|_| 1).unwrap_or(0);
            let short_sha = cursor.chars().take(7).collect();
            out.push(CommitInfo {
                sha: cursor.clone(),
                short_sha,
                summary: commit.message.lines().next().unwrap_or("").to_string(),
                author_name: commit.author_name.clone(),
                author_email: commit.author_email.clone(),
                time: commit.time,
                parent_count,
            });
            match &commit.parent {
                Some(p) => cursor = p.clone(),
                None => break,
            }
        }
        out
    }

    fn amend_head(
        &self,
        tree: &BTreeMap<PathBuf, String>,
        message: Option<&str>,
    ) -> Result<String, GitApplyError> {
        let mut state = self.state.lock().unwrap();
        let Some(head_sha) = state.head.clone() else {
            return Err(GitApplyError::Backend("HEAD has no commit".into()));
        };
        let Some(head_commit) = state.commits.get(&head_sha).cloned() else {
            return Err(GitApplyError::Backend("HEAD commit missing".into()));
        };
        state.synth_counter += 1;
        let new_sha = format!(
            "amended-{}-{}",
            &head_sha[..head_sha.len().min(6)],
            state.synth_counter
        );
        let new_msg = message
            .map(str::to_string)
            .unwrap_or(head_commit.message.clone());
        let new_commit = FakeCommit {
            parent: head_commit.parent.clone(),
            tree: tree.clone(),
            message: new_msg,
            author_name: head_commit.author_name.clone(),
            author_email: head_commit.author_email.clone(),
            time: head_commit.time,
        };
        state.commits.insert(new_sha.clone(), new_commit);
        state.head = Some(new_sha.clone());
        state.amend_history.push(RecordedAmend {
            old_head: head_sha,
            new_head: new_sha.clone(),
        });
        Ok(new_sha)
    }

    fn rewrite_commit(
        &self,
        sha: &str,
        tree: &BTreeMap<PathBuf, String>,
        message: Option<&str>,
        descendants: &[String],
    ) -> Result<RewriteResult, GitApplyError> {
        let mut state = self.state.lock().unwrap();
        if let Some(c) = &state.conflict_at {
            if c == sha || descendants.iter().any(|d| d == c) {
                return Err(GitApplyError::Backend(format!(
                    "simulated cherry-pick conflict at {c}"
                )));
            }
        }
        let Some(target) = state.commits.get(sha).cloned() else {
            return Err(GitApplyError::Backend(format!("unknown sha: {sha}")));
        };

        state.synth_counter += 1;
        let new_target_sha = format!(
            "rewritten-{}-{}",
            &sha[..sha.len().min(6)],
            state.synth_counter
        );
        let new_target = FakeCommit {
            parent: target.parent.clone(),
            tree: tree.clone(),
            message: message
                .map(str::to_string)
                .unwrap_or(target.message.clone()),
            author_name: target.author_name.clone(),
            author_email: target.author_email.clone(),
            time: target.time,
        };
        state.commits.insert(new_target_sha.clone(), new_target);

        let mut mapping: HashMap<String, String> = HashMap::new();
        mapping.insert(sha.to_string(), new_target_sha.clone());
        let mut current = new_target_sha.clone();

        for desc_sha in descendants {
            let Some(desc) = state.commits.get(desc_sha).cloned() else {
                return Err(GitApplyError::Backend(format!(
                    "unknown descendant sha: {desc_sha}"
                )));
            };
            state.synth_counter += 1;
            let new_sha = format!(
                "rewritten-{}-{}",
                &desc_sha[..desc_sha.len().min(6)],
                state.synth_counter
            );
            let new_commit = FakeCommit {
                parent: Some(current.clone()),
                tree: desc.tree.clone(),
                message: desc.message.clone(),
                author_name: desc.author_name.clone(),
                author_email: desc.author_email.clone(),
                time: desc.time,
            };
            state.commits.insert(new_sha.clone(), new_commit);
            mapping.insert(desc_sha.clone(), new_sha.clone());
            current = new_sha;
        }

        state.head = Some(current.clone());
        Ok(RewriteResult {
            new_head: current,
            mapping,
        })
    }

    fn run_rebase(&self, onto: &str, todo: &[RebaseTodo]) -> Result<String, RebaseError> {
        let mut state = self.state.lock().unwrap();
        if let Some(c) = &state.conflict_at {
            if todo.iter().any(|t| &t.sha == c) {
                return Err(RebaseError::Conflict { at_sha: c.clone() });
            }
        }
        if !state.commits.contains_key(onto) && !onto.is_empty() {
            return Err(RebaseError::Backend(format!("unknown onto sha: {onto}")));
        }

        let mut current = onto.to_string();
        let mut last_commit: Option<String> = None;
        let mut last_message: Option<String> = None;

        for entry in todo {
            match entry.op {
                RebaseTodoOp::Drop => continue,
                RebaseTodoOp::Pick => {
                    let Some(src) = state.commits.get(&entry.sha).cloned() else {
                        return Err(RebaseError::Backend(format!(
                            "unknown sha in rebase: {}",
                            entry.sha
                        )));
                    };
                    state.synth_counter += 1;
                    let new_sha = format!(
                        "rebased-{}-{}",
                        &entry.sha[..entry.sha.len().min(6)],
                        state.synth_counter
                    );
                    let new_commit = FakeCommit {
                        parent: Some(current.clone()),
                        tree: src.tree.clone(),
                        message: src.message.clone(),
                        author_name: src.author_name.clone(),
                        author_email: src.author_email.clone(),
                        time: src.time,
                    };
                    state.commits.insert(new_sha.clone(), new_commit);
                    last_message = Some(src.message);
                    current = new_sha.clone();
                    last_commit = Some(new_sha);
                },
                RebaseTodoOp::Squash | RebaseTodoOp::Fixup => {
                    let Some(prev_sha) = last_commit.clone() else {
                        return Err(RebaseError::Backend(
                            "squash/fixup without preceding pick".into(),
                        ));
                    };
                    let Some(prev) = state.commits.get(&prev_sha).cloned() else {
                        return Err(RebaseError::Backend("previous commit missing".into()));
                    };
                    let Some(src) = state.commits.get(&entry.sha).cloned() else {
                        return Err(RebaseError::Backend(format!(
                            "unknown sha in rebase: {}",
                            entry.sha
                        )));
                    };
                    let mut merged_tree = prev.tree.clone();
                    for (path, content) in &src.tree {
                        merged_tree.insert(path.clone(), content.clone());
                    }
                    let combined_message = match entry.op {
                        RebaseTodoOp::Squash => {
                            let base = last_message.clone().unwrap_or_default();
                            format!("{}\n\n{}", base.trim_end(), src.message.trim_end())
                        },
                        _ => last_message.clone().unwrap_or(prev.message.clone()),
                    };

                    state.synth_counter += 1;
                    let new_sha = format!(
                        "rebased-{}-{}",
                        &entry.sha[..entry.sha.len().min(6)],
                        state.synth_counter
                    );
                    let new_commit = FakeCommit {
                        parent: prev.parent.clone(),
                        tree: merged_tree,
                        message: combined_message.clone(),
                        author_name: prev.author_name.clone(),
                        author_email: prev.author_email.clone(),
                        time: prev.time,
                    };
                    state.commits.insert(new_sha.clone(), new_commit);
                    state.commits.remove(&prev_sha);
                    current = new_sha.clone();
                    last_commit = Some(new_sha);
                    last_message = Some(combined_message);
                },
            }
        }

        state.head = Some(current.clone());
        state.applied_rebases.push(RecordedRebase {
            onto: onto.to_string(),
            todo: todo.to_vec(),
            new_head: current.clone(),
        });
        Ok(current)
    }

    fn commit_file_changes(&self, sha: &str) -> Vec<CommitFileChange> {
        let state = self.state.lock().unwrap();
        let Some(commit) = state.commits.get(sha) else {
            return Vec::new();
        };
        let parent_tree = commit
            .parent
            .as_ref()
            .and_then(|p| state.commits.get(p))
            .map(|p| p.tree.clone())
            .unwrap_or_default();
        let new_tree = &commit.tree;

        let mut paths: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
        for p in parent_tree.keys().chain(new_tree.keys()) {
            paths.insert(p.clone());
        }

        let mut out: Vec<CommitFileChange> = Vec::with_capacity(paths.len());
        for rel_path in paths {
            let base = parent_tree.get(&rel_path);
            let new = new_tree.get(&rel_path);
            let (kind, additions, deletions) = match (base, new) {
                (None, Some(n)) => {
                    let adds = n.lines().count() as u32;
                    (CommitFileChangeKind::Added, adds, 0)
                },
                (Some(b), None) => {
                    let dels = b.lines().count() as u32;
                    (CommitFileChangeKind::Deleted, 0, dels)
                },
                (Some(b), Some(n)) => {
                    if b == n {
                        continue;
                    }
                    let (adds, dels) = line_delta(b, n);
                    (CommitFileChangeKind::Modified, adds, dels)
                },
                (None, None) => continue,
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
}

/// Crude add/delete counts for the fake git host: the total distinct
/// lines on each side. Matches the shape of a unified diff well enough
/// for UI tests that only care about "something changed".
fn line_delta(base: &str, new: &str) -> (u32, u32) {
    let base_lines: std::collections::BTreeSet<&str> = base.lines().collect();
    let new_lines: std::collections::BTreeSet<&str> = new.lines().collect();
    let adds = new_lines.difference(&base_lines).count() as u32;
    let dels = base_lines.difference(&new_lines).count() as u32;
    (adds, dels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workdir() -> PathBuf {
        PathBuf::from("/work")
    }

    #[test]
    fn empty_host_discovers_nothing() {
        let host = FakeGit::new();
        assert!(host.discover(Path::new("/anywhere")).is_none());
    }

    #[test]
    fn discover_from_workdir() {
        let host = FakeGit::new();
        host.add_repo(workdir());
        let repo = host.discover(&workdir()).expect("repo");
        assert_eq!(repo.workdir().as_deref(), Some(workdir().as_path()));
    }

    #[test]
    fn discover_from_nested_path() {
        let host = FakeGit::new();
        host.add_repo(workdir());
        let repo = host
            .discover(Path::new("/work/src/a.rs"))
            .expect("repo via nested path");
        assert_eq!(repo.workdir().as_deref(), Some(workdir().as_path()));
    }

    #[test]
    fn discover_prefers_most_specific_workdir() {
        let host = FakeGit::new();
        host.add_repo("/outer");
        host.add_repo("/outer/inner");
        let repo = host.discover(Path::new("/outer/inner/sub/a.rs")).unwrap();
        assert_eq!(repo.workdir().as_deref(), Some(Path::new("/outer/inner")));
    }

    #[test]
    fn head_and_modified_round_trip() {
        let host = FakeGit::new();
        host.add_repo(workdir())
            .head_file("a.rs", "v1")
            .unstaged_file("a.rs", "v2");
        let repo = host.discover(&workdir()).unwrap();
        assert_eq!(
            repo.head_content(&workdir().join("a.rs")).as_deref(),
            Some("v1")
        );
        let changed = repo.changed_files();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].path, workdir().join("a.rs"));
        assert!(!changed[0].staged);
    }

    #[test]
    fn modified_helper_writes_head_and_working() {
        let host = FakeGit::new();
        host.add_repo(workdir()).modified("a.rs", "v1", "v2");
        let repo = host.discover(&workdir()).unwrap();
        assert_eq!(
            repo.head_content(&workdir().join("a.rs")).as_deref(),
            Some("v1")
        );
        assert_eq!(repo.changed_files().len(), 1);
    }

    #[test]
    fn with_fs_populates_working_tree() {
        use crate::host::fs::FsHost;

        let fs = FakeFs::new();
        let host = FakeGit::new();
        host.add_repo(workdir())
            .with_fs(&fs)
            .modified("a.rs", "v1", "v2")
            .modified("b.rs", "B1", "B2");

        let mut buf = Vec::new();
        fs.read(&workdir().join("a.rs"), &mut buf).unwrap();
        assert_eq!(buf, b"v2");
        buf.clear();
        fs.read(&workdir().join("b.rs"), &mut buf).unwrap();
        assert_eq!(buf, b"B2");
    }

    #[test]
    fn staged_and_unstaged_in_one_repo() {
        let host = FakeGit::new();
        host.add_repo(workdir())
            .head_file("a.rs", "v1")
            .staged_file("a.rs", "v2")
            .head_file("b.rs", "v1")
            .unstaged_file("b.rs", "v2");
        let repo = host.discover(&workdir()).unwrap();
        let changed = repo.changed_files();
        assert_eq!(changed.len(), 2);
        // Staged sorts first, matching LocalGit's ordering.
        assert!(changed[0].staged);
        assert!(changed[0].path.ends_with("a.rs"));
        assert!(!changed[1].staged);
        assert!(changed[1].path.ends_with("b.rs"));
    }

    #[test]
    fn added_file_has_no_head_content() {
        let host = FakeGit::new();
        host.add_repo(workdir()).added("new.rs", "body");
        let repo = host.discover(&workdir()).unwrap();
        assert!(repo.head_content(&workdir().join("new.rs")).is_none());
        let changed = repo.changed_files();
        assert_eq!(changed.len(), 1);
    }

    #[test]
    fn apply_to_index_records_patches() {
        let host = FakeGit::new();
        host.add_repo(workdir());
        let repo = host.discover(&workdir()).unwrap();
        repo.apply_to_index("--- a\n+++ b\n").unwrap();
        repo.apply_to_index("--- c\n+++ d\n").unwrap();
        assert_eq!(
            host.applied_patches(&workdir()),
            vec!["--- a\n+++ b\n".to_string(), "--- c\n+++ d\n".to_string()],
        );
    }

    #[test]
    fn applied_patches_empty_for_unknown_repo() {
        let host = FakeGit::new();
        assert!(host.applied_patches(Path::new("/none")).is_empty());
    }

    #[test]
    fn head_content_outside_workdir_is_none() {
        let host = FakeGit::new();
        host.add_repo(workdir()).head_file("a.rs", "v1");
        let repo = host.discover(&workdir()).unwrap();
        assert!(repo.head_content(Path::new("/elsewhere/a.rs")).is_none());
    }

    #[test]
    fn re_adding_file_replaces_entry() {
        let host = FakeGit::new();
        host.add_repo(workdir())
            .unstaged_file("a.rs", "v2")
            .staged_file("a.rs", "v3");
        let repo = host.discover(&workdir()).unwrap();
        let changed = repo.changed_files();
        assert_eq!(changed.len(), 1, "duplicate path not deduplicated");
        assert!(changed[0].staged);
    }

    #[test]
    fn fail_apply_with_returns_error_and_records_attempt() {
        let host = FakeGit::new();
        host.add_repo(workdir()).fail_apply_with("disk full");
        let repo = host.discover(&workdir()).unwrap();
        let err = repo
            .apply_to_index("--- a/x\n+++ b/x\n")
            .expect_err("must error");
        assert_eq!(err, GitApplyError::Backend("disk full".into()));
        assert_eq!(
            host.applied_patches(&workdir()),
            vec!["--- a/x\n+++ b/x\n".to_string()],
            "failing patches are still recorded for introspection"
        );
    }

    #[test]
    fn clear_apply_failure_restores_ok() {
        let host = FakeGit::new();
        host.add_repo(workdir()).fail_apply_with("nope");
        let repo = host.discover(&workdir()).unwrap();
        repo.apply_to_index("p1").unwrap_err();

        host.add_repo(workdir()).clear_apply_failure();
        repo.apply_to_index("p2")
            .expect("clear restores ok behavior");
    }

    #[test]
    fn applied_patches_by_path_keys_on_plus_plus_target() {
        let host = FakeGit::new();
        host.add_repo(workdir());
        let repo = host.discover(&workdir()).unwrap();
        repo.apply_to_index("+++ b/a.rs\n").unwrap();
        repo.apply_to_index("+++ b/sub/b.rs\n").unwrap();

        let by_path = host.applied_patches_by_path(&workdir());
        assert_eq!(by_path.len(), 2);
        assert_eq!(by_path[0].0, workdir().join("a.rs"));
        assert_eq!(by_path[1].0, workdir().join("sub/b.rs"));
    }

    #[test]
    fn commit_tree_returns_seeded_entries() {
        let host = FakeGit::new();
        host.add_repo(workdir())
            .commit("sha1", &[("a.rs", "A"), ("sub/b.rs", "B")]);
        let repo = host.discover(&workdir()).unwrap();
        let tree = repo.commit_tree("sha1").expect("tree");
        assert_eq!(tree.get(Path::new("a.rs")).map(String::as_str), Some("A"));
        assert_eq!(
            tree.get(Path::new("sub/b.rs")).map(String::as_str),
            Some("B")
        );
        assert_eq!(tree.len(), 2);
    }

    #[test]
    fn commit_tree_unknown_sha_is_none() {
        let host = FakeGit::new();
        host.add_repo(workdir());
        let repo = host.discover(&workdir()).unwrap();
        assert!(repo.commit_tree("nope").is_none());
    }

    #[test]
    fn parent_sha_returns_chain() {
        let host = FakeGit::new();
        host.add_repo(workdir())
            .commit("c1", &[("a.rs", "v1")])
            .commit_with_parent("c2", "c1", &[("a.rs", "v2")])
            .commit_with_parent("c3", "c2", &[("a.rs", "v3")]);
        let repo = host.discover(&workdir()).unwrap();
        assert_eq!(repo.parent_sha("c3").as_deref(), Some("c2"));
        assert_eq!(repo.parent_sha("c2").as_deref(), Some("c1"));
        assert!(repo.parent_sha("c1").is_none());
        assert!(repo.parent_sha("missing").is_none());
    }

    #[test]
    fn applied_patches_by_path_uses_base_for_pure_deletion() {
        let host = FakeGit::new();
        host.add_repo(workdir());
        let repo = host.discover(&workdir()).unwrap();
        repo.apply_to_index("--- a/gone.rs\n+++ /dev/null\n")
            .unwrap();
        let by_path = host.applied_patches_by_path(&workdir());
        assert_eq!(by_path.len(), 1);
        assert_eq!(by_path[0].0, workdir().join("gone.rs"));
    }
}

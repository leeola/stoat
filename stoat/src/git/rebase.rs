use crate::{
    fs::Fs,
    git::{provider::GitRepo, repository::CommitLogEntry, status::DiffPreviewData},
    stoat::KeyContext,
};
use gpui::Task;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebaseOperation {
    Pick,
    Reword,
    Edit,
    Squash,
    Fixup,
    Drop,
}

impl RebaseOperation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pick => "pick",
            Self::Reword => "reword",
            Self::Edit => "edit",
            Self::Squash => "squash",
            Self::Fixup => "fixup",
            Self::Drop => "drop",
        }
    }

    pub fn short(&self) -> &'static str {
        match self {
            Self::Pick => "p",
            Self::Reword => "r",
            Self::Edit => "e",
            Self::Squash => "s",
            Self::Fixup => "f",
            Self::Drop => "d",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pick" | "p" => Some(Self::Pick),
            "reword" | "r" => Some(Self::Reword),
            "edit" | "e" => Some(Self::Edit),
            "squash" | "s" => Some(Self::Squash),
            "fixup" | "f" => Some(Self::Fixup),
            "drop" | "d" => Some(Self::Drop),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RebaseCommit {
    pub oid: String,
    pub short_hash: String,
    pub author: String,
    pub date: String,
    pub message: String,
    pub operation: RebaseOperation,
}

impl RebaseCommit {
    pub fn from_log_entry(entry: CommitLogEntry) -> Self {
        Self {
            oid: entry.oid,
            short_hash: entry.short_hash,
            author: entry.author,
            date: format_relative_time(entry.timestamp),
            message: entry.message,
            operation: RebaseOperation::Pick,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RebasePhase {
    Planning,
    PausedConflict { step: usize, total: usize },
    PausedEdit { step: usize, total: usize },
    PausedReword { step: usize, total: usize },
}

#[derive(Clone, Debug)]
pub struct RebaseInProgress {
    pub head_name: String,
    pub onto: String,
    pub step: usize,
    pub total: usize,
    pub has_conflicts: bool,
    pub stopped_sha: Option<String>,
    pub done_commits: Vec<RebaseCommit>,
    pub pending_commits: Vec<RebaseCommit>,
}

pub struct RebaseState {
    pub phase: RebasePhase,
    pub commits: Vec<RebaseCommit>,
    pub selected: usize,
    pub base_ref: String,
    pub previous_mode: Option<String>,
    pub previous_key_context: Option<KeyContext>,
    pub preview: Option<DiffPreviewData>,
    pub preview_task: Option<Task<()>>,
    pub in_progress: Option<RebaseInProgress>,
    pub conflict_files: Vec<PathBuf>,
}

impl Default for RebaseState {
    fn default() -> Self {
        Self {
            phase: RebasePhase::Planning,
            commits: Vec::new(),
            selected: 0,
            base_ref: String::new(),
            previous_mode: None,
            previous_key_context: None,
            preview: None,
            preview_task: None,
            in_progress: None,
            conflict_files: Vec::new(),
        }
    }
}

/// Detect an in-progress rebase by reading `.git/rebase-merge/` or `.git/rebase-apply/`.
///
/// Uses the [`Fs`] abstraction for file reads and [`GitRepo::has_unmerged_paths`]
/// for conflict detection, making this testable with `FakeFs`/`FakeGitRepo`.
pub async fn detect_rebase_state(
    git_dir: &Path,
    fs: &dyn Fs,
    repo: &dyn GitRepo,
) -> Option<RebaseInProgress> {
    let rebase_merge = git_dir.join("rebase-merge");
    if fs.exists(&rebase_merge).await {
        return detect_rebase_merge(git_dir, fs, repo).await;
    }
    let rebase_apply = git_dir.join("rebase-apply");
    if fs.exists(&rebase_apply).await {
        return detect_rebase_apply(git_dir, fs, repo).await;
    }
    None
}

async fn detect_rebase_merge(
    git_dir: &Path,
    fs: &dyn Fs,
    repo: &dyn GitRepo,
) -> Option<RebaseInProgress> {
    let rebase_merge = git_dir.join("rebase-merge");

    let head_name = fs
        .read_to_string(&rebase_merge.join("head-name"))
        .await
        .ok()?
        .trim()
        .to_string();
    let onto = fs
        .read_to_string(&rebase_merge.join("onto"))
        .await
        .ok()?
        .trim()
        .to_string();
    let step: usize = fs
        .read_to_string(&rebase_merge.join("msgnum"))
        .await
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let total: usize = fs
        .read_to_string(&rebase_merge.join("end"))
        .await
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let stopped_sha = fs
        .read_to_string(&rebase_merge.join("stopped-sha"))
        .await
        .ok()
        .map(|s| s.trim().to_string());
    let has_conflicts = repo.has_unmerged_paths().await;

    let done_commits = fs
        .read_to_string(&rebase_merge.join("done"))
        .await
        .ok()
        .map(|s| parse_todo(&s))
        .unwrap_or_default();
    let pending_commits = fs
        .read_to_string(&rebase_merge.join("git-rebase-todo"))
        .await
        .ok()
        .map(|s| parse_todo(&s))
        .unwrap_or_default();

    Some(RebaseInProgress {
        head_name,
        onto,
        step,
        total,
        has_conflicts,
        stopped_sha,
        done_commits,
        pending_commits,
    })
}

async fn detect_rebase_apply(
    git_dir: &Path,
    fs: &dyn Fs,
    repo: &dyn GitRepo,
) -> Option<RebaseInProgress> {
    let rebase_apply = git_dir.join("rebase-apply");

    let head_name = fs
        .read_to_string(&rebase_apply.join("head-name"))
        .await
        .ok()?
        .trim()
        .to_string();
    let onto = fs
        .read_to_string(&rebase_apply.join("onto"))
        .await
        .ok()?
        .trim()
        .to_string();
    let step: usize = fs
        .read_to_string(&rebase_apply.join("next"))
        .await
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let total: usize = fs
        .read_to_string(&rebase_apply.join("last"))
        .await
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let stopped_sha = fs
        .read_to_string(&rebase_apply.join("original-commit"))
        .await
        .ok()
        .map(|s| s.trim().to_string());
    let has_conflicts = repo.has_unmerged_paths().await;

    Some(RebaseInProgress {
        head_name,
        onto,
        step,
        total,
        has_conflicts,
        stopped_sha,
        done_commits: Vec::new(),
        pending_commits: Vec::new(),
    })
}

/// Determine the [`RebasePhase`] from an in-progress rebase state.
///
/// Distinguishes reword (`.git/rebase-merge/amend` exists) from edit pauses.
pub async fn phase_from_in_progress(
    ip: &RebaseInProgress,
    git_dir: &Path,
    fs: &dyn Fs,
) -> RebasePhase {
    if ip.has_conflicts {
        return RebasePhase::PausedConflict {
            step: ip.step,
            total: ip.total,
        };
    }
    if fs.exists(&git_dir.join("rebase-merge/amend")).await
        || fs.exists(&git_dir.join("rebase-apply/amend")).await
    {
        return RebasePhase::PausedReword {
            step: ip.step,
            total: ip.total,
        };
    }
    RebasePhase::PausedEdit {
        step: ip.step,
        total: ip.total,
    }
}

/// Validate that a planned todo list is safe to execute.
///
/// Squash/fixup as the first commit has no target to fold into.
pub fn validate_todo(commits: &[RebaseCommit]) -> Result<(), String> {
    if let Some(first) = commits.first() {
        if matches!(
            first.operation,
            RebaseOperation::Squash | RebaseOperation::Fixup
        ) {
            return Err(format!(
                "Cannot {} the first commit",
                first.operation.as_str()
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub enum TodoEntry {
    Commit(RebaseCommit),
    RawLine(String),
}

/// Parse a git rebase-todo preserving non-commit lines (exec, break, label, etc.).
pub fn parse_todo_full(content: &str) -> Vec<TodoEntry> {
    let mut entries = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut parts = trimmed.splitn(3, ' ');
        let op_str = match parts.next() {
            Some(s) => s,
            None => continue,
        };
        if let Some(op) = RebaseOperation::parse(op_str) {
            let hash = parts.next().unwrap_or("").to_string();
            let message = parts.next().unwrap_or("").to_string();
            entries.push(TodoEntry::Commit(RebaseCommit {
                oid: hash.clone(),
                short_hash: hash,
                author: String::new(),
                date: String::new(),
                message,
                operation: op,
            }));
        } else {
            entries.push(TodoEntry::RawLine(line.to_string()));
        }
    }
    entries
}

/// Serialize a mixed todo list back to rebase-todo format.
pub fn format_todo_full(entries: &[TodoEntry]) -> String {
    let mut out = String::new();
    for entry in entries {
        match entry {
            TodoEntry::Commit(c) => {
                out.push_str(c.operation.as_str());
                out.push(' ');
                out.push_str(&c.short_hash);
                out.push(' ');
                out.push_str(&c.message);
                out.push('\n');
            },
            TodoEntry::RawLine(line) => {
                out.push_str(line);
                out.push('\n');
            },
        }
    }
    out
}

/// Serialize commits to git rebase-todo format.
pub fn format_todo(commits: &[RebaseCommit]) -> String {
    let mut out = String::new();
    for c in commits {
        out.push_str(c.operation.as_str());
        out.push(' ');
        out.push_str(&c.short_hash);
        out.push(' ');
        out.push_str(&c.message);
        out.push('\n');
    }
    out
}

/// Parse a git rebase-todo file into commits.
pub fn parse_todo(content: &str) -> Vec<RebaseCommit> {
    let mut commits = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(3, ' ');
        let op_str = match parts.next() {
            Some(s) => s,
            None => continue,
        };
        let op = match RebaseOperation::parse(op_str) {
            Some(op) => op,
            None => continue,
        };
        let hash = parts.next().unwrap_or("").to_string();
        let message = parts.next().unwrap_or("").to_string();

        commits.push(RebaseCommit {
            oid: hash.clone(),
            short_hash: hash,
            author: String::new(),
            date: String::new(),
            message,
            operation: op,
        });
    }
    commits
}

pub fn format_relative_time(timestamp: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let diff = now - timestamp;
    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        let mins = diff / 60;
        if mins == 1 {
            "1 minute ago".to_string()
        } else {
            format!("{mins} minutes ago")
        }
    } else if diff < 86400 {
        let hours = diff / 3600;
        if hours == 1 {
            "1 hour ago".to_string()
        } else {
            format!("{hours} hours ago")
        }
    } else {
        let days = diff / 86400;
        if days == 1 {
            "1 day ago".to_string()
        } else {
            format!("{days} days ago")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        fs::FakeFs,
        git::provider::{FakeGitProvider, GitProvider},
    };
    use std::{path::PathBuf, sync::Arc};

    fn setup_rebase_fs(git_dir: &Path, fs: &FakeFs) {
        fs.insert_file(
            git_dir.join("rebase-merge/head-name"),
            "refs/heads/feature\n",
        );
        fs.insert_file(git_dir.join("rebase-merge/onto"), "abc123def456\n");
        fs.insert_file(git_dir.join("rebase-merge/msgnum"), "2\n");
        fs.insert_file(git_dir.join("rebase-merge/end"), "5\n");
        fs.insert_file(git_dir.join("rebase-merge/stopped-sha"), "deadbeef\n");
    }

    fn make_commit(hash: &str, msg: &str, op: RebaseOperation) -> RebaseCommit {
        RebaseCommit {
            oid: hash.into(),
            short_hash: hash.into(),
            author: String::new(),
            date: String::new(),
            message: msg.into(),
            operation: op,
        }
    }

    #[test]
    fn detect_no_rebase_dir() {
        smol::block_on(async {
            let fs = Arc::new(FakeFs::new());
            let provider = FakeGitProvider::new(fs.clone());
            let workdir = PathBuf::from("/fake/repo");
            provider.set_exists(true);
            provider.set_workdir(workdir.clone());
            let repo = provider.open(&workdir).await.unwrap();
            let git_dir = workdir.join(".git");

            assert!(detect_rebase_state(&git_dir, &*fs, &*repo).await.is_none());
        });
    }

    #[test]
    fn detect_with_rebase_dir() {
        smol::block_on(async {
            let fs = Arc::new(FakeFs::new());
            let provider = FakeGitProvider::new(fs.clone());
            let workdir = PathBuf::from("/fake/repo");
            provider.set_exists(true);
            provider.set_workdir(workdir.clone());
            let repo = provider.open(&workdir).await.unwrap();
            let git_dir = workdir.join(".git");

            setup_rebase_fs(&git_dir, &fs);

            let state = detect_rebase_state(&git_dir, &*fs, &*repo).await.unwrap();
            assert_eq!(state.head_name, "refs/heads/feature");
            assert_eq!(state.onto, "abc123def456");
            assert_eq!(state.step, 2);
            assert_eq!(state.total, 5);
            assert!(!state.has_conflicts);
            assert_eq!(state.stopped_sha.as_deref(), Some("deadbeef"));
        });
    }

    #[test]
    fn detect_missing_required_files() {
        smol::block_on(async {
            let fs = Arc::new(FakeFs::new());
            let provider = FakeGitProvider::new(fs.clone());
            let workdir = PathBuf::from("/fake/repo");
            provider.set_exists(true);
            provider.set_workdir(workdir.clone());
            let repo = provider.open(&workdir).await.unwrap();
            let git_dir = workdir.join(".git");

            // Only head-name, missing other required files
            fs.insert_file(
                git_dir.join("rebase-merge/head-name"),
                "refs/heads/feature\n",
            );

            assert!(detect_rebase_state(&git_dir, &*fs, &*repo).await.is_none());
        });
    }

    #[test]
    fn phase_from_in_progress_reword() {
        smol::block_on(async {
            let fs = Arc::new(FakeFs::new());
            let git_dir = PathBuf::from("/fake/repo/.git");
            fs.insert_file(git_dir.join("rebase-merge/amend"), "");

            let ip = RebaseInProgress {
                head_name: "refs/heads/feature".into(),
                onto: "abc123".into(),
                step: 3,
                total: 5,
                has_conflicts: false,
                stopped_sha: Some("deadbeef".into()),
                done_commits: Vec::new(),
                pending_commits: Vec::new(),
            };

            assert_eq!(
                phase_from_in_progress(&ip, &git_dir, &*fs).await,
                RebasePhase::PausedReword { step: 3, total: 5 }
            );
        });
    }

    #[test]
    fn phase_from_in_progress_conflict() {
        smol::block_on(async {
            let fs = Arc::new(FakeFs::new());
            let git_dir = PathBuf::from("/fake/repo/.git");

            let ip = RebaseInProgress {
                head_name: "refs/heads/feature".into(),
                onto: "abc123".into(),
                step: 1,
                total: 3,
                has_conflicts: true,
                stopped_sha: None,
                done_commits: Vec::new(),
                pending_commits: Vec::new(),
            };

            assert_eq!(
                phase_from_in_progress(&ip, &git_dir, &*fs).await,
                RebasePhase::PausedConflict { step: 1, total: 3 }
            );
        });
    }

    #[test]
    fn phase_from_in_progress_edit() {
        smol::block_on(async {
            let fs = Arc::new(FakeFs::new());
            let git_dir = PathBuf::from("/fake/repo/.git");

            let ip = RebaseInProgress {
                head_name: "refs/heads/feature".into(),
                onto: "abc123".into(),
                step: 2,
                total: 4,
                has_conflicts: false,
                stopped_sha: Some("cafe1234".into()),
                done_commits: Vec::new(),
                pending_commits: Vec::new(),
            };

            assert_eq!(
                phase_from_in_progress(&ip, &git_dir, &*fs).await,
                RebasePhase::PausedEdit { step: 2, total: 4 }
            );
        });
    }

    #[test]
    fn from_log_entry_conversion() {
        let entry = CommitLogEntry {
            oid: "abc1234567890".into(),
            short_hash: "abc1234".into(),
            author: "Alice".into(),
            timestamp: 0,
            message: "Add feature".into(),
            parent_oids: vec![],
        };
        let commit = RebaseCommit::from_log_entry(entry);
        assert_eq!(commit.oid, "abc1234567890");
        assert_eq!(commit.short_hash, "abc1234");
        assert_eq!(commit.author, "Alice");
        assert_eq!(commit.message, "Add feature");
        assert_eq!(commit.operation, RebaseOperation::Pick);
    }

    #[test]
    fn format_and_parse_todo_roundtrip() {
        let commits = vec![
            RebaseCommit {
                oid: "abc1234".into(),
                short_hash: "abc1234".into(),
                author: "Alice".into(),
                date: "1 day ago".into(),
                message: "Add feature X".into(),
                operation: RebaseOperation::Pick,
            },
            RebaseCommit {
                oid: "def5678".into(),
                short_hash: "def5678".into(),
                author: "Bob".into(),
                date: "2 days ago".into(),
                message: "Fix bug Y".into(),
                operation: RebaseOperation::Squash,
            },
            RebaseCommit {
                oid: "ghi9012".into(),
                short_hash: "ghi9012".into(),
                author: "Carol".into(),
                date: "3 days ago".into(),
                message: "Refactor Z".into(),
                operation: RebaseOperation::Drop,
            },
        ];

        let todo = format_todo(&commits);
        assert_eq!(
            todo,
            "pick abc1234 Add feature X\nsquash def5678 Fix bug Y\ndrop ghi9012 Refactor Z\n"
        );

        let parsed = parse_todo(&todo);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].operation, RebaseOperation::Pick);
        assert_eq!(parsed[0].short_hash, "abc1234");
        assert_eq!(parsed[0].message, "Add feature X");
        assert_eq!(parsed[1].operation, RebaseOperation::Squash);
        assert_eq!(parsed[2].operation, RebaseOperation::Drop);
    }

    #[test]
    fn parse_todo_skips_comments_and_empty() {
        let content = "# This is a comment\n\npick abc123 Do something\n# Another comment\n";
        let parsed = parse_todo(content);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].short_hash, "abc123");
    }

    #[test]
    fn operation_roundtrip() {
        for op in [
            RebaseOperation::Pick,
            RebaseOperation::Reword,
            RebaseOperation::Edit,
            RebaseOperation::Squash,
            RebaseOperation::Fixup,
            RebaseOperation::Drop,
        ] {
            assert_eq!(RebaseOperation::parse(op.as_str()), Some(op));
            assert_eq!(RebaseOperation::parse(op.short()), Some(op));
        }
    }

    #[test]
    fn parse_todo_full_preserves_exec() {
        let content =
            "pick abc123 First commit\nexec make test\npick def456 Second commit\nbreak\n";
        let entries = parse_todo_full(content);
        assert_eq!(entries.len(), 4);
        assert!(matches!(&entries[0], TodoEntry::Commit(c) if c.short_hash == "abc123"));
        assert!(matches!(&entries[1], TodoEntry::RawLine(l) if l == "exec make test"));
        assert!(matches!(&entries[2], TodoEntry::Commit(c) if c.short_hash == "def456"));
        assert!(matches!(&entries[3], TodoEntry::RawLine(l) if l == "break"));
    }

    #[test]
    fn format_todo_full_roundtrip() {
        let input = "pick abc123 First commit\nexec make test\npick def456 Second commit\nbreak\n";
        let entries = parse_todo_full(input);
        let output = format_todo_full(&entries);
        assert_eq!(output, input);
    }

    #[test]
    fn parse_todo_full_preserves_label_reset() {
        let content = "pick abc123 msg\nlabel onto\nreset onto\nmerge -C def456 branch\n";
        let entries = parse_todo_full(content);
        assert_eq!(entries.len(), 4);
        assert!(matches!(&entries[0], TodoEntry::Commit(_)));
        assert!(matches!(&entries[1], TodoEntry::RawLine(l) if l == "label onto"));
        assert!(matches!(&entries[2], TodoEntry::RawLine(l) if l == "reset onto"));
        assert!(matches!(&entries[3], TodoEntry::RawLine(l) if l == "merge -C def456 branch"));
    }

    #[test]
    fn validate_todo_squash_first() {
        let commits = vec![RebaseCommit {
            oid: "a".into(),
            short_hash: "a".into(),
            author: String::new(),
            date: String::new(),
            message: "msg".into(),
            operation: RebaseOperation::Squash,
        }];
        let err = validate_todo(&commits).unwrap_err();
        assert!(err.contains("squash"), "{err}");
    }

    #[test]
    fn validate_todo_fixup_first() {
        let commits = vec![RebaseCommit {
            oid: "a".into(),
            short_hash: "a".into(),
            author: String::new(),
            date: String::new(),
            message: "msg".into(),
            operation: RebaseOperation::Fixup,
        }];
        let err = validate_todo(&commits).unwrap_err();
        assert!(err.contains("fixup"), "{err}");
    }

    #[test]
    fn validate_todo_valid() {
        let ops = [
            RebaseOperation::Pick,
            RebaseOperation::Squash,
            RebaseOperation::Fixup,
            RebaseOperation::Drop,
        ];
        let commits: Vec<_> = ops
            .iter()
            .map(|&op| RebaseCommit {
                oid: "a".into(),
                short_hash: "a".into(),
                author: String::new(),
                date: String::new(),
                message: "msg".into(),
                operation: op,
            })
            .collect();
        assert!(validate_todo(&commits).is_ok());
    }

    #[test]
    fn validate_todo_empty() {
        assert!(validate_todo(&[]).is_ok());
    }

    #[test]
    fn detect_rebase_apply() {
        smol::block_on(async {
            let fs = Arc::new(FakeFs::new());
            let provider = FakeGitProvider::new(fs.clone());
            let workdir = PathBuf::from("/fake/repo");
            provider.set_exists(true);
            provider.set_workdir(workdir.clone());
            let repo = provider.open(&workdir).await.unwrap();
            let git_dir = workdir.join(".git");

            fs.insert_file(
                git_dir.join("rebase-apply/head-name"),
                "refs/heads/feature\n",
            );
            fs.insert_file(git_dir.join("rebase-apply/onto"), "abc123\n");
            fs.insert_file(git_dir.join("rebase-apply/next"), "3\n");
            fs.insert_file(git_dir.join("rebase-apply/last"), "7\n");
            fs.insert_file(git_dir.join("rebase-apply/original-commit"), "cafe1234\n");

            let state = detect_rebase_state(&git_dir, &*fs, &*repo).await.unwrap();
            assert_eq!(state.head_name, "refs/heads/feature");
            assert_eq!(state.onto, "abc123");
            assert_eq!(state.step, 3);
            assert_eq!(state.total, 7);
            assert_eq!(state.stopped_sha.as_deref(), Some("cafe1234"));
            assert!(state.done_commits.is_empty());
            assert!(state.pending_commits.is_empty());
        });
    }

    #[test]
    fn detect_with_done_and_pending() {
        smol::block_on(async {
            let fs = Arc::new(FakeFs::new());
            let provider = FakeGitProvider::new(fs.clone());
            let workdir = PathBuf::from("/fake/repo");
            provider.set_exists(true);
            provider.set_workdir(workdir.clone());
            let repo = provider.open(&workdir).await.unwrap();
            let git_dir = workdir.join(".git");

            setup_rebase_fs(&git_dir, &fs);
            fs.insert_file(
                git_dir.join("rebase-merge/done"),
                "pick aaa1111 Done commit\n",
            );
            fs.insert_file(
                git_dir.join("rebase-merge/git-rebase-todo"),
                "pick bbb2222 Pending one\npick ccc3333 Pending two\n",
            );

            let state = detect_rebase_state(&git_dir, &*fs, &*repo).await.unwrap();
            assert_eq!(state.done_commits.len(), 1);
            assert_eq!(state.done_commits[0].short_hash, "aaa1111");
            assert_eq!(state.pending_commits.len(), 2);
            assert_eq!(state.pending_commits[0].short_hash, "bbb2222");
            assert_eq!(state.pending_commits[1].short_hash, "ccc3333");
        });
    }

    #[test]
    fn reorder_swap_adjacent() {
        let mut commits = vec![
            make_commit("aaa", "A", RebaseOperation::Pick),
            make_commit("bbb", "B", RebaseOperation::Pick),
            make_commit("ccc", "C", RebaseOperation::Pick),
        ];
        commits.swap(0, 1);
        let todo = format_todo(&commits);
        assert_eq!(todo, "pick bbb B\npick aaa A\npick ccc C\n");
    }

    #[test]
    fn reorder_move_to_end() {
        let mut commits = vec![
            make_commit("aaa", "A", RebaseOperation::Pick),
            make_commit("bbb", "B", RebaseOperation::Pick),
            make_commit("ccc", "C", RebaseOperation::Pick),
            make_commit("ddd", "D", RebaseOperation::Pick),
        ];
        commits.swap(0, 1);
        commits.swap(1, 2);
        commits.swap(2, 3);
        assert_eq!(
            format_todo(&commits),
            "pick bbb B\npick ccc C\npick ddd D\npick aaa A\n"
        );
    }

    #[test]
    fn reorder_reverse_entire_list() {
        let mut commits = vec![
            make_commit("aaa", "A", RebaseOperation::Pick),
            make_commit("bbb", "B", RebaseOperation::Pick),
            make_commit("ccc", "C", RebaseOperation::Pick),
            make_commit("ddd", "D", RebaseOperation::Pick),
        ];
        commits.reverse();
        assert_eq!(
            format_todo(&commits),
            "pick ddd D\npick ccc C\npick bbb B\npick aaa A\n"
        );
    }

    #[test]
    fn reorder_single_commit_noop() {
        let mut commits = vec![make_commit("aaa", "A", RebaseOperation::Pick)];
        if commits.len() > 1 {
            commits.swap(0, 1);
        }
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].short_hash, "aaa");
    }

    #[test]
    fn drop_middle_commit() {
        let commits = vec![
            make_commit("aaa", "A", RebaseOperation::Pick),
            make_commit("bbb", "B", RebaseOperation::Drop),
            make_commit("ccc", "C", RebaseOperation::Pick),
        ];
        let todo = format_todo(&commits);
        assert_eq!(todo, "pick aaa A\ndrop bbb B\npick ccc C\n");
        let parsed = parse_todo(&todo);
        assert_eq!(parsed[1].operation, RebaseOperation::Drop);
    }

    #[test]
    fn drop_all_but_one() {
        let commits = vec![
            make_commit("aaa", "A", RebaseOperation::Pick),
            make_commit("bbb", "B", RebaseOperation::Drop),
            make_commit("ccc", "C", RebaseOperation::Drop),
            make_commit("ddd", "D", RebaseOperation::Drop),
        ];
        assert!(validate_todo(&commits).is_ok());
        let todo = format_todo(&commits);
        assert_eq!(todo.matches("drop").count(), 3);
        assert_eq!(todo.matches("pick").count(), 1);
    }

    #[test]
    fn drop_all_commits() {
        let commits = vec![
            make_commit("aaa", "A", RebaseOperation::Drop),
            make_commit("bbb", "B", RebaseOperation::Drop),
        ];
        assert!(validate_todo(&commits).is_ok());
    }

    #[test]
    fn squash_second_into_first() {
        let commits = vec![
            make_commit("aaa", "A", RebaseOperation::Pick),
            make_commit("bbb", "B", RebaseOperation::Squash),
        ];
        assert!(validate_todo(&commits).is_ok());
        assert_eq!(format_todo(&commits), "pick aaa A\nsquash bbb B\n");
    }

    #[test]
    fn squash_chain() {
        let commits = vec![
            make_commit("aaa", "A", RebaseOperation::Pick),
            make_commit("bbb", "B", RebaseOperation::Squash),
            make_commit("ccc", "C", RebaseOperation::Squash),
        ];
        assert!(validate_todo(&commits).is_ok());
        assert_eq!(format_todo(&commits).matches("squash").count(), 2);
    }

    #[test]
    fn squash_with_gap() {
        let commits = vec![
            make_commit("aaa", "A", RebaseOperation::Pick),
            make_commit("bbb", "B", RebaseOperation::Pick),
            make_commit("ccc", "C", RebaseOperation::Squash),
        ];
        assert!(validate_todo(&commits).is_ok());
        assert_eq!(
            format_todo(&commits),
            "pick aaa A\npick bbb B\nsquash ccc C\n"
        );
    }

    #[test]
    fn fixup_second_into_first() {
        let commits = vec![
            make_commit("aaa", "A", RebaseOperation::Pick),
            make_commit("bbb", "B", RebaseOperation::Fixup),
        ];
        assert!(validate_todo(&commits).is_ok());
        assert_eq!(format_todo(&commits), "pick aaa A\nfixup bbb B\n");
    }

    #[test]
    fn fixup_chain() {
        let commits = vec![
            make_commit("aaa", "A", RebaseOperation::Pick),
            make_commit("bbb", "B", RebaseOperation::Fixup),
            make_commit("ccc", "C", RebaseOperation::Fixup),
        ];
        assert!(validate_todo(&commits).is_ok());
        assert_eq!(format_todo(&commits).matches("fixup").count(), 2);
    }

    #[test]
    fn mixed_all_operations() {
        let commits = vec![
            make_commit("aaa", "A", RebaseOperation::Pick),
            make_commit("bbb", "B", RebaseOperation::Reword),
            make_commit("ccc", "C", RebaseOperation::Edit),
            make_commit("ddd", "D", RebaseOperation::Squash),
            make_commit("eee", "E", RebaseOperation::Fixup),
            make_commit("fff", "F", RebaseOperation::Drop),
        ];
        assert!(validate_todo(&commits).is_ok());
        let todo = format_todo(&commits);
        let parsed = parse_todo(&todo);
        assert_eq!(parsed.len(), 6);
        assert_eq!(parsed[0].operation, RebaseOperation::Pick);
        assert_eq!(parsed[1].operation, RebaseOperation::Reword);
        assert_eq!(parsed[2].operation, RebaseOperation::Edit);
        assert_eq!(parsed[3].operation, RebaseOperation::Squash);
        assert_eq!(parsed[4].operation, RebaseOperation::Fixup);
        assert_eq!(parsed[5].operation, RebaseOperation::Drop);
    }

    #[test]
    fn reorder_then_squash() {
        let mut commits = vec![
            make_commit("aaa", "A", RebaseOperation::Pick),
            make_commit("bbb", "B", RebaseOperation::Pick),
            make_commit("ccc", "C", RebaseOperation::Pick),
        ];
        commits.swap(0, 1);
        commits[0].operation = RebaseOperation::Squash;
        assert!(validate_todo(&commits).is_err());
    }

    #[test]
    fn reorder_breaks_squash_chain() {
        let mut commits = vec![
            make_commit("aaa", "A", RebaseOperation::Pick),
            make_commit("bbb", "B", RebaseOperation::Squash),
            make_commit("ccc", "C", RebaseOperation::Pick),
        ];
        commits.swap(1, 2);
        // [pick A, pick C, squash B] -- still valid (B squashes into C)
        assert_eq!(commits[0].short_hash, "aaa");
        assert_eq!(commits[1].short_hash, "ccc");
        assert_eq!(commits[2].short_hash, "bbb");
        assert!(validate_todo(&commits).is_ok());
    }

    #[test]
    fn reorder_creates_valid_from_invalid() {
        let mut commits = vec![
            make_commit("aaa", "A", RebaseOperation::Squash),
            make_commit("bbb", "B", RebaseOperation::Pick),
        ];
        assert!(validate_todo(&commits).is_err());
        commits.swap(0, 1);
        assert!(validate_todo(&commits).is_ok());
    }

    #[test]
    fn drop_then_squash_adjacent() {
        let commits = vec![
            make_commit("aaa", "A", RebaseOperation::Pick),
            make_commit("bbb", "B", RebaseOperation::Drop),
            make_commit("ccc", "C", RebaseOperation::Squash),
        ];
        assert!(validate_todo(&commits).is_ok());
    }

    #[test]
    fn fixup_after_drop_chain() {
        let commits = vec![
            make_commit("aaa", "A", RebaseOperation::Pick),
            make_commit("bbb", "B", RebaseOperation::Drop),
            make_commit("ccc", "C", RebaseOperation::Drop),
            make_commit("ddd", "D", RebaseOperation::Fixup),
        ];
        assert!(validate_todo(&commits).is_ok());
    }

    #[test]
    fn validate_reword_first() {
        let commits = vec![
            make_commit("aaa", "A", RebaseOperation::Reword),
            make_commit("bbb", "B", RebaseOperation::Pick),
        ];
        assert!(validate_todo(&commits).is_ok());
    }

    #[test]
    fn validate_edit_first() {
        let commits = vec![
            make_commit("aaa", "A", RebaseOperation::Edit),
            make_commit("bbb", "B", RebaseOperation::Pick),
        ];
        assert!(validate_todo(&commits).is_ok());
    }

    #[test]
    fn validate_drop_first() {
        let commits = vec![
            make_commit("aaa", "A", RebaseOperation::Drop),
            make_commit("bbb", "B", RebaseOperation::Pick),
        ];
        assert!(validate_todo(&commits).is_ok());
    }

    #[test]
    fn validate_single_squash() {
        let commits = vec![make_commit("aaa", "A", RebaseOperation::Squash)];
        assert!(validate_todo(&commits).is_err());
    }

    #[test]
    fn validate_single_fixup() {
        let commits = vec![make_commit("aaa", "A", RebaseOperation::Fixup)];
        assert!(validate_todo(&commits).is_err());
    }

    #[test]
    fn parse_todo_short_form_operations() {
        let content =
            "p aaa First\nr bbb Second\ne ccc Third\ns ddd Fourth\nf eee Fifth\nd fff Sixth\n";
        let parsed = parse_todo(content);
        assert_eq!(parsed.len(), 6);
        assert_eq!(parsed[0].operation, RebaseOperation::Pick);
        assert_eq!(parsed[1].operation, RebaseOperation::Reword);
        assert_eq!(parsed[2].operation, RebaseOperation::Edit);
        assert_eq!(parsed[3].operation, RebaseOperation::Squash);
        assert_eq!(parsed[4].operation, RebaseOperation::Fixup);
        assert_eq!(parsed[5].operation, RebaseOperation::Drop);
    }

    #[test]
    fn parse_todo_message_with_spaces() {
        let content = "pick abc123 This is a commit with many spaces\n";
        let parsed = parse_todo(content);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].message, "This is a commit with many spaces");
    }

    #[test]
    fn parse_todo_unknown_operation_skipped() {
        let content = "pick aaa Valid\nunknownop bbb Invalid\npick ccc Also valid\n";
        let parsed = parse_todo(content);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].short_hash, "aaa");
        assert_eq!(parsed[1].short_hash, "ccc");
    }

    #[test]
    fn format_parse_roundtrip_all_operations() {
        let ops = [
            RebaseOperation::Pick,
            RebaseOperation::Reword,
            RebaseOperation::Edit,
            RebaseOperation::Squash,
            RebaseOperation::Fixup,
            RebaseOperation::Drop,
        ];
        let commits: Vec<_> = ops
            .iter()
            .enumerate()
            .map(|(i, &op)| make_commit(&format!("hash{i}"), &format!("msg {i}"), op))
            .collect();
        let todo = format_todo(&commits);
        let parsed = parse_todo(&todo);
        assert_eq!(parsed.len(), commits.len());
        for (original, roundtripped) in commits.iter().zip(parsed.iter()) {
            assert_eq!(original.operation, roundtripped.operation);
            assert_eq!(original.short_hash, roundtripped.short_hash);
            assert_eq!(original.message, roundtripped.message);
        }
    }
}

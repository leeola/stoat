use crate::{
    fs::Fs,
    git::{provider::GitRepo, repository::CommitLogEntry, status::DiffPreviewData},
    stoat::KeyContext,
};
use gpui::Task;
use std::path::Path;

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
        }
    }
}

/// Detect an in-progress rebase by reading `.git/rebase-merge/` files.
///
/// Uses the [`Fs`] abstraction for file reads and [`GitRepo::has_unmerged_paths`]
/// for conflict detection, making this testable with `FakeFs`/`FakeGitRepo`.
pub fn detect_rebase_state(
    git_dir: &Path,
    fs: &dyn Fs,
    repo: &dyn GitRepo,
) -> Option<RebaseInProgress> {
    let rebase_merge = git_dir.join("rebase-merge");
    if !fs.exists(&rebase_merge) {
        return None;
    }

    let head_name = fs
        .read_to_string(&rebase_merge.join("head-name"))
        .ok()?
        .trim()
        .to_string();
    let onto = fs
        .read_to_string(&rebase_merge.join("onto"))
        .ok()?
        .trim()
        .to_string();
    let step: usize = fs
        .read_to_string(&rebase_merge.join("msgnum"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let total: usize = fs
        .read_to_string(&rebase_merge.join("end"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let stopped_sha = fs
        .read_to_string(&rebase_merge.join("stopped-sha"))
        .ok()
        .map(|s| s.trim().to_string());
    let has_conflicts = repo.has_unmerged_paths();

    Some(RebaseInProgress {
        head_name,
        onto,
        step,
        total,
        has_conflicts,
        stopped_sha,
    })
}

/// Determine the [`RebasePhase`] from an in-progress rebase state.
///
/// Distinguishes reword (`.git/rebase-merge/amend` exists) from edit pauses.
pub fn phase_from_in_progress(ip: &RebaseInProgress, git_dir: &Path, fs: &dyn Fs) -> RebasePhase {
    if ip.has_conflicts {
        return RebasePhase::PausedConflict {
            step: ip.step,
            total: ip.total,
        };
    }
    if fs.exists(&git_dir.join("rebase-merge/amend")) {
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

fn format_relative_time(timestamp: i64) -> String {
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

    #[test]
    fn detect_no_rebase_dir() {
        let fs = Arc::new(FakeFs::new());
        let provider = FakeGitProvider::new(fs.clone());
        let workdir = PathBuf::from("/fake/repo");
        provider.set_exists(true);
        provider.set_workdir(workdir.clone());
        let repo = provider.open(&workdir).unwrap();
        let git_dir = workdir.join(".git");

        assert!(detect_rebase_state(&git_dir, &*fs, &*repo).is_none());
    }

    #[test]
    fn detect_with_rebase_dir() {
        let fs = Arc::new(FakeFs::new());
        let provider = FakeGitProvider::new(fs.clone());
        let workdir = PathBuf::from("/fake/repo");
        provider.set_exists(true);
        provider.set_workdir(workdir.clone());
        let repo = provider.open(&workdir).unwrap();
        let git_dir = workdir.join(".git");

        setup_rebase_fs(&git_dir, &fs);

        let state = detect_rebase_state(&git_dir, &*fs, &*repo).unwrap();
        assert_eq!(state.head_name, "refs/heads/feature");
        assert_eq!(state.onto, "abc123def456");
        assert_eq!(state.step, 2);
        assert_eq!(state.total, 5);
        assert!(!state.has_conflicts);
        assert_eq!(state.stopped_sha.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn detect_missing_required_files() {
        let fs = Arc::new(FakeFs::new());
        let provider = FakeGitProvider::new(fs.clone());
        let workdir = PathBuf::from("/fake/repo");
        provider.set_exists(true);
        provider.set_workdir(workdir.clone());
        let repo = provider.open(&workdir).unwrap();
        let git_dir = workdir.join(".git");

        // Only head-name, missing other required files
        fs.insert_file(
            git_dir.join("rebase-merge/head-name"),
            "refs/heads/feature\n",
        );

        assert!(detect_rebase_state(&git_dir, &*fs, &*repo).is_none());
    }

    #[test]
    fn phase_from_in_progress_reword() {
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
        };

        assert_eq!(
            phase_from_in_progress(&ip, &git_dir, &*fs),
            RebasePhase::PausedReword { step: 3, total: 5 }
        );
    }

    #[test]
    fn phase_from_in_progress_conflict() {
        let fs = Arc::new(FakeFs::new());
        let git_dir = PathBuf::from("/fake/repo/.git");

        let ip = RebaseInProgress {
            head_name: "refs/heads/feature".into(),
            onto: "abc123".into(),
            step: 1,
            total: 3,
            has_conflicts: true,
            stopped_sha: None,
        };

        assert_eq!(
            phase_from_in_progress(&ip, &git_dir, &*fs),
            RebasePhase::PausedConflict { step: 1, total: 3 }
        );
    }

    #[test]
    fn phase_from_in_progress_edit() {
        let fs = Arc::new(FakeFs::new());
        let git_dir = PathBuf::from("/fake/repo/.git");

        let ip = RebaseInProgress {
            head_name: "refs/heads/feature".into(),
            onto: "abc123".into(),
            step: 2,
            total: 4,
            has_conflicts: false,
            stopped_sha: Some("cafe1234".into()),
        };

        assert_eq!(
            phase_from_in_progress(&ip, &git_dir, &*fs),
            RebasePhase::PausedEdit { step: 2, total: 4 }
        );
    }

    #[test]
    fn from_log_entry_conversion() {
        let entry = CommitLogEntry {
            oid: "abc1234567890".into(),
            short_hash: "abc1234".into(),
            author: "Alice".into(),
            timestamp: 0,
            message: "Add feature".into(),
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
}

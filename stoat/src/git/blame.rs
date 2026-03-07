use crate::git::repository::{GitError, Repository};
use std::{collections::HashMap, path::Path};

#[derive(Clone)]
pub struct BlameEntry {
    pub full_oid: String,
    pub short_hash: String,
    pub author_name: String,
    pub timestamp: i64,
    pub date_display: String,
    pub summary: String,
    pub message: String,
}

#[derive(Clone)]
pub struct BlameData {
    pub entries: Vec<BlameEntry>,
    pub line_to_entry: Vec<usize>,
}

pub struct BlameState {
    pub active: bool,
    pub data: Option<BlameData>,
    pub show_author: bool,
    pub show_date: bool,
}

impl Default for BlameState {
    fn default() -> Self {
        Self {
            active: false,
            data: None,
            show_author: false,
            show_date: false,
        }
    }
}

pub fn blame_file(repo: &Repository, path: &Path) -> Result<BlameData, GitError> {
    let inner = repo.inner();
    let workdir = repo.workdir();

    let relative_path = if path.is_absolute() {
        let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let wd = workdir
            .canonicalize()
            .unwrap_or_else(|_| workdir.to_path_buf());
        abs.strip_prefix(&wd)
            .map(|p| p.to_path_buf())
            .map_err(|_| {
                GitError::GitOperationFailed(format!("Path {abs:?} is not in repository {wd:?}"))
            })?
    } else {
        path.to_path_buf()
    };

    let blame = inner
        .blame_file(&relative_path, None)
        .map_err(|e| GitError::GitOperationFailed(format!("Blame failed: {e}")))?;

    let mut entry_map: HashMap<git2::Oid, usize> = HashMap::new();
    let mut entries: Vec<BlameEntry> = Vec::new();
    let mut line_to_entry: Vec<usize> = Vec::new();

    for hunk_idx in 0..blame.len() {
        let hunk = blame
            .get_index(hunk_idx)
            .ok_or_else(|| GitError::GitOperationFailed("Blame hunk index out of range".into()))?;

        let oid = hunk.final_commit_id();
        let lines_in_hunk = hunk.lines_in_hunk();

        let entry_idx = if let Some(&idx) = entry_map.get(&oid) {
            idx
        } else {
            let sig = hunk.final_signature();
            let author_name = String::from_utf8_lossy(sig.name_bytes()).into_owned();
            let timestamp = sig.when().seconds();

            let date_display = {
                let secs = timestamp;
                let days = secs / 86400 + 719468;
                let era = if days >= 0 { days } else { days - 146096 } / 146097;
                let doe = (days - era * 146097) as u32;
                let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
                let y = yoe as i64 + era * 400;
                let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
                let mp = (5 * doy + 2) / 153;
                let d = doy - (153 * mp + 2) / 5 + 1;
                let m = if mp < 10 { mp + 3 } else { mp - 9 };
                let y = if m <= 2 { y + 1 } else { y };
                format!("{y:04}-{m:02}-{d:02}")
            };

            let (summary, message) = match inner.find_commit(oid) {
                Ok(commit) => {
                    let msg = commit.message().unwrap_or("").to_string();
                    let sum = msg.lines().next().unwrap_or("").to_string();
                    (sum, msg)
                },
                Err(_) => (String::new(), String::new()),
            };

            let full_oid = format!("{}", oid);
            let short_hash = format!("{:.8}", oid);

            let idx = entries.len();
            entries.push(BlameEntry {
                full_oid,
                short_hash,
                author_name,
                timestamp,
                date_display,
                summary,
                message,
            });
            entry_map.insert(oid, idx);
            idx
        };

        for _ in 0..lines_in_hunk {
            line_to_entry.push(entry_idx);
        }
    }

    Ok(BlameData {
        entries,
        line_to_entry,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        fs::FakeFs,
        git::provider::{FakeGitProvider, GitProvider},
    };
    use std::{path::PathBuf, sync::Arc};

    #[test]
    fn blame_single_commit() {
        let fs = Arc::new(FakeFs::new());
        let provider = FakeGitProvider::new(fs);
        let workdir = PathBuf::from("/fake/repo");
        provider.set_exists(true);
        provider.set_workdir(workdir.clone());

        let file = workdir.join("test.txt");
        let data = BlameData {
            entries: vec![BlameEntry {
                full_oid: "a".repeat(40),
                short_hash: "a1b2c3d4".to_string(),
                author_name: "Alice".to_string(),
                timestamp: 1704067200,
                date_display: "2024-01-01".to_string(),
                summary: "initial".to_string(),
                message: "initial".to_string(),
            }],
            line_to_entry: vec![0, 0, 0],
        };
        provider.set_blame_data(&file, data);

        let repo = provider.discover(&workdir).unwrap();
        let result = repo.blame_file(&file).unwrap();

        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.line_to_entry.len(), 3);
        assert_eq!(result.entries[0].author_name, "Alice");
        assert!(result.entries[0].summary.contains("initial"));
        assert_eq!(result.entries[0].full_oid.len(), 40);
        assert!(result.entries[0].short_hash.len() <= 8);
    }

    #[test]
    fn blame_multiple_commits() {
        let fs = Arc::new(FakeFs::new());
        let provider = FakeGitProvider::new(fs);
        let workdir = PathBuf::from("/fake/repo");
        provider.set_exists(true);
        provider.set_workdir(workdir.clone());

        let file = workdir.join("test.txt");
        let data = BlameData {
            entries: vec![
                BlameEntry {
                    full_oid: "a".repeat(40),
                    short_hash: "a1b2c3d4".to_string(),
                    author_name: "Alice".to_string(),
                    timestamp: 1704067200,
                    date_display: "2024-01-01".to_string(),
                    summary: "first".to_string(),
                    message: "first".to_string(),
                },
                BlameEntry {
                    full_oid: "b".repeat(40),
                    short_hash: "b1c2d3e4".to_string(),
                    author_name: "Bob".to_string(),
                    timestamp: 1704153600,
                    date_display: "2024-01-02".to_string(),
                    summary: "bob edit".to_string(),
                    message: "bob edit".to_string(),
                },
            ],
            line_to_entry: vec![0, 1, 0],
        };
        provider.set_blame_data(&file, data);

        let repo = provider.discover(&workdir).unwrap();
        let result = repo.blame_file(&file).unwrap();

        assert_eq!(result.entries.len(), 2);
        assert_eq!(result.line_to_entry.len(), 3);

        let line1_entry = &result.entries[result.line_to_entry[0]];
        let line2_entry = &result.entries[result.line_to_entry[1]];
        let line3_entry = &result.entries[result.line_to_entry[2]];

        assert_eq!(line1_entry.author_name, "Alice");
        assert_eq!(line2_entry.author_name, "Bob");
        assert_eq!(line3_entry.author_name, "Alice");
    }
}

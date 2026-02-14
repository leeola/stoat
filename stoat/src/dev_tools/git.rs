use anyhow::{Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub struct TestRepo {
    pub dir: PathBuf,
    pub changed_files: Vec<PathBuf>,
    persist: bool,
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        if !self.persist {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

pub fn list_scenarios(fixtures_dir: &Path) -> Vec<String> {
    git_fixture::list_scenarios(fixtures_dir)
}

pub fn create_test_repo(
    fixture_dir: &Path,
    base_temp_dir: Option<&Path>,
    persist: bool,
) -> Result<TestRepo> {
    let scenario = fixture_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string());
    let base = base_temp_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(std::env::temp_dir);
    let repo = base.join("stoat-dev").join(&scenario);
    if repo.exists() {
        fs::remove_dir_all(&repo).context("cleaning previous fixture")?;
    }
    fs::create_dir_all(&repo).context("creating fixture dir")?;

    let changed_files = git_fixture::init_and_apply(fixture_dir, &repo)?;

    Ok(TestRepo {
        dir: repo,
        changed_files,
        persist,
    })
}

#[cfg(test)]
mod tests {
    use super::{create_test_repo, list_scenarios};
    use std::{fs, path::PathBuf};

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/git")
    }

    #[test]
    fn list() {
        let scenarios = list_scenarios(&fixtures_dir());
        assert!(
            scenarios.contains(&"basic-diff".to_string()),
            "expected basic-diff in {scenarios:?}"
        );
        assert!(
            scenarios.contains(&"head-vs-parent".to_string()),
            "expected head-vs-parent in {scenarios:?}"
        );
    }

    #[test]
    fn create_basic_diff() {
        let fixture = fixtures_dir().join("basic-diff");
        let tmp = create_test_repo(&fixture, None, false).unwrap();
        let repo = &tmp.dir;

        let file = repo.join("file.txt");
        assert!(file.exists(), "file.txt should exist");

        let content = fs::read_to_string(&file).unwrap();
        assert!(
            !content.contains("10 this is line ten"),
            "lines 10-20 should be removed by working tree patch"
        );
        assert!(content.contains("9 this is line nine"));
        assert!(content.contains("21 this is line twenty-one"));
    }

    #[test]
    fn create_head_vs_parent() {
        let fixture = fixtures_dir().join("head-vs-parent");
        let tmp = create_test_repo(&fixture, None, false).unwrap();
        let repo = &tmp.dir;

        let file = repo.join("main.rs");
        assert!(file.exists(), "main.rs should exist");

        let content = fs::read_to_string(&file).unwrap();

        assert!(content.contains("hello stoat editor"));
        assert!(content.contains("let first = 1;"));
        assert!(content.contains("let name = \"stoat editor\";"));

        let log = git_fixture::run_git(repo, &["log", "--oneline"]).unwrap();
        assert_eq!(log.lines().count(), 2, "should have 2 commits");

        assert!(
            tmp.changed_files.is_empty(),
            "should have no changed files: {:#?}",
            tmp.changed_files
        );
    }
}

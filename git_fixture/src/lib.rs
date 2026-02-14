use anyhow::{bail, Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::TempDir;

/// Run a git command in `dir`, returning stdout on success.
pub fn run_git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git {} failed in {}:\n{stderr}",
            args.join(" "),
            dir.display()
        );
    }

    String::from_utf8(output.stdout).context("git output is not utf-8")
}

/// List scenario subdirectories under `fixtures_dir`.
pub fn list_scenarios(fixtures_dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(fixtures_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| {
            let e = e.ok()?;
            if e.file_type().ok()?.is_dir() {
                Some(e.file_name().to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .collect();
    names.sort();
    names
}

/// Collect files changed in working tree or index (vs HEAD).
///
/// Returns absolute (canonicalized) paths.
pub fn collect_changed_files(repo: &Path) -> Result<Vec<PathBuf>> {
    let canonical_repo = fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    let mut files = Vec::new();
    for args in [
        &["diff", "--name-only"][..],
        &["diff", "--cached", "--name-only"],
    ] {
        let stdout = run_git(repo, args)?;
        for line in stdout.lines() {
            let line = line.trim();
            if !line.is_empty() {
                let path = canonical_repo.join(line);
                if !files.contains(&path) {
                    files.push(path);
                }
            }
        }
    }
    Ok(files)
}

/// Apply patches from `fixture_dir` into `repo_dir` in sorted order.
///
/// Patch prefix conventions:
/// - `c-` -- committed via `git am`
/// - `s-` -- staged via `git apply --cached`
/// - `w-` -- working tree via `git apply`
pub fn apply_patches(fixture_dir: &Path, repo_dir: &Path) -> Result<()> {
    let mut patches: Vec<_> = fs::read_dir(fixture_dir)
        .with_context(|| format!("reading fixture dir: {}", fixture_dir.display()))?
        .filter_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".patch") {
                Some((name, e.path()))
            } else {
                None
            }
        })
        .collect();
    patches.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, path) in &patches {
        let abs_patch = fs::canonicalize(path)
            .with_context(|| format!("canonicalizing patch: {}", path.display()))?;
        let patch_str = abs_patch.to_string_lossy();

        if name.starts_with("c-") {
            run_git(repo_dir, &["am", &patch_str])?;
        } else if name.starts_with("s-") {
            run_git(repo_dir, &["apply", "--cached", &patch_str])?;
        } else if name.starts_with("w-") {
            run_git(repo_dir, &["apply", &patch_str])?;
        } else {
            bail!("unknown patch prefix in '{name}', expected c-/s-/w-");
        }
    }

    Ok(())
}

/// Init a git repo at `repo_dir` with test user config, apply patches, collect changed files.
pub fn init_and_apply(fixture_dir: &Path, repo_dir: &Path) -> Result<Vec<PathBuf>> {
    run_git(repo_dir, &["init"])?;
    run_git(repo_dir, &["config", "user.name", "Test"])?;
    run_git(repo_dir, &["config", "user.email", "test@test.com"])?;
    apply_patches(fixture_dir, repo_dir)?;
    collect_changed_files(repo_dir)
}

/// Reproducible git repository from patch files, backed by a [`TempDir`].
pub struct GitFixture {
    _temp_dir: TempDir,
    dir: PathBuf,
    changed_files: Vec<PathBuf>,
}

impl GitFixture {
    /// Create a fixture by applying patches from `fixture_dir` into a new temp repo.
    pub fn new(fixture_dir: &Path) -> Result<Self> {
        let temp_dir = TempDir::new().context("creating temp dir")?;
        let dir = fs::canonicalize(temp_dir.path()).context("canonicalizing temp dir")?;
        let changed_files = init_and_apply(fixture_dir, &dir)?;
        Ok(Self {
            _temp_dir: temp_dir,
            dir,
            changed_files,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Absolute paths of files modified in the working tree or index (vs HEAD).
    pub fn changed_files(&self) -> &[PathBuf] {
        &self.changed_files
    }

    /// Run a git command in the fixture directory, returning stdout.
    pub fn git(&self, args: &[&str]) -> Result<String> {
        run_git(&self.dir, args)
    }
}

#[cfg(test)]
mod tests {
    use super::{list_scenarios, GitFixture};
    use std::path::PathBuf;

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../stoat/fixtures/git")
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
    fn basic_diff() {
        let fixture_dir = fixtures_dir().join("basic-diff");
        let fixture = GitFixture::new(&fixture_dir).unwrap();

        let file = fixture.dir().join("file.txt");
        assert!(file.exists(), "file.txt should exist");

        let content = std::fs::read_to_string(&file).unwrap();
        assert!(
            !content.contains("10 this is line ten"),
            "lines 10-20 should be removed by working tree patch"
        );
        assert!(content.contains("9 this is line nine"));
        assert!(content.contains("21 this is line twenty-one"));
    }

    #[test]
    fn head_vs_parent() {
        let fixture_dir = fixtures_dir().join("head-vs-parent");
        let fixture = GitFixture::new(&fixture_dir).unwrap();

        let file = fixture.dir().join("main.rs");
        assert!(file.exists(), "main.rs should exist");

        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.contains("hello stoat editor"));
        assert!(content.contains("let first = 1;"));
        assert!(content.contains("let name = \"stoat editor\";"));

        let log = fixture.git(&["log", "--oneline"]).unwrap();
        assert_eq!(log.lines().count(), 2, "should have 2 commits");

        assert!(
            fixture.changed_files().is_empty(),
            "should have no changed files: {:#?}",
            fixture.changed_files()
        );
    }
}

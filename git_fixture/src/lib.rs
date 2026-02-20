use anyhow::{bail, Context, Result};
use std::{
    collections::BTreeSet,
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

    if let Ok(stdout) = run_git(repo, &["diff", "HEAD~1", "--name-only"]) {
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

struct PatchInfo {
    kind: char,
    branch: String,
}

/// Parse patch filename in `{NN}-{type}-{branch}[-{description}].patch` format.
///
/// Branch names must not contain hyphens (use underscores for multi-word names).
fn parse_patch_name(name: &str) -> Result<PatchInfo> {
    let stem = name
        .strip_suffix(".patch")
        .ok_or_else(|| anyhow::anyhow!("not a .patch file: {name}"))?;
    let parts: Vec<&str> = stem.splitn(4, '-').collect();
    if parts.len() < 3 {
        bail!("invalid patch name '{name}', expected {{NN}}-{{type}}-{{branch}}[.patch]");
    }
    let kind = parts[1]
        .chars()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty type in '{name}'"))?;
    if !matches!(kind, 'c' | 's' | 'w') {
        bail!("unknown patch type '{kind}' in '{name}', expected c/s/w");
    }
    Ok(PatchInfo {
        kind,
        branch: parts[2].to_string(),
    })
}

/// Apply patches from `fixture_dir` into `repo_dir` in sorted order.
///
/// Patch filename format: `{NN}-{type}-{branch}[-{description}].patch`
///
/// - `NN` -- two-digit number controlling application order
/// - `type` -- `c` (committed via `git am`), `s` (staged via `git apply --cached`), `w` (working
///   tree via `git apply`)
/// - `branch` -- target branch; the first branch seen becomes the default. Subsequent branches are
///   created automatically and merged into the default after all patches.
///
/// Branch names must not contain hyphens.
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

    let mut default_branch: Option<String> = None;
    let mut known_branches: BTreeSet<String> = BTreeSet::new();
    let mut current_branch: Option<String> = None;

    for (name, path) in &patches {
        let info = parse_patch_name(name)?;
        let abs_patch = fs::canonicalize(path)
            .with_context(|| format!("canonicalizing patch: {}", path.display()))?;
        let patch_str = abs_patch.to_string_lossy();

        // Handle branch switching
        if default_branch.is_none() {
            default_branch = Some(info.branch.clone());
            let _ = run_git(repo_dir, &["branch", "-M", &info.branch]);
            known_branches.insert(info.branch.clone());
            current_branch = Some(info.branch.clone());
        } else if current_branch.as_deref() != Some(&info.branch) {
            if known_branches.contains(&info.branch) {
                run_git(repo_dir, &["checkout", &info.branch])?;
            } else {
                run_git(repo_dir, &["checkout", "-b", &info.branch])?;
                known_branches.insert(info.branch.clone());
            }
            current_branch = Some(info.branch.clone());
        }

        match info.kind {
            'c' => {
                run_git(repo_dir, &["am", &patch_str])?;
            },
            's' => {
                run_git(repo_dir, &["apply", "--cached", &patch_str])?;
            },
            'w' => {
                run_git(repo_dir, &["apply", &patch_str])?;
            },
            _ => unreachable!(),
        }
    }

    // Merge non-default branches into the default branch
    if let Some(ref default) = default_branch {
        if current_branch.as_deref() != Some(default) {
            run_git(repo_dir, &["checkout", default])?;
        }
        for branch in &known_branches {
            if branch != default {
                merge_no_fail(repo_dir, branch)?;
            }
        }
    }

    Ok(())
}

/// Attempt `git merge`; allow it to fail with conflicts (exit code 1).
///
/// Returns `Ok(())` whether the merge succeeds cleanly or produces conflicts.
/// Only propagates errors from process spawn or unexpected exit codes.
fn merge_no_fail(repo_dir: &Path, branch: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["merge", branch])
        .current_dir(repo_dir)
        .output()
        .context("running git merge")?;

    // Exit code 1 means conflicts -- that's expected
    if !output.status.success() && output.status.code() != Some(1) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git merge {branch} failed unexpectedly:\n{stderr}");
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
    fn merge_conflict() {
        let fixture_dir = fixtures_dir().join("merge-conflict");
        let fixture = GitFixture::new(&fixture_dir).unwrap();

        for name in ["file.txt", "config.txt"] {
            let content = std::fs::read_to_string(fixture.dir().join(name)).unwrap();
            assert!(
                content.contains("<<<<<<<"),
                "{name} should have conflict markers: {content}"
            );
        }

        let file_content = std::fs::read_to_string(fixture.dir().join("file.txt")).unwrap();
        assert!(file_content.contains("ours alpha"), "{file_content}");
        assert!(file_content.contains("theirs alpha"), "{file_content}");

        let config_content = std::fs::read_to_string(fixture.dir().join("config.txt")).unwrap();
        assert!(config_content.contains("ours-name"), "{config_content}");
        assert!(config_content.contains("theirs-name"), "{config_content}");
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
            fixture
                .changed_files()
                .iter()
                .any(|p| p.ends_with("main.rs")),
            "should contain main.rs from HEAD~1 diff: {:#?}",
            fixture.changed_files()
        );
    }
}

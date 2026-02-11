use anyhow::{bail, Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
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

    run_git(&repo, &["init"])?;
    run_git(&repo, &["config", "user.name", "Test"])?;
    run_git(&repo, &["config", "user.email", "test@test.com"])?;

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

        if name.starts_with("c-") {
            run_git(&repo, &["am", &abs_patch.to_string_lossy()])?;
        } else if name.starts_with("s-") {
            run_git(&repo, &["apply", "--cached", &abs_patch.to_string_lossy()])?;
        } else if name.starts_with("w-") {
            run_git(&repo, &["apply", &abs_patch.to_string_lossy()])?;
        } else {
            bail!("unknown patch prefix in '{name}', expected c-/s-/w-");
        }
    }

    let changed_files = collect_changed_files(&repo)?;

    Ok(TestRepo {
        dir: repo,
        changed_files,
        persist,
    })
}

fn collect_changed_files(repo: &Path) -> Result<Vec<PathBuf>> {
    let canonical_repo = fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    let mut files = Vec::new();
    for args in [
        &["diff", "--name-only"][..],
        &["diff", "--cached", "--name-only"],
    ] {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .context("running git diff --name-only")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let path = canonical_repo.join(line);
            if !files.contains(&path) {
                files.push(path);
            }
        }
    }
    Ok(files)
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {stderr}", args.join(" "));
    }
    Ok(())
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
}

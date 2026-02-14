use std::path::{Path, PathBuf};

/// Reproducible git repository from patch files.
///
/// Loads a fixture scenario from `fixtures/git/{scenario}/`, applying patches in sorted order
/// using prefix conventions:
/// - `c-` -- committed (applied via `git am`)
/// - `s-` -- staged (applied via `git apply --cached`)
/// - `w-` -- working tree only (applied via `git apply`)
///
/// The repository lives in a [`tempfile::TempDir`] that is cleaned up on drop.
pub struct GitFixture {
    inner: git_fixture::GitFixture,
}

impl GitFixture {
    /// Create a fixture from the named scenario directory under `fixtures/git/`.
    ///
    /// Panics if git commands fail or the scenario directory doesn't exist.
    pub fn load(scenario: &str) -> Self {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let fixture_dir = manifest_dir.join("fixtures/git").join(scenario);
        Self {
            inner: git_fixture::GitFixture::new(&fixture_dir)
                .unwrap_or_else(|e| panic!("failed to load fixture {scenario}: {e}")),
        }
    }

    pub fn dir(&self) -> &Path {
        self.inner.dir()
    }

    /// Absolute paths of files modified in the working tree or index (vs HEAD).
    pub fn changed_files(&self) -> &[PathBuf] {
        self.inner.changed_files()
    }

    /// Run a git command in the fixture directory and return stdout.
    ///
    /// Panics on non-zero exit.
    pub fn git(&self, args: &[&str]) -> String {
        self.inner
            .git(args)
            .unwrap_or_else(|e| panic!("git {} failed: {e}", args.join(" ")))
    }
}

#[cfg(test)]
mod tests {
    use super::GitFixture;

    #[test]
    fn load_basic_diff() {
        let fixture = GitFixture::load("basic-diff");
        assert!(!fixture.changed_files().is_empty());
        assert!(fixture.git(&["log", "--oneline"]).contains("initial"));
    }

    #[test]
    fn load_head_vs_parent() {
        let fixture = GitFixture::load("head-vs-parent");
        let log = fixture.git(&["log", "--oneline"]);
        let commit_count = log.lines().count();
        assert_eq!(commit_count, 2);
    }
}

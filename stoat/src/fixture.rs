//! Deterministic on-disk git fixtures for manual runs and integration tests.
//!
//! Exercising the real [`crate::host::LocalGit`] path (discovery, status, head
//! content, commit walks) needs an actual git repository on disk. The in-memory
//! fakes cannot stand in for libgit2. This module materializes named
//! repositories whose history is byte-for-byte reproducible. Every commit is
//! authored with a pinned signature and clock, and no gitconfig is consulted,
//! so the resulting SHAs are identical across runs and machines.
//!
//! That stability is what lets integration tests assert against concrete commit
//! shas, and lets a `--fixture <name>` run reproduce the same working-tree
//! state every time.
//!
//! Gated behind the non-default `fixture` feature so production builds carry no
//! test-scaffolding code.

use git2::{build::CheckoutBuilder, Commit, Repository, RepositoryInitOptions, Signature, Time};
use snafu::{ResultExt, Snafu};
use std::{
    io,
    path::{Path, PathBuf},
};

/// Unix epoch seconds for the first commit's author/committer clock. Each
/// subsequent commit advances by one second, keeping the timeline monotonic
/// while fully determined by commit order rather than wall-clock time.
const FIXTURE_EPOCH: i64 = 1_700_000_000;

const FIXTURE_AUTHOR: &str = "stoat fixture";

const FIXTURE_EMAIL: &str = "fixture@stoat.invalid";

const STAGED_HEAD: &str = "\
1 alpha
2 bravo
3 charlie
4 delta
5 echo
6 foxtrot
";

const STAGED_WORK: &str = "\
1 alpha
2 bravo
3 charlie
4 delta changed
5 echo
6 foxtrot
";

const UNSTAGED_HEAD: &str = "\
1 one
2 two
3 three
4 four
5 five
6 six
";

const UNSTAGED_WORK: &str = "\
1 one
2 two
3 three
4 four changed
5 five
6 six
";

const HISTORY_API: &str = "\
GET /api/health
GET /api/version
";

const HISTORY_APP: &str = "\
name = fixture
port = 8080
";

const HISTORY_CI: &str = "\
steps:
  - build
  - test
";

const HISTORY_TESTS: &str = "\
test health
test version
";

const CONFLICT_BASE: &str = "\
1 shared header
2 middle line
3 shared footer
";

const CONFLICT_OURS: &str = "\
1 shared header
2 ours edit
3 shared footer
";

const CONFLICT_THEIRS: &str = "\
1 shared header
2 theirs edit
3 shared footer
";

const RUST_LSP_CARGO: &str = r#"[package]
name = "fixture-rust-lsp"
version = "0.1.0"
edition = "2021"

[workspace]
"#;

const RUST_LSP_MAIN: &str = r#"struct Greeter {
    name: String,
}

impl Greeter {
    fn greet(&self) -> String {
        format!("hello, {}", self.name)
    }
}

fn main() {
    let unused = 1;
    let greeter = Greeter {
        name: "world".to_string(),
    };
    println!("{}", greeter.greet());
}
"#;

/// Failure materializing a fixture repository.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum FixtureError {
    #[snafu(display("unknown fixture: {name}"))]
    UnknownFixture {
        name: String,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("git operation failed: {source}"))]
    Git {
        source: git2::Error,
        #[snafu(implicit)]
        location: snafu::Location,
    },

    #[snafu(display("filesystem operation failed for {}: {source}", path.display()))]
    Io {
        source: io::Error,
        path: PathBuf,
        #[snafu(implicit)]
        location: snafu::Location,
    },
}

/// Build the named fixture as a git repository rooted at `dest`.
///
/// `dest` must be an existing, empty directory. It becomes a non-bare repo
/// whose commits reproduce byte-for-byte across runs and machines. Every commit
/// is authored with a pinned signature and clock, and no gitconfig is read.
///
/// The defined fixtures are:
/// - `basic-diff`: two files at HEAD, then one staged and one unstaged modification, so
///   [`crate::host::GitRepo::changed_files`] reports one entry of each kind.
/// - `history`: a four-commit linear chain, each commit adding a distinct file, giving
///   [`crate::host::GitRepo::log_commits`] and [`crate::host::GitRepo::commit_file_changes`] real
///   history to walk.
/// - `conflict`: a base commit with two divergent children on `main` and `theirs` editing the same
///   lines, so [`crate::host::GitRepo::cherry_pick_tree`] between the tips conflicts.
/// - `rust-lsp`: a clean, minimal cargo crate at HEAD (Cargo.toml plus src/main.rs) as a target for
///   rust-analyzer.
///
/// Fails with [`FixtureError::UnknownFixture`] for an unrecognized `name`, or
/// [`FixtureError::Git`] / [`FixtureError::Io`] if the repository cannot be
/// written.
pub fn materialize(name: &str, dest: &Path) -> Result<(), FixtureError> {
    match name {
        "basic-diff" => materialize_basic_diff(dest),
        "history" => materialize_history(dest),
        "conflict" => materialize_conflict(dest),
        "rust-lsp" => materialize_rust_lsp(dest),
        _ => UnknownFixtureSnafu {
            name: name.to_string(),
        }
        .fail(),
    }
}

fn materialize_basic_diff(dest: &Path) -> Result<(), FixtureError> {
    let mut repo = FixtureRepo::init(dest)?;
    repo.commit(
        "initial commit",
        &[("staged.txt", STAGED_HEAD), ("unstaged.txt", UNSTAGED_HEAD)],
    )?;
    repo.staged_file("staged.txt", STAGED_WORK)?;
    repo.unstaged_file("unstaged.txt", UNSTAGED_WORK)?;
    Ok(())
}

fn materialize_history(dest: &Path) -> Result<(), FixtureError> {
    let mut repo = FixtureRepo::init(dest)?;
    repo.commit("add api endpoints", &[("api.txt", HISTORY_API)])?;
    repo.commit("add app config", &[("app.txt", HISTORY_APP)])?;
    repo.commit("add ci pipeline", &[("ci.txt", HISTORY_CI)])?;
    repo.commit("add tests", &[("tests.txt", HISTORY_TESTS)])?;
    Ok(())
}

fn materialize_conflict(dest: &Path) -> Result<(), FixtureError> {
    let mut repo = FixtureRepo::init(dest)?;
    repo.commit("base", &[("file.txt", CONFLICT_BASE)])?;
    repo.branch("theirs")?;
    repo.commit("ours change", &[("file.txt", CONFLICT_OURS)])?;
    repo.checkout("theirs")?;
    repo.commit("theirs change", &[("file.txt", CONFLICT_THEIRS)])?;
    repo.checkout("main")?;
    Ok(())
}

fn materialize_rust_lsp(dest: &Path) -> Result<(), FixtureError> {
    let mut repo = FixtureRepo::init(dest)?;
    repo.commit(
        "initial commit",
        &[
            ("Cargo.toml", RUST_LSP_CARGO),
            ("src/main.rs", RUST_LSP_MAIN),
        ],
    )?;
    Ok(())
}

/// Builder over a real git2 repository, modeled on the `TestRepo` helper used
/// by the integration tests but authoring every commit with a deterministic
/// signature so SHAs reproduce. Method vocabulary mirrors the in-memory
/// `FakeRepoBuilder`: `commit` seeds history, `staged_file` / `unstaged_file`
/// leave working-tree changes against it.
struct FixtureRepo {
    repo: Repository,
    commit_count: i64,
}

impl FixtureRepo {
    fn init(dest: &Path) -> Result<Self, FixtureError> {
        // Pin the initial branch to `main` rather than inheriting
        // init.defaultBranch, so fixtures reference a fixed branch name and
        // stay independent of the machine's gitconfig.
        let mut opts = RepositoryInitOptions::new();
        opts.initial_head("main");
        let repo = Repository::init_opts(dest, &opts).context(GitSnafu)?;
        Ok(Self {
            repo,
            commit_count: 0,
        })
    }

    /// Write each `(name, content)` file, stage it, and commit the tree with a
    /// pinned signature. The first call produces a root commit. Later calls
    /// parent off the current HEAD.
    fn commit(&mut self, message: &str, files: &[(&str, &str)]) -> Result<&mut Self, FixtureError> {
        for (name, content) in files {
            self.write(name, content)?;
            self.stage(name)?;
        }

        let time = Time::new(FIXTURE_EPOCH + self.commit_count, 0);
        self.commit_count += 1;
        let sig = Signature::new(FIXTURE_AUTHOR, FIXTURE_EMAIL, &time).context(GitSnafu)?;

        // Scope the tree and parent borrows of `self.repo` so they drop before
        // the `&mut self` return. `git2::Tree` holds an immutable borrow of the
        // repo alive until it is dropped.
        {
            let tree = {
                let mut index = self.repo.index().context(GitSnafu)?;
                let tree_id = index.write_tree().context(GitSnafu)?;
                self.repo.find_tree(tree_id).context(GitSnafu)?
            };

            let parents: Vec<Commit<'_>> = self
                .repo
                .head()
                .ok()
                .and_then(|head| head.peel_to_commit().ok())
                .into_iter()
                .collect();
            let parent_refs: Vec<&Commit<'_>> = parents.iter().collect();

            self.repo
                .commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)
                .context(GitSnafu)?;
        }

        Ok(self)
    }

    /// Write `content` to `name` and stage it, leaving a staged modification
    /// against HEAD.
    fn staged_file(&mut self, name: &str, content: &str) -> Result<&mut Self, FixtureError> {
        self.write(name, content)?;
        self.stage(name)?;
        Ok(self)
    }

    /// Write `content` to `name` in the working tree only, leaving an unstaged
    /// modification against the index.
    fn unstaged_file(&mut self, name: &str, content: &str) -> Result<&mut Self, FixtureError> {
        self.write(name, content)?;
        Ok(self)
    }

    /// Create a branch named `name` pointing at the current HEAD commit.
    fn branch(&self, name: &str) -> Result<&Self, FixtureError> {
        let head = self
            .repo
            .head()
            .context(GitSnafu)?
            .peel_to_commit()
            .context(GitSnafu)?;
        self.repo.branch(name, &head, false).context(GitSnafu)?;
        Ok(self)
    }

    /// Point HEAD at branch `name` and reset the working tree and index to it,
    /// so a following [`Self::commit`] extends that branch.
    fn checkout(&self, name: &str) -> Result<&Self, FixtureError> {
        let refname = format!("refs/heads/{name}");
        let target = self.repo.revparse_single(&refname).context(GitSnafu)?;
        self.repo
            .checkout_tree(&target, Some(CheckoutBuilder::new().force()))
            .context(GitSnafu)?;
        self.repo.set_head(&refname).context(GitSnafu)?;
        Ok(self)
    }

    fn workdir(&self) -> PathBuf {
        self.repo
            .workdir()
            .expect("fixture repo is always non-bare")
            .to_path_buf()
    }

    fn write(&self, name: &str, content: &str) -> Result<(), FixtureError> {
        let path = self.workdir().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context(IoSnafu {
                path: parent.to_path_buf(),
            })?;
        }
        std::fs::write(&path, content).context(IoSnafu { path })?;
        Ok(())
    }

    fn stage(&self, name: &str) -> Result<(), FixtureError> {
        let mut index = self.repo.index().context(GitSnafu)?;
        index.add_path(Path::new(name)).context(GitSnafu)?;
        index.write().context(GitSnafu)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{CherryPickOutcome, CommitFileChangeKind, GitHost, LocalGit};

    #[test]
    fn basic_diff_reports_staged_and_unstaged_split() {
        let dir = tempfile::tempdir().unwrap();
        materialize("basic-diff", dir.path()).unwrap();

        let repo = LocalGit::new().discover(dir.path()).unwrap();
        let files = repo.changed_files();

        assert_eq!(files.len(), 2, "one staged and one unstaged entry");
        assert!(files[0].staged);
        assert!(files[0].path.ends_with("staged.txt"));
        assert!(!files[1].staged);
        assert!(files[1].path.ends_with("unstaged.txt"));
    }

    #[test]
    fn unknown_fixture_name_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = materialize("does-not-exist", dir.path()).unwrap_err();
        assert!(matches!(err, FixtureError::UnknownFixture { .. }));
    }

    #[test]
    fn basic_diff_head_sha_is_deterministic() {
        let head_sha = || {
            let dir = tempfile::tempdir().unwrap();
            materialize("basic-diff", dir.path()).unwrap();
            let repo = LocalGit::new().discover(dir.path()).unwrap();
            repo.log_commits(None, 1)[0].sha.clone()
        };
        assert_eq!(
            head_sha(),
            head_sha(),
            "pinned signatures must reproduce the sha"
        );
    }

    #[test]
    fn history_has_four_commit_chain() {
        let dir = tempfile::tempdir().unwrap();
        materialize("history", dir.path()).unwrap();

        let repo = LocalGit::new().discover(dir.path()).unwrap();
        let log = repo.log_commits(None, 10);

        assert_eq!(log.len(), 4, "four-commit linear chain");
        let head_changes = repo.commit_file_changes(&log[0].sha);
        assert!(head_changes
            .iter()
            .any(|c| c.rel_path.ends_with("tests.txt") && c.kind == CommitFileChangeKind::Added));
    }

    #[test]
    fn conflict_cherry_pick_reports_conflict() {
        let dir = tempfile::tempdir().unwrap();
        materialize("conflict", dir.path()).unwrap();

        let git = Repository::open(dir.path()).unwrap();
        let ours = git.revparse_single("main").unwrap().id().to_string();
        let theirs = git.revparse_single("theirs").unwrap().id().to_string();

        let repo = LocalGit::new().discover(dir.path()).unwrap();
        match repo.cherry_pick_tree(&theirs, &ours).unwrap() {
            CherryPickOutcome::Conflict { files } => {
                assert!(files.iter().any(|f| f.path.ends_with("file.txt")));
            },
            other => panic!("expected conflict, got {other:?}"),
        }
    }

    #[test]
    fn rust_lsp_is_clean_crate() {
        let dir = tempfile::tempdir().unwrap();
        materialize("rust-lsp", dir.path()).unwrap();

        let repo = LocalGit::new().discover(dir.path()).unwrap();
        assert!(repo.changed_files().is_empty(), "clean tree at HEAD");

        let head = repo.log_commits(None, 1)[0].sha.clone();
        let tree = repo.commit_tree(&head).unwrap();
        assert!(tree.contains_key(Path::new("Cargo.toml")));
        assert!(tree.contains_key(Path::new("src/main.rs")));
    }
}

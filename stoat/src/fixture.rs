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

pub mod harness;

pub use harness::LiveHarness;

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

const DIFF_MODIFIED_STAGED_HEAD: &str = "\
1 modified-staged
2 original line
3 tail
";

const DIFF_MODIFIED_STAGED_WORK: &str = "\
1 modified-staged
2 staged edit
3 tail
";

const DIFF_MODIFIED_UNSTAGED_HEAD: &str = "\
1 modified-unstaged
2 original line
3 tail
";

const DIFF_MODIFIED_UNSTAGED_WORK: &str = "\
1 modified-unstaged
2 unstaged edit
3 tail
";

const DIFF_DELETED_STAGED: &str = "staged deletion: removed from index and tree\n";

const DIFF_DELETED_UNSTAGED: &str = "unstaged deletion: removed from working tree only\n";

const DIFF_RENAMED: &str = "content carried across a staged rename\n";

const DIFF_ADDED_STAGED: &str = "freshly added and staged file\n";

const DIFF_UNTRACKED: &str = "untracked working-tree file\n";

const DIFF_HUNKS_HEAD: &str = "\
01 alpha
02 bravo
03 charlie
04 delta
05 echo
06 foxtrot
07 golf
08 hotel
09 india
10 juliet
11 kilo
12 lima
13 mike
14 november
15 oscar
16 papa
17 quebec
18 romeo
19 sierra
20 tango
";

const DIFF_HUNKS_WORK: &str = "\
01 alpha
02 bravo
03 charlie edited
04 delta
05 echo
06 foxtrot
07 golf
08 hotel
09 india
10 juliet
11 kilo
12 lima
13 mike
14 november
15 oscar
16 papa
17 quebec edited
18 romeo
19 sierra
20 tango
";

const MANY_FILES_STAGED: &[(&str, &str, &str)] = &[
    ("src/alpha.txt", "alpha original\n", "alpha staged edit\n"),
    ("src/bravo.txt", "bravo original\n", "bravo staged edit\n"),
    (
        "docs/charlie.txt",
        "charlie original\n",
        "charlie staged edit\n",
    ),
    ("docs/delta.txt", "delta original\n", "delta staged edit\n"),
    ("config/echo.txt", "echo original\n", "echo staged edit\n"),
];

const MANY_FILES_UNSTAGED: &[(&str, &str, &str)] = &[
    (
        "src/foxtrot.txt",
        "foxtrot original\n",
        "foxtrot unstaged edit\n",
    ),
    ("src/golf.txt", "golf original\n", "golf unstaged edit\n"),
    (
        "docs/hotel.txt",
        "hotel original\n",
        "hotel unstaged edit\n",
    ),
    (
        "config/india.txt",
        "india original\n",
        "india unstaged edit\n",
    ),
    (
        "config/juliet.txt",
        "juliet original\n",
        "juliet unstaged edit\n",
    ),
];

const MANY_FILES_CLEAN: &[(&str, &str)] = &[
    ("src/kilo.txt", "kilo unchanged\n"),
    ("docs/lima.txt", "lima unchanged\n"),
];

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

const REBASE_BASE: &str = "base module\n";

const REBASE_DEPLOY: &str = "deploy pipeline\n";

const REBASE_DOCS: &str = "documentation\n";

const REBASE_PARSER: &str = "parser stage\n";

const REBASE_LEXER: &str = "lexer stage\n";

const REBASE_AST: &str = "ast nodes\n";

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

const RUST_DIFF_MAIN_HEAD: &str = r#"mod util;

struct Greeter {
    name: String,
}

impl Greeter {
    fn greet(&self) -> String {
        format!("hello, {}", self.name)
    }
}

fn main() {
    let greeter = Greeter {
        name: "world".to_string(),
    };
    println!("{}", util::shout(&greeter.greet()));
}
"#;

const RUST_DIFF_MAIN_WORK: &str = r#"mod util;

struct Greeter {
    name: String,
}

impl Greeter {
    fn greet(&self) -> String {
        format!("hey there, {}", self.name)
    }
}

fn main() {
    let greeter = Greeter {
        name: "world".to_string(),
    };
    println!("{}", util::shout(&greeter.greet()));
}
"#;

const RUST_DIFF_UTIL_HEAD: &str = r#"pub fn shout(text: &str) -> String {
    text.to_uppercase()
}
"#;

const RUST_DIFF_UTIL_WORK: &str = r#"pub fn shout(text: &str) -> String {
    emphasize(&text.to_uppercase())
}

fn emphasize(text: &str) -> String {
    format!("{text}!")
}
"#;

const RUST_DIFF_GITIGNORE: &str = "/target\n";

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
/// - `diff-kinds`: one working tree carrying every git change kind at once -- staged and unstaged
///   modifications, a staged addition, an untracked file, staged and unstaged deletions, a staged
///   rename, and a two-hunk unstaged edit. It probes the full status surface.
///   [`crate::host::GitRepo::changed_files`] reports all of them, five staged (the addition, the
///   staged deletion, the staged modification, and the rename's old and new paths -- status runs no
///   rename detection, so the old path shows as a deletion) and four unstaged (the untracked file,
///   the unstaged deletion, and the two unstaged modifications).
/// - `many-files`: one HEAD commit of twelve files across `src/`, `docs/`, and `config/`, then five
///   staged and five unstaged modifications with two left clean, so
///   [`crate::host::GitRepo::changed_files`] reports ten changed files spanning nested directories
///   for file-list navigation and scale testing.
/// - `history`: a four-commit linear chain, each commit adding a distinct file, giving
///   [`crate::host::GitRepo::log_commits`] and [`crate::host::GitRepo::commit_file_changes`] real
///   history to walk.
/// - `conflict`: a base commit with two divergent children on `main` and `theirs` editing the same
///   lines, so [`crate::host::GitRepo::cherry_pick_tree`] between the tips conflicts.
/// - `rebase`: a base commit with `main` and `feature` diverging over disjoint files -- two commits
///   advance `main`, three advance `feature` -- so [`crate::host::GitRepo::run_rebase`] replays
///   `feature` onto `main` cleanly. The non-conflicting complement to `conflict`; HEAD is left on
///   `feature`.
/// - `rust-lsp`: a clean, minimal cargo crate at HEAD (Cargo.toml plus src/main.rs) as a target for
///   rust-analyzer.
/// - `rust-diff`: the minimal `rust-lsp` cargo crate plus a live working-tree diff -- a staged
///   change to `src/util.rs` and an unstaged change to `src/main.rs`, with every layer (HEAD,
///   index, worktree) compiling so `cargo build` stays green and rust-analyzer runs against a dirty
///   tree.
///
/// Fails with [`FixtureError::UnknownFixture`] for an unrecognized `name`, or
/// [`FixtureError::Git`] / [`FixtureError::Io`] if the repository cannot be
/// written.
pub fn materialize(name: &str, dest: &Path) -> Result<(), FixtureError> {
    match name {
        "basic-diff" => materialize_basic_diff(dest),
        "diff-kinds" => materialize_diff_kinds(dest),
        "many-files" => materialize_many_files(dest),
        "history" => materialize_history(dest),
        "conflict" => materialize_conflict(dest),
        "rebase" => materialize_rebase(dest),
        "rust-lsp" => materialize_rust_lsp(dest),
        "rust-diff" => materialize_rust_diff(dest),
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

fn materialize_diff_kinds(dest: &Path) -> Result<(), FixtureError> {
    let mut repo = FixtureRepo::init(dest)?;
    repo.commit(
        "initial commit",
        &[
            ("modified-staged.txt", DIFF_MODIFIED_STAGED_HEAD),
            ("modified-unstaged.txt", DIFF_MODIFIED_UNSTAGED_HEAD),
            ("deleted-staged.txt", DIFF_DELETED_STAGED),
            ("deleted-unstaged.txt", DIFF_DELETED_UNSTAGED),
            ("renamed-from.txt", DIFF_RENAMED),
            ("hunks.txt", DIFF_HUNKS_HEAD),
        ],
    )?;
    repo.staged_file("modified-staged.txt", DIFF_MODIFIED_STAGED_WORK)?;
    repo.unstaged_file("modified-unstaged.txt", DIFF_MODIFIED_UNSTAGED_WORK)?;
    repo.staged_file("added-staged.txt", DIFF_ADDED_STAGED)?;
    repo.unstaged_file("untracked.txt", DIFF_UNTRACKED)?;
    repo.staged_delete("deleted-staged.txt")?;
    repo.unstaged_delete("deleted-unstaged.txt")?;
    repo.staged_rename("renamed-from.txt", "renamed-to.txt")?;
    repo.unstaged_file("hunks.txt", DIFF_HUNKS_WORK)?;
    Ok(())
}

fn materialize_many_files(dest: &Path) -> Result<(), FixtureError> {
    let mut repo = FixtureRepo::init(dest)?;

    let mut head_files: Vec<(&str, &str)> = Vec::new();
    for &(path, head, _work) in MANY_FILES_STAGED.iter().chain(MANY_FILES_UNSTAGED) {
        head_files.push((path, head));
    }
    for &(path, content) in MANY_FILES_CLEAN {
        head_files.push((path, content));
    }
    repo.commit("initial commit", &head_files)?;

    for &(path, _head, work) in MANY_FILES_STAGED {
        repo.staged_file(path, work)?;
    }
    for &(path, _head, work) in MANY_FILES_UNSTAGED {
        repo.unstaged_file(path, work)?;
    }
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

fn materialize_rebase(dest: &Path) -> Result<(), FixtureError> {
    let mut repo = FixtureRepo::init(dest)?;
    repo.commit("base", &[("base.txt", REBASE_BASE)])?;
    repo.branch("feature")?;
    repo.commit("deploy", &[("deploy.txt", REBASE_DEPLOY)])?;
    repo.commit("docs", &[("docs.txt", REBASE_DOCS)])?;
    repo.checkout("feature")?;
    repo.commit("parser", &[("parser.txt", REBASE_PARSER)])?;
    repo.commit("lexer", &[("lexer.txt", REBASE_LEXER)])?;
    repo.commit("ast", &[("ast.txt", REBASE_AST)])?;
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

fn materialize_rust_diff(dest: &Path) -> Result<(), FixtureError> {
    let mut repo = FixtureRepo::init(dest)?;
    repo.commit(
        "initial commit",
        &[
            (".gitignore", RUST_DIFF_GITIGNORE),
            ("Cargo.toml", RUST_LSP_CARGO),
            ("src/main.rs", RUST_DIFF_MAIN_HEAD),
            ("src/util.rs", RUST_DIFF_UTIL_HEAD),
        ],
    )?;
    repo.staged_file("src/util.rs", RUST_DIFF_UTIL_WORK)?;
    repo.unstaged_file("src/main.rs", RUST_DIFF_MAIN_WORK)?;
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

    /// Remove `name` from both the working tree and the index, leaving a staged
    /// deletion against HEAD.
    fn staged_delete(&mut self, name: &str) -> Result<&mut Self, FixtureError> {
        let path = self.workdir().join(name);
        std::fs::remove_file(&path).context(IoSnafu { path })?;
        let mut index = self.repo.index().context(GitSnafu)?;
        index.remove_path(Path::new(name)).context(GitSnafu)?;
        index.write().context(GitSnafu)?;
        Ok(self)
    }

    /// Remove `name` from the working tree only, leaving an unstaged deletion
    /// against the index.
    fn unstaged_delete(&mut self, name: &str) -> Result<&mut Self, FixtureError> {
        let path = self.workdir().join(name);
        std::fs::remove_file(&path).context(IoSnafu { path })?;
        Ok(self)
    }

    /// Rename `old` to `new` in both the working tree and the index, leaving a
    /// staged rename against HEAD. Status carries no rename detection, so this
    /// surfaces as a deletion of `old` plus an addition of `new`.
    fn staged_rename(&mut self, old: &str, new: &str) -> Result<&mut Self, FixtureError> {
        let workdir = self.workdir();
        let from = workdir.join(old);
        std::fs::rename(&from, workdir.join(new)).context(IoSnafu { path: from })?;
        let mut index = self.repo.index().context(GitSnafu)?;
        index.remove_path(Path::new(old)).context(GitSnafu)?;
        index.add_path(Path::new(new)).context(GitSnafu)?;
        index.write().context(GitSnafu)?;
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
    use crate::host::{
        CherryPickOutcome, CommitFileChangeKind, GitHost, LocalGit, RebaseTodo, RebaseTodoOp,
    };
    use git2::Sort;

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
    fn diff_kinds_surfaces_supported_change_kinds() {
        let dir = tempfile::tempdir().unwrap();
        materialize("diff-kinds", dir.path()).unwrap();

        let repo = LocalGit::new().discover(dir.path()).unwrap();
        let got: Vec<(String, bool)> = repo
            .changed_files()
            .iter()
            .map(|f| {
                (
                    f.path.file_name().unwrap().to_str().unwrap().to_string(),
                    f.staged,
                )
            })
            .collect();

        assert_eq!(
            got,
            vec![
                ("added-staged.txt".to_string(), true),
                ("deleted-staged.txt".to_string(), true),
                ("modified-staged.txt".to_string(), true),
                ("renamed-from.txt".to_string(), true),
                ("renamed-to.txt".to_string(), true),
                ("deleted-unstaged.txt".to_string(), false),
                ("hunks.txt".to_string(), false),
                ("modified-unstaged.txt".to_string(), false),
                ("untracked.txt".to_string(), false),
            ],
            "every change kind surfaces: the untracked file, both deletions, and the rename source (a delete of the old path, no rename detection) join the modifications and additions",
        );
    }

    #[test]
    fn many_files_reports_ten_changed_across_directories() {
        let dir = tempfile::tempdir().unwrap();
        materialize("many-files", dir.path()).unwrap();

        let repo = LocalGit::new().discover(dir.path()).unwrap();
        let workdir = repo.workdir().unwrap();
        let got: Vec<(String, bool)> = repo
            .changed_files()
            .iter()
            .map(|f| {
                (
                    f.path
                        .strip_prefix(&workdir)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_string(),
                    f.staged,
                )
            })
            .collect();

        assert_eq!(
            got,
            vec![
                ("config/echo.txt".to_string(), true),
                ("docs/charlie.txt".to_string(), true),
                ("docs/delta.txt".to_string(), true),
                ("src/alpha.txt".to_string(), true),
                ("src/bravo.txt".to_string(), true),
                ("config/india.txt".to_string(), false),
                ("config/juliet.txt".to_string(), false),
                ("docs/hotel.txt".to_string(), false),
                ("src/foxtrot.txt".to_string(), false),
                ("src/golf.txt".to_string(), false),
            ],
            "ten changed files, staged block then unstaged block, each sorted by path; two clean files absent",
        );
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

    #[test]
    fn rust_diff_reports_staged_util_and_unstaged_main() {
        let dir = tempfile::tempdir().unwrap();
        materialize("rust-diff", dir.path()).unwrap();

        let repo = LocalGit::new().discover(dir.path()).unwrap();
        let workdir = repo.workdir().unwrap();
        let got: Vec<(String, bool)> = repo
            .changed_files()
            .iter()
            .map(|f| {
                (
                    f.path
                        .strip_prefix(&workdir)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_string(),
                    f.staged,
                )
            })
            .collect();

        assert_eq!(
            got,
            vec![
                ("src/util.rs".to_string(), true),
                ("src/main.rs".to_string(), false),
            ],
            "staged util.rs before unstaged main.rs",
        );
    }

    #[test]
    fn rebase_replays_feature_onto_main_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        materialize("rebase", dir.path()).unwrap();

        let git = Repository::open(dir.path()).unwrap();
        let main_tip = git.revparse_single("main").unwrap().id();
        let feature_tip = git.revparse_single("feature").unwrap().id();

        let feature_shas: Vec<String> = {
            let mut walk = git.revwalk().unwrap();
            walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE).unwrap();
            walk.push(feature_tip).unwrap();
            walk.hide(main_tip).unwrap();
            walk.map(|oid| oid.unwrap().to_string()).collect()
        };

        let todo: Vec<RebaseTodo> = feature_shas
            .iter()
            .map(|sha| RebaseTodo {
                op: RebaseTodoOp::Pick,
                sha: sha.clone(),
                message: String::new(),
            })
            .collect();

        let repo = LocalGit::new().discover(dir.path()).unwrap();
        let new_sha = repo
            .run_rebase(&main_tip.to_string(), &todo)
            .expect("disjoint-file feature rebases onto main without conflict");

        let tree = repo.commit_tree(&new_sha).unwrap();
        let files: Vec<&str> = tree.keys().map(|p| p.to_str().unwrap()).collect();
        assert_eq!(
            files,
            vec![
                "ast.txt",
                "base.txt",
                "deploy.txt",
                "docs.txt",
                "lexer.txt",
                "parser.txt",
            ],
            "rebased feature tip carries all six files from both branches",
        );
    }
}

//! Integration tests for `stoat diff`.
//!
//! Production wires `LocalGit` + `LocalFs` against the real
//! filesystem; these tests do the same against a per-test
//! `tempfile::TempDir`. The same carve-out the existing
//! `stoat/tests/local_git.rs` integration tests rely on -- a real
//! git2 repo is the only realistic way to exercise `LocalGit`'s
//! discovery and head-content paths.

use git2::Repository;
use std::path::Path;
use stoat::{
    diff_render_cli::{CliLayout, CliRenderOptions},
    host::{LocalFs, LocalGit},
};
use stoat_bin::commands::diff::{run_with_io, DiffArgs, WriteError};

struct TestRepo {
    repo: Repository,
    _dir: tempfile::TempDir,
}

impl TestRepo {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("create tempdir");
        let repo = Repository::init(dir.path()).expect("init repo");
        Self { repo, _dir: dir }
    }

    fn path(&self) -> &Path {
        self.repo.workdir().expect("repo has workdir")
    }

    fn write(&self, name: &str, content: &str) {
        let path = self.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&path, content).expect("write file");
    }

    fn delete(&self, name: &str) {
        let path = self.path().join(name);
        std::fs::remove_file(&path).expect("remove file");
    }

    fn stage(&self, name: &str) {
        let mut index = self.repo.index().expect("index");
        index.add_path(Path::new(name)).expect("add path");
        index.write().expect("write index");
    }

    fn commit_initial(&self, files: &[(&str, &str)]) {
        for (name, content) in files {
            self.write(name, content);
            self.stage(name);
        }
        let mut index = self.repo.index().expect("index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = self.repo.find_tree(tree_id).expect("find tree");
        let sig = git2::Signature::now("test", "t@t").expect("sig");
        self.repo
            .commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .expect("commit");
    }
}

fn opts_with(layout: CliLayout, color: bool) -> CliRenderOptions {
    CliRenderOptions {
        layout,
        width: 80,
        color,
    }
}

fn default_args() -> DiffArgs {
    DiffArgs {
        git: false,
        side_by_side: true,
        unified: false,
        no_color: true,
        no_pager: true,
        width: Some(80),
        language: None,
        git_args: Vec::new(),
    }
}

fn run_capture(args: &DiffArgs, cwd: &Path, opts: &CliRenderOptions) -> Vec<u8> {
    let fs = LocalFs;
    let git = LocalGit::new();
    let mut buf = Vec::new();
    match run_with_io(args, &fs, &git, cwd, opts, &mut buf) {
        Ok(()) => buf,
        Err(WriteError::BrokenPipe) => buf,
        Err(WriteError::Other(e)) => panic!("run_with_io failed: {e}"),
    }
}

#[test]
fn default_clean_tree_no_output() {
    let repo = TestRepo::new();
    repo.commit_initial(&[("a.rs", "fn a() { 1 }\n")]);

    let out = run_capture(
        &default_args(),
        repo.path(),
        &opts_with(CliLayout::Unified, false),
    );
    assert_eq!(out, Vec::<u8>::new());
}

#[test]
fn default_single_file_modification() {
    let repo = TestRepo::new();
    repo.commit_initial(&[("a.rs", "fn a() { 1 }\n")]);
    repo.write("a.rs", "fn a() { 2 }\n");

    let out = run_capture(
        &default_args(),
        repo.path(),
        &opts_with(CliLayout::Unified, false),
    );
    insta::assert_snapshot!("diff_cli_modification", String::from_utf8(out).unwrap());
}

#[test]
fn default_file_added() {
    let repo = TestRepo::new();
    repo.commit_initial(&[("a.rs", "fn a() { 1 }\n")]);
    repo.write("b.rs", "fn b() { 2 }\n");

    let out = run_capture(
        &default_args(),
        repo.path(),
        &opts_with(CliLayout::Unified, false),
    );
    insta::assert_snapshot!("diff_cli_added", String::from_utf8(out).unwrap());
}

#[test]
fn default_file_deleted() {
    let repo = TestRepo::new();
    repo.commit_initial(&[("a.rs", "fn a() { 1 }\n"), ("b.rs", "fn b() { 2 }\n")]);
    repo.delete("b.rs");

    let out = run_capture(
        &default_args(),
        repo.path(),
        &opts_with(CliLayout::Unified, false),
    );
    insta::assert_snapshot!("diff_cli_deleted", String::from_utf8(out).unwrap());
}

#[test]
fn default_no_color_strips_sgr() {
    let repo = TestRepo::new();
    repo.commit_initial(&[("a.rs", "fn a() { 1 }\n")]);
    repo.write("a.rs", "fn a() { 2 }\n");

    let out = run_capture(
        &default_args(),
        repo.path(),
        &opts_with(CliLayout::Unified, false),
    );
    let text = String::from_utf8(out).unwrap();
    assert!(
        !text.contains('\x1b'),
        "expected no SGR escapes, got: {text}"
    );
}

#[test]
fn default_with_color_emits_sgr() {
    let repo = TestRepo::new();
    repo.commit_initial(&[("a.rs", "fn a() { 1 }\n")]);
    repo.write("a.rs", "fn a() { 2 }\n");

    let out = run_capture(
        &default_args(),
        repo.path(),
        &opts_with(CliLayout::Unified, true),
    );
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains('\x1b'), "expected SGR escapes, got: {text}");
}

#[test]
fn default_structural_rename_chip() {
    let repo = TestRepo::new();
    let a_base = "fn migrated() {\n    let x = 1;\n}\nfn stays_a() {\n    call_a();\n}\n";
    let a_rhs = "fn stays_a() {\n    call_a();\n}\n";
    let b_base = "fn stays_b() {\n    call_b();\n}\n";
    let b_rhs = "fn stays_b() {\n    call_b();\n}\nfn migrated() {\n    let x = 1;\n}\n";
    repo.commit_initial(&[("a.rs", a_base), ("b.rs", b_base)]);
    repo.write("a.rs", a_rhs);
    repo.write("b.rs", b_rhs);

    let out = run_capture(
        &default_args(),
        repo.path(),
        &opts_with(CliLayout::SideBySide, false),
    );
    insta::assert_snapshot!("diff_cli_rename_chip", String::from_utf8(out).unwrap());
}

#[test]
fn git_mode_create_via_dev_null() {
    let dir = tempfile::tempdir().unwrap();
    let new_path = dir.path().join("new.rs");
    std::fs::write(&new_path, "fn new() {}\n").unwrap();

    let mut args = default_args();
    args.git = true;
    args.git_args = vec![
        "new.rs".to_string(),
        "/dev/null".to_string(),
        "0000000000000000000000000000000000000000".to_string(),
        "000000".to_string(),
        new_path.display().to_string(),
        "1111111111111111111111111111111111111111".to_string(),
        "100644".to_string(),
    ];

    let out = run_capture(&args, dir.path(), &opts_with(CliLayout::Unified, false));
    insta::assert_snapshot!("diff_cli_git_create", String::from_utf8(out).unwrap());
}

#[test]
fn git_mode_delete_via_dev_null() {
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.rs");
    std::fs::write(&old_path, "fn old() {}\n").unwrap();

    let mut args = default_args();
    args.git = true;
    args.git_args = vec![
        "old.rs".to_string(),
        old_path.display().to_string(),
        "1111111111111111111111111111111111111111".to_string(),
        "100644".to_string(),
        "/dev/null".to_string(),
        "0000000000000000000000000000000000000000".to_string(),
        "000000".to_string(),
    ];

    let out = run_capture(&args, dir.path(), &opts_with(CliLayout::Unified, false));
    insta::assert_snapshot!("diff_cli_git_delete", String::from_utf8(out).unwrap());
}

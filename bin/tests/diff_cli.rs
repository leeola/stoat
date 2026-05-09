//! Integration tests for `stoat diff`.
//!
//! Production wires `LocalGit` + `LocalFs` against the real
//! filesystem; these tests do the same against a per-test
//! `tempfile::TempDir`. The same carve-out the existing
//! `stoat/tests/local_git.rs` integration tests rely on -- a real
//! git2 repo is the only realistic way to exercise `LocalGit`'s
//! discovery and head-content paths.

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use git2::Repository;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat::{
    diff::{extract_review_hunks_changeset, ReviewFileInput},
    diff_cache::serialize_hunks,
    diff_render_cli::{CliLayout, CliRenderOptions},
    host::{LocalFs, LocalGit},
};
use stoat_bin::commands::diff::{run_with_io, DiffArgs, WriteError};
use tokio::net::UnixListener;
use tokio_util::codec::{FramedRead, FramedWrite};
use viewport::protocol::{ToMain, ToMainCodec, ToViewport, ToViewportCodec};

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
    run_capture_with_socket_dir(args, cwd, None, opts)
}

fn run_capture_with_socket_dir(
    args: &DiffArgs,
    cwd: &Path,
    socket_dir: Option<&Path>,
    opts: &CliRenderOptions,
) -> Vec<u8> {
    let fs = LocalFs;
    let git = LocalGit::new();
    let mut buf = Vec::new();
    match run_with_io(args, &fs, &git, cwd, socket_dir, opts, &mut buf) {
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

#[tokio::test(flavor = "current_thread")]
async fn cache_rpc_hit_uses_cached_hunks() {
    let socket_dir = tempfile::tempdir().expect("tempdir");
    let socket_path = socket_dir.path().join("stoat-test.sock");
    let listener = UnixListener::bind(&socket_path).expect("bind socket");

    let repo = TestRepo::new();
    repo.commit_initial(&[("a.rs", "fn a() { 1 }\n")]);
    repo.write("a.rs", "fn a() { 2 }\n");

    let synthetic_input = ReviewFileInput {
        path: PathBuf::from("synthetic.rs"),
        rel_path: "synthetic.rs".to_string(),
        language: None,
        base_text: Arc::new("synthetic_left".to_string()),
        buffer_text: Arc::new("synthetic_right".to_string()),
    };
    let synthetic_hunks = extract_review_hunks_changeset(std::slice::from_ref(&synthetic_input), 3)
        .into_iter()
        .next()
        .expect("synthetic hunks");
    let synthetic_payload = serialize_hunks(&synthetic_hunks);

    let payload_for_listener = synthetic_payload.clone();
    let listener_handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let (read_half, write_half) = stream.into_split();
        let mut reader = FramedRead::new(read_half, ToMainCodec::new());
        let mut writer = FramedWrite::new(write_half, ToViewportCodec::new());
        while let Some(msg) = reader.next().await {
            match msg.expect("decode") {
                ToMain::DiffRequest { .. } => {
                    writer
                        .send(ToViewport::DiffResponse {
                            result: Some(Bytes::from(payload_for_listener.clone())),
                        })
                        .await
                        .expect("send response");
                },
                _ => break,
            }
        }
    });

    let cwd = repo.path().to_path_buf();
    let socket_dir_path = socket_dir.path().to_path_buf();
    let listener_done = tokio::task::spawn_blocking(move || {
        let fs = LocalFs;
        let git = LocalGit::new();
        let mut buf = Vec::new();
        run_with_io(
            &default_args(),
            &fs,
            &git,
            &cwd,
            Some(&socket_dir_path),
            &opts_with(CliLayout::Unified, false),
            &mut buf,
        )
        .expect("run_with_io ok");
        buf
    });
    let out = listener_done.await.expect("blocking task");
    listener_handle.abort();

    let text = String::from_utf8(out).expect("utf8");
    assert!(
        text.contains("synthetic_right") || text.contains("synthetic_left"),
        "expected cached hunks (synthetic) in output, got: {text}"
    );
}

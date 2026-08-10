// These tests exercise LocalFs against the real filesystem, so they set up
// files in a tempdir with std::fs directly.
#![allow(clippy::disallowed_methods)]

use stoat::host::{FsHost, LocalFs};

#[test]
fn read_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hello.txt");
    std::fs::write(&path, b"hello world").unwrap();

    let fs = LocalFs;
    let mut buf = Vec::new();
    fs.read(&path, &mut buf).unwrap();
    assert_eq!(buf, b"hello world");
}

#[test]
fn read_prefix_caps_at_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let big = dir.path().join("big.txt");
    std::fs::write(&big, b"0123456789").unwrap();
    let small = dir.path().join("small.txt");
    std::fs::write(&small, b"hi").unwrap();

    let fs = LocalFs;
    let mut buf = Vec::new();
    fs.read_prefix(&big, 4, &mut buf).unwrap();
    assert_eq!(buf, b"0123", "reads only the first `limit` bytes");
    fs.read_prefix(&small, 128, &mut buf).unwrap();
    assert_eq!(
        buf, b"hi",
        "a shorter file yields all of it and clears prior bytes"
    );
}

#[test]
fn write_read_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.bin");

    let fs = LocalFs;
    fs.write(&path, b"round trip").unwrap();

    let mut buf = Vec::new();
    fs.read(&path, &mut buf).unwrap();
    assert_eq!(buf, b"round trip");
}

#[test]
fn metadata_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, b"abc").unwrap();

    let fs = LocalFs;
    let m = fs.metadata(&path).unwrap().unwrap();
    assert_eq!(m.len, 3);
    assert!(!m.is_dir);
    assert!(!m.is_symlink);
}

#[test]
fn metadata_nonexistent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nope");

    let fs = LocalFs;
    assert!(fs.metadata(&path).unwrap().is_none());
}

#[test]
fn list_dir_entries() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), b"").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();

    let fs = LocalFs;
    let mut entries = fs.list_dir(dir.path()).unwrap();
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name.as_str(), "a.txt");
    assert!(!entries[0].is_dir);
    assert_eq!(entries[1].name.as_str(), "sub");
    assert!(entries[1].is_dir);
}

#[test]
fn rename_moves_file() {
    let dir = tempfile::tempdir().unwrap();
    let from = dir.path().join("a.txt");
    let to = dir.path().join("b.txt");
    std::fs::write(&from, b"contents").unwrap();

    let fs = LocalFs;
    fs.rename(&from, &to).unwrap();

    assert!(!fs.exists(&from));
    let mut buf = Vec::new();
    fs.read(&to, &mut buf).unwrap();
    assert_eq!(buf, b"contents");
}

#[test]
fn exists_true_and_false() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("yes");
    std::fs::write(&path, b"").unwrap();

    let fs = LocalFs;
    assert!(fs.exists(&path));
    assert!(!fs.exists(&dir.path().join("no")));
}

#[test]
fn walk_workspace_files_honors_dot_git_info_exclude() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".git/info")).unwrap();
    std::fs::write(root.join(".git/info/exclude"), "excluded.txt\n").unwrap();
    std::fs::write(root.join("excluded.txt"), b"").unwrap();
    std::fs::write(root.join("kept.txt"), b"").unwrap();

    let fs = LocalFs;
    let walked: Vec<_> = fs
        .walk_workspace_files(root)
        .into_iter()
        .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
        .collect();

    assert_eq!(walked, vec!["kept.txt".to_string()]);
}

#[test]
fn walk_workspace_files_honors_gitignore() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(root.join("ignored.txt"), b"").unwrap();
    std::fs::write(root.join("kept.txt"), b"").unwrap();

    let fs = LocalFs;
    let mut walked: Vec<_> = fs
        .walk_workspace_files(root)
        .into_iter()
        .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
        .collect();
    walked.sort();

    assert_eq!(
        walked,
        vec![".gitignore".to_string(), "kept.txt".to_string()]
    );
}

#[test]
fn walk_workspace_files_excludes_baked_in_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("target/debug")).unwrap();
    std::fs::write(root.join("target/debug/bin"), b"").unwrap();
    std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
    std::fs::write(root.join("node_modules/pkg/index.js"), b"").unwrap();
    std::fs::write(root.join("src.rs"), b"").unwrap();

    let fs = LocalFs;
    let walked: Vec<_> = fs
        .walk_workspace_files(root)
        .into_iter()
        .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
        .collect();

    assert_eq!(walked, vec!["src.rs".to_string()]);
}

/// The parallel walk has to deliver every file, which the serial one gets for
/// free. Each of its threads buffers a batch and flushes the remainder when it
/// ends, so a count that is not a multiple of the batch size is what catches a
/// missing flush.
#[test]
fn walk_workspace_files_parallel_delivers_every_file() {
    use std::{
        ops::ControlFlow,
        sync::{Arc, Mutex},
    };

    let dir = tempfile::tempdir().unwrap();
    // Enough to fill batches on several threads and leave each a partial one.
    let count = 700;
    for i in 0..count {
        std::fs::write(dir.path().join(format!("f{i}.txt")), b"x").unwrap();
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    LocalFs.walk_workspace_files_parallel(dir.path(), &move |batch| {
        sink.lock().unwrap().extend(batch);
        ControlFlow::Continue(())
    });

    let mut got = seen.lock().unwrap().clone();
    got.sort();
    let mut want = LocalFs.walk_workspace_files(dir.path());
    want.sort();
    assert_eq!(got.len(), count, "every file arrives exactly once");
    assert_eq!(
        got, want,
        "and the parallel walk sees what the serial one does"
    );
}

/// Breaking stops the walk rather than running it out, which is what lets a
/// closed search abandon a large tree.
///
/// Measured in files rather than batches. How many batches a break costs
/// depends on how many walker threads were mid-batch when it landed, and each
/// flushes its remainder on the way out, so any batch count is a fact about the
/// machine. What the contract promises is that the rest of the tree goes
/// unenumerated, and that is what a file count shows on any machine.
#[test]
fn walk_workspace_files_parallel_stops_on_break() {
    use std::{
        ops::ControlFlow,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    let dir = tempfile::tempdir().unwrap();
    let count = 4_000;
    for i in 0..count {
        std::fs::write(dir.path().join(format!("f{i}.txt")), b"x").unwrap();
    }

    let delivered = Arc::new(AtomicUsize::new(0));
    let counter = delivered.clone();
    LocalFs.walk_workspace_files_parallel(dir.path(), &move |batch| {
        counter.fetch_add(batch.len(), Ordering::Relaxed);
        ControlFlow::Break(())
    });

    let seen = delivered.load(Ordering::Relaxed);
    assert!(
        seen < count,
        "a break must abandon the rest of the tree, but all {count} files arrived",
    );
}

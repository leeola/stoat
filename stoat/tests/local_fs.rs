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

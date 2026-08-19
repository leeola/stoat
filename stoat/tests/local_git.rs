// These tests exercise LocalGit against real git repositories, so they
// materialize working trees in a tempdir with std::fs directly.
#![allow(clippy::disallowed_methods)]

use git2::{FileMode, Oid, Repository, Signature};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use stoat::host::{
    CommitFileChangeKind, GitApplyError, GitHost, LocalGit, RebaseTodo, RebaseTodoOp,
};

/// On-disk git repo for integration-testing [`LocalGit`].
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

    fn join(&self, name: &str) -> PathBuf {
        self.path().join(name)
    }

    fn write(&self, name: &str, content: &str) -> &Self {
        let path = self.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(&path, content).expect("write file");
        self
    }

    fn stage(&self, name: &str) -> &Self {
        let mut index = self.repo.index().expect("index");
        index.add_path(Path::new(name)).expect("add path");
        index.write().expect("write index");
        self
    }

    fn write_and_stage(&self, name: &str, content: &str) -> &Self {
        self.write(name, content).stage(name)
    }

    fn commit(&self, message: &str) -> &Self {
        let mut index = self.repo.index().expect("index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = self.repo.find_tree(tree_id).expect("find tree");
        let sig = Signature::now("test", "t@t").expect("sig");
        let parents: Vec<_> = self
            .repo
            .head()
            .ok()
            .and_then(|h| h.peel_to_commit().ok())
            .into_iter()
            .collect();
        let parent_refs: Vec<_> = parents.iter().collect();
        self.repo
            .commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)
            .expect("commit");
        self
    }

    fn commit_file(&self, name: &str, content: &str) -> &Self {
        self.write_and_stage(name, content).commit("c")
    }

    fn head_sha(&self) -> String {
        self.repo
            .head()
            .expect("HEAD")
            .peel_to_commit()
            .expect("commit")
            .id()
            .to_string()
    }

    /// Commit a flat tree of `files` onto `parent_sha`, pointing `refname`
    /// (e.g. `refs/heads/theirs`) at the new commit. Leaves HEAD and the
    /// working tree untouched, so a divergent branch can be built to merge.
    fn commit_flat_tree_on(
        &self,
        refname: &str,
        parent_sha: &str,
        files: &[(&str, &str)],
        message: &str,
    ) -> String {
        let mut builder = self.repo.treebuilder(None).expect("treebuilder");
        for (name, content) in files {
            let oid = self.repo.blob(content.as_bytes()).expect("blob");
            builder
                .insert(name, oid, FileMode::Blob.into())
                .expect("insert");
        }
        let tree = self
            .repo
            .find_tree(builder.write().expect("write tree"))
            .expect("find tree");
        let parent = self
            .repo
            .find_commit(Oid::from_str(parent_sha).expect("oid"))
            .expect("parent");
        let sig = Signature::now("test", "t@t").expect("sig");
        self.repo
            .commit(Some(refname), &sig, &sig, message, &tree, &[&parent])
            .expect("commit")
            .to_string()
    }
}

fn staged_blob(workdir: &Path, rel: &str) -> Option<String> {
    let repo = Repository::open(workdir).ok()?;
    let mut index = repo.index().ok()?;
    index.read(true).ok()?;
    let entry = index.get_path(Path::new(rel), 0)?;
    let blob = repo.find_blob(entry.id).ok()?;
    std::str::from_utf8(blob.content()).ok().map(String::from)
}

#[test]
fn discover_finds_repo() {
    let tr = TestRepo::new();
    let host = LocalGit::new();
    assert!(host.discover(tr.path()).is_some());
}

#[test]
fn discover_returns_none_outside_repo() {
    let dir = tempfile::tempdir().unwrap();
    let host = LocalGit::new();
    assert!(host.discover(dir.path()).is_none());
}

#[test]
fn workdir_returns_repo_workdir() {
    let tr = TestRepo::new();
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    assert_eq!(repo.workdir().as_deref(), Some(tr.path()));
}

#[test]
fn changed_files_empty_when_clean() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "fn main() {}");
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    assert!(repo.changed_files().is_empty());
}

#[test]
fn changed_files_detects_modified() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "v1");
    tr.write("a.rs", "v2");
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    let files = repo.changed_files();
    assert_eq!(files.len(), 1);
    assert!(!files[0].staged);
    assert!(!files[0].untracked, "tracked modification is not untracked");
    assert!(files[0].path.ends_with("a.rs"));
}

#[test]
fn changed_files_staged_first() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "v1").commit_file("b.rs", "v1");
    tr.write_and_stage("b.rs", "v2");
    tr.write("a.rs", "v2");
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    let files = repo.changed_files();
    assert_eq!(files.len(), 2);
    assert!(files[0].staged);
    assert!(files[0].path.ends_with("b.rs"));
    assert!(!files[1].staged);
    assert!(files[1].path.ends_with("a.rs"));
}

#[test]
fn changed_files_deduplicates_staged_and_unstaged() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "v1");
    tr.write_and_stage("a.rs", "v2");
    tr.write("a.rs", "v3");
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    let files = repo.changed_files();
    assert_eq!(files.len(), 1);
    assert!(files[0].staged);
}

/// The two sides are counted apart, unlike `changed_files`, which resolves each
/// file to one side. A half-staged file is work waiting on both, so the bar's
/// tally has to see it twice.
#[test]
fn change_counts_tallies_each_side_independently() {
    let tr = TestRepo::new();
    tr.commit_file("staged.rs", "v1")
        .commit_file("unstaged.rs", "v1")
        .commit_file("both.rs", "v1");

    tr.write_and_stage("staged.rs", "v2");
    tr.write("unstaged.rs", "v2");
    tr.write("untracked.rs", "v1");
    tr.write_and_stage("both.rs", "v2").write("both.rs", "v3");

    let repo = LocalGit::new().discover(tr.path()).unwrap();
    assert_eq!(
        repo.change_counts(),
        (2, 3),
        "both.rs counts on each side, and the untracked file counts as unstaged"
    );
}

#[test]
fn change_counts_zero_on_a_clean_repo() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "v1");
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    assert_eq!(repo.change_counts(), (0, 0));
}

#[test]
fn changed_files_reports_untracked_on_no_commits() {
    let tr = TestRepo::new();
    tr.write("new.rs", "x");
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    let files = repo.changed_files();
    assert_eq!(files.len(), 1);
    assert!(!files[0].staged);
    assert!(files[0].untracked, "untracked file carries the flag");
    assert!(files[0].path.ends_with("new.rs"));
}

#[test]
fn head_content_reads_blob() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "fn main() {}");
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    assert_eq!(
        repo.head_content(&tr.join("a.rs")).as_deref(),
        Some("fn main() {}")
    );
}

#[test]
fn blob_text_arrives_with_normalized_line_endings() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "one\r\ntwo\r\n");
    tr.write_and_stage("a.rs", "one\r\nTWO\r\n");
    let repo = LocalGit::new().discover(tr.path()).unwrap();

    assert_eq!(
        repo.head_content(&tr.join("a.rs")).as_deref(),
        Some("one\ntwo\n"),
        "a CRLF HEAD blob reads back as the buffer holds it"
    );
    assert_eq!(
        repo.index_content(&tr.join("a.rs")).as_deref(),
        Some("one\nTWO\n"),
        "a CRLF index blob reads back as the buffer holds it"
    );
}

#[test]
fn head_content_none_for_new_file() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "v1");
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    assert!(repo.head_content(&tr.join("b.rs")).is_none());
}

#[test]
fn head_content_none_on_orphan_branch() {
    let tr = TestRepo::new();
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    assert!(repo.head_content(&tr.join("a.rs")).is_none());
}

#[test]
fn apply_to_index_stages_modification() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "old\n");
    tr.write("a.rs", "new\n");
    let patch = "diff --git a/a.rs b/a.rs\n\
                 --- a/a.rs\n\
                 +++ b/a.rs\n\
                 @@ -1,1 +1,1 @@\n\
                 -old\n\
                 +new\n";
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    repo.apply_to_index(patch).expect("apply ok");
    assert_eq!(staged_blob(tr.path(), "a.rs").as_deref(), Some("new\n"));
}

#[test]
fn apply_to_index_stages_pure_addition() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "seed\n");
    tr.write("b.rs", "hello\n");
    let patch = "diff --git a/b.rs b/b.rs\n\
                 new file mode 100644\n\
                 --- /dev/null\n\
                 +++ b/b.rs\n\
                 @@ -0,0 +1,1 @@\n\
                 +hello\n";
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    repo.apply_to_index(patch).expect("apply ok");
    assert_eq!(staged_blob(tr.path(), "b.rs").as_deref(), Some("hello\n"));
}

#[test]
fn apply_to_index_stages_pure_deletion() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "gone\n");
    std::fs::remove_file(tr.join("a.rs")).unwrap();
    let patch = "diff --git a/a.rs b/a.rs\n\
                 deleted file mode 100644\n\
                 --- a/a.rs\n\
                 +++ /dev/null\n\
                 @@ -1,1 +0,0 @@\n\
                 -gone\n";
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    repo.apply_to_index(patch).expect("apply ok");
    assert!(
        staged_blob(tr.path(), "a.rs").is_none(),
        "index should no longer have a.rs"
    );
}

#[test]
fn apply_to_index_rejects_malformed_patch() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "x\n");
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    let err = repo
        .apply_to_index("not a patch")
        .expect_err("garbage must fail");
    let GitApplyError::Backend { reason, .. } = err;
    assert!(!reason.is_empty(), "error message must be non-empty");
}

#[test]
fn apply_to_index_rejects_conflicting_patch() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "actual\n");
    let patch = "diff --git a/a.rs b/a.rs\n\
                 --- a/a.rs\n\
                 +++ b/a.rs\n\
                 @@ -1,1 +1,1 @@\n\
                 -unexpected\n\
                 +new\n";
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    let err = repo
        .apply_to_index(patch)
        .expect_err("conflicting context must fail");
    let GitApplyError::Backend { reason, .. } = err;
    assert!(
        reason.ends_with(r#"(a.rs: index line 1 is "actual" but patch expects "unexpected")"#),
        "reason must name the file and the diverging line, got: {reason}"
    );
}

#[test]
fn commit_tree_reads_all_blobs() {
    let tr = TestRepo::new();
    tr.write_and_stage("a.rs", "hello a")
        .write_and_stage("sub/b.rs", "hello b")
        .commit("c1");
    let sha = tr.head_sha();
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    let tree = repo.commit_tree(&sha).expect("tree");
    assert_eq!(
        tree.get(Path::new("a.rs")).map(String::as_str),
        Some("hello a")
    );
    assert_eq!(
        tree.get(Path::new("sub/b.rs")).map(String::as_str),
        Some("hello b")
    );
    assert_eq!(tree.len(), 2);
}

/// Reading only the changed blobs has to land on the same answer as comparing
/// the two whole trees, which is what the trait's own default does.
///
/// The saving is invisible in the result, so the differential against the
/// trees is the only thing that says the faster path is also the right one.
#[test]
fn changed_contents_agrees_with_diffing_the_whole_trees() {
    let tr = TestRepo::new();
    tr.write_and_stage("keep.rs", "unchanged")
        .write_and_stage("edit.rs", "before")
        .write_and_stage("gone.rs", "removed later")
        .commit("c1");
    let first = tr.head_sha();

    tr.write_and_stage("edit.rs", "after")
        .write_and_stage("added.rs", "new file")
        .commit("c2");
    {
        let mut index = tr.repo.index().expect("index");
        index.remove_path(Path::new("gone.rs")).expect("remove");
        index.write().expect("write index");
    }
    tr.commit("c3");
    let last = tr.head_sha();

    let repo = LocalGit::new().discover(tr.path()).unwrap();
    let base: BTreeMap<PathBuf, String> = repo.commit_tree(&first).expect("base tree");
    let new: BTreeMap<PathBuf, String> = repo.commit_tree(&last).expect("new tree");

    let mut expected: Vec<(PathBuf, String, String)> = Vec::new();
    for rel in base.keys().chain(new.keys()).collect::<BTreeSet<_>>() {
        let before = base.get(rel).cloned().unwrap_or_default();
        let after = new.get(rel).cloned().unwrap_or_default();
        if before != after {
            expected.push((rel.clone(), before, after));
        }
    }

    let mut got = repo
        .changed_contents(Some(&first), &last)
        .expect("changed contents");
    got.sort_by(|a, b| a.0.cmp(&b.0));

    assert_eq!(got, expected);
    assert!(
        !got.iter().any(|(rel, _, _)| rel == Path::new("keep.rs")),
        "an unchanged file is never read, let alone reported",
    );
}

/// A commit that touches a binary still yields its text changes.
///
/// `commit_tree` refuses a whole tree holding any non-UTF-8 blob, so a
/// repository with one image in it could not be reviewed at all. Reading only
/// what changed drops the file a review could not show and keeps the rest.
#[test]
fn changed_contents_skips_a_changed_binary_and_keeps_the_text() {
    let tr = TestRepo::new();
    std::fs::write(tr.join("logo.png"), [0xff, 0xd8, 0x00, 0x01]).expect("write binary");
    tr.stage("logo.png")
        .write_and_stage("a.rs", "v1")
        .commit("c1");
    let first = tr.head_sha();

    // The binary changes too, so it reaches the diff rather than being absent
    // from it, which is what makes this exercise the skip.
    std::fs::write(tr.join("logo.png"), [0xff, 0xd8, 0x00, 0x02]).expect("rewrite binary");
    tr.stage("logo.png")
        .write_and_stage("a.rs", "v2")
        .commit("c2");
    let second = tr.head_sha();

    let repo = LocalGit::new().discover(tr.path()).unwrap();
    assert!(
        repo.commit_tree(&second).is_none(),
        "the whole-tree read gives up over the binary",
    );
    assert_eq!(
        repo.changed_contents(Some(&first), &second),
        Some(vec![(
            PathBuf::from("a.rs"),
            "v1".to_string(),
            "v2".to_string()
        )]),
    );
}

/// A file whose mode changed but whose bytes did not is not a changed file.
///
/// The tree diff reports it as a delta all the same, so it is the one case
/// where the equal-contents check earns its keep. Reporting it would put a file
/// with nothing to show into a review.
#[test]
fn changed_contents_ignores_a_mode_only_change() {
    let tr = TestRepo::new();
    tr.write_and_stage("run.sh", "echo hi").commit("c1");
    let first = tr.head_sha();

    {
        let mut index = tr.repo.index().expect("index");
        let mut entry = index.get_path(Path::new("run.sh"), 0).expect("entry");
        entry.mode = u32::from(FileMode::BlobExecutable);
        index.add(&entry).expect("re-add executable");
        index.write().expect("write index");
    }
    tr.commit("c2");
    let second = tr.head_sha();

    let repo = LocalGit::new().discover(tr.path()).unwrap();
    assert_ne!(first, second, "the mode change made a commit of its own");
    assert_eq!(
        repo.changed_contents(Some(&first), &second),
        Some(Vec::new())
    );
}

#[test]
fn commit_tree_unknown_sha_returns_none() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "x");
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    assert!(repo.commit_tree("bad-sha").is_none());
    assert!(repo.commit_tree(&"0".repeat(40)).is_none());
}

#[test]
fn parent_sha_follows_first_parent() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "v1");
    let c1 = tr.head_sha();
    tr.commit_file("a.rs", "v2");
    let c2 = tr.head_sha();

    let repo = LocalGit::new().discover(tr.path()).unwrap();
    assert_eq!(repo.parent_sha(&c2), Some(c1));
}

#[test]
fn parent_sha_root_commit_returns_none() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "root");
    let root = tr.head_sha();
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    assert!(repo.parent_sha(&root).is_none());
}

#[test]
fn log_commits_walks_first_parent_from_head() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "v1");
    tr.commit_file("a.rs", "v2");
    tr.commit_file("a.rs", "v3");

    let repo = LocalGit::new().discover(tr.path()).unwrap();
    let log = repo.log_commits(None, 10);
    assert_eq!(log.len(), 3, "three commits on this branch");
    assert!(log[0].summary.contains('c') || log[0].summary.is_empty());
    let shas: Vec<_> = log.iter().map(|c| c.sha.clone()).collect();
    let unique: BTreeSet<_> = shas.iter().collect();
    assert_eq!(unique.len(), 3, "shas must be distinct");
}

#[test]
fn log_commits_respects_limit_and_after() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "v1");
    tr.commit_file("a.rs", "v2");
    tr.commit_file("a.rs", "v3");

    let repo = LocalGit::new().discover(tr.path()).unwrap();
    let first = repo.log_commits(None, 2);
    assert_eq!(first.len(), 2);

    let second = repo.log_commits(Some(&first[1].sha), 10);
    assert_eq!(
        second.len(),
        1,
        "one commit remains after skipping the first two"
    );
    assert_ne!(second[0].sha, first[0].sha);
    assert_ne!(second[0].sha, first[1].sha);
}

#[test]
fn log_commits_empty_on_orphan_branch() {
    let tr = TestRepo::new();
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    assert!(repo.log_commits(None, 10).is_empty());
}

#[test]
fn commit_file_changes_reports_added_modified_deleted() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "alpha\n");
    tr.commit_file("b.rs", "beta\n");
    let add_sha = tr.head_sha();

    tr.write_and_stage("a.rs", "alpha-v2\n");
    tr.stage("b.rs");
    std::fs::remove_file(tr.join("b.rs")).unwrap();
    let mut index = tr.repo.index().unwrap();
    index.remove_path(Path::new("b.rs")).unwrap();
    index.write().unwrap();
    tr.commit("rewrite");
    let rewrite_sha = tr.head_sha();

    let repo = LocalGit::new().discover(tr.path()).unwrap();
    let changes_add = repo.commit_file_changes(&add_sha);
    assert!(changes_add
        .iter()
        .any(|c| c.rel_path == Path::new("b.rs") && c.kind == CommitFileChangeKind::Added));

    let changes_rewrite = repo.commit_file_changes(&rewrite_sha);
    assert!(changes_rewrite
        .iter()
        .any(|c| c.rel_path == Path::new("a.rs") && c.kind == CommitFileChangeKind::Modified));
    assert!(changes_rewrite
        .iter()
        .any(|c| c.rel_path == Path::new("b.rs") && c.kind == CommitFileChangeKind::Deleted));
}

#[test]
fn amend_head_replaces_tree_and_updates_head() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "v1\n");
    let before = tr.head_sha();
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    let mut new_tree = BTreeMap::new();
    new_tree.insert(PathBuf::from("a.rs"), "amended\n".to_string());
    let new_sha = repo.amend_head(&new_tree, None).expect("amend ok");
    assert_ne!(new_sha, before, "amend must produce a new sha");
    let tree = repo.commit_tree(&new_sha).unwrap();
    assert_eq!(
        tree.get(Path::new("a.rs")).map(String::as_str),
        Some("amended\n")
    );
}

#[test]
fn rewrite_commit_cherry_picks_descendants() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "v1\n");
    let c1 = tr.head_sha();
    tr.commit_file("a.rs", "v1\nv2\n");
    let c2 = tr.head_sha();
    tr.commit_file("b.rs", "b1\n");
    let c3 = tr.head_sha();

    let repo = LocalGit::new().discover(tr.path()).unwrap();

    let mut tree = BTreeMap::new();
    tree.insert(PathBuf::from("a.rs"), "v1_rewritten\n".to_string());
    let result = repo
        .rewrite_commit(&c2, &tree, None, std::slice::from_ref(&c3))
        .expect("rewrite ok");
    assert_ne!(result.new_head, c3);
    assert!(result.mapping.contains_key(&c2));
    assert!(result.mapping.contains_key(&c3));

    let new_c2 = result.mapping.get(&c2).unwrap();
    let new_c2_tree = repo.commit_tree(new_c2).unwrap();
    assert_eq!(
        new_c2_tree.get(Path::new("a.rs")).map(String::as_str),
        Some("v1_rewritten\n")
    );

    let new_c3 = &result.new_head;
    let new_c3_tree = repo.commit_tree(new_c3).unwrap();
    assert_eq!(
        new_c3_tree.get(Path::new("b.rs")).map(String::as_str),
        Some("b1\n"),
        "descendant's additions carried through cherry-pick"
    );
    assert_eq!(
        repo.commit_tree(&c1)
            .unwrap()
            .get(Path::new("a.rs"))
            .map(String::as_str),
        Some("v1\n")
    );
}

#[test]
fn run_rebase_drop_skips_entry() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "v1\n");
    let c1 = tr.head_sha();
    tr.commit_file("b.rs", "b1\n");
    let c2 = tr.head_sha();
    tr.commit_file("c.rs", "c1\n");
    let c3 = tr.head_sha();

    let repo = LocalGit::new().discover(tr.path()).unwrap();
    let plan = vec![
        RebaseTodo {
            op: RebaseTodoOp::Drop,
            sha: c2.clone(),
            message: String::new(),
        },
        RebaseTodo {
            op: RebaseTodoOp::Pick,
            sha: c3.clone(),
            message: String::new(),
        },
    ];
    let new_head = repo.run_rebase(&c1, &plan).expect("rebase ok");
    let tree = repo.commit_tree(&new_head).unwrap();
    assert!(tree.contains_key(Path::new("a.rs")), "a.rs from c1 present");
    assert!(
        !tree.contains_key(Path::new("b.rs")),
        "b.rs from dropped c2 gone"
    );
    assert!(
        tree.contains_key(Path::new("c.rs")),
        "c.rs from picked c3 present"
    );
}

#[test]
fn commit_file_changes_unknown_sha_empty() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "x");
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    assert!(repo.commit_file_changes("nope").is_empty());
}

#[test]
fn apply_to_index_rejects_double_apply() {
    let tr = TestRepo::new();
    tr.commit_file("a.rs", "v1\n");
    tr.write("a.rs", "v2\n");
    let patch = "diff --git a/a.rs b/a.rs\n\
                 --- a/a.rs\n\
                 +++ b/a.rs\n\
                 @@ -1,1 +1,1 @@\n\
                 -v1\n\
                 +v2\n";
    let repo = LocalGit::new().discover(tr.path()).unwrap();
    repo.apply_to_index(patch).expect("first apply ok");
    let err = repo
        .apply_to_index(patch)
        .expect_err("context no longer matches on second apply");
    let GitApplyError::Backend { reason, .. } = err;
    assert!(
        reason.ends_with(r#"(a.rs: index line 1 is "v2" but patch expects "v1")"#),
        "reason must name the file and the already-applied line, got: {reason}"
    );
}

#[test]
fn conflicted_paths_and_stages_read_on_disk_index_conflicts() {
    let tr = TestRepo::new();
    tr.commit_file("f.txt", "base\n");
    let base = tr.head_sha();

    let theirs = tr.commit_flat_tree_on(
        "refs/heads/theirs",
        &base,
        &[("f.txt", "theirs\n")],
        "theirs",
    );
    tr.commit_file("f.txt", "ours\n");

    let annotated = tr
        .repo
        .find_annotated_commit(Oid::from_str(&theirs).unwrap())
        .unwrap();
    tr.repo.merge(&[&annotated], None, None).unwrap();
    // Flush the merged index to disk for the fresh discover handle below.
    tr.repo.index().unwrap().write().unwrap();

    let repo = LocalGit::new().discover(tr.path()).unwrap();
    let f = tr.join("f.txt");
    assert_eq!(repo.conflicted_paths(), vec![f.clone()]);

    let stages = repo.conflict_stages(&f).expect("f.txt conflicted");
    assert_eq!(stages.ancestor.as_deref(), Some("base\n"));
    assert_eq!(stages.ours.as_deref(), Some("ours\n"));
    assert_eq!(stages.theirs.as_deref(), Some("theirs\n"));

    tr.write("f.txt", "merged\n");
    repo.mark_resolved(&f).expect("mark resolved");
    assert!(repo.conflicted_paths().is_empty());
    assert_eq!(staged_blob(tr.path(), "f.txt").as_deref(), Some("merged\n"));
}

#[test]
fn conflict_stages_add_add_conflict_has_no_ancestor() {
    let tr = TestRepo::new();
    tr.commit_file("f.txt", "base\n");
    let base = tr.head_sha();

    let theirs = tr.commit_flat_tree_on(
        "refs/heads/theirs",
        &base,
        &[("f.txt", "base\n"), ("g.txt", "theirs\n")],
        "add g on theirs",
    );
    tr.commit_file("g.txt", "ours\n");

    let annotated = tr
        .repo
        .find_annotated_commit(Oid::from_str(&theirs).unwrap())
        .unwrap();
    tr.repo.merge(&[&annotated], None, None).unwrap();
    tr.repo.index().unwrap().write().unwrap();

    let repo = LocalGit::new().discover(tr.path()).unwrap();
    let stages = repo
        .conflict_stages(&tr.join("g.txt"))
        .expect("g.txt conflicted");
    assert_eq!(stages.ancestor, None);
    assert_eq!(stages.ours.as_deref(), Some("ours\n"));
    assert_eq!(stages.theirs.as_deref(), Some("theirs\n"));
}
